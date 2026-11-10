//! [`ECLIPSE_BLOCK_SERVE`](super::ECLIPSE_BLOCK_SERVE): a disk that a process
//! drives, given to the kernel as a block device of its own.
//!
//! The process (a userspace driver such as `tools/zx-virtio-blk`) keeps one
//! end of a FIFO of 32-byte block messages and hands the kernel the other,
//! with a data window: a VMO both sides reach, which the driver's device
//! reads and writes directly. The kernel sees a [`BlockScheme`] like any
//! other and puts it under `/dev` through the hot-plug path, so everything
//! above it -- the page cache, the filesystems, `pread` on the node -- works
//! unchanged.
//!
//! A request is one message out and the same message back with its status:
//! the data is copied between the caller's buffer and the window on this
//! side, and the driver never sees kernel memory.
//!
//! [`BlockScheme`] is synchronous and bare metal has no `block_on`, so the
//! wait for the reply parks the calling executor with
//! [`yield_now_sync`](kernel_hal::thread::yield_now_sync) between looks,
//! as the NFS transport does, and opens a window for interrupts each time
//! (see `between_looks`).
//!
//! The window is cut into slots, one request in flight on each, up to the
//! driver's FIFO depth: the callers of the moment -- several threads, or one
//! read longer than a slot -- each take the slots they need and put their
//! messages on the FIFO together, so the driver sees them in one read and
//! puts them on its device at once. Whoever waits reads the replies, all of
//! them, and marks each slot answered; a reply's slot is in its `reqid`.
//!
//! `pread` on the node does not wait like that: it takes
//! [`read_block_async`](BlockScheme::read_block_async), and its task sleeps
//! on the FIFO until the reply comes. Keeping the CPU in a loop that yields
//! costs a fresh executor each time the task gives the CPU back mid-poll --
//! its stack, guard pages and TLB shootdown -- which with sixteen readers was
//! most of the machine's time.
//!
//! What this trusts: the driver is a process the system started for the
//! purpose. It can answer wrongly (the data is whatever it put in the window)
//! or slowly (a request gives up after [`REPLY_TIMEOUT_MS`]); it cannot make
//! the kernel read or write anything but the window.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::convert::TryInto;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use kernel_hal::drivers::prelude::{Device, DeviceError, DeviceResult};
use kernel_hal::drivers::scheme::{BlockReadFuture, BlockScheme, IoSnapshot, IoStats, Scheme};
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, Rights, Signal};
use zircon_object::task::Process;
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

/// One FIFO element: `op, reqid, sectors, buf_off` (u32 each), `sector`
/// (u64), `status` (i64, the driver's answer). The driver's own layout.
pub(super) const BLOCK_MSG: usize = 32;
const OP_READ: u32 = 1;
const OP_WRITE: u32 = 2;
const SECTOR: usize = 512;
/// How long a request waits for its reply before it is an I/O error.
pub(super) const REPLY_TIMEOUT_MS: u64 = 30_000;
/// The smallest slot worth serving through: one page.
const MIN_SLOT: usize = 4096;
/// The most slots, and so the most requests in flight: the driver's FIFO
/// depth. A power of two: a `reqid`'s low bits are its slot.
const SLOTS: usize = 16;
const SLOT_BITS: u32 = SLOTS.trailing_zeros();

/// How long a lone request looks for its reply before it sleeps: a sleep and
/// the wake behind it cost more than a driver on another CPU takes to answer.
const LOOK_US: u64 = 300;
/// The most sleeps in a row after looks that found nothing.
const LOOK_SKIP_MAX: u32 = 64;

/// What follows a look: how many of the next waits sleep at once, and the
/// run of them after the next miss. A look that pays keeps every wait
/// looking; one that does not (one CPU: the driver cannot run meanwhile; a
/// slow device) doubles the sleeps before the next try, up to
/// [`LOOK_SKIP_MAX`].
pub(super) fn after_look(hit: bool, backoff: u32) -> (u32, u32) {
    if hit {
        (0, 1)
    } else {
        let run = backoff.clamp(1, LOOK_SKIP_MAX);
        (run, (run * 2).min(LOOK_SKIP_MAX))
    }
}

/// A slot's answer, as [`ServedDisk::answers`] holds it.
const PENDING: u8 = 0;
const ANSWERED: u8 = 1;
const FAILED: u8 = 2;

/// A disk behind a FIFO.
pub(super) struct ServedDisk {
    fifo: Arc<Fifo>,
    window: Arc<VmObject>,
    /// How many slots the window has, at most [`SLOTS`].
    slots: usize,
    /// Bytes of the window each slot owns, a whole number of sectors.
    slot_len: usize,
    sectors: u64,
    /// The slots no request holds, one bit each.
    free: AtomicU32,
    /// The `reqid` each slot waits for, and what came back for it.
    expect: [AtomicU32; SLOTS],
    answers: [AtomicU8; SLOTS],
    /// The task asleep on each slot's answer, if any.
    wakers: [lock::Mutex<Option<Waker>>; SLOTS],
    /// The driver closed its end: nothing will be answered again.
    dead: AtomicBool,
    next_id: AtomicU32,
    /// Waits left to sleep through before the next look, and the run after
    /// the next miss: see [`after_look`]. Racy on purpose: a lost update
    /// costs a look or saves one.
    look_skip: AtomicU32,
    look_backoff: AtomicU32,
    stats: IoStats,
}

/// A message, as bytes.
pub(super) fn encode(
    op: u32,
    reqid: u32,
    sectors: u32,
    buf_off: u32,
    sector: u64,
) -> [u8; BLOCK_MSG] {
    let mut m = [0u8; BLOCK_MSG];
    m[0..4].copy_from_slice(&op.to_ne_bytes());
    m[4..8].copy_from_slice(&reqid.to_ne_bytes());
    m[8..12].copy_from_slice(&sectors.to_ne_bytes());
    m[12..16].copy_from_slice(&buf_off.to_ne_bytes());
    m[16..24].copy_from_slice(&sector.to_ne_bytes());
    m
}

/// How a window of `len` bytes is cut: `(slots, bytes per slot)`, or `None`
/// when not even one slot fits.
pub(super) fn cut_window(len: usize) -> Option<(usize, usize)> {
    let slots = (len / MIN_SLOT).min(SLOTS);
    if slots == 0 {
        return None;
    }
    Some((slots, len / slots / SECTOR * SECTOR))
}

/// Up to `want` of the slots in `free`, lowest first.
pub(super) fn pick_slots(free: u32, want: usize) -> u32 {
    let mut left = free;
    let mut take = 0;
    for _ in 0..want {
        if left == 0 {
            break;
        }
        let low = left.isolate_lowest_one();
        take |= low;
        left &= !low;
    }
    take
}

/// The reply's `(reqid, status)`.
pub(super) fn decode(m: &[u8; BLOCK_MSG]) -> (u32, i64) {
    let reqid = u32::from_ne_bytes(m[4..8].try_into().unwrap());
    let status = i64::from_ne_bytes(m[24..32].try_into().unwrap());
    (reqid, status)
}

/// Whether `len` bytes at `sector` are a request this disk can take: whole
/// sectors, at least one, and inside the disk.
pub(super) fn in_range(sector: u64, len: usize, sectors: u64) -> bool {
    let n = (len / SECTOR) as u64;
    len.is_multiple_of(SECTOR) && n != 0 && sector <= sectors && n <= sectors - sector
}

/// What a request does while it waits: let the CPU go, and let it take
/// interrupts.
///
/// Kernel tasks run with interrupts masked. While this loop has the CPU, the
/// interrupt that would wake the driver -- its device's -- has nowhere to
/// land on this CPU, and the driver sleeps through the request until the
/// deadline. So each look opens a window, the same one the idle loop's poll
/// opens, and closes it again: the caller's state is what it was.
fn between_looks() {
    open_interrupt_window();
    kernel_hal::thread::yield_now_sync();
}

/// The interrupt half of [`between_looks`], for a spin that keeps the CPU.
fn open_interrupt_window() {
    use kernel_hal::interrupt::{intr_get, intr_off, intr_on};
    if !intr_get() {
        intr_on();
        core::hint::spin_loop();
        intr_off();
        // And a TLB shootdown another CPU is waiting on: what every spin
        // with interrupts off does.
        kernel_hal::tlb_shootdown_pump();
    } else {
        core::hint::spin_loop();
    }
}

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

/// One chunk of a request: its slot, where in the caller's buffer it
/// starts, and its length.
#[derive(Clone, Copy, Default)]
struct Part {
    slot: usize,
    at: usize,
    len: usize,
}

impl ServedDisk {
    pub(super) fn new(fifo: Arc<Fifo>, window: Arc<VmObject>, sectors: u64) -> ZxResult<Self> {
        let (slots, slot_len) = cut_window(window.len()).ok_or(ZxError::INVALID_ARGS)?;
        if sectors == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        // The FIFO must carry the driver's messages, or every write fails.
        fifo.read_buffer_elems(BLOCK_MSG, 1)?;
        Ok(Self {
            fifo,
            window,
            slots,
            slot_len,
            sectors,
            free: AtomicU32::new(((1u64 << slots) - 1) as u32),
            expect: [const { AtomicU32::new(0) }; SLOTS],
            answers: [const { AtomicU8::new(PENDING) }; SLOTS],
            wakers: [const { lock::Mutex::new(None) }; SLOTS],
            dead: AtomicBool::new(false),
            next_id: AtomicU32::new(0),
            look_skip: AtomicU32::new(0),
            look_backoff: AtomicU32::new(1),
            stats: IoStats::new(),
        })
    }

    /// Read whatever replies have come back and mark their slots. Returns
    /// how many there were.
    fn pump(&self) -> usize {
        let mut back = [0u8; BLOCK_MSG * SLOTS];
        let n = match self.fifo.read(BLOCK_MSG, &mut back, SLOTS) {
            Ok(n) => n,
            Err(ZxError::SHOULD_WAIT) => return 0,
            Err(e) => {
                if !self.dead.swap(true, Ordering::AcqRel) {
                    warn!("served disk: the driver is gone ({e:?})");
                }
                return 0;
            }
        };
        for m in back.as_chunks::<BLOCK_MSG>().0.iter().take(n) {
            let (id, status) = decode(m);
            let slot = (id as usize) & (SLOTS - 1);
            let mine = slot < self.slots
                && self.expect[slot].load(Ordering::Acquire) == id
                && self.answers[slot].load(Ordering::Acquire) == PENDING;
            if !mine {
                // A reply to nothing in flight: a driver answering twice, or
                // late, after its request gave up and left the slot.
                warn!("served disk: an answer to no request ({id}, status {status})");
                continue;
            }
            let answer = if status == 0 { ANSWERED } else { FAILED };
            self.answers[slot].store(answer, Ordering::Release);
            let asleep = self.wakers[slot].lock().take();
            if let Some(w) = asleep {
                w.wake();
            }
        }
        n
    }

    /// Up to `want` free slots, waiting for one while there are none.
    fn claim(&self, want: usize, deadline: u64) -> Option<u32> {
        loop {
            let free = self.free.load(Ordering::Acquire);
            let take = pick_slots(free, want);
            if take != 0 {
                if self
                    .free
                    .compare_exchange(free, free & !take, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(take);
                }
                continue;
            }
            if self.dead.load(Ordering::Acquire) || now_ms() > deadline {
                return None;
            }
            // The holders may be parked: their replies are read by whoever
            // is here.
            if self.pump() == 0 {
                between_looks();
            }
        }
    }

    /// Put `n` messages on the FIFO, all of them.
    fn send(&self, msgs: &[u8], n: usize, deadline: u64) -> DeviceResult {
        let mut sent = 0;
        while sent < n {
            match self
                .fifo
                .write(BLOCK_MSG, &msgs[sent * BLOCK_MSG..n * BLOCK_MSG], n - sent)
            {
                Ok(k) => sent += k,
                Err(ZxError::SHOULD_WAIT) => {}
                Err(e) => {
                    self.dead.store(true, Ordering::Release);
                    warn!("served disk: sending a request: {e:?}");
                    return Err(DeviceError::IoError);
                }
            }
            if sent < n {
                if now_ms() > deadline {
                    warn!("served disk: a request could not be sent in time");
                    return Err(DeviceError::IoError);
                }
                between_looks();
            }
        }
        Ok(())
    }

    /// Wait until every one of `parts` is answered: `Ok` only if all of them
    /// went right.
    fn wait(&self, parts: &[Part], deadline: u64) -> DeviceResult {
        loop {
            let mut pending = false;
            let mut failed = false;
            for p in parts {
                match self.answers[p.slot].load(Ordering::Acquire) {
                    PENDING => pending = true,
                    FAILED => failed = true,
                    _ => {}
                }
            }
            if !pending {
                return if failed {
                    Err(DeviceError::IoError)
                } else {
                    Ok(())
                };
            }
            if self.dead.load(Ordering::Acquire) {
                return Err(DeviceError::IoError);
            }
            if now_ms() > deadline {
                warn!("served disk: a request was not answered in time");
                return Err(DeviceError::IoError);
            }
            if self.pump() == 0 {
                between_looks();
            }
        }
    }

    /// Give back the slots of `parts` that were answered. One still waiting
    /// stays taken for good: the driver may yet write into it.
    fn release(&self, parts: &[Part]) {
        let mut back = 0;
        for p in parts {
            if self.answers[p.slot].load(Ordering::Acquire) != PENDING {
                back |= 1 << p.slot;
            }
        }
        self.free.fetch_or(back, Ordering::AcqRel);
    }

    /// The next piece of a `len`-byte request at `block_id`, `*done` bytes
    /// in: as many slots as are free, filled by `before` (a write) and sent.
    /// Returns the parts in flight; `*done` moves past them.
    fn submit(
        &self,
        op: u32,
        block_id: usize,
        len: usize,
        done: &mut usize,
        deadline: u64,
        before: &mut impl FnMut(usize, usize, usize) -> DeviceResult,
    ) -> Result<([Part; SLOTS], usize), DeviceError> {
        let chunks = (len - *done).div_ceil(self.slot_len).min(self.slots);
        let mut taken = self.claim(chunks, deadline).ok_or(DeviceError::IoError)?;
        let mut parts = [Part::default(); SLOTS];
        let mut msgs = [0u8; BLOCK_MSG * SLOTS];
        let mut k = 0;
        let mut r = Ok(());
        while taken != 0 {
            let slot = taken.trailing_zeros() as usize;
            taken &= taken - 1;
            let part = Part {
                slot,
                at: *done,
                len: (len - *done).min(self.slot_len),
            };
            let off = slot * self.slot_len;
            if r.is_ok() {
                r = before(off, part.at, part.len);
            }
            let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
            let id = (seq << SLOT_BITS) | slot as u32;
            self.answers[slot].store(PENDING, Ordering::Relaxed);
            self.expect[slot].store(id, Ordering::Release);
            let sector = block_id as u64 + (part.at / SECTOR) as u64;
            let m = encode(op, id, (part.len / SECTOR) as u32, off as u32, sector);
            msgs[k * BLOCK_MSG..(k + 1) * BLOCK_MSG].copy_from_slice(&m);
            parts[k] = part;
            k += 1;
            *done += part.len;
        }
        if let Err(e) = r {
            // Nothing went out: every slot is free again.
            for p in &parts[..k] {
                self.answers[p.slot].store(FAILED, Ordering::Release);
            }
            self.release(&parts[..k]);
            return Err(e);
        }
        // On failure some may be on their way: none of these slots comes back.
        self.send(&msgs, k, deadline)?;
        Ok((parts, k))
    }

    /// Empty the answered parts with `after` (a read) and give their slots
    /// back.
    fn finish(
        &self,
        parts: &[Part],
        answered: DeviceResult,
        after: &mut impl FnMut(usize, usize, usize) -> DeviceResult,
    ) -> DeviceResult {
        let mut r = answered;
        if r.is_ok() {
            for p in parts {
                r = after(p.slot * self.slot_len, p.at, p.len);
                if r.is_err() {
                    break;
                }
            }
        }
        self.release(parts);
        r
    }

    /// `len` bytes at `block_id`, in as many slots at once as are free,
    /// waiting with the CPU kept. `before` fills a slot's part of the window
    /// (a write), `after` empties it (a read): `(window offset, offset in the
    /// caller's buffer, length)`.
    fn transfer(
        &self,
        op: u32,
        block_id: usize,
        len: usize,
        mut before: impl FnMut(usize, usize, usize) -> DeviceResult,
        mut after: impl FnMut(usize, usize, usize) -> DeviceResult,
    ) -> DeviceResult {
        if !in_range(block_id as u64, len, self.sectors) {
            debug!("served disk: {len} bytes at sector {block_id} refused");
            return Err(DeviceError::InvalidParam);
        }
        let deadline = now_ms() + REPLY_TIMEOUT_MS;
        let mut done = 0;
        while done < len {
            let (parts, k) = self.submit(op, block_id, len, &mut done, deadline, &mut before)?;
            let parts = &parts[..k];
            let answered = self.wait(parts, deadline);
            self.finish(parts, answered, &mut after)?;
        }
        Ok(())
    }

    /// A read, the task asleep while the driver works: what `pread` on the
    /// node awaits.
    async fn read_async(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        let len = buf.len();
        if !in_range(block_id as u64, len, self.sectors) {
            debug!("served disk: {len} bytes at sector {block_id} refused");
            return Err(DeviceError::InvalidParam);
        }
        let window = &self.window;
        let mut after = |off: usize, at: usize, n: usize| {
            window.read(off, &mut buf[at..at + n]).map_err(|e| {
                warn!("served disk: reading the window: {e:?}");
                DeviceError::IoError
            })
        };
        let deadline = now_ms() + REPLY_TIMEOUT_MS;
        let mut done = 0;
        while done < len {
            let (parts, k) = self.submit(
                OP_READ,
                block_id,
                len,
                &mut done,
                deadline,
                &mut |_, _, _| Ok(()),
            )?;
            let parts = &parts[..k];
            let answered = self.wait_async(parts, deadline).await;
            self.finish(parts, answered, &mut after)?;
        }
        Ok(())
    }

    /// What `parts` came back as: `None` while any is still out.
    fn verdict(&self, parts: &[Part]) -> Option<DeviceResult> {
        let mut failed = false;
        for p in parts {
            match self.answers[p.slot].load(Ordering::Acquire) {
                PENDING => return None,
                FAILED => failed = true,
                _ => {}
            }
        }
        Some(if failed {
            Err(DeviceError::IoError)
        } else {
            Ok(())
        })
    }

    /// The start of a wait: for [`LOOK_US`], keep reading replies instead of
    /// sleeping. Only while these are the only requests out (with more, one
    /// wake brings a batch, and the CPU is better spent on the others), and
    /// only when the last looks paid: see [`after_look`]. `None` when the
    /// answer is still out.
    fn look(&self, parts: &[Part]) -> Option<DeviceResult> {
        let skip = self.look_skip.load(Ordering::Relaxed);
        if skip > 0 {
            self.look_skip.store(skip - 1, Ordering::Relaxed);
            return None;
        }
        let all = ((1u64 << self.slots) - 1) as u32;
        let until = kernel_hal::timer::timer_now() + Duration::from_micros(LOOK_US);
        let found = loop {
            if let Some(r) = self.verdict(parts) {
                break Some(r);
            }
            if self.pump() > 0 {
                continue;
            }
            let out = (!self.free.load(Ordering::Relaxed) & all).count_ones() as usize;
            if out > parts.len() {
                // Not a miss: nothing was learnt about the driver.
                return None;
            }
            if self.dead.load(Ordering::Acquire) || kernel_hal::timer::timer_now() > until {
                break None;
            }
            open_interrupt_window();
        };
        let (skip, backoff) =
            after_look(found.is_some(), self.look_backoff.load(Ordering::Relaxed));
        self.look_skip.store(skip, Ordering::Relaxed);
        self.look_backoff.store(backoff, Ordering::Relaxed);
        found
    }

    /// [`ServedDisk::wait`], asleep: on the FIFO's replies, and on whichever
    /// other waiter reads ours first.
    async fn wait_async(&self, parts: &[Part], deadline: u64) -> DeviceResult {
        if let Some(r) = self.look(parts) {
            return r;
        }
        let fifo: Arc<dyn KernelObject> = self.fifo.clone();
        loop {
            if let Some(r) = self.verdict(parts) {
                return r;
            }
            if self.pump() > 0 {
                continue;
            }
            if self.dead.load(Ordering::Acquire) {
                return Err(DeviceError::IoError);
            }
            if now_ms() > deadline {
                warn!("served disk: a request was not answered in time");
                return Err(DeviceError::IoError);
            }
            Asleep {
                disk: self,
                parts,
                replies: Box::pin(fifo.wait_signal(Signal::READABLE | Signal::PEER_CLOSED)),
                timeout: Box::pin(kernel_hal::thread::sleep_until(Duration::from_millis(
                    deadline,
                ))),
            }
            .await;
        }
    }
}

/// One sleep of [`ServedDisk::wait_async`]: over when a reply is there to
/// read, when another waiter has read one of ours, or at the deadline.
struct Asleep<'a, R, T> {
    disk: &'a ServedDisk,
    parts: &'a [Part],
    replies: Pin<Box<R>>,
    timeout: Pin<Box<T>>,
}

impl<R: Future, T: Future> Future for Asleep<'_, R, T> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Waker first, then the look: a reply read by another waiter between
        // the two is seen here, and one read after wakes this task.
        for p in self.parts {
            *self.disk.wakers[p.slot].lock() = Some(cx.waker().clone());
        }
        if self.disk.verdict(self.parts).is_some() {
            return Poll::Ready(());
        }
        if self.replies.as_mut().poll(cx).is_ready() || self.timeout.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Scheme for ServedDisk {
    fn name(&self) -> &str {
        // The `virtio` family, so the node is a `vdX`: what is behind it is
        // a virtio disk, driven from userspace.
        "virtio-blk-served"
    }
}

impl BlockScheme for ServedDisk {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> DeviceResult {
        let len = buf.len();
        let window = &self.window;
        let r = self.transfer(
            OP_READ,
            block_id,
            len,
            |_, _, _| Ok(()),
            |off, at, n| {
                window.read(off, &mut buf[at..at + n]).map_err(|e| {
                    warn!("served disk: reading the window: {e:?}");
                    DeviceError::IoError
                })
            },
        );
        self.stats.note_read(&r, len);
        r
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> DeviceResult {
        let len = buf.len();
        let window = &self.window;
        let r = self.transfer(
            OP_WRITE,
            block_id,
            len,
            |off, at, n| {
                window.write(off, &buf[at..at + n]).map_err(|e| {
                    warn!("served disk: writing the window: {e:?}");
                    DeviceError::IoError
                })
            },
            |_, _, _| Ok(()),
        );
        self.stats.note_write(&r, len);
        r
    }

    fn read_block_async<'a>(
        &'a self,
        block_id: usize,
        buf: &'a mut [u8],
    ) -> Option<BlockReadFuture<'a>> {
        Some(Box::pin(async move {
            let len = buf.len();
            let r = self.read_async(block_id, buf).await;
            self.stats.note_read(&r, len);
            r
        }))
    }

    fn flush(&self) -> DeviceResult {
        // The driver answers a write once the device has it.
        Ok(())
    }

    fn block_count(&self) -> usize {
        self.sectors as usize
    }

    fn io_stats(&self) -> Option<IoSnapshot> {
        Some(self.stats.snapshot())
    }
}

/// [`ECLIPSE_BLOCK_SERVE`](super::ECLIPSE_BLOCK_SERVE): `args[0]` the FIFO end
/// the kernel keeps, `args[1]` the data window, `args[2]` the disk's size in
/// sectors. Both handles are the kernel's from here on, whatever the answer.
pub(super) fn block_serve(proc: &Arc<Process>, args: &[usize; 6]) -> ZxResult {
    let fifo = proc.get_object_with_rights::<Fifo>(args[0] as u32, Rights::READ | Rights::WRITE);
    let window =
        proc.get_object_with_rights::<VmObject>(args[1] as u32, Rights::READ | Rights::WRITE);
    let (fifo, window) = (fifo?, window?);
    let _ = proc.remove_handle(args[0] as u32);
    let _ = proc.remove_handle(args[1] as u32);
    let disk = ServedDisk::new(fifo, window, args[2] as u64)?;
    if kernel_hal::drivers::hotplug::add(Device::Block(Arc::new(disk))) {
        Ok(())
    } else {
        Err(ZxError::NOT_SUPPORTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_look_that_pays_keeps_every_wait_looking() {
        assert_eq!(after_look(true, 1), (0, 1));
        assert_eq!(after_look(true, LOOK_SKIP_MAX), (0, 1));
    }

    #[test]
    fn misses_in_a_row_double_the_sleeps_up_to_the_cap() {
        let (mut skip, mut backoff) = after_look(false, 1);
        assert_eq!((skip, backoff), (1, 2));
        let mut runs = alloc::vec![skip];
        for _ in 0..10 {
            (skip, backoff) = after_look(false, backoff);
            runs.push(skip);
        }
        assert_eq!(&runs[..7], &[1, 2, 4, 8, 16, 32, 64]);
        assert!(runs[7..].iter().all(|&r| r == LOOK_SKIP_MAX));
        // And a backoff nobody set still sleeps at least once.
        assert_eq!(after_look(false, 0).0, 1);
    }

    #[test]
    fn a_message_is_the_drivers_layout() {
        let m = encode(OP_READ, 7, 8, 0, 0x1_0000_0002);
        assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()), OP_READ);
        assert_eq!(u32::from_ne_bytes(m[8..12].try_into().unwrap()), 8);
        assert_eq!(u32::from_ne_bytes(m[12..16].try_into().unwrap()), 0);
        assert_eq!(
            u64::from_ne_bytes(m[16..24].try_into().unwrap()),
            0x1_0000_0002
        );
        let mut back = m;
        back[24..32].copy_from_slice(&(-40i64).to_ne_bytes());
        assert_eq!(decode(&back), (7, -40));
    }

    #[test]
    fn a_message_carries_its_place_in_the_window() {
        let m = encode(OP_WRITE, 0x31, 8, 3 * 4096, 9);
        assert_eq!(u32::from_ne_bytes(m[12..16].try_into().unwrap()), 3 * 4096);
        assert_eq!(
            decode(&m).0 as usize & (SLOTS - 1),
            1,
            "the slot is in the id"
        );
    }

    #[test]
    fn the_window_is_cut_into_whole_page_slots() {
        assert_eq!(cut_window(64 * 1024), Some((16, 4096)));
        assert_eq!(
            cut_window(1024 * 1024),
            Some((16, 65536)),
            "no more than the FIFO holds"
        );
        assert_eq!(cut_window(3 * 4096 + 100), Some((3, 4096)));
        assert_eq!(cut_window(4096), Some((1, 4096)));
        assert_eq!(cut_window(4095), None);
    }

    #[test]
    fn slots_are_taken_lowest_first_and_only_free_ones() {
        assert_eq!(pick_slots(0b1011_0100, 2), 0b0001_0100);
        assert_eq!(pick_slots(0b1011_0100, 9), 0b1011_0100);
        assert_eq!(pick_slots(0, 3), 0);
        assert_eq!(pick_slots(0b1, 0), 0);
    }

    #[test]
    fn a_request_is_whole_sectors_inside_the_disk() {
        assert!(in_range(0, 512, 1));
        assert!(in_range(8, 4096, 16));
        assert!(!in_range(9, 4096, 16), "one sector past the end");
        assert!(!in_range(0, 0, 16), "nothing to transfer");
        assert!(!in_range(0, 100, 16), "part of a sector");
        assert!(!in_range(17, 512, 16), "starts past the end");
        assert!(!in_range(u64::MAX, 512, 16), "and does not wrap");
    }
}
