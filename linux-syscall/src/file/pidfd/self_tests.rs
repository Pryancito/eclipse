//! `PIDFD_SELF_THREAD` and `PIDFD_SELF_THREAD_GROUP` (Linux 6.15), and what
//! `pidfd_getfd` and `waitid` take from a pidfd.
//!
//! -10000 and -10001 are numbers no descriptor has, which
//! `pidfd_send_signal` takes as a pidfd of the calling thread and one of its
//! process, so a program can signal itself without opening anything. Here
//! they were `EBADF`. What Linux 6.18 does, measured with glibc and musl
//! (`pidfd-self-survey`, 14 checks):
//!
//! - -10000 is the calling thread, with `SI_TKILL`; -10001 its process, with
//!   `SI_USER`; the scope flags reach from them as from any pidfd, and
//!   "yourself", where a forged `si_code` is allowed, is -10000 only;
//! - `pidfd_getfd` takes neither (`EBADF`), and `waitid(P_PIDFD)` refuses any
//!   negative number with `EINVAL` before it looks for a descriptor;
//! - `pidfd_getfd` takes the descriptor out of the task the pid names, so it
//!   is `ESRCH` through a thread pidfd whose thread has exited, and through
//!   the pidfd of a process whose leader has, while another thread runs on.
//!
//! Fixed pids live in 49_22x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::sync::Arc;
use linux_object::signal::{SignalCode, Sigset};
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

const SIGUSR1: usize = 10;
const SI_USER: i32 = 0;
const P_PIDFD: i32 = 3;
const WNOHANG: u32 = 1;
const WEXITED: u32 = 4;
/// A descriptor number the target process does not have open.
const A_FD_OF_NOBODY: i32 = 90;

/// `PIDFD_SELF_THREAD` and `PIDFD_SELF_THREAD_GROUP`.
fn self_thread() -> FileDesc {
    FileDesc::from(-10_000)
}
fn self_group() -> FileDesc {
    FileDesc::from(-10_001)
}

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "self", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// The `si_code` of `signal` pending on `thread`, taken out.
fn taken(thread: &Arc<Thread>, signal: Signal) -> Option<i32> {
    let mut lt = thread.lock_linux();
    lt.signals
        .contains(signal)
        .then(|| lt.take_siginfo(signal).code.0)
}

fn blocking(thread: &Arc<Thread>, signal: Signal) {
    let mut mask = Sigset::empty();
    mask.insert(signal);
    thread.lock_linux().set_signal_mask(mask);
}

#[test]
fn pidfd_self_thread_is_the_calling_thread_and_pidfd_self_thread_group_its_process() {
    // The caller is not the leader, and blocks the signal: one at its
    // process goes to the leader instead.
    let proc = a_process(49_220);
    let leader = Thread::create_linux(&proc).unwrap();
    let l = leader.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let me = sys.thread.inner();
            assert_ne!(me.id(), l.id(), "not the leader");
            blocking(&me, Signal::SIGUSR1);
            let send =
                |fd, flags| sys.sys_pidfd_send_signal(fd, SIGUSR1, UserInPtr::from(0), flags);

            assert_eq!(send(self_thread(), 0), Ok(0));
            assert_eq!(taken(&me, Signal::SIGUSR1), Some(SignalCode::TKILL.0));
            assert_eq!(taken(&l, Signal::SIGUSR1), None);

            assert_eq!(send(self_group(), 0), Ok(0));
            assert_eq!(taken(&l, Signal::SIGUSR1), Some(SignalCode::USER.0));
            assert_eq!(taken(&me, Signal::SIGUSR1), None);

            // The scope flags reach from them as from a pidfd.
            assert_eq!(send(self_group(), 1), Ok(0), "PIDFD_SIGNAL_THREAD");
            assert_eq!(taken(&l, Signal::SIGUSR1), Some(SignalCode::TKILL.0));
            assert_eq!(taken(&me, Signal::SIGUSR1), None);
            assert_eq!(send(self_thread(), 2), Ok(0), "PIDFD_SIGNAL_THREAD_GROUP");
            assert_eq!(taken(&l, Signal::SIGUSR1), Some(SignalCode::USER.0));
            assert_eq!(taken(&me, Signal::SIGUSR1), None);
            assert_eq!(
                send(self_thread(), 4),
                Err(LxError::ESRCH),
                "a thread that is not the leader leads no group"
            );
            assert_eq!(send(self_group(), 4), Ok(0), "the group its process leads");
            assert_eq!(taken(&l, Signal::SIGUSR1), Some(SignalCode::USER.0));

            for fd in [self_thread(), self_group()] {
                assert_eq!(
                    sys.sys_pidfd_send_signal(fd, 0, UserInPtr::from(0), 0),
                    Ok(0),
                    "the probe"
                );
            }
        })
    });
    proc.exit(0);
    leader.terminate_abandoned();
}

#[test]
fn pidfd_self_thread_of_the_leader_is_the_leader() {
    on_a_thread(&a_process(49_221), |sys| {
        Box::pin(async move {
            let me = sys.thread.inner();
            assert_eq!(me.id(), sys.zircon_process().id(), "the leader");
            blocking(&me, Signal::SIGUSR1);
            assert_eq!(
                sys.sys_pidfd_send_signal(self_thread(), SIGUSR1, UserInPtr::from(0), 0),
                Ok(0)
            );
            assert_eq!(taken(&me, Signal::SIGUSR1), Some(SignalCode::TKILL.0));
        })
    });
}

#[test]
fn yourself_is_pidfd_self_thread_when_forging_a_code() {
    let mut probe = SigInfo::from_user(Signal::SIGUSR1, 0, 0, SignalCode(SI_USER));
    probe.signo = 0;
    let addr = &probe as *const SigInfo as usize;
    let at = move || UserInPtr::<SigInfo>::from(addr);

    let proc = a_process(49_222);
    let leader = Thread::create_linux(&proc).unwrap();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_ne!(sys.thread.id(), sys.zircon_process().id(), "not the leader");
            assert_eq!(sys.sys_pidfd_send_signal(self_thread(), 0, at(), 0), Ok(0));
            assert_eq!(
                sys.sys_pidfd_send_signal(self_group(), 0, at(), 0),
                Err(LxError::EPERM),
                "its process is not the thread"
            );
        })
    });
    proc.exit(0);
    leader.terminate_abandoned();

    on_a_thread(&a_process(49_223), move |sys| {
        Box::pin(async move {
            assert_eq!(sys.thread.id(), sys.zircon_process().id(), "the leader");
            assert_eq!(
                sys.sys_pidfd_send_signal(self_group(), 0, at(), 0),
                Ok(0),
                "the leader is its process"
            );
            assert_eq!(
                sys.sys_pidfd_send_signal(self_group(), 0, at(), 4),
                Err(LxError::EPERM),
                "a process group is never yourself"
            );
        })
    });
}

#[test]
fn only_pidfd_send_signal_takes_the_callers_numbers_and_in_linux_order() {
    on_a_thread(&a_process(49_224), |sys| {
        Box::pin(async move {
            let send =
                |fd, sig, flags| sys.sys_pidfd_send_signal(fd, sig, UserInPtr::from(0), flags);
            assert_eq!(send(self_thread(), 999, 0), Err(LxError::EINVAL));
            assert_eq!(
                send(self_thread(), 0, 3),
                Err(LxError::EINVAL),
                "two scopes"
            );
            assert_eq!(send(self_group(), 0, 8), Err(LxError::EINVAL), "flag 8");
            // `FD_PIDFS_ROOT`, `FD_INVALID`, and a plain negative number.
            for fd in [-10_002, -10_009, -2] {
                assert_eq!(send(FileDesc::from(fd), 0, 0), Err(LxError::EBADF), "{fd}");
            }
            assert_eq!(
                send(FileDesc::from(-10_002), 999, 0),
                Err(LxError::EBADF),
                "the descriptor before the signal number"
            );

            for fd in [self_thread(), self_group()] {
                assert_eq!(sys.sys_pidfd_getfd(fd, 0, 0), Err(LxError::EBADF));
            }
            assert_eq!(
                sys.sys_pidfd_getfd(self_thread(), 0, 1),
                Err(LxError::EINVAL),
                "its flags first"
            );

            // `waitid` is handed an `id_t`, so -10000 can come zero-extended.
            for id in [
                -10_000i32 as u32 as usize,
                -10_001i32 as u32 as usize,
                -1i32 as u32 as usize,
                usize::MAX,
            ] {
                assert_eq!(
                    sys.sys_waitid(P_PIDFD, id, 0usize.into(), WEXITED | WNOHANG, 0usize.into())
                        .await,
                    Err(LxError::EINVAL),
                    "{id:#x}"
                );
            }
            // Zero is a descriptor like any other: the caller's own pidfd
            // there is no child of its own, `ECHILD`, not `EINVAL`.
            let own = sys
                .sys_pidfd_open(sys.zircon_process().id() as usize, 0)
                .unwrap();
            if own != 0 {
                assert_eq!(sys.sys_dup3(own.into(), 0, 0), Ok(0));
            }
            assert_eq!(
                sys.sys_waitid(P_PIDFD, 0, 0usize.into(), WEXITED | WNOHANG, 0usize.into())
                    .await,
                Err(LxError::ECHILD),
                "descriptor 0"
            );
        })
    });
}

#[test]
fn pidfd_getfd_takes_nothing_out_of_a_task_that_has_exited() {
    // Through a live task the lookup is reached, which is `EBADF` for a
    // descriptor the target does not have; once that task has exited it is
    // `ESRCH`, with its process still running.
    let target = a_process(49_225);
    let leader = Thread::create_linux(&target).unwrap();
    let gone = Thread::create_linux(&target).unwrap();
    let staying = Thread::create_linux(&target).unwrap();
    let (pid, tid) = (target.id() as usize, gone.id() as usize);
    let (l, g, t) = (leader.clone(), gone.clone(), target.clone());
    on_a_thread(&a_process(49_226), move |sys| {
        Box::pin(async move {
            let thread = FileDesc::from(sys.sys_pidfd_open(tid, PIDFD_THREAD).unwrap());
            let process = FileDesc::from(sys.sys_pidfd_open(pid, 0).unwrap());
            for fd in [thread, process] {
                assert_eq!(
                    sys.sys_pidfd_getfd(fd, A_FD_OF_NOBODY, 0),
                    Err(LxError::EBADF)
                );
            }
            g.terminate_abandoned();
            assert_eq!(
                sys.sys_pidfd_getfd(thread, A_FD_OF_NOBODY, 0),
                Err(LxError::ESRCH),
                "a thread that has exited"
            );
            assert_eq!(
                sys.sys_pidfd_getfd(process, A_FD_OF_NOBODY, 0),
                Err(LxError::EBADF),
                "its process's leader is still there"
            );
            l.terminate_abandoned();
            assert!(!matches!(t.status(), Status::Exited(_)), "one thread left");
            assert_eq!(
                sys.sys_pidfd_getfd(process, A_FD_OF_NOBODY, 0),
                Err(LxError::ESRCH),
                "a process whose leader has exited"
            );
        })
    });
    target.exit(0);
    staying.terminate_abandoned();
}
