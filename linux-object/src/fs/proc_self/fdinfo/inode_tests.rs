//! What `stat` says about `/proc/<pid>/fdinfo` and its files, which is the
//! half of this module no test looked at: every test here reads the text, and
//! the text comes out of `render`, not out of `metadata`.
//!
//! Both halves matter to the same tools. `cmp`, `tar`, `diff` and
//! `[ a -ef b ]` take two files with the same (dev, ino) for one file and
//! never read the second, so an inode number that falls into another part of
//! procfs makes two different files the same one; `find` prunes a directory
//! that says it has fewer than two links; and a size that is not zero is a
//! promise about a file whose length nobody knows until it is read.

use super::*;
use crate::fs::procfs::{pid_inode, PID_FDINFO_DIR_SLOT, PID_INODE_BASE, PID_INODE_SLOTS};
use crate::process::{LinuxProcess, NR_OPEN};
use alloc::vec::Vec;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::Job;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &Job::root(),
        pid,
        "fdinfo-inodes",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

#[test]
fn an_fdinfo_file_has_an_inode_of_its_own_above_every_per_pid_one() {
    // The same ground the per-thread numbers stand on, and for the same
    // reason: `pid_inode` reaches as high as this for the highest pid, so
    // anything below it is a file of some process's own directory.
    let pid_max = 4_194_304usize;
    let highest_pid_inode = PID_INODE_BASE + pid_max * PID_INODE_SLOTS;
    let mut seen = Vec::new();
    for pid in [0u64, 1, 1000, 4_194_303] {
        for fd in [0i32, 1, 3, NR_OPEN as i32 - 1] {
            let ino = fdinfo_inode(pid, FileDesc::from(fd));
            assert!(
                ino > highest_pid_inode,
                "pid {} fd {} lands in the per-pid range at {}",
                pid,
                fd,
                ino
            );
            assert!(
                !seen.contains(&ino),
                "{} twice (pid {} fd {})",
                ino,
                pid,
                fd
            );
            seen.push(ino);
        }
    }
    // And the directory's own number is a per-pid one, so it must not be any
    // of its files': that pair is what `[ /proc/N/fdinfo -ef /proc/N/fdinfo/3 ]`
    // asks about.
    let dir_ino = pid_inode(7731, PID_FDINFO_DIR_SLOT);
    assert!(!seen.contains(&dir_ino));
    assert!(dir_ino < highest_pid_inode);
}

#[test]
fn the_directory_has_two_links_and_its_files_have_no_length() {
    let proc = a_process(7731);
    let dir = ProcFdInfoDir {
        process: proc.clone(),
    };
    let meta = dir.metadata().unwrap();
    assert_eq!(meta.type_, FileType::Dir);
    assert_eq!(meta.nlinks, 2, "`find` prunes a directory with fewer");
    assert_eq!(meta.inode, pid_inode(proc.id(), PID_FDINFO_DIR_SLOT));

    let fd = FileDesc::from(3);
    let file = FdInfo {
        process: proc.clone(),
        fd,
    }
    .metadata()
    .unwrap();
    assert_eq!(file.type_, FileType::File);
    assert_eq!(file.size, 0, "its length is not known until it is read");
    assert_eq!(file.inode, fdinfo_inode(proc.id(), fd));
    assert_ne!(file.inode, meta.inode);
}
