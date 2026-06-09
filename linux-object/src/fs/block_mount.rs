//! Block-backed and file-backed devices for mountable filesystems.

use alloc::sync::Arc;
use core::cmp::min;

use rcore_fs::dev::{DevError, Device, Result as DevResult};
use rcore_fs::vfs::{FsError, INode, Result as VfsResult};
use zcore_drivers::scheme::BlockScheme;

use super::devfs::BlockDev;

/// 512-byte sector buffer aligned for AHCI DMA.
#[repr(align(4096))]
struct SectorBuf([u8; 512]);

/// Backing store for a mount operation.
pub enum MountBackend {
    Block(Arc<dyn BlockScheme>),
    File(Arc<dyn INode>),
}

impl MountBackend {
    pub fn from_inode(inode: Arc<dyn INode>) -> VfsResult<Self> {
        use rcore_fs::vfs::FileType;
        let ty = inode.metadata()?.type_;
        match ty {
            FileType::BlockDevice => {
                let dev = inode
                    .as_any_ref()
                    .downcast_ref::<BlockDev>()
                    .ok_or(FsError::NotSupported)?;
                Ok(Self::Block(dev.block_scheme()))
            }
            FileType::File => Ok(Self::File(inode)),
            _ => Err(FsError::NotSupported),
        }
    }

}

/// Byte-oriented device over a block driver.
pub struct BlockByteDevice {
    block: Arc<dyn BlockScheme>,
}

impl BlockByteDevice {
    pub fn new(block: Arc<dyn BlockScheme>) -> Self {
        Self { block }
    }
}

impl Device for BlockByteDevice {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> DevResult<usize> {
        let block_size = 512;
        let mut done = 0;
        while done < buf.len() {
            let abs = offset + done;
            let block_id = abs / block_size;
            let block_off = abs % block_size;
            let take = min(buf.len() - done, block_size - block_off);
            let mut temp = SectorBuf([0u8; 512]);
            self.block
                .read_block(block_id, &mut temp.0)
                .map_err(|_| DevError)?;
            buf[done..done + take].copy_from_slice(&temp.0[block_off..block_off + take]);
            done += take;
        }
        Ok(done)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> DevResult<usize> {
        let block_size = 512;
        let mut done = 0;
        while done < buf.len() {
            let abs = offset + done;
            let block_id = abs / block_size;
            let block_off = abs % block_size;
            let take = min(buf.len() - done, block_size - block_off);
            let mut temp = SectorBuf([0u8; 512]);
            if block_off != 0 || take != block_size {
                self.block
                    .read_block(block_id, &mut temp.0)
                    .map_err(|_| DevError)?;
            }
            temp.0[block_off..block_off + take].copy_from_slice(&buf[done..done + take]);
            self.block
                .write_block(block_id, &temp.0)
                .map_err(|_| DevError)?;
            done += take;
        }
        Ok(done)
    }

    fn sync(&self) -> DevResult<()> {
        self.block.flush().map_err(|_| DevError)
    }
}

/// Byte-oriented device over a regular file inode (loop-less loop mounts).
pub struct FileByteDevice {
    inode: Arc<dyn INode>,
    len: usize,
}

impl FileByteDevice {
    pub fn new(inode: Arc<dyn INode>) -> VfsResult<Self> {
        Ok(Self {
            len: inode.metadata()?.size,
            inode,
        })
    }
}

impl Device for FileByteDevice {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> DevResult<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let take = min(buf.len(), self.len - offset);
        self.inode
            .read_at(offset, &mut buf[..take])
            .map_err(|_| DevError)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> DevResult<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let take = min(buf.len(), self.len - offset);
        self.inode
            .write_at(offset, &buf[..take])
            .map_err(|_| DevError)
    }

    fn sync(&self) -> DevResult<()> {
        self.inode.sync_all().map_err(|_| DevError)
    }
}

pub fn device_from_backend(backend: &MountBackend) -> VfsResult<Arc<dyn Device>> {
    match backend {
        MountBackend::Block(block) => Ok(Arc::new(BlockByteDevice::new(block.clone()))),
        MountBackend::File(file) => Ok(Arc::new(FileByteDevice::new(file.clone())?)),
    }
}

/// ext2 superblock magic (`0xEF53`) at byte offset 56 within the superblock.
const EXT2_MAGIC: u16 = 0xEF53;
/// Superblock starts at byte 1024 → sector index 2 on a 512-byte device.
const EXT2_SUPERBLOCK_SECTOR: usize = 2;

/// Cheap pre-check before mounting: reject devices whose sector 2 does not look
/// like a plausible ext2 superblock for this partition size.  Without this,
/// a false-positive magic on vfat/gpt data can make ext2-rs read a huge bogus
/// block-group table and stall boot for a long time.
pub(crate) fn probe_ext2_superblock(block: &Arc<dyn BlockScheme>) -> bool {
    let mut sb = SectorBuf([0u8; 512]);
    if block
        .read_block(EXT2_SUPERBLOCK_SECTOR, &mut sb.0)
        .is_err()
    {
        return false;
    }
    let magic = u16::from_le_bytes([sb.0[56], sb.0[57]]);
    if magic != EXT2_MAGIC {
        return false;
    }
    let blocks_count = u32::from_le_bytes([sb.0[4], sb.0[5], sb.0[6], sb.0[7]]) as usize;
    let inodes_count = u32::from_le_bytes([sb.0[0], sb.0[1], sb.0[2], sb.0[3]]) as usize;
    if blocks_count < 11 || inodes_count < 11 {
        return false;
    }
    let log_block_size = i32::from_le_bytes([sb.0[24], sb.0[25], sb.0[26], sb.0[27]]);
    if log_block_size < 0 || log_block_size > 6 {
        return false;
    }
    let block_size = 1024usize << log_block_size;
    let device_sectors = block.block_count();
    let fs_sectors = blocks_count.saturating_mul(block_size / 512);
    if fs_sectors > device_sectors.saturating_mul(2) {
        return false;
    }
    let blocks_per_group =
        u32::from_le_bytes([sb.0[32], sb.0[33], sb.0[34], sb.0[35]]).max(1) as usize;
    let bg_by_blocks = blocks_count / blocks_per_group + 1;
    if bg_by_blocks > 8192 {
        return false;
    }
    true
}
