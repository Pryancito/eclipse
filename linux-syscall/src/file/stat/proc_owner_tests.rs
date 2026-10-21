//! What a process that is not root may do with its own `/proc/self`, now
//! that those files are owned as on Linux 6.18 (measured there with glibc
//! and musl): by its effective ids, and by root while it is not dumpable.
//! It used to be root's always, so the `comm` of a thread (0644) could not
//! be written by a process that was not root.

use super::pipe_tests::a_process;
use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use linux_object::process::SUID_DUMP_USER;

const O_WRONLY: usize = 1;
const O_DIRECTORY: usize = 0o200_000;
const R_OK: usize = 4;

fn open(sys: &Syscall<'_>, path: &str, flags: usize) -> SysResult {
    let path = format!("{}\0", path);
    sys.sys_openat(
        FileDesc::CWD,
        UserInPtr::from(path.as_ptr() as usize),
        flags,
        0,
    )
}

fn c(path: &str) -> String {
    format!("{}\0", path)
}

/// Uid 1000, 1001, 1002 and gid 2000, 2001, 2002, which leaves the process
/// not dumpable.
fn become_a_user(sys: &Syscall<'_>) {
    sys.linux_process().set_resgid(2000, 2001, 2002).unwrap();
    sys.linux_process().set_resuid(1000, 1001, 1002).unwrap();
}

/// Its `fd` is root's and 0500 then, and still its own to open, list, go
/// into and ask about (`proc_fd_permission`); its `environ`, root's and
/// 0400, is not, until it is dumpable again.
#[test]
fn a_process_that_is_not_dumpable_still_lists_its_own_fd() {
    let proc = a_process(45_587);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            become_a_user(sys);
            let fd = open(sys, "/proc/self/fd", O_DIRECTORY).map(|fd| fd as i32);
            let dir = c("/proc/self/fd");
            let access = sys.sys_faccessat(
                FileDesc::CWD,
                UserInPtr::from(dir.as_ptr() as usize),
                R_OK,
                0,
            );
            let fchdir = match fd {
                Ok(fd) => sys.sys_fchdir(fd.into()),
                Err(e) => Err(e),
            };
            let chdir = sys.sys_chdir(UserInPtr::from(dir.as_ptr() as usize));
            let environ = open(sys, "/proc/self/environ", 0).map(|_| ());
            sys.linux_process().set_dumpable(SUID_DUMP_USER);
            let environ_again = open(sys, "/proc/self/environ", 0).map(|_| ());
            (
                fd.map(|_| ()),
                access,
                fchdir,
                chdir,
                environ,
                environ_again,
            )
        })
    });
    let (fd, access, fchdir, chdir, environ, environ_again) = got;
    assert_eq!(fd, Ok(()), "open");
    assert_eq!(access, Ok(0), "access");
    assert_eq!(fchdir, Ok(0), "fchdir");
    assert_eq!(chdir, Ok(0), "chdir");
    assert_eq!(
        environ,
        Err(LxError::EACCES),
        "its environ while not dumpable"
    );
    assert_eq!(environ_again, Ok(()), "its environ once dumpable again");
}

/// `pthread_setname_np` of a thread other than the caller writes
/// `/proc/self/task/<tid>/comm`, which is 0644 and was root's.
#[test]
fn a_process_that_is_not_root_writes_the_comm_of_its_threads() {
    let proc = a_process(45_588);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            become_a_user(sys);
            sys.linux_process().set_dumpable(SUID_DUMP_USER);
            let tid = sys.sys_gettid().unwrap();
            let comm = open(sys, &format!("/proc/self/task/{}/comm", tid), O_WRONLY)?;
            let name = b"renamed";
            sys.sys_write(
                (comm as i32).into(),
                UserInPtr::from(name.as_ptr() as usize),
                name.len(),
            )
            .await?;
            let comm = open(sys, "/proc/self/comm", 0)?;
            let mut buf = [0u8; 32];
            let n = sys
                .sys_read(
                    (comm as i32).into(),
                    UserOutPtr::from(buf.as_mut_ptr() as usize),
                    buf.len(),
                )
                .await?;
            Ok::<_, LxError>(String::from_utf8_lossy(&buf[..n]).into_owned())
        })
    });
    assert_eq!(got.as_deref(), Ok("renamed\n"));
}
