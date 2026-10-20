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

//!
//! And a zombie stops being one when somebody takes it: `release_task()`
//! frees the pid in the same breath, so from then on the pid names nothing.
//! The object here can outlive that -- a `wait*` that finds the exit status
//! on file does not wait for the last thread to finish dying, and the
//! process stays in its job until it has -- so the stage says which of the
//! two a process in the job is. See [`find_task`].

use super::*;
use core::sync::atomic::AtomicU8;

/// How far a process is on its way out, as Linux's pid table sees it: alive,
/// a zombie a `wait*` may collect, or reaped.
#[derive(Debug, Default)]
pub(super) struct ExitStage(AtomicU8);

const ZOMBIE: u8 = 1;
const REAPED: u8 = 2;

impl LinuxProcess {
    /// Said by the exit teardown once this process's children, record locks
    /// and files are gone: a `wait*` may take it from here.
    pub(super) fn mark_exit_handed_over(&self) {
        // `fetch_max`: never back from reaped.
        self.exit_stage.0.fetch_max(ZOMBIE, Ordering::AcqRel);
    }

    /// Whether this process's exit has been collected (or let go of by a
    /// reaper that ignores `SIGCHLD`): its pid names nothing any more.
    pub fn is_reaped(&self) -> bool {
        self.exit_stage.0.load(Ordering::Acquire) == REAPED
    }
}

/// `release_task()` for a dead child its reaper lets go of, however it was
/// kept: from here its pid names nothing, even while the object is still
/// tearing down in its job.
pub(super) fn released(child: Option<Arc<Process>>) -> Option<Arc<Process>> {
    if let Some(lp) = child.as_ref().and_then(|child| child.try_linux()) {
        lp.exit_stage.0.store(REAPED, Ordering::Release);
    }
    child
}

/// [`released`], for a zombie that was on file.
pub(super) fn released_zombie(zombie: Option<ReapedChild>) -> Option<ReapedChild> {
    if let Some(entry) = &zombie {
        released(entry._wears_pid.clone());
    }
    zombie
}

/// The exit code a `wait*` may collect from `child` now: it has exited AND
/// its teardown has handed over what it had. A process with no Linux side
/// has no teardown to wait for.
pub(super) fn waitable_exit_code(child: &Arc<Process>) -> Option<i64> {
    match child.status() {
        Status::Exited(code)
            if child
                .try_linux()
                .is_none_or(|lp| lp.exit_stage.0.load(Ordering::Acquire) >= ZOMBIE) =>
        {
            Some(code)
        }
        _ => None,
    }
}
