//! Device wrappers that implement `rcore_fs::dev::Device`, which can loaded
//! file systems on (e.g. `rcore_fs_sfs::SimpleFileSystem::open()`).

use alloc::sync::Arc;

extern crate rcore_fs;

use kernel_hal::drivers::scheme::BlockScheme;
use kernel_hal::sync::RwLock;
use rcore_fs::dev::{BlockDevice, DevError, Device, Result};

/// A naive LRU cache layer for `BlockDevice`, re-exported from `rcore-fs`.
pub use rcore_fs::dev::block_cache::BlockCache;

/// Memory buffer for device.
pub struct MemBuf(RwLock<&'static mut [u8]>);

impl MemBuf {
    /// create a [`MemBuf`] struct.
    pub fn new(buf: &'static mut [u8]) -> Self {
        MemBuf(RwLock::new(buf))
    }
}

impl Device for MemBuf {
    // Past the end is a short read of zero bytes, not an error and not a
    // panic: `slice.len() - offset` used to underflow, which takes the kernel
    // down with an overflow panic in a debug build and, in a release one, with
    // an out-of-range slice index one line later. A file system whose
    // superblock claims more blocks than the image holds is enough to get
    // there, and a truncated initramfs is exactly that.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let slice = self.0.read();
        let begin = offset.min(slice.len());
        let len = buf.len().min(slice.len() - begin);
        buf[..len].copy_from_slice(&slice[begin..begin + len]);
        Ok(len)
    }
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let mut slice = self.0.write();
        let begin = offset.min(slice.len());
        let len = buf.len().min(slice.len() - begin);
        slice[begin..begin + len].copy_from_slice(&buf[..len]);
        Ok(len)
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
}

/// Block device implements [`BlockScheme`].
pub struct Block(Arc<dyn BlockScheme>);

impl Block {
    /// create a [`Block`] struct.
    pub fn new(block: Arc<dyn BlockScheme>) -> Self {
        Self(block)
    }
}

impl BlockDevice for Block {
    const BLOCK_SIZE_LOG2: u8 = 9; // 512

    fn read_at(&self, block_id: usize, buf: &mut [u8]) -> Result<()> {
        self.0.read_block(block_id, buf).map_err(|_| DevError)
    }

    fn write_at(&self, block_id: usize, buf: &[u8]) -> Result<()> {
        self.0.write_block(block_id, buf).map_err(|_| DevError)
    }

    fn sync(&self) -> Result<()> {
        self.0.flush().map_err(|_| DevError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use zcore_drivers::scheme::Scheme;
    use zcore_drivers::{DeviceError, DeviceResult};

    /// A [`MemBuf`] over `len` bytes, each byte its own index. The buffer has
    /// to outlive the test, which is what `MemBuf` asks for.
    fn membuf(len: usize) -> MemBuf {
        let mut v = vec![0u8; len];
        for (i, b) in v.iter_mut().enumerate() {
            *b = i as u8;
        }
        MemBuf::new(Box::leak(v.into_boxed_slice()))
    }

    #[test]
    fn a_read_inside_the_buffer_gives_the_bytes_asked_for() {
        let dev = membuf(64);
        let mut out = [0u8; 4];
        assert_eq!(Device::read_at(&dev, 8, &mut out), Ok(4));
        assert_eq!(out, [8, 9, 10, 11]);
    }

    #[test]
    fn a_read_that_runs_off_the_end_is_short_and_not_a_panic() {
        let dev = membuf(16);
        let mut out = [0xffu8; 8];
        assert_eq!(Device::read_at(&dev, 12, &mut out), Ok(4));
        assert_eq!(&out[..4], &[12, 13, 14, 15]);
        assert_eq!(&out[4..], &[0xff; 4], "bytes past the end were touched");
    }

    #[test]
    fn a_read_starting_at_the_end_reads_nothing() {
        let dev = membuf(16);
        let mut out = [0xffu8; 4];
        assert_eq!(Device::read_at(&dev, 16, &mut out), Ok(0));
        assert_eq!(out, [0xff; 4]);
    }

    #[test]
    fn a_read_starting_past_the_end_reads_nothing_rather_than_taking_the_kernel_down() {
        // A file system whose superblock claims more blocks than the image has
        // asks for exactly this, and a truncated initramfs is that image. It
        // used to be `slice.len() - offset`: an overflow panic in a debug
        // build, an out-of-range slice index in a release one.
        let dev = membuf(16);
        let mut out = [0xffu8; 4];
        assert_eq!(Device::read_at(&dev, 17, &mut out), Ok(0));
        assert_eq!(Device::read_at(&dev, 4096, &mut out), Ok(0));
        assert_eq!(Device::read_at(&dev, usize::MAX, &mut out), Ok(0));
        assert_eq!(out, [0xff; 4], "the buffer was written to");
    }

    #[test]
    fn a_write_past_the_end_writes_nothing_rather_than_taking_the_kernel_down() {
        let dev = membuf(16);
        assert_eq!(Device::write_at(&dev, 17, &[1, 2, 3, 4]), Ok(0));
        assert_eq!(Device::write_at(&dev, usize::MAX, &[1, 2, 3, 4]), Ok(0));
        // And the buffer is untouched.
        let mut out = [0u8; 16];
        assert_eq!(Device::read_at(&dev, 0, &mut out), Ok(16));
        assert_eq!(out[15], 15);
    }

    #[test]
    fn a_write_that_runs_off_the_end_writes_what_fits() {
        let dev = membuf(16);
        assert_eq!(Device::write_at(&dev, 14, &[0xaa, 0xbb, 0xcc, 0xdd]), Ok(2));
        let mut out = [0u8; 16];
        assert_eq!(Device::read_at(&dev, 0, &mut out), Ok(16));
        assert_eq!(&out[13..], &[13, 0xaa, 0xbb]);
    }

    #[test]
    fn what_was_written_reads_back() {
        let dev = membuf(32);
        assert_eq!(Device::write_at(&dev, 4, &[9; 8]), Ok(8));
        let mut out = [0u8; 8];
        assert_eq!(Device::read_at(&dev, 4, &mut out), Ok(8));
        assert_eq!(out, [9; 8]);
        assert!(Device::sync(&dev).is_ok());
    }

    #[test]
    fn an_empty_request_moves_nothing() {
        let dev = membuf(16);
        assert_eq!(Device::read_at(&dev, 8, &mut []), Ok(0));
        assert_eq!(Device::write_at(&dev, 8, &[]), Ok(0));
    }

    /// A block device that records what it was asked for, so a test can say
    /// whether the wrapper passed the request through unchanged.
    struct Recording {
        data: kernel_hal::sync::Mutex<alloc::vec::Vec<u8>>,
        flushes: AtomicUsize,
        fail: AtomicUsize,
    }

    impl Scheme for Recording {
        fn name(&self) -> &str {
            "recording"
        }
    }

    impl BlockScheme for Recording {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
            if self.fail.load(Ordering::SeqCst) != 0 {
                return Err(DeviceError::InvalidParam);
            }
            let data = self.data.lock();
            let begin = block_id * 512;
            if begin + buf.len() > data.len() {
                return Err(DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&data[begin..begin + buf.len()]);
            Ok(())
        }
        fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
            if self.fail.load(Ordering::SeqCst) != 0 {
                return Err(DeviceError::InvalidParam);
            }
            let mut data = self.data.lock();
            let begin = block_id * 512;
            if begin + buf.len() > data.len() {
                return Err(DeviceError::InvalidParam);
            }
            data[begin..begin + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.data.lock().len() / 512
        }
    }

    fn recording(blocks: usize) -> Arc<Recording> {
        Arc::new(Recording {
            data: kernel_hal::sync::Mutex::new(vec![0u8; blocks * 512]),
            flushes: AtomicUsize::new(0),
            fail: AtomicUsize::new(0),
        })
    }

    #[test]
    fn the_block_wrapper_speaks_512_byte_blocks() {
        assert_eq!(1usize << <Block as BlockDevice>::BLOCK_SIZE_LOG2, 512);
    }

    #[test]
    fn a_block_written_through_the_wrapper_reads_back() {
        let inner = recording(4);
        let dev = Block::new(inner.clone());
        BlockDevice::write_at(&dev, 2, &[0x5a; 512]).unwrap();
        let mut out = [0u8; 512];
        BlockDevice::read_at(&dev, 2, &mut out).unwrap();
        assert_eq!(out, [0x5a; 512]);
        // And only that block moved.
        let mut other = [0xffu8; 512];
        BlockDevice::read_at(&dev, 1, &mut other).unwrap();
        assert_eq!(other, [0u8; 512]);
    }

    #[test]
    fn a_flush_reaches_the_device() {
        let inner = recording(2);
        let dev = Block::new(inner.clone());
        BlockDevice::sync(&dev).unwrap();
        assert_eq!(inner.flushes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_device_error_comes_back_as_a_device_error() {
        let inner = recording(2);
        inner.fail.store(1, Ordering::SeqCst);
        let dev = Block::new(inner);
        let mut out = [0u8; 512];
        assert_eq!(BlockDevice::read_at(&dev, 0, &mut out), Err(DevError));
        assert_eq!(BlockDevice::write_at(&dev, 0, &[0; 512]), Err(DevError));
    }

    #[test]
    fn a_byte_range_through_the_wrapper_asks_for_one_block_at_a_time() {
        // The blanket `Device` impl above turns byte offsets into block
        // requests. A request for more than one block at a time would be
        // refused at the end of the device -- and this device, like the disk
        // drivers, moves exactly as many bytes as it is handed.
        let inner = recording(2);
        let dev = Block::new(inner);
        // The last two bytes of the last block: nothing beyond it exists.
        assert_eq!(Device::write_at(&dev, 1022, &[7, 7]), Ok(2));
        let mut out = [0u8; 2];
        assert_eq!(Device::read_at(&dev, 1022, &mut out), Ok(2));
        assert_eq!(out, [7, 7]);
    }

    #[test]
    fn a_block_read_into_a_longer_buffer_is_refused_by_the_device() {
        // `BlockScheme` moves what it is handed, so a caller must hand it one
        // block. Asking for two at the end of the device is a refusal, which is
        // why the layer above sizes its buffer to the block.
        let inner = recording(2);
        let dev = Block::new(inner);
        let mut two = [0u8; 1024];
        assert_eq!(BlockDevice::read_at(&dev, 1, &mut two), Err(DevError));
    }
}
