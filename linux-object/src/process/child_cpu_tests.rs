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
