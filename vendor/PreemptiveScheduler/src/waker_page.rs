use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
// use core::task::{RawWaker, RawWakerVTable};
use woke::Woke;

#[derive(Debug)]
pub struct AtomicU64SC(AtomicU64);
pub const WAKER_PAGE_SIZE: usize = 64;

impl AtomicU64SC {
    #[inline(always)]
    #[allow(unused)]
    pub fn new(val: u64) -> Self {
        AtomicU64SC(AtomicU64::new(val))
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn fetch_or(&self, val: u64) {
        self.0.fetch_or(val, Ordering::SeqCst);
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn fetch_and(&self, val: u64) {
        self.0.fetch_and(val, Ordering::SeqCst);
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn fetch_add(&self, val: u64) -> u64 {
        self.0.fetch_add(val, Ordering::SeqCst)
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn fetch_sub(&self, val: u64) -> u64 {
        self.0.fetch_sub(val, Ordering::SeqCst)
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn load(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn swap(&self, val: u64) -> u64 {
        self.0.swap(val, Ordering::SeqCst)
    }

    #[inline(always)]
    #[allow(unused)]
    pub fn as_mut_ptr(&mut self) -> *mut u64 {
        // `AtomicU64::as_mut_ptr` ya no existe en Rust moderno; usamos `as_ptr`.
        self.0.as_ptr()
    }
}

/// A page is used by the scheduler to hold the current status of 64 different futures in the
/// scheduler. So we use 64bit integers where the ith bit represents the ith future. Pages are
/// arranged by the scheduler in a `pages` vector of pages which grows as needed allocating space
/// for 64 more futures at a time.
#[derive(Debug)]
#[repr(align(64))]
pub struct WakerPage {
    /// Futures notified by an external wake — highest priority runnable lane.
    notified: AtomicU64SC,
    /// Voluntary yields (`YieldFuture` self-wake). Lower priority than [`notified`]:
    /// a CPU-bound hog that yields for wake-up preemption must not race an
    /// externally woken task for the next poll slot, or the woken task waits a
    /// full timeslice again (the interactivity failure `eclipse-bench` sees as
    /// `sleep 1ms late, load` jumping from ~µs to tens of ms).
    yielded: AtomicU64SC,
    // completed: AtomicU64SC,
    dropped: AtomicU64SC,
    borrowed: AtomicU64SC,
    /// Logical CPU whose [`TaskCollection`](crate::task_collection::TaskCollection)
    /// owns this page. A cross-CPU wake targets this CPU's run queue; if it is
    /// halted, `wake_by_ref` sends it a reschedule IPI so the wake is honoured
    /// immediately instead of at the next periodic tick (up to 4 ms away).
    pub(crate) owner_cpu: u8,
}

impl WakerPage {
    pub fn new_inner(owner_cpu: u8) -> Self {
        WakerPage {
            notified: AtomicU64SC::new(0),
            yielded: AtomicU64SC::new(0),
            // completed: AtomicU64SC::new(0),
            dropped: AtomicU64SC::new(0),
            borrowed: AtomicU64SC::new(0),
            owner_cpu,
        }
    }

    pub fn new(owner_cpu: u8) -> Arc<Self> {
        Arc::new(WakerPage::new_inner(owner_cpu))
    }

    pub fn initialize(&self, idx: usize) {
        debug_assert!(idx < 64);
        self.notified.fetch_or(1 << idx);
        self.yielded.fetch_and(!(1 << idx));
        // self.completed.fetch_and(!(1 << idx));
        self.dropped.fetch_and(!(1 << idx));
        self.borrowed.fetch_and(!(1 << idx));
    }

    pub fn mark_dropped(&self, idx: usize) {
        debug_assert!(idx < 64);
        self.dropped.fetch_or(1 << idx);
    }

    // pub fn mark_complete(&self, idx: usize) {
    //     debug_assert!(idx < 64);
    //     self.completed.fetch_or(1 << idx);
    // }

    pub fn notify(&self, offset: usize) {
        debug_assert!(offset < 64);
        self.notified.fetch_or(1 << offset);
        // An external wake promotes out of the voluntary-yield lane.
        self.yielded.fetch_and(!(1 << offset));
    }

    /// Park a self-wake from `YieldFuture` behind every urgent notify.
    ///
    /// Unlike [`notify`], this does not raise wake-up preemption: the task is
    /// already on the CPU and is voluntarily giving it up.
    pub fn mark_yielded(&self, offset: usize) {
        debug_assert!(offset < 64);
        self.yielded.fetch_or(1 << offset);
    }

    pub fn mark_borrowed(&self, offset: usize, borrowed: bool) {
        debug_assert!(offset < 64);
        if borrowed {
            self.borrowed.fetch_or(1 << offset);
        } else {
            self.borrowed.fetch_and(!(1 << offset));
        }
    }

    /// Whether the task at `offset` is currently checked out to an executor.
    ///
    /// A wake for a borrowed task is *deferred* by `take_notified` until the
    /// in-flight poll releases the borrow, so it must not raise a reschedule
    /// request — the task already owns a CPU. This also filters the
    /// self-wake that `YieldFuture` performs (`wake_by_ref` from inside its own
    /// poll), which would otherwise request a preemption on every yield.
    #[inline]
    pub fn is_borrowed(&self, offset: usize) -> bool {
        debug_assert!(offset < 64);
        self.borrowed.load() & (1 << offset) != 0
    }

    // pub fn mark_completed(&self, offset: usize) {
    //     debug_assert!(offset < 64);
    //     self.completed.fetch_or(1 << offset);
    // }

    /// Return urgent (externally woken) futures ready to poll. Does **not**
    /// promote voluntary yields — see [`take_yielded`].
    pub fn take_notified(&self) -> u64 {
        // Unset all ready bits, since spurious notifications for completed futures would lead
        // us to poll them after completion.
        let raw = self.notified.swap(0);
        let dropped = self.dropped.load();
        let borrowed = self.borrowed.load();
        // A wake that races with an in-flight poll (bit borrowed by an executor,
        // possibly on another CPU via work stealing) must not be discarded with
        // the swap above: re-publish it so the task is polled again once the
        // borrow is released. Discarding it left the task asleep forever.
        let deferred = raw & borrowed & !dropped;
        if deferred != 0 {
            self.notified.fetch_or(deferred);
        }
        raw & !dropped & !borrowed
    }

    /// Return voluntarily-yielded futures, only after urgent notifies are drained
    /// across the collection (see the generator's two-pass scan).
    pub fn take_yielded(&self) -> u64 {
        let raw = self.yielded.swap(0);
        let dropped = self.dropped.load();
        let borrowed = self.borrowed.load();
        let deferred = raw & borrowed & !dropped;
        if deferred != 0 {
            self.yielded.fetch_or(deferred);
        }
        raw & !dropped & !borrowed
    }

    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0)
    }

    /// Whether any future on this page has a wake **this CPU could act on**.
    ///
    /// The same question [`TaskCollection::has_ready`] asks, and it has to be
    /// asked the same way: a published bit is not work unless the task is
    /// neither finished nor already checked out to an executor. This read the
    /// raw lanes, so a page whose only wake belonged to a task being polled on
    /// another CPU reported work — and the caller this doc named is a pre-halt
    /// recheck, so that CPU would skip the halt, drain nothing (both
    /// `take_*`s defer a borrowed slot), and go round again until the other
    /// CPU released the borrow.
    ///
    /// It has no caller today: the executor asks `TaskCollection::has_ready`,
    /// which was written with the mask and answers per CPU (it also honours
    /// affinity). This is the page-local form of it, correct now if anything
    /// reaches for it.
    ///
    /// SeqCst loads pair with the SeqCst `notified.fetch_or` in [`notify`].
    ///
    /// [`TaskCollection::has_ready`]: crate::task_collection::TaskCollection::has_ready
    #[inline]
    pub fn has_notified(&self) -> bool {
        let published = self.notified.load() | self.yielded.load();
        let blocked = self.dropped.load() | self.borrowed.load();
        published & !blocked != 0
    }

    /// Whether the task at `offset` has a wake published and nothing blocking
    /// it — the state that makes it work for whoever owns this page.
    ///
    /// Same mask as [`has_notified`], narrowed to one slot, minus `borrowed`:
    /// the one caller is [`WakerRef::mark_borrowed`] releasing that very
    /// borrow, so it asks about the slot it is about to unblock.
    #[inline]
    pub fn has_pending_wake(&self, offset: usize) -> bool {
        debug_assert!(offset < 64);
        let bit = 1u64 << offset;
        (self.notified.load() | self.yielded.load()) & !self.dropped.load() & bit != 0
    }

    /// Non-destructive snapshot of `(notified | yielded, dropped, borrowed)` for diagnostics.
    pub fn peek(&self) -> (u64, u64, u64) {
        (
            self.notified.load() | self.yielded.load(),
            self.dropped.load(),
            self.borrowed.load(),
        )
    }

    pub fn clear(&self, idx: usize) {
        debug_assert!(idx < 64);
        let mask = !(1 << idx);
        self.notified.fetch_and(mask);
        self.yielded.fetch_and(mask);
        // self.completed.fetch_and(mask);
        self.dropped.fetch_and(mask);
        self.borrowed.fetch_and(mask)
    }

    pub fn make_waker(self: &Arc<Self>, idx: usize, dropped: &Arc<AtomicBool>) -> WakerRef {
        WakerRef {
            page: self.clone(),
            idx,
            dropped: dropped.clone(),
        }
    }
}

pub type DroperRef = WakerRef;

pub struct WakerRef {
    page: Arc<WakerPage>,
    idx: usize,
    dropped: Arc<AtomicBool>,
}

impl WakerRef {
    // pub fn mark_complete(&self) {
    //     self.page.mark_completed(self.idx);
    // }

    /// Check this task out to an executor, or hand it back.
    ///
    /// Releasing the borrow is a wake in its own right, and until now it was
    /// the one state change on this page that published nothing and kicked
    /// nobody. `take_notified` and `take_yielded` both *defer* a wake that
    /// lands on a borrowed slot — they re-publish it and report nothing — and
    /// `has_ready`, the owner's pre-halt recheck, masks `borrowed` out for the
    /// same reason. So while a task is checked out, every reader agrees there
    /// is no work, correctly. What makes it work again is this call.
    ///
    /// On the CPU that owns the queue that costs nothing: it goes straight
    /// back to `take_task` and finds the task itself. Under work stealing it
    /// is a lost wake. The borrow is held by the *thief*, the wake was
    /// published on the *owner's* page, and the owner — having seen the bit
    /// masked by `borrowed` on its own last look — is halted. The thief then
    /// releases the borrow and returns to its own run queue. Nothing tells the
    /// owner, so a task that is runnable right now waits for that CPU's next
    /// periodic tick: up to 4 ms at 250 Hz, on every wake that races a steal.
    ///
    /// So ask, and kick a sleeping owner. The two halves close over each
    /// other: the waker samples `borrowed` before publishing, we publish
    /// `borrowed = false` before reading its lanes, and both pairs are SeqCst.
    /// In the one interleaving where the waker still saw us borrowed and we
    /// still miss its notify, its own `maybe_send_resched_ipi` ran after the
    /// notify — and if the owner was not sleeping yet, the owner's halt
    /// protocol (publish sleeping, then recheck) is ordered after both and
    /// sees the task runnable. No interleaving leaves everyone silent.
    ///
    /// `maybe_send_resched_ipi` and not `request_resched`: the task is already
    /// on the owner's queue and there is nothing to preempt for, only a halted
    /// CPU to wake. It also makes the owner's own release free — a CPU that is
    /// executing is never in the sleeping mask.
    pub fn mark_borrowed(&self, borrowed: bool) {
        self.page.mark_borrowed(self.idx, borrowed);
        if !borrowed && self.page.has_pending_wake(self.idx) {
            crate::runtime::maybe_send_resched_ipi(self.page.owner_cpu);
        }
    }

    pub fn wake_by_ref(&self) {
        if !self.dropped.load(Ordering::SeqCst) {
            // Sampled BEFORE the notify: a task already checked out to an
            // executor is running (or about to run) on a CPU of its own, so its
            // wake is deferred by `take_notified`/`take_yielded` and there is
            // nothing to preempt for. Sampling after would race with the poll
            // releasing the borrow and turn every `yield_now` into a reschedule
            // request.
            let in_flight = self.page.is_borrowed(self.idx);
            // Keyed by this waker's own address — the number
            // `YieldFuture` publishes through `begin_voluntary_yield` — so a
            // wake raised on this CPU for a *different* task cannot borrow the
            // yield marker and be filed in the low-priority lane.
            let voluntary =
                crate::runtime::is_voluntary_yield_wake(self as *const WakerRef as usize);
            if in_flight && voluntary {
                // `YieldFuture` self-wake: park in the yielded lane so an
                // externally woken peer is preferred on the next take_task.
                // Still deliver the IPI so a stolen task's owner is not left
                // halted with a deferred bit.
                self.page.mark_yielded(self.idx);
                crate::runtime::maybe_send_resched_ipi(self.page.owner_cpu);
                return;
            }
            // External wake (possibly while borrowed — deferred by take_*).
            // If that CPU is halted it would not look again until its next
            // periodic tick (up to 4 ms); if it is *busy* running another task
            // it would not look again until that task's timeslice expires.
            // `request_resched` covers both. Ordering: `notify` is a SeqCst
            // RMW and the sleeping-mask load inside is SeqCst, which pairs with
            // the executor's publish-sleeping-then-recheck sequence so a wake
            // can never fall between its final queue check and the halt.
            self.page.notify(self.idx);
            if in_flight {
                crate::runtime::maybe_send_resched_ipi(self.page.owner_cpu);
            } else {
                crate::runtime::request_resched(self.page.owner_cpu);
            }
        }
    }

    pub fn drop_by_ref(&self) {
        if !self.dropped.swap(true, Ordering::SeqCst) {
            self.page.mark_dropped(self.idx);
        }
    }
}

impl Woke for WakerRef {
    fn wake_by_ref(waker: &Arc<Self>) {
        waker.wake_by_ref();
    }
}

impl Clone for WakerRef {
    fn clone(&self) -> Self {
        WakerRef {
            page: self.page.clone(),
            idx: self.idx,
            dropped: self.dropped.clone(),
        }
    }
}

/// The scheduler's cross-CPU wake bitmap, which had no tests.
///
/// Every wake in the kernel arrives here: a page holds 64 tasks' worth of
/// state in four `u64`s, and the decisions it makes — which lane a wake goes
/// in, whether it is deferred, whether it is discarded — are what the
/// generator hands the executor. Its own comments record what the mistakes
/// cost: a discarded wake left a task asleep forever, and a completed task
/// republished as runnable let two executors poll one future (the 8-second
/// deadlock banner).
///
/// These tests share no global state, so they do not need serializing; the
/// two that go through `WakerRef::wake_by_ref` do touch the runtime's
/// reschedule globals, and say so.
#[cfg(test)]
mod waker_page_tests {
    use super::*;

    fn page() -> Arc<WakerPage> {
        WakerPage::new(0)
    }

    // ── the four lanes ─────────────────────────────────────────────────────

    #[test]
    fn a_fresh_slot_is_published_as_runnable_and_nothing_else() {
        let p = page();
        // A task nobody has woken yet still has to be polled once, or its
        // future never starts. `initialize` is also reached on a slab index
        // that a *previous* task used, so every other lane has to be wiped.
        p.mark_dropped(7);
        p.mark_borrowed(7, true);
        p.mark_yielded(7);
        p.initialize(7);
        assert_eq!(p.peek(), (1 << 7, 0, 0));
        assert_eq!(p.take_notified(), 1 << 7);
        // And nothing in the other lane: `peek` sums the two, so a leftover
        // yielded bit hides there and would hand the slot out a second time
        // on the generator's second pass.
        assert_eq!(p.take_yielded(), 0);
        assert!(!p.is_borrowed(7));
    }

    #[test]
    fn an_external_wake_promotes_a_task_out_of_the_yielded_lane() {
        let p = page();
        p.mark_yielded(3);
        p.notify(3);
        // Not in both: the yielded lane is drained only after the notified
        // one, so a task left in it would be handed out twice.
        assert_eq!(p.take_notified(), 1 << 3);
        assert_eq!(p.take_yielded(), 0);
    }

    #[test]
    fn a_voluntary_yield_does_not_demote_a_wake_that_already_arrived() {
        let p = page();
        p.notify(3);
        p.mark_yielded(3);
        // `mark_yielded` deliberately does not touch the notified lane: the
        // urgent wake still wins, and the extra yielded bit is consumed
        // harmlessly on the pass after it.
        assert_eq!(p.take_notified(), 1 << 3);
    }

    // ── deferral: a wake that races an in-flight poll ──────────────────────

    #[test]
    fn a_wake_that_races_an_in_flight_poll_is_held_not_lost() {
        let p = page();
        p.initialize(1);
        assert_eq!(p.take_notified(), 1 << 1);
        p.mark_borrowed(1, true);
        // The task is checked out to an executor (possibly on another CPU, via
        // work stealing). The wake cannot be handed out now — that is two
        // executors on one future — and it must not be dropped either.
        p.notify(1);
        assert_eq!(p.take_notified(), 0, "a borrowed task was handed out");
        assert_eq!(p.peek().0, 1 << 1, "the deferred wake was swallowed");
        p.mark_borrowed(1, false);
        assert_eq!(
            p.take_notified(),
            1 << 1,
            "the deferred wake never came back"
        );
    }

    #[test]
    fn the_yielded_lane_defers_a_borrowed_task_the_same_way() {
        let p = page();
        p.mark_borrowed(2, true);
        p.mark_yielded(2);
        assert_eq!(p.take_yielded(), 0);
        p.mark_borrowed(2, false);
        assert_eq!(p.take_yielded(), 1 << 2);
    }

    #[test]
    fn a_wake_for_a_completed_task_is_discarded_by_both_lanes() {
        let p = page();
        p.mark_borrowed(4, true);
        p.mark_dropped(4);
        // A stale waker clone (a net-RX slot, an epoll timer) firing after the
        // poll returned Ready. Republishing it would get the slab slot handed
        // out and the finished future polled again over its own teardown.
        p.notify(4);
        p.mark_yielded(4);
        assert_eq!(p.take_notified(), 0);
        assert_eq!(p.take_yielded(), 0);
        assert_eq!(
            p.peek().0,
            0,
            "a dropped task's wake was republished for the next drain"
        );
    }

    #[test]
    fn taking_the_dropped_bits_reports_them_once() {
        let p = page();
        p.mark_dropped(5);
        p.mark_dropped(9);
        assert_eq!(p.take_dropped(), (1 << 5) | (1 << 9));
        assert_eq!(p.take_dropped(), 0);
    }

    #[test]
    fn clearing_a_slot_wipes_every_lane_so_the_index_can_be_reused() {
        let p = page();
        p.notify(11);
        p.mark_yielded(11);
        p.mark_dropped(11);
        p.mark_borrowed(11, true);
        p.notify(12);
        p.clear(11);
        // Only slot 11: `remove` frees one slab index, and wiping a neighbour
        // would lose a live task's wake.
        assert_eq!(p.peek(), (1 << 12, 0, 0));
        assert!(!p.is_borrowed(11));
    }

    #[test]
    fn peeking_consumes_nothing() {
        let p = page();
        p.notify(0);
        p.mark_yielded(1);
        p.mark_dropped(2);
        p.mark_borrowed(3, true);
        let before = p.peek();
        assert_eq!(before, ((1 << 0) | (1 << 1), 1 << 2, 1 << 3));
        assert_eq!(p.peek(), before);
        assert_eq!(p.take_notified(), 1 << 0);
    }

    #[test]
    fn a_pending_wake_is_only_pending_while_something_can_act_on_it() {
        let p = page();
        // `has_notified` is the cheap "is there anything to do here" question,
        // and it must answer the same one `TaskCollection::has_ready` asks
        // with `notified & !dropped & !borrowed`. Answering on the raw bits
        // instead made a page whose only wake belonged to a task checked out
        // to another CPU report work this CPU could not take — and the caller
        // of that question is a pre-halt recheck, so the CPU would skip the
        // halt, find nothing, and spin until the other CPU released the
        // borrow.
        p.mark_borrowed(6, true);
        p.notify(6);
        assert!(!p.has_notified(), "a deferred wake is not work for us");
        p.mark_borrowed(6, false);
        assert!(p.has_notified());

        let q = page();
        q.mark_dropped(6);
        q.notify(6);
        assert!(!q.has_notified(), "a completed task is not work for anyone");
    }

    #[test]
    fn a_page_remembers_which_cpu_owns_it() {
        // The wake kick is addressed with this: a page stamped with the wrong
        // CPU sends the reschedule IPI to a core that owns none of its tasks.
        assert_eq!(WakerPage::new(0).owner_cpu, 0);
        assert_eq!(WakerPage::new(5).owner_cpu, 5);
    }

    // ── WakerRef: which lane a wake is filed in ────────────────────────────
    //
    // These reach `crate::runtime`'s reschedule globals, which the resched
    // tests also own, so they take the same kind of lock.

    fn wake_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::runtime::resched_test_lock()
    }

    fn waker_for(p: &Arc<WakerPage>, idx: usize) -> Arc<WakerRef> {
        let flag = Arc::new(AtomicBool::new(false));
        p.initialize(idx);
        Arc::new(p.make_waker(idx, &flag))
    }

    #[test]
    fn a_tasks_own_yield_is_filed_behind_every_urgent_wake() {
        let _g = wake_lock();
        let p = page();
        let w = waker_for(&p, 20);
        p.take_notified();
        p.mark_borrowed(20, true);

        crate::runtime::begin_voluntary_yield(Arc::as_ptr(&w) as usize);
        w.wake_by_ref();
        crate::runtime::end_voluntary_yield();

        p.mark_borrowed(20, false);
        assert_eq!(p.take_notified(), 0, "a yield jumped the urgent lane");
        assert_eq!(p.take_yielded(), 1 << 20);
    }

    #[test]
    fn an_interrupt_inside_the_yield_window_does_not_demote_someone_elses_wake() {
        let _g = wake_lock();
        let p = page();
        let mine = waker_for(&p, 21);
        let theirs = waker_for(&p, 22);
        p.take_notified();
        p.mark_borrowed(21, true);
        p.mark_borrowed(22, true);

        // We are inside our own `yield_now`, which is three instructions long
        // and runs with interrupts ON. An IRQ lands there and wakes another
        // task — net RX, a timer, a futex release. That is an external wake
        // and belongs in the urgent lane, whatever this CPU happens to be
        // doing at the time.
        crate::runtime::begin_voluntary_yield(Arc::as_ptr(&mine) as usize);
        theirs.wake_by_ref();
        crate::runtime::end_voluntary_yield();

        p.mark_borrowed(22, false);
        assert_eq!(
            p.take_notified(),
            1 << 22,
            "an external wake was parked in the low-priority lane"
        );
    }

    #[test]
    fn a_yield_marker_left_behind_cannot_demote_a_later_wake() {
        let _g = wake_lock();
        let p = page();
        let stale = waker_for(&p, 23);
        let live = waker_for(&p, 24);
        p.take_notified();
        p.mark_borrowed(24, true);

        // `end_voluntary_yield` never ran: the poll was abandoned between the
        // two calls (oops containment). Unkeyed, this CPU would answer
        // "voluntary" to every external wake it raised from here on.
        crate::runtime::begin_voluntary_yield(Arc::as_ptr(&stale) as usize);
        live.wake_by_ref();

        p.mark_borrowed(24, false);
        assert_eq!(p.take_notified(), 1 << 24);
        crate::runtime::end_voluntary_yield();
    }

    #[test]
    fn a_wake_for_a_finished_task_never_reaches_its_page() {
        let _g = wake_lock();
        let p = page();
        let flag = Arc::new(AtomicBool::new(false));
        p.initialize(25);
        let w = Arc::new(p.make_waker(25, &flag));
        p.take_notified();

        w.drop_by_ref();
        assert_eq!(p.peek().1, 1 << 25);
        // The `dropped` flag is shared with `Task::poll`, which returns Ready
        // without entering the future once it is set; a second retire must not
        // look like a second completion.
        w.drop_by_ref();

        w.wake_by_ref();
        assert_eq!(p.take_notified(), 0);
        assert_eq!(p.take_yielded(), 0);
    }

    // ── handing a task back: the wake that was deferred while it ran ───────
    //
    // `take_notified`/`take_yielded` defer a wake that lands on a borrowed
    // slot, and `has_ready` masks `borrowed` out, so while a task is checked
    // out every reader agrees there is nothing to do. Releasing the borrow is
    // what makes it work again — and under work stealing the CPU doing the
    // releasing is not the CPU that owns the queue.

    /// Install the recording sender and put `owner` in (or out of) the
    /// sleeping mask. Returns the shared lock, so the sender cannot be swapped
    /// out from under the test by `runtime`'s own resched tests.
    fn owner_asleep(owner: u8, asleep: bool) -> std::sync::MutexGuard<'static, ()> {
        let guard = wake_lock();
        KICKED.store(0, Ordering::SeqCst);
        for cpu in 0..8 {
            crate::runtime::set_cpu_sleeping(cpu, false);
        }
        crate::runtime::set_cpu_sleeping(owner as usize, asleep);
        crate::runtime::set_resched_ipi_sender(record_kick);
        guard
    }

    static KICKED: AtomicU64 = AtomicU64::new(0);

    fn record_kick(cpu: usize) {
        if cpu < 64 {
            KICKED.fetch_or(1u64 << cpu, Ordering::SeqCst);
        }
    }

    fn kicked() -> u64 {
        KICKED.load(Ordering::SeqCst)
    }

    #[test]
    fn giving_back_a_stolen_task_wakes_the_halted_owner() {
        let _g = owner_asleep(3, true);
        let p = WakerPage::new(3);
        let w = waker_for(&p, 9);
        p.take_notified();
        // CPU 3's task is being polled somewhere else; a wake lands and is
        // deferred, so CPU 3's own look found nothing and it halted.
        p.mark_borrowed(9, true);
        p.notify(9);
        assert_eq!(p.take_notified(), 0, "a borrowed slot was handed out");

        // The thief finishes the poll and hands the task back. Nothing else
        // will tell CPU 3, and the task is runnable right now.
        w.mark_borrowed(false);
        assert_eq!(kicked(), 1 << 3, "the owner was left halted");
    }

    #[test]
    fn a_voluntary_yield_deferred_by_the_steal_wakes_the_owner_too() {
        let _g = owner_asleep(2, true);
        let p = WakerPage::new(2);
        let w = waker_for(&p, 4);
        p.take_notified();
        p.mark_borrowed(4, true);
        // The other lane is deferred by exactly the same rule, and a stolen
        // task that yields is how a CPU-bound thread gives its slice back.
        p.mark_yielded(4);

        w.mark_borrowed(false);
        assert_eq!(kicked(), 1 << 2);
    }

    #[test]
    fn handing_back_a_task_nobody_woke_interrupts_nobody() {
        let _g = owner_asleep(3, true);
        let p = WakerPage::new(3);
        let w = waker_for(&p, 9);
        p.take_notified();
        p.mark_borrowed(9, true);

        // A poll that returned Pending with no wake pending is the common
        // case by far. Kicking here would put an IPI on every single one.
        w.mark_borrowed(false);
        assert_eq!(kicked(), 0);
    }

    #[test]
    fn a_finished_task_does_not_get_its_owner_woken() {
        let _g = owner_asleep(3, true);
        let p = WakerPage::new(3);
        let w = waker_for(&p, 9);
        p.take_notified();
        p.mark_borrowed(9, true);
        // A stale waker re-notifying a completed task is the race `Task::poll`
        // guards with `finish`; the bit is set but there is nothing to run.
        p.notify(9);
        p.mark_dropped(9);

        w.mark_borrowed(false);
        assert_eq!(kicked(), 0, "a dead task woke a CPU");
    }

    #[test]
    fn an_owner_that_is_not_halted_is_not_interrupted() {
        let _g = owner_asleep(3, false);
        let p = WakerPage::new(3);
        let w = waker_for(&p, 9);
        p.take_notified();
        p.mark_borrowed(9, true);
        p.notify(9);

        // A CPU that is executing reaches its own `take_task` without an
        // interrupt. This is also what makes the un-stolen case free: the
        // releasing CPU is the owner, and it is never in the sleeping mask.
        w.mark_borrowed(false);
        assert_eq!(kicked(), 0);
    }

    #[test]
    fn taking_a_task_out_never_kicks() {
        let _g = owner_asleep(3, true);
        let p = WakerPage::new(3);
        let w = waker_for(&p, 9);
        // The slot is notified from `initialize` and never taken, so the
        // pending-wake test would say yes — checking out a task must not ask.
        w.mark_borrowed(true);
        assert_eq!(kicked(), 0);
        assert!(p.is_borrowed(9));
    }

    #[test]
    fn the_pending_wake_test_answers_for_one_slot_and_not_its_neighbours() {
        let p = page();
        p.notify(30);
        assert!(p.has_pending_wake(30));
        assert!(!p.has_pending_wake(29));
        assert!(!p.has_pending_wake(31));
        // Both lanes count, and `dropped` blocks either.
        let q = page();
        q.mark_yielded(30);
        assert!(q.has_pending_wake(30));
        q.mark_dropped(30);
        assert!(!q.has_pending_wake(30));
        // A borrow does not: the one caller is releasing that very borrow, so
        // it would answer no to every question it asks.
        let r = page();
        r.notify(30);
        r.mark_borrowed(30, true);
        assert!(r.has_pending_wake(30));
    }
}
