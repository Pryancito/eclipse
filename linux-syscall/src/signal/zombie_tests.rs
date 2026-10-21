//! A dead process is a pid until somebody reaps it, and nothing after.
//!
//! `kill(2)` found a zombie only when its own parent asked, answered `ESRCH`
//! to a real signal while the dead process was still in its job, and found a
//! reaped one that was still finishing dying there. `getpgid`, `getsid` and
//! `pidfd_open` found no zombie at all once it had left its job.
//!
//! Fixed pids live in 45_35x here.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{effective_pgid, effective_sid, LinuxProcess, ProcessExt};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const SIGTERM: usize = 15;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "zombi", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// Dead and still in its job, because a thread of it has not finished
/// dying: a signal finds no thread to take it, and that is not `ESRCH`.
#[test]
fn a_dead_process_still_in_its_job_takes_a_signal_and_answers_0() {
    let dead = a_process(45_351);
    Thread::create_linux(&dead).unwrap();
    dead.exit(0);
    assert!(
        ROOT_JOB.find_process(45_351).is_some(),
        "the case this test is about"
    );
    let got = on_a_thread(&a_process(45_352), |sys| {
        Box::pin(async move { (sys.sys_kill(45_351, SIGTERM), sys.sys_kill(45_351, 0)) })
    });
    assert_eq!(got, (Ok(0), Ok(0)));
}

/// Somebody else's zombie, gone from its job: whoever may signal it gets 0,
/// not only its parent, and it keeps its group and session.
#[test]
fn somebody_else_s_zombie_is_a_pid_too() {
    let parent = a_process(45_353);
    Thread::create_linux(&parent).unwrap();
    let child = Process::fork_from(&parent).unwrap();
    let (pid, pgid, sid) = (child.id(), effective_pgid(&child), effective_sid(&child));
    child.exit(0);
    drop(child);
    let got = on_a_thread(&a_process(45_354), move |sys| {
        Box::pin(async move {
            let pidfd = sys.sys_pidfd_open(pid as usize, 0);
            (
                sys.sys_kill(pid as isize, SIGTERM),
                sys.sys_kill(pid as isize, 0),
                sys.sys_getpgid(pid as i32),
                sys.sys_getsid(pid as i32),
                pidfd
                    .and_then(|fd| sys.sys_pidfd_send_signal(fd.into(), SIGTERM, 0usize.into(), 0)),
            )
        })
    });
    assert_eq!(
        got,
        (Ok(0), Ok(0), Ok(pgid as usize), Ok(sid as usize), Ok(0))
    );
}

/// Reaped, the pid names nothing -- even while the process is still
/// finishing dying in its job, which a `wait*` does not wait for.
#[test]
fn a_reaped_child_still_dying_in_its_job_is_gone() {
    let caller = a_process(45_355);
    let child = Process::fork_from(&caller).unwrap();
    let pid = child.id();
    Thread::create_linux(&child).unwrap();
    child.exit(0);
    let got = on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let pidfd = sys.sys_pidfd_open(pid as usize, 0).unwrap();
            let reaped = sys
                .sys_wait4(pid as i32, 0usize.into(), 0, 0usize.into())
                .await;
            (
                reaped,
                sys.sys_kill(pid as isize, 0),
                sys.sys_getpgid(pid as i32),
                sys.sys_pidfd_open(pid as usize, 0),
                sys.sys_pidfd_send_signal(pidfd.into(), 0, 0usize.into(), 0),
            )
        })
    });
    assert!(
        ROOT_JOB.find_process(pid).is_some(),
        "still in its job, which is the case"
    );
    assert_eq!(
        got,
        (
            Ok(pid as usize),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH),
            Err(LxError::ESRCH)
        )
    );
}
