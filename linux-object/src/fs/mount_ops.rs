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
use super::ext2_mount::open_ext2;
use super::fat_mount::open_fat;
use super::flagged_fs::wrap_fs;
use super::mount_state::{
    self, build_options_string, flags_read_only, MS_BIND, MS_MOVE, MS_REMOUNT, MNT_DETACH,
    MNT_FORCE,
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
    } else if fstype.eq_ignore_ascii_case("ext2")
        || fstype.eq_ignore_ascii_case("ext3")
        || fstype.eq_ignore_ascii_case("ext4")
    {
        Ok("ext2")
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

pub(crate) fn open_filesystem(backend: MountBackend, fstype: &str) -> LxResult<Arc<dyn FileSystem>> {
    match fstype {
        "btrfs" => open_btrfs(&backend).map_err(LxError::from),
        "ext2" => open_ext2(&backend).map_err(LxError::from),
        "vfat" => open_fat(backend)
            .map(|fs| fs as Arc<dyn FileSystem>)
            .map_err(LxError::from),
        _ => Err(LxError::ENODEV),
    }
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
        let inner = source_node
            .mounted_inner_fs()
            .ok_or(LxError::EINVAL)?;
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

    let fstype = parse_fstype(fstype)?;
    let mount_node = resolve_mnode(&target_norm)?;
    if mount_node.is_mountpoint() {
        return Err(LxError::EBUSY);
    }
    let source_inode = proc.lookup_inode(source)?;
    let backend = MountBackend::from_inode(source_inode).map_err(|_| LxError::ENOTBLK)?;
    let fs = open_filesystem(backend, fstype)?;
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
    let fs = source_node
        .mounted_inner_fs()
        .ok_or(LxError::EINVAL)?;
    source_node.umount().map_err(LxError::from)?;
    target_node.mount(fs).map_err(LxError::from)?;
    super::move_mount_entry(&source_norm, &target_norm)?;
    Ok(())
}

/// Unmount a filesystem mounted at `target`.
pub fn umount_fs(target: &str, flags: usize) -> LxResult<()> {
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
