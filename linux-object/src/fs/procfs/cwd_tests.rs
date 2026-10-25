//! `/proc/<pid>/cwd` and `/proc/<pid>/root`, which were not there.
//!
//! Fixed pids live in 45_42x here.

use super::*;
use crate::process::{wait_child, LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

/// What the link `name` in `/proc/<pid>` points at.
fn link(pid: u64, name: &str) -> Result<String> {
    let inode = ProcPidDirINode { pid }.find(name)?;
    assert_eq!(inode.metadata()?.type_, FileType::SymLink);
    let mut buf = [0u8; 256];
    let len = inode.read_at(0, &mut buf)?;
    Ok(String::from_utf8(buf[..len].to_vec()).unwrap())
}

#[test]
fn cwd_and_root_are_the_process_s_and_cwd_says_when_it_has_gone() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_421,
        "cwd",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let lp = proc.linux();
    let top = lp
        .root_inode()
        .create("pcwd_a", FileType::Dir, 0o755)
        .unwrap();
    top.create("d", FileType::Dir, 0o755).unwrap();
    lp.change_directory("/pcwd_a/d");
    let pid = proc.id();

    assert_eq!(link(pid, "cwd").unwrap(), "/pcwd_a/d");
    assert_eq!(link(pid, "root").unwrap(), "/");
    top.unlink("d").unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(link(pid, "cwd").unwrap(), "/pcwd_a/d (deleted)");

    let listed: Vec<String> = (0..)
        .map_while(|id| ProcPidDirINode { pid }.get_entry(id).ok())
        .collect();
    assert!(
        listed.iter().any(|n| n == "cwd") && listed.iter().any(|n| n == "root"),
        "{:?}",
        listed
    );
}

#[test]
fn a_zombie_has_neither() {
    let parent = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_422,
        "padre",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    assert_eq!(link(pid, "cwd").unwrap(), "/");
    child.exit(0);
    drop(child);

    assert!(
        ProcPidDirINode { pid }.find("stat").is_ok(),
        "the zombie is there"
    );
    assert!(link(pid, "cwd").is_err());
    assert!(link(pid, "root").is_err());
    block_on(wait_child(&parent, pid, true, true)).unwrap();
}

/// `cwd` follows a rename of the working directory, and going through it
/// leads to the directory itself, removed or not: `ls /proc/<pid>/cwd/` of a
/// process whose directory was removed is that empty directory on Linux, not
/// `ENOENT`.
#[test]
fn cwd_follows_a_rename_and_leads_to_the_directory_even_once_it_has_gone() {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        45_423,
        "cwd",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    let lp = proc.linux();
    let top = lp
        .root_inode()
        .create("pcwd_c", FileType::Dir, 0o755)
        .unwrap();
    let d = top.create("d", FileType::Dir, 0o755).unwrap();
    lp.change_directory("/pcwd_c/d");
    let pid = proc.id();

    top.move_("d", &top, "e").unwrap();
    crate::fs::dcache_invalidate();
    assert_eq!(link(pid, "cwd").unwrap(), "/pcwd_c/e");

    top.unlink("e").unwrap();
    crate::fs::dcache_invalidate();
    let held = ProcPidDirINode { pid }.find("cwd").unwrap();
    let (led_to, name) = held.jump().unwrap().unwrap();
    assert_eq!(
        led_to.metadata().unwrap().inode,
        d.metadata().unwrap().inode
    );
    assert_eq!(name, "/pcwd_c/e");
    assert_eq!(link(pid, "cwd").unwrap(), "/pcwd_c/e (deleted)");
}
