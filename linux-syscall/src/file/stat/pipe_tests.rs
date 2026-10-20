//! A pipe is a FIFO to `fstat`, with one inode number for both ends, and
//! `/proc/self/fd/N` names it by that number and reopens it. `fstat` of a
//! pipe failed (`ENOSYS`), the link said `pipe_r:[]` or `pipe_w:[]`, and a
//! reopen through it failed the same way (on Linux that reopen is how `cat
//! /dev/stdin` reads a pipe, `/dev/stdin` being a link to `/proc/self/fd/0`).

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const O_WRONLY: usize = 1;

pub(super) fn a_process(pid: KoID) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "pipes", lp).unwrap()
}

fn pipe(sys: &Syscall<'_>) -> (FileDesc, FileDesc) {
    let mut fds = [0i32; 2];
    sys.sys_pipe2(UserOutPtr::from(fds.as_mut_ptr() as usize), 0)
        .unwrap();
    (fds[0].into(), fds[1].into())
}

/// `(type and mode, ino, uid, gid, blksize)` of `fstat(fd)`.
pub(super) fn fstat(sys: &Syscall<'_>, fd: FileDesc) -> LxResult<(u32, u64, u32, u32, u64)> {
    // Room for a `Stat`, aligned for it; its fields are read back by offset.
    let mut st = [0u64; 32];
    assert!(core::mem::size_of::<Stat>() <= core::mem::size_of_val(&st));
    sys.sys_fstat(fd, UserOutPtr::from(st.as_mut_ptr() as usize))?;
    let bytes: Vec<u8> = st.iter().flat_map(|w| w.to_ne_bytes()).collect();
    let u32_at =
        |at: usize| u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let u64_at = |at: usize| u64::from(u32_at(at)) | u64::from(u32_at(at + 4)) << 32;
    Ok((
        u32_at(core::mem::offset_of!(Stat, mode)),
        u64_at(core::mem::offset_of!(Stat, ino)),
        u32_at(core::mem::offset_of!(Stat, uid)),
        u32_at(core::mem::offset_of!(Stat, gid)),
        u64_at(core::mem::offset_of!(Stat, blksize)),
    ))
}

pub(super) fn readlink(sys: &Syscall<'_>, path: &str) -> LxResult<String> {
    let path = alloc::format!("{}\0", path);
    let mut buf = [0u8; 64];
    let n = sys.sys_readlinkat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        UserOutPtr::from(buf.as_mut_ptr() as usize),
        buf.len(),
    )?;
    Ok(String::from(core::str::from_utf8(&buf[..n]).unwrap()))
}

pub(super) fn fd_link(fd: FileDesc) -> String {
    alloc::format!("/proc/self/fd/{}", i32::from(fd))
}

#[test]
fn fstat_of_a_pipe_is_one_fifo_for_both_ends_owned_by_its_creator() {
    let proc = a_process(45_481);
    proc.linux().set_fsuid(1000);
    proc.linux().set_fsgid(100);
    let (one, two, other) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys);
            let (r2, _) = pipe(sys);
            (fstat(sys, r), fstat(sys, w), fstat(sys, r2))
        })
    });
    let (one, two, other) = (one.unwrap(), two.unwrap(), other.unwrap());
    let fifo = StatMode::FIFO.bits() | 0o600;
    assert_eq!((one.0, two.0), (fifo, fifo));
    assert_eq!(one.1, two.1, "both ends are one inode");
    assert_ne!(one.1, other.1, "and the next pipe another");
    assert_eq!((one.2, one.3, one.4), (1000, 100, 4096));
}

#[test]
fn proc_self_fd_names_a_pipe_by_its_inode() {
    let proc = a_process(45_482);
    let (ino, links) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys);
            let ino = fstat(sys, r).unwrap().1;
            (
                ino,
                [readlink(sys, &fd_link(r)), readlink(sys, &fd_link(w))],
            )
        })
    });
    let want = alloc::format!("pipe:[{}]", ino);
    assert_eq!(links, [Ok(want.clone()), Ok(want)]);
}

#[test]
fn a_pipe_reopened_through_proc_self_fd_is_the_same_pipe() {
    let proc = a_process(45_483);
    let (written, read, name, first) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys);
            let path = alloc::format!("{}\0", fd_link(w));
            let again: FileDesc = sys
                .sys_openat(
                    FileDesc::CWD,
                    UserInPtr::from(path.as_ptr() as usize),
                    O_WRONLY,
                    0,
                )
                .unwrap()
                .into();
            let written = sys
                .sys_write(again, UserInPtr::from(b"x".as_ptr() as usize), 1)
                .await;
            let mut got = [0u8; 4];
            let read = sys
                .sys_read(r, UserOutPtr::from(got.as_mut_ptr() as usize), got.len())
                .await
                .map(|n| got[..n].to_vec());
            (
                written,
                read,
                readlink(sys, &fd_link(again)),
                readlink(sys, &fd_link(w)),
            )
        })
    });
    assert_eq!(written, Ok(1));
    assert_eq!(read, Ok(b"x".to_vec()));
    assert_eq!(name, first, "under the pipe's own name");
}

#[test]
fn a_descriptor_that_is_not_open_is_not_in_proc_self_fd() {
    let proc = a_process(45_484);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let path = b"/proc/self/fd/999\0";
            let mut st = [0u64; 32];
            let stat = sys.sys_fstatat(
                FileDesc::CWD,
                UserInPtr::from(path.as_ptr() as usize),
                UserOutPtr::from(st.as_mut_ptr() as usize),
                0,
            );
            (
                readlink(sys, "/proc/self/fd/999"),
                stat,
                readlink(sys, "/proc/self/fd/x"),
            )
        })
    });
    assert_eq!(
        got,
        (
            Err(LxError::ENOENT),
            Err(LxError::ENOENT),
            Err(LxError::ENOENT)
        )
    );
}

/// `posix_fadvise` on either end is `ESPIPE`, as on a node from `mkfifo`:
/// `generic_fadvise` asks `S_ISFIFO`, which a pipe now answers.
#[test]
fn fadvise_on_a_pipe_is_espipe() {
    let proc = a_process(45_485);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let (r, w) = pipe(sys);
            [sys.sys_fadvise64(r, 0, 0, 0), sys.sys_fadvise64(w, 0, 0, 0)]
        })
    });
    assert_eq!(got, [Err(LxError::ESPIPE), Err(LxError::ESPIPE)]);
}
