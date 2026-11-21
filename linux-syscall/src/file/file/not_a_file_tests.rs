//! A descriptor that is open on something that is not a file -- a socket,
//! an eventfd, an epoll -- handed to a call that wants a file.
//!
//! Linux has an inode behind every descriptor, so the descriptor is open and
//! the answer is what the call says to any inode of the wrong kind, never
//! `EBADF`, which is only for a descriptor that is not open. Every one of
//! these used to be `EBADF`, from `LinuxProcess::get_file`.

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

/// `SYNC_FILE_RANGE_WRITE`.
const SYNC_FILE_RANGE_WRITE: usize = 2;
/// A descriptor number this harness never hands out.
const A_CLOSED_FD: i32 = 100;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.root_inode().create(name, FileType::Dir, 0o755).unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

fn c(s: &str) -> String {
    format!("{}\0", s)
}

/// Every answer a call that wants a file gives to `fd`, by name.
async fn answers(sys: &Syscall<'_>, fd: FileDesc, name: &str) -> Vec<(&'static str, SysResult)> {
    let mut buf = [0u8; 64];
    let dir = c(&format!("{}_d", name));
    alloc::vec![
        (
            "getdents64",
            sys.sys_getdents64(fd, UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len()),
        ),
        ("fchdir", sys.sys_fchdir(fd)),
        (
            "mkdirat",
            sys.sys_mkdirat(fd, UserInPtr::from(dir.as_ptr() as usize), 0o755),
        ),
        ("fsync", sys.sys_fsync(fd).await),
        ("fdatasync", sys.sys_fdatasync(fd).await),
        ("ftruncate", sys.sys_ftruncate(fd, 0)),
        ("fallocate", sys.sys_fallocate(fd, 0, 0, 1)),
        ("readahead", sys.sys_readahead(fd, 0, 1)),
        (
            "sync_file_range",
            sys.sys_sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE),
        ),
        ("syncfs", sys.sys_syncfs(fd)),
    ]
}

/// What Linux 6.18 answers to each call on a socket or an anonymous inode.
const LINUX: [(&str, SysResult); 10] = [
    // `iterate_dir`, `fchdir` and `path_init`: not a directory.
    ("getdents64", Err(LxError::ENOTDIR)),
    ("fchdir", Err(LxError::ENOTDIR)),
    ("mkdirat", Err(LxError::ENOTDIR)),
    // `vfs_fsync_range`: no `->fsync`.
    ("fsync", Err(LxError::EINVAL)),
    ("fdatasync", Err(LxError::EINVAL)),
    // `do_ftruncate`: not a regular file.
    ("ftruncate", Err(LxError::EINVAL)),
    // `vfs_fallocate`: open for writing, and neither a regular file nor a
    // block device.
    ("fallocate", Err(LxError::ENODEV)),
    // `ksys_readahead`: open for reading, and no page cache.
    ("readahead", Err(LxError::EINVAL)),
    // `sync_file_range`: not a file, a block device, a directory or a link.
    ("sync_file_range", Err(LxError::ESPIPE)),
    // `sync_filesystem` of sockfs or anon_inodefs: nothing to write.
    ("syncfs", Ok(0)),
];

fn assert_linux(got: Vec<(&'static str, SysResult)>, what: &str) {
    for ((call, got), (want_call, want)) in got.into_iter().zip(LINUX) {
        assert_eq!(call, want_call);
        assert_eq!(got, want, "{} on {}", call, what);
    }
}

#[test]
fn an_eventfd_is_open_and_not_a_file() {
    let proc = a_process(45_720, "naf_eventfd");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = sys.sys_eventfd2(0, 0).unwrap();
            answers(sys, (fd as i32).into(), "naf_eventfd").await
        })
    });
    assert_linux(got, "an eventfd");
}

#[test]
fn a_socket_is_open_and_not_a_file() {
    const AF_UNIX: usize = 1;
    const SOCK_STREAM: usize = 1;
    let proc = a_process(45_721, "naf_sock");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let mut sv = [0i32; 2];
            own.sys_socketpair(
                AF_UNIX,
                SOCK_STREAM,
                0,
                UserOutPtr::from(sv.as_mut_ptr() as usize),
            )
            .unwrap();
            answers(sys, sv[0].into(), "naf_sock").await
        })
    });
    assert_linux(got, "a socket");
}

#[test]
fn a_pipe_has_no_fsync_and_no_range_to_sync() {
    let proc = a_process(45_722, "naf_pipe");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut ends = [0i32; 2];
            sys.sys_pipe2(UserOutPtr::from(ends.as_mut_ptr() as usize), 0)
                .unwrap();
            let fd: FileDesc = ends[1].into();
            [
                sys.sys_fsync(fd).await,
                sys.sys_fdatasync(fd).await,
                sys.sys_sync_file_range(fd, 0, 0, SYNC_FILE_RANGE_WRITE),
            ]
        })
    });
    assert_eq!(
        got,
        [
            Err(LxError::EINVAL),
            Err(LxError::EINVAL),
            Err(LxError::ESPIPE)
        ]
    );
}

#[test]
fn a_descriptor_that_is_not_open_is_still_ebadf() {
    let proc = a_process(45_723, "naf_closed");
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { answers(sys, A_CLOSED_FD.into(), "naf_closed").await })
    });
    for (call, got) in got {
        assert_eq!(got, Err(LxError::EBADF), "{} on a closed descriptor", call);
    }
}
