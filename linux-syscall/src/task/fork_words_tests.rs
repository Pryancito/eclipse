//! The words a fork-like clone writes into its caller's memory, the pidfd's
//! number and `parent_tid`, and when it writes them.
//!
//! What Linux 6.18 does, measured with glibc and musl (`pidfd-fork-survey`,
//! 9 checks): `copy_process` reserves the pidfd's descriptor and writes its
//! number before the child is anybody's to see, and a full table (`EMFILE`)
//! or a word it cannot write (`EFAULT`) makes nothing, no child, no
//! descriptor and no `SIGCHLD`; `kernel_clone` writes `parent_tid`
//! before it wakes the child. So a vfork child, which runs in its parent's
//! memory, finds both already written. Here the fork came first and both
//! words after it: a word that could not be written was `EFAULT` with the
//! child already running, and a vfork child ran before either was written.
//!
//! The child's one thread runs a `ThreadFn` of the test's own, which looks
//! at the words when it starts: that is the child running before the parent
//! returns, which a vfork guarantees.
//!
//! A word the kernel cannot write is a misaligned one here, as in
//! `clone_slots_tests`. Fixed pids live in 49_24x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicI32, Ordering::SeqCst};
use linux_object::process::{LinuxProcess, ProcessExt, RLimit, RLIMIT_NOFILE};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const SIGCHLD: usize = 17;
const PIDFD: u64 = CloneFlags::PIDFD.bits() as u64;
const SETTID: u64 = CloneFlags::PARENT_SETTID.bits() as u64;
const VFORK: u64 = (CloneFlags::VFORK.bits() | CloneFlags::VM.bits()) as u64;
/// What a word holds before anything is written to it.
const A_SEED: i32 = -0x55;
const WNOHANG: u32 = 1;

fn a_process(pid: KoID) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "words", LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// An `i32` word the kernel cannot write: one byte into `buf`.
fn unwritable(buf: &mut [u8; 16]) -> usize {
    buf.as_mut_ptr() as usize + 1
}

/// A `struct clone_args`, version 0, with exit signal `SIGCHLD`.
fn clone_args(flags: u64, pidfd: usize, parent_tid: usize) -> [u64; 8] {
    [
        flags,
        pidfd as u64,
        0,
        parent_tid as u64,
        SIGCHLD as u64,
        0,
        0,
        0,
    ]
}

async fn clone3(sys: &Syscall<'_>, mut words: [u64; 8]) -> SysResult {
    let at = UserInPtr::from(words.as_mut_ptr() as usize);
    sys.sys_clone3(at, CLONE_ARGS_SIZE_VER0).await
}

async fn no_child(sys: &Syscall<'_>) -> bool {
    sys.sys_wait4(-1, UserOutPtr::from(0), WNOHANG, UserOutPtr::from(0))
        .await
        == Err(LxError::ECHILD)
}

#[test]
fn a_pidfd_word_it_cannot_write_makes_no_child() {
    on_a_thread(&a_process(49_240), |sys| {
        Box::pin(async move {
            let files = sys.linux_process().file_count();
            let mut buf = [0u8; 16];
            let bad = unwritable(&mut buf);
            assert_eq!(
                clone3(sys, clone_args(PIDFD, bad, 0)).await,
                Err(LxError::EFAULT),
                "clone3"
            );
            assert!(no_child(sys).await, "clone3 made a child all the same");
            // Legacy clone hands the pidfd over in `parent_tid`'s word.
            let flags = CloneFlags::PIDFD.bits() | SIGCHLD;
            assert_eq!(
                sys.sys_clone(flags, 0, bad.into(), 0, UserOutPtr::from(0))
                    .await,
                Err(LxError::EFAULT),
                "clone"
            );
            assert!(no_child(sys).await, "clone made a child all the same");
            // And a vfork's would never have run, so it is never waited for.
            assert_eq!(
                clone3(sys, clone_args(VFORK | PIDFD, bad, 0)).await,
                Err(LxError::EFAULT),
                "vfork"
            );
            assert!(no_child(sys).await, "the vfork made a child all the same");
            assert_eq!(sys.linux_process().file_count(), files, "a descriptor");
            assert_eq!(buf, [0; 16], "a byte was written");
        })
    });
}

/// `copy_process` reserves the pidfd's descriptor before anything else of
/// it: a table that has no room makes no child either.
#[test]
fn a_pidfd_with_no_descriptor_left_for_it_makes_no_child() {
    on_a_thread(&a_process(49_242), |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            let files = lp.file_count();
            let max = lp.rlimit(RLIMIT_NOFILE, None, false).unwrap().max;
            lp.rlimit(
                RLIMIT_NOFILE,
                Some(RLimit {
                    cur: files as u64,
                    max,
                }),
                false,
            )
            .unwrap();
            let mut word = A_SEED;
            let at = &mut word as *mut i32 as usize;
            assert_eq!(
                clone3(sys, clone_args(PIDFD, at, 0)).await,
                Err(LxError::EMFILE)
            );
            assert!(no_child(sys).await, "a child all the same");
            assert_eq!(word, A_SEED, "the word was written");
            // Without a pidfd the same table forks.
            let pid = clone3(sys, clone_args(0, 0, 0))
                .await
                .expect("a plain fork");
            assert_eq!(
                sys.sys_wait4(pid as i32, UserOutPtr::from(0), 0, UserOutPtr::from(0))
                    .await,
                Ok(pid)
            );
        })
    });
}

static PIDFD_WORD: AtomicI32 = AtomicI32::new(A_SEED);
static TID_WORD: AtomicI32 = AtomicI32::new(A_SEED);
static SEEN_PIDFD: AtomicI32 = AtomicI32::new(0);
static SEEN_TID: AtomicI32 = AtomicI32::new(0);
/// 1 when the child found the pidfd's number in its own descriptor table.
static CHILD_HELD_IT: AtomicI32 = AtomicI32::new(-1);

/// The vfork child: look at the two words, and at whether its own table
/// holds the pidfd, then exit.
///
/// It looks before its body exists. `Thread::start` calls the `ThreadFn` to
/// build the body and only then hands it to the executor, so what is read
/// here is what a child that ran the moment it could would find. Read in
/// the body, it would find both words written even with them written after
/// `start`: the executor here runs the child only once the parent awaits.
fn look_at_the_words(child: CurrentThread) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    let fd = PIDFD_WORD.load(SeqCst);
    SEEN_PIDFD.store(fd, SeqCst);
    SEEN_TID.store(TID_WORD.load(SeqCst), SeqCst);
    let held = fd >= 0
        && child
            .proc()
            .linux()
            .get_file_like((fd as usize).into())
            .is_ok();
    CHILD_HELD_IT.store(held as i32, SeqCst);
    // Dropping `child` ends the thread, so the body is where it goes.
    Box::pin(async move { drop(child) })
}

#[test]
fn a_vfork_child_finds_its_pidfd_and_its_tid_written_when_it_starts() {
    on_a_thread(&a_process(49_241), |sys| {
        Box::pin(async move {
            let vfork = Syscall {
                thread: sys.thread,
                thread_fn: look_at_the_words,
                syscall_entry: 0,
            };
            let at = |word: &AtomicI32| word as *const AtomicI32 as usize;
            let pid = clone3(
                &vfork,
                clone_args(VFORK | PIDFD | SETTID, at(&PIDFD_WORD), at(&TID_WORD)),
            )
            .await
            .expect("the vfork");
            let fd = PIDFD_WORD.load(SeqCst);
            assert!(fd >= 0, "the pidfd's word got {}", fd);
            assert_eq!(
                SEEN_PIDFD.load(SeqCst),
                fd,
                "the child ran before the pidfd's number was written"
            );
            assert_eq!(TID_WORD.load(SeqCst), pid as i32, "parent_tid");
            assert_eq!(
                SEEN_TID.load(SeqCst),
                pid as i32,
                "the child ran before its tid was written"
            );
            // Its table is a copy made before the pidfd was, as on Linux
            // (`copy_files` comes before `pidfd_prepare`).
            assert_eq!(CHILD_HELD_IT.load(SeqCst), 0, "the child holds the pidfd");
            assert_eq!(
                sys.sys_wait4(pid as i32, UserOutPtr::from(0), 0, UserOutPtr::from(0))
                    .await,
                Ok(pid)
            );
        })
    });
}
