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
