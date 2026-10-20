//! `/proc/<pid>` of a zombie: there until somebody reaps it, which is how
//! `ps` shows a `<defunct>`. It had no directory at all once its last thread
//! was gone, and a reaped process could still be looked up while it finished
//! dying in its job.

use super::*;
use crate::process::{wait_child, wait_status_exited, LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use alloc::vec::Vec;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

/// The pids `ls /proc` lists.
fn listed() -> Vec<u64> {
    (2 + PROC_ROOT_STATIC.len()..)
        .map_while(|id| ProcRootINode::entry_name(id).ok())
        .filter_map(|name| name.parse().ok())
        .collect()
}

fn read(pid: u64, kind: ProcPidFileKind) -> Result<String> {
    let bytes = ProcPidFileINode { pid, kind }.bytes()?;
    Ok(String::from_utf8(bytes).unwrap())
}

#[test]
fn a_zombie_has_its_directory_until_it_is_reaped() {
    let parent = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_341,
        "padre",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    child.linux().set_cmdline(alloc::vec![String::from("hijo")]);
    child.exit(0);
    drop(child);

    let name = alloc::format!("{}", pid);
    assert!(ProcRootINode.find(&name).is_ok(), "no /proc/<zombie>");
    assert!(listed().contains(&pid), "`ls /proc` leaves the zombie out");
    let stat = read(pid, ProcPidFileKind::Stat).unwrap();
    let after_name = &stat[stat.rfind(')').unwrap() + 2..];
    assert!(
        after_name.starts_with(&alloc::format!("Z {} ", parent.id())),
        "{}",
        stat
    );
    assert_eq!(
        read(pid, ProcPidFileKind::Cmdline).unwrap(),
        "",
        "a zombie's cmdline is empty"
    );

    assert_eq!(
        block_on(wait_child(&parent, pid, true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0))
    );
    assert!(
        ProcRootINode.find(&name).is_err(),
        "reaped, and still there"
    );
    assert!(!listed().contains(&pid));
    assert!(read(pid, ProcPidFileKind::Stat).is_err());
}
