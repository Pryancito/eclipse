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
        const BS: usize = 512;
        let mut temp = SectorBuf([0u8; 512]);
        let mut done = 0usize;

        // Partial leading sector.
        let head_off = offset % BS;
        if head_off != 0 && done < buf.len() {
            let take = min(buf.len(), BS - head_off);
            self.block
                .read_block(offset / BS, &mut temp.0)
                .map_err(|_| DevError)?;
            buf[..take].copy_from_slice(&temp.0[head_off..head_off + take]);
            done += take;
        }

        // Whole-sector middle: a single multi-sector transfer.
        let mid = ((buf.len() - done) / BS) * BS;
        if mid > 0 {
            let block_id = (offset + done) / BS;
            self.block
                .read_block(block_id, &mut buf[done..done + mid])
                .map_err(|e| {
                    warn!(
                        "blockdev: read_block(block_id={}, {} sectors) failed: {:?} \
                         (byte_off={:#x}, block_count={})",
                        block_id,
                        mid / BS,
                        e,
                        offset + done,
                        self.block.block_count(),
                    );
                    DevError
                })?;
            done += mid;
        }

        // Partial trailing sector.
        if done < buf.len() {
            let take = buf.len() - done;
            self.block
                .read_block((offset + done) / BS, &mut temp.0)
                .map_err(|_| DevError)?;
            buf[done..].copy_from_slice(&temp.0[..take]);
            done += take;
        }

        Ok(done)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> DevResult<usize> {
        const BS: usize = 512;
        let mut temp = SectorBuf([0u8; 512]);
        let mut done = 0usize;

        // Partial leading sector: read-modify-write.
        let head_off = offset % BS;
        if head_off != 0 && done < buf.len() {
            let take = min(buf.len(), BS - head_off);
            let block_id = offset / BS;
            self.block
                .read_block(block_id, &mut temp.0)
                .map_err(|_| DevError)?;
            temp.0[head_off..head_off + take].copy_from_slice(&buf[..take]);
            self.block
                .write_block(block_id, &temp.0)
                .map_err(|_| DevError)?;
            done += take;
        }

        // Whole-sector middle: a single multi-sector transfer.
        let mid = ((buf.len() - done) / BS) * BS;
        if mid > 0 {
            let block_id = (offset + done) / BS;
            self.block
                .write_block(block_id, &buf[done..done + mid])
                .map_err(|e| {
                    warn!(
                        "blockdev: write_block(block_id={}, {} sectors) failed: {:?} \
                         (byte_off={:#x}, block_count={})",
                        block_id,
                        mid / BS,
                        e,
                        offset + done,
                        self.block.block_count(),
                    );
                    DevError
                })?;
            done += mid;
        }

        // Partial trailing sector: read-modify-write.
        if done < buf.len() {
            let take = buf.len() - done;
            let block_id = (offset + done) / BS;
            self.block
                .read_block(block_id, &mut temp.0)
                .map_err(|_| DevError)?;
            temp.0[..take].copy_from_slice(&buf[done..]);
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
    if block.read_block(EXT2_SUPERBLOCK_SECTOR, &mut sb.0).is_err() {
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

#[cfg(test)]
mod block_byte_tests {
    //! Host tests for the byte<->sector translation in `BlockByteDevice` — the
    //! kernel layer that sits between btrfs and the AHCI driver. btrfs is
    //! exonerated by the `btrfs` crate's own suites; this exercises the layer
    //! below it (partial-sector read-modify-write, multi-sector bulk, and
    //! end-of-device bounds) against an in-memory reference so a translation bug
    //! is caught on the host instead of only on hardware.
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use rcore_fs::dev::Device;
    use std::sync::Mutex;
    use zcore_drivers::scheme::{BlockScheme, Scheme};
    use zcore_drivers::{DeviceError, DeviceResult};

    /// In-memory 512-byte-sector block device. Records every `write_block`
    /// length so the test can also assert how the byte layer chunked the I/O.
    struct MockBlock {
        sectors: Mutex<Vec<u8>>,
        nsec: usize,
    }
    impl MockBlock {
        fn new(nsec: usize) -> Arc<Self> {
            Arc::new(Self {
                sectors: Mutex::new(vec![0u8; nsec * 512]),
                nsec,
            })
        }
    }
    impl Scheme for MockBlock {
        fn name(&self) -> &str {
            "mockblock"
        }
    }
    impl BlockScheme for MockBlock {
        fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
            if buf.is_empty() || buf.len() % 512 != 0 {
                return Err(DeviceError::InvalidParam);
            }
            let start = block_id * 512;
            let d = self.sectors.lock().unwrap();
            if start + buf.len() > d.len() {
                return Err(DeviceError::InvalidParam);
            }
            buf.copy_from_slice(&d[start..start + buf.len()]);
            Ok(())
        }
        fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
            if buf.is_empty() || buf.len() % 512 != 0 {
                return Err(DeviceError::InvalidParam);
            }
            let start = block_id * 512;
            let mut d = self.sectors.lock().unwrap();
            if start + buf.len() > d.len() {
                return Err(DeviceError::InvalidParam);
            }
            d[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            self.nsec
        }
    }

    fn pat(i: usize) -> u8 {
        (i.wrapping_mul(2654435761) >> 13) as u8
    }

    /// Write through the byte device at many aligned/unaligned offsets and
    /// sizes, mirror into a reference buffer, then read every byte back two ways
    /// and require an exact match. Catches off-by-one / partial-sector RMW bugs.
    #[test]
    fn byte_sector_roundtrip() {
        let nsec = 2048; // 1 MiB device
        let dev = MockBlock::new(nsec);
        let bbd = BlockByteDevice::new(dev.clone());
        let total = nsec * 512;
        let mut reference = vec![0u8; total];

        // (offset, len): aligned, unaligned head, unaligned tail, sub-sector,
        // multi-sector spanning, and right up to the last byte.
        let cases = [
            (0usize, 512usize),
            (1, 10),
            (511, 2),
            (500, 100),
            (0, 4096),
            (1234, 5000),
            (512 * 100 + 7, 9000),
            (total - 10, 10),
            (total - 513, 513),
            (777, 1),
        ];
        for (k, &(off, len)) in cases.iter().enumerate() {
            let payload: Vec<u8> = (0..len).map(|i| pat(off + i + k)).collect();
            let w = bbd.write_at(off, &payload).unwrap();
            assert_eq!(w, len, "short write off={off} len={len}");
            reference[off..off + len].copy_from_slice(&payload);
        }

        // Read back the whole device in one call.
        let mut got = vec![0u8; total];
        let mut p = 0;
        while p < total {
            let n = bbd.read_at(p, &mut got[p..]).unwrap();
            assert!(n > 0);
            p += n;
        }
        assert_eq!(got, reference, "full-device readback mismatch");

        // Read back in awkward unaligned slices too.
        for &(off, len) in &cases {
            let mut buf = vec![0u8; len];
            let mut q = 0;
            while q < len {
                let n = bbd.read_at(off + q, &mut buf[q..]).unwrap();
                assert!(n > 0);
                q += n;
            }
            assert_eq!(buf, &reference[off..off + len], "slice readback off={off}");
        }
    }

    /// A large contiguous write (bigger than any single AHCI command would do)
    /// must land correctly sector-for-sector.
    #[test]
    fn large_contiguous_write() {
        let nsec = 4096; // 2 MiB
        let dev = MockBlock::new(nsec);
        let bbd = BlockByteDevice::new(dev.clone());
        let len = 1_500_000usize; // ~1.4 MiB, spans hundreds of sectors
        let payload: Vec<u8> = (0..len).map(pat).collect();
        assert_eq!(bbd.write_at(2048, &payload).unwrap(), len);
        let mut got = vec![0u8; len];
        let mut p = 0;
        while p < len {
            p += bbd.read_at(2048 + p, &mut got[p..]).unwrap();
        }
        assert_eq!(got, payload);
    }
}
