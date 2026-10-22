//! The signal of a POSIX timer that is deleted, disarmed or set again while
//! it waits to be taken, as Linux 6.18 treats it (`stale-survey`, 19 checks,
//! measured there with glibc and musl): it is never delivered. Linux 6.13
//! gave each timer a sequence number that its signal carries, moved on by
//! `timer_delete` and `timer_settime`, and drops a signal with an old number
//! when it comes to be taken (`posixtimer_deliver_signal`).
//!
//! Here it was delivered. A program that disarmed its timeout with the
//! signal blocked still got the `SIGALRM` once it unblocked, a handler ran
//! for a timer that was gone, and a new program inherited the signal of a
//! timer its `execve` had deleted.
//!
//! It is thrown away when the timer is deleted or set, not when it comes to
//! be taken, so `sigpending` stops listing it at once; Linux lists it until
//! something takes it and finds it stale.

use super::*;
use linux_object::process::drop_pending_timer_signal;

/// Throw away the signal `owner`'s timer `id`, sending `notify`, queued and
/// nobody has taken yet. A signal of the same number from anybody else
/// stays.
fn drop_waiting_signal(id: usize, owner: KoID, notify: TimerNotify) {
    if let Some((signal, _)) = posix_timer_expiry_signal(id, notify, 0) {
        drop_pending_timer_signal(
            owner as usize,
            notify.thread.map(|tid| tid as usize),
            signal,
            id as i32,
        );
    }
}

/// [`drop_waiting_signal`] for `owner`'s timer `id`, which `timer_settime`
/// has just set: called before the new setting is armed, so that what it
/// throws away can only be from before.
pub(super) fn drop_signal_from_before(id: usize, owner: KoID) {
    let notify = POSIX_TIMERS
        .lock()
        .get(&id)
        .filter(|t| t.owner == owner)
        .map(|t| t.notify);
    if let Some(notify) = notify {
        drop_waiting_signal(id, owner, notify);
    }
}

/// `timer_delete`: `owner`'s timer `id` deleted, with the signal it had
/// waiting. `EINVAL` if `owner` has no timer `id`.
pub(super) fn delete_posix_timer(id: usize, owner: KoID) -> Result<(), LxError> {
    let removed = {
        let mut timers = POSIX_TIMERS.lock();
        if !timers.get(&id).is_some_and(|t| t.owner == owner) {
            return Err(LxError::EINVAL);
        }
        timers.remove(&id)
    };
    if let Some(timer) = removed {
        drop_waiting_signal(id, owner, timer.notify);
    }
    Ok(())
}

/// `exit_itimers` at an `execve`: `owner`'s timers deleted, and the signals
/// they had waiting with them, which the new program never asked for.
pub fn delete_posix_timers_at_exec(owner: KoID) {
    let mut gone = Vec::new();
    POSIX_TIMERS.lock().retain(|&id, t| {
        let keep = t.owner != owner;
        if !keep {
            gone.push((id, t.notify));
        }
        keep
    });
    for (id, notify) in gone {
        drop_waiting_signal(id, owner, notify);
    }
}

#[cfg(test)]
mod tests;
