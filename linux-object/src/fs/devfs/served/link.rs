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

/// The `reqid` of the watch's ping ([`Link::look_quiet`]): an operation no
/// server knows, which every server answers (`EINVAL`), from the thread
/// that answers everything else.
pub const PING_REQID: u32 = u32::MAX - 1;
const OP_PING: u32 = 0x7069;

/// How long a server with something open of it may say nothing before the
/// watch pings it.
const PING_AFTER_MS: u64 = 5_000;

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
    /// When the watch pinged the server, while no answer came since; 0
    /// when none is out. And how many times the server had been heard
    /// then, of [`Link::heard`].
    pinged_at: AtomicU64,
    heard_at_ping: AtomicU64,
    /// How many times [`Link::pump`] read something of the server's.
    heard: AtomicU64,
    pub hung_after_ms: AtomicU64,
    /// The process that answers on this link, 0 until it is said
    /// ([`Link::killed_with`]): what a server's other links are found by.
    server: AtomicU64,
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

pub(super) fn now_ms() -> u64 {
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
            pinged_at: AtomicU64::new(0),
            heard_at_ping: AtomicU64::new(0),
            heard: AtomicU64::new(0),
            hung_after_ms: AtomicU64::new(HUNG_AFTER_MS),
            server: AtomicU64::new(0),
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

    /// The process that answers on this link (`server`, its id), and how
    /// it is ended when it hangs.
    pub fn killed_with(&self, server: u64, kill: Kill) {
        self.server.store(server, Ordering::Release);
        *self.kill.lock() = Some(kill);
    }

    /// The process that answers on this link; 0 when nobody said.
    pub fn server(&self) -> u64 {
        self.server.load(Ordering::Acquire)
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

    /// Whether the server has said nothing since `since`, or since its last
    /// answer, for as long as a request waits. Not while a request known to
    /// take long is waiting on it (`super::long_request`): a GPU's firmware
    /// boot, and what the server's other threads are asked meanwhile.
    fn silent_since(&self, since: u64) -> bool {
        let now = now_ms();
        let silent = || {
            let heard = since.max(self.answered_at.load(Ordering::Acquire));
            now > heard + self.hung_after_ms.load(Ordering::Relaxed)
        };
        // The long request is looked for only by a link that would be hung:
        // it is a list, under a lock. And the silence is counted again after
        // the look: a request that ended in between left the time it ended
        // here ([`Link::heard_now`]) before it stopped waiting.
        silent() && !super::long_request_waits(self, now) && silent()
    }

    /// A request known to take long, on a link of this one's server, is
    /// over (`super::long_request`): the server was busy until now, and its
    /// silence is counted from here.
    pub fn heard_now(&self) {
        self.answered_at.fetch_max(now_ms(), Ordering::AcqRel);
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
            self.heard.fetch_add(1, Ordering::AcqRel);
        }
        for m in back.as_chunks::<MSG>().0.iter().take(n) {
            let reply = Msg::decode(m);
            // What it said is the answer: it was heard.
            if reply.reqid == PING_REQID {
                continue;
            }
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

    /// The watch's look at `now` at a server that something is `open` of:
    /// one that has said nothing for [`PING_AFTER_MS`] is pinged, which a
    /// live one answers at once; one that leaves the ping unanswered for as
    /// long as a request waits is hung, and is killed. What a reader waits
    /// for is the server's word that an open can be read, with no request
    /// out, so without this a frozen server would never be found out.
    pub fn look_quiet(&self, now: u64, open: bool) {
        if self.is_dead() {
            return;
        }
        let heard = self.heard.load(Ordering::Acquire);
        let pinged = self.pinged_at.load(Ordering::Acquire);
        if pinged != 0 {
            if heard != self.heard_at_ping.load(Ordering::Acquire) {
                self.pinged_at.store(0, Ordering::Release);
            } else if now > pinged + self.hung_after_ms.load(Ordering::Relaxed)
                && !super::long_request_waits(self, now)
            {
                self.hung();
            }
            return;
        }
        if !open || now <= self.answered_at.load(Ordering::Acquire) + PING_AFTER_MS {
            return;
        }
        let Some(fifo) = self.fifo.lock().clone() else {
            return;
        };
        let msg = Msg {
            op: OP_PING,
            reqid: PING_REQID,
            ..Msg::default()
        }
        .encode();
        // A FIFO too full to take it is the server not reading: the ping is
        // out as much as it would be.
        self.heard_at_ping.store(heard, Ordering::Release);
        let _ = fifo.write(MSG, &msg, 1);
        self.pinged_at.store(now.max(1), Ordering::Release);
    }

    /// Tell the server `op` on open `fh`, and wait for no answer: what a
    /// close says, which must not hold up the closer. The `reqid` is
    /// [`NO_REPLY`].
    pub fn tell(&self, op: u32, fh: u32) {
        self.tell_with(op, fh, 0, 0)
    }

    /// [`tell`](Self::tell), with the words `a` and `b`.
    pub fn tell_with(&self, op: u32, fh: u32, a: u64, b: u64) {
        let msg = Msg {
            op,
            reqid: NO_REPLY,
            fh,
            a,
            b,
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

#[cfg(test)]
mod patience_tests {
    use super::super::long_request;
    use super::*;

    /// A link of process `server`, hung the moment a request waits on it.
    fn link_of(server: u64) -> (Arc<Link>, Arc<Fifo>) {
        let (ours, theirs) = Fifo::create(SLOTS * 2, MSG);
        let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
        let koid = ours.id();
        let link = Arc::new(Link::new(ours, window).unwrap());
        link.hung_after_ms.store(0, Ordering::Relaxed);
        if server != 0 {
            link.killed_with(server, Box::new(|| {}));
        }
        super::super::remember(koid, &link);
        (link, theirs)
    }

    /// A time every link was silent since, once the clock has moved on.
    fn long_ago() -> u64 {
        while now_ms() == 0 {
            core::hint::spin_loop();
        }
        0
    }

    /// Wait for the clock to be past the last time `link` heard its server.
    fn past_its_answer(link: &Link) {
        while now_ms() <= link.answered_at.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }

    #[test]
    fn a_long_request_keeps_the_silence_from_being_a_hang() {
        let (link, _theirs) = link_of(0);
        let long_ago = long_ago();
        assert!(link.silent_since(long_ago), "past the wait: hung");
        let boot = long_request(&link, 60_000);
        assert!(!link.silent_since(long_ago), "a firmware boot is waiting");
        drop(boot);
        // One that outlasts its patience is a hang, as any other.
        let _late = long_request(&link, 0);
        past_its_answer(&link);
        assert!(link.silent_since(long_ago));
    }

    /// The GPU driver answers on a link for each card and one for the RM,
    /// each on a thread of its own. While a card's thread boots the GPU's
    /// firmware it holds the RM, and what the other threads were asked
    /// meanwhile waits for it unanswered: that is no hang either, on any
    /// link of that process, and still one on every other server's.
    #[test]
    fn a_long_request_makes_every_link_of_its_server_patient() {
        let (card0, _a) = link_of(4101);
        let (card1, _b) = link_of(4101);
        let (rm, _c) = link_of(4101);
        let (other, _d) = link_of(4102);
        let (nobodys, _e) = link_of(0);
        let long_ago = long_ago();
        assert!(card1.silent_since(long_ago) && rm.silent_since(long_ago));
        let boot = long_request(&card0, 60_000);
        assert!(!card0.silent_since(long_ago), "the link that was asked");
        assert!(!card1.silent_since(long_ago), "the other card's");
        assert!(!rm.silent_since(long_ago), "the RM's");
        assert!(other.silent_since(long_ago), "another server hangs as ever");
        assert!(nobodys.silent_since(long_ago));
        drop(boot);
        // A link nobody said the server of is patient alone.
        let _alone = long_request(&nobodys, 60_000);
        assert!(!nobodys.silent_since(long_ago));
        assert!(other.silent_since(long_ago));
    }

    /// The patience is the request's: answered, it leaves none behind, and
    /// each link of the server counts its silence from the answer. A driver
    /// that hangs right after a firmware boot is found as soon as any other,
    /// and one that was only busy with it is not taken for hung.
    #[test]
    fn the_patience_ends_with_the_request() {
        let (card, _a) = link_of(4201);
        let (rm, _b) = link_of(4201);
        let long_ago = long_ago();
        let boot = long_request(&card, 60_000);
        assert!(!rm.silent_since(long_ago));
        let answered = now_ms();
        drop(boot);
        for link in [&card, &rm] {
            assert!(
                link.answered_at.load(Ordering::Acquire) >= answered,
                "the server was busy until the answer, not silent"
            );
        }
        // With a wait to count it by, that is no hang yet...
        rm.hung_after_ms.store(60_000, Ordering::Relaxed);
        assert!(!rm.silent_since(long_ago));
        // ...and past the wait it is one again: no patience is left.
        rm.hung_after_ms.store(0, Ordering::Relaxed);
        past_its_answer(&rm);
        assert!(rm.silent_since(long_ago));
    }

    /// A link is covered from the moment its server is said: one made, or
    /// told whose it is, after the long request was asked waits with the
    /// others of that process.
    #[test]
    fn a_link_that_learns_its_server_late_is_patient_too() {
        let (card, _a) = link_of(4301);
        let (late, _b) = link_of(0);
        let long_ago = long_ago();
        let _boot = long_request(&card, 60_000);
        assert!(late.silent_since(long_ago), "nobody's yet");
        late.killed_with(4301, Box::new(|| {}));
        assert!(!late.silent_since(long_ago));
        let (later, _c) = link_of(4301);
        assert!(!later.silent_since(long_ago), "made after the ask");
    }
}

#[cfg(test)]
mod ping_tests {
    use super::*;

    /// A link whose server says nothing until the test does, and whether
    /// its kill was called.
    fn quiet_link() -> (Arc<Link>, Arc<Fifo>, Arc<AtomicBool>) {
        let (ours, theirs) = Fifo::create(SLOTS * 2, MSG);
        let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
        let link = Arc::new(Link::new(ours, window).unwrap());
        link.hung_after_ms.store(0, Ordering::Relaxed);
        let killed = Arc::new(AtomicBool::new(false));
        let k = killed.clone();
        link.killed_with(0, Box::new(move || k.store(true, Ordering::Release)));
        (link, theirs, killed)
    }

    #[test]
    fn a_quiet_server_that_nothing_is_open_of_is_never_pinged() {
        let (link, theirs, killed) = quiet_link();
        link.look_quiet(now_ms() + PING_AFTER_MS + 1, false);
        let mut m = [0u8; MSG];
        assert!(theirs.read(MSG, &mut m, 1).is_err(), "nothing sent");
        assert!(!killed.load(Ordering::Acquire));
    }

    #[test]
    fn a_frozen_server_leaves_its_ping_unanswered_and_is_killed() {
        let (link, theirs, killed) = quiet_link();
        let now = now_ms() + PING_AFTER_MS + 1;
        link.look_quiet(now, true);
        let mut m = [0u8; MSG];
        theirs.read(MSG, &mut m, 1).unwrap();
        assert_eq!(Msg::decode(&m).reqid, PING_REQID);
        link.look_quiet(now + 1, true);
        assert!(
            killed.load(Ordering::Acquire),
            "no answer for as long as a request waits"
        );
        assert!(link.is_dead());
    }

    #[test]
    fn a_live_server_answers_its_ping_and_lives() {
        let (link, theirs, killed) = quiet_link();
        let now = now_ms() + PING_AFTER_MS + 1;
        link.look_quiet(now, true);
        let mut m = [0u8; MSG];
        theirs.read(MSG, &mut m, 1).unwrap();
        // The server's answer: whatever it says of an operation it does not
        // know, under the same id.
        let mut reply = Msg::decode(&m);
        reply.b = (-22i64) as u64;
        theirs.write(MSG, &reply.encode(), 1).unwrap();
        assert_eq!(link.pump(), 1, "read, and handed to no request");
        link.look_quiet(now + 1, true);
        assert!(!killed.load(Ordering::Acquire));
        assert_eq!(link.pinged_at.load(Ordering::Acquire), 0, "no ping out");
    }
}
