//! Which files of `/proc` are written out when they are read and read from
//! one copy per open file, as a `seq_file` is on Linux 6.18 (measured there
//! with glibc and musl), and which are not: `cmdline` and `environ` of a
//! process, and the values of `/proc/sys`, which Linux reads at each offset
//! and seeks as files of size 0.
//!
//! Fixed pids live in 45_57x here.

use super::*;
use crate::fs::proc_self::ProcFdInfoDir;
use crate::fs::{EventFd, File, FileLike, OpenFlags, SeekFrom};
use crate::process::{LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use alloc::vec::Vec;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn open(inode: Arc<dyn INode>) -> Arc<File> {
    File::new(inode, OpenFlags::RDONLY, String::from("/proc/x"))
}

/// Where `lseek(fd, at, SEEK_END)` puts an open `inode`.
fn from_the_end(inode: Arc<dyn INode>, at: i64) -> Option<u64> {
    File::seek(&open(inode), SeekFrom::End(at)).ok()
}

fn lookup(path: &str) -> Arc<dyn INode> {
    (&ProcRootINode as &dyn INode).lookup(path).unwrap()
}

#[test]
fn the_files_of_a_process_but_cmdline_and_environ_have_no_end() {
    let proc = a_process(45_571, "seq");
    let thread = Thread::create_linux(&proc).unwrap();
    let (pid, tid) = (proc.id(), thread.id());
    let dir = ProcPidDirINode { pid };
    for name in [
        "stat", "status", "statm", "maps", "comm", "limits", "threads",
    ] {
        assert_eq!(from_the_end(dir.find(name).unwrap(), 0), None, "{}", name);
    }
    let tid_dir = ProcTidDirINode { pid, tid };
    for name in ["stat", "status", "comm"] {
        let file = tid_dir.find(name).unwrap();
        assert_eq!(from_the_end(file, 0), None, "task/{}", name);
    }
    let fd = proc
        .linux()
        .add_file(EventFd::new(0, OpenFlags::RDWR))
        .unwrap();
    let info = ProcFdInfoDir { process: proc }
        .find(&alloc::format!("{}", i32::from(fd)))
        .unwrap();
    assert_eq!(from_the_end(info, 0), None, "fdinfo");
    for name in ["cmdline", "environ"] {
        assert_eq!(
            from_the_end(dir.find(name).unwrap(), 0),
            Some(0),
            "{}",
            name
        );
        assert_eq!(
            from_the_end(dir.find(name).unwrap(), 5),
            Some(5),
            "{}",
            name
        );
    }
}

#[test]
fn the_files_of_proc_have_no_end_and_the_values_of_proc_sys_end_at_0() {
    for path in ["meminfo", "uptime", "cmdline", "sysvipc/msg", "net/tcp"] {
        assert_eq!(from_the_end(lookup(path), 0), None, "{}", path);
    }
    for path in [
        "sys/kernel/pid_max",
        "sys/kernel/osrelease",
        "sys/kernel/random/boot_id",
        "sys/kernel/perf_event_paranoid",
        "sys/fs/inotify/max_user_watches",
        "sys/kernel/hostname",
    ] {
        assert_eq!(from_the_end(lookup(path), 0), Some(0), "{}", path);
        assert_eq!(from_the_end(lookup(path), 5), Some(5), "{}", path);
    }
}

/// The process renamed between two reads of its `stat`: the second read is
/// of the copy the first wrote out, and a new open has the new name.
#[test]
fn stat_read_in_pieces_is_of_one_moment() {
    let proc = a_process(45_572, "before");
    let leader = Thread::create_linux(&proc).unwrap();
    let dir = ProcPidDirINode { pid: proc.id() };
    let stat = open(dir.find("stat").unwrap());
    let mut text = alloc::vec![0u8; 1];
    assert_eq!(block_on(FileLike::read(&*stat, &mut text)).unwrap(), 1);
    write_comm(&proc, &leader, b"after").unwrap();
    let mut rest = alloc::vec![0u8; 4096];
    let n = block_on(FileLike::read(&*stat, &mut rest)).unwrap();
    text.extend_from_slice(&rest[..n]);
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("(before)"), "{}", text);
    let mut now = alloc::vec![0u8; 4096];
    let n = block_on(FileLike::read(&*open(dir.find("stat").unwrap()), &mut now)).unwrap();
    let now: Vec<u8> = now[..n].to_vec();
    assert!(String::from_utf8(now).unwrap().contains("(after)"));
}
