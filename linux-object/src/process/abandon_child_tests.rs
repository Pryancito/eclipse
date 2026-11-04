//! Taking back a child that was filed and then could not be finished.
//!
//! `fork_from` files the new child and stamps it `Running`; the pieces of a
//! `clone` that can still fail come after that (`CLONE_SETTLS` reads a
//! `struct user_desc` out of user memory), and a `clone` that answers an
//! error must leave NOTHING behind: no live child, and no zombie either.
//!
//! The two have to be undone together, and in this order. The caller exits
//! the child right after the undo --- that is what releases its address
//! space and takes it out of its job --- and that exit runs the termination
//! callback, which files a zombie. So `abandon_child` leaves the mark
//! `record_child_exit` looks for, which is the one line that separates it
//! from [`LinuxProcess::forget_child`].

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_parent(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "abandon",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

/// **An abandoned child is gone from both tables, across its own exit.**
///
/// The live table first, which is what a `wait` asks before it answers
/// `ECHILD`; and then no zombie, although the exit that follows is a real
/// one and takes the ordinary notification path.
#[test]
fn an_abandoned_child_leaves_neither_a_live_child_nor_a_zombie() {
    let parent = a_parent(47_230);
    let child = Process::fork_from(&parent).unwrap();
    assert!(
        parent.linux().has_child(child.id()),
        "fork_from stopped filing the child, which is the premise of all this"
    );
    parent.linux().abandon_child(child.id());
    assert!(
        !parent.linux().has_child(child.id()),
        "the child is still in the live table: every later wait answers 0"
    );
    child.exit(0);
    assert!(
        !parent.linux().is_zombie_child(child.id()),
        "the exit filed a zombie for a child the clone reported as never born"
    );
}

/// **And [`LinuxProcess::forget_child`] would not have done**: it is the
/// other direction, for a child that has already died.
///
/// This is the mutation guard for the one line that differs. Forget the child
/// first and let it die afterwards and the zombie comes back, because the
/// termination callback has no reason not to file one. The test asserts the
/// behaviour of the call that is NOT for this, so that deleting the mark from
/// `abandon_child` cannot pass as "the two calls do the same thing".
#[test]
fn forgetting_a_child_that_has_not_died_yet_still_leaves_a_zombie() {
    let parent = a_parent(47_231);
    let child = Process::fork_from(&parent).unwrap();
    parent.linux().forget_child(child.id());
    assert!(!parent.linux().has_child(child.id()));
    child.exit(0);
    assert!(
        parent.linux().is_zombie_child(child.id()),
        "forget_child kept the exit from filing a zombie, so the mark in \
         abandon_child is dead code and this whole distinction is gone"
    );
}

/// **The mark is consumed by the child's own death, not left behind.**
///
/// This is the order, and it is the opposite of the obvious one. The teardown
/// is what RUNS the termination callback --- `remove_thread` on the last
/// thread sees an empty list and calls `terminate()` --- so a mark left for
/// after it arrives too late: the zombie is already filed, and the mark stays
/// under a pid this kernel recycles, where it swallows the zombie of the next
/// real child to wear it. (Pointed out by Copilot on the #2428.)
#[test]
fn the_mark_is_consumed_by_the_childs_death_and_not_left_behind() {
    let parent = a_parent(47_232);
    let child = Process::fork_from(&parent).unwrap();
    let thread = Thread::create_linux(&child).unwrap();
    parent.linux().abandon_child(child.id());
    // What `abandon_unstarted_child` does third: its one thread leaving is
    // the teardown, and the callback runs inside it.
    thread.terminate_abandoned();
    assert!(
        !parent.linux().is_zombie_child(child.id()),
        "the callback filed a zombie for a child that was never born"
    );
    assert_eq!(
        parent.linux().pending_zombie_suppressions(),
        0,
        "the mark outlived the death it was for: the next child to wear this \
         pid would die unreapable"
    );
}

/// **And in the wrong order it is exactly what happens**, which is why the
/// one above is not decoration.
///
/// Tear the child down first and the callback files its zombie with no mark
/// to stop it; the `abandon_child` that follows cleans the zombie up, so the
/// visible table looks right --- and the mark is left behind for a pid that
/// no longer has anything to do with it.
#[test]
fn abandoning_after_the_teardown_leaves_the_mark_behind() {
    let parent = a_parent(47_233);
    let child = Process::fork_from(&parent).unwrap();
    let thread = Thread::create_linux(&child).unwrap();
    thread.terminate_abandoned();
    parent.linux().abandon_child(child.id());
    assert!(!parent.linux().is_zombie_child(child.id()));
    assert_eq!(
        parent.linux().pending_zombie_suppressions(),
        1,
        "the stale mark stopped being observable, so the test above cannot \
         tell the two orders apart any more"
    );
}
