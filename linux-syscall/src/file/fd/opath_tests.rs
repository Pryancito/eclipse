//! What an `O_PATH` descriptor may and may not be used for.
//!
//! `O_PATH` opens a place in the tree, not a file: Linux hands it out from
//! `fdget_raw` only, so every call that goes through `fdget` (anything that
//! reads, writes, seeks, maps, syncs, locks, changes the mode or owner of,
//! or asks the driver of the file) answers `EBADF`, and `poll` reports it
//! `POLLNVAL`. What remains is what a path is good for: `fstat`, `fstatfs`,
//! `fchdir`, a `dirfd` for the `*at` calls, `dup`, `close`, and the
//! `fcntl` commands about the descriptor itself.
//!
//! The descriptor used to be a full one here in all but `read`/`write`:
//! `fchmod`, `fchown`, `lseek`, `ioctl`, `flock`, `fsync` and `F_SETFL`
//! all went through, so code that opens with `O_PATH` precisely to make
//! sure nothing touches the file had no such guarantee.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const O_PATH: usize = 0o10000000;
const O_DIRECTORY: usize = 0o200000;
const F_GETFD: usize = 1;
const F_GETFL: usize = 3;
const F_SETFL: usize = 4;
const F_GETLK: usize = 5;
const O_NONBLOCK: usize = 0o4000;
const FIONREAD: usize = 0x541B;
const LOCK_SH: usize = 1;
const PROT_READ: usize = 1;
const MAP_PRIVATE: usize = 2;
const POLLIN: u16 = 0x1;
const POLLNVAL: u16 = 0x20;

/// A process with `/p/f` (five bytes) and the `O_PATH` descriptors of the
/// file and of `/p`.
fn a_process(pid: KoID) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let top = lp.root_inode().create("p", FileType::Dir, 0o755).unwrap();
    let f = top.create("f", FileType::File, 0o644).unwrap();
    f.write_at(0, b"hello").unwrap();
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "opath", lp).unwrap()
}

fn path_fd(sys: &Syscall<'_>, path: &[u8], extra: usize) -> FileDesc {
    let fd = sys
        .sys_openat(
            FileDesc::CWD,
            UserInPtr::from(path.as_ptr() as usize),
            O_PATH | extra,
            0,
        )
        .expect("an O_PATH open of a file that is there");
    FileDesc::from(fd as i32)
}

#[test]
fn an_o_path_descriptor_does_no_io_and_changes_nothing() {
    let proc = a_process(45_740);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = path_fd(sys, b"/p/f\0", 0);
            let mut buf = [0u8; 64];
            let out = UserOutPtr::<u8>::from(buf.as_mut_ptr() as usize);
            let bad = Err(LxError::EBADF);
            assert_eq!(sys.sys_read(fd, out, 5).await, bad, "read");
            assert_eq!(sys.sys_lseek(fd, 0, 0), bad, "lseek");
            assert_eq!(sys.sys_fchmod(fd, 0o600), bad, "fchmod");
            assert_eq!(sys.sys_fchown(fd, usize::MAX, usize::MAX), bad, "fchown");
            assert_eq!(sys.sys_ftruncate(fd, 0), bad, "ftruncate");
            assert_eq!(sys.sys_fadvise64(fd, 0, 0, 0), bad, "fadvise64");
            let meta = sys
                .linux_process()
                .get_file(fd)
                .unwrap()
                .metadata()
                .unwrap();
            assert_eq!(
                (meta.mode & 0o777, meta.size),
                (0o644, 5),
                "the file was touched"
            );
        })
    });
}

#[test]
fn an_o_path_descriptor_is_not_synced_locked_mapped_or_asked_by_ioctl() {
    let proc = a_process(45_741);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = path_fd(sys, b"/p/f\0", 0);
            let mut word = 0u32;
            let bad = Err(LxError::EBADF);
            assert_eq!(sys.sys_fsync(fd).await, bad, "fsync");
            assert_eq!(sys.sys_fdatasync(fd).await, bad, "fdatasync");
            assert_eq!(sys.sys_syncfs(fd), bad, "syncfs");
            assert_eq!(sys.sys_sync_file_range(fd, 0, 0, 0), bad, "sync_file_range");
            assert_eq!(
                sys.sys_ioctl(fd, FIONREAD, &mut word as *mut u32 as usize, 0, 0)
                    .await,
                bad,
                "ioctl"
            );
            assert_eq!(sys.sys_flock(fd, LOCK_SH).await, bad, "flock");
            assert_eq!(
                sys.sys_mmap(0, 4096, PROT_READ, MAP_PRIVATE, fd, 0).await,
                bad,
                "mmap"
            );
            let ep = FileDesc::from(sys.sys_epoll_create1(0).unwrap() as i32);
            let ev = [0u8; 16];
            assert_eq!(
                sys.sys_epoll_ctl(ep, 1, fd, UserInPtr::from(ev.as_ptr() as usize)),
                bad,
                "epoll_ctl"
            );
        })
    });
}

#[test]
fn fcntl_on_an_o_path_descriptor_takes_only_the_descriptor_commands() {
    let proc = a_process(45_742);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = path_fd(sys, b"/p/f\0", 0);
            assert_eq!(sys.sys_fcntl(fd, F_GETFD, 0).await, Ok(0));
            assert_eq!(sys.sys_fcntl(fd, F_GETFL, 0).await, Ok(O_PATH));
            assert_eq!(
                sys.sys_fcntl(fd, F_SETFL, O_NONBLOCK).await,
                Err(LxError::EBADF),
                "F_SETFL changed an O_PATH descriptor"
            );
            assert_eq!(sys.sys_fcntl(fd, F_GETFL, 0).await, Ok(O_PATH));
            let mut lock = [0u8; 32];
            assert_eq!(
                sys.sys_fcntl(fd, F_GETLK, lock.as_mut_ptr() as usize).await,
                Err(LxError::EBADF),
                "a record lock was asked of an O_PATH descriptor"
            );
        })
    });
}

#[test]
fn what_a_path_is_good_for_still_works_through_an_o_path_descriptor() {
    let proc = a_process(45_743);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = path_fd(sys, b"/p/f\0", 0);
            let dir = path_fd(sys, b"/p\0", O_DIRECTORY);
            let mut buf = [0u8; 256];
            assert_eq!(
                sys.sys_fstat(fd, UserOutPtr::from(buf.as_mut_ptr() as usize)),
                Ok(0)
            );
            assert_eq!(
                sys.sys_fstatfs(fd, UserOutPtr::from(buf.as_mut_ptr() as usize)),
                Ok(0)
            );
            assert!(sys.sys_dup(fd).is_ok(), "dup");
            // A dirfd for the `*at` family.
            let name = b"f\0";
            assert!(
                sys.sys_openat(dir, UserInPtr::from(name.as_ptr() as usize), 0, 0)
                    .is_ok(),
                "openat relative to an O_PATH directory"
            );
            assert_eq!(sys.sys_fchdir(dir), Ok(0), "fchdir");
        })
    });
}

#[test]
fn poll_reports_an_o_path_descriptor_as_pollnval() {
    let proc = a_process(45_744);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let fd = path_fd(sys, b"/p/f\0", 0);
            // `struct pollfd`: int fd, short events, short revents.
            let mut entry = [i32::from(fd), POLLIN as i32];
            let mut own = Syscall {
                thread: sys.thread,
                thread_fn: sys.thread_fn,
                syscall_entry: sys.syscall_entry,
            };
            let n = own
                .sys_poll(UserInOutPtr::from(entry.as_mut_ptr() as usize), 1, 0)
                .await;
            assert_eq!(n, Ok(1));
            assert_eq!((entry[1] as u32 >> 16) as u16, POLLNVAL, "revents");
        })
    });
}
