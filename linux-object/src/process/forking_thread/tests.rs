//! Fixed pids live in 45_46x here.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::Task;

/// A process with two threads, and a child that the first one forked and
/// that asked for SIGUSR1 when its parent dies.
fn a_family(
    pid: KoID,
) -> (
    Arc<Process>,
    Arc<Thread>,
    Arc<Thread>,
    Arc<Process>,
    Arc<Thread>,
) {
    let parent = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "padre",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let first = Thread::create_linux(&parent).unwrap();
    let second = Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let child_thread = Thread::create_linux(&child).unwrap();
    child.linux().set_forking_thread(first.id());
    child.linux().set_pdeathsig(LinuxSignal::SIGUSR1 as u8);
    (parent, first, second, child, child_thread)
}

fn told(thread: &Thread) -> bool {
    thread.lock_linux().signals.contains(LinuxSignal::SIGUSR1)
}

#[test]
fn the_child_is_told_when_the_thread_that_forked_it_leaves() {
    let (parent, first, second, child, child_thread) = a_family(45_461);
    assert!(!told(&child_thread));
    forget_forking_thread(&parent, first.id());
    assert!(
        told(&child_thread),
        "the forking thread left and the child was never told"
    );
    assert_eq!(child.linux().forking_thread(), second.id());
    parent.exit(0);
}

#[test]
fn another_thread_leaving_tells_the_child_nothing() {
    let (parent, _first, second, child, child_thread) = a_family(45_462);
    forget_forking_thread(&parent, second.id());
    assert!(!told(&child_thread));
    assert_ne!(child.linux().forking_thread(), second.id());
    parent.exit(0);
}

#[test]
fn a_child_that_asked_for_nothing_is_only_handed_on() {
    let (parent, first, second, child, child_thread) = a_family(45_463);
    child.linux().set_pdeathsig(0);
    forget_forking_thread(&parent, first.id());
    assert!(child_thread.lock_linux().signals.is_empty());
    assert_eq!(child.linux().forking_thread(), second.id());
    parent.exit(0);
}

#[test]
fn the_last_thread_leaves_the_children_to_the_process_s_own_exit() {
    let (parent, first, second, child, child_thread) = a_family(45_464);
    second.kill();
    forget_forking_thread(&parent, first.id());
    assert!(
        !told(&child_thread),
        "told twice: here and again by the process's exit"
    );
    assert_eq!(child.linux().forking_thread(), first.id());
    parent.exit(0);
}
