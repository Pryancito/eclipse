//! Block-backed and file-backed devices for mountable filesystems.

use alloc::sync::Arc;
use core::cmp::min;

use rcore_fs::dev::{DevError, Device, Result as DevResult};
use rcore_fs::vfs::{FsError, INode, Result as VfsResult};
use zcore_drivers::scheme::BlockScheme;

use super::devfs::BlockDev;

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
            let mut temp = [0u8; 512];
            self.block
                .read_block(block_id, &mut temp)
                .map_err(|_| DevError)?;
            buf[done..done + take].copy_from_slice(&temp[block_off..block_off + take]);
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
            let mut temp = [0u8; 512];
            if block_off != 0 || take != block_size {
                self.block
                    .read_block(block_id, &mut temp)
                    .map_err(|_| DevError)?;
            }
            temp[block_off..block_off + take].copy_from_slice(&buf[done..done + take]);
            self.block
                .write_block(block_id, &temp)
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
