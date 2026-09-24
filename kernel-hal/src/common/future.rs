use alloc::{boxed::Box, sync::Arc};
use core::task::{Context, Poll};
use core::time::Duration;
use core::{future::Future, pin::Pin};
use zcore_drivers::scheme::DisplayScheme;

use crate::timer;
use crate::timer_waker::{self, TimerWakerSlot};

#[must_use = "`yield_now()` does nothing unless polled/`await`-ed"]
#[derive(Default)]
pub(super) struct YieldFuture {
    flag: bool,
}

impl Future for YieldFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        if self.flag {
            Poll::Ready(())
        } else {
            self.flag = true;
            // Park the self-wake in the scheduler's yielded lane (behind any
            // external notify) so wake-up preemption actually hands the CPU to
            // the woken task instead of re-electing this one. See
            // `executor::begin_voluntary_yield` / `WakerPage::mark_yielded`.
            #[cfg(target_os = "none")]
            executor::begin_voluntary_yield(cx.waker().data() as usize);
            cx.waker().wake_by_ref();
            #[cfg(target_os = "none")]
            executor::end_voluntary_yield();
            Poll::Pending
        }
    }
}

#[must_use = "`sleep_until()` does nothing unless polled/`await`-ed"]
pub(super) struct SleepFuture {
    deadline: Duration,
    /// Waker slot shared with the armed timer callback. Armed once; re-polls
    /// refresh in place via [`timer_waker::ensure_timer_waker`].
    slot: Option<TimerWakerSlot>,
}

impl SleepFuture {
    pub fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            slot: None,
        }
    }

    /// Whether `deadline` is so far out that arming a timer for it is
    /// meaningless.
    ///
    /// The deadline is an unsigned `Duration` here, but every clock it has to
    /// reach is a signed count of nanoseconds, so "never" has to stop at this
    /// end rather than wrap around at the other one.
    fn is_never(deadline: Duration) -> bool {
        deadline.as_nanos() >= i64::MAX as u128
    }
}

impl Drop for SleepFuture {
    fn drop(&mut self) {
        // Cancel so a late tick cannot wake a finished/reused task.
        timer_waker::kill_timer_waker(&mut self.slot);
    }
}

impl Future for SleepFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        if timer::timer_now() >= this.deadline {
            timer_waker::kill_timer_waker(&mut this.slot);
            return Poll::Ready(());
        }
        if Self::is_never(this.deadline) {
            // The caller relies on some other wake source.
            return Poll::Pending;
        }
        let deadline = this.deadline;
        timer_waker::ensure_timer_waker(&mut this.slot, deadline, cx);
        Poll::Pending
    }
}

#[must_use = "`console_read()` does nothing unless polled/`await`-ed"]
pub(super) struct SerialReadFuture<'a> {
    buf: &'a mut [u8],
    sub_id: Option<u64>,
}

impl<'a> SerialReadFuture<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, sub_id: None }
    }
}

impl Drop for SerialReadFuture<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id.take() {
            if let Some(uart) = crate::drivers::all_uart().first() {
                uart.unsubscribe(id);
            }
        }
    }
}

impl Future for SerialReadFuture<'_> {
    type Output = usize;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();
        // `read(fd, _, 0)` has nothing to wait for, and this wait can only ever
        // end when a byte arrives: a zero-length read would then copy nothing,
        // find `n == 0` and park again, for ever. `zx_debug_read(h, buf, 0)`
        // never returned. Asked before the uart is even looked up, because the
        // answer does not depend on there being one.
        if this.buf.is_empty() {
            return Poll::Ready(0);
        }
        let uart = if let Some(uart) = crate::drivers::all_uart().first() {
            uart
        } else {
            return Poll::Pending;
        };
        let buf = &mut this.buf;
        let mut n = 0;
        for i in 0..buf.len() {
            if let Some(c) = uart.try_recv().unwrap_or(None) {
                buf[i] = c;
                n += 1;
            } else {
                break;
            }
        }
        if n > 0 {
            if let Some(id) = this.sub_id.take() {
                uart.unsubscribe(id);
            }
            return Poll::Ready(n);
        }
        if this.sub_id.is_none() {
            let waker = cx.waker().clone();
            this.sub_id = uart.subscribe(Box::new(move |_| waker.wake_by_ref()), true);
        }
        Poll::Pending
    }
}

/// Frame interval for a display advertising `refresh_rate` Hz.
///
/// Total, where `1000 / refresh_rate` was not at either end: it divides by zero
/// on a display that reports no rate at all, and above a kilohertz it rounds
/// down to a zero-length frame -- and a zero-length frame is a deadline that is
/// always already past, i.e. a whole-framebuffer flush every time round the
/// executor, for ever.
fn frame_time_of(refresh_rate: usize) -> Duration {
    /// What to assume when the display does not say. This flush loop is the
    /// only way the console reaches the screen on the boards that need it, so
    /// "unknown" has to mean a usable rate, not one frame per second.
    const ASSUMED_HZ: u64 = 60;
    let hz = match refresh_rate as u64 {
        0 => ASSUMED_HZ,
        hz => hz.min(1000),
    };
    Duration::from_millis(1000 / hz)
}

/// When the next frame is due, having just flushed at `now` the one that was
/// scheduled for `scheduled`.
///
/// Adding a frame to the instant that was *due*, rather than to `now`, is what
/// keeps a late wake-up from pushing the whole schedule back a little further
/// every time. But the schedule starts at zero while the clock starts at
/// however long the machine has been up, so the first frame was due one frame
/// after the epoch and every frame after it was still behind: this flushed the
/// whole framebuffer once per frame, as fast as the executor would go round,
/// until the schedule caught up with the uptime. Resync whenever a whole frame
/// has already been missed.
fn next_flush_after(now: Duration, scheduled: Duration, frame_time: Duration) -> Duration {
    let cadence = scheduled + frame_time;
    if cadence > now {
        cadence
    } else {
        now + frame_time
    }
}

pub(crate) struct DisplayFlushFuture {
    next_flush_time: Duration,
    frame_time: Duration,
    display: Arc<dyn DisplayScheme>,
    slot: Option<TimerWakerSlot>,
}

impl DisplayFlushFuture {
    #[allow(dead_code)]
    pub fn new(display: Arc<dyn DisplayScheme>, refresh_rate: usize) -> Self {
        Self {
            next_flush_time: Duration::default(),
            frame_time: frame_time_of(refresh_rate),
            display,
            slot: None,
        }
    }

    /// Push a frame if one is due at `now`, and say when the next one is.
    ///
    /// Split from [`Future::poll`] so the schedule can be watched a frame at a
    /// time: `poll` reads the clock and arms a timer, and neither of those is
    /// something a test can hold still.
    fn flush_due(&mut self, now: Duration) -> Duration {
        if now >= self.next_flush_time {
            self.display.flush().ok();
            self.next_flush_time = next_flush_after(now, self.next_flush_time, self.frame_time);
        }
        self.next_flush_time
    }
}

impl Drop for DisplayFlushFuture {
    fn drop(&mut self) {
        timer_waker::kill_timer_waker(&mut self.slot);
    }
}

impl Future for DisplayFlushFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let deadline = self.flush_due(timer::timer_now());
        // Every poll, not only the ones that flushed: a re-poll from another
        // task context otherwise leaves the timer pointing at whoever polled
        // last. `ensure_timer_waker` cancels the old cell itself when the
        // frame has moved on, and refreshes it in place when it has not.
        timer_waker::ensure_timer_waker(&mut self.slot, deadline, cx);
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timer_waker::test_waker::{waker_of, Probe};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use zcore_drivers::prelude::{ColorFormat, DisplayInfo, FrameBuffer};
    use zcore_drivers::scheme::Scheme;
    use zcore_drivers::DeviceResult;

    /// A display with no pixels that counts how often it is asked to push a
    /// frame. This future never reads the framebuffer -- only how often it
    /// decides a frame is due -- so an empty one is the honest fake.
    struct FlushCounter {
        flushes: AtomicUsize,
    }

    impl FlushCounter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                flushes: AtomicUsize::new(0),
            })
        }

        fn flushes(&self) -> usize {
            self.flushes.load(Ordering::SeqCst)
        }
    }

    impl Scheme for FlushCounter {
        fn name(&self) -> &str {
            "flush-counter"
        }
    }

    impl DisplayScheme for FlushCounter {
        fn info(&self) -> DisplayInfo {
            DisplayInfo {
                width: 0,
                height: 0,
                pitch: 0,
                format: ColorFormat::RGB888,
                fb_base_vaddr: 0,
                fb_size: 0,
            }
        }

        fn fb(&self) -> FrameBuffer<'_> {
            FrameBuffer::from_slice(&mut [])
        }

        fn need_flush(&self) -> bool {
            true
        }

        fn flush(&self) -> DeviceResult {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Poll a future once. Everything here is `Unpin`: no field of any of them
    /// points at another.
    fn poll_once<F: Future + Unpin>(f: &mut F, probe: &Arc<Probe>) -> Poll<F::Output> {
        let waker = waker_of(probe);
        let mut cx = Context::from_waker(&waker);
        Pin::new(f).poll(&mut cx)
    }

    #[test]
    fn yielding_wakes_itself_once_and_then_completes() {
        let probe = Probe::new();
        let mut f = YieldFuture::default();
        assert_eq!(poll_once(&mut f, &probe), Poll::Pending);
        assert_eq!(probe.wakes(), 1, "a yield has to re-schedule itself");
        assert_eq!(poll_once(&mut f, &probe), Poll::Ready(()));
    }

    #[test]
    fn a_deadline_already_past_is_ready_without_arming_a_timer() {
        let probe = Probe::new();
        let mut f = SleepFuture::new(Duration::from_nanos(1));
        assert_eq!(poll_once(&mut f, &probe), Poll::Ready(()));
        assert!(f.slot.is_none());
    }

    #[test]
    fn a_deadline_that_never_arrives_parks_without_arming_anything() {
        let probe = Probe::new();
        let mut f = SleepFuture::new(Duration::MAX);
        assert_eq!(poll_once(&mut f, &probe), Poll::Pending);
        assert!(f.slot.is_none(), "no timer heap entry for the heat death");
        assert_eq!(probe.clones(), 0, "and no waker parked anywhere");
    }

    #[test]
    fn a_deadline_inside_the_clock_is_not_never() {
        let never = Duration::from_nanos(i64::MAX as u64);
        assert!(SleepFuture::is_never(Duration::MAX));
        assert!(SleepFuture::is_never(never));
        assert!(!SleepFuture::is_never(never - Duration::from_nanos(1)));
        // 292 years of uptime is where this sits; everything real is below it.
        assert!(!SleepFuture::is_never(Duration::from_secs(86400 * 365)));
        assert!(!SleepFuture::is_never(Duration::from_millis(1)));
    }

    #[test]
    fn a_zero_length_console_read_returns_at_once() {
        let probe = Probe::new();
        let mut buf = [];
        let mut f = SerialReadFuture::new(&mut buf);
        assert_eq!(
            poll_once(&mut f, &probe),
            Poll::Ready(0),
            "`zx_debug_read(h, buf, 0)` waited for a byte it had nowhere to put"
        );
    }

    #[test]
    fn a_display_that_reports_no_refresh_rate_does_not_divide_by_zero() {
        assert_eq!(frame_time_of(0), Duration::from_millis(1000 / 60));
    }

    #[test]
    fn a_refresh_rate_above_a_kilohertz_still_leaves_a_frame_to_wait() {
        for hz in [1001, 10_000, usize::MAX] {
            assert_eq!(frame_time_of(hz), Duration::from_millis(1));
        }
    }

    #[test]
    fn every_refresh_rate_gets_a_frame_longer_than_nothing() {
        for hz in [0, 1, 24, 30, 50, 60, 75, 120, 144, 240, 1000, 100_000] {
            assert!(
                frame_time_of(hz) > Duration::ZERO,
                "{} Hz gave a zero-length frame, which never ends",
                hz
            );
        }
    }

    #[test]
    fn the_ordinary_refresh_rates_are_left_alone() {
        assert_eq!(frame_time_of(1), Duration::from_millis(1000));
        assert_eq!(frame_time_of(30), Duration::from_millis(33));
        assert_eq!(frame_time_of(60), Duration::from_millis(16));
        assert_eq!(frame_time_of(1000), Duration::from_millis(1));
    }

    #[test]
    fn a_display_future_for_a_rate_of_zero_can_be_built() {
        let f = DisplayFlushFuture::new(FlushCounter::new(), 0);
        assert_eq!(f.frame_time, frame_time_of(0));
    }

    #[test]
    fn a_frame_delivered_on_time_keeps_the_cadence() {
        let frame = Duration::from_millis(33);
        let scheduled = Duration::from_secs(900);
        let now = scheduled + Duration::from_millis(1);
        assert_eq!(
            next_flush_after(now, scheduled, frame),
            scheduled + frame,
            "one frame after it was due, not one frame after the late wake-up"
        );
    }

    #[test]
    fn a_schedule_the_clock_has_left_behind_resyncs_instead_of_catching_up() {
        let frame = Duration::from_millis(33);
        let now = Duration::from_secs(900);
        // What the future starts with: nothing flushed yet, and an uptime of
        // fifteen minutes. Catching up frame by frame is 27_000 flushes.
        assert_eq!(next_flush_after(now, Duration::ZERO, frame), now + frame);
    }

    #[test]
    fn the_next_frame_is_always_still_to_come() {
        let frame = Duration::from_millis(16);
        let now = Duration::from_secs(900);
        for behind_ms in [0, 1, 15, 16, 17, 1_000, 900_000] {
            let scheduled = now - Duration::from_millis(behind_ms);
            assert!(
                next_flush_after(now, scheduled, frame) > now,
                "a deadline already past is a flush loop that never yields"
            );
        }
    }

    /// The first flush used to be followed by one flush per frame since the
    /// epoch, as fast as the executor would go round, because the schedule
    /// started at zero while the clock starts at the machine's uptime.
    #[test]
    fn a_future_that_starts_late_flushes_once_not_once_per_frame_since_boot() {
        let display = FlushCounter::new();
        let mut f = DisplayFlushFuture::new(display.clone(), 60);
        let frame = Duration::from_millis(16);
        // Fifteen minutes of uptime: 56_250 frames to catch up on.
        let up = Duration::from_secs(900);

        assert_eq!(f.flush_due(up), up + frame);
        assert_eq!(display.flushes(), 1);
        assert_eq!(f.flush_due(up + Duration::from_millis(1)), up + frame);
        assert_eq!(display.flushes(), 1, "the next frame is not due yet");
        assert_eq!(f.flush_due(up + frame), up + frame + frame);
        assert_eq!(display.flushes(), 2, "and then it is, once");
    }

    #[test]
    fn a_future_kept_on_time_flushes_one_frame_at_a_time() {
        let display = FlushCounter::new();
        let mut f = DisplayFlushFuture::new(display.clone(), 60);
        let frame = Duration::from_millis(16);
        let mut now = Duration::from_secs(900);
        for expected in 1..=10 {
            let next = f.flush_due(now);
            assert_eq!(next, now + frame, "one frame on, with no drift");
            assert_eq!(display.flushes(), expected);
            now = next;
        }
    }
}
