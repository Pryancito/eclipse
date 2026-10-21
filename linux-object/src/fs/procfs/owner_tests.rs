//! Who owns the files of `/proc/<pid>` and with what modes, as on Linux 6.18
//! (measured there with glibc and musl): the effective ids of the process,
//! root's for all but its directories of mode 0555 while it is not dumpable
//! or is a zombie, and `fd` 0500 with `environ` 0400.
//!
//! Fixed pids live in 45_58x here. The ids are apart for each of real,
//! effective and saved, and for users and groups, so that taking one for
//! another is seen.

use super::*;
use crate::error::{LxError, LxResult};
use crate::fs::proc_self::{fd_permission, ProcFdInfoDir, ProcSelfFdDir};
use crate::fs::{EventFd, OpenFlags};
use crate::process::{LinuxProcess, ProcessExt, SUID_DUMP_DISABLE, SUID_DUMP_USER};
use crate::thread::ThreadExt;
use alloc::format;
use alloc::vec::Vec;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "owned",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// Make `proc` uid 1000, 1001, 1002 and gid 2000, 2001, 2002, which leaves it
/// not dumpable, as on Linux.
fn of_a_user(proc: &Process) {
    proc.linux().set_resgid(2000, 2001, 2002).unwrap();
    proc.linux().set_resuid(1000, 1001, 1002).unwrap();
}

/// Every file of `proc` that is not a directory of mode 0555, by name.
fn files(proc: &Process) -> Vec<(String, Arc<dyn INode>)> {
    let pid = proc.id();
    let fd = i32::from(
        proc.linux()
            .add_file(EventFd::new(0, OpenFlags::RDWR))
            .unwrap(),
    );
    let tid = proc.thread_ids()[0];
    let dir = ProcPidDirINode { pid };
    let task = ProcTidDirINode { pid, tid };
    let mut all: Vec<(String, Arc<dyn INode>)> = [
        "stat", "status", "statm", "cmdline", "comm", "environ", "maps", "limits", "perf",
        "threads", "exe", "cwd", "root", "fd",
    ]
    .iter()
    .map(|name| (String::from(*name), dir.find(name).unwrap()))
    .collect();
    let fd = format!("{}", fd);
    all.push((
        format!("fd/{}", fd),
        dir.find("fd").unwrap().find(&fd).unwrap(),
    ));
    all.push((
        format!("fdinfo/{}", fd),
        dir.find("fdinfo").unwrap().find(&fd).unwrap(),
    ));
    for name in ["stat", "status", "comm", "fd"] {
        all.push((format!("task/{}/{}", tid, name), task.find(name).unwrap()));
    }
    all
}

/// The directories of `proc` of mode 0555, by name.
fn world_dirs(proc: &Process) -> Vec<(String, Arc<dyn INode>)> {
    let pid = proc.id();
    let tid = proc.thread_ids()[0];
    let dir = ProcPidDirINode { pid };
    alloc::vec![
        (
            String::from("."),
            Arc::new(ProcPidDirINode { pid }) as Arc<dyn INode>
        ),
        (String::from("fdinfo"), dir.find("fdinfo").unwrap()),
        (String::from("task"), dir.find("task").unwrap()),
        (
            format!("task/{}", tid),
            Arc::new(ProcTidDirINode { pid, tid })
        ),
    ]
}

fn assert_owned(of: &[(String, Arc<dyn INode>)], uid: usize, gid: usize) {
    for (name, inode) in of {
        let m = inode.metadata().unwrap();
        assert_eq!((m.uid, m.gid), (uid, gid), "{}", name);
    }
}

#[test]
fn every_file_of_a_process_is_its_effective_ids() {
    let proc = a_process(45_581);
    of_a_user(&proc);
    proc.linux().set_dumpable(SUID_DUMP_USER);
    assert_owned(&files(&proc), 1001, 2001);
    assert_owned(&world_dirs(&proc), 1001, 2001);
}

#[test]
fn a_process_that_is_not_dumpable_keeps_only_its_0555_directories() {
    let proc = a_process(45_582);
    of_a_user(&proc);
    assert_eq!(proc.linux().dumpable(), SUID_DUMP_DISABLE);
    assert_owned(&files(&proc), 0, 0);
    assert_owned(&world_dirs(&proc), 1001, 2001);
}

#[test]
fn the_modes_are_those_of_linux() {
    let proc = a_process(45_583);
    let modes: Vec<(String, u16)> = files(&proc)
        .into_iter()
        .chain(world_dirs(&proc))
        .map(|(name, inode)| (name, inode.metadata().unwrap().mode))
        .collect();
    for (name, mode) in &modes {
        let want = match name.as_str() {
            "." | "fdinfo" | "task" => 0o555,
            n if n.starts_with("task/") && !n["task/".len()..].contains('/') => 0o555,
            // An eventfd, open for reading and writing.
            n if n.starts_with("fd/") => 0o700,
            n => match n.rsplit('/').next().unwrap() {
                "environ" => 0o400,
                "fd" => 0o500,
                "comm" => 0o644,
                "exe" | "cwd" | "root" => 0o777,
                _ => 0o444,
            },
        };
        assert_eq!(*mode, want, "{} is {:o}", name, mode);
    }
}

/// A zombie keeps its credentials and has no memory left.
#[test]
fn a_zombie_keeps_only_its_0555_directories() {
    let parent = a_process(45_584);
    let child = Process::fork_from(&parent).unwrap();
    Thread::create_linux(&child).unwrap();
    of_a_user(&child);
    child.linux().set_dumpable(SUID_DUMP_USER);
    let (files, dirs) = (files(&child), world_dirs(&child));
    assert_owned(&files, 1001, 2001);
    child.exit(0);
    let dir = ProcPidDirINode { pid: child.id() };
    let still: Vec<_> = files
        .into_iter()
        .filter(|(name, _)| ["stat", "status", "cmdline", "environ"].contains(&name.as_str()))
        .collect();
    assert_eq!(still.len(), 4);
    assert_owned(&still, 0, 0);
    assert_owned(&dirs[..1], 1001, 2001);
    let m = dir.find("task").unwrap().metadata().unwrap();
    assert_eq!((m.uid, m.gid), (1001, 2001), "task");
}

#[test]
fn a_file_of_a_process_that_is_gone_is_root_s() {
    let m = owner::of_pid(45_589, file_metadata(1, 0o444));
    assert_eq!((m.uid, m.gid, m.mode), (0, 0, 0o444));
}

/// `proc_fd_permission`: its own `fd` directory, and only that, lets a
/// process in whatever the ordinary check said.
#[test]
fn a_process_is_let_into_its_own_fd_directory_only() {
    let (mine, theirs) = (a_process(45_585), a_process(45_586));
    let fd_of = |proc: &Arc<Process>| -> Arc<dyn INode> {
        Arc::new(ProcSelfFdDir {
            process: proc.clone(),
        })
    };
    let denied: LxResult = Err(LxError::EACCES);
    assert_eq!(fd_permission(denied.clone(), &fd_of(&mine), &mine), Ok(()));
    assert_eq!(
        fd_permission(denied.clone(), &fd_of(&theirs), &mine),
        denied
    );
    let fdinfo: Arc<dyn INode> = Arc::new(ProcFdInfoDir {
        process: mine.clone(),
    });
    assert_eq!(fd_permission(denied.clone(), &fdinfo, &mine), denied);
    // Anything but a refusal of permission stands.
    let gone: LxResult = Err(LxError::ENOENT);
    assert_eq!(fd_permission(gone.clone(), &fd_of(&mine), &mine), gone);
    assert_eq!(fd_permission(Ok(()), &fd_of(&theirs), &mine), Ok(()));
}
