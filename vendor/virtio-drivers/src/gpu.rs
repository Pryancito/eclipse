use super::*;
use crate::queue::VirtQueue;
use bitflags::*;
use core::{fmt, hint::spin_loop};
use log::*;
use volatile::{ReadOnly, Volatile, WriteOnly};

/// A virtio based graphics adapter.
///
/// It can operate in 2D mode and in 3D (virgl) mode.
/// 3D mode will offload rendering ops to the host gpu and therefore requires
/// a gpu with 3D support on the host machine.
/// In 2D mode the virtio-gpu device provides support for ARGB Hardware cursors
/// and multiple scanouts (aka heads).
pub struct VirtIOGpu<'a> {
    header: &'static mut VirtIOHeader,
    rect: Rect,
    /// DMA area of frame buffer.
    frame_buffer_dma: Option<DMA>,
    /// DMA area of cursor image buffer.
    cursor_buffer_dma: Option<DMA>,
    /// Queue for sending control commands.
    control_queue: VirtQueue<'a>,
    /// Queue for sending cursor commands.
    cursor_queue: VirtQueue<'a>,
    /// Queue buffer DMA
    queue_buf_dma: DMA,
    /// Send buffer for queue.
    queue_buf_send: &'a mut [u8],
    /// Recv buffer for queue.
    queue_buf_recv: &'a mut [u8],
}

impl VirtIOGpu<'_> {
    /// Create a new VirtIO-Gpu driver.
    pub fn new(header: &'static mut VirtIOHeader) -> Result<Self> {
        header.begin_init(|features| {
            let features = Features::from_bits_truncate(features);
            info!("Device features {:?}", features);
            let supported_features = Features::empty();
            (features & supported_features).bits()
        });

        // read configuration space
        let config = unsafe { &mut *(header.config_space() as *mut Config) };
        info!("Config: {:?}", config);

        let control_queue = VirtQueue::new(header, QUEUE_TRANSMIT, 2)?;
        let cursor_queue = VirtQueue::new(header, QUEUE_CURSOR, 2)?;

        let queue_buf_dma = DMA::new(2)?;
        let queue_buf_send = unsafe { &mut queue_buf_dma.as_buf()[..PAGE_SIZE] };
        let queue_buf_recv = unsafe { &mut queue_buf_dma.as_buf()[PAGE_SIZE..] };

        header.finish_init();

        Ok(VirtIOGpu {
            header,
            frame_buffer_dma: None,
            cursor_buffer_dma: None,
            rect: Rect::default(),
            control_queue,
            cursor_queue,
            queue_buf_dma,
            queue_buf_send,
            queue_buf_recv,
        })
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> bool {
        self.header.ack_interrupt()
    }

    /// Get the resolution (width, height).
    pub fn resolution(&self) -> (u32, u32) {
        (self.rect.width, self.rect.height)
    }

    /// Setup framebuffer
    pub fn setup_framebuffer(&mut self) -> Result<&mut [u8]> {
        // get display info
        let display_info = self.get_display_info()?;
        info!("=> {:?}", display_info);

        // The rectangle is the DEVICE's word, and everything downstream sizes
        // its writes from it: `resolution()` feeds `DisplayInfo`, whose `pitch`
        // and `fb_size` the console, `/dev/fb0` and the compositor all trust.
        // `width * height * 4` used to be worked out in `u32`, so a device
        // reporting a large mode overflowed it -- a panic in debug, and in
        // release a framebuffer allocated from the wrapped-around size while
        // the resolution stayed enormous, which is a buffer smaller than
        // everyone who writes into it believes.
        let size = display_info
            .rect
            .width
            .checked_mul(display_info.rect.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| {
                warn!(
                    "[virtio-gpu] device reports a {}x{} scanout, which has no framebuffer",
                    display_info.rect.width, display_info.rect.height
                );
                Error::InvalidParam
            })?;
        self.rect = display_info.rect;

        // create resource 2d
        self.resource_create_2d(
            RESOURCE_ID_FB,
            display_info.rect.width,
            display_info.rect.height,
        )?;

        // alloc continuous pages for the frame buffer
        let frame_buffer_dma = DMA::new(pages(size as usize))?;

        // resource_attach_backing
        self.resource_attach_backing(RESOURCE_ID_FB, frame_buffer_dma.paddr() as u64, size)?;

        // map frame buffer to screen
        self.set_scanout(display_info.rect, SCANOUT_ID, RESOURCE_ID_FB)?;

        let buf = unsafe { frame_buffer_dma.as_buf() };
        self.frame_buffer_dma = Some(frame_buffer_dma);
        Ok(buf)
    }

    /// Flush framebuffer to screen.
    pub fn flush(&mut self) -> Result {
        // copy data from guest to host
        self.transfer_to_host_2d(self.rect, 0, RESOURCE_ID_FB)?;
        // flush data to screen
        self.resource_flush(self.rect, RESOURCE_ID_FB)?;
        Ok(())
    }

    /// Flush only a damage rectangle to the host (guest → host transfer +
    /// resource flush). Coordinates are in framebuffer pixels; clamped to the
    /// scanout. Empty/degenerate rects are no-ops.
    pub fn flush_region(&mut self, x: u32, y: u32, width: u32, height: u32) -> Result {
        let x2 = x.saturating_add(width).min(self.rect.width);
        let y2 = y.saturating_add(height).min(self.rect.height);
        // These two are redundant for the outcome, and honestly so: an origin
        // outside the scanout gives `x2 <= x` below and the flush is dropped
        // either way, so a mutant that removes them survives every test here.
        // They are kept because the rectangle that goes out is then built from
        // numbers that are all inside the screen.
        let x = x.min(self.rect.width);
        let y = y.min(self.rect.height);
        if x2 <= x || y2 <= y {
            return Ok(());
        }
        let rect = Rect {
            x,
            y,
            width: x2 - x,
            height: y2 - y,
        };
        let stride = (self.rect.width as u64) * 4;
        let offset = (y as u64) * stride + (x as u64) * 4;
        self.transfer_to_host_2d(rect, offset, RESOURCE_ID_FB)?;
        self.resource_flush(rect, RESOURCE_ID_FB)?;
        Ok(())
    }

    /// Set the pointer shape and position.
    pub fn setup_cursor(
        &mut self,
        cursor_image: &[u8],
        pos_x: u32,
        pos_y: u32,
        hot_x: u32,
        hot_y: u32,
    ) -> Result {
        let size = CURSOR_RECT.width * CURSOR_RECT.height * 4;
        if cursor_image.len() != size as usize {
            return Err(Error::InvalidParam);
        }
        let cursor_buffer_dma = DMA::new(pages(size as usize))?;
        let buf = unsafe { cursor_buffer_dma.as_buf() };
        buf.copy_from_slice(cursor_image);

        self.resource_create_2d(RESOURCE_ID_CURSOR, CURSOR_RECT.width, CURSOR_RECT.height)?;
        self.resource_attach_backing(RESOURCE_ID_CURSOR, cursor_buffer_dma.paddr() as u64, size)?;
        self.transfer_to_host_2d(CURSOR_RECT, 0, RESOURCE_ID_CURSOR)?;
        self.update_cursor(
            RESOURCE_ID_CURSOR,
            SCANOUT_ID,
            pos_x,
            pos_y,
            hot_x,
            hot_y,
            false,
        )?;
        self.cursor_buffer_dma = Some(cursor_buffer_dma);
        Ok(())
    }

    /// Move the pointer without updating the shape.
    pub fn move_cursor(&mut self, pos_x: u32, pos_y: u32) -> Result {
        self.update_cursor(RESOURCE_ID_CURSOR, SCANOUT_ID, pos_x, pos_y, 0, 0, true)?;
        Ok(())
    }
}

impl VirtIOGpu<'_> {
    /// Send a request to the device and block for a response.
    fn request<Req, Rsp>(&mut self, req: Req) -> Result<Rsp> {
        // The two buffers are the two halves of one DMA block, so a request
        // bigger than a page would write over the response buffer it is about
        // to read back. Every command in this file fits many times over; this
        // is here so that adding one that does not is an error rather than a
        // quiet corruption.
        if size_of::<Req>() > self.queue_buf_send.len()
            || size_of::<Rsp>() > self.queue_buf_recv.len()
        {
            warn!(
                "[virtio-gpu] a {}-byte request or a {}-byte response does not fit the \
                 {}-byte queue buffers",
                size_of::<Req>(),
                size_of::<Rsp>(),
                self.queue_buf_send.len()
            );
            return Err(Error::InvalidParam);
        }
        unsafe {
            (self.queue_buf_send.as_mut_ptr() as *mut Req).write(req);
        }
        self.control_queue
            .add(&[self.queue_buf_send], &[self.queue_buf_recv])?;
        self.header.notify(QUEUE_TRANSMIT as u32);
        while !self.control_queue.can_pop() {
            spin_loop();
        }
        self.control_queue.pop_used()?;
        Ok(unsafe { (self.queue_buf_recv.as_ptr() as *const Rsp).read() })
    }

    /// Send a mouse cursor operation request to the device and block for a response.
    fn cursor_request<Req>(&mut self, req: Req) -> Result {
        if size_of::<Req>() > self.queue_buf_send.len() {
            warn!(
                "[virtio-gpu] a {}-byte cursor request does not fit the {}-byte queue buffer",
                size_of::<Req>(),
                self.queue_buf_send.len()
            );
            return Err(Error::InvalidParam);
        }
        unsafe {
            (self.queue_buf_send.as_mut_ptr() as *mut Req).write(req);
        }
        self.cursor_queue.add(&[self.queue_buf_send], &[])?;
        self.header.notify(QUEUE_CURSOR as u32);
        while !self.cursor_queue.can_pop() {
            spin_loop();
        }
        self.cursor_queue.pop_used()?;
        Ok(())
    }

    fn get_display_info(&mut self) -> Result<RespDisplayInfo> {
        let info: RespDisplayInfo = self.request(CtrlHeader::with_type(Command::GetDisplayInfo))?;
        info.header.check_type(Command::OkDisplayInfo)?;
        Ok(info)
    }

    fn resource_create_2d(&mut self, resource_id: u32, width: u32, height: u32) -> Result {
        let rsp: CtrlHeader = self.request(ResourceCreate2D {
            header: CtrlHeader::with_type(Command::ResourceCreate2d),
            resource_id,
            format: Format::B8G8R8A8UNORM,
            width,
            height,
        })?;
        rsp.check_type(Command::OkNodata)
    }

    fn set_scanout(&mut self, rect: Rect, scanout_id: u32, resource_id: u32) -> Result {
        let rsp: CtrlHeader = self.request(SetScanout {
            header: CtrlHeader::with_type(Command::SetScanout),
            rect,
            scanout_id,
            resource_id,
        })?;
        rsp.check_type(Command::OkNodata)
    }

    fn resource_flush(&mut self, rect: Rect, resource_id: u32) -> Result {
        let rsp: CtrlHeader = self.request(ResourceFlush {
            header: CtrlHeader::with_type(Command::ResourceFlush),
            rect,
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OkNodata)
    }

    fn transfer_to_host_2d(&mut self, rect: Rect, offset: u64, resource_id: u32) -> Result {
        let rsp: CtrlHeader = self.request(TransferToHost2D {
            header: CtrlHeader::with_type(Command::TransferToHost2d),
            rect,
            offset,
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OkNodata)
    }

    fn resource_attach_backing(&mut self, resource_id: u32, paddr: u64, length: u32) -> Result {
        let rsp: CtrlHeader = self.request(ResourceAttachBacking {
            header: CtrlHeader::with_type(Command::ResourceAttachBacking),
            resource_id,
            nr_entries: 1,
            addr: paddr,
            length,
            _padding: 0,
        })?;
        rsp.check_type(Command::OkNodata)
    }

    fn update_cursor(
        &mut self,
        resource_id: u32,
        scanout_id: u32,
        pos_x: u32,
        pos_y: u32,
        hot_x: u32,
        hot_y: u32,
        is_move: bool,
    ) -> Result {
        self.cursor_request(UpdateCursor {
            header: if is_move {
                CtrlHeader::with_type(Command::MoveCursor)
            } else {
                CtrlHeader::with_type(Command::UpdateCursor)
            },
            pos: CursorPos {
                scanout_id,
                x: pos_x,
                y: pos_y,
                _padding: 0,
            },
            resource_id,
            hot_x,
            hot_y,
            _padding: 0,
        })
    }
}

#[repr(C)]
struct Config {
    /// Signals pending events to the driver。
    events_read: ReadOnly<u32>,

    /// Clears pending events in the device.
    events_clear: WriteOnly<u32>,

    /// Specifies the maximum number of scanouts supported by the device.
    ///
    /// Minimum value is 1, maximum value is 16.
    num_scanouts: Volatile<u32>,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Config")
            .field("events_read", &self.events_read)
            .field("num_scanouts", &self.num_scanouts)
            .finish()
    }
}

/// Display configuration has changed.
const EVENT_DISPLAY: u32 = 1 << 0;

bitflags! {
    struct Features: u64 {
        /// virgl 3D mode is supported.
        const VIRGL                 = 1 << 0;
        /// EDID is supported.
        const EDID                  = 1 << 1;

        // device independent
        const NOTIFY_ON_EMPTY       = 1 << 24; // legacy
        const ANY_LAYOUT            = 1 << 27; // legacy
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30; // legacy
        const VERSION_1             = 1 << 32; // detect legacy

        // since virtio v1.1
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    GetDisplayInfo = 0x100,
    ResourceCreate2d = 0x101,
    ResourceUnref = 0x102,
    SetScanout = 0x103,
    ResourceFlush = 0x104,
    TransferToHost2d = 0x105,
    ResourceAttachBacking = 0x106,
    ResourceDetachBacking = 0x107,
    GetCapsetInfo = 0x108,
    GetCapset = 0x109,
    GetEdid = 0x10a,

    UpdateCursor = 0x300,
    MoveCursor = 0x301,

    OkNodata = 0x1100,
    OkDisplayInfo = 0x1101,
    OkCapsetInfo = 0x1102,
    OkCapset = 0x1103,
    OkEdid = 0x1104,

    ErrUnspec = 0x1200,
    ErrOutOfMemory = 0x1201,
    ErrInvalidScanoutId = 0x1202,
    ErrInvalidResourceId = 0x1203,
    ErrInvalidContextId = 0x1204,
    ErrInvalidParameter = 0x1205,
}

/// Name a response code for the log, without pretending it is a [`Command`].
///
/// `ERR_INVALID_PARAMETER` is the one a driver really meets: any rectangle the
/// host does not like earns it, and until now it was neither named nor
/// representable.
fn describe_response(code: u32) -> &'static str {
    match code {
        0x1100 => "OK_NODATA",
        0x1101 => "OK_DISPLAY_INFO",
        0x1102 => "OK_CAPSET_INFO",
        0x1103 => "OK_CAPSET",
        0x1104 => "OK_EDID",
        0x1200 => "ERR_UNSPEC",
        0x1201 => "ERR_OUT_OF_MEMORY",
        0x1202 => "ERR_INVALID_SCANOUT_ID",
        0x1203 => "ERR_INVALID_RESOURCE_ID",
        0x1204 => "ERR_INVALID_CONTEXT_ID",
        0x1205 => "ERR_INVALID_PARAMETER",
        _ => "a code no version of the specification defines",
    }
}

const GPU_FLAG_FENCE: u32 = 1 << 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CtrlHeader {
    /// The command or response code, as a plain number.
    ///
    /// **Not** a [`Command`]. On a response this field is bytes the device
    /// wrote, and a `#[repr(u32)]` enum may only ever hold one of its
    /// discriminants: reading one out of device memory is undefined behaviour
    /// for every other value, and the device has 2^32 of them to choose from.
    /// Two of those are reachable without a hostile device at all -- the three
    /// `ERR_INVALID_*` codes below, which the enum never listed, and an
    /// uninitialised response buffer, which is what a recycled DMA frame is.
    hdr_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    _padding: u32,
}

impl CtrlHeader {
    fn with_type(hdr_type: Command) -> CtrlHeader {
        CtrlHeader {
            hdr_type: hdr_type as u32,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            _padding: 0,
        }
    }

    /// Return error if the type is not same as expected.
    fn check_type(&self, expected: Command) -> Result {
        if self.hdr_type == expected as u32 {
            Ok(())
        } else {
            warn!(
                "[virtio-gpu] device answered {} where {:?} ({}) was expected",
                describe_response(self.hdr_type),
                expected,
                expected as u32
            );
            Err(Error::IoError)
        }
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug)]
struct RespDisplayInfo {
    header: CtrlHeader,
    rect: Rect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug)]
struct ResourceCreate2D {
    header: CtrlHeader,
    resource_id: u32,
    format: Format,
    width: u32,
    height: u32,
}

#[repr(u32)]
#[derive(Debug)]
enum Format {
    B8G8R8A8UNORM = 1,
}

#[repr(C)]
#[derive(Debug)]
struct ResourceAttachBacking {
    header: CtrlHeader,
    resource_id: u32,
    nr_entries: u32, // always 1
    addr: u64,
    length: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug)]
struct SetScanout {
    header: CtrlHeader,
    rect: Rect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Debug)]
struct TransferToHost2D {
    header: CtrlHeader,
    rect: Rect,
    offset: u64,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug)]
struct ResourceFlush {
    header: CtrlHeader,
    rect: Rect,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CursorPos {
    scanout_id: u32,
    x: u32,
    y: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct UpdateCursor {
    header: CtrlHeader,
    pos: CursorPos,
    resource_id: u32,
    hot_x: u32,
    hot_y: u32,
    _padding: u32,
}

const QUEUE_TRANSMIT: usize = 0;
const QUEUE_CURSOR: usize = 1;

const SCANOUT_ID: u32 = 0;
const RESOURCE_ID_FB: u32 = 0xbabe;
const RESOURCE_ID_CURSOR: u32 = 0xdade;

const CURSOR_RECT: Rect = Rect {
    x: 0,
    y: 0,
    width: 64,
    height: 64,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dev::{fake_header, Ring};
    use core::convert::TryInto;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    /// virtio-gpu, from the device-id table (5.7: GPU device).
    const DEVICE_ID_GPU: u32 = 16;

    /// The queue size `new` asks for.
    const QUEUE_SIZE: u16 = 2;

    /// A driver over a fake device, plus the device's side of both queues.
    ///
    /// `VirtIOGpu::new` cannot be called: it builds two queues back to back, and
    /// in the fake header one memory cell stands in for the per-queue `QueuePFN`
    /// register, so the second queue would be told the first one's address and
    /// refused with `AlreadyUsed`. What follows is `new`'s body with
    /// `fake_forget_queue_pfn` in between.
    fn driver() -> (VirtIOGpu<'static>, Ring, Ring) {
        let header = fake_header(DEVICE_ID_GPU, 16);
        header.begin_init(|_| 0);
        let control_queue = VirtQueue::new(header, QUEUE_TRANSMIT, QUEUE_SIZE)
            .expect("the control queue was refused");
        // Each ring has to be found while the single PFN cell still holds its
        // address, so the control ring is taken before the cursor queue exists.
        let control_ring = Ring::of(header, QUEUE_TRANSMIT as u32, QUEUE_SIZE);
        header.fake_forget_queue_pfn();
        let cursor_queue =
            VirtQueue::new(header, QUEUE_CURSOR, QUEUE_SIZE).expect("the cursor queue was refused");
        let cursor_ring = Ring::of(header, QUEUE_CURSOR as u32, QUEUE_SIZE);
        let queue_buf_dma = DMA::new(2).expect("no DMA for the queue buffers");
        let queue_buf_send = unsafe { &mut queue_buf_dma.as_buf()[..PAGE_SIZE] };
        let queue_buf_recv = unsafe { &mut queue_buf_dma.as_buf()[PAGE_SIZE..] };
        header.finish_init();
        (
            VirtIOGpu {
                header,
                frame_buffer_dma: None,
                cursor_buffer_dma: None,
                rect: Rect::default(),
                control_queue,
                cursor_queue,
                queue_buf_dma,
                queue_buf_send,
                queue_buf_recv,
            },
            control_ring,
            cursor_ring,
        )
    }

    /// One request as the device saw it: its type and its bytes.
    #[derive(Clone)]
    struct Seen {
        hdr_type: u32,
        bytes: Vec<u8>,
    }

    impl Seen {
        fn u32_at(&self, offset: usize) -> u32 {
            u32::from_le_bytes(self.bytes[offset..offset + 4].try_into().unwrap())
        }

        fn u64_at(&self, offset: usize) -> u64 {
            u64::from_le_bytes(self.bytes[offset..offset + 8].try_into().unwrap())
        }

        /// The rectangle that follows the 24-byte header.
        fn rect(&self) -> (u32, u32, u32, u32) {
            (
                self.u32_at(24),
                self.u32_at(28),
                self.u32_at(32),
                self.u32_at(36),
            )
        }
    }

    /// A virtio-gpu made of ordinary memory: it answers control requests with
    /// the display it was told to have, and records what it was asked.
    ///
    /// It judges nothing. `request` spins on `can_pop`, so an assert in this
    /// thread would leave the driver spinning and hang the suite without naming
    /// a test; every judgement is made on the test thread after `stop`.
    struct Screen {
        width: u32,
        height: u32,
        /// What to answer instead of the correct code, once, if set.
        wrong_answer: AtomicU32,
        /// Answer with nothing at all: complete the chain and write no bytes,
        /// which leaves whatever the DMA frame came with.
        answer_nothing: AtomicBool,
        seen: Mutex<Vec<Seen>>,
        stop: AtomicBool,
    }

    const NO_WRONG_ANSWER: u32 = u32::MAX;

    impl Screen {
        fn of(width: u32, height: u32) -> Arc<Self> {
            Arc::new(Screen {
                width,
                height,
                wrong_answer: AtomicU32::new(NO_WRONG_ANSWER),
                answer_nothing: AtomicBool::new(false),
                seen: Mutex::new(Vec::new()),
                stop: AtomicBool::new(false),
            })
        }

        fn requests(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn stop(&self) {
            self.stop.store(true, Ordering::SeqCst);
        }

        /// Serve both rings on this thread until [`stop`](Self::stop).
        fn serve(self: &Arc<Self>, control: Ring, cursor: Ring) {
            let mut control_seen: u16 = 0;
            let mut cursor_seen: u16 = 0;
            while !self.stop.load(Ordering::SeqCst) {
                while control_seen != control.avail_idx() {
                    let head = control.avail_entry(control_seen);
                    self.serve_one(&control, head, true);
                    control_seen = control_seen.wrapping_add(1);
                }
                while cursor_seen != cursor.avail_idx() {
                    let head = cursor.avail_entry(cursor_seen);
                    self.serve_one(&cursor, head, false);
                    cursor_seen = cursor_seen.wrapping_add(1);
                }
                std::thread::yield_now();
            }
        }

        fn serve_one(&self, ring: &Ring, head: u16, control: bool) {
            let chain = ring.chain(head);
            let (req_at, req_len, _) = chain[0];
            let bytes = unsafe { core::slice::from_raw_parts(req_at as *const u8, req_len) };
            let hdr_type = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
            self.seen.lock().unwrap().push(Seen {
                hdr_type,
                bytes: bytes.to_vec(),
            });
            if !control {
                // The cursor queue takes no response at all.
                ring.complete(head, 0);
                return;
            }
            if self.answer_nothing.load(Ordering::SeqCst) {
                ring.complete(head, 0);
                return;
            }
            let wrong = self.wrong_answer.swap(NO_WRONG_ANSWER, Ordering::SeqCst);
            let mut response = Vec::new();
            let code = if wrong != NO_WRONG_ANSWER {
                wrong
            } else if hdr_type == Command::GetDisplayInfo as u32 {
                Command::OkDisplayInfo as u32
            } else {
                Command::OkNodata as u32
            };
            // The 24-byte control header: type, flags, fence id, context id, pad.
            response.extend_from_slice(&code.to_le_bytes());
            response.extend_from_slice(&0u32.to_le_bytes());
            response.extend_from_slice(&0u64.to_le_bytes());
            response.extend_from_slice(&0u32.to_le_bytes());
            response.extend_from_slice(&0u32.to_le_bytes());
            if code == Command::OkDisplayInfo as u32 {
                // One `virtio_gpu_display_one`: the rectangle, enabled, flags.
                // The specification carries sixteen; the driver reads the first,
                // which is the one scanout it ever uses.
                for value in [0u32, 0, self.width, self.height, 1, 0] {
                    response.extend_from_slice(&value.to_le_bytes());
                }
            }
            let written = ring.fill(head, &response);
            ring.complete(head, written);
        }
    }

    /// Run `body` with a device serving both queues on another thread.
    fn with_screen<R>(
        screen: &Arc<Screen>,
        control: Ring,
        cursor: Ring,
        body: impl FnOnce() -> R,
    ) -> R {
        let device = {
            let screen = Arc::clone(screen);
            std::thread::spawn(move || screen.serve(control, cursor))
        };
        let out = body();
        screen.stop();
        device.join().expect("the device thread panicked");
        out
    }

    #[test]
    fn the_config_space_is_the_layout_the_specification_describes() {
        // 5.7.4: events_read at 0, events_clear at 4, num_scanouts at 8.
        use core::mem::MaybeUninit;
        let config = MaybeUninit::<Config>::uninit();
        let base = config.as_ptr() as usize;
        let c = config.as_ptr();
        unsafe {
            assert_eq!(core::ptr::addr_of!((*c).events_read) as usize - base, 0);
            assert_eq!(core::ptr::addr_of!((*c).events_clear) as usize - base, 4);
            assert_eq!(core::ptr::addr_of!((*c).num_scanouts) as usize - base, 8);
        }
    }

    #[test]
    fn the_control_header_is_the_twenty_four_bytes_the_specification_describes() {
        // 5.7.6.7. Every request and response in this file begins with it, so a
        // field inserted here shifts every field the device reads and writes.
        use core::mem::MaybeUninit;
        let header = MaybeUninit::<CtrlHeader>::uninit();
        let base = header.as_ptr() as usize;
        let h = header.as_ptr();
        unsafe {
            assert_eq!(core::ptr::addr_of!((*h).hdr_type) as usize - base, 0);
            assert_eq!(core::ptr::addr_of!((*h).flags) as usize - base, 4);
            assert_eq!(core::ptr::addr_of!((*h).fence_id) as usize - base, 8);
            assert_eq!(core::ptr::addr_of!((*h).ctx_id) as usize - base, 16);
        }
        assert_eq!(size_of::<CtrlHeader>(), 24);
        assert_eq!(size_of::<Rect>(), 16, "a rectangle is four u32");
        assert_eq!(
            size_of::<RespDisplayInfo>(),
            24 + 16 + 4 + 4,
            "the display info response is a header and one scanout"
        );
    }

    #[test]
    fn the_framebuffer_setup_is_the_sequence_the_specification_requires() {
        // Four requests, in this order, and each one's fields matter: a resource
        // created at the wrong size, or a scanout pointed at the wrong resource,
        // leaves a screen that is simply black.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(640, 480);
        let fb_len = with_screen(&screen, control, cursor, || {
            gpu.setup_framebuffer().expect("setup failed").len()
        });
        let seen = screen.requests();
        let types: Vec<u32> = seen.iter().map(|s| s.hdr_type).collect();
        assert_eq!(
            types,
            vec![
                Command::GetDisplayInfo as u32,
                Command::ResourceCreate2d as u32,
                Command::ResourceAttachBacking as u32,
                Command::SetScanout as u32,
            ]
        );
        // resource_create_2d: id, format, width, height after the header.
        assert_eq!(seen[1].u32_at(24), RESOURCE_ID_FB);
        assert_eq!(seen[1].u32_at(28), Format::B8G8R8A8UNORM as u32);
        assert_eq!((seen[1].u32_at(32), seen[1].u32_at(36)), (640, 480));
        // resource_attach_backing: id, entries, address, length.
        assert_eq!(seen[2].u32_at(24), RESOURCE_ID_FB);
        assert_eq!(seen[2].u32_at(28), 1, "one backing entry");
        assert_ne!(seen[2].u64_at(32), 0, "the backing address is null");
        assert_eq!(seen[2].u32_at(40), 640 * 480 * 4);
        // set_scanout: the whole rectangle, then scanout id and resource id.
        assert_eq!(seen[3].rect(), (0, 0, 640, 480));
        assert_eq!(seen[3].u32_at(40), SCANOUT_ID);
        assert_eq!(seen[3].u32_at(44), RESOURCE_ID_FB);
        assert_eq!(gpu.resolution(), (640, 480));
        assert_eq!(
            fb_len,
            pages(640 * 480 * 4) * PAGE_SIZE,
            "the framebuffer is not the pages the resolution needs"
        );
        assert!(fb_len >= 640 * 480 * 4, "the framebuffer is short");
    }

    #[test]
    fn a_scanout_whose_framebuffer_does_not_fit_in_a_u32_is_refused() {
        // The rectangle is the device's word. `width * height * 4` used to be
        // worked out in `u32`: at 65536 square that overflows, so in release the
        // framebuffer came out of a wrapped-around size while `resolution()`
        // kept reporting 65536x65536 -- and `pitch` and `fb_size` go straight
        // into `DisplayInfo` for the compositor to write through.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(65536, 65536);
        let result = with_screen(&screen, control, cursor, || gpu.setup_framebuffer().err());
        assert_eq!(result, Some(Error::InvalidParam));
        assert_eq!(
            gpu.resolution(),
            (0, 0),
            "a refused scanout was still adopted as the resolution"
        );
        // Only the display info went out: nothing was created or scanned out.
        assert_eq!(screen.requests().len(), 1);
    }

    #[test]
    fn a_scanout_one_pixel_row_too_tall_to_measure_is_refused() {
        // The boundary of the same multiplication: 2^30 pixels is the most that
        // can be counted in bytes, so one more must be refused rather than
        // wrapped.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(1 << 15, (1 << 15) + 1);
        let result = with_screen(&screen, control, cursor, || gpu.setup_framebuffer().err());
        assert_eq!(result, Some(Error::InvalidParam));
    }

    #[test]
    fn a_scanout_with_no_pixels_is_refused_before_a_resource_is_created() {
        // A zero allocates no pages, and `DMA::new` refuses it with the same
        // error the driver gives -- so the error alone proves nothing. What the
        // check is for is everything that happens first: without it the driver
        // asks the host to create a 0x480 resource and only then finds out, and
        // the host is left holding a resource for a screen that cannot exist.
        for (width, height) in [(0, 480), (640, 0), (0, 0)] {
            let (mut gpu, control, cursor) = driver();
            let screen = Screen::of(width, height);
            let result = with_screen(&screen, control, cursor, || gpu.setup_framebuffer().err());
            assert_eq!(
                result,
                Some(Error::InvalidParam),
                "a {}x{} scanout was accepted",
                width,
                height
            );
            assert_eq!(
                screen.requests().len(),
                1,
                "a {}x{} scanout was carried on past the display info",
                width,
                height
            );
        }
    }

    #[test]
    fn a_request_too_big_for_the_queue_buffer_is_refused_before_it_is_written() {
        // The send and receive buffers are the two halves of one DMA block, so a
        // request bigger than a page would be written over the response buffer
        // it is about to read back. Every command in this file fits many times
        // over, so this is reached only by adding one that does not -- which is
        // why it is asserted here directly rather than through a command.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        with_screen(&screen, control, cursor, || {
            let huge = [0u8; PAGE_SIZE + 1];
            assert_eq!(
                gpu.request::<_, CtrlHeader>(huge).err(),
                Some(Error::InvalidParam),
                "a request larger than the queue buffer was written into it"
            );
            assert_eq!(
                gpu.cursor_request(huge).err(),
                Some(Error::InvalidParam),
                "a cursor request larger than the queue buffer was written into it"
            );
            // And the response side of the same buffer.
            assert_eq!(
                gpu.request::<CtrlHeader, [u8; PAGE_SIZE + 1]>(CtrlHeader::with_type(
                    Command::GetDisplayInfo
                ))
                .err(),
                Some(Error::InvalidParam),
                "a response larger than the queue buffer was read out of it"
            );
        });
        assert!(
            screen.requests().is_empty(),
            "a refused request still reached the device"
        );
    }

    #[test]
    fn a_response_code_no_enum_lists_is_an_error_and_not_a_command() {
        // `ERR_INVALID_PARAMETER` is 0x1205, which the `Command` enum never
        // listed, and the response header used to BE that enum -- read straight
        // out of the bytes the device wrote. A `#[repr(u32)]` enum may only hold
        // one of its discriminants, so that read was undefined behaviour for
        // every other value, and this is one the host sends for any rectangle it
        // does not like.
        for code in [0x1205u32, 0x1203, 0x1204, 0xdead_beef, 0] {
            let (mut gpu, control, cursor) = driver();
            let screen = Screen::of(640, 480);
            screen.wrong_answer.store(code, Ordering::SeqCst);
            let result = with_screen(&screen, control, cursor, || gpu.setup_framebuffer().err());
            assert_eq!(
                result,
                Some(Error::IoError),
                "the code {:#x} was not reported as an error",
                code
            );
        }
    }

    #[test]
    fn a_response_the_device_never_wrote_is_an_error_and_not_a_command() {
        // The other half of the same bug: the response buffer is a DMA frame,
        // and the kernel's allocator does not clear the frames it recycles, so
        // a device that completes the chain without writing leaves whatever was
        // there. The test arena poisons its blocks for exactly this.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(640, 480);
        screen.answer_nothing.store(true, Ordering::SeqCst);
        let result = with_screen(&screen, control, cursor, || gpu.setup_framebuffer().err());
        assert_eq!(result, Some(Error::IoError));
    }

    #[test]
    fn a_whole_flush_transfers_the_whole_scanout_from_its_start() {
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        with_screen(&screen, control, cursor, || {
            gpu.setup_framebuffer().expect("setup failed");
            gpu.flush().expect("flush failed");
        });
        let seen = screen.requests();
        let flush: Vec<&Seen> = seen
            .iter()
            .filter(|s| {
                s.hdr_type == Command::TransferToHost2d as u32
                    || s.hdr_type == Command::ResourceFlush as u32
            })
            .collect();
        assert_eq!(flush.len(), 2, "a flush is a transfer and a flush");
        assert_eq!(flush[0].rect(), (0, 0, 320, 200));
        assert_eq!(flush[0].u64_at(40), 0, "the whole scanout starts at zero");
        assert_eq!(flush[1].rect(), (0, 0, 320, 200));
    }

    #[test]
    fn a_damage_rectangle_transfers_from_where_it_starts_in_the_framebuffer() {
        // The offset is where the host reads the guest's pixels from, so getting
        // it wrong paints the right area of the screen with the wrong part of
        // the framebuffer -- a smear rather than a blank.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        with_screen(&screen, control, cursor, || {
            gpu.setup_framebuffer().expect("setup failed");
            gpu.flush_region(16, 8, 32, 4).expect("flush_region failed");
        });
        let seen = screen.requests();
        let transfer = seen
            .iter()
            .find(|s| s.hdr_type == Command::TransferToHost2d as u32)
            .expect("no transfer went out");
        assert_eq!(transfer.rect(), (16, 8, 32, 4));
        assert_eq!(transfer.u64_at(40), 8 * 320 * 4 + 16 * 4);
    }

    #[test]
    fn a_damage_rectangle_is_cut_to_the_scanout() {
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        with_screen(&screen, control, cursor, || {
            gpu.setup_framebuffer().expect("setup failed");
            gpu.flush_region(300, 190, 1000, 1000)
                .expect("flush_region failed");
        });
        let seen = screen.requests();
        let transfer = seen
            .iter()
            .find(|s| s.hdr_type == Command::TransferToHost2d as u32)
            .expect("no transfer went out");
        assert_eq!(transfer.rect(), (300, 190, 20, 10));
    }

    #[test]
    fn a_damage_rectangle_outside_the_scanout_sends_nothing() {
        // Sending it would earn an `ERR_INVALID_PARAMETER` per frame, which is
        // the code that used to be undefined behaviour to read.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        with_screen(&screen, control, cursor, || {
            gpu.setup_framebuffer().expect("setup failed");
            for (x, y, w, h) in [
                (400, 0, 10, 10),
                (0, 300, 10, 10),
                (0, 0, 0, 10),
                (0, 0, 10, 0),
            ] {
                gpu.flush_region(x, y, w, h).expect("flush_region failed");
            }
        });
        let seen = screen.requests();
        assert_eq!(
            seen.len(),
            4,
            "a rectangle outside the scanout still went to the device"
        );
    }

    #[test]
    fn a_cursor_image_of_the_wrong_size_is_refused_before_anything_is_allocated() {
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        let size = (CURSOR_RECT.width * CURSOR_RECT.height * 4) as usize;
        with_screen(&screen, control, cursor, || {
            for wrong in [size - 1, size + 1, 0] {
                assert_eq!(
                    gpu.setup_cursor(&vec![0u8; wrong], 0, 0, 0, 0).err(),
                    Some(Error::InvalidParam),
                    "a cursor of {} bytes was accepted where {} is the shape",
                    wrong,
                    size
                );
            }
        });
        assert!(
            screen.requests().is_empty(),
            "a refused cursor still spoke to the device"
        );
    }

    #[test]
    fn moving_the_cursor_goes_out_on_the_cursor_queue() {
        // The cursor queue exists so a pointer move does not queue behind a
        // frame on the control queue. A move sent on the control queue would
        // work and drag.
        let (mut gpu, control, cursor) = driver();
        let screen = Screen::of(320, 200);
        with_screen(&screen, control, cursor, || {
            gpu.move_cursor(17, 23).expect("move_cursor failed");
        });
        let seen = screen.requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].hdr_type, Command::MoveCursor as u32);
        // The cursor position follows the header: scanout id, x, y, pad.
        assert_eq!(seen[0].u32_at(24), SCANOUT_ID);
        assert_eq!((seen[0].u32_at(28), seen[0].u32_at(32)), (17, 23));
        assert_eq!(
            gpu.header.fake_notified(),
            QUEUE_CURSOR as u32,
            "the move was notified on the wrong queue"
        );
    }
}
