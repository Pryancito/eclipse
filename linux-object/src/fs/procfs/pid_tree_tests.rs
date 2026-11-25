//! `/proc/<pid>` and `/proc/<pid>/task/<tid>`, walked: what `ps`, `top`,
//! `pgrep` and `htop` find in them, as `fs/proc/base.c` lays them out.
//!
//! Fixed pids live in 45_8xx here.

use super::*;
use crate::process::LinuxProcess;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "walk", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn listing(node: &Arc<dyn INode>) -> Vec<String> {
    let mut names = Vec::new();
    while let Ok(name) = node.get_entry(names.len()) {
        names.push(name);
        assert!(names.len() < 4096);
    }
    names
}

/// Every name `/proc/<pid>` lists can be looked up, its directories list
/// `.` and `..` first, and nothing in it but the links is of another filesystem.
#[test]
fn every_entry_of_a_process_directory_is_there() {
    let proc = a_process(45_801);
    let _thread = Thread::create_linux(&proc).unwrap();
    let dir: Arc<dyn INode> = Arc::new(ProcPidDirINode { pid: proc.id() });
    let names = listing(&dir);
    assert_eq!(&names[..2], [".", ".."]);
    for name in &names[2..] {
        let node = dir
            .find(name)
            .unwrap_or_else(|e| panic!("/proc/<pid>/{} listed but {:?}", name, e));
        let md = node.metadata().unwrap();
        if md.type_ == FileType::SymLink {
            // exe, cwd, root: statfs follows the link, on Linux too.
            continue;
        }
        assert_eq!(
            node.fs().info().ftype,
            rcore_fs::vfs::PROC_SUPER_MAGIC,
            "{}",
            name
        );
        if md.type_ == FileType::Dir {
            let inner = listing(&node);
            assert_eq!(&inner[..2], [".", ".."], "{}", name);
            assert!(node.write_at(0, b"x").is_err(), "{}", name);
        }
    }
    let md = dir.metadata().unwrap();
    assert_eq!(md.type_, FileType::Dir);
    assert_eq!(dir.find(".").unwrap().metadata().unwrap().inode, md.inode);
    assert_eq!(
        dir.find("..").unwrap().metadata().unwrap().inode,
        10,
        "/proc"
    );
    assert_eq!(dir.find("nope").err(), Some(FsError::EntryNotFound));
    assert!(dir.write_at(0, b"x").is_err());
    assert!(dir.poll().unwrap().read);
}

/// `/proc/<pid>/task` lists one directory per thread, named by its tid,
/// and each has its own `stat`, `status` and `comm`; a tid that is not
/// one of the process's is not there.
#[test]
fn the_task_directory_has_one_entry_per_thread() {
    let proc = a_process(45_802);
    let a = Thread::create_linux(&proc).unwrap();
    let b = Thread::create_linux(&proc).unwrap();
    let pid_dir: Arc<dyn INode> = Arc::new(ProcPidDirINode { pid: proc.id() });
    let task = pid_dir.find("task").unwrap();
    let names = listing(&task);
    assert_eq!(&names[..2], [".", ".."]);
    let mut tids: Vec<u64> = names[2..].iter().map(|n| n.parse().unwrap()).collect();
    tids.sort_unstable();
    let mut want = vec![a.id(), b.id()];
    want.sort_unstable();
    assert_eq!(tids, want);
    for tid in &tids {
        let t = task.find(&format!("{}", tid)).unwrap();
        assert_eq!(t.metadata().unwrap().type_, FileType::Dir);
        let inner = listing(&t);
        assert_eq!(&inner[..2], [".", ".."]);
        for name in &inner[2..] {
            assert!(t.find(name).is_ok(), "task/{}/{}", tid, name);
        }
        for name in ["stat", "status", "comm"] {
            let f = t.find(name).unwrap();
            let mut buf = [0u8; 4096];
            let n = f.read_at(0, &mut buf).unwrap();
            assert!(n > 0 && buf[n - 1] == b'\n', "task/{}/{}", tid, name);
            assert_eq!(f.fs().info().ftype, rcore_fs::vfs::PROC_SUPER_MAGIC);
        }
        // `stat` begins with the tid, not the pid.
        let mut buf = [0u8; 64];
        let n = t.find("stat").unwrap().read_at(0, &mut buf).unwrap();
        let first = core::str::from_utf8(&buf[..n])
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        assert_eq!(first, format!("{}", tid));
        assert_eq!(
            t.find("..").unwrap().metadata().unwrap().inode,
            task.metadata().unwrap().inode
        );
        assert!(t.write_at(0, b"x").is_err());
    }
    let other = a_process(45_803);
    let stranger = Thread::create_linux(&other).unwrap();
    assert!(
        task.find(&format!("{}", stranger.id())).is_err(),
        "another's thread"
    );
    assert!(task.find("x").is_err());
    assert!(task.write_at(0, b"x").is_err());
}

/// `/proc/<pid>` of a pid that is not a process is not there, and is not
/// listed.
#[test]
fn a_pid_that_is_no_process_is_not_in_proc() {
    let proc = a_process(45_804);
    let pid = proc.id();
    let root = PROC_ROOT.clone();
    assert!(root.find(&format!("{}", pid)).is_ok());
    assert!(listing(&root).contains(&format!("{}", pid)));
    assert!(root.find("45899").is_err());
}

/// `/proc/net` lists what is in it and each name opens; `/proc/self/net`
/// is the same directory.
#[test]
fn every_name_of_proc_net_opens() {
    let net = PROC_ROOT.find("net").unwrap();
    let names = listing(&net);
    assert_eq!(&names[..2], [".", ".."]);
    for want in ["dev", "route", "tcp", "udp", "unix", "arp"] {
        assert!(names.iter().any(|n| n == want), "no /proc/net/{}", want);
    }
    for name in &names[2..] {
        let node = net
            .find(name)
            .unwrap_or_else(|e| panic!("net/{}: {:?}", name, e));
        let mut buf = [0u8; 8192];
        node.read_at(0, &mut buf).unwrap();
    }
    assert!(net.find("nope").is_err());
    assert!(net.write_at(0, b"x").is_err());
    assert_eq!(net.find("..").unwrap().metadata().unwrap().inode, 10);
}
