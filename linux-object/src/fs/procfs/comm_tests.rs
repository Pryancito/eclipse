//! `/proc/<pid>/comm` takes a name as `task/<pid>/comm` does, and only a
//! process's own threads may rename it.
//!
//! Fixed pids live in 45_44x here.

use super::*;
use crate::process::LinuxProcess;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

#[test]
fn writing_proc_pid_comm_renames_the_leader_and_only_it() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_441,
        "programa",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let leader = Thread::create_linux(&proc).unwrap();
    let other = Thread::create_linux(&proc).unwrap();
    let comm = ProcPidFileINode {
        pid: proc.id(),
        kind: ProcPidFileKind::Comm,
    };
    assert_eq!(comm.metadata().unwrap().mode, 0o644);
    assert!(comm.poll().unwrap().write);
    // `fopen(.., "w")` truncates first.
    comm.resize(0).unwrap();

    assert_eq!(comm.write_at(0, b"0123456789ABCDEFGHIJ\n").unwrap(), 21);
    assert_eq!(leader.lock_linux().comm, "0123456789ABCDE");
    assert!(
        other.lock_linux().comm.is_empty(),
        "a sibling keeps its name"
    );
    let mut buf = [0u8; 64];
    let n = comm.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"0123456789ABCDE\n");
}

#[test]
fn the_other_files_take_no_writes() {
    let stat = ProcPidFileINode {
        pid: 45_442,
        kind: ProcPidFileKind::Stat,
    };
    assert_eq!(stat.write_at(0, b"x"), Err(FsError::NotSupported));
    assert_eq!(stat.resize(0), Err(FsError::NotSupported));
}

#[test]
fn only_a_process_s_own_threads_rename_it() {
    assert!(may_rename(Some(45_443), 45_443));
    assert!(!may_rename(Some(45_444), 45_443), "another process: EINVAL");
    assert!(may_rename(None, 45_443), "the kernel itself");
}
