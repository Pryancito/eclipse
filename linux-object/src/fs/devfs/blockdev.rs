use alloc::sync::Arc;
use core::any::Any;
use rcore_fs::vfs::{make_rdev, FileType, FsError, INode, Metadata, PollStatus, Result, Timespec};
use rcore_fs_devfs::DevFS;
use zcore_drivers::{scheme::BlockScheme, DeviceError};

/// Linux `BLKGETSIZE64` — total size in bytes (`linux/fs.h`).
const BLKGETSIZE64: u32 = 0x8008_1272;
/// Linux `BLKGETSIZE` — size in 512-byte sectors (`linux/fs.h`).
const BLKGETSIZE: u32 = 0x0000_1260;

/// Block device INode.
pub struct BlockDev {
    index: usize,
    block: Arc<dyn BlockScheme>,
    inode_id: usize,
}

impl BlockDev {
    pub fn new(index: usize, block: Arc<dyn BlockScheme>) -> Self {
        Self {
            index,
            block,
            inode_id: DevFS::new_inode_id(),
        }
    }
}

impl INode for BlockDev {
    fn read_at(&self, mut offset: usize, buf: &mut [u8]) -> Result<usize> {
        let block_size = 512;
        let mut read_len = 0;
        #[repr(align(4096))]
        struct AlignedBuf([u8; 512]);
        let mut temp_buf = AlignedBuf([0u8; 512]);

        while read_len < buf.len() {
            let block_id = offset / block_size;
            let block_offset = offset % block_size;
            let current_len = (buf.len() - read_len).min(block_size - block_offset);

            if block_offset == 0 && current_len == block_size {
                self.block
                    .read_block(block_id, &mut temp_buf.0)
                    .map_err(convert_error)?;
                buf[read_len..read_len + block_size].copy_from_slice(&temp_buf.0);
            } else {
                self.block
                    .read_block(block_id, &mut temp_buf.0)
                    .map_err(convert_error)?;
                buf[read_len..read_len + current_len].copy_from_slice(&temp_buf.0[block_offset..block_offset + current_len]);
            }

            read_len += current_len;
            offset += current_len;
        }

        Ok(read_len)
    }

    fn write_at(&self, mut offset: usize, buf: &[u8]) -> Result<usize> {
        let block_size = 512;
        let mut write_len = 0;
        #[repr(align(4096))]
        struct AlignedBuf([u8; 512]);
        let mut temp_buf = AlignedBuf([0u8; 512]);

        while write_len < buf.len() {
            let block_id = offset / block_size;
            let block_offset = offset % block_size;
            let current_len = (buf.len() - write_len).min(block_size - block_offset);

            if block_offset == 0 && current_len == block_size {
                temp_buf.0.copy_from_slice(&buf[write_len..write_len + block_size]);
                self.block
                    .write_block(block_id, &temp_buf.0)
                    .map_err(convert_error)?;
            } else {
                self.block
                    .read_block(block_id, &mut temp_buf.0)
                    .map_err(convert_error)?;
                temp_buf.0[block_offset..block_offset + current_len].copy_from_slice(&buf[write_len..write_len + current_len]);
                self.block
                    .write_block(block_id, &temp_buf.0)
                    .map_err(convert_error)?;
            }

            write_len += current_len;
            offset += current_len;
        }

        Ok(write_len)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: true,
            write: true,
            error: false,
        })
    }

    fn metadata(&self) -> Result<Metadata> {
        let blocks = self.block.block_count();
        let size = blocks.saturating_mul(512);
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size,
            blk_size: 512,
            blocks,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::BlockDevice,
            mode: 0o660, // owner & group read/write
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(3, self.index),
        })
    }

    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        if data == 0 {
            return Err(FsError::InvalidParam);
        }
        let sectors = self.block.block_count() as u64;
        match cmd {
            BLKGETSIZE64 => {
                let size = sectors.saturating_mul(512);
                let mut out_ptr = kernel_hal::user::UserOutPtr::<u64>::from(data);
                out_ptr.write(size).map_err(|_| FsError::InvalidParam)?;
                Ok(0)
            }
            BLKGETSIZE => {
                let legacy = sectors as usize;
                let mut out_ptr = kernel_hal::user::UserOutPtr::<usize>::from(data);
                out_ptr.write(legacy).map_err(|_| FsError::InvalidParam)?;
                Ok(0)
            }
            _ => Err(FsError::NotSupported),
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn sync_all(&self) -> Result<()> {
        Ok(())
    }

    fn sync_data(&self) -> Result<()> {
        Ok(())
    }
}

fn convert_error(e: DeviceError) -> FsError {
    match e {
        DeviceError::NotSupported => FsError::NotSupported,
        DeviceError::NotReady => FsError::Busy,
        DeviceError::InvalidParam => FsError::InvalidParam,
        DeviceError::BufferTooSmall
        | DeviceError::DmaError
        | DeviceError::IoError
        | DeviceError::AlreadyExists
        | DeviceError::NoResources => FsError::DeviceError,
    }
}
