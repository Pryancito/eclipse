//! The six session and process-group syscalls, asked through the syscall.
//!
//! The pure halves are covered twice over: `setpgid_verdict` has thirteen
//! tests in `linux-object` and the two argument gates that live here,
//! `setpgid_args` and `resolve_pid_arg`, have `setpgid_argument_tests`.
//! What nothing could see from either side is the wiring: whether the
//! crate asks those gates at all, whether the number they agreed to
//! reaches the process, whether the reader reads back the same field, and
//! whether a refusal leaves the old membership where it was. A kernel that
//! resolved the arguments itself and never called `setpgid_args` passes
//! every one of those nineteen tests.
//!
//! It matters because a process group is a kill list. `send_signal_to_pgrp`
//! walks every live process whose effective pgid matches the foreground
//! group, which is how one Ctrl-C reaches a whole pipeline, so a `setpgid`
//! that files a process under the wrong number either hands it somebody
//! else's Ctrl-C or puts it where no terminal signal can reach it at all.
//!
//! Every test here works on a FORKED child, because a process made from
//! nothing is a session leader by default -- an unset sid resolves to the
//! process's own pid -- and a session leader is pinned to its group and may
//! neither move nor start a session. A fork inherits its parent's session,
//! which is the ordinary state of every process a shell runs, and is also
//! the reason the daemonize idiom forks before it calls `setsid`.
//!
//! Fixed pids live in 47_12x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// A pid nothing in this binary has.
const A_MISSING_PID: i32 = 999_999;

/// A shell: a process of its own that leads its own session, as anything
/// made from nothing here does.
fn a_shell(pid: KoID) -> Arc<Process> {
    Process::create_linux(&ROOT_JOB, RamFS::new(), 0, None, pid).unwrap()
}

/// That shell and a fresh child of it, forked and not exec'd: the job
/// -control idiom, and the only shape in which these calls are allowed at
/// all.
fn a_shell_and_its_child(pid: KoID) -> (Arc<Process>, Arc<Process>) {
    let shell = a_shell(pid);
    let child = Process::fork_from(&shell).unwrap();
    (shell, child)
}

#[test]
fn a_forked_child_inherits_the_group_and_the_session_and_leads_neither() {
    // The four readers at once, from the child, because each of them reads
    // a different field and three of the four answer the same number in
    // the obvious test: a process that leads its own session has pgid ==
    // sid == pid == ppid-of-nothing. Here they are all different.
    let (shell, child) = a_shell_and_its_child(47_120);
    let (shell_pid, child_pid) = (shell.id(), child.id());
    // A leader for the child, so that the harness thread is NOT it: a
    // process's first thread is given a tid equal to its pid, as Linux
    // gives the thread-group leader, and on that thread `getpid` and
    // `gettid` answer the same number whichever field they read.
    let leader = Thread::create_linux(&child).unwrap();
    assert_eq!(
        leader.id(),
        child_pid,
        "the first thread of a process is its leader and wears its pid"
    );
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_getpid(),
                Ok(child_pid as usize),
                "getpid did not answer the calling process"
            );
            assert_eq!(
                sys.sys_getppid(),
                Ok(shell_pid as usize),
                "getppid did not answer the process that forked this one"
            );
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(shell_pid as usize),
                "a forked child did not join its parent's process group"
            );
            assert_eq!(
                sys.sys_getsid(0),
                Ok(shell_pid as usize),
                "a forked child did not join its parent's session"
            );

            // And the thread is not the process: this one is not the
            // group leader, so its tid is a number of its own. A kernel
            // that answered the thread id to `getpid` would be right on
            // every single-threaded leader and wrong on every thread.
            let tid = sys.sys_gettid().unwrap();
            assert_ne!(
                tid, child_pid as usize,
                "the harness thread is the leader, so this proves nothing"
            );
            assert_eq!(sys.sys_getpid(), Ok(child_pid as usize));
        })
    });
}

#[test]
fn a_zero_pgid_is_a_group_of_the_targets_own_and_not_of_the_callers() {
    // `setpgid(pid, 0)` means "a group whose number is the TARGET's pid",
    // and that is the whole of `fg`/`bg`: a shell puts each job into a
    // group named after the job, not after the shell, or every job would
    // share one group and one Ctrl-C would hit them all. Told to use the
    // caller's pid instead, this kernel would answer Ok and leave the
    // child exactly where it already was, which is why it is read back.
    let (shell, child) = a_shell_and_its_child(47_121);
    let (shell_pid, child_pid) = (shell.id(), child.id());
    on_a_thread(&shell, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_getpgid(child_pid as i32),
                Ok(shell_pid as usize),
                "the child did not start in its parent's group"
            );
            assert_eq!(sys.sys_setpgid(child_pid as i32, 0), Ok(0));
            assert_eq!(
                sys.sys_getpgid(child_pid as i32),
                Ok(child_pid as usize),
                "setpgid with a zero pgid did not make the target its own leader"
            );

            // The shell itself did not move, and may not: it leads its
            // session, so its pid IS the session id and its group is
            // pinned to that number.
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(shell_pid as usize),
                "the caller's own group moved with the target's"
            );
            assert_eq!(
                sys.sys_setpgid(0, 0),
                Err(LxError::EPERM),
                "a session leader was allowed to change its process group"
            );
        })
    });
}

#[test]
fn a_negative_pgid_is_einval_and_is_judged_before_the_pid_is_looked_up() {
    // The order is `do_setpgid`'s, and it is the one thing between
    // `setpgid(0, -1)` and filing the caller under group
    // 0xffff_ffff_ffff_ffff, where no signal sent to any real group can
    // reach it. The request below is wrong in two ways at once on
    // purpose: the errno says which gate answered.
    let (_shell, child) = a_shell_and_its_child(47_122);
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            let was = sys.sys_getpgid(0).unwrap();
            assert_eq!(
                sys.sys_setpgid(A_MISSING_PID, -1),
                Err(LxError::EINVAL),
                "the pid was looked up before the pgid was judged"
            );
            assert_eq!(sys.sys_setpgid(0, -1), Err(LxError::EINVAL));
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(was),
                "a refused setpgid moved the caller's group anyway"
            );
        })
    });
}

#[test]
fn a_negative_pid_is_esrch_and_not_a_pid_sign_extended_into_the_sky() {
    // `pid_t` is signed, and these three used to take it as a `usize`, so
    // a negative pid arrived sign-extended: `getpgid(-1)` asked about pid
    // 0xffff_ffff_ffff_ffff and `setpgid(-1, 0)` would have made that its
    // own group leader.
    let (_shell, child) = a_shell_and_its_child(47_123);
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            let was = sys.sys_getpgid(0).unwrap();
            assert_eq!(sys.sys_getpgid(-1), Err(LxError::ESRCH));
            assert_eq!(sys.sys_getsid(-1), Err(LxError::ESRCH));
            assert_eq!(sys.sys_setpgid(-1, 0), Err(LxError::ESRCH));
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(was),
                "a setpgid of a negative pid moved the caller's group"
            );
        })
    });
}

#[test]
fn a_pid_that_names_nothing_is_esrch_for_each_of_the_three_calls_that_take_one() {
    let (_shell, child) = a_shell_and_its_child(47_124);
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            assert_eq!(sys.sys_getpgid(A_MISSING_PID), Err(LxError::ESRCH));
            assert_eq!(sys.sys_getsid(A_MISSING_PID), Err(LxError::ESRCH));
            assert_eq!(sys.sys_setpgid(A_MISSING_PID, 0), Err(LxError::ESRCH));
        })
    });
}

#[test]
fn setsid_moves_the_session_and_the_group_in_one_go_and_answers_the_new_id() {
    // `setsid` writes TWO fields -- the session id and the process group
    // id -- and both have to become the caller's own pid, or the session
    // ends up named after a group that nobody is in. It is also the one
    // call here that returns an id rather than 0.
    let (shell, child) = a_shell_and_its_child(47_125);
    let (shell_pid, child_pid) = (shell.id(), child.id());
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            assert_eq!(sys.sys_getsid(0), Ok(shell_pid as usize));
            assert_eq!(
                sys.sys_setsid(),
                Ok(child_pid as usize),
                "setsid did not answer the new session id"
            );
            assert_eq!(
                sys.sys_getsid(0),
                Ok(child_pid as usize),
                "setsid did not start a session of the caller's own"
            );
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(child_pid as usize),
                "setsid left the caller in its old process group"
            );
        })
    });
}

#[test]
fn a_group_leader_may_not_start_a_session_and_the_refusal_changes_nothing() {
    // `ksys_setsid` refuses when the caller's pid already NAMES a process
    // group, because the new session would be given that same number for
    // its own group and two unrelated groups would share an id. This is
    // the rule the daemonize idiom is built around: fork first, so the
    // child is not a leader of anything, and only then `setsid`.
    //
    // The refusal has to be a no-op in both fields at once. One that
    // wrote the session id before judging would leave a process whose
    // session says it leads and whose group says it does not.
    let (shell, child) = a_shell_and_its_child(47_126);
    let (shell_pid, child_pid) = (shell.id(), child.id());
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            assert_eq!(sys.sys_setpgid(0, 0), Ok(0));
            assert_eq!(sys.sys_getpgid(0), Ok(child_pid as usize));
            assert_eq!(
                sys.sys_setsid(),
                Err(LxError::EPERM),
                "a process group leader was allowed to start a session"
            );
            assert_eq!(
                sys.sys_getsid(0),
                Ok(shell_pid as usize),
                "a refused setsid moved the session anyway"
            );
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(child_pid as usize),
                "a refused setsid moved the process group anyway"
            );
        })
    });
}

#[test]
fn the_group_a_process_is_moved_into_has_to_exist_in_its_own_session() {
    // The last of `do_setpgid`'s four rules: joining an existing group is
    // only allowed when that group is in the caller's session, and
    // `getpgid` is where the refusal is read. A process may always CREATE
    // the group named after itself, which is the case above; what it may
    // not do is name a number nobody in its session is using, because a
    // group is a kill list and an empty one is a process no terminal
    // signal can reach.
    let (shell, child) = a_shell_and_its_child(47_127);
    let other = a_shell(47_128);
    let (shell_pid, child_pid, other_pid) = (shell.id(), child.id(), other.id());
    on_a_thread(&child, move |sys| {
        Box::pin(async move {
            // A live process group, but one in a session of its own.
            assert_eq!(
                sys.sys_setpgid(0, other_pid as i32),
                Err(LxError::EPERM),
                "a process joined a group of another session"
            );
            // And one that is nobody's.
            assert_eq!(sys.sys_setpgid(0, A_MISSING_PID), Err(LxError::EPERM));
            assert_eq!(
                sys.sys_getpgid(0),
                Ok(shell_pid as usize),
                "a refused setpgid moved the caller's group anyway"
            );

            // The parent's group, on the other hand, IS in the caller's
            // session, and rejoining it is allowed -- which is what makes
            // the two refusals above about the session and not about the
            // call. Asked from a group of its own, so the move is a real
            // one and not a number that was already there.
            assert_eq!(sys.sys_setpgid(0, 0), Ok(0));
            assert_eq!(sys.sys_getpgid(0), Ok(child_pid as usize));
            assert_eq!(
                sys.sys_setpgid(0, shell_pid as i32),
                Ok(0),
                "a process could not rejoin a group of its own session"
            );
            assert_eq!(sys.sys_getpgid(0), Ok(shell_pid as usize));
        })
    });
}
