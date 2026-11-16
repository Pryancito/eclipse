//! The kernel's end of a served filesystem: the FIFO its requests go out
//! on, the window their names and data pass through, and what is in flight.
//!
//! The VFS is synchronous and a request is made from wherever the VFS is
//! called: a system call, a page fault with the file's cache locked. So the
//! wait for the reply parks the calling executor between looks, as a request
//! to a served disk does (`loader::linux::block_serve`), and opens a window
//! for interrupts each time: the server's own disk may be what it waits on.
//!
//! One request is in flight on each slot of the window. Whoever waits reads
//! the replies, all of them, and marks each slot answered; a reply's slot is
//! in its `reqid`.
//!
//! A link is one server's. When the server dies the link is dead, and says
//! since when: the mount waits that long for the next server's link
//! (`ServedFs::ask`).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use kernel_hal::thread::SyncWait;
use rcore_fs::vfs::FsError;
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, KickOn, Signal};
use zircon_object::vm::VmObject;
use zircon_object::{ZxError, ZxResult};

use super::wire::{self, DATA_AREA, MSG, NAME_AREA, SLOTS, SLOT_LEN};

const SLOT_BITS: u32 = SLOTS.trailing_zeros();

/// Raised on the kernel's end of the FIFO when the link is closed: the
/// link's own end is dropped then, and the server sees its end closed once
/// every other holder has dropped theirs.
pub const LEFT: Signal = Signal::USER_SIGNAL_0;

const PENDING: u8 = 0;
const ANSWERED: u8 = 1;

/// How long a request waits while the server answers nothing, this
/// request or any other. A server that reads a slow disk takes a while;
/// one that says nothing for this long is not going to, and is taken for
/// hung: it is killed, and the mount waits for the next one as it does
/// when a server dies.
const HUNG_AFTER_MS: u64 = 30_000;

/// What ends a server that hangs: its process, killed.
pub(super) type Kill = alloc::boxed::Box<dyn Fn() + Send + Sync>;

pub(super) struct Link {
    /// The kernel's end, until the mount is gone: dropping it is what tells
    /// the server.
    fifo: lock::Mutex<Option<Arc<Fifo>>>,
    window: Arc<VmObject>,
    slots: usize,
    /// The slots no request holds, one bit each.
    free: AtomicU32,
    /// The `reqid` each slot waits for, and what came back for it.
    expect: [AtomicU32; SLOTS],
    state: [AtomicU8; SLOTS],
    status: [AtomicI64; SLOTS],
    ret_a: [AtomicU64; SLOTS],
    ret_b: [AtomicU32; SLOTS],
    /// The server closed its end, or the mount is gone: nothing will be
    /// answered again.
    dead: AtomicBool,
    /// When that was seen, in milliseconds; 0 while it has not been.
    died_at: AtomicU64,
    /// When the server last answered a request, in milliseconds.
    answered_at: AtomicU64,
    /// [`HUNG_AFTER_MS`], or what a test has for it.
    pub hung_after_ms: AtomicU64,
    /// What kills the server when it hangs, once there is something that
    /// can.
    kill: lock::Mutex<Option<Kill>>,
    next_id: AtomicU32,
}

/// A request that was answered, and its slot of the window until this is
/// dropped: what the server put there is read through it.
pub(super) struct Answer {
    link: Arc<Link>,
    slot: usize,
    /// The reply's count: never negative.
    pub count: usize,
    pub a: u64,
    /// The bytes of the data area the server filled.
    pub len: usize,
}

impl Answer {
    /// `buf.len()` bytes of the data area, from `offset`: only what the
    /// server said it filled.
    pub fn data(&self, offset: usize, buf: &mut [u8]) -> Result<(), FsError> {
        if offset
            .checked_add(buf.len())
            .is_none_or(|end| end > self.len)
        {
            return Err(FsError::DeviceError);
        }
        self.link
            .window
            .read(self.slot * SLOT_LEN + NAME_AREA + offset, buf)
            .map_err(|_| FsError::DeviceError)
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        self.link.free.fetch_or(1 << self.slot, Ordering::AcqRel);
    }
}

/// Why a request has no answer.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Fail {
    /// The server's answer: its `errno`, or a request that could not be
    /// made at all.
    Said(FsError),
    /// The server is gone, and nothing says whether it did what was asked.
    Gone,
}

impl From<Fail> for FsError {
    fn from(fail: Fail) -> Self {
        match fail {
            Fail::Said(e) => e,
            Fail::Gone => FsError::DeviceError,
        }
    }
}

pub(super) fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
}

/// What a request does while it waits: let the CPU go, and let it take
/// interrupts. Kernel tasks run with interrupts masked, and the interrupt
/// that wakes the server -- its disk's -- needs somewhere to land.
pub(super) fn between_looks(wait: &mut SyncWait) {
    use kernel_hal::interrupt::{intr_get, intr_off, intr_on};
    if !intr_get() {
        intr_on();
        core::hint::spin_loop();
        intr_off();
        // And a TLB shootdown another CPU is waiting on: what every spin
        // with interrupts off does.
        kernel_hal::tlb_shootdown_pump();
    }
    wait.yield_now();
}

impl Link {
    pub fn new(fifo: Arc<Fifo>, window: Arc<VmObject>) -> ZxResult<Self> {
        let slots = wire::slots_of(window.len()).ok_or(ZxError::INVALID_ARGS)?;
        // The FIFO must carry these messages, or every write fails.
        fifo.read_buffer_elems(MSG, 1)?;
        Ok(Self {
            fifo: lock::Mutex::new(Some(fifo)),
            window,
            slots,
            free: AtomicU32::new(((1u64 << slots) - 1) as u32),
            expect: [const { AtomicU32::new(0) }; SLOTS],
            state: [const { AtomicU8::new(PENDING) }; SLOTS],
            status: [const { AtomicI64::new(0) }; SLOTS],
            ret_a: [const { AtomicU64::new(0) }; SLOTS],
            ret_b: [const { AtomicU32::new(0) }; SLOTS],
            dead: AtomicBool::new(false),
            died_at: AtomicU64::new(0),
            answered_at: AtomicU64::new(0),
            hung_after_ms: AtomicU64::new(HUNG_AFTER_MS),
            kill: lock::Mutex::new(None),
            next_id: AtomicU32::new(0),
        })
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// When the server was seen to be gone.
    pub fn died_at(&self) -> u64 {
        self.died_at.load(Ordering::Acquire)
    }

    /// The server is gone. Whether this is the first to say so.
    fn die(&self) -> bool {
        let first = !self.dead.swap(true, Ordering::AcqRel);
        if first {
            self.died_at.store(now_ms().max(1), Ordering::Release);
        }
        first
    }

    /// Whether the server is gone, after a look at its end of the FIFO.
    pub fn looked_dead(&self) -> bool {
        self.pump();
        self.is_dead()
    }

    /// How the server is ended when it hangs.
    pub fn killed_with(&self, kill: Kill) {
        *self.kill.lock() = Some(kill);
    }

    /// The server has said nothing for as long as a request waits: it is
    /// killed and the link closed, so that the next server can have the
    /// mount. Killed first, because the next one starts from the memory
    /// this one writes; closed whether or not the kill does anything,
    /// because nothing more is to be had from it either way.
    fn hung(&self) {
        if self.is_dead() {
            return;
        }
        warn!("served fs: the server answers nothing: it is killed");
        let kill = self.kill.lock().take();
        if let Some(kill) = kill {
            kill();
        }
        self.close();
    }

    /// Whether the server has said nothing since `since` for as long as a
    /// request waits.
    fn silent_since(&self, since: u64) -> bool {
        let heard = since.max(self.answered_at.load(Ordering::Acquire));
        now_ms() > heard + self.hung_after_ms.load(Ordering::Relaxed)
    }

    /// The mount is gone, or the server: nothing is asked on this link
    /// again, and a server sees its FIFO closed.
    pub fn close(&self) {
        self.die();
        let fifo = self.fifo.lock().take();
        if let Some(fifo) = fifo {
            // Whoever waits on it to learn of the server's going
            // (`loader::linux::fs_serve`) lets go of it too.
            let object: Arc<dyn KernelObject> = fifo;
            object.signal_set(LEFT);
        }
    }

    /// Read whatever replies have come back and mark their slots. Returns
    /// how many there were.
    fn pump(&self) -> usize {
        let Some(fifo) = self.fifo.lock().clone() else {
            return 0;
        };
        let mut back = [0u8; MSG * SLOTS];
        let n = match fifo.read(MSG, &mut back, SLOTS) {
            Ok(n) => n,
            Err(ZxError::SHOULD_WAIT) => return 0,
            Err(e) => {
                if self.die() {
                    warn!("served fs: the server is gone ({e:?})");
                }
                return 0;
            }
        };
        if n > 0 {
            self.answered_at.store(now_ms(), Ordering::Release);
        }
        for m in back.as_chunks::<MSG>().0.iter().take(n) {
            let reply = wire::decode(m);
            let slot = (reply.reqid as usize) & (SLOTS - 1);
            let mine = slot < self.slots
                && self.expect[slot].load(Ordering::Acquire) == reply.reqid
                && self.state[slot].load(Ordering::Acquire) == PENDING;
            if !mine {
                // A server answering twice, or late, after its request gave
                // up and left the slot.
                warn!("served fs: an answer to no request ({})", reply.reqid);
                continue;
            }
            self.status[slot].store(reply.status, Ordering::Relaxed);
            self.ret_a[slot].store(reply.a, Ordering::Relaxed);
            self.ret_b[slot].store(reply.b, Ordering::Relaxed);
            self.state[slot].store(ANSWERED, Ordering::Release);
        }
        n
    }

    /// A free slot, waiting for one while there are none and the server
    /// answers those that are held.
    fn claim(&self, since: u64) -> Option<usize> {
        let mut away = SyncWait::new();
        loop {
            let free = self.free.load(Ordering::Acquire);
            if free != 0 {
                let slot = free.trailing_zeros() as usize;
                if self
                    .free
                    .compare_exchange(
                        free,
                        free & !(1 << slot),
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return Some(slot);
                }
                continue;
            }
            if self.is_dead() {
                return None;
            }
            if self.silent_since(since) {
                self.hung();
                return None;
            }
            // The holders may be parked: their replies are read by whoever
            // is here.
            if self.pump() == 0 {
                between_looks(&mut away);
            }
        }
    }

    /// Ask the server `op`, with the names as they go in the slot (one
    /// path, or two with a NUL between), `a`, `b` and `data` in its data
    /// area, and wait for what it says. Its `errno` is the error; a server
    /// that is gone is [`Fail::Gone`], and so is one that hangs, which is
    /// gone by the time this says so.
    pub fn call_with(
        self: &Arc<Self>,
        op: u32,
        path: &[u8],
        a: u64,
        b: u32,
        data: &[u8],
    ) -> Result<Answer, Fail> {
        let failed = Fail::Said(FsError::DeviceError);
        if path.len() > NAME_AREA {
            return Err(Fail::Said(FsError::NameTooLong));
        }
        if data.len() > DATA_AREA {
            return Err(Fail::Said(FsError::InvalidParam));
        }
        if self.is_dead() {
            return Err(Fail::Gone);
        }
        let since = now_ms();
        let Some(slot) = self.claim(since) else {
            return Err(Fail::Gone);
        };
        let id = (self.next_id.fetch_add(1, Ordering::Relaxed) << SLOT_BITS) | slot as u32;
        self.expect[slot].store(id, Ordering::Release);
        self.state[slot].store(PENDING, Ordering::Release);
        let give_back = |link: &Link| {
            link.free.fetch_or(1 << slot, Ordering::AcqRel);
        };
        let at = slot * SLOT_LEN;
        let put =
            |at: usize, bytes: &[u8]| bytes.is_empty() || self.window.write(at, bytes).is_ok();
        if !put(at, path) || !put(at + NAME_AREA, data) {
            give_back(self);
            return Err(failed);
        }
        let msg = wire::encode(op, id, a, b, path.len() as u32);
        let Some(fifo) = self.fifo.lock().clone() else {
            give_back(self);
            return Err(Fail::Gone);
        };
        let mut away = SyncWait::new();
        // The FIFO holds a message for every slot, so there is room unless
        // the server left requests that gave up unread.
        loop {
            match fifo.write(MSG, &msg, 1) {
                Ok(_) => break,
                Err(ZxError::SHOULD_WAIT) if !self.silent_since(since) => between_looks(&mut away),
                // A server that reads nothing is as one that answers
                // nothing.
                Err(ZxError::SHOULD_WAIT) => {
                    give_back(self);
                    self.hung();
                    return Err(Fail::Gone);
                }
                Err(_) => {
                    give_back(self);
                    self.die();
                    return Err(Fail::Gone);
                }
            }
        }
        // From the first turn given away, the reply's arrival has this CPU
        // take the wait up again: see `KickOn`. The FIFO is not held past
        // that: a link that is closed drops its own.
        let mut fifo = Some(fifo);
        let mut _kick = None;
        loop {
            if self.state[slot].load(Ordering::Acquire) == ANSWERED {
                break;
            }
            if self.pump() > 0 {
                continue;
            }
            // A slot still waiting stays taken for good: the server may yet
            // write into it.
            if self.is_dead() {
                return Err(Fail::Gone);
            }
            if self.silent_since(since) {
                self.hung();
                return Err(Fail::Gone);
            }
            if let Some(fifo) = fifo.take() {
                _kick = Some(KickOn::new(&*fifo, Signal::READABLE | Signal::PEER_CLOSED));
                // What came before the callback was there raised nothing.
                continue;
            }
            between_looks(&mut away);
        }
        let status = self.status[slot].load(Ordering::Relaxed);
        if status < 0 {
            give_back(self);
            return Err(Fail::Said(wire::error_of(status)));
        }
        Ok(Answer {
            link: self.clone(),
            slot,
            count: status as usize,
            a: self.ret_a[slot].load(Ordering::Relaxed),
            len: (self.ret_b[slot].load(Ordering::Relaxed) as usize).min(DATA_AREA),
        })
    }
}
