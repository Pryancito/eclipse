//! `/proc/<pid>` of a process whose leader has called `exit(2)` while
//! another thread runs on: on Linux 6.18 the state is `Z`, `Threads:` still
//! counts the leader, and `task/` lists it, as a zombie. Here it showed the
//! next thread, asleep, one thread short, and the leader's own `task/<pid>`
//! was gone.

use super::*;
use crate::process::{LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use alloc::string::ToString;
use rcore_fs_ramfs::RamFS;

fn read(pid: u64, kind: ProcPidFileKind) -> String {
    String::from_utf8(ProcPidFileINode { pid, kind }.bytes().unwrap()).unwrap()
}

fn read_task(pid: u64, tid: u64, kind: ProcTidFileKind) -> String {
    String::from_utf8(ProcTidFileINode { pid, tid, kind }.bytes().unwrap()).unwrap()
}

/// The fields of a `stat` line after the name, field 3 (the state) first.
fn after_name(stat: &str) -> Vec<&str> {
    stat[stat.rfind(')').unwrap() + 2..].split(' ').collect()
}

/// A process with its leader gone the way `exit(2)` takes it, and one
/// worker left running.
fn the_leader_gone_first(pid: u64) -> (Arc<Process>, Arc<Thread>) {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "lider",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let leader = Thread::create_linux(&proc).unwrap();
    let worker = Thread::create_linux(&proc).unwrap();
    assert_eq!(leader.id(), pid, "the premise: the leader's tid is the pid");
    proc.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    assert_eq!(proc.thread_ids(), [worker.id()], "the case this is about");
    (proc, worker)
}

#[test]
fn the_process_of_an_exited_leader_is_a_zombie_with_both_tasks() {
    let (proc, worker) = the_leader_gone_first(46_901);
    let pid = proc.id();

    assert_eq!(proc_state_char(&proc), 'Z');
    let stat = read(pid, ProcPidFileKind::Stat);
    let fields = after_name(&stat);
    assert_eq!(fields[0], "Z", "{}", stat);
    assert_eq!(fields[20 - 3], "2", "field 20 counts the leader: {}", stat);
    let status = read(pid, ProcPidFileKind::Status);
    assert!(status.contains("State:\tZ (zombie)\n"), "{}", status);
    assert!(status.contains("Threads:\t2\n"), "{}", status);

    assert_eq!(ProcPidTaskDirINode { pid }.tids(), [pid, worker.id()]);
    let own = read_task(pid, pid, ProcTidFileKind::Stat);
    assert_eq!(after_name(&own)[0], "Z", "{}", own);
    let other = read_task(pid, worker.id(), ProcTidFileKind::Stat);
    assert_ne!(after_name(&other)[0], "Z", "{}", other);
    let other = read_task(pid, worker.id(), ProcTidFileKind::Status);
    assert!(other.contains("Threads:\t2\n"), "{}", other);
    // And Eclipse's own task list says the same.
    let tasks = proc_perf_tasks_content();
    let row: Vec<&str> = tasks
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|row| row.first() == Some(&&*pid.to_string()))
        .unwrap();
    assert_eq!((row[1], row[3]), ("2", "Z"), "{:?}", row);

    drop(proc.linux().release_exited_leader());
}

/// Once it is let go, the process is its threads again: what `de_thread`
/// leaves after an `execve` from the worker, before the new leader is made.
#[test]
fn a_leader_let_go_is_no_task() {
    let (proc, worker) = the_leader_gone_first(46_902);
    let pid = proc.id();
    drop(proc.linux().release_exited_leader());

    assert_ne!(proc_state_char(&proc), 'Z');
    assert!(read(pid, ProcPidFileKind::Status).contains("Threads:\t1\n"));
    assert_eq!(ProcPidTaskDirINode { pid }.tids(), [worker.id()]);
    assert!(ProcTidFileINode {
        pid,
        tid: pid,
        kind: ProcTidFileKind::Stat
    }
    .bytes()
    .is_err());
}
