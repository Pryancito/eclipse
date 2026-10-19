//! What a `vfork` child runs in: its parent's address space, where `fork`
//! gives it a copy. glibc's `posix_spawn` and busybox's `spawn()` learn that
//! the child's `exec` failed from a word the child writes there, and with a
//! copy `posix_spawn("/no/such/file")` answered 0.

use super::*;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn linux_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "parent", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

#[test]
fn a_vfork_child_runs_in_its_parents_address_space() {
    let parent = linux_process(48_401);
    let child = Process::vfork_from(&parent).unwrap();
    assert!(Arc::ptr_eq(&child.vmar(), &parent.vmar()));
}

#[test]
fn a_fork_child_in_a_copy_of_it() {
    let parent = linux_process(48_403);
    let child = Process::fork_from(&parent).unwrap();
    assert!(!Arc::ptr_eq(&child.vmar(), &parent.vmar()));
}

/// The rest of what makes it a child is the same as for a `fork`.
#[test]
fn and_is_a_child_like_any_other() {
    let parent = linux_process(48_405);
    let child = Process::vfork_from(&parent).unwrap();
    assert!(parent
        .linux()
        .inner
        .lock()
        .children
        .contains_key(&child.id()));
    assert!(Arc::ptr_eq(&child.linux().parent().unwrap(), &parent));
    assert_eq!(effective_pgid(&child), effective_pgid(&parent));
}

/// A `vfork` child that forks before its `exec` -- vfork(2) leaves that
/// undefined, but nothing stops it -- is one thread in memory its parent's
/// other threads run in, so its fork may not batch the shootdowns.
#[test]
fn a_fork_from_a_vfork_child_is_not_alone_in_its_memory() {
    let parent = linux_process(48_407);
    Thread::create_linux(&parent).unwrap();
    assert!(alone_in_its_address_space(&parent));

    let child = Process::vfork_from(&parent).unwrap();
    Thread::create_linux(&child).unwrap();
    assert!(!alone_in_its_address_space(&child));
    assert!(!alone_in_its_address_space(&parent));

    // Its `execve` leaves the parent alone in it again.
    child.own_address_space();
    assert!(alone_in_its_address_space(&parent));
}

#[test]
fn nor_is_a_process_with_two_threads() {
    let parent = linux_process(48_409);
    Thread::create_linux(&parent).unwrap();
    Thread::create_linux(&parent).unwrap();
    assert!(!alone_in_its_address_space(&parent));
}
