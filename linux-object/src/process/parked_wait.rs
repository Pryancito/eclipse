//! The wait a thread is told of a signal by: its waker is left on the
//! thread ([`LinuxThreadExt::park_waiter`](crate::thread::LinuxThreadExt))
//! and woken when a signal is queued, so the wait needs no timer to look
//! for one.

use super::Thread;
use crate::error::LxResult;

/// Wake the wait that parked its waker on `thread` to be told of a signal
/// ([`LinuxThreadExt::park_waiter`](crate::thread::LinuxThreadExt)).
pub(super) fn wake_parked_wait(thread: &Thread) {
    use crate::thread::ThreadExt;
    if let Some(ext) = thread.linux_ext() {
        ext.wake_waiter();
    }
}

/// [`check_signals`] for a wait that is about to sleep on something else,
/// with `waker` left on the calling thread first so that a signal queued
/// from here on wakes it: the answer of the check, and whether the waker
/// was taken. A wait whose waker was not taken -- no Linux thread here, or
/// another wait of this thread has the place -- has to look again by a
/// timer of its own.
pub fn check_signals_parking(
    wait: &mut Option<u64>,
    waker: &core::task::Waker,
) -> (LxResult<()>, bool) {
    use crate::thread::ThreadExt;
    let thread =
        kernel_hal::thread::get_current_thread().and_then(|arc| arc.downcast::<Thread>().ok());
    match thread {
        Some(thread) => {
            // Parked before the look: see `LinuxThreadExt::park_waiter`.
            let parked = thread
                .linux_ext()
                .is_some_and(|ext| ext.park_waiter(wait, waker));
            (super::check_signals_of(&thread), parked)
        }
        None => (Ok(()), false),
    }
}

/// The wait numbered `wait` is over: take its waker off the calling thread.
/// On another thread, or on none, the number names nothing and nothing is
/// done; the waker goes with the thread it was left on.
pub fn unpark_wait(wait: u64) {
    use crate::thread::ThreadExt;
    if let Some(thread) =
        kernel_hal::thread::get_current_thread().and_then(|arc| arc.downcast::<Thread>().ok())
    {
        if let Some(ext) = thread.linux_ext() {
            ext.unpark_waiter(wait);
        }
    }
}
