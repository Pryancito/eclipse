//! Cancellable timer wake-ups for futures that block on I/O.
//!
//! [`timer_set`](crate::timer::timer_set) is fire-and-forget: it takes an owned
//! callback and hands back no handle, so a future that arms a fallback deadline
//! (poll/epoll/select backstops, TTY reads, socket waits) has no way to say
//! "never mind" when the I/O it was waiting for arrives first. Leaving those
//! callbacks to fire on their own is not harmless: each one owns a cloned
//! [`Waker`], so a task that completed and was freed still gets woken through a
//! stale waker minutes later — the use-after-free class of bug that
//! `docs/README-crash-repro.md` traces back to exactly this shape.
//!
//! A [`TimerWakerSlot`] is that missing handle. The armed callback and the slot
//! share one refcounted cell; cancelling flips a flag and **drops the waker**,
//! so when the timer eventually fires it finds nothing to do. The callback is
//! never unregistered from the timer heap — it simply becomes a no-op — which
//! keeps cancellation lock-free-ish and safe to call from `Drop`.
//!
//! Callers keep an `Option<TimerWakerSlot>` in their future and drive it with
//! [`ensure_timer_waker`] (arm, or refresh the waker if the same deadline is
//! already armed) and [`kill_timer_waker`] (cancel; idempotent). Dropping the
//! slot cancels too, so a future that forgets to cancel in its own `Drop` still
//! cannot leave a stale waker behind.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Waker};
use core::time::Duration;

use lock::Mutex;

/// The owned callback [`crate::timer::timer_set`] takes.
type TimerCallback = Box<dyn FnOnce(Duration) + Send + Sync>;

/// State shared between the armed timer callback and the owner's slot.
struct TimerWakerInner {
    /// Deadline this cell was armed for. Lets [`ensure_timer_waker`] recognise
    /// a re-poll that wants the same wake-up and refresh the waker in place
    /// instead of arming a second timer for the same instant.
    deadline: Duration,
    /// Set by whichever side gets there first: the owner cancelling, or the
    /// callback firing. One-way, so a fire and a cancel can race and exactly
    /// one of them wins.
    done: AtomicBool,
    /// The waker to fire. Taken on fire, dropped on cancel — never left behind
    /// pointing at a task that may already be gone.
    waker: Mutex<Option<Waker>>,
}

impl TimerWakerInner {
    /// Neuter the armed callback: mark the cell done and drop the waker.
    ///
    /// Idempotent, and safe from `Drop`.
    fn cancel(&self) {
        // Mark done first: a callback firing concurrently on another CPU then
        // sees `done` already set and returns without touching the waker.
        self.done.store(true, Ordering::SeqCst);
        // Drop the waker even if the callback won the race and already took
        // it — `take` on `None` is fine, and this is what guarantees no stale
        // waker outlives the future.
        *self.waker.lock() = None;
    }
}

/// A live timer wake-up that its owner can cancel.
///
/// Dropping the slot cancels it. The armed callback keeps its own reference to
/// the shared cell, so the timer itself stays in the heap either way — it just
/// finds nothing left to wake.
pub struct TimerWakerSlot {
    inner: Arc<TimerWakerInner>,
}

impl TimerWakerSlot {
    /// The deadline this slot was armed for.
    pub fn deadline(&self) -> Duration {
        self.inner.deadline
    }

    /// Whether the timer has already fired or been cancelled.
    pub fn is_done(&self) -> bool {
        self.inner.done.load(Ordering::SeqCst)
    }
}

impl Drop for TimerWakerSlot {
    /// Cancel on the way out.
    ///
    /// Every owner is *also* expected to call [`kill_timer_waker`] explicitly —
    /// it says what it means, and it empties the slot — but whether a timer
    /// still points at a task is not really the owner's decision to make, and
    /// getting it wrong is a use-after-free rather than a missed wake-up. Ten
    /// futures across four crates park a slot in a field; this makes the
    /// eleventh safe by construction.
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

/// Ensure `slot` holds a live wake-up for `deadline`, registering `cx`'s waker.
///
/// - Slot already armed for this exact deadline and still live: the stored
///   waker is replaced with `cx`'s. A future may be polled by a different task
///   context than the one that armed the timer, so refreshing is what keeps the
///   wake-up pointed at whoever is actually waiting.
/// - Slot empty, already fired, or armed for a *different* deadline: the old
///   one is cancelled and a fresh timer is armed.
pub fn ensure_timer_waker(slot: &mut Option<TimerWakerSlot>, deadline: Duration, cx: &Context<'_>) {
    ensure_timer_waker_with(slot, deadline, cx.waker(), |deadline, callback| {
        crate::timer::timer_set(deadline, callback)
    })
}

/// The decision behind [`ensure_timer_waker`], with the timer handed in.
///
/// `arm` gets the deadline and the callback a real `timer_set` would own. The
/// only reason this is a parameter is that the interesting behaviour here is
/// *when the callback runs relative to the rest of this function*, and a test
/// that cannot hold the callback cannot ask that question at all.
fn ensure_timer_waker_with(
    slot: &mut Option<TimerWakerSlot>,
    deadline: Duration,
    waker: &Waker,
    arm: impl FnOnce(Duration, TimerCallback),
) {
    if let Some(existing) = slot.as_ref() {
        if !existing.is_done() && existing.inner.deadline == deadline {
            // Clone before taking the cell's lock: cloning a waker runs the
            // task's own code, and this lock is also taken from IRQ context.
            let fresh = waker.clone();
            *existing.inner.waker.lock() = Some(fresh);
            // The armed callback may have fired between the `is_done` above
            // and that store. It runs exactly once and takes whatever waker it
            // finds, so if it took the *old* one, the waker just stored sits in
            // a cell no timer will ever visit again: nothing re-polls this
            // future, and the `poll`/`read`/`semop` waiting on it hangs for
            // good instead of timing out. Deliver the wake here instead.
            if existing.is_done() {
                let orphan = existing.inner.waker.lock().take();
                if let Some(orphan) = orphan {
                    orphan.wake();
                }
            }
            return;
        }
        // Fired, cancelled, or armed for another instant: let it go.
        kill_timer_waker(slot);
    }

    let inner = Arc::new(TimerWakerInner {
        deadline,
        done: AtomicBool::new(false),
        waker: Mutex::new(Some(waker.clone())),
    });
    let fired = inner.clone();
    arm(
        deadline,
        Box::new(move |_now: Duration| {
            // Lost the race against a cancel (or a duplicate fire): the waker
            // is already gone and the owner is no longer interested.
            if fired.done.swap(true, Ordering::SeqCst) {
                return;
            }
            // Take the waker out from under the lock before waking: `wake`
            // runs scheduler code (and this callback runs in timer-IRQ
            // context), so holding this cell's lock across it would widen the
            // window for no reason.
            let waker = fired.waker.lock().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }),
    );
    *slot = Some(TimerWakerSlot { inner });
}

/// Cancel the wake-up in `slot`, if any, and empty the slot.
///
/// Idempotent and safe from `Drop`. After this returns, the armed callback (if
/// it has not run yet) is guaranteed to be a no-op and to hold no `Waker`, so
/// the task it referred to can be freed.
pub fn kill_timer_waker(slot: &mut Option<TimerWakerSlot>) {
    // The cancelling is `TimerWakerSlot`'s own `Drop`; taking the slot out of
    // the caller's field is what this function adds.
    drop(slot.take());
}

/// A [`Waker`] the tests can watch — and interrupt.
///
/// Wakers are opaque: nothing in `core` says who was woken, or when. These
/// tests need both, plus one thing more. The window this module has to get
/// right is *inside* `ensure_timer_waker_with`'s refresh, between reading
/// `done` and storing the new waker, and the only work done there is cloning
/// the waker. So the probe can run a hook from its own `clone`, which puts a
/// test exactly where a second CPU's timer IRQ used to be able to land —
/// deterministically, with no threads and no sleeping.
#[cfg(test)]
pub(crate) mod test_waker {
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{RawWaker, RawWakerVTable, Waker};

    use lock::Mutex;

    pub(crate) struct Probe {
        wakes: AtomicUsize,
        clones: AtomicUsize,
        on_next_clone: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Probe {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                wakes: AtomicUsize::new(0),
                clones: AtomicUsize::new(0),
                on_next_clone: Mutex::new(None),
            })
        }

        /// How many times this probe's waker has been woken.
        pub(crate) fn wakes(&self) -> usize {
            self.wakes.load(Ordering::SeqCst)
        }

        /// How many times it has been cloned (i.e. parked somewhere).
        pub(crate) fn clones(&self) -> usize {
            self.clones.load(Ordering::SeqCst)
        }

        /// Run `f` once, from inside the next clone of this probe's waker.
        pub(crate) fn interrupt_next_clone(&self, f: Box<dyn FnOnce() + Send>) {
            *self.on_next_clone.lock() = Some(f);
        }
    }

    pub(crate) fn waker_of(probe: &Arc<Probe>) -> Waker {
        let data = Arc::into_raw(probe.clone()) as *const ();
        unsafe { Waker::from_raw(RawWaker::new(data, &VTABLE)) }
    }

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_raw);

    unsafe fn clone(data: *const ()) -> RawWaker {
        let probe = unsafe { &*(data as *const Probe) };
        probe.clones.fetch_add(1, Ordering::SeqCst);
        unsafe { Arc::increment_strong_count(data as *const Probe) };
        // Out from under the lock before running it: the hook is free to touch
        // this probe again.
        let hook = probe.on_next_clone.lock().take();
        if let Some(hook) = hook {
            hook();
        }
        RawWaker::new(data, &VTABLE)
    }

    unsafe fn wake(data: *const ()) {
        let probe = unsafe { Arc::from_raw(data as *const Probe) };
        probe.wakes.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn wake_by_ref(data: *const ()) {
        let probe = unsafe { &*(data as *const Probe) };
        probe.wakes.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn drop_raw(data: *const ()) {
        unsafe { drop(Arc::from_raw(data as *const Probe)) };
    }
}

#[cfg(test)]
mod tests {
    use super::test_waker::{waker_of, Probe};
    use super::*;
    use alloc::sync::Arc;

    const DL: Duration = Duration::from_millis(50);
    const LATER: Duration = Duration::from_millis(70);

    /// A timer tick. Nothing here reads it.
    const TICK: Duration = Duration::from_millis(50);

    /// Arm `slot`, handing back the callback a real timer would have owned.
    fn arm(
        slot: &mut Option<TimerWakerSlot>,
        deadline: Duration,
        probe: &Arc<Probe>,
    ) -> TimerCallback {
        let mut armed = None;
        ensure_timer_waker_with(slot, deadline, &waker_of(probe), |asked, callback| {
            assert_eq!(asked, deadline, "armed for an instant nobody asked for");
            armed = Some(callback);
        });
        armed.expect("no timer was armed")
    }

    /// Re-poll `slot` for `deadline`, asserting that no second timer is armed.
    fn refresh(slot: &mut Option<TimerWakerSlot>, deadline: Duration, probe: &Arc<Probe>) {
        ensure_timer_waker_with(slot, deadline, &waker_of(probe), |_, _| {
            panic!("a live slot for the same deadline must not arm a second timer")
        });
    }

    #[test]
    fn arming_records_the_deadline_and_parks_the_waker() {
        let probe = Probe::new();
        let mut slot = None;
        let _callback = arm(&mut slot, DL, &probe);
        let slot = slot.expect("the slot was left empty");
        assert_eq!(slot.deadline(), DL);
        assert!(!slot.is_done());
        assert_eq!(
            probe.clones(),
            1,
            "the waker is parked, so it is cloned once"
        );
        assert_eq!(probe.wakes(), 0, "nothing has fired yet");
    }

    #[test]
    fn the_timer_firing_wakes_the_waker_and_marks_the_slot_done() {
        let probe = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &probe);
        callback(TICK);
        assert_eq!(probe.wakes(), 1);
        assert!(slot.as_ref().unwrap().is_done());
    }

    #[test]
    fn the_same_deadline_refreshes_in_place_instead_of_arming_a_second_timer() {
        let probe = Probe::new();
        let mut slot = None;
        let _callback = arm(&mut slot, DL, &probe);
        // `refresh` panics if a second timer is armed.
        refresh(&mut slot, DL, &probe);
        assert_eq!(slot.as_ref().unwrap().deadline(), DL);
    }

    #[test]
    fn a_refresh_replaces_the_waker_the_timer_will_wake() {
        let armed_by = Probe::new();
        let polled_by = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &armed_by);
        refresh(&mut slot, DL, &polled_by);
        callback(TICK);
        assert_eq!(polled_by.wakes(), 1, "the task that is actually waiting");
        assert_eq!(armed_by.wakes(), 0, "the one that armed it has moved on");
    }

    /// The reason this module's refresh re-reads `done`.
    ///
    /// The timer fires on another CPU in the instant between the `is_done`
    /// check and the store: it takes the *old* waker and wakes it, and the one
    /// being stored lands in a cell whose timer has already come and gone.
    /// Without the re-read nothing ever wakes it again, which is a `poll`,
    /// `read` or `semop` with a timeout that never returns.
    #[test]
    fn a_timer_that_fires_while_its_waker_is_refreshed_still_wakes_it() {
        let armed_by = Probe::new();
        let polled_by = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &armed_by);

        polled_by.interrupt_next_clone(Box::new(move || callback(TICK)));
        refresh(&mut slot, DL, &polled_by);

        assert_eq!(armed_by.wakes(), 1, "the timer woke whoever it found");
        assert_eq!(polled_by.wakes(), 1, "and the waiter is not left asleep");
        assert!(slot.as_ref().unwrap().is_done());
    }

    #[test]
    fn a_slot_whose_timer_already_fired_is_armed_afresh() {
        let probe = Probe::new();
        let mut slot = None;
        let first = arm(&mut slot, DL, &probe);
        first(TICK);
        let second = arm(&mut slot, DL, &probe);
        assert!(!slot.as_ref().unwrap().is_done(), "the new cell is live");
        second(TICK);
        assert_eq!(probe.wakes(), 2, "once per timer");
    }

    #[test]
    fn a_different_deadline_arms_a_new_timer_and_silences_the_old_one() {
        let probe = Probe::new();
        let mut slot = None;
        let stale = arm(&mut slot, DL, &probe);
        let fresh = arm(&mut slot, LATER, &probe);
        assert_eq!(slot.as_ref().unwrap().deadline(), LATER);

        stale(TICK);
        assert_eq!(probe.wakes(), 0, "the timer for the old instant is a no-op");
        fresh(TICK);
        assert_eq!(probe.wakes(), 1);
    }

    #[test]
    fn a_deadline_one_nanosecond_apart_is_a_different_deadline() {
        let probe = Probe::new();
        let mut slot = None;
        let _stale = arm(&mut slot, DL, &probe);
        let _fresh = arm(&mut slot, DL + Duration::from_nanos(1), &probe);
        assert_eq!(
            slot.as_ref().unwrap().deadline(),
            DL + Duration::from_nanos(1)
        );
    }

    #[test]
    fn cancelling_drops_the_waker_so_a_late_tick_wakes_nobody() {
        let probe = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &probe);
        kill_timer_waker(&mut slot);
        assert!(slot.is_none(), "the owner is left holding nothing");
        callback(TICK);
        assert_eq!(probe.wakes(), 0);
    }

    /// Cancelling has to *drop* the waker, not just mark the cell dead: the
    /// waker is what keeps the task alive, and the armed callback holds the
    /// cell until its deadline comes round however dead it is.
    #[test]
    fn cancelling_releases_the_task_the_waker_pointed_at() {
        let probe = Probe::new();
        let mut slot = None;
        let _callback = arm(&mut slot, DL, &probe);
        assert_eq!(Arc::strong_count(&probe), 2, "the parked waker holds one");
        kill_timer_waker(&mut slot);
        assert_eq!(
            Arc::strong_count(&probe),
            1,
            "nothing points at the task now"
        );
    }

    #[test]
    fn cancelling_an_empty_slot_does_nothing() {
        let mut slot = None;
        kill_timer_waker(&mut slot);
        assert!(slot.is_none());
    }

    #[test]
    fn cancelling_twice_does_nothing_the_second_time() {
        let probe = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &probe);
        kill_timer_waker(&mut slot);
        kill_timer_waker(&mut slot);
        callback(TICK);
        assert_eq!(probe.wakes(), 0);
    }

    /// Cancelling is not the owner's to forget: ten futures across four crates
    /// park a slot in a field, and one that drops without cancelling leaves a
    /// timer pointing at freed task memory.
    #[test]
    fn dropping_the_slot_cancels_the_timer() {
        let probe = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &probe);
        drop(slot);
        callback(TICK);
        assert_eq!(probe.wakes(), 0);
    }

    #[test]
    fn a_cancelled_slot_is_armed_again_by_the_next_poll() {
        let probe = Probe::new();
        let mut slot = None;
        let stale = arm(&mut slot, DL, &probe);
        kill_timer_waker(&mut slot);
        let fresh = arm(&mut slot, DL, &probe);
        stale(TICK);
        assert_eq!(probe.wakes(), 0);
        fresh(TICK);
        assert_eq!(probe.wakes(), 1);
    }

    #[test]
    fn a_cancel_that_arrives_after_the_timer_fired_wakes_nobody_twice() {
        let probe = Probe::new();
        let mut slot = None;
        let callback = arm(&mut slot, DL, &probe);
        callback(TICK);
        kill_timer_waker(&mut slot);
        assert_eq!(
            probe.wakes(),
            1,
            "the fire counts once, the cancel not at all"
        );
    }
}
