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
}
