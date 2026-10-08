//! The waiter behind a blocking `read()` on the card fd.
//!
//! A compositor's frame loop lives here: it page-flips, then blocks reading
//! the card fd until the completion arrives. The wait is an event-bus
//! subscription, and the bus drops a callback that returns `true` as soon
//! as it fires. So a waiter that woke, found nothing to read, and parked
//! again had to make sure it still HAD a callback -- nothing in this file
//! re-polls the fd on a tick, so a park with no callback is a frame loop
//! that stops for good.
//!
//! Waking with nothing to read is ordinary, not exotic: another reader of
//! the same `drm_file` (a `dup`ed fd, a second compositor thread) can drain
//! the queue in between, and the bus flag is latched independently of the
//! queue.

use super::*;
use crate::sync::Event;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, RawWaker, RawWakerVTable, Waker};

/// A waker that counts how many times the bus reached it.
fn counting_waker(hits: &AtomicUsize) -> Waker {
    fn raw(ptr: *const ()) -> RawWaker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            raw(ptr)
        }
        unsafe fn wake(ptr: *const ()) {
            wake_by_ref(ptr)
        }
        unsafe fn wake_by_ref(ptr: *const ()) {
            // SAFETY: `ptr` is the `&AtomicUsize` this waker was built
            // from, which outlives the waker (it is a local of the test).
            unsafe { &*(ptr as *const AtomicUsize) }.fetch_add(1, Ordering::Relaxed);
        }
        unsafe fn drop(_: *const ()) {}
        RawWaker::new(ptr, &RawWakerVTable::new(clone, wake, wake_by_ref, drop))
    }
    // SAFETY: the vtable above only ever reads `ptr` as the `&AtomicUsize`.
    unsafe { Waker::from_raw(raw(hits as *const AtomicUsize as *const ())) }
}

/// The bug this pins down: one wake that produced nothing to read used to
/// leave the reader parked with no callback on the bus.
#[test]
fn a_wake_that_finds_nothing_to_read_leaves_the_reader_still_subscribed() {
    let hits = AtomicUsize::new(0);
    let waker = counting_waker(&hits);
    let mut cx = Context::from_waker(&waker);

    let dev = DrmDev::new(0);
    let bus = dev.file.eventbus();
    let mut fut = core::pin::pin!(DrmEventWait {
        dev: &dev,
        bus: bus.clone(),
        sub_id: None,
    });

    // Nothing queued, so the first poll parks with a callback on the bus.
    assert!(matches!(fut.as_mut().poll(&mut cx), TaskPoll::Pending));
    assert_eq!(
        bus.lock().get_callback_len(),
        1,
        "parked without a callback"
    );

    // A readable edge that leaves the queue empty: somebody else read it.
    bus.lock().change(Event::empty(), Event::READABLE);
    assert_eq!(hits.load(Ordering::Relaxed), 1, "the edge has to wake it");

    // Woken, still nothing to read, parks again -- and MUST still be
    // subscribed. This is the assertion the one-shot callback failed.
    assert!(matches!(fut.as_mut().poll(&mut cx), TaskPoll::Pending));
    assert_eq!(
        bus.lock().get_callback_len(),
        1,
        "parked with no callback on the bus: nothing will ever wake this \
         reader again, and a compositor blocked in read() on the card fd is \
         a frame loop that has stopped"
    );

    // And the proof it is a live callback and not merely a present one: the
    // next edge reaches the same waiter.
    bus.lock().change(Event::READABLE, Event::empty());
    bus.lock().change(Event::empty(), Event::READABLE);
    assert_eq!(
        hits.load(Ordering::Relaxed),
        2,
        "the second edge never reached the reader"
    );
}

/// The other half, so the fix cannot be "never unsubscribe": dropping the
/// future still takes the callback off the bus. A card fd is opened and
/// closed on every compositor restart, and the bus evicts its oldest entry
/// when the table fills, so a leak here would silently cost somebody else
/// their wakeup.
#[test]
fn dropping_the_waiter_takes_its_callback_off_the_bus() {
    let hits = AtomicUsize::new(0);
    let waker = counting_waker(&hits);
    let mut cx = Context::from_waker(&waker);

    let dev = DrmDev::new(0);
    let bus = dev.file.eventbus();
    {
        let mut fut = core::pin::pin!(DrmEventWait {
            dev: &dev,
            bus: bus.clone(),
            sub_id: None,
        });
        assert!(matches!(fut.as_mut().poll(&mut cx), TaskPoll::Pending));
        assert_eq!(bus.lock().get_callback_len(), 1);
    }
    assert_eq!(
        bus.lock().get_callback_len(),
        0,
        "the callback outlived the waiter"
    );
}
