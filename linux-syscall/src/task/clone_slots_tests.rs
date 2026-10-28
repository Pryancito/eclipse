//! The words `clone` and `clone3` write for the caller: `parent_tid`,
//! `child_tid` and the pidfd's.
//!
//! What Linux 6.18 does, measured with glibc and musl (`clone-slots-survey`,
//! 11 checks):
//!
//! - `clone3` takes `CLONE_PIDFD` with `CLONE_PARENT_SETTID`, each word in
//!   its own field, and refuses the two at one address (`EINVAL`, nothing
//!   made). Here the pair was refused whatever the addresses.
//! - A `parent_tid` or a `child_tid` it cannot write costs the clone
//!   nothing: `kernel_clone` and `schedule_tail` drop what `put_user`
//!   answers. Here a fork was answered `EFAULT` with its child already
//!   running, and a thread was taken back.
//! - The pidfd's word is the one that can undo a clone, and it comes first
//!   (`copy_process`): when it faults no tid has been written anywhere.
//!
//! A word the kernel cannot write is a misaligned one here: `check` refuses
//! it before any store, as an unmapped page is refused on the machine, and a
//! host test cannot fault.
//!
//! Fixed pids live in 49_23x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const SIGCHLD: usize = 17;
const PIDFD: u64 = CloneFlags::PIDFD.bits() as u64;
const SETTID: u64 = CloneFlags::PARENT_SETTID.bits() as u64;
const A_THREAD: u64 =
    (CloneFlags::THREAD.bits() | CloneFlags::SIGHAND.bits() | CloneFlags::VM.bits()) as u64;
/// What a word holds before anything is written to it.
const A_SEED: i32 = -0x55;
const WNOHANG: u32 = 1;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "slots", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// An `i32` word the kernel cannot write: one byte into `buf`.
fn unwritable(buf: &mut [u8; 16]) -> usize {
    buf.as_mut_ptr() as usize + 1
}

/// A `struct clone_args`, version 0, with exit signal `SIGCHLD` unless a
/// thread is asked for.
fn clone_args(flags: u64, pidfd: usize, child_tid: usize, parent_tid: usize) -> [u64; 8] {
    let signal = if flags & A_THREAD != 0 {
        0
    } else {
        SIGCHLD as u64
    };
    [
        flags,
        pidfd as u64,
        child_tid as u64,
        parent_tid as u64,
        signal,
        0,
        0,
        0,
    ]
}

async fn clone3(sys: &Syscall<'_>, mut words: [u64; 8]) -> SysResult {
    let at = UserInPtr::from(words.as_mut_ptr() as usize);
    sys.sys_clone3(at, CLONE_ARGS_SIZE_VER0).await
}

/// The process `fd` is a pidfd of.
fn pidfd_target(sys: &Syscall<'_>, fd: i32) -> KoID {
    let file = sys
        .linux_process()
        .get_file_like((fd as usize).into())
        .unwrap();
    PidFd::from_file_like(file).unwrap().target().id()
}

async fn reaped(sys: &Syscall<'_>, pid: usize) -> SysResult {
    sys.sys_wait4(pid as i32, UserOutPtr::from(0), 0, UserOutPtr::from(0))
        .await
}

#[test]
fn clone3_hands_over_a_pidfd_and_the_tid_each_in_its_own_word() {
    on_a_thread(&a_process(49_230), |sys| {
        Box::pin(async move {
            let (mut fd, mut tid) = (A_SEED, A_SEED);
            let (fd_at, tid_at) = (&mut fd as *mut i32 as usize, &mut tid as *mut i32 as usize);
            let pid = clone3(sys, clone_args(PIDFD | SETTID, fd_at, 0, tid_at))
                .await
                .expect("clone3 refused a pidfd with a parent_tid");
            assert_eq!(tid, pid as i32, "the tid's word");
            assert!(fd >= 0, "the pidfd's word got {}", fd);
            assert_eq!(pidfd_target(sys, fd), pid as KoID, "a pidfd of the child");
            assert_eq!(reaped(sys, pid).await, Ok(pid));
        })
    });
}

#[test]
fn clone3_refuses_both_at_one_word_and_makes_nothing() {
    on_a_thread(&a_process(49_231), |sys| {
        Box::pin(async move {
            let files = sys.linux_process().file_count();
            let mut word = A_SEED;
            let at = &mut word as *mut i32 as usize;
            assert_eq!(
                clone3(sys, clone_args(PIDFD | SETTID, at, 0, at)).await,
                Err(LxError::EINVAL)
            );
            assert_eq!(word, A_SEED, "the word was written");
            assert_eq!(sys.linux_process().file_count(), files, "a descriptor");
            assert_eq!(
                sys.sys_wait4(-1, UserOutPtr::from(0), WNOHANG, UserOutPtr::from(0))
                    .await,
                Err(LxError::ECHILD),
                "a child"
            );
        })
    });
}

#[test]
fn a_parent_tid_it_cannot_write_does_not_undo_a_fork() {
    on_a_thread(&a_process(49_232), |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 16];
            let bad = unwritable(&mut buf);
            let flags = CloneFlags::PARENT_SETTID.bits() | SIGCHLD;
            let pid = sys
                .sys_clone(flags, 0, bad.into(), 0, UserOutPtr::from(0))
                .await
                .expect("clone undone by its parent_tid");
            assert_eq!(reaped(sys, pid).await, Ok(pid), "clone");

            let pid = clone3(sys, clone_args(SETTID, 0, 0, bad))
                .await
                .expect("clone3 undone by its parent_tid");
            assert_eq!(reaped(sys, pid).await, Ok(pid), "clone3");
            assert_eq!(buf, [0; 16], "written anyway");
        })
    });
}

#[test]
fn clone3_with_both_and_a_parent_tid_it_cannot_write_still_hands_the_pidfd_over() {
    on_a_thread(&a_process(49_233), |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 16];
            let mut fd = A_SEED;
            let fd_at = &mut fd as *mut i32 as usize;
            let pid = clone3(
                sys,
                clone_args(PIDFD | SETTID, fd_at, 0, unwritable(&mut buf)),
            )
            .await
            .expect("clone3 undone by its parent_tid");
            assert!(fd >= 0, "the pidfd's word got {}", fd);
            assert_eq!(pidfd_target(sys, fd), pid as KoID);
            assert_eq!(reaped(sys, pid).await, Ok(pid));
        })
    });
}

#[test]
fn a_thread_comes_out_of_a_parent_tid_or_child_tid_it_cannot_write() {
    let proc = a_process(49_234);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 16];
            let bad = unwritable(&mut buf);
            let me = sys.thread.id() as usize;
            let thread = A_THREAD as usize;
            let tid = sys
                .sys_clone(
                    thread | CloneFlags::PARENT_SETTID.bits(),
                    0,
                    bad.into(),
                    0,
                    UserOutPtr::from(0),
                )
                .await
                .expect("a thread undone by its parent_tid");
            assert_ne!(tid, me);
            let tid = sys
                .sys_clone(
                    thread | CloneFlags::CHILD_SETTID.bits(),
                    0,
                    UserOutPtr::from(0),
                    0,
                    bad.into(),
                )
                .await
                .expect("a thread undone by its child_tid");
            assert_ne!(tid, me);
            assert_eq!(buf, [0; 16], "written anyway");
        })
    });
}

#[test]
fn clone3_hands_a_thread_its_pidfd_and_its_tid_each_in_its_own_word() {
    on_a_thread(&a_process(49_237), |sys| {
        Box::pin(async move {
            let (mut fd, mut tid) = (A_SEED, A_SEED);
            let (fd_at, tid_at) = (&mut fd as *mut i32 as usize, &mut tid as *mut i32 as usize);
            let new = clone3(sys, clone_args(A_THREAD | PIDFD | SETTID, fd_at, 0, tid_at))
                .await
                .expect("clone3 refused a thread's pidfd with its parent_tid");
            assert_eq!(tid, new as i32, "the tid's word");
            let file = sys
                .linux_process()
                .get_file_like((fd as usize).into())
                .unwrap();
            let pidfd = PidFd::from_file_like(file).unwrap();
            assert_eq!(
                pidfd.thread().map(|t| t.id()),
                Some(new as KoID),
                "its pidfd"
            );
        })
    });
}

#[test]
fn a_pidfd_it_cannot_hand_over_undoes_a_thread_before_any_tid_is_written() {
    let proc = a_process(49_235);
    on_a_thread(&proc, |sys| {
        Box::pin(async move {
            let mut buf = [0u8; 16];
            let (mut tid, mut ctid) = (A_SEED, A_SEED);
            let tid_at = &mut tid as *mut i32 as usize;
            let ctid_at = &mut ctid as *mut i32 as usize;
            let threads = sys.zircon_process().thread_count();
            let files = sys.linux_process().file_count();
            let flags = A_THREAD | PIDFD | SETTID | CloneFlags::CHILD_SETTID.bits() as u64;
            assert_eq!(
                clone3(
                    sys,
                    clone_args(flags, unwritable(&mut buf), ctid_at, tid_at)
                )
                .await,
                Err(LxError::EFAULT)
            );
            assert_eq!((tid, ctid), (A_SEED, A_SEED), "a tid was written");
            assert_eq!(
                sys.zircon_process().thread_count(),
                threads,
                "a thread left"
            );
            assert_eq!(sys.linux_process().file_count(), files, "a descriptor left");
        })
    });
}

#[test]
fn legacy_clone_hands_the_pidfd_over_in_parent_tid_and_so_refuses_both() {
    on_a_thread(&a_process(49_236), |sys| {
        Box::pin(async move {
            let mut word = A_SEED;
            let at = &mut word as *mut i32 as usize;
            let pidfd = CloneFlags::PIDFD.bits() | SIGCHLD;
            let pid = sys
                .sys_clone(pidfd, 0, at.into(), 0, UserOutPtr::from(0))
                .await
                .unwrap();
            assert_eq!(pidfd_target(sys, word), pid as KoID, "parent_tid's word");
            assert_eq!(reaped(sys, pid).await, Ok(pid));

            word = A_SEED;
            let files = sys.linux_process().file_count();
            assert_eq!(
                sys.sys_clone(
                    pidfd | CloneFlags::PARENT_SETTID.bits(),
                    0,
                    at.into(),
                    0,
                    UserOutPtr::from(0)
                )
                .await,
                Err(LxError::EINVAL)
            );
            assert_eq!(word, A_SEED);
            assert_eq!(sys.linux_process().file_count(), files);
        })
    });
}
