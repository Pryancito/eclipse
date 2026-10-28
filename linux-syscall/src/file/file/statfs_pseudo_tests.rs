//! `fstatfs(2)` of a descriptor that is not an opened file, as Linux 6.18
//! answers it (measured there with glibc and musl): a pipe is on pipefs, a
//! socket on sockfs and every anonymous object on anon_inodefs, each with
//! what `simple_statfs()` fills in; a FIFO is on the filesystem its path is
//! on. Only an opened file used to answer: the rest were `EBADF`, and a pipe
//! said ext4.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `include/uapi/linux/magic.h`, written out rather than imported: what is
/// asked is whether the kernel's numbers are these.
const PIPEFS: i64 = 0x5049_5045;
const SOCKFS: i64 = 0x534F_434B;
const ANON_INODEFS: i64 = 0x0904_1934;
const ST_VALID: isize = 0x20;
const CLOCK_MONOTONIC: usize = 1;
const S_IFIFO: usize = 0o010000;
const O_RDWR: usize = 2;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(name, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

fn c(s: &str) -> String {
    format!("{}\0", s)
}

fn at(s: &str) -> UserInPtr<u8> {
    UserInPtr::from(s.as_ptr() as usize)
}

fn fd(n: usize) -> FileDesc {
    (n as i32).into()
}

/// A `struct statfs` for a call to write, with nothing in it yet.
fn blank() -> StatFs {
    let zero = FsInfo {
        bsize: 0,
        frsize: 0,
        blocks: 0,
        bfree: 0,
        bavail: 0,
        files: 0,
        ffree: 0,
        namemax: 0,
        ftype: 0,
    };
    StatFs {
        f_flags: -1,
        ..zero.into()
    }
}

fn out(st: &mut StatFs) -> UserOutPtr<StatFs> {
    UserOutPtr::from(st as *mut StatFs as usize)
}

fn fstatfs(sys: &Syscall<'_>, fd: FileDesc) -> LxResult<StatFs> {
    let mut st = blank();
    sys.sys_fstatfs(fd, out(&mut st))?;
    Ok(st)
}

/// What `simple_statfs()` leaves in a `struct statfs` of `magic`'s: a page
/// for the block and fragment sizes, `NAME_MAX`, `ST_VALID` and nothing
/// counted.
fn assert_pseudo(st: &StatFs, magic: i64, what: &str) {
    assert_eq!(st.f_type, magic, "{}: f_type", what);
    assert_eq!(
        st.f_bsize,
        kernel_hal::PAGE_SIZE as i64,
        "{}: f_bsize",
        what
    );
    assert_eq!(
        st.f_frsize,
        kernel_hal::PAGE_SIZE as isize,
        "{}: f_frsize",
        what
    );
    assert_eq!(st.f_namelen, 255, "{}: f_namelen", what);
    assert_eq!(st.f_flags, ST_VALID, "{}: f_flags", what);
    let counts = [st.f_blocks, st.f_bfree, st.f_bavail, st.f_files, st.f_ffree];
    assert_eq!(counts, [0; 5], "{}: the counts", what);
}

#[test]
fn both_ends_of_a_pipe_are_on_pipefs() {
    let proc = a_process(45_640, "sp_pipe");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut ends = [0i32; 2];
            sys.sys_pipe2(UserOutPtr::from(ends.as_mut_ptr() as usize), 0)
                .unwrap();
            ends.map(|end| fstatfs(sys, end.into()))
        })
    });
    for (end, st) in ["the read end", "the write end"].iter().zip(got) {
        assert_pseudo(&st.unwrap(), PIPEFS, end);
    }
}

#[test]
fn a_socket_is_on_sockfs() {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    let proc = a_process(45_641, "sp_sock");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let mut sv = [0i32; 2];
            let pair = own.sys_socketpair(
                AF_UNIX,
                SOCK_STREAM,
                0,
                UserOutPtr::from(sv.as_mut_ptr() as usize),
            );
            (pair, sv.map(|end| fstatfs(sys, end.into())))
        })
    });
    assert_eq!(got.0, Ok(0), "socketpair");
    for st in got.1 {
        assert_pseudo(&st.unwrap(), SOCKFS, "a socket");
    }
}

#[test]
fn every_anonymous_object_is_on_anon_inodefs() {
    let proc = a_process(45_642, "sp_anon");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mask: u64 = 1 << 9;
            let mut attr = [0u8; 64];
            attr[0] = 1;
            attr[40] = 1;
            let made = [
                ("an eventfd", sys.sys_eventfd2(0, 0)),
                ("an epoll", sys.sys_epoll_create1(0)),
                ("a timerfd", sys.sys_timerfd_create(CLOCK_MONOTONIC, 0)),
                (
                    "a signalfd",
                    sys.sys_signalfd4(
                        (-1i32).into(),
                        UserInPtr::from(&mask as *const u64 as usize),
                        8,
                        0,
                    ),
                ),
                ("an inotify", sys.sys_inotify_init1(0)),
                ("a pidfd", sys.sys_pidfd_open(45_642, 0)),
                (
                    "a perf event",
                    sys.sys_perf_event_open(attr.as_ptr() as usize, 0, -1, -1, 0),
                ),
            ];
            IntoIterator::into_iter(made)
                .map(|(what, made)| (what, fstatfs(sys, fd(made.unwrap()))))
                .collect::<Vec<_>>()
        })
    });
    for (what, st) in got {
        assert_pseudo(&st.unwrap(), ANON_INODEFS, what);
    }
}

#[test]
fn a_fifo_is_on_the_filesystem_its_path_is_on() {
    let proc = a_process(45_643, "sp_fifo");
    let (dir, fifo) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let path = c("/sp_fifo/f");
            sys.sys_mknodat(FileDesc::CWD, at(&path), S_IFIFO | 0o600, 0)
                .unwrap();
            let opened = sys.sys_openat(FileDesc::CWD, at(&path), O_RDWR, 0).unwrap();
            let dir = c("/sp_fifo");
            let mut want = blank();
            sys.sys_statfs(at(&dir), out(&mut want)).unwrap();
            (want, fstatfs(sys, fd(opened)))
        })
    });
    let fifo = fifo.unwrap();
    assert_ne!(fifo.f_type, PIPEFS, "a FIFO was answered as a pipe");
    assert_eq!(fifo.f_type, dir.f_type, "f_type");
    assert_eq!(fifo.f_bsize, dir.f_bsize, "f_bsize");
    assert_eq!(fifo.f_flags, dir.f_flags, "f_flags");
}

#[test]
fn a_descriptor_that_is_not_open_is_ebadf() {
    let proc = a_process(45_644, "sp_badf");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { fstatfs(sys, fd(1000)).map(|_| ()) })
    });
    assert_eq!(got, Err(LxError::EBADF));
}

#[test]
fn the_name_says_which_pseudo_filesystem() {
    let magic = |name: &str| pseudo_fs_of(name).map(|m| m as i64);
    assert_eq!(magic("pipe:[17]"), Some(PIPEFS));
    assert_eq!(magic("socket:[9]"), Some(SOCKFS));
    assert_eq!(magic("anon_inode:[eventfd]"), Some(ANON_INODEFS));
    assert_eq!(magic("anon_inode:inotify"), Some(ANON_INODEFS));
    assert_eq!(magic("/dmabuf:"), Some(0x444D_4142));
    // A path is on what the path is on, whatever its last name says.
    assert_eq!(magic("/tmp/pipe:[17]"), None);
    assert_eq!(magic("/tmp/x"), None);
    assert_eq!(magic("/memfd:buf (deleted)"), None);
}
