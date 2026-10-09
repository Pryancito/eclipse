//! Who reaps an orphan, and what the handover moves with it.
//!
//! `reaper_for`, `nearest_live_subreaper` and
//! `reparent_live_children_to_init` are the whole of this kernel's orphan
//! reparenting -- the `prctl(PR_SET_CHILD_SUBREAPER)` chain a session
//! manager like `tmux` or `systemd --user` relies on, the
//! `prctl(PR_SET_PDEATHSIG)` a child asked for, and the `getppid()` that
//! the daemonize idiom polls until it reads 1. Not one of the three had a
//! test, and the comment inside the handover records that the adopter was
//! once not written into the orphan at all, so `getppid()` kept naming the
//! corpse.

use super::*;
use crate::signal::Signal as LinuxSignal;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

/// A process of its own, with the termination callback installed (it is
/// `create_linux` that installs it, and the callback is what runs the
/// handover).
fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap()
}

/// A forked child, which is how a process acquires a parent link and a
/// termination callback of its own.
fn a_child_of(parent: &Arc<Process>) -> Arc<Process> {
    Process::fork_from(parent).unwrap()
}

fn ppid_of(proc: &Arc<Process>) -> Option<KoID> {
    proc.linux().parent().map(|p| p.id())
}

fn holds_child(proc: &Arc<Process>, child: KoID) -> bool {
    proc.linux().inner.lock().children.contains_key(&child)
}

fn holds_zombie(proc: &Arc<Process>, child: KoID) -> bool {
    proc.linux()
        .inner
        .lock()
        .reaped_children
        .contains_key(&child)
}

/// `fork_from` releases the parent's big lock for the address-space copy
/// and takes it again to insert the child, so the parent can die in
/// between. Its death already drained `children`, and an insert after it
/// would hide the child in a corpse's map: nothing walks a dead process's
/// children again, so no `wait` would ever reach it and its exit status
/// would be collected by nobody.
#[test]
fn a_child_forked_from_a_parent_that_just_died_is_not_stranded_on_it() {
    let dying = a_process(46_301);
    dying.exit(0);
    let child = Process::fork_from(&dying).unwrap();
    assert!(
        !holds_child(&dying, child.id()),
        "the child was filed under a dead parent, where no wait can find it"
    );
}

#[test]
fn a_living_parent_reaps_its_own_child() {
    let parent = a_process(46_001);
    assert_eq!(
        reaper_for(&parent).map(|p| p.id()),
        Some(parent.id()),
        "a live parent is the reaper, with no walk at all"
    );
}

#[test]
fn a_dead_parent_hands_the_child_to_the_nearest_subreaper() {
    let top = a_process(46_002);
    let mid = a_child_of(&top);
    let parent = a_child_of(&mid);
    assert!(
        !top.linux().is_child_subreaper(),
        "nobody starts volunteered"
    );
    top.linux().set_child_subreaper(true);
    assert!(top.linux().is_child_subreaper());
    parent.exit(0);

    assert_eq!(
        reaper_for(&parent).map(|p| p.id()),
        Some(top.id()),
        "the subreaper ancestor reaps, not init"
    );
    assert_eq!(
        nearest_live_subreaper(&parent).map(|p| p.id()),
        Some(top.id()),
        "and the walk finds it past a parent that never volunteered"
    );
}

/// The nearest volunteer wins: `systemd --user` under a `tmux` that also
/// volunteered must keep its own descendants.
#[test]
fn the_nearest_volunteer_wins_over_one_further_up() {
    let top = a_process(46_003);
    let mid = a_child_of(&top);
    let parent = a_child_of(&mid);
    top.linux().set_child_subreaper(true);
    mid.linux().set_child_subreaper(true);
    parent.exit(0);

    assert_eq!(
        nearest_live_subreaper(&parent).map(|p| p.id()),
        Some(mid.id())
    );
}

/// A volunteer that has itself died cannot reap anything, and the walk
/// has to carry on above it instead of stopping there.
///
/// The chain is wired with `set_parent` and not with forks: a fork would
/// put the middle process's children in its own table, and its death
/// would then hand them straight to the top before this walk ever ran.
#[test]
fn a_subreaper_that_has_died_is_skipped_for_the_one_above_it() {
    let top = a_process(46_004);
    let mid = a_process(46_014);
    let child = a_process(46_015);
    mid.linux().set_parent(&top);
    child.linux().set_parent(&mid);
    top.linux().set_child_subreaper(true);
    mid.linux().set_child_subreaper(true);
    assert_eq!(
        nearest_live_subreaper(&child).map(|p| p.id()),
        Some(mid.id()),
        "while it lives, the nearest volunteer is the middle one"
    );

    mid.exit(0);

    assert_eq!(
        nearest_live_subreaper(&child).map(|p| p.id()),
        Some(top.id()),
        "a dead volunteer was taken for a live one"
    );
}

/// `PR_SET_CHILD_SUBREAPER` makes a process the reaper for its
/// *descendants'* orphans, so a process that volunteered is never a
/// candidate for its own children: the walk starts at the parent.
#[test]
fn a_process_that_volunteered_is_not_its_own_subreaper() {
    let top = a_process(46_005);
    let me = a_child_of(&top);
    me.linux().set_child_subreaper(true);
    assert!(me.linux().is_child_subreaper());

    assert_eq!(
        nearest_live_subreaper(&me).map(|p| p.id()),
        None,
        "a live volunteer answered for its own orphans"
    );
    // And the one above it is found, so the walk is not simply broken.
    top.linux().set_child_subreaper(true);
    assert_eq!(nearest_live_subreaper(&me).map(|p| p.id()), Some(top.id()));
}

#[test]
fn with_nobody_volunteering_the_walk_finds_no_subreaper() {
    let top = a_process(46_006);
    let parent = a_child_of(&top);
    parent.exit(0);

    assert!(nearest_live_subreaper(&parent).is_none());
}

/// A parent chain that points back at itself must not cost the teardown
/// path the CPU it is running on. The cap is what makes that true, and
/// `set_parent` is public, so the loop is reachable without corruption.
#[test]
fn a_parent_chain_that_loops_ends_the_walk_instead_of_wedging_it() {
    let a = a_process(46_007);
    let b = a_process(46_008);
    a.linux().set_parent(&b);
    b.linux().set_parent(&a);

    assert!(
        nearest_live_subreaper(&a).is_none(),
        "a cycle with no volunteer in it must end the walk"
    );

    // And a volunteer inside the cycle is still found rather than missed.
    b.linux().set_child_subreaper(true);
    assert_eq!(nearest_live_subreaper(&a).map(|p| p.id()), Some(b.id()));
}

/// The bug the handover's own comment records: the adopter was inserted
/// into the orphan's new parent's table but never written into the orphan,
/// so `getppid()` went on naming the dead process -- which is exactly what
/// the daemonize idiom waits on -- and the walk above climbed a chain
/// through the dead.
#[test]
fn an_adopted_orphan_names_its_adopter_as_its_parent() {
    let top = a_process(46_009);
    top.linux().set_child_subreaper(true);
    let parent = a_child_of(&top);
    let orphan = a_child_of(&parent);
    assert_eq!(ppid_of(&orphan), Some(parent.id()));

    parent.exit(0);

    assert_eq!(
        ppid_of(&orphan),
        Some(top.id()),
        "the orphan still names the process that died"
    );
    assert!(
        holds_child(&top, orphan.id()),
        "the adopter does not hold the orphan it adopted"
    );
    assert!(
        !holds_child(&parent, orphan.id()),
        "the dead process still holds the child it handed over"
    );
}

/// An exit status the dying process collected but never reaped is still
/// owed to somebody: it moves to the adopter so an adopter blocked in
/// `wait(-1)` sees it.
#[test]
fn the_zombies_the_dying_process_never_reaped_move_to_the_adopter() {
    let top = a_process(46_010);
    top.linux().set_child_subreaper(true);
    let parent = a_child_of(&top);
    parent
        .linux()
        .record_child_exit(46_777, 9, Default::default());
    assert!(holds_zombie(&parent, 46_777));

    parent.exit(0);

    assert!(
        holds_zombie(&top, 46_777),
        "an uncollected exit status was dropped on the floor"
    );
    assert!(!holds_zombie(&parent, 46_777));
}

/// `PR_SET_PDEATHSIG`: the child asked to be told when *this* parent dies,
/// and the signal has to go out whoever adopts it afterwards.
#[test]
fn an_orphan_that_asked_for_a_parent_death_signal_gets_it() {
    let top = a_process(46_011);
    top.linux().set_child_subreaper(true);
    let parent = a_child_of(&top);
    let orphan = a_child_of(&parent);
    let thread = Thread::create_linux(&orphan).unwrap();
    orphan.linux().set_pdeathsig(LinuxSignal::SIGUSR1 as u8);
    assert!(!thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1));

    parent.exit(0);

    assert!(
        thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1),
        "the parent died and the child was never told"
    );
}

/// Without the `prctl`, nothing is sent: a signal on every parent death
/// would kill every child of every shell.
#[test]
fn an_orphan_that_asked_for_nothing_is_signalled_with_nothing() {
    let top = a_process(46_012);
    top.linux().set_child_subreaper(true);
    let parent = a_child_of(&top);
    let orphan = a_child_of(&parent);
    let thread = Thread::create_linux(&orphan).unwrap();
    assert_eq!(orphan.linux().pdeathsig(), 0);

    parent.exit(0);

    assert!(
        thread.lock_linux().signals.is_empty(),
        "a child that asked for no parent-death signal was signalled anyway"
    );
}

/// A process whose children have all been reaped already has nothing to
/// hand over, and must not pulse `SIGCHLD` at an adopter that is owed
/// nothing: a spurious wake is a `wait` that returns with no child.
#[test]
fn a_process_with_nothing_to_hand_over_leaves_the_adopter_alone() {
    let top = a_process(46_013);
    top.linux().set_child_subreaper(true);
    let top_thread = Thread::create_linux(&top).unwrap();
    let parent = a_child_of(&top);
    top_thread.lock_linux().signals = Sigset::default();

    reparent_live_children_to_init(&parent);

    assert!(
        !top.signal().contains(Signal::SIGCHLD),
        "the adopter was woken for a handover that moved nothing"
    );
}
