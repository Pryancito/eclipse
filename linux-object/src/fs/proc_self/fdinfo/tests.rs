//! `/proc/<pid>/fdinfo/N` as Linux 6.18 writes it, measured there with glibc
//! and musl. The directory did not exist.
//!
//! Fixed pids live in 7721..=7729 here.

extern crate std;

use super::*;
use crate::fs::{EventFd, FileLike, PidFd, SeekFrom, SignalFd};
use crate::process::{wait_child, wait_status_exited, LinuxProcess};
use crate::signal::Signal;
use crate::thread::ThreadExt;
use alloc::string::ToString;
use alloc::vec::Vec;
use async_std::task::block_on;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Job, Thread};

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &Job::root(),
        pid,
        "fdinfo",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

fn dir(proc: &Arc<Process>) -> ProcFdInfoDir {
    ProcFdInfoDir {
        process: proc.clone(),
    }
}

fn read(info: &dyn INode) -> Result<String> {
    let mut buf = [0u8; 512];
    let n = info.read_at(0, &mut buf)?;
    Ok(String::from(core::str::from_utf8(&buf[..n]).unwrap()))
}

/// The text of fdinfo for `file`, once it is a descriptor of `proc`.
fn info_of(proc: &Arc<Process>, file: Arc<dyn FileLike>) -> String {
    let fd: i32 = proc.linux().add_file(file).unwrap().into();
    read(&*dir(proc).find(&fd.to_string()).unwrap()).unwrap()
}

#[test]
fn a_file_s_lines_are_its_offset_its_status_flags_and_its_inode() {
    let proc = a_process(7721);
    let inode = RamFS::new()
        .root_inode()
        .create("f", FileType::File, 0o644)
        .unwrap();
    inode.write_at(0, b"0123456789").unwrap();
    let ino = inode.metadata().unwrap().inode;
    // The creation flags went into `F_GETFL` here; Linux keeps none of them.
    let flags = OpenFlags::RDWR
        | OpenFlags::CREATE
        | OpenFlags::EXCLUSIVE
        | OpenFlags::NOCTTY
        | OpenFlags::TRUNCATE
        | OpenFlags::APPEND;
    let file = File::new(inode, flags, String::from("/f"));
    file.seek(SeekFrom::Start(7)).unwrap();
    let fd = proc.linux().add_file(file).unwrap();
    let info = dir(&proc).find(&i32::from(fd).to_string()).unwrap();
    assert_eq!(
        read(&*info).unwrap(),
        alloc::format!("pos:\t7\nflags:\t02002\nmnt_id:\t0\nino:\t{ino}\n")
    );
    // `O_CLOEXEC` is the descriptor's, and fdinfo shows it with the rest.
    proc.linux().set_fd_cloexec(fd, true).unwrap();
    assert!(read(&*info).unwrap().contains("flags:\t02002002\n"));
    // Read from the middle, the rest; from past the end, nothing.
    let mut buf = [0u8; 8];
    assert_eq!(
        info.read_at(5, &mut buf).map(|n| &buf[..n]),
        Ok(&b"7\nflags:"[..])
    );
    assert_eq!(info.read_at(4096, &mut buf), Ok(0));
}

#[test]
fn an_eventfd_tells_its_count_its_id_and_whether_it_is_a_semaphore() {
    let proc = a_process(7722);
    // `EFD_SEMAPHORE` is bit 0 of the flags word.
    let efd = EventFd::new(0x2a, OpenFlags::from_bits_truncate(1));
    let id = efd.id();
    assert_eq!(
        info_of(&proc, efd),
        alloc::format!(
            "pos:\t0\nflags:\t02\nmnt_id:\t0\nino:\t{id}\n\
             eventfd-count:               2a\neventfd-id: {id}\neventfd-semaphore: 1\n"
        )
    );
    let plain = info_of(
        &proc,
        EventFd::new(0, OpenFlags::RDWR | OpenFlags::NON_BLOCK),
    );
    assert!(plain.contains("flags:\t04002\n"), "{}", plain);
    assert!(
        plain.contains("eventfd-count:                0\n"),
        "{}",
        plain
    );
    assert!(plain.ends_with("eventfd-semaphore: 0\n"), "{}", plain);
}

#[test]
fn a_pidfd_names_its_pid_until_the_pid_is_reaped() {
    let parent = a_process(7723);
    Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let pid = child.id();
    let fd = parent
        .linux()
        .add_file(PidFd::new(child.clone(), OpenFlags::RDWR))
        .unwrap();
    let info = dir(&parent).find(&i32::from(fd).to_string()).unwrap();
    let tail = |text: String| {
        String::from(
            text.split_once("ino:")
                .unwrap()
                .1
                .split_once('\n')
                .unwrap()
                .1,
        )
    };
    assert_eq!(
        tail(read(&*info).unwrap()),
        alloc::format!("Pid:\t{pid}\nNSpid:\t{pid}\n")
    );
    child.exit(0);
    drop(child);
    // A zombie still has its pid.
    assert_eq!(
        tail(read(&*info).unwrap()),
        alloc::format!("Pid:\t{pid}\nNSpid:\t{pid}\n")
    );
    assert_eq!(
        block_on(wait_child(&parent, pid, true, true)).map(|(status, _)| status),
        Ok(wait_status_exited(0))
    );
    assert_eq!(tail(read(&*info).unwrap()), "Pid:\t-1\nNSpid:\t-1\n");
}

#[test]
fn a_signalfd_shows_the_signals_it_accepts() {
    let proc = a_process(7724);
    let mask = Signal::SIGUSR1.as_bit() | Signal::SIGTERM.as_bit() | Signal::SIGKILL.as_bit();
    let text = info_of(&proc, SignalFd::new(mask, OpenFlags::RDWR));
    // SIGKILL is never accepted, so it is not shown either.
    assert!(text.ends_with("\nsigmask:\t0000000000004200\n"), "{}", text);
}

#[test]
fn a_descriptor_closed_since_its_file_was_opened_is_gone() {
    let proc = a_process(7725);
    let fd = proc
        .linux()
        .add_file(EventFd::new(0, OpenFlags::RDWR))
        .unwrap();
    let name = i32::from(fd).to_string();
    let info = dir(&proc).find(&name).unwrap();
    proc.linux().close_file(fd).unwrap();
    assert_eq!(read(&*info).unwrap_err(), FsError::EntryNotFound);
    assert_eq!(dir(&proc).find(&name).err(), Some(FsError::EntryNotFound));
    assert_eq!(dir(&proc).find("x").err(), Some(FsError::EntryNotFound));
}

/// Each file is its own inode, so `diff` of two of them reads both.
#[test]
fn no_two_files_of_the_directory_are_one_inode() {
    let a = a_process(7726);
    let b = a_process(7727);
    let mut inodes = Vec::new();
    for proc in [&a, &b] {
        for fd in [0, 1, 2] {
            inodes.push(
                dir(proc)
                    .find(&fd.to_string())
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .inode,
            );
        }
        inodes.push(dir(proc).metadata().unwrap().inode);
    }
    let mut unique = inodes.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), inodes.len(), "{:?}", inodes);
}

/// The listing is `/proc/<pid>/fd`'s, of files, and it does not look at the
/// descriptors: an open `/proc/self/fdinfo` is one of them, and `getdents`
/// holds its lock while it lists.
#[test]
fn listing_the_directory_does_not_look_at_its_own_descriptor() {
    let proc = a_process(7728);
    let inode: Arc<dyn INode> = Arc::new(dir(&proc));
    let file = File::new(inode, OpenFlags::RDONLY, String::from("/proc/self/fdinfo"));
    let fd: i32 = proc.linux().add_file(file.clone()).unwrap().into();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut entries = Vec::new();
        while let Ok((meta, name)) = file.read_entry_with_metadata() {
            entries.push((name, meta.type_, meta.mode));
        }
        let _ = tx.send(entries);
    });
    let entries = rx
        .recv_timeout(core::time::Duration::from_secs(3))
        .expect("the listing never ended");
    let mut want = alloc::vec![
        (String::from("."), FileType::Dir, 0o555),
        (String::from(".."), FileType::Dir, 0o555),
    ];
    want.extend((0..=fd).map(|n| (n.to_string(), FileType::File, 0o444)));
    assert_eq!(entries, want);
}
