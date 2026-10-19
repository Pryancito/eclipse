//! When a dead child becomes something its parent can `wait` for.
//!
//! `Process::exit` publishes `Status::Exited` first, kills the threads and
//! closes the handles, and only then sets `PROCESS_TERMINATED`, whose
//! callback (the one `fork` installs) is this kernel's `do_exit` tail: the
//! children go to their adopter, the record locks and the files go, the
//! layers above forget the pid. Linux makes a task a zombie in
//! `exit_notify`, after all of that, so whoever `wait`s for it finds a world
//! where it has nothing left.
//!
//! A `wait*` here took a child the moment its status said `Exited`, and on
//! another CPU that landed inside the gap. The orphans of the dead child
//! were still the dead child's: a subreaper that reaped the intermediate
//! process of a double fork and then waited for the grandchild was told
//! `ECHILD`, and a process that reaped its child and then volunteered as a
//! subreaper itself (`PR_SET_CHILD_SUBREAPER`) was handed grandchildren that
//! were never going to be its own. `signal-probe`'s `orphan` section failed
//! that way about one run in thirty, on every kernel.
//!
//! So a live `Exited` child is waitable once its teardown says so, which it
//! does right before it tells the reaper.

use super::*;

impl LinuxProcess {
    /// Said by the exit teardown once this process's children, record locks
    /// and files are gone: a `wait*` may take it from here.
    pub(super) fn mark_exit_handed_over(&self) {
        self.exit_handed_over.store(true, Ordering::Release);
    }
}

/// The exit code a `wait*` may collect from `child` now: it has exited AND
/// its teardown has handed over what it had. A process with no Linux side
/// has no teardown to wait for.
pub(super) fn waitable_exit_code(child: &Arc<Process>) -> Option<i64> {
    match child.status() {
        Status::Exited(code)
            if child
                .try_linux()
                .is_none_or(|lp| lp.exit_handed_over.load(Ordering::Acquire)) =>
        {
            Some(code)
        }
        _ => None,
    }
}
