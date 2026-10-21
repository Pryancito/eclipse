//! `si_overrun` and `timer_getoverrun(2)` as Linux 6.18 counts them
//! (`timer-survey`, 20 checks, measured there with glibc and musl).
//!
//! Linux queues a periodic timer's signal once and counts every expiry that
//! goes by while it waits into it: a 1 ms timer whose signal is blocked for
//! 100 ms is taken once with an `si_overrun` of 99, and `timer_getoverrun`
//! answers 0 until it is taken and 99 after. Here each expiry queued the
//! signal again, the queue kept the first one, and it was taken with an
//! `si_overrun` of 0 while `timer_getoverrun` answered for an expiry nobody
//! had taken. An absolute periodic timer armed in the past counted nothing
//! either: its first expiry was moved up to now, and Linux counts from the
//! one it was given.

use super::*;
use linux_object::process::with_pending_timer_signal;

/// What a POSIX timer has said, and has still to say, about the expiries
/// that went by without a signal of their own.
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub(super) struct Overrun {
    /// `it_overrun_last`: the `si_overrun` of the last signal of this timer
    /// that was taken, which is what `timer_getoverrun(2)` answers. Zero
    /// again at each `timer_settime`.
    pub(super) last: u32,
    /// The `si_overrun` of the signal this timer queued last, which may not
    /// have been taken yet; `None` since `timer_settime` or before the first.
    pub(super) queued: Option<u32>,
    /// Periods of an absolute first expiry that fell before the monotonic
    /// clock began, which `next` cannot go back to (see
    /// [`programmed_expiry`]): counted into the first expiry, once.
    pub(super) before_boot: u32,
}

/// One expiry, as the timer's lock saw it.
#[derive(Debug, Copy, Clone)]
pub(super) struct Expiry {
    /// The periods it stands for besides its own.
    overrun: u32,
    /// [`Overrun::queued`] before it.
    queued: Option<u32>,
}

impl Overrun {
    /// An expiry that came `late` periods after the one it was due at.
    pub(super) fn expiry(&mut self, late: u32) -> Expiry {
        let before_boot = core::mem::take(&mut self.before_boot);
        Expiry {
            overrun: late.saturating_add(before_boot).min(DELAYTIMER_MAX),
            queued: self.queued,
        }
    }
}

/// Tell `owner` about one expiry of timer `id`, armed as generation `gen`.
///
/// A signal of the timer still waiting to be taken takes this expiry into
/// its `si_overrun`: one more period, and the ones the expiry was late by.
/// One queued before the last `timer_settime` starts again from this expiry,
/// as Linux does with a signal of a timer that was re-armed. Otherwise the
/// signal goes, and the one it follows has been taken, so what that one
/// carried is what `timer_getoverrun` answers from now on.
///
/// `SIGEV_NONE` sends nothing, and with nothing taken `timer_getoverrun`
/// stays 0.
pub(super) fn signal_posix_timer_expiry(
    id: usize,
    gen: u64,
    owner: KoID,
    notify: TimerNotify,
    expiry: Expiry,
) {
    let Some((signal, info)) = posix_timer_expiry_signal(id, notify, expiry.overrun) else {
        return;
    };
    let mut carries = expiry.overrun;
    let waiting = with_pending_timer_signal(
        owner as usize,
        notify.thread.map(|tid| tid as usize),
        signal,
        id as i32,
        |info| {
            if expiry.queued.is_some() {
                carries = info
                    .timer_overrun()
                    .saturating_add(1)
                    .saturating_add(expiry.overrun)
                    .min(DELAYTIMER_MAX);
            }
            info.set_timer_overrun(carries);
        },
    );
    if waiting {
        settle(id, gen, |o| o.queued = Some(carries));
        return;
    }
    // Before the new one goes: a `timer_getoverrun` that finds it waiting
    // answers what the one before it carried.
    settle(id, gen, |o| {
        if let Some(taken) = o.queued.take() {
            o.last = taken;
        }
    });
    deliver_timer_signal(owner, notify.thread, signal, info);
    settle(id, gen, |o| o.queued = Some(expiry.overrun));
}

/// Change what timer `id` says about its overruns, if it is still the
/// generation `gen` that expired.
fn settle(id: usize, gen: u64, change: impl FnOnce(&mut Overrun)) {
    if let Some(t) = POSIX_TIMERS
        .lock()
        .get_mut(&id)
        .filter(|t| t.generation == gen)
    {
        change(&mut t.overrun);
    }
}

/// `timer_getoverrun(2)` of `owner`'s timer `id`: what the last signal of it
/// that was taken carried in `si_overrun`, which is the one it queued last
/// once that is no longer waiting. `EINVAL` if `owner` has no timer `id`.
pub(super) fn posix_timer_overrun(id: usize, owner: KoID) -> Result<u32, LxError> {
    let (notify, overrun) = POSIX_TIMERS
        .lock()
        .get(&id)
        .filter(|t| t.owner == owner)
        .map(|t| (t.notify, t.overrun))
        .ok_or(LxError::EINVAL)?;
    let (Some(queued), Some((signal, _))) =
        (overrun.queued, posix_timer_expiry_signal(id, notify, 0))
    else {
        return Ok(overrun.last);
    };
    let waiting = with_pending_timer_signal(
        owner as usize,
        notify.thread.map(|tid| tid as usize),
        signal,
        id as i32,
        |_| {},
    );
    Ok(if waiting { overrun.last } else { queued })
}

/// Where the first expiry of an absolute periodic timer, `init` on `base`'s
/// clock, sits on the monotonic timeline, gone by or not, and the periods to
/// count into it besides the ones from there to its first expiry.
///
/// Linux counts a periodic timer's overruns from the expiry it was given
/// (`hrtimer_forward`), so one armed a second in the past at 10 ms goes off
/// at once with an `si_overrun` of 100. One given an expiry before the
/// monotonic clock began, a `CLOCK_REALTIME` time from before the boot,
/// starts instead at its first expiry after, with the whole periods in
/// between to add. When that one is still to come, the timer still goes off
/// at once for the expiry that is past, one period fewer, and next at that
/// one (see [`next_periodic`]).
pub(super) fn programmed_expiry(
    base: ClockBase,
    init: Duration,
    interval: Duration,
    now: Duration,
    now_wall: Duration,
) -> (Duration, u32) {
    let base_now = match base {
        ClockBase::Monotonic => now,
        ClockBase::Wall => now_wall,
    };
    let Some(late) = base_now.checked_sub(init) else {
        return (timer_arm_deadline(base, true, init, now, now_wall), 0);
    };
    match now.checked_sub(late) {
        Some(expiry) if !expiry.is_zero() => (expiry, 0),
        _ => {
            let before = (late - now).as_nanos();
            let step = interval.as_nanos();
            let periods = before / step + 1;
            let first = periods * step - before;
            let first = Duration::from_nanos(u64::try_from(first).unwrap_or(u64::MAX));
            let periods = if first > now { periods - 1 } else { periods };
            (first, periods.min(DELAYTIMER_MAX as u128) as u32)
        }
    }
}

/// [`forward_periodic`] for a timer due at `next` that went off at `now`,
/// which is before `next` only for the first expiry of one that
/// [`programmed_expiry`] started after the monotonic clock's zero: that one
/// is not moved on, and stands for no period besides its own.
pub(super) fn next_periodic(next: Duration, interval: Duration, now: Duration) -> (Duration, u32) {
    if next > now {
        return (next, 0);
    }
    forward_periodic(next, interval, now)
}

#[cfg(test)]
mod tests;
