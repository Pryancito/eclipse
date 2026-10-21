//! The ten priority and scheduling-policy syscalls, asked through the
//! syscall.
//!
//! `sched_validate`, `rt_priority_bounds` and `prio_target` are the pure
//! halves and have tests of their own; `may_set_scheduler` and
//! `set_priority_verdict` live in `linux-object` with theirs. What nothing
//! could see from either side is the wiring, and this family has two
//! pieces of it that a reader would not guess from the names.
//!
//! The first is `getpriority`'s **encoding**. It does not return the nice
//! value: it returns `20 - nice`, so that the raw syscall return stays
//! non-negative and a nice of -20 cannot be mistaken for an errno. glibc
//! undoes it. A kernel that hands back the raw number makes every
//! favoured task look like a failed call.
//!
//! The second is `SCHED_RESET_ON_FORK`, which rides in the **policy**
//! argument rather than in a flags word of its own. `sched_setscheduler`
//! has to take it out before it judges the policy, and
//! `sched_getscheduler` has to put it back; drop either half and a
//! program that sets the bit reads back a policy it never asked for, or
//! one that does not set it inherits it.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{LinuxProcess, PRIO_PGRP, PRIO_PROCESS};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// Somebody who is not root.
const A_USER: u32 = 1000;

/// A policy number this kernel does not define: 0, 1, 2, 3 and 5 are taken
/// and 6 is `SCHED_DEADLINE`, which `rt_priority_bounds` also refuses.
const AN_UNKNOWN_POLICY: usize = 4;

/// A `which` for `getpriority`/`setpriority` that names nothing.
const AN_UNKNOWN_WHICH: usize = 99;

/// A thread id nothing in this binary has.
const A_MISSING_PID: usize = 999_999;

/// A real-time priority inside `1..=99`, and a higher one.
const A_PRIORITY: i32 = 50;
const A_HIGHER_PRIORITY: i32 = 80;

/// A process of its own, so that moving its priority or its credentials
/// cannot reach any other test in this binary.
fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, lp).unwrap()
}

/// A `struct sched_param` as userspace hands one over. Built per call:
/// `UserInPtr` is not `Copy`.
fn param(priority: &i32) -> UserInPtr<i32> {
    UserInPtr::from(priority as *const i32 as usize)
}

fn out_param(slot: &mut i32) -> UserOutPtr<i32> {
    UserOutPtr::from(slot as *mut i32 as usize)
}

fn out_interval(slot: &mut TimeSpec) -> UserOutPtr<TimeSpec> {
    UserOutPtr::from(slot as *mut TimeSpec as usize)
}

#[test]
fn getpriority_hands_back_twenty_minus_the_nice_so_the_return_stays_positive() {
    // Three nice values, including both ends, because the encoding runs
    // backwards: the largest number out is the smallest nice in. A kernel
    // returning the raw nice would answer 5, 19 and -20 here, and the
    // last of those is what a caller reads as an error.
    let proc = a_process(47_100, "prio-encoding");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // Root, so lowering the nice value is allowed at all.
            assert_eq!(sys.sys_setpriority(PRIO_PROCESS, 0, 5), Ok(0));
            assert_eq!(
                sys.sys_getpriority(PRIO_PROCESS, 0),
                Ok(15),
                "getpriority returned the raw nice value"
            );

            assert_eq!(sys.sys_setpriority(PRIO_PROCESS, 0, MAX_NICE as i32), Ok(0));
            assert_eq!(
                sys.sys_getpriority(PRIO_PROCESS, 0),
                Ok(1),
                "the most unfavoured task did not come back as 1"
            );

            assert_eq!(sys.sys_setpriority(PRIO_PROCESS, 0, MIN_NICE as i32), Ok(0));
            assert_eq!(
                sys.sys_getpriority(PRIO_PROCESS, 0),
                Ok(40),
                "the most favoured task did not come back as 40"
            );
        })
    });
}

#[test]
fn setpriority_clamps_the_nice_value_instead_of_refusing_it() {
    // `set_one_prio_perm`'s caller clamps rather than failing, which is
    // what makes `renice +1000` work the way the shell's users expect.
    // Both ends are asked for, because a clamp written once is easy to
    // write once.
    let proc = a_process(47_101, "prio-clamp");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_setpriority(PRIO_PROCESS, 0, 1000),
                Ok(0),
                "a nice value past the top was refused instead of clamped"
            );
            assert_eq!(
                sys.sys_getpriority(PRIO_PROCESS, 0),
                Ok(1),
                "a nice value past the top was not clamped to 19"
            );

            assert_eq!(
                sys.sys_setpriority(PRIO_PROCESS, 0, -1000),
                Ok(0),
                "a nice value past the bottom was refused instead of clamped"
            );
            assert_eq!(
                sys.sys_getpriority(PRIO_PROCESS, 0),
                Ok(40),
                "a nice value past the bottom was not clamped to -20"
            );

            // And a `which` that names nothing is EINVAL on both calls.
            assert_eq!(
                sys.sys_setpriority(AN_UNKNOWN_WHICH, 0, 0),
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_getpriority(AN_UNKNOWN_WHICH, 0),
                Err(LxError::EINVAL)
            );
        })
    });
}

#[test]
fn lowering_the_nice_value_is_the_half_that_needs_privilege() {
    // The asymmetry: being nicer is free, being less nice is not. And the
    // errno says which of the two rules refused -- EACCES means this
    // caller may touch this task but has no `RLIMIT_NICE` budget for the
    // number it asked for, where EPERM would mean it may not touch it at
    // all. Both halves are asked of the SAME task by the SAME caller, one
    // after the other, so only the direction can account for them.
    let proc = a_process(47_102, "prio-privilege");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            assert_eq!(sys.sys_setpriority(PRIO_PROCESS, 0, 0), Ok(0));
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();

            assert_eq!(
                sys.sys_setpriority(PRIO_PROCESS, 0, 5),
                Ok(0),
                "an unprivileged caller could not make itself nicer"
            );
            assert_eq!(sys.sys_getpriority(PRIO_PROCESS, 0), Ok(15));

            assert_eq!(
                sys.sys_setpriority(PRIO_PROCESS, 0, 4),
                Err(LxError::EACCES),
                "an unprivileged caller lowered its own nice value"
            );
            assert_eq!(
                sys.sys_getpriority(PRIO_PROCESS, 0),
                Ok(15),
                "a refused setpriority moved the nice value anyway"
            );
        })
    });
}

#[test]
fn the_priority_bounds_are_per_policy_and_an_unknown_policy_is_einval() {
    // The two numbers a program needs before it may call
    // `sched_setscheduler` at all. The fair policies answer 0 for both,
    // which is not "no answer": it is the only `sched_priority` they
    // accept.
    let proc = a_process(47_103, "prio-bounds");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            for policy in [SCHED_FIFO, SCHED_RR] {
                assert_eq!(
                    sys.sys_sched_get_priority_min(policy as usize),
                    Ok(MIN_RT_PRIO as usize),
                    "policy {}",
                    policy
                );
                assert_eq!(
                    sys.sys_sched_get_priority_max(policy as usize),
                    Ok(MAX_RT_PRIO as usize),
                    "policy {}",
                    policy
                );
            }
            for policy in [SCHED_NORMAL, SCHED_BATCH, SCHED_IDLE] {
                assert_eq!(
                    sys.sys_sched_get_priority_min(policy as usize),
                    Ok(0),
                    "policy {}",
                    policy
                );
                assert_eq!(
                    sys.sys_sched_get_priority_max(policy as usize),
                    Ok(0),
                    "policy {}",
                    policy
                );
            }
            assert_eq!(
                sys.sys_sched_get_priority_min(AN_UNKNOWN_POLICY),
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_sched_get_priority_max(AN_UNKNOWN_POLICY),
                Err(LxError::EINVAL)
            );
        })
    });
}

#[test]
fn the_reset_on_fork_bit_comes_out_of_the_policy_and_goes_back_in_on_the_way_out() {
    // It travels inside the policy argument, so both halves of the round
    // trip are wiring: taken out before the policy is judged, and put
    // back by the getter. The same policy is set twice, with the bit and
    // without it, so a getter that always reports it and one that never
    // does are both caught.
    let proc = a_process(47_104, "prio-resetfork");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let priority = A_PRIORITY;
            assert_eq!(
                sys.sys_sched_setscheduler(
                    0,
                    SCHED_FIFO as usize | SCHED_RESET_ON_FORK,
                    param(&priority)
                ),
                Ok(0),
                "the reset-on-fork bit was taken for part of the policy number"
            );
            assert_eq!(
                sys.sys_sched_getscheduler(0),
                Ok(SCHED_FIFO as usize | SCHED_RESET_ON_FORK),
                "the reset-on-fork bit did not survive the round trip"
            );

            assert_eq!(
                sys.sys_sched_setscheduler(0, SCHED_FIFO as usize, param(&priority)),
                Ok(0)
            );
            assert_eq!(
                sys.sys_sched_getscheduler(0),
                Ok(SCHED_FIFO as usize),
                "the policy came back carrying a flag nobody asked for"
            );
        })
    });
}

#[test]
fn a_null_param_is_einval_before_the_pointer_is_read_and_before_the_pid_is_looked_up() {
    // `do_sched_setscheduler`: `!param || pid < 0` is EINVAL, and it is
    // asked first. The pid here names nothing, so a kernel that looked it
    // up first would answer ESRCH and one that read the pointer first
    // would answer EFAULT -- three different answers to the same call,
    // and only one of them tells the caller which argument to fix.
    let proc = a_process(47_105, "prio-null-param");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_sched_setscheduler(A_MISSING_PID, SCHED_FIFO as usize, UserInPtr::from(0)),
                Err(LxError::EINVAL),
                "a null sched_param was read, or the pid was looked up first"
            );
            assert_eq!(
                sys.sys_sched_setparam(A_MISSING_PID, UserInPtr::from(0)),
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_sched_getparam(A_MISSING_PID, UserOutPtr::from(0)),
                Err(LxError::EINVAL)
            );

            // With a pointer that is fine, the missing pid is what is left
            // to complain about.
            let priority = A_PRIORITY;
            assert_eq!(
                sys.sys_sched_setscheduler(A_MISSING_PID, SCHED_FIFO as usize, param(&priority)),
                Err(LxError::ESRCH),
                "a pid that names nothing was not ESRCH"
            );
        })
    });
}

#[test]
fn sched_setparam_moves_the_priority_and_leaves_the_policy_where_it_was() {
    // The difference between the two setters is the whole reason there are
    // two: `sched_setparam` keeps the policy. A kernel that reset it to
    // the fair default would refuse the real-time priority it was just
    // handed, or accept it and never run the thread as real-time again.
    let proc = a_process(47_106, "prio-setparam");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let priority = A_PRIORITY;
            assert_eq!(
                sys.sys_sched_setscheduler(0, SCHED_RR as usize, param(&priority)),
                Ok(0)
            );

            let higher = A_HIGHER_PRIORITY;
            assert_eq!(sys.sys_sched_setparam(0, param(&higher)), Ok(0));
            assert_eq!(
                sys.sys_sched_getscheduler(0),
                Ok(SCHED_RR as usize),
                "sched_setparam changed the policy"
            );

            let mut back = 0i32;
            assert_eq!(sys.sys_sched_getparam(0, out_param(&mut back)), Ok(0));
            assert_eq!(
                back, A_HIGHER_PRIORITY,
                "sched_getparam did not report the priority sched_setparam set"
            );
        })
    });
}

#[test]
fn sched_rr_get_interval_is_zero_for_everything_but_round_robin() {
    // The one call whose answer is a struct rather than a number, and the
    // buffer is seeded with something recognizable so that "wrote
    // nothing" cannot pass as "wrote zero". Both policies are asked of
    // the same thread, so only the policy can account for the two
    // answers.
    let proc = a_process(47_107, "prio-interval");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let priority = A_PRIORITY;
            assert_eq!(
                sys.sys_sched_setscheduler(0, SCHED_RR as usize, param(&priority)),
                Ok(0)
            );

            let mut round_robin = TimeSpec { sec: 7, nsec: 7 };
            assert_eq!(
                sys.sys_sched_rr_get_interval(0, out_interval(&mut round_robin)),
                Ok(0)
            );
            assert_eq!(
                (round_robin.sec, round_robin.nsec),
                (0, 100_000_000),
                "a round-robin thread reported no timeslice"
            );

            assert_eq!(
                sys.sys_sched_setscheduler(0, SCHED_FIFO as usize, param(&priority)),
                Ok(0)
            );
            let mut fifo = TimeSpec { sec: 7, nsec: 7 };
            assert_eq!(
                sys.sys_sched_rr_get_interval(0, out_interval(&mut fifo)),
                Ok(0)
            );
            assert_eq!(
                (fifo.sec, fifo.nsec),
                (0, 0),
                "a FIFO thread reported a round-robin timeslice"
            );
        })
    });
}

#[test]
fn getpriority_answers_the_most_favoured_of_the_set_and_not_the_least() {
    // Over a set of tasks `getpriority` is a MAXIMUM, and that is only
    // visible with two tasks that differ: every other test here asks
    // about a single target, where the maximum and the minimum are the
    // same number. Because the encoding runs backwards, a minimum would
    // report the LEAST favoured task of the group, which is the opposite
    // of what the call promises -- and `renice -g` reads the answer back
    // to decide what to ask for next.
    //
    // The set is a process group: the process is born with pgid 0, so
    // `effective_pgid` answers its own pid, which nothing else in this
    // binary has.
    let proc = a_process(47_108, "prio-most-favoured");
    let sibling = Thread::create_linux(&proc).unwrap();
    let group = proc.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                group.threads().len(),
                2,
                "the group under test does not have two tasks in it"
            );

            // Root, so moving either way is allowed at all. 20 - 10 = 10
            // for the one that gave way, 20 - -5 = 25 for the favoured
            // one.
            assert_eq!(sys.sys_setpriority(PRIO_PROCESS, 0, 10), Ok(0));
            assert_eq!(
                sys.sys_setpriority(PRIO_PROCESS, sibling.id() as usize, -5),
                Ok(0)
            );
            assert_eq!(
                sys.sys_getpriority(PRIO_PGRP, 0),
                Ok(25),
                "getpriority reported the least favoured task of the group"
            );

            // And the group walk really is a walk: renicing the group has
            // to move the sibling too, or the favoured 25 would still be
            // there to be read.
            assert_eq!(sys.sys_setpriority(PRIO_PGRP, 0, 7), Ok(0));
            assert_eq!(
                sys.sys_getpriority(PRIO_PGRP, 0),
                Ok(13),
                "setpriority over a group left a task where it was"
            );
        })
    });
}
