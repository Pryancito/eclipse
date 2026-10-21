//! The signal of a POSIX timer that has not been taken yet, found where it
//! waits so that the timer's next expiry can count itself into it, which is
//! how Linux counts `si_overrun`: one signal queued, every expiry while it
//! waits added to it.

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
    use crate::thread::ThreadExt;
    let Some(process) = ROOT_JOB.find_process(pid as KoID) else {
        return false;
    };
    let mut ours = |info: Option<&mut SigInfo>| match info {
        Some(info) if info.timer_id() == Some(timerid) => {
            restamp(info);
            true
        }
        _ => false,
    };
    if let Some(tid) = tid {
        let Some(thread) = process
            .get_child(tid as KoID)
            .ok()
            .and_then(|obj| obj.downcast_arc::<Thread>().ok())
        else {
            return false;
        };
        let mut lt = thread.lock_linux();
        return ours(lt.pending_siginfo_mut(signal));
    }
    if let Some(lp) = process.try_linux() {
        let mut inner = lp.inner.lock();
        if inner.shared_pending.contains(signal)
            && ours(inner.shared_pending_info.get_mut(&(signal as u8)))
        {
            return true;
        }
    }
    process.threads().iter().any(|thread| {
        thread
            .try_lock_linux()
            .is_some_and(|mut lt| ours(lt.pending_siginfo_mut(signal)))
    })
}

#[cfg(test)]
mod tests;
