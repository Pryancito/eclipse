//! Futures for I/O waits (NIC, HID, TTY).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;

use kernel_hal::timer_waker::{self, TimerWakerSlot};

use super::{
    clear_io_wait_wakers, clear_io_wait_wakers_hid, register_io_wait_wakers,
    register_io_wait_wakers_hid, retain_io_wait_wakers, retain_io_wait_wakers_hid,
};

/// Fallback timer when no IRQ wakes a multiplex wait (poll/epoll/select).
pub const IO_WAIT_TICK_MS: u64 = 4;

/// Fallback timer when EVERY watched fd carries a parked readiness waker
/// (`FileLike::subscribe_readiness` returned `Some` for all of them).
///
/// With full coverage the wakeup path is the event itself — a pipe write, a
/// unix-socket send, a timerfd expiry — delivered through the fd's EventBus
/// the instant it happens, so the timer is pure insurance against a missed
/// wake in the subscription wiring. 100 ms turns the old 250 re-scans per
/// second per parked process into 10, while bounding any wiring bug to
/// 100 ms of added latency instead of a hang.
pub const IO_WAIT_COVERED_TICK_MS: u64 = 100;

/// Resolves when Ctrl+C is pending, NET RX wakers fire, or deadline.
pub struct NetOrTtyWait {
    deadline: Duration,
    armed: bool,
    timer: Option<TimerWakerSlot>,
    watch_net: bool,
    watch_interactive: bool,
    io_waker: Option<core::task::Waker>,
}

impl NetOrTtyWait {
    pub fn new_after_ms(ms: u64) -> Self {
        Self {
            deadline: kernel_hal::timer::timer_now() + Duration::from_millis(ms),
            armed: false,
            timer: None,
            watch_net: true,
            watch_interactive: true,
            io_waker: None,
        }
    }
}

impl Drop for NetOrTtyWait {
    fn drop(&mut self) {
        timer_waker::kill_timer_waker(&mut self.timer);
        if let Some(w) = self.io_waker.take() {
            clear_io_wait_wakers(&w, self.watch_net, self.watch_interactive);
        }
    }
}

impl Future for NetOrTtyWait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if crate::fs::stdio::ctrl_c_pending_peek() {
            timer_waker::kill_timer_waker(&mut self.timer);
            if let Some(w) = self.io_waker.take() {
                clear_io_wait_wakers(&w, self.watch_net, self.watch_interactive);
            }
            return Poll::Ready(());
        }
        if kernel_hal::timer::timer_now() >= self.deadline {
            timer_waker::kill_timer_waker(&mut self.timer);
            if let Some(w) = self.io_waker.take() {
                clear_io_wait_wakers(&w, self.watch_net, self.watch_interactive);
            } else {
                clear_io_wait_wakers(cx.waker(), self.watch_net, self.watch_interactive);
            }
            return Poll::Ready(());
        }
        if self.armed {
            retain_io_wait_wakers(cx.waker(), self.watch_net, self.watch_interactive);
            timer_waker::kill_timer_waker(&mut self.timer);
            clear_io_wait_wakers(cx.waker(), self.watch_net, self.watch_interactive);
            self.io_waker = None;
            return Poll::Ready(());
        }
        register_io_wait_wakers(cx.waker(), self.watch_net, self.watch_interactive);
        self.io_waker = Some(cx.waker().clone());
        let dl = self.deadline;
        timer_waker::ensure_timer_waker(&mut self.timer, dl, cx);
        self.armed = true;
        Poll::Pending
    }
}

/// One sleep cycle in epoll/poll: wake on NET/TTY IRQ or fallback timer.
pub struct IoMultiplexWait {
    deadline: Option<Duration>,
    watch_net: bool,
    watch_interactive: bool,
    /// Whether this wait's set holds an input device, so an input frame must
    /// wake it. See `crate::fs::devfs::input::wait`.
    watch_hid: bool,
    /// `crate::fs::devfs::input::wait::input_seq` as it stood BEFORE the
    /// caller's readiness scan. A frame landing between that scan and the
    /// registration below would otherwise be lost -- the wake drains a list
    /// this wait is not in yet -- and the task would sleep out the fallback
    /// tick. Compared after registering; a move means re-scan now.
    hid_seq: u64,
    armed: bool,
    timer: Option<TimerWakerSlot>,
    /// Fallback re-scan interval: [`IO_WAIT_TICK_MS`] normally,
    /// [`IO_WAIT_COVERED_TICK_MS`] when the caller parked a readiness waker
    /// on every watched fd.
    tick_ms: u64,
    /// Waker parked in NET_RX/TTY lists — needed so Drop can unregister without
    /// a `Context` (Ready already clears via `cx.waker()`).
    io_waker: Option<core::task::Waker>,
}

impl IoMultiplexWait {
    pub fn new(timeout_msecs: isize, watch_net: bool, watch_interactive: bool) -> Self {
        Self::with_tick(timeout_msecs, watch_net, watch_interactive, IO_WAIT_TICK_MS)
    }

    /// [`new`](Self::new) with an explicit fallback interval — pass
    /// [`IO_WAIT_COVERED_TICK_MS`] when every watched fd has a parked
    /// readiness subscription doing the real waking.
    pub fn with_tick(
        timeout_msecs: isize,
        watch_net: bool,
        watch_interactive: bool,
        tick_ms: u64,
    ) -> Self {
        Self::with_tick_hid(
            timeout_msecs,
            watch_net,
            watch_interactive,
            false,
            0,
            tick_ms,
        )
    }

    /// [`with_tick`](Self::with_tick) plus the input-device waker list, for a
    /// wait whose set holds an evdev node (directly, or through a nested
    /// epoll: see [`crate::fs::FileLike::is_input_device`]). That
    /// registration is what makes an input interrupt wake this task instead
    /// of leaving it to the fallback tick.
    pub fn with_tick_hid(
        timeout_msecs: isize,
        watch_net: bool,
        watch_interactive: bool,
        watch_hid: bool,
        hid_seq: u64,
        tick_ms: u64,
    ) -> Self {
        let deadline = if timeout_msecs >= 0 {
            Some(kernel_hal::timer::timer_now() + Duration::from_millis(timeout_msecs as u64))
        } else {
            None
        };
        Self {
            deadline,
            watch_net,
            watch_interactive,
            watch_hid,
            hid_seq,
            armed: false,
            timer: None,
            tick_ms,
            io_waker: None,
        }
    }
}

impl Drop for IoMultiplexWait {
    fn drop(&mut self) {
        timer_waker::kill_timer_waker(&mut self.timer);
        if let Some(w) = self.io_waker.take() {
            clear_io_wait_wakers_hid(&w, self.watch_net, self.watch_interactive, self.watch_hid);
        }
    }
}

impl Future for IoMultiplexWait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(dl) = self.deadline {
            if kernel_hal::timer::timer_now() >= dl {
                timer_waker::kill_timer_waker(&mut self.timer);
                clear_io_wait_wakers_hid(
                    cx.waker(),
                    self.watch_net,
                    self.watch_interactive,
                    self.watch_hid,
                );
                self.io_waker = None;
                return Poll::Ready(());
            }
        }
        if self.armed {
            retain_io_wait_wakers_hid(
                cx.waker(),
                self.watch_net,
                self.watch_interactive,
                self.watch_hid,
            );
            timer_waker::kill_timer_waker(&mut self.timer);
            // Drop the IRQ registration: the epoll loop boxes a fresh wait each
            // cycle. Leaving the waker in NET_RX/TTY lists after Ready meant a
            // later IRQ could wake a completed wait's task slot under churn
            // (observed as delayed KERNEL PAGE FAULT / null fn-ptr after a
            // long labwc session).
            clear_io_wait_wakers_hid(
                cx.waker(),
                self.watch_net,
                self.watch_interactive,
                self.watch_hid,
            );
            self.io_waker = None;
            return Poll::Ready(());
        }
        register_io_wait_wakers_hid(
            cx.waker(),
            self.watch_net,
            self.watch_interactive,
            self.watch_hid,
        );
        // Registered: from here a frame wakes this task. The one that could
        // still be lost is the frame between the caller's readiness scan and
        // the line above -- its wake drained a list this task was not in yet.
        // `hid_seq` was read before that scan, so a move means exactly that
        // happened: go round again rather than sleep out the fallback tick.
        if self.watch_hid && crate::fs::devfs::input::wait::input_seq() != self.hid_seq {
            clear_io_wait_wakers_hid(
                cx.waker(),
                self.watch_net,
                self.watch_interactive,
                self.watch_hid,
            );
            self.io_waker = None;
            return Poll::Ready(());
        }
        self.io_waker = Some(cx.waker().clone());
        let tick = Duration::from_millis(self.tick_ms);
        let wake_at = if let Some(dl) = self.deadline {
            let now = kernel_hal::timer::timer_now();
            if now + tick < dl {
                now + tick
            } else {
                dl
            }
        } else {
            kernel_hal::timer::timer_now() + tick
        };
        timer_waker::ensure_timer_waker(&mut self.timer, wake_at, cx);
        self.armed = true;
        Poll::Pending
    }
}

/// The handshake between a caller's readiness scan and this wait's
/// registration, which is where an input frame used to be lost: the frame's
/// wake drained a waker list the task had not joined yet, so the task slept
/// out the fallback tick with a packet already queued. `epoll`, `poll` and
/// `select` all guard it the same way -- read
/// [`crate::fs::devfs::input::wait::input_seq`] *before* the scan, hand it to
/// [`IoMultiplexWait::with_tick_hid`], and compare once registered.
#[cfg(test)]
mod hid_handshake_tests {
    use super::*;
    use crate::fs::devfs::input::wait::{input_seq, wake_input_waiters};
    use core::task::{Context, Poll};

    #[allow(unsafe_code)]
    fn nop_waker() -> core::task::Waker {
        use core::task::{RawWaker, RawWakerVTable, Waker};
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        // SAFETY: the vtable's four functions are all no-ops over a null
        // pointer they never dereference.
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
    }

    /// `watch_hid`, a sequence read before the scan, and the first `poll` --
    /// everything the real callers do, with the frame injected where it hurts.
    fn first_poll(watch_hid: bool, hid_seq: u64) -> Poll<()> {
        let waker = nop_waker();
        let mut cx = Context::from_waker(&waker);
        let fut = IoMultiplexWait::with_tick_hid(1_000, false, false, watch_hid, hid_seq, 100);
        let mut fut = core::pin::pin!(fut);
        fut.as_mut().poll(&mut cx)
    }

    #[test]
    fn a_frame_between_the_scan_and_the_registration_rescans_at_once() {
        // Read before the caller's scan, as epoll/poll/select do.
        let seq = input_seq();
        // The frame the old code lost: queued after the scan found nothing,
        // woken before this task was in the list.
        wake_input_waiters();
        assert_eq!(
            first_poll(true, seq),
            Poll::Ready(()),
            "a frame that landed after the scan has to send the caller round \
             again now, not after the fallback tick"
        );
    }

    #[test]
    fn with_no_frame_in_the_window_the_wait_parks() {
        assert_eq!(
            first_poll(true, input_seq()),
            Poll::Pending,
            "nothing arrived in the window, so the wait sleeps on the input \
             waker list"
        );
    }

    #[test]
    fn a_set_without_an_input_device_does_not_consult_the_counter() {
        let seq = input_seq();
        wake_input_waiters();
        assert_eq!(
            first_poll(false, seq),
            Poll::Pending,
            "no evdev node in this set: an input frame is not this wait's \
             business and must not cut its sleep short"
        );
    }
}
