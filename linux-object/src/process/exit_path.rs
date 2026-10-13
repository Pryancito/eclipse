//! The two answers the return to user mode needs, readable without a lock.
//!
//! Lives in its own file because [`super::LinuxProcess`] sits at its
//! ceiling in `xtask tamano`, and because the invariant below is the whole
//! of the change: it reads better next to the thing it governs than buried
//! in a six-thousand-line file.

use super::*;
use core::sync::atomic::AtomicBool;
use kernel_hal::sync::MutexGuard;

/// The answers the return to user mode needs, readable without a lock.
///
/// Every syscall ends in the loop in `loader/src/linux.rs::run_user`, which
/// before re-entering user mode asks the process two questions: are there
/// process-directed signals waiting to be pulled onto this thread, and is
/// the process job-control stopped? Both answers used to come out of
/// [`LinuxProcessInner`] -- the big lock, the one that also guards the
/// file-descriptor table, the credentials and the working directory -- so
/// **every thread of the process took it twice per syscall return** to read
/// two words that are zero on all but a handful of returns in a program's
/// life.
///
/// Measured on the host (`process::exit_path_benches`), the three-question
/// prologue of a return:
///
/// | other threads of the process doing ordinary work | ns |
/// |---|---|
/// | none | 34.2 |
/// | one | 232 |
/// | two | 1445 |
/// | three | 2390 |
///
/// The contended rows are saturation points on a shared four-core container,
/// not predictions; what they establish is that the cost is **the lock**,
/// since nothing else on that path grows with the number of threads. A
/// browser has dozens of threads and all of them return from syscalls.
///
/// Reading the two words from here instead flattens that column to 10.8 /
/// 11.1 / 11.1 / 11.2 ns, and what is left of it is the thread's **own**
/// lock in `handle_signal`, which the threads of a process do not share.
///
/// So the two words are mirrored here, outside the mutex. The invariant is
/// the one that makes a mirror safe, and it is deliberately the whole of it:
///
/// > every mutator of [`LinuxProcessInner::shared_pending`] or
/// > [`LinuxProcessInner::job_stopped`] calls
/// > [`LinuxProcess::refresh_exit_path`] **while still holding the lock**.
///
/// Refreshing both words from the one source of truth, rather than tracking
/// each at its own call site, is what keeps that invariant checkable: a
/// mutator either refreshes or it does not, and there is no third state in
/// which the mirror holds one stale word and one fresh one.
///
/// A reader without the lock can see a value that is one store old; it
/// cannot see one that was never true. That window is the same one Linux
/// has with `TIF_SIGPENDING` (the sender sets the flag after queueing, and
/// the target notices on its next trip through the kernel boundary), and it
/// is what the old code had too: the check happens at the next return to
/// user mode either way, so the only thing that changed is where it reads
/// from.
pub(super) struct ExitPathSummary {
    /// `shared_pending.val()`: nonzero means "take the lock and look".
    pub(super) shared_pending_bits: AtomicU64,
    /// `job_stopped`: a stopped process must not be re-entered.
    ///
    /// An `AtomicBool`, which is what the field is. The first version of
    /// this said "a byte, because there is no `AtomicBool` in the imports
    /// here" -- which was not a reason, it was a missing `use`, and Copilot
    /// said so.
    pub(super) job_stopped: AtomicBool,
}

impl Default for ExitPathSummary {
    fn default() -> Self {
        Self {
            shared_pending_bits: AtomicU64::new(0),
            job_stopped: AtomicBool::new(false),
        }
    }
}

impl LinuxProcess {
    /// Re-publishes the two words the return to user mode reads.
    ///
    /// Call it from every mutator of `shared_pending` or `job_stopped`.
    ///
    /// It takes the **guard**, not the data behind it, so "while still
    /// holding the lock" is not a sentence a caller has to read: a caller
    /// that has released the lock has nothing to pass. The first version
    /// took `&LinuxProcessInner`, where a deref coercion made the guard fit
    /// and the invariant was a comment; Copilot pointed that out.
    ///
    /// The stores happen under the lock so two concurrent mutators cannot
    /// leave the mirror disagreeing with the field. See [`ExitPathSummary`]
    /// for the invariant and why both words are refreshed together.
    pub(super) fn refresh_exit_path(&self, inner: &MutexGuard<'_, LinuxProcessInner>) {
        self.exit_path
            .shared_pending_bits
            .store(inner.shared_pending.val(), Ordering::Release);
        self.exit_path
            .job_stopped
            .store(inner.job_stopped, Ordering::Release);
    }

    /// True while this process is job-control stopped.
    ///
    /// Read from the lock-free mirror: this is on the return path of every
    /// syscall. See [`ExitPathSummary`].
    pub fn is_job_stopped(&self) -> bool {
        self.exit_path.job_stopped.load(Ordering::Acquire)
    }
}
