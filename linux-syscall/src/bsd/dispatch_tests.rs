//! A FreeBSD/amd64 program's calls, made through [`Syscall::bsd_syscall`]
//! with FreeBSD's numbers (`sys/kern/syscalls.master`), flags
//! (`sys/sys/fcntl.h`), structures (`sys/sys/stat.h`, `sys/sys/dirent.h`)
//! and return convention: a positive errno with the carry flag set.
//!
//! The numbers are written out from FreeBSD's headers, never taken from
//! `consts`, so that a wrong constant there fails here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};

// syscalls.master
const READ: usize = 3;
const WRITE: usize = 4;
const OPEN: usize = 5;
const CLOSE: usize = 6;
const GETPID: usize = 20;
const ISSETUGID: usize = 253;
const SIGPROCMASK: usize = 340;
const SIGACTION: usize = 416;
const THR_SELF: usize = 432;
const LSEEK: usize = 478;
const MKDIRAT: usize = 496;
const OPENAT: usize = 499;
const UNLINKAT: usize = 503;
const FSTAT: usize = 551;
const FSTATAT: usize = 552;
const GETDIRENTRIES: usize = 554;

// fcntl.h
const O_RDWR: usize = 0x0002;
const O_APPEND: usize = 0x0008;
const O_CREAT: usize = 0x0200;
const O_EXCL: usize = 0x0800;
const O_DIRECTORY: usize = 0x0002_0000;
const AT_FDCWD: usize = -100isize as usize;
const AT_SYMLINK_NOFOLLOW: usize = 0x0200;
const AT_REMOVEDIR: usize = 0x0800;

// errno.h
const ENOENT: usize = 2;
const EBADF: usize = 9;
const EEXIST: usize = 17;
const ENOTDIR: usize = 20;
const ENOTEMPTY: usize = 66;
const ENOSYS: usize = 78;

fn a_process(pid: KoID) -> alloc::sync::Arc<Process> {
    Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "freebsd",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap()
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// What a FreeBSD libc stub sees: `rax`, `rdx`, and the carry flag.
type Ret = (usize, usize, bool);

async fn call(sys: &mut Syscall<'_>, num: usize, args: [usize; 6]) -> Ret {
    let r = sys.bsd_syscall(num, args).await;
    (r.rax, r.rdx, r.error)
}

fn ok(v: usize) -> Ret {
    (v, 0, false)
}

fn err(errno: usize) -> Ret {
    (errno, 0, true)
}

fn p(s: &'static [u8]) -> usize {
    s.as_ptr() as usize
}

/// `open` with FreeBSD's `O_CREAT | O_EXCL` creates once and is `EEXIST`
/// the second time, with the errno positive and the carry set; a missing
/// file is `ENOENT`; and what is written is read back after an `lseek`.
#[test]
fn open_read_write_and_lseek_speak_freebsds_flags_and_errnos() {
    let proc = a_process(48_960);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = call(
                &mut sys,
                OPEN,
                [p(b"/f\0"), O_RDWR | O_CREAT | O_EXCL, 0o644, 0, 0, 0],
            )
            .await;
            let again = call(
                &mut sys,
                OPEN,
                [p(b"/f\0"), O_RDWR | O_CREAT | O_EXCL, 0o644, 0, 0, 0],
            )
            .await;
            let missing = call(&mut sys, OPEN, [p(b"/nope\0"), O_RDWR, 0, 0, 0, 0]).await;
            let f = fd.0;
            let wrote = call(&mut sys, WRITE, [f, p(b"freebsd"), 7, 0, 0, 0]).await;
            let seek = call(&mut sys, LSEEK, [f, 4, 0, 0, 0, 0]).await;
            let mut buf = [0u8; 8];
            let read = call(&mut sys, READ, [f, buf.as_mut_ptr() as usize, 8, 0, 0, 0]).await;
            // O_APPEND through openat: a write lands at the end.
            let g = call(
                &mut sys,
                OPENAT,
                [AT_FDCWD, p(b"/f\0"), O_RDWR | O_APPEND, 0, 0, 0],
            )
            .await;
            call(&mut sys, WRITE, [g.0, p(b"!"), 1, 0, 0, 0]).await;
            let end = call(&mut sys, LSEEK, [g.0, 0, 2, 0, 0, 0]).await;
            let closed = call(&mut sys, CLOSE, [f, 0, 0, 0, 0, 0]).await;
            let twice = call(&mut sys, CLOSE, [f, 0, 0, 0, 0, 0]).await;
            (
                fd, again, missing, wrote, seek, read, buf, end, closed, twice,
            )
        })
    });
    let (fd, again, missing, wrote, seek, read, buf, end, closed, twice) = got;
    assert!(!fd.2, "open answered errno {}", fd.0);
    assert_eq!(again, err(EEXIST));
    assert_eq!(missing, err(ENOENT));
    assert_eq!(wrote, ok(7));
    assert_eq!(seek, ok(4));
    assert_eq!(read, ok(3));
    assert_eq!(&buf[..3], b"bsd");
    assert_eq!(end, ok(8));
    assert_eq!(closed, ok(0));
    assert_eq!(twice, err(EBADF));
}

/// A number FreeBSD's running kernel does not have is `ENOSYS` (78 there,
/// 38 on Linux); `getpid` answers in `rax`; `issetugid` is 0 for a process
/// that changed no id.
#[test]
fn an_unknown_number_is_freebsds_enosys() {
    let proc = a_process(48_961);
    let pid = proc.id() as usize;
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            (
                call(&mut sys, 9999, [0; 6]).await,
                call(&mut sys, GETPID, [0; 6]).await,
                call(&mut sys, ISSETUGID, [0; 6]).await,
            )
        })
    });
    assert_eq!(got, (err(ENOSYS), ok(pid), ok(0)));
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

/// `fstat` and `fstatat` write FreeBSD 12's `struct stat`: 224 bytes,
/// `st_nlink` a u64 at 16, `st_mode` a u16 at 24, `st_size` at 112 and
/// `st_blksize` at 128; `AT_SYMLINK_NOFOLLOW` (0x200 there, 0x100 on
/// Linux) gives the link itself.
#[test]
fn fstat_writes_freebsds_struct_stat() {
    let proc = a_process(48_962);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = call(
                &mut sys,
                OPEN,
                [p(b"/s\0"), O_RDWR | O_CREAT, 0o640, 0, 0, 0],
            )
            .await
            .0;
            call(&mut sys, WRITE, [fd, p(b"twelve bytes"), 12, 0, 0, 0]).await;
            let mut st = [0xeeu8; 240];
            let r = call(&mut sys, FSTAT, [fd, st.as_mut_ptr() as usize, 0, 0, 0, 0]).await;
            sys.sys_symlinkat(
                UserInPtr::from(p(b"/s\0")),
                FileDesc::CWD,
                UserInPtr::from(p(b"/l\0")),
            )
            .unwrap();
            let mut lst = [0u8; 224];
            let lr = call(
                &mut sys,
                FSTATAT,
                [
                    AT_FDCWD,
                    p(b"/l\0"),
                    lst.as_mut_ptr() as usize,
                    AT_SYMLINK_NOFOLLOW,
                    0,
                    0,
                ],
            )
            .await;
            let mut fst = [0u8; 224];
            let fr = call(
                &mut sys,
                FSTATAT,
                [AT_FDCWD, p(b"/l\0"), fst.as_mut_ptr() as usize, 0, 0, 0],
            )
            .await;
            let bad = call(&mut sys, FSTAT, [77, st.as_mut_ptr() as usize, 0, 0, 0, 0]).await;
            (r, st, lr, lst, fr, fst, bad)
        })
    });
    let (r, st, lr, lst, fr, fst, bad) = got;
    assert_eq!(r, ok(0));
    assert_eq!(&st[224..], [0xee; 16], "224 bytes and no more");
    assert_eq!(u64_at(&st, 16), 1, "st_nlink");
    assert_eq!(u16_at(&st, 24) as u32 & 0o170000, 0o100000, "S_IFREG");
    assert_eq!(u64_at(&st, 112), 12, "st_size");
    assert_ne!(u64_at(&st, 8), 0, "st_ino");
    assert_eq!(lr, ok(0));
    assert_eq!(u16_at(&lst, 24) as u32 & 0o170000, 0o120000, "S_IFLNK");
    assert_eq!(fr, ok(0));
    assert_eq!(u16_at(&fst, 24) as u32 & 0o170000, 0o100000, "followed");
    assert_eq!(u64_at(&fst, 8), u64_at(&st, 8), "the same file");
    assert_eq!(bad, err(EBADF));
}

/// `mkdirat`, then `unlinkat` with FreeBSD's `AT_REMOVEDIR` (0x800 there,
/// 0x200 on Linux): a directory with something in it is `ENOTEMPTY`, an
/// empty one goes.
#[test]
fn unlinkat_removes_a_directory_with_freebsds_flag() {
    let proc = a_process(48_963);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mk = call(&mut sys, MKDIRAT, [AT_FDCWD, p(b"/d\0"), 0o755, 0, 0, 0]).await;
            call(
                &mut sys,
                OPEN,
                [p(b"/d/x\0"), O_RDWR | O_CREAT, 0o644, 0, 0, 0],
            )
            .await;
            let full = call(
                &mut sys,
                UNLINKAT,
                [AT_FDCWD, p(b"/d\0"), AT_REMOVEDIR, 0, 0, 0],
            )
            .await;
            let file = call(&mut sys, UNLINKAT, [AT_FDCWD, p(b"/d/x\0"), 0, 0, 0, 0]).await;
            let empty = call(
                &mut sys,
                UNLINKAT,
                [AT_FDCWD, p(b"/d\0"), AT_REMOVEDIR, 0, 0, 0],
            )
            .await;
            let gone = call(&mut sys, OPEN, [p(b"/d\0"), O_DIRECTORY, 0, 0, 0, 0]).await;
            (mk, full, file, empty, gone)
        })
    });
    assert_eq!(got, (ok(0), err(ENOTEMPTY), ok(0), ok(0), err(ENOENT)));
}

/// `getdirentries` fills FreeBSD's `struct dirent`: `d_fileno` u64 at 0,
/// `d_off` at 8, `d_reclen` u16 at 16, `d_type` at 18, `d_namlen` u16 at
/// 20 and the name at 24, each record a multiple of 8; `*basep` is where
/// the read started; a file is `ENOTDIR`.
#[test]
fn getdirentries_fills_freebsds_dirents() {
    let proc = a_process(48_964);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            call(&mut sys, MKDIRAT, [AT_FDCWD, p(b"/e\0"), 0o755, 0, 0, 0]).await;
            call(
                &mut sys,
                OPEN,
                [p(b"/e/file\0"), O_RDWR | O_CREAT, 0o644, 0, 0, 0],
            )
            .await;
            let dir = call(&mut sys, OPEN, [p(b"/e\0"), O_DIRECTORY, 0, 0, 0, 0])
                .await
                .0;
            let mut buf = [0u8; 512];
            let mut base: i64 = -1;
            let n = call(
                &mut sys,
                GETDIRENTRIES,
                [
                    dir,
                    buf.as_mut_ptr() as usize,
                    buf.len(),
                    &mut base as *mut i64 as usize,
                    0,
                    0,
                ],
            )
            .await;
            let file = call(&mut sys, OPEN, [p(b"/e/file\0"), O_RDWR, 0, 0, 0, 0])
                .await
                .0;
            let not_dir = call(
                &mut sys,
                GETDIRENTRIES,
                [file, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
            )
            .await;
            (n, buf, base, not_dir)
        })
    });
    let (n, buf, base, not_dir) = got;
    assert!(!n.2, "getdirentries answered errno {}", n.0);
    assert_eq!(base, 0, "the first read starts at the start");
    let mut at = 0;
    let mut entries = Vec::new();
    while at < n.0 {
        let reclen = u16_at(&buf, at + 16) as usize;
        let namlen = u16_at(&buf, at + 20) as usize;
        assert_eq!(reclen % 8, 0, "d_reclen {} at {}", reclen, at);
        assert!(reclen >= 24 + namlen + 1, "room for the name and its NUL");
        let name = core::str::from_utf8(&buf[at + 24..at + 24 + namlen]).unwrap();
        assert_eq!(buf[at + 24 + namlen], 0, "{} is NUL-terminated", name);
        entries.push((String::from(name), buf[at + 18]));
        at += reclen;
    }
    assert_eq!(at, n.0, "the records fill exactly what was returned");
    // DT_DIR 4, DT_REG 8.
    assert_eq!(
        entries,
        [
            (String::from("."), 4),
            (String::from(".."), 4),
            (String::from("file"), 8)
        ]
    );
    assert_eq!(not_dir, err(ENOTDIR));
}

/// The out-parameters of the signal calls are written even though the
/// personality delivers no signal: `sigprocmask`'s `oset` is FreeBSD's four
/// `u32`s, `sigaction`'s `oact` 32 bytes of `SIG_DFL`; `thr_self` stores the
/// thread's id.
#[test]
fn the_signal_calls_write_what_they_are_handed() {
    let proc = a_process(48_965);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mut oset = [0xffff_ffffu32; 5];
            let m = call(
                &mut sys,
                SIGPROCMASK,
                [1, 0, oset.as_mut_ptr() as usize, 0, 0, 0],
            )
            .await;
            let mut oact = [0xaau8; 40];
            let a = call(
                &mut sys,
                SIGACTION,
                [2, 0, oact.as_mut_ptr() as usize, 0, 0, 0],
            )
            .await;
            let mut id: i64 = 0;
            let t = call(
                &mut sys,
                THR_SELF,
                [&mut id as *mut i64 as usize, 0, 0, 0, 0, 0],
            )
            .await;
            (m, oset, a, oact, t, id, sys.thread.id())
        })
    });
    let (m, oset, a, oact, t, id, tid) = got;
    assert_eq!(m, ok(0));
    assert_eq!(oset, [0, 0, 0, 0, 0xffff_ffff], "nothing blocked, 16 bytes");
    assert_eq!(a, ok(0));
    assert_eq!(&oact[..32], [0; 32]);
    assert_eq!(&oact[32..], [0xaa; 8]);
    assert_eq!(t, ok(0));
    assert_eq!(id as u64, tid);
}
