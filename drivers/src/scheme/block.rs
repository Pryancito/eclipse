use super::Scheme;
use crate::DeviceResult;

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
