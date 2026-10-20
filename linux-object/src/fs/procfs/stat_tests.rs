//! What `stat` says of the files, links and directories of `/proc/<pid>`:
//! size 0 and blocks of 1024, as in Linux, with the content written out only
//! when it is read; a number of its own for each link; and in the size of
//! `fd`, how many descriptors are open.
//!
//! Fixed pids live in 45_55x here.

use super::*;
use crate::fs::proc_self::{ProcFdInfoDir, ProcSelfFdDir};
use crate::fs::{EventFd, FileLike, OpenFlags};
use crate::process::{wait_child, LinuxProcess, ProcessExt};
use crate::thread::ThreadExt;
use alloc::vec::Vec;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "stat", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// Size, blocks and block size.
fn sized(inode: &dyn INode) -> (usize, usize, usize) {
    let meta = inode.metadata().unwrap();
    (meta.size, meta.blocks, meta.blk_size)
}

fn read(inode: &dyn INode) -> Vec<u8> {
    let mut buf = alloc::vec![0u8; 8192];
    let n = inode.read_at(0, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn the_files_of_a_process_say_size_0_and_are_read_whole() {
    let proc = a_process(45_551);
    proc.linux().set_cmdline(alloc::vec![String::from("stat")]);
    let dir = ProcPidDirINode { pid: proc.id() };
    for name in [
        "stat", "status", "cmdline", "environ", "comm", "limits", "maps", "statm",
    ] {
        let file = dir.find(name).unwrap();
        assert_eq!(sized(&*file), (0, 0, 1024), "{}", name);
    }
    for (name, head) in [
        ("stat", "45551 ("),
        ("status", "Name:\t"),
        ("cmdline", "stat"),
    ] {
        let text = read(&*dir.find(name).unwrap());
        assert!(text.starts_with(head.as_bytes()), "{}: {:?}", name, text);
    }
}

#[test]
fn the_files_of_a_thread_as_well() {
    let proc = a_process(45_552);
    let thread = Thread::create_linux(&proc).unwrap();
    let dir = ProcTidDirINode {
        pid: proc.id(),
        tid: thread.id(),
    };
    for name in ["stat", "status", "comm"] {
        let file = dir.find(name).unwrap();
        assert_eq!(sized(&*file), (0, 0, 1024), "{}", name);
        assert!(!read(&*file).is_empty(), "{}", name);
    }
}

/// Saying 0 without writing the file out still asks whether the process is
/// there: a `stat` of a file of a reaped one is `ENOENT`.
#[test]
fn a_file_of_a_process_that_is_gone_is_not_there() {
    let parent = a_process(45_553);
    let thread = Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    let stat = ProcPidDirINode { pid }.find("stat").unwrap();
    assert!(stat.metadata().is_ok());
    child.exit(0);
    drop(child);
    block_on(wait_child(&parent, pid, true, true)).unwrap();
    assert_eq!(stat.metadata().err(), Some(FsError::EntryNotFound));
    // A thread the process does not have.
    let (pid, tid) = (parent.id(), thread.id() + 1_000_000);
    let gone: [Arc<dyn INode>; 2] = [
        Arc::new(ProcTidFileINode {
            pid,
            tid,
            kind: ProcTidFileKind::Stat,
        }),
        Arc::new(ProcTidCommINode { pid, tid }),
    ];
    for file in gone {
        assert_eq!(file.metadata().err(), Some(FsError::EntryNotFound));
    }
}

#[test]
fn the_links_say_size_0_and_have_numbers_of_their_own() {
    let proc = a_process(45_554);
    let dir = ProcPidDirINode { pid: proc.id() };
    let mut seen = Vec::new();
    for name in ["exe", "cwd", "root", "stat"] {
        let meta = dir.find(name).unwrap().metadata().unwrap();
        if name != "stat" {
            assert_eq!(meta.type_, FileType::SymLink, "{}", name);
            assert_eq!(
                (meta.size, meta.blocks, meta.blk_size),
                (0, 0, 1024),
                "{}",
                name
            );
        }
        assert!(
            meta.inode != 0 && !seen.contains(&meta.inode),
            "{}: {}",
            name,
            meta.inode
        );
        seen.push(meta.inode);
    }
    for thread in [false, true] {
        assert_eq!(sized(&ProcSelfSymINode { thread }), (0, 0, 1024));
    }
}

/// An entry of `fd` has a number of its own, the same in a listing as in
/// `lstat`, and found without looking at the descriptor.
#[test]
fn an_entry_of_fd_is_listed_under_the_number_lstat_gives_it() {
    let proc = a_process(45_555);
    let lp = proc.linux();
    for _ in 0..2 {
        let file: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::RDWR);
        lp.add_file(file).unwrap();
    }
    let dir = ProcSelfFdDir {
        process: proc.clone(),
    };
    let mut seen = Vec::new();
    for id in 2.. {
        let Ok((listed, name)) = dir.get_entry_with_metadata(id) else {
            break;
        };
        let link = dir.find(&name).unwrap().metadata().unwrap();
        assert_eq!(listed.inode, link.inode, "fd {}", name);
        assert_eq!((link.size, link.blk_size), (64, 1024), "fd {}", name);
        assert!(
            link.inode != 0 && !seen.contains(&link.inode),
            "fd {}",
            name
        );
        seen.push(link.inode);
    }
    assert_eq!(seen.len(), lp.get_files().unwrap().len());
    // And another process's descriptor of the same number is another entry.
    let other = a_process(45_556);
    other
        .linux()
        .add_file(EventFd::new(0, OpenFlags::RDWR))
        .unwrap();
    let (theirs, _) = ProcSelfFdDir {
        process: other.clone(),
    }
    .get_entry_with_metadata(2)
    .unwrap();
    assert!(!seen.contains(&theirs.inode));
}

#[test]
fn the_size_of_fd_is_how_many_descriptors_are_open() {
    let proc = a_process(45_557);
    let lp = proc.linux();
    let dir = ProcSelfFdDir {
        process: proc.clone(),
    };
    let before = lp.get_files().unwrap().len();
    assert_eq!(sized(&dir), (before, 0, 1024));
    let fds: Vec<_> = (0..3)
        .map(|_| lp.add_file(EventFd::new(0, OpenFlags::RDWR)).unwrap())
        .collect();
    assert_eq!(dir.metadata().unwrap().size, before + 3);
    lp.close_file(fds[1]).unwrap();
    assert_eq!(dir.metadata().unwrap().size, before + 2);
}

#[test]
fn the_directories_of_proc_have_blocks_of_1024() {
    let proc = a_process(45_558);
    let thread = Thread::create_linux(&proc).unwrap();
    let pid = proc.id();
    let dirs: [Arc<dyn INode>; 6] = [
        Arc::new(ProcPidDirINode { pid }),
        Arc::new(ProcPidTaskDirINode { pid }),
        Arc::new(ProcTidDirINode {
            pid,
            tid: thread.id(),
        }),
        Arc::new(ProcFdInfoDir {
            process: proc.clone(),
        }),
        Arc::new(ProcRootINode),
        ProcRootINode.find("sys").unwrap(),
    ];
    for dir in dirs {
        assert_eq!(sized(&*dir), (0, 0, 1024));
    }
    let fd = proc
        .linux()
        .add_file(EventFd::new(0, OpenFlags::RDWR))
        .unwrap();
    let info = ProcFdInfoDir {
        process: proc.clone(),
    }
    .find(&alloc::format!("{}", i32::from(fd)))
    .unwrap();
    assert_eq!(sized(&*info), (0, 0, 1024));
}

#[test]
fn the_other_files_of_proc_say_size_0_and_blocks_of_1024() {
    for name in ["meminfo", "uptime", "loadavg", "stat", "filesystems"] {
        let file = ProcRootINode.find(name).unwrap();
        assert_eq!(sized(&*file), (0, 0, 1024), "{}", name);
    }
    // A file of `/proc/sys` that only says what it holds, and one that is
    // also written to.
    for name in ["sys/kernel/pid_max", "sys/kernel/hostname"] {
        let file = (&ProcRootINode as &dyn INode).lookup(name).unwrap();
        assert_eq!(sized(&*file), (0, 0, 1024), "{}", name);
    }
}
