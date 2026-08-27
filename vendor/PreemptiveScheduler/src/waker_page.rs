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

    /// Whether any future on this page has a pending (published) wake. Used by
    /// the executor's pre-halt recheck; SeqCst load pairs with the SeqCst
    /// `notified.fetch_or` in `notify`.
    #[inline]
    pub fn has_notified(&self) -> bool {
        self.notified.load() != 0 || self.yielded.load() != 0
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

    pub fn mark_borrowed(&self, borrowed: bool) {
        self.page.mark_borrowed(self.idx, borrowed);
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
            let voluntary = crate::runtime::is_voluntary_yield_wake();
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
