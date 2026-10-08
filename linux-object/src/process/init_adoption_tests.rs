//! INIT (pid 1) as the reaper of last resort, and as the one process the
//! handover must leave alone.
//!
//! **One test, on purpose.** `INIT_PID` is a single fixed pid in a shared
//! `ROOT_JOB`, so only one test in the binary can own it; creating it
//! twice is an error and leaving it alive would make every later orphan in
//! the binary adoptable by it. So the whole question is asked here, in
//! order, and init is exited at the end -- `live_init` then answers `None`
//! for the rest of the run, which is the state every other test was
//! written against.

use super::*;
use core::sync::atomic::AtomicBool;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap()
}

static INIT_DEATH_SEEN: AtomicBool = AtomicBool::new(false);
static INIT_STILL_LIVE_WHILE_DYING: AtomicBool = AtomicBool::new(false);

/// `live_init` asked from inside init's own death, which is the only place
/// its `Status::Exited` check is doing anything: after the exit returns,
/// pid 1 is out of the job and the lookup fails on its own. An init that
/// answered while terminating would be handed orphans it can never reap.
fn look_at_the_dying_init(pid: KoID) {
    if pid != INIT_PID {
        return;
    }
    INIT_STILL_LIVE_WHILE_DYING.store(live_init().is_some(), Ordering::SeqCst);
    INIT_DEATH_SEEN.store(true, Ordering::SeqCst);
}

#[test]
fn init_reaps_what_nobody_else_will_and_hands_over_nothing_itself() {
    register_process_exit_hook(look_at_the_dying_init);
    let init = a_process(INIT_PID);
    assert_eq!(init.id(), 1, "the pid every orphan falls back to");
    assert_eq!(
        live_init().map(|p| p.id()),
        Some(INIT_PID),
        "init is not being seen while it runs"
    );

    // 1. A dead parent with nobody volunteering resolves to live init.
    //    This is the fallback `reaper_for` exists for, and the only
    //    position from which it is reachable.
    let lonely = a_process(46_201);
    let orphan = Process::fork_from(&lonely).unwrap();
    assert!(nearest_live_subreaper(&lonely).is_none(), "no volunteer");
    lonely.exit(0);
    assert_eq!(
        reaper_for(&lonely).map(|p| p.id()),
        Some(INIT_PID),
        "a dead parent with no subreaper was left with no reaper at all"
    );

    // 2. And the handover that ran inside that exit put the orphan on
    //    init, parent link included.
    assert_eq!(
        orphan.linux().parent().map(|p| p.id()),
        Some(INIT_PID),
        "the orphan still names the process that died"
    );
    assert!(
        init.linux()
            .inner
            .lock()
            .children
            .contains_key(&orphan.id()),
        "init does not hold the orphan it adopted"
    );

    // 3. A volunteer closer than init wins even with init alive, so the
    //    fallback is a fallback and not the only answer.
    let volunteer = a_process(46_202);
    volunteer.linux().set_child_subreaper(true);
    let parent = Process::fork_from(&volunteer).unwrap();
    let kept = Process::fork_from(&parent).unwrap();
    parent.exit(0);
    assert_eq!(
        kept.linux().parent().map(|p| p.id()),
        Some(volunteer.id()),
        "init took a child the volunteer had asked for"
    );

    // 4. Init itself hands over nothing. `set_parent` gives init an
    //    adopter that the walk could reach, which is the only way to tell
    //    the pid-1 guard from its absence: without it, `live_init` would
    //    make init adopt from itself, and with a reachable volunteer above
    //    it the children would leave altogether.
    let its_own = Process::fork_from(&init).unwrap();
    init.linux().set_parent(&volunteer);
    assert_eq!(
        nearest_live_subreaper(&init).map(|p| p.id()),
        Some(volunteer.id()),
        "the adopter this step needs is not reachable"
    );

    reparent_live_children_to_init(&init);

    assert!(
        init.linux()
            .inner
            .lock()
            .children
            .contains_key(&its_own.id()),
        "init handed its own children away"
    );
    assert_eq!(
        its_own.linux().parent().map(|p| p.id()),
        Some(INIT_PID),
        "init's child was given another parent"
    );

    // 5. Clean up, and on the way out the last question: with init exited,
    //    `live_init` is `None` again -- which is both what every other test
    //    in this binary was written against and, asked from inside the
    //    death itself, the only place that answer is not free.
    init.exit(0);
    assert!(
        INIT_DEATH_SEEN.load(Ordering::SeqCst),
        "the hook never saw init die"
    );
    assert!(
        !INIT_STILL_LIVE_WHILE_DYING.load(Ordering::SeqCst),
        "a terminating init still offered itself as the reaper of last resort"
    );
    assert!(live_init().is_none(), "init outlived its own test");
}
