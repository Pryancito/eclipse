//! A `vfork` child runs in its parent's address space (`CLONE_VM`), as it
//! does in Linux, where the two share the `mm`. glibc's `posix_spawn` and
//! busybox's `spawn()` hear about a failed `exec` through a word the child
//! writes there. Neither one's end may unmap it from under the other, and an
//! `execve` in either gives that one an address space of its own.

use super::*;
use crate::vm::{MMUFlags, VmObject, PAGE_SIZE};

const WORD: [u8; 4] = *b"errn";

/// A parent with one page mapped, and a word written in it.
fn parent_with_a_page() -> (Arc<Process>, usize) {
    let parent = Process::create(&Job::root(), "parent").unwrap();
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let at = parent
        .vmar()
        .map_at(0, VmObject::new_paged(1), 0, PAGE_SIZE, flags)
        .unwrap();
    parent.vmar().write_memory(at, &WORD).unwrap();
    (parent, at)
}

fn vforked(parent: &Arc<Process>) -> Arc<Process> {
    Process::create_sharing_address_space(&Job::root(), "child", parent, ()).unwrap()
}

/// What is at `at` in `vmar`, or `None` once nothing is mapped there.
fn word_at(vmar: &VmAddressRegion, at: usize) -> Option<[u8; 4]> {
    let mut buf = [0u8; 4];
    vmar.read_memory(at, &mut buf).ok().map(|_| buf)
}

#[test]
fn a_vforked_child_runs_in_its_parents_address_space() {
    let (parent, at) = parent_with_a_page();
    let child = vforked(&parent);
    assert!(Arc::ptr_eq(&child.vmar(), &parent.vmar()));

    // glibc's `posix_spawn` child writes the `exec`'s errno; its parent reads it.
    child.vmar().write_memory(at, b"ENOE").unwrap();
    assert_eq!(word_at(&parent.vmar(), at), Some(*b"ENOE"));
}

#[test]
fn both_know_they_share_it_until_one_lets_go() {
    let (parent, _) = parent_with_a_page();
    assert!(!parent.shares_address_space());
    let child = vforked(&parent);
    assert!(parent.shares_address_space());
    assert!(child.shares_address_space());

    child.own_address_space();

    assert!(!parent.shares_address_space());
    assert!(!child.shares_address_space());
}

/// `posix_spawn`'s child `_exit`s with 127 when its `exec` fails.
#[test]
fn the_childs_exit_leaves_the_parent_its_memory() {
    let (parent, at) = parent_with_a_page();
    let child = vforked(&parent);

    child.exit(127);

    assert_eq!(word_at(&parent.vmar(), at), Some(WORD));
}

/// A parent killed while it waits for its child's `exec`: the child runs on
/// in the memory, and lets it go with its own end.
#[test]
fn so_does_the_parents_end_while_the_child_runs_in_it() {
    let (parent, at) = parent_with_a_page();
    let child = vforked(&parent);

    parent.exit(0);
    assert_eq!(
        word_at(&child.vmar(), at),
        Some(WORD),
        "unmapped from under the child"
    );

    child.exit(0);
    assert_eq!(word_at(&child.vmar(), at), None, "the last one out unmaps");
}

#[test]
fn an_exec_in_the_child_gives_it_one_of_its_own() {
    let (parent, at) = parent_with_a_page();
    let lent = parent.vmar();
    let child = vforked(&parent);

    let own = child.own_address_space();

    assert!(!Arc::ptr_eq(&own, &lent));
    assert!(Arc::ptr_eq(&child.vmar(), &own));
    assert!(Arc::ptr_eq(&parent.vmar(), &lent));
    assert_eq!(word_at(&own, at), None, "the new image starts from nothing");
    assert_eq!(
        word_at(&lent, at),
        Some(WORD),
        "the parent's program is left mapped"
    );

    // And the parent is again the only one in it: its own end unmaps it.
    parent.exit(0);
    assert_eq!(word_at(&lent, at), None);
}

/// `execve` in another thread of the parent, while the child has not
/// exec'd: the parent gets the new address space, the child keeps the old.
#[test]
fn and_one_in_the_parent_leaves_the_child_the_old_one() {
    let (parent, at) = parent_with_a_page();
    let lent = parent.vmar();
    let child = vforked(&parent);

    let own = parent.own_address_space();

    assert!(!Arc::ptr_eq(&own, &lent));
    assert!(Arc::ptr_eq(&child.vmar(), &lent));
    assert_eq!(word_at(&child.vmar(), at), Some(WORD));
}

/// Nobody else in it: `execve` clears and reloads it in place, as before.
#[test]
fn an_exec_with_nobody_to_share_with_keeps_its_address_space() {
    let (parent, at) = parent_with_a_page();
    let before = parent.vmar();

    let own = parent.own_address_space();

    assert!(Arc::ptr_eq(&own, &before));
    assert_eq!(word_at(&own, at), Some(WORD), "execve clears it itself");
}

/// A child that exits leaves its parent alone in the address space again.
#[test]
fn nor_once_the_child_has_gone() {
    let (parent, _) = parent_with_a_page();
    let before = parent.vmar();
    vforked(&parent).exit(0);

    assert!(Arc::ptr_eq(&parent.own_address_space(), &before));
}

#[test]
fn a_process_that_has_terminated_has_nothing_to_lend() {
    let (parent, _) = parent_with_a_page();
    parent.exit(0);

    assert_eq!(
        Process::create_sharing_address_space(&Job::root(), "child", &parent, ()).err(),
        Some(ZxError::BAD_STATE)
    );
}
