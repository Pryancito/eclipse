//! What a reaper is told a dead child used: its threads' measured CPU time,
//! including the ones still running when it called `exit_group`.
//!
//! `exit_group` makes a child waitable at once, before its threads have
//! gone, and the reaper's copy is taken then. It was the time of the threads
//! that had ALREADY exited, which for a child that called `exit` from its
//! only thread is none, plus `/proc/perf`'s estimate of its syscalls as
//! kernel time: `wait4` reported a child that had worked 300 ms as 70.

extern crate std;

use super::*;
use crate::thread::ThreadExt;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

#[test]
fn a_child_that_called_exit_group_used_what_its_threads_did() {
    let parent = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 45_121).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let thread = Thread::create_linux(&child).unwrap();
    thread.time_add(7_000_000);
    thread.sys_time_add(3_000_000);
    child.exit(0);

    let (_, cpu) = block_on(wait_child(&parent, child.id(), true, true)).unwrap();
    thread.terminate_abandoned();
    assert_eq!(
        (cpu.utime_ns, cpu.stime_ns),
        (7_000_000, 3_000_000),
        "wait4 left out a thread that was still there at exit_group"
    );
    assert_eq!(parent.linux().children_cpu_ns(), (7_000_000, 3_000_000));
}

/// A child's own reaped children are the child's too, in what its reaper is
/// told and in what it adds up: `time sh -c make` is the compilers' time, and
/// their faults are the child's faults.
#[test]
fn a_child_hands_up_what_its_reaped_children_used() {
    let parent = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 45_122).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let child_thread = Thread::create_linux(&child).unwrap();
    let grandchild = Process::fork_from(&child).unwrap();
    let grandchild_thread = Thread::create_linux(&grandchild).unwrap();
    grandchild_thread.time_add(300_000_000);
    grandchild_thread.sys_time_add(20_000_000);
    for _ in 0..25 {
        grandchild_thread.note_context_switch(false);
    }
    grandchild_thread.note_context_switch(true);
    for _ in 0..1_024 {
        grandchild_thread.note_minor_fault();
    }
    grandchild.exit(0);
    let (_, cpu) = block_on(wait_child(&child, grandchild.id(), true, true)).unwrap();
    grandchild_thread.terminate_abandoned();
    assert_eq!(child.linux().children_usage(), cpu);

    child_thread.time_add(1_000_000);
    child_thread.note_context_switch(false);
    child_thread.note_context_switch(true);
    for _ in 0..3 {
        child_thread.note_minor_fault();
    }
    child.exit(0);
    let (_, cpu) = block_on(wait_child(&parent, child.id(), true, true)).unwrap();
    child_thread.terminate_abandoned();
    let both = ChildCpu {
        utime_ns: 301_000_000,
        stime_ns: 20_000_000,
        voluntary_switches: 26,
        involuntary_switches: 2,
        minor_faults: 1_027,
        // Neither touched any memory.
        max_rss_bytes: 0,
    };
    assert_eq!(
        cpu, both,
        "wait4 left out the grandchild, the switches or the faults"
    );
    assert_eq!(parent.linux().children_usage(), both);
    assert_eq!(parent.linux().children_cpu_ns(), (301_000_000, 20_000_000));
}

/// Map `pages` fresh pages in `proc`'s address space and write each one, at
/// window `window` of 16 MiB windows nobody else in this binary maps: libos
/// keeps every process's pages in one host address space.
fn touch(proc: &Arc<Process>, window: usize, pages: usize) {
    use zircon_object::vm::{MMUFlags, VmObject};
    let vmar = proc.vmar();
    let rw = MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER;
    let addr = vmar
        .map_at(
            0x3_9000_0000 + window * 0x100_0000,
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
}

/// `ru_maxrss` is a peak, and so is what the reaper keeps of it: the largest
/// child's, a grandchild's included, never the sum (`wait_task_zombie`:
/// `cmaxrss = max(cmaxrss, max(sig->maxrss, sig->cmaxrss))`). Summed, a
/// shell that ran a thousand small commands would report gigabytes.
#[test]
fn a_reaper_keeps_the_largest_peak_and_not_the_sum() {
    const K: u64 = 4096;
    let parent = Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, 45_123).unwrap();
    let reap = |child: &Arc<Process>, of: &Arc<Process>| {
        child.exit(0);
        block_on(wait_child(of, child.id(), true, true)).unwrap().1
    };

    // A child that ran a bigger grandchild hands up the grandchild's peak.
    let child = Process::fork_from(&parent).unwrap();
    let grandchild = Process::fork_from(&child).unwrap();
    touch(&grandchild, 0, 5);
    assert_eq!(reap(&grandchild, &child).max_rss_bytes, 5 * K);
    touch(&child, 1, 2);
    let first = reap(&child, &parent);
    assert_eq!(first.max_rss_bytes, 5 * K, "the grandchild's, not 2 + 5");
    assert_eq!(parent.linux().children_usage().max_rss_bytes, 5 * K);

    // A smaller one moves nothing; a bigger one is the new peak.
    let small = Process::fork_from(&parent).unwrap();
    touch(&small, 2, 3);
    assert_eq!(reap(&small, &parent).max_rss_bytes, 3 * K);
    assert_eq!(parent.linux().children_usage().max_rss_bytes, 5 * K);
    let big = Process::fork_from(&parent).unwrap();
    touch(&big, 3, 9);
    reap(&big, &parent);
    assert_eq!(parent.linux().children_usage().max_rss_bytes, 9 * K);
}
