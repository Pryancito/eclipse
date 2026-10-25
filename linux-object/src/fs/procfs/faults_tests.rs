//! Fields 10 and 11 of `/proc/<pid>/stat`, `minflt` and `cminflt`: the
//! faults the address space resolved, the whole group's on the process's
//! file and the task's own on a thread's, and the reaped children's on both,
//! as `do_task_stat` writes them. They were 0.
//!
//! Fixed pids live in 47_10x here.

use super::*;
use crate::process::{ChildCpu, LinuxProcess};
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "stat", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// Field `n` (1-based, as proc(5) numbers them) of the stat line.
fn field(line: &str, n: usize) -> i64 {
    let after_comm = &line[line.rfind(')').unwrap() + 2..];
    after_comm
        .split(' ')
        .nth(n - 3)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn minflt_is_the_group_s_on_the_process_and_the_task_s_on_a_thread() {
    let proc = a_process(47_101);
    let one = Thread::create_linux(&proc).unwrap();
    let other = Thread::create_linux(&proc).unwrap();
    for _ in 0..5 {
        one.note_minor_fault();
    }
    other.note_minor_fault();
    other.note_minor_fault();
    assert_eq!(field(&proc_pid_stat(&proc), 10), 7);
    assert_eq!(field(&proc_task_stat(&proc, Some(&one)), 10), 5);
    assert_eq!(field(&proc_task_stat(&proc, Some(&other)), 10), 2);
}

#[test]
fn cminflt_is_the_reaped_children_s_on_every_file_of_the_group() {
    let proc = a_process(47_102);
    let thread = Thread::create_linux(&proc).unwrap();
    proc.linux().credit_children_cpu(ChildCpu {
        minor_faults: 1_027,
        ..Default::default()
    });
    let line = proc_pid_stat(&proc);
    assert_eq!(field(&line, 11), 1_027, "{:?}", line);
    assert_eq!(field(&line, 10), 0, "a child's faults are not its parent's");
    assert_eq!(field(&proc_task_stat(&proc, Some(&thread)), 11), 1_027);
}
