use crate::{util::*, vfs::Timespec};
use alloc::vec;

pub mod block_cache;
pub mod std_impl;

/// A current time provider
pub trait TimeProvider: Send + Sync {
    fn current_time(&self) -> Timespec;
}

/// Interface for FS to read & write
pub trait Device: Send + Sync {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize>;
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize>;
    fn sync(&self) -> Result<()>;
}

/// Device which can only R/W in blocks
pub trait BlockDevice: Send + Sync {
    const BLOCK_SIZE_LOG2: u8;
    fn read_at(&self, block_id: BlockId, buf: &mut [u8]) -> Result<()>;
    fn write_at(&self, block_id: BlockId, buf: &[u8]) -> Result<()>;
    fn sync(&self) -> Result<()>;
}

/// The error type for device.
#[derive(Debug, PartialEq, Eq)]
pub struct DevError;

/// A specialized `Result` type for device.
pub type Result<T> = core::result::Result<T, DevError>;

pub type BlockId = usize;

/// Stop at the first block the device refuses and report how many bytes were
/// moved before it, the way `read(2)` reports a short read. That is what makes
/// a read past the end of the device answer `Ok(0)` instead of an error, which
/// the callers above rely on.
macro_rules! try0 {
    ($len:expr, $res:expr) => {
        if $res.is_err() {
            return Ok($len);
        }
    };
}

/// Helper functions to R/W BlockDevice in bytes
impl<T: BlockDevice> Device for T {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize> {
        let iter = BlockIter {
            begin: offset,
            end: offset.checked_add(buf.len()).ok_or(DevError)?,
            block_size_log2: Self::BLOCK_SIZE_LOG2,
        };

        // For each block
        for range in iter {
            let len = range.origin_begin() - offset;
            let buf = &mut buf[range.origin_begin() - offset..range.origin_end() - offset];
            if range.is_full() {
                // Read to target buf directly. On the read side this branch
                // saves a copy and nothing else: the path below asks the device
                // for the same one block and hands back the same bytes, so the
                // two are the same program. On the write side it is not -- see
                // there.
                try0!(len, BlockDevice::read_at(self, range.block, buf));
            } else {
                // Exactly one block, and on the heap: a fixed stack array
                // would both cap the block size and, being longer than one
                // block, ask the device for the block after this one as well.
                let mut block_buf = vec![0u8; 1usize << Self::BLOCK_SIZE_LOG2];
                // Read to local buf first
                try0!(len, BlockDevice::read_at(self, range.block, &mut block_buf));
                // Copy to target buf then
                buf.copy_from_slice(&block_buf[range.begin..range.end]);
            }
        }
        Ok(buf.len())
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize> {
        let iter = BlockIter {
            begin: offset,
            end: offset.checked_add(buf.len()).ok_or(DevError)?,
            block_size_log2: Self::BLOCK_SIZE_LOG2,
        };

        // For each block
        for range in iter {
            let len = range.origin_begin() - offset;
            let buf = &buf[range.origin_begin() - offset..range.origin_end() - offset];
            if range.is_full() {
                // Write to target buf directly. There is nothing of the old
                // block to keep, so the read-modify-write below would be a disk
                // round trip for nothing -- and a block the device refuses to
                // read (a write-only device, or one past what it will serve)
                // would take the write down with it, reported as a success.
                try0!(len, BlockDevice::write_at(self, range.block, buf));
            } else {
                // See `read_at`: exactly one block, and on the heap. Here the
                // length matters twice over, because this buffer is what gets
                // written back -- a longer one would rewrite the next block
                // with whatever the read left in the tail.
                let mut block_buf = vec![0u8; 1usize << Self::BLOCK_SIZE_LOG2];
                // Read to local buf first
                try0!(len, BlockDevice::read_at(self, range.block, &mut block_buf));
                // Write to local buf
                block_buf[range.begin..range.end].copy_from_slice(buf);
                // Write back to target buf
                try0!(len, BlockDevice::write_at(self, range.block, &block_buf));
            }
        }
        Ok(buf.len())
    }

    fn sync(&self) -> Result<()> {
        BlockDevice::sync(self)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::Mutex;

    impl BlockDevice for Mutex<[u8; 16]> {
        const BLOCK_SIZE_LOG2: u8 = 2;
        fn read_at(&self, block_id: BlockId, buf: &mut [u8]) -> Result<()> {
            if block_id >= 4 {
                return Err(DevError);
            }
            let begin = block_id << 2;
            buf[..4].copy_from_slice(&self.lock().unwrap()[begin..begin + 4]);
            Ok(())
        }
        fn write_at(&self, block_id: BlockId, buf: &[u8]) -> Result<()> {
            if block_id >= 4 {
                return Err(DevError);
            }
            let begin = block_id << 2;
            self.lock().unwrap()[begin..begin + 4].copy_from_slice(&buf[..4]);
            Ok(())
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
    }

    /// A device that behaves the way the disk drivers in this tree do
    /// (`ata/ahci.rs`, `nvme`, `virtio/blk`): it moves exactly as many bytes as
    /// it is handed, and refuses a request that runs past its capacity. Handing
    /// it a buffer longer than one block is therefore asking it for more than
    /// one block -- which is a write nobody asked for, and, at the end of the
    /// disk, a refusal.
    struct Strict(Mutex<[u8; 16]>, core::sync::atomic::AtomicUsize);

    impl BlockDevice for Strict {
        const BLOCK_SIZE_LOG2: u8 = 2;
        fn read_at(&self, block_id: BlockId, buf: &mut [u8]) -> Result<()> {
            let begin = block_id << 2;
            if begin + buf.len() > 16 {
                return Err(DevError);
            }
            self.1.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            buf.copy_from_slice(&self.0.lock().unwrap()[begin..begin + buf.len()]);
            Ok(())
        }
        fn write_at(&self, block_id: BlockId, buf: &[u8]) -> Result<()> {
            let begin = block_id << 2;
            if begin + buf.len() > 16 {
                return Err(DevError);
            }
            self.0.lock().unwrap()[begin..begin + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
    }

    impl Strict {
        fn new(fill: u8) -> Self {
            Strict(
                Mutex::new([fill; 16]),
                core::sync::atomic::AtomicUsize::new(0),
            )
        }
        fn snapshot(&self) -> [u8; 16] {
            *self.0.lock().unwrap()
        }
        fn reads(&self) -> usize {
            self.1.load(core::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn read() {
        let buf: Mutex<[u8; 16]> =
            Mutex::new([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        let mut res: [u8; 6] = [0; 6];

        // all inside
        let ret = Device::read_at(&buf, 3, &mut res);
        assert_eq!(ret, Ok(6));
        assert_eq!(res, [3, 4, 5, 6, 7, 8]);

        // partly inside
        let ret = Device::read_at(&buf, 11, &mut res);
        assert_eq!(ret, Ok(5));
        assert_eq!(res, [11, 12, 13, 14, 15, 8]);

        // all outside
        let ret = Device::read_at(&buf, 16, &mut res);
        assert_eq!(ret, Ok(0));
        assert_eq!(res, [11, 12, 13, 14, 15, 8]);
    }

    /// A device whose blocks are four bytes and whose last block is at the end
    /// of the array, so a request that runs one block past it is refused the
    /// way a real disk refuses one past its capacity.
    #[test]
    fn a_partial_write_touches_exactly_one_block() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        // Two bytes inside block 1: blocks 0 and 2 must not move.
        let ret = Device::write_at(&buf, 5, &[9, 9]);
        assert_eq!(ret, Ok(2));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 0, 0, 9, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_partial_write_to_the_last_block_still_lands() {
        let dev = Strict::new(0);
        // Block 3 is the last one. A read-modify-write of it must ask the
        // device for block 3 and nothing else: a request for block 4 as well
        // is one past the capacity, the device refuses the whole thing, and
        // the write is dropped with a success reported to the caller.
        let ret = Device::write_at(&dev, 13, &[7, 7]);
        assert_eq!(ret, Ok(2), "the write to the last block was dropped");
        assert_eq!(
            dev.snapshot(),
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 7, 0]
        );
    }

    #[test]
    fn a_partial_read_of_the_last_block_still_answers() {
        let dev = Strict::new(0);
        dev.0.lock().unwrap()[12..].copy_from_slice(&[12, 13, 14, 15]);
        let mut res = [0u8; 2];
        assert_eq!(Device::read_at(&dev, 13, &mut res), Ok(2));
        assert_eq!(res, [13, 14]);
    }

    #[test]
    fn a_partial_write_asks_the_device_for_one_block_and_no_more() {
        let dev = Strict::new(0xff);
        // Block 1 only. Block 2 must not be rewritten, even with what a read
        // of it would have returned.
        assert_eq!(Device::write_at(&dev, 5, &[1]), Ok(1));
        let data = dev.snapshot();
        assert_eq!(data[4], 0xff, "the byte before was clobbered");
        assert_eq!(data[5], 1);
        assert_eq!(data[6], 0xff, "the byte after was clobbered");
        assert_eq!(&data[8..12], &[0xff; 4], "the next block was written too");
    }

    #[test]
    fn a_partial_write_keeps_the_bytes_either_side_of_it() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0xff; 16]);
        let ret = Device::write_at(&buf, 9, &[1]);
        assert_eq!(ret, Ok(1));
        let data = *buf.lock().unwrap();
        assert_eq!(data[8], 0xff, "the byte before was zeroed");
        assert_eq!(data[9], 1);
        assert_eq!(data[10], 0xff, "the byte after was zeroed");
    }

    #[test]
    fn an_offset_that_would_overflow_is_refused_and_touches_nothing() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        let mut res = [0xccu8; 8];
        assert_eq!(
            Device::read_at(&buf, usize::MAX - 3, &mut res),
            Err(DevError)
        );
        assert_eq!(res, [0xcc; 8], "the buffer was reported read but untouched");
        assert_eq!(Device::write_at(&buf, usize::MAX - 3, &res), Err(DevError));
        assert_eq!(*buf.lock().unwrap(), [0; 16]);
    }

    #[test]
    fn an_offset_at_the_very_end_reads_nothing_rather_than_failing() {
        let buf: Mutex<[u8; 16]> = Mutex::new([1; 16]);
        let mut res = [0u8; 4];
        assert_eq!(Device::read_at(&buf, usize::MAX, &mut res), Err(DevError));
        // One past the last byte: nothing to read, and that is not an error.
        assert_eq!(Device::read_at(&buf, 16, &mut res), Ok(0));
    }

    #[test]
    fn an_empty_request_is_not_an_error() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        assert_eq!(Device::read_at(&buf, 4, &mut []), Ok(0));
        assert_eq!(Device::write_at(&buf, 4, &[]), Ok(0));
        assert_eq!(*buf.lock().unwrap(), [0; 16]);
    }

    #[test]
    fn a_whole_block_is_written_without_reading_it_first() {
        // Block-aligned and block-sized: there is nothing of the old block to
        // keep, so reading it first is a disk round trip for nothing -- and on
        // a device that cannot read, or a block the device refuses to read, the
        // refusal would drop the write and report success.
        let dev = Strict::new(0);
        assert_eq!(Device::write_at(&dev, 4, &[1, 2, 3, 4]), Ok(4));
        assert_eq!(
            dev.snapshot(),
            [0, 0, 0, 0, 1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(dev.reads(), 0, "a whole-block write read the block first");
    }

    #[test]
    fn a_partial_write_does_read_the_block_first() {
        // The other side of it: the bytes either side of the caller's range
        // have to come from somewhere.
        let dev = Strict::new(0);
        assert_eq!(Device::write_at(&dev, 5, &[1, 2]), Ok(2));
        assert_eq!(dev.reads(), 1);
    }

    #[test]
    fn a_request_spanning_three_blocks_moves_every_byte() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        let src: Vec<u8> = (1..=10).collect();
        assert_eq!(Device::write_at(&buf, 3, &src), Ok(10));
        let mut back = [0u8; 10];
        assert_eq!(Device::read_at(&buf, 3, &mut back), Ok(10));
        assert_eq!(back.to_vec(), src);
    }

    #[test]
    fn a_request_that_runs_off_the_end_is_a_short_transfer() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        // Blocks 3 and 4; only 3 exists.
        let ret = Device::write_at(&buf, 12, &[5, 5, 5, 5, 6, 6, 6, 6]);
        assert_eq!(ret, Ok(4), "reported {:?} bytes of the two blocks", ret);
        assert_eq!(&buf.lock().unwrap()[12..], &[5, 5, 5, 5]);
    }

    #[test]
    fn write() {
        let buf: Mutex<[u8; 16]> = Mutex::new([0; 16]);
        let res: [u8; 6] = [3, 4, 5, 6, 7, 8];

        // all inside
        let ret = Device::write_at(&buf, 3, &res);
        assert_eq!(ret, Ok(6));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0]
        );

        // partly inside
        let ret = Device::write_at(&buf, 11, &res);
        assert_eq!(ret, Ok(5));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 3, 4, 5, 6, 7, 8, 0, 0, 3, 4, 5, 6, 7]
        );

        // all outside
        let ret = Device::write_at(&buf, 16, &res);
        assert_eq!(ret, Ok(0));
        assert_eq!(
            *buf.lock().unwrap(),
            [0, 0, 0, 3, 4, 5, 6, 7, 8, 0, 0, 3, 4, 5, 6, 7]
        );
    }
}
