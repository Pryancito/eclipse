//! Reading one line of `/etc/fstab`.
//!
//! Split out of [`super::mount_fstab`] rather than left in it, because the
//! loop in there needs a root filesystem with an fstab inside it before it
//! will run at all and none of these decisions do. Every one of them was a
//! few lines inside a six-deep nesting with no test, which is how an `nfs`
//! line could sit in the fstab doing nothing for as long as it did.

use crate::error::LxError;

use super::{mount_ops, mount_state};

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

/// The service of the root filesystem that mounts the lines this pass
/// leaves, from the root: `eclipse-fs-mounts`, which starts a server
/// process for each (`eclipse-fatfs`, `eclipse-btrfs`, `eclipse-eclipsefs`,
/// `eclipse-nfs`).
pub(super) const FS_MOUNTS_SERVICE: &str = "etc/eclipse/services/fs-mounts.service";

/// Whether a line is left to its server, mounted by [`FS_MOUNTS_SERVICE`].
///
/// Every FAT and NFS line is, as the kernel has no FAT and no NFS client
/// of its own. A btrfs or EclipseFS line is when the root filesystem has
/// the service that mounts it, the boot does not keep those for the kernel
/// (`ufs=kernel`), and the line asks nothing a served mount would drop
/// ([`mount_ops::served_by_default`], the rule of `mount(2)`). A root
/// filesystem older than this kernel has no such service, and there the
/// kernel mounts them as it did; its FAT and NFS lines are not mounted. `parsed` is the type as
/// [`mount_ops::parse_fstype`] spells it, so every spelling is left or none
/// is.
pub(super) fn left_to_server(
    parsed: &str,
    options: &str,
    ufs: Option<&str>,
    has_service: bool,
) -> bool {
    parsed == "vfat"
        || parsed == "nfs"
        || has_service
            && mount_ops::served_by_default(
                parsed,
                fstab_flags(options),
                fstab_mount_data(options),
                ufs,
            )
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

#[cfg(test)]
#[path = "fstab_line_tests.rs"]
mod fstab_line_tests;
