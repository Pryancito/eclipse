//! The order of the two halves of `execve`, and what the first half may
//! touch.
//!
//! `sys_execve` is two stages with a line between them. Before it, every
//! failure is an ordinary errno handed back to a caller that is still whole:
//! the path does not resolve, it is not executable, it is on a `noexec`
//! mount, its `#!` line names an interpreter that is not installed. After it,
//! the process has lost every thread but one and its address space has been
//! replaced, so there is nobody left to report an error to.
//!
//! The kill of the sibling threads was on the WRONG SIDE of that line -- it
//! was the first thing the function did, above all four checks -- so
//! `execve("/does/not/exist")` from a threaded program answered `ENOENT` to a
//! program that no longer had its threads. Linux calls `de_thread()` from
//! `begin_new_exec()`, after the `bprm` is built, which is the same line.
//!
//! What holds the order now is a type: [`super::ExecPreflight`] is built by
//! the last check and demanded by the kill, so the two cannot be swapped
//! without the compiler saying so. These tests cover the parts a type cannot:
//! which thread the kill picks, and that the check that builds the token
//! costs the caller nothing when it refuses.
//!
//! Fixed pids live in 43_39x here; they are a global table, so every file
//! that pins one has its own range.

use super::*;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{ThreadState, ROOT_JOB};

/// A process with `threads` threads, the first of which stands in for the one
/// that called `execve`.
fn a_process_with(pid: KoID, threads: usize) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "threaded",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    for _ in 0..threads {
        Thread::create_linux(&proc).unwrap();
    }
    proc
}

/// A token, by the only route there is to one: a head that is not a script.
/// Four zero bytes are not `#!`, which is the whole of what the preflight
/// looks at before deciding there is no interpreter chain to walk.
fn a_token(proc: &Arc<Process>) -> ExecPreflight {
    execve_preflight(&[0u8; 4], 0, proc.linux().root_inode())
        .expect("a head that is not a shebang has no interpreter to resolve")
}

/// The caller of `execve` is the one thread that comes out the other side.
/// Picking it out of the list is the whole of this function's job, and getting
/// it backwards means `execve` kills the thread that asked for it.
#[test]
fn the_caller_is_the_only_thread_left_running() {
    let proc = a_process_with(43_390, 3);
    let threads = proc.threads();
    let me = threads[0].id();

    let killed = kill_siblings(&proc, me);

    assert_eq!(killed.len(), 2, "the two that are not the caller");
    assert!(
        killed.iter().all(|t| t.id() != me),
        "the caller must not be in its own kill list"
    );
    assert_ne!(
        threads[0].state(),
        ThreadState::Dying,
        "the thread that called execve is the one that goes on to run the new image"
    );
    for sibling in &killed {
        assert_eq!(
            sibling.state(),
            ThreadState::Dying,
            "sibling {}",
            sibling.id()
        );
    }
}

/// By id, not by position. The caller is as likely to be the last thread the
/// process made as the first -- `pthread_create` hands `execve` to whichever
/// thread reaches it -- so a `skip(1)` or a `pop()` in there is a function
/// that works only for the thread that happened to be at the front.
#[test]
fn the_caller_is_picked_by_id_and_not_by_position() {
    let proc = a_process_with(43_391, 3);
    let threads = proc.threads();
    let me = threads[2].id();

    let killed = kill_siblings(&proc, me);

    let mut left: Vec<KoID> = killed.iter().map(|t| t.id()).collect();
    left.sort_unstable();
    let mut expected = vec![threads[0].id(), threads[1].id()];
    expected.sort_unstable();
    assert_eq!(left, expected);
    assert_ne!(threads[2].state(), ThreadState::Dying);
}

/// `de_thread` comes back with the caller alone in the list, which is what
/// lets the leader's tid be handed on without a second thread holding it.
/// The siblings' run loops are stood in for by a host thread that finishes
/// them once they are dying -- and holds open the step `Thread::terminate`
/// takes between signalling `THREAD_TERMINATED` and leaving the list, which
/// on a real CPU is too short to land in on purpose.
#[test]
fn de_thread_comes_back_with_the_caller_alone_in_the_list() {
    extern crate std;
    let proc = a_process_with(43_395, 3);
    let threads = proc.threads();
    let me = threads[1].id();
    let token = a_token(&proc);

    let others: Vec<Arc<Thread>> = threads.iter().filter(|t| t.id() != me).cloned().collect();
    std::thread::spawn(move || {
        while others.iter().any(|t| t.state() != ThreadState::Dying) {
            std::thread::sleep(core::time::Duration::from_millis(5));
        }
        for t in &others {
            t.signal_set(Signal::THREAD_TERMINATED);
        }
        std::thread::sleep(core::time::Duration::from_millis(100));
        for t in &others {
            t.terminate_abandoned();
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let proc2 = proc.clone();
    std::thread::spawn(move || {
        async_std::task::block_on(de_thread(&proc2, me, &token));
        let _ = tx.send(());
    });
    rx.recv_timeout(core::time::Duration::from_secs(5))
        .expect("de_thread never came back");
    assert_eq!(proc.thread_ids(), [me]);
}

/// The ordinary case: a shell forks and execs, single-threaded, and there is
/// nothing to kill and nothing to wait for. Returning the caller here would
/// make every `execve` in the system wait for the death of the thread doing
/// the waiting.
#[test]
fn a_single_threaded_process_has_no_siblings_to_kill() {
    let proc = a_process_with(43_392, 1);
    let me = proc.threads()[0].id();

    assert!(kill_siblings(&proc, me).is_empty());
}

/// A `#!` line naming an interpreter that is not installed is the common
/// failure of the whole syscall -- a script from a distribution that assumes
/// `/bin/bash` -- and it has to come back as an errno with the process
/// untouched. This is the check that builds the token, so it is the last
/// chance the caller gets.
#[test]
fn a_shebang_naming_a_missing_interpreter_is_refused_and_costs_the_caller_nothing() {
    let proc = a_process_with(43_393, 3);
    let before = proc.thread_count();

    let got = execve_preflight(
        b"#!/nonexistent/interpreter\n",
        0,
        proc.linux().root_inode(),
    );

    assert!(
        got.is_err(),
        "there is no such interpreter to run the script with"
    );
    assert_eq!(
        proc.thread_count(),
        before,
        "a refusal on this side of the line leaves the thread group whole"
    );
    assert!(
        proc.threads()
            .iter()
            .all(|t| t.state() != ThreadState::Dying),
        "and leaves every thread of it alive"
    );
}

/// The other half of the same property: the preflight is reached for every
/// `execve`, so an ordinary binary has to come out of it with a token and
/// without having touched anything. A preflight that refused a plain ELF
/// would turn every exec in the system into an errno.
#[test]
fn an_image_that_is_not_a_script_passes_the_preflight() {
    let proc = a_process_with(43_394, 2);
    let before = proc.thread_count();

    let _token = a_token(&proc);

    assert_eq!(proc.thread_count(), before);
    assert!(proc
        .threads()
        .iter()
        .all(|t| t.state() != ThreadState::Dying));
}
