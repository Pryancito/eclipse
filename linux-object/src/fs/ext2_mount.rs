//! ext2/ext3/ext4 mount support via ext2-rs.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use lock::Mutex;
use alloc::vec::Vec;
use core::any::Any;
use core::cmp::min;
use core::ops::Range;

use ext2::error::Error as Ext2RawError;
use ext2::fs::sync::{Inode as Ext2Inode, Synced};
use ext2::fs::Ext2;
use ext2::sector::{Address, Size512};
use ext2::volume::size::Size;
use ext2::volume::{Volume, VolumeCommit, VolumeSlice};
use rcore_fs::dev::Device;
use rcore_fs::vfs::{
    FileSystem, FileType, FsError, FsInfo, INode, Metadata, PollStatus, Result, Timespec,
};

use super::block_mount::device_from_backend;
use super::block_mount::MountBackend;

#[derive(Clone)]
pub(crate) struct Ext2Volume {
    inner: Arc<dyn Device>,
}

#[derive(Debug)]
pub(crate) struct Ext2VfsError(Ext2RawError);

impl From<Ext2VfsError> for FsError {
    fn from(_: Ext2VfsError) -> Self {
        FsError::DeviceError
    }
}

impl From<Ext2RawError> for Ext2VfsError {
    fn from(err: Ext2RawError) -> Self {
        Ext2VfsError(err)
    }
}

impl From<Ext2VfsError> for Ext2RawError {
    fn from(err: Ext2VfsError) -> Self {
        err.0
    }
}

impl From<rcore_fs::dev::DevError> for Ext2VfsError {
    fn from(_: rcore_fs::dev::DevError) -> Self {
        Ext2VfsError(Ext2RawError::Other(String::from("device error")))
    }
}

impl Volume<u8, Size512> for Ext2Volume {
    type Error = Ext2VfsError;

    fn size(&self) -> Size<Size512> {
        Size::Unbounded
    }

    fn commit(
        &mut self,
        _slice: Option<VolumeCommit<u8, Size512>>,
    ) -> core::result::Result<(), Self::Error> {
        Ok(())
    }

    unsafe fn slice_unchecked<'a>(
        &'a self,
        range: Range<Address<Size512>>,
    ) -> VolumeSlice<'a, u8, Size512> {
        let index = range.start;
        let len = range.end - range.start;
        let mut vec = vec![0u8; len.into_index() as usize];
        self.inner
            .read_at(index.into_index() as usize, vec.as_mut_slice())
            .unwrap();
        VolumeSlice::new_owned(vec, index)
    }

    fn slice<'a>(
        &'a self,
        range: Range<Address<Size512>>,
    ) -> core::result::Result<VolumeSlice<'a, u8, Size512>, Self::Error> {
        let index = range.start;
        let len = range.end - range.start;
        let mut vec = vec![0u8; len.into_index() as usize];
        self.inner
            .read_at(index.into_index() as usize, vec.as_mut_slice())
            .map_err(|_| Ext2VfsError(Ext2RawError::Other(String::from("device read"))))?;
        Ok(VolumeSlice::new_owned(vec, index))
    }
}

pub struct Ext2MountFs {
    pub(crate) synced: Synced<Ext2<Size512, Ext2Volume>>,
    pub(crate) device: Arc<dyn Device>,
    pub(crate) block_size: usize,
    this: Mutex<Weak<Self>>,
}

impl Ext2MountFs {
    pub fn open(backend: &MountBackend) -> Result<Arc<Self>> {
        let device = device_from_backend(backend)?;
        let volume = Ext2Volume {
            inner: device.clone(),
        };
        let synced = Synced::new(volume).map_err(|e: Ext2RawError| FsError::from(Ext2VfsError(e)))?;
        let block_size = {
            let inner = synced.inner();
            inner.block_size()
        };
        let arc = Arc::new(Self {
            synced,
            device,
            block_size,
            this: Mutex::new(Weak::new()),
        });
        *arc.this.lock() = Arc::downgrade(&arc);
        Ok(arc)
    }

    fn arc(&self) -> Arc<Self> {
        self.this.lock().upgrade().expect("Ext2MountFs dropped")
    }

    pub(crate) fn inode_from_num(&self, num: usize) -> Result<Ext2Inode<Size512, Ext2Volume>> {
        self.synced
            .inode_nth(num)
            .ok_or(FsError::EntryNotFound)
    }
}

impl FileSystem for Ext2MountFs {
    fn sync(&self) -> Result<()> {
        self.device.sync().map_err(|_| FsError::DeviceError)
    }

    fn root_inode(&self) -> Arc<dyn INode> {
        Arc::new(Ext2MountINode {
            fs: self.arc(),
            inode: self.synced.root_inode(),
            inode_num: 2,
        })
    }

    fn info(&self) -> FsInfo {
        let inner = self.synced.inner();
        FsInfo {
            bsize: self.block_size,
            frsize: self.block_size,
            blocks: inner.total_block_count(),
            bfree: inner.free_block_count(),
            bavail: inner.free_block_count(),
            files: inner.total_inodes_count(),
            ffree: inner.free_inodes_count_raw() as usize,
            namemax: 255,
        }
    }
}

struct Ext2MountINode {
    fs: Arc<Ext2MountFs>,
    inode: Ext2Inode<Size512, Ext2Volume>,
    inode_num: usize,
}

impl Ext2MountINode {
    fn read_inode_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        if self.inode.is_dir() {
            return Err(FsError::IsDir);
        }
        if self.inode.is_symlink() {
            let target = self
                .fs
                .editor()
                .read_symlink(self.inode_num as u32)?;
            if offset >= target.len() {
                return Ok(0);
            }
            let take = min(buf.len(), target.len() - offset);
            buf[..take].copy_from_slice(&target[offset..offset + take]);
            return Ok(take);
        }
        let total = self.inode.size();
        if offset >= total {
            return Ok(0);
        }
        let want = min(buf.len(), total - offset);
        let block_size = self.fs.block_size;
        let mut done = 0;
        let mut skip = offset % block_size;
        for block in self.inode.blocks() {
            if done >= want {
                break;
            }
            let (data, _) = block.map_err(|_| FsError::DeviceError)?;
            if skip >= data.len() {
                skip -= data.len();
                continue;
            }
            let take = min(want - done, data.len() - skip);
            buf[done..done + take].copy_from_slice(&data[skip..skip + take]);
            done += take;
            skip = 0;
        }
        Ok(done)
    }

    fn write_inode_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        if self.inode.is_dir() {
            return Err(FsError::IsDir);
        }
        if self.inode.is_symlink() {
            return self
                .fs
                .editor()
                .write_symlink(self.inode_num as u32, offset, buf);
        }
        let end = offset.saturating_add(buf.len());
        let mut inode = self.inode.clone();
        if end > inode.size() {
            self.fs
                .editor()
                .ensure_file_size(self.inode_num as u32, end)?;
            inode = self.fs.inode_from_num(self.inode_num)?;
        }
        let total = inode.size();
        let block_size = self.fs.block_size;
        let mut done = 0;
        while done < buf.len() {
            let abs = offset + done;
            if abs >= total {
                break;
            }
            let file_block = abs / block_size;
            let block_off = abs % block_size;
            let take = min(buf.len() - done, block_size - block_off);
            let disk_block = inode
                .try_block(file_block)
                .map_err(|_| FsError::DeviceError)?
                .ok_or(FsError::NoDeviceSpace)?;
            let byte_base = Address::<Size512>::with_block_size(
                disk_block.get(),
                block_off as i32,
                self.fs.synced.inner().log_block_size(),
            )
            .into_index() as usize;
            let mut sector = vec![0u8; take];
            sector.copy_from_slice(&buf[done..done + take]);
            if take < block_size {
                let mut temp = vec![0u8; block_size];
                self.fs
                    .device
                    .read_at(byte_base & !(block_size - 1), &mut temp)
                    .map_err(|_| FsError::DeviceError)?;
                temp[block_off..block_off + take].copy_from_slice(&sector);
                self.fs
                    .device
                    .write_at(byte_base & !(block_size - 1), &temp)
                    .map_err(|_| FsError::DeviceError)?;
            } else {
                self.fs
                    .device
                    .write_at(byte_base, &sector)
                    .map_err(|_| FsError::DeviceError)?;
            }
            done += take;
        }
        Ok(done)
    }

    fn list_dir_entries(&self) -> Result<Vec<(String, usize)>> {
        let mut out = Vec::new();
        let dir = self.inode.directory().ok_or(FsError::NotDir)?;
        for entry in dir {
            let entry = entry.map_err(|_| FsError::DeviceError)?;
            let name = String::from_utf8(entry.name).map_err(|_| FsError::InvalidParam)?;
            if name == "." || name == ".." {
                continue;
            }
            out.push((name, entry.inode));
        }
        Ok(out)
    }
}

impl INode for Ext2MountINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.read_inode_at(offset, buf)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        self.write_inode_at(offset, buf)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: !self.inode.is_dir(),
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let is_dir = self.inode.is_dir();
        let is_symlink = self.inode.is_symlink();
        let editor = self.fs.editor();
        let size = if is_symlink {
            editor.read_symlink(self.inode_num as u32)?.len()
        } else {
            self.inode.size()
        };
        let (uid, gid) = editor.raw_uid_gid(self.inode_num as u32)?;
        Ok(Metadata {
            dev: 0,
            inode: self.inode_num,
            size,
            blk_size: 512,
            blocks: (size + 511) / 512,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: if is_dir {
                FileType::Dir
            } else if is_symlink {
                FileType::SymLink
            } else {
                FileType::File
            },
            mode: editor.raw_mode(self.inode_num as u32)?,
            nlinks: editor.raw_hard_links(self.inode_num as u32)? as usize,
            uid: uid as usize,
            gid: gid as usize,
            rdev: 0,
        })
    }

    fn find(&self, name: &str) -> Result<Arc<dyn INode>> {
        match name {
            "." => Ok(Arc::new(Ext2MountINode {
                fs: self.fs.clone(),
                inode: self.inode.clone(),
                inode_num: self.inode_num,
            })),
            ".." => Err(FsError::EntryNotFound),
            name => {
                for (entry_name, inode_num) in self.list_dir_entries()? {
                    if entry_name == name {
                        let child = self.fs.inode_from_num(inode_num)?;
                        return Ok(Arc::new(Ext2MountINode {
                            fs: self.fs.clone(),
                            inode: child,
                            inode_num,
                        }));
                    }
                }
                Err(FsError::EntryNotFound)
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        match id {
            0 => Ok(String::from(".")),
            1 => Ok(String::from("..")),
            i => {
                let entries = self.list_dir_entries()?;
                entries
                    .get(i - 2)
                    .map(|(name, _)| name.clone())
                    .ok_or(FsError::EntryNotFound)
            }
        }
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.fs.editor().update_metadata(
            self.inode_num as u32,
            metadata.mode as u32,
            metadata.uid,
            metadata.gid,
        )
    }

    fn sync_all(&self) -> Result<()> {
        self.fs.device.sync().map_err(|_| FsError::DeviceError)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    fn create(&self, name: &str, type_: FileType, mode: u32) -> Result<Arc<dyn INode>> {
        if !self.inode.is_dir() {
            return Err(FsError::NotDir);
        }
        let child = self
            .fs
            .editor()
            .create(self.inode_num as u32, name, type_, mode)?;
        let inode = self.fs.inode_from_num(child as usize)?;
        Ok(Arc::new(Ext2MountINode {
            fs: self.fs.clone(),
            inode,
            inode_num: child as usize,
        }))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        if !self.inode.is_dir() {
            return Err(FsError::NotDir);
        }
        self.fs
            .editor()
            .unlink(self.inode_num as u32, name)
    }

    fn resize(&self, len: usize) -> Result<()> {
        if self.inode.is_dir() {
            return Err(FsError::IsDir);
        }
        self.fs.editor().resize(self.inode_num as u32, len)
    }

    fn link(&self, name: &str, other: &Arc<dyn INode>) -> Result<()> {
        if !self.inode.is_dir() {
            return Err(FsError::NotDir);
        }
        let other = other
            .downcast_ref::<Ext2MountINode>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &other.fs) {
            return Err(FsError::NotSameFs);
        }
        self.fs
            .editor()
            .link(self.inode_num as u32, name, other.inode_num as u32)
    }

    fn move_(&self, old_name: &str, target: &Arc<dyn INode>, new_name: &str) -> Result<()> {
        let target = target
            .downcast_ref::<Ext2MountINode>()
            .ok_or(FsError::NotSameFs)?;
        if !Arc::ptr_eq(&self.fs, &target.fs) {
            return Err(FsError::NotSameFs);
        }
        self.fs.editor().rename_across(
            self.inode_num as u32,
            old_name,
            target.inode_num as u32,
            new_name,
        )
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

pub fn open_ext2(backend: &MountBackend) -> Result<Arc<dyn FileSystem>> {
    Ext2MountFs::open(backend).map(|fs| fs as Arc<dyn FileSystem>)
}
