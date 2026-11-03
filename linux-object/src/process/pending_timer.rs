//! The signal of a POSIX timer that has not been taken yet, found where it
//! waits so that the timer's next expiry can count itself into it, which is
//! how Linux counts `si_overrun`: one signal queued, every expiry while it
//! waits added to it. And thrown away once the timer has been deleted or
//! set again, which Linux 6.13 and later do with it.

use super::*;

/// Hand `restamp` the `siginfo_t` that POSIX timer `timerid` queued its
/// `signal` to process `pid` with, if it is still waiting to be taken, and
/// say whether it was.
///
/// A `SIGEV_THREAD_ID` timer's waits on its thread `tid`. A process-directed
/// one waits on the process while every thread has it blocked, and on the
/// thread it was queued to otherwise; the process is looked at first, since
/// a signal only ever moves from there onto a thread
/// ([`LinuxProcess::pull_shared_pending_locked`]), so one that moves while
/// this looks is found on the thread.
///
/// A signal of the same number from anybody else is not the timer's, and is
/// left alone.
pub fn with_pending_timer_signal(
    pid: usize,
    tid: Option<usize>,
    signal: LinuxSignal,
    timerid: i32,
    mut restamp: impl FnMut(&mut SigInfo),
) -> bool {
    at_pending_timer_signal(pid, tid, signal, timerid, |info| {
        restamp(info);
        false
    })
}

/// Throw away the signal POSIX timer `timerid` queued its `signal` to
/// process `pid` with, if it is still waiting to be taken, and say whether
/// it was: the timer has been deleted or set again, and Linux never delivers
/// such a signal (`posixtimer_deliver_signal` drops it when it comes to be
/// taken). It is found as [`with_pending_timer_signal`] finds it, and a
/// signal of the same number from anybody else stays.
pub fn drop_pending_timer_signal(
    pid: usize,
    tid: Option<usize>,
    signal: LinuxSignal,
    timerid: i32,
) -> bool {
    at_pending_timer_signal(pid, tid, signal, timerid, |_| true)
}

/// Hand `act` timer `timerid`'s waiting signal, and throw it away when
/// `act` says so. Only the timer's own entry is looked at
/// ([`SigQueue::at_timer`]): a signal of the same number from anybody else,
/// queued before or after it, stays where it is.
fn at_pending_timer_signal(
    pid: usize,
    tid: Option<usize>,
    signal: LinuxSignal,
    timerid: i32,
    mut act: impl FnMut(&mut SigInfo) -> bool,
) -> bool {
    use crate::thread::ThreadExt;
    let Some(process) = ROOT_JOB.find_process(pid as KoID) else {
        return false;
    };
    if let Some(tid) = tid {
        let Some(thread) = process
            .get_child(tid as KoID)
            .ok()
            .and_then(|obj| obj.downcast_arc::<Thread>().ok())
        else {
            return false;
        };
        let found = thread
            .lock_linux()
            .at_pending_timer(signal, timerid, &mut act);
        return found;
    }
    if let Some(lp) = process.try_linux() {
        let mut guard = lp.inner.lock();
        let inner = &mut *guard;
        if (inner.shared_queue).at_timer(&mut inner.shared_pending, signal, timerid, &mut act) {
            lp.refresh_exit_path(&guard);
            return true;
        }
    }
    process.threads().iter().any(|thread| {
        thread
            .try_lock_linux()
            .is_some_and(|mut lt| lt.at_pending_timer(signal, timerid, &mut act))
    })
}

#[cfg(test)]
mod tests;
