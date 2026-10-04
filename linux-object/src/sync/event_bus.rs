//! Event bus implement
//!
//! An Eventbus is a mechanism that allows different components to communicate with each other without knowing about each other.
use alloc::boxed::Box;
use alloc::{sync::Arc, vec::Vec};
use bitflags::bitflags;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use lock::Mutex;

const MAX_EVENT_CALLBACKS: usize = 4096;

/// Cap on parked EventBus callbacks (see [`EventBus::subscribe`]). Exposed for
/// soak/regression tests that assert long poll/epoll sessions cannot fill the
/// bus via orphaned wakers.
#[cfg(test)]
pub(crate) const TEST_MAX_EVENT_CALLBACKS: usize = MAX_EVENT_CALLBACKS;

bitflags! {
    #[derive(Default)]
    /// event bus Event flags
    pub struct Event: u32 {
        /// File: is readable
        const READABLE                      = 1 << 0;
        /// File: is writeable
        const WRITABLE                      = 1 << 1;
        /// File: has error
        const ERROR                         = 1 << 2;
        /// File: is closed
        const CLOSED                        = 1 << 3;

        /// Process: is Quit
        const PROCESS_QUIT                  = 1 << 10;
        /// Process: child process is Quit
        const CHILD_PROCESS_QUIT            = 1 << 11;
        /// Process: received signal
        const RECEIVE_SIGNAL                = 1 << 12;

        /// Semaphore: is removed
        const SEMAPHORE_REMOVED             = 1 << 20;
        /// Semaphore: can acquired a resource of this semaphore
        const SEMAPHORE_CAN_ACQUIRE         = 1 << 21;
    }
}

/// handler of event in the event bus
pub type EventHandler = Box<dyn Fn(Event) -> bool + Send>;

/// event bus struct
#[derive(Default)]
pub struct EventBus {
    /// event type
    event: Event,
    /// EventBus callbacks paired with unique subscription IDs
    callbacks: Vec<(u64, EventHandler)>,
    /// counter for subscription IDs
    next_id: u64,
    /// How many times a producer has published readiness (`set`/`change`
    /// with a non-empty `set`), whether or not the flags changed. A level does
    /// not move when data lands on a file that already had some, and an
    /// edge-triggered `epoll` still owes its waiter an event for it: this is
    /// the counter it compares against. See [`EventBus::subscribe_edge`].
    seq: u64,
    /// [`seq`](Self::seq) split by direction: one counter per file-readiness
    /// bit (`READABLE`, `WRITABLE`, `ERROR`, `CLOSED`, in that order).
    ///
    /// The whole-bus counter cannot answer "did anything *this waiter cares
    /// about* get published": a producer pulsing `WRITABLE` to wake a writer
    /// parked on room moves it too, and an edge-triggered `EPOLLIN` entry
    /// measured against it then reports the same readable level `epoll_wait`
    /// had already handed out. See [`EventBus::seq_for`].
    seq_bits: [u64; 4],
    /// Waiters parked by [`EventBus::subscribe_edge`]: `(id, mask, waker)`.
    edges: Vec<(u64, Event, core::task::Waker)>,
}
impl core::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventBus")
            .field("event", &self.event)
            .field("callbacks_len", &self.callbacks.len())
            .field("seq", &self.seq)
            .finish()
    }
}

impl EventBus {
    /// create an event bus
    pub fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    /// set event flag
    pub fn set(&mut self, set: Event) {
        self.change(Event::empty(), set);
    }

    /// clear all event flag
    pub fn clear(&mut self, set: Event) {
        self.change(set, Event::empty());
    }

    /// change event flag
    /// - `reset`: flag to remove
    /// - `set`: flag to insert
    ///
    /// Callbacks are taken out of the table before firing so a re-entrant
    /// `set`/`change` on the same bus (common when a waker touches the fd again)
    /// cannot deadlock holding this mutex, and late `subscribe`s during fire
    /// are preserved.
    pub fn change(&mut self, reset: Event, set: Event) {
        if !set.is_empty() {
            self.seq = self.seq.wrapping_add(1);
            for (i, bit) in Self::SEQ_BITS.iter().enumerate() {
                if set.contains(*bit) {
                    self.seq_bits[i] = self.seq_bits[i].wrapping_add(1);
                }
            }
            if !self.edges.is_empty() {
                self.edges.retain(|(_, mask, waker)| {
                    if mask.intersects(set) {
                        waker.wake_by_ref();
                        false
                    } else {
                        true
                    }
                });
            }
        }
        let orig = self.event;
        let mut new = self.event;
        new.remove(reset);
        new.insert(set);
        self.event = new;
        if new != orig {
            let pending = core::mem::take(&mut self.callbacks);
            let mut kept = Vec::with_capacity(pending.len());
            for (id, f) in pending {
                if !f(new) {
                    kept.push((id, f));
                }
            }
            // Subscriptions that arrived while we were firing.
            let mut late = core::mem::take(&mut self.callbacks);
            kept.append(&mut late);
            self.callbacks = kept;
        }
    }

    /// The currently set event flags. Callers that need a race-free
    /// check-then-subscribe must hold the same lock that writers use to
    /// `set()`/`change()` the bus while calling this and `subscribe`.
    pub fn events(&self) -> Event {
        self.event
    }

    /// push a EventHandler into the callback vector, returning a subscription ID if registered
    pub fn subscribe(&mut self, callback: EventHandler) -> Option<u64> {
        // A subscriber arriving while events are already active must observe
        // them NOW, not wait for the next transition. `change` fires callbacks
        // only when the flag set CHANGES, and the flags are latched — so a
        // waiter that checked readiness, lost the race to a producer that set
        // the flag in between, and then subscribed, would otherwise sleep on an
        // event that is already on and will never re-fire: a second `set` of an
        // already-set flag is not a transition. That was observable as both
        // ends of a pipe ping-pong asleep forever (the reader missed READABLE
        // by microseconds; the writer then blocked reading the reply) with the
        // whole machine idle around them — a silent, un-diagnosable hang,
        // roughly once per two benchmark rounds under load.
        //
        // Same contract as `change`: a callback returning true is one-shot and
        // is not retained after firing.
        if !self.event.is_empty() && callback(self.event) {
            return None;
        }
        if self.callbacks.len() >= MAX_EVENT_CALLBACKS {
            // The table only fills on a long-idle bus being poll-scanned
            // (poll/select/epoll park a fresh waker per scan and drop none, so
            // orphaned entries pile up until the next event drains them). Evict
            // the OLDEST entry instead of ignoring the newcomer: silently
            // dropping the incoming subscription loses the wakeup of a waiter
            // that may have no other entry — a blocking socket read parked
            // here forever froze the whole compositor.
            //
            // Wake the oldest *before* drop: otherwise a live waiter whose
            // only subscription was evicted sleeps forever, and under Wayland
            // bring-up that pushed more timer_set backstops / heap churn.
            trace!(
                "EventBus: callback table full ({}), waking+evicting oldest",
                MAX_EVENT_CALLBACKS
            );
            let (_id, oldest) = self.callbacks.remove(0);
            let _ = oldest(self.event);
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.callbacks.push((id, callback));
        Some(id)
    }

    /// Whether `id` still names a callback on this bus.
    ///
    /// A subscription is one-shot in both directions: [`EventBus::change`]
    /// drops a callback the moment it returns true, and a full table evicts the
    /// oldest entry outright. So a holder of an id cannot tell from the id
    /// alone whether it is still parked here, and has to ask.
    pub fn is_subscribed(&self, id: u64) -> bool {
        self.callbacks.iter().any(|(item_id, _)| *item_id == id)
    }

    /// Unsubscribe a previously registered callback by its ID.
    pub fn unsubscribe(&mut self, id: u64) {
        self.callbacks.retain(|(item_id, _)| *item_id != id);
        self.edges.retain(|(item_id, _, _)| *item_id != id);
    }

    /// The file-readiness bits [`seq_for`](Self::seq_for) counts separately,
    /// in the order of [`seq_bits`](Self::seq_bits).
    const SEQ_BITS: [Event; 4] = [
        Event::READABLE,
        Event::WRITABLE,
        Event::ERROR,
        Event::CLOSED,
    ];

    /// The publication counter (see the `seq` field).
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The publication counter restricted to `mask`: it moves only when a
    /// producer publishes a bit the caller asked about.
    ///
    /// This is what an edge-triggered waiter must measure against. A unix
    /// stream socket pulses `WRITABLE` on its peer's bus whenever a reader
    /// drains a full queue (`pulse_writable`), to wake a writer parked on
    /// room; nothing about that peer's own receive queue changed, so an
    /// `EPOLLIN | EPOLLET` entry on it owes its waiter nothing. Measured
    /// against the whole-bus [`seq`](Self::seq) it re-reported the level it
    /// had already delivered, which is the `epoll_wait` spin `EPOLLET` exists
    /// to stop. An eventfd does the same on every `read` that empties it.
    ///
    /// A mask naming none of the four file bits gets the whole-bus counter:
    /// there is nothing finer to answer with.
    pub fn seq_for(&self, mask: Event) -> u64 {
        let mut sum: u64 = 0;
        let mut any = false;
        for (i, bit) in Self::SEQ_BITS.iter().enumerate() {
            if mask.contains(*bit) {
                any = true;
                sum = sum.wrapping_add(self.seq_bits[i]);
            }
        }
        if any {
            sum
        } else {
            self.seq
        }
    }

    /// Park `waker` until a producer next publishes an event in `mask`, even
    /// one that leaves the flags as they were: edge-triggered `epoll`.
    ///
    /// [`EventBus::subscribe`] cannot do this. It fires at once on a latched
    /// flag, and an edge-triggered waiter that has already been told about a
    /// readable file would be woken by that latch forever: the spin this
    /// exists to end. It fires only on a *change* of the flags, too, and a
    /// second write to a file that is already readable changes none.
    ///
    /// `seen` is the [`EventBus::seq_for`] of this same `mask` that the
    /// waiter last looked at, read before it looked at the file's level. If
    /// anything in `mask` was published since, the waker fires here and
    /// `None` is returned, so no publication between that look and this call
    /// can be missed. It is compared against the same mask-restricted
    /// counter, not the whole-bus one: measured against `seq`, a publication
    /// in the *other* direction would make every subscription fire at once
    /// and the edge wait would degenerate into a busy re-poll.
    pub fn subscribe_edge(
        &mut self,
        mask: Event,
        waker: &core::task::Waker,
        seen: u64,
    ) -> Option<u64> {
        if self.seq_for(mask) != seen {
            waker.wake_by_ref();
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.edges.push((id, mask, waker.clone()));
        Some(id)
    }

    /// get the callback vector length
    pub fn get_callback_len(&self) -> usize {
        self.callbacks.len() + self.edges.len()
    }
}

/// RAII handle for a waker parked on some file's event source (see
/// `FileLike::subscribe_readiness`). Dropping it unregisters the waker so a
/// poller that stops waiting (completed, timed out, or was killed) does not
/// leave a callback behind to wake a recycled task slot later.
///
/// Carries a type-erased unsubscribe closure rather than an
/// `Arc<Mutex<EventBus>>` because the bus lives behind a different owner per
/// file type (a bare field inside the pipe's data mutex, an `Arc<Mutex<_>>`
/// on eventfd/timerfd, a global for the DRM device); each subscriber captures
/// whatever handle it needs.
pub struct ReadinessSub {
    unsub: Option<alloc::boxed::Box<dyn FnOnce() + Send>>,
}

impl ReadinessSub {
    /// A subscription whose cleanup runs `unsub` on drop.
    pub fn new(unsub: alloc::boxed::Box<dyn FnOnce() + Send>) -> Self {
        Self { unsub: Some(unsub) }
    }

    /// A subscription with nothing to clean up: the waker already fired at
    /// subscribe time (events were pending), so no callback was stored.
    pub fn noop() -> Self {
        Self { unsub: None }
    }
}

impl Drop for ReadinessSub {
    fn drop(&mut self) {
        if let Some(f) = self.unsub.take() {
            f();
        }
    }
}

/// Park `waker` on `bus` as a one-shot callback for any event in `mask`.
///
/// Returns the subscription id, or `None` when events in `mask` were already
/// pending — in that case the waker has been woken right here (the latched
/// flags + subscribe-time fire in [`EventBus::subscribe`] make
/// check-then-subscribe race-free) and nothing was stored.
pub fn subscribe_waker(bus: &mut EventBus, mask: Event, waker: &core::task::Waker) -> Option<u64> {
    let waker = waker.clone();
    bus.subscribe(Box::new(move |events| {
        if (events & mask).is_empty() {
            return false;
        }
        waker.wake_by_ref();
        true
    }))
}

/// [`subscribe_waker`] + RAII handle for the common `Arc<Mutex<EventBus>>`
/// bus owner (eventfd, timerfd, signalfd, pty buses, the DRM device bus).
pub fn subscribe_readiness_on(
    bus: &Arc<Mutex<EventBus>>,
    mask: Event,
    waker: &core::task::Waker,
) -> ReadinessSub {
    match subscribe_waker(&mut bus.lock(), mask, waker) {
        Some(id) => {
            let bus = bus.clone();
            ReadinessSub::new(Box::new(move || {
                bus.lock().unsubscribe(id);
            }))
        }
        None => ReadinessSub::noop(),
    }
}

/// [`EventBus::subscribe_edge`] + RAII handle for the `Arc<Mutex<EventBus>>`
/// bus owners, as [`subscribe_readiness_on`] is for level subscriptions.
pub fn subscribe_edge_on(
    bus: &Arc<Mutex<EventBus>>,
    mask: Event,
    waker: &core::task::Waker,
    seen: u64,
) -> ReadinessSub {
    match bus.lock().subscribe_edge(mask, waker, seen) {
        Some(id) => {
            let bus = bus.clone();
            ReadinessSub::new(Box::new(move || {
                bus.lock().unsubscribe(id);
            }))
        }
        None => ReadinessSub::noop(),
    }
}

/// How often a parked wait wakes up just to ask whether it should still be
/// waiting.
///
/// The event bus is the fast path and covers everything the *data* can do: a
/// pipe write, a peer close, a timer expiring. What it cannot report is
/// anything that happens to the WAITER -- a signal arriving, the thread being
/// killed, the process exiting -- because `send_signal_to_process` writes into
/// the target's signal set and never touches this bus. Without a backstop
/// nothing re-polls the future, so nothing ever calls [`wait_interrupted`],
/// and the wait outlives every attempt to end it.
///
/// 100 ms is the same backstop the io-multiplex path already uses
/// (`net::wait::IO_WAIT_COVERED_TICK_MS`), and it is a ceiling on how late a
/// `kill` can be, not on how fast data arrives.
const INTERRUPT_CHECK_TICK_MS: u64 = 100;

/// Whether a thread parked in a blocking wait must stop waiting -- because a
/// signal is deliverable, the thread is dying, or the process has exited.
///
/// Shared by every interruptible wait in this crate that is not driven through
/// `EventBusFuture` itself (the pty, the serial console, the input nodes), so
/// that they all flip together in a host test.
///
/// Indirected so the host tests can drive it: there is no current thread in a
/// host test, so [`crate::process::check_signals`] always answers `Ok` there
/// and no test could otherwise reach the interrupted branch at all.
#[cfg(not(test))]
pub(crate) fn wait_interrupted() -> crate::error::LxResult<()> {
    crate::process::check_signals()
}

#[cfg(test)]
pub(crate) fn wait_interrupted() -> crate::error::LxResult<()> {
    self::test_interrupt::check()
}

/// The test-only stand-in for [`crate::process::check_signals`].
#[cfg(test)]
pub(crate) mod test_interrupt {
    extern crate std;

    use crate::error::{LxError, LxResult};
    use core::cell::Cell;

    self::std::thread_local! {
        /// What the next check answers, and how many are left. Thread-local so
        /// the suite stays safe under the `--test-threads=1` CI *and* under a
        /// parallel local run: nothing here is shared between tests, so no
        /// test lock is needed and a test that panics cannot poison the next.
        static PENDING: Cell<Option<LxError>> = const { Cell::new(None) };
        static DELAY: Cell<usize> = const { Cell::new(0) };
    }

    /// Answer `Ok` for the next `after_polls` checks, then `Err(err)` for
    /// every one after that.
    pub(crate) fn interrupt_after(after_polls: usize, err: LxError) {
        PENDING.with(|p| p.set(Some(err)));
        DELAY.with(|d| d.set(after_polls));
    }

    /// Back to "nothing is interrupting anything".
    pub(crate) fn clear() {
        PENDING.with(|p| p.set(None));
        DELAY.with(|d| d.set(0));
    }

    pub(super) fn check() -> LxResult<()> {
        match PENDING.with(|p| p.get()) {
            Some(err) => {
                let left = DELAY.with(|d| d.get());
                if left == 0 {
                    Err(err)
                } else {
                    DELAY.with(|d| d.set(left - 1));
                    Ok(())
                }
            }
            None => Ok(()),
        }
    }
}

/// wait for a event async
///
/// Resolves with the events that matched, or with an error when the wait was
/// interrupted -- see [`wait_interrupted`]. Callers must propagate that error
/// rather than loop: it means "stop waiting", and swallowing it puts the
/// thread straight back into the wait it was just pulled out of.
pub fn wait_for_event(
    bus: Arc<Mutex<EventBus>>,
    mask: Event,
) -> impl Future<Output = crate::error::LxResult<Event>> {
    EventBusFuture {
        bus,
        mask,
        sub_id: None,
        timer: None,
    }
}

/// EventBus future for async
#[must_use = "future does nothing unless polled/`await`-ed"]
struct EventBusFuture {
    bus: Arc<Mutex<EventBus>>,
    mask: Event,
    sub_id: Option<u64>,
    /// Backstop slot (`timer_waker`): armed once while Pending, refreshed in
    /// place on re-polls, cancelled on Ready and on Drop so a late tick cannot
    /// wake a finished task. See [`INTERRUPT_CHECK_TICK_MS`].
    timer: Option<kernel_hal::timer_waker::TimerWakerSlot>,
}

impl Drop for EventBusFuture {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            self.bus.lock().unsubscribe(id);
        }
        kernel_hal::timer_waker::kill_timer_waker(&mut self.timer);
    }
}

impl Future for EventBusFuture {
    type Output = crate::error::LxResult<Event>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();
        {
            let mut lock = this.bus.lock();
            if !(lock.event & this.mask).is_empty() {
                if let Some(id) = this.sub_id.take() {
                    lock.unsubscribe(id);
                }
                let event = lock.event;
                drop(lock);
                kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
                return Poll::Ready(Ok(event));
            }
            // Not `sub_id.is_none()`: the callback registered below is
            // one-shot -- `change` drops it as soon as it fires, and a full
            // table evicts the oldest entry outright -- so after the first
            // wakeup the id this future holds names nothing on the bus. Asking
            // only whether the id is `None` therefore left the future with NO
            // callback parked for the rest of its life on any poll that did
            // not find the events set: the common one, where the waiter is
            // woken and another thread drains the data before it runs. From
            // then on the only thing that ever re-polled it was the 100 ms
            // backstop tick, so a blocking `read` on a pipe or a socket
            // degenerated into a poll loop -- every byte up to 100 ms late,
            // ten wakeups a second per waiter, and nothing to show why.
            if !this
                .sub_id
                .map(|id| lock.is_subscribed(id))
                .unwrap_or(false)
            {
                this.sub_id = None;
                let waker = cx.waker().clone();
                let mask = this.mask;
                let sub_id = lock.subscribe(Box::new(move |s| {
                    if (s & mask).is_empty() {
                        return false;
                    }
                    waker.wake_by_ref();
                    true
                }));
                this.sub_id = sub_id;
            }
        }
        // The data is not there. Before parking again, ask whether this thread
        // is still supposed to be here at all: this is the ONLY thing in the
        // whole wait that can answer a signal or a kill, because the bus only
        // ever reports what the file did.
        //
        // Checked AFTER the readiness test, deliberately, and in that order:
        // Linux gives a read that can be served right now its data, and
        // synthesises EINTR only when it would otherwise block.
        if let Err(err) = wait_interrupted() {
            if let Some(id) = this.sub_id.take() {
                this.bus.lock().unsubscribe(id);
            }
            kernel_hal::timer_waker::kill_timer_waker(&mut this.timer);
            return Poll::Ready(Err(err));
        }
        let deadline =
            kernel_hal::timer::deadline_after(Duration::from_millis(INTERRUPT_CHECK_TICK_MS));
        kernel_hal::timer_waker::ensure_timer_waker(&mut this.timer, deadline, cx);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{RawWaker, RawWakerVTable, Waker};
    use lock::Mutex;

    fn flag_waker(flag: &'static AtomicBool) -> Waker {
        fn raw(ptr: *const ()) -> RawWaker {
            unsafe fn clone(ptr: *const ()) -> RawWaker {
                raw(ptr)
            }
            unsafe fn wake(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn wake_by_ref(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn drop(_: *const ()) {}
            RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
        }
        unsafe { Waker::from_raw(raw(flag as *const AtomicBool as *const ())) }
    }

    /// Simulate ~minutes of poll/epoll re-scans: each parks then drops.
    /// Without Drop-unsubscribe this would climb to [`MAX_EVENT_CALLBACKS`] and
    /// then thrash (the 30–40 min KERNEL PAGE FAULT class).
    #[test]
    fn wait_for_event_drop_does_not_fill_bus() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus = EventBus::new();
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        // Compressed soak: 50k cycles ≫ 4096 cap; must stay near-empty.
        for _ in 0..50_000 {
            let mut fut = wait_for_event(bus.clone(), Event::READABLE);
            assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
            assert_eq!(bus.lock().get_callback_len(), 1);
            drop(fut);
            assert_eq!(
                bus.lock().get_callback_len(),
                0,
                "Drop must clear the parked waiter every cycle"
            );
        }
        assert!(!WOKE.load(Ordering::SeqCst));
    }

    #[test]
    fn subscribe_cap_never_exceeds_max() {
        let mut bus = EventBus::default();
        for _ in 0..(TEST_MAX_EVENT_CALLBACKS + 200) {
            let _ = bus.subscribe(Box::new(|_| false));
        }
        assert!(
            bus.get_callback_len() <= TEST_MAX_EVENT_CALLBACKS,
            "callback table must stay ≤ {}, got {}",
            TEST_MAX_EVENT_CALLBACKS,
            bus.get_callback_len()
        );
    }

    #[test]
    fn unsubscribe_removes_exact_id() {
        let mut bus = EventBus::default();
        let a = bus.subscribe(Box::new(|_| false)).unwrap();
        let b = bus.subscribe(Box::new(|_| false)).unwrap();
        assert_eq!(bus.get_callback_len(), 2);
        bus.unsubscribe(a);
        assert_eq!(bus.get_callback_len(), 1);
        bus.unsubscribe(b);
        assert_eq!(bus.get_callback_len(), 0);
    }

    #[test]
    fn wait_for_event_ready_unsubscribes() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        bus.lock().set(Event::READABLE);
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        assert!(matches!(
            Pin::new(&mut fut).poll(&mut cx),
            Poll::Ready(Ok(Event::READABLE))
        ));
        assert_eq!(bus.lock().get_callback_len(), 0);
    }

    /// The race this is about: the waiter is woken, and by the time it runs
    /// another thread has already drained the data. It has to park itself on
    /// the bus again -- its own callback was spent by the wakeup -- or nothing
    /// but the 100 ms backstop will ever look at it again.
    #[test]
    fn a_woken_waiter_that_finds_nothing_parks_itself_again() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);

        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
        assert_eq!(bus.lock().get_callback_len(), 1, "parked once");

        // The event arrives -- which spends the callback -- and is consumed by
        // somebody else before this future is polled.
        bus.lock().set(Event::READABLE);
        assert!(WOKE.load(Ordering::SeqCst), "the wakeup must have happened");
        assert_eq!(bus.lock().get_callback_len(), 0, "one-shot, so it is gone");
        bus.lock().clear(Event::READABLE);

        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
        assert_eq!(
            bus.lock().get_callback_len(),
            1,
            "a waiter that goes back to sleep must leave a callback behind, or \
             only the 100 ms tick will ever wake it"
        );
        drop(fut);
        assert_eq!(bus.lock().get_callback_len(), 0);
    }

    /// And the other side of it: a re-poll of a waiter whose callback is still
    /// parked must not park a second one.
    #[test]
    fn re_polling_a_live_waiter_does_not_park_a_second_callback() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        for _ in 0..8 {
            assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
            assert_eq!(bus.lock().get_callback_len(), 1);
        }
        drop(fut);
        assert_eq!(bus.lock().get_callback_len(), 0);
    }

    #[test]
    fn an_evicted_waiter_parks_itself_again_too() {
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = flag_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));

        // Fill the table: this future's callback is the oldest, so it is the
        // one the cap evicts.
        for _ in 0..TEST_MAX_EVENT_CALLBACKS {
            let _ = bus.lock().subscribe(Box::new(|_| false));
        }
        assert!(
            !bus.lock().is_subscribed(0),
            "the oldest entry is the one that gets evicted"
        );

        // `TEST_MAX_EVENT_CALLBACKS + 1` ids have been handed out (this
        // future's 0, then the loop's), so a waiter that parks itself again
        // gets the next one.
        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
        assert!(
            bus.lock()
                .is_subscribed(TEST_MAX_EVENT_CALLBACKS as u64 + 1),
            "an evicted waiter has to park itself again as well"
        );
    }
}

#[cfg(test)]
mod interruptible_wait_tests {
    //! A blocking wait that only ever hears from the FILE is a wait that
    //! nothing which happens to the *waiter* can end.
    //!
    //! `wait_for_event` is what `read(2)` comes down to on a pipe, an
    //! eventfd, a timerfd, an inotify fd and a perf fd. The event bus is a
    //! fine fast path for data, but a signal does not go through it:
    //! `send_signal_to_process` writes into the target's signal set and
    //! pulses the *process* object, not this bus. So a reader parked here
    //! used to be unreachable by `kill`, by a signal, and by its own process
    //! exiting -- it sat there until a peer wrote, forever if none ever did.
    //!
    //! These drive the check through the hook in [`test_interrupt`], because
    //! a host test has no current thread and the real
    //! `process::check_signals` would always answer `Ok`.

    use super::test_interrupt;
    use super::*;
    use crate::error::LxError;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{RawWaker, RawWakerVTable, Waker};
    use lock::Mutex;

    fn noop_waker(flag: &'static AtomicBool) -> Waker {
        fn raw(ptr: *const ()) -> RawWaker {
            unsafe fn clone(ptr: *const ()) -> RawWaker {
                raw(ptr)
            }
            unsafe fn wake(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn wake_by_ref(ptr: *const ()) {
                (*(ptr as *const AtomicBool)).store(true, Ordering::SeqCst);
            }
            unsafe fn drop(_: *const ()) {}
            RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
        }
        unsafe { Waker::from_raw(raw(flag as *const AtomicBool as *const ())) }
    }

    /// Every test leaves the hook the way it found it, so one that interrupts
    /// cannot make the next one fail.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            test_interrupt::clear();
        }
    }

    /// A reader parked on a bus that never fires must come back with the
    /// interruption rather than wait for data that is not coming. This is the
    /// whole bug: before, the only two outcomes were "the file became ready"
    /// and "never".
    #[test]
    fn an_interrupted_wait_gives_up_instead_of_parking_forever() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        test_interrupt::clear();
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        // Nothing on the bus and nothing interrupting: it parks, which is
        // correct and is what it always did.
        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));

        // Now the thread takes a signal. The bus has not changed at all.
        test_interrupt::interrupt_after(0, LxError::EINTR);
        assert!(matches!(
            Pin::new(&mut fut).poll(&mut cx),
            Poll::Ready(Err(LxError::EINTR))
        ));
    }

    /// A thread being torn down (`kill -9`, the process exiting) reaches the
    /// wait as `ESRCH`/`EINTR` just the same -- whatever the check answers is
    /// what the caller gets, unchanged. A wait that translated one error into
    /// another would hide which of the two it was.
    #[test]
    fn the_reason_for_the_interruption_reaches_the_caller_unchanged() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        for err in [LxError::EINTR, LxError::ESRCH, LxError::EIDRM] {
            let bus: Arc<Mutex<EventBus>> = EventBus::new();
            test_interrupt::interrupt_after(0, err);
            let mut fut = wait_for_event(bus, Event::READABLE);
            match Pin::new(&mut fut).poll(&mut cx) {
                Poll::Ready(Err(got)) => assert_eq!(got, err),
                other => panic!("expected {:?}, got {:?}", err, other.is_pending()),
            }
        }
    }

    /// Data wins over a signal: the readiness test comes FIRST, so a read
    /// that can be served right now is served. Linux synthesises `EINTR` only
    /// for a read that would otherwise block, and a kernel that checked the
    /// other way round would drop bytes that were already in the pipe.
    #[test]
    fn data_already_there_beats_a_pending_signal() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        bus.lock().set(Event::READABLE);
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        test_interrupt::interrupt_after(0, LxError::EINTR);
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        assert!(
            matches!(
                Pin::new(&mut fut).poll(&mut cx),
                Poll::Ready(Ok(Event::READABLE))
            ),
            "a signal must not swallow data that is already readable"
        );
    }

    /// An event the waiter did not ask for is still not a wakeup. The mask
    /// governs readiness exactly as before; the interruption check is an
    /// extra way OUT of the wait, never a new way to end it early with a
    /// success.
    #[test]
    fn an_unwanted_event_is_still_not_readiness() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        bus.lock().set(Event::WRITABLE);
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        test_interrupt::clear();
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
    }

    /// The parked callback and the backstop timer are both let go when the
    /// wait ends in an interruption, not only when it ends in data. A bus
    /// that kept one callback per interrupted read would fill its 4096-entry
    /// table and then thrash -- the same failure `Drop` already guards.
    #[test]
    fn an_interrupted_wait_leaves_nothing_parked_on_the_bus() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        for _ in 0..5_000 {
            test_interrupt::clear();
            let mut fut = wait_for_event(bus.clone(), Event::READABLE);
            assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
            assert_eq!(bus.lock().get_callback_len(), 1);

            test_interrupt::interrupt_after(0, LxError::EINTR);
            assert!(matches!(
                Pin::new(&mut fut).poll(&mut cx),
                Poll::Ready(Err(LxError::EINTR))
            ));
            assert_eq!(
                bus.lock().get_callback_len(),
                0,
                "an interrupted wait must unsubscribe, like a satisfied one"
            );
        }
    }

    /// A wait that is NOT interrupted keeps waiting however many times it is
    /// re-polled. The backstop tick re-polls a parked reader several times a
    /// second, so a check that answered "interrupted" spuriously -- or once
    /// and then latched -- would turn every quiet read into an `EINTR` storm.
    #[test]
    fn a_wait_nobody_interrupts_keeps_waiting_across_re_polls() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        test_interrupt::clear();
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        for _ in 0..1_000 {
            assert!(matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending));
        }
        // And exactly one callback the whole time: re-polls refresh the
        // subscription in place rather than pushing a new one.
        assert_eq!(bus.lock().get_callback_len(), 1);
    }

    /// The check runs on every pass, not only on the first. A reader parks
    /// long before the signal arrives -- that is the whole point of a
    /// blocking read -- so a check that only ran when the future was first
    /// polled would leave every already-parked reader exactly as stuck as
    /// before the fix.
    #[test]
    fn the_check_runs_on_every_pass_not_just_the_first() {
        let _restore = Restore;
        static WOKE: AtomicBool = AtomicBool::new(false);
        let bus: Arc<Mutex<EventBus>> = EventBus::new();
        let waker = noop_waker(&WOKE);
        let mut cx = Context::from_waker(&waker);

        // Ok for the first 20 passes, interrupted from the 21st on.
        test_interrupt::interrupt_after(20, LxError::EINTR);
        let mut fut = wait_for_event(bus.clone(), Event::READABLE);
        for pass in 0..20 {
            assert!(
                matches!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending),
                "pass {} should still be waiting",
                pass
            );
        }
        assert!(matches!(
            Pin::new(&mut fut).poll(&mut cx),
            Poll::Ready(Err(LxError::EINTR))
        ));
    }
}

#[cfg(test)]
mod seq_for_tests {
    //! The publication counter split by direction.

    use super::{Event, EventBus};

    const IN_MASK: Event = Event::READABLE.union(Event::ERROR).union(Event::CLOSED);

    /// What `poll_events_to_bus_mask(POLLIN)` asks for must not move when a
    /// producer publishes `WRITABLE` alone -- the pulse a unix socket sends a
    /// peer when a reader drains a full queue, and the one an eventfd sends
    /// on every `read` that empties it.
    #[test]
    fn a_writable_only_publication_leaves_the_readable_counter_alone() {
        let mut bus = EventBus::default();
        let before = bus.seq_for(IN_MASK);
        bus.set(Event::WRITABLE);
        assert_eq!(
            bus.seq_for(IN_MASK),
            before,
            "nothing readable was published"
        );
        assert_eq!(bus.seq(), 1, "the whole-bus counter still counts it");
        // And the other way round, which is the same bug for an EPOLLOUT entry.
        let out_before = bus.seq_for(Event::WRITABLE);
        bus.set(Event::READABLE);
        assert_eq!(bus.seq_for(Event::WRITABLE), out_before);
        assert_eq!(bus.seq_for(IN_MASK), before + 1);
    }

    /// A publication in the mask moves it even when the flag was already set:
    /// that republication is the whole reason the counter exists.
    #[test]
    fn a_republication_of_a_latched_flag_still_moves_the_masked_counter() {
        let mut bus = EventBus::default();
        bus.set(Event::READABLE);
        let once = bus.seq_for(IN_MASK);
        bus.set(Event::READABLE);
        assert_eq!(bus.seq_for(IN_MASK), once + 1);
        // Clearing publishes nothing, on either counter.
        bus.clear(Event::READABLE);
        assert_eq!(bus.seq_for(IN_MASK), once + 1);
    }

    /// One `change` publishing both directions moves both counters, and the
    /// sum a multi-bit mask reports moves by one per bit it covers.
    #[test]
    fn a_publication_of_several_bits_counts_on_each_of_them() {
        let mut bus = EventBus::default();
        bus.change(Event::empty(), Event::READABLE | Event::WRITABLE);
        assert_eq!(bus.seq_for(Event::READABLE), 1);
        assert_eq!(bus.seq_for(Event::WRITABLE), 1);
        assert_eq!(bus.seq_for(Event::READABLE | Event::WRITABLE), 2);
        assert_eq!(bus.seq(), 1, "one call, one whole-bus publication");
    }

    /// A mask naming none of the four file bits has nothing finer to measure,
    /// so it gets the whole-bus counter.
    #[test]
    fn a_mask_outside_the_file_bits_falls_back_to_the_whole_bus_counter() {
        let mut bus = EventBus::default();
        bus.set(Event::READABLE);
        bus.set(Event::PROCESS_QUIT);
        assert_eq!(bus.seq_for(Event::PROCESS_QUIT), bus.seq());
        assert_eq!(bus.seq_for(Event::empty()), bus.seq());
    }
}
