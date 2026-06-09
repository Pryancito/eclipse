//! Linux file objects

mod block_mount;
mod devfs;
mod ext2_editor;
mod ext2_mount;
mod fat_mount;
mod flagged_fs;
mod file;
pub mod ioctl;
mod mount_ops;
mod mount_state;
mod pipe;
mod procfs;
mod proc_self;
mod pseudo;
mod sysfs;
mod epoll;
mod eventfd;
pub mod rcore_fs_wrapper;
pub mod stdio;

#[cfg(feature = "mock-disk")]
pub mod mock;

#[cfg(feature = "mock-disk")]
/// Start simulating the disk
pub fn mocking_block(initrd: &'static mut [u8]) -> ! {
    mock::mocking(initrd)
}

#[cfg(feature = "mock-disk")]
/// Drivers for the mock disk
pub fn mock_block() -> mock::MockBlock {
    mock::MockBlock::new()
}

use alloc::{boxed::Box, fmt::Write as _, string::String, string::ToString, sync::Arc, vec::Vec};
use core::convert::TryFrom;

use async_trait::async_trait;
use lazy_static::lazy_static;
use lock::Mutex;

use kernel_hal::drivers;
use rcore_fs::vfs::{FileSystem, FileType, INode, Result};
use rcore_fs_devfs::{
    special::{NullINode, ZeroINode},
    DevFS, DevINode,
};
use rcore_fs_mountfs::{MountFS, MNode};
use rcore_fs_ramfs::RamFS;

lazy_static! {
    pub(crate) static ref DEVFS_ROOT: Mutex<Option<Arc<DevINode>>> = Mutex::new(None);
}
use zircon_object::{object::KernelObject, vm::VmObject};

use crate::error::{LxError, LxResult};
use crate::net::Socket;
use crate::process::LinuxProcess;
use devfs::RandomINode;
use procfs::ProcFS;
use pseudo::Pseudo;
use sysfs::SysFS;

pub use file::{File, OpenFlags, PollEvents, SeekFrom};
pub use pipe::Pipe;
pub use epoll::{Epoll, EpollEvent};
pub use eventfd::EventFd;
pub use rcore_fs::vfs::{self, PollStatus};
pub use stdio::{STDIN, STDOUT};

#[derive(Clone)]
struct MountEntry {
    source: String,
    target: String,
    fstype: String,
    options: String,
    state: Arc<mount_state::MountState>,
}

lazy_static! {
    static ref MOUNT_TABLE: Mutex<Vec<MountEntry>> = Mutex::new(Vec::new());
}

fn reset_mount_table() {
    MOUNT_TABLE.lock().clear();
}

fn boot_mount_state() -> Arc<mount_state::MountState> {
    Arc::new(mount_state::MountState::new(false))
}

pub(crate) fn register_mount(
    source: &str,
    target: &str,
    fstype: &str,
    options: &str,
    state: Arc<mount_state::MountState>,
) {
    MOUNT_TABLE.lock().push(MountEntry {
        source: source.to_string(),
        target: target.to_string(),
        fstype: fstype.to_string(),
        options: options.to_string(),
        state,
    });
}

pub(crate) fn unregister_mount(target: &str) {
    MOUNT_TABLE.lock().retain(|m| m.target != target);
}

pub(crate) fn remount_flags(target: &str, flags: usize, data: &str) -> LxResult<()> {
    let target = normalize_mount_target(target);
    let mut mounts = MOUNT_TABLE.lock();
    let entry = mounts
        .iter_mut()
        .find(|m| m.target == target)
        .ok_or(LxError::EINVAL)?;
    let ro = mount_state::flags_read_only(flags, data);
    entry.state.set_read_only(ro);
    entry.options = mount_state::build_options_string(flags, data);
    Ok(())
}

pub(crate) fn move_mount_entry(old_target: &str, new_target: &str) -> LxResult<()> {
    let old_target = normalize_mount_target(old_target);
    let new_target = normalize_mount_target(new_target);
    let mut mounts = MOUNT_TABLE.lock();
    let entry = mounts
        .iter_mut()
        .find(|m| m.target == old_target)
        .ok_or(LxError::EINVAL)?;
    entry.target = new_target;
    Ok(())
}

fn normalize_mount_target(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        String::from("/")
    } else {
        String::from(path.trim_end_matches('/'))
    }
}

pub(crate) fn proc_mounts_content() -> String {
    let mounts = MOUNT_TABLE.lock();
    let mut out = String::new();
    for m in mounts.iter() {
        let _ = writeln!(
            out,
            "{} {} {} {} 0 0",
            m.source, m.target, m.fstype, m.options
        );
    }
    out
}

#[async_trait]
/// Generic file interface
///
/// - Normal file, Directory
/// - Socket
/// - Epoll instance
pub trait FileLike: KernelObject + downcast_rs::DowncastSync {
    /// Returns open flags.
    fn flags(&self) -> OpenFlags;
    /// Set open flags.
    fn set_flags(&self, f: OpenFlags) -> LxResult;
    /// Duplicate the file.
    fn dup(&self) -> Arc<dyn FileLike> {
        unimplemented!()
    }
    /// read to buffer
    async fn read(&self, buf: &mut [u8]) -> LxResult<usize>;
    /// write from buffer
    fn write(&self, buf: &[u8]) -> LxResult<usize>;
    /// read to buffer at given offset
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> LxResult<usize>;
    /// write from buffer at given offset
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }
    /// wait for some event on a file descriptor
    fn poll(&self, events: PollEvents) -> LxResult<PollStatus>;
    /// wait for some event on a file descriptor use async
    async fn async_poll(&self, events: PollEvents) -> LxResult<PollStatus>;
    /// manipulates the underlying device parameters of special files
    fn ioctl(&self, _request: usize, _arg1: usize, _arg2: usize, _arg3: usize) -> LxResult<usize> {
        Err(LxError::ENOSYS)
    }
    /// Returns the [`VmObject`] representing the file with given `offset` and `len`.
    fn get_vmo(&self, _offset: usize, _len: usize) -> LxResult<Arc<VmObject>> {
        Err(LxError::ENOSYS)
    }
    /// Casting between trait objects, or use crate: cast_trait_object
    fn as_socket(&self) -> LxResult<&dyn Socket> {
        Err(LxError::ENOTSOCK)
    }
}

downcast_rs::impl_downcast!(sync FileLike);

/// file descriptor wrapper
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub struct FileDesc(i32);

impl FileDesc {
    /// Pathname is interpreted relative to the current working directory(CWD)
    pub const CWD: Self = FileDesc(-100);
}

impl From<usize> for FileDesc {
    fn from(x: usize) -> Self {
        FileDesc(x as i32)
    }
}

impl From<i32> for FileDesc {
    fn from(x: i32) -> Self {
        FileDesc(x)
    }
}

impl TryFrom<&str> for FileDesc {
    type Error = LxError;
    fn try_from(name: &str) -> LxResult<Self> {
        let x: i32 = name.parse().map_err(|_| LxError::EINVAL)?;
        Ok(FileDesc(x))
    }
}

impl From<FileDesc> for usize {
    fn from(f: FileDesc) -> Self {
        f.0 as _
    }
}

impl From<FileDesc> for i32 {
    fn from(f: FileDesc) -> Self {
        f.0
    }
}

/// create root filesystem, mount DevFS and RamFS
pub fn create_root_fs(rootfs: Arc<dyn FileSystem>) -> Arc<dyn INode> {
    // Filesystem from the boot medium (initrd / SFS). We use it to read the boot
    // `/etc/fstab` and, when an installed ext2 ROOT partition is detected, to
    // pivot the real root onto it (similar to an initramfs `switch_root`).
    let boot_mountfs = MountFS::new(rootfs);
    let boot_root = boot_mountfs.mountpoint_root_inode();

    // Block devices / partitions registered in DevFS, used to locate the root.
    let mut block_candidates: Vec<(String, Arc<dyn INode>)> = Vec::new();

    // create DevFS
    let devfs = DevFS::new();
    let devfs_root = devfs.root();
    *DEVFS_ROOT.lock() = Some(devfs_root.clone());
    devfs_root
        .add("null", Arc::new(NullINode::new()))
        .expect("failed to mknod /dev/null");
    devfs_root
        .add("zero", Arc::new(ZeroINode::new()))
        .expect("failed to mknod /dev/zero");
    devfs_root
        .add("random", Arc::new(RandomINode::new(false)))
        .expect("failed to mknod /dev/random");
    devfs_root
        .add("urandom", Arc::new(RandomINode::new(true)))
        .expect("failed to mknod /dev/urandom");
    devfs_root
        .add("shm", Arc::new(RandomINode::new(true)))
        .expect("failed to mknod /dev/shm");
    devfs_root
        .add("tty", stdio::STDIN.clone())
        .expect("failed to mknod /dev/tty");
    if let Some(display) = drivers::all_display().first() {
        use devfs::FbDev;

        // Add framebuffer device at `/dev/fb0`
        if let Err(e) = devfs_root.add("fb0", Arc::new(FbDev::new(display.clone()))) {
            warn!("failed to mknod /dev/fb0: {:?}", e);
        }
    }

    // Add input devices at `/dev/input/`
    {
        use devfs::{EventDev, MiceDev};
        if !drivers::all_input().as_vec().is_empty() {
            if let Ok(input_dev) = devfs_root.add_dir("input") {
                // Add mouse devices at `/dev/input/mouseX` and `/dev/input/mice`
                for (id, m) in MiceDev::from_input_devices(&drivers::all_input().as_vec()) {
                    let fname = id.map_or("mice".to_string(), |id| format!("mouse{}", id));
                    if let Err(e) = input_dev.add(&fname, Arc::new(m)) {
                        warn!("failed to mknod /dev/input/{}: {:?}", &fname, e);
                    }
                }

                // Add input event devices at `/dev/input/eventX`
                for (id, i) in drivers::all_input().as_vec().iter().enumerate() {
                    let fname = format!("event{}", id);
                    if let Err(e) = input_dev.add(&fname, Arc::new(EventDev::new(i.clone(), id))) {
                        warn!("failed to mknod /dev/input/{}: {:?}", &fname, e);
                    }
                }
            } else {
                warn!("failed to mkdir /dev/input");
            }
        }
    }

    // Register DRM drivers and add DRM devices
    {
        // Register DRM drivers from kernel-hal
        for drm in drivers::all_drm().as_vec().iter() {
            devfs::drm::register_driver(drm.clone());
        }

        if !drivers::all_drm().as_vec().is_empty() {
            if let Ok(dri_dev) = devfs_root.add_dir("dri") {
                if let Err(e) = dri_dev.add("card0", Arc::new(devfs::DrmDev::new(0))) {
                    warn!("failed to mknod /dev/dri/card0: {:?}", e);
                }
            } else {
                warn!("failed to mkdir /dev/dri");
            }
        }
    }

    // Add uart devices at `/dev/ttyS{i}`
    for (i, uart) in drivers::all_uart().as_vec().iter().enumerate() {
        let fname = format!("ttyS{}", i);
        if let Err(e) = devfs_root.add(&fname, Arc::new(devfs::UartDev::new(i, uart.clone()))) {
            warn!("failed to mknod /dev/{}: {:?}", &fname, e);
        }
    }

    // Add block devices at `/dev/` using Linux naming conventions
    let blocks = drivers::all_block().as_vec();
    for (i, block) in blocks.iter().enumerate() {
        let name = block.name();
        let fname = if name.starts_with("nvme") {
            let nvme_idx = blocks[..i]
                .iter()
                .filter(|b| b.name().starts_with("nvme"))
                .count();
            format!("nvme{}n1", nvme_idx)
        } else if name.starts_with("virtio") {
            let virtio_idx = blocks[..i]
                .iter()
                .filter(|b| b.name().starts_with("virtio"))
                .count();
            let name_char = (b'a' + (virtio_idx % 26) as u8) as char;
            format!("vd{}", name_char)
        } else {
            let other_idx = blocks[..i]
                .iter()
                .filter(|b| !b.name().starts_with("nvme") && !b.name().starts_with("virtio"))
                .count();
            let name_char = (b'a' + (other_idx % 26) as u8) as char;
            format!("sd{}", name_char)
        };
        
        // Use i * 16 as the base index for minor numbers to leave room for partitions
        let base_index = i * 16;
        let dev = Arc::new(devfs::BlockDev::new(base_index, block.clone(), fname.clone()));
        let dev_dyn: Arc<dyn INode> = dev.clone();
        if let Err(e) = devfs_root.add(&fname, dev) {
            warn!("failed to mknod /dev/{}: {:?}", &fname, e);
        } else {
            block_candidates.push((fname.clone(), dev_dyn));
        }

        // Scan for partitions on this block device
        let partitions = devfs::blockdev::scan_partitions(block);
        for (part_idx, &(start_block, block_count)) in partitions.iter().enumerate() {
            let part_num = part_idx + 1;
            let part_name = if fname.starts_with("nvme") {
                format!("{}p{}", fname, part_num)
            } else {
                format!("{}{}", fname, part_num)
            };
            let partition_driver = Arc::new(devfs::blockdev::PartitionBlock::new(
                block.clone(),
                format!("{}-part{}", name, part_num),
                start_block,
                block_count,
            ));
            let part_dev_index = base_index + part_num;
            let part = Arc::new(devfs::BlockDev::new(part_dev_index, partition_driver, part_name.clone()));
            let part_dyn: Arc<dyn INode> = part.clone();
            if let Err(e) = devfs_root.add(&part_name, part) {
                warn!("failed to mknod /dev/{}: {:?}", &part_name, e);
            } else {
                info!("Registered partition /dev/{} (start: {}, count: {})", part_name, start_block, block_count);
                block_candidates.push((part_name.clone(), part_dyn));
            }
        }
    }

    // Decide the real root filesystem: pivot from the boot medium onto an
    // installed ext2 ROOT partition when one is available, otherwise keep the
    // boot medium as `/`.
    let (rootfs, root_source, root_fstype) =
        match determine_real_root(&boot_root, &block_candidates) {
            Some((fs, source)) => {
                info!("create_root_fs: pivoting root onto {} (ext2)", source);
                (MountFS::new(fs), source, "ext2")
            }
            None => (boot_mountfs, String::from("rootfs"), "rootfs"),
        };
    let root = rootfs.mountpoint_root_inode();
    reset_mount_table();
    register_mount(&root_source, "/", root_fstype, "rw", boot_mount_state());

    // mount DevFS at /dev
    let dev = root.find(true, "dev").unwrap_or_else(|_| {
        root.create("dev", FileType::Dir, 0o666)
            .expect("failed to mkdir /dev")
    });
    dev.mount(devfs).expect("failed to mount DevFS");
    register_mount("devfs", "/dev", "devtmpfs", "rw,nosuid", boot_mount_state());

    // mount RamFS at /tmp
    let ramfs = RamFS::new();
    let tmp = root.find(true, "tmp").unwrap_or_else(|_| {
        root.create("tmp", FileType::Dir, 0o666)
            .expect("failed to mkdir /tmp")
    });
    tmp.mount(ramfs).expect("failed to mount RamFS");
    register_mount("tmpfs", "/tmp", "tmpfs", "rw,nosuid,nodev", boot_mount_state());

    // mount RamFS at /run (essential for DHCP clients and other daemons)
    let run_ramfs = RamFS::new();
    let run = root.find(true, "run").unwrap_or_else(|_| {
        root.create("run", FileType::Dir, 0o755)
            .expect("failed to mkdir /run")
    });
    run.mount(run_ramfs).expect("failed to mount RamFS at /run");
    register_mount("tmpfs", "/run", "tmpfs", "rw,nosuid,nodev", boot_mount_state());

    // Ensure /var/run exists and can be used (often it's a symlink or needs its own mount)
    if let Ok(var) = root.find(true, "var") {
        if var.find(true, "run").is_err() {
            var.create("run", FileType::Dir, 0o755).ok();
        }
    }

    // mount ProcFS at /proc
    let proc = root.find(true, "proc").unwrap_or_else(|_| {
        root.create("proc", FileType::Dir, 0o755)
            .expect("failed to mkdir /proc")
    });
    proc.mount(Arc::new(ProcFS::new()))
        .expect("failed to mount ProcFS");
    register_mount(
        "proc",
        "/proc",
        "proc",
        "rw,nosuid,nodev,noexec,relatime",
        boot_mount_state(),
    );

    // mount SysFS at /sys
    let sys = root.find(true, "sys").unwrap_or_else(|_| {
        root.create("sys", FileType::Dir, 0o755)
            .expect("failed to mkdir /sys")
    });
    sys.mount(Arc::new(SysFS::new()))
        .expect("failed to mount SysFS");
    register_mount(
        "sysfs",
        "/sys",
        "sysfs",
        "rw,nosuid,nodev,noexec,relatime",
        boot_mount_state(),
    );

    mount_ops::set_vfs_root(root.clone());
    mount_fstab(&root);
    root
}

/// Choose the real root filesystem, pivoting from the boot medium onto an
/// installed ext2 ROOT partition when one is available.
///
/// Resolution order:
/// 1. `ROOT=<dev>` on the kernel command line (e.g. `ROOT=/dev/sda2`) is
///    *authoritative*: if present we obey it, and if it cannot be resolved
///    (e.g. the unpatched placeholder of a live/installer medium) we keep the
///    boot medium as `/` rather than guessing — so a live medium never pivots
///    onto an attached installed disk by accident.
/// 2. If there is no `ROOT=` directive at all: the root (`/`) entry of the boot
///    medium's `/etc/fstab`, when it names a real, resolvable device.
/// 3. Otherwise: auto-detection of the first ext2 partition whose own
///    `/etc/fstab` declares that very partition as `/` (self-consistent root).
///
/// Returns the filesystem to use as `/` together with its device path, or
/// `None` to keep the boot medium as the root.
fn determine_real_root(
    boot_root: &Arc<MNode>,
    candidates: &[(String, Arc<dyn INode>)],
) -> Option<(Arc<dyn FileSystem>, String)> {
    // 1. An explicit `ROOT=<dev>` on the kernel command line is authoritative.
    let cmdline = kernel_hal::boot::cmdline();
    if let Some(dev) = parse_root_cmdline(&cmdline) {
        match lookup_candidate(candidates, dev) {
            Some(inode) => match open_ext2_root(inode) {
                Some(fs) => return Some((fs, String::from(dev))),
                None => warn!("create_root_fs: ROOT={} no es un ext2 montable", dev),
            },
            None => info!(
                "create_root_fs: ROOT={} sin resolver; se mantiene el medio de arranque",
                dev
            ),
        }
        // A ROOT= directive was given but is unusable: do not auto-detect.
        return None;
    }

    // 2. No explicit ROOT=: use the boot medium's fstab root entry, if real.
    if let Some(res) = root_fs_from_fstab(boot_root, candidates) {
        return Some(res);
    }

    // 3. Auto-detect a self-consistent installed ext2 root.
    for (name, inode) in candidates {
        if let Some(fs) = open_ext2_root(inode.clone()) {
            if fstab_declares_self_root(&fs, name) {
                return Some((fs, format!("/dev/{}", name)));
            }
        }
    }
    None
}

/// Extract the `ROOT=` device from the kernel command line, which is a
/// `:`-separated list of `KEY=value` pairs (e.g. `LOG=info:ROOT=/dev/sda2`).
fn parse_root_cmdline(cmdline: &str) -> Option<&str> {
    for opt in cmdline.split(':') {
        let mut it = opt.trim().splitn(2, '=');
        let key = it.next().unwrap_or("").trim();
        if key.eq_ignore_ascii_case("ROOT") {
            let val = it.next().unwrap_or("").trim();
            if !val.is_empty() {
                return Some(val);
            }
        }
    }
    None
}

/// Find a registered block device whose name matches the basename of `dev`
/// (e.g. `/dev/sda2` -> `sda2`).
fn lookup_candidate(
    candidates: &[(String, Arc<dyn INode>)],
    dev: &str,
) -> Option<Arc<dyn INode>> {
    let want = dev.trim().rsplit('/').next()?;
    candidates
        .iter()
        .find(|(n, _)| n.as_str() == want)
        .map(|(_, i)| i.clone())
}

/// Open a block-device inode as an ext2 filesystem, if possible.
fn open_ext2_root(inode: Arc<dyn INode>) -> Option<Arc<dyn FileSystem>> {
    let backend = block_mount::MountBackend::from_inode(inode).ok()?;
    mount_ops::open_filesystem(backend, "ext2").ok()
}

/// Open the ext2 root declared by the `/` entry of the boot medium's fstab,
/// when that entry names a real, resolvable device.
fn root_fs_from_fstab(
    boot_root: &Arc<MNode>,
    candidates: &[(String, Arc<dyn INode>)],
) -> Option<(Arc<dyn FileSystem>, String)> {
    let etc = boot_root.find(true, "etc").ok()?;
    let fstab = etc.find(true, "fstab").ok()?;
    let fstab_dyn: Arc<dyn INode> = fstab;
    let content_vec = fstab_dyn.read_as_vec().ok()?;
    let content = core::str::from_utf8(&content_vec).ok()?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "/" {
            continue;
        }
        // Only ext-family roots can be mounted here; bail out otherwise.
        if mount_ops::parse_fstype(parts[2]).ok()? != "ext2" {
            return None;
        }
        let inode = lookup_candidate(candidates, parts[0])?;
        let fs = open_ext2_root(inode)?;
        return Some((fs, String::from(parts[0])));
    }
    None
}

/// Returns `true` when `fs` contains an `/etc/fstab` whose root (`/`) entry
/// names exactly the partition `dev_name`, i.e. this is the installed root.
fn fstab_declares_self_root(fs: &Arc<dyn FileSystem>, dev_name: &str) -> bool {
    let root = fs.root_inode();
    let etc = match root.find("etc") {
        Ok(n) => n,
        Err(_) => return false,
    };
    let fstab = match etc.find("fstab") {
        Ok(n) => n,
        Err(_) => return false,
    };
    let content_vec = match fstab.read_as_vec() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let content = match core::str::from_utf8(&content_vec) {
        Ok(s) => s,
        Err(_) => return false,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 || parts[1] != "/" {
            continue;
        }
        // The root entry must name a real device (not a placeholder) that
        // corresponds to this very partition.
        return parts[0].trim().rsplit('/').next() == Some(dev_name);
    }
    false
}

fn resolve_or_create_dir(root: &Arc<MNode>, path: &str) -> LxResult<Arc<MNode>> {
    let mut cur = root.clone();
    for comp in path.split('/').filter(|s| !s.is_empty()) {
        cur = match cur.find(true, comp) {
            Ok(node) => node,
            Err(_) => cur.create(comp, FileType::Dir, 0o755).map_err(LxError::from)?,
        };
    }
    Ok(cur)
}

fn mount_fstab(root: &Arc<MNode>) {
    info!("mount_fstab: parsing /etc/fstab");
    if let Ok(etc) = root.find(true, "etc") {
        if let Ok(fstab_inode) = etc.find(true, "fstab") {
            let fstab_dyn: Arc<dyn INode> = fstab_inode;
            if let Ok(content_vec) = fstab_dyn.read_as_vec() {
                if let Ok(content) = core::str::from_utf8(&content_vec) {
                    for line in content.lines() {
                        let line = line.trim();
                        if line.is_empty() || line.starts_with('#') {
                            continue;
                        }
                        let parts: Vec<&str> = line.split_whitespace().collect();
                        if parts.len() < 3 {
                            continue;
                        }
                        let source = parts[0];
                        let target = parts[1];
                        let fstype = parts[2];
                        let options = parts.get(3).copied().unwrap_or("defaults");

                        if target == "/" || target == "none" || fstype == "swap" {
                            continue;
                        }

                        // Resolve the source inode using coerced root_dyn
                        let source_rel = source.trim_start_matches('/');
                        let root_dyn: Arc<dyn INode> = root.clone();
                        let source_inode = match root_dyn.lookup_follow(source_rel, 4) {
                            Ok(inode) => inode,
                            Err(e) => {
                                warn!("mount_fstab: failed to lookup source {:?}: {:?}", source, e);
                                continue;
                            }
                        };

                        let backend = match block_mount::MountBackend::from_inode(source_inode) {
                            Ok(b) => b,
                            Err(e) => {
                                warn!("mount_fstab: failed to create MountBackend for {:?}: {:?}", source, e);
                                continue;
                            }
                        };

                        let fstype_parsed = match mount_ops::parse_fstype(fstype) {
                            Ok(ft) => ft,
                            Err(e) => {
                                warn!("mount_fstab: unsupported fstype {:?}: {:?}", fstype, e);
                                continue;
                            }
                        };

                        let target_node = match resolve_or_create_dir(root, target) {
                            Ok(node) => node,
                            Err(e) => {
                                warn!("mount_fstab: failed to resolve/create target {:?}: {:?}", target, e);
                                continue;
                            }
                        };

                        if target_node.is_mountpoint() {
                            warn!("mount_fstab: target {:?} is already a mountpoint", target);
                            continue;
                        }

                        let fs = match mount_ops::open_filesystem(backend, fstype_parsed) {
                            Ok(f) => f,
                            Err(e) => {
                                warn!("mount_fstab: failed to open filesystem for {:?}: {:?}", source, e);
                                continue;
                            }
                        };

                        // Parse options for flags
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

                        let (fs, state) = mount_ops::prepare_fs(fs, flags, options);
                        if let Err(e) = target_node.mount(fs) {
                            warn!("mount_fstab: failed to mount {:?} to {:?}: {:?}", source, target, e);
                            continue;
                        }

                        let opts = mount_state::build_options_string(flags, options);
                        register_mount(source, target, fstype_parsed, &opts, state);
                        info!("mount_fstab: successfully mounted {:?} to {:?}", source, target);
                    }
                }
            }
        }
    }
}

pub use mount_ops::{mount_fs, umount_fs};

/// extension for INode
pub trait INodeExt {
    /// similar to read, but return a u8 vector
    fn read_as_vec(&self) -> Result<Vec<u8>>;
    /// read to VmObject
    fn read_as_vmo(&self) -> Result<Arc<VmObject>>;
}

impl INodeExt for dyn INode {
    #[allow(unsafe_code, clippy::uninit_vec)]
    fn read_as_vec(&self) -> Result<Vec<u8>> {
        let size = self.metadata()?.size;
        let mut buf = Vec::with_capacity(size);
        unsafe {
            buf.set_len(size);
        }
        self.read_at(0, buf.as_mut_slice())?;
        Ok(buf)
    }

    fn read_as_vmo(&self) -> Result<Arc<VmObject>> {
        let size = self.metadata()?.size;
        let pages = (size + 0xfff) >> 12;
        let vmo = VmObject::new_paged(pages);
        let mut offset = 0;
        let mut buf = [0u8; 16384];
        while offset < size {
            let len = (size - offset).min(buf.len());
            let read_len = self.read_at(offset, &mut buf[..len])?;
            if read_len == 0 {
                break;
            }
            vmo.write(offset, &buf[..read_len]).map_err(|_| rcore_fs::vfs::FsError::DeviceError)?;
            offset += read_len;
        }
        vmo.set_content_size(size).map_err(|_| rcore_fs::vfs::FsError::DeviceError)?;
        Ok(vmo)
    }
}

impl LinuxProcess {
    /// Lookup INode from the process.
    ///
    /// - If `path` is relative, then it is interpreted relative to the directory
    ///   referred to by the file descriptor `dirfd`.
    ///
    /// - If the `dirfd` is the special value `AT_FDCWD`, then the directory is
    ///   current working directory of the process.
    ///
    /// - If `path` is absolute, then `dirfd` is ignored.
    ///
    /// - If `follow` is true, then dereference `path` if it is a symbolic link.
    pub fn lookup_inode_at(
        &self,
        dirfd: FileDesc,
        path: &str,
        follow: bool,
    ) -> LxResult<Arc<dyn INode>> {
        debug!(
            "lookup_inode_at: dirfd: {:?}, cwd: {:?}, path: {:?}, follow: {:?}",
            dirfd,
            self.current_working_directory(),
            path,
            follow
        );
        // hard code special path
        if path == "/proc/self/exe" {
            return Ok(Arc::new(Pseudo::new(
                &self.execute_path(),
                FileType::SymLink,
            )));
        }
        if path == "/proc/self/fd" || path == "/proc/self/fd/" {
            return Ok(Arc::new(proc_self::ProcSelfFdDir {
                process: self.zircon_process().clone(),
            }));
        }
        let (fd_dir_path, fd_name) = split_path(path);
        if fd_dir_path == "/proc/self/fd" {
            let fd = FileDesc::try_from(fd_name)?;
            let file = self.get_file(fd)?;
            return Ok(Arc::new(Pseudo::new(file.path(), FileType::SymLink)));
        }

        let follow_max_depth = if follow { FOLLOW_MAX_DEPTH } else { 0 };
        if dirfd == FileDesc::CWD {
            Ok(self
                .root_inode()
                .lookup(&self.current_working_directory())?
                .lookup_follow(path, follow_max_depth)?)
        } else {
            let file = self.get_file(dirfd)?;
            Ok(file.lookup_follow(path, follow_max_depth)?)
        }
    }

    /// Lookup INode from the process.
    ///
    /// see `lookup_inode_at`
    pub fn lookup_inode(&self, path: &str) -> LxResult<Arc<dyn INode>> {
        self.lookup_inode_at(FileDesc::CWD, path, true)
    }
}

/// Split a `path` str to `(base_path, file_name)`
pub fn split_path(path: &str) -> (&str, &str) {
    let mut split = path.trim_end_matches('/').rsplitn(2, '/');
    let file_name = split.next().unwrap();
    let mut dir_path = split.next().unwrap_or(".");
    if dir_path.is_empty() {
        dir_path = "/";
    }
    (dir_path, file_name)
}

/// the max depth for following a link
const FOLLOW_MAX_DEPTH: usize = 1;

/// Rescans and registers partitions for a block device in devfs.
pub fn rescan_partitions(fname: &str, block: &Arc<dyn zcore_drivers::scheme::BlockScheme>, base_index: usize) -> LxResult<()> {
    if let Some(devfs_root) = DEVFS_ROOT.lock().as_ref() {
        // First, remove existing partition nodes (e.g. sda1..=sda15)
        for part_num in 1..=15 {
            let part_name = if fname.starts_with("nvme") {
                format!("{}p{}", fname, part_num)
            } else {
                format!("{}{}", fname, part_num)
            };
            let _ = devfs_root.remove(&part_name);
        }

        // Now, scan partitions
        let partitions = devfs::blockdev::scan_partitions(block);
        for (part_idx, &(start_block, block_count)) in partitions.iter().enumerate() {
            let part_num = part_idx + 1;
            let part_name = if fname.starts_with("nvme") {
                format!("{}p{}", fname, part_num)
            } else {
                format!("{}{}", fname, part_num)
            };
            let partition_driver = Arc::new(devfs::blockdev::PartitionBlock::new(
                block.clone(),
                format!("{}-part{}", fname, part_num),
                start_block,
                block_count,
            ));
            let part_dev_index = base_index + part_num;
            if let Err(e) = devfs_root.add(&part_name, Arc::new(devfs::BlockDev::new(part_dev_index, partition_driver, part_name.clone()))) {
                warn!("failed to mknod /dev/{} during rescan: {:?}", &part_name, e);
            } else {
                info!("Rescanned and registered partition /dev/{} (start: {}, count: {})", part_name, start_block, block_count);
            }
        }
    }
    Ok(())
}
