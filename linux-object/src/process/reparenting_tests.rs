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
        .record_child_exit(46_777, 9, Default::default(), 0);
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

/// Whoever reaps a child is its parent when it dies, and after an adoption
/// that is the adopter. The death notification worked it out again from
/// the parent the child was FORKED from, walking that corpse's ancestors
/// for a volunteer, and the walk only agrees with the adoption while
/// nobody's `PR_SET_CHILD_SUBREAPER` has changed in between.
///
/// Here a process between the dead parent and the adopter volunteers after
/// the adoption. The walk stops at it, so it was the one told: it got a
/// zombie of a process that was never its child (its `wait(-1)` hands
/// back a stranger's pid), and the real parent got no SIGCHLD, so a `wait`
/// it is blocked in sleeps on with its child dead.
#[test]
fn an_adopted_orphan_dies_into_its_adopter_after_a_later_volunteer() {
    let top = a_process(46_020);
    top.linux().set_child_subreaper(true);
    let mid = a_child_of(&top);
    let parent = a_child_of(&mid);
    let orphan = a_child_of(&parent);
    parent.exit(0);
    assert_eq!(ppid_of(&orphan), Some(top.id()), "adopted by the volunteer");

    mid.linux().set_child_subreaper(true);
    top.signal_clear(Signal::SIGCHLD);
    orphan.exit(7);

    assert!(
        holds_zombie(&top, orphan.id()),
        "the adopter, its parent, holds no exit status for it"
    );
    assert!(
        !holds_child(&top, orphan.id()),
        "the adopter still holds the dead child as a live one"
    );
    assert!(
        !holds_zombie(&mid, orphan.id()),
        "a process that never had it as a child was handed its zombie"
    );
    assert!(
        top.signal().contains(Signal::SIGCHLD),
        "the adopter was not woken, so a wait it is blocked in sleeps on"
    );
}

/// The other direction: the adopter withdraws after the adoption. The walk
/// from the dead parent now passes it by and climbs to a volunteer further
/// up, but the orphan is still the adopter's child. Linux tells the
/// parent whatever its flag says now: the flag decides who ADOPTS an
/// orphan, not who is told of a child's death.
#[test]
fn an_adopted_orphan_dies_into_its_adopter_after_it_withdraws() {
    let top = a_process(46_021);
    top.linux().set_child_subreaper(true);
    let mid = a_child_of(&top);
    mid.linux().set_child_subreaper(true);
    let parent = a_child_of(&mid);
    let orphan = a_child_of(&parent);
    parent.exit(0);
    assert_eq!(ppid_of(&orphan), Some(mid.id()), "adopted by the nearest");

    mid.linux().set_child_subreaper(false);
    mid.signal_clear(Signal::SIGCHLD);
    orphan.exit(7);

    assert!(
        holds_zombie(&mid, orphan.id()),
        "the adopter, its parent, holds no exit status for it"
    );
    assert!(
        !holds_zombie(&top, orphan.id()),
        "a process that never had it as a child was handed its zombie"
    );
    assert!(
        mid.signal().contains(Signal::SIGCHLD),
        "the adopter was not woken, so a wait it is blocked in sleeps on"
    );
}

/// And a child nobody adopted still dies into the parent it was forked
/// from: reading the parent pointer must not change the plain case.
#[test]
fn a_child_that_was_never_adopted_dies_into_its_own_parent() {
    let top = a_process(46_022);
    top.linux().set_child_subreaper(true);
    let parent = a_child_of(&top);
    let child = a_child_of(&parent);
    parent.signal_clear(Signal::SIGCHLD);

    child.exit(3);

    assert!(holds_zombie(&parent, child.id()));
    assert!(!holds_zombie(&top, child.id()));
    assert!(parent.signal().contains(Signal::SIGCHLD));
}

/// A child forked while its parent was dying was never filed under anyone
/// (see the test at the top), and its parent pointer names the corpse. Its
/// death has to walk past that corpse to the volunteer above it, or the
/// exit status lands on a process that will never `wait` again.
#[test]
fn a_child_forked_from_a_dead_parent_dies_into_the_subreaper_above_it() {
    let top = a_process(46_023);
    top.linux().set_child_subreaper(true);
    let dying = a_child_of(&top);
    dying.exit(0);
    let child = Process::fork_from(&dying).unwrap();
    assert_eq!(ppid_of(&child), Some(dying.id()), "nobody adopted it");

    child.exit(5);

    assert!(
        holds_zombie(&top, child.id()),
        "the exit status went to the dead parent, where no wait will look"
    );
    assert!(!holds_zombie(&dying, child.id()));
}

/// `clone(2)` lets a parent pick the signal its child's death sends it, and
/// an unprivileged one may pick any: it is only ever sent to itself. An
/// adoption hands the child to a process that never chose it, so Linux
/// puts it back to SIGCHLD there (`reparent_leader`: "We don't want people
/// slaying init"). It was kept, so a child cloned with SIGUSR1 or SIGTERM
/// whose parent then exits delivered that signal to init when it died --
/// which eclipse-init reads as halt and reboot -- with no permission check
/// at all, since a death notification is the kernel's own send.
#[test]
fn an_adopted_orphan_dies_with_sigchld_whatever_its_parent_chose() {
    let top = a_process(46_024);
    top.linux().set_child_subreaper(true);
    let top_thread = Thread::create_linux(&top).unwrap();
    let parent = a_child_of(&top);
    let orphan = a_child_of(&parent);
    orphan.linux().set_exit_signal(LinuxSignal::SIGUSR1 as u8);
    parent.exit(0);
    assert_eq!(ppid_of(&orphan), Some(top.id()), "adopted by the volunteer");
    assert_eq!(
        orphan.linux().exit_signal(),
        LinuxSignal::SIGCHLD as u8,
        "the adopter inherited the exit signal the dead parent chose"
    );
    top_thread.lock_linux().signals = Sigset::default();

    orphan.exit(0);

    let pending = top_thread.lock_linux().signals;
    assert!(
        !pending.contains(LinuxSignal::SIGUSR1),
        "the adopter was sent the signal the dead parent chose"
    );
    assert!(
        pending.contains(LinuxSignal::SIGCHLD),
        "the adopter was not told its child died"
    );
}

/// The parent that chose the signal still gets it: only an adoption resets
/// the choice.
#[test]
fn a_child_dies_with_the_signal_its_own_parent_chose() {
    let parent = a_process(46_025);
    let parent_thread = Thread::create_linux(&parent).unwrap();
    let child = a_child_of(&parent);
    child.linux().set_exit_signal(LinuxSignal::SIGUSR1 as u8);
    parent_thread.lock_linux().signals = Sigset::default();

    child.exit(0);

    let pending = parent_thread.lock_linux().signals;
    assert!(pending.contains(LinuxSignal::SIGUSR1));
    assert!(!pending.contains(LinuxSignal::SIGCHLD));
}

/// The same signal through the one door the adoption does not cover: a
/// child forked while its parent was dying is filed under nobody, so
/// nothing resets its choice, and its death walks past the corpse to the
/// volunteer above it. That volunteer never chose the signal either.
#[test]
fn a_child_forked_from_a_dead_parent_dies_with_sigchld_whatever_was_chosen() {
    let top = a_process(46_026);
    top.linux().set_child_subreaper(true);
    let top_thread = Thread::create_linux(&top).unwrap();
    let dying = a_child_of(&top);
    dying.exit(0);
    let child = Process::fork_from(&dying).unwrap();
    child.linux().set_exit_signal(LinuxSignal::SIGUSR1 as u8);
    top_thread.lock_linux().signals = Sigset::default();

    child.exit(0);

    let pending = top_thread.lock_linux().signals;
    assert!(
        !pending.contains(LinuxSignal::SIGUSR1),
        "a process that never chose the signal was sent it"
    );
    assert!(
        pending.contains(LinuxSignal::SIGCHLD),
        "the volunteer was not told the child died"
    );
}
