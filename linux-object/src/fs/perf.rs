//! Minimal `perf_event_open(2)` support: software CPU-clock sampling.
//!
//! This is **not** a hardware-PMU implementation. It implements enough of the
//! perf ring-buffer ABI for `perf top` / `perf record` to run and display a
//! live, sampled profile of user-space code:
//!
//! - `perf_event_open` returns a working fd (see [`sys_perf_event_open`]).
//! - `mmap`-ing the fd returns the ring buffer (control page + data pages).
//! - `ioctl(ENABLE/DISABLE/RESET/...)` toggles sampling.
//! - the timer-interrupt return path calls [`sample_user`], which appends a
//!   `PERF_RECORD_SAMPLE` (honouring the event's `sample_type`) into every
//!   enabled, matching ring buffer.
//!
//! Records are encoded byte-exactly per the kernel ABI so unmodified `perf`
//! parses them. Hardware events are accepted but sampled with the same
//! timer-driven software clock (there is no real PMU here), which is the
//! standard fallback when no PMU is available.

use super::*;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;
use zircon_object::object::*;
use zircon_object::vm::{pages, VmObject};

use crate::sync::{Event, EventBus};

const PAGE_SIZE: usize = 4096;

// ---- perf_event_attr field offsets (LP64 ABI) ----
const ATTR_OFF_TYPE: usize = 0; // u32
const ATTR_OFF_CONFIG: usize = 8; // u64
const ATTR_OFF_SAMPLE_PERIOD: usize = 16; // u64 (sample_period | sample_freq)
const ATTR_OFF_SAMPLE_TYPE: usize = 24; // u64
const ATTR_OFF_READ_FORMAT: usize = 32; // u64
const ATTR_OFF_FLAGS: usize = 40; // u64 bitfield

// ---- attr flag bits (within the u64 at ATTR_OFF_FLAGS) ----
const ATTR_FLAG_DISABLED: u64 = 1 << 0;
const ATTR_FLAG_FREQ: u64 = 1 << 10;

// ---- PERF_SAMPLE_* bits (sample_type) ----
const PERF_SAMPLE_IP: u64 = 1 << 0;
const PERF_SAMPLE_TID: u64 = 1 << 1;
const PERF_SAMPLE_TIME: u64 = 1 << 2;
const PERF_SAMPLE_ADDR: u64 = 1 << 3;
const PERF_SAMPLE_READ: u64 = 1 << 4;
const PERF_SAMPLE_CALLCHAIN: u64 = 1 << 5;
const PERF_SAMPLE_ID: u64 = 1 << 6;
const PERF_SAMPLE_CPU: u64 = 1 << 7;
const PERF_SAMPLE_PERIOD: u64 = 1 << 8;
const PERF_SAMPLE_STREAM_ID: u64 = 1 << 9;
const PERF_SAMPLE_RAW: u64 = 1 << 10;
const PERF_SAMPLE_BRANCH_STACK: u64 = 1 << 11;
const PERF_SAMPLE_REGS_USER: u64 = 1 << 12;
const PERF_SAMPLE_STACK_USER: u64 = 1 << 13;
const PERF_SAMPLE_WEIGHT: u64 = 1 << 14;
const PERF_SAMPLE_DATA_SRC: u64 = 1 << 15;
const PERF_SAMPLE_IDENTIFIER: u64 = 1 << 16;
const PERF_SAMPLE_TRANSACTION: u64 = 1 << 17;
const PERF_SAMPLE_REGS_INTR: u64 = 1 << 18;
const PERF_SAMPLE_PHYS_ADDR: u64 = 1 << 19;

// ---- read_format bits ----
const PERF_FORMAT_TOTAL_TIME_ENABLED: u64 = 1 << 0;
const PERF_FORMAT_TOTAL_TIME_RUNNING: u64 = 1 << 1;
const PERF_FORMAT_ID: u64 = 1 << 2;

// ---- record types / misc ----
const PERF_RECORD_SAMPLE: u32 = 9;
const PERF_RECORD_MISC_USER: u16 = 2;

// ---- ioctl numbers (magic '$' = 0x24) ----
const PERF_EVENT_IOC_ENABLE: usize = 0x2400;
const PERF_EVENT_IOC_DISABLE: usize = 0x2401;
const PERF_EVENT_IOC_REFRESH: usize = 0x2402;
const PERF_EVENT_IOC_RESET: usize = 0x2403;
const PERF_EVENT_IOC_PERIOD: usize = 0x4008_2404;
const PERF_EVENT_IOC_SET_OUTPUT: usize = 0x2405;
const PERF_EVENT_IOC_SET_FILTER: usize = 0x4008_2406;
const PERF_EVENT_IOC_ID: usize = 0x8008_2407;

// ---- control-page (perf_event_mmap_page) field offsets ----
const PC_VERSION: usize = 0; // u32
const PC_DATA_HEAD: usize = 1024; // u64
const PC_DATA_TAIL: usize = 1032; // u64
const PC_DATA_OFFSET: usize = 1040; // u64
const PC_DATA_SIZE: usize = 1048; // u64

/// Next event id, used as the perf `id` / `stream_id`.
static NEXT_ID: Mutex<u64> = Mutex::new(1);

fn alloc_id() -> u64 {
    let mut g = NEXT_ID.lock();
    let id = *g;
    *g += 1;
    id
}

/// Global registry of live perf events, consulted by the sampler.
static PERF_EVENTS: Mutex<Vec<Weak<PerfEvent>>> = Mutex::new(Vec::new());

/// Set once any perf event is enabled, so the hot timer path can cheaply skip
/// the registry lock when nothing is profiling.
static ANY_ENABLED: AtomicBool = AtomicBool::new(false);

/// The mmap ring buffer of one perf event.
struct Ring {
    vmo: Arc<VmObject>,
    /// Size in bytes of the data region (excludes the control page).
    data_size: usize,
}

/// The monotonic clock, in nanoseconds, as every time field here reads it.
fn now_ns() -> u64 {
    kernel_hal::timer::timer_now().as_nanos() as u64
}

struct PerfInner {
    /// `type` field of `perf_event_attr` (PERF_TYPE_*).
    _type: u32,
    /// `config` field (which event).
    _config: u64,
    sample_type: u64,
    read_format: u64,
    /// Effective sampling period in "events" (software clock ticks here). When
    /// the attr asked for a frequency we still sample once per timer tick and
    /// report a period of 1; perf re-derives a rate from the timestamps.
    period: u64,
    id: u64,
    enabled: bool,
    /// Nanoseconds this event has been enabled, not counting the stretch it is
    /// in now. `enabled_since` carries that one.
    time_enabled: u64,
    /// When the current enabled stretch began, or `None` while disabled.
    enabled_since: Option<u64>,
    /// Free-running write counter (bytes ever written to the data region).
    data_head: u64,
    /// Accumulated event count, returned by `read(2)`.
    count: u64,
    /// Dropped samples because the ring was full (reported as LOST is TODO).
    lost: u64,
    ring: Option<Ring>,
}

impl PerfInner {
    /// Nanoseconds this event has been enabled, the stretch it is in now
    /// included.
    fn time_ns(&self) -> u64 {
        let running = self
            .enabled_since
            .map(|since| now_ns().saturating_sub(since))
            .unwrap_or(0);
        self.time_enabled.saturating_add(running)
    }

    /// Enable or disable, closing or opening the current stretch. Called twice
    /// with the same answer it changes nothing, which is what an `ENABLE` on an
    /// already-enabled event has to do: restarting the stretch would lose the
    /// time already run.
    fn set_enabled(&mut self, on: bool) {
        if on == self.enabled {
            return;
        }
        self.enabled = on;
        if on {
            self.enabled_since = Some(now_ns());
        } else if let Some(since) = self.enabled_since.take() {
            self.time_enabled = self
                .time_enabled
                .saturating_add(now_ns().saturating_sub(since));
        }
    }
}

/// A single `perf_event_open` file descriptor.
pub struct PerfEvent {
    base: KObjectBase,
    /// CPU this event is bound to, or `-1` for any CPU.
    cpu: i32,
    /// PID this event profiles, or `-1` for all processes.
    pid: i32,
    flags: Mutex<OpenFlags>,
    eventbus: Arc<Mutex<EventBus>>,
    inner: Arc<Mutex<PerfInner>>,
}

impl_kobject!(PerfEvent);

impl PerfEvent {
    /// Build an event from a raw `perf_event_attr` byte image.
    pub fn new(attr: &[u8], pid: i32, cpu: i32, flags: OpenFlags) -> Arc<Self> {
        let rd_u32 = |off: usize| -> u32 {
            let mut b = [0u8; 4];
            if off + 4 <= attr.len() {
                b.copy_from_slice(&attr[off..off + 4]);
            }
            u32::from_ne_bytes(b)
        };
        let rd_u64 = |off: usize| -> u64 {
            let mut b = [0u8; 8];
            if off + 8 <= attr.len() {
                b.copy_from_slice(&attr[off..off + 8]);
            }
            u64::from_ne_bytes(b)
        };

        let type_ = rd_u32(ATTR_OFF_TYPE);
        let config = rd_u64(ATTR_OFF_CONFIG);
        let sample_type = rd_u64(ATTR_OFF_SAMPLE_TYPE);
        let read_format = rd_u64(ATTR_OFF_READ_FORMAT);
        let attr_flags = rd_u64(ATTR_OFF_FLAGS);
        let sample_period = rd_u64(ATTR_OFF_SAMPLE_PERIOD);
        let freq_mode = attr_flags & ATTR_FLAG_FREQ != 0;
        // In freq mode the field is a target Hz; we still sample per tick, so a
        // reported period of 1 keeps perf's accounting consistent. In period
        // mode, report the configured period (min 1).
        let period = if freq_mode { 1 } else { sample_period.max(1) };
        let enabled = attr_flags & ATTR_FLAG_DISABLED == 0;

        let event = Arc::new(PerfEvent {
            base: KObjectBase::new(),
            cpu,
            pid,
            flags: Mutex::new(flags),
            eventbus: EventBus::new(),
            inner: Arc::new(Mutex::new(PerfInner {
                _type: type_,
                _config: config,
                sample_type,
                read_format,
                period,
                id: alloc_id(),
                enabled,
                time_enabled: 0,
                enabled_since: enabled.then(now_ns),
                data_head: 0,
                count: 0,
                lost: 0,
                ring: None,
            })),
        });
        if enabled {
            ANY_ENABLED.store(true, Ordering::Relaxed);
        }
        register(&event);
        event
    }

    /// Append a `PERF_RECORD_SAMPLE` for an interrupted user instruction.
    ///
    fn record_sample(&self, pid: i32, tid: i32, cpu: u32, ip: u64, time_ns: u64) {
        let mut inner = self.inner.lock();
        if !inner.enabled {
            return;
        }
        // Before the ring check: the count is what a *counting* event — one
        // that is opened, never mmap-ed and read with `read(2)`, which is
        // `perf stat`'s whole mode of operation — reports. Moving it only
        // when a ring buffer happens to exist left every such event reading
        // a flat zero for its entire life.
        let period = inner.period;
        inner.count = inner.count.wrapping_add(period);
        if inner.ring.is_none() {
            return;
        }

        // Encode the sample body in the canonical field order, emitting exactly
        // the fields requested in `sample_type` so unmodified perf parses it.
        let st = inner.sample_type;
        let mut body: Vec<u8> = Vec::with_capacity(64);
        let push_u64 = |v: &mut Vec<u8>, x: u64| v.extend_from_slice(&x.to_ne_bytes());
        let push_u32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_ne_bytes());

        if st & PERF_SAMPLE_IDENTIFIER != 0 {
            push_u64(&mut body, inner.id);
        }
        if st & PERF_SAMPLE_IP != 0 {
            push_u64(&mut body, ip);
        }
        if st & PERF_SAMPLE_TID != 0 {
            push_u32(&mut body, pid as u32);
            push_u32(&mut body, tid as u32);
        }
        if st & PERF_SAMPLE_TIME != 0 {
            push_u64(&mut body, time_ns);
        }
        if st & PERF_SAMPLE_ADDR != 0 {
            push_u64(&mut body, 0);
        }
        if st & PERF_SAMPLE_ID != 0 {
            push_u64(&mut body, inner.id);
        }
        if st & PERF_SAMPLE_STREAM_ID != 0 {
            push_u64(&mut body, inner.id);
        }
        if st & PERF_SAMPLE_CPU != 0 {
            push_u32(&mut body, cpu);
            push_u32(&mut body, 0);
        }
        if st & PERF_SAMPLE_PERIOD != 0 {
            push_u64(&mut body, period);
        }
        if st & PERF_SAMPLE_READ != 0 {
            // Non-group read_format only.
            push_u64(&mut body, inner.count);
            if inner.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
                push_u64(&mut body, time_ns);
            }
            if inner.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
                push_u64(&mut body, time_ns);
            }
            if inner.read_format & PERF_FORMAT_ID != 0 {
                push_u64(&mut body, inner.id);
            }
        }
        if st & PERF_SAMPLE_CALLCHAIN != 0 {
            push_u64(&mut body, 0); // nr = 0
        }
        if st & PERF_SAMPLE_RAW != 0 {
            // `{ u32 size; char data[size]; }`. With no raw data the kernel
            // still writes eight bytes — `size = sizeof(u32)` and a zero
            // word — and perf's parser skips `sizeof(u32) + size`. Claiming
            // `size = 0` here left it four bytes short, so every field of
            // this record after RAW (branch stack, regs, weight, data_src,
            // transaction, phys_addr) was read off by four.
            push_u32(&mut body, 4);
            push_u32(&mut body, 0);
        }
        if st & PERF_SAMPLE_BRANCH_STACK != 0 {
            push_u64(&mut body, 0); // nr = 0
        }
        if st & PERF_SAMPLE_REGS_USER != 0 {
            push_u64(&mut body, 0); // abi = NONE (no regs follow)
        }
        if st & PERF_SAMPLE_STACK_USER != 0 {
            push_u64(&mut body, 0); // size = 0 (no data / dyn_size)
        }
        if st & PERF_SAMPLE_WEIGHT != 0 {
            push_u64(&mut body, 0);
        }
        if st & PERF_SAMPLE_DATA_SRC != 0 {
            push_u64(&mut body, 0);
        }
        if st & PERF_SAMPLE_TRANSACTION != 0 {
            push_u64(&mut body, 0);
        }
        if st & PERF_SAMPLE_REGS_INTR != 0 {
            push_u64(&mut body, 0); // abi = NONE
        }
        if st & PERF_SAMPLE_PHYS_ADDR != 0 {
            push_u64(&mut body, 0);
        }
        // Pad the whole record to an 8-byte boundary.
        while !body.len().is_multiple_of(8) {
            body.push(0);
        }

        let total = 8 + body.len(); // perf_event_header is 8 bytes
        let mut record: Vec<u8> = Vec::with_capacity(total);
        record.extend_from_slice(&PERF_RECORD_SAMPLE.to_ne_bytes());
        record.extend_from_slice(&PERF_RECORD_MISC_USER.to_ne_bytes());
        record.extend_from_slice(&(total as u16).to_ne_bytes());
        record.extend_from_slice(&body);

        self.ring_write(&mut inner, &record);
        drop(inner);
        // Wake any poller waiting for data.
        self.eventbus.lock().set(Event::READABLE);
    }

    /// Write a fully-formed record into the data region, wrapping as needed,
    /// then publish the new `data_head`. Drops the sample if the consumer
    /// (perf) has not made room.
    fn ring_write(&self, inner: &mut PerfInner, record: &[u8]) {
        let Some(ring) = inner.ring.as_ref() else {
            return;
        };
        let data_size = ring.data_size;
        if record.is_empty() || record.len() > data_size {
            return;
        }
        // Read the consumer tail from the control page.
        let mut tail_b = [0u8; 8];
        let _ = ring.vmo.read(PC_DATA_TAIL, &mut tail_b);
        let data_tail = u64::from_ne_bytes(tail_b);
        let head = inner.data_head;
        // Available space in a non-overwrite ring.
        // `data_tail` is a word USERSPACE writes into the control page, so it
        // cannot be trusted to be behind the head: a tail ahead of it makes
        // this subtraction wrap to something near `u64::MAX`, and then
        // `used + record.len()` wrapped a second time, to a small number that
        // passes for "there is room". Linux masks instead of trusting it
        // (`CIRC_SPACE`, and `ring_buffer_has_space` on top), so a garbage tail
        // costs the consumer its own records and nothing else. Here the write
        // below is bounded by `head % data_size` either way, so nothing left
        // the ring; what the wrap did was hand out space that was not there,
        // and panic a build with overflow checks.
        let used = head.wrapping_sub(data_tail);
        if used > data_size as u64 || used + record.len() as u64 > data_size as u64 {
            inner.lost = inner.lost.wrapping_add(1);
            return;
        }
        let pos = (head % data_size as u64) as usize;
        let base = PAGE_SIZE; // data region starts after the control page
        if pos + record.len() <= data_size {
            let _ = ring.vmo.write(base + pos, record);
        } else {
            let first = data_size - pos;
            let _ = ring.vmo.write(base + pos, &record[..first]);
            let _ = ring.vmo.write(base, &record[first..]);
        }
        let new_head = head.wrapping_add(record.len() as u64);
        inner.data_head = new_head;
        // Publish head last so the consumer never sees a head past unwritten data.
        let _ = ring.vmo.write(PC_DATA_HEAD, &new_head.to_ne_bytes());
    }

    fn has_data(&self) -> bool {
        let inner = self.inner.lock();
        let Some(ring) = inner.ring.as_ref() else {
            return false;
        };
        let mut tail_b = [0u8; 8];
        let _ = ring.vmo.read(PC_DATA_TAIL, &mut tail_b);
        inner.data_head != u64::from_ne_bytes(tail_b)
    }

    /// Whether the ring has data, with the readiness flag brought in line
    /// with the answer.
    ///
    /// The flag is sticky and only `record_sample` ever sets it, while the
    /// consumer drains the ring by moving `data_tail` in its own mapping,
    /// which this side never sees happen. So after the first sample is read
    /// the flag stays set over an empty ring for good, and `async_poll`'s
    /// wait returns Ready the instant it is awaited: the loop below spins on
    /// a full CPU instead of parking. Clearing it here is what lets the wait
    /// actually wait.
    fn readiness(&self) -> bool {
        let has = self.has_data();
        let mut bus = self.eventbus.lock();
        if has {
            bus.set(Event::READABLE);
        } else {
            bus.clear(Event::READABLE);
        }
        has
    }
}

#[async_trait]
impl FileLike for PerfEvent {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        // Same trap as epoll/eventfd before `take_settable`: a no-op made
        // `fcntl(F_SETFL, O_NONBLOCK)` "succeed" while `F_GETFL` never changed.
        self.flags.lock().take_settable(f);
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        // Non-mmap read returns the accumulated count (optionally enabled/
        // running/id per read_format), like a counting event.
        let inner = self.inner.lock();
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&inner.count.to_ne_bytes());
        // Both of these used to be a hardcoded zero, and a zero
        // `time_running` is not a missing extra: `perf_counts_values__scale`
        // is `if (count->run == 0) { scaled = -1; count->val = 0; }`, and
        // `perf stat` prints that as `<not counted>`. `evsel__config` asks for
        // both fields on every event it opens, so every count this file
        // computed was thrown away by the one tool that reads it. Nothing here
        // multiplexes a counter off its PMU, so running == enabled, which is
        // also what the sampling path has always written.
        let time = inner.time_ns();
        if inner.read_format & PERF_FORMAT_TOTAL_TIME_ENABLED != 0 {
            out.extend_from_slice(&time.to_ne_bytes());
        }
        if inner.read_format & PERF_FORMAT_TOTAL_TIME_RUNNING != 0 {
            out.extend_from_slice(&time.to_ne_bytes());
        }
        if inner.read_format & PERF_FORMAT_ID != 0 {
            out.extend_from_slice(&inner.id.to_ne_bytes());
        }
        // A short buffer is refused rather than filled with a prefix: half of
        // a `u64` count is not a smaller count, it is a wrong one, and the
        // caller has no way to tell the difference from the return value.
        // `__perf_read` is `if (count < event->read_size) return -ENOSPC;`,
        // and a caller that sizes its buffer from `read_format` and retries on
        // `ENOSPC` was getting `EINVAL`.
        if buf.len() < out.len() {
            return Err(LxError::ENOSPC);
        }
        buf[..out.len()].copy_from_slice(&out);
        Ok(out.len())
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        self.read(buf).await
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: self.readiness(),
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        loop {
            if self.readiness() {
                return Ok(PollStatus {
                    read: true,
                    write: false,
                    error: false,
                    hangup: false,
                });
            }
            let bus = self.eventbus.clone();
            crate::sync::wait_for_event(bus, Event::READABLE).await?;
        }
    }

    fn ioctl(&self, request: usize, arg1: usize, _arg2: usize, _arg3: usize) -> LxResult<usize> {
        match request {
            PERF_EVENT_IOC_ENABLE => {
                self.inner.lock().set_enabled(true);
                ANY_ENABLED.store(true, Ordering::Relaxed);
                Ok(0)
            }
            PERF_EVENT_IOC_DISABLE => {
                self.inner.lock().set_enabled(false);
                Ok(0)
            }
            PERF_EVENT_IOC_RESET => {
                let mut inner = self.inner.lock();
                inner.count = 0;
                // `perf_event_reset` zeroes the times with the count: a `read`
                // right after a reset must not scale by a window that has
                // already gone by.
                inner.time_enabled = 0;
                inner.enabled_since = inner.enabled.then(now_ns);
                Ok(0)
            }
            PERF_EVENT_IOC_REFRESH => {
                // arg is a refresh count; treat as enable.
                self.inner.lock().set_enabled(true);
                ANY_ENABLED.store(true, Ordering::Relaxed);
                Ok(0)
            }
            // `arg` points at the new period, and what happens to it is not
            // best-effort: `_perf_event_period` is
            //
            //     if (!value) return -EINVAL;
            //     ...
            //     if (copy_from_user(&value, (u64 __user *)arg, sizeof(value)))
            //             return -EFAULT;
            //
            // Swallowing both left `perf record -F <freq>`, which is how the
            // period is changed while an event runs, told its request had been
            // honoured while the event kept the old one: samples at a rate
            // nobody asked for and no way to find out. A period of 0 was worse
            // than refused, it was quietly turned into 1 -- the fastest rate
            // there is, out of a value that means "stop sampling by count".
            PERF_EVENT_IOC_PERIOD => {
                let value = kernel_hal::user::UserInPtr::<u64>::from(arg1)
                    .read()
                    .map_err(|_| LxError::EFAULT)?;
                if value == 0 {
                    return Err(LxError::EINVAL);
                }
                self.inner.lock().period = value;
                Ok(0)
            }
            // Same for the id, which `copy_to_user` in `perf_event_ioctl`
            // reports on: `perf` asks for it to tell whose sample is whose
            // inside a group's ring, so a write that silently did not happen
            // left it demultiplexing by whatever was in that word already.
            PERF_EVENT_IOC_ID => {
                let id = self.inner.lock().id;
                kernel_hal::user::UserOutPtr::<u64>::from(arg1)
                    .write(id)
                    .map_err(|_| LxError::EFAULT)?;
                Ok(0)
            }
            // Grouping / output redirection / filters are accepted as no-ops so
            // perf does not bail; samples still land in this event's own ring.
            PERF_EVENT_IOC_SET_OUTPUT | PERF_EVENT_IOC_SET_FILTER => Ok(0),
            _ => Err(LxError::ENOTTY),
        }
    }

    fn get_vmo(&self, offset: usize, len: usize) -> LxResult<Arc<VmObject>> {
        // perf maps the ring buffer at file offset 0: one control page followed
        // by `2^n` data pages. Create it on first mmap and cache it so the
        // sampler and the user mapping share the same physical frames.
        //
        // A ONE-page mapping (the control page alone, no data pages) is valid
        // too: Linux allows it, and it is what programs that only want the
        // `cap_user_time`/`time_mult` clock calibration do — GZDoom's
        // `CalculateCPUSpeed` maps exactly 4096 bytes, and then dereferences
        // the result after checking it against `nullptr` rather than
        // `MAP_FAILED`. Rejecting the mapping with EINVAL therefore did not
        // "fail cleanly": it made the game read through `(void*)-1` and die
        // with SIGSEGV right after `I_Init: Setting up machine state.` on
        // real hardware. With no data pages the sampler never writes a record
        // (`data_size == 0`), and the zeroed capabilities word tells the
        // caller no user-space clock is available, which is the graceful
        // path.
        if offset != 0 || len < PAGE_SIZE {
            return Err(LxError::EINVAL);
        }
        let mut inner = self.inner.lock();
        // Reuse the cached ring only if it is big enough for this mapping; a
        // control-page-only ring created by an earlier caller must not be
        // handed to `perf` asking for real data pages.
        if let Some(ring) = inner.ring.as_ref() {
            if ring.vmo.len() >= pages(len) * PAGE_SIZE {
                return Ok(ring.vmo.clone());
            }
        }
        let total_pages = pages(len);
        let data_pages = total_pages - 1;
        // The data region is `1 + 2^n` pages. perf's consumer wraps with
        // `offset & (data_size - 1)`, so a count that is not a power of two
        // makes it read the ring at an offset this side never wrote: it would
        // not fail, it would quietly report samples that are not there. Zero
        // data pages stays allowed — that is the control-page-only mapping
        // the comment above describes.
        if data_pages != 0 && !data_pages.is_power_of_two() {
            return Err(LxError::EINVAL);
        }
        let data_size = data_pages * PAGE_SIZE;
        let vmo = VmObject::new_paged(total_pages);
        // Initialise the control page so perf finds a sane header.
        let _ = vmo.write(PC_VERSION, &0u32.to_ne_bytes());
        let _ = vmo.write(PC_DATA_HEAD, &0u64.to_ne_bytes());
        let _ = vmo.write(PC_DATA_TAIL, &0u64.to_ne_bytes());
        let _ = vmo.write(PC_DATA_OFFSET, &(PAGE_SIZE as u64).to_ne_bytes());
        let _ = vmo.write(PC_DATA_SIZE, &(data_size as u64).to_ne_bytes());
        inner.ring = Some(Ring {
            vmo: vmo.clone(),
            data_size,
        });
        Ok(vmo)
    }
}

fn register(event: &Arc<PerfEvent>) {
    let mut list = PERF_EVENTS.lock();
    list.retain(|w| w.strong_count() > 0);
    list.push(Arc::downgrade(event));
}

/// Whether an event opened for `(ev_cpu, ev_pid)` wants a sample taken on
/// `cpu` by `pid`. `-1` on either side of the event means "any".
///
/// A `pid` of 0 never reaches here: `perf_event_open(2)` reads it as "the
/// calling process", and the syscall resolves it to that process's id before
/// the event is built. An event that kept the literal 0 would match no
/// sample ever taken, which is silence, not an error.
fn event_matches(ev_cpu: i32, ev_pid: i32, cpu: u32, pid: i32) -> bool {
    (ev_cpu < 0 || ev_cpu as u32 == cpu) && (ev_pid < 0 || ev_pid == pid)
}

/// Record a user-space sample on every enabled perf event that matches the
/// given CPU and PID. Called from the timer-interrupt return path.
///
/// Cheap and non-blocking: returns immediately when nothing is profiling.
pub fn sample_user(pid: i32, tid: i32, cpu: u32, ip: u64) {
    if !ANY_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let time_ns = kernel_hal::timer::timer_now().as_nanos() as u64;
    // Snapshot the matching events, then drop the registry lock before touching
    // each event's own lock to avoid holding two locks at once.
    let events: Vec<Arc<PerfEvent>> = {
        let list = PERF_EVENTS.lock();
        list.iter().filter_map(|w| w.upgrade()).collect()
    };
    let mut any_enabled = false;
    for ev in events {
        let (matches, enabled) = {
            let inner = ev.inner.lock();
            any_enabled |= inner.enabled;
            (event_matches(ev.cpu, ev.pid, cpu, pid), inner.enabled)
        };
        if matches && enabled {
            ev.record_sample(pid, tid, cpu, ip, time_ns);
        }
    }
    if !any_enabled {
        ANY_ENABLED.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::convert::TryInto;

    /// `PERF_EVENTS` is a process-wide registry and the test binary runs the
    /// whole crate in one process, so `sample_user` reaches every live event
    /// any test has opened -- including the ones whose ring another test is
    /// about to read back. The tests that measure a ring or a count take turns.
    static SAMPLER_TURN: Mutex<()> = Mutex::new(());

    /// A `perf_event_attr` image carrying the fields this implementation
    /// reads. Real ones are 128 bytes; the syscall clamps the caller's
    /// `attr.size` to `[64, 4096]`.
    fn attr(sample_type: u64, read_format: u64, flags: u64, period: u64) -> Vec<u8> {
        let mut a = alloc::vec![0u8; 128];
        let put = |a: &mut Vec<u8>, off: usize, v: &[u8]| a[off..off + v.len()].copy_from_slice(v);
        put(&mut a, ATTR_OFF_TYPE, &1u32.to_ne_bytes());
        put(&mut a, ATTR_OFF_SAMPLE_PERIOD, &period.to_ne_bytes());
        put(&mut a, ATTR_OFF_SAMPLE_TYPE, &sample_type.to_ne_bytes());
        put(&mut a, ATTR_OFF_READ_FORMAT, &read_format.to_ne_bytes());
        put(&mut a, ATTR_OFF_FLAGS, &flags.to_ne_bytes());
        a
    }

    fn open(sample_type: u64, read_format: u64, flags: u64, period: u64) -> Arc<PerfEvent> {
        PerfEvent::new(
            &attr(sample_type, read_format, flags, period),
            -1,
            -1,
            OpenFlags::empty(),
        )
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on a perf fd used to return success while
    /// leaving the bit clear.
    #[test]
    fn set_flags_turns_a_blocking_perf_event_non_blocking() {
        let fd = open(0, 0, 0, 1);
        assert!(!fd.flags().non_block());
        fd.set_flags(OpenFlags::NON_BLOCK).unwrap();
        assert!(fd.flags().non_block());
        fd.set_flags(OpenFlags::empty()).unwrap();
        assert!(!fd.flags().non_block());
    }

    /// An event with `data_pages` data pages behind its control page.
    fn mapped(sample_type: u64, data_pages: usize) -> Arc<PerfEvent> {
        let ev = open(sample_type, 0, 0, 1);
        ev.get_vmo(0, (data_pages + 1) * PAGE_SIZE).unwrap();
        ev
    }

    /// Everything written to the data region so far, as the consumer would
    /// read it from byte 0 of its own mapping of that region.
    fn written(ev: &PerfEvent) -> Vec<u8> {
        let inner = ev.inner.lock();
        let ring = inner.ring.as_ref().unwrap();
        let mut buf = alloc::vec![0u8; inner.data_head as usize];
        let _ = ring.vmo.read(PAGE_SIZE, &mut buf);
        buf
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_ne_bytes(b[off..off + 4].try_into().unwrap())
    }

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_ne_bytes(b[off..off + 8].try_into().unwrap())
    }

    /// Read a `u64` field and step the cursor, the way a parser walks a body.
    fn take64(b: &[u8], at: &mut usize) -> u64 {
        let v = u64_at(b, *at);
        *at += 8;
        v
    }

    /// Move the consumer's tail, which is the only thing this side ever sees
    /// `perf` do: it drains the ring in its own mapping.
    fn set_tail(ev: &PerfEvent, tail: u64) {
        let inner = ev.inner.lock();
        let ring = inner.ring.as_ref().unwrap();
        let _ = ring.vmo.write(PC_DATA_TAIL, &tail.to_ne_bytes());
    }

    #[test]
    fn a_record_carries_a_header_that_says_its_own_size() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(PERF_SAMPLE_IP | PERF_SAMPLE_TID, 1);
        ev.record_sample(7, 9, 3, 0xdead_beef, 12_345);
        let b = written(&ev);
        assert_eq!(u32_at(&b, 0), PERF_RECORD_SAMPLE);
        assert_eq!(
            u16::from_ne_bytes(b[4..6].try_into().unwrap()),
            PERF_RECORD_MISC_USER
        );
        assert_eq!(
            u16::from_ne_bytes(b[6..8].try_into().unwrap()) as usize,
            b.len(),
            "the header's size is how the consumer finds the next record"
        );
        assert_eq!(b.len() % 8, 0, "records are eight-byte aligned");
        assert_eq!(u64_at(&b, 8), 0xdead_beef);
        assert_eq!(u32_at(&b, 16), 7);
        assert_eq!(u32_at(&b, 20), 9);
    }

    /// The body carries exactly the fields named in `sample_type`, in the
    /// kernel's order. A parser walks it field by field, so one field of the
    /// wrong width or in the wrong place spoils everything after it.
    #[test]
    fn the_body_follows_the_canonical_field_order() {
        let _turn = SAMPLER_TURN.lock();
        let all = PERF_SAMPLE_IDENTIFIER
            | PERF_SAMPLE_IP
            | PERF_SAMPLE_TID
            | PERF_SAMPLE_TIME
            | PERF_SAMPLE_ADDR
            | PERF_SAMPLE_ID
            | PERF_SAMPLE_STREAM_ID
            | PERF_SAMPLE_CPU
            | PERF_SAMPLE_PERIOD
            | PERF_SAMPLE_READ
            | PERF_SAMPLE_CALLCHAIN
            | PERF_SAMPLE_RAW
            | PERF_SAMPLE_BRANCH_STACK
            | PERF_SAMPLE_REGS_USER
            | PERF_SAMPLE_STACK_USER
            | PERF_SAMPLE_WEIGHT
            | PERF_SAMPLE_DATA_SRC
            | PERF_SAMPLE_TRANSACTION
            | PERF_SAMPLE_REGS_INTR
            | PERF_SAMPLE_PHYS_ADDR;
        let ev = open(all, 0, 0, 5);
        ev.get_vmo(0, 2 * PAGE_SIZE).unwrap();
        let id = ev.inner.lock().id;
        ev.record_sample(11, 22, 4, 0xabc_def, 777);
        let b = written(&ev);

        let mut at = 8; // past the header
        assert_eq!(take64(&b, &mut at), id, "identifier");
        assert_eq!(take64(&b, &mut at), 0xabc_def, "ip");
        assert_eq!(u32_at(&b, at), 11, "pid");
        assert_eq!(u32_at(&b, at + 4), 22, "tid");
        at += 8;
        assert_eq!(take64(&b, &mut at), 777, "time");
        assert_eq!(take64(&b, &mut at), 0, "addr");
        assert_eq!(take64(&b, &mut at), id, "id");
        assert_eq!(take64(&b, &mut at), id, "stream_id");
        assert_eq!(u32_at(&b, at), 4, "cpu");
        assert_eq!(u32_at(&b, at + 4), 0, "cpu reserved word");
        at += 8;
        assert_eq!(take64(&b, &mut at), 5, "period");
        assert_eq!(take64(&b, &mut at), 5, "read: the count, one period in");
        assert_eq!(take64(&b, &mut at), 0, "callchain nr");
        assert_eq!(u32_at(&b, at), 4, "raw size");
        at += 8;
        assert_eq!(take64(&b, &mut at), 0, "branch stack nr");
        assert_eq!(take64(&b, &mut at), 0, "regs_user abi");
        assert_eq!(take64(&b, &mut at), 0, "stack_user size");
        assert_eq!(take64(&b, &mut at), 0, "weight");
        assert_eq!(take64(&b, &mut at), 0, "data_src");
        assert_eq!(take64(&b, &mut at), 0, "transaction");
        assert_eq!(take64(&b, &mut at), 0, "regs_intr abi");
        assert_eq!(take64(&b, &mut at), 0, "phys_addr");
        assert_eq!(at, b.len(), "nothing left over, nothing missing");
    }

    /// `{ u32 size; char data[size]; }`, and the consumer walks it as
    /// `sizeof(u32) + size`. The kernel writes eight bytes with `size` set to
    /// `sizeof(u32)`; a `size` of zero writes the same eight and tells the
    /// consumer to skip four, putting every later field four bytes out.
    #[test]
    fn a_raw_sample_accounts_for_the_bytes_it_writes() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(PERF_SAMPLE_RAW | PERF_SAMPLE_PHYS_ADDR, 1);
        ev.record_sample(1, 1, 0, 0, 0);
        let b = written(&ev);
        let raw_size = u32_at(&b, 8) as usize;
        assert_eq!(
            8 + 4 + raw_size,
            b.len() - 8,
            "a consumer walking the raw field must land on phys_addr"
        );
    }

    #[test]
    fn a_short_attr_reads_as_zeros_instead_of_running_off_the_end() {
        for len in [0usize, 1, 7, 8, 20, 41] {
            let ev = PerfEvent::new(&alloc::vec![0xffu8; len], -1, -1, OpenFlags::empty());
            let inner = ev.inner.lock();
            // Only whole fields inside the image are read; a field that would
            // straddle the end reads as zero rather than as part of it.
            if len < ATTR_OFF_SAMPLE_TYPE + 8 {
                assert_eq!(inner.sample_type, 0, "len {}", len);
            }
            assert!(inner.period >= 1, "len {}", len);
        }
    }

    #[test]
    fn frequency_mode_reports_a_period_of_one() {
        // Sampling is timer-driven here, so a target Hz cannot be honoured;
        // reporting 1 keeps perf's own rate arithmetic consistent.
        assert_eq!(open(0, 0, ATTR_FLAG_FREQ, 4000).inner.lock().period, 1);
        assert_eq!(open(0, 0, 0, 4000).inner.lock().period, 4000);
        // A period of zero would make every sample count for nothing.
        assert_eq!(open(0, 0, 0, 0).inner.lock().period, 1);
    }

    #[test]
    fn an_event_opened_disabled_stays_quiet_until_it_is_enabled() {
        let _turn = SAMPLER_TURN.lock();
        let ev = PerfEvent::new(
            &attr(PERF_SAMPLE_IP, 0, ATTR_FLAG_DISABLED, 1),
            -1,
            -1,
            OpenFlags::empty(),
        );
        assert!(!ev.inner.lock().enabled);
        ev.get_vmo(0, 2 * PAGE_SIZE).unwrap();
        ev.record_sample(1, 1, 0, 0x1000, 0);
        assert!(written(&ev).is_empty());
        assert_eq!(ev.inner.lock().count, 0);

        ev.ioctl(PERF_EVENT_IOC_ENABLE, 0, 0, 0).unwrap();
        ev.record_sample(1, 1, 0, 0x1000, 0);
        let after_enable = written(&ev).len();
        assert!(after_enable > 0);

        ev.ioctl(PERF_EVENT_IOC_DISABLE, 0, 0, 0).unwrap();
        ev.record_sample(1, 1, 0, 0x1000, 0);
        assert_eq!(written(&ev).len(), after_enable);
    }

    /// `perf stat` opens an event, never maps it, and reads the count. That
    /// count has to move for samples that had nowhere to be written.
    #[async_std::test]
    async fn a_counting_event_with_no_ring_still_counts() {
        let ev = open(0, 0, 0, 3);
        for _ in 0..4 {
            ev.record_sample(1, 1, 0, 0x20, 0);
        }
        assert_eq!(ev.inner.lock().data_head, 0);
        let mut buf = [0u8; 8];
        assert_eq!(ev.read(&mut buf).await.unwrap(), 8);
        assert_eq!(
            u64::from_ne_bytes(buf),
            12,
            "four samples at a period of three"
        );
    }

    #[async_std::test]
    async fn read_format_adds_its_fields_and_a_short_buffer_is_refused() {
        let ev = open(0, PERF_FORMAT_TOTAL_TIME_ENABLED | PERF_FORMAT_ID, 0, 1);
        ev.record_sample(1, 1, 0, 0x20, 0);
        let id = ev.inner.lock().id;

        let mut buf = [0u8; 24];
        assert_eq!(ev.read(&mut buf).await.unwrap(), 24);
        assert_eq!(u64_at(&buf, 0), 1, "count");
        // Not zero: a zero `time_running` is what `perf stat` prints as
        // `<not counted>`, so both time fields used to throw the count away.
        assert_ne!(u64_at(&buf, 8), 0, "time enabled");
        assert_eq!(u64_at(&buf, 16), id, "id");

        // A disabled event stops the clock, and enabling it again does not
        // lose what it had already run.
        let before = ev.inner.lock().time_ns();
        ev.ioctl(PERF_EVENT_IOC_DISABLE, 0, 0, 0).unwrap();
        let stopped = ev.inner.lock().time_ns();
        assert!(stopped >= before, "the clock does not go backwards");
        assert_eq!(stopped, ev.inner.lock().time_ns(), "and it is stopped");
        ev.ioctl(PERF_EVENT_IOC_ENABLE, 0, 0, 0).unwrap();
        assert!(ev.inner.lock().time_ns() >= stopped, "and it carries on");

        // Half of a u64 is not a smaller count, it is a wrong one. ENOSPC, as
        // `__perf_read` answers it.
        let mut short = [0u8; 16];
        assert_eq!(ev.read(&mut short).await.unwrap_err(), LxError::ENOSPC);
        assert_eq!(short, [0u8; 16]);
    }

    #[test]
    fn the_control_page_tells_the_consumer_where_the_data_is() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(0, 4);
        let inner = ev.inner.lock();
        let vmo = &inner.ring.as_ref().unwrap().vmo;
        let rd = |off: usize| {
            let mut b = [0u8; 8];
            let _ = vmo.read(off, &mut b);
            u64::from_ne_bytes(b)
        };
        assert_eq!(rd(PC_DATA_OFFSET), PAGE_SIZE as u64);
        assert_eq!(rd(PC_DATA_SIZE), 4 * PAGE_SIZE as u64);
        assert_eq!(rd(PC_DATA_HEAD), 0);
        assert_eq!(rd(PC_DATA_TAIL), 0);
    }

    /// perf wraps the ring with `offset & (data_size - 1)`, so a data region
    /// that is not a power of two in pages makes it read where nothing was
    /// written — quietly, as samples that never happened.
    #[test]
    fn a_ring_that_is_not_a_power_of_two_in_data_pages_is_refused() {
        for data_pages in [3usize, 5, 6, 7, 9] {
            let ev = open(0, 0, 0, 1);
            assert_eq!(
                ev.get_vmo(0, (data_pages + 1) * PAGE_SIZE).unwrap_err(),
                LxError::EINVAL,
                "{} data pages",
                data_pages
            );
        }
        for data_pages in [0usize, 1, 2, 4, 8] {
            let ev = open(0, 0, 0, 1);
            assert!(
                ev.get_vmo(0, (data_pages + 1) * PAGE_SIZE).is_ok(),
                "{} data pages",
                data_pages
            );
        }
        let ev = open(0, 0, 0, 1);
        assert_eq!(
            ev.get_vmo(PAGE_SIZE, 2 * PAGE_SIZE).unwrap_err(),
            LxError::EINVAL
        );
        assert_eq!(ev.get_vmo(0, PAGE_SIZE - 1).unwrap_err(), LxError::EINVAL);
    }

    /// A one-page mapping is the control page alone. GZDoom maps exactly that
    /// to read the clock calibration, and checks the result against null
    /// rather than `MAP_FAILED`, so refusing it made the game dereference
    /// `(void*)-1`.
    #[test]
    fn a_control_page_only_mapping_is_allowed_and_holds_no_samples() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(PERF_SAMPLE_IP, 0);
        assert_eq!(ev.inner.lock().ring.as_ref().unwrap().data_size, 0);
        ev.record_sample(1, 1, 0, 0x1000, 0);
        let inner = ev.inner.lock();
        assert_eq!(inner.data_head, 0);
        assert_eq!(inner.count, 1, "the count moves even with nowhere to write");
        assert_eq!(inner.lost, 0, "a ring with no data region loses nothing");
    }

    #[test]
    fn a_full_ring_loses_the_sample_rather_than_overwrite_what_is_unread() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(PERF_SAMPLE_IP, 1);
        let record = 16; // header + ip
        for i in 0..(PAGE_SIZE / record) {
            ev.record_sample(1, 1, 0, i as u64, 0);
        }
        assert_eq!(ev.inner.lock().data_head, PAGE_SIZE as u64);
        assert_eq!(ev.inner.lock().lost, 0);

        ev.record_sample(1, 1, 0, 0xffff, 0);
        assert_eq!(ev.inner.lock().lost, 1);
        assert_eq!(
            ev.inner.lock().data_head,
            PAGE_SIZE as u64,
            "a lost sample must not move the head"
        );

        // The consumer catches up; the next record goes back to the start.
        set_tail(&ev, PAGE_SIZE as u64);
        ev.record_sample(1, 1, 0, 0xabcd, 0);
        let mut b = [0u8; 16];
        let inner = ev.inner.lock();
        let _ = inner.ring.as_ref().unwrap().vmo.read(PAGE_SIZE, &mut b);
        assert_eq!(u64_at(&b, 8), 0xabcd);
        assert_eq!(inner.data_head, PAGE_SIZE as u64 + record as u64);
    }

    #[test]
    fn a_record_that_reaches_the_end_of_the_ring_is_written_in_two_halves() {
        let _turn = SAMPLER_TURN.lock();
        // 24-byte records over a 4096-byte region: 170 fit, leaving 16 bytes.
        let ev = mapped(PERF_SAMPLE_IP | PERF_SAMPLE_TIME, 1);
        let record = 24;
        let fits = PAGE_SIZE / record;
        for i in 0..fits {
            ev.record_sample(1, 1, 0, i as u64, i as u64);
        }
        let head = ev.inner.lock().data_head;
        assert_eq!(head, (fits * record) as u64);
        set_tail(&ev, head);

        ev.record_sample(1, 1, 0, 0x1234, 0x5678);
        let inner = ev.inner.lock();
        let vmo = &inner.ring.as_ref().unwrap().vmo;
        // Header and ip at the end of the region, time back at its start.
        let mut tail_half = [0u8; 16];
        let _ = vmo.read(PAGE_SIZE + head as usize % PAGE_SIZE, &mut tail_half);
        assert_eq!(u64_at(&tail_half, 8), 0x1234);
        let mut head_half = [0u8; 8];
        let _ = vmo.read(PAGE_SIZE, &mut head_half);
        assert_eq!(u64::from_ne_bytes(head_half), 0x5678);
        assert_eq!(inner.data_head, head + record as u64);
        assert_eq!(inner.lost, 0);
    }

    /// The readiness flag is sticky and only a new sample sets it, while the
    /// consumer drains the ring in its own mapping. A flag left set over an
    /// empty ring makes `async_poll`'s wait return at once, every time.
    #[test]
    fn a_drained_ring_stops_reporting_itself_readable() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(PERF_SAMPLE_IP, 1);
        assert!(!ev.poll(PollEvents::empty()).unwrap().read);

        ev.record_sample(1, 1, 0, 0x20, 0);
        assert!(ev.poll(PollEvents::empty()).unwrap().read);
        assert!(!(ev.eventbus.lock().events() & Event::READABLE).is_empty());

        let head = ev.inner.lock().data_head;
        // The consumer never sees this side's bookkeeping, only the head
        // published in the control page it shares.
        {
            let inner = ev.inner.lock();
            let mut b = [0u8; 8];
            let _ = inner.ring.as_ref().unwrap().vmo.read(PC_DATA_HEAD, &mut b);
            assert_eq!(u64::from_ne_bytes(b), head, "the head has to be published");
        }
        set_tail(&ev, head);
        assert!(!ev.poll(PollEvents::empty()).unwrap().read);
        assert!(
            (ev.eventbus.lock().events() & Event::READABLE).is_empty(),
            "a flag nobody clears makes the wait spin instead of parking"
        );
    }

    #[test]
    fn the_ioctls_perf_sends() {
        let _turn = SAMPLER_TURN.lock();
        let ev = mapped(PERF_SAMPLE_IP, 1);
        ev.record_sample(1, 1, 0, 0x20, 0);
        assert_eq!(ev.inner.lock().count, 1);
        ev.ioctl(PERF_EVENT_IOC_RESET, 0, 0, 0).unwrap();
        assert_eq!(ev.inner.lock().count, 0);

        // REFRESH carries a count perf expects to be honoured; it is treated
        // as a plain enable, which is why a disabled event comes back up.
        ev.ioctl(PERF_EVENT_IOC_DISABLE, 0, 0, 0).unwrap();
        ev.ioctl(PERF_EVENT_IOC_REFRESH, 1, 0, 0).unwrap();
        assert!(ev.inner.lock().enabled);

        // Grouping and filters are accepted so perf does not bail out.
        assert_eq!(ev.ioctl(PERF_EVENT_IOC_SET_OUTPUT, 0, 0, 0), Ok(0));
        assert_eq!(ev.ioctl(PERF_EVENT_IOC_SET_FILTER, 0, 0, 0), Ok(0));
        // A null argument is a pointer that cannot be followed, and both of
        // these follow one: `copy_from_user`/`copy_to_user` on it is `EFAULT`,
        // and answering 0 told perf its period had changed, or that the id it
        // was about to demultiplex by had been written.
        assert_eq!(
            ev.ioctl(PERF_EVENT_IOC_PERIOD, 0, 0, 0),
            Err(LxError::EFAULT)
        );
        assert_eq!(ev.ioctl(PERF_EVENT_IOC_ID, 0, 0, 0), Err(LxError::EFAULT));
        // The period it does read has to be one: 0 means "do not sample by
        // count", and turning it into 1 is the fastest rate there is.
        let period = ev.inner.lock().period;
        assert_ne!(period, 0);
        assert_eq!(ev.ioctl(0x1234, 0, 0, 0), Err(LxError::ENOTTY));
    }

    /// An event bound to one pid and cpu, with `data_pages` data pages.
    fn mapped_on(sample_type: u64, data_pages: usize, pid: i32, cpu: i32) -> Arc<PerfEvent> {
        let ev = PerfEvent::new(&attr(sample_type, 0, 0, 1), pid, cpu, OpenFlags::empty());
        ev.get_vmo(0, (data_pages + 1) * PAGE_SIZE).unwrap();
        ev
    }

    // ── what the event counts of its own running time ──────────────────

    #[test]
    fn the_stretch_an_event_is_enabled_for_is_the_time_it_reports() {
        // `perf stat` divides by this: `perf_counts_values__scale` prints
        // `<not counted>` for a zero, and scales the count by
        // enabled/running otherwise. A stretch that restarts, never opens, or
        // banks the wrong number is a rate nobody asked for.
        assert!(now_ns() > 0, "the monotonic clock has not started");

        // Opening disabled opens no stretch; opening enabled opens one.
        let off = open(0, 0, 1, 1);
        assert!(off.inner.lock().enabled_since.is_none());
        let on = open(0, 0, 0, 1);
        assert!(on.inner.lock().enabled_since.is_some());

        // Enabling opens the stretch.
        off.inner.lock().set_enabled(true);
        assert!(
            off.inner.lock().enabled_since.is_some(),
            "enabled and not counting"
        );

        // A stretch that started at instant zero is the whole clock so far.
        on.inner.lock().enabled_since = Some(0);
        on.inner.lock().time_enabled = 0;
        assert!(on.inner.lock().time_ns() > 0);

        // Enabling again keeps it: restarting would lose the time already run.
        on.inner.lock().set_enabled(true);
        assert_eq!(
            on.inner.lock().enabled_since,
            Some(0),
            "the stretch restarted"
        );

        // Disabling banks the time RUN, not the instant it started.
        on.inner.lock().set_enabled(false);
        assert!(
            on.inner.lock().time_enabled > 0,
            "the stretch banked nothing"
        );
        assert!(on.inner.lock().enabled_since.is_none());

        // And a reset leaves an enabled event still counting.
        let ev = open(0, 0, 0, 1);
        ev.ioctl(PERF_EVENT_IOC_RESET, 0, 0, 0).unwrap();
        assert!(
            ev.inner.lock().enabled_since.is_some(),
            "a reset stopped the clock of an event that is still running"
        );
        assert_eq!(ev.inner.lock().time_enabled, 0);
    }

    // ── the numbers of the ABI, written out rather than referred to ────

    #[test]
    fn the_attr_fields_are_where_perf_puts_them() {
        // Written as literals on purpose: a test that indexes the attr with
        // the same constant the code does agrees with it whatever it says.
        // `perf_event_attr` is `{ u32 type; u32 size; u64 config;
        // u64 sample_period; u64 sample_type; u64 read_format; u64 flags; }`,
        // and the flags word carries `disabled` at bit 0 and `freq` at bit 10.
        let with = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut a = alloc::vec![0u8; 128];
            f(&mut a);
            PerfEvent::new(&a, -1, -1, OpenFlags::empty())
        };
        assert!(
            !with(&|a| a[40] = 1).inner.lock().enabled,
            "disabled is bit 0 of byte 40"
        );
        assert!(with(&|a| a[40] = 0).inner.lock().enabled);

        // freq is bit 10, so byte 41 bit 2; in freq mode the period reported
        // is 1 whatever Hz was asked for.
        let freq = with(&|a| {
            a[41] = 1 << 2;
            a[16..24].copy_from_slice(&4000u64.to_ne_bytes());
        });
        assert_eq!(freq.inner.lock().period, 1);
        let by_count = with(&|a| a[16..24].copy_from_slice(&4000u64.to_ne_bytes()));
        assert_eq!(by_count.inner.lock().period, 4000, "period is at byte 16");

        let st = with(&|a| a[24..32].copy_from_slice(&0x1234u64.to_ne_bytes()));
        assert_eq!(
            st.inner.lock().sample_type,
            0x1234,
            "sample_type is at byte 24"
        );
        let rf = with(&|a| a[32..40].copy_from_slice(&0x5678u64.to_ne_bytes()));
        assert_eq!(
            rf.inner.lock().read_format,
            0x5678,
            "read_format is at byte 32"
        );
    }

    #[test]
    fn an_attr_that_ends_exactly_on_its_last_field_is_read_whole() {
        // The syscall clamps `attr.size` to at least 64, but the guard here is
        // `off + width <= len`, and the only image that tells `<=` from `<` is
        // one that ends on the very last byte of a field.
        let read = |len: usize, off: usize, byte: u8| {
            let mut a = alloc::vec![0u8; len];
            a[off] = byte;
            PerfEvent::new(&a, -1, -1, OpenFlags::empty())
        };
        // sample_type is bytes 24..32: 32 is exactly enough, 31 is one short.
        assert_eq!(read(32, 24, 0xff).inner.lock().sample_type, 0xff);
        assert_eq!(
            read(31, 24, 0xff).inner.lock().sample_type,
            0,
            "a field that straddles the end reads as zero, not as part of it"
        );
        // and the flags word is bytes 40..48.
        assert!(!read(48, 40, 1).inner.lock().enabled);
        assert!(
            read(47, 40, 1).inner.lock().enabled,
            "no flags word, no disabled bit"
        );
        // `type` is the one u32, bytes 0..4.
        assert_eq!(read(4, 0, 0xab).inner.lock()._type, 0xab);
        assert_eq!(read(3, 0, 0xab).inner.lock()._type, 0);
    }

    #[test]
    fn a_record_header_carries_the_numbers_a_consumer_switches_on() {
        let _turn = SAMPLER_TURN.lock();
        // `PERF_RECORD_SAMPLE` is 9 and `PERF_RECORD_MISC_USER` is 2 in perf's
        // own ABI; unmodified perf reads the first to know what this record is
        // and the second to attribute the sample to user space.
        let ev = mapped(PERF_SAMPLE_IP, 1);
        ev.record_sample(1, 2, 0, 0x30, 0);
        let b = written(&ev);
        assert_eq!(u32_at(&b, 0), 9);
        assert_eq!(u16::from_ne_bytes(b[4..6].try_into().unwrap()), 2);
    }

    #[async_std::test]
    async fn the_read_format_bits_are_the_ones_perf_asks_with() {
        // `evsel__config` sets TOTAL_TIME_ENABLED|TOTAL_TIME_RUNNING on every
        // event it opens, and PERF_FORMAT_ID when it needs to tell a group's
        // samples apart. They are bits 0, 1 and 2.
        let mut buf = [0u8; 64];
        for (rf, words) in [(0u64, 1usize), (1, 2), (2, 2), (3, 3), (4, 2), (7, 4)] {
            let ev = open(0, rf, 0, 1);
            let n = ev.read(&mut buf).await.unwrap();
            assert_eq!(n, words * 8, "read_format {}", rf);
        }
        // With ID alone the second word is the id, not a time.
        let ev = open(0, 4, 0, 1);
        let n = ev.read(&mut buf).await.unwrap();
        assert_eq!(n, 16);
        assert_eq!(u64_at(&buf, 8), ev.inner.lock().id);
    }

    #[test]
    fn the_ioctl_numbers_perf_actually_sends() {
        let _turn = SAMPLER_TURN.lock();
        // ENABLE, DISABLE and RESET are 0x2400, 0x2401 and 0x2403; an event
        // that answers ENOTTY to them is one `perf record` gives up on.
        let ev = mapped(PERF_SAMPLE_IP, 1);
        ev.ioctl(0x2401, 0, 0, 0).unwrap();
        assert!(!ev.inner.lock().enabled, "0x2401 is DISABLE");
        ev.ioctl(0x2400, 0, 0, 0).unwrap();
        assert!(ev.inner.lock().enabled, "0x2400 is ENABLE");
        ev.record_sample(1, 1, 0, 0x40, 0);
        assert_eq!(ev.inner.lock().count, 1);
        ev.ioctl(0x2403, 0, 0, 0).unwrap();
        assert_eq!(ev.inner.lock().count, 0, "0x2403 is RESET");
    }

    // ── the ring, the period and the mapping ───────────────────────────

    #[test]
    fn a_second_record_lands_after_the_first_and_not_over_it() {
        let _turn = SAMPLER_TURN.lock();
        // The write position comes from the producer's own head. Taking it
        // from `data_tail` — the word the consumer moves — puts every record
        // at the same place until perf drains, so a ring that looks full of
        // samples holds one.
        let ev = mapped(PERF_SAMPLE_IP, 1);
        ev.record_sample(1, 1, 0, 0xaaaa, 0);
        let first_len = written(&ev).len();
        ev.record_sample(1, 1, 0, 0xbbbb, 0);
        let b = written(&ev);
        assert_eq!(b.len(), 2 * first_len, "two records, two record lengths");
        assert_eq!(u64_at(&b, 8), 0xaaaa, "the first record was overwritten");
        assert_eq!(u64_at(&b, first_len + 8), 0xbbbb);
    }

    #[test]
    fn the_period_ioctl_refuses_zero_and_keeps_what_it_is_given() {
        let _turn = SAMPLER_TURN.lock();
        // `_perf_event_period` is `if (!value) return -EINVAL;`. Turning a 0
        // into a 1 is the fastest rate there is, out of a value that means
        // "stop sampling by count"; and a period the caller did not ask for is
        // `perf record -F` told its request was honoured when it was not.
        let ev = mapped(PERF_SAMPLE_PERIOD, 1);
        let zero: u64 = 0;
        assert_eq!(
            ev.ioctl(PERF_EVENT_IOC_PERIOD, &zero as *const u64 as usize, 0, 0),
            Err(LxError::EINVAL)
        );
        let want: u64 = 7;
        assert_eq!(
            ev.ioctl(PERF_EVENT_IOC_PERIOD, &want as *const u64 as usize, 0, 0),
            Ok(0)
        );
        assert_eq!(ev.inner.lock().period, 7);
        ev.record_sample(1, 1, 0, 0, 0);
        assert_eq!(ev.inner.lock().count, 7, "a sample counts for one period");
        assert_eq!(u64_at(&written(&ev), 8), 7, "and the record says which");
    }

    #[test]
    fn a_mapping_that_wants_data_pages_is_not_given_a_control_page_only_ring() {
        // A one-page mapping is legal and holds no samples; handing that ring
        // back to `perf`, which mapped data pages, would leave it reading a
        // region this side never writes.
        let ev = open(PERF_SAMPLE_IP, 0, 0, 1);
        let _control_only = ev.get_vmo(0, PAGE_SIZE).unwrap();
        assert_eq!(ev.inner.lock().ring.as_ref().unwrap().data_size, 0);
        let big = ev.get_vmo(0, 2 * PAGE_SIZE).unwrap();
        assert!(big.len() >= 2 * PAGE_SIZE);
        assert_eq!(
            ev.inner.lock().ring.as_ref().unwrap().data_size,
            PAGE_SIZE,
            "the data-page mapping was answered with the control-only ring"
        );
        // The other way round the ring is reused: it is big enough.
        let again = ev.get_vmo(0, PAGE_SIZE).unwrap();
        assert!(
            Arc::ptr_eq(&again, &big),
            "a ring big enough was built again"
        );
    }

    #[test]
    fn a_sample_reaches_the_events_it_matches_and_goes_on_reaching_them() {
        let _turn = SAMPLER_TURN.lock();
        // `sample_user` runs from the timer-interrupt return path and is the
        // only caller of `record_sample`. Two things it has to get right: who
        // the sample is for, and that the cheap flag it checks first stays set
        // while anything is still profiling.
        let mine = mapped_on(PERF_SAMPLE_IP, 1, 77, 3);
        let other = mapped_on(PERF_SAMPLE_IP, 1, 78, 3);
        sample_user(77, 77, 3, 0xaaaa);
        assert_eq!(mine.inner.lock().count, 1);
        assert_eq!(
            other.inner.lock().count,
            0,
            "an event opened for another process took this one's sample"
        );
        sample_user(77, 77, 3, 0xbbbb);
        assert_eq!(
            mine.inner.lock().count,
            2,
            "sampling stopped while the event was still enabled"
        );
        assert_eq!(other.inner.lock().count, 0);
    }

    #[test]
    fn an_event_only_takes_the_samples_it_was_opened_for() {
        // -1 is "any" on either side.
        assert!(event_matches(-1, -1, 3, 77));
        assert!(event_matches(3, -1, 3, 77));
        assert!(!event_matches(3, -1, 4, 77));
        assert!(event_matches(-1, 77, 4, 77));
        assert!(!event_matches(-1, 77, 4, 78));
        assert!(event_matches(3, 77, 3, 77));
        // A pid of 0 reaches here only if the syscall failed to resolve it to
        // the calling process, and then it matches nothing ever sampled.
        assert!(!event_matches(-1, 0, 0, 77));
    }

    #[async_std::test]
    async fn a_perf_fd_is_not_writable() {
        let ev = open(0, 0, 0, 1);
        assert_eq!(ev.write(b"x").unwrap_err(), LxError::EINVAL);
        assert!(!ev.poll(PollEvents::empty()).unwrap().write);
        // read_at ignores the offset: the fd has no seekable contents.
        let mut buf = [0u8; 8];
        assert_eq!(ev.read_at(4096, &mut buf).await.unwrap(), 8);
    }
}
