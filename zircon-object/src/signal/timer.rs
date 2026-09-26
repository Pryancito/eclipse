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

impl Timer {
    /// Create a new `Timer`.
    pub fn new() -> Arc<Self> {
        Self::with_slack(Slack::Center)
    }

    /// Create a new `Timer` with slack.
    pub fn with_slack(slack: Slack) -> Arc<Self> {
        Arc::new(Timer {
            base: KObjectBase::default(),
            _counter: CountHelper::new(),
            slack,
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
        let mut inner = self.inner.lock();
        if deadline <= timer_now() {
            inner.deadline = None;
            inner.slack = Duration::ZERO;
            self.base.signal_set(Signal::SIGNALED);
            return;
        }
        inner.deadline = Some(deadline);
        inner.slack = slack;
        self.base.signal_clear(Signal::SIGNALED);
        let me = Arc::downgrade(self);
        kernel_hal::timer::timer_set(
            deadline,
            Box::new(move |now| me.upgrade().map(|timer| timer.touch(now)).unwrap_or(())),
        );
    }

    /// Cancel the pending timer started by `set`.
    pub fn cancel(&self) {
        let mut inner = self.inner.lock();
        inner.deadline = None;
        inner.slack = Duration::ZERO;
        self.base.signal_clear(Signal::SIGNALED);
    }

    /// Return the creation options, next deadline, and slack for ZX_INFO_TIMER.
    pub fn get_info(&self) -> (u32, u64, u64) {
        let inner = self.inner.lock();
        (
            self.slack as u32,
            inner
                .deadline
                .map(|value| value.as_nanos() as u64)
                .unwrap_or(0),
            inner.slack.as_nanos() as u64,
        )
    }

    /// Called by HAL timer.
    fn touch(&self, now: Duration) {
        let mut inner = self.inner.lock();
        if let Some(deadline) = inner.deadline {
            if now >= deadline {
                self.base.signal_set(Signal::SIGNALED);
                inner.deadline = None;
                inner.slack = Duration::ZERO;
            }
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
