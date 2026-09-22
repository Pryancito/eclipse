//! `timerfd_create(2)` — a timer that delivers expirations through a readable
//! file descriptor. libwayland's `wl_event_loop` arms one of these for all its
//! timers, so a Wayland compositor (labwc/wlroots) needs it to run.

use super::*;
use crate::sync::{Event, EventBus};
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
    /// (Re)arm the timer. `value_ns == 0` disarms it. `abs` selects an absolute
    /// monotonic deadline (`TFD_TIMER_ABSTIME`) rather than a relative one.
    fn arm(self: &Arc<Self>, value_ns: u64, interval_ns: u64, abs: bool) {
        let generation = self.generation.fetch_add(1, SeqCst) + 1;
        self.interval_ns.store(interval_ns, SeqCst);
        if value_ns == 0 {
            self.next_deadline_ns.store(0, SeqCst);
            return; // disarmed
        }
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        let deadline = if abs {
            value_ns
        } else {
            now.saturating_add(value_ns)
        };
        self.schedule(deadline, generation);
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
                if inner.generation.load(SeqCst) != generation {
                    return; // disarmed / re-armed: this callback is stale
                }
                inner.count.fetch_add(1, SeqCst);
                inner.publish_readiness();
                let interval = inner.interval_ns.load(SeqCst);
                if interval > 0 {
                    let next = kernel_hal::timer::timer_now().as_nanos() as u64 + interval;
                    inner.schedule(next, generation);
                }
            }),
        );
    }
}

/// timerfd implementation.
pub struct TimerFd {
    base: KObjectBase,
    inner: Arc<TimerInner>,
    /// Behind a lock so `fcntl(F_SETFL)` can change it after creation.
    flags: Mutex<OpenFlags>,
}

impl_kobject!(TimerFd);

impl TimerFd {
    /// Create a disarmed timerfd.
    pub fn new(flags: OpenFlags) -> Arc<Self> {
        Arc::new(TimerFd {
            base: KObjectBase::new(),
            inner: Arc::new(TimerInner {
                count: AtomicU64::new(0),
                interval_ns: AtomicU64::new(0),
                next_deadline_ns: AtomicU64::new(0),
                generation: AtomicU64::new(0),
                eventbus: EventBus::new(),
            }),
            flags: Mutex::new(flags),
        })
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
        self.inner.arm(value_ns, interval_ns, abs);
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

    fn dup(&self) -> Arc<dyn FileLike> {
        Arc::new(Self {
            base: KObjectBase::new(),
            inner: self.inner.clone(),
            flags: Mutex::new(self.flags()),
        })
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
        TimerFd::new(flags)
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
        let copy = fd.dup();
        assert!(
            copy.flags().non_block(),
            "a dup copies the flags as set now"
        );
        copy.set_flags(OpenFlags::empty()).unwrap();
        assert!(fd.flags().non_block(), "and keeps its own copy of them");
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
        let dup = fd.dup();
        let dup = dup.downcast_arc::<TimerFd>().ok().unwrap();
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
