use crate::object::*;
use alloc::boxed::Box;
use alloc::sync::Arc;
use core::time::Duration;
use kernel_hal::{sync::Mutex, timer::timer_now};

/// An object that may be signaled at some point in the future
///
/// ## SYNOPSIS
///
/// A timer is used to wait until a specified point in time has occurred
/// or the timer has been canceled.
pub struct Timer {
    base: KObjectBase,
    _counter: CountHelper,
    #[allow(dead_code)]
    slack: Slack,
    /// `ZX_CLOCK_MONOTONIC` (0) or `ZX_CLOCK_BOOT` (1) from `zx_timer_create`.
    clock_id: u32,
    inner: Mutex<TimerInner>,
}

impl_kobject!(Timer);
define_count_helper!(Timer);

#[derive(Default)]
struct TimerInner {
    deadline: Option<Duration>,
    slack: Duration,
}

/// Slack specifies how much a timer or event is allowed to deviate from its deadline.
///
/// **Not supported: Now slack has no effect on the timer.**
#[repr(u32)]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Slack {
    /// slack is centered around deadline
    Center = 0,
    /// slack interval is (deadline - slack, deadline]
    Early = 1,
    /// slack interval is [deadline, deadline + slack)
    Late = 2,
}

impl Slack {
    /// Convert the raw `zx_timer_slack_t` mode a process wrote.
    ///
    /// `zx_job_set_policy` reads a `zx_policy_timer_slack_t` straight out of
    /// the caller's memory, so the mode is whatever the process put there and
    /// must not be materialised as this enum without checking.
    pub fn from_raw(raw: u32) -> ZxResult<Self> {
        Ok(match raw {
            0 => Self::Center,
            1 => Self::Early,
            2 => Self::Late,
            _ => return Err(ZxError::INVALID_ARGS),
        })
    }
}

/// Has `deadline` arrived, as of `now`?
///
/// A deadline falling exactly on `now` **has** arrived: `zx_timer_set` with a
/// deadline in the past, and the past includes this instant, signals at once
/// rather than arming anything.
///
/// Written out here because the two places that ask -- [`Timer::set`], which
/// signals on the spot instead of handing the HAL a deadline, and
/// [`Timer::touch`], which the HAL calls back -- both read the clock
/// themselves. That leaves no way in from a test to the one case where `<=`
/// and `<` part company, because the clock has moved on between the caller
/// picking a deadline and the body reading `timer_now()`. With the comparison
/// inside those bodies, both of its boundaries passed green under mutation.
fn has_arrived(deadline: Duration, now: Duration) -> bool {
    deadline <= now
}

impl Timer {
    /// Create a new `Timer`.
    pub fn new() -> Arc<Self> {
        Self::with_slack(Slack::Center)
    }

    /// Create a new `Timer` with slack (monotonic clock).
    pub fn with_slack(slack: Slack) -> Arc<Self> {
        Self::with_slack_clock(slack, 0)
    }

    /// Create a new `Timer` with slack and the create-time `clock_id`.
    pub fn with_slack_clock(slack: Slack, clock_id: u32) -> Arc<Self> {
        Arc::new(Timer {
            base: KObjectBase::default(),
            _counter: CountHelper::new(),
            slack,
            clock_id,
            inner: Mutex::default(),
        })
    }

    /// Create a one-shot timer.
    pub fn one_shot(deadline: Duration) -> Arc<Self> {
        let timer = Timer::new();
        timer.set(deadline, Duration::default());
        timer
    }

    /// Starts a one-shot timer that will fire when `deadline` passes.
    ///
    /// If a previous call to `set` was pending, the previous timer is canceled
    /// and `Signal::SIGNALED` is de-asserted as needed.
    pub fn set(self: &Arc<Self>, deadline: Duration, slack: Duration) {
        // Everything that leaves this object happens with the lock let go:
        // `signal_change` runs the observers' callbacks with its own lock held,
        // and `timer_set` hands the HAL a closure whose body is `touch`, which
        // takes this same lock. Both used to run with `inner` held. These are
        // spin locks taken with interrupts off, so a callback that asks this
        // timer anything -- `cancel`, `get_info`, another `set` -- and a HAL
        // that ever calls a deadline back inline are each a CPU that never
        // comes back, and two of those have already been found in this kernel.
        let already_passed = {
            let mut inner = self.inner.lock();
            if has_arrived(deadline, timer_now()) {
                inner.deadline = None;
                inner.slack = Duration::ZERO;
                true
            } else {
                inner.deadline = Some(deadline);
                inner.slack = slack;
                false
            }
        };
        if already_passed {
            self.base.signal_set(Signal::SIGNALED);
            return;
        }
        self.base.signal_clear(Signal::SIGNALED);
        let me = Arc::downgrade(self);
        kernel_hal::timer::timer_set(
            deadline,
            Box::new(move |now| me.upgrade().map(|timer| timer.touch(now)).unwrap_or(())),
        );
    }

    /// Cancel the pending timer started by `set`.
    pub fn cancel(&self) {
        {
            let mut inner = self.inner.lock();
            inner.deadline = None;
            inner.slack = Duration::ZERO;
        }
        // Outside the lock, as in `set`.
        self.base.signal_clear(Signal::SIGNALED);
    }

    /// Return (options, clock_id, deadline, slack) for `ZX_INFO_TIMER`.
    pub fn get_info(&self) -> (u32, u32, u64, u64) {
        let inner = self.inner.lock();
        (
            self.slack as u32,
            self.clock_id,
            inner
                .deadline
                .map(|value| value.as_nanos() as u64)
                .unwrap_or(0),
            inner.slack.as_nanos() as u64,
        )
    }

    /// Called by HAL timer.
    fn touch(&self, now: Duration) {
        let arrived = {
            let mut inner = self.inner.lock();
            match inner.deadline {
                Some(deadline) if has_arrived(deadline, now) => {
                    inner.deadline = None;
                    inner.slack = Duration::ZERO;
                    true
                }
                _ => false,
            }
        };
        // Outside the lock, as in `set`: this runs from the HAL's timer
        // callback, and the observers it wakes are waiters on this very timer.
        if arrived {
            self.base.signal_set(Signal::SIGNALED);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Two properties, and each needs its own timescale to be asserted without
    //! racing the clock.
    //!
    //! "It has not fired yet" is only safe to assert against a deadline that is
    //! still a long way off, because a host `sleep` takes AT LEAST as long as
    //! asked and, on a loaded machine, a great deal longer: reading a
    //! `sleep(10ms)` as "we are still before the 15 ms deadline" is a guess.
    //! "It fired" is only safe to assert by WAITING for it, because under libos
    //! the callback is an `async_std` task ([`kernel_hal::timer::timer_set`]),
    //! so it runs when the executor gets a core -- and a test binary has dozens
    //! of threads competing for those.
    //!
    //! `set` used to sleep 10 ms and then 15 ms against a 20 ms deadline and
    //! assert both halves off those sleeps. It failed about one run in fifteen
    //! of the suite, and **at `--test-threads=1` too**, so the CI could hit it.
    //! A new test here picks [`FAR`] for the first kind of assertion and
    //! [`wait_signaled`] for the second, rather than a sleep sized to the
    //! deadline.

    use super::*;
    use kernel_hal::timer::timer_now;

    /// A deadline far enough out that "it has not fired yet" can be asserted
    /// with no wait at all: the host would have to stall for two seconds
    /// between two adjacent statements for this to go wrong.
    const FAR: Duration = Duration::from_secs(2);

    /// How long a deadline that has already passed is given to show up as
    /// `SIGNALED` before it counts as never having arrived. Generous on
    /// purpose -- see the module note.
    const SETTLE: Duration = Duration::from_secs(2);

    /// Wait for `timer` to signal, and fail if it never does.
    fn wait_signaled(timer: &Arc<Timer>) {
        let give_up = timer_now() + SETTLE;
        while timer.signal() != Signal::SIGNALED {
            assert!(
                timer_now() < give_up,
                "the deadline passed and the timer never signalled, {:?} later",
                SETTLE
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn one_shot() {
        // Before its deadline: nothing.
        let early = Timer::one_shot(timer_now() + FAR);
        assert_eq!(early.signal(), Signal::empty());

        // Past it: signalled, once the callback gets a turn.
        let fired = Timer::one_shot(timer_now() + Duration::from_millis(5));
        wait_signaled(&fired);
    }

    #[test]
    fn set() {
        let timer = Timer::new();

        // A second `set` cancels the first, so the first deadline goes by
        // without a signal. The sleep can only overshoot, and the deadline now
        // standing is two seconds out, so a signal here could only have come
        // from the deadline that was replaced.
        timer.set(timer_now() + Duration::from_millis(5), Duration::default());
        timer.set(timer_now() + FAR, Duration::default());
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            timer.signal(),
            Signal::empty(),
            "the replaced deadline signalled anyway"
        );

        // A deadline that does arrive signals.
        timer.set(timer_now() + Duration::from_millis(5), Duration::default());
        wait_signaled(&timer);

        // And `set` de-asserts the signal the arrived deadline raised.
        timer.set(timer_now() + FAR, Duration::default());
        assert_eq!(timer.signal(), Signal::empty());
    }

    /// Runs `body` on its own thread and turns a wedge into a named failure: a
    /// re-entrant acquire of a spin lock does not panic, it spins, so without
    /// this the test hangs and says nothing at all.
    fn with_watchdog(what: &str, body: impl FnOnce() + Send + 'static) {
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            body();
            let _ = done.send(());
        });
        assert!(
            finished.recv_timeout(Duration::from_secs(30)).is_ok(),
            "{} wedged or panicked",
            what,
        );
    }

    #[test]
    /// `set` used to raise `SIGNALED` with `inner` held, and `signal_change`
    /// runs the observers' callbacks under its own lock, there and then. A
    /// callback that asks this timer anything takes `inner` again -- on the
    /// same CPU, on a spin lock held with interrupts off, which is a CPU that
    /// never comes back. `get_info` is the cheapest thing to ask it.
    fn a_waiter_woken_by_the_timer_may_ask_the_timer_about_itself() {
        with_watchdog("a callback asking the timer for its info", || {
            let timer = Timer::new();
            let seen = Arc::new(Mutex::new(None));
            let out = seen.clone();
            let me = Arc::downgrade(&timer);
            let watched: Arc<dyn KernelObject> = timer.clone();
            watched.add_signal_callback(Box::new(move |s| {
                if s.contains(Signal::SIGNALED) {
                    if let Some(timer) = me.upgrade() {
                        *out.lock() = Some(timer.get_info());
                    }
                }
                false
            }));

            // A deadline already past signals from inside `set` itself.
            timer.set(timer_now(), Duration::default());
            assert_eq!(timer.signal(), Signal::SIGNALED);
            let info = seen.lock().expect("the callback never ran");
            assert_eq!(info.1, 0, "a timer that has fired holds no deadline");
        });
    }

    /// The one case `<=` and `<` disagree about, which neither `set` nor
    /// `touch` can be steered to from outside: both read the clock themselves.
    #[test]
    fn a_deadline_falling_exactly_on_the_instant_asked_about_has_arrived() {
        let t = Duration::from_secs(9);
        assert!(
            has_arrived(t, t),
            "a deadline of exactly now is in the past, not the future"
        );
        assert!(has_arrived(t, t + Duration::from_nanos(1)));
        assert!(!has_arrived(t, t - Duration::from_nanos(1)));

        // And the far ends, since this is what decides between signalling on
        // the spot and handing the HAL a deadline.
        assert!(has_arrived(Duration::ZERO, Duration::ZERO));
        assert!(!has_arrived(Duration::MAX, Duration::ZERO));
    }

    /// `ZX_INFO_TIMER` fields: creation-time slack **mode**, `clock_id`,
    /// deadline in **nanoseconds**, and the slack of the pending `set`.
    #[test]
    fn the_three_fields_of_the_timer_info_are_the_mode_the_deadline_and_the_slack() {
        // The mode is the one the timer was created with, and it is the raw
        // `zx_timer_slack_t` a process would read back.
        for mode in [Slack::Center, Slack::Early, Slack::Late] {
            assert_eq!(
                Timer::with_slack(mode).get_info().0,
                mode as u32,
                "{:?} did not come back as the creation mode",
                mode
            );
        }
        assert_eq!(
            Timer::new().get_info().0,
            Slack::Center as u32,
            "a timer created without a mode is not centred"
        );
        assert_eq!(
            Timer::with_slack_clock(Slack::Center, 1).get_info().1,
            1,
            "boot clock_id must round-trip through get_info"
        );

        // The deadline and the slack are the two of the pending `set`, both in
        // nanoseconds, and each in its own field.
        let timer = Timer::with_slack(Slack::Late);
        let deadline = timer_now() + FAR;
        let slack = Duration::from_millis(7);
        timer.set(deadline, slack);

        let (mode, clock_id, reported_deadline, reported_slack) = timer.get_info();
        assert_eq!(clock_id, 0, "default create clock is monotonic");
        assert_eq!(mode, Slack::Late as u32);
        assert_eq!(
            reported_deadline,
            deadline.as_nanos() as u64,
            "the deadline came back in the wrong unit"
        );
        assert_eq!(
            reported_slack, 7_000_000,
            "the slack came back in the wrong unit, or is not the slack"
        );

        // `one_shot` asks for no slack at all, which is not the same as asking
        // for the deadline's worth of it. (index 3 = pending slack)
        assert_eq!(Timer::one_shot(timer_now() + FAR).get_info().3, 0);
    }

    /// A timer that has fired holds no deadline any more, and `cancel` leaves
    /// none either: `ZX_INFO_TIMER` reports 0 for "nothing pending", so a
    /// deadline left behind is a timer that reads as still armed.
    #[test]
    fn a_timer_that_is_done_reports_no_deadline_left() {
        let timer = Timer::new();
        // get_info: (mode, clock_id, deadline, slack)
        assert_eq!(timer.get_info().2, 0, "a fresh timer has nothing pending");

        // Cancelled.
        timer.set(timer_now() + FAR, Duration::from_millis(3));
        assert_ne!(timer.get_info().2, 0);
        timer.cancel();
        assert_eq!(timer.get_info().2, 0, "a cancelled deadline is still there");
        assert_eq!(timer.get_info().3, 0, "and its slack with it");

        // Arrived, which is the half that goes through `touch` rather than
        // through `set` or `cancel`.
        timer.set(
            timer_now() + Duration::from_millis(5),
            Duration::from_millis(3),
        );
        wait_signaled(&timer);
        assert_eq!(
            timer.get_info().2,
            0,
            "the deadline survived the firing that consumed it"
        );
        assert_eq!(timer.get_info().3, 0);
    }

    #[test]
    fn cancel() {
        let timer = Timer::new();
        timer.set(timer_now() + Duration::from_millis(10), Duration::default());

        std::thread::sleep(Duration::from_millis(5));
        timer.cancel();

        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(timer.signal(), Signal::empty());
    }
}
