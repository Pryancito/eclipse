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

fn dir_metadata() -> Metadata {
    Metadata {
        dev: 0,
        inode: 0,
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
    fn entries() -> [&'static str; 2] {
        ["class", "block"]
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
        Ok(dir_metadata())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." | ".." => Ok(Arc::new(SysRootINode)),
            "class" => Ok(Arc::new(SysClassINode)),
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
        Ok(dir_metadata())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
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
        Ok(dir_metadata())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
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
        Ok(dir_metadata())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
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
