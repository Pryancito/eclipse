//! Metadata journal.
//!
//! Every metadata write (inode records, directories, extent blocks, bitmaps,
//! the superblock) goes to an in-memory [`Overlay`] instead of its home
//! location. File data is written straight to the device before the
//! metadata that points at it is committed (ext4's "ordered" mode).
//!
//! A commit writes the overlay as one transaction to the journal area,
//! syncs, writes each block home and syncs again. Only the latest
//! transaction is ever in the journal, and replaying it is idempotent, so
//! mount simply replays whatever valid transaction it finds:
//!
//! ```text
//! block 0      header: magic "EFSJ", version, seq, count, descriptor
//!              blocks, filesystem UUID, CRC32 of the rest, CRC32 of header
//! block 1..d   descriptor: home block number of each payload block (u64)
//! block d+1..  payload: the block images, in descriptor order
//! ```
//!
//! A crash while a transaction is being written leaves a header whose
//! checksum no longer matches its blocks, and the previous transaction was
//! already written home before this one started, so nothing is lost.
//!
//! Two rules keep replay from clobbering live data:
//! - blocks freed in a transaction can only be reused after it commits
//!   (`BlockAllocator` holds them back), so file data never lands on a
//!   block whose old image is still waiting in the overlay;
//! - a transaction too large for the journal is written home directly, but
//!   only after the journal is invalidated.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::crc::{crc32, crc32_update};
use crate::device::BlockDevice;
use crate::format::BLOCK_SIZE;
use crate::{Error, Result};

pub const JOURNAL_MAGIC: u32 = u32::from_le_bytes(*b"EFSJ");
pub const JOURNAL_VERSION: u32 = 1;
/// Home block numbers per descriptor block.
pub const TARGETS_PER_BLOCK: usize = BLOCK_SIZE as usize / 8;
const HDR_CRC: usize = 44;

const BS: usize = BLOCK_SIZE as usize;

/// A block number and its full image.
pub type Block = (u64, Vec<u8>);

/// Minimal lock for the overlay map: the filesystem is used behind the
/// caller's own lock, so this never actually contends; it only makes the
/// overlay `Sync` without `std`.
pub(crate) struct Spin<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// The value is only reached through `with`, which holds the lock.
unsafe impl<T: Send> Sync for Spin<T> {}
unsafe impl<T: Send> Send for Spin<T> {}

impl<T> Spin<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        struct Unlock<'a>(&'a AtomicBool);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _unlock = Unlock(&self.locked);
        // SAFETY: the lock is held until `_unlock` drops.
        f(unsafe { &mut *self.value.get() })
    }
}

/// Block cache of uncommitted metadata in front of the device. Reads see the
/// overlay first; writes never reach the device until [`Overlay::take`].
pub struct Overlay {
    inner: Arc<dyn BlockDevice>,
    blocks: Spin<BTreeMap<u64, Vec<u8>>>,
}

impl Overlay {
    pub fn new(inner: Arc<dyn BlockDevice>) -> Self {
        Self {
            inner,
            blocks: Spin::new(BTreeMap::new()),
        }
    }

    pub fn inner(&self) -> &Arc<dyn BlockDevice> {
        &self.inner
    }

    /// Blocks waiting to be committed.
    pub fn len(&self) -> usize {
        self.blocks.with(|m| m.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove and return everything waiting, in block order.
    pub fn take(&self) -> Vec<(u64, Vec<u8>)> {
        self.blocks.with(core::mem::take).into_iter().collect()
    }

    pub fn contains(&self, block: u64) -> bool {
        self.blocks.with(|m| m.contains_key(&block))
    }

    /// Put whole block images in (used by replay).
    pub fn insert(&self, block: u64, data: Vec<u8>) {
        debug_assert_eq!(data.len(), BS);
        self.blocks.with(|m| {
            m.insert(block, data);
        });
    }
}

impl BlockDevice for Overlay {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset.checked_add(buf.len() as u64).ok_or(Error::Io)?;
        let first = offset / BLOCK_SIZE;
        let last = (end - 1) / BLOCK_SIZE;
        self.blocks.with(|m| {
            // Device reads for the gaps between overlay blocks, one per gap.
            let mut pos = offset;
            for (&b, data) in m.range(first..=last) {
                let bstart = b * BLOCK_SIZE;
                if bstart > pos {
                    let o = (pos - offset) as usize;
                    let n = (bstart - pos) as usize;
                    self.inner.read_at(pos, &mut buf[o..o + n])?;
                    pos = bstart;
                }
                let bend = (bstart + BLOCK_SIZE).min(end);
                let o = (pos - offset) as usize;
                let n = (bend - pos) as usize;
                let in_blk = (pos - bstart) as usize;
                buf[o..o + n].copy_from_slice(&data[in_blk..in_blk + n]);
                pos = bend;
            }
            if pos < end {
                let o = (pos - offset) as usize;
                self.inner.read_at(pos, &mut buf[o..])?;
            }
            Ok(())
        })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let end = offset.checked_add(buf.len() as u64).ok_or(Error::Io)?;
        if end > self.inner.size() {
            return Err(Error::Io);
        }
        self.blocks.with(|m| {
            let mut pos = offset;
            while pos < end {
                let b = pos / BLOCK_SIZE;
                let bstart = b * BLOCK_SIZE;
                let bend = (bstart + BLOCK_SIZE).min(end);
                let src = &buf[(pos - offset) as usize..(bend - offset) as usize];
                let in_blk = (pos - bstart) as usize;
                if let Some(data) = m.get_mut(&b) {
                    data[in_blk..in_blk + src.len()].copy_from_slice(src);
                } else {
                    let mut data = vec![0u8; BS];
                    if src.len() < BS {
                        self.inner.read_at(bstart, &mut data)?;
                    }
                    data[in_blk..in_blk + src.len()].copy_from_slice(src);
                    m.insert(b, data);
                }
                pos = bend;
            }
            Ok(())
        })
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync()
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }
}

/// Where the journal lives on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalArea {
    pub start: u64,
    pub blocks: u64,
}

fn descriptor_blocks(count: usize) -> usize {
    count.div_ceil(TARGETS_PER_BLOCK)
}

impl JournalArea {
    /// Whether a transaction of `count` blocks fits.
    pub fn fits(&self, count: usize) -> bool {
        (1 + descriptor_blocks(count) + count) as u64 <= self.blocks
    }

    /// Largest transaction that fits.
    pub fn capacity(&self) -> usize {
        let mut n = self.blocks.saturating_sub(1) as usize;
        while n > 0 && !self.fits(n) {
            n -= 1;
        }
        n
    }

    /// Write `blocks` as transaction `seq`: descriptors and payload first,
    /// the header that makes them valid last. Does not sync.
    pub fn write(
        &self,
        dev: &dyn BlockDevice,
        uuid: &[u8; 16],
        seq: u64,
        blocks: &[(u64, Vec<u8>)],
    ) -> Result<()> {
        if !self.fits(blocks.len()) {
            return Err(Error::NoSpace);
        }
        let ndesc = descriptor_blocks(blocks.len());
        let mut crc = 0;
        let mut at = (self.start + 1) * BLOCK_SIZE;
        for chunk in blocks.chunks(TARGETS_PER_BLOCK) {
            let mut d = vec![0u8; BS];
            for (i, (b, _)) in chunk.iter().enumerate() {
                d[i * 8..i * 8 + 8].copy_from_slice(&b.to_le_bytes());
            }
            crc = crc32_update(crc, &d);
            dev.write_at(at, &d)?;
            at += BLOCK_SIZE;
        }
        // Payload: consecutive journal blocks, written in large pieces.
        for chunk in blocks.chunks(256) {
            let mut run = Vec::with_capacity(chunk.len() * BS);
            for (_, data) in chunk {
                crc = crc32_update(crc, data);
                run.extend_from_slice(data);
            }
            dev.write_at(at, &run)?;
            at += (chunk.len() * BS) as u64;
        }

        let mut h = vec![0u8; BS];
        h[0..4].copy_from_slice(&JOURNAL_MAGIC.to_le_bytes());
        h[4..8].copy_from_slice(&JOURNAL_VERSION.to_le_bytes());
        h[8..16].copy_from_slice(&seq.to_le_bytes());
        h[16..20].copy_from_slice(&(blocks.len() as u32).to_le_bytes());
        h[20..24].copy_from_slice(&(ndesc as u32).to_le_bytes());
        h[24..40].copy_from_slice(uuid);
        h[40..44].copy_from_slice(&crc.to_le_bytes());
        let hcrc = crc32(&h[..HDR_CRC]);
        h[HDR_CRC..HDR_CRC + 4].copy_from_slice(&hcrc.to_le_bytes());
        dev.write_at(self.start * BLOCK_SIZE, &h)
    }

    /// Make the journal hold no transaction. Does not sync.
    pub fn invalidate(&self, dev: &dyn BlockDevice) -> Result<()> {
        dev.write_at(self.start * BLOCK_SIZE, &[0u8; BS])
    }

    /// The transaction in the journal, if there is a complete one for this
    /// filesystem: `(seq, [(home block, image)])`. Blocks outside
    /// `0..total_blocks` make the whole transaction invalid.
    pub fn read(
        &self,
        dev: &dyn BlockDevice,
        uuid: &[u8; 16],
        total_blocks: u64,
    ) -> Result<Option<(u64, Vec<Block>)>> {
        let mut h = vec![0u8; BS];
        dev.read_at(self.start * BLOCK_SIZE, &mut h)?;
        let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        if le32(&h, 0) != JOURNAL_MAGIC
            || le32(&h, 4) != JOURNAL_VERSION
            || le32(&h, HDR_CRC) != crc32(&h[..HDR_CRC])
            || h[24..40] != uuid[..]
        {
            return Ok(None);
        }
        let seq = u64::from_le_bytes(h[8..16].try_into().unwrap());
        let count = le32(&h, 16) as usize;
        let ndesc = le32(&h, 20) as usize;
        if ndesc != descriptor_blocks(count) || !self.fits(count) {
            return Ok(None);
        }
        let mut body = vec![0u8; (ndesc + count) * BS];
        dev.read_at((self.start + 1) * BLOCK_SIZE, &mut body)?;
        if crc32(&body) != le32(&h, 40) {
            return Ok(None);
        }
        let (desc, payload) = body.split_at(ndesc * BS);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let target = u64::from_le_bytes(desc[i * 8..i * 8 + 8].try_into().unwrap());
            if target >= total_blocks || (target >= self.start && target < self.start + self.blocks)
            {
                return Ok(None);
            }
            out.push((target, payload[i * BS..(i + 1) * BS].to_vec()));
        }
        Ok(Some((seq, out)))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    struct Ram(Spin<Vec<u8>>);

    impl BlockDevice for Ram {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            self.0.with(|d| {
                let o = offset as usize;
                buf.copy_from_slice(d.get(o..o + buf.len()).ok_or(Error::Io)?);
                Ok(())
            })
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
            self.0.with(|d| {
                let o = offset as usize;
                d.get_mut(o..o + buf.len())
                    .ok_or(Error::Io)?
                    .copy_from_slice(buf);
                Ok(())
            })
        }
        fn sync(&self) -> Result<()> {
            Ok(())
        }
        fn size(&self) -> u64 {
            self.0.with(|d| d.len() as u64)
        }
    }

    fn ram(blocks: usize) -> Arc<Ram> {
        let data = (0..blocks * BS).map(|i| (i / BS) as u8).collect();
        Arc::new(Ram(Spin::new(data)))
    }

    fn block(fill: u8) -> Vec<u8> {
        vec![fill; BS]
    }

    #[test]
    fn overlay_reads_merge_with_the_device() {
        let dev = ram(16);
        let o = Overlay::new(dev.clone());
        o.write_at(3 * BLOCK_SIZE + 10, &[0xAA; 20]).unwrap();
        o.write_at(5 * BLOCK_SIZE, &block(0xBB)).unwrap();
        assert_eq!(o.len(), 2);
        // The device itself is untouched.
        let mut b = [0u8; 1];
        dev.read_at(3 * BLOCK_SIZE + 10, &mut b).unwrap();
        assert_eq!(b[0], 3);
        // A read across both overlay blocks and the gaps around them.
        let mut buf = vec![0u8; 4 * BS];
        o.read_at(2 * BLOCK_SIZE, &mut buf).unwrap();
        assert!(buf[..BS].iter().all(|&x| x == 2));
        assert!(buf[BS..BS + 10].iter().all(|&x| x == 3));
        assert!(buf[BS + 10..BS + 30].iter().all(|&x| x == 0xAA));
        assert!(buf[BS + 30..2 * BS].iter().all(|&x| x == 3));
        assert!(buf[2 * BS..3 * BS].iter().all(|&x| x == 4));
        assert!(buf[3 * BS..].iter().all(|&x| x == 0xBB));
        // Unaligned read inside one overlay block.
        let mut small = [0u8; 4];
        o.read_at(3 * BLOCK_SIZE + 28, &mut small).unwrap();
        assert_eq!(small, [0xAA, 0xAA, 3, 3]);
    }

    #[test]
    fn overlay_writes_span_blocks_and_take_drains() {
        let dev = ram(8);
        let o = Overlay::new(dev.clone());
        o.write_at(BLOCK_SIZE - 2, &[9; 4]).unwrap();
        o.write_at(BLOCK_SIZE - 1, &[7]).unwrap();
        assert!(o.contains(0) && o.contains(1) && !o.contains(2));
        let blocks = o.take();
        assert!(o.is_empty());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, 0);
        assert_eq!(&blocks[0].1[BS - 2..], &[9, 7]);
        assert!(blocks[0].1[..BS - 2].iter().all(|&x| x == 0));
        assert_eq!(&blocks[1].1[..3], &[9, 9, 1]);
        assert!(o.write_at(8 * BLOCK_SIZE - 1, &[0; 2]).is_err());
        assert!(o.write_at(u64::MAX, &[0; 2]).is_err());
        o.write_at(0, &[]).unwrap();
        o.read_at(0, &mut []).unwrap();
        assert!(o.is_empty());
    }

    #[test]
    fn capacity_accounts_for_descriptors() {
        let j = JournalArea {
            start: 2,
            blocks: 64,
        };
        assert_eq!(j.capacity(), 62);
        assert!(j.fits(62) && !j.fits(63));
        let big = JournalArea {
            start: 2,
            blocks: 4096,
        };
        // 4096 = header + 8 descriptors + 4087 payload.
        assert_eq!(big.capacity(), 4087);
        assert!(big.fits(0));
    }

    #[test]
    fn transaction_round_trip() {
        let dev = ram(128);
        let j = JournalArea {
            start: 2,
            blocks: 64,
        };
        let uuid = [5u8; 16];
        let tx: Vec<Block> = (0..30).map(|i| (70 + i, block(i as u8))).collect();
        j.write(&*dev, &uuid, 42, &tx).unwrap();
        let (seq, back) = j.read(&*dev, &uuid, 128).unwrap().unwrap();
        assert_eq!(seq, 42);
        assert_eq!(back, tx);
        // Another filesystem's journal, or a too-small device, is ignored.
        assert!(j.read(&*dev, &[6; 16], 128).unwrap().is_none());
        assert!(j.read(&*dev, &uuid, 90).unwrap().is_none());
        j.invalidate(&*dev).unwrap();
        assert!(j.read(&*dev, &uuid, 128).unwrap().is_none());
    }

    #[test]
    fn any_damage_invalidates_the_transaction() {
        let dev = ram(128);
        let j = JournalArea {
            start: 2,
            blocks: 64,
        };
        let uuid = [5u8; 16];
        let tx: Vec<Block> = (0..5).map(|i| (80 + i, block(0x10 + i as u8))).collect();
        j.write(&*dev, &uuid, 1, &tx).unwrap();
        for at in [0, 8, 16, 44, BS + 3, 2 * BS + 100, 7 * BS - 1] {
            let pos = 2 * BLOCK_SIZE + at as u64;
            let mut b = [0u8; 1];
            dev.read_at(pos, &mut b).unwrap();
            dev.write_at(pos, &[b[0] ^ 1]).unwrap();
            assert!(j.read(&*dev, &uuid, 128).unwrap().is_none(), "byte {}", at);
            dev.write_at(pos, &b).unwrap();
        }
        assert!(j.read(&*dev, &uuid, 128).unwrap().is_some());
    }

    #[test]
    fn targets_inside_the_journal_are_refused() {
        let dev = ram(128);
        let j = JournalArea {
            start: 2,
            blocks: 16,
        };
        j.write(&*dev, &[1; 16], 1, &[(10, block(1))]).unwrap();
        assert!(j.read(&*dev, &[1; 16], 128).unwrap().is_none());
        assert_eq!(
            j.write(&*dev, &[1; 16], 1, &vec![(40, block(1)); 15]),
            Err(Error::NoSpace)
        );
    }

    /// A header whose descriptor count does not match its block count (or
    /// asks for more than the area holds) is no transaction.
    #[test]
    fn a_header_with_the_wrong_shape_is_ignored() {
        let dev = ram(64);
        let area = JournalArea {
            start: 4,
            blocks: 16,
        };
        let uuid = [3u8; 16];
        area.write(&*dev, &uuid, 9, &[(40, block(1)), (41, block(2))])
            .unwrap();
        assert!(area.read(&*dev, &uuid, 64).unwrap().is_some());
        let forge = |count: u32, ndesc: u32| {
            let mut h = vec![0u8; BS];
            dev.read_at(4 * BLOCK_SIZE, &mut h).unwrap();
            h[16..20].copy_from_slice(&count.to_le_bytes());
            h[20..24].copy_from_slice(&ndesc.to_le_bytes());
            let c = crc32(&h[..HDR_CRC]);
            h[HDR_CRC..HDR_CRC + 4].copy_from_slice(&c.to_le_bytes());
            dev.write_at(4 * BLOCK_SIZE, &h).unwrap();
        };
        forge(2, 2);
        assert_eq!(area.read(&*dev, &uuid, 64).unwrap(), None);
        forge(100, 1);
        assert_eq!(area.read(&*dev, &uuid, 64).unwrap(), None);
    }

    /// The lock really excludes: two threads adding through it lose nothing.
    #[cfg(feature = "std")]
    #[test]
    fn the_spin_lock_excludes() {
        let s = Arc::new(Spin::new(0u64));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for _ in 0..50_000 {
                        s.with(|v| *v += 1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(s.with(|v| *v), 200_000);
    }

    #[test]
    fn the_test_disk_syncs() {
        ram(1).sync().unwrap();
    }
}
