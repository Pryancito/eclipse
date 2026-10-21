//! A `CLONE_PARENT` child is its creator's parent's child: in that parent's
//! table and not the creator's, naming it as its parent, and dying with the
//! creator's exit signal to it.

use super::*;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap()
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

fn ppid_of(proc: &Arc<Process>) -> Option<KoID> {
    proc.linux().parent().map(|p| p.id())
}

#[test]
fn the_child_is_the_callers_parents_and_not_the_callers() {
    let top = a_process(44_401);
    let caller = Process::fork_from(&top).unwrap();
    let child = Process::fork_from(&caller).unwrap();
    give_to_callers_parent(&caller, &child);
    assert_eq!(
        ppid_of(&child),
        Some(top.id()),
        "getppid() names the caller"
    );
    assert!(
        holds_child(&top, child.id()),
        "its parent cannot wait for it"
    );
    assert!(
        !holds_child(&caller, child.id()),
        "the caller can still wait for it"
    );
}

/// `p->exit_signal = current->group_leader->exit_signal`, whatever the
/// `clone` asked for.
#[test]
fn it_dies_with_the_callers_exit_signal() {
    let top = a_process(44_402);
    let caller = Process::fork_from(&top).unwrap();
    caller.linux().set_exit_signal(LinuxSignal::SIGUSR2 as u8);
    let child = Process::fork_from(&caller).unwrap();
    child.linux().set_exit_signal(LinuxSignal::SIGUSR1 as u8);
    give_to_callers_parent(&caller, &child);
    assert_eq!(child.linux().exit_signal(), LinuxSignal::SIGUSR2 as u8);
}

/// Its death is the parent's to collect: the zombie is filed there.
#[test]
fn its_parent_collects_its_death() {
    let top = a_process(44_403);
    let caller = Process::fork_from(&top).unwrap();
    let child = Process::fork_from(&caller).unwrap();
    give_to_callers_parent(&caller, &child);
    child.exit(7);
    assert!(
        holds_zombie(&top, child.id()),
        "the zombie is not its parent's"
    );
    assert!(
        !holds_zombie(&caller, child.id()),
        "the caller got the zombie"
    );
}

/// The parent the caller has now: after an adoption, the adopter.
#[test]
fn a_caller_that_was_adopted_gives_it_to_the_adopter() {
    let top = a_process(44_404);
    top.linux().set_child_subreaper(true);
    let mid = Process::fork_from(&top).unwrap();
    let caller = Process::fork_from(&mid).unwrap();
    mid.exit(0);
    assert_eq!(
        ppid_of(&caller),
        Some(top.id()),
        "the test needs the adoption"
    );
    let child = Process::fork_from(&caller).unwrap();
    give_to_callers_parent(&caller, &child);
    assert_eq!(ppid_of(&child), Some(top.id()));
    assert!(holds_child(&top, child.id()));
    assert!(!holds_child(&mid, child.id()), "filed under a dead process");
}

/// A parent that died before the adoption reached the caller: the child
/// goes where an orphan of it would, and not into the dead one's table.
#[test]
fn a_callers_parent_that_is_dead_is_taken_for_its_reaper() {
    let top = a_process(44_405);
    top.linux().set_child_subreaper(true);
    let dead = a_process(44_406);
    dead.linux().set_parent(&top);
    let caller = Process::fork_from(&top).unwrap();
    caller.linux().set_parent(&dead);
    dead.exit(0);
    let child = Process::fork_from(&caller).unwrap();
    give_to_callers_parent(&caller, &child);
    assert!(
        !holds_child(&dead, child.id()),
        "filed under a dead process"
    );
    assert_eq!(ppid_of(&child), Some(top.id()));
    assert!(holds_child(&top, child.id()));
}
