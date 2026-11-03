//! What `getrusage(2)` reports for each `who`: the calling thread's own, the
//! process's with its exited threads, the reaped children's with theirs.
//! The context switch counts and the minor faults were 0 for all three.
//!
//! Fixed pids live in 45_39x here, and 47_11x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use async_std::task::block_on;
use linux_object::process::{wait_child, LinuxProcess};
use rcore_fs_ramfs::RamFS;

const RUSAGE_SELF: usize = 0;
const RUSAGE_THREAD: usize = 1;
const RUSAGE_CHILDREN: usize = -1i64 as usize;

fn usage(sys: &Syscall<'_>, who: usize) -> RUsage {
    let mut ru = RUsage::default();
    sys.sys_getrusage(who, UserOutPtr::from(&mut ru as *mut RUsage as usize))
        .unwrap();
    ru
}

fn note(thread: &Thread, voluntary: usize, involuntary: usize) {
    for _ in 0..voluntary {
        thread.note_context_switch(false);
    }
    for _ in 0..involuntary {
        thread.note_context_switch(true);
    }
}

#[test]
fn each_who_reports_its_own_tasks_context_switches() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_391,
        "rusage",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    // A thread still there, and one gone.
    let other = Thread::create_linux(&proc).unwrap();
    note(&other, 2, 1);
    let gone = Thread::create_linux(&proc).unwrap();
    note(&gone, 4, 0);
    gone.terminate_abandoned();
    // A child that has been reaped.
    let child = Process::fork_from(&proc).unwrap();
    let child_thread = Thread::create_linux(&child).unwrap();
    note(&child_thread, 8, 5);
    child.exit(0);
    block_on(wait_child(&proc, child.id(), true, true)).unwrap();
    child_thread.terminate_abandoned();

    let (mine, thread, process, children) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            note(sys.thread, 16, 3);
            (
                sys.thread.context_switches(),
                usage(sys, RUSAGE_THREAD),
                usage(sys, RUSAGE_SELF),
                usage(sys, RUSAGE_CHILDREN),
            )
        })
    });
    let (v, i) = (mine.0 as i64, mine.1 as i64);
    assert!(v >= 16 && i >= 3, "{:?}", mine);
    assert_eq!((thread.nvcsw, thread.nivcsw), (v, i), "RUSAGE_THREAD");
    assert_eq!(
        (process.nvcsw, process.nivcsw),
        (v + 2 + 4, i + 1),
        "RUSAGE_SELF: every thread, the one gone too"
    );
    assert_eq!((children.nvcsw, children.nivcsw), (8, 5), "RUSAGE_CHILDREN");
}

fn faults(thread: &Thread, faults: usize) {
    for _ in 0..faults {
        thread.note_minor_fault();
    }
}

#[test]
fn each_who_reports_its_own_tasks_minor_faults() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        47_111,
        "rusage",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    // A thread still there, and one gone.
    let other = Thread::create_linux(&proc).unwrap();
    faults(&other, 2);
    let gone = Thread::create_linux(&proc).unwrap();
    faults(&gone, 4);
    gone.terminate_abandoned();
    // A child that has been reaped.
    let child = Process::fork_from(&proc).unwrap();
    let child_thread = Thread::create_linux(&child).unwrap();
    faults(&child_thread, 1_024);
    child.exit(0);
    block_on(wait_child(&proc, child.id(), true, true)).unwrap();
    child_thread.terminate_abandoned();

    let (thread, process, children) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            faults(sys.thread, 16);
            (
                usage(sys, RUSAGE_THREAD),
                usage(sys, RUSAGE_SELF),
                usage(sys, RUSAGE_CHILDREN),
            )
        })
    });
    assert_eq!(thread.minflt, 16, "RUSAGE_THREAD");
    assert_eq!(
        process.minflt,
        16 + 2 + 4,
        "RUSAGE_SELF: every thread, the one gone too"
    );
    assert_eq!(children.minflt, 1_024, "RUSAGE_CHILDREN");
    assert_eq!(
        (thread.majflt, process.majflt, children.majflt),
        (0, 0, 0),
        "no fault here reads a disk"
    );
}

/// Map `pages` fresh pages in `proc`'s address space and write each one, in
/// window `window` of 16 MiB windows from an address nothing else in this
/// binary maps: libos keeps every process's pages in one host address space.
fn touch(proc: &Arc<Process>, window: usize, pages: usize) -> usize {
    use zircon_object::vm::{MMUFlags, VmObject};
    let vmar = proc.vmar();
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let addr = vmar
        .map_at(
            0x3_a000_0000 + window * 0x100_0000,
            VmObject::new_paged(pages),
            0,
            pages * 4096,
            rw,
        )
        .unwrap();
    for i in 0..pages {
        vmar.handle_page_fault(addr + i * 4096, MMUFlags::WRITE | MMUFlags::USER)
            .unwrap();
    }
    addr
}

/// `ru_maxrss` is the most memory the tasks have had resident, in kB: for
/// `RUSAGE_SELF` still there after a `munmap`, for `RUSAGE_THREAD` the same,
/// since the memory is the process's and not one thread's, and for
/// `RUSAGE_CHILDREN` the largest reaped child's, not the sum. All three were
/// 0, which `time -v` printed as "Maximum resident set size (kbytes): 0".
#[test]
fn each_who_reports_its_tasks_peak_resident_size() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        47_112,
        "rusage",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let at = touch(&proc, 0, 24);
    proc.vmar().unmap(at, 24 * 4096).unwrap();
    for (window, pages) in [(1, 40), (2, 4)] {
        let child = Process::fork_from(&proc).unwrap();
        touch(&child, window, pages);
        child.exit(0);
        block_on(wait_child(&proc, child.id(), true, true)).unwrap();
    }

    let (thread, process, children) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                usage(sys, RUSAGE_THREAD),
                usage(sys, RUSAGE_SELF),
                usage(sys, RUSAGE_CHILDREN),
            )
        })
    });
    assert_eq!(process.maxrss, 24 * 4, "RUSAGE_SELF, after the munmap");
    assert_eq!(thread.maxrss, 24 * 4, "RUSAGE_THREAD is the process's");
    assert_eq!(
        children.maxrss,
        40 * 4,
        "RUSAGE_CHILDREN, the largest child"
    );
}
