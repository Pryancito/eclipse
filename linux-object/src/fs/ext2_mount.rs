//! ext2/ext3/ext4 mount support via ext2-rs.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::cmp::min;
use core::ops::Range;
use lock::Mutex;

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
        // Do NOT panic on device I/O errors: this is reachable from mount and
        // inode lookups. On failure leave the buffer zero-filled rather than
        // crashing the whole kernel.
        let _ = self
            .inner
            .read_at(index.into_index() as usize, vec.as_mut_slice());
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
    /// Cached directory listings keyed by inode number.
    dir_cache: Mutex<BTreeMap<usize, Vec<(String, usize)>>>,
    this: Mutex<Weak<Self>>,
}

impl Ext2MountFs {
    pub fn open(backend: &MountBackend) -> Result<Arc<Self>> {
        let device = device_from_backend(backend)?;
        let volume = Ext2Volume {
            inner: device.clone(),
        };
        let synced =
            Synced::new(volume).map_err(|e: Ext2RawError| FsError::from(Ext2VfsError(e)))?;
        let block_size = {
            let inner = synced.inner();
            inner.block_size()
        };
        let arc = Arc::new(Self {
            synced,
            device,
            block_size,
            dir_cache: Mutex::new(BTreeMap::new()),
            this: Mutex::new(Weak::new()),
        });
        *arc.this.lock() = Arc::downgrade(&arc);
        Ok(arc)
    }

    fn arc(&self) -> Arc<Self> {
        self.this.lock().upgrade().expect("Ext2MountFs dropped")
    }

    pub(crate) fn inode_from_num(&self, num: usize) -> Result<Ext2Inode<Size512, Ext2Volume>> {
        self.synced.inode_nth(num).ok_or(FsError::EntryNotFound)
    }

    pub(crate) fn dir_entries_cached(
        &self,
        inode_num: usize,
        scan: impl FnOnce() -> Result<Vec<(String, usize)>>,
    ) -> Result<Vec<(String, usize)>> {
        if let Some(entries) = self.dir_cache.lock().get(&inode_num) {
            return Ok(entries.clone());
        }
        let entries = scan()?;
        self.dir_cache.lock().insert(inode_num, entries.clone());
        Ok(entries)
    }

    pub(crate) fn invalidate_dir_cache(&self, inode_num: usize) {
        self.dir_cache.lock().remove(&inode_num);
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
    /// Re-read this inode from the backing store.
    ///
    /// The `self.inode` field is an immutable in-memory snapshot taken at
    /// lookup time, while every mutation (write, resize, dir growth, …) goes
    /// through the on-disk `editor`.  Reading through the snapshot therefore
    /// returns stale size/type/nlink and, crucially, a stale block map (so
    /// freshly allocated directory/file blocks would be invisible).  Read
    /// paths must use this fresh copy instead.  Falls back to the snapshot if
    /// the inode can no longer be read (e.g. concurrently freed).
    fn fresh_inode(&self) -> Ext2Inode<Size512, Ext2Volume> {
        self.fs
            .inode_from_num(self.inode_num)
            .unwrap_or_else(|_| self.inode.clone())
    }

    fn read_inode_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let cur = self.fresh_inode();
        if cur.is_dir() {
            return Err(FsError::IsDir);
        }
        if cur.is_symlink() {
            let target = self.fs.editor().read_symlink(self.inode_num as u32)?;
            if offset >= target.len() {
                return Ok(0);
            }
            let take = min(buf.len(), target.len() - offset);
            buf[..take].copy_from_slice(&target[offset..offset + take]);
            return Ok(take);
        }
        let total = cur.size();
        if offset >= total {
            return Ok(0);
        }
        let want = min(buf.len(), total - offset);
        let block_size = self.fs.block_size;
        let log_block_size = self.fs.synced.inner().log_block_size();
        let mut done = 0;
        let mut pos = offset;
        // Use the freshly-read inode's block map. Fall back to the on-disk
        // editor only when try_block cannot resolve an indirect pointer.
        while done < want {
            let file_block = pos / block_size;
            let block_off = pos % block_size;
            let disk_block = match cur.try_block(file_block) {
                Ok(Some(b)) => b.get(),
                Ok(None) => {
                    if done == 0 {
                        return self
                            .fs
                            .editor()
                            .read_file_at(self.inode_num as u32, offset, buf);
                    }
                    let tail = self.fs.editor().read_file_at(
                        self.inode_num as u32,
                        pos,
                        &mut buf[done..],
                    )?;
                    done += tail;
                    break;
                }
                Err(_) => return Err(FsError::DeviceError),
            };
            let byte_base =
                Address::<Size512>::with_block_size(disk_block, block_off as i32, log_block_size)
                    .into_index() as usize;
            let take = min(want - done, block_size - block_off);
            self.fs
                .device
                .read_at(byte_base, &mut buf[done..done + take])
                .map_err(|_| FsError::DeviceError)?;
            done += take;
            pos += take;
        }
        Ok(done)
    }

    fn write_inode_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut inode = self.fresh_inode();
        if inode.is_dir() {
            return Err(FsError::IsDir);
        }
        if inode.is_symlink() {
            return self
                .fs
                .editor()
                .write_symlink(self.inode_num as u32, offset, buf);
        }
        let end = offset.saturating_add(buf.len());
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
        if done > 0 {
            let _ = self.fs.editor().touch_times(self.inode_num as u32, true);
        }
        Ok(done)
    }

    /// Number of data blocks backing this inode, derived from its size.
    ///
    /// Directories store their entries in fully-allocated blocks up to
    /// `i_size`, so this also bounds the directory-scan loops below.  Capped at
    /// a generous limit to stay safe against a corrupted size field.
    fn data_block_count(&self, inode: &Ext2Inode<Size512, Ext2Volume>) -> usize {
        const MAX_DIR_BLOCKS: usize = 1 << 20; // 1M blocks (>= 1GiB of dir data)
        let block_size = self.fs.block_size;
        let blocks = (inode.size() + block_size - 1) / block_size;
        blocks.min(MAX_DIR_BLOCKS)
    }

    /// Look up a single child by name scanning every directory block,
    /// including those reachable through indirect block pointers.
    fn find_direct_child(&self, name: &str) -> Result<usize> {
        let cur = self.fresh_inode();
        if !cur.is_dir() {
            return Err(FsError::NotDir);
        }
        let block_size = self.fs.block_size;
        let log_block_size = self.fs.synced.inner().log_block_size();
        let n_blocks = self.data_block_count(&cur).max(1);
        for block_idx in 0..n_blocks {
            let disk_block = match cur.try_block(block_idx) {
                Ok(Some(b)) => b.get(),
                Ok(None) => break,
                Err(_) => return Err(FsError::DeviceError),
            };
            let byte_base = Address::<Size512>::with_block_size(disk_block, 0, log_block_size)
                .into_index() as usize;
            let mut block = vec![0u8; block_size];
            self.fs
                .device
                .read_at(byte_base, &mut block)
                .map_err(|_| FsError::DeviceError)?;
            let mut off = 0usize;
            while off < block_size {
                if off + 8 > block_size {
                    break;
                }
                let rec = &block[off..];
                let inode_num = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
                let rec_len = u16::from_le_bytes([rec[4], rec[5]]) as usize;
                if rec_len < 8 || rec_len % 4 != 0 || off + rec_len > block_size {
                    break;
                }
                if inode_num == 0 {
                    // A zero inode is a deleted entry (hole), not end-of-block:
                    // `remove_dir_entry` leaves one when the first entry of a
                    // block is unlinked. Live entries may follow it.
                    off += rec_len;
                    continue;
                }
                let name_len = rec[6] as usize;
                if name_len + 8 > rec_len {
                    break;
                }
                let entry_name = core::str::from_utf8(&rec[8..8 + name_len])
                    .map_err(|_| FsError::InvalidParam)?;
                if entry_name != "." && entry_name != ".." && entry_name == name {
                    return Ok(inode_num as usize);
                }
                off += rec_len;
            }
        }
        Err(FsError::EntryNotFound)
    }

    /// Scan directory data blocks using the synced inode's block map, walking
    /// both direct and indirect block pointers. Used for path lookup during
    /// boot and for `readdir`; file reads use `read_file_at`.
    fn scan_direct_dir_blocks(&self) -> Result<Vec<(String, usize)>> {
        const MAX_DIR_ENTRIES: usize = 65536;
        let cur = self.fresh_inode();
        if !cur.is_dir() {
            return Err(FsError::NotDir);
        }
        let block_size = self.fs.block_size;
        let log_block_size = self.fs.synced.inner().log_block_size();
        let n_blocks = self.data_block_count(&cur).max(1);
        let mut out = Vec::new();
        for block_idx in 0..n_blocks {
            let disk_block = match cur.try_block(block_idx) {
                Ok(Some(b)) => b.get(),
                Ok(None) => break,
                Err(_) => return Err(FsError::DeviceError),
            };
            let byte_base = Address::<Size512>::with_block_size(disk_block, 0, log_block_size)
                .into_index() as usize;
            let mut block = vec![0u8; block_size];
            self.fs
                .device
                .read_at(byte_base, &mut block)
                .map_err(|_| FsError::DeviceError)?;
            let mut off = 0usize;
            while off < block_size {
                if out.len() >= MAX_DIR_ENTRIES {
                    return Ok(out);
                }
                if off + 8 > block_size {
                    break;
                }
                let rec = &block[off..];
                let inode_num = u32::from_le_bytes([rec[0], rec[1], rec[2], rec[3]]);
                let rec_len = u16::from_le_bytes([rec[4], rec[5]]) as usize;
                if rec_len < 8 || rec_len % 4 != 0 || off + rec_len > block_size {
                    break;
                }
                if inode_num == 0 {
                    // Deleted entry (hole); skip it, live entries may follow.
                    off += rec_len;
                    continue;
                }
                let name_len = rec[6] as usize;
                if name_len + 8 > rec_len {
                    break;
                }
                let name = core::str::from_utf8(&rec[8..8 + name_len])
                    .map_err(|_| FsError::InvalidParam)?;
                if name != "." && name != ".." {
                    out.push((String::from(name), inode_num as usize));
                }
                off += rec_len;
            }
        }
        Ok(out)
    }

    fn list_dir_entries(&self) -> Result<Vec<(String, usize)>> {
        if !self.inode.is_dir() {
            return Err(FsError::NotDir);
        }
        self.fs
            .dir_entries_cached(self.inode_num, || self.scan_direct_dir_blocks())
    }

    fn child_metadata(inode_num: usize, inode: &Ext2Inode<Size512, Ext2Volume>) -> Metadata {
        let size = inode.size();
        Metadata {
            dev: 0,
            inode: inode_num,
            size,
            blk_size: 512,
            blocks: (size + 511) / 512,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: if inode.is_dir() {
                FileType::Dir
            } else if inode.is_symlink() {
                FileType::SymLink
            } else {
                FileType::File
            },
            mode: inode.mode_bits(),
            nlinks: inode.nlink() as usize,
            uid: inode.uid() as usize,
            gid: inode.gid() as usize,
            rdev: 0,
        }
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
        // Read the live inode so `stat` reflects writes/resizes/chmod made
        // through the editor since this handle was opened.
        let cur = self.fresh_inode();
        // Authoritative type/rdev/timestamps from the raw inode (the synced
        // `is_dir`/`is_symlink` accessors cannot distinguish device/fifo/socket
        // nodes, which share format bits with dir/symlink).
        let (type_, rdev, atime, mtime, ctime) = self
            .fs
            .editor()
            .inode_meta_extra(self.inode_num as u32)
            .unwrap_or((FileType::File, 0, 0, 0, 0));
        let size = if matches!(type_, FileType::SymLink) {
            // Fall back to the inode's size field if the target cannot be
            // read (e.g. a corrupted block-mapped symlink): failing lstat
            // here would make the entry impossible to inspect or replace.
            self.fs
                .editor()
                .read_symlink(self.inode_num as u32)
                .map(|t| t.len())
                .unwrap_or_else(|_| cur.size())
        } else {
            cur.size()
        };
        Ok(Metadata {
            dev: 0,
            inode: self.inode_num,
            size,
            blk_size: 512,
            blocks: (size + 511) / 512,
            atime: Timespec {
                sec: atime as i64,
                nsec: 0,
            },
            mtime: Timespec {
                sec: mtime as i64,
                nsec: 0,
            },
            ctime: Timespec {
                sec: ctime as i64,
                nsec: 0,
            },
            type_,
            mode: cur.mode_bits(),
            nlinks: cur.nlink() as usize,
            uid: cur.uid() as usize,
            gid: cur.gid() as usize,
            rdev,
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
                let inode_num = self.find_direct_child(name)?;
                let child = self.fs.inode_from_num(inode_num)?;
                Ok(Arc::new(Ext2MountINode {
                    fs: self.fs.clone(),
                    inode: child,
                    inode_num,
                }))
            }
        }
    }

    fn get_entry(&self, id: usize) -> Result<String> {
        Ok(self.get_entry_with_metadata(id)?.1)
    }

    fn get_entry_with_metadata(&self, id: usize) -> Result<(Metadata, String)> {
        match id {
            0 => Ok((self.metadata()?, String::from("."))),
            1 => Ok((self.metadata()?, String::from(".."))),
            i => {
                let entries = self.list_dir_entries()?;
                let (name, child_num) = entries.get(i - 2).ok_or(FsError::EntryNotFound)?;
                let child = self.fs.inode_from_num(*child_num)?;
                Ok((Self::child_metadata(*child_num, &child), name.clone()))
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

    fn create2(
        &self,
        name: &str,
        type_: FileType,
        mode: u32,
        data: usize,
    ) -> Result<Arc<dyn INode>> {
        if !self.fresh_inode().is_dir() {
            return Err(FsError::NotDir);
        }
        let child = self
            .fs
            .editor()
            .create(self.inode_num as u32, name, type_, mode, data)?;
        self.fs.invalidate_dir_cache(self.inode_num);
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
        self.fs.editor().unlink(self.inode_num as u32, name)?;
        self.fs.invalidate_dir_cache(self.inode_num);
        Ok(())
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
            .link(self.inode_num as u32, name, other.inode_num as u32)?;
        self.fs.invalidate_dir_cache(self.inode_num);
        Ok(())
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
        )?;
        self.fs.invalidate_dir_cache(self.inode_num);
        self.fs.invalidate_dir_cache(target.inode_num);
        Ok(())
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

pub fn open_ext2(backend: &MountBackend) -> Result<Arc<dyn FileSystem>> {
    Ext2MountFs::open(backend).map(|fs| fs as Arc<dyn FileSystem>)
}
