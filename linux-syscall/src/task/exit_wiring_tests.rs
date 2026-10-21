//! `exit(2)` and `exit_group(2)`, asked through the syscall.
//!
//! `exit_code_of` is pure and `exit_code_tests` next door already fixes the
//! low byte it keeps. What has no test is everything around it: which of the
//! two calls ends a THREAD and which ends a PROCESS, where the code a thread
//! named goes when that thread turns out to be the last one, and the word
//! `set_tid_address` registered, which a thread's exit has to zero because
//! that is what `pthread_join` waits on.
//!
//! The dispatch pair (`EXIT` against `EXIT_GROUP`) is watched by
//! `dispatch_tests`, but by syscall NUMBER: it never calls either one.
//!
//! These are the only two calls here that take `&mut self`, so the `Syscall`
//! is rebuilt inside the closure from the one the harness lends. And the
//! calls kill the thread they are made on, which the harness survives --
//! nothing is awaited after them, so the answer still reaches the channel --
//! but it does mean every assertion has to happen inside the one closure,
//! or afterwards through a handle cloned before it.
//!
//! Fixed pids live in 47_17x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{wait_status_exited, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// A code whose low byte is not the whole of it, and that byte.
const A_WIDE_CODE: i32 = 0x1_2a;
const ITS_LOW_BYTE: i64 = 0x2a;

/// Codes to tell the two calls apart by, so neither can pass for the other.
const A_CODE: i32 = 7;
const ANOTHER_CODE: i32 = 5;

/// What a thread's `clear_child_tid` word holds before anything clears it,
/// so "left alone" cannot pass for "cleared".
const A_SEED: i32 = 0x55;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// The `Syscall` the harness lends, as a `&mut` one. `exit` and `exit_group`
/// are the only calls in this file that need it: every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// Whether a wait status says the process exited rather than died of a
/// signal, and with what code. `WIFEXITED` and `WEXITSTATUS`.
fn wifexited(status: i32) -> bool {
    status & 0x7f == 0
}
fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}

#[test]
fn exit_group_ends_the_whole_process_however_many_threads_it_has() {
    // The difference the two calls exist for, from the `exit_group` side: it
    // ends the process, and the thread count has no say in it. A sibling
    // thread is registered first precisely so that this thread is NOT the
    // last one -- which is the condition `exit` turns on and `exit_group`
    // must not.
    let proc = a_process(47_170, "exit-group-any");
    let sibling = Thread::create_linux(&proc).unwrap();
    let watch = proc.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let _ = &sibling;
            let mut sys = as_mut(sys);
            assert_eq!(
                watch.thread_count(),
                2,
                "the process under test does not have a second thread"
            );

            assert_eq!(sys.sys_exit_group(ANOTHER_CODE), Ok(0));
            assert_eq!(
                watch.exit_code(),
                Some(i64::from(ANOTHER_CODE)),
                "exit_group left the process running, or stored another code"
            );
        })
    });
}

#[test]
fn exit_from_a_thread_that_is_not_the_last_leaves_the_process_running() {
    // And the same difference from the `exit` side. `_exit(2)` ends the
    // calling thread; a process whose other threads are still there is still
    // there. Reading this wrong is how a `pthread_exit` from a worker takes
    // the whole program down.
    let proc = a_process(47_171, "exit-not-last");
    let sibling = Thread::create_linux(&proc).unwrap();
    let watch = proc.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let _ = &sibling;
            let mut sys = as_mut(sys);
            assert_eq!(watch.thread_count(), 2);

            assert_eq!(sys.sys_exit(A_CODE), Ok(0));
            assert_eq!(
                watch.exit_code(),
                None,
                "a thread's own exit took the whole process with it"
            );
        })
    });
}

#[test]
fn the_code_a_thread_exited_with_is_the_processs_code_once_the_last_one_goes() {
    // `exit(2)` from a thread that is not the last still has to say what the
    // process's code will be, because it may turn out to have been the last
    // one by the time the count is read: two concurrent `_exit` calls can
    // both see two threads and neither win the race. The code is published as
    // a hint on the way out, and the thread that empties the process list
    // picks it up. Without it, that path invented `Exited(0)`, so a process
    // whose threads all called `_exit(1)` was reported as a success.
    //
    // The sibling here is the one that empties the list, and it never ran:
    // what it exercises is the handover, not the scheduler.
    //
    // What this cannot pin is WHERE in the exit the hint is published. The
    // mutation batch moved it after `self.exit()` and after a real
    // `terminate()`, and both survived -- because the hint is read by
    // whichever thread empties the list, which here is strictly later, and on
    // the one path where the exiting thread empties the list itself
    // (`last_thread_of`) `Process::exit` sets the status directly and the
    // hint never decides. The ordering only bites in the race the comment
    // above describes, and that race needs two running threads. What is
    // pinned is the hint's value reaching the process, which is what the two
    // dead mutants -- no hint at all, and a hint of zero -- say.
    let proc = a_process(47_172, "exit-hint");
    let sibling = Thread::create_linux(&proc).unwrap();
    let watch = proc.clone();
    let held = sibling.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let _ = &held;
            let mut sys = as_mut(sys);
            assert_eq!(sys.sys_exit(A_CODE), Ok(0));
            assert_eq!(watch.exit_code(), None);
        })
    });

    assert_eq!(
        proc.exit_code(),
        None,
        "the process went while its other thread was still in the list"
    );
    // The sibling never ran, so the `CurrentThread::drop` that normally takes
    // a thread out of its process's list never happens for it.
    // `terminate_abandoned` is the door built for exactly that, and it is the
    // `remove_thread` path the hint is read on.
    sibling.terminate_abandoned();
    assert_eq!(
        proc.exit_code(),
        Some(i64::from(A_CODE)),
        "the code the exiting thread named was not the one the process kept"
    );
}

#[test]
fn exit_from_the_only_thread_is_the_processs_own_exit_and_keeps_the_low_byte() {
    // The other side of the same count: "last" is one thread, not none,
    // because the caller is still in the list while it runs. Off by one here
    // and a single-threaded program's `_exit` leaves the process neither
    // exited nor running.
    //
    // And the code goes through `exit_code_of` on this path too, which is a
    // second call site of it and its own chance to be forgotten.
    let proc = a_process(47_173, "exit-only");
    let watch = proc.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            assert_eq!(watch.thread_count(), 1, "something else is in this process");

            assert_eq!(sys.sys_exit(A_WIDE_CODE), Ok(0));
            assert_eq!(
                watch.exit_code(),
                Some(ITS_LOW_BYTE),
                "the only thread's exit was not the process's, or kept more than a byte"
            );
        })
    });
}

#[test]
fn a_negative_code_is_an_exit_of_its_low_byte_and_not_a_death_by_signal() {
    // `exit(-1)` is the commonest error exit there is, and this kernel spells
    // a death by signal as a NEGATIVE exit code -- so storing the caller's
    // whole `int` reached the parent as `WIFSIGNALED` with `WTERMSIG == 1`,
    // and a shell printed "Hangup" for a program that had simply failed.
    //
    // Asked of BOTH calls, because each carries its own call to
    // `exit_code_of` and losing it from one is losing it.
    for (pid, name, group) in [
        (47_174, "exit-neg", false),
        (47_175, "exit-group-neg", true),
    ] {
        let proc = a_process(pid, name);
        let watch = proc.clone();
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let mut sys = as_mut(sys);
                let answer = if group {
                    sys.sys_exit_group(-1)
                } else {
                    sys.sys_exit(-1)
                };
                assert_eq!(answer, Ok(0));

                let code = watch.exit_code().expect("the process did not exit");
                assert_eq!(code, 255, "exit(-1) did not keep its low byte");
                let status = wait_status_exited(code);
                assert!(
                    wifexited(status),
                    "exit(-1) reached the parent as a death by signal: status {:#x}",
                    status
                );
                assert_eq!(wexitstatus(status), 255);
            })
        });
    }
}

#[test]
fn a_thread_that_exits_clears_the_word_set_tid_address_registered() {
    // The chain `pthread_join` rides on, and neither end of it had a test
    // that ran the other: `set_tid_address` stores the word (glibc and musl
    // both call it for every thread they create), and the thread's exit has
    // to zero it and wake whoever waits there. A thread that died without
    // clearing it left `pthread_join` blocked for the life of the process,
    // with nothing userspace could do about it.
    //
    // The word is seeded, so "never written" cannot pass for "cleared".
    let proc = a_process(47_176, "exit-tidaddr");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut word: i32 = A_SEED;
            let at = &mut word as *mut i32 as usize;
            assert_eq!(
                sys.sys_set_tid_address(UserOutPtr::from(at)),
                Ok(sys.thread.id() as usize)
            );
            assert_eq!(
                sys.thread.tid_address().as_addr(),
                at,
                "the word was not registered"
            );

            let mut sys = as_mut(sys);
            assert_eq!(sys.sys_exit(0), Ok(0));
            assert_eq!(
                word, 0,
                "the exiting thread left the word a join is waiting on as it was"
            );
        })
    });
}
