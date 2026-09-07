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
    MS_PROPAGATION, MS_REMOUNT,
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

fn normalize_target(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        return String::from("/");
    }
    if path.ends_with('/') {
        String::from(path.trim_end_matches('/'))
    } else {
        String::from(path)
    }
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
        "vfat" => open_fat(backend)
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
    let state = Arc::new(mount_state::MountState::new(flags_read_only(flags, data)));
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

    // Propagation-only (bwrap: mount(NULL, "/", NULL, MS_REC|MS_PRIVATE)).
    if flags & MS_PROPAGATION != 0 && flags & MS_BIND == 0 && flags & MS_MOVE == 0 {
        return Ok(());
    }

    if flags & MS_REMOUNT != 0 {
        return super::remount_flags(&target_norm, flags, data);
    }

    if flags & MS_MOVE != 0 {
        return mount_move(source, &target_norm);
    }

    let isolated = !proc.ns().mount().is_init();
    let overlay_key = crate::ns::join_chroot_prefix(&proc.chroot_prefix_text(), &target_norm);

    if flags & MS_BIND != 0 {
        if isolated {
            let src_inode = proc.lookup_inode(if source.is_empty() { target } else { source })?;
            proc.ns().mount().bind(&overlay_key, src_inode)?;
            let opts = build_options_string(flags, data);
            super::register_mount(
                source,
                &target_norm,
                "none",
                &opts,
                Arc::new(mount_state::MountState::new(flags_read_only(flags, data))),
            );
            return Ok(());
        }
        let source_node = resolve_mnode(source)?;
        if !source_node.is_mountpoint() {
            // Directory/file bind on the host ns: overlay so we don't require
            // the source to already be a mountpoint (what bwrap needs).
            let src_inode = proc.lookup_inode(if source.is_empty() { target } else { source })?;
            proc.ns().mount().bind(&overlay_key, src_inode)?;
            let opts = build_options_string(flags, data);
            super::register_mount(
                source,
                &target_norm,
                "none",
                &opts,
                Arc::new(mount_state::MountState::new(flags_read_only(flags, data))),
            );
            return Ok(());
        }
        let inner = source_node.mounted_inner_fs().ok_or(LxError::EINVAL)?;
        let mount_node = resolve_mnode(&target_norm)?;
        if mount_node.is_mountpoint() {
            return Err(LxError::EBUSY);
        }
        let (fs, state) = prepare_fs(inner, flags, data);
        mount_node.mount(fs).map_err(LxError::from)?;
        let opts = build_options_string(flags, data);
        super::register_mount(source, &target_norm, "none", &opts, state);
        return Ok(());
    }

    // Pseudo-filesystems. In an isolated mount ns, tmpfs/ramfs become a real
    // ramfs overlay (the sandbox root). On the host ns they stay the
    // historical no-op so init's `mount -t proc /proc` does not shadow live
    // procfs / EBUSY an already-mounted /tmp.
    if is_virtual_fstype(fstype) {
        if isolated
            && (fstype.eq_ignore_ascii_case("tmpfs") || fstype.eq_ignore_ascii_case("ramfs"))
        {
            proc.ns().mount().mount_tmpfs(&overlay_key)?;
        }
        let opts = build_options_string(flags, data);
        let state = Arc::new(mount_state::MountState::new(flags_read_only(flags, data)));
        super::register_mount(source, &target_norm, fstype, &opts, state);
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
    super::register_mount(source, &target_norm, fstype, &opts, state);
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
    let fs = source_node.mounted_inner_fs().ok_or(LxError::EINVAL)?;
    source_node.umount().map_err(LxError::from)?;
    target_node.mount(fs).map_err(LxError::from)?;
    super::move_mount_entry(&source_norm, &target_norm)?;
    Ok(())
}

/// Unmount a filesystem mounted at `target`.
pub fn umount_fs(proc: &LinuxProcess, target: &str, flags: usize) -> LxResult<()> {
    let _ = flags & (MNT_FORCE | MNT_DETACH);
    let target_norm = normalize_target(target);
    if !proc.ns().mount().is_init() {
        let key = crate::ns::join_chroot_prefix(&proc.chroot_prefix_text(), &target_norm);
        if proc.ns().mount().umount(&key).is_ok() {
            super::unregister_mount(&target_norm);
            return Ok(());
        }
    }
    let mount_node = resolve_mnode(&target_norm)?;
    if !mount_node.is_mountpoint() {
        return Err(LxError::EINVAL);
    }
    if let Some(fs) = mount_node.mounted_inner_fs() {
        let _ = fs.sync();
    }
    mount_node.umount().map_err(LxError::from)?;
    super::unregister_mount(&target_norm);
    Ok(())
}
