use alloc::{boxed::Box, sync::Arc};
use core::task::{Context, Poll};
use core::time::Duration;
use core::{future::Future, pin::Pin};
use zcore_drivers::scheme::DisplayScheme;

use crate::timer;
use crate::timer_waker::{self, TimerWakerSlot};

/// The voluntary-yield marker, as the two builds see it.
///
/// On bare metal this is the scheduler's own per-CPU marker. The hosted
/// `libos` build has no run queue and no lanes, so there is nothing to mark —
/// but the *decision* is shared code, and before this seam existed it lived
/// inside a `cfg(target_os = "none")` block where no test on this side could
/// reach it. Wiring `preempt_now` to the voluntary marker would put the
/// starvation straight back, and nothing would have failed. So the libos stub
/// counts instead of doing nothing, and
/// `sched_yield_marks_the_yield_lane_and_a_preemption_does_not` reads it.
mod mark {
    #[cfg(target_os = "none")]
    pub(super) fn begin_voluntary_yield(waker_id: usize) {
        executor::begin_voluntary_yield(waker_id);
    }

    #[cfg(target_os = "none")]
    pub(super) fn end_voluntary_yield() {
        executor::end_voluntary_yield();
    }

    #[cfg(not(target_os = "none"))]
    pub(super) use host::{begin_voluntary_yield, end_voluntary_yield};

    #[cfg(not(target_os = "none"))]
    mod host {
        use core::sync::atomic::{AtomicUsize, Ordering};

        /// How many voluntary-yield markers this build has raised, and how
        /// many it has taken back down. Only a test reads them; `libos` has no
        /// lanes for them to steer. Both are counted because a marker left up
        /// is worse than one never raised: on bare metal that CPU then answers
        /// "voluntary" to every external wake it raises, forever.
        pub(super) static MARKED: AtomicUsize = AtomicUsize::new(0);
        pub(super) static CLEARED: AtomicUsize = AtomicUsize::new(0);

        pub(crate) fn begin_voluntary_yield(_waker_id: usize) {
            MARKED.fetch_add(1, Ordering::Relaxed);
        }

        pub(crate) fn end_voluntary_yield() {
            CLEARED.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(all(test, not(target_os = "none")))]
    pub(super) fn marks_raised() -> usize {
        host::MARKED.load(core::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(all(test, not(target_os = "none")))]
    pub(super) fn marks_cleared() -> usize {
        host::CLEARED.load(core::sync::atomic::Ordering::Relaxed)
    }
}

#[must_use = "`yield_now()` does nothing unless polled/`await`-ed"]
#[derive(Default)]
pub(super) struct YieldFuture {
    flag: bool,
    /// Whether the task is *choosing* to give up the CPU.
    ///
    /// The two callers of this future are not the same event, and filing them
    /// in the same lane starved the second one. `sched_yield(2)` is a task
    /// saying "someone else first", and the yielded lane is exactly right for
    /// it. The trap path's end-of-timeslice preemption is the scheduler taking
    /// the CPU away from a task that asked for nothing — and the yielded lane
    /// is drained only once no urgent notify is left anywhere on the CPU's
    /// queue, so a CPU-bound task filed there did not run again for as long as
    /// any peer kept waking. One peer waking every 200 us is enough to hold it
    /// off indefinitely, which makes the slice length irrelevant: losing the
    /// CPU at the end of a slice has to be survivable.
    voluntary: bool,
}

impl YieldFuture {
    /// The task is giving up the CPU of its own accord (`sched_yield(2)`).
    pub(super) fn voluntary() -> Self {
        Self {
            flag: false,
            voluntary: true,
        }
    }

    /// The scheduler is taking the CPU away (timeslice expiry, wake-up
    /// preemption). The task competes for it again on equal terms.
    pub(super) fn involuntary() -> Self {
        Self {
            flag: false,
            voluntary: false,
        }
    }
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
            //
            // Only for a *voluntary* yield: without the marker the wake takes
            // the ordinary notified path, which for an already-borrowed task
            // (which this is — the self-wake runs inside its own poll) is the
            // same `maybe_send_resched_ipi` and the same deferral, differing
            // only in the lane it waits in.
            if self.voluntary {
                mark::begin_voluntary_yield(cx.waker().data() as usize);
            }
            cx.waker().wake_by_ref();
            if self.voluntary {
                mark::end_voluntary_yield();
            }
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

/// What a poll of [`SleepFuture`] does about its deadline, given the clock.
#[derive(Debug, PartialEq, Eq)]
enum Sleep {
    /// The deadline has arrived: the sleep is over.
    Over,
    /// So far out that no timer can name it. Some other wake source has to
    /// end this wait.
    Unreachable,
    /// Arm a timer for this instant.
    Arm(Duration),
}

impl SleepFuture {
    pub fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            slot: None,
        }
    }

    /// What to do at `now` about a sleep until `deadline`.
    ///
    /// Split from [`Future::poll`] for the same reason as
    /// [`DisplayFlushFuture::flush_due`]: `poll` reads the clock and arms a
    /// timer, and a test can hold neither of those still. The boundary is the
    /// whole of the first arm -- `sleep_until(t)` is over *at* `t`, not only
    /// after it, and a deadline that lands on the very tick that reads it
    /// would otherwise arm a timer for an instant already gone instead of
    /// returning.
    fn step(now: Duration, deadline: Duration) -> Sleep {
        if now >= deadline {
            Sleep::Over
        } else if Self::is_never(deadline) {
            Sleep::Unreachable
        } else {
            Sleep::Arm(deadline)
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
        match Self::step(timer::timer_now(), this.deadline) {
            Sleep::Over => {
                timer_waker::kill_timer_waker(&mut this.slot);
                Poll::Ready(())
            }
            Sleep::Unreachable => Poll::Pending,
            Sleep::Arm(deadline) => {
                timer_waker::ensure_timer_waker(&mut this.slot, deadline, cx);
                Poll::Pending
            }
        }
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
    use alloc::collections::VecDeque;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use zcore_drivers::prelude::{ColorFormat, DisplayInfo, FrameBuffer};
    use zcore_drivers::scheme::{EventScheme, Scheme, UartScheme};
    use zcore_drivers::utils::EventHandler;
    use zcore_drivers::{Device, DeviceResult};

    // ── which of the two ways of giving up the CPU this is ────────────────

    /// Poll a future to completion, counting the voluntary-yield markers it
    /// raises on the way.
    fn marks_while_polling(fut: impl Future<Output = ()>) -> usize {
        let (raised, cleared) = (mark::marks_raised(), mark::marks_cleared());
        let probe = Probe::new();
        let waker = waker_of(&probe);
        let mut cx = Context::from_waker(&waker);
        let mut fut = Box::pin(fut);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "did not yield once"
        );
        assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(()));
        let n = mark::marks_raised() - raised;
        // Every marker raised comes back down before the poll returns. One left
        // up outlives the task it names: see the `VOLUNTARY_YIELD` comment in
        // `PreemptiveScheduler`, where a flag kept up made its CPU file every
        // external wake it raised in the low-priority lane.
        assert_eq!(
            mark::marks_cleared() - cleared,
            n,
            "a voluntary-yield marker was left up"
        );
        n
    }

    #[test]
    fn sched_yield_marks_the_yield_lane_and_a_preemption_does_not() {
        // This is the whole fix, read through the two public entry points
        // rather than through the struct: wiring `preempt_now` to the
        // voluntary marker puts the starvation straight back, and wiring
        // `yield_now` away from it inverts it. Driven through
        // `crate::thread::*` on purpose — the mutants that matter are in those
        // two one-line bodies.
        //
        // What the marker then does to the lane is `PreemptiveScheduler`'s
        // `waker_page`: see
        // `a_thread_the_scheduler_preempted_keeps_its_place_in_the_urgent_lane`.
        assert_eq!(
            marks_while_polling(crate::thread::yield_now()),
            1,
            "sched_yield(2) stopped asking to go behind the urgent lane"
        );
        assert_eq!(
            marks_while_polling(crate::thread::preempt_now()),
            0,
            "an end-of-slice preemption is being filed as a voluntary yield"
        );
    }

    #[test]
    fn the_old_single_constructor_is_the_involuntary_one() {
        // `Default` is what the one constructor used to be. Nothing should be
        // reaching for it now that the two events are told apart, and if
        // something does, the safe reading is "the scheduler took the CPU".
        assert!(!YieldFuture::default().voluntary);
        assert_eq!(marks_while_polling(YieldFuture::default()), 0);
    }

    /// A uart whose bytes a test hands it, counting its subscriptions.
    ///
    /// `zcore_drivers::mock::MockUart` reads the process's real stdin and
    /// writes its real stdout, so it cannot answer a test.
    struct FakeUart {
        incoming: std::sync::Mutex<VecDeque<u8>>,
        subscribed: AtomicUsize,
        unsubscribed: AtomicUsize,
    }

    impl FakeUart {
        fn with(bytes: &[u8]) -> Arc<Self> {
            Arc::new(Self {
                incoming: std::sync::Mutex::new(bytes.iter().copied().collect()),
                subscribed: AtomicUsize::new(0),
                unsubscribed: AtomicUsize::new(0),
            })
        }

        fn push(&self, byte: u8) {
            self.incoming
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(byte);
        }

        fn subscribed(&self) -> usize {
            self.subscribed.load(Ordering::SeqCst)
        }

        fn unsubscribed(&self) -> usize {
            self.unsubscribed.load(Ordering::SeqCst)
        }
    }

    impl Scheme for FakeUart {
        fn name(&self) -> &str {
            "fake-uart"
        }
    }

    impl EventScheme for FakeUart {
        type Event = ();

        fn trigger(&self, _event: ()) {}

        fn subscribe(&self, _handler: EventHandler<()>, _once: bool) -> Option<u64> {
            Some(self.subscribed.fetch_add(1, Ordering::SeqCst) as u64)
        }

        fn unsubscribe(&self, _id: u64) {
            self.unsubscribed.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl UartScheme for FakeUart {
        fn try_recv(&self) -> DeviceResult<Option<u8>> {
            Ok(self
                .incoming
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front())
        }

        fn send(&self, _ch: u8) -> DeviceResult {
            Ok(())
        }

        /// The console pushes the kernel log at whatever uart is registered,
        /// and while this one is attached it is that uart: swallow another
        /// test's log line rather than print it.
        fn write_str(&self, _s: &str) -> DeviceResult {
            Ok(())
        }
    }

    /// Register `uart` as *the* uart for as long as the guard lives.
    ///
    /// The device lists are process-wide and `all_uart().first()` takes the
    /// first entry, so only one test at a time may hold one.
    struct Attached {
        dev: Device,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Attached {
        fn new(uart: &Arc<FakeUart>) -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dev = Device::Uart(uart.clone());
            crate::drivers::add_device_hosted(dev.clone());
            Self { dev, _guard: guard }
        }
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            let _ = crate::drivers::remove_device_hosted(&self.dev);
        }
    }

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
    fn the_sleep_is_over_at_the_deadline_itself_and_not_a_tick_later() {
        // `sleep_until(t)` is over *at* `t`. This is the only thing the
        // decision decides, and the clock `poll` reads cannot be held still,
        // so the boundary is asked of the seam.
        let t = Duration::from_secs(9);
        assert_eq!(SleepFuture::step(t, t), Sleep::Over);
        assert_eq!(
            SleepFuture::step(t + Duration::from_nanos(1), t),
            Sleep::Over
        );
        assert_eq!(
            SleepFuture::step(t - Duration::from_nanos(1), t),
            Sleep::Arm(t),
            "a nanosecond short is still a wait, and what gets armed is the deadline"
        );
        let never = Duration::from_nanos(i64::MAX as u64);
        assert_eq!(SleepFuture::step(Duration::ZERO, never), Sleep::Unreachable);
    }

    #[test]
    fn a_single_byte_is_a_whole_console_read() {
        // A read that waits for input has to come back on the first byte.
        // Waiting for a second one leaves a reader parked on every single
        // keystroke, which is every prompt there is.
        let uart = FakeUart::with(b"k");
        let _attached = Attached::new(&uart);
        let probe = Probe::new();
        let mut buf = [0u8; 16];
        {
            let mut f = SerialReadFuture::new(&mut buf);
            assert_eq!(poll_once(&mut f, &probe), Poll::Ready(1));
        }
        assert_eq!(buf[0], b'k');
    }

    #[test]
    fn the_reader_subscribes_once_and_only_while_it_is_waiting() {
        // The subscription is what the arrival of a byte wakes. Subscribing
        // only when one is already held means none is ever made and the read
        // sleeps through every keystroke; subscribing on every poll parks a
        // waker per poll inside the driver.
        let uart = FakeUart::with(b"");
        let _attached = Attached::new(&uart);
        let probe = Probe::new();
        let mut buf = [0u8; 4];
        let mut f = SerialReadFuture::new(&mut buf);
        assert_eq!(poll_once(&mut f, &probe), Poll::Pending);
        assert_eq!(uart.subscribed(), 1);
        assert_eq!(poll_once(&mut f, &probe), Poll::Pending);
        assert_eq!(uart.subscribed(), 1, "one wait, one waker");
        // And the byte that ends the wait takes the subscription with it.
        uart.push(b'k');
        assert_eq!(poll_once(&mut f, &probe), Poll::Ready(1));
        assert_eq!(uart.unsubscribed(), 1);
    }

    #[test]
    fn the_next_frame_is_measured_from_the_schedule_and_not_from_the_flush() {
        // `flush_due` hands the clock and the schedule to `next_flush_after`
        // in that order. The other way round, a frame pushed early sets the
        // next deadline a whole frame after the push instead of after the
        // frame it was due, which is the drift the resync exists to avoid.
        let display = FlushCounter::new();
        let mut f = DisplayFlushFuture::new(display.clone(), 100);
        assert_eq!(f.frame_time, Duration::from_millis(10));
        assert_eq!(
            f.flush_due(Duration::from_millis(5)),
            Duration::from_millis(10)
        );
        assert_eq!(display.flushes(), 1);
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
