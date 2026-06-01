//! Minimal sysfs implementation for Linux userland compatibility.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::Any;

use kernel_hal::drivers;
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
        Arc::new(SysRootINode)
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
        names.push(fname);
    }
    names
}

fn block_index_by_name(name: &str) -> Option<usize> {
    list_block_devices()
        .iter()
        .position(|n| n.as_str() == name)
}

fn block_size_sectors(index: usize) -> Option<usize> {
    drivers::all_block().as_vec().get(index).map(|b| b.block_count())
}

struct SysRootINode;

impl SysRootINode {
    fn entries() -> [&'static str; 4] {
        ["class", "block", "bus", "devices"]
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
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(10))
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | ".." => Ok(Arc::new(SysRootINode)),
            "class" => Ok(Arc::new(SysClassINode)),
            "block" => Ok(Arc::new(SysBlockDirINode)),
            "bus" => Ok(Arc::new(SysBusDirINode)),
            "devices" => Ok(Arc::new(SysDevicesDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

struct SysClassINode;

impl SysClassINode {
    fn entries() -> [&'static str; 1] {
        ["block"]
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
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(20))
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
            "block" => Ok(Arc::new(SysBlockDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
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
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(30))
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
        let entries = list_block_devices();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].clone())
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
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(40 + self.index))
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
                Ok(Arc::new(Pseudo::new(&format!("{}\n", sectors), FileType::File)))
            }
            "removable" => Ok(Arc::new(Pseudo::new("0\n", FileType::File))),
            _ => Err(FsError::EntryNotFound),
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(50))
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
        if id == 0 {
            Ok("pci".into())
        } else {
            Err(FsError::EntryNotFound)
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(60))
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
        if id == 0 {
            Ok("devices".into())
        } else {
            Err(FsError::EntryNotFound)
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(70))
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
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        if id == 0 {
            Ok("pci0000:00".into())
        } else {
            Err(FsError::EntryNotFound)
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(80))
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
        if let Some((idx, dev)) = devices.iter().enumerate().find(|(_, d)| d.name == name) {
            log::trace!("SysDevicesPciBusDirINode::find name={} -> found", name);
            Ok(Arc::new(SysPciDevDirINode {
                index: idx,
                name: dev.name.clone(),
                vendor: dev.vendor.clone(),
                device: dev.device.clone(),
                class: dev.class.clone(),
            }))
        } else {
            log::trace!("SysDevicesPciBusDirINode::find name={} -> EntryNotFound", name);
            Err(FsError::EntryNotFound)
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let devices = get_pci_devices();
        if id >= devices.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(devices[id].name.clone())
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(90))
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
            log::trace!("SysPciDevicesDirINode::find name={} -> target={}", name, target);
            Ok(Arc::new(Pseudo::new(&target, FileType::SymLink)))
        } else {
            log::trace!("SysPciDevicesDirINode::find name={} -> EntryNotFound", name);
            Err(FsError::EntryNotFound)
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let devices = get_pci_devices();
        if id >= devices.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(devices[id].name.clone())
    }
}

struct SysPciDevDirINode {
    index: usize,
    name: String,
    vendor: String,
    device: String,
    class: String,
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(100 + self.index))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        log::trace!("SysPciDevDirINode::find name={} self.name={}", name, self.name);
        match name {
            "." => Ok(Arc::new(SysPciDevDirINode {
                index: self.index,
                name: self.name.clone(),
                vendor: self.vendor.clone(),
                device: self.device.clone(),
                class: self.class.clone(),
            })),
            ".." => Ok(Arc::new(SysDevicesPciBusDirINode)),
            "vendor" => Ok(Arc::new(Pseudo::new(&format!("{}\n", self.vendor), FileType::File))),
            "device" => Ok(Arc::new(Pseudo::new(&format!("{}\n", self.device), FileType::File))),
            "class" => Ok(Arc::new(Pseudo::new(&format!("{}\n", self.class), FileType::File))),
            "uevent" => {
                let vendor_hex = self.vendor.trim_start_matches("0x");
                let device_hex = self.device.trim_start_matches("0x");
                let class_hex = self.class.trim_start_matches("0x");
                let uevent_content = format!(
                    "PCI_CLASS={}\nPCI_ID={}:{}\nPCI_SUBSYS_ID=0000:0000\nPCI_SLOT_NAME={}\n",
                    class_hex, vendor_hex, device_hex, self.name
                );
                Ok(Arc::new(Pseudo::new(&uevent_content, FileType::File)))
            }
            "config" => {
                let mut cfg = [0u8; 256];
                let v = u16::from_str_radix(self.vendor.trim_start_matches("0x"), 16).unwrap_or(0);
                let d = u16::from_str_radix(self.device.trim_start_matches("0x"), 16).unwrap_or(0);
                let c = u32::from_str_radix(self.class.trim_start_matches("0x"), 16).unwrap_or(0);

                cfg[0..2].copy_from_slice(&v.to_le_bytes());
                cfg[2..4].copy_from_slice(&d.to_le_bytes());

                cfg[9] = (c & 0xff) as u8;
                cfg[10] = ((c >> 8) & 0xff) as u8;
                cfg[11] = ((c >> 16) & 0xff) as u8;

                Ok(Arc::new(Pseudo::new_bytes(cfg.to_vec(), FileType::File)))
            }
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = ["vendor", "device", "class", "config", "uevent"];
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
    }
}

struct PciDevInfo {
    name: String,
    vendor: String,
    device: String,
    class: String,
}

fn get_pci_devices() -> Vec<PciDevInfo> {
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
