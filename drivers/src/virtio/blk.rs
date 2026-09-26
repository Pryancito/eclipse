use crate::sync::Mutex;
use virtio_drivers::{VirtIOBlk as InnerDriver, VirtIOHeader};

use crate::scheme::{BlockScheme, Scheme};
use crate::DeviceResult;

pub struct VirtIoBlk<'a> {
    inner: Mutex<InnerDriver<'a>>,
    capacity: usize,
}

impl<'a> VirtIoBlk<'a> {
    pub fn new(header: &'static mut VirtIOHeader) -> DeviceResult<Self> {
        // The capacity comes from the driver, which reads it during its own
        // initialisation. This used to read the config space here instead,
        // with `&*(header.config_space() as *const u64)`: a plain, non-volatile
        // load of a device register, taken **before** `InnerDriver::new` had
        // acknowledged the device and negotiated features -- which is the
        // point at which the specification says the config space means
        // anything.
        let inner = InnerDriver::new(header)?;
        let capacity = inner.capacity();
        Ok(Self {
            inner: Mutex::new(inner),
            capacity,
        })
    }
}

impl<'a> Scheme for VirtIoBlk<'a> {
    fn name(&self) -> &str {
        "virtio-blk"
    }

    fn handle_irq(&self, _irq_num: usize) {
        self.inner.lock().ack_interrupt();
    }
}

/// Sector size assumed by the underlying `virtio-drivers` crate, whose
/// `VirtIOBlk::{read,write}_block` hard-assert `buf.len() == 512` and only
/// ever transfer one sector per call.
const SECTOR_SIZE: usize = 512;

/// How many sectors a request covers, or why it cannot be served.
///
/// `block_id` indexes 512-byte sectors and `buf.len()` may be any multiple of
/// 512 (the `BlockScheme` contract, and what the cache and read-ahead layer in
/// `linux-object`'s `BlockByteDevice` relies on for multi-sector transfers).
/// The driver underneath moves exactly one sector per call and `assert!`s on
/// anything else, so a longer request is split here.
///
/// Every request is also checked against the disk. It used to go straight
/// through: a `block_id` past the end reached the device, and one close enough
/// to the end that `block_id + i` wrapped came back as a sector near zero --
/// a read of the wrong data, or a write over the superblock, with `Ok(())`
/// reported to the caller either way. The layers above genuinely ask for this:
/// `BlockCache` hands down a whole block at whatever offset its caller named,
/// and `rcore-fs`'s `Device::read_at` turns a device error into a short read,
/// so a wrong answer here is never seen again.
fn sectors_of(block_id: usize, len: usize, capacity: usize) -> DeviceResult<usize> {
    if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
        return Err(crate::DeviceError::InvalidParam);
    }
    let sectors = len / SECTOR_SIZE;
    let end = block_id
        .checked_add(sectors)
        .ok_or(crate::DeviceError::InvalidParam)?;
    if end > capacity {
        return Err(crate::DeviceError::InvalidParam);
    }
    Ok(sectors)
}

impl<'a> BlockScheme for VirtIoBlk<'a> {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        sectors_of(block_id, buf.len(), self.capacity)?;
        let mut inner = self.inner.lock();
        for (i, chunk) in buf.chunks_mut(SECTOR_SIZE).enumerate() {
            inner.read_block(block_id + i, chunk)?;
        }
        Ok(())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        sectors_of(block_id, buf.len(), self.capacity)?;
        let mut inner = self.inner.lock();
        for (i, chunk) in buf.chunks(SECTOR_SIZE).enumerate() {
            inner.write_block(block_id + i, chunk)?;
        }
        Ok(())
    }

    /// Nothing to do, and that is not the same as "the data is on the disk".
    ///
    /// A cache flush is `VIRTIO_BLK_T_FLUSH`, which a device only accepts once
    /// `VIRTIO_BLK_F_FLUSH` has been negotiated -- and the driver underneath
    /// negotiates `BlkFeature::empty()`, so it never is. A device that did not
    /// offer the feature has no write cache to flush, so answering `Ok` is
    /// correct for it; the day the driver negotiates the feature, this has to
    /// send the command instead of agreeing.
    fn flush(&self) -> DeviceResult {
        Ok(())
    }

    fn block_count(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::sectors_of;
    use crate::DeviceError;

    /// A disk of 64 sectors, i.e. sectors 0..64.
    const DISK: usize = 64;

    #[test]
    fn one_sector_is_one_sector() {
        assert_eq!(sectors_of(0, 512, DISK), Ok(1));
    }

    #[test]
    fn a_multi_sector_request_is_split() {
        assert_eq!(sectors_of(0, 4096, DISK), Ok(8));
    }

    #[test]
    fn a_request_of_nothing_is_refused() {
        // Not `Ok(())`: the loop below would do nothing and report success,
        // and a caller that asked for zero bytes has a bug of its own.
        assert_eq!(sectors_of(0, 0, DISK), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn a_buffer_that_is_not_whole_sectors_is_refused() {
        // The driver underneath asserts on it, which is a kernel panic on the
        // word of whatever called `read_block`.
        assert_eq!(sectors_of(0, 500, DISK), Err(DeviceError::InvalidParam));
        assert_eq!(sectors_of(0, 513, DISK), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn the_last_sector_of_the_disk_is_readable() {
        assert_eq!(sectors_of(DISK - 1, 512, DISK), Ok(1));
    }

    #[test]
    fn a_request_that_ends_one_sector_past_the_disk_is_refused() {
        assert_eq!(sectors_of(DISK, 512, DISK), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn a_request_that_starts_inside_the_disk_and_runs_off_the_end_is_refused() {
        // The whole request is refused rather than clipped: a short transfer
        // reported as success is how the layers above lose a write.
        assert_eq!(
            sectors_of(DISK - 2, 4096, DISK),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_request_whose_end_overflows_is_refused_rather_than_wrapping() {
        // `block_id + sectors` used to be computed inside the loop with no
        // check at all, so a block id near `usize::MAX` came back round to a
        // sector near zero -- the superblock, on a write.
        assert_eq!(
            sectors_of(usize::MAX, 512, DISK),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            sectors_of(usize::MAX - 1, 4096, DISK),
            Err(DeviceError::InvalidParam)
        );
    }

    #[test]
    fn a_disk_of_no_sectors_serves_nothing() {
        assert_eq!(sectors_of(0, 512, 0), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn the_whole_disk_in_one_request_is_allowed() {
        assert_eq!(sectors_of(0, DISK * 512, DISK), Ok(DISK));
    }
}
