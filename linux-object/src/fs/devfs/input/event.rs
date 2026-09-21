use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use core::task::{Context, Poll};
use core::{any::Any, future::Future, mem::size_of, pin::Pin};

use kernel_hal::sync::Mutex;

use kernel_hal::drivers::prelude::{CapabilityType, InputCapability, InputEvent, InputEventType};
use kernel_hal::drivers::scheme::InputScheme;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;
use zcore_drivers::input::input_event_codes::syn::SYN_REPORT;

use crate::time::TimeVal;

const BUF_CAPACITY: usize = 64;

const EVENT_DEV_MINOR_BASE: usize = 0x40;

/// The event structure itself
#[repr(C)]
struct TimedInputEvent {
    time: TimeVal,
    event_type: InputEventType,
    code: u16,
    value: i32,
}

struct EventDevInner {
    buf: VecDeque<TimedInputEvent>,
    /// Timestamp shared by every event of the frame being assembled (up to
    /// and including its `SYN_REPORT`). Linux stamps a whole evdev frame with
    /// one time; libinput relies on that and treats a timestamp change
    /// BEFORE the `SYN_REPORT` as a missing frame boundary — stamping each
    /// event individually produced `kernel bug: event frame missing
    /// SYN_REPORT, forcing frame` on real hardware whenever a multi-event
    /// report straddled a clock tick.
    frame_time: Option<TimeVal>,
}

/// Event char device, giving access to raw input device events.
pub struct EventDev {
    id: usize,
    inode_id: usize,
    input: Arc<dyn InputScheme>,
    inner: Arc<Mutex<EventDevInner>>,
}

impl TimedInputEvent {
    /// `time` must be CLOCK_MONOTONIC (libinput sets that via EVIOCSCLOCKID
    /// and times its filters against it); the wall clock would desync
    /// libinput's button-debounce/tap/scroll timers.
    pub fn with_time(e: &InputEvent, time: TimeVal) -> Self {
        TimedInputEvent {
            time,
            event_type: e.event_type,
            code: e.code,
            value: e.value,
        }
    }

    #[allow(unsafe_code)]
    pub fn as_buf(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const _ as _, size_of::<TimedInputEvent>()) }
    }
}

impl EventDevInner {
    fn read_at(&mut self, buf: &mut [u8]) -> Result<usize> {
        let event_size = size_of::<TimedInputEvent>();
        if buf.len() < event_size {
            return Err(FsError::InvalidParam);
        }
        if self.buf.is_empty() {
            return Err(FsError::Again);
        }
        let mut read = 0;
        while read + event_size <= buf.len() {
            if let Some(e) = self.buf.pop_front() {
                buf[read..read + event_size].copy_from_slice(e.as_buf());
                read += event_size;
            } else {
                break;
            }
        }
        Ok(read)
    }

    fn handle_input_event(&mut self, e: &InputEvent) {
        while self.buf.len() >= BUF_CAPACITY {
            self.buf.pop_front();
        }
        // One timestamp per frame: taken at the frame's first event, reused
        // until its SYN_REPORT closes it.
        let time = *self.frame_time.get_or_insert_with(TimeVal::now_monotonic);
        self.buf.push_back(TimedInputEvent::with_time(e, time));
        if e.event_type == InputEventType::Syn && e.code == SYN_REPORT {
            self.frame_time = None;
        }
    }
}

/// Maps a `kernel_hal::user` pointer-operation result onto `FsError`.
/// `linux-object` cannot add a `From<kernel_hal::user::Error>` impl for
/// `FsError` (orphan rule: neither type is local to this crate), so every
/// checked-pointer call site below routes through this instead of a bare `?`.
/// Mirrors `fs/stdio.rs`'s private `user_copy`, which cannot be reused across
/// modules.
fn user_copy<T>(r: core::result::Result<T, kernel_hal::user::Error>) -> Result<T> {
    r.map_err(|_| FsError::InvalidParam)
}

impl EventDev {
    /// Create a input event INode
    pub fn new(input: Arc<dyn InputScheme>, id: usize) -> Self {
        let inner = Arc::new(Mutex::new(EventDevInner {
            buf: VecDeque::with_capacity(BUF_CAPACITY),
            frame_time: None,
        }));
        let cloned = inner.clone();
        input.subscribe(
            Box::new(move |e| cloned.lock().handle_input_event(e)),
            false,
        );
        Self {
            id,
            input,
            inner,
            inode_id: DevFS::new_inode_id(),
        }
    }

    fn can_read(&self) -> bool {
        !self.inner.lock().buf.is_empty()
    }

    /// Map a Linux `EV_*` event-type code to the driver capability bitmap that
    /// `EVIOCGBIT(ev)` should report. `ev == 0` asks for the set of supported
    /// event types themselves.
    fn capability_for_ev(&self, ev: u16) -> InputCapability {
        let cap_type = match ev {
            0x00 => CapabilityType::Event,   // EVIOCGBIT(0): supported event types
            0x01 => CapabilityType::Key,     // EV_KEY
            0x02 => CapabilityType::RelAxis, // EV_REL
            0x03 => CapabilityType::AbsAxis, // EV_ABS
            0x04 => CapabilityType::Misc,    // EV_MSC
            0x05 => CapabilityType::Switch,  // EV_SW
            0x11 => CapabilityType::Led,     // EV_LED
            0x12 => CapabilityType::Sound,   // EV_SND
            0x15 => CapabilityType::FeedBack, // EV_FF
            _ => return InputCapability::empty(),
        };
        self.input.capability(cap_type)
    }
}

impl INode for EventDev {
    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize> {
        self.inner.lock().read_at(buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize> {
        Err(FsError::NotSupported)
    }

    fn poll(&self) -> Result<PollStatus> {
        Ok(PollStatus {
            read: self.can_read(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        /// Parks on the input EventListener until readable. Must unsubscribe
        /// on Ready/`Drop`: poll/epoll re-scan (and Ready-after-subscribe) used
        /// to leave one-shot wakers that later `trigger` into freed tasks —
        /// UAF that surfaces as KERNEL PAGE FAULT after a long desktop idle.
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct EventFuture<'a> {
            dev: &'a EventDev,
            sub_id: Option<u64>,
        }

        impl Drop for EventFuture<'_> {
            fn drop(&mut self) {
                if let Some(id) = self.sub_id.take() {
                    self.dev.input.unsubscribe(id);
                }
            }
        }

        impl<'a> Future for EventFuture<'a> {
            type Output = Result<PollStatus>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                let this = self.as_mut().get_mut();
                // Fast path: data already available.
                if this.dev.can_read() {
                    if let Some(id) = this.sub_id.take() {
                        this.dev.input.unsubscribe(id);
                    }
                    return Poll::Ready(this.dev.poll());
                }
                // Register the waker BEFORE the second can_read() check to
                // eliminate the TOCTOU race: if an event arrives between the
                // first check (false) and subscribe(), it would not fire any
                // waker and the task would sleep indefinitely until the next
                // event.  By registering first, any event that arrives during
                // or after subscribe() will call the waker and reschedule the
                // task.
                if this.sub_id.is_none() {
                    let waker = cx.waker().clone();
                    this.sub_id = this
                        .dev
                        .input
                        .subscribe(Box::new(move |_| waker.wake_by_ref()), true);
                }
                // Re-check after registering the waker in case an event
                // arrived in the window between the first check and subscribe().
                if this.dev.can_read() {
                    if let Some(id) = this.sub_id.take() {
                        this.dev.input.unsubscribe(id);
                    }
                    return Poll::Ready(this.dev.poll());
                }
                Poll::Pending
            }
        }

        Box::pin(EventFuture {
            dev: self,
            sub_id: None,
        })
    }

    /// Implement the `EVIOC*` ioctls (Linux `<linux/input.h>`) that
    /// `evdev`/`libinput` issue while probing a device. The request encodes a
    /// direction, size, type (`'E'`) and number; we decode the number and the
    /// userspace buffer size from it.
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        let size = (((cmd >> 16) & 0x3fff) as usize).min(256);
        let typ = (cmd >> 8) & 0xff;
        let nr = (cmd & 0xff) as usize;
        // Only the input ioctl group ('E').
        if typ != 'E' as u32 {
            return Err(FsError::NotSupported);
        }
        // EVIOCSCLOCKID / EVIOCGRAB / EVIOCREVOKE pass their argument by value
        // and never dereference the pointer; in particular seatd issues
        // EVIOCREVOKE with a NULL argument (`data == 0`) when it revokes a
        // device on session (de)activation. Handle them before the null-pointer
        // guard below — that guard is only for the ioctls that read/write
        // through `data`. We do not enforce grab/revoke, so accept as no-ops.
        // (Rejecting EVIOCREVOKE with EINVAL made seatd treat the open as failed
        // and never hand the device fd to libinput → "no input devices".)
        if matches!(nr, 0xa0 | 0x90 | 0x91) {
            return Ok(0);
        }
        if data == 0 {
            return Err(FsError::InvalidParam);
        }
        match nr {
            // EVIOCGVERSION -> EV_VERSION (0x010001).
            0x01 => {
                let mut ptr = kernel_hal::user::UserOutPtr::<i32>::from(data);
                user_copy(ptr.write(0x01_0001))?;
                Ok(core::mem::size_of::<i32>())
            }
            // EVIOCGID -> struct input_id { bustype, vendor, product, version }.
            // Report a virtual bus; vendor/product/version are not meaningful.
            0x02 => {
                let mut ptr = kernel_hal::user::UserOutPtr::<[u16; 4]>::from(data);
                user_copy(ptr.write([0x06, 0, 0, 0]))?;
                Ok(8)
            }
            // EVIOCGREP -> repeat [delay_ms, period_ms].
            0x03 => {
                let mut ptr = kernel_hal::user::UserOutPtr::<[u32; 2]>::from(data);
                user_copy(ptr.write([250, 33]))?;
                Ok(8)
            }
            // EVIOCGNAME(len) -> device name (NUL-terminated).
            0x06 => {
                let name = self.input.name().as_bytes();
                let n = (name.len() + 1).min(size);
                if n == 0 {
                    return Ok(0);
                }
                let mut buf = [0u8; 256];
                let body = n - 1;
                buf[..body].copy_from_slice(&name[..body]);
                // buf[body] is already 0 from initialization -- the NUL terminator.
                let mut ptr = kernel_hal::user::UserOutPtr::<u8>::from(data);
                user_copy(ptr.write_array(&buf[..n]))?;
                Ok(n)
            }
            // EVIOCGPHYS / EVIOCGUNIQ: physical location / unique id strings.
            // We have neither, but libevdev (which libinput uses) treats any
            // ioctl error here OTHER than ENOENT as fatal and aborts the whole
            // device setup — so returning ENOTTY made libinput reject every
            // device. Return an empty (NUL-terminated) string instead.
            0x07 | 0x08 => {
                let mut ptr = kernel_hal::user::UserOutPtr::<u8>::from(data);
                user_copy(ptr.write(0))?;
                Ok(1)
            }
            // EVIOCGPROP: input properties bitmap. A USB tablet is a pointer
            // (needs an on-screen cursor), not a direct touchscreen. Use the
            // scheme's InputProp bits so this cannot drift from capability().
            0x09 => {
                let bytes = self
                    .input
                    .capability(CapabilityType::InputProp)
                    .to_le_bytes();
                let n = size.min(bytes.len());
                let mut ptr = kernel_hal::user::UserOutPtr::<u8>::from(data);
                user_copy(ptr.write_array(&bytes[..n]))?;
                Ok(n)
            }
            // EVIOCGKEY / EVIOCGLED / EVIOCGSND / EVIOCGSW: report
            // an all-zero state (nothing currently pressed/lit).
            0x18..=0x1b => {
                let zeros = [0u8; 256];
                let mut ptr = kernel_hal::user::UserOutPtr::<u8>::from(data);
                user_copy(ptr.write_array(&zeros[..size]))?;
                Ok(size)
            }
            // EVIOCGBIT(ev, len): supported event types / codes bitmap.
            0x20..=0x3f => {
                let bytes = self.capability_for_ev((nr - 0x20) as u16).to_le_bytes();
                let n = size.min(bytes.len());
                let mut ptr = kernel_hal::user::UserOutPtr::<u8>::from(data);
                user_copy(ptr.write_array(&bytes[..n]))?;
                Ok(n)
            }
            // EVIOCGABS(abs): struct input_absinfo { value, min, max, fuzz, flat, res }.
            0x40..=0x7f => {
                let axis = (nr - 0x40) as u16;
                let info = self.input.abs_info(axis).unwrap_or_default();
                let words = [
                    info.value,
                    info.minimum,
                    info.maximum,
                    info.fuzz,
                    info.flat,
                    info.resolution,
                ];
                let mut bytes = [0u8; 24];
                for (i, v) in words.iter().enumerate() {
                    bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
                let n = size.min(bytes.len());
                let mut ptr = kernel_hal::user::UserOutPtr::<u8>::from(data);
                user_copy(ptr.write_array(&bytes[..n]))?;
                Ok(n)
            }
            _ => Err(FsError::NotSupported),
        }
    }

    fn metadata(&self) -> Result<Metadata> {
        Ok(Metadata {
            dev: 1,
            inode: self.inode_id,
            size: 0,
            blk_size: 0,
            blocks: 0,
            atime: Timespec { sec: 0, nsec: 0 },
            mtime: Timespec { sec: 0, nsec: 0 },
            ctime: Timespec { sec: 0, nsec: 0 },
            type_: FileType::CharDevice,
            mode: 0o660,
            nlinks: 1,
            uid: 0,
            gid: 0,
            rdev: make_rdev(0xd, EVENT_DEV_MINOR_BASE + self.id),
        })
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod frame_tests {
    //! An evdev frame is a run of events ending in `SYN_REPORT`, and libinput
    //! treats the whole run as one instant. It checks that by the timestamp:
    //! if the time changes before the `SYN_REPORT` arrives, it reports
    //! `kernel bug: event frame missing SYN_REPORT, forcing frame` and
    //! resynchronises, which on a mouse looks like the pointer stuttering.
    //!
    //! That happened here for real, on hardware and not in QEMU, because it
    //! needs a multi-event report to straddle a clock tick -- and a report
    //! only has several events when a real mouse sends X and Y (and a wheel)
    //! in one packet, at a rate that makes straddling likely. It was fixed
    //! once with no test on it; this is that test.

    use super::*;
    use core::convert::TryInto;
    use zcore_drivers::input::input_event_codes::{rel::*, syn::*};

    fn inner() -> EventDevInner {
        EventDevInner {
            buf: VecDeque::with_capacity(BUF_CAPACITY),
            frame_time: None,
        }
    }

    fn rel(code: u16, value: i32) -> InputEvent {
        InputEvent {
            event_type: InputEventType::RelAxis,
            code,
            value,
        }
    }

    fn syn() -> InputEvent {
        InputEvent {
            event_type: InputEventType::Syn,
            code: SYN_REPORT,
            value: 0,
        }
    }

    #[test]
    fn every_event_of_a_frame_carries_the_same_timestamp() {
        // The mouse reports X, Y and a wheel notch in one packet, and they
        // must all be stamped with the instant the frame opened -- not each
        // with the clock as it was when the kernel got round to it.
        //
        // The host clock cannot be moved from here, so the frame is opened
        // with a marker no monotonic clock can reach (it would need sixty
        // years of uptime). Any event stamped from the clock instead of from
        // the open frame therefore shows up as a different value.
        let mut dev = inner();
        let marker = TimeVal {
            sec: 0x7000_0000,
            usec: 500,
        };
        dev.frame_time = Some(marker);

        for e in [rel(REL_X, 3), rel(REL_Y, -2), rel(REL_WHEEL, 1)] {
            dev.handle_input_event(&e);
        }
        dev.handle_input_event(&syn());

        assert_eq!(dev.buf.len(), 4, "the frame and its SYN_REPORT");
        for (i, ev) in dev.buf.iter().enumerate() {
            assert_eq!(
                (ev.time.sec, ev.time.usec),
                (marker.sec, marker.usec),
                "event {} of the frame was stamped separately",
                i
            );
        }
    }

    #[test]
    fn the_syn_report_closes_the_frame_so_the_next_one_takes_a_new_time() {
        // If `SYN_REPORT` did not clear it, every event for the rest of the
        // session would carry the first frame's timestamp and libinput's
        // debounce, tap and scroll timers would all fire on a clock that
        // never moves.
        let mut dev = inner();
        dev.frame_time = Some(TimeVal { sec: 1, usec: 0 });
        dev.handle_input_event(&rel(REL_X, 1));
        assert!(dev.frame_time.is_some(), "the frame is still open");
        dev.handle_input_event(&syn());
        assert!(
            dev.frame_time.is_none(),
            "the SYN_REPORT did not close the frame"
        );

        // The next frame opens with its own time.
        dev.frame_time = Some(TimeVal { sec: 2, usec: 0 });
        dev.handle_input_event(&rel(REL_X, 1));
        assert_eq!(dev.buf.back().unwrap().time.sec, 2);
    }

    #[test]
    fn a_syn_of_another_kind_does_not_close_the_frame() {
        // `SYN_MT_REPORT` separates the fingers of a multitouch packet and is
        // NOT a frame boundary; only `SYN_REPORT` is. Closing on any Syn
        // event would split one touch frame into several.
        let mut dev = inner();
        dev.frame_time = Some(TimeVal { sec: 5, usec: 0 });
        dev.handle_input_event(&rel(REL_X, 1));
        dev.handle_input_event(&InputEvent {
            event_type: InputEventType::Syn,
            code: SYN_MT_REPORT,
            value: 0,
        });
        assert!(
            dev.frame_time.is_some(),
            "SYN_MT_REPORT closed the frame, which only SYN_REPORT may do"
        );
        for ev in dev.buf.iter() {
            assert_eq!(ev.time.sec, 5);
        }
    }

    #[test]
    fn events_are_read_back_in_the_order_they_arrived() {
        // evdev is a stream: a reader that gets Y before X moves the pointer
        // somewhere else entirely.
        let mut dev = inner();
        dev.frame_time = Some(TimeVal { sec: 1, usec: 0 });
        dev.handle_input_event(&rel(REL_X, 7));
        dev.handle_input_event(&rel(REL_Y, -9));
        dev.handle_input_event(&syn());

        let size = size_of::<TimedInputEvent>();
        let mut buf = alloc::vec![0u8; size * 3];
        let n = dev.read_at(&mut buf).unwrap();
        assert_eq!(n, size * 3, "all three events fit and must all come out");

        let decode = |i: usize| -> (u16, i32) {
            let base = i * size + size_of::<TimeVal>();
            let code = u16::from_ne_bytes([buf[base + 2], buf[base + 3]]);
            let value =
                i32::from_ne_bytes([buf[base + 4], buf[base + 5], buf[base + 6], buf[base + 7]]);
            (code, value)
        };
        assert_eq!(decode(0), (REL_X, 7));
        assert_eq!(decode(1), (REL_Y, -9));
        assert_eq!(decode(2), (SYN_REPORT, 0));
        assert!(dev.buf.is_empty(), "the events were not consumed");
    }

    #[test]
    fn a_read_takes_only_whole_events_and_leaves_the_rest() {
        // A reader with room for two events must get exactly two, not two and
        // a fragment: the next read would then start mid-struct and every
        // event after it would be garbage.
        let mut dev = inner();
        dev.frame_time = Some(TimeVal { sec: 1, usec: 0 });
        for i in 0..5 {
            dev.handle_input_event(&rel(REL_X, i));
        }
        let size = size_of::<TimedInputEvent>();
        let mut buf = alloc::vec![0u8; size * 2 + size / 2];
        let n = dev.read_at(&mut buf).unwrap();
        assert_eq!(n, size * 2);
        assert_eq!(dev.buf.len(), 3, "the rest must still be queued");
    }

    #[test]
    fn a_reader_with_no_room_for_one_event_is_refused() {
        // Returning a short count here would have the reader advance by less
        // than a struct and desynchronise for good.
        let mut dev = inner();
        dev.frame_time = Some(TimeVal { sec: 1, usec: 0 });
        dev.handle_input_event(&rel(REL_X, 1));
        let mut tiny = alloc::vec![0u8; size_of::<TimedInputEvent>() - 1];
        assert!(matches!(dev.read_at(&mut tiny), Err(FsError::InvalidParam)));
    }

    #[test]
    fn an_empty_queue_says_try_again_rather_than_end_of_file() {
        // A blocking reader takes a zero-length read as EOF and closes the
        // device; `EAGAIN` is what tells it to wait for more.
        let mut dev = inner();
        let mut buf = alloc::vec![0u8; size_of::<TimedInputEvent>()];
        assert!(matches!(dev.read_at(&mut buf), Err(FsError::Again)));
    }

    #[test]
    fn a_reader_that_never_drains_loses_the_oldest_events_not_the_newest() {
        // The queue is bounded, and when a client stops reading, what matters
        // is that the pointer ends up where the mouse actually is. Dropping
        // the NEWEST events would leave it lagging for ever.
        let mut dev = inner();
        dev.frame_time = Some(TimeVal { sec: 1, usec: 0 });
        for i in 0..(BUF_CAPACITY as i32 + 10) {
            dev.handle_input_event(&rel(REL_X, i));
        }
        assert_eq!(dev.buf.len(), BUF_CAPACITY, "the queue grew past its bound");
        assert_eq!(
            dev.buf.back().unwrap().value,
            BUF_CAPACITY as i32 + 9,
            "the most recent event was dropped"
        );
        assert_eq!(
            dev.buf.front().unwrap().value,
            10,
            "the oldest survivor is not the one the bound implies"
        );
    }

    #[test]
    fn the_struct_on_the_wire_is_the_one_evdev_clients_read() {
        // `struct input_event` on 64-bit Linux is a 16-byte timeval, then
        // type, code and value. libinput reads exactly this many bytes per
        // event and indexes the fields by offset, so the layout is ABI.
        assert_eq!(size_of::<TimeVal>(), 16, "timeval is two 64-bit words");
        assert_eq!(size_of::<TimedInputEvent>(), 24);
        let e = TimedInputEvent::with_time(&rel(REL_X, -5), TimeVal { sec: 3, usec: 4 });
        let raw = e.as_buf();
        assert_eq!(raw.len(), 24);
        assert_eq!(usize::from_ne_bytes(raw[0..8].try_into().unwrap()), 3);
        assert_eq!(usize::from_ne_bytes(raw[8..16].try_into().unwrap()), 4);
        assert_eq!(u16::from_ne_bytes([raw[18], raw[19]]), REL_X);
        assert_eq!(i32::from_ne_bytes(raw[20..24].try_into().unwrap()), -5);
    }
}
