//! Linux mount(2) / umount2(2) helpers.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use smoltcp::wire::IpAddress;

use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::{FileSystem, FileType, INode};
use rcore_fs_mountfs::MNode;

use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;

use super::block_mount::MountBackend;
use super::btrfs_mount::open_btrfs;
use super::eclipsefs_mount::open_eclipsefs;
use super::fat_mount::open_fat;
use super::flagged_fs::{wrap_fs, wrap_fs_at};
use super::nfs::connect::TcpConnector;
use super::nfs::greeting::Budget;
use super::nfs::mount::{connect_export, connect_export_with_deadline};
use super::nfs::source::{NfsOptions, NfsSource};
use crate::net::dns::{self, DnsFamily};

use super::mount_state::{
    self, build_options_string, flags_read_only, MNT_DETACH, MNT_FORCE, MS_BIND, MS_MOVE,
    MS_REMOUNT,
};
use super::{MountEntry, MOUNT_TABLE};

/// The mount table, the kernel's or one a test built.
type Table = Mutex<Vec<MountEntry>>;

lazy_static! {
    static ref VFS_ROOT: Mutex<Option<Arc<MNode>>> = Mutex::new(None);
}

/// Remember the VFS root after `create_root_fs`.
pub(crate) fn set_vfs_root(root: Arc<MNode>) {
    *VFS_ROOT.lock() = Some(root);
}

/// The VFS root remembered by `set_vfs_root`, if any.
pub(crate) fn vfs_root() -> Option<Arc<MNode>> {
    VFS_ROOT.lock().clone()
}

/// The one spelling of a mount point, used both to resolve it and to name it
/// in `/proc/mounts`.
///
/// It used to trim a trailing slash and nothing else, which left two spellings
/// of the same path that this kernel then treated as different paths:
///
/// - `"//"` came out as the EMPTY string, not `"/"`, so
///   `mount --move // /somewhere` walked straight past the `source == "/"`
///   guard and moved the ROOT mount, which is the one move Linux refuses.
/// - `"///mnt"` came out unchanged, because it does not end in a slash.
///   `resolve_mnode` skips the empty components and finds the right node, so
///   the mount succeeds and `/proc/mounts` records `///mnt` -- and a later
///   `umount /mnt` normalises to `/mnt`, matches nothing, and leaves the line
///   behind after the filesystem is gone.
///
/// It also trimmed whitespace. A directory may be called `" "`, and Linux
/// mounts on the path it was given, so trimming silently mounted somewhere
/// else. Splitting on the separator and rebuilding is all three at once.
pub(crate) fn normalize_target(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for comp in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(comp);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// The directory `target` shows: where a mount point is crossed, the root of
/// the top filesystem mounted there.
fn resolve_mnode(target: &str) -> LxResult<Arc<MNode>> {
    resolve_from(&vfs_root().ok_or(LxError::EINVAL)?, target)
}

/// [`resolve_mnode`] from the root it is given.
fn resolve_from(root: &Arc<MNode>, target: &str) -> LxResult<Arc<MNode>> {
    let mut cur = root.clone();
    for comp in normalize_target(target)
        .split('/')
        .filter(|s| !s.is_empty())
    {
        cur = cur.find(false, comp).map_err(LxError::from)?;
    }
    Ok(cur)
}

/// The directory the top filesystem mounted at `target` is mounted on, or
/// `None` where the kernel mounted none there.
///
/// [`resolve_from`] cannot answer it: a lookup crosses a mount point into
/// what is mounted on it, so what it lands on is the mounted root, which is
/// not a mount point itself. A bind, a move and an `umount` of anything the
/// kernel had really mounted were `EINVAL` because they asked that root. This
/// walks to the directory above, takes the last name without crossing, and
/// climbs the stack mounted there to the top.
fn mount_holder_from(root: &Arc<MNode>, target: &str) -> LxResult<Option<Arc<MNode>>> {
    let target = normalize_target(target);
    let (dir, name) = target.split_at(target.rfind('/').unwrap_or(0) + 1);
    if name.is_empty() {
        return Ok(None);
    }
    let parent = resolve_from(root, dir)?;
    let mut holder = parent.backing_find(name).map_err(LxError::from)?;
    if !holder.is_mountpoint() {
        return Ok(None);
    }
    while let Some(above) = holder.mounted_root().filter(|r| r.is_mountpoint()) {
        holder = above;
    }
    Ok(Some(holder))
}

pub(crate) fn parse_fstype(fstype: &str) -> LxResult<&'static str> {
    if fstype.is_empty() {
        return Err(LxError::EINVAL);
    }
    if fstype.eq_ignore_ascii_case("btrfs") {
        Ok("btrfs")
    } else if fstype.eq_ignore_ascii_case("eclipsefs") {
        Ok("eclipsefs")
    } else if fstype.eq_ignore_ascii_case("vfat")
        || fstype.eq_ignore_ascii_case("fat")
        || fstype.eq_ignore_ascii_case("fat32")
        || fstype.eq_ignore_ascii_case("msdos")
        || fstype.eq_ignore_ascii_case("fat16")
    {
        Ok("vfat")
    } else if fstype.eq_ignore_ascii_case("nfs") || fstype.eq_ignore_ascii_case("nfs3") {
        // One name for both: `nfs3` is not a filesystem type Linux
        // registers, but it is what someone writes when they mean "v3 and
        // not v4", and this client speaks only v3 — so it is the same mount
        // and `/proc/mounts` says `nfs`, as Linux's does for a v3 mount.
        Ok("nfs")
    } else {
        Err(LxError::ENODEV)
    }
}

/// Open `backend` as a filesystem of type `fstype`. `data` is mount(2)'s
/// option string; only EclipseFS reads options of its own from it.
pub(crate) fn open_filesystem(
    backend: MountBackend,
    fstype: &str,
    read_only: bool,
    data: &str,
) -> LxResult<Arc<dyn FileSystem>> {
    match fstype {
        "btrfs" => open_btrfs(&backend, read_only).map_err(LxError::from),
        "eclipsefs" => open_eclipsefs(&backend, read_only, data).map_err(LxError::from),
        "vfat" => open_fat(&backend)
            .map(|fs| fs as Arc<dyn FileSystem>)
            .map_err(LxError::from),
        _ => Err(LxError::ENODEV),
    }
}

/// Pseudo-filesystems the Eclipse kernel already provides (procfs at `/proc`,
/// sysfs at `/sys`) or that need no backing block device and live on the
/// kernel's writable `/dev`, `/run`, `/tmp` trees. They are not separately
/// mountable through the block-device path, but they ARE already present, so an
/// attempt to mount one is treated as a successful no-op rather than ENODEV.
const VIRTUAL_FSTYPES: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "ramfs",
    "cgroup",
    "cgroup2",
    "mqueue",
    "debugfs",
    "securityfs",
    "configfs",
    "tracefs",
    "fusectl",
];

/// True for a [`VIRTUAL_FSTYPES`] pseudo-filesystem (case-insensitive).
pub(crate) fn is_virtual_fstype(fstype: &str) -> bool {
    VIRTUAL_FSTYPES
        .iter()
        .any(|t| fstype.eq_ignore_ascii_case(t))
}

pub(crate) fn prepare_fs(
    fs: Arc<dyn FileSystem>,
    flags: usize,
    data: &str,
) -> (Arc<dyn FileSystem>, Arc<mount_state::MountState>) {
    let state = Arc::new(mount_state::MountState::from_options(flags, data));
    let wrapped = wrap_fs(fs, state.clone());
    (wrapped, state)
}

/// Mount a block device or loop image at `target`.
///
/// `source` is `None` when the caller passed no string at all, which a
/// remount ignores, a bind and a move refuse (`EINVAL`, as an empty one), a
/// pseudo-filesystem shows as `none` and a filesystem on a device refuses.
pub fn mount_fs(
    proc: &LinuxProcess,
    source: Option<&str>,
    target: &str,
    fstype: &str,
    flags: usize,
    data: &str,
) -> LxResult<()> {
    let target_norm = normalize_target(target);

    if flags & MS_REMOUNT != 0 {
        return super::remount_flags(&target_norm, flags, data);
    }

    // `do_move_mount_old` and `do_loopback`: what is moved or bound has to
    // be named. An empty name is refused further in, as the root, which is
    // neither movable nor a mount to bind.
    if flags & MS_MOVE != 0 {
        let source = source.ok_or(LxError::EINVAL)?;
        let root = vfs_root().ok_or(LxError::EINVAL)?;
        move_from(&root, &MOUNT_TABLE, source, &target_norm)?;
        hunter::media::note_move(&normalize_target(source), &target_norm);
        return Ok(());
    }

    if flags & MS_BIND != 0 {
        let source = source.ok_or(LxError::EINVAL)?;
        let root = vfs_root().ok_or(LxError::EINVAL)?;
        return bind_from(&root, &MOUNT_TABLE, source, &target_norm);
    }

    // Pseudo-filesystems the kernel already provides (procfs at /proc, sysfs at
    // /sys, the writable dev/run/tmp trees). A real mount here is unnecessary
    // and would shadow the live procfs, so acknowledge the request as a
    // successful no-op — and record it in /proc/mounts — instead of failing
    // with ENODEV. This is what lets init's `mount -t proc proc /proc`
    // (and the other pseudo-fs mounts) succeed quietly rather than logging
    // "mounting proc on /proc failed: No such device".
    if is_virtual_fstype(fstype) {
        let opts = build_options_string(flags, data);
        let source = source.unwrap_or("none");
        super::register_line(source, &target_norm, fstype, &opts);
        return Ok(());
    }

    let fstype = parse_fstype(fstype)?;
    let mount_node = resolve_mnode(&target_norm)?;
    let source = source.ok_or(LxError::EINVAL)?;

    // A network filesystem leaves the block path here, before the source is
    // looked up as a file. This is Linux's `FS_REQUIRES_DEV`: a type that
    // has the flag gets `mount_bdev` and its source opened as a device, and
    // one that does not gets `mount_nodev` and its source handed over as an
    // uninterpreted string. `nfs` is the second kind — `192.168.1.10:/srv`
    // is not a path and `lookup_inode` on it is `ENOENT` — so the branch has
    // to come before the lookup and not after it.
    if is_network_fstype(fstype) {
        return mount_nfs(source, &target_norm, &mount_node, flags, data);
    }

    let source_inode = proc.lookup_inode(source)?;
    let backend = MountBackend::from_inode(source_inode).map_err(|_| LxError::ENOTBLK)?;
    let read_only = flags_read_only(flags, data);
    let fs = open_filesystem(backend, fstype, read_only, data)?;
    let (fs, state) = prepare_fs(fs, flags, data);
    mount_node.mount(fs).map_err(LxError::from)?;
    let opts = build_options_string(flags, data);
    super::register_mount(source, &target_norm, fstype, &opts, Some(state));
    // A filesystem on a device, mounted after boot: a USB stick, a card, an
    // external disk, a loop image. hunter queues it for an antivirus scan
    // (and, under `media-lock on`, keeps its programs from running until the
    // scan is clean). The boot mounts from fstab do not come through here.
    hunter::media::note_mount(source, &target_norm, fstype);
    Ok(())
}

/// `mount -t nfs host:/export target`.
///
/// Everything network-specific is here and nothing of it leaks into the
/// block path: the device string is split, the options are parsed, the
/// server is dialled, and what comes back goes into the tree and the table
/// the same way a block filesystem does.
///
/// `hunter::media::note_mount` is deliberately not called. It queues a newly
/// mounted filesystem for an antivirus scan, which is right for a USB stick
/// and wrong for an export: the scan would read the whole of a remote
/// filesystem over the network at mount time, and the thing it guards
/// against — a stick carried in and plugged in — is not what an export is.
pub(crate) fn mount_nfs(
    source: &str,
    target: &str,
    mount_node: &Arc<MNode>,
    flags: usize,
    data: &str,
) -> LxResult<()> {
    mount_nfs_with_deadline(source, target, mount_node, flags, data, None)
}

/// [`mount_nfs`] with the deadline given rather than derived from the
/// options.
///
/// `None` is [`mount_nfs`]'s own behaviour: a fresh budget sized by
/// [`greeting_budget`](super::nfs::greeting::greeting_budget). `Some` is for
/// `/etc/fstab`, which shares one deadline across every network line in the
/// run — see [`connect_export_with_deadline`].
pub(crate) fn mount_nfs_with_deadline(
    source: &str,
    target: &str,
    mount_node: &Arc<MNode>,
    flags: usize,
    data: &str,
    budget: Option<&Budget>,
) -> LxResult<()> {
    let parsed = NfsSource::parse(source)?;
    let opts = nfs_options_for(flags, data)?;
    let addr = match opts.addr {
        Some(addr) => addr,
        // No resolver reachable from here: see `nfs_host_to_resolve`, which
        // is how the syscall layer resolves a name before this runs. A name
        // that gets this far is one nothing local knows.
        None => server_address(&parsed.host)?,
    };
    let mut connector = TcpConnector::new(addr);
    let mounted = match budget {
        Some(budget) => connect_export_with_deadline(&mut connector, &parsed, &opts, budget)?,
        None => connect_export(&mut connector, &parsed, &opts)?,
    };
    let (fs, state) = prepare_fs(mounted.fs, flags, data);
    mount_node.mount(fs).map_err(LxError::from)?;
    let line = build_options_string(flags, data);
    // The device column keeps `host:/export` as it was written, which is
    // what Linux records (`fc->source`) and what an `umount` names.
    super::register_mount(source, target, "nfs", &line, Some(state));
    Ok(())
}

/// Whether `fstype`, as [`parse_fstype`] spells it, is mounted over the
/// network rather than from a block device.
///
/// A function and not an `==` inside [`mount_fs`] so the two cannot drift:
/// what routes a mount to the network path is the *output* of
/// `parse_fstype`, and a branch written against the input (`nfs3`, say)
/// would send `mount -t nfs3` down the block path to have its `host:/export`
/// looked up as a file. That is a silent `ENOENT` on a mount that should
/// have worked, and it is the kind of mismatch only a test that reads both
/// ends catches.
pub(crate) fn is_network_fstype(fstype: &str) -> bool {
    fstype == "nfs"
}

/// The NFS options a `mount(2)` asked for, with the flags folded in.
///
/// `MS_RDONLY` and `ro` in the option string are one request made two ways,
/// and the flagged-fs wrapper would refuse the writes either way. Telling
/// the *session* too is what keeps a write from being sent to the server and
/// refused there instead of refused here -- and what keeps a read-only mount
/// from needing the server's cooperation to be read-only.
fn nfs_options_for(flags: usize, data: &str) -> LxResult<NfsOptions> {
    let mut opts = NfsOptions::parse(data)?;
    if flags_read_only(flags, data) {
        opts.read_only = true;
    }
    Ok(opts)
}

/// The server's address without asking the network: a literal, or a name
/// `/etc/hosts` knows.
///
/// `ENOENT` when it is neither, which is the errno [`dns::resolve`] answers
/// for a name that does not resolve: a name that reaches here has already
/// been through the syscall layer's resolver, or has come from a caller that
/// has none, and either way nothing local knows it.
fn server_address(host: &str) -> LxResult<IpAddress> {
    let root = super::dns_vfs_root().ok_or(LxError::ENOENT)?;
    dns::resolve_local(&root, host, DnsFamily::Unspec).ok_or(LxError::ENOENT)
}

/// The hostname a network mount names and cannot resolve by itself, or
/// `None` when there is nothing to resolve.
///
/// This exists because of where the two halves of the work can run.
/// `mount_fs` is synchronous and is called from places that have no
/// executor, while a DNS query is `async` — it waits for a reply, and the
/// synchronous version of that wait used to spin a CPU at 100%. So the
/// kernel does what Linux's userspace does: resolve the name outside the
/// mount and hand the mount an address, which is what `addr=` is for.
///
/// `None` covers every case the mount can finish on its own — not an NFS
/// mount, `addr=` already given, an address literal, a name in `/etc/hosts`
/// — so the syscall layer awaits a resolver only when there is genuinely no
/// other way, and a mount against a literal works on a machine that has no
/// resolver at all.
///
/// A device string or option string this cannot parse is `None` as well. It
/// is not this function's business to report it: `mount_fs` parses the same
/// strings and refuses them with the right errno, and a second opinion here
/// would only be a second place for the two to disagree.
pub fn nfs_host_to_resolve(fstype: &str, source: Option<&str>, data: &str) -> Option<String> {
    if parse_fstype(fstype).ok() != Some("nfs") {
        return None;
    }
    let parsed = NfsSource::parse(source?).ok()?;
    if NfsOptions::parse(data).ok()?.addr.is_some() {
        return None;
    }
    // The literal before `/etc/hosts`, and both before the VFS root is
    // asked for: a literal needs no files at all, so a mount against one
    // works whether or not there is a root to read `/etc/hosts` from. With
    // no root there is nothing local left to try, and the name goes back as
    // one to resolve -- the syscall layer needs the same root and reports
    // its absence there, rather than this quietly saying "nothing to do"
    // and leaving the mount to fail further in.
    if dns::parse_literal(&parsed.host).is_some() {
        return None;
    }
    if let Some(root) = super::dns_vfs_root() {
        if dns::resolve_local(&root, &parsed.host, DnsFamily::Unspec).is_some() {
            return None;
        }
    }
    Some(parsed.host)
}

/// The option string a mount should be given: `data` with `addr=<ip>` added
/// when the server has to be resolved, or `None` when `data` already says
/// everything.
///
/// The one place the resolving happens, shared by the two callers that mount
/// from an option string -- `mount(2)` and `/etc/fstab` -- so the two cannot
/// disagree about when a name is looked up. `async` only because of the
/// lookup: see [`nfs_host_to_resolve`], which answers `None` for every case
/// that needs no network, and that is every case but one.
pub async fn with_resolved_addr(
    fstype: &str,
    source: Option<&str>,
    data: &str,
) -> LxResult<Option<String>> {
    let Some(host) = nfs_host_to_resolve(fstype, source, data) else {
        return Ok(None);
    };
    let root = super::dns_vfs_root().ok_or(LxError::ENOENT)?;
    let addrs = dns::resolve(&root, &host, DnsFamily::Unspec).await?;
    Ok(Some(with_addr_option(data, first_address(&addrs)?)))
}

/// The address a resolver's answer names, or `ENOENT`.
///
/// A function because an empty answer is a real shape: `dns::resolve` can
/// succeed and return nothing — a reply that carried no address of the
/// family asked for, a `/etc/hosts` line with a name and no address. The
/// reading to avoid is treating "resolved to nothing" as resolved, because
/// what the mount then dials is whatever an empty list defaults to, and
/// `0.0.0.0` is a connection attempt to the local host rather than a
/// mount that says the name could not be found.
fn first_address(addrs: &[IpAddress]) -> LxResult<IpAddress> {
    addrs.first().copied().ok_or(LxError::ENOENT)
}

/// `data` with `addr=<ip>` added, which is how a resolved address reaches
/// [`mount_fs`].
///
/// Appended rather than inserted, and nothing already in the string is
/// removed: an `addr=` the caller wrote is theirs, and the parse takes the
/// last occurrence of a repeated option, so appending would override it. So
/// the one caller asks [`nfs_host_to_resolve`] first, which says `None` when
/// `addr=` is already there.
pub fn with_addr_option(data: &str, addr: IpAddress) -> String {
    if data.is_empty() {
        format!("addr={addr}")
    } else {
        format!("{data},addr={addr}")
    }
}

/// `mount --bind source target`, on the tree under `root` and the table
/// `table`. A mount on a mount point goes on top of what is there, which is
/// where [`resolve_from`] lands: the stack is how Linux takes it, not `EBUSY`.
///
/// The source is any directory, as Linux binds one: at a mount point the
/// root of the top filesystem there, anywhere else the directory itself.
/// Only a whole mount was bound before, and a plain directory was `EINVAL`.
/// Either way the bind shows what that filesystem has from there down and
/// nothing mounted inside it, which only `MS_REC` would bring.
///
/// The bind takes its flags and its options from the mount the source is
/// on, not from the call (`do_loopback` is handed no flags, and `clone_mnt`
/// copies the old mount's): a bind of a directory in a `nosuid,nodev,noexec`
/// mount is all three, and one of a read-only mount is read-only. They came
/// from the call before, so a bind of a directory in `/tmp` lost its
/// `nosuid` and `nodev`. Linux ignores the flags and the option string of a
/// plain bind, and makes a bind read-only with a remount after it.
fn bind_from(root: &Arc<MNode>, table: &Table, source: &str, target: &str) -> LxResult<()> {
    let mount_node = resolve_from(root, target)?;
    let source_node = resolve_from(root, source)?;
    if source_node.metadata()?.type_ != FileType::Dir {
        return Err(LxError::ENOTDIR);
    }
    // The filesystem under it is the source's, so the line names the device
    // and type of the mount the source is on, not the path it was reached by
    // and "none", and the directory of it the bind shows: Linux copies
    // `mnt_devname`, keeps the superblock and roots the mount at the dentry.
    // Asked before anything is mounted, and with the table let go again: a
    // mount takes the tree first and the table after it.
    let origin = super::origin_in(&table.lock(), source);
    let (origin, fstype, fs_root, opts) = origin.unwrap_or_else(|| {
        (
            String::from(source),
            String::from("none"),
            String::from("/"),
            String::from("rw"),
        )
    });
    // Its own copy: a remount of the bind changes the bind, not its source.
    let state = Arc::new(mount_state::MountState::from_options(0, &opts));
    let fs = wrap_fs_at(
        source_node.backing_fs(),
        source_node.backing_inode(),
        state.clone(),
    );
    mount_node.mount(fs).map_err(LxError::from)?;
    let entry = MountEntry {
        root: fs_root,
        ..super::new_mount_entry(&origin, target, &fstype, &opts, Some(state))
    };
    super::record_mount(&mut table.lock(), entry);
    Ok(())
}

/// `mount --move source target`, on the tree under `root` and the table
/// `table`.
fn move_from(root: &Arc<MNode>, table: &Table, source: &str, target: &str) -> LxResult<()> {
    let source_norm = normalize_target(source);
    let target_norm = normalize_target(target);
    if source_norm == "/" {
        return Err(LxError::EINVAL);
    }
    if source_norm == target_norm {
        return Ok(());
    }
    if target_norm.starts_with(&alloc::format!("{}/", source_norm)) {
        return Err(LxError::EINVAL);
    }
    resolve_from(root, &source_norm)?;
    let backed = super::top_backed(&table.lock(), &source_norm).ok_or(LxError::EINVAL)?;
    let target_node = resolve_from(root, &target_norm)?;
    // A line with nothing mounted under it moves as a line.
    if backed {
        let holder = mount_holder_from(root, &source_norm)?.ok_or(LxError::EINVAL)?;
        move_mount_between(&holder, &target_node)?;
    }
    super::move_entry(&mut table.lock(), &source_norm, &target_norm)
}

/// Move the filesystem mounted at `source_node`, with what is mounted inside
/// it, onto `target_node`, leaving it where it was if the destination refuses
/// it.
///
/// A move that fails has to leave the mount alone. Detaching first and finding
/// out afterwards left the filesystem mounted **nowhere** -- unreachable from
/// any path, with `/proc/mounts` still naming the old one, and the caller told
/// only that the move had failed. The checks in [`move_from`] make a refusal
/// rare, not impossible: `mount` also refuses a poisoned node and anything
/// that is not a directory.
fn move_mount_between(source_node: &Arc<MNode>, target_node: &Arc<MNode>) -> LxResult<()> {
    source_node.move_mount(target_node).map_err(LxError::from)
}

/// Unmount a filesystem mounted at `target`.
pub fn umount_fs(target: &str, flags: usize) -> LxResult<()> {
    // The word first, before anything is unmounted: an option this kernel does
    // not know must come back as EINVAL, which is how a program finds out --
    // `umount -l` succeeding while unmounting eagerly is worse than a refusal,
    // because the caller believes the mount was detached and it was not.
    mount_state::check_umount_flags(flags)?;
    let root = vfs_root().ok_or(LxError::EINVAL)?;
    umount_from(&root, &MOUNT_TABLE, target, flags)?;
    hunter::media::note_umount(&normalize_target(target));
    Ok(())
}

/// `umount2` of `target` with `flags`, on the tree under `root` and the table
/// `table`.
///
/// `MNT_DETACH` takes whatever is mounted on it too. `MNT_FORCE` unmounts a
/// filesystem even when what it wrote cannot reach the device: without it
/// that is `EIO` and the mount stays, where Linux unmounts and loses the
/// writes. A disk that went away can still be taken down with `umount -f`.
fn umount_from(root: &Arc<MNode>, table: &Table, target: &str, flags: usize) -> LxResult<()> {
    let target = normalize_target(target);
    resolve_from(root, &target)?;
    let plan = super::unmount_in(&table.lock(), &target, flags & MNT_DETACH != 0)?;
    if plan.backed {
        // Whatever is mounted inside it goes with it, as a lazy unmount
        // takes it on Linux. What they wrote reaches the device first, and if
        // it cannot, nothing goes, not the mount and not its line, unless
        // `MNT_FORCE` asked for it to go anyway.
        if let Some(holder) = mount_holder_from(root, &target)? {
            if flags & MNT_FORCE != 0 {
                holder.force_umount()
            } else {
                holder.umount()
            }
            .map_err(LxError::from)?;
        }
    }
    table.lock().retain(|m| !plan.ids.contains(&m.id));
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The three pure decisions `mount(2)` and `umount2(2)` take before they
    //! touch the tree: what path was asked for, what filesystem was asked for,
    //! and whether this kernel already provides it. Everything past them needs
    //! a block device.

    use super::mount_state::MS_RDONLY;
    use super::{
        is_network_fstype, is_virtual_fstype, nfs_host_to_resolve, nfs_options_for,
        normalize_target, parse_fstype, with_addr_option,
    };
    use crate::error::LxError;
    use alloc::string::String;
    use smoltcp::wire::{IpAddress, Ipv4Address};

    #[test]
    fn a_path_spelled_with_extra_slashes_is_one_path() {
        assert_eq!(normalize_target("/mnt"), "/mnt");
        assert_eq!(normalize_target("/mnt/"), "/mnt");
        assert_eq!(normalize_target("/mnt//"), "/mnt");
        // The spelling that used to come through untouched, because it does
        // not END in a slash: mounted as `///mnt`, recorded in /proc/mounts as
        // `///mnt`, and never matched again by a `umount /mnt`.
        assert_eq!(normalize_target("///mnt"), "/mnt");
        assert_eq!(normalize_target("/a//b///c/"), "/a/b/c");
    }

    #[test]
    fn the_root_is_the_root_however_many_slashes_it_is_spelled_with() {
        assert_eq!(normalize_target("/"), "/");
        assert_eq!(normalize_target(""), "/");
        // `"//"` used to come out EMPTY, and the empty string is not `"/"`:
        // `mount --move // /elsewhere` walked past the guard that exists to
        // refuse moving the root mount.
        assert_eq!(normalize_target("//"), "/");
        assert_eq!(normalize_target("/////"), "/");
    }

    #[test]
    fn a_directory_whose_name_is_spaces_is_not_trimmed_away() {
        // A mount point may be called `" "`, and Linux mounts on the path it
        // was handed. Trimming mounted somewhere else and said nothing.
        assert_eq!(normalize_target("/ "), "/ ");
        assert_eq!(normalize_target("/a /b"), "/a /b");
        assert_eq!(normalize_target(" "), "/ ");
    }

    #[test]
    fn the_filesystem_names_userland_uses_all_reach_the_same_driver() {
        assert_eq!(parse_fstype("btrfs"), Ok("btrfs"));
        assert_eq!(parse_fstype("BTRFS"), Ok("btrfs"));
        assert_eq!(parse_fstype("eclipsefs"), Ok("eclipsefs"));
        assert_eq!(parse_fstype("EclipseFS"), Ok("eclipsefs"));
        // `mount -t vfat`, `-t msdos` and what a fstab or a udisks helper
        // writes: five spellings, one driver.
        for name in ["vfat", "fat", "fat32", "fat16", "msdos", "MSDOS", "FAT32"] {
            assert_eq!(parse_fstype(name), Ok("vfat"), "{}", name);
        }
    }

    #[test]
    fn a_filesystem_this_kernel_has_no_driver_for_is_told_apart_from_none_at_all() {
        // `mount` with no `-t` is not the same complaint as `-t ext4`, and a
        // program reading errno acts on the difference.
        assert_eq!(parse_fstype(""), Err(LxError::EINVAL));
        assert_eq!(parse_fstype("ext4"), Err(LxError::ENODEV));
        assert_eq!(parse_fstype("xfs"), Err(LxError::ENODEV));
    }

    #[test]
    fn the_pseudo_filesystems_the_kernel_already_provides_are_recognised() {
        for name in ["proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup2"] {
            assert!(is_virtual_fstype(name), "{}", name);
        }
        assert!(is_virtual_fstype("PROC"));
        assert!(!is_virtual_fstype("btrfs"));
        assert!(!is_virtual_fstype("vfat"));
        assert!(!is_virtual_fstype(""));
    }

    /// `nfs` is a type this kernel mounts now, and `nfs3` is the same
    /// mount: this client speaks only v3, and `nfs3` is what someone writes
    /// when they mean to say so.
    #[test]
    fn nfs_and_nfs3_are_one_type_and_are_spelled_nfs_in_the_table() {
        assert_eq!(parse_fstype("nfs"), Ok("nfs"));
        assert_eq!(parse_fstype("nfs3"), Ok("nfs"));
        assert_eq!(parse_fstype("NFS"), Ok("nfs"));
        // Still not this client's: v4 is a different protocol, not a
        // version of this one, and accepting the name would mount it with a
        // v3 client.
        assert_eq!(parse_fstype("nfs4"), Err(LxError::ENODEV));
    }

    /// A literal server needs no resolver, which is the whole reason the
    /// literal is checked before `/etc/hosts` and before the VFS root.
    #[test]
    fn an_address_literal_is_not_a_name_to_resolve() {
        assert_eq!(
            nfs_host_to_resolve("nfs", Some("192.168.1.10:/srv/media"), ""),
            None
        );
        assert_eq!(nfs_host_to_resolve("nfs", Some("[fe80::1]:/srv"), ""), None);
    }

    /// `addr=` means "do not resolve", so a name beside it is a name the
    /// caller has already resolved and does not want looked up again.
    #[test]
    fn an_addr_option_takes_the_place_of_resolving_the_name() {
        assert_eq!(
            nfs_host_to_resolve("nfs", Some("nas:/srv"), "addr=10.0.0.3"),
            None
        );
        assert_eq!(
            nfs_host_to_resolve("nfs", Some("nas:/srv"), ""),
            Some(String::from("nas")),
            "with no addr= and nothing local, the name goes back to be \
             resolved"
        );
    }

    /// Only an NFS mount has a server to resolve, and the question is asked
    /// of every mount.
    #[test]
    fn nothing_is_resolved_for_a_mount_that_is_not_nfs() {
        // The device string here is a perfectly good `host:/export`, which
        // is the point: a test that passed `/dev/sda1` would pass however
        // the type was checked, because the device string alone would stop
        // it. What has to be measured is that the **type** is what decides.
        assert_eq!(nfs_host_to_resolve("vfat", Some("nas:/srv"), ""), None);
        assert_eq!(nfs_host_to_resolve("btrfs", Some("nas:/srv"), ""), None);
        assert_eq!(nfs_host_to_resolve("tmpfs", Some("nas:/srv"), ""), None);
        assert_eq!(nfs_host_to_resolve("", Some("nas:/srv"), ""), None);
        assert_eq!(nfs_host_to_resolve("nfs", None, ""), None);
        assert_eq!(
            nfs_host_to_resolve("nfs3", Some("nas:/srv"), ""),
            Some(String::from("nas")),
            "the other spelling of the same type is resolved too"
        );
    }

    /// Every mount that needs no lookup is handed its options back
    /// untouched, and `with_resolved_addr` and `nfs_host_to_resolve` agree
    /// on which those are.
    ///
    /// The agreement is the thing worth measuring. `with_resolved_addr` is
    /// what both `mount(2)` and `/etc/fstab` call, and it `await`s only on
    /// the cases `nfs_host_to_resolve` picks out; if it awaited on a case
    /// that one calls settled, boot would make a DNS query for every `vfat`
    /// line in the fstab, before the network is necessarily up.
    #[test]
    fn a_mount_with_nothing_to_resolve_awaits_nothing_and_changes_nothing() {
        use async_std::task::block_on;
        for (fstype, source, data) in [
            ("vfat", Some("nas:/srv"), "rw"),
            ("btrfs", Some("nas:/srv"), "rw"),
            ("tmpfs", Some("nas:/srv"), ""),
            ("nfs", None, ""),
            ("nfs", Some("nas:/srv"), "addr=10.0.0.4"),
            ("nfs", Some("192.168.1.10:/srv"), ""),
        ] {
            assert_eq!(
                nfs_host_to_resolve(fstype, source, data),
                None,
                "{:?} {:?} {:?}: this case is meant to need no lookup",
                fstype,
                source,
                data
            );
            assert_eq!(
                block_on(super::with_resolved_addr(fstype, source, data)),
                Ok(None),
                "{:?} {:?} {:?}: nothing to resolve must be no new options, \
                 not options rebuilt the same",
                fstype,
                source,
                data
            );
        }
    }

    /// A resolver that succeeded and named nothing has not resolved the
    /// name.
    ///
    /// `dns::resolve` can do exactly that: a reply carrying no address of
    /// the family asked for, or an `/etc/hosts` line with a name and no
    /// address. Treating it as resolved hands the mount whatever an empty
    /// list falls back to, and every candidate for that is a connection to
    /// somewhere other than the server — `0.0.0.0` is the local host.
    #[test]
    fn a_resolver_that_names_nothing_has_not_resolved_the_name() {
        assert_eq!(super::first_address(&[]), Err(LxError::ENOENT));
        assert_eq!(
            super::first_address(&[IpAddress::v4(10, 0, 0, 4)]),
            Ok(IpAddress::v4(10, 0, 0, 4))
        );
        assert_eq!(
            super::first_address(&[IpAddress::v4(10, 0, 0, 4), IpAddress::v4(10, 0, 0, 5)]),
            Ok(IpAddress::v4(10, 0, 0, 4)),
            "the first the resolver named, as Linux takes it"
        );
    }

    /// What routes a mount to the network path and what `parse_fstype`
    /// answers have to be the same string, in both directions.
    #[test]
    fn every_spelling_of_nfs_reaches_the_network_path_and_nothing_else_does() {
        for name in ["nfs", "nfs3", "NFS"] {
            let parsed = parse_fstype(name).expect(name);
            assert!(is_network_fstype(parsed), "{} -> {}", name, parsed);
        }
        for name in ["btrfs", "eclipsefs", "vfat", "msdos"] {
            let parsed = parse_fstype(name).expect(name);
            assert!(!is_network_fstype(parsed), "{} -> {}", name, parsed);
        }
        // Not the *input* spelling: the branch reads what `parse_fstype`
        // produced, and `nfs3` is not that.
        assert!(!is_network_fstype("nfs3"));
    }

    /// A read-only mount is read-only in the session as well, so a write is
    /// refused here rather than sent to the server to be refused there.
    #[test]
    fn a_read_only_mount_is_read_only_for_the_session_too() {
        assert!(nfs_options_for(MS_RDONLY, "").unwrap().read_only);
        assert!(nfs_options_for(0, "ro").unwrap().read_only);
        assert!(!nfs_options_for(0, "").unwrap().read_only);
        assert!(!nfs_options_for(0, "rw").unwrap().read_only);
        // The option string is still parsed, and a bad value in it is still
        // a refused mount.
        assert_eq!(nfs_options_for(0, "rsize=0"), Err(LxError::EINVAL));
    }

    /// A device string this cannot parse is not reported here: `mount_fs`
    /// parses the same string and refuses it with the right errno, and a
    /// second opinion would only be a second place for the two to disagree.
    #[test]
    fn a_device_string_that_is_not_host_colon_export_is_left_to_mount_fs() {
        assert_eq!(nfs_host_to_resolve("nfs", Some("nas"), ""), None);
        assert_eq!(
            nfs_host_to_resolve("nfs", Some("nas:/srv"), "port=abc"),
            None
        );
    }

    /// The resolved address reaches `mount_fs` through the option string,
    /// which is also what puts it in `/proc/mounts` -- as Linux's NFS mounts
    /// show it.
    #[test]
    fn a_resolved_address_is_appended_to_the_options_it_was_asked_about() {
        let addr = IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 3));
        assert_eq!(with_addr_option("", addr), "addr=10.0.0.3");
        assert_eq!(
            with_addr_option("rw,vers=3", addr),
            "rw,vers=3,addr=10.0.0.3",
            "appended, and nothing already there is dropped"
        );
    }
}

#[cfg(test)]
mod move_tests {
    //! `mount --move`, on a tree built here rather than through `VFS_ROOT`:
    //! that root is process-wide and set once at boot, so a test that pointed
    //! it somewhere would point it for every other test in the binary.

    use super::move_mount_between;
    use alloc::sync::Arc;
    use rcore_fs::vfs::{FileSystem, FileType};
    use rcore_fs_mountfs::MountFS;
    use rcore_fs_ramfs::RamFS;

    #[test]
    fn a_move_that_the_destination_refuses_leaves_the_mount_where_it_was() {
        let vfs = MountFS::new(RamFS::new());
        let root = vfs.mountpoint_root_inode();
        let from = root.create("from", FileType::Dir, 0o755).unwrap();
        // A file is not a mount point and never can be, so this stands in for
        // every reason `mount` can refuse a destination.
        let onto = root.create("onto", FileType::File, 0o644).unwrap();

        from.mount(RamFS::new()).unwrap();
        assert!(from.is_mountpoint());

        assert!(move_mount_between(&from, &onto).is_err());
        assert!(
            from.is_mountpoint(),
            "a refused move left the filesystem mounted nowhere"
        );
        assert!(!onto.is_mountpoint());
    }

    #[test]
    fn a_move_the_destination_accepts_takes_the_mount_with_it() {
        let vfs = MountFS::new(RamFS::new());
        let root = vfs.mountpoint_root_inode();
        let from = root.create("from", FileType::Dir, 0o755).unwrap();
        let onto = root.create("onto", FileType::Dir, 0o755).unwrap();

        let inner = RamFS::new();
        from.mount(inner.clone()).unwrap();
        move_mount_between(&from, &onto).unwrap();
        assert!(!from.is_mountpoint());
        assert!(onto.is_mountpoint());
        assert!(Arc::ptr_eq(
            &onto.mounted_inner_fs().unwrap(),
            &(inner as Arc<dyn FileSystem>)
        ));
    }

    #[test]
    fn moving_something_that_is_not_a_mount_point_changes_nothing() {
        let vfs = MountFS::new(RamFS::new());
        let root = vfs.mountpoint_root_inode();
        let from = root.create("from", FileType::Dir, 0o755).unwrap();
        let onto = root.create("onto", FileType::Dir, 0o755).unwrap();
        assert!(move_mount_between(&from, &onto).is_err());
        assert!(!from.is_mountpoint());
        assert!(!onto.is_mountpoint());
    }
}

#[cfg(test)]
mod mount_tree_tests;
