//! `RLIMIT_NPROC` was stored, read back by `getrlimit` and held nowhere:
//! with `ulimit -u` set, a user's `fork`, `vfork` and `pthread_create` went
//! on succeeding past it, so the limit a fork bomb is meant to hit was not
//! there. Each call is made here as a program makes it, from a thread of a
//! process of a user of its own, whose harness thread is its first task.
//!
//! Fixed pids live in 45_32x here.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{LinuxProcess, RLimit, RLIMIT_NPROC};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD`.
const A_THREAD: usize = 0x10f00;

/// A process of `uid`, held to `tasks` of them.
fn a_process_of(pid: KoID, uid: u32, tasks: u64) -> Arc<Process> {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "t", LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    let limit = RLimit {
        cur: tasks,
        max: tasks,
    };
    proc.linux()
        .rlimit(RLIMIT_NPROC, Some(limit), true)
        .unwrap();
    proc.linux().set_resuid(uid, uid, uid).unwrap();
    proc
}

async fn a_thread(sys: &Syscall<'_>) -> SysResult {
    sys.sys_clone(A_THREAD, 0, 0usize.into(), 0, 0usize.into())
        .await
}

#[test]
fn fork_vfork_and_a_thread_are_each_refused_at_the_limit() {
    let proc = a_process_of(45_321, 45_321, 1);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            (
                sys.sys_fork(0, 0),
                sys.sys_vfork(0, 0).await,
                a_thread(sys).await,
            )
        })
    });
    assert_eq!(
        got,
        (
            Err(LxError::EAGAIN),
            Err(LxError::EAGAIN),
            Err(LxError::EAGAIN)
        )
    );
}

/// Under the limit a fork goes through; its child is a task of the user
/// until it is reaped, zombie included, and then the room is back.
#[test]
fn a_child_takes_its_room_until_it_is_reaped() {
    let proc = a_process_of(45_322, 45_322, 2);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let child = sys.sys_fork(0, 0)?;
            // The child has nothing to run and exits; wait for it to be a
            // zombie, which is a task Linux still counts.
            let zombie = (0..2000).any(|_| {
                let zombie = sys.linux_process().is_zombie_child(child as KoID);
                if !zombie {
                    std::thread::sleep(core::time::Duration::from_millis(1));
                }
                zombie
            });
            let while_zombie = sys.sys_fork(0, 0);
            sys.sys_wait4(child as i32, 0usize.into(), 0, 0usize.into())
                .await?;
            let reaped = sys.sys_fork(0, 0).map(|_| ());
            Ok::<_, LxError>((zombie, while_zombie, reaped))
        })
    });
    assert_eq!(got, Ok((true, Err(LxError::EAGAIN), Ok(()))));
}

/// `p->real_cred->user != INIT_USER`: root forks past any limit.
#[test]
fn root_forks_past_a_limit_of_zero() {
    let proc = a_process_of(45_323, 0, 0);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move { sys.sys_fork(0, 0).map(|_| ()) })
    });
    assert_eq!(got, Ok(()));
}
