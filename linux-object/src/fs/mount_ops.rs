//! Linux mount(2) / umount2(2) helpers.

use alloc::string::String;
use alloc::sync::Arc;

use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::FileSystem;
use rcore_fs_mountfs::MNode;

use crate::error::{LxError, LxResult};
use crate::process::LinuxProcess;

use super::block_mount::MountBackend;
use super::btrfs_mount::open_btrfs;
use super::fat_mount::open_fat;
use super::flagged_fs::wrap_fs;
use super::mount_state::{
    self, build_options_string, flags_read_only, MNT_DETACH, MNT_FORCE, MS_BIND, MS_MOVE,
    MS_REMOUNT,
};

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

fn resolve_mnode(target: &str) -> LxResult<Arc<MNode>> {
    let root = VFS_ROOT.lock().clone().ok_or(LxError::EINVAL)?;
    let target = normalize_target(target);
    if target == "/" {
        return Ok(root);
    }
    let mut cur = root;
    for comp in target.split('/').filter(|s| !s.is_empty()) {
        cur = cur.find(false, comp).map_err(LxError::from)?;
    }
    Ok(cur)
}

pub(crate) fn parse_fstype(fstype: &str) -> LxResult<&'static str> {
    if fstype.is_empty() {
        return Err(LxError::EINVAL);
    }
    if fstype.eq_ignore_ascii_case("btrfs") {
        Ok("btrfs")
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

pub(crate) fn open_filesystem(
    backend: MountBackend,
    fstype: &str,
    read_only: bool,
) -> LxResult<Arc<dyn FileSystem>> {
    match fstype {
        "btrfs" => open_btrfs(&backend, read_only).map_err(LxError::from),
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
pub fn mount_fs(
    proc: &LinuxProcess,
    source: &str,
    target: &str,
    fstype: &str,
    flags: usize,
    data: &str,
) -> LxResult<()> {
    let target_norm = normalize_target(target);

    if flags & MS_REMOUNT != 0 {
        return super::remount_flags(&target_norm, flags, data);
    }

    if flags & MS_MOVE != 0 {
        return mount_move(source, &target_norm);
    }

    if flags & MS_BIND != 0 {
        let source_node = resolve_mnode(source)?;
        if !source_node.is_mountpoint() {
            return Err(LxError::EINVAL);
        }
        let inner = source_node.mounted_inner_fs().ok_or(LxError::EINVAL)?;
        let mount_node = resolve_mnode(&target_norm)?;
        if mount_node.is_mountpoint() {
            return Err(LxError::EBUSY);
        }
        let (fs, state) = prepare_fs(inner, flags, data);
        mount_node.mount(fs).map_err(LxError::from)?;
        let opts = build_options_string(flags, data);
        super::register_mount(source, &target_norm, "none", &opts, Some(state));
        return Ok(());
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
        super::register_mount(source, &target_norm, fstype, &opts, None);
        return Ok(());
    }

    let fstype = parse_fstype(fstype)?;
    let mount_node = resolve_mnode(&target_norm)?;
    if mount_node.is_mountpoint() {
        return Err(LxError::EBUSY);
    }
    let source_inode = proc.lookup_inode(source)?;
    let backend = MountBackend::from_inode(source_inode).map_err(|_| LxError::ENOTBLK)?;
    let read_only = flags_read_only(flags, data);
    let fs = open_filesystem(backend, fstype, read_only)?;
    let (fs, state) = prepare_fs(fs, flags, data);
    mount_node.mount(fs).map_err(LxError::from)?;
    let opts = build_options_string(flags, data);
    super::register_mount(source, &target_norm, fstype, &opts, Some(state));
    Ok(())
}

fn mount_move(source: &str, target: &str) -> LxResult<()> {
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
    let source_node = resolve_mnode(&source_norm)?;
    if !source_node.is_mountpoint() {
        return Err(LxError::EINVAL);
    }
    let target_node = resolve_mnode(&target_norm)?;
    if target_node.is_mountpoint() {
        return Err(LxError::EBUSY);
    }
    move_mount_between(&source_node, &target_node)?;
    super::move_mount_entry(&source_norm, &target_norm)?;
    Ok(())
}

/// Detach the filesystem mounted at `source_node` and attach it at
/// `target_node`, leaving it where it was if the destination refuses it.
///
/// A move that fails has to leave the mount alone. Detaching first and finding
/// out afterwards left the filesystem mounted **nowhere** -- unreachable from
/// any path, with `/proc/mounts` still naming the old one, and the caller told
/// only that the move had failed. The checks in [`mount_move`] make a refusal
/// rare, not impossible: `mount` also refuses a poisoned node and anything
/// that is not a directory.
fn move_mount_between(source_node: &Arc<MNode>, target_node: &Arc<MNode>) -> LxResult<()> {
    let fs = source_node.mounted_inner_fs().ok_or(LxError::EINVAL)?;
    source_node.umount().map_err(LxError::from)?;
    if let Err(e) = target_node.mount(fs.clone()) {
        let _ = source_node.mount(fs);
        return Err(LxError::from(e));
    }
    Ok(())
}

/// Unmount a filesystem mounted at `target`.
pub fn umount_fs(target: &str, flags: usize) -> LxResult<()> {
    // The word first, before anything is unmounted: an option this kernel does
    // not know must come back as EINVAL, which is how a program finds out --
    // `umount -l` succeeding while unmounting eagerly is worse than a refusal,
    // because the caller believes the mount was detached and it was not.
    mount_state::check_umount_flags(flags)?;
    // MNT_FORCE and MNT_DETACH are accepted and then not acted on: an unmount
    // here never reports EBUSY, so there is nothing for either to change.
    let _ = flags & (MNT_FORCE | MNT_DETACH);
    let target_norm = normalize_target(target);
    let mount_node = resolve_mnode(&target_norm)?;
    if !mount_node.is_mountpoint() {
        return Err(LxError::EINVAL);
    }
    mount_node.umount().map_err(LxError::from)?;
    super::unregister_mount(&target_norm);
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
