//! The waker list an input frame fires.
//!
//! A `poll(2)` / `select(2)` / `epoll_wait(2)` parked on `/dev/input/event*`
//! (or on `/dev/input/mice`, or on a nested epoll holding either -- which is
//! the shape of a Wayland compositor's event loop, since `libinput_get_fd`
//! hands out libinput's own epoll fd) had no way to be told that a frame had
//! arrived. The evdev node carries no `EventBus`, so
//! [`crate::fs::FileLike::subscribe_readiness`] answers `None` for it, and the
//! wait fell back to its periodic re-scan: the keyboard and the mouse were the
//! only devices in the system whose interrupt did not wake the task waiting
//! for them.
//!
//! That is what libinput reports as
//!
//! ```text
//! client bug: event processing lagging behind by 54ms, your system is too slow
//! ```
//!
//! -- it stamps each frame with `CLOCK_MONOTONIC` as the kernel queues it and
//! compares that against the clock when the compositor finally reads it, so
//! everything between the interrupt and the read shows up there. A re-scan
//! tick is in that gap, and the task still has to be scheduled afterwards.
//!
//! This is the TTY-interrupt list (`crate::fs::stdio::wake_tty_intr_waiters`)
//! applied to input devices, deliberately kept as a list of its own rather
//! than folded into that one: `watch_interactive` is true for *every*
//! non-socket fd, so firing the TTY list on a 1 kHz mouse would wake every
//! shell parked on a spare VT a thousand times a second -- the exact busy-loop
//! the background-VT slow tick exists to prevent. Only a wait whose set holds
//! an input device registers here.

use alloc::vec::Vec;
use kernel_hal::sync::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    static ref INPUT_WAKERS: Mutex<Vec<core::task::Waker>> = Mutex::new(Vec::new());
}

/// Ceiling on parked waiters, as `MAX_TTY_INTR_WAKERS` is for the TTY list:
/// a registration that is never cleared (a task killed between `register` and
/// `clear`) must not grow this without bound.
const MAX_INPUT_WAKERS: usize = 64;

fn register_once(wakers: &mut Vec<core::task::Waker>, waker: &core::task::Waker) {
    if wakers.iter().any(|w| w.will_wake(waker)) {
        return;
    }
    if wakers.len() >= MAX_INPUT_WAKERS {
        wakers.remove(0);
    }
    wakers.push(waker.clone());
}

/// Wake everything parked on an input device. Called when a device finishes a
/// frame and has a packet a reader can take -- never for a partial frame,
/// which a reader could not take anyway.
///
/// Takes the whole list, exactly as `wake_tty_intr_waiters` does: a woken
/// wait re-registers on its next poll, which is what keeps a 1 kHz mouse from
/// costing one wake per report per waiter.
pub fn wake_input_waiters() {
    let wakers: Vec<core::task::Waker> = core::mem::take(&mut *INPUT_WAKERS.lock());
    for w in wakers {
        w.wake();
    }
}

/// Park `waker` until the next input frame.
pub fn register_input_waker(waker: core::task::Waker) {
    register_once(&mut INPUT_WAKERS.lock(), &waker);
}

/// Keep THIS waker registered for another sleep cycle without disturbing the
/// other waiters. See `retain_tty_intr_waker`: the inverted predicate there
/// made one waiter's re-arm delete everybody else's registration.
pub fn retain_input_waker(waker: &core::task::Waker) {
    register_once(&mut INPUT_WAKERS.lock(), waker);
}

/// Drop a wait's registration when it completes or is dropped, so a later
/// frame cannot fire into a recycled task slot.
pub fn clear_input_waker(waker: &core::task::Waker) {
    INPUT_WAKERS.lock().retain(|w| !w.will_wake(waker));
}

#[cfg(test)]
mod tests {
    //! The list is the whole of the mechanism: a frame fires it, a wait
    //! registers on it, and a wait that is gone must not be in it. Each of
    //! those three has its own way of going wrong, and none of them needs a
    //! device.

    use super::*;
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::Waker;

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counter>, Waker) {
        let c = Arc::new(Counter(AtomicUsize::new(0)));
        let w = Waker::from(c.clone());
        (c, w)
    }

    /// The list is one global, and the host test binary runs these in
    /// parallel: without a lock of their own two tests share the list and
    /// each wakes the other's waker. Taking it and draining the list is what
    /// gives each test the empty list it asserts about.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn drained() -> kernel_hal::sync::MutexGuard<'static, ()> {
        let g = TEST_LOCK.lock();
        INPUT_WAKERS.lock().clear();
        g
    }

    #[test]
    fn a_frame_wakes_a_registered_wait() {
        let _serial = drained();
        let (c, w) = counting();
        register_input_waker(w);
        wake_input_waiters();
        assert_eq!(c.0.load(Ordering::SeqCst), 1);
    }

    /// Taking the whole list is what bounds the cost of a 1 kHz mouse: one
    /// wake per waiter per sleep cycle, not one per report. A second frame
    /// before the woken task re-registers must find nothing.
    #[test]
    fn a_wake_consumes_the_registration() {
        let _serial = drained();
        let (c, w) = counting();
        register_input_waker(w);
        wake_input_waiters();
        wake_input_waiters();
        assert_eq!(c.0.load(Ordering::SeqCst), 1);
    }

    /// `retain` is the re-arm of a wait that is going round again. It had to
    /// be written as "add me", not as "keep only me": the TTY list's version
    /// of this had the predicate inverted, and one waiter's re-arm deleted
    /// every other waiter's registration.
    #[test]
    fn retaining_one_waker_leaves_the_others_registered() {
        let _serial = drained();
        let (a, wa) = counting();
        let (b, wb) = counting();
        register_input_waker(wa.clone());
        register_input_waker(wb);
        retain_input_waker(&wa);
        wake_input_waiters();
        assert_eq!(a.0.load(Ordering::SeqCst), 1, "the re-armed waiter");
        assert_eq!(b.0.load(Ordering::SeqCst), 1, "the untouched waiter");
    }

    /// Registering the same waker twice is one registration, so a wait that
    /// goes round the loop many times does not fill the list with copies of
    /// itself and evict everybody else.
    #[test]
    fn registering_the_same_waker_twice_stores_it_once() {
        let _serial = drained();
        let (c, w) = counting();
        register_input_waker(w.clone());
        register_input_waker(w);
        assert_eq!(INPUT_WAKERS.lock().len(), 1);
        wake_input_waiters();
        assert_eq!(c.0.load(Ordering::SeqCst), 1);
    }

    /// A wait that completed or was dropped must leave nothing behind: a
    /// stale waker is what fires into a recycled task slot.
    #[test]
    fn clearing_a_waker_takes_it_out_of_the_list() {
        let _serial = drained();
        let (c, w) = counting();
        register_input_waker(w.clone());
        clear_input_waker(&w);
        wake_input_waiters();
        assert_eq!(c.0.load(Ordering::SeqCst), 0);
    }

    /// Clearing a waker that was never registered is a no-op, which is what
    /// lets the clear path pass `watch_hid` unconditionally.
    #[test]
    fn clearing_an_unregistered_waker_does_nothing() {
        let _serial = drained();
        let (_a, wa) = counting();
        let (b, wb) = counting();
        register_input_waker(wb);
        clear_input_waker(&wa);
        wake_input_waiters();
        assert_eq!(b.0.load(Ordering::SeqCst), 1);
    }

    /// The ceiling is a leak guard, not a policy: past it the oldest
    /// registration goes, and the list never grows without bound.
    #[test]
    fn the_list_is_capped() {
        let _serial = drained();
        let mut kept = alloc::vec::Vec::new();
        for _ in 0..(MAX_INPUT_WAKERS + 8) {
            let (c, w) = counting();
            kept.push(c);
            register_input_waker(w);
        }
        assert_eq!(INPUT_WAKERS.lock().len(), MAX_INPUT_WAKERS);
    }
}
