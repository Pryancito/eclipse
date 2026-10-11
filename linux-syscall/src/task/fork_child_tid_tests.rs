use super::*;
use kernel_hal::PAGE_SIZE;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;
use zircon_object::vm::{MMUFlags, VmObject};

/// glibc's `arch_fork`: `CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | SIGCHLD`.
const GLIBC_FORK: usize = 0x0120_0011;
/// The word every fork-time bit will be stored at (the parent's page is
/// seeded with this so the test can tell whose copy got written).
const SENTINEL: i32 = 0x5a5a_5a5a;

fn flags(bits: usize) -> CloneFlags {
    CloneFlags::from_bits_truncate(bits)
}

/// A parent with one thread and one writable page holding a word set to
/// [`SENTINEL`]; returns it with that word's address.
fn a_parent(pid: KoID) -> (Arc<Process>, usize) {
    let parent = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "parent",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let _main = Thread::create_linux(&parent).unwrap();
    // At a fixed, non-zero offset: an auto-placed page can land at 0,
    // which every caller of these functions reads as "no pointer".
    let page = parent
        .vmar()
        .map(
            Some(0x10_0000),
            VmObject::new_paged(1),
            0,
            PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .unwrap();
    assert_ne!(page, 0);
    // Inside the page, not at its start: a tid word is a field of the
    // TCB, and a page-aligned one would let a rounded address pass.
    let word = page + 0x40;
    assert_eq!(
        parent
            .vmar()
            .write_memory(word, &SENTINEL.to_ne_bytes())
            .unwrap(),
        4
    );
    (parent, word)
}

fn word_at(proc: &Arc<Process>, vaddr: usize) -> i32 {
    let mut b = [0u8; 4];
    assert_eq!(proc.vmar().read_memory(vaddr, &mut b).unwrap(), 4);
    i32::from_ne_bytes(b)
}

/// A forked child of `parent` with its one, unstarted thread.
fn a_forked_child(parent: &Arc<Process>) -> (Arc<Process>, Arc<Thread>) {
    let child = Process::fork_from(parent).unwrap();
    let thread = Thread::create_linux(&child).unwrap();
    (child, thread)
}

#[test]
fn glibcs_fork_flags_ask_for_both_the_store_and_the_clear() {
    let req = ChildTidRequest::from_clone(flags(GLIBC_FORK), 0x7000_0010.into());
    assert_eq!(
        req,
        ChildTidRequest {
            ptr: 0x7000_0010,
            set: true,
            clear: true
        }
    );
    // musl's fork is a bare SIGCHLD: nothing asked, nothing done.
    let req = ChildTidRequest::from_clone(flags(0x11), 0x7000_0010.into());
    assert_eq!(
        req,
        ChildTidRequest {
            ptr: 0x7000_0010,
            set: false,
            clear: false
        }
    );
    assert_eq!(ChildTidRequest::none().ptr, 0);
}

#[test]
fn the_childs_tid_lands_in_the_childs_copy_of_the_page_not_the_parents() {
    // The very bug: the store went through the parent's pointer, i.e.
    // into the parent's copy, and the child's TCB kept the parent's tid.
    let (parent, page) = a_parent(43_361);
    let (child, thread) = a_forked_child(&parent);
    publish_child_tid(
        &thread,
        ChildTidRequest::from_clone(flags(GLIBC_FORK), page.into()),
    );
    assert_eq!(word_at(&child, page), thread.id() as i32, "child's copy");
    assert_eq!(word_at(&parent, page), SENTINEL, "parent's copy");
    // (The first thread of a process carries the pid as its tid, as on
    // Linux, so the word also reads as the child's getpid().)
    assert_eq!(thread.id(), child.id());
}

#[test]
fn the_clear_on_exit_is_registered_as_set_tid_address_would() {
    let (parent, page) = a_parent(43_362);
    let (_child, thread) = a_forked_child(&parent);
    assert!(thread.tid_address().is_null());
    publish_child_tid(
        &thread,
        ChildTidRequest::from_clone(flags(GLIBC_FORK), page.into()),
    );
    assert_eq!(thread.tid_address().as_addr(), page);
}

#[test]
fn without_the_flags_the_word_is_left_alone() {
    // A `clone(SIGCHLD, ..., ctid)` with a live ctid and neither bit
    // (musl leaves the register holding whatever): nothing may be written
    // and nothing registered, or exit would zero a word nobody gave it.
    let (parent, page) = a_parent(43_363);
    let (child, thread) = a_forked_child(&parent);
    publish_child_tid(
        &thread,
        ChildTidRequest::from_clone(flags(0x11), page.into()),
    );
    assert_eq!(word_at(&child, page), SENTINEL);
    assert!(thread.tid_address().is_null());
    // CLEARTID alone registers and does not store (pthread-style callers
    // that pass the two separately).
    publish_child_tid(
        &thread,
        ChildTidRequest::from_clone(flags(0x0020_0011), page.into()),
    );
    assert_eq!(word_at(&child, page), SENTINEL);
    assert_eq!(thread.tid_address().as_addr(), page);
}

#[test]
fn a_word_the_child_has_not_mapped_is_skipped_not_fatal() {
    // Linux stores from the child (`schedule_tail`) and ignores the
    // fault; the fork has already happened, so failing it here would
    // leave a child the caller was told does not exist.
    let (parent, page) = a_parent(43_364);
    let (child, thread) = a_forked_child(&parent);
    publish_child_tid(
        &thread,
        ChildTidRequest::from_clone(flags(GLIBC_FORK), (page + 0x10_0000).into()),
    );
    assert_eq!(word_at(&child, page), SENTINEL);
    // The clear is still registered: exit tolerates an unmapped word.
    assert_eq!(thread.tid_address().as_addr(), page + 0x10_0000);
}

#[test]
fn a_null_word_is_a_no_op_whatever_the_flags() {
    let (parent, page) = a_parent(43_365);
    let (child, thread) = a_forked_child(&parent);
    publish_child_tid(
        &thread,
        ChildTidRequest::from_clone(flags(GLIBC_FORK), 0.into()),
    );
    assert_eq!(word_at(&child, page), SENTINEL);
    assert!(thread.tid_address().is_null());
}
