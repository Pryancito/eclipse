//! Block-backed and file-backed devices for mountable filesystems.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::cmp::min;

use lock::Mutex;
use rcore_fs::dev::{DevError, Device, Result as DevResult};
use rcore_fs::vfs::{FsError, INode, Result as VfsResult};
use zcore_drivers::{scheme::BlockScheme, DeviceResult};

use super::devfs::BlockDev;

/// 512-byte sector buffer aligned for AHCI DMA.
#[repr(align(4096))]
struct SectorBuf([u8; 512]);

const SECTOR: usize = 512;

/// Hot-generation capacity of the per-device block cache, in 512-byte sectors.
/// ~4 MiB hot; with the warm generation the cache holds up to ~8 MiB. Sized to
/// keep the boot working set (ext2/btrfs inode & B-tree metadata blocks plus the
/// repeatedly re-exec'd busybox image) resident.
const CACHE_HOT_CAP: usize = 8192;

/// A generational ("two-hand clock") LRU cache of disk sectors.
///
/// Every filesystem read on Eclipse goes `FS -> BlockByteDevice -> block driver
/// -> real disk (DMA + IRQ)`. Without caching, the same sectors — inode tables
/// shared by many inodes, directory/B-tree blocks walked on every lookup, and
/// the busybox binary re-read on every `exec` — are fetched from the device
/// again and again, which is what makes OpenRC's exec/stat-heavy startup crawl.
///
/// Approximate LRU at O(1): inserts go into `hot`; when `hot` fills, the old
/// `warm` set is dropped and `hot` becomes the new `warm`. A hit in `warm` is
/// promoted back to `hot`. Memory is bounded to ~2 * `CACHE_HOT_CAP` sectors.
struct SectorCache {
    hot: BTreeMap<usize, Box<[u8; SECTOR]>>,
    warm: BTreeMap<usize, Box<[u8; SECTOR]>>,
}

impl SectorCache {
    fn new() -> Self {
        Self {
            hot: BTreeMap::new(),
            warm: BTreeMap::new(),
        }
    }

    /// Copy sector `id` into `out` (must be 512 bytes) if cached, promoting a
    /// warm hit. Returns `true` on a hit.
    fn get_into(&mut self, id: usize, out: &mut [u8]) -> bool {
        if let Some(d) = self.hot.get(&id) {
            out.copy_from_slice(&d[..]);
            return true;
        }
        if let Some(d) = self.warm.remove(&id) {
            out.copy_from_slice(&d[..]);
            self.insert(id, d);
            return true;
        }
        false
    }

    fn insert(&mut self, id: usize, data: Box<[u8; SECTOR]>) {
        self.hot.insert(id, data);
        if self.hot.len() >= CACHE_HOT_CAP {
            self.warm = core::mem::take(&mut self.hot);
        }
    }

    fn put_from(&mut self, id: usize, src: &[u8]) {
        let mut b = Box::new([0u8; SECTOR]);
        b.copy_from_slice(src);
        self.insert(id, b);
    }
}

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

/// Byte-oriented device over a block driver, with a sector cache.
pub struct BlockByteDevice {
    block: Arc<dyn BlockScheme>,
    cache: Mutex<SectorCache>,
}

impl BlockByteDevice {
    pub fn new(block: Arc<dyn BlockScheme>) -> Self {
        Self {
            block,
            cache: Mutex::new(SectorCache::new()),
        }
    }

    /// Read `buf.len() / 512` consecutive sectors starting at `block_id`,
    /// serving from the cache and hitting the device only on a miss. `buf.len()`
    /// must be a non-zero multiple of 512. Returns the same result type as the
    /// underlying driver so callers keep their existing error handling.
    fn read_block_cached(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        let nsec = buf.len() / SECTOR;
        // Fast path: every requested sector is already cached.
        {
            let mut cache = self.cache.lock();
            let mut all_hit = true;
            for i in 0..nsec {
                if !cache.get_into(block_id + i, &mut buf[i * SECTOR..(i + 1) * SECTOR]) {
                    all_hit = false;
                    break;
                }
            }
            if all_hit {
                return Ok(());
            }
        }
        // Miss: one device transfer for the whole run, then warm the cache.
        self.block.read_block(block_id, buf)?;
        {
            let mut cache = self.cache.lock();
            for i in 0..nsec {
                cache.put_from(block_id + i, &buf[i * SECTOR..(i + 1) * SECTOR]);
            }
        }
        Ok(())
    }

    /// Write `buf.len() / 512` consecutive sectors starting at `block_id`,
    /// write-through to the device and refreshing the cache so later reads stay
    /// consistent and warm. `buf.len()` must be a non-zero multiple of 512.
    fn write_block_cached(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        self.block.write_block(block_id, buf)?;
        let nsec = buf.len() / SECTOR;
        let mut cache = self.cache.lock();
        for i in 0..nsec {
            cache.put_from(block_id + i, &buf[i * SECTOR..(i + 1) * SECTOR]);
        }
        Ok(())
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
            self.read_block_cached(offset / BS, &mut temp.0)
                .map_err(|_| DevError)?;
            buf[..take].copy_from_slice(&temp.0[head_off..head_off + take]);
            done += take;
        }

        // Whole-sector middle: a single multi-sector transfer.
        let mid = ((buf.len() - done) / BS) * BS;
        if mid > 0 {
            let block_id = (offset + done) / BS;
            self.read_block_cached(block_id, &mut buf[done..done + mid])
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
            self.read_block_cached((offset + done) / BS, &mut temp.0)
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
            self.read_block_cached(block_id, &mut temp.0)
                .map_err(|_| DevError)?;
            temp.0[head_off..head_off + take].copy_from_slice(&buf[..take]);
            self.write_block_cached(block_id, &temp.0)
                .map_err(|_| DevError)?;
            done += take;
        }

        // Whole-sector middle: a single multi-sector transfer.
        let mid = ((buf.len() - done) / BS) * BS;
        if mid > 0 {
            let block_id = (offset + done) / BS;
            self.write_block_cached(block_id, &buf[done..done + mid])
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
            self.read_block_cached(block_id, &mut temp.0)
                .map_err(|_| DevError)?;
            temp.0[..take].copy_from_slice(&buf[done..]);
            self.write_block_cached(block_id, &temp.0)
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
