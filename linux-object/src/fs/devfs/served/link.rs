//! The kernel's end of a served device node: the FIFO its requests go out
//! on, the window their bytes pass through, and what is in flight.
//!
//! The same shape as a served filesystem's link (`crate::fs::served`), with
//! two differences. A request names an open of the node and carries no
//! path, so a slot is all data. And the server also writes messages nobody
//! asked for, readiness ([`OP_READY`]), which whoever reads the FIFO hands
//! to the node's [`Ready`] as it comes upon them: a waiting request, or the
//! task that watches the FIFO while nothing is asked
//! (`loader::linux::dev_serve`).
//!
//! A request is made from a system call (a `read`, a `write`, an `ioctl`
//! on the node), which waits for the reply with its executor parked between
//! looks, as a request to a served filesystem does.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering};
use kernel_hal::thread::SyncWait;
use zircon_object::ipc::Fifo;
use zircon_object::object::{KernelObject, KickOn, Signal};
use zircon_object::vm::{VmAddressRegion, VmObject};
use zircon_object::{ZxError, ZxResult};

use super::wire::{self, Msg, MSG, OP_READY, SLOTS, SLOT_LEN};
use crate::fs::served::between_looks;

const SLOT_BITS: u32 = SLOTS.trailing_zeros();

/// Raised on the kernel's end of the FIFO when the link is closed, for the
/// task that watches it to let go of its own reference.
pub const LEFT: Signal = Signal::USER_SIGNAL_0;

/// The `reqid` of a message that wants no answer.
pub const NO_REPLY: u32 = u32::MAX;

const PENDING: u8 = 0;
const ANSWERED: u8 = 1;

/// How long a request waits while the server answers nothing, this
/// request or any other. A device answers at once or says `EAGAIN`; one
/// that says nothing for this long is hung, and is killed.
const HUNG_AFTER_MS: u64 = 10_000;

/// A slot's lent address space, with the id of the request it is lent for.
type Lent = lock::Mutex<Option<(u32, Arc<VmAddressRegion>)>>;

/// What the server says unasked: open `fh` is now as the mask says.
pub(super) type Ready = Box<dyn Fn(u32, u64) + Send + Sync>;
/// What ends a server that hangs.
pub(super) type Kill = Box<dyn Fn() + Send + Sync>;

pub(super) struct Link {
    fifo: lock::Mutex<Option<Arc<Fifo>>>,
    window: Arc<VmObject>,
    slots: usize,
    free: AtomicU32,
    expect: [AtomicU32; SLOTS],
    state: [AtomicU8; SLOTS],
    status: [AtomicI64; SLOTS],
    ret_a: [AtomicU64; SLOTS],
    ret_len: [AtomicU32; SLOTS],
    ret_fh: [AtomicU32; SLOTS],
    /// The address space of the program whose `ioctl` is in each slot, for
    /// as long as it waits: what the server may read and write of it
    /// ([`Link::asker`]).
    asker: [Lent; SLOTS],
    dead: AtomicBool,
    answered_at: AtomicU64,
    pub hung_after_ms: AtomicU64,
    ready: lock::Mutex<Option<Ready>>,
    kill: lock::Mutex<Option<Kill>>,
    next_id: AtomicU32,
}

/// A request that was answered, and its slot until this is dropped.
pub(super) struct Answer {
    link: Arc<Link>,
    slot: usize,
    /// The reply's `status`: never negative.
    pub count: usize,
    pub a: u64,
    pub fh: u32,
    /// The bytes of the slot the server filled.
    pub len: usize,
}

impl Answer {
    /// The first `buf.len()` bytes the server put in the slot: no more
    /// than it said it put there.
    pub fn data(&self, buf: &mut [u8]) -> Result<(), Fail> {
        if buf.len() > self.len {
            return Err(Fail::Said(5));
        }
        self.link
            .window
            .read(self.slot * SLOT_LEN, buf)
            .map_err(|_| Fail::Said(5))
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
    /// The server's `errno`, or the kernel's own when the request could
    /// not be made.
    Said(usize),
    /// The server is gone.
    Gone,
}

fn now_ms() -> u64 {
    kernel_hal::timer::timer_now().as_millis() as u64
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
            ret_len: [const { AtomicU32::new(0) }; SLOTS],
            ret_fh: [const { AtomicU32::new(0) }; SLOTS],
            asker: [const { lock::Mutex::new(None) }; SLOTS],
            dead: AtomicBool::new(false),
            answered_at: AtomicU64::new(0),
            hung_after_ms: AtomicU64::new(HUNG_AFTER_MS),
            ready: lock::Mutex::new(None),
            kill: lock::Mutex::new(None),
            next_id: AtomicU32::new(0),
        })
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Where readiness the server sends goes.
    pub fn ready_to(&self, ready: Ready) {
        *self.ready.lock() = Some(ready);
    }

    /// How the server is ended when it hangs.
    pub fn killed_with(&self, kill: Kill) {
        *self.kill.lock() = Some(kill);
    }

    /// The server has said nothing for as long as a request waits.
    fn hung(&self) {
        if self.is_dead() {
            return;
        }
        warn!("served dev: the server answers nothing: it is killed");
        let kill = self.kill.lock().take();
        if let Some(kill) = kill {
            kill();
        }
        self.close();
    }

    fn silent_since(&self, since: u64) -> bool {
        let heard = since.max(self.answered_at.load(Ordering::Acquire));
        now_ms() > heard + self.hung_after_ms.load(Ordering::Relaxed)
    }

    /// Nothing is asked on this link again. Whether this was the first to
    /// close it.
    pub fn close(&self) -> bool {
        let first = !self.dead.swap(true, Ordering::AcqRel);
        let fifo = self.fifo.lock().take();
        if let Some(fifo) = fifo {
            let object: Arc<dyn KernelObject> = fifo;
            object.signal_set(LEFT);
        }
        first
    }

    /// Read what the server wrote: mark the slots of the replies, hand on
    /// the readiness. How many messages there were; the server going is
    /// [`is_dead`](Self::is_dead) afterwards.
    pub fn pump(&self) -> usize {
        let Some(fifo) = self.fifo.lock().clone() else {
            return 0;
        };
        let mut back = [0u8; MSG * SLOTS];
        let n = match fifo.read(MSG, &mut back, SLOTS) {
            Ok(n) => n,
            Err(ZxError::SHOULD_WAIT) => return 0,
            Err(_) => {
                self.dead.store(true, Ordering::Release);
                return 0;
            }
        };
        if n > 0 {
            self.answered_at.store(now_ms(), Ordering::Release);
        }
        for m in back.as_chunks::<MSG>().0.iter().take(n) {
            let reply = Msg::decode(m);
            if reply.op == OP_READY {
                if let Some(ready) = &*self.ready.lock() {
                    ready(reply.fh, reply.a);
                }
                continue;
            }
            let slot = (reply.reqid as usize) & (SLOTS - 1);
            let mine = slot < self.slots
                && self.expect[slot].load(Ordering::Acquire) == reply.reqid
                && self.state[slot].load(Ordering::Acquire) == PENDING;
            if !mine {
                warn!("served dev: an answer to no request ({})", reply.reqid);
                continue;
            }
            self.status[slot].store(reply.status(), Ordering::Relaxed);
            self.ret_a[slot].store(reply.a, Ordering::Relaxed);
            self.ret_len[slot].store(reply.len, Ordering::Relaxed);
            self.ret_fh[slot].store(reply.fh, Ordering::Relaxed);
            self.state[slot].store(ANSWERED, Ordering::Release);
        }
        n
    }

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
            if self.pump() == 0 {
                between_looks(&mut away);
            }
        }
    }

    /// Tell the server `op` on open `fh`, and wait for no answer: what a
    /// close says, which must not hold up the closer. The `reqid` is
    /// [`NO_REPLY`].
    pub fn tell(&self, op: u32, fh: u32) {
        let msg = Msg {
            op,
            reqid: NO_REPLY,
            fh,
            ..Msg::default()
        }
        .encode();
        let since = now_ms();
        let mut away = SyncWait::new();
        loop {
            let Some(fifo) = self.fifo.lock().clone() else {
                return;
            };
            match fifo.write(MSG, &msg, 1) {
                Err(ZxError::SHOULD_WAIT) if !self.silent_since(since) => {
                    if self.pump() == 0 {
                        between_looks(&mut away);
                    }
                }
                Err(ZxError::SHOULD_WAIT) => return self.hung(),
                _ => return,
            }
        }
    }

    /// Ask the server `op` on open `fh`, with `a`, `b` and `data` in the
    /// slot (its length is the message's `len`, unless `len` says more:
    /// a read's room), and wait for what it says.
    pub fn call(
        self: &Arc<Self>,
        op: u32,
        fh: u32,
        len: usize,
        a: u64,
        b: u64,
        data: &[u8],
    ) -> Result<Answer, Fail> {
        self.call_for(None, op, fh, len, a, b, data)
    }

    /// [`Link::call`] for a program whose memory the server may reach
    /// while the request waits: `asker`, its address space.
    #[allow(clippy::too_many_arguments)]
    pub fn call_for(
        self: &Arc<Self>,
        asker: Option<Arc<VmAddressRegion>>,
        op: u32,
        fh: u32,
        len: usize,
        a: u64,
        b: u64,
        data: &[u8],
    ) -> Result<Answer, Fail> {
        if data.len() > SLOT_LEN || len > SLOT_LEN {
            return Err(Fail::Said(22));
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
        // Gone with the request, however it ends: answered, refused, or
        // left waiting on a server that is gone.
        // (Each request empties its own, so a request that lends nothing
        // finds the slot empty and takes no lock: reads and writes stay as
        // cheap as they were.)
        let _forget = asker.map(|vmar| {
            *self.asker[slot].lock() = Some((id, vmar));
            Forget(&self.asker[slot], id)
        });
        let give_back = |link: &Link| {
            link.free.fetch_or(1 << slot, Ordering::AcqRel);
        };
        if !data.is_empty() && self.window.write(slot * SLOT_LEN, data).is_err() {
            give_back(self);
            return Err(Fail::Said(5));
        }
        let msg = Msg {
            op,
            reqid: id,
            fh,
            len: len.max(data.len()) as u32,
            a,
            b,
        }
        .encode();
        let Some(fifo) = self.fifo.lock().clone() else {
            give_back(self);
            return Err(Fail::Gone);
        };
        let mut away = SyncWait::new();
        loop {
            match fifo.write(MSG, &msg, 1) {
                Ok(_) => break,
                Err(ZxError::SHOULD_WAIT) if !self.silent_since(since) => {
                    // The server's replies may be what fills it.
                    if self.pump() == 0 {
                        between_looks(&mut away);
                    }
                }
                Err(ZxError::SHOULD_WAIT) => {
                    give_back(self);
                    self.hung();
                    return Err(Fail::Gone);
                }
                Err(_) => {
                    give_back(self);
                    self.dead.store(true, Ordering::Release);
                    return Err(Fail::Gone);
                }
            }
        }
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
                continue;
            }
            between_looks(&mut away);
        }
        let status = self.status[slot].load(Ordering::Relaxed);
        if status < 0 {
            give_back(self);
            return Err(Fail::Said(wire::errno_of(status)));
        }
        Ok(Answer {
            link: self.clone(),
            slot,
            count: status as usize,
            a: self.ret_a[slot].load(Ordering::Relaxed),
            fh: self.ret_fh[slot].load(Ordering::Relaxed),
            len: (self.ret_len[slot].load(Ordering::Relaxed) as usize).min(SLOT_LEN),
        })
    }
}

impl Link {
    /// The address space of the program whose request `reqid` is, while it
    /// waits for its answer: `None` once it is answered, for a request that
    /// is not one of this link's, or one that lent no memory (only an
    /// `ioctl` does). The server reaches it through this, and only for a
    /// request it has been asked and not yet answered: what CUSE's
    /// unrestricted `ioctl` lets a server read and write, without the
    /// rounds of asking again.
    pub fn asker(&self, reqid: u32) -> Option<Arc<VmAddressRegion>> {
        let slot = (reqid as usize) & (SLOTS - 1);
        if slot >= self.slots || self.expect[slot].load(Ordering::Acquire) != reqid {
            return None;
        }
        let asker = match &*self.asker[slot].lock() {
            Some((id, vmar)) if *id == reqid => vmar.clone(),
            _ => return None,
        };
        // Looked at after the address space: an answer between the two
        // would otherwise hand it out once the request was over.
        if self.state[slot].load(Ordering::Acquire) != PENDING {
            return None;
        }
        Some(asker)
    }
}

/// Empties a slot's [`Link::asker`] when the request ends: only if it is
/// still this request's, since the slot may already be the next one's.
struct Forget<'a>(&'a Lent, u32);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        let mut asker = self.0.lock();
        if matches!(&*asker, Some((id, _)) if *id == self.1) {
            *asker = None;
        }
    }
}
