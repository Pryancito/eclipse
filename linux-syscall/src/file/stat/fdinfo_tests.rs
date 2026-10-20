//! `F_GETFL` and `/proc/self/fdinfo/N` say what Linux 6.18 says, measured
//! there with glibc and musl. `F_GETFL` reported the flags that only matter
//! while opening (`O_CREAT`, `O_EXCL`, `O_TRUNC`, `O_CLOEXEC`) and never the
//! `O_LARGEFILE` every `open` adds on a 64-bit kernel; `fdinfo` did not exist.

use super::pipe_tests::a_process;
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

const O_WRONLY: usize = 1;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_NOCTTY: usize = 0o400;
const O_TRUNC: usize = 0o1000;
const O_APPEND: usize = 0o2000;
const O_NONBLOCK: usize = 0o4000;
const O_DIRECTORY: usize = 0o200_000;
const O_NOFOLLOW: usize = 0o400_000;
const O_NOATIME: usize = 0o1_000_000;
const O_CLOEXEC: usize = 0o2_000_000;
const O_SYNC: usize = 0o4_010_000;
const O_PATH: usize = 0o10_000_000;
const F_GETFL: usize = 3;
const MFD_CLOEXEC: usize = 1;

fn open(sys: &Syscall<'_>, path: &str, flags: usize) -> FileDesc {
    let path = alloc::format!("{}\0", path);
    let fd = sys
        .sys_openat(
            FileDesc::CWD,
            UserInPtr::from(path.as_ptr() as usize),
            flags,
            0o600,
        )
        .unwrap();
    (fd as i32).into()
}

async fn getfl(sys: &Syscall<'_>, fd: FileDesc) -> usize {
    sys.sys_fcntl(fd, F_GETFL, 0).await.unwrap()
}

/// What `cat /proc/self/fdinfo/<fd>` prints.
async fn fdinfo(sys: &Syscall<'_>, fd: FileDesc) -> String {
    let info = open(
        sys,
        &alloc::format!("/proc/self/fdinfo/{}", i32::from(fd)),
        0,
    );
    let mut buf = [0u8; 256];
    let n = sys
        .sys_read(info, UserOutPtr::from(buf.as_mut_ptr() as usize), buf.len())
        .await
        .unwrap();
    String::from(core::str::from_utf8(&buf[..n]).unwrap())
}

#[test]
fn f_getfl_is_what_an_open_file_keeps_of_its_flags() {
    let proc = a_process(45_501);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut got = Vec::new();
            for flags in [
                O_CLOEXEC,
                O_WRONLY | O_CREAT | O_TRUNC | O_APPEND | O_EXCL,
                O_RDWR | O_SYNC | O_NOCTTY,
                O_NOFOLLOW | O_NOATIME,
                O_PATH | O_CLOEXEC,
            ] {
                // `O_EXCL` needs the file not to be there yet.
                let path = alloc::format!("/f{}", got.len());
                if flags & O_CREAT == 0 {
                    open(sys, &path, O_CREAT | O_WRONLY);
                }
                got.push(getfl(sys, open(sys, &path, flags)).await);
            }
            got.push(getfl(sys, open(sys, "/", O_DIRECTORY)).await);
            let mut fds = [0i32; 2];
            sys.sys_pipe2(
                UserOutPtr::from(fds.as_mut_ptr() as usize),
                O_CLOEXEC | O_NONBLOCK,
            )
            .unwrap();
            got.push(getfl(sys, fds[0].into()).await);
            let memfd = sys
                .sys_memfd_create(UserInPtr::from(b"m\0".as_ptr() as usize), MFD_CLOEXEC)
                .unwrap();
            got.push(getfl(sys, (memfd as i32).into()).await);
            got
        })
    });
    assert_eq!(
        got,
        [
            0o100_000,
            0o102_001,
            0o4_110_002,
            0o1_500_000,
            0o10_000_000,
            0o300_000,
            0o4_000,
            0o100_002,
        ]
    );
}

#[test]
fn fdinfo_reads_as_linux_writes_it() {
    let proc = a_process(45_502);
    let (file, ino, eventfd, memfd) = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = open(sys, "/f", O_CREAT | O_RDWR | O_CLOEXEC);
            let ino = super::pipe_tests::fstat(sys, fd).unwrap().1;
            let file = fdinfo(sys, fd).await;
            let efd = (sys.sys_eventfd2(5, O_NONBLOCK).unwrap() as i32).into();
            let eventfd = fdinfo(sys, efd).await;
            let memfd = sys
                .sys_memfd_create(UserInPtr::from(b"m\0".as_ptr() as usize), MFD_CLOEXEC)
                .unwrap();
            let memfd = fdinfo(sys, (memfd as i32).into()).await;
            (file, ino, eventfd, memfd)
        })
    });
    assert_eq!(
        file,
        alloc::format!("pos:\t0\nflags:\t02100002\nmnt_id:\t0\nino:\t{}\n", ino)
    );
    assert!(
        eventfd.starts_with("pos:\t0\nflags:\t04002\n"),
        "{}",
        eventfd
    );
    assert!(
        eventfd.contains("\neventfd-count:                5\n"),
        "{}",
        eventfd
    );
    assert!(memfd.contains("\nflags:\t02100002\n"), "{}", memfd);
}
