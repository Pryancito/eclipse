//! Linux mount(2) / umount2(2) helpers.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

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
fn normalize_target(path: &str) -> String {
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
        return move_from(&root, &MOUNT_TABLE, source, &target_norm);
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
    let source_inode = proc.lookup_inode(source)?;
    let backend = MountBackend::from_inode(source_inode).map_err(|_| LxError::ENOTBLK)?;
    let read_only = flags_read_only(flags, data);
    let fs = open_filesystem(backend, fstype, read_only, data)?;
    let (fs, state) = prepare_fs(fs, flags, data);
    mount_node.mount(fs).map_err(LxError::from)?;
    let opts = build_options_string(flags, data);
    super::register_mount(source, &target_norm, fstype, &opts, Some(state));
    Ok(())
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
    umount_from(&root, &MOUNT_TABLE, target, flags)
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

    use super::{is_virtual_fstype, normalize_target, parse_fstype};
    use crate::error::LxError;

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
