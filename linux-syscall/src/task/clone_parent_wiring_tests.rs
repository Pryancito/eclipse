//! `clone` without `CLONE_VM`, through the syscall: a `CLONE_PARENT` child
//! is the caller's sibling, and `CLONE_SYSVSEM` is refused like the other
//! sharing a plain fork cannot do.
//!
//! Fixed pids live in 44_41x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use linux_object::signal::Signal as LinuxSignal;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A fork-like `clone` of `flags`, with nothing else asked.
async fn clone(sys: &Syscall<'_>, flags: usize) -> SysResult {
    sys.sys_clone(flags, 0, UserOutPtr::from(0), 0, UserOutPtr::from(0))
        .await
}

/// Asked while the caller lives: its death hands what it has to init.
#[test]
fn a_clone_parent_child_is_the_callers_parents_and_not_the_callers() {
    let top = a_process(44_411, "top");
    let caller = Process::fork_from(&top).unwrap();
    on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let pid = clone(
                sys,
                CloneFlags::PARENT.bits() | LinuxSignal::SIGUSR1 as usize,
            )
            .await
            .expect("CLONE_PARENT is a fork");
            assert!(
                top.linux().has_child(pid as KoID),
                "its parent does not have it"
            );
            assert!(
                !sys.linux_process().has_child(pid as KoID),
                "the caller has it"
            );
            assert_eq!(
                sys.sys_wait4(pid as i32, UserOutPtr::from(0), 0, UserOutPtr::from(0))
                    .await,
                Err(LxError::ECHILD),
                "the caller could wait for its sibling"
            );
        })
    });
}

#[test]
fn a_plain_fork_is_still_the_callers() {
    let top = a_process(44_412, "top");
    let caller = Process::fork_from(&top).unwrap();
    on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let pid = clone(sys, LinuxSignal::SIGCHLD as usize).await.unwrap() as KoID;
            assert!(sys.linux_process().has_child(pid));
            assert!(!top.linux().has_child(pid));
        })
    });
}

/// A child's `semop(.., SEM_UNDO)` was undone when the child exited, where
/// Linux keeps it on the list it shares with its parent.
#[test]
fn clone_sysvsem_without_a_thread_is_refused() {
    let proc = a_process(44_413, "sysvsem");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                clone(
                    sys,
                    CloneFlags::SYSVSEM.bits() | LinuxSignal::SIGCHLD as usize
                )
                .await,
                Err(LxError::EINVAL)
            );
        })
    });
}

/// Init has no parent to give a sibling to. The one test in this binary
/// that makes a process with pid 1, which is global: the suite runs one
/// test at a time, and the process is gone when its thread is.
#[test]
fn init_cannot_make_a_sibling() {
    let init = a_process(zircon_object::task::INIT_PID, "init");
    on_a_thread(&init, move |sys| {
        Box::pin(async move {
            assert_eq!(
                clone(
                    sys,
                    CloneFlags::PARENT.bits() | LinuxSignal::SIGCHLD as usize
                )
                .await,
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_wait4(-1, UserOutPtr::from(0), 1, UserOutPtr::from(0))
                    .await,
                Err(LxError::ECHILD),
                "it forked before it refused"
            );
        })
    });
}
