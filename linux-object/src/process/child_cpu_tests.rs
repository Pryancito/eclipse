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
/// told and in what it adds up: `time sh -c make` is the compilers' time.
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
    grandchild.exit(0);
    let (_, cpu) = block_on(wait_child(&child, grandchild.id(), true, true)).unwrap();
    grandchild_thread.terminate_abandoned();
    assert_eq!(child.linux().children_usage(), cpu);

    child_thread.time_add(1_000_000);
    child_thread.note_context_switch(false);
    child_thread.note_context_switch(true);
    child.exit(0);
    let (_, cpu) = block_on(wait_child(&parent, child.id(), true, true)).unwrap();
    child_thread.terminate_abandoned();
    let both = ChildCpu {
        utime_ns: 301_000_000,
        stime_ns: 20_000_000,
        voluntary_switches: 26,
        involuntary_switches: 2,
    };
    assert_eq!(cpu, both, "wait4 left out the grandchild, or the switches");
    assert_eq!(parent.linux().children_usage(), both);
    assert_eq!(parent.linux().children_cpu_ns(), (301_000_000, 20_000_000));
}
