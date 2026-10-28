//! `PIDFD_THREAD` (Linux 6.9) and the scopes of `pidfd_send_signal`.
//!
//! `pidfd_open(tid, PIDFD_THREAD)` was `EINVAL`, so a pidfd could only follow
//! a whole process: a thread pool that waits on its workers through `poll`,
//! or signals one of them by descriptor rather than by a tid that may have
//! been reused, had no way to. What Linux 6.18 does, measured with glibc and
//! musl (`pidfd-thread-survey`, 20 checks):
//!
//! - a thread's pidfd says `O_RDWR | O_EXCL` to `F_GETFL`, its tid in
//!   `fdinfo`, and is readable and hung up once the thread has exited; it is
//!   `ESRCH` to `pidfd_open` from then on, and `Pid: -1`;
//! - `pidfd_send_signal` takes a scope in its flags, `PIDFD_SIGNAL_THREAD`,
//!   `_THREAD_GROUP` or `_PROCESS_GROUP`, and without one uses the pidfd's
//!   own; a thread's signal says `SI_TKILL`, the others `SI_USER`;
//! - "yourself", where a forged `si_code` is allowed, is the calling thread;
//! - a descriptor that is not a pidfd is `EBADF` (here it was `EINVAL`);
//! - `waitid(P_PIDFD)` on a thread that is not a leader is `ECHILD`;
//! - `clone(CLONE_PIDFD | CLONE_THREAD)` gives the new thread's pidfd (here
//!   it was `EINVAL`);
//! - a process pidfd is hung up too once a `wait` has reaped it.
//!
//! Fixed pids live in 49_2xx here.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::sync::Arc;
use linux_object::fs::{FileLike, PollEvents};
use linux_object::process::ProcessExt;
use linux_object::signal::{SignalCode, Sigset};
use linux_object::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ThreadState, ROOT_JOB};

const SIGUSR1: usize = 10;
const SIGKILL: usize = 9;
const F_GETFL: usize = 3;
const P_PIDFD: i32 = 3;
const WNOHANG: u32 = 1;
const WEXITED: u32 = 4;
const SI_USER: i32 = 0;
/// `CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD`.
const A_THREAD: usize = 0x10f00;
const CLONE_PIDFD: usize = 0x1000;
/// An ordinary unprivileged user, as `setresuid` leaves one.
const A_PLAIN_USER: usize = 1000;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "hilos", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// `proc` with its leader and one more thread, neither of them running.
fn with_a_worker(proc: &Arc<Process>) -> (Arc<Thread>, Arc<Thread>) {
    let leader = Thread::create_linux(proc).unwrap();
    let worker = Thread::create_linux(proc).unwrap();
    assert_eq!(leader.id(), proc.id());
    (leader, worker)
}

fn pidfd_of(sys: &Syscall<'_>, fd: usize) -> Arc<PidFd> {
    PidFd::from_file_like(sys.linux_process().get_file_like(fd.into()).unwrap()).unwrap()
}

/// What `cat /proc/self/fdinfo/<fd>` prints.
async fn fdinfo(sys: &Syscall<'_>, fd: usize) -> alloc::string::String {
    let path = alloc::format!("/proc/self/fdinfo/{fd}\0");
    let info = sys
        .sys_openat(FileDesc::CWD, UserInPtr::from(path.as_ptr() as usize), 0, 0)
        .unwrap();
    let mut buf = [0u8; 256];
    let n = sys
        .sys_read(
            info.into(),
            UserOutPtr::from(buf.as_mut_ptr() as usize),
            buf.len(),
        )
        .await
        .unwrap();
    alloc::string::String::from(core::str::from_utf8(&buf[..n]).unwrap())
}

/// What `poll(POLLIN)` says of it: readable, hung up.
fn polled(pidfd: &PidFd) -> (bool, bool) {
    let got = FileLike::poll(pidfd, PollEvents::IN).unwrap();
    (got.read, got.hangup)
}

/// The `si_code` of `signal` pending on `thread`, taken out.
fn taken(thread: &Arc<Thread>, signal: Signal) -> Option<i32> {
    let mut lt = thread.lock_linux();
    lt.signals
        .contains(signal)
        .then(|| lt.take_siginfo(signal).code.0)
}

fn blocking(thread: &Arc<Thread>, signal: Option<Signal>) {
    let mut mask = Sigset::empty();
    if let Some(signal) = signal {
        mask.insert(signal);
    }
    thread.lock_linux().set_signal_mask(mask);
}

#[test]
fn a_thread_that_is_not_the_leader_has_a_pidfd_of_its_own() {
    let target = a_process(49_200);
    let (leader, worker) = with_a_worker(&target);
    let (tid, pid) = (worker.id() as usize, target.id() as usize);
    let got = on_a_thread(&a_process(49_201), move |sys| {
        Box::pin(async move {
            let nonblock = OpenFlags::NON_BLOCK.bits() as u32;
            let thread = PIDFD_THREAD;
            let refused = sys.sys_pidfd_open(tid, 0);
            let fds = [
                sys.sys_pidfd_open(tid, thread).unwrap(),
                sys.sys_pidfd_open(tid, thread | nonblock).unwrap(),
                sys.sys_pidfd_open(pid, thread).unwrap(),
                sys.sys_pidfd_open(pid, 0).unwrap(),
            ];
            let mut flags = [0; 4];
            for (at, fd) in fds.iter().enumerate() {
                flags[at] = sys.sys_fcntl((*fd).into(), F_GETFL, 0).await.unwrap();
            }
            let theirs = pidfd_of(sys, fds[0]);
            let leaders = pidfd_of(sys, fds[2]);
            (
                refused,
                flags,
                theirs.task_id(),
                fdinfo(sys, fds[0]).await,
                leaders.thread().is_none() && leaders.is_thread(),
                fdinfo(sys, fds[2]).await,
            )
        })
    });
    assert_eq!(got.0, Err(LxError::ENOENT), "without PIDFD_THREAD");
    assert_eq!(got.1, [0o202, 0o4202, 0o202, 0o2], "F_GETFL");
    assert_eq!(got.2, worker.id());
    // Linux 6.18: `flags:\t02000202`, and the tid.
    assert!(got.3.contains("flags:\t02000202\n"), "{}", got.3);
    assert!(
        got.3.contains(&alloc::format!("\nPid:\t{tid}\n")),
        "{}",
        got.3
    );
    assert!(got.4, "a leader's thread pidfd follows its process");
    assert!(got.5.contains("flags:\t02000202\n"), "{}", got.5);
    assert!(
        got.5.contains(&alloc::format!("\nPid:\t{pid}\n")),
        "{}",
        got.5
    );
    // `pidfd_open` finds the leader's pid as its process and never asks
    // `of_thread` about it; what `of_thread` makes of a leader all the same
    // is its process's pidfd, as `PIDFD_THREAD` on a leader is on Linux.
    let leaders = PidFd::of_thread(target.clone(), leader.clone(), OpenFlags::RDWR);
    assert!(leaders.thread().is_none() && leaders.is_thread());
    assert_eq!(leaders.task_id(), target.id());
    let workers = PidFd::of_thread(target.clone(), worker.clone(), OpenFlags::RDWR);
    assert_eq!(workers.task_id(), worker.id());
    target.exit(0);
    worker.terminate_abandoned();
    leader.terminate_abandoned();
}

#[test]
fn a_thread_pidfd_is_readable_and_hung_up_once_its_thread_has_exited() {
    let target = a_process(49_202);
    let (leader, worker) = with_a_worker(&target);
    let (tid, pid) = (worker.id() as usize, target.id() as usize);
    on_a_thread(&a_process(49_203), move |sys| {
        Box::pin(async move {
            let fd = sys.sys_pidfd_open(tid, PIDFD_THREAD).unwrap();
            let theirs = pidfd_of(sys, fd);
            let process = pidfd_of(sys, sys.sys_pidfd_open(pid, 0).unwrap());
            assert_eq!(polled(&theirs), (false, false), "while it runs");
            assert_eq!(
                sys.sys_pidfd_send_signal(fd.into(), 0, UserInPtr::from(0), 0),
                Ok(0),
                "and it is there to be probed"
            );

            worker.terminate_abandoned();
            assert_eq!(worker.state(), ThreadState::Dead);
            assert_eq!(polled(&theirs), (true, true), "once it has exited");
            assert_eq!(
                polled(&process),
                (false, false),
                "its process is not, with its leader still there"
            );
            assert!(
                theirs.fdinfo().starts_with("Pid:\t-1\n"),
                "{}",
                theirs.fdinfo()
            );
            assert_eq!(
                sys.sys_pidfd_send_signal(fd.into(), 0, UserInPtr::from(0), 0),
                Err(LxError::ESRCH),
                "an exited thread is nobody to signal"
            );
            assert_eq!(
                sys.sys_pidfd_send_signal(fd.into(), 0, UserInPtr::from(0), 2),
                Err(LxError::ESRCH),
                "not even to its process through it"
            );
            assert_eq!(
                sys.sys_pidfd_open(tid, PIDFD_THREAD),
                Err(LxError::ESRCH),
                "nor to open"
            );
        })
    });
    target.exit(0);
    leader.terminate_abandoned();
}

#[test]
fn a_process_pidfd_is_hung_up_once_its_process_is_reaped() {
    let proc = a_process(49_204);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let child = sys.sys_fork(0, 0)?;
            let fd = sys.sys_pidfd_open(child, 0)?;
            let pidfd = pidfd_of(sys, fd);
            // The child has nothing to run and exits.
            let zombie = (0..2000).any(|_| {
                let zombie = sys.linux_process().is_zombie_child(child as KoID);
                if !zombie {
                    std::thread::sleep(core::time::Duration::from_millis(1));
                }
                zombie
            });
            let as_a_zombie = polled(&pidfd);
            sys.sys_wait4(child as i32, 0usize.into(), 0, 0usize.into())
                .await?;
            let reaped = polled(&pidfd);
            let asked_nothing = FileLike::poll(pidfd.as_ref(), PollEvents::empty())?.hangup;
            Ok::<_, LxError>((zombie, as_a_zombie, reaped, asked_nothing))
        })
    });
    assert_eq!(got, Ok((true, (true, false), (true, true), true)));
}

#[test]
fn a_signal_through_a_thread_pidfd_is_that_threads_and_says_tkill() {
    let target = a_process(49_205);
    let (leader, worker) = with_a_worker(&target);
    let tid = worker.id() as usize;
    let (l, w) = (leader.clone(), worker.clone());
    on_a_thread(&a_process(49_206), move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(sys.sys_pidfd_open(tid, PIDFD_THREAD).unwrap());
            let send = |flags| sys.sys_pidfd_send_signal(fd, SIGUSR1, UserInPtr::from(0), flags);
            // The worker blocks it, so a signal at the process would go to
            // the leader instead.
            blocking(&w, Some(Signal::SIGUSR1));
            assert_eq!(send(0), Ok(0));
            assert_eq!(taken(&w, Signal::SIGUSR1), Some(SignalCode::TKILL.0));
            assert_eq!(taken(&l, Signal::SIGUSR1), None);
            assert_eq!(send(1), Ok(0), "PIDFD_SIGNAL_THREAD says the same");
            assert_eq!(taken(&w, Signal::SIGUSR1), Some(SignalCode::TKILL.0));
            // `PIDFD_SIGNAL_THREAD_GROUP`: its process, as a `kill(2)`.
            assert_eq!(send(2), Ok(0));
            assert_eq!(taken(&w, Signal::SIGUSR1), None);
            assert_eq!(taken(&l, Signal::SIGUSR1), Some(SignalCode::USER.0));
            // `PIDFD_SIGNAL_PROCESS_GROUP`: a tid leads no group.
            assert_eq!(send(4), Err(LxError::ESRCH));
        })
    });
    target.exit(0);
    worker.terminate_abandoned();
    leader.terminate_abandoned();
}

#[test]
fn a_process_pidfd_reaches_its_leader_or_its_group_when_asked() {
    let target = a_process(49_207);
    let (leader, worker) = with_a_worker(&target);
    let pid = target.id() as usize;
    let (l, w) = (leader.clone(), worker.clone());
    on_a_thread(&a_process(49_208), move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(sys.sys_pidfd_open(pid, 0).unwrap());
            let send = |flags| sys.sys_pidfd_send_signal(fd, SIGUSR1, UserInPtr::from(0), flags);
            // The leader blocks it, so a signal at the process goes to the
            // worker and one at the leader stays with the leader.
            blocking(&l, Some(Signal::SIGUSR1));
            assert_eq!(send(1), Ok(0));
            assert_eq!(taken(&l, Signal::SIGUSR1), Some(SignalCode::TKILL.0));
            assert_eq!(taken(&w, Signal::SIGUSR1), None);
            assert_eq!(send(0), Ok(0));
            assert_eq!(taken(&w, Signal::SIGUSR1), Some(SignalCode::USER.0));
            assert_eq!(taken(&l, Signal::SIGUSR1), None);
            // Its own process group, which a fresh process leads.
            assert_eq!(send(4), Ok(0));
            assert_eq!(taken(&w, Signal::SIGUSR1), Some(SignalCode::USER.0));
            // And a `sigqueue` there arrives as the sender wrote it.
            let queued = SigInfo::from_user(Signal::SIGUSR1, 0, 0, SignalCode::QUEUE);
            let at = UserInPtr::from(&queued as *const SigInfo as usize);
            assert_eq!(sys.sys_pidfd_send_signal(fd, SIGUSR1, at, 4), Ok(0));
            assert_eq!(taken(&w, Signal::SIGUSR1), Some(SignalCode::QUEUE.0));
        })
    });
    target.exit(0);
    worker.terminate_abandoned();
    leader.terminate_abandoned();
}

#[test]
fn a_thread_pidfd_is_no_way_past_kill_permission() {
    let target = a_process(49_209);
    let (leader, worker) = with_a_worker(&target);
    let tid = worker.id() as usize;
    let w = worker.clone();
    on_a_thread(&a_process(49_210), move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(sys.sys_pidfd_open(tid, PIDFD_THREAD).unwrap());
            assert_eq!(
                sys.sys_setresuid(A_PLAIN_USER, A_PLAIN_USER, A_PLAIN_USER),
                Ok(0)
            );
            for flags in [0, 1] {
                assert_eq!(
                    sys.sys_pidfd_send_signal(fd, SIGUSR1, UserInPtr::from(0), flags),
                    Err(LxError::EPERM),
                    "a plain user at root's thread, flags {flags}"
                );
            }
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, 0, UserInPtr::from(0), 0),
                Err(LxError::EPERM),
                "the probe included"
            );
            assert_eq!(taken(&w, Signal::SIGUSR1), None);
        })
    });
    target.exit(0);
    worker.terminate_abandoned();
    leader.terminate_abandoned();
}

#[test]
fn sigkill_through_a_thread_pidfd_ends_its_process() {
    // `complete_signal()`: a fatal signal at one thread takes the group.
    // Queued on a thread that never runs again, it ended nothing.
    let target = a_process(49_211);
    let (leader, worker) = with_a_worker(&target);
    let tid = worker.id() as usize;
    on_a_thread(&a_process(49_212), move |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(sys.sys_pidfd_open(tid, PIDFD_THREAD).unwrap());
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, SIGKILL, UserInPtr::from(0), 0),
                Ok(0)
            );
        })
    });
    assert!(matches!(target.status(), Status::Exited(_)));
    worker.terminate_abandoned();
    leader.terminate_abandoned();
}

#[test]
fn yourself_is_the_calling_thread_when_forging_a_code() {
    // `task_pid(current) != pid`: a thread that is not the leader may not
    // claim a kernel code at its own process's pidfd, and may at its own.
    let proc = a_process(49_213);
    let leader = Thread::create_linux(&proc).unwrap();
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let me = sys.thread.id() as usize;
            assert_ne!(me, sys.zircon_process().id() as usize, "not the leader");
            let process = FileDesc::from(
                sys.sys_pidfd_open(sys.zircon_process().id() as usize, 0)
                    .unwrap(),
            );
            let own = FileDesc::from(sys.sys_pidfd_open(me, PIDFD_THREAD).unwrap());
            let forged = SigInfo::from_user(Signal::SIGUSR1, 0, 0, SignalCode(SI_USER));
            let mut probe = forged;
            probe.signo = 0;
            let at = || UserInPtr::from(&probe as *const SigInfo as usize);
            assert_eq!(
                sys.sys_pidfd_send_signal(process, 0, at(), 0),
                Err(LxError::EPERM)
            );
            assert_eq!(sys.sys_pidfd_send_signal(own, 0, at(), 0), Ok(0));
            assert_eq!(
                sys.sys_pidfd_send_signal(own, 0, at(), 4),
                Err(LxError::EPERM),
                "a process group is never yourself"
            );
        })
    });
    on_a_thread(&a_process(49_214), |sys| {
        Box::pin(async move {
            let me = sys.zircon_process().id() as usize;
            assert_eq!(sys.thread.id() as usize, me, "the leader");
            let process = FileDesc::from(sys.sys_pidfd_open(me, 0).unwrap());
            let mut probe = SigInfo::from_user(Signal::SIGUSR1, 0, 0, SignalCode(SI_USER));
            probe.signo = 0;
            let at = UserInPtr::from(&probe as *const SigInfo as usize);
            assert_eq!(
                sys.sys_pidfd_send_signal(process, 0, at, 0),
                Ok(0),
                "the leader is its process"
            );
        })
    });
    proc.exit(0);
    leader.terminate_abandoned();
}

#[test]
fn a_descriptor_that_is_not_a_pidfd_is_ebadf_and_the_signal_number_comes_after() {
    let proc = a_process(49_215);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let fd = FileDesc::from(sys.sys_eventfd2(0, 0).unwrap());
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, SIGUSR1, UserInPtr::from(0), 0),
                Err(LxError::EBADF)
            );
            assert_eq!(
                sys.sys_pidfd_send_signal(fd, 999, UserInPtr::from(0), 0),
                Err(LxError::EBADF),
                "the descriptor before the signal number"
            );
            assert_eq!(sys.sys_pidfd_getfd(fd, 0, 0), Err(LxError::EBADF));
            assert_eq!(
                sys.sys_waitid(P_PIDFD, fd.into(), 0usize.into(), WEXITED, 0usize.into())
                    .await,
                Err(LxError::EBADF)
            );
            let own = FileDesc::from(
                sys.sys_pidfd_open(sys.zircon_process().id() as usize, 0)
                    .unwrap(),
            );
            assert_eq!(
                sys.sys_pidfd_send_signal(own, 999, UserInPtr::from(0), 0),
                Err(LxError::EINVAL),
                "and a pidfd's is"
            );
        })
    });
}

#[test]
fn waitid_on_a_thread_that_is_not_a_leader_is_echild() {
    let parent = a_process(49_216);
    let child = Process::fork_from(&parent).unwrap();
    let (leader, worker) = with_a_worker(&child);
    let (tid, pid) = (worker.id() as usize, child.id() as usize);
    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let wait = |fd: usize| {
                sys.sys_waitid(P_PIDFD, fd, 0usize.into(), WEXITED | WNOHANG, 0usize.into())
            };
            let theirs = sys.sys_pidfd_open(tid, PIDFD_THREAD).unwrap();
            let leaders = sys.sys_pidfd_open(pid, PIDFD_THREAD).unwrap();
            (wait(theirs).await, wait(leaders).await)
        })
    });
    assert_eq!(
        got,
        (Err(LxError::ECHILD), Ok(0)),
        "the leader is the child"
    );
    child.exit(0);
    worker.terminate_abandoned();
    leader.terminate_abandoned();
}

#[test]
fn clone_pidfd_with_clone_thread_gives_the_new_threads_pidfd() {
    let proc = a_process(49_217);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut slot: i32 = -1;
            let tid = sys
                .sys_clone(
                    A_THREAD | CLONE_PIDFD,
                    0,
                    UserOutPtr::from(&mut slot as *mut i32 as usize),
                    0,
                    0usize.into(),
                )
                .await?;
            let pidfd = pidfd_of(sys, slot as usize);
            let flags = sys.sys_fcntl((slot as usize).into(), F_GETFL, 0).await?;
            // The new thread has nothing to run and exits.
            let exited = (0..2000).any(|_| {
                let exited = polled(&pidfd) == (true, true);
                if !exited {
                    std::thread::sleep(core::time::Duration::from_millis(1));
                }
                exited
            });
            Ok::<_, LxError>((
                pidfd.thread().map(|t| t.id() as usize) == Some(tid),
                flags,
                exited,
            ))
        })
    });
    assert_eq!(got, Ok((true, 0o202, true)));
}
