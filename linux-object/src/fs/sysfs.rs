//! Minimal sysfs implementation for Linux userland compatibility.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::Any;

use kernel_hal::drivers;
use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
use lock::Mutex;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};

use crate::fs::pseudo::Pseudo;

pub struct SysFS;

impl SysFS {
    pub fn new() -> Self {
        Self
    }
}

impl FileSystem for SysFS {
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        SYS_ROOT.clone()
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            bsize: 4096,
            frsize: 4096,
            blocks: 0,
            bfree: 0,
            bavail: 0,
            files: 0,
            ffree: 0,
            namemax: 255,
        }
    }
}

/// Inode numbers for the tree.
///
/// Every directory needs one of its own. They used to be literals spaced ten
/// apart, with the per-device ones written `40 + index` and `100 + index` --
/// spacing that holds until a machine has enough of the thing being counted,
/// and then silently stops: the eleventh PCI function is `/sys/devices/system`
/// again, and the first disk is `/sys/class/thermal`. Moebius's machine has
/// more than eleven functions, so that collision is not hypothetical. Here the
/// growing ranges are disjoint by construction instead of by arithmetic.
mod ino {
    pub const ROOT: usize = 1;
    pub const CLASS: usize = 2;
    pub const CLASS_DRM: usize = 3;
    pub const CLASS_INPUT: usize = 4;
    pub const CLASS_NET: usize = 5;
    pub const CLASS_POWER_SUPPLY: usize = 6;
    pub const CLASS_THERMAL: usize = 7;
    pub const THERMAL_ZONE: usize = 8;
    pub const THERMAL_COOLING: usize = 9;
    pub const BLOCK: usize = 10;
    pub const CLASS_BLOCK: usize = 23;
    pub const BUS: usize = 11;
    pub const BUS_PCI: usize = 12;
    pub const BUS_PCI_DEVICES: usize = 13;
    pub const DEVICES: usize = 14;
    pub const DEVICES_PCI_BUS: usize = 15;
    pub const DEVICES_SYSTEM: usize = 16;
    pub const SYSTEM_NODE: usize = 17;
    pub const SYSTEM_NODE0: usize = 18;
    pub const SYSTEM_CPU: usize = 19;
    pub const DEV: usize = 20;
    pub const DEV_CHAR: usize = 21;
    pub const POWER: usize = 22;

    /// Plus the device's index on the bus.
    pub const PCI_DEV: usize = 0x1000;
    /// Plus the disk's index.
    pub const BLOCK_DEV: usize = 0x2000;
    /// Plus the CPU's number.
    pub const CPU: usize = 0x3000;
    /// Plus the node's minor, which is unique across every GPU.
    pub const DRM_NODE: usize = 0x4000;
    /// Plus the owning device's index on the bus.
    pub const DRM_DIR: usize = 0x5000;
    /// Plus the event device's number.
    pub const INPUT_EVENT: usize = 0x6000;
    /// Plus the interface's index.
    pub const NET_IFACE: usize = 0x7000;
    /// The compute-only alias device, which has no index on the bus: its
    /// [`super::COMPUTE_ALIAS_INDEX`] is `usize::MAX`, a sentinel, so adding it
    /// to `PCI_DEV` or `DRM_DIR` overflows -- a panic in a build with overflow
    /// checks and a wrap to an arbitrary number in the kernel's, which is not
    /// the unique inode the sum was there to produce. One number each instead.
    pub const PCI_DEV_COMPUTE_ALIAS: usize = 0x8000;
    /// The `drm` directory of that same alias device.
    pub const DRM_DIR_COMPUTE_ALIAS: usize = 0x8001;
}

/// Where an interface sits in the list `/sys/class/net` shows, which is all
/// the identity it has: the node carries the name, not a number.
fn net_iface_index(name: &str) -> usize {
    list_net_ifnames()
        .iter()
        .position(|n| n == name)
        .unwrap_or(0)
}

/// The `id`-th name of a directory whose own contents are `names`.
///
/// A listing opens with `.` and `..`. Linux does it, and so does every other
/// filesystem this kernel mounts: `rcore-fs-ramfs` and `rcore-fs-devfs` both
/// answer those two at 0 and 1 and their own contents from 2. `getdents` here
/// goes straight through `get_entry` and synthesises nothing, so the
/// twenty-nine directories of this tree -- every one of which numbered from
/// its first real name -- returned neither. Both have always been reachable
/// by name, since every `find` below answers them; they were only invisible
/// to a listing, which is the harder half to notice.
fn nth_entry<S: AsRef<str>>(id: usize, names: &[S]) -> Result<String> {
    match id {
        0 => Ok(String::from(".")),
        1 => Ok(String::from("..")),
        i => names
            .get(i - 2)
            .map(|n| String::from(n.as_ref()))
            .ok_or(FsError::EntryNotFound),
    }
}

fn dir_metadata(inode: usize) -> Metadata {
    Metadata {
        dev: 0,
        inode,
        size: 0,
        blk_size: 0,
        blocks: 0,
        atime: Timespec { sec: 0, nsec: 0 },
        mtime: Timespec { sec: 0, nsec: 0 },
        ctime: Timespec { sec: 0, nsec: 0 },
        type_: FileType::Dir,
        mode: 0o555,
        nlinks: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
    }
}

/// The letters Linux gives the `index`-th disk of a family: `a`..`z`, then
/// `aa`..`az`, `ba`.. -- `sd_format_disk_name()` in `drivers/scsi/sd.c`.
///
/// It is bijective base-26, not ordinary base-26, so it never runs out and
/// never repeats. This used to be `b'a' + index % 26`, which wrapped: the
/// twenty-seventh disk was `sda` again, and since `block_index_by_name`
/// answers with the FIRST name that matches, everything addressed to it --
/// `/sys/block/sda/size`, a partition table, a mount -- landed on disk zero.
/// Twenty-seven disks is a lot for a desk and nothing for the kind of box
/// that gets an HBA.
fn disk_suffix(mut index: usize) -> String {
    let mut out = [0u8; 8];
    let mut at = out.len();
    loop {
        at -= 1;
        out[at] = b'a' + (index % 26) as u8;
        match (index / 26).checked_sub(1) {
            Some(next) => index = next,
            None => break,
        }
        if at == 0 {
            break;
        }
    }
    String::from_utf8_lossy(&out[at..]).into_owned()
}

fn list_block_devices() -> Vec<String> {
    let blocks = drivers::all_block().as_vec();
    let mut names = Vec::new();
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
            format!("vd{}", disk_suffix(virtio_idx))
        } else {
            let other_idx = blocks[..i]
                .iter()
                .filter(|b| !b.name().starts_with("nvme") && !b.name().starts_with("virtio"))
                .count();
            format!("sd{}", disk_suffix(other_idx))
        };
        names.push(fname);
    }
    names
}

fn block_index_by_name(name: &str) -> Option<usize> {
    list_block_devices().iter().position(|n| n.as_str() == name)
}

fn block_size_sectors(index: usize) -> Option<usize> {
    drivers::all_block()
        .as_vec()
        .get(index)
        .map(|b| b.block_count())
}

struct SysRootINode;

impl SysRootINode {
    fn entries() -> [&'static str; 6] {
        ["class", "block", "bus", "dev", "devices", "power"]
    }
}

impl INode for SysRootINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::ROOT))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | ".." => Ok(SYS_ROOT.clone()),
            "class" => Ok(Arc::new(SysClassINode)),
            "block" => Ok(Arc::new(SysBlockDirINode)),
            "bus" => Ok(Arc::new(SysBusDirINode)),
            "dev" => Ok(Arc::new(SysDevDirINode)),
            "devices" => Ok(Arc::new(SysDevicesDirINode)),
            "power" => Ok(Arc::new(SysPowerDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysClassINode;

impl SysClassINode {
    fn entries() -> [&'static str; 6] {
        ["block", "drm", "input", "net", "power_supply", "thermal"]
    }
}

impl INode for SysClassINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassINode)),
            ".." => Ok(Arc::new(SysRootINode)),
            "block" => Ok(Arc::new(SysClassBlockDirINode)),
            "drm" => Ok(Arc::new(SysClassDrmDirINode)),
            "input" => Ok(Arc::new(SysClassInputDirINode)),
            "net" => Ok(Arc::new(SysClassNetDirINode)),
            "power_supply" => Ok(Arc::new(SysClassPowerSupplyDirINode)),
            "thermal" => Ok(Arc::new(SysClassThermalDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysBlockDirINode;

impl INode for SysBlockDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::BLOCK))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysBlockDirINode)),
            ".." => Ok(Arc::new(SysRootINode)),
            _ => {
                if let Some(index) = block_index_by_name(name) {
                    Ok(Arc::new(SysBlockDevINode { index }))
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &list_block_devices())
    }
}

struct SysBlockDevINode {
    index: usize,
}

impl SysBlockDevINode {
    fn entries() -> [&'static str; 2] {
        ["size", "removable"]
    }
}

impl INode for SysBlockDevINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::BLOCK_DEV + self.index))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysBlockDevINode { index: self.index })),
            ".." => Ok(Arc::new(SysBlockDirINode)),
            "size" => {
                let sectors = block_size_sectors(self.index).ok_or(FsError::EntryNotFound)?;
                Ok(Arc::new(Pseudo::new(
                    &format!("{}\n", sectors),
                    FileType::File,
                )))
            }
            "removable" => Ok(Arc::new(Pseudo::new("0\n", FileType::File))),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

/// `/sys/class/block`, which is not `/sys/block` again.
///
/// Both exist in Linux and both list the disks, but they are two directories:
/// the names under `class/` are symlinks to the canonical ones, exactly as
/// `/sys/class/drm/card0` is a symlink here already. Mounting one directory at
/// both places instead gave `/sys/class/block` the inode of `/sys/block` and,
/// worse, its `..` -- so walking up from a disk found under `class/` landed in
/// `/sys`, skipping `/sys/class`. That walk is how libudev names a device's
/// subsystem.
struct SysClassBlockDirINode;

impl INode for SysClassBlockDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS_BLOCK))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassBlockDirINode)),
            ".." => Ok(Arc::new(SysClassINode)),
            _ => {
                if block_index_by_name(name).is_some() {
                    Ok(Arc::new(Pseudo::new(
                        &format!("../../block/{}", name),
                        FileType::SymLink,
                    )))
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &list_block_devices())
    }
}

struct SysBusDirINode;

impl INode for SysBusDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::BUS))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysBusDirINode)),
            ".." => Ok(Arc::new(SysRootINode)),
            "pci" => Ok(Arc::new(SysBusPciDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &["pci"])
    }
}

struct SysBusPciDirINode;

impl INode for SysBusPciDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::BUS_PCI))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysBusPciDirINode)),
            ".." => Ok(Arc::new(SysBusDirINode)),
            "devices" => Ok(Arc::new(SysPciDevicesDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &["devices"])
    }
}

struct SysDevicesDirINode;

impl INode for SysDevicesDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::DEVICES))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDevicesDirINode)),
            ".." => Ok(Arc::new(SysRootINode)),
            "pci0000:00" => Ok(Arc::new(SysDevicesPciBusDirINode)),
            "system" => Ok(Arc::new(SysDevicesSystemDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &["pci0000:00", "system"])
    }
}

struct SysDevicesSystemDirINode;

impl INode for SysDevicesSystemDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::DEVICES_SYSTEM))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDevicesSystemDirINode)),
            ".." => Ok(Arc::new(SysDevicesDirINode)),
            "node" => Ok(Arc::new(SysDevicesSystemNodeDirINode)),
            "cpu" => Ok(Arc::new(SysDevicesSystemCpuDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &["node", "cpu"])
    }
}

struct SysDevicesSystemNodeDirINode;

impl INode for SysDevicesSystemNodeDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::SYSTEM_NODE))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDevicesSystemNodeDirINode)),
            ".." => Ok(Arc::new(SysDevicesSystemDirINode)),
            "node0" => Ok(Arc::new(SysDevicesSystemNode0DirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &["node0"])
    }
}

struct SysDevicesSystemNode0DirINode;

impl SysDevicesSystemNode0DirINode {
    fn entries() -> [&'static str; 2] {
        ["cpulist", "cpumap"]
    }
}

impl INode for SysDevicesSystemNode0DirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::SYSTEM_NODE0))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDevicesSystemNode0DirINode)),
            ".." => Ok(Arc::new(SysDevicesSystemNodeDirINode)),
            "cpulist" => {
                let cpu_count = kernel_hal::cpu::cpu_count() as usize;
                let cpulist = if cpu_count <= 1 {
                    String::from("0\n")
                } else {
                    format!("0-{}\n", cpu_count - 1)
                };
                Ok(Arc::new(Pseudo::new(&cpulist, FileType::File)))
            }
            "cpumap" => {
                let cpu_count = kernel_hal::cpu::cpu_count() as usize;
                let mut cpumap = String::new();
                let num_groups = cpu_count.div_ceil(32);
                for g in (0..num_groups).rev() {
                    let mut group_val = 0u32;
                    for i in 0..32 {
                        let cpu_idx = g * 32 + i;
                        if cpu_idx < cpu_count {
                            group_val |= 1 << i;
                        }
                    }
                    if !cpumap.is_empty() {
                        cpumap.push(',');
                    }
                    cpumap.push_str(&format!("{:08x}", group_val));
                }
                cpumap.push('\n');
                Ok(Arc::new(Pseudo::new(&cpumap, FileType::File)))
            }
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysDevicesPciBusDirINode;

impl INode for SysDevicesPciBusDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::DEVICES_PCI_BUS))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        log::trace!("SysDevicesPciBusDirINode::find name={}", name);
        if name == "." {
            return Ok(Arc::new(SysDevicesPciBusDirINode));
        }
        if name == ".." {
            return Ok(Arc::new(SysDevicesDirINode));
        }
        let devices = get_pci_devices();
        if let Some((idx, _dev)) = devices.iter().enumerate().find(|(_, d)| d.name == name) {
            log::trace!("SysDevicesPciBusDirINode::find name={} -> found", name);
            pci_dev_inode(idx)
        } else if name == COMPUTE_ALIAS_BDF && compute_alias_needed() {
            pci_dev_inode(COMPUTE_ALIAS_INDEX)
        } else {
            log::trace!(
                "SysDevicesPciBusDirINode::find name={} -> EntryNotFound",
                name
            );
            Err(FsError::EntryNotFound)
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &pci_device_names())
    }
}

struct SysPciDevicesDirINode;

impl INode for SysPciDevicesDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::BUS_PCI_DEVICES))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        log::trace!("SysPciDevicesDirINode::find name={}", name);
        if name == "." {
            return Ok(Arc::new(SysPciDevicesDirINode));
        }
        if name == ".." {
            return Ok(Arc::new(SysBusPciDirINode));
        }
        let devices = get_pci_devices();
        if let Some(dev) = devices.iter().find(|d| d.name == name) {
            let target = format!("../../../devices/pci0000:00/{}", dev.name);
            log::trace!(
                "SysPciDevicesDirINode::find name={} -> target={}",
                name,
                target
            );
            Ok(Arc::new(Pseudo::new(&target, FileType::SymLink)))
        } else if name == COMPUTE_ALIAS_BDF && compute_alias_needed() {
            Ok(Arc::new(Pseudo::new(
                &format!("../../../devices/pci0000:00/{}", COMPUTE_ALIAS_BDF),
                FileType::SymLink,
            )))
        } else {
            log::trace!("SysPciDevicesDirINode::find name={} -> EntryNotFound", name);
            Err(FsError::EntryNotFound)
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &pci_device_names())
    }
}

struct SysPciDevDirINode {
    index: usize,
    name: String,
    vendor: String,
    device: String,
    class: String,
}

impl SysPciDevDirINode {
    fn ids(&self) -> PciIds {
        PciIds::parse(&self.vendor, &self.device, &self.class)
    }
}

impl INode for SysPciDevDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(if self.index == COMPUTE_ALIAS_INDEX {
            ino::PCI_DEV_COMPUTE_ALIAS
        } else {
            ino::PCI_DEV + self.index
        }))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        log::trace!(
            "SysPciDevDirINode::find name={} self.name={}",
            name,
            self.name
        );
        match name {
            "." => Ok(Arc::new(SysPciDevDirINode {
                index: self.index,
                name: self.name.clone(),
                vendor: self.vendor.clone(),
                device: self.device.clone(),
                class: self.class.clone(),
            })),
            ".." => Ok(Arc::new(SysDevicesPciBusDirINode)),
            "vendor" => Ok(Arc::new(Pseudo::new(
                &format!("{}\n", self.vendor),
                FileType::File,
            ))),
            "device" => Ok(Arc::new(Pseudo::new(
                &format!("{}\n", self.device),
                FileType::File,
            ))),
            "class" => Ok(Arc::new(Pseudo::new(
                &format!("{}\n", self.class),
                FileType::File,
            ))),
            "uevent" => Ok(Arc::new(Pseudo::new(
                &self.ids().uevent(&self.name),
                FileType::File,
            ))),
            "config" => {
                let ids = self.ids();
                let mut cfg = [0u8; 256];

                cfg[0..2].copy_from_slice(&(ids.vendor as u16).to_le_bytes());
                cfg[2..4].copy_from_slice(&(ids.device as u16).to_le_bytes());

                cfg[9] = ids.prog_if();
                cfg[10] = ids.subclass();
                cfg[11] = ids.base_class();

                Ok(Arc::new(Pseudo::new_bytes(cfg.to_vec(), FileType::File)))
            }
            "modalias" => Ok(Arc::new(Pseudo::new(
                &self.ids().modalias(),
                FileType::File,
            ))),
            // libdrm's drmParseSubsystemType() readlink()s `<dev>/subsystem`
            // and takes the basename ("pci") to classify the bus; without it
            // drmGetDevice2() can't determine the device type.
            "subsystem" => Ok(Arc::new(Pseudo::new("../../../bus/pci", FileType::SymLink))),
            "revision" => Ok(Arc::new(Pseudo::new("0x00\n", FileType::File))),
            "subsystem_vendor" => Ok(Arc::new(Pseudo::new("0x0000\n", FileType::File))),
            "subsystem_device" => Ok(Arc::new(Pseudo::new("0x0000\n", FileType::File))),
            // `<dev>/drm/` lists this device's DRM nodes. Only the PCI
            // device that actually backs a DRM node exposes the directory —
            // advertising card0 on every PCI function (host bridge, console
            // GPU, ...) makes libdrm merge the wrong render node.
            "drm" if !drm_nodes_for_pci_index(self.index).is_empty() => {
                Ok(Arc::new(SysDrmDeviceDrmDirINode {
                    pci_index: self.index,
                }))
            }
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let mut entries: Vec<&str> = vec![
            "vendor",
            "device",
            "class",
            "config",
            "uevent",
            "modalias",
            "subsystem",
            "revision",
            "subsystem_vendor",
            "subsystem_device",
        ];
        if !drm_nodes_for_pci_index(self.index).is_empty() {
            entries.push("drm");
        }
        nth_entry(id, &entries)
    }
}

#[derive(Clone)]
struct PciDevInfo {
    name: String,
    vendor: String,
    device: String,
    class: String,
}

/// What `/sys/bus/pci/devices` and `/sys/devices/pci0000:00` list: every
/// device on the bus, plus the synthetic function that names a second GPU as
/// a compute device when one is needed.
fn pci_device_names() -> Vec<String> {
    let mut names: Vec<String> = get_pci_devices().iter().map(|d| d.name.clone()).collect();
    if compute_alias_needed() {
        names.push(String::from(COMPUTE_ALIAS_BDF));
    }
    names
}

/// The three numbers a PCI device is identified by, decoded once.
///
/// They reach this tree as the `0x`-prefixed strings `scan_pci_devices` wrote,
/// and three different places used to pick them apart again: `config` and
/// `modalias` parsed them, `uevent` trimmed the prefix off and published the
/// text as it stood. So the same decision was written three times and the odd
/// one out disagreed with the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PciIds {
    vendor: u32,
    device: u32,
    class: u32,
}

impl PciIds {
    fn parse(vendor: &str, device: &str, class: &str) -> Self {
        fn hex(s: &str) -> u32 {
            u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(0)
        }
        PciIds {
            vendor: hex(vendor),
            device: hex(device),
            class: hex(class),
        }
    }

    fn base_class(&self) -> u8 {
        (self.class >> 16) as u8
    }

    fn subclass(&self) -> u8 {
        (self.class >> 8) as u8
    }

    fn prog_if(&self) -> u8 {
        self.class as u8
    }

    /// What Linux's `modalias_show()` writes, **in upper case**.
    ///
    /// `file2alias` builds every `pci:v...` pattern in `modules.alias` with
    /// `%02X`, and `kmod` matches the two with `fnmatch`, which respects case.
    /// So a lower-case modalias matched nothing for any device whose ids carry
    /// a hex letter -- `10de` (NVIDIA) and `8086` are the two on this desk, and
    /// the first of them is all letters.
    fn modalias(&self) -> String {
        format!(
            "pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}\n",
            self.vendor,
            self.device,
            0,
            0,
            self.base_class(),
            self.subclass(),
            self.prog_if()
        )
    }

    /// What Linux's `pci_uevent()` adds to the environment, same case and same
    /// widths -- `%04X`, so a class of `0x030000` prints as five digits, not
    /// six. The `MODALIAS=` line was missing altogether, and it is the one
    /// systemd-udevd's `kmod` builtin reads; the `modalias` file beside it is
    /// for a human with `cat`.
    fn uevent(&self, slot: &str) -> String {
        format!(
            "PCI_CLASS={:04X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\nPCI_SLOT_NAME={}\nMODALIAS={}",
            self.class,
            self.vendor,
            self.device,
            0,
            0,
            slot,
            self.modalias()
        )
    }
}

fn display_pci_index() -> Option<usize> {
    let devs = get_pci_devices();
    // With the nouveau uAPI enabled there is a real NVIDIA GPU the driver bound
    // to, and the DRM node (card0/renderD128) MUST back onto that GPU: its
    // sysfs `device` symlink, `vendor`, `config` etc. are what libdrm reads to
    // identify the device. The plain "first display-class device" scan below is
    // wrong on any board that also carries an integrated GPU — the iGPU usually
    // enumerates first, so sysfs would report the iGPU's 0x8086/0x1002 vendor
    // for our render node. NVK filters candidate DRM devices by PCI vendor
    // 0x10de *before* opening them (it never reaches nouveau_ws_device_new, so
    // not one nouveau ioctl is issued); a non-NVIDIA vendor there makes NVK skip
    // the node entirely and vkEnumeratePhysicalDevices returns 0 GPUs. Prefer
    // the NVIDIA display-class device so the render node advertises the RTX.
    // The node's identity must describe the SAME GPU that serves its ioctls.
    // On a multi-GPU box those can disagree: the ioctl target is
    // `devfs::drm::get_primary_driver()`, while this function used to pick the
    // first display-class device in PCI scan order. libdrm feeds NVK the sysfs
    // side (bus info from uevent's PCI_SLOT_NAME, ids from `config`) while
    // every GETPARAM/NVIF answer comes from the driver side, so a mismatch
    // reports one card's PCI id with another card's VRAM size and topology.
    // (Mesa does assert the two device_ids agree, but Alpine builds with
    // `-Db_ndebug=true`, so that check is compiled out and the inconsistency
    // is silent.)
    //
    // Match on the BDF NUMBERS, never on the driver's display name: names
    // render the bus in DECIMAL ("nvidia-gpu-101:0.0") while sysfs paths use
    // HEX ("0000:65:00.0").
    if let Some((_dom, bus, dev, func)) =
        crate::fs::devfs::drm::get_primary_driver().and_then(|d| d.pci_bdf())
    {
        let want = alloc::format!("0000:{:02x}:{:02x}.{:x}", bus, dev, func);
        if let Some(idx) = devs.iter().position(|d| d.name == want) {
            return Some(idx);
        }
        log::warn!(
            "[drm] primary DRM driver reports PCI {} but the sysfs PCI scan has no such device -- \
             falling back to scan order",
            want
        );
    }
    if zcore_drivers::display::nouveau_uapi_enabled() {
        if let Some(idx) = devs
            .iter()
            .position(|d| d.class.starts_with("0x03") && d.vendor == "0x10de")
        {
            return Some(idx);
        }
    }
    devs.iter()
        .position(|d| d.class.starts_with("0x03"))
        .or_else(|| (!devs.is_empty()).then_some(0))
}

/// Whether `/sys/class/drm/card0` should be exposed, and which PCI device (if
/// any) backs it. card0 exists whenever `/dev/dri/card0` does — i.e. there is a
/// real PCI GPU, a registered framebuffer display (UEFI GOP has no PCI GPU
/// node), or a DRM driver. The PCI index is best-effort, used only for the
/// `device`/`modalias` attributes.
fn drm_card0_pci_index() -> Option<usize> {
    if let Some(idx) = display_pci_index() {
        return Some(idx);
    }
    let have_fb =
        drivers::all_display().first().is_some() || !drivers::all_drm().as_vec().is_empty();
    have_fb.then_some(0)
}

fn pci_index_for_bdf(bus: u8, dev: u8, func: u8) -> Option<usize> {
    let want = alloc::format!("0000:{:02x}:{:02x}.{:x}", bus, dev, func);
    get_pci_devices().iter().position(|d| d.name == want)
}

/// Sentinel PCI sysfs index for the compute-only DRM nodes when they would
/// otherwise share the compute GPU's BDF with card0/renderD128.
///
/// libdrm's `drmGetDevices2` merges every `/dev/dri/card*` + `renderD*` with
/// the same PCI businfo into one `drmDevice`, last node of each type wins.
/// card1/renderD129 are named `eclipse-compute` so Mesa/NVK skip them; if they
/// share the compute GPU's BDF with card0, that last-wins pass replaces
/// renderD128 (nouveau) with renderD129 and wlroots GLES2 dies with
/// `Failed to get DRM device: No such device` / `Failed to create GBM device`.
/// A distinct fake BDF keeps the pairs as two drmDevices. NVK filters the
/// alias by vendor `0x0000`. Do not point card0 at the console GPU.
///
/// The alias is NOT a display controller. libdrm only needs its `config`
/// (vendor/device/revision) and the `subsystem` link; it never reads the PCI
/// class. But userspace GPU inventories do: `fastfetch`, `lspci`-style tools
/// and anything walking `/sys/bus/pci/devices` count every base-class `0x03`
/// entry as a GPU, and with the alias advertised as `0x038000` a 2-GPU board
/// showed a third "Unknown Device 0000". Base class `0x12` ("Processing
/// accelerators") describes a compute-only node honestly and is skipped by
/// every GPU enumerator.
const COMPUTE_ALIAS_INDEX: usize = usize::MAX;
const COMPUTE_ALIAS_BDF: &str = "0000:ee:00.0";
const COMPUTE_ALIAS_CLASS: &str = "0x120000";

/// Whether the fake PCI device has to appear in `/sys/bus/pci/devices`.
///
/// Derived from the node table rather than recomputed: it must be true
/// exactly when some node's `device` symlink points at the alias, or that
/// symlink dangles and libdrm fails to identify the node. Asking
/// [`drm_nodes_for_pci_index`] is the same question the symlinks answer.
fn compute_alias_needed() -> bool {
    !drm_nodes_for_pci_index(COMPUTE_ALIAS_INDEX).is_empty()
}

fn pci_bdf_name(index: usize) -> Option<String> {
    if index == COMPUTE_ALIAS_INDEX {
        return Some(COMPUTE_ALIAS_BDF.into());
    }
    get_pci_devices().get(index).map(|d| d.name.clone())
}

fn pci_dev_inode(index: usize) -> Result<Arc<dyn INode>> {
    if index == COMPUTE_ALIAS_INDEX {
        return Ok(Arc::new(SysPciDevDirINode {
            index: COMPUTE_ALIAS_INDEX,
            name: COMPUTE_ALIAS_BDF.into(),
            vendor: "0x0000".into(),
            device: "0x0000".into(),
            class: COMPUTE_ALIAS_CLASS.into(),
        }));
    }
    let devices = get_pci_devices();
    let dev = devices.get(index).ok_or(FsError::EntryNotFound)?;
    Ok(Arc::new(SysPciDevDirINode {
        index,
        name: dev.name.clone(),
        vendor: dev.vendor.clone(),
        device: dev.device.clone(),
        class: dev.class.clone(),
    }))
}

/// Parse a DRM node name back into its minor: `card{n}` -> n, `renderD{n}`
/// -> n. Anything else is not a DRM node.
///
/// Accepts a name only if it is the one [`crate::fs::devfs::drm::node_name`]
/// would produce for that minor, so a lookup of `card007` or `renderD0129`
/// does not silently resolve to a node that is not spelled that way anywhere
/// else in the tree.
fn drm_minor_from_name(name: &str) -> Option<u32> {
    let digits = name
        .strip_prefix("renderD")
        .or_else(|| name.strip_prefix("card"))?;
    let minor = digits.parse::<u32>().ok()?;
    (crate::fs::devfs::drm::node_name(minor) == name).then_some(minor)
}

/// The sysfs PCI index that backs a DRM node, or `None` when that minor has
/// no node.
///
/// `card0`/`renderD128` keep their own resolution ([`drm_card0_pci_index`],
/// which prefers the primary driver's BDF and falls back to the display-class
/// scan), because that is the node whose identity NVK filters on. Every other
/// node resolves through the `/dev/dri` table, so a node's sysfs identity and
/// the GPU serving its ioctls can no longer name different cards.
fn drm_node_pci_index(minor: u32) -> Option<usize> {
    if minor == 0 || minor == crate::fs::devfs::drm::RENDER_MINOR_BASE {
        return drm_card0_pci_index();
    }
    let driver = crate::fs::devfs::drm::driver_for_minor(minor)?;
    let (_, bus, dev, func) = driver.pci_bdf()?;
    let idx = pci_index_for_bdf(bus, dev, func)?;
    // Only a node that would share card0's PCI device needs the alias; the
    // GPUs at index 2 and up are distinct cards with distinct BDFs, so they
    // keep their real one and libdrm sees them as separate devices already.
    if drm_card0_pci_index() == Some(idx) {
        Some(COMPUTE_ALIAS_INDEX)
    } else {
        Some(idx)
    }
}

/// The DRM node minors whose sysfs identity lives under this PCI device, in
/// `card` then `renderD` order per GPU.
///
/// This replaces a fixed `Primary` / `ComputeOnly` role: with N cards a PCI
/// device is simply whichever node pairs resolve onto it.
///
/// Enumerated from [`drm_class_entries`], NOT from the `/dev/dri` GPU table.
/// The table is built from the registered DRM drivers, and `card0` exists
/// without one: a UEFI GOP framebuffer has a display but no DRM driver and no
/// PCI GPU node, which is the ordinary QEMU case and the software-KMS path
/// labwc drives. Reading the table here made `/sys/devices/pci.../drm`
/// disappear on exactly those machines, which dangles the
/// `/sys/class/drm/card0` symlink, so `drmGetDevices2` identifies no device
/// and the compositor finds no card to open -- a desktop that never appears.
/// [`drm_class_entries`] already treats `card0`/`renderD128` as existing
/// whenever [`drm_card0_pci_index`] resolves, which is the pre-table rule.
fn drm_nodes_for_pci_index(pci_index: usize) -> Vec<u32> {
    drm_class_entries()
        .into_iter()
        .filter(|m| drm_node_pci_index(*m) == Some(pci_index))
        .collect()
}

/// The DRM minors that have a sysfs node, in listing order: `card0`,
/// `renderD128`, then each further GPU's `card{n}` and `renderD{128+n}`.
///
/// `/sys/class/drm` and `/sys/dev/char` both enumerate from this, so the two
/// listings cannot go out of step, and neither can drift from `/dev/dri`.
/// `card0` and `renderD128` are listed whenever [`drm_card0_pci_index`]
/// resolves, with no GPU table involved: `/dev/dri/card0` is created on the
/// same condition (a display OR a DRM driver), so a framebuffer-only machine
/// has the node and must have its sysfs identity too.
fn drm_class_entries() -> Vec<u32> {
    if drm_card0_pci_index().is_none() {
        return Vec::new();
    }
    let mut out = vec![0, crate::fs::devfs::drm::RENDER_MINOR_BASE];
    for node in crate::fs::devfs::drm::gpu_nodes().iter().skip(1) {
        if drm_node_pci_index(node.card_minor()).is_some() {
            out.push(node.card_minor());
            out.push(node.render_minor());
        }
    }
    out
}

/// The sysfs inode for a DRM node, as a symlink into `/sys/devices` when the
/// PCI device has a BDF name (what libudev insists on) and as the node
/// directory itself otherwise.
fn drm_node_inode(minor: u32) -> Result<Arc<dyn INode>> {
    let idx = drm_node_pci_index(minor).ok_or(FsError::EntryNotFound)?;
    match pci_bdf_name(idx) {
        Some(bdf) => Ok(drm_devices_symlink(
            &crate::fs::devfs::drm::node_name(minor),
            &bdf,
        )),
        None => Ok(Arc::new(SysDrmNodeINode::new(idx, minor))),
    }
}

/// Linux: `/sys/class/drm/card0` and `/sys/dev/char/226:0` are symlinks into
/// `/sys/devices/pci.../drm/card0`. libudev's `udev_device_new_from_devnum`
/// rejects a syspath that is not under `/sys/devices` (`ENODEV`) — the exact
/// wlroots `Failed to get DRM device: No such device` on GLES2/EGL.
fn drm_devices_symlink(devname: &str, bdf: &str) -> Arc<dyn INode> {
    Arc::new(Pseudo::new(
        &format!("../../devices/pci0000:00/{}/drm/{}", bdf, devname),
        FileType::SymLink,
    ))
}

/// One-shot boot diagnostic, only meaningful with the nouveau uAPI enabled:
/// dump the PCI inventory and which device the DRM render node backs onto.
/// On real hardware this is the fastest way to catch the render node pointing
/// at the wrong GPU (e.g. an integrated GPU winning the display-class scan),
/// which makes NVK's PCI-vendor filter skip our node before any nouveau ioctl.
/// Logged at `warn` so it survives the default boot log level.
pub(crate) fn log_drm_pci_backing() {
    let devs = get_pci_devices();
    // klog_info!, not log::warn!: real-hardware builds default to LOG=error,
    // which drops warn -- and this whole diagnostic is only reachable on the
    // opt-in nouveau experiment anyway, so make it survive the quiet level the
    // same way the "graphics: drm[N]" inventory line does.
    for (i, d) in devs.iter().enumerate() {
        kernel_hal::klog_info!(
            "[drm-probe] PCI[{}] {} vendor={} class={}",
            i,
            d.name,
            d.vendor,
            d.class
        );
    }
    // Print the ioctl target next to the sysfs identity: on a multi-GPU box
    // these must name the SAME card, and that is exactly what a single boot
    // log can now confirm.
    match crate::fs::devfs::drm::get_primary_driver() {
        Some(d) => kernel_hal::klog_info!(
            "[drm-probe] ioctl target (primary driver) = {:?} pci_bdf={:x?} console_gpu={}",
            d.name(),
            d.pci_bdf(),
            d.is_console_gpu()
        ),
        None => kernel_hal::klog_info!("[drm-probe] no primary DRM driver registered"),
    }
    let idx = drm_card0_pci_index();
    match idx.and_then(|i| devs.get(i)) {
        Some(d) => kernel_hal::klog_info!(
            "[drm-probe] render node backed by PCI[{:?}] {} vendor={} (NVK requires vendor=0x10de)",
            idx,
            d.name,
            d.vendor
        ),
        None => {
            kernel_hal::klog_info!("[drm-probe] render node has NO PCI backing (idx={:?})", idx)
        }
    }
    // One line per compute-only node pair: which sysfs PCI device backs it,
    // and whether that is the fake BDF that keeps libdrm from merging it into
    // card0's device. With three or more cards there is more than one pair, so
    // this enumerates rather than describing "the" compute node.
    for node in crate::fs::devfs::drm::gpu_nodes().iter().skip(1) {
        let Some(idx) = drm_node_pci_index(node.card_minor()) else {
            kernel_hal::klog_info!(
                "[drm-probe] {} / {} have NO sysfs PCI backing -- libdrm cannot identify them",
                crate::fs::devfs::drm::node_name(node.card_minor()),
                crate::fs::devfs::drm::node_name(node.render_minor()),
            );
            continue;
        };
        kernel_hal::klog_info!(
            "[drm-probe] compute-only nodes {} / {} sysfs PCI index={:?} bdf={} (alias={} so they do not merge with card0)",
            crate::fs::devfs::drm::node_name(node.card_minor()),
            crate::fs::devfs::drm::node_name(node.render_minor()),
            idx,
            pci_bdf_name(idx).unwrap_or_else(|| "<none>".into()),
            idx == COMPUTE_ALIAS_INDEX
        );
    }
    // Actively resolve the EXACT sysfs chain libdrm's drmGetDevices2 walks, so
    // one boot tells us whether it resolves at runtime and what it reports --
    // no userspace probe needed. A dangling `device` symlink (the PCI scan
    // missed the GPU) or a vendor != 0x10de is what makes NVK skip the node
    // before it issues a single ioctl.
    fn read_small(n: &Arc<dyn INode>) -> String {
        let mut b = [0u8; 64];
        match n.read_at(0, &mut b) {
            Ok(l) => String::from_utf8_lossy(&b[..l]).trim().into(),
            Err(_) => "<read-err>".into(),
        }
    }
    match SYS_ROOT.lookup_follow("dev/char/226:128/device", 40) {
        Ok(pcidir) => {
            let vendor = pcidir
                .find("vendor")
                .map(|n| read_small(&n))
                .unwrap_or_else(|_| "<no-vendor>".into());
            let subsystem = pcidir
                .find("subsystem")
                .map(|n| read_small(&n))
                .unwrap_or_else(|_| "<no-subsystem>".into());
            kernel_hal::klog_info!(
                "[drm-probe] sysfs chain resolves: renderD128/device -> vendor={} subsystem={} (libdrm CAN identify the node; needs vendor=0x10de + subsystem .../bus/pci)",
                vendor,
                subsystem
            );
        }
        Err(e) => kernel_hal::klog_info!(
            "[drm-probe] sysfs chain BROKEN: /sys/dev/char/226:128/device does not resolve ({:?}) -- libdrm cannot read vendor/subsystem, so NVK skips the node",
            e
        ),
    }
}

fn list_net_ifnames() -> Vec<String> {
    let ifaces = get_net_device();
    if ifaces.is_empty() {
        vec!["lo".into()]
    } else {
        ifaces.iter().map(|i| i.get_ifname()).collect()
    }
}

struct SysClassDrmDirINode;

impl INode for SysClassDrmDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS_DRM))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassDrmDirINode)),
            ".." => Ok(Arc::new(SysClassINode)),
            _ => drm_node_inode(drm_minor_from_name(name).ok_or(FsError::EntryNotFound)?),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        if drm_card0_pci_index().is_none() {
            return nth_entry(id, &[] as &[&str]);
        }
        // card0, renderD128, then each further GPU's pair -- the same order
        // and the same set as `/dev/dri`, because both read one table.
        let names: Vec<String> = drm_class_entries()
            .iter()
            .map(|m| crate::fs::devfs::drm::node_name(*m))
            .collect();
        nth_entry(id, &names)
    }
}

/// A DRM device node in sysfs: a primary node `card{n}` or a render node
/// `renderD{128+n}`. Both of a GPU's nodes share its backing PCI device.
struct SysDrmNodeINode {
    pci_index: usize,
    minor: u32,
}

impl SysDrmNodeINode {
    fn new(pci_index: usize, minor: u32) -> Self {
        Self { pci_index, minor }
    }
    /// `dri/<devname>`, derived from the minor rather than carried alongside
    /// it: a name and a minor stored separately are two things that can
    /// disagree, and `uevent`'s DEVNAME is exactly where that would be
    /// invisible.
    fn devname(&self) -> String {
        crate::fs::devfs::drm::node_name(self.minor)
    }
    fn entries() -> [&'static str; 4] {
        ["dev", "uevent", "device", "subsystem"]
    }
}

impl INode for SysDrmNodeINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::DRM_NODE + self.minor as usize))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDrmNodeINode::new(self.pci_index, self.minor))),
            // Canonical node lives at /sys/devices/pci0000:00/<BDF>/drm/<name>.
            // Parent walks (libdrm/udev) must see the BDF-named PCI directory.
            ".." => Ok(Arc::new(SysDrmDeviceDrmDirINode {
                pci_index: self.pci_index,
            })),
            // libdrm/libudev read `dev` (major:minor) and `uevent`
            // (DEVNAME/MAJOR/MINOR). DRM major is 226.
            "dev" => Ok(Arc::new(Pseudo::new(
                &format!("226:{}\n", self.minor),
                FileType::File,
            ))),
            "uevent" => Ok(Arc::new(Pseudo::new(
                &format!(
                    "MAJOR=226\nMINOR={}\nDEVNAME=dri/{}\n",
                    self.minor,
                    self.devname()
                ),
                FileType::File,
            ))),
            // From .../pci0000:00/<BDF>/drm/card0, five `..` reach /sys.
            "subsystem" => Ok(Arc::new(Pseudo::new(
                "../../../../../class/drm",
                FileType::SymLink,
            ))),
            // `<card>/device` must resolve (via realpath) to a PCI-BDF-named
            // directory so libdrm's drmGetDevice2() can parse the bus info —
            // otherwise Mesa/wlroots fail with "failed to retrieve device
            // information" / "Failed to get DRM device".
            "device" => {
                let bdf = pci_bdf_name(self.pci_index).ok_or(FsError::EntryNotFound)?;
                Ok(Arc::new(Pseudo::new(
                    &format!("../../../{}", bdf),
                    FileType::SymLink,
                )))
            }
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

/// `<pci-dev>/drm/` — lists this device's DRM nodes. libdrm's
/// `drmGetRenderDeviceNameFromFd()` scans it for the `renderD*` entry.
/// Primary (card0/renderD128) and compute-only (card1/renderD129) never
/// share this directory: if they did, libdrm would last-wins the render
/// node onto eclipse-compute and GBM/EGL would fail.
struct SysDrmDeviceDrmDirINode {
    pci_index: usize,
}

impl INode for SysDrmDeviceDrmDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(if self.pci_index == COMPUTE_ALIAS_INDEX {
            ino::DRM_DIR_COMPUTE_ALIAS
        } else {
            ino::DRM_DIR + self.pci_index
        }))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDrmDeviceDrmDirINode {
                pci_index: self.pci_index,
            })),
            ".." => pci_dev_inode(self.pci_index),
            _ => {
                let minor = drm_minor_from_name(name).ok_or(FsError::EntryNotFound)?;
                if drm_node_pci_index(minor) != Some(self.pci_index) {
                    return Err(FsError::EntryNotFound);
                }
                Ok(Arc::new(SysDrmNodeINode::new(self.pci_index, minor)))
            }
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let names: Vec<String> = drm_nodes_for_pci_index(self.pci_index)
            .iter()
            .map(|m| crate::fs::devfs::drm::node_name(*m))
            .collect();
        nth_entry(id, &names)
    }
}

// ---------------------------------------------------------------------------
// `/sys/dev/char/<major>:<minor>` — the reverse map from a device number to its
// sysfs node. libdrm's drmGetDeviceNameFromFd2() fstat()s the card fd and reads
// `/sys/dev/char/226:0/uevent` for DEVNAME; without this it fails with ENOENT
// ("drmGetDeviceNameFromFd2() failed: No such file or directory") and wlroots
// cannot create the DRM backend. We map the relevant device numbers onto the
// existing class nodes (which already carry uevent/dev/subsystem).
// ---------------------------------------------------------------------------

struct SysDevDirINode;

impl INode for SysDevDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::DEV))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDevDirINode)),
            ".." => Ok(Arc::new(SysRootINode)),
            "char" => Ok(Arc::new(SysDevCharDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &["char"])
    }
}

struct SysDevCharDirINode;

impl INode for SysDevCharDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::DEV_CHAR))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        if name == "." {
            return Ok(Arc::new(SysDevCharDirINode));
        }
        if name == ".." {
            return Ok(Arc::new(SysDevDirINode));
        }
        // DRM nodes: Linux makes these *symlinks* into
        // `/sys/devices/pci.../drm/<name>` so realpath() lands under
        // `/sys/devices` (libudev requires that; a directory here yields
        // ENODEV from drmGetDevice2). A compute node that would otherwise
        // share card0's PCI device gets a distinct alias BDF instead, so
        // libdrm does not merge the two pairs into one device.
        //
        // Every DRM minor with a node resolves here, not just the four that
        // used to be spelled out, so a third GPU's `226:2` / `226:130` is
        // reachable the same way.
        if let Some(minor) = name
            .strip_prefix("226:")
            .and_then(|m| m.parse::<u32>().ok())
        {
            if drm_node_pci_index(minor).is_some() {
                return drm_node_inode(minor);
            }
        }
        // evdev: 13:<64+N> -> a *symlink* to /sys/class/input/eventN.
        //
        // libinput's evdev_device_have_same_syspath() builds a udev device from
        // the opened fd's device number (reading this `/sys/dev/char/13:N`
        // path) and compares its canonical syspath to the syspath of the
        // enumerated device (`/sys/class/input/eventN`). If they differ it
        // closes the fd with no ioctl and rejects the device. Returning the
        // SysInputEventINode directory here gave the canonical path
        // `/sys/dev/char/13:N`, which never equals `/sys/class/input/eventN`,
        // so every input device was rejected. A symlink makes realpath() of
        // both resolve to the same `/sys/class/input/eventN`.
        if let Some(rest) = name.strip_prefix("13:") {
            if let Ok(minor) = rest.parse::<usize>() {
                if minor >= EVDEV_EVENT_MINOR_BASE {
                    let id = minor - EVDEV_EVENT_MINOR_BASE;
                    if id < input_event_count() {
                        return Ok(Arc::new(Pseudo::new(
                            &format!("../../class/input/event{}", id),
                            FileType::SymLink,
                        )));
                    }
                }
            }
        }
        Err(FsError::EntryNotFound)
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        // Every DRM node first, in the same order `/sys/class/drm` lists them
        // (226:0, 226:128, then each further GPU's pair), then evdev 13:64..
        let mut names: Vec<String> = drm_class_entries()
            .iter()
            .map(|minor| format!("226:{}", minor))
            .collect();
        for ev in 0..input_event_count() {
            names.push(format!("13:{}", EVDEV_EVENT_MINOR_BASE + ev));
        }
        nth_entry(id, &names)
    }
}

// ---------------------------------------------------------------------------
// `/sys/class/input` — make evdev nodes discoverable by libinput's udev
// backend (used by wlroots/labwc). Linux's evdev nodes are major 13, with the
// event devices at minor 64+. libinput's udev backend ignores a device unless
// it carries the `ID_INPUT*` properties that udevd's `input_id` builtin would
// normally add; we synthesize those into the `uevent` file (libudev exposes
// uevent keys as device properties), so input works without a running udevd.
// ---------------------------------------------------------------------------

const EVDEV_MAJOR: usize = 13;
const EVDEV_EVENT_MINOR_BASE: usize = 64;

/// Number of input event devices (one `eventN` per registered input device),
/// matching the `/dev/input/eventN` numbering in `create_root_fs`.
fn input_event_count() -> usize {
    drivers::all_input().as_vec().len()
}

/// `ID_INPUT*` udev properties for input device `id`, derived from its evdev
/// capability bitmaps (the same classification udev's `input_id` performs).
fn input_id_props(id: usize) -> String {
    use kernel_hal::drivers::prelude::CapabilityType;
    let devs = drivers::all_input().as_vec();
    let Some(dev) = devs.get(id) else {
        return String::from("ID_INPUT=1\n");
    };
    let key = dev.capability(CapabilityType::Key);
    let rel = dev.capability(CapabilityType::RelAxis);
    let abs = dev.capability(CapabilityType::AbsAxis);

    // Linux input-event-codes: REL_X=0 REL_Y=1; BTN_LEFT=0x110 BTN_TOUCH=0x14a;
    // KEY_ESC=1 KEY_SPACE=57; ABS_X=0.
    let is_mouse = rel.contains(0)
        || rel.contains(1)
        || key.contains(0x110)
        || (abs.contains(0) && abs.contains(1) && key.contains(0x110));
    let is_touch = abs.contains(0) && key.contains(0x14a);
    let is_keyboard = key.contains(1) && key.contains(57);

    let mut s = String::from("ID_INPUT=1\n");
    if is_keyboard {
        s.push_str("ID_INPUT_KEYBOARD=1\n");
    }
    if is_mouse {
        s.push_str("ID_INPUT_MOUSE=1\n");
    }
    if is_touch {
        s.push_str("ID_INPUT_TOUCHSCREEN=1\n");
    }
    // If nothing matched but the device has keys, mark it a key device so
    // libinput still assigns it a capability instead of ignoring it.
    if !is_keyboard && !is_mouse && !is_touch && key.contains(1) {
        s.push_str("ID_INPUT_KEY=1\n");
    }
    s
}

struct SysClassInputDirINode;

impl INode for SysClassInputDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS_INPUT))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassInputDirINode)),
            ".." => Ok(Arc::new(SysClassINode)),
            _ => {
                if let Some(id) = name
                    .strip_prefix("event")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    if id < input_event_count() {
                        return Ok(Arc::new(SysInputEventINode { id }));
                    }
                }
                Err(FsError::EntryNotFound)
            }
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let names: Vec<String> = (0..input_event_count())
            .map(|i| format!("event{}", i))
            .collect();
        nth_entry(id, &names)
    }
}

struct SysInputEventINode {
    id: usize,
}

impl SysInputEventINode {
    fn entries() -> [&'static str; 3] {
        ["dev", "uevent", "subsystem"]
    }
}

impl INode for SysInputEventINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::INPUT_EVENT + self.id))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        let minor = EVDEV_EVENT_MINOR_BASE + self.id;
        match name {
            "." => Ok(Arc::new(SysInputEventINode { id: self.id })),
            ".." => Ok(Arc::new(SysClassInputDirINode)),
            "dev" => Ok(Arc::new(Pseudo::new(
                &format!("{}:{}\n", EVDEV_MAJOR, minor),
                FileType::File,
            ))),
            "uevent" => {
                let content = format!(
                    "MAJOR={}\nMINOR={}\nDEVNAME=input/event{}\n{}",
                    EVDEV_MAJOR,
                    minor,
                    self.id,
                    input_id_props(self.id),
                );
                Ok(Arc::new(Pseudo::new(&content, FileType::File)))
            }
            "subsystem" => Ok(Arc::new(Pseudo::new(
                "../../../class/input",
                FileType::SymLink,
            ))),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysClassNetDirINode;

impl INode for SysClassNetDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS_NET))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassNetDirINode)),
            ".." => Ok(Arc::new(SysClassINode)),
            name => {
                if list_net_ifnames().iter().any(|n| n.as_str() == name) {
                    Ok(Arc::new(SysNetIfaceINode { name: name.into() }))
                } else {
                    Err(FsError::EntryNotFound)
                }
            }
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &list_net_ifnames())
    }
}

struct SysNetIfaceINode {
    name: String,
}

impl SysNetIfaceINode {
    fn entries() -> [&'static str; 3] {
        ["address", "operstate", "carrier"]
    }
}

impl INode for SysNetIfaceINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::NET_IFACE + net_iface_index(&self.name)))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysNetIfaceINode {
                name: self.name.clone(),
            })),
            ".." => Ok(Arc::new(SysClassNetDirINode)),
            "operstate" => {
                let state = if self.name == "lo" || self.name == "loopback" {
                    "unknown"
                } else {
                    "up"
                };
                Ok(Arc::new(Pseudo::new(
                    &format!("{}\n", state),
                    FileType::File,
                )))
            }
            "carrier" => Ok(Arc::new(Pseudo::new("1\n", FileType::File))),
            "address" => {
                let mac = get_net_device()
                    .iter()
                    .find(|i| i.get_ifname() == self.name)
                    .map(|i| i.get_mac())
                    .unwrap_or_default();
                let bytes = mac.as_bytes();
                let content = format!(
                    "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]
                );
                Ok(Arc::new(Pseudo::new(&content, FileType::File)))
            }
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysClassPowerSupplyDirINode;

impl INode for SysClassPowerSupplyDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS_POWER_SUPPLY))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassPowerSupplyDirINode)),
            // Not itself. Every one of its twenty-eight siblings answers its
            // parent here, and a `..` that is the directory again is a loop:
            // anything walking up from a power supply -- which is what
            // upower does to find the device a battery belongs to -- never
            // leaves `/sys/class/power_supply`.
            ".." => Ok(Arc::new(SysClassINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &[] as &[&str])
    }
}

// ---------------------------------------------------------------------------
// `/sys/class/thermal` — minimal thermal zone + cooling device interface.
//
// Models a single CPU package thermal zone (`thermal_zone0`, type
// "x86_pkg_temp") with two user-configurable trip points, plus one cooling
// device (`cooling_device0`, type "Processor"). This mirrors the ABI described
// in Documentation/driver-api/thermal/{sysfs-api,x86_pkg_temperature_thermal}.rst
// so userspace thermal tooling can probe and configure trip points / policy.
// ---------------------------------------------------------------------------

/// Static zone type reported by `thermal_zone0/type`.
const THERMAL_ZONE_TYPE: &str = "x86_pkg_temp";

/// Current CPU temperature in milli-degrees Celsius: the real digital thermal
/// sensor when the hardware exposes it (bare metal Intel), else the static
/// placeholder so VMs / unsupported parts still present a plausible value.
pub(crate) fn current_temp_mc() -> i32 {
    kernel_hal::cpu::cpu_temperature_mc().unwrap_or_else(|| THERMAL.lock().temp_mc)
}
/// Trip-point types: a passive (throttling) trip and a critical (shutdown) trip.
const THERMAL_TRIP_TYPES: [&str; 2] = ["passive", "critical"];
/// Maximum cooling-device state advertised by `cooling_device0/max_state`.
const COOLING_MAX_STATE: u32 = 10;

/// Mutable state shared by all (stateless) thermal sysfs INodes. `find()` mints
/// fresh INodes on every lookup, so the configurable values must live here for
/// writes to persist across reopen.
struct ThermalState {
    /// Current package temperature, in milli-degrees Celsius.
    temp_mc: i32,
    /// Active governor policy (`thermal_zone0/policy`).
    policy: String,
    /// Trip-point temperatures in milli-degrees Celsius; 0 disables the trip.
    trip_temp: [i32; 2],
    /// Current cooling-device state (`cooling_device0/cur_state`).
    cooling_cur: u32,
}

lazy_static! {
    static ref THERMAL: Mutex<ThermalState> = Mutex::new(ThermalState {
        temp_mc: 45000,
        policy: String::from("step_wise"),
        trip_temp: [0, 0],
        cooling_cur: 0,
    });
}

/// One writable thermal attribute. Read renders the current value; write parses
/// and stores it in [`THERMAL`].
#[derive(Clone, Copy)]
enum ThermalAttr {
    Policy,
    Trip0,
    Trip1,
    CoolingCur,
}

struct ThermalAttrINode {
    attr: ThermalAttr,
}

impl ThermalAttrINode {
    fn value(&self) -> String {
        let t = THERMAL.lock();
        match self.attr {
            ThermalAttr::Policy => format!("{}\n", t.policy),
            ThermalAttr::Trip0 => format!("{}\n", t.trip_temp[0]),
            ThermalAttr::Trip1 => format!("{}\n", t.trip_temp[1]),
            ThermalAttr::CoolingCur => format!("{}\n", t.cooling_cur),
        }
    }
}

impl INode for ThermalAttrINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let content = self.value();
        let bytes = content.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let s = core::str::from_utf8(buf)
            .map_err(|_| FsError::InvalidParam)?
            .trim();
        let mut t = THERMAL.lock();
        match self.attr {
            ThermalAttr::Policy => t.policy = String::from(s),
            ThermalAttr::Trip0 => t.trip_temp[0] = s.parse().map_err(|_| FsError::InvalidParam)?,
            ThermalAttr::Trip1 => t.trip_temp[1] = s.parse().map_err(|_| FsError::InvalidParam)?,
            ThermalAttr::CoolingCur => {
                let v: u32 = s.parse().map_err(|_| FsError::InvalidParam)?;
                t.cooling_cur = v.min(COOLING_MAX_STATE);
            }
        }
        // Report the whole buffer consumed so the writer doesn't loop.
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 0,
            size: self.value().len(),
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
}

/// Read-only static file helper for thermal attributes.
fn thermal_ro(content: &str) -> Arc<dyn INode> {
    Arc::new(Pseudo::new(content, FileType::File))
}

struct SysClassThermalDirINode;

impl SysClassThermalDirINode {
    fn entries() -> [&'static str; 2] {
        ["thermal_zone0", "cooling_device0"]
    }
}

impl INode for SysClassThermalDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CLASS_THERMAL))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysClassThermalDirINode)),
            ".." => Ok(Arc::new(SysClassINode)),
            "thermal_zone0" => Ok(Arc::new(SysThermalZoneDirINode)),
            "cooling_device0" => Ok(Arc::new(SysThermalCoolingDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysThermalZoneDirINode;

impl SysThermalZoneDirINode {
    fn entries() -> [&'static str; 10] {
        [
            "type",
            "temp",
            "policy",
            "available_policies",
            "mode",
            "trip_point_0_temp",
            "trip_point_0_type",
            "trip_point_1_temp",
            "trip_point_1_type",
            "uevent",
        ]
    }
}

impl INode for SysThermalZoneDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::THERMAL_ZONE))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysThermalZoneDirINode)),
            ".." => Ok(Arc::new(SysClassThermalDirINode)),
            "type" => Ok(thermal_ro(&format!("{}\n", THERMAL_ZONE_TYPE))),
            "temp" => Ok(thermal_ro(&format!("{}\n", current_temp_mc()))),
            "policy" => Ok(Arc::new(ThermalAttrINode {
                attr: ThermalAttr::Policy,
            })),
            "available_policies" => Ok(thermal_ro("step_wise user_space\n")),
            "mode" => Ok(thermal_ro("enabled\n")),
            "trip_point_0_temp" => Ok(Arc::new(ThermalAttrINode {
                attr: ThermalAttr::Trip0,
            })),
            "trip_point_0_type" => Ok(thermal_ro(&format!("{}\n", THERMAL_TRIP_TYPES[0]))),
            "trip_point_1_temp" => Ok(Arc::new(ThermalAttrINode {
                attr: ThermalAttr::Trip1,
            })),
            "trip_point_1_type" => Ok(thermal_ro(&format!("{}\n", THERMAL_TRIP_TYPES[1]))),
            "uevent" => Ok(thermal_ro("")),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysThermalCoolingDirINode;

impl SysThermalCoolingDirINode {
    fn entries() -> [&'static str; 4] {
        ["type", "max_state", "cur_state", "uevent"]
    }
}

impl INode for SysThermalCoolingDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::THERMAL_COOLING))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysThermalCoolingDirINode)),
            ".." => Ok(Arc::new(SysClassThermalDirINode)),
            "type" => Ok(thermal_ro("Processor\n")),
            "max_state" => Ok(thermal_ro(&format!("{}\n", COOLING_MAX_STATE))),
            "cur_state" => Ok(Arc::new(ThermalAttrINode {
                attr: ThermalAttr::CoolingCur,
            })),
            "uevent" => Ok(thermal_ro("")),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

// ---------------------------------------------------------------------------
// System power management: `/sys/power` (system sleep) and
// `/sys/devices/system/cpu` (CPU hotplug).
//
// These mirror the userspace ABI described in
// Documentation/{driver-api/pm,power} and the suspend/CPU-hotplug docs: a
// thermal/power manager writes "mem"/"disk" to /sys/power/state and toggles
// CPUs via /sys/devices/system/cpu/cpuN/online. eclipse does not actually
// enter ACPI sleep states or park CPUs, so writes are validated and recorded
// (a compatibility shim) rather than driving real hardware transitions.
// ---------------------------------------------------------------------------

/// System sleep states advertised by `/sys/power/state`.
const POWER_STATES: &str = "freeze mem disk";

/// Mutable PM state shared by the (stateless) sysfs INodes.
struct PmState {
    /// Per-CPU online bitmask (bit N set ⇒ CPU N online). Boot CPU stays online.
    cpu_online: u64,
    /// Hibernation mode reported by `/sys/power/disk`.
    disk_mode: String,
}

lazy_static! {
    static ref PM: Mutex<PmState> = Mutex::new(PmState {
        cpu_online: u64::MAX,
        disk_mode: String::from("platform"),
    });
}

/// Comma-separated list of currently-online CPUs (e.g. "0,1,3").
fn online_cpu_list(count: usize) -> String {
    let mask = PM.lock().cpu_online;
    let mut parts: Vec<String> = Vec::new();
    for i in 0..count.min(64) {
        if mask & (1u64 << i) != 0 {
            parts.push(format!("{}", i));
        }
    }
    format!("{}\n", parts.join(","))
}

/// `0` or `0-(count-1)` range string used by present/possible CPU masks.
fn cpu_range(count: usize) -> String {
    if count <= 1 {
        String::from("0\n")
    } else {
        format!("0-{}\n", count - 1)
    }
}

/// A writable power-management sysfs attribute.
#[derive(Clone, Copy)]
enum PmAttr {
    /// `/sys/power/state`.
    PowerState,
    /// `/sys/power/disk`.
    PowerDisk,
    /// `/sys/devices/system/cpu/cpuN/online` for the given CPU index.
    CpuOnline(usize),
}

struct PmAttrINode {
    attr: PmAttr,
}

impl PmAttrINode {
    fn value(&self) -> String {
        match self.attr {
            PmAttr::PowerState => format!("{}\n", POWER_STATES),
            PmAttr::PowerDisk => format!("{}\n", PM.lock().disk_mode),
            PmAttr::CpuOnline(i) => {
                let online = PM.lock().cpu_online & (1u64 << (i.min(63))) != 0;
                format!("{}\n", if online { 1 } else { 0 })
            }
        }
    }
}

impl INode for PmAttrINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let content = self.value();
        let bytes = content.as_bytes();
        if offset >= bytes.len() {
            return Ok(0);
        }
        let len = (bytes.len() - offset).min(buf.len());
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> Result<usize> {
        let s = core::str::from_utf8(buf)
            .map_err(|_| FsError::InvalidParam)?
            .trim();
        match self.attr {
            PmAttr::PowerState => {
                // Validate against the advertised states; we don't actually
                // suspend, so a successful write is a logged no-op.
                if POWER_STATES.split_whitespace().any(|st| st == s) || s == "standby" {
                    warn!(
                        "/sys/power/state: '{}' requested (suspend not implemented)",
                        s
                    );
                } else {
                    return Err(FsError::InvalidParam);
                }
            }
            PmAttr::PowerDisk => match s {
                "platform" | "shutdown" | "reboot" | "suspend" => {
                    PM.lock().disk_mode = String::from(s)
                }
                _ => return Err(FsError::InvalidParam),
            },
            PmAttr::CpuOnline(i) => {
                let on: u32 = s.parse().map_err(|_| FsError::InvalidParam)?;
                if i == 0 && on == 0 {
                    // The boot CPU cannot be taken offline.
                    return Err(FsError::NotSupported);
                }
                if i >= 64 {
                    return Err(FsError::InvalidParam);
                }
                let mut pm = PM.lock();
                if on != 0 {
                    pm.cpu_online |= 1u64 << i;
                } else {
                    pm.cpu_online &= !(1u64 << i);
                }
            }
        }
        Ok(buf.len())
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
            hangup: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 0,
            inode: 0,
            size: self.value().len(),
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::File,
            mode: 0o644,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
}

struct SysPowerDirINode;

impl SysPowerDirINode {
    fn entries() -> [&'static str; 3] {
        ["state", "disk", "wakeup_count"]
    }
}

impl INode for SysPowerDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::POWER))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysPowerDirINode)),
            ".." => Ok(SYS_ROOT.clone()),
            "state" => Ok(Arc::new(PmAttrINode {
                attr: PmAttr::PowerState,
            })),
            "disk" => Ok(Arc::new(PmAttrINode {
                attr: PmAttr::PowerDisk,
            })),
            "wakeup_count" => Ok(Arc::new(Pseudo::new("0\n", FileType::File))),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        nth_entry(id, &Self::entries())
    }
}

struct SysDevicesSystemCpuDirINode;

impl INode for SysDevicesSystemCpuDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::SYSTEM_CPU))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        let count = kernel_hal::cpu::cpu_count() as usize;
        match name {
            "." => Ok(Arc::new(SysDevicesSystemCpuDirINode)),
            ".." => Ok(Arc::new(SysDevicesSystemDirINode)),
            "online" => Ok(Arc::new(Pseudo::new(
                &online_cpu_list(count),
                FileType::File,
            ))),
            "present" | "possible" => Ok(Arc::new(Pseudo::new(&cpu_range(count), FileType::File))),
            "kernel_max" => Ok(Arc::new(Pseudo::new(
                &format!("{}\n", count.saturating_sub(1)),
                FileType::File,
            ))),
            _ => {
                // cpuN directories.
                if let Some(idx) = name
                    .strip_prefix("cpu")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    if idx < count {
                        return Ok(Arc::new(SysCpuNDirINode { cpu: idx }));
                    }
                }
                Err(FsError::EntryNotFound)
            }
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        // Aggregate files first, then one entry per CPU.
        let mut names: Vec<String> = ["online", "present", "possible", "kernel_max"]
            .iter()
            .map(|s| String::from(*s))
            .collect();
        for cpu in 0..kernel_hal::cpu::cpu_count() as usize {
            names.push(format!("cpu{}", cpu));
        }
        nth_entry(id, &names)
    }
}

struct SysCpuNDirINode {
    cpu: usize,
}

impl INode for SysCpuNDirINode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize> {
        Ok(0)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }
    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: false,
            error: false,
            hangup: false,
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(ino::CPU + self.cpu))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysCpuNDirINode { cpu: self.cpu })),
            ".." => Ok(Arc::new(SysDevicesSystemCpuDirINode)),
            "online" => Ok(Arc::new(PmAttrINode {
                attr: PmAttr::CpuOnline(self.cpu),
            })),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        // The boot CPU has no `online` toggle in Linux, but exposing it for all
        // CPUs keeps the shim uniform and simple.
        nth_entry(id, &["online"])
    }
}

/// PCI device list for `/sys`, scanned once and cached.
///
/// `scan_bus` probes every (bus, device, function) with config-space port I/O.
/// Every trap is cheap on real hardware but murderous under QEMU's TCG (each
/// `in`/`out` exits to the emulator), and this was re-run on **every** `/sys`
/// path-component resolution — resolving one `card0/device` symlink triggered
/// two full bus scans, and libdrm walks these paths dozens of times while
/// enumerating the GPU. That was the whole multi-second-per-`readlink` sysfs
/// stall (`readlink` 3.3s, `stat` 1.9s, `open` 0.6s) that kept the GL path from
/// ever finishing GPU init — so the compositor never rendered. PCI topology is
/// fixed after boot, so a scan-once cache is correct and collapses each of those
/// syscalls to a small `Vec` clone.
#[cfg(not(test))]
fn get_pci_devices() -> Vec<PciDevInfo> {
    PCI_DEVICES.clone()
}

/// The bus a test walks the tree over.
///
/// One seam covers the whole of `/sys`, because every directory here that
/// names a PCI device asks this same question. It has to be a seam and not a
/// fake bus: [`scan_pci_devices`] is `in`/`out` on the configuration ports,
/// and off a real machine that is a fault, not an empty list, so without it
/// the tree could not be walked outside a kernel at all -- which is why 2750
/// lines of it had three tests.
#[cfg(test)]
fn get_pci_devices() -> Vec<PciDevInfo> {
    test_bus::table()
}

#[cfg(not(test))]
lazy_static! {
    static ref PCI_DEVICES: Vec<PciDevInfo> = scan_pci_devices();
}

#[cfg(not(test))]
fn scan_pci_devices() -> Vec<PciDevInfo> {
    #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
    {
        let mut devs = Vec::new();
        let ops = &zcore_drivers::bus::pci::PortOpsImpl;
        let am = zcore_drivers::bus::pci::PCI_ACCESS;
        let pci_iter = unsafe { pci::scan_bus(ops, am) };
        for dev in pci_iter {
            let name = format!(
                "0000:{:02x}:{:02x}.{:x}",
                dev.loc.bus, dev.loc.device, dev.loc.function
            );
            let vendor = format!("{:#06x}", dev.id.vendor_id);
            let device = format!("{:#06x}", dev.id.device_id);
            let class = format!(
                "0x{:02x}{:02x}{:02x}",
                dev.id.class, dev.id.subclass, dev.id.prog_if
            );
            devs.push(PciDevInfo {
                name,
                vendor,
                device,
                class,
            });
        }
        devs
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
    {
        Vec::new()
    }
}

/// The PCI bus the tests build `/sys` over, in place of the configuration
/// ports.
#[cfg(test)]
mod test_bus {
    extern crate std;

    use super::PciDevInfo;
    use alloc::{format, string::String, vec, vec::Vec};

    /// The table is process-wide and cargo runs a crate's tests in threads, so
    /// every test that seeds a bus takes this first.
    static LOCK: self::std::sync::Mutex<()> = self::std::sync::Mutex::new(());
    static TABLE: self::std::sync::Mutex<Option<Vec<PciDevInfo>>> =
        self::std::sync::Mutex::new(None);

    /// Take the bus, seeded with `devs`, and hold it until the guard goes.
    ///
    /// A test that panics while holding the lock poisons it; that is not a
    /// failure for the tests that follow, so step over the poison.
    pub(super) fn with(devs: Vec<PciDevInfo>) -> Bus {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        *TABLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(devs);
        Bus { _guard: guard }
    }

    pub(super) struct Bus {
        _guard: self::std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for Bus {
        fn drop(&mut self) {
            *TABLE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    pub(super) fn table() -> Vec<PciDevInfo> {
        TABLE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }

    pub(super) fn dev(name: &str, vendor: u16, device: u16, class: u32) -> PciDevInfo {
        PciDevInfo {
            name: String::from(name),
            vendor: format!("{:#06x}", vendor),
            device: format!("{:#06x}", device),
            class: format!("{:#08x}", class),
        }
    }

    /// Moebius's machine: a host bridge, an NVMe disk, and the two RTX 2060
    /// SUPERs, each with its HDMI audio function.
    pub(super) fn a_real_machine() -> Vec<PciDevInfo> {
        vec![
            dev("0000:00:00.0", 0x8086, 0x3e30, 0x060000),
            dev("0000:01:00.0", 0x144d, 0xa808, 0x010802),
            dev("0000:02:00.0", 0x10de, 0x1f06, 0x030000),
            dev("0000:02:00.1", 0x10de, 0x10f9, 0x040300),
            dev("0000:03:00.0", 0x10de, 0x1f06, 0x030000),
            dev("0000:03:00.1", 0x10de, 0x10f9, 0x040300),
        ]
    }

    /// A disk with nothing behind it, so that `/sys/block` has something in
    /// it at all: with no driver registered both it and `/sys/class/block`
    /// are empty, which is every machine the CI runs on.
    struct FakeDisk {
        name: String,
        sectors: usize,
    }

    impl zcore_drivers::scheme::Scheme for FakeDisk {
        fn name(&self) -> &str {
            &self.name
        }
    }

    impl zcore_drivers::scheme::BlockScheme for FakeDisk {
        fn read_block(&self, _id: usize, _buf: &mut [u8]) -> zcore_drivers::DeviceResult {
            Ok(())
        }
        fn write_block(&self, _id: usize, _buf: &[u8]) -> zcore_drivers::DeviceResult {
            Ok(())
        }
        fn flush(&self) -> zcore_drivers::DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.sectors
        }
    }

    /// The disks a test plugs in, unplugged again when it ends.
    ///
    /// `DeviceList` is append-only on bare metal and `add_device_hosted`'s
    /// partner only exists under `libos`, which is how these tests build.
    pub(super) struct Disks {
        devs: Vec<zcore_drivers::Device>,
    }

    impl Drop for Disks {
        fn drop(&mut self) {
            for dev in self.devs.drain(..) {
                kernel_hal::drivers::remove_device_hosted(&dev);
            }
        }
    }

    /// Plug in one disk per driver name given, each `sectors` sectors long.
    ///
    /// The names are the DRIVER's (`nvme0`, `virtio-blk`, `ahci0`), not the
    /// ones `/sys/block` shows -- turning one into the other is what
    /// `list_block_devices` is for and what the test is measuring.
    ///
    /// The bus guard is asked for and dropped, rather than described in a
    /// comment, because the device list is process-wide the same way the PCI
    /// table is: a test that plugs a disk in without holding the lock puts it
    /// in front of whatever another test is walking. That is exactly the shape
    /// of flaky suite this kernel has shipped five of, and `--test-threads=1`
    /// in the CI hides every one of them.
    pub(super) fn disks(_bus: &Bus, names: &[&str], sectors: usize) -> Disks {
        let mut devs = Vec::new();
        for name in names {
            let disk = alloc::sync::Arc::new(FakeDisk {
                name: String::from(*name),
                sectors,
            });
            let dev = zcore_drivers::Device::Block(disk);
            kernel_hal::drivers::add_device_hosted(dev.clone());
            devs.push(dev);
        }
        Disks { devs }
    }
}

/// Resolve an absolute `/sys/...` path without walking the ext2 backing store.
pub(crate) fn lookup_path(path: &str, follow_times: usize) -> Result<Arc<dyn INode>> {
    let path = path.trim_end_matches('/');
    if path == "/sys" {
        return Ok(SYS_ROOT.clone());
    }
    let rest = path.strip_prefix("/sys/").ok_or(FsError::EntryNotFound)?;
    if rest.is_empty() {
        return Ok(SYS_ROOT.clone());
    }
    SYS_ROOT.lookup_follow(rest, follow_times)
}

lazy_static! {
    static ref SYS_ROOT: Arc<dyn INode> = Arc::new(SysRootINode);
}

/// Node-name parsing. `/sys/class/drm`, `/sys/dev/char` and each PCI device's
/// `drm/` directory all resolve a lookup through this, so a name it accepts
/// that `/dev/dri` never emits is a path that exists in one tree and not the
/// other.
#[cfg(test)]
mod drm_name_tests {
    use super::*;

    #[test]
    fn card_and_render_names_round_trip() {
        for minor in [0u32, 1, 2, 63, 128, 129, 130, 191] {
            let name = crate::fs::devfs::drm::node_name(minor);
            assert_eq!(
                drm_minor_from_name(&name),
                Some(minor),
                "{name} did not round-trip"
            );
        }
    }

    #[test]
    fn a_name_the_tree_never_emits_is_rejected() {
        // Non-canonical spellings of a real minor: these would otherwise
        // resolve to a node that is listed under a different name.
        assert_eq!(drm_minor_from_name("card007"), None);
        assert_eq!(drm_minor_from_name("renderD0129"), None);
        // A render minor spelled as a card and vice versa.
        assert_eq!(drm_minor_from_name("card128"), None);
        assert_eq!(drm_minor_from_name("renderD1"), None);
        // Not DRM nodes at all.
        assert_eq!(drm_minor_from_name("card"), None);
        assert_eq!(drm_minor_from_name("cardX"), None);
        assert_eq!(drm_minor_from_name("controlD64"), None);
        assert_eq!(drm_minor_from_name(""), None);
        assert_eq!(drm_minor_from_name("."), None);
        assert_eq!(drm_minor_from_name(".."), None);
    }

    #[test]
    fn an_out_of_range_minor_does_not_overflow() {
        // u32::MAX parses; it must simply not be a node, not panic.
        assert_eq!(drm_minor_from_name("card4294967296"), None);
        assert_eq!(
            drm_minor_from_name("renderD4294967295"),
            Some(u32::MAX),
            "it parses as a render minor; whether a node exists is the table's call"
        );
        assert!(drm_node_pci_index(u32::MAX).is_none());
    }
}

#[cfg(test)]
mod compute_alias_inode_tests {
    //! The compute-only alias device has no index on the bus, so its index is
    //! the sentinel `usize::MAX`. Adding a sentinel to an inode base is an
    //! overflow, and it was done twice.

    use super::*;

    fn alias_dir() -> SysPciDevDirINode {
        SysPciDevDirINode {
            index: COMPUTE_ALIAS_INDEX,
            name: COMPUTE_ALIAS_BDF.into(),
            vendor: "0x0000".into(),
            device: "0x0000".into(),
            class: COMPUTE_ALIAS_CLASS.into(),
        }
    }

    /// `ino::PCI_DEV + usize::MAX` panics in a build with overflow checks --
    /// this test is one -- and wraps to an arbitrary number in the kernel's,
    /// which is not the unique inode the sum exists to produce. A plain
    /// `ls -l /sys/bus/pci/devices/` is enough to ask for it.
    #[test]
    fn the_alias_device_has_an_inode_that_does_not_overflow() {
        let m = alias_dir().metadata().unwrap();
        assert_eq!(m.inode, ino::PCI_DEV_COMPUTE_ALIAS);
        assert_eq!(m.type_, FileType::Dir);
    }

    /// And so does its `drm` directory.
    #[test]
    fn the_alias_drm_directory_has_one_too() {
        let drm = SysDrmDeviceDrmDirINode {
            pci_index: COMPUTE_ALIAS_INDEX,
        };
        assert_eq!(drm.metadata().unwrap().inode, ino::DRM_DIR_COMPUTE_ALIAS);
    }

    /// Neither number may be one a real device on the bus could reach, which is
    /// the whole point of giving the alias its own.
    #[test]
    fn the_alias_numbers_are_outside_every_real_devices_range() {
        for index in [0usize, 1, 7, 255] {
            assert_ne!(ino::PCI_DEV + index, ino::PCI_DEV_COMPUTE_ALIAS);
            assert_ne!(ino::DRM_DIR + index, ino::DRM_DIR_COMPUTE_ALIAS);
        }
        assert_ne!(ino::PCI_DEV_COMPUTE_ALIAS, ino::DRM_DIR_COMPUTE_ALIAS);
    }
}

#[cfg(test)]
mod tree_tests {
    //! The shape of `/sys`, walked whole.
    //!
    //! Everything userspace knows about the machine it runs on comes from
    //! walking this tree: udev enumerates it, libinput reads the input class
    //! out of it, and libdrm and Mesa resolve a GPU through four of its
    //! symlinks before they will open a device. All of that is `readdir`,
    //! `open` and `readlink` over twenty-nine directories, and none of it was
    //! reachable from a test until the bus became a seam.

    use super::*;
    use alloc::vec;

    /// One node of the tree, with where it was found and who found it.
    struct Node {
        path: String,
        inode: Arc<dyn INode>,
        parent_ino: usize,
        type_: FileType,
    }

    /// Every node reachable from `/sys`, without following a symlink --
    /// following them would make the walk infinite, and they get a test of
    /// their own.
    fn whole_tree() -> Vec<Node> {
        let mut out: Vec<Node> = vec![];
        let root = SYS_ROOT.clone();
        let root_ino = root.metadata().unwrap().inode;
        let mut queue = vec![Node {
            path: String::from(""),
            inode: root,
            parent_ino: root_ino,
            type_: FileType::Dir,
        }];
        while let Some(node) = queue.pop() {
            if node.type_ == FileType::Dir {
                let ino = node.inode.metadata().unwrap().inode;
                for name in listing(&node.inode) {
                    if name == "." || name == ".." {
                        continue;
                    }
                    let child = node
                        .inode
                        .find(&name)
                        .unwrap_or_else(|e| panic!("{}/{} listed but {:?}", node.path, name, e));
                    let type_ = child.metadata().unwrap().type_;
                    queue.push(Node {
                        path: format!("{}/{}", node.path, name),
                        inode: child,
                        parent_ino: ino,
                        type_,
                    });
                }
            }
            out.push(node);
        }
        out
    }

    /// What `readdir` would return: `get_entry` from 0 until it stops.
    fn listing(node: &Arc<dyn INode>) -> Vec<String> {
        let mut names = vec![];
        for id in 0.. {
            match node.get_entry(id) {
                Ok(name) => names.push(name),
                Err(_) => break,
            }
            assert!(
                id < 4096,
                "a listing that does not end is an `ls` that does not end"
            );
        }
        names
    }

    fn contents(node: &Arc<dyn INode>) -> Vec<u8> {
        let mut out = vec![];
        let mut buf = [0u8; 64];
        loop {
            let n = node.read_at(out.len(), &mut buf).unwrap();
            if n == 0 {
                return out;
            }
            out.extend_from_slice(&buf[..n]);
            assert!(out.len() < 1 << 16, "a file that never ends");
        }
    }

    #[test]
    fn every_directory_lists_itself_and_its_parent_first() {
        // `getdents` goes straight through `get_entry` and synthesises
        // nothing, so a directory numbering from its first real name is one
        // `readdir` never reports `.` or `..` for -- which every other
        // filesystem this kernel mounts does report.
        let _bus = test_bus::with(test_bus::a_real_machine());
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            let names = listing(&node.inode);
            assert_eq!(
                names.first().map(|s| s.as_str()),
                Some("."),
                "{}",
                node.path
            );
            assert_eq!(
                names.get(1).map(|s| s.as_str()),
                Some(".."),
                "{}",
                node.path
            );
            assert!(
                !names[2..].iter().any(|n| n == "." || n == ".."),
                "{} lists them twice",
                node.path
            );
        }
    }

    #[test]
    fn an_empty_directory_still_has_the_two() {
        let _bus = test_bus::with(vec![]);
        let empty = SYS_ROOT.lookup_follow("class/power_supply", 4).unwrap();
        assert_eq!(listing(&empty), vec![String::from("."), String::from("..")]);
    }

    #[test]
    fn dot_is_the_directory_and_dotdot_is_the_one_above_it() {
        let _bus = test_bus::with(test_bus::a_real_machine());
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            let ino = node.inode.metadata().unwrap().inode;
            let here = node.inode.find(".").unwrap().metadata().unwrap().inode;
            assert_eq!(here, ino, "{}/. is somewhere else", node.path);
            let up = node.inode.find("..").unwrap().metadata().unwrap().inode;
            assert_eq!(
                up, node.parent_ino,
                "{}/.. is not the directory above",
                node.path
            );
        }
    }

    #[test]
    fn walking_up_from_a_power_supply_reaches_the_class_directory() {
        // Its `..` used to be itself, so anything walking up from a battery
        // -- which is how a power daemon finds the device one belongs to --
        // stayed in `/sys/class/power_supply` for ever.
        let _bus = test_bus::with(vec![]);
        let class = SYS_ROOT.lookup_follow("class", 4).unwrap();
        let up = SYS_ROOT.lookup_follow("class/power_supply/..", 4).unwrap();
        assert_eq!(
            up.metadata().unwrap().inode,
            class.metadata().unwrap().inode
        );
        let twice = SYS_ROOT
            .lookup_follow("class/power_supply/../..", 4)
            .unwrap();
        assert_eq!(
            twice.metadata().unwrap().inode,
            SYS_ROOT.metadata().unwrap().inode
        );
    }

    #[test]
    fn every_name_a_directory_lists_can_be_opened() {
        // The walk panics on a name `find` refuses, so reaching the end is
        // the assertion; the count keeps it from passing on an empty tree.
        let _bus = test_bus::with(test_bus::a_real_machine());
        assert!(whole_tree().len() > 60);
    }

    #[test]
    fn a_listing_ends_instead_of_repeating_itself_or_panicking() {
        let _bus = test_bus::with(test_bus::a_real_machine());
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            let n = listing(&node.inode).len();
            assert_eq!(
                node.inode.get_entry(n),
                Err(FsError::EntryNotFound),
                "{}",
                node.path
            );
            assert_eq!(
                node.inode.get_entry(usize::MAX),
                Err(FsError::EntryNotFound),
                "{} on the last id there is",
                node.path
            );
        }
    }

    #[test]
    fn no_two_directories_of_the_tree_claim_the_same_inode() {
        // They were literals spaced ten apart with the per-device ones
        // written `40 + index` and `100 + index`, so the eleventh PCI
        // function was `/sys/devices/system` over again and the first disk
        // was `/sys/class/thermal`.
        let _bus = test_bus::with(test_bus::a_real_machine());
        let mut seen: Vec<(usize, String)> = vec![];
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            let ino = node.inode.metadata().unwrap().inode;
            if let Some((_, other)) = seen.iter().find(|(i, _)| *i == ino) {
                panic!("{} and {} are both inode {}", other, node.path, ino);
            }
            seen.push((ino, node.path.clone()));
        }
    }

    #[test]
    fn a_bus_wider_than_the_old_numbering_allowed_still_numbers_apart() {
        // Twenty functions, which an ordinary desktop passes once the
        // chipset's own are counted.
        let devs: Vec<PciDevInfo> = (0..20)
            .map(|i| test_bus::dev(&format!("0000:00:{:02x}.0", i), 0x8086, 0x1234, 0x060000))
            .collect();
        let _bus = test_bus::with(devs);
        let mut seen: Vec<usize> = vec![];
        for node in whole_tree() {
            if node.type_ != FileType::Dir {
                continue;
            }
            let ino = node.inode.metadata().unwrap().inode;
            assert!(!seen.contains(&ino), "{} repeats inode {}", node.path, ino);
            seen.push(ino);
        }
    }

    #[test]
    fn every_symlink_resolves_to_something_in_the_tree() {
        // Four of these are what libdrm walks to decide which device a card
        // belongs to, and the only thing that can be wrong with one is the
        // number of `..` in it, which reads as correct however long you look.
        let _bus = test_bus::with(test_bus::a_real_machine());
        let mut links = 0;
        for node in whole_tree() {
            if node.type_ != FileType::SymLink {
                continue;
            }
            let target = contents(&node.inode);
            let target = core::str::from_utf8(&target).unwrap();
            let path = node.path.trim_start_matches('/');
            let found = SYS_ROOT
                .lookup_follow(path, 8)
                .unwrap_or_else(|e| panic!("{} -> {} is {:?}", node.path, target, e));
            assert_eq!(
                found.metadata().unwrap().type_,
                FileType::Dir,
                "{} -> {} is not a directory",
                node.path,
                target
            );
            assert_eq!(
                node.inode.metadata().unwrap().size,
                target.len(),
                "{} reports the wrong length for its target",
                node.path
            );
            links += 1;
        }
        assert!(
            links >= 10,
            "only {} symlinks: the tree came up empty",
            links
        );
    }

    #[test]
    fn a_file_read_from_the_middle_gives_the_rest_of_it() {
        // `cat` reads whatever its buffer holds and asks again from where it
        // stopped. A file answering from the beginning every time never ends.
        let _bus = test_bus::with(test_bus::a_real_machine());
        for node in whole_tree() {
            if node.type_ != FileType::File {
                continue;
            }
            let whole = contents(&node.inode);
            for offset in [0, 1, whole.len() / 2, whole.len()] {
                let offset = offset.min(whole.len());
                let mut buf = vec![0u8; whole.len() + 8];
                let n = node.inode.read_at(offset, &mut buf).unwrap();
                assert_eq!(&buf[..n], &whole[offset..], "{} at {}", node.path, offset);
            }
            assert_eq!(
                node.inode.read_at(whole.len() + 1, &mut [0u8; 8]).unwrap(),
                0,
                "{} past its end",
                node.path
            );
        }
    }

    #[test]
    fn the_bus_the_tree_shows_is_the_bus_it_was_given() {
        let _bus = test_bus::with(test_bus::a_real_machine());
        let devices = SYS_ROOT.lookup_follow("bus/pci/devices", 4).unwrap();
        let names = listing(&devices);
        assert_eq!(&names[..2], &[String::from("."), String::from("..")]);
        assert_eq!(names[2], "0000:00:00.0");
        assert_eq!(names.len(), 8, "six functions and the two");

        // The card Moebius runs, read back the way udev reads it.
        let card = SYS_ROOT
            .lookup_follow("devices/pci0000:00/0000:02:00.0", 4)
            .unwrap();
        assert_eq!(contents(&card.find("vendor").unwrap()), b"0x10de\n");
        assert_eq!(contents(&card.find("device").unwrap()), b"0x1f06\n");
        assert_eq!(contents(&card.find("class").unwrap()), b"0x030000\n");
        // Upper case, because `modules.alias` is upper case and `kmod`
        // compares the two with `fnmatch`.
        assert_eq!(
            contents(&card.find("modalias").unwrap()),
            b"pci:v000010DEd00001F06sv00000000sd00000000bc03sc00i00\n"
        );
        assert_eq!(
            contents(&card.find("uevent").unwrap()),
            b"PCI_CLASS=30000\nPCI_ID=10DE:1F06\nPCI_SUBSYS_ID=0000:0000\n\
              PCI_SLOT_NAME=0000:02:00.0\n\
              MODALIAS=pci:v000010DEd00001F06sv00000000sd00000000bc03sc00i00\n"
                .as_slice()
        );

        // The 256 bytes `config` hands out are the header a driver expects to
        // find the device at: ids at 0, then revision, prog-if, subclass and
        // base class at 8 through 11.
        let cfg = contents(&card.find("config").unwrap());
        assert_eq!(cfg.len(), 256);
        assert_eq!(&cfg[0..4], &[0xde, 0x10, 0x06, 0x1f]);
        assert_eq!(&cfg[8..12], &[0x00, 0x00, 0x00, 0x03]);

        // Its HDMI audio function is a different device, not the card again.
        let audio = SYS_ROOT
            .lookup_follow("devices/pci0000:00/0000:02:00.1", 4)
            .unwrap();
        assert_eq!(contents(&audio.find("device").unwrap()), b"0x10f9\n");
        assert_eq!(contents(&audio.find("class").unwrap()), b"0x040300\n");
        assert_ne!(
            card.metadata().unwrap().inode,
            audio.metadata().unwrap().inode
        );
    }

    #[test]
    fn a_class_with_a_hex_letter_in_it_keeps_its_case() {
        // An xHCI controller is class 0x0c0330, and the `0c` is the only
        // reason to look: every other number on a desk is digits. The old
        // modalias took the three bytes as text straight out of the string
        // the scan had formatted, which was lower case.
        let _bus = test_bus::with(vec![test_bus::dev(
            "0000:00:14.0",
            0x8086,
            0xa36d,
            0x0c0330,
        )]);
        let dev = SYS_ROOT
            .lookup_follow("bus/pci/devices/0000:00:14.0", 4)
            .unwrap();
        assert_eq!(
            contents(&dev.find("modalias").unwrap()),
            b"pci:v00008086d0000A36Dsv00000000sd00000000bc0Csc03i30\n"
        );
        let cfg = contents(&dev.find("config").unwrap());
        assert_eq!(&cfg[9..12], &[0x30, 0x03, 0x0c], "prog-if, subclass, class");
    }

    #[test]
    fn a_device_with_nothing_readable_in_it_still_answers() {
        // `scan_pci_devices` writes these strings and they are always
        // well-formed, but the synthetic compute alias builds one by hand
        // with `0x0000`, and a parse that gave up used to leave three
        // different files disagreeing about what it had given up on.
        let ids = PciIds::parse("nonsense", "0x", "");
        assert_eq!(
            ids,
            PciIds {
                vendor: 0,
                device: 0,
                class: 0
            }
        );
        assert_eq!(
            ids.modalias(),
            "pci:v00000000d00000000sv00000000sd00000000bc00sc00i00\n"
        );
        assert!(ids.uevent("0000:00:00.0").ends_with(&ids.modalias()));
    }

    #[test]
    fn a_disk_is_the_same_disk_from_both_places_that_show_it() {
        let _bus = test_bus::with(test_bus::a_real_machine());
        // Moebius's NVMe, plus the SATA disk he has said he does not have
        // here but the installer still has to name.
        let _disks = test_bus::disks(&_bus, &["nvme0", "ahci0"], 2048);

        let block = SYS_ROOT.lookup_follow("block", 4).unwrap();
        assert_eq!(
            listing(&block),
            vec![
                String::from("."),
                String::from(".."),
                String::from("nvme0n1"),
                String::from("sda"),
            ]
        );

        // `/sys/class/block` shows the same names, as links to those.
        let class_block = SYS_ROOT.lookup_follow("class/block", 4).unwrap();
        assert_eq!(listing(&class_block), listing(&block));
        let link = class_block.find("nvme0n1").unwrap();
        assert_eq!(link.metadata().unwrap().type_, FileType::SymLink);
        assert_eq!(contents(&link), b"../../block/nvme0n1");

        // And following it lands on the disk itself, not somewhere else
        // with the same name.
        let by_link = SYS_ROOT.lookup_follow("class/block/nvme0n1", 4).unwrap();
        let direct = SYS_ROOT.lookup_follow("block/nvme0n1", 4).unwrap();
        assert_eq!(
            by_link.metadata().unwrap().inode,
            direct.metadata().unwrap().inode
        );
        assert_eq!(contents(&direct.find("size").unwrap()), b"2048\n");
    }

    #[test]
    fn walking_up_from_a_disk_goes_back_the_way_it_came() {
        let _bus = test_bus::with(vec![]);
        let _disks = test_bus::disks(&_bus, &["nvme0"], 512);

        // `/sys/block/<dev>/..` is `/sys/block`, whose own `..` is `/sys`.
        let disk = SYS_ROOT.lookup_follow("block/nvme0n1", 4).unwrap();
        let up = disk.find("..").unwrap();
        assert_eq!(
            up.metadata().unwrap().inode,
            SYS_ROOT
                .lookup_follow("block", 4)
                .unwrap()
                .metadata()
                .unwrap()
                .inode
        );
        assert_eq!(
            up.find("..").unwrap().metadata().unwrap().inode,
            SYS_ROOT.metadata().unwrap().inode
        );

        // `/sys/class/block`'s is `/sys/class`. Mounting one directory at
        // both places made this one `/sys`, so a walk up from a disk found
        // under `class/` skipped the subsystem it belongs to -- which is
        // precisely what libudev reads that walk for.
        let cb = SYS_ROOT.lookup_follow("class/block", 4).unwrap();
        assert_eq!(
            cb.find("..").unwrap().metadata().unwrap().inode,
            SYS_ROOT
                .lookup_follow("class", 4)
                .unwrap()
                .metadata()
                .unwrap()
                .inode
        );
        assert_ne!(
            cb.metadata().unwrap().inode,
            SYS_ROOT
                .lookup_follow("block", 4)
                .unwrap()
                .metadata()
                .unwrap()
                .inode
        );
    }

    #[test]
    fn a_twenty_seventh_disk_does_not_take_the_first_one_s_name() {
        // `sda` through `sdz`, and then what? The naming wrapped with
        // `% 26` and started over, so two disks answered to `sda` and
        // `block_index_by_name` handed out the first of them for both --
        // the installer would have written a partition table to the wrong
        // one. Linux goes on to `sdaa`.
        let _bus = test_bus::with(vec![]);
        let names: Vec<String> = (0..27).map(|i| format!("ahci{}", i)).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let _disks = test_bus::disks(&_bus, &refs, 64);

        let listed = list_block_devices();
        assert_eq!(listed.len(), 27);
        assert_eq!(listed[25], "sdz");
        assert_eq!(listed[26], "sdaa");

        let mut sorted = listed.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 27, "two disks share a name: {:?}", listed);

        // And every name still finds the disk it belongs to.
        for (i, name) in listed.iter().enumerate() {
            assert_eq!(block_index_by_name(name), Some(i), "{}", name);
        }

        // A name that is merely the start of one is not that disk. `sd`
        // resolving to `sda` would make `/sys/block/sd` a second way in.
        assert_eq!(block_index_by_name("sd"), None);
        assert_eq!(block_index_by_name("sda"), Some(0));
        assert_eq!(block_index_by_name("sdaa"), Some(26));
        assert_eq!(block_index_by_name("sdaaa"), None);
    }

    #[test]
    fn a_machine_with_no_bus_at_all_still_has_a_tree() {
        // Not hypothetical: it is what every non-PCI machine looks like, and
        // the walk has to end rather than fault or run on.
        let _bus = test_bus::with(vec![]);
        let devices = SYS_ROOT.lookup_follow("bus/pci/devices", 4).unwrap();
        assert_eq!(
            listing(&devices),
            vec![String::from("."), String::from("..")]
        );
        assert!(whole_tree().len() > 30);
    }
}
