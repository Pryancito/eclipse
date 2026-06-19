use alloc::{boxed::Box, collections::VecDeque, sync::Arc};
use core::task::{Context, Poll};
use core::{any::Any, future::Future, mem::size_of, pin::Pin};

use lock::Mutex;

use kernel_hal::drivers::prelude::{CapabilityType, InputCapability, InputEvent, InputEventType};
use kernel_hal::drivers::scheme::InputScheme;
use rcore_fs::vfs::*;
use rcore_fs_devfs::DevFS;

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
}

/// Event char device, giving access to raw input device events.
pub struct EventDev {
    id: usize,
    inode_id: usize,
    input: Arc<dyn InputScheme>,
    inner: Arc<Mutex<EventDevInner>>,
}

impl TimedInputEvent {
    pub fn from(e: &InputEvent) -> Self {
        TimedInputEvent {
            time: TimeVal::now(),
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
        self.buf.push_back(TimedInputEvent::from(e));
    }
}

impl EventDev {
    /// Create a input event INode
    pub fn new(input: Arc<dyn InputScheme>, id: usize) -> Self {
        let inner = Arc::new(Mutex::new(EventDevInner {
            buf: VecDeque::with_capacity(BUF_CAPACITY),
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
            0x00 => CapabilityType::Event, // EVIOCGBIT(0): supported event types
            0x01 => CapabilityType::Key,   // EV_KEY
            0x02 => CapabilityType::RelAxis, // EV_REL
            0x03 => CapabilityType::AbsAxis, // EV_ABS
            0x04 => CapabilityType::Misc,  // EV_MSC
            0x05 => CapabilityType::Switch, // EV_SW
            0x11 => CapabilityType::Led,   // EV_LED
            0x12 => CapabilityType::Sound, // EV_SND
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
        })
    }

    fn async_poll<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PollStatus>> + Send + Sync + 'a>> {
        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct EventFuture<'a> {
            dev: &'a EventDev,
        }

        impl<'a> Future for EventFuture<'a> {
            type Output = Result<PollStatus>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                // Fast path: data already available.
                if self.dev.can_read() {
                    return Poll::Ready(self.dev.poll());
                }
                // Register the waker BEFORE the second can_read() check to
                // eliminate the TOCTOU race: if an event arrives between the
                // first check (false) and subscribe(), it would not fire any
                // waker and the task would sleep indefinitely until the next
                // event.  By registering first, any event that arrives during
                // or after subscribe() will call the waker and reschedule the
                // task.
                let waker = cx.waker().clone();
                self.dev
                    .input
                    .subscribe(Box::new(move |_| waker.wake_by_ref()), true);
                // Re-check after registering the waker in case an event
                // arrived in the window between the first check and subscribe().
                if self.dev.can_read() {
                    return Poll::Ready(self.dev.poll());
                }
                Poll::Pending
            }
        }

        Box::pin(EventFuture { dev: self })
    }

    /// Implement the `EVIOC*` ioctls (Linux `<linux/input.h>`) that
    /// `evdev`/`libinput` issue while probing a device. The request encodes a
    /// direction, size, type (`'E'`) and number; we decode the number and the
    /// userspace buffer size from it.
    #[allow(unsafe_code)]
    fn io_control(&self, cmd: u32, data: usize) -> Result<usize> {
        if data == 0 {
            return Err(FsError::InvalidParam);
        }
        let size = ((cmd >> 16) & 0x3fff) as usize;
        let typ = (cmd >> 8) & 0xff;
        let nr = (cmd & 0xff) as usize;
        // Only the input ioctl group ('E').
        if typ != 'E' as u32 {
            return Err(FsError::NotSupported);
        }
        match nr {
            // EVIOCGVERSION -> EV_VERSION (0x010001).
            0x01 => {
                unsafe { *(data as *mut i32) = 0x01_0001 };
                Ok(core::mem::size_of::<i32>())
            }
            // EVIOCGID -> struct input_id { bustype, vendor, product, version }.
            // Report a virtual bus; vendor/product/version are not meaningful.
            0x02 => {
                unsafe { *(data as *mut [u16; 4]) = [0x06, 0, 0, 0] };
                Ok(8)
            }
            // EVIOCGREP -> repeat [delay_ms, period_ms].
            0x03 => {
                unsafe { *(data as *mut [u32; 2]) = [250, 33] };
                Ok(8)
            }
            // EVIOCGNAME(len) -> device name (NUL-terminated).
            0x06 => {
                let name = self.input.name().as_bytes();
                let n = (name.len() + 1).min(size);
                if n == 0 {
                    return Ok(0);
                }
                let dst = unsafe { core::slice::from_raw_parts_mut(data as *mut u8, n) };
                let body = n - 1;
                dst[..body].copy_from_slice(&name[..body]);
                dst[n - 1] = 0;
                Ok(n)
            }
            // EVIOCGPROP / EVIOCGKEY / EVIOCGLED / EVIOCGSND / EVIOCGSW: report
            // an all-zero state (no properties, nothing currently pressed/lit).
            0x09 | 0x18 | 0x19 | 0x1a | 0x1b => {
                let dst = unsafe { core::slice::from_raw_parts_mut(data as *mut u8, size) };
                dst.fill(0);
                Ok(size)
            }
            // EVIOCSCLOCKID / EVIOCGRAB / EVIOCREVOKE: accept as no-ops.
            0xa0 | 0x90 | 0x91 => Ok(0),
            // EVIOCGBIT(ev, len): supported event types / codes bitmap.
            0x20..=0x3f => {
                let bytes = self.capability_for_ev((nr - 0x20) as u16).to_le_bytes();
                let n = size.min(bytes.len());
                let dst = unsafe { core::slice::from_raw_parts_mut(data as *mut u8, n) };
                dst.copy_from_slice(&bytes[..n]);
                Ok(n)
            }
            // EVIOCGABS(abs): struct input_absinfo — zeroed (no absolute axes).
            0x40..=0x7f => {
                let n = size.min(24);
                let dst = unsafe { core::slice::from_raw_parts_mut(data as *mut u8, n) };
                dst.fill(0);
                Ok(0)
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
