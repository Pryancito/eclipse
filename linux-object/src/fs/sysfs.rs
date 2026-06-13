//! Minimal sysfs implementation for Linux userland compatibility.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::Any;

use kernel_hal::drivers;
use kernel_hal::net::get_net_device;
use lazy_static::lazy_static;
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
            "." | ".." => Ok(SYS_ROOT.clone()),
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
    fn entries() -> [&'static str; 4] {
        ["block", "drm", "net", "power_supply"]
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
            "drm" => Ok(Arc::new(SysClassDrmDirINode)),
            "net" => Ok(Arc::new(SysClassNetDirINode)),
            "power_supply" => Ok(Arc::new(SysClassPowerSupplyDirINode)),
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
            "system" => Ok(Arc::new(SysDevicesSystemDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok("pci0000:00".into()),
            1 => Ok("system".into()),
            _ => Err(FsError::EntryNotFound),
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(110))
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
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        if id == 0 {
            Ok("node".into())
        } else {
            Err(FsError::EntryNotFound)
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(120))
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
        if id == 0 {
            Ok("node0".into())
        } else {
            Err(FsError::EntryNotFound)
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(130))
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
                let num_groups = (cpu_count + 31) / 32;
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
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
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
            log::trace!(
                "SysDevicesPciBusDirINode::find name={} -> EntryNotFound",
                name
            );
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
            log::trace!(
                "SysPciDevicesDirINode::find name={} -> target={}",
                name,
                target
            );
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
            "modalias" => Ok(Arc::new(Pseudo::new(
                &pci_modalias(&self.vendor, &self.device, &self.class),
                FileType::File,
            ))),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        let entries = ["vendor", "device", "class", "config", "uevent", "modalias"];
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

fn pci_modalias(vendor: &str, device: &str, class: &str) -> String {
    let v = u32::from_str_radix(vendor.trim_start_matches("0x"), 16).unwrap_or(0);
    let d = u32::from_str_radix(device.trim_start_matches("0x"), 16).unwrap_or(0);
    let c = class.trim_start_matches("0x");
    let (bc, sc, pi) = if c.len() >= 6 {
        (&c[0..2], &c[2..4], &c[4..6])
    } else {
        ("00", "00", "00")
    };
    format!(
        "pci:v{v:08x}d{d:08x}sv00000000sd00000000bc{bc}sc{sc}i{pi}\n",
        v = v,
        d = d,
        bc = bc,
        sc = sc,
        pi = pi
    )
}

fn display_pci_index() -> Option<usize> {
    let devs = get_pci_devices();
    devs.iter()
        .position(|d| d.class.starts_with("0x03"))
        .or_else(|| (!devs.is_empty()).then_some(0))
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(21))
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
            "card0" => display_pci_index()
                .map(|idx| Arc::new(SysDrmCardINode { pci_index: idx }) as Arc<dyn INode>)
                .ok_or(FsError::EntryNotFound),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        if id == 0 && display_pci_index().is_some() {
            Ok("card0".into())
        } else {
            Err(FsError::EntryNotFound)
        }
    }
}

struct SysDrmCardINode {
    pci_index: usize,
}

impl SysDrmCardINode {
    fn entries() -> [&'static str; 1] {
        ["device"]
    }
}

impl INode for SysDrmCardINode {
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
        Ok(dir_metadata(22))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(SysDrmCardINode {
                pci_index: self.pci_index,
            })),
            ".." => Ok(Arc::new(SysClassDrmDirINode)),
            "device" => Ok(Arc::new(SysDrmCardDeviceINode {
                pci_index: self.pci_index,
            })),
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

struct SysDrmCardDeviceINode {
    pci_index: usize,
}

impl INode for SysDrmCardDeviceINode {
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
        Ok(dir_metadata(23))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        let devs = get_pci_devices();
        let pci = devs.get(self.pci_index).ok_or(FsError::EntryNotFound)?;
        match name {
            "." => Ok(Arc::new(SysDrmCardDeviceINode {
                pci_index: self.pci_index,
            })),
            ".." => Ok(Arc::new(SysDrmCardINode {
                pci_index: self.pci_index,
            })),
            "modalias" => Ok(Arc::new(Pseudo::new(
                &pci_modalias(&pci.vendor, &pci.device, &pci.class),
                FileType::File,
            ))),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, id: usize) -> Result<String> {
        if id == 0 {
            Ok("modalias".into())
        } else {
            Err(FsError::EntryNotFound)
        }
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(24))
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
        let names = list_net_ifnames();
        if id >= names.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(names[id].clone())
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(25))
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
        let entries = Self::entries();
        if id >= entries.len() {
            return Err(FsError::EntryNotFound);
        }
        Ok(entries[id].into())
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
        })
    }
    fn metadata(&self) -> Result<Metadata> {
        Ok(dir_metadata(26))
    }
    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(SysFS)
    }
    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | ".." => Ok(Arc::new(SysClassPowerSupplyDirINode)),
            _ => Err(FsError::EntryNotFound),
        }
    }
    fn get_entry(&self, _id: usize) -> Result<String> {
        Err(FsError::EntryNotFound)
    }
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
