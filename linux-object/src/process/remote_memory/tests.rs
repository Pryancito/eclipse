//! Fixed pids live in 47_5xx here.

use super::*;
use crate::process::{LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use alloc::sync::Arc;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, Thread, ROOT_JOB};
use zircon_object::vm::VmObject;

const BASE: usize = 0x20_0000;
const RW: MMUFlags = MMUFlags::READ.union(MMUFlags::WRITE).union(MMUFlags::USER);

/// A process with one thread, so its address space stays while it lives.
fn a_process(pid: u64) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "remote", lp).unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// Map `pages` pages at `addr` with `flags`, filled with `fill`.
fn map(proc: &Process, addr: usize, pages: usize, flags: MMUFlags, fill: u8) {
    let vmo = VmObject::new_paged(pages);
    vmo.write(0, &alloc::vec![fill; pages * PAGE_SIZE]).unwrap();
    let at = proc
        .vmar()
        .map(Some(addr), vmo, 0, pages * PAGE_SIZE, flags);
    assert_eq!(at, Ok(addr));
}

/// A read that runs over a page boundary inside one mapping is one read.
#[test]
fn a_read_goes_across_pages() {
    let proc = a_process(47_501);
    map(&proc, BASE, 2, RW, b'a');
    let mut buf = [0u8; 20];
    assert_eq!(read(&proc.vmar(), BASE + PAGE_SIZE - 10, &mut buf), 20);
    assert_eq!(buf, [b'a'; 20]);
    proc.exit(0);
}

/// Two mappings side by side read as one stretch, and a hole after them
/// stops the read where the memory ends.
#[test]
fn a_read_goes_from_one_mapping_to_the_next_and_stops_at_a_hole() {
    let proc = a_process(47_502);
    map(&proc, BASE, 1, RW, b'a');
    map(&proc, BASE + PAGE_SIZE, 1, RW, b'b');
    let mut buf = [0u8; 20];
    assert_eq!(read(&proc.vmar(), BASE + PAGE_SIZE - 10, &mut buf), 20);
    assert_eq!(&buf[..10], &[b'a'; 10]);
    assert_eq!(&buf[10..], &[b'b'; 10]);
    let mut buf = [0u8; 20];
    assert_eq!(read(&proc.vmar(), BASE + 2 * PAGE_SIZE - 6, &mut buf), 6);
    assert_eq!(read(&proc.vmar(), BASE + 2 * PAGE_SIZE, &mut buf), 0);
    proc.exit(0);
}

/// A page the process cannot read is not read for it either, and one it
/// cannot write is not written; what it can read of the second still is.
#[test]
fn the_protection_of_the_page_is_the_process_s_own() {
    let proc = a_process(47_503);
    map(&proc, BASE, 1, MMUFlags::USER, b'n');
    map(
        &proc,
        BASE + PAGE_SIZE,
        1,
        MMUFlags::READ | MMUFlags::USER,
        b'r',
    );
    let mut buf = [0u8; 4];
    assert_eq!(read(&proc.vmar(), BASE, &mut buf), 0);
    assert_eq!(write(&proc.vmar(), BASE + PAGE_SIZE, b"WXYZ"), 0);
    assert_eq!(read(&proc.vmar(), BASE + PAGE_SIZE, &mut buf), 4);
    assert_eq!(&buf, b"rrrr");
    // Written up to the page that is read-only, and no further.
    map(&proc, BASE + 2 * PAGE_SIZE, 1, RW, b'w');
    let read_only = MMUFlags::READ | MMUFlags::USER;
    map(&proc, BASE + 3 * PAGE_SIZE, 1, read_only, b'r');
    assert_eq!(write(&proc.vmar(), BASE + 3 * PAGE_SIZE - 2, b"WXYZ"), 2);
    assert_eq!(read(&proc.vmar(), BASE + 3 * PAGE_SIZE - 2, &mut buf), 4);
    assert_eq!(&buf, b"WXrr");
    proc.exit(0);
}

/// What is written goes into that process's memory, and into nobody
/// else's: a forked child's copy is its own.
#[test]
fn a_write_into_a_child_stays_in_the_child() {
    let parent = a_process(47_504);
    map(&parent, BASE, 1, RW, b'p');
    let child = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&child).unwrap();
    assert_eq!(write(&child.vmar(), BASE + 1, b"CHILD"), 5);
    let mut buf = [0u8; 7];
    assert_eq!(read(&child.vmar(), BASE, &mut buf), 7);
    assert_eq!(&buf, b"pCHILDp");
    assert_eq!(read(&parent.vmar(), BASE, &mut buf), 7);
    assert_eq!(&buf, b"ppppppp");
    child.exit(0);
    parent.exit(0);
}

/// An address that wraps round the end of the address space is no address.
#[test]
fn an_address_that_wraps_reads_nothing() {
    let proc = a_process(47_505);
    let mut buf = [0u8; 8];
    assert_eq!(read(&proc.vmar(), usize::MAX - 3, &mut buf), 0);
    assert_eq!(write(&proc.vmar(), usize::MAX - 3, &buf), 0);
    proc.exit(0);
}
