//! The readiness waker a VT terminal parks, which is what lets a
//! `poll(stdin)` stop re-scanning at 250 Hz (see
//! [`super::Stdin::subscribe_readiness`]).

use super::*;
use crate::fs::PollEvents;
use core::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use core::task::{RawWaker, RawWakerVTable, Waker};

fn flag_waker(flag: &'static AtomicBool) -> Waker {
    fn raw(ptr: *const ()) -> RawWaker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            raw(ptr)
        }
        unsafe fn wake(ptr: *const ()) {
            (*(ptr as *const AtomicBool)).store(true, AtomicOrdering::SeqCst);
        }
        unsafe fn wake_by_ref(ptr: *const ()) {
            (*(ptr as *const AtomicBool)).store(true, AtomicOrdering::SeqCst);
        }
        unsafe fn drop(_: *const ()) {}
        RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
    }
    unsafe { Waker::from_raw(raw(flag as *const AtomicBool as *const ())) }
}

/// The whole point: a terminal answers with a subscription at all. While it
/// did not, every `poll(stdin)` in the system kept the 4 ms backstop and
/// polled the xHCI 250 times a second for nothing.
#[test]
fn a_terminal_parks_a_waker_instead_of_asking_to_be_re_scanned() {
    static WOKE: AtomicBool = AtomicBool::new(false);
    WOKE.store(false, AtomicOrdering::SeqCst);
    let stdin = vt_stdin(0);
    stdin.eventbus.lock().clear(Event::READABLE);
    let sub = stdin.subscribe_readiness(PollEvents::IN, &flag_waker(&WOKE));
    assert!(
        !WOKE.load(AtomicOrdering::SeqCst),
        "nothing was readable, so the waker must have been stored, not fired"
    );
    stdin.eventbus.lock().set(Event::READABLE);
    assert!(
        WOKE.load(AtomicOrdering::SeqCst),
        "a terminal becoming readable must fire the parked waker"
    );
    drop(sub);
    stdin.eventbus.lock().clear(Event::READABLE);
}

/// `flush_ready_flag` before subscribing, and this is why: `push` leaves a
/// byte in `data_ready` without touching the bus when the bus lock is
/// contended. Subscribing on a bus that does not know about it yet stores the
/// waker instead of firing it, and the reader sleeps out the whole backstop on
/// data it already had.
#[test]
fn a_byte_latched_outside_the_bus_fires_the_waker_at_subscribe_time() {
    static WOKE: AtomicBool = AtomicBool::new(false);
    WOKE.store(false, AtomicOrdering::SeqCst);
    let stdin = vt_stdin(1);
    stdin.eventbus.lock().clear(Event::READABLE);
    // Exactly what the contended branch of `push`/`wake_readers` leaves.
    stdin.data_ready.store(true, AtomicOrdering::Release);
    let _sub = stdin.subscribe_readiness(PollEvents::IN, &flag_waker(&WOKE));
    assert!(
        WOKE.load(AtomicOrdering::SeqCst),
        "the latched byte was already there: the waker had to fire here"
    );
    stdin.eventbus.lock().clear(Event::READABLE);
}

/// Dropping the subscription takes the callback off the bus. Without this the
/// handle would have to hold the bus, and the bus is a field of a `Stdin` and
/// not an `Arc` -- the reason the handle goes back through `vt_stdin` instead.
#[test]
fn dropping_the_subscription_leaves_nothing_on_the_bus() {
    static WOKE: AtomicBool = AtomicBool::new(false);
    WOKE.store(false, AtomicOrdering::SeqCst);
    let stdin = vt_stdin(2);
    stdin.eventbus.lock().clear(Event::READABLE);
    drop(stdin.subscribe_readiness(PollEvents::IN, &flag_waker(&WOKE)));
    stdin.eventbus.lock().set(Event::READABLE);
    assert!(
        !WOKE.load(AtomicOrdering::SeqCst),
        "the waker was unsubscribed, so a later event must not reach it"
    );
    stdin.eventbus.lock().clear(Event::READABLE);
}
