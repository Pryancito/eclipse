//! `timerfd_create(2)` — a timer that delivers expirations through a readable
//! file descriptor. libwayland's `wl_event_loop` arms one of these for all its
//! timers, so a Wayland compositor (labwc/wlroots) needs it to run.

use super::*;
use crate::sync::{Event, EventBus};
use crate::time::{timer_arm_deadline, ClockBase};
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};
use core::time::Duration;
use lock::Mutex;
use zircon_object::object::*;

/// Shared timer state, separated from the [`TimerFd`] wrapper so the kernel
/// timer callback can hold a `Weak` to *just this* and not keep the fd alive.
struct TimerInner {
    /// Number of expirations since the last successful `read`.
    count: AtomicU64,
    /// Period for a recurring timer, in nanoseconds. `0` = one-shot.
    interval_ns: AtomicU64,
    /// Absolute monotonic deadline (ns) of the next expiration, for `gettime`.
    next_deadline_ns: AtomicU64,
    /// Bumped on every `settime`; an in-flight callback whose generation no
    /// longer matches was disarmed/re-armed and must not fire or re-schedule.
    generation: AtomicU64,
    eventbus: Arc<Mutex<EventBus>>,
}

impl TimerInner {
    /// (Re)arm the timer. `value_ns == 0` disarms it. `abs` selects an
    /// absolute deadline (`TFD_TIMER_ABSTIME`) rather than a relative one --
    /// absolute **on `base`**, the clock `timerfd_create` was given, which is
    /// not always the one the kernel timer runs on.
    fn arm(self: &Arc<Self>, base: ClockBase, value_ns: u64, interval_ns: u64, abs: bool) {
        let generation = self.generation.fetch_add(1, SeqCst) + 1;
        self.interval_ns.store(interval_ns, SeqCst);
        if value_ns == 0 {
            self.next_deadline_ns.store(0, SeqCst);
            return; // disarmed
        }
        let deadline = timer_arm_deadline(
            base,
            abs,
            Duration::from_nanos(value_ns),
            kernel_hal::timer::timer_now(),
            kernel_hal::timer::wall_clock_now(),
        );
        self.schedule(deadline.as_nanos() as u64, generation);
    }

    /// Publish readiness from the expiration count, which is the only thing
    /// that makes this fd readable.
    ///
    /// Same rule as `EventFd`: the bit must never disagree with the count in
    /// either direction. Set over a zero count spins a blocking reader (it
    /// re-checks the count, then waits on a bit that is already up, so
    /// `wait_for_event` returns immediately); clear over a non-zero count is a
    /// poller that never wakes. Taking the bus lock before reading the count
    /// is what orders this against a timer callback firing concurrently —
    /// every change to the count is followed by this call, so whichever
    /// publishes last read the count after the other's change.
    fn publish_readiness(&self) {
        let mut bus = self.eventbus.lock();
        if self.count.load(SeqCst) > 0 {
            bus.set(Event::READABLE);
        } else {
            bus.clear(Event::READABLE);
        }
    }

    fn schedule(self: &Arc<Self>, deadline_ns: u64, generation: u64) {
        self.next_deadline_ns.store(deadline_ns, SeqCst);
        let weak = Arc::downgrade(self);
        kernel_hal::timer::timer_set(
            Duration::from_nanos(deadline_ns),
            Box::new(move |_now| {
                let Some(inner) = weak.upgrade() else { return };
                let now = kernel_hal::timer::timer_now().as_nanos() as u64;
                if let Some(next) = inner.expire(deadline_ns, generation, now) {
                    inner.schedule(next, generation);
                }
            }),
        );
    }

    /// The kernel timer scheduled for `deadline_ns` has fired, at `now_ns`.
    /// Counts the expirations it stands for and says where a periodic timer
    /// fires next, or `None` for a one-shot or a stale callback.
    ///
    /// A periodic timer used to be re-armed at `now + interval` and counted
    /// as one expiration, however late the callback ran. That is not what a
    /// period is: Linux keeps the timer on the grid the arm laid down
    /// (`hrtimer_forward`), so the next expiry is a whole number of periods
    /// after the programmed one, and every period that went by while the
    /// timer could not fire (the process was stopped, the CPU was busy, the
    /// callback ran late) is an expiration the next `read` reports. Here
    /// each late callback pushed the whole grid back by its own lateness --
    /// a 16 ms frame timer drifted by every tick's latency, for good -- and
    /// a timer that missed five periods reported one, so a program pacing
    /// work by the count it reads did one fifth of it.
    fn expire(&self, deadline_ns: u64, generation: u64, now_ns: u64) -> Option<u64> {
        if self.generation.load(SeqCst) != generation {
            return None; // disarmed / re-armed: this callback is stale
        }
        let interval = self.interval_ns.load(SeqCst);
        let (next, expirations) = if interval > 0 {
            forward_periodic_ns(deadline_ns, interval, now_ns)
        } else {
            (0, 1)
        };
        self.count.fetch_add(expirations, SeqCst);
        self.publish_readiness();
        (interval > 0).then_some(next)
    }
}

/// Where a periodic timer whose expiry was at `expiry_ns` fires next, given
/// that the callback ran at `now_ns`, and how many expirations that stands
/// for: `hrtimer_forward`. The next expiry is a whole number of periods
/// after the programmed one and strictly after `now_ns`; the count is the
/// expiry that fired plus every whole period that had already gone by.
/// The twin for POSIX timers and `setitimer` is `forward_periodic` in
/// linux-syscall's `time.rs`, which reports the overruns separately.
///
/// `interval_ns` must be non-zero.
fn forward_periodic_ns(expiry_ns: u64, interval_ns: u64, now_ns: u64) -> (u64, u64) {
    let next = expiry_ns.saturating_add(interval_ns);
    if next > now_ns {
        return (next, 1);
    }
    // Whole periods since the programmed expiry, at least one: the first is
    // the expiry that fires now, the rest went by unfired.
    let periods = (now_ns - expiry_ns) / interval_ns;
    // Saturating all the way: `interval_ns` comes from userspace.
    let advance = (periods + 1).saturating_mul(interval_ns);
    (expiry_ns.saturating_add(advance), periods + 1)
}

/// timerfd implementation.
pub struct TimerFd {
    base: KObjectBase,
    inner: Arc<TimerInner>,
    /// The timeline `timerfd_create`'s `clockid` names, which is what an
    /// absolute `timerfd_settime` counts against. The id itself used to be
    /// thrown away, so every absolute deadline was read as a monotonic one
    /// and a `CLOCK_REALTIME` timer was armed for roughly the age of the
    /// Unix epoch from now.
    clock: ClockBase,
    /// Behind a lock so `fcntl(F_SETFL)` can change it after creation.
    flags: Mutex<OpenFlags>,
}

impl_kobject!(TimerFd);

impl TimerFd {
    /// Create a disarmed timerfd on `clock`.
    pub fn new(flags: OpenFlags, clock: ClockBase) -> Arc<Self> {
        Arc::new(TimerFd {
            base: KObjectBase::new(),
            inner: Arc::new(TimerInner {
                count: AtomicU64::new(0),
                interval_ns: AtomicU64::new(0),
                next_deadline_ns: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                eventbus: EventBus::new(),
            }),
            clock,
            flags: Mutex::new(flags),
        })
    }

    /// The timeline this timerfd's absolute deadlines are counted on.
    pub fn clock(&self) -> ClockBase {
        self.clock
    }

    /// Arm/disarm (`timerfd_settime`). `abs` = `TFD_TIMER_ABSTIME`.
    pub fn set_time(&self, value_ns: u64, interval_ns: u64, abs: bool) {
        // A fresh arm starts a new expiration epoch. Retire any callback still
        // in flight FIRST: one firing between the reset and the arm would add
        // an expiration belonging to the old epoch, and the reset would then
        // hide it — leaving a non-zero count under a cleared readiness bit,
        // which is a poller that never wakes.
        self.inner.generation.fetch_add(1, SeqCst);
        self.inner.count.store(0, SeqCst);
        self.inner.publish_readiness();
        self.inner.arm(self.clock, value_ns, interval_ns, abs);
    }

    /// `(interval_ns, remaining_ns)` for `timerfd_gettime`.
    pub fn get_time(&self) -> (u64, u64) {
        let interval = self.inner.interval_ns.load(SeqCst);
        let deadline = self.inner.next_deadline_ns.load(SeqCst);
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        let remaining = deadline.saturating_sub(now);
        (interval, remaining)
    }
}

#[async_trait]
impl FileLike for TimerFd {
    fn flags(&self) -> OpenFlags {
        *self.flags.lock()
    }

    fn set_flags(&self, f: OpenFlags) -> LxResult {
        self.flags.lock().take_settable(f);
        Ok(())
    }

    async fn read(&self, buf: &mut [u8]) -> LxResult<usize> {
        if buf.len() < 8 {
            return Err(LxError::EINVAL);
        }
        loop {
            let count = self.inner.count.swap(0, SeqCst);
            if count > 0 {
                self.inner.publish_readiness();
                buf[..8].copy_from_slice(&count.to_ne_bytes());
                return Ok(8);
            }
            if self.flags().non_block() {
                return Err(LxError::EAGAIN);
            }
            self.async_poll(PollEvents::IN).await?;
        }
    }

    fn write(&self, _buf: &[u8]) -> LxResult<usize> {
        Err(LxError::EINVAL)
    }

    async fn read_at(&self, _offset: u64, buf: &mut [u8]) -> LxResult<usize> {
        self.read(buf).await
    }

    fn poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        Ok(PollStatus {
            read: self.inner.count.load(SeqCst) > 0,
            write: false,
            error: false,
            hangup: false,
        })
    }

    async fn async_poll(&self, _events: PollEvents) -> LxResult<PollStatus> {
        loop {
            let status = self.poll(_events)?;
            if status.read {
                return Ok(status);
            }
            let bus = self.inner.eventbus.clone();
            crate::sync::wait_for_event(bus, Event::READABLE).await?;
        }
    }

    fn subscribe_readiness(
        &self,
        events: PollEvents,
        waker: &core::task::Waker,
    ) -> Option<crate::sync::ReadinessSub> {
        // Expiry sets READABLE from the kernel timer callback (see
        // `TimerInner::schedule`), and the deadline-programmed LAPIC fires at
        // the exact deadline — so a parked poller wakes AT expiry instead of
        // on its next 4 ms re-scan tick.
        let mask = super::poll_events_to_bus_mask(events);
        Some(crate::sync::subscribe_readiness_on(
            &self.inner.eventbus,
            mask,
            waker,
        ))
    }
}

#[cfg(test)]
mod tests {
    //! Host tests for `timerfd_create(2)`.
    //!
    //! These run against the real `kernel_hal::timer`, which under `libos` is
    //! an `async_std` sleep, so anything that waits for an expiration waits for
    //! wall-clock time. Every such test polls for a condition with a generous
    //! deadline rather than sleeping for a fixed span, so a slow CI runner
    //! makes them slower and not flaky. The bookkeeping tests (arming,
    //! disarming, `gettime`, the error paths) touch no timer at all.

    use super::*;
    use async_std::task::block_on;
    use core::time::Duration as CoreDuration;

    const MS: u64 = 1_000_000;

    fn tfd(flags: OpenFlags) -> Arc<TimerFd> {
        TimerFd::new(flags, ClockBase::Monotonic)
    }

    /// A timerfd on `CLOCK_REALTIME`, the clock `timerfd_create` used to
    /// throw away.
    fn wall_tfd(flags: OpenFlags) -> Arc<TimerFd> {
        TimerFd::new(flags, ClockBase::Wall)
    }

    fn nonblock() -> OpenFlags {
        OpenFlags::NON_BLOCK
    }

    /// `fcntl(F_SETFL, O_NONBLOCK)` on a timerfd made without `TFD_NONBLOCK`
    /// used to change nothing, so a read of a disarmed timer parked for good
    /// instead of answering EAGAIN. If this test hangs, that is the bug back.
    #[test]
    fn set_flags_turns_a_blocking_timerfd_non_blocking() {
        let fd = tfd(OpenFlags::empty());
        assert!(!fd.flags().non_block());
        fd.set_flags(nonblock()).unwrap();
        assert!(fd.flags().non_block());
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
    }

    /// The clock the fd was made on is what an absolute deadline is counted
    /// against. `timerfd_create` used to log its `clockid` and drop it, so
    /// this was the monotonic clock whatever the caller asked for.
    #[test]
    fn a_timerfd_remembers_the_clock_it_was_made_on() {
        assert_eq!(tfd(nonblock()).clock(), ClockBase::Monotonic);
        assert_eq!(wall_tfd(nonblock()).clock(), ClockBase::Wall);
    }

    /// `timerfd_settime(TFD_TIMER_ABSTIME)` on a `CLOCK_REALTIME` fd: the
    /// value is seconds since 1970, and arming it as a monotonic deadline
    /// put the expiry more than half a century out. glibc's `sleep`-style
    /// helpers and every `sd_event` REALTIME source pass exactly this.
    #[test]
    fn an_absolute_deadline_on_the_wall_clock_is_not_half_a_century_away() {
        let fd = wall_tfd(nonblock());
        let wall_now = kernel_hal::timer::wall_clock_now().as_nanos() as u64;
        fd.set_time(wall_now + 500 * MS, 0, true);
        let (_, remaining) = fd.get_time();
        assert!(
            remaining > 100 * MS && remaining <= 500 * MS,
            "remaining {} is not half a second; the wall clock was read as monotonic",
            remaining
        );
    }

    /// And the same deadline on a monotonic fd is decades away, which is
    /// what every realtime timerfd used to get. Nothing here waits for it:
    /// the point is that `gettime` reports it, so the timer was armed and
    /// will not fire in this machine's lifetime.
    #[test]
    fn the_same_date_on_a_monotonic_timerfd_is_the_bug_it_used_to_be() {
        let fd = tfd(nonblock());
        let wall_now = kernel_hal::timer::wall_clock_now().as_nanos() as u64;
        fd.set_time(wall_now, 0, true);
        let (_, remaining) = fd.get_time();
        const TEN_YEARS: u64 = 10 * 365 * 24 * 3600 * 1_000_000_000;
        assert!(
            remaining > TEN_YEARS,
            "remaining {} should be the decades a monotonic reading gives",
            remaining
        );
    }

    /// A wall-clock deadline already gone by fires at once, the same as a
    /// monotonic one. Subtracting the wrong `now` would make it a deadline
    /// far in the future instead of a due one.
    #[test]
    fn an_absolute_wall_clock_deadline_already_past_fires_at_once() {
        let fd = wall_tfd(nonblock());
        let wall_now = kernel_hal::timer::wall_clock_now().as_nanos() as u64;
        fd.set_time(wall_now.saturating_sub(500 * MS), 0, true);
        assert!(within_two_seconds(|| fd.poll(PollEvents::IN).unwrap().read));
        assert_eq!(read8(&fd).unwrap(), 1);
    }

    /// A relative arm means the same thing on either clock: it is a length,
    /// and the wall clock never enters into it.
    #[test]
    fn a_relative_arm_on_the_wall_clock_is_still_a_length() {
        let fd = wall_tfd(nonblock());
        fd.set_time(300 * MS, 0, false);
        let (_, remaining) = fd.get_time();
        assert!(
            remaining > 0 && remaining <= 300 * MS,
            "remaining {} is not the 300 ms just armed",
            remaining
        );
    }

    fn read8(fd: &TimerFd) -> LxResult<u64> {
        let mut buf = [0u8; 8];
        let n = block_on(fd.read(&mut buf))?;
        assert_eq!(n, 8);
        Ok(u64::from_ne_bytes(buf))
    }

    fn bit_says_readable(fd: &TimerFd) -> bool {
        fd.inner.eventbus.lock().events().contains(Event::READABLE)
    }

    /// Poll `cond` until it holds or two seconds pass. Two seconds is far
    /// beyond any interval these tests arm, so a failure means the timer never
    /// fired rather than that the machine was busy.
    fn within_two_seconds(mut cond: impl FnMut() -> bool) -> bool {
        block_on(async {
            for _ in 0..400 {
                if cond() {
                    return true;
                }
                async_std::task::sleep(CoreDuration::from_millis(5)).await;
            }
            false
        })
    }

    fn sleep_ms(ms: u64) {
        block_on(async_std::task::sleep(CoreDuration::from_millis(ms)));
    }

    #[test]
    fn a_fresh_timerfd_is_disarmed_and_never_readable() {
        let fd = tfd(nonblock());
        assert_eq!(fd.get_time(), (0, 0));
        assert!(!fd.poll(PollEvents::IN).unwrap().read);
        assert!(!bit_says_readable(&fd));
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
    }

    #[test]
    fn a_timerfd_is_read_only_and_reads_whole_counters() {
        let fd = tfd(nonblock());
        assert_eq!(fd.write(&[0u8; 8]), Err(LxError::EINVAL));
        let mut small = [0u8; 7];
        assert_eq!(block_on(fd.read(&mut small)), Err(LxError::EINVAL));
        // `poll` never advertises writability, so an event loop does not spin
        // on an fd it can never write.
        let s = fd.poll(PollEvents::IN | PollEvents::OUT).unwrap();
        assert!(!s.write && !s.error && !s.hangup);
    }

    #[test]
    fn arming_reports_the_interval_and_a_shrinking_remainder() {
        let fd = tfd(nonblock());
        fd.set_time(500 * MS, 250 * MS, false);
        let (interval, first) = fd.get_time();
        assert_eq!(interval, 250 * MS);
        assert!(
            first > 0 && first <= 500 * MS,
            "remaining {} is not inside the half second just armed",
            first
        );
        sleep_ms(30);
        let (_, later) = fd.get_time();
        assert!(later < first, "the deadline is not approaching");
    }

    #[test]
    fn an_absolute_deadline_is_taken_as_given_not_added_to_now() {
        let fd = tfd(nonblock());
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        fd.set_time(now + 500 * MS, 0, true);
        let (_, remaining) = fd.get_time();
        // Read relatively it would be a second away; read absolutely, half of
        // one. TFD_TIMER_ABSTIME is how a Wayland compositor schedules a frame
        // callback, so getting it wrong doubles every timeout.
        assert!(
            remaining > 100 * MS && remaining <= 500 * MS,
            "remaining {} does not look like an absolute deadline",
            remaining
        );
    }

    #[test]
    fn an_absolute_deadline_already_past_fires_at_once() {
        let fd = tfd(nonblock());
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        fd.set_time(now.saturating_sub(500 * MS), 0, true);
        assert!(within_two_seconds(|| fd.poll(PollEvents::IN).unwrap().read));
        assert_eq!(read8(&fd).unwrap(), 1);
    }

    #[test]
    fn a_one_shot_expires_once_and_is_then_empty() {
        let fd = tfd(nonblock());
        fd.set_time(10 * MS, 0, false);
        assert!(within_two_seconds(|| fd.poll(PollEvents::IN).unwrap().read));
        assert!(bit_says_readable(&fd));
        assert_eq!(read8(&fd).unwrap(), 1);
        // The read consumed it, and nothing re-arms a one-shot.
        assert!(!bit_says_readable(&fd));
        sleep_ms(60);
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
    }

    #[test]
    fn a_periodic_timer_keeps_expiring_and_one_read_drains_the_backlog() {
        let fd = tfd(nonblock());
        fd.set_time(5 * MS, 5 * MS, false);
        // Let several periods go by without reading: the expirations pile up
        // into the counter, which is exactly what the count is for.
        assert!(within_two_seconds(|| fd.inner.count.load(SeqCst) >= 3));
        let drained = read8(&fd).unwrap();
        assert!(
            drained >= 3,
            "one read must take the whole backlog, got {}",
            drained
        );
        // And it keeps going afterwards.
        assert!(within_two_seconds(|| fd.poll(PollEvents::IN).unwrap().read));
        fd.set_time(0, 0, false);
    }

    #[test]
    fn disarming_stops_the_expirations_for_good() {
        let fd = tfd(nonblock());
        fd.set_time(5 * MS, 5 * MS, false);
        assert!(within_two_seconds(|| fd.inner.count.load(SeqCst) >= 2));

        // The generation guard is what makes this work: the callback already
        // scheduled for the next period is still in flight and must retire
        // itself instead of firing and re-scheduling.
        fd.set_time(0, 0, false);
        assert_eq!(fd.get_time(), (0, 0));
        assert!(!bit_says_readable(&fd));
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));

        sleep_ms(100); // twenty periods' worth
        assert_eq!(
            fd.inner.count.load(SeqCst),
            0,
            "a disarmed timer kept firing"
        );
    }

    #[test]
    fn re_arming_throws_away_the_expirations_of_the_old_epoch() {
        let fd = tfd(nonblock());
        fd.set_time(5 * MS, 5 * MS, false);
        assert!(within_two_seconds(|| fd.inner.count.load(SeqCst) >= 2));

        // `timerfd_settime` starts a new epoch: the pending count belongs to
        // the settings that were just replaced and must not be delivered.
        fd.set_time(500 * MS, 0, false);
        assert_eq!(fd.inner.count.load(SeqCst), 0);
        assert!(!bit_says_readable(&fd));
        assert_eq!(read8(&fd), Err(LxError::EAGAIN));
        // The old period was 5 ms; if the old callbacks were still live this
        // would have gone readable many times over by now.
        sleep_ms(80);
        assert_eq!(fd.inner.count.load(SeqCst), 0);
        assert_eq!(fd.get_time().0, 0, "the old interval survived the re-arm");
        fd.set_time(0, 0, false);
    }

    #[test]
    fn a_blocking_read_parks_until_the_timer_expires() {
        let fd = tfd(OpenFlags::empty());
        let before = kernel_hal::timer::timer_now();
        fd.set_time(40 * MS, 0, false);
        assert_eq!(read8(&fd).unwrap(), 1);
        let waited = kernel_hal::timer::timer_now() - before;
        // It really waited rather than returning a stale expiration.
        assert!(
            waited >= CoreDuration::from_millis(30),
            "the read came back after {:?}, before the deadline",
            waited
        );
    }

    #[test]
    fn a_dup_shares_the_timer_and_its_expirations() {
        let fd = tfd(nonblock());
        let dup = fd.clone();
        fd.set_time(10 * MS, 0, false);
        // Arming through one fd arms the other: they are one timer.
        assert!(dup.get_time().1 > 0);
        assert!(within_two_seconds(|| dup
            .poll(PollEvents::IN)
            .unwrap()
            .read));
        assert_eq!(read8(&dup).unwrap(), 1);
        // And draining through the dup empties the original.
        assert!(!fd.poll(PollEvents::IN).unwrap().read);
        assert!(!bit_says_readable(&fd));
    }

    #[test]
    fn the_fd_dying_does_not_keep_the_timer_alive() {
        // The callback holds a `Weak`, so a dropped timerfd must not be
        // resurrected by its own pending expiration — that would keep a
        // closed fd's memory alive for the whole period of a long timer.
        let fd = tfd(nonblock());
        fd.set_time(20 * MS, 20 * MS, false);
        let weak = Arc::downgrade(&fd.inner);
        drop(fd);
        assert!(within_two_seconds(|| weak.upgrade().is_none()));
    }

    #[test]
    fn a_subscriber_is_woken_by_the_expiry_itself() {
        use core::sync::atomic::{AtomicBool, Ordering};
        use core::task::{RawWaker, RawWakerVTable, Waker};

        static WOKE: AtomicBool = AtomicBool::new(false);
        fn vtable() -> &'static RawWakerVTable {
            &RawWakerVTable::new(
                |p| RawWaker::new(p, vtable()),
                |_| WOKE.store(true, Ordering::SeqCst),
                |_| WOKE.store(true, Ordering::SeqCst),
                |_| {},
            )
        }
        WOKE.store(false, Ordering::SeqCst);
        let waker = unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), vtable())) };

        let fd = tfd(nonblock());
        let sub = fd.subscribe_readiness(PollEvents::IN, &waker);
        assert!(sub.is_some(), "epoll relies on this being wired up");
        assert!(!WOKE.load(Ordering::SeqCst));

        fd.set_time(10 * MS, 0, false);
        assert!(
            within_two_seconds(|| WOKE.load(Ordering::SeqCst)),
            "the expiry never reached the parked poller"
        );
    }
}

/// A periodic timerfd stays on its grid and counts the periods it missed.
#[cfg(test)]
mod periodic_grid_tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn a_callback_that_runs_a_little_late_steps_from_the_deadline_not_from_now() {
        // Armed for 100 ms with a 16 ms period, the callback ran at 101 ms.
        assert_eq!(
            forward_periodic_ns(100 * MS, 16 * MS, 101 * MS),
            (116 * MS, 1)
        );
        // Running exactly on time is the same step.
        assert_eq!(
            forward_periodic_ns(100 * MS, 16 * MS, 100 * MS),
            (116 * MS, 1)
        );
    }

    #[test]
    fn periods_that_went_by_unfired_are_counted_and_skipped() {
        // Two and a half periods late: this expiry, the two it missed, and
        // the next one is the first grid point after now.
        assert_eq!(
            forward_periodic_ns(100 * MS, 16 * MS, 140 * MS),
            (148 * MS, 3)
        );
        // Exactly one period late: `hrtimer_forward` never returns an expiry
        // at or before now, so the one at 116 counts as passed too.
        assert_eq!(
            forward_periodic_ns(100 * MS, 16 * MS, 116 * MS),
            (132 * MS, 2)
        );
    }

    #[test]
    fn a_stopped_process_s_timer_does_not_fire_ten_thousand_times_to_catch_up() {
        let (next, count) = forward_periodic_ns(0, MS, 10_000 * MS + MS / 2);
        assert_eq!(count, 10_001);
        assert_eq!(next, 10_001 * MS);
    }

    #[test]
    fn an_interval_that_overflows_saturates_instead_of_wrapping() {
        // Armed at 0 with a period of 2^63 ns, reached at the end of time:
        // one period went by, and two of them do not fit in a u64.
        let (next, count) = forward_periodic_ns(0, 1 << 63, u64::MAX);
        assert_eq!(count, 2);
        assert_eq!(next, u64::MAX);
        let (next, count) = forward_periodic_ns(u64::MAX - 5, u64::MAX / 2, u64::MAX);
        assert_eq!(count, 1);
        assert_eq!(next, u64::MAX);
    }

    fn inner(interval_ns: u64) -> Arc<TimerInner> {
        Arc::new(TimerInner {
            count: AtomicU64::new(0),
            interval_ns: AtomicU64::new(interval_ns),
            next_deadline_ns: AtomicU64::new(0),
            generation: AtomicU64::new(7),
            eventbus: EventBus::new(),
        })
    }

    #[test]
    fn a_late_expiry_adds_every_missed_period_to_the_count_and_reschedules_on_the_grid() {
        let t = inner(5 * MS);
        // Scheduled for 50 ms, ran at 63 ms: 50 fired, 55 and 60 went by.
        assert_eq!(t.expire(50 * MS, 7, 63 * MS), Some(65 * MS));
        assert_eq!(t.count.load(SeqCst), 3);
        assert!(t.eventbus.lock().events().contains(Event::READABLE));
        // The next one on time adds one more.
        assert_eq!(t.expire(65 * MS, 7, 65 * MS + 100), Some(70 * MS));
        assert_eq!(t.count.load(SeqCst), 4);
    }

    #[test]
    fn a_one_shot_counts_once_and_asks_for_nothing_more() {
        let t = inner(0);
        assert_eq!(t.expire(50 * MS, 7, 90 * MS), None);
        assert_eq!(t.count.load(SeqCst), 1);
    }

    #[test]
    fn a_stale_callback_counts_nothing() {
        let t = inner(5 * MS);
        assert_eq!(t.expire(50 * MS, 6, 63 * MS), None);
        assert_eq!(t.count.load(SeqCst), 0);
        assert!(!t.eventbus.lock().events().contains(Event::READABLE));
    }
}
