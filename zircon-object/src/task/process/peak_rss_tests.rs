//! `ru_maxrss`: the most a process has had resident, which outlives the
//! address space it was measured in. Linux keeps it in `signal->maxrss` and
//! folds each `mm`'s `hiwater_rss` into it at `execve` and at exit, so the
//! peak of a program before it exec'd, and of a child that has exited, is
//! still there to report.
//!
//! The pages are touched at addresses no other test in the crate maps, 16 MiB
//! apiece from [`WINDOWS`]: libos keeps every page of every process in one
//! host address space, and two tests mapping one address would count each
//! other's pages. Faults are taken by hand, so a page is resident whether or
//! not the build maps eagerly.

use super::*;
use crate::vm::{MMUFlags, VirtAddr, VmObject, PAGE_SIZE};

const K: usize = PAGE_SIZE;
const WINDOWS: usize = 0x3_9000_0000;

/// Map `pages` fresh pages in window `window` of `vmar` and write each one.
fn touch(vmar: &Arc<VmAddressRegion>, window: usize, pages: usize) -> VirtAddr {
    let flags = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let addr = vmar
        .map_at(
            WINDOWS + window * 0x100_0000,
            VmObject::new_paged(pages),
            0,
            pages * K,
            flags,
        )
        .unwrap();
    for i in 0..pages {
        vmar.handle_page_fault(addr + i * K, MMUFlags::WRITE | MMUFlags::USER)
            .unwrap();
    }
    addr
}

#[test]
fn a_process_s_peak_is_its_address_space_s_and_outlasts_the_unmap() {
    let proc = Process::create(&Job::root(), "peak").unwrap();
    assert_eq!(proc.peak_resident_bytes(), 0);
    let at = touch(&proc.vmar(), 0, 3);
    assert_eq!(proc.peak_resident_bytes(), 3 * K);
    proc.vmar().unmap(at, 3 * K).unwrap();
    assert_eq!(proc.peak_resident_bytes(), 3 * K);
}

/// `exec_mmap()`: the old image's peak stays the process's, and the new
/// image's own starts from nothing, which is what `VmHWM:` shows. A smaller
/// new image leaves it where it was, through its own exit too.
#[test]
fn an_execve_keeps_the_old_image_s_peak_for_the_process() {
    let proc = Process::create(&Job::root(), "peak").unwrap();
    touch(&proc.vmar(), 1, 4);
    let own = proc.own_address_space();
    own.clear().unwrap();
    assert_eq!(own.resident_peak_bytes(), 0, "the new image starts over");
    assert_eq!(proc.peak_resident_bytes(), 4 * K);
    touch(&own, 1, 1);
    assert_eq!(proc.peak_resident_bytes(), 4 * K, "under the old peak");
    proc.exit(0);
    assert_eq!(
        proc.peak_resident_bytes(),
        4 * K,
        "the smaller exit kept it"
    );
}

/// A new image that grows past the old peak is the new peak.
#[test]
fn a_new_image_bigger_than_the_old_one_is_the_new_peak() {
    let proc = Process::create(&Job::root(), "peak").unwrap();
    touch(&proc.vmar(), 6, 4);
    let own = proc.own_address_space();
    own.clear().unwrap();
    touch(&own, 6, 1);
    touch(&own, 7, 6);
    assert_eq!(proc.peak_resident_bytes(), 7 * K);
}

/// `do_exit()`: emptying the address space starts its peak over, and the
/// reaper still has to be told the child's.
#[test]
fn the_exit_keeps_the_peak_the_emptied_space_lost() {
    let proc = Process::create(&Job::root(), "peak").unwrap();
    touch(&proc.vmar(), 3, 5);
    proc.exit(0);
    assert_eq!(
        proc.vmar().resident_peak_bytes(),
        0,
        "the space was emptied"
    );
    assert_eq!(proc.peak_resident_bytes(), 5 * K);
}

/// A `vfork` child runs in its parent's address space, and its `execve`
/// leaves it there: the child keeps that space's peak, as Linux's child
/// folds the shared `mm`'s, and the parent's is its own still.
#[test]
fn a_vfork_child_s_execve_keeps_the_peak_of_the_space_it_borrowed() {
    let parent = Process::create(&Job::root(), "parent").unwrap();
    touch(&parent.vmar(), 4, 6);
    let child = Process::create_sharing_address_space(&Job::root(), "child", &parent, ()).unwrap();
    assert_eq!(child.peak_resident_bytes(), 6 * K, "it runs in that space");

    let own = child.own_address_space();
    assert_eq!(own.resident_peak_bytes(), 0);
    assert_eq!(child.peak_resident_bytes(), 6 * K);
    touch(&own, 5, 2);
    assert_eq!(child.peak_resident_bytes(), 6 * K);
    assert_eq!(parent.peak_resident_bytes(), 6 * K);
}

/// A `vfork` child that exits without an `execve` leaves its parent's space
/// behind, still running. Its peak is the one it had at its exit, as Linux's
/// zombie has only `signal->maxrss`: the parent growing the space before it
/// reaps the child must not make the child's peak grow too.
#[test]
fn an_exited_vfork_child_s_peak_stays_where_its_exit_left_it() {
    let parent = Process::create(&Job::root(), "parent").unwrap();
    touch(&parent.vmar(), 8, 6);
    let child = Process::create_sharing_address_space(&Job::root(), "child", &parent, ()).unwrap();
    child.exit(0);
    assert_eq!(
        child.peak_resident_bytes(),
        6 * K,
        "the space's, at the exit"
    );

    touch(&parent.vmar(), 9, 4);
    assert_eq!(parent.peak_resident_bytes(), 10 * K);
    assert_eq!(child.peak_resident_bytes(), 6 * K, "not the parent's since");
}
