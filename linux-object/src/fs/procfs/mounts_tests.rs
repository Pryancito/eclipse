//! `/proc/<pid>/mounts` and `/proc/<pid>/mountinfo`, which every process
//! has and Linux 6.18 refuses to open (`EINVAL`) once it has left its
//! namespace.
//!
//! Fixed pids live in 49_56x here.

use super::*;
use crate::process::LinuxProcess;
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "mnt", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

fn read(pid: u64, name: &str) -> String {
    let file = ProcPidDirINode { pid }.find(name).unwrap();
    let mut buf = alloc::vec![0u8; 16384];
    let len = file.read_at(0, &mut buf).unwrap();
    String::from_utf8(buf[..len].to_vec()).unwrap()
}

#[test]
fn every_process_lists_both_files_for_anyone_to_read() {
    let proc = a_process(49_561);
    let dir = ProcPidDirINode { pid: proc.id() };
    let names = ProcPidDirINode::entries();
    for name in ["mounts", "mountinfo"] {
        assert!(names.contains(&name), "{} is not listed", name);
        let m = dir.find(name).unwrap().metadata().unwrap();
        assert_eq!((m.type_, m.mode), (FileType::File, 0o444), "{name}");
    }
    // The live table is anybody's to clear, so only its shape is asked.
    let info = read(proc.id(), "mountinfo");
    for line in info.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        assert!(fields.len() >= 10 && fields.contains(&"-"), "{}", line);
    }
    assert!(read(proc.id(), "mounts")
        .lines()
        .all(|l| l.ends_with(" 0 0")));
    proc.exit(0);
}

#[test]
fn a_zombie_and_a_leader_gone_first_cannot_open_them() {
    let parent = a_process(49_562);
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    let _thread = Thread::create_linux(&child).unwrap();
    assert!(ProcPidDirINode { pid }.find("mounts").is_ok());
    child.exit(0);
    for name in ["mounts", "mountinfo"] {
        assert_eq!(
            ProcPidDirINode { pid }.find(name).err(),
            Some(FsError::InvalidParam),
            "a zombie's {name}"
        );
    }

    let proc = a_process(49_563);
    let leader = Thread::create_linux(&proc).unwrap();
    let _worker = Thread::create_linux(&proc).unwrap();
    proc.linux().keep_exited_leader(leader.clone());
    leader.terminate_abandoned();
    for name in ["mounts", "mountinfo"] {
        assert_eq!(
            ProcPidDirINode { pid: proc.id() }.find(name).err(),
            Some(FsError::InvalidParam),
            "{name} with its leader gone"
        );
    }
    drop(proc.linux().release_exited_leader());
    proc.exit(0);
    parent.exit(0);
}
