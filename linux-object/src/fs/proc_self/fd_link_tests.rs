//! An entry of `/proc/<pid>/fd` for every descriptor, named as Linux names
//! it. Only a `File` had one: a socket's or an eventfd's entry was listed and
//! then `ENOENT` when looked at, a file with no name left lost its
//! ` (deleted)`, and every link had the mode and size of an ordinary one.

extern crate std;

use super::*;
use crate::fs::file::File;
use crate::fs::{anon_metadata, new_memfd, EventFd, Inotify, OpenFlags};
use crate::net::UnixSocketState;
use crate::process::LinuxProcess;
use alloc::format;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::Job;

fn a_process(pid: u64) -> Arc<Process> {
    Process::create_with_fixed_id_ext(
        &Job::root(),
        pid,
        "fdlinks",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

fn text(link: &dyn INode) -> String {
    let mut buf = [0u8; 128];
    let n = link.read_at(0, &mut buf).unwrap();
    String::from(core::str::from_utf8(&buf[..n]).unwrap())
}

/// `(text, type, mode, size)` of the entry for `file` in its process's
/// `fd` directory.
fn entry(proc: &Arc<Process>, file: Arc<dyn FileLike>) -> (String, FileType, u16, usize) {
    let fd: i32 = proc.linux().add_file(file).unwrap().into();
    let dir = ProcSelfFdDir {
        process: proc.clone(),
    };
    let link = dir.find(&fd.to_string()).unwrap();
    let meta = link.metadata().unwrap();
    (text(&*link), meta.type_, meta.mode, meta.size)
}

#[test]
fn a_descriptor_that_is_no_file_has_an_entry_under_its_own_name() {
    let proc = a_process(7711);
    let eventfd = entry(&proc, EventFd::new(0, OpenFlags::RDWR));
    let inotify = entry(&proc, Inotify::new(OpenFlags::RDONLY));
    assert_eq!(
        eventfd,
        ("anon_inode:[eventfd]".into(), FileType::SymLink, 0o700, 64)
    );
    assert_eq!(
        inotify,
        ("anon_inode:inotify".into(), FileType::SymLink, 0o500, 64)
    );
    // Read from the middle, the rest of the name; from past its end, nothing.
    let eventfd: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::RDWR);
    let link = FdLink::of(7712, 3.into(), &eventfd);
    let mut buf = [0u8; 16];
    assert_eq!(
        link.read_at(11, &mut buf).map(|n| &buf[..n]),
        Ok(&b"[eventfd]"[..])
    );
    assert_eq!(link.read_at(64, &mut buf), Ok(0));
}

#[test]
fn the_link_of_a_descriptor_says_how_it_was_opened() {
    let inode: Arc<dyn INode> = Arc::new(Pseudo::new("x", FileType::File));
    let mode = |flags| {
        let file: Arc<dyn FileLike> = File::new(inode.clone(), flags, String::from("/x"));
        FdLink::of(7713, 3.into(), &file).metadata().unwrap().mode
    };
    assert_eq!(
        [
            mode(OpenFlags::RDONLY),
            mode(OpenFlags::WRONLY),
            mode(OpenFlags::RDWR),
            mode(OpenFlags::PATH | OpenFlags::RDWR),
        ],
        [0o500, 0o300, 0o700, 0]
    );
}

#[test]
fn a_file_with_no_name_left_is_deleted() {
    let fs = RamFS::new();
    let root = fs.root_inode();
    let inode = root.create("f", FileType::File, 0o600).unwrap();
    let file = File::new(inode, OpenFlags::RDONLY, String::from("/f"));
    let before = file.link_name();
    root.unlink("f").unwrap();
    assert_eq!(
        (before, file.link_name()),
        (String::from("/f"), String::from("/f (deleted)"))
    );
    let memfd = new_memfd("hello", 0).unwrap();
    assert_eq!(memfd.link_name(), "/memfd:hello (deleted)");
}

#[test]
fn a_socket_is_named_by_the_inode_fstat_gives_it() {
    let socket = UnixSocketState::new();
    let ino = socket.metadata().unwrap().inode;
    assert_eq!(socket.link_name(), format!("socket:[{}]", ino));
    // Whatever names itself nothing and is no socket is named by its kind.
    assert_eq!(
        default_link_name(Ok(anon_metadata(FileType::File, ino)), "Thing"),
        "anon_inode:[Thing]"
    );
}

/// `getdents` holds the lock of the directory it lists for the whole call,
/// and an open `/proc/self/fd` is one of its own entries: naming that entry
/// took the same lock again, and `ls -l /proc/self/fd` hung for good.
#[test]
fn listing_the_fd_directory_does_not_look_at_its_own_descriptor() {
    let proc = a_process(7712);
    let dir: Arc<dyn INode> = Arc::new(ProcSelfFdDir {
        process: proc.clone(),
    });
    let file = File::new(dir, OpenFlags::RDONLY, String::from("/proc/self/fd"));
    let fd: i32 = proc.linux().add_file(file.clone()).unwrap().into();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut entries = Vec::new();
        while let Ok((meta, name)) = file.read_entry_with_metadata() {
            entries.push((name, meta.type_));
        }
        let _ = tx.send(entries);
    });
    let entries = rx
        .recv_timeout(core::time::Duration::from_secs(3))
        .expect("the listing never ended");
    // `.`, `..`, then the descriptors the process starts with and the
    // directory's own, each a link.
    let mut want = alloc::vec![
        (String::from("."), FileType::Dir),
        (String::from(".."), FileType::Dir),
    ];
    want.extend((0..=fd).map(|n| (n.to_string(), FileType::SymLink)));
    assert_eq!(entries, want);
}
