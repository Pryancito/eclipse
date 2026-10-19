//! A dead child wears its pid until it is reaped, as in Linux: the pid
//! goes back to the pool when the `Process` object drops, so the zombie
//! record has to keep that object.
//!
//! Without it the pid was free the moment the death was recorded, and the
//! next fork could be handed it while the zombie still waited: a
//! `waitpid` for the new child collected the old one's status, and the
//! new child's own death went unseen. `signal-probe`'s `orphan` section
//! saw it as an orphan that was "already dead" right after its birth.

extern crate std;

use super::*;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

/// A parent and the pid of a child that died and has not been waited for,
/// with a weak pointer to the child object: as long as it upgrades, the pid
/// is still worn.
fn a_parent_with_a_zombie(pid: KoID) -> (Arc<Process>, KoID, Weak<Process>) {
    let parent = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let (id, weak) = (child.id(), Arc::downgrade(&child));
    child.exit(0);
    drop(child);
    (parent, id, weak)
}

#[test]
fn a_zombie_keeps_its_pid_until_it_is_waited_for() {
    let (parent, pid, child) = a_parent_with_a_zombie(45_111);

    assert!(
        child.upgrade().is_some(),
        "the pid of a zombie nobody waited for went back to the pool"
    );
    assert_eq!(
        block_on(wait_child(&parent, pid, true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0))
    );
    assert!(child.upgrade().is_none(), "a reaped child is never let go");
}

#[test]
fn so_does_one_a_wait_for_any_child_reaps() {
    let (parent, pid, child) = a_parent_with_a_zombie(45_112);

    assert!(child.upgrade().is_some());
    assert_eq!(
        block_on(wait_child_any(&parent, true, true)).map(|(id, status, _)| (id, status)),
        Ok((pid, wait_status_exited(0)))
    );
    assert!(child.upgrade().is_none(), "a reaped child is never let go");
}

/// `waitid(WNOWAIT)` looks without reaping: the pid stays worn.
#[test]
fn and_a_look_that_does_not_reap_leaves_it_worn() {
    let (parent, pid, child) = a_parent_with_a_zombie(45_113);

    assert!(block_on(wait_child(&parent, pid, true, false)).is_ok());
    assert!(child.upgrade().is_some(), "WNOWAIT let the zombie's pid go");
}

/// A parent that dies with a zombie hands it to the adopter, pid and all.
#[test]
fn an_adopted_zombie_still_wears_it() {
    let reaper = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 45_114).unwrap();
    reaper.linux().set_child_subreaper(true);
    let parent = Process::fork_from(&reaper).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let (pid, weak) = (child.id(), Arc::downgrade(&child));
    child.exit(0);
    drop(child);
    parent.exit(0);

    assert!(
        weak.upgrade().is_some(),
        "the adoption let the zombie's pid go"
    );
    assert_eq!(
        block_on(wait_child(&reaper, pid, true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0))
    );
    assert!(weak.upgrade().is_none());
}

/// And a child nobody will wait for is no zombie at all: with SIGCHLD
/// ignored its pid goes back the moment it dies.
#[test]
fn a_child_nobody_will_wait_for_lets_it_go_at_death() {
    let parent = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 45_115).unwrap();
    parent.linux().set_signal_action(
        LinuxSignal::SIGCHLD,
        crate::signal::SignalAction {
            handler: crate::signal::SIG_IGN,
            flags: crate::signal::SignalActionFlags::empty(),
            restorer: 0,
            mask: Sigset::default(),
        },
    );
    let child = Process::fork_from(&parent).unwrap();
    let weak = Arc::downgrade(&child);
    child.exit(0);
    drop(child);

    assert!(weak.upgrade().is_none(), "an autoreaped child kept its pid");
}
