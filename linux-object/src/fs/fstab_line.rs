//! Reading one line of `/etc/fstab`, and mounting the ones that come over
//! the network.
//!
//! Split out of [`super::mount_fstab`] rather than left in it, because the
//! loop in there needs a root filesystem with an fstab inside it before it
//! will run at all and none of these decisions do. Every one of them was a
//! few lines inside a six-deep nesting with no test, which is how an `nfs`
//! line could sit in the fstab doing nothing for as long as it did.

use alloc::sync::Arc;

use rcore_fs_mountfs::MNode;

use crate::error::LxError;

use super::nfs::greeting::Budget;
use super::{mount_ops, mount_state, resolve_or_create_dir};

/// How long the whole fstab run may spend greeting NFS servers, in total.
///
/// Not per line, and that is the point. A mount's own budget is a minute
/// (`greeting_budget`), which is right for `mount(2)`: someone typed it and
/// is waiting. At boot nobody is, and an fstab with three unreachable
/// servers would hold the deferred task for three minutes. Linux does not
/// need this bound because `mount -a` skips `_netdev` lines and leaves them
/// to a network-online target that this kernel has no equivalent of, so the
/// bound goes here instead. A **divergence**, and the reversible kind: a
/// reachable server answers in milliseconds and never touches it.
pub(super) const FSTAB_NETWORK_BUDGET_MS: u64 = 60_000;

/// What one line of `/etc/fstab` asks boot to do.
///
/// An enum and not three `continue`s inside the loop because the loop needs
/// a root filesystem with an fstab in it to run at all, and the decision
/// does not. Every one of these arms was untestable while it lived in
/// there, which is how an `nfs` line could sit in the fstab doing nothing.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FstabAction<'a> {
    /// Not boot's business, for the reason named.
    Skip(&'a str),
    /// A type this kernel has no driver for.
    Unsupported(LxError),
    /// Mounted over the network: the source is a string, not a path. Carries
    /// the type as [`mount_ops::parse_fstype`] spells it.
    Network(&'static str),
    /// Mounted from a block device, which is every other line.
    Block(&'static str),
}

/// Read one fstab line's target, type and options into what boot should do
/// with it.
///
/// The order is the part that matters and the part that was wrong. The type
/// is parsed **before** anything looks the source up, because for a network
/// line the source is not a path: looking `nas:/srv` up as a file is the
/// silent `ENOENT` that kept `nfs` out of the fstab for as long as it did.
/// It is the same ordering `mount_fs` uses and the same reason — Linux's
/// `FS_REQUIRES_DEV`, where a flagged type gets its source opened as a
/// device and an unflagged one gets it as an uninterpreted string.
///
/// What routes to [`FstabAction::Network`] is the output of `parse_fstype`
/// and not the word in the file, so `nfs3` cannot go down the block path.
pub(super) fn fstab_line_action<'a>(
    target: &'a str,
    fstype: &'a str,
    options: &'a str,
) -> FstabAction<'a> {
    if target == "/" {
        return FstabAction::Skip("the root is already mounted");
    }
    if target == "none" || fstype == "swap" {
        return FstabAction::Skip("no mountpoint");
    }
    // `mount -a`'s own rule, and honoured nowhere in this tree until now:
    // `noauto` means "not at boot". It is the escape hatch for a line that
    // exists for a later `mount /mnt` and should not be dialled while the
    // machine is coming up, so a network line has somewhere to go that
    // costs nothing.
    if fstab_has_option(options, "noauto") {
        return FstabAction::Skip("noauto");
    }
    match mount_ops::parse_fstype(fstype) {
        Err(e) => FstabAction::Unsupported(e),
        Ok(parsed) if mount_ops::is_network_fstype(parsed) => FstabAction::Network(parsed),
        Ok(parsed) => FstabAction::Block(parsed),
    }
}

/// The deadline every network line of this fstab run shares, built on the
/// first line that needs one.
///
/// `get_or_insert_with` and not `insert`, which is the whole of it: an
/// `insert` per line is a fresh minute per line, which is the arithmetic
/// [`FSTAB_NETWORK_BUDGET_MS`] exists to stop. Lazily, so a fstab with no
/// network line spends none of it and the clock starts when the first
/// server is actually dialled rather than when the file was opened.
pub(super) fn run_budget(slot: &mut Option<Budget>) -> &Budget {
    slot.get_or_insert_with(|| Budget::new(FSTAB_NETWORK_BUDGET_MS))
}

/// Whether `name` is one of the comma-separated options on an fstab line.
///
/// A function because `options.contains("noauto")` is wrong in the way that
/// only bites someone later: `auto` is also an option, `nfsnoauto` is a
/// plausible future one, and a substring match on a comma-separated list
/// says yes to a prefix of a longer name. Splitting is the only reading that
/// matches what `mount(8)` does with the field.
pub(super) fn fstab_has_option(options: &str, name: &str) -> bool {
    options.split(',').any(|opt| opt.trim() == name)
}

/// The `MS_*` flags an fstab option field asks for.
///
/// Lifted out of [`super::mount_fstab`]'s loop so it has a name and a test.
/// It was four lines inside a six-deep nesting, and the thing worth checking
/// about it is not that it loops but that `rw` *clears* `MS_RDONLY` rather than
/// setting a flag of its own — which is how `ro,rw` and `rw,ro` come out
/// different, as they do in `mount(8)`.
pub(super) fn fstab_flags(options: &str) -> usize {
    let mut flags = 0;
    for opt in options.split(',') {
        match opt.trim() {
            "ro" => flags |= mount_state::MS_RDONLY,
            "rw" => flags &= !mount_state::MS_RDONLY,
            "nosuid" => flags |= mount_state::MS_NOSUID,
            "nodev" => flags |= mount_state::MS_NODEV,
            "noexec" => flags |= mount_state::MS_NOEXEC,
            _ => {}
        }
    }
    flags
}

/// The option string an fstab line hands a mount.
///
/// `defaults` is `mount(8)`'s word for "nothing in particular", and it is
/// not an NFS option: passed through it would reach `NfsOptions::parse` as
/// an option nobody recognises. The block path never noticed because
/// `open_filesystem` ignores what it does not know; the network path parses
/// its options strictly, which is the better behaviour and the reason this
/// has to be said out loud.
pub(super) fn fstab_mount_data(options: &str) -> &str {
    if options == "defaults" {
        ""
    } else {
        options
    }
}

/// Mount one network line from `/etc/fstab`.
///
/// Everything that differs from the block path is here, and it is the same
/// list as in `mount_fs`: the source is a string and not a path, the
/// options are the filesystem's own rather than a block device's, and the
/// server's name may need resolving before the synchronous mount can run.
///
/// `budget` is the whole run's, not this line's: see
/// [`FSTAB_NETWORK_BUDGET_MS`].
pub(super) async fn mount_fstab_network(
    root: &Arc<MNode>,
    source: &str,
    target: &str,
    fstype: &str,
    options: &str,
    budget: &Budget,
) {
    let target_node = match resolve_or_create_dir(root, target) {
        Ok(node) => node,
        Err(e) => {
            warn!(
                "mount_fstab: failed to resolve/create target {:?}: {:?}",
                target, e
            );
            return;
        }
    };
    if target_node.is_mountpoint() {
        warn!("mount_fstab: target {:?} is already a mountpoint", target);
        return;
    }

    let flags = fstab_flags(options);
    let data = fstab_mount_data(options);
    // The name is resolved here and the address is passed in the options,
    // which is exactly what `sys_mount` does and for the same reason: the
    // mount itself is synchronous. `None` means the options already said
    // everything, and that is every case but a name DNS alone knows.
    let resolved = match mount_ops::with_resolved_addr(fstype, Some(source), data).await {
        Ok(resolved) => resolved,
        Err(e) => {
            warn!(
                "mount_fstab: failed to resolve server for {:?}: {:?}",
                source, e
            );
            return;
        }
    };
    let data = resolved.as_deref().unwrap_or(data);

    // The target is normalised the way `mount(2)` normalises it, so the
    // mount table reads the same whether a mount came from the syscall or
    // from this file. An fstab may well say `/mnt/nas/`.
    let line_target = mount_ops::normalize_target(target);
    match mount_ops::mount_nfs_with_deadline(
        source,
        &line_target,
        &target_node,
        flags,
        data,
        Some(budget),
    ) {
        Ok(()) => info!(
            "mount_fstab: successfully mounted {:?} to {:?}",
            source, line_target
        ),
        Err(e) => warn!(
            "mount_fstab: failed to mount {:?} to {:?}: {:?}",
            source, line_target, e
        ),
    }
}

#[cfg(test)]
#[path = "fstab_line_tests.rs"]
mod fstab_line_tests;
