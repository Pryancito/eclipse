//! ext2 on-disk mutations (create, unlink, resize).

use alloc::vec::Vec;
use core::mem;

use ext2::sector::Address;
use ext2::sys::inode::{Inode as RawInode, TypePerm, DIRECTORY, FILE, SYMLINK};
use rcore_fs::vfs::{FileType, FsError, Result};

use super::ext2_mount::Ext2MountFs;

const SUPERBLOCK_OFFSET: usize = 1024;
const FAST_SYMLINK_LEN: usize = 60;

pub(crate) struct Ext2Editor<'a> {
    fs: &'a Ext2MountFs,
}

impl<'a> Ext2Editor<'a> {
    pub fn new(fs: &'a Ext2MountFs) -> Self {
        Self { fs }
    }

    fn block_size(&self) -> usize {
        self.fs.block_size
    }

    fn log_block_size(&self) -> u32 {
        self.fs.synced.inner().log_block_size()
    }

    fn inodes_per_group(&self) -> usize {
        self.fs.synced.inner().inodes_count()
    }

    fn blocks_per_group(&self) -> u32 {
        self.fs.synced.inner().blocks_per_group()
    }

    fn first_data_block(&self) -> u32 {
        self.fs.synced.inner().first_data_block()
    }

    fn inode_size(&self) -> usize {
        self.fs.synced.inner().inode_size()
    }

    fn bgdt_byte_offset(&self, bg: usize) -> usize {
        let first = self.first_data_block();
        let table_base = Address::<ext2::sector::Size512>::with_block_size(
            first + 1,
            0,
            self.log_block_size(),
        );
        let descr_size = mem::size_of::<ext2::sys::block_group::BlockGroupDescriptor>();
        (table_base + Address::<ext2::sector::Size512>::from(bg * descr_size)).into_index() as usize
    }

    fn inode_byte_offset(&self, inode_num: u32) -> usize {
        // NOTE: `synced.inner()` is a non-reentrant `spin::Mutex`.  The helper
        // accessors below (`inodes_per_group`, `inode_size`, `log_block_size`)
        // each lock it independently, so they MUST be evaluated *before* we hold
        // our own guard — otherwise we self-deadlock and hang the whole core.
        // This path is reached by `read_raw_inode`, e.g. when resolving a
        // symlink (`/bin/ls` -> busybox), which is why `ls` froze the system
        // while `busybox ls` (a regular file, read via the synced inode) did not.
        let inodes_per_group = self.inodes_per_group();
        let inode_size = self.inode_size();
        let log_block_size = self.log_block_size();
        let index = (inode_num - 1) as usize;
        let bg = index / inodes_per_group;
        let idx = index % inodes_per_group;
        let table_block = self.fs.synced.inner().block_group(bg).inode_table_block;
        Address::<ext2::sector::Size512>::with_block_size(
            table_block,
            (idx * inode_size) as i32,
            log_block_size,
        )
        .into_index() as usize
    }

    fn read_block(&self, block: u32) -> Result<Vec<u8>> {
        let bs = self.block_size();
        let base =
            Address::<ext2::sector::Size512>::with_block_size(block, 0, self.log_block_size())
                .into_index() as usize;
        let mut buf = vec![0u8; bs];
        self.fs
            .device
            .read_at(base, &mut buf)
            .map_err(|_| FsError::DeviceError)?;
        Ok(buf)
    }

    fn write_block(&self, block: u32, data: &[u8]) -> Result<()> {
        let bs = self.block_size();
        let base =
            Address::<ext2::sector::Size512>::with_block_size(block, 0, self.log_block_size())
                .into_index() as usize;
        let slice = if data.len() == bs {
            data
        } else {
            let mut temp = vec![0u8; bs];
            temp[..data.len()].copy_from_slice(data);
            self.fs
                .device
                .write_at(base, &temp)
                .map_err(|_| FsError::DeviceError)?;
            return Ok(());
        };
        self.fs
            .device
            .write_at(base, slice)
            .map_err(|_| FsError::DeviceError)?;
        Ok(())
    }

    fn read_raw_inode(&self, inode_num: u32) -> Result<RawInode> {
        let offset = self.inode_byte_offset(inode_num);
        let size = self.inode_size();
        let mut buf = vec![0u8; size];
        self.fs
            .device
            .read_at(offset, &mut buf)
            .map_err(|_| FsError::DeviceError)?;
        Ok(unsafe { *(buf.as_ptr() as *const RawInode) })
    }

    fn write_raw_inode(&self, inode_num: u32, inode: &RawInode) -> Result<()> {
        let offset = self.inode_byte_offset(inode_num);
        let bytes = unsafe {
            core::slice::from_raw_parts(
                inode as *const RawInode as *const u8,
                self.inode_size(),
            )
        };
        self.fs
            .device
            .write_at(offset, bytes)
            .map_err(|_| FsError::DeviceError)?;
        Ok(())
    }

    fn write_superblock_counts(&self) -> Result<()> {
        let inner = self.fs.synced.inner();
        let mut buf = vec![0u8; mem::size_of::<ext2::sys::superblock::Superblock>()];
        self.fs
            .device
            .read_at(SUPERBLOCK_OFFSET, &mut buf)
            .map_err(|_| FsError::DeviceError)?;
        buf[12..16].copy_from_slice(&inner.free_blocks_count_raw().to_le_bytes());
        buf[16..20].copy_from_slice(&inner.free_inodes_count_raw().to_le_bytes());
        self.fs
            .device
            .write_at(SUPERBLOCK_OFFSET, &buf)
            .map_err(|_| FsError::DeviceError)?;
        Ok(())
    }

    fn write_bg_descriptor(&self, bg: usize) -> Result<()> {
        let descr = self.fs.synced.inner().block_group(bg);
        let offset = self.bgdt_byte_offset(bg);
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &descr as *const ext2::sys::block_group::BlockGroupDescriptor as *const u8,
                mem::size_of::<ext2::sys::block_group::BlockGroupDescriptor>(),
            )
        };
        self.fs
            .device
            .write_at(offset, bytes)
            .map_err(|_| FsError::DeviceError)?;
        Ok(())
    }

    fn alloc_inode(&self) -> Result<u32> {
        let inodes_per_group = self.inodes_per_group();
        let groups = self.fs.synced.inner().block_groups_len();
        for bg in 0..groups {
            if self.fs.synced.inner().block_group(bg).free_inodes_count == 0 {
                continue;
            }
            let bitmap_block = self.fs.synced.inner().block_group(bg).inode_usage_addr;
            let mut bitmap = self.read_block(bitmap_block)?;
            for bit in 0..inodes_per_group {
                let byte = bit / 8;
                let mask = 1u8 << (bit % 8);
                if bitmap[byte] & mask != 0 {
                    continue;
                }
                bitmap[byte] |= mask;
                self.write_block(bitmap_block, &bitmap)?;
                {
                    let mut inner = self.fs.synced.inner();
                    inner.block_group_mut(bg).free_inodes_count -= 1;
                    inner.superblock_mut().free_inodes_count -= 1;
                }
                self.write_bg_descriptor(bg)?;
                self.write_superblock_counts()?;
                return Ok((bg * inodes_per_group + bit + 1) as u32);
            }
        }
        Err(FsError::NoDeviceSpace)
    }

    fn free_inode(&self, inode_num: u32) -> Result<()> {
        let index = (inode_num - 1) as usize;
        let bg = index / self.inodes_per_group();
        let bit = index % self.inodes_per_group();
        let bitmap_block = self.fs.synced.inner().block_group(bg).inode_usage_addr;
        let mut bitmap = self.read_block(bitmap_block)?;
        let byte = bit / 8;
        let mask = 1u8 << (bit % 8);
        if bitmap[byte] & mask == 0 {
            return Ok(());
        }
        bitmap[byte] &= !mask;
        self.write_block(bitmap_block, &bitmap)?;
        {
            let mut inner = self.fs.synced.inner();
            inner.block_group_mut(bg).free_inodes_count += 1;
            inner.superblock_mut().free_inodes_count += 1;
        }
        self.write_bg_descriptor(bg)?;
        self.write_superblock_counts()?;
        Ok(())
    }

    fn alloc_block(&self) -> Result<u32> {
        let blocks_per_group = self.blocks_per_group() as usize;
        let first = self.first_data_block();
        let groups = self.fs.synced.inner().block_groups_len();
        for bg in 0..groups {
            if self.fs.synced.inner().block_group(bg).free_blocks_count == 0 {
                continue;
            }
            let bitmap_block = self.fs.synced.inner().block_group(bg).block_usage_addr;
            let mut bitmap = self.read_block(bitmap_block)?;
            for bit in 0..blocks_per_group {
                let byte = bit / 8;
                let mask = 1u8 << (bit % 8);
                if bitmap[byte] & mask != 0 {
                    continue;
                }
                bitmap[byte] |= mask;
                self.write_block(bitmap_block, &bitmap)?;
                {
                    let mut inner = self.fs.synced.inner();
                    inner.block_group_mut(bg).free_blocks_count -= 1;
                    inner.superblock_mut().free_blocks_count -= 1;
                }
                self.write_bg_descriptor(bg)?;
                self.write_superblock_counts()?;
                return Ok(first + (bg as u32) * self.blocks_per_group() + bit as u32);
            }
        }
        Err(FsError::NoDeviceSpace)
    }

    fn free_block(&self, block: u32) -> Result<()> {
        let first = self.first_data_block();
        if block < first {
            return Ok(());
        }
        let rel = (block - first) as usize;
        let bg = rel / self.blocks_per_group() as usize;
        let bit = rel % self.blocks_per_group() as usize;
        let bitmap_block = self.fs.synced.inner().block_group(bg).block_usage_addr;
        let mut bitmap = self.read_block(bitmap_block)?;
        let byte = bit / 8;
        let mask = 1u8 << (bit % 8);
        bitmap[byte] &= !mask;
        self.write_block(bitmap_block, &bitmap)?;
        {
            let mut inner = self.fs.synced.inner();
            inner.block_group_mut(bg).free_blocks_count += 1;
            inner.superblock_mut().free_blocks_count += 1;
        }
        self.write_bg_descriptor(bg)?;
        self.write_superblock_counts()?;
        Ok(())
    }

    fn dirent_rec_len(name_len: usize) -> u16 {
        let mut len = 8 + name_len;
        len = (len + 3) & !3;
        len as u16
    }

    fn inode_full_size(raw: &RawInode) -> usize {
        raw.size_low as usize | ((raw.size_high as usize) << 32)
    }

    fn set_inode_full_size(raw: &mut RawInode, size: usize) {
        raw.size_low = size as u32;
        raw.size_high = (size >> 32) as u32;
        raw.sectors_count = ((size + 511) / 512) as u32;
    }

    fn read_u32_at(block: &[u8], index: usize) -> u32 {
        let off = index * 4;
        u32::from_le_bytes([
            block[off],
            block[off + 1],
            block[off + 2],
            block[off + 3],
        ])
    }

    fn write_u32_at(block: &mut [u8], index: usize, value: u32) {
        let off = index * 4;
        block[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn inode_block_at(&self, raw: &RawInode, index: usize) -> Result<Option<u32>> {
        let bs4 = self.block_size() / 4;
        if index < 12 {
            let b = raw.direct_pointer[index];
            return Ok(if b == 0 { None } else { Some(b) });
        }
        let mut index = index - 12;
        if index < bs4 {
            if raw.indirect_pointer == 0 {
                return Ok(None);
            }
            let indirect = self.read_block(raw.indirect_pointer)?;
            let b = Self::read_u32_at(&indirect, index);
            return Ok(if b == 0 { None } else { Some(b) });
        }
        index -= bs4;
        if index < bs4 * bs4 {
            if raw.doubly_indirect == 0 {
                return Ok(None);
            }
            let lvl1 = index / bs4;
            let lvl2 = index % bs4;
            let dind = self.read_block(raw.doubly_indirect)?;
            let ptr = Self::read_u32_at(&dind, lvl1);
            if ptr == 0 {
                return Ok(None);
            }
            let indirect = self.read_block(ptr)?;
            let b = Self::read_u32_at(&indirect, lvl2);
            return Ok(if b == 0 { None } else { Some(b) });
        }
        index -= bs4 * bs4;
        if index < bs4 * bs4 * bs4 {
            if raw.triply_indirect == 0 {
                return Ok(None);
            }
            let lvl1 = index / (bs4 * bs4);
            let lvl2 = (index / bs4) % bs4;
            let lvl3 = index % bs4;
            let tind = self.read_block(raw.triply_indirect)?;
            let ptr1 = Self::read_u32_at(&tind, lvl1);
            if ptr1 == 0 {
                return Ok(None);
            }
            let dind = self.read_block(ptr1)?;
            let ptr2 = Self::read_u32_at(&dind, lvl2);
            if ptr2 == 0 {
                return Ok(None);
            }
            let indirect = self.read_block(ptr2)?;
            let b = Self::read_u32_at(&indirect, lvl3);
            return Ok(if b == 0 { None } else { Some(b) });
        }
        Ok(None)
    }

    fn inode_data_blocks(&self, raw: &RawInode) -> usize {
        if Self::is_fast_symlink(raw) {
            return 0;
        }
        let bs = self.block_size();
        (Self::inode_full_size(raw) + bs - 1) / bs
    }

    fn dirent_ty(raw: &RawInode) -> u8 {
        if raw.type_perm.contains(TypePerm::DIRECTORY) {
            DIRECTORY
        } else if raw.type_perm.contains(TypePerm::SYMLINK) {
            SYMLINK
        } else {
            FILE
        }
    }

    fn is_fast_symlink(raw: &RawInode) -> bool {
        raw.type_perm.contains(TypePerm::SYMLINK)
            && Self::inode_full_size(raw) <= FAST_SYMLINK_LEN
            && raw.sectors_count == 0
    }

    fn unpack_fast_symlink(raw: &RawInode) -> Vec<u8> {
        let len = Self::inode_full_size(raw);
        let mut buf = [0u8; FAST_SYMLINK_LEN];
        for i in 0..12 {
            buf[i * 4..(i + 1) * 4].copy_from_slice(&raw.direct_pointer[i].to_le_bytes());
        }
        buf[48..52].copy_from_slice(&raw.indirect_pointer.to_le_bytes());
        buf[52..56].copy_from_slice(&raw.doubly_indirect.to_le_bytes());
        buf[56..60].copy_from_slice(&raw.triply_indirect.to_le_bytes());
        buf[..len].to_vec()
    }

    fn pack_fast_symlink(raw: &mut RawInode, target: &[u8]) {
        let mut buf = [0u8; FAST_SYMLINK_LEN];
        buf[..target.len()].copy_from_slice(target);
        for i in 0..12 {
            raw.direct_pointer[i] =
                u32::from_le_bytes([buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]]);
        }
        raw.indirect_pointer = u32::from_le_bytes([buf[48], buf[49], buf[50], buf[51]]);
        raw.doubly_indirect = u32::from_le_bytes([buf[52], buf[53], buf[54], buf[55]]);
        raw.triply_indirect = u32::from_le_bytes([buf[56], buf[57], buf[58], buf[59]]);
        raw.sectors_count = 0;
        Self::set_inode_full_size(raw, target.len());
    }

    /// Read bytes from a regular file or symlink inode using on-disk block maps.
    pub(crate) fn read_file_at(
        &self,
        inode_num: u32,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<usize> {
        let raw = self.read_raw_inode(inode_num)?;
        if raw.type_perm.contains(TypePerm::DIRECTORY) {
            return Err(FsError::IsDir);
        }
        if raw.type_perm.contains(TypePerm::SYMLINK) {
            let target = self.read_symlink(inode_num)?;
            if offset >= target.len() {
                return Ok(0);
            }
            let take = (target.len() - offset).min(buf.len());
            buf[..take].copy_from_slice(&target[offset..offset + take]);
            return Ok(take);
        }
        let total = Self::inode_full_size(&raw);
        if offset >= total {
            return Ok(0);
        }
        let want = (total - offset).min(buf.len());
        let bs = self.block_size();
        let mut done = 0usize;
        let mut pos = offset;
        while done < want {
            let file_block = pos / bs;
            let block_off = pos % bs;
            let disk_block = match self.inode_block_at(&raw, file_block)? {
                Some(b) => b,
                None => break,
            };
            let data = self.read_block(disk_block)?;
            let take = (want - done).min(bs - block_off);
            buf[done..done + take].copy_from_slice(&data[block_off..block_off + take]);
            done += take;
            pos += take;
        }
        Ok(done)
    }

    pub(crate) fn read_symlink(&self, inode_num: u32) -> Result<Vec<u8>> {
        let raw = self.read_raw_inode(inode_num)?;
        if !raw.type_perm.contains(TypePerm::SYMLINK) {
            return Err(FsError::NotSupported);
        }
        if Self::is_fast_symlink(&raw) {
            return Ok(Self::unpack_fast_symlink(&raw));
        }
        let bs = self.block_size();
        let len = Self::inode_full_size(&raw);
        let mut out = vec![0u8; len];
        for block_idx in 0..self.inode_data_blocks(&raw) {
            let disk = self
                .inode_block_at(&raw, block_idx)?
                .ok_or(FsError::DeviceError)?;
            let data = self.read_block(disk)?;
            let off = block_idx * bs;
            let take = (len - off).min(bs);
            out[off..off + take].copy_from_slice(&data[..take]);
        }
        Ok(out)
    }

    pub(crate) fn write_symlink(&self, inode_num: u32, offset: usize, data: &[u8]) -> Result<usize> {
        let mut raw = self.read_raw_inode(inode_num)?;
        if !raw.type_perm.contains(TypePerm::SYMLINK) {
            return Err(FsError::NotSupported);
        }
        let end = offset + data.len();
        let mut target = if Self::is_fast_symlink(&raw) {
            Self::unpack_fast_symlink(&raw)
        } else if Self::inode_full_size(&raw) > 0 {
            self.read_symlink(inode_num)?
        } else {
            Vec::new()
        };
        if end > target.len() {
            target.resize(end, 0);
        }
        target[offset..end].copy_from_slice(data);
        if target.len() <= FAST_SYMLINK_LEN {
            if !Self::is_fast_symlink(&raw) {
                self.free_inode_completely(inode_num)?;
            }
            Self::pack_fast_symlink(&mut raw, &target);
            self.write_raw_inode(inode_num, &raw)?;
        } else {
            if Self::is_fast_symlink(&raw) {
                raw.direct_pointer = [0; 12];
                raw.indirect_pointer = 0;
                raw.doubly_indirect = 0;
                raw.triply_indirect = 0;
                self.write_raw_inode(inode_num, &raw)?;
            }
            self.ensure_file_size(inode_num, target.len())?;
            let bs = self.block_size();
            let mut raw = self.read_raw_inode(inode_num)?;
            for (block_idx, chunk) in target.chunks(bs).enumerate() {
                let disk = self
                    .inode_block_at(&raw, block_idx)?
                    .ok_or(FsError::DeviceError)?;
                self.write_block(disk, chunk)?;
            }
            Self::set_inode_full_size(&mut raw, target.len());
            raw.sectors_count = ((target.len() + 511) / 512) as u32;
            self.write_raw_inode(inode_num, &raw)?;
        }
        Ok(data.len())
    }

    fn parent_inode(&self, dir_inode: u32) -> Result<u32> {
        let inode = self.read_raw_inode(dir_inode)?;
        if !inode.type_perm.contains(TypePerm::DIRECTORY) {
            return Err(FsError::NotDir);
        }
        let blocks = self.inode_data_blocks(&inode).max(1);
        for block_idx in 0..blocks {
            let disk_block = match self.inode_block_at(&inode, block_idx)? {
                Some(b) => b,
                None => continue,
            };
            let data = self.read_block(disk_block)?;
            let mut off = 0usize;
            while off + 8 <= data.len() {
                let rec = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
                if rec < 8 {
                    break;
                }
                let ino =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
                let nlen = data[off + 6] as usize;
                if ino != 0 && nlen == 2 && &data[off + 8..off + 10] == b".." {
                    return Ok(ino);
                }
                off += rec;
            }
        }
        Err(FsError::EntryNotFound)
    }

    fn is_subdir_of(&self, child: u32, ancestor: u32) -> Result<bool> {
        let mut cur = child;
        while cur != 2 {
            if cur == ancestor {
                return Ok(true);
            }
            cur = self.parent_inode(cur)?;
        }
        Ok(false)
    }

    fn adjust_parent_nlink(&self, parent_num: u32, delta: i16) -> Result<()> {
        let mut raw = self.read_raw_inode(parent_num)?;
        if !raw.type_perm.contains(TypePerm::DIRECTORY) {
            return Err(FsError::NotDir);
        }
        if delta < 0 {
            let dec = (-delta) as u16;
            if raw.hard_links <= dec {
                raw.hard_links = 1;
            } else {
                raw.hard_links -= dec;
            }
        } else {
            raw.hard_links = raw.hard_links.saturating_add(delta as u16);
        }
        self.write_raw_inode(parent_num, &raw)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn raw_uid_gid(&self, inode_num: u32) -> Result<(u16, u16)> {
        let raw = self.read_raw_inode(inode_num)?;
        Ok((raw.uid, raw.gid))
    }

    #[allow(dead_code)]
    pub(crate) fn raw_hard_links(&self, inode_num: u32) -> Result<u16> {
        Ok(self.read_raw_inode(inode_num)?.hard_links)
    }

    #[allow(dead_code)]
    pub(crate) fn raw_mode(&self, inode_num: u32) -> Result<u16> {
        Ok(self.read_raw_inode(inode_num)?.type_perm.bits())
    }

    pub(crate) fn update_metadata(
        &self,
        inode_num: u32,
        mode: u32,
        uid: usize,
        gid: usize,
    ) -> Result<()> {
        let mut raw = self.read_raw_inode(inode_num)?;
        let type_mask = TypePerm::FIFO
            | TypePerm::CHAR_DEVICE
            | TypePerm::DIRECTORY
            | TypePerm::BLOCK_DEVICE
            | TypePerm::FILE
            | TypePerm::SYMLINK
            | TypePerm::SOCKET;
        let type_bits = raw.type_perm & type_mask;
        let perm = TypePerm::from_bits_truncate((mode & 0o7777) as u16);
        raw.type_perm = type_bits | perm;
        raw.uid = uid as u16;
        raw.gid = gid as u16;
        self.write_raw_inode(inode_num, &raw)?;
        Ok(())
    }

    fn update_dotdot(&self, dir_inode: u32, new_parent: u32) -> Result<()> {
        let inode = self.read_raw_inode(dir_inode)?;
        let blocks = self.inode_data_blocks(&inode).max(1);
        for block_idx in 0..blocks {
            let disk_block = match self.inode_block_at(&inode, block_idx)? {
                Some(b) => b,
                None => continue,
            };
            let mut data = self.read_block(disk_block)?;
            let mut off = 0usize;
            let mut found = false;
            while off + 8 <= data.len() {
                let rec = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
                if rec < 8 {
                    break;
                }
                let nlen = data[off + 6] as usize;
                if nlen == 2 && &data[off + 8..off + 10] == b".." {
                    data[off..off + 4].copy_from_slice(&new_parent.to_le_bytes());
                    found = true;
                    break;
                }
                off += rec;
            }
            if found {
                self.write_block(disk_block, &data)?;
                return Ok(());
            }
        }
        Err(FsError::EntryNotFound)
    }

    fn pack_dirent(inode: u32, rec_len: u16, name: &[u8], ty: u8) -> Vec<u8> {
        let mut out = vec![0u8; rec_len as usize];
        out[0..4].copy_from_slice(&inode.to_le_bytes());
        out[4..6].copy_from_slice(&rec_len.to_le_bytes());
        out[6] = name.len() as u8;
        out[7] = ty;
        out[8..8 + name.len()].copy_from_slice(name);
        out
    }

    fn add_dir_entry(&self, dir_inode: u32, name: &str, child: u32, ty: u8) -> Result<()> {
        if name == "." || name == ".." {
            return Err(FsError::InvalidParam);
        }
        let name_bytes = name.as_bytes();
        let need = Self::dirent_rec_len(name_bytes.len()) as usize;
        let mut inode = self.read_raw_inode(dir_inode)?;
        let bs = self.block_size();
        let blocks_needed = self.inode_data_blocks(&inode).max(1);
        for block_idx in 0..blocks_needed {
            let disk_block = match self.inode_block_at(&inode, block_idx)? {
                Some(b) => b,
                None => continue,
            };
            let mut data = self.read_block(disk_block)?;
            let mut off = 0usize;
            while off + 8 <= data.len() {
                let rec = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
                if rec < 8 {
                    break;
                }
                let ino =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
                if ino == 0 && rec >= need {
                    let ent = Self::pack_dirent(child, rec as u16, name_bytes, ty);
                    data[off..off + ent.len()].copy_from_slice(&ent);
                    self.write_block(disk_block, &data)?;
                    return Ok(());
                }
                let nlen = data[off + 6] as usize;
                let used = Self::dirent_rec_len(nlen) as usize;
                if rec > used && rec - used >= need {
                    let ent = Self::pack_dirent(child, (rec - used) as u16, name_bytes, ty);
                    data[off + used..off + used + ent.len()].copy_from_slice(&ent);
                    data[off + 4] = used as u8;
                    data[off + 5] = (used >> 8) as u8;
                    self.write_block(disk_block, &data)?;
                    return Ok(());
                }
                off += rec;
            }
            if off + need <= bs {
                let ent = Self::pack_dirent(child, (bs - off) as u16, name_bytes, ty);
                data[off..off + ent.len()].copy_from_slice(&ent);
                self.write_block(disk_block, &data)?;
                Self::set_inode_full_size(&mut inode, bs);
                self.write_raw_inode(dir_inode, &inode)?;
                return Ok(());
            }
        }
        let new_block = self.alloc_block()?;
        let ent = Self::pack_dirent(child, bs as u16, name_bytes, ty);
        let mut data = vec![0u8; bs];
        data[..ent.len()].copy_from_slice(&ent);
        self.write_block(new_block, &data)?;
        let block_idx = blocks_needed;
        let new_size = Self::inode_full_size(&inode).saturating_add(bs);
        self.set_inode_block(&mut inode, block_idx, new_block)?;
        Self::set_inode_full_size(&mut inode, new_size);
        self.write_raw_inode(dir_inode, &inode)?;
        Ok(())
    }

    fn remove_dir_entry(&self, dir_inode: u32, name: &str) -> Result<()> {
        let inode = self.read_raw_inode(dir_inode)?;
        let blocks = self.inode_data_blocks(&inode).max(1);
        for block_idx in 0..blocks {
            let disk_block = match self.inode_block_at(&inode, block_idx)? {
                Some(b) => b,
                None => continue,
            };
            let mut data = self.read_block(disk_block)?;
            let mut off = 0usize;
            let mut prev: Option<usize> = None;
            while off + 8 <= data.len() {
                let rec = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
                if rec < 8 {
                    break;
                }
                let ino =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
                let nlen = data[off + 6] as usize;
                let entry_name = &data[off + 8..off + 8 + nlen];
                if ino != 0 && entry_name == name.as_bytes() {
                    if let Some(p) = prev {
                        let prev_rec =
                            u16::from_le_bytes([data[p + 4], data[p + 5]]) as usize + rec;
                        data[p + 4] = prev_rec as u8;
                        data[p + 5] = (prev_rec >> 8) as u8;
                    } else {
                        data[off] = 0;
                        data[off + 1] = 0;
                        data[off + 2] = 0;
                        data[off + 3] = 0;
                    }
                    self.write_block(disk_block, &data)?;
                    return Ok(());
                }
                prev = Some(off);
                off += rec;
            }
        }
        Err(FsError::EntryNotFound)
    }

    fn zero_block(&self) -> Result<Vec<u8>> {
        Ok(vec![0u8; self.block_size()])
    }

    fn ensure_indirect_child(
        &self,
        parent_block: u32,
        parent_data: &mut [u8],
        index: usize,
    ) -> Result<u32> {
        let mut ptr = Self::read_u32_at(parent_data, index);
        if ptr == 0 {
            ptr = self.alloc_block()?;
            Self::write_u32_at(parent_data, index, ptr);
            self.write_block(parent_block, parent_data)?;
            self.write_block(ptr, &self.zero_block()?)?;
        }
        Ok(ptr)
    }

    fn set_inode_block(&self, inode: &mut RawInode, index: usize, block: u32) -> Result<()> {
        let bs4 = self.block_size() / 4;
        if index < 12 {
            inode.direct_pointer[index] = block;
            return Ok(());
        }
        let mut index = index - 12;
        if index < bs4 {
            if inode.indirect_pointer == 0 && block != 0 {
                inode.indirect_pointer = self.alloc_block()?;
                self.write_block(inode.indirect_pointer, &self.zero_block()?)?;
            }
            if inode.indirect_pointer == 0 {
                return Ok(());
            }
            let mut indirect = self.read_block(inode.indirect_pointer)?;
            Self::write_u32_at(&mut indirect, index, block);
            self.write_block(inode.indirect_pointer, &indirect)?;
            return Ok(());
        }
        index -= bs4;
        if index < bs4 * bs4 {
            if inode.doubly_indirect == 0 && block != 0 {
                inode.doubly_indirect = self.alloc_block()?;
                self.write_block(inode.doubly_indirect, &self.zero_block()?)?;
            }
            if inode.doubly_indirect == 0 {
                return Ok(());
            }
            let lvl1 = index / bs4;
            let lvl2 = index % bs4;
            let mut dind = self.read_block(inode.doubly_indirect)?;
            let ptr = self.ensure_indirect_child(inode.doubly_indirect, &mut dind, lvl1)?;
            let mut indirect = self.read_block(ptr)?;
            Self::write_u32_at(&mut indirect, lvl2, block);
            self.write_block(ptr, &indirect)?;
            return Ok(());
        }
        index -= bs4 * bs4;
        if index < bs4 * bs4 * bs4 {
            if inode.triply_indirect == 0 && block != 0 {
                inode.triply_indirect = self.alloc_block()?;
                self.write_block(inode.triply_indirect, &self.zero_block()?)?;
            }
            if inode.triply_indirect == 0 {
                return Ok(());
            }
            let lvl1 = index / (bs4 * bs4);
            let lvl2 = (index / bs4) % bs4;
            let lvl3 = index % bs4;
            let mut tind = self.read_block(inode.triply_indirect)?;
            let ptr1 = self.ensure_indirect_child(inode.triply_indirect, &mut tind, lvl1)?;
            let mut dind = self.read_block(ptr1)?;
            let ptr2 = self.ensure_indirect_child(ptr1, &mut dind, lvl2)?;
            let mut indirect = self.read_block(ptr2)?;
            Self::write_u32_at(&mut indirect, lvl3, block);
            self.write_block(ptr2, &indirect)?;
            return Ok(());
        }
        Err(FsError::NoDeviceSpace)
    }

    fn block_ptrs_all_zero(block: &[u8]) -> bool {
        block.chunks_exact(4).all(|c| c == [0, 0, 0, 0])
    }

    fn free_metadata_block(&self, block: u32) -> Result<()> {
        if block != 0 {
            let _ = self.free_block(block);
        }
        Ok(())
    }

    fn release_indirect_block(&self, block: u32) -> Result<()> {
        if block == 0 {
            return Ok(());
        }
        let bs4 = self.block_size() / 4;
        let data = self.read_block(block)?;
        for i in 0..bs4 {
            let ptr = Self::read_u32_at(&data, i);
            if ptr != 0 {
                let _ = self.free_block(ptr);
            }
        }
        self.free_metadata_block(block)
    }

    fn release_doubly_indirect_block(&self, block: u32) -> Result<()> {
        if block == 0 {
            return Ok(());
        }
        let bs4 = self.block_size() / 4;
        let data = self.read_block(block)?;
        for i in 0..bs4 {
            let ptr = Self::read_u32_at(&data, i);
            if ptr != 0 {
                self.release_indirect_block(ptr)?;
            }
        }
        self.free_metadata_block(block)
    }

    fn release_triply_indirect_block(&self, block: u32) -> Result<()> {
        if block == 0 {
            return Ok(());
        }
        let bs4 = self.block_size() / 4;
        let data = self.read_block(block)?;
        for i in 0..bs4 {
            let ptr = Self::read_u32_at(&data, i);
            if ptr != 0 {
                self.release_doubly_indirect_block(ptr)?;
            }
        }
        self.free_metadata_block(block)
    }

    fn truncate_inode_blocks(&self, inode_num: u32, new_blocks: usize) -> Result<()> {
        let mut raw = self.read_raw_inode(inode_num)?;
        let old_blocks = self.inode_data_blocks(&raw);
        for idx in new_blocks..old_blocks {
            if let Some(b) = self.inode_block_at(&raw, idx)? {
                let _ = self.free_block(b);
            }
            self.set_inode_block(&mut raw, idx, 0)?;
        }
        self.write_raw_inode(inode_num, &raw)?;

        let mut raw = self.read_raw_inode(inode_num)?;
        if raw.indirect_pointer != 0 {
            let data = self.read_block(raw.indirect_pointer)?;
            if Self::block_ptrs_all_zero(&data) {
                self.free_metadata_block(raw.indirect_pointer)?;
                raw.indirect_pointer = 0;
            }
        }
        if raw.doubly_indirect != 0 {
            let data = self.read_block(raw.doubly_indirect)?;
            if Self::block_ptrs_all_zero(&data) {
                self.release_doubly_indirect_block(raw.doubly_indirect)?;
                raw.doubly_indirect = 0;
            }
        }
        if raw.triply_indirect != 0 {
            let data = self.read_block(raw.triply_indirect)?;
            if Self::block_ptrs_all_zero(&data) {
                self.release_triply_indirect_block(raw.triply_indirect)?;
                raw.triply_indirect = 0;
            }
        }
        self.write_raw_inode(inode_num, &raw)?;
        Ok(())
    }

    pub(crate) fn ensure_file_size(&self, inode_num: u32, size: usize) -> Result<()> {
        let bs = self.block_size();
        let blocks_needed = (size + bs - 1) / bs;
        let mut raw = self.read_raw_inode(inode_num)?;
        if raw.type_perm.contains(TypePerm::DIRECTORY) {
            return Err(FsError::IsDir);
        }
        let current_blocks = self.inode_data_blocks(&raw);
        for idx in current_blocks..blocks_needed {
            let block = self.alloc_block()?;
            self.set_inode_block(&mut raw, idx, block)?;
        }
        Self::set_inode_full_size(&mut raw, size);
        self.write_raw_inode(inode_num, &raw)?;
        Ok(())
    }

    fn free_inode_completely(&self, inode_num: u32) -> Result<()> {
        let raw = self.read_raw_inode(inode_num)?;
        if Self::is_fast_symlink(&raw) {
            return Ok(());
        }
        let blocks = self.inode_data_blocks(&raw);
        for idx in 0..blocks {
            if let Some(b) = self.inode_block_at(&raw, idx)? {
                let _ = self.free_block(b);
            }
        }
        self.release_indirect_block(raw.indirect_pointer)?;
        self.release_doubly_indirect_block(raw.doubly_indirect)?;
        self.release_triply_indirect_block(raw.triply_indirect)?;
        Ok(())
    }

    fn dir_is_empty(&self, inode_num: u32) -> Result<bool> {
        let inode = self.read_raw_inode(inode_num)?;
        if !inode.type_perm.contains(TypePerm::DIRECTORY) {
            return Ok(true);
        }
        let blocks = self.inode_data_blocks(&inode).max(1);
        let mut count = 0usize;
        for block_idx in 0..blocks {
            let disk_block = match self.inode_block_at(&inode, block_idx)? {
                Some(b) => b,
                None => continue,
            };
            let data = self.read_block(disk_block)?;
            let mut off = 0usize;
            while off + 8 <= data.len() {
                let rec = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
                if rec < 8 {
                    break;
                }
                let ino =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
                if ino != 0 {
                    count += 1;
                }
                off += rec;
            }
        }
        Ok(count <= 2)
    }

    pub fn create(
        &self,
        parent_num: u32,
        name: &str,
        type_: FileType,
        mode: u32,
    ) -> Result<u32> {
        if !self
            .fs
            .inode_from_num(parent_num as usize)?
            .is_dir()
        {
            return Err(FsError::NotDir);
        }
        if self.lookup_in_dir(parent_num, name).is_ok() {
            return Err(FsError::EntryExist);
        }
        let child = self.alloc_inode()?;
        let mut raw = RawInode {
            type_perm: TypePerm::empty(),
            uid: 0,
            size_low: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            hard_links: 1,
            sectors_count: 0,
            flags: ext2::sys::inode::Flags::empty(),
            _os_specific_1: [0; 4],
            direct_pointer: [0; 12],
            indirect_pointer: 0,
            doubly_indirect: 0,
            triply_indirect: 0,
            gen_number: 0,
            ext_attribute_block: 0,
            size_high: 0,
            frag_block_addr: 0,
            _os_specific_2: [0; 12],
        };
        let perm = TypePerm::from_bits_truncate((mode & 0o7777) as u16);
        match type_ {
            FileType::Dir => {
                raw.type_perm = TypePerm::DIRECTORY | perm;
                raw.hard_links = 2;
                let block = self.alloc_block()?;
                raw.direct_pointer[0] = block;
                raw.size_low = self.block_size() as u32;
                self.write_raw_inode(child, &raw)?;
                let bs = self.block_size();
                let mut data = vec![0u8; bs];
                let dot = Self::pack_dirent(child, 12, b".", DIRECTORY);
                let dotdot = Self::pack_dirent(parent_num, 12, b"..", DIRECTORY);
                data[..dot.len()].copy_from_slice(&dot);
                data[12..12 + dotdot.len()].copy_from_slice(&dotdot);
                self.write_block(block, &data)?;
                self.add_dir_entry(parent_num, name, child, DIRECTORY)?;
                self.adjust_parent_nlink(parent_num, 1)?;
                let bg = ((child - 1) as usize) / self.inodes_per_group();
                {
                    let mut inner = self.fs.synced.inner();
                    inner.block_group_mut(bg).dirs_count += 1;
                }
                self.write_bg_descriptor(bg)?;
            }
            FileType::File => {
                raw.type_perm = TypePerm::FILE | perm;
                self.write_raw_inode(child, &raw)?;
                self.add_dir_entry(parent_num, name, child, FILE)?;
            }
            FileType::SymLink => {
                raw.type_perm = TypePerm::SYMLINK | perm;
                self.write_raw_inode(child, &raw)?;
                self.add_dir_entry(parent_num, name, child, SYMLINK)?;
            }
            _ => return Err(FsError::NotSupported),
        }
        Ok(child)
    }

    fn lookup_in_dir(&self, dir_inode: u32, name: &str) -> Result<u32> {
        let inode = self.read_raw_inode(dir_inode)?;
        let blocks = self.inode_data_blocks(&inode).max(1);
        for block_idx in 0..blocks {
            let disk_block = match self.inode_block_at(&inode, block_idx)? {
                Some(b) => b,
                None => continue,
            };
            let data = self.read_block(disk_block)?;
            let mut off = 0usize;
            while off + 8 <= data.len() {
                let rec = u16::from_le_bytes([data[off + 4], data[off + 5]]) as usize;
                if rec < 8 {
                    break;
                }
                let ino =
                    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
                let nlen = data[off + 6] as usize;
                if ino != 0 && &data[off + 8..off + 8 + nlen] == name.as_bytes() {
                    return Ok(ino);
                }
                off += rec;
            }
        }
        Err(FsError::EntryNotFound)
    }

    pub fn unlink(&self, parent_num: u32, name: &str) -> Result<()> {
        let child_inode = self.lookup_in_dir(parent_num, name)?;
        let child_raw = self.read_raw_inode(child_inode)?;
        if child_raw.type_perm.contains(TypePerm::DIRECTORY) && !self.dir_is_empty(child_inode)? {
            return Err(FsError::DirNotEmpty);
        }
        let is_dir = child_raw.type_perm.contains(TypePerm::DIRECTORY);
        self.remove_dir_entry(parent_num, name)?;
        let mut raw = self.read_raw_inode(child_inode)?;
        if raw.hard_links <= 1 {
            self.free_inode_completely(child_inode)?;
            if is_dir {
                self.adjust_parent_nlink(parent_num, -1)?;
                let bg = ((child_inode - 1) as usize) / self.inodes_per_group();
                {
                    let mut inner = self.fs.synced.inner();
                    if inner.block_group(bg).dirs_count > 0 {
                        inner.block_group_mut(bg).dirs_count -= 1;
                    }
                }
                self.write_bg_descriptor(bg)?;
            }
            self.free_inode(child_inode)?;
        } else {
            raw.hard_links -= 1;
            self.write_raw_inode(child_inode, &raw)?;
        }
        Ok(())
    }

    pub fn link(&self, parent_num: u32, name: &str, target_inode: u32) -> Result<()> {
        if !self
            .fs
            .inode_from_num(parent_num as usize)?
            .is_dir()
        {
            return Err(FsError::NotDir);
        }
        if self.lookup_in_dir(parent_num, name).is_ok() {
            return Err(FsError::EntryExist);
        }
        let mut raw = self.read_raw_inode(target_inode)?;
        if raw.type_perm.contains(TypePerm::DIRECTORY) {
            return Err(FsError::IsDir);
        }
        if raw.type_perm.contains(TypePerm::SYMLINK) {
            return Err(FsError::NotSupported);
        }
        self.add_dir_entry(parent_num, name, target_inode, FILE)?;
        raw.hard_links += 1;
        self.write_raw_inode(target_inode, &raw)?;
        Ok(())
    }

    pub fn rename_across(
        &self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<()> {
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }
        let inode = self.lookup_in_dir(old_parent, old_name)?;
        let raw = self.read_raw_inode(inode)?;
        let src_is_dir = raw.type_perm.contains(TypePerm::DIRECTORY);

        if src_is_dir && self.is_subdir_of(new_parent, inode)? {
            return Err(FsError::InvalidParam);
        }

        if let Ok(existing) = self.lookup_in_dir(new_parent, new_name) {
            if existing == inode {
                return Ok(());
            }
            let existing_raw = self.read_raw_inode(existing)?;
            let dst_is_dir = existing_raw.type_perm.contains(TypePerm::DIRECTORY);
            if src_is_dir != dst_is_dir {
                return if dst_is_dir {
                    Err(FsError::IsDir)
                } else {
                    Err(FsError::NotDir)
                };
            }
            if dst_is_dir && !self.dir_is_empty(existing)? {
                return Err(FsError::DirNotEmpty);
            }
            self.unlink(new_parent, new_name)?;
        }

        if src_is_dir {
            if old_parent != new_parent {
                self.update_dotdot(inode, new_parent)?;
                self.adjust_parent_nlink(old_parent, -1)?;
                self.adjust_parent_nlink(new_parent, 1)?;
            }
        }
        let ty = Self::dirent_ty(&raw);
        self.remove_dir_entry(old_parent, old_name)?;
        self.add_dir_entry(new_parent, new_name, inode, ty)?;
        Ok(())
    }

    pub fn resize(&self, inode_num: u32, len: usize) -> Result<()> {
        let raw = self.read_raw_inode(inode_num)?;
        if raw.type_perm.contains(TypePerm::DIRECTORY) {
            return Err(FsError::IsDir);
        }
        if raw.type_perm.contains(TypePerm::SYMLINK) {
            let mut target = self.read_symlink(inode_num)?;
            if len < target.len() {
                target.truncate(len);
            } else if len > target.len() {
                target.resize(len, 0);
            }
            self.write_symlink(inode_num, 0, &target)?;
            return Ok(());
        }
        let old = Self::inode_full_size(&raw);
        if len > old {
            self.ensure_file_size(inode_num, len)?;
        } else if len < old {
            let bs = self.block_size();
            let new_blocks = (len + bs - 1) / bs;
            self.truncate_inode_blocks(inode_num, new_blocks)?;
            let mut raw = self.read_raw_inode(inode_num)?;
            Self::set_inode_full_size(&mut raw, len);
            self.write_raw_inode(inode_num, &raw)?;
        }
        Ok(())
    }
}

impl Ext2MountFs {
    pub(crate) fn editor(&self) -> Ext2Editor<'_> {
        Ext2Editor::new(self)
    }
}
