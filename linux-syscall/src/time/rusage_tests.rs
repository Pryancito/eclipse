//! What `getrusage(2)` reports for each `who`: the calling thread's own, the
//! process's with its exited threads, the reaped children's with theirs.
//! The context switch counts were 0 for all three.
//!
//! Fixed pids live in 45_39x here.

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
