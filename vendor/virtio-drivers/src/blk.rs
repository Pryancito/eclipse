use super::*;
use crate::header::VirtIOHeader;
use crate::queue::VirtQueue;
use alloc::vec::Vec;
use bitflags::*;
use core::hint::spin_loop;
use log::*;
use volatile::Volatile;

/// The virtio block device is a simple virtual block device (ie. disk).
///
/// Read and write requests (and other exotic requests) are placed in the queue,
/// and serviced (probably out of order) by the device except where noted.
pub struct VirtIOBlk<'a> {
    header: &'static mut VirtIOHeader,
    queue: VirtQueue<'a>,
    capacity: usize,
}

impl VirtIOBlk<'_> {
    /// Create a new VirtIO-Blk driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        header.begin_init(|features| {
            let features = BlkFeature::from_bits_truncate(features);
            info!("device features: {:?}", features);
            // negotiate these flags only
            let supported_features = BlkFeature::empty();
            (features & supported_features).bits()
        });

        // read configuration space
        let config = unsafe { &mut *(header.config_space() as *mut BlkConfig) };
        info!("config: {:?}", config);
        info!(
            "found a block device of size {}KB",
            config.capacity.read() / 2
        );

        let queue = VirtQueue::new(header, 0, QUEUE_SIZE as u16)?;
        header.finish_init();

        Ok(VirtIOBlk {
            header,
            queue,
            capacity: config.capacity.read() as usize,
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.header.ack_interrupt()
    }

    /// The size of the disk, in 512-byte sectors.
    ///
    /// Read once, during initialisation, from the config space -- which is the
    /// only time it is valid to read: before `begin_init` the device has not
    /// been acknowledged, and a caller that reads the window itself has to
    /// dereference it as a plain `u64`, without the `Volatile` the register
    /// deserves. `zcore-drivers` used to do exactly that, from a raw pointer,
    /// before this driver had even initialised the device.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Read `buf.len() / 512` consecutive sectors from `block_id`, in as few
    /// requests as the queue allows.
    ///
    /// `read_block` is one sector per request, and a request is a notify, a
    /// trip through the device and a wait: reading a 4 KiB page that way was
    /// eight of them, which cost the same whether the host had the data cached
    /// or not. Here the buffer goes out as one request of up to
    /// [`MAX_REQUEST_BYTES`], cut at page boundaries because each segment is
    /// handed to the device by the physical address of its first byte, and a
    /// virtually contiguous buffer need not be physically contiguous.
    pub fn read_blocks(&mut self, block_id: usize, buf: &mut [u8]) -> Result {
        if !buf.len().is_multiple_of(BLK_SIZE) {
            return Err(Error::InvalidParam);
        }
        let mut sector = block_id;
        for chunk in buf.chunks_mut(MAX_REQUEST_BYTES) {
            let req = BlkReq {
                type_: ReqType::In,
                reserved: 0,
                sector: sector as u64,
            };
            let mut resp = BlkResp::default();
            let mut outputs: Vec<&mut [u8]> = page_segments_mut(chunk);
            outputs.push(resp.as_buf_mut());
            self.queue.add(&[req.as_buf()], &outputs)?;
            drop(outputs);
            self.wait_one()?;
            if resp.status != RespStatus::Ok {
                return Err(Error::IoError);
            }
            sector += chunk.len() / BLK_SIZE;
        }
        Ok(())
    }

    /// Write `buf.len() / 512` consecutive sectors from `block_id`: the
    /// counterpart of [`read_blocks`](Self::read_blocks), cut the same way.
    pub fn write_blocks(&mut self, block_id: usize, buf: &[u8]) -> Result {
        if !buf.len().is_multiple_of(BLK_SIZE) {
            return Err(Error::InvalidParam);
        }
        let mut sector = block_id;
        for chunk in buf.chunks(MAX_REQUEST_BYTES) {
            let req = BlkReq {
                type_: ReqType::Out,
                reserved: 0,
                sector: sector as u64,
            };
            let mut resp = BlkResp::default();
            let mut inputs: Vec<&[u8]> = Vec::with_capacity(MAX_SEGMENTS + 1);
            inputs.push(req.as_buf());
            inputs.extend(page_segments(chunk));
            self.queue.add(&inputs, &[resp.as_buf_mut()])?;
            drop(inputs);
            self.wait_one()?;
            if resp.status != RespStatus::Ok {
                return Err(Error::IoError);
            }
            sector += chunk.len() / BLK_SIZE;
        }
        Ok(())
    }

    /// Tell the device there is a request and spin until it is back.
    fn wait_one(&mut self) -> Result {
        self.header.notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used()?;
        Ok(())
    }

    /// Read a block.
    pub fn read_block(&mut self, block_id: usize, buf: &mut [u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::In,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut resp = BlkResp::default();
        self.queue.add(&[req.as_buf()], &[buf, resp.as_buf_mut()])?;
        self.header.notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used()?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }

    /// Write a block.
    pub fn write_block(&mut self, block_id: usize, buf: &[u8]) -> Result {
        assert_eq!(buf.len(), BLK_SIZE);
        let req = BlkReq {
            type_: ReqType::Out,
            reserved: 0,
            sector: block_id as u64,
        };
        let mut resp = BlkResp::default();
        self.queue.add(&[req.as_buf(), buf], &[resp.as_buf_mut()])?;
        self.header.notify(0);
        while !self.queue.can_pop() {
            spin_loop();
        }
        self.queue.pop_used()?;
        match resp.status {
            RespStatus::Ok => Ok(()),
            _ => Err(Error::IoError),
        }
    }
}

#[repr(C)]
#[derive(Debug)]
struct BlkConfig {
    /// Number of 512 Bytes sectors
    capacity: Volatile<u64>,
    size_max: Volatile<u32>,
    seg_max: Volatile<u32>,
    cylinders: Volatile<u16>,
    heads: Volatile<u8>,
    sectors: Volatile<u8>,
    blk_size: Volatile<u32>,
    physical_block_exp: Volatile<u8>,
    alignment_offset: Volatile<u8>,
    min_io_size: Volatile<u16>,
    opt_io_size: Volatile<u32>,
    // ... ignored
}

#[repr(C)]
#[derive(Debug)]
struct BlkReq {
    type_: ReqType,
    reserved: u32,
    sector: u64,
}

#[repr(C)]
#[derive(Debug)]
struct BlkResp {
    status: RespStatus,
}

#[repr(u32)]
#[derive(Debug)]
enum ReqType {
    In = 0,
    Out = 1,
    Flush = 4,
    Discard = 11,
    WriteZeroes = 13,
}

#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
enum RespStatus {
    Ok = 0,
    IoErr = 1,
    Unsupported = 2,
    _NotReady = 3,
}

impl Default for BlkResp {
    fn default() -> Self {
        BlkResp {
            status: RespStatus::_NotReady,
        }
    }
}

const BLK_SIZE: usize = 512;

/// Descriptors in the request queue (`VirtQueue::new` in [`VirtIOBlk::new`]).
const QUEUE_SIZE: usize = 16;

/// Data segments one request can carry: the queue less the header and the
/// status byte.
const MAX_SEGMENTS: usize = QUEUE_SIZE - 2;

/// The most one request moves. A span of this many bytes, wherever it
/// starts, touches at most [`MAX_SEGMENTS`] pages; and it is a whole number
/// of sectors, so every request but the last of a buffer is full.
const MAX_REQUEST_BYTES: usize = (MAX_SEGMENTS - 1) * PAGE_SIZE;

/// The length of the first segment of a buffer at `addr`: up to the next page
/// boundary, or all of it.
fn first_segment(addr: usize, len: usize) -> usize {
    (PAGE_SIZE - addr % PAGE_SIZE).min(len)
}

/// `buf`, cut at page boundaries.
fn page_segments(mut buf: &[u8]) -> Vec<&[u8]> {
    let mut segments = Vec::with_capacity(MAX_SEGMENTS);
    while !buf.is_empty() {
        let (head, rest) = buf.split_at(first_segment(buf.as_ptr() as usize, buf.len()));
        segments.push(head);
        buf = rest;
    }
    segments
}

/// `buf`, cut at page boundaries, writable.
fn page_segments_mut(mut buf: &mut [u8]) -> Vec<&mut [u8]> {
    let mut segments = Vec::with_capacity(MAX_SEGMENTS + 1);
    while !buf.is_empty() {
        let at = first_segment(buf.as_ptr() as usize, buf.len());
        let (head, rest) = buf.split_at_mut(at);
        segments.push(head);
        buf = rest;
    }
    segments
}

bitflags! {
    struct BlkFeature: u64 {
        /// Device supports request barriers. (legacy)
        const BARRIER       = 1 << 0;
        /// Maximum size of any single segment is in `size_max`.
        const SIZE_MAX      = 1 << 1;
        /// Maximum number of segments in a request is in `seg_max`.
        const SEG_MAX       = 1 << 2;
        /// Disk-style geometry specified in geometry.
        const GEOMETRY      = 1 << 4;
        /// Device is read-only.
        const RO            = 1 << 5;
        /// Block size of disk is in `blk_size`.
        const BLK_SIZE      = 1 << 6;
        /// Device supports scsi packet commands. (legacy)
        const SCSI          = 1 << 7;
        /// Cache flush command support.
        const FLUSH         = 1 << 9;
        /// Device exports information on optimal I/O alignment.
        const TOPOLOGY      = 1 << 10;
        /// Device can toggle its cache between writeback and writethrough modes.
        const CONFIG_WCE    = 1 << 11;
        /// Device can support discard command, maximum discard sectors size in
        /// `max_discard_sectors` and maximum discard segment number in
        /// `max_discard_seg`.
        const DISCARD       = 1 << 13;
        /// Device can support write zeroes command, maximum write zeroes sectors
        /// size in `max_write_zeroes_sectors` and maximum write zeroes segment
        /// number in `max_write_zeroes_seg`.
        const WRITE_ZEROES  = 1 << 14;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // the following since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

unsafe impl AsBuf for BlkReq {}
unsafe impl AsBuf for BlkResp {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::{fake_header, set_config_u64, Disk, Ring};
    use core::convert::TryInto;
    use std::sync::Arc;

    /// A driver talking to a disk of `sectors` sectors, with the device served
    /// on its own thread: `read_block` spins on `can_pop`, so nothing can
    /// answer it from the calling thread.
    struct Attached {
        blk: VirtIOBlk<'static>,
        disk: Arc<Disk>,
        device: Option<std::thread::JoinHandle<()>>,
    }

    impl Attached {
        fn of(sectors: usize) -> Self {
            let header = fake_header(2, 256);
            set_config_u64(header, sectors as u64);
            let blk = VirtIOBlk::new(header).expect("the driver refused the device");
            let ring = Ring::of(blk.header, 0, 16);
            let disk = Disk::of(sectors);
            let served = disk.clone();
            let device = std::thread::spawn(move || served.serve(ring));
            Attached {
                blk,
                disk,
                device: Some(device),
            }
        }
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            self.disk.stop();
            if let Some(device) = self.device.take() {
                let _ = device.join();
            }
        }
    }

    /// Run `body` with a deadline: a driver waiting on a device that will not
    /// answer spins for ever, and a test that hangs says nothing.
    fn within(what: &'static str, secs: u64, body: impl FnOnce() + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            body();
            let _ = tx.send(());
        });
        match rx.recv_timeout(core::time::Duration::from_secs(secs)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                handle.join().expect("the body of the test failed");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!(
                    "{} never returned: the driver is waiting on the device",
                    what
                )
            }
        }
    }

    #[test]
    fn the_capacity_comes_from_the_config_space() {
        let header = fake_header(2, 256);
        set_config_u64(header, 2048);
        let blk = VirtIOBlk::new(header).unwrap();
        assert_eq!(blk.capacity(), 2048);
    }

    #[test]
    fn a_read_brings_back_the_sector_that_was_asked_for() {
        within("a read of sector 7", 20, || {
            let mut attached = Attached::of(64);
            let mut buf = [0u8; 512];
            attached.blk.read_block(7, &mut buf).unwrap();
            assert_eq!(&buf[..], &attached.disk.sector(7)[..]);
            assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 7);
        });
    }

    #[test]
    fn a_write_lands_on_the_sector_that_was_asked_for() {
        within("a write of sector 3", 20, || {
            let mut attached = Attached::of(64);
            let mut buf = [0u8; 512];
            buf[..5].copy_from_slice(b"hello");
            attached.blk.write_block(3, &buf).unwrap();
            assert_eq!(&attached.disk.sector(3)[..5], b"hello");
            // and its neighbours are untouched
            assert_eq!(
                u32::from_le_bytes(attached.disk.sector(4)[0..4].try_into().unwrap()),
                4
            );
        });
    }

    #[test]
    fn a_write_then_a_read_of_the_same_sector_agree() {
        within("a write followed by a read", 20, || {
            let mut attached = Attached::of(8);
            let mut written = [0u8; 512];
            for (i, byte) in written.iter_mut().enumerate() {
                *byte = (i % 251) as u8;
            }
            attached.blk.write_block(5, &written).unwrap();
            let mut read = [0u8; 512];
            attached.blk.read_block(5, &mut read).unwrap();
            assert_eq!(&read[..], &written[..]);
        });
    }

    #[test]
    fn a_disk_that_reports_an_error_is_an_error_and_not_a_silent_short_read() {
        within("a read of a failing disk", 20, || {
            let mut attached = Attached::of(8);
            attached
                .disk
                .fail
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 512];
            assert_eq!(
                attached.blk.read_block(0, &mut buf).err(),
                Some(Error::IoError)
            );
        });
    }

    #[test]
    fn the_queue_is_reusable_across_many_requests() {
        // Each request takes three descriptors out of sixteen and hands them
        // back, so the free list has to survive being walked over and over.
        within("a hundred reads", 30, || {
            let mut attached = Attached::of(16);
            let mut buf = [0u8; 512];
            for round in 0..100 {
                let sector = round % 16;
                attached.blk.read_block(sector, &mut buf).unwrap();
                assert_eq!(
                    u32::from_le_bytes(buf[0..4].try_into().unwrap()),
                    sector as u32,
                    "round {} read the wrong sector",
                    round
                );
            }
            assert_eq!(attached.disk.served(), 100);
        });
    }

    /// A buffer that starts `offset` bytes into a page, `len` bytes long.
    fn straddling(offset: usize, len: usize) -> (Vec<u8>, usize) {
        let backing = vec![0u8; len + 2 * PAGE_SIZE];
        let base = backing.as_ptr() as usize;
        let start = (PAGE_SIZE - base % PAGE_SIZE) % PAGE_SIZE + offset;
        (backing, start)
    }

    #[test]
    fn a_multi_sector_read_is_one_request_cut_at_the_page() {
        within("a read of eight sectors across a page", 20, || {
            let mut attached = Attached::of(64);
            // 4 KiB starting 512 bytes before a page boundary: two segments
            // on the device's side, one request.
            let (mut backing, start) = straddling(PAGE_SIZE - 512, 4096);
            let buf = &mut backing[start..start + 4096];
            attached.blk.read_blocks(10, buf).unwrap();
            for i in 0..8 {
                assert_eq!(
                    &buf[i * 512..(i + 1) * 512],
                    &attached.disk.sector(10 + i)[..]
                );
            }
            assert_eq!(attached.disk.served(), 1);
        });
    }

    #[test]
    fn a_multi_sector_write_lands_where_it_was_asked() {
        within("a write of eight sectors across a page", 20, || {
            let mut attached = Attached::of(64);
            let (mut backing, start) = straddling(PAGE_SIZE - 1024, 4096);
            let buf = &mut backing[start..start + 4096];
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = (i % 253) as u8;
            }
            attached.blk.write_blocks(20, buf).unwrap();
            for i in 0..8 {
                assert_eq!(
                    &attached.disk.sector(20 + i)[..],
                    &buf[i * 512..(i + 1) * 512]
                );
            }
            assert_eq!(
                u32::from_le_bytes(attached.disk.sector(28)[0..4].try_into().unwrap()),
                28,
                "the sector after the write was touched"
            );
            assert_eq!(attached.disk.served(), 1);
        });
    }

    #[test]
    fn a_read_larger_than_one_request_is_split_and_still_right() {
        within("a read of more than the queue carries", 30, || {
            let sectors = 2 * MAX_REQUEST_BYTES / 512 + 3;
            let mut attached = Attached::of(sectors + 4);
            let len = sectors * 512;
            let (mut backing, start) = straddling(512, len);
            let buf = &mut backing[start..start + len];
            attached.blk.read_blocks(1, buf).unwrap();
            for i in 0..sectors {
                assert_eq!(
                    &buf[i * 512..(i + 1) * 512],
                    &attached.disk.sector(1 + i)[..]
                );
            }
            assert_eq!(attached.disk.served(), 3);
        });
    }

    #[test]
    fn no_segment_crosses_a_page() {
        // The device gets each segment by the physical address of its first
        // byte, and the fake disk maps virtual to physical one to one, so the
        // requests above cannot catch a segment that runs into the next page.
        for offset in [0, 1, 511, 512, PAGE_SIZE - 1] {
            let len = MAX_REQUEST_BYTES;
            let (backing, start) = straddling(offset, len);
            let segments = page_segments(&backing[start..start + len]);
            assert!(
                segments.len() <= MAX_SEGMENTS,
                "{} segments",
                segments.len()
            );
            assert_eq!(segments.iter().map(|s| s.len()).sum::<usize>(), len);
            for segment in &segments {
                let first = segment.as_ptr() as usize / PAGE_SIZE;
                let last = (segment.as_ptr() as usize + segment.len() - 1) / PAGE_SIZE;
                assert_eq!(first, last, "a segment crosses a page at offset {}", offset);
            }
        }
    }

    #[test]
    fn a_multi_sector_buffer_that_is_not_whole_sectors_is_refused() {
        let header = fake_header(2, 256);
        set_config_u64(header, 8);
        let mut blk = VirtIOBlk::new(header).unwrap();
        let mut buf = [0u8; 700];
        assert_eq!(
            blk.read_blocks(0, &mut buf).err(),
            Some(Error::InvalidParam)
        );
        assert_eq!(blk.write_blocks(0, &buf).err(), Some(Error::InvalidParam));
    }

    #[test]
    fn a_failing_disk_fails_a_multi_sector_read() {
        within("a multi-sector read of a failing disk", 20, || {
            let mut attached = Attached::of(16);
            attached
                .disk
                .fail
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let mut buf = [0u8; 2048];
            assert_eq!(
                attached.blk.read_blocks(0, &mut buf).err(),
                Some(Error::IoError)
            );
        });
    }

    #[test]
    fn a_buffer_that_is_not_a_sector_is_refused_by_the_assert() {
        // The one-sector call asserts rather than returning an error; a
        // multi-sector request goes through `read_blocks` instead.
        let header = fake_header(2, 256);
        set_config_u64(header, 8);
        let mut blk = VirtIOBlk::new(header).unwrap();
        let mut buf = [0u8; 1024];
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = blk.read_block(0, &mut buf);
        }));
        assert!(panicked.is_err(), "a two-sector buffer was accepted");
    }

    #[test]
    fn the_driver_finishes_telling_the_device_it_is_ready() {
        let header = fake_header(2, 256);
        set_config_u64(header, 8);
        let blk = VirtIOBlk::new(header).unwrap();
        assert_eq!(
            blk.header.fake_status() & 4,
            4,
            "the device was never told the driver is ready"
        );
    }

    #[test]
    fn an_interrupt_is_acknowledged_once() {
        let header = fake_header(2, 256);
        set_config_u64(header, 8);
        let mut blk = VirtIOBlk::new(header).unwrap();
        assert!(!blk.ack_interrupt(), "an interrupt nobody raised");
        blk.header.fake_raise_interrupt(1);
        assert!(blk.ack_interrupt());
        assert_eq!(blk.header.fake_interrupt_ack(), 1);
    }
}

#[cfg(test)]
mod blk_config_layout_tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    /// `BlkConfig` is the virtio-blk configuration space: the device writes
    /// it and the driver reads it, so every offset here is the ABI. Linux
    /// spells the same fields in the same order in
    /// `include/uapi/linux/virtio_blk.h`, with the three geometry fields
    /// wrapped in a struct of their own:
    ///
    /// ```text
    /// __virtio64 capacity;
    /// __virtio32 size_max;
    /// __virtio32 seg_max;
    /// struct virtio_blk_geometry {
    ///         __virtio16 cylinders;
    ///         __u8 heads;
    ///         __u8 sectors;
    /// } geometry;
    /// __virtio32 blk_size;
    /// __u8 physical_block_exp;
    /// __u8 alignment_offset;
    /// __virtio16 min_io_size;
    /// __virtio32 opt_io_size;
    /// ```
    ///
    /// Flattening that geometry struct, as here, keeps the same bytes: it is
    /// two-byte aligned and sits on a two-byte boundary either way.
    ///
    /// The suite cannot pin any of this, because its tests drive the request
    /// path and never the configuration space: `size_max` against `seg_max`
    /// is the largest segment read as how many of them, `heads` against
    /// `sectors` is the geometry upside down, and `physical_block_exp`
    /// against `alignment_offset` is the physical block size read as the
    /// offset into it. The three swaps survived.
    ///
    /// The size goes in this test rather than its own, because no swap of two
    /// same-width neighbours can falsify it.
    #[test]
    fn the_capacity_comes_first_then_the_two_limits() {
        assert_eq!(size_of::<BlkConfig>(), 32);
        assert_eq!(align_of::<BlkConfig>(), 8);
        assert_eq!(offset_of!(BlkConfig, capacity), 0);
        assert_eq!(offset_of!(BlkConfig, size_max), 8);
        assert_eq!(offset_of!(BlkConfig, seg_max), 12);
    }

    #[test]
    fn the_geometry_is_cylinders_then_heads_then_sectors() {
        assert_eq!(offset_of!(BlkConfig, cylinders), 16);
        assert_eq!(offset_of!(BlkConfig, heads), 18);
        assert_eq!(offset_of!(BlkConfig, sectors), 19);
        assert_eq!(offset_of!(BlkConfig, blk_size), 20);
    }

    #[test]
    fn the_block_size_hints_come_after_the_geometry() {
        assert_eq!(offset_of!(BlkConfig, physical_block_exp), 24);
        assert_eq!(offset_of!(BlkConfig, alignment_offset), 25);
        assert_eq!(offset_of!(BlkConfig, min_io_size), 26);
        assert_eq!(offset_of!(BlkConfig, opt_io_size), 28);
    }
}
