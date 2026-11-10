use super::Scheme;
use crate::DeviceResult;
use core::sync::atomic::{AtomicU64, Ordering};

/// What a disk has transferred since boot, for `/proc/diskstats`: requests
/// and 512-byte sectors, each way. Only the ones that succeeded count, as in
/// Linux, where the counters are bumped on completion.
///
/// Kept by the driver itself and not by a wrapper around it: every consumer
/// (the filesystems, `/dev`, the hot-plug path) holds the driver's own `Arc`
/// and matches it by identity, so a wrapper would have to be the one thing
/// everybody holds.
#[derive(Debug, Default)]
pub struct IoStats {
    reads: AtomicU64,
    read_sectors: AtomicU64,
    writes: AtomicU64,
    write_sectors: AtomicU64,
}

/// One reading of an [`IoStats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IoSnapshot {
    pub reads: u64,
    pub read_sectors: u64,
    pub writes: u64,
    pub write_sectors: u64,
}

impl IoStats {
    pub const fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            read_sectors: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            write_sectors: AtomicU64::new(0),
        }
    }

    /// Count a read of `bytes` that ended in `r`.
    pub fn note_read(&self, r: &DeviceResult, bytes: usize) {
        if r.is_ok() {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.read_sectors
                .fetch_add((bytes / 512) as u64, Ordering::Relaxed);
        }
    }

    /// Count a write of `bytes` that ended in `r`.
    pub fn note_write(&self, r: &DeviceResult, bytes: usize) {
        if r.is_ok() {
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.write_sectors
                .fetch_add((bytes / 512) as u64, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> IoSnapshot {
        IoSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            read_sectors: self.read_sectors.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            write_sectors: self.write_sectors.load(Ordering::Relaxed),
        }
    }
}

/// A read to wait for without holding the CPU: see
/// [`BlockScheme::read_block_async`].
pub type BlockReadFuture<'a> =
    core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = DeviceResult> + Send + 'a>>;

/// Block device interface.
///
/// Convention shared by all implementations (AHCI, NVMe, partitions):
/// `block_id` indexes 512-byte sectors and `buf.len()` must be a non-zero
/// multiple of 512. A single call may transfer many sectors; drivers split
/// the request internally as needed.
pub trait BlockScheme: Scheme {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult;
    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult;
    fn flush(&self) -> DeviceResult;
    /// Total capacity in 512-byte sectors.
    fn block_count(&self) -> usize;
    /// Size in bytes of one **device** logical block — the unit the disk
    /// itself addresses, and therefore the unit every on-disk structure
    /// located "at LBA n" uses, the GPT header and its partition array
    /// included.
    ///
    /// This is *not* the unit of `block_id`/`block_count`, which stay in
    /// 512-byte sectors for every implementation; drivers whose device uses
    /// a larger block translate internally. It exists so that code which has
    /// to find something the disk placed at a given LBA — so far only the
    /// partition-table scanner — can compute the right byte offset. On a 4Kn
    /// namespace the GPT header is at byte 4096, not byte 512, and a scanner
    /// that assumes otherwise reports zero partitions on a perfectly good
    /// disk.
    ///
    /// Always a power of two and at least 512.
    fn logical_block_size(&self) -> usize {
        512
    }
    /// Prepare the device for a warm reset / power-off: flush volatile write
    /// caches and, where the protocol defines one (NVMe CC.SHN), perform an
    /// orderly shutdown. DRAM-less SSDs persist their FTL mapping tables on
    /// this signal, so skipping it across a warm reset costs them a recovery
    /// scan (and, on marginal firmware, risks mapping-table damage). Must be
    /// best-effort and time-bounded — it runs on the reboot path.
    fn quiesce_for_reboot(&self) {
        let _ = self.flush();
    }
    /// The read [`read_block`](Self::read_block) does, as a future, from a
    /// disk whose reads wait on something other than this CPU -- one served
    /// by a process, which answers when it is scheduled. A task that awaits
    /// it sleeps until the answer, where `read_block` keeps its CPU looking.
    /// `None`, the default, is a disk that only reads synchronously: the
    /// caller uses `read_block`.
    fn read_block_async<'a>(
        &'a self,
        _block_id: usize,
        _buf: &'a mut [u8],
    ) -> Option<BlockReadFuture<'a>> {
        None
    }
    /// What this disk has transferred, when its driver counts it ([`IoStats`]).
    /// `None` for what is not a disk of its own: a partition is counted in its
    /// disk.
    fn io_stats(&self) -> Option<IoSnapshot> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheme::Scheme;
    use crate::DeviceError;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// A block device that only answers the three required methods, so what a
    /// test reads back from it is the trait's own default and nothing else.
    struct BareBlock {
        flushes: AtomicUsize,
    }

    impl BareBlock {
        fn new() -> Self {
            Self {
                flushes: AtomicUsize::new(0),
            }
        }
    }

    impl Scheme for BareBlock {
        fn name(&self) -> &str {
            "bare-block"
        }
    }

    impl BlockScheme for BareBlock {
        fn read_block(&self, _block_id: usize, _buf: &mut [u8]) -> DeviceResult {
            Ok(())
        }
        fn write_block(&self, _block_id: usize, _buf: &[u8]) -> DeviceResult {
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn block_count(&self) -> usize {
            8
        }
    }

    /// A device that cannot flush. `quiesce_for_reboot` runs on the reboot
    /// path, where there is nobody left to return an error to, so it must
    /// swallow one rather than propagate it.
    struct FailingFlush;

    impl Scheme for FailingFlush {
        fn name(&self) -> &str {
            "failing-flush"
        }
    }

    impl BlockScheme for FailingFlush {
        fn read_block(&self, _block_id: usize, _buf: &mut [u8]) -> DeviceResult {
            Ok(())
        }
        fn write_block(&self, _block_id: usize, _buf: &[u8]) -> DeviceResult {
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            Err(DeviceError::IoError)
        }
        fn block_count(&self) -> usize {
            0
        }
    }

    /// 512 is the only answer that is right for a driver that did not think
    /// about its logical block size: every implementation addresses `block_id`
    /// in 512-byte sectors, so a default of anything else would make the
    /// partition scanner read a 4Kn offset off a 512-byte disk.
    #[test]
    fn io_stats_count_what_succeeded_in_sectors() {
        let st = IoStats::new();
        st.note_read(&Ok(()), 4096);
        st.note_read(&Err(DeviceError::IoError), 4096);
        st.note_write(&Ok(()), 512);
        st.note_write(&Ok(()), 1024);
        assert_eq!(
            st.snapshot(),
            IoSnapshot {
                reads: 1,
                read_sectors: 8,
                writes: 2,
                write_sectors: 3
            }
        );
        assert_eq!(
            BareBlock::new().io_stats(),
            None,
            "a driver that does not count says so"
        );
    }

    #[test]
    fn the_default_logical_block_size_is_a_512_byte_sector() {
        assert_eq!(BareBlock::new().logical_block_size(), 512);
    }

    /// Whatever a driver reports has to be a power of two and at least a
    /// sector, because the scanner multiplies an LBA by it.
    #[test]
    fn the_default_logical_block_size_is_a_power_of_two_sector_multiple() {
        let size = BareBlock::new().logical_block_size();
        assert!(size.is_power_of_two(), "{} is not a power of two", size);
        assert!(size >= 512, "{} is smaller than a sector", size);
    }

    /// The point of the default is the flush: a DRAM-less SSD persists its
    /// mapping tables on it, so a driver that does not override
    /// `quiesce_for_reboot` must still reach the disk exactly once.
    #[test]
    fn quiescing_for_reboot_flushes_once_by_default() {
        let dev = BareBlock::new();
        dev.quiesce_for_reboot();
        assert_eq!(dev.flushes.load(Ordering::Relaxed), 1);
        dev.quiesce_for_reboot();
        assert_eq!(dev.flushes.load(Ordering::Relaxed), 2);
    }

    /// Best-effort means best-effort: a flush that fails must not unwind out
    /// of the reboot path.
    #[test]
    fn a_failed_flush_does_not_escape_the_reboot_path() {
        FailingFlush.quiesce_for_reboot();
    }
}
