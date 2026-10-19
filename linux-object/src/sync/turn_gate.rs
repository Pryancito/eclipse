//! One task at a time through a section, and the rest asleep while they wait.
//!
//! The kernel's `Mutex` is a ticket spinlock that turns interrupts off before
//! it takes a ticket, so a CPU waiting for it is a CPU that runs nothing else
//! and hears no timer until its turn comes. That is fine for the short
//! sections it was made for. It is not fine in front of a disk: the
//! filesystems mounted from one serve each read under a single lock held
//! across the polled device command, and with as many readers as CPUs every
//! CPU ends up either polling the disk or spinning for the lock, all with
//! interrupts off. A [`TurnGate`] is the queue in front of such a section,
//! where a waiter is a parked task and its CPU stays free for others.
//!
//! A section that several tasks may share (EclipseFS reads, which hold its
//! lock shared) asks for its turn with [`TurnGate::turn_of`] and lets that
//! many through at once, still parking the rest.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use kernel_hal::sync::Mutex;

/// Lets one task at a time through (or a few, see [`TurnGate::turn_of`]);
/// the others wait parked.
pub struct TurnGate {
    state: Mutex<GateState>,
}

struct GateState {
    /// How many [`TurnGuard`]s are alive.
    held: usize,
    /// Who asked while it was taken. All of them are woken when it is handed
    /// back and all but one find it taken again and ask again. Waking every
    /// waiter costs a few polls and keeps a turn from being lost when the
    /// task a single wake-up went to is dropped instead of polled.
    waiting: Vec<Waker>,
}

impl TurnGate {
    /// A gate nobody is through.
    pub const fn new() -> Self {
        TurnGate {
            state: Mutex::new(GateState {
                held: 0,
                waiting: Vec::new(),
            }),
        }
    }

    /// Wait for this task's turn. The turn lasts as long as the guard.
    pub fn turn(&self) -> Turn<'_> {
        self.turn_of(1)
    }

    /// Wait for a turn while fewer than `slots` (at least one) are held.
    /// Everyone who asks a gate should give it the same `slots`.
    pub fn turn_of(&self, slots: usize) -> Turn<'_> {
        Turn {
            gate: self,
            slots: slots.max(1),
        }
    }
}

impl Default for TurnGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The wait for a turn at a [`TurnGate`].
#[must_use = "future does nothing unless polled/`await`-ed"]
pub struct Turn<'a> {
    gate: &'a TurnGate,
    slots: usize,
}

impl<'a> Future for Turn<'a> {
    type Output = TurnGuard<'a>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<TurnGuard<'a>> {
        let gate = self.gate;
        let mut state = gate.state.lock();
        if state.held < self.slots {
            state.held += 1;
            return Poll::Ready(TurnGuard { gate });
        }
        if !state.waiting.iter().any(|w| w.will_wake(cx.waker())) {
            state.waiting.push(cx.waker().clone());
        }
        Poll::Pending
    }
}

/// A turn at a [`TurnGate`]; dropping it hands the gate back.
pub struct TurnGuard<'a> {
    gate: &'a TurnGate,
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        let waiting = {
            let mut state = self.gate.state.lock();
            state.held -= 1;
            core::mem::take(&mut state.waiting)
        };
        // Woken with the gate's lock released: a wake-up can take the
        // scheduler's.
        for waker in waiting {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// A waker that counts its wake-ups.
    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counter() -> (Arc<Counter>, Waker) {
        let c = Arc::new(Counter(AtomicUsize::new(0)));
        (c.clone(), Waker::from(c))
    }

    fn poll<'a>(turn: &mut Turn<'a>, waker: &Waker) -> Poll<TurnGuard<'a>> {
        Pin::new(turn).poll(&mut Context::from_waker(waker))
    }

    fn woken(c: &Counter) -> usize {
        c.0.load(Ordering::SeqCst)
    }

    #[test]
    fn a_free_gate_lets_the_first_one_through() {
        let gate = TurnGate::new();
        let (_, w) = counter();
        assert!(poll(&mut gate.turn(), &w).is_ready());
    }

    #[test]
    fn the_second_one_waits_until_the_first_is_done_and_is_woken_then() {
        let gate = TurnGate::new();
        let (_, wa) = counter();
        let (cb, wb) = counter();
        let first = poll(&mut gate.turn(), &wa);
        assert!(first.is_ready());

        let mut second = gate.turn();
        assert!(poll(&mut second, &wb).is_pending());
        assert_eq!(woken(&cb), 0, "nothing woke it while the turn was held");

        drop(first);
        assert_eq!(woken(&cb), 1, "handing the gate back wakes the waiter");
        assert!(poll(&mut second, &wb).is_ready());
    }

    /// Every waiter wakes; one gets through and the others wait again and
    /// are woken by the next hand-back.
    #[test]
    fn every_waiter_is_woken_and_the_ones_left_out_wait_again() {
        let gate = TurnGate::new();
        let (_, wa) = counter();
        let held = poll(&mut gate.turn(), &wa);

        let (cb, wb) = counter();
        let (cc, wc) = counter();
        let mut b = gate.turn();
        let mut c = gate.turn();
        assert!(poll(&mut b, &wb).is_pending());
        assert!(poll(&mut c, &wc).is_pending());

        drop(held);
        assert_eq!((woken(&cb), woken(&cc)), (1, 1));

        let through = poll(&mut b, &wb);
        assert!(through.is_ready());
        assert!(poll(&mut c, &wc).is_pending(), "one turn at a time");

        drop(through);
        assert_eq!(woken(&cc), 2);
        assert!(poll(&mut c, &wc).is_ready());
    }

    /// A gate of three slots lets three through and parks the fourth until
    /// one of them is done.
    #[test]
    fn a_gate_with_slots_lets_that_many_through() {
        let gate = TurnGate::new();
        let (_, w) = counter();
        let held: Vec<_> = (0..3)
            .map(|_| match poll(&mut gate.turn_of(3), &w) {
                Poll::Ready(g) => g,
                Poll::Pending => panic!("a free slot was refused"),
            })
            .collect();
        let (cd, wd) = counter();
        let mut fourth = gate.turn_of(3);
        assert!(poll(&mut fourth, &wd).is_pending());
        let mut held = held;
        held.pop();
        assert_eq!(woken(&cd), 1);
        assert!(poll(&mut fourth, &wd).is_ready());
    }

    #[test]
    fn zero_slots_means_one() {
        let gate = TurnGate::new();
        let (_, w) = counter();
        let first = poll(&mut gate.turn_of(0), &w);
        assert!(first.is_ready());
        assert!(poll(&mut gate.turn_of(0), &w).is_pending());
    }

    /// A waiter that goes away without taking its turn -- its task was
    /// dropped -- must not take the wake-up the next one needed with it.
    #[test]
    fn a_waiter_that_goes_away_does_not_strand_the_next_one() {
        let gate = TurnGate::new();
        let (_, wa) = counter();
        let held = poll(&mut gate.turn(), &wa);

        let (_, wb) = counter();
        let (cc, wc) = counter();
        let mut b = gate.turn();
        let mut c = gate.turn();
        assert!(poll(&mut b, &wb).is_pending());
        assert!(poll(&mut c, &wc).is_pending());

        drop(b);
        drop(held);
        assert_eq!(woken(&cc), 1);
        assert!(poll(&mut c, &wc).is_ready());
    }

    /// Polling again while waiting does not queue the same task twice.
    #[test]
    fn a_waiter_polled_twice_is_queued_once() {
        let gate = TurnGate::new();
        let (_, wa) = counter();
        let held = poll(&mut gate.turn(), &wa);

        let (cb, wb) = counter();
        let mut b = gate.turn();
        assert!(poll(&mut b, &wb).is_pending());
        assert!(poll(&mut b, &wb).is_pending());

        drop(held);
        assert_eq!(woken(&cb), 1);
    }
}
