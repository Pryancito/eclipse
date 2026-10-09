//! A process that has been terminated but is still in its job.
//!
//! `Process::exit` fires `PROCESS_TERMINATED`, runs every callback and
//! only then takes the process out of its job, so for the whole of that
//! window `Job::find_process` still answers with it. Everything that asks
//! "who is still there?" during a teardown -- a group signal fanning out,
//! `send_signal_to_process`, the `wait` bookkeeping -- runs inside that
//! window, which is why `all_live_processes` and `process_exists` check
//! the status as well as the lookup. A test that asks after the exit has
//! returned cannot see any of it: by then the lookup fails on its own and
//! the status check is doing nothing.

use super::*;
use core::sync::atomic::AtomicBool;
use rcore_fs_ramfs::RamFS;

/// The pid the hook below is watching, so every other process's death
/// costs the hook one comparison and nothing else.
const DYING_PID: KoID = 46_301;

static HOOK_RAN: AtomicBool = AtomicBool::new(false);
static LISTED_AS_LIVE: AtomicBool = AtomicBool::new(false);
static SAID_TO_EXIST: AtomicBool = AtomicBool::new(false);

/// Asked from inside the death, where the lookup still succeeds.
fn look_at_the_dying(pid: KoID) {
    if pid != DYING_PID {
        return;
    }
    LISTED_AS_LIVE.store(
        all_live_processes().iter().any(|p| p.id() == pid),
        Ordering::SeqCst,
    );
    SAID_TO_EXIST.store(process_exists(pid), Ordering::SeqCst);
    HOOK_RAN.store(true, Ordering::SeqCst);
}

#[test]
fn a_terminated_process_is_already_neither_live_nor_existing() {
    register_process_exit_hook(look_at_the_dying);
    let proc = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, DYING_PID).unwrap();
    assert!(
        ROOT_JOB.find_process(DYING_PID).is_some(),
        "the lookup this test turns on"
    );
    assert!(process_exists(DYING_PID), "and it exists while it runs");

    proc.exit(0);

    assert!(HOOK_RAN.load(Ordering::SeqCst), "the hook never ran");
    assert!(
        !LISTED_AS_LIVE.load(Ordering::SeqCst),
        "a terminated process was still counted among the living, so a \
         group signal would have been sent to a corpse"
    );
    assert!(
        !SAID_TO_EXIST.load(Ordering::SeqCst),
        "a terminated process still said it exists"
    );
}
