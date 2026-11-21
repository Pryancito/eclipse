//! A directory whose file system has no such operation -- `/dev/pts`,
//! `/sys`, `/proc` -- asked to make, link or rename a name in it.
//!
//! `vfs_mkdir`, `vfs_mknod`, `vfs_symlink` and `vfs_rename` answer `EPERM`
//! when the inode has no method for it. Here `mkdir` recursed until the
//! stack ran out (`INode::create` and `create2` each defaulted to the other),
//! and the rest said `ENOSYS`, the errno of a system call that does not
//! exist.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// `S_IFIFO | 0644`.
const A_FIFO: usize = 0o010644;

fn c(s: &str) -> String {
    alloc::format!("{}\0", s)
}

fn at(s: &String) -> UserInPtr<u8> {
    UserInPtr::from(s.as_ptr() as usize)
}

/// `call` alone, on a process of its own: each answer is one process's.
fn on_its_own(pid: KoID, call: fn(&Syscall<'_>) -> SysResult) -> SysResult {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "no-such-op", lp).unwrap();
    on_a_thread(&proc, move |sys| Box::pin(async move { call(sys) }))
}

#[test]
fn mkdir_in_a_directory_that_cannot_make_one_is_eperm_not_a_stack_overflow() {
    assert_eq!(
        on_its_own(49_980, |s| s.sys_mkdirat(
            FileDesc::CWD,
            at(&c("/dev/pts/no_such_op")),
            0o755
        )),
        Err(LxError::EPERM),
        "/dev/pts"
    );
    assert_eq!(
        on_its_own(49_981, |s| s.sys_mkdirat(
            FileDesc::CWD,
            at(&c("/sys/no_such_op")),
            0o755
        )),
        Err(LxError::EPERM),
        "/sys"
    );
}

#[test]
fn mknod_and_symlink_there_are_eperm_too() {
    assert_eq!(
        on_its_own(49_982, |s| s.sys_mknodat(
            FileDesc::CWD,
            at(&c("/dev/pts/no_such_op")),
            A_FIFO,
            0
        )),
        Err(LxError::EPERM),
        "mknod"
    );
    assert_eq!(
        on_its_own(49_983, |s| s.sys_symlinkat(
            at(&c("target")),
            FileDesc::CWD,
            at(&c("/dev/pts/no_such_op"))
        )),
        Err(LxError::EPERM),
        "symlink"
    );
}

#[test]
fn rename_in_proc_is_eperm() {
    assert_eq!(
        on_its_own(49_984, |s| s.sys_renameat2(
            FileDesc::CWD,
            at(&c("/proc/version")),
            FileDesc::CWD,
            at(&c("/proc/no_such_op")),
            0
        )),
        Err(LxError::EPERM)
    );
}

#[test]
fn an_o_creat_there_is_eacces() {
    // `lookup_open`: no `->create` is `EACCES`, which is what `touch` in
    // `/sys` prints on Linux ("Permission denied").
    const O_CREAT_WRONLY: usize = 0o100 | 0o1;
    assert_eq!(
        on_its_own(49_985, |s| s.sys_openat(
            FileDesc::CWD,
            at(&c("/sys/no_such_op")),
            O_CREAT_WRONLY,
            0o644
        )),
        Err(LxError::EACCES)
    );
}
