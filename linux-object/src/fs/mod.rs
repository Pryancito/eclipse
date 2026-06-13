//! Linux file objects

mod block_mount;
mod btrfs_mount;
mod devfs;
mod epoll;
mod eventfd;
mod ext2_editor;
mod ext2_mount;
mod fat_mount;
mod file;
mod flagged_fs;
pub mod ioctl;
mod mount_ops;
mod mount_state;
mod pidfd;
mod pipe;
mod proc_self;
mod procfs;
mod pseudo;
pub mod rcore_fs_wrapper;
pub mod stdio;
mod sysfs;

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
use rcore_fs_mountfs::{MNode, MountFS};
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

pub use epoll::{Epoll, EpollEvent};
pub use eventfd::EventFd;
pub use file::{File, OpenFlags, PollEvents, SeekFrom};
pub use pidfd::{PidFd, PIDFD_THREAD};
pub use pipe::Pipe;
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

/// Resolve a top-level mount directory on the pivoted block-device root
/// (btrfs/ext2) without `MNode::find` overlay/metadata overhead (VBox disk
/// boot).
fn boot_resolve_mount_dir(
    rootfs: &Arc<MountFS>,
    root: &Arc<MNode>,
    name: &str,
    mode: u32,
) -> Arc<MNode> {
    warn!("[boot] lookup /{} on backing", name);
    if let Ok(inode) = rootfs.inner_fs().root_inode().find(name) {
        warn!("[boot] found /{}", name);
        return MNode::from_backing(rootfs.clone(), inode);
    }
    warn!("[boot] mkdir /{}", name);
    root.create(name, FileType::Dir, mode)
        .expect("failed to mkdir")
}

fn resolve_mount_dir(
    rootfs: &Arc<MountFS>,
    root: &Arc<MNode>,
    root_fstype: &str,
    name: &str,
    mode: u32,
) -> Arc<MNode> {
    if root_fstype == "ext2" || root_fstype == "btrfs" {
        boot_resolve_mount_dir(rootfs, root, name, mode)
    } else {
        root.find(true, name).unwrap_or_else(|_| {
            root.create(name, FileType::Dir, mode)
                .expect("failed to mkdir")
        })
    }
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
    warn!("[boot] create_root_fs: begin");
    // Filesystem from the boot medium (initrd / SFS). We use it to read the boot
    // `/etc/fstab` and, when an installed btrfs/ext2 ROOT partition is
    // detected, to pivot the real root onto it (similar to an initramfs
    // `switch_root`).
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

    warn!("[boot] create_root_fs: devfs ready");

    // Add block devices at `/dev/` using Linux naming conventions
    let blocks = drivers::all_block().as_vec();
    warn!(
        "[boot] create_root_fs: scanning {} block device(s)",
        blocks.len()
    );
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
        let dev = Arc::new(devfs::BlockDev::new(
            base_index,
            block.clone(),
            fname.clone(),
        ));
        let dev_dyn: Arc<dyn INode> = dev.clone();
        if let Err(e) = devfs_root.add(&fname, dev) {
            warn!("failed to mknod /dev/{}: {:?}", &fname, e);
        } else {
            block_candidates.push((fname.clone(), dev_dyn));
        }

        // Scan for partitions on this block device
        let partitions = devfs::blockdev::scan_partitions(block);
        warn!(
            "[boot] create_root_fs: /dev/{} has {} partition(s)",
            fname,
            partitions.len()
        );
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
            let part = Arc::new(devfs::BlockDev::new(
                part_dev_index,
                partition_driver,
                part_name.clone(),
            ));
            let part_dyn: Arc<dyn INode> = part.clone();
            if let Err(e) = devfs_root.add(&part_name, part) {
                warn!("failed to mknod /dev/{}: {:?}", &part_name, e);
            } else {
                info!(
                    "Registered partition /dev/{} (start: {}, count: {})",
                    part_name, start_block, block_count
                );
                block_candidates.push((part_name.clone(), part_dyn));
            }
        }
    }

    // Decide the real root filesystem: pivot from the boot medium onto an
    // installed btrfs/ext2 ROOT partition when one is available, otherwise
    // keep the boot medium as `/`.
    warn!(
        "[boot] create_root_fs: determine_real_root ({} candidate(s))",
        block_candidates.len()
    );
    let (rootfs, root_source, root_fstype) =
        match determine_real_root(&boot_root, &block_candidates) {
            Some((fs, source, fstype)) => {
                warn!("[boot] create_root_fs: pivot onto {} ({})", source, fstype);
                (MountFS::new(fs), source, fstype)
            }
            None => {
                warn!("[boot] create_root_fs: keep boot medium as /");
                (boot_mountfs, String::from("rootfs"), "rootfs")
            }
        };
    warn!("[boot] create_root_fs: root inode");
    let root = rootfs.mountpoint_root_inode();
    reset_mount_table();
    register_mount(&root_source, "/", root_fstype, "rw", boot_mount_state());

    // mount DevFS at /dev
    let dev = resolve_mount_dir(&rootfs, &root, root_fstype, "dev", 0o666);
    warn!("[boot] create_root_fs: mount devfs on /dev");
    if let Err(e) = dev.mount(devfs) {
        warn!("[boot] create_root_fs: mount /dev failed: {:?}", e);
    } else {
        register_mount("devfs", "/dev", "devtmpfs", "rw,nosuid", boot_mount_state());
    }

    // mount RamFS at /tmp
    warn!("[boot] create_root_fs: mount /tmp");
    let ramfs = RamFS::new();
    let tmp = resolve_mount_dir(&rootfs, &root, root_fstype, "tmp", 0o666);
    if let Err(e) = tmp.mount(ramfs) {
        warn!("[boot] create_root_fs: mount /tmp failed: {:?}", e);
    } else {
        register_mount(
            "tmpfs",
            "/tmp",
            "tmpfs",
            "rw,nosuid,nodev",
            boot_mount_state(),
        );
    }

    // mount RamFS at /run (essential for DHCP clients and other daemons)
    warn!("[boot] create_root_fs: mount /run");
    let run_ramfs = RamFS::new();
    let run = resolve_mount_dir(&rootfs, &root, root_fstype, "run", 0o755);
    if let Err(e) = run.mount(run_ramfs) {
        warn!("[boot] create_root_fs: mount /run failed: {:?}", e);
    } else {
        register_mount(
            "tmpfs",
            "/run",
            "tmpfs",
            "rw,nosuid,nodev",
            boot_mount_state(),
        );
    }

    // Ensure /var/run exists. Skip while pivoting onto an installed block
    // root (btrfs/ext2): scanning /var during early boot has stalled some
    // VBox/VDI setups, and /run is already a dedicated tmpfs mount above.
    if root_fstype != "ext2" && root_fstype != "btrfs" {
        if let Ok(var) = root.find(true, "var") {
            if var.find(true, "run").is_err() {
                var.create("run", FileType::Dir, 0o755).ok();
            }
        }
        // Keep apk's download cache off the small initramfs SFS: edge indexes
        // plus .apk blobs can exceed the free space left after zip_dir.
        warn!("[boot] create_root_fs: mount /var/cache/apk on tmpfs");
        if let Ok(var) = root.find(true, "var") {
            let cache = var.find(true, "cache").unwrap_or_else(|_| {
                var.create("cache", FileType::Dir, 0o755)
                    .expect("failed to mkdir /var/cache")
            });
            let apk_cache = cache.find(true, "apk").unwrap_or_else(|_| {
                cache
                    .create("apk", FileType::Dir, 0o755)
                    .expect("failed to mkdir /var/cache/apk")
            });
            if apk_cache.mount(RamFS::new()).is_ok() {
                register_mount(
                    "tmpfs",
                    "/var/cache/apk",
                    "tmpfs",
                    "rw,nosuid,nodev",
                    boot_mount_state(),
                );
            } else {
                warn!("[boot] create_root_fs: mount /var/cache/apk failed");
            }
        }
    }

    // mount ProcFS at /proc
    warn!("[boot] create_root_fs: mount /proc");
    let proc = resolve_mount_dir(&rootfs, &root, root_fstype, "proc", 0o755);
    if let Err(e) = proc.mount(Arc::new(ProcFS::new())) {
        warn!("[boot] create_root_fs: mount /proc failed: {:?}", e);
    } else {
        register_mount(
            "proc",
            "/proc",
            "proc",
            "rw,nosuid,nodev,noexec,relatime",
            boot_mount_state(),
        );
    }

    // mount SysFS at /sys
    warn!("[boot] create_root_fs: mount /sys");
    let sys = resolve_mount_dir(&rootfs, &root, root_fstype, "sys", 0o755);
    if let Err(e) = sys.mount(Arc::new(SysFS::new())) {
        warn!("[boot] create_root_fs: mount /sys failed: {:?}", e);
    } else {
        register_mount(
            "sysfs",
            "/sys",
            "sysfs",
            "rw,nosuid,nodev,noexec,relatime",
            boot_mount_state(),
        );
    }

    mount_ops::set_vfs_root(root.clone());
    // Defer non-root fstab mounts (/boot vfat, /home, …) until after init starts.
    // Mounting them here can stall disk boot (AHCI + SMP) before the shell appears.
    warn!("[boot] create_root_fs: done");
    root
}

/// Choose the real root filesystem, pivoting from the boot medium onto an
/// installed btrfs (or legacy ext2) ROOT partition when one is available.
///
/// Resolution order:
/// 1. `ROOT=<dev>` on the kernel command line (e.g. `ROOT=/dev/sda2`) when it
///    resolves to a real btrfs/ext2 device — a deterministic, explicit pivot.
/// 2. The root (`/`) entry of the boot medium's `/etc/fstab`, when it names a
///    real, resolvable device.
/// 3. Auto-detection: the first partition block device that passes the btrfs
///    or ext2 superblock probe and mounts cleanly (typically `/dev/sda2` on an
///    AHCI install with EFI on `sda1`).  We intentionally avoid walking
///    installed root directories here — that has stalled some VBox/VDI
///    setups.
///
/// An unresolved `ROOT=` (for instance the unpatched placeholder baked into a
/// live medium's `rboot.conf`) is ignored and we fall through to auto-detection
/// instead of staying on the boot medium.
///
/// Returns the filesystem to use as `/` together with its device path, or
/// `None` to keep the boot medium as the root.
fn determine_real_root(
    boot_root: &Arc<MNode>,
    candidates: &[(String, Arc<dyn INode>)],
) -> Option<(Arc<dyn FileSystem>, String, &'static str)> {
    // 1. An explicit `ROOT=<dev>` that resolves to a real device wins.
    let cmdline = kernel_hal::boot::cmdline();
    if let Some(dev) = parse_root_cmdline(&cmdline) {
        if let Some(inode) = lookup_candidate(candidates, dev) {
            if let Some((fs, fstype)) = open_block_root(inode) {
                info!("create_root_fs: root via ROOT={} ({})", dev, fstype);
                return Some((fs, String::from(dev), fstype));
            }
            warn!("create_root_fs: ROOT={} no es un btrfs/ext2 montable", dev);
        } else {
            info!(
                "create_root_fs: ROOT={} sin resolver; se intenta fstab/auto-detección",
                dev
            );
        }
    }

    // 2. The boot medium's fstab root entry, if it names a real device.
    warn!("[boot] determine_real_root: boot fstab");
    if let Some(res) = root_fs_from_fstab(boot_root, candidates) {
        return Some(res);
    }

    // 3. First mountable btrfs/ext2 partition (vfat EFI on sda1 fails the
    //    probes).
    for (name, inode) in root_mount_candidates(candidates) {
        warn!("[boot] determine_real_root: probe /dev/{}", name);
        if let Some((fs, fstype)) = open_block_root(inode.clone()) {
            warn!(
                "[boot] determine_real_root: pivot /dev/{} ({})",
                name, fstype
            );
            return Some((fs, format!("/dev/{}", name), fstype));
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
            if !val.is_empty()
                && !val.starts_with("__ECLIPSE_")
                && val != "/dev/__ECLIPSE_CMDROOTDEV"
            {
                return Some(val);
            }
        }
    }
    None
}

/// True for partition nodes (`sda2`, `nvme0n1p3`, …), false for whole disks (`sda`).
fn is_partition_candidate(name: &str) -> bool {
    name.chars().last().is_some_and(|c| c.is_ascii_digit())
}

/// Prefer partition devices when GPT/MBR children exist; probing whole disks is
/// slow and often matches garbage superblocks on protective-MBR layouts.
fn root_mount_candidates<'a>(
    candidates: &'a [(String, Arc<dyn INode>)],
) -> impl Iterator<Item = &'a (String, Arc<dyn INode>)> {
    let prefer_partitions = candidates
        .iter()
        .any(|(name, _)| is_partition_candidate(name));
    candidates
        .iter()
        .filter(move |(name, _)| !prefer_partitions || is_partition_candidate(name))
}

/// Find a registered block device whose name matches the basename of `dev`
/// (e.g. `/dev/sda2` -> `sda2`).
fn lookup_candidate(candidates: &[(String, Arc<dyn INode>)], dev: &str) -> Option<Arc<dyn INode>> {
    let want = dev.trim().rsplit('/').next()?;
    candidates
        .iter()
        .find(|(n, _)| n.as_str() == want)
        .map(|(_, i)| i.clone())
}

/// Open a block-device inode as a btrfs (preferred) or ext2 filesystem, if
/// possible. Returns the filesystem together with its fstype name.
fn open_block_root(inode: Arc<dyn INode>) -> Option<(Arc<dyn FileSystem>, &'static str)> {
    let backend = block_mount::MountBackend::from_inode(inode.clone()).ok()?;
    if let block_mount::MountBackend::Block(block) = &backend {
        if btrfs_mount::probe_btrfs_superblock(block) {
            if let Ok(fs) = mount_ops::open_filesystem(backend, "btrfs") {
                return Some((fs, "btrfs"));
            }
            return None;
        }
        if !block_mount::probe_ext2_superblock(block) {
            return None;
        }
        let fs = mount_ops::open_filesystem(backend, "ext2").ok()?;
        return Some((fs, "ext2"));
    }
    // File-backed (loop) roots: try btrfs, then ext2.
    match mount_ops::open_filesystem(backend, "btrfs") {
        Ok(fs) => Some((fs, "btrfs")),
        Err(_) => {
            let backend = block_mount::MountBackend::from_inode(inode).ok()?;
            let fs = mount_ops::open_filesystem(backend, "ext2").ok()?;
            Some((fs, "ext2"))
        }
    }
}

/// Open the root declared by the `/` entry of the boot medium's fstab, when
/// that entry names a real, resolvable device.
fn root_fs_from_fstab(
    boot_root: &Arc<MNode>,
    candidates: &[(String, Arc<dyn INode>)],
) -> Option<(Arc<dyn FileSystem>, String, &'static str)> {
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
        // Only block-device roots (btrfs/ext-family) can be mounted here.
        let fstype = mount_ops::parse_fstype(parts[2]).ok()?;
        if fstype != "btrfs" && fstype != "ext2" {
            return None;
        }
        let inode = lookup_candidate(candidates, parts[0])?;
        let (fs, fstype) = open_block_root(inode)?;
        return Some((fs, String::from(parts[0]), fstype));
    }
    None
}

fn resolve_or_create_dir(root: &Arc<MNode>, path: &str) -> LxResult<Arc<MNode>> {
    let mut cur = root.clone();
    for comp in path.split('/').filter(|s| !s.is_empty()) {
        cur = match cur.find(true, comp) {
            Ok(node) => node,
            Err(_) => cur
                .create(comp, FileType::Dir, 0o755)
                .map_err(LxError::from)?,
        };
    }
    Ok(cur)
}

/// Mount entries from `/etc/fstab` (except `/`). Call after init is up.
pub fn mount_vfs_fstab(root: &Arc<MNode>) {
    mount_fstab(root);
}

/// Process `/etc/fstab` using the VFS root remembered by `create_root_fs`.
///
/// Intended to be called *after* init has started (e.g. spawned as a kernel
/// task), since mounting extra filesystems (/boot/efi vfat, /home, …) does
/// blocking block-device I/O that must not run on the early-boot path before
/// the shell appears.
pub fn mount_fstab_deferred() {
    match mount_ops::vfs_root() {
        Some(root) => {
            warn!("[boot] mount_fstab_deferred: processing /etc/fstab");
            mount_fstab(&root);
        }
        None => warn!("[boot] mount_fstab_deferred: no VFS root set; skipping"),
    }
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

                        // When the installer left an unsubstituted __ECLIPSE_EFI_DEV*
                        // placeholder for /boot/efi, try to derive the EFI partition
                        // from ROOT= on the kernel command line (e.g. ROOT=/dev/sda2 →
                        // EFI=/dev/sda1). This covers the edge case where raw-block
                        // patching of fstab did not take effect on the first boot.
                        let derived_efi: Option<String>;
                        let effective_source = if source_rel.starts_with("__ECLIPSE_EFI")
                            && target == "/boot/efi"
                        {
                            derived_efi = efi_dev_from_root_cmdline();
                            match derived_efi.as_deref() {
                                Some(dev) => {
                                    warn!(
                                        "mount_fstab: fstab EFI placeholder sin sustituir; \
                                         intentando ROOT= derivado {:?} -> {:?}",
                                        dev, target
                                    );
                                    dev
                                }
                                None => {
                                    info!(
                                        "mount_fstab: skipping unsubstituted EFI placeholder \
                                         (ROOT= no disponible para derivar EFI)"
                                    );
                                    continue;
                                }
                            }
                        } else if source_rel.starts_with("__ECLIPSE_") {
                            // Other unsubstituted placeholders: skip silently.
                            info!(
                                "mount_fstab: skipping unsubstituted placeholder source {:?} -> {:?}",
                                source, target
                            );
                            continue;
                        } else {
                            derived_efi = None;
                            source_rel
                        };

                        let effective_source_rel = effective_source.trim_start_matches('/');
                        let root_dyn: Arc<dyn INode> = root.clone();
                        let source_inode = match root_dyn.lookup_follow(effective_source_rel, 4) {
                            Ok(inode) => inode,
                            Err(e) => {
                                warn!(
                                    "mount_fstab: failed to lookup source {:?}: {:?}",
                                    effective_source, e
                                );
                                continue;
                            }
                        };

                        let backend = match block_mount::MountBackend::from_inode(source_inode) {
                            Ok(b) => b,
                            Err(e) => {
                                warn!(
                                    "mount_fstab: failed to create MountBackend for {:?}: {:?}",
                                    effective_source, e
                                );
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
                                warn!(
                                    "mount_fstab: failed to resolve/create target {:?}: {:?}",
                                    target, e
                                );
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
                                warn!(
                                    "mount_fstab: failed to open filesystem for {:?}: {:?}",
                                    effective_source, e
                                );
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
                            warn!(
                                "mount_fstab: failed to mount {:?} to {:?}: {:?}",
                                effective_source, target, e
                            );
                            continue;
                        }

                        let mount_source = derived_efi.as_deref().unwrap_or(source);
                        let opts = mount_state::build_options_string(flags, options);
                        register_mount(mount_source, target, fstype_parsed, &opts, state);
                        info!(
                            "mount_fstab: successfully mounted {:?} to {:?}",
                            mount_source, target
                        );
                    }
                }
            }
        }
    }
}

/// Derive the EFI partition path from `ROOT=` on the kernel command line.
///
/// `ROOT=` names the installed ext2 root (e.g. `/dev/sda2`). On a standard
/// Eclipse OS layout the EFI system partition is always partition 1 on the
/// same disk (e.g. `/dev/sda1`, `/dev/nvme0n1p1`, `/dev/vda1`).
///
/// Returns `None` when `ROOT=` is absent, unresolved (still a placeholder),
/// or cannot be mapped to a partition-1 path.
fn efi_dev_from_root_cmdline() -> Option<String> {
    let cmdline = kernel_hal::boot::cmdline();
    let root_dev = parse_root_cmdline(&cmdline)?;
    // Strip trailing partition number. For NVMe paths (…p2) strip the 'p'
    // separator too; for sda/vda paths the separator is implicit.
    let without_digits = root_dev.trim_end_matches(|c: char| c.is_ascii_digit());
    if without_digits.len() == root_dev.len() {
        // No trailing digits — not a partition path we can map.
        return None;
    }
    if without_digits.ends_with('p') {
        // NVMe style: /dev/nvme0n1p2 → /dev/nvme0n1p1
        Some(format!("{}p1", &without_digits[..without_digits.len() - 1]))
    } else {
        // SATA/virtio style: /dev/sda2 → /dev/sda1, /dev/vda2 → /dev/vda1
        Some(format!("{}1", without_digits))
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
            vmo.write(offset, &buf[..read_len])
                .map_err(|_| rcore_fs::vfs::FsError::DeviceError)?;
            offset += read_len;
        }
        vmo.set_content_size(size)
            .map_err(|_| rcore_fs::vfs::FsError::DeviceError)?;
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
            if follow {
                let exe = self.execute_path();
                return self.lookup_inode_at(FileDesc::CWD, &exe, true);
            }
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
            if follow {
                // Magic link: resolve to the open file itself (like Linux),
                // so execve("/proc/self/fd/N") runs the file, not the
                // symlink's path text.
                return Ok(file.inode());
            }
            return Ok(Arc::new(Pseudo::new(file.path(), FileType::SymLink)));
        }

        let follow_max_depth = if follow { FOLLOW_MAX_DEPTH } else { 0 };
        if path.starts_with('/') {
            if let Some(result) = lookup_virtual_fs(path, follow_max_depth) {
                return result;
            }
        }
        if dirfd == FileDesc::CWD {
            Ok(self
                .root_inode()
                .lookup(&self.current_working_directory())?
                .lookup_follow(path, follow_max_depth)?)
        } else {
            let file = self.get_file(dirfd)?;
            if path.starts_with('/') {
                if let Some(result) = lookup_virtual_fs(path, follow_max_depth) {
                    return result;
                }
            }
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

/// Fast path for virtual filesystems mounted at `/proc`, `/sys`, and `/dev`.
/// Avoids ext2 directory scans on every access (VBox AHCI can stall there).
fn lookup_virtual_fs(path: &str, follow_times: usize) -> Option<LxResult<Arc<dyn INode>>> {
    let path = path.trim_end_matches('/');
    if path == "/proc" || path.starts_with("/proc/") {
        return Some(procfs::lookup_path(path, follow_times).map_err(LxError::from));
    }
    if path == "/sys" || path.starts_with("/sys/") {
        return Some(sysfs::lookup_path(path, follow_times).map_err(LxError::from));
    }
    if path == "/dev" || path.starts_with("/dev/") {
        let root = DEVFS_ROOT.lock().clone()?;
        let root: Arc<dyn INode> = root;
        if path == "/dev" {
            return Some(Ok(root));
        }
        let rest = path.strip_prefix("/dev/").unwrap();
        return Some(
            root.lookup_follow(rest, follow_times)
                .map_err(LxError::from),
        );
    }
    None
}

/// Rescans and registers partitions for a block device in devfs.
pub fn rescan_partitions(
    fname: &str,
    block: &Arc<dyn zcore_drivers::scheme::BlockScheme>,
    base_index: usize,
) -> LxResult<()> {
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
            if let Err(e) = devfs_root.add(
                &part_name,
                Arc::new(devfs::BlockDev::new(
                    part_dev_index,
                    partition_driver,
                    part_name.clone(),
                )),
            ) {
                warn!("failed to mknod /dev/{} during rescan: {:?}", &part_name, e);
            } else {
                info!(
                    "Rescanned and registered partition /dev/{} (start: {}, count: {})",
                    part_name, start_block, block_count
                );
            }
        }
    }
    Ok(())
}
