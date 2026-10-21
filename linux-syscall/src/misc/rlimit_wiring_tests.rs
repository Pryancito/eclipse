//! The sixteen resource limits, asked through the three syscalls that carry
//! them.
//!
//! `rlimit_tests` in `linux-object` covers the rules thoroughly -- the four
//! of `rlimit_check`, the six id comparisons of `may_touch_limits_of`, the
//! boot table, and the store -- and every one of them is a pure function
//! that is handed what it needs. What none of them can say is who asks them
//! what, and that is where this area's bug was:
//!
//!  * `prlimit64` had no test at all, by name or by number, and the three
//!    lines that are only its own are exactly the ones a pure test cannot
//!    reach: which process `pid` names, whose `CAP_SYS_RESOURCE` the
//!    `may_raise_hard` it passes down is, and the OLD limit going out while
//!    the new one goes in.
//!  * the bug it was rewritten for is a wiring bug of that shape. Every call
//!    landed on the CALLER -- `pid` was read only to be printed -- so
//!    `prlimit(other, RLIMIT_NOFILE, &new)` moved the caller's own
//!    descriptor budget and reported success. The gates all said yes to the
//!    right question; the answer reached the wrong process.
//!  * `getrlimit` and `setrlimit` are one line of argument passing each,
//!    into `prlimit64`, which is where a pid or a pointer in the wrong slot
//!    would live. `dispatch_tests` asks those two by number; nothing asks
//!    them by name, and nothing asks `prlimit64` at all.
//!
//! So each test here goes through the syscall and reads the answer off the
//! process that was meant to change -- and, wherever there is a second
//! process, off the one that was not. Two of them need an asymmetry no
//! fixture gets for free, because a caller and a target with the same ids
//! cannot show which of the two was asked: the capability test has a root
//! caller and a target who is not root, and the set-user-ID one has a caller
//! whose own three ids disagree.
//!
//! Fixed pids live in 47_19x and 47_20x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{
    find_process, LinuxProcess, RLimit, NR_OPEN, RLIMIT_CPU, RLIMIT_FSIZE, RLIMIT_NOFILE,
    RLIMIT_RTTIME, RLIM_INFINITY, RLIM_NLIMITS,
};
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// An ordinary user.
const A_USER: u32 = 1000;

/// Somebody else, whose processes are none of `A_USER`'s business.
const ANOTHER_USER: u32 = 2000;

/// What an out-parameter holds before a call, so that "left alone" cannot
/// pass for "written".
///
/// It is also not a limit anybody could install -- a soft limit above the
/// hard one is `EINVAL` -- so a call that read this slot as its NEW limit
/// would answer an errno where it answers zero.
const A_SEED: RLimit = RLimit { cur: 7, max: 0 };

/// A pid nothing in this binary is given, for the lookup that must not find
/// one. Checked rather than assumed in the test that uses it.
const A_PID_OF_NOBODY: usize = 47_209;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A process of a user of its own: all three uids `uid`, so two built with
/// the same number hold every id of each other and two built with different
/// ones hold none.
fn a_process_of(pid: KoID, name: &str, uid: u32) -> Arc<Process> {
    let proc = a_process(pid, name);
    proc.linux().set_resuid(uid, uid, uid).unwrap();
    proc
}

/// The `Syscall` the harness lends, as a `&mut` one: all three calls here
/// take `&mut self`, and every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

fn prlimit(sys: &Syscall<'_>, pid: usize, resource: usize, new: usize, old: usize) -> SysResult {
    as_mut(sys).sys_prlimit64(pid, resource, new.into(), old.into())
}

fn getrlimit(sys: &Syscall<'_>, resource: usize, at: usize) -> SysResult {
    as_mut(sys).sys_getrlimit(resource, at.into())
}

fn setrlimit(sys: &Syscall<'_>, resource: usize, at: usize) -> SysResult {
    as_mut(sys).sys_setrlimit(resource, at.into())
}

/// `prlimit64` by the number userspace sends, which is the only way to ask
/// the dispatch arm anything.
async fn by_number(sys: &Syscall<'_>, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(crate::Sys::PRLIMIT64 as u32, args).await
}

/// The limit a process really has, asked of the process rather than of the
/// syscall under test. Reading takes no privilege: `rlimit_check` runs only
/// over a limit that is being installed.
fn limit_of(proc: &Arc<Process>, resource: usize) -> RLimit {
    proc.linux().rlimit(resource, None, false).unwrap()
}

#[test]
fn prlimit64_answers_the_old_limit_while_it_installs_the_new_one() {
    // The one call that does both halves at once, which is the whole reason
    // `prlimit64` exists and the thing neither `getrlimit` nor `setrlimit`
    // can do. The row has to be read BEFORE it is written: an
    // implementation that stored first and reported afterwards would answer
    // the caller its own argument, and a program that uses the answer to put
    // the limit back -- which is what a library does around a call that
    // needs more descriptors -- would restore the limit it had just set.
    let proc = a_process(47_190, "prlimit-old");
    let was = limit_of(&proc, RLIMIT_NOFILE);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let want = RLimit {
                cur: 512,
                max: was.max,
            };
            let mut out = A_SEED;
            assert_eq!(
                prlimit(
                    sys,
                    0,
                    RLIMIT_NOFILE,
                    &want as *const RLimit as usize,
                    &mut out as *mut RLimit as usize,
                ),
                Ok(0),
                "prlimit64 with both pointers failed"
            );
            assert_eq!(out, was, "what went out was not the limit as it was");

            // And the row really moved. A second call, with no new limit, is
            // the only way to read what the first one left behind.
            let mut now = A_SEED;
            assert_eq!(
                prlimit(sys, 0, RLIMIT_NOFILE, 0, &mut now as *mut RLimit as usize),
                Ok(0),
                "prlimit64 with no new limit failed"
            );
            assert_eq!(now, want, "the new limit was not installed");
        })
    });
}

#[test]
fn prlimit64_answers_from_its_own_number_with_its_four_arguments_in_order() {
    // What userspace has is a number, and the arm
    // `self.sys_prlimit64(a0, a1, a2.into(), a3.into())` had never run. Both
    // of its first two arguments are plain numbers and both of its last two
    // are pointers, so either pair the other way round reads as correct
    // code.
    //
    // Each swap is caught by what it answers: with the pid and the resource
    // exchanged the pid becomes 7, which names no process (`ESRCH`); with
    // the two pointers exchanged the seeded out-slot is read as the new
    // limit, and its soft limit above its hard one is `EINVAL`.
    let proc = a_process(47_191, "prlimit-number");
    let was = limit_of(&proc, RLIMIT_NOFILE);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let want = RLimit {
                cur: 256,
                max: was.max,
            };
            let mut out = A_SEED;
            assert_eq!(
                by_number(
                    sys,
                    [
                        0,
                        RLIMIT_NOFILE,
                        &want as *const RLimit as usize,
                        &mut out as *mut RLimit as usize,
                        0,
                        0,
                    ],
                )
                .await,
                0,
                "prlimit64 by number did not answer"
            );
            assert_eq!(out, was, "the old limit did not come back out of the arm");
            assert_eq!(
                limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                want,
                "the new limit did not reach the row through the arm"
            );

            // The errno comes back through the arm as a negative number, not
            // as a zero with the limit untouched.
            let mut seed = A_SEED;
            assert_eq!(
                by_number(
                    sys,
                    [0, RLIM_NLIMITS, 0, &mut seed as *mut RLimit as usize, 0, 0],
                )
                .await,
                -(LxError::EINVAL as isize),
                "a resource that is not one was not refused through the arm"
            );
            assert_eq!(seed, A_SEED, "a refused call still wrote to the out-slot");
        })
    });
}

#[test]
fn getrlimit_and_setrlimit_are_prlimit64_on_the_caller_and_on_nobody_else() {
    // Each is one line: `sys_prlimit64(0, resource, 0.into(), rlim)` and the
    // same with the two pointers the other way round. The pid in both is a
    // literal zero, and a zero there is "whoever is calling": a number that
    // drifted would send a program's own `ulimit` to another process, which
    // is precisely the failure this area had. A bystander with a limit of
    // its own is here to say so.
    let proc = a_process(47_192, "rlimit-caller");
    let bystander = a_process(47_193, "rlimit-bystander");
    bystander
        .linux()
        .rlimit(RLIMIT_NOFILE, Some(RLimit { cur: 64, max: 64 }), true)
        .unwrap();
    let watched = bystander.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let want = RLimit {
                cur: 512,
                max: 2048,
            };
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &want as *const RLimit as usize),
                Ok(0),
                "setrlimit failed"
            );
            assert_eq!(
                limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                want,
                "setrlimit did not move the caller's own limit"
            );

            // The out-slot is seeded with a limit that could be installed,
            // so that a `getrlimit` passing its pointer as the NEW limit
            // instead of the old one would be caught by the row moving
            // rather than by an errno.
            let mut out = RLimit { cur: 3, max: 3 };
            assert_eq!(
                getrlimit(sys, RLIMIT_NOFILE, &mut out as *mut RLimit as usize),
                Ok(0),
                "getrlimit failed"
            );
            assert_eq!(out, want, "getrlimit did not answer the limit in force");
            assert_eq!(
                limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                want,
                "getrlimit installed what it was supposed to read into"
            );

            assert_eq!(
                limit_of(&watched, RLIMIT_NOFILE),
                RLimit { cur: 64, max: 64 },
                "another process's limit moved"
            );
        })
    });
}

#[test]
fn a_resource_number_that_is_not_one_is_einval_and_the_last_one_answers() {
    // Sixteen is the count, so fifteen is a resource and sixteen is not.
    // Twelve of the sixteen used to answer `ENOSYS` and the seventeenth
    // nothing in particular; `ulimit -a` walks every one of them.
    let proc = a_process(47_194, "rlimit-range");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut out = A_SEED;
            let at = &mut out as *mut RLimit as usize;
            let want = RLimit { cur: 1, max: 1 };
            let giving = &want as *const RLimit as usize;
            for (asked, what) in [
                (prlimit(sys, 0, RLIM_NLIMITS, 0, at), "prlimit64 reading"),
                (
                    prlimit(sys, 0, RLIM_NLIMITS, giving, at),
                    "prlimit64 writing",
                ),
                (getrlimit(sys, RLIM_NLIMITS, at), "getrlimit"),
                (setrlimit(sys, RLIM_NLIMITS, giving), "setrlimit"),
            ] {
                assert_eq!(
                    asked,
                    Err(LxError::EINVAL),
                    "{} answered a resource that is not one",
                    what
                );
            }
            assert_eq!(out, A_SEED, "a refused call wrote to the out-slot anyway");

            assert_eq!(
                prlimit(sys, 0, RLIMIT_RTTIME, 0, at),
                Ok(0),
                "the sixteenth resource was refused"
            );
            assert_eq!(
                out,
                RLimit {
                    cur: RLIM_INFINITY,
                    max: RLIM_INFINITY
                },
                "the sixteenth resource answered somebody else's row"
            );
        })
    });
}

#[test]
fn raising_the_hard_limit_is_eperm_without_the_capability_and_lowering_it_is_not() {
    // `may_raise_hard` is read in `prlimit64` and used three frames down, in
    // `rlimit_check`. What the syscall contributes is the reading, and the
    // rule only bites for a caller who has not got the capability -- so the
    // test has to put the caller's ids down first. Without that step every
    // call in this file is root's, and a root fixture cannot tell a gate
    // that is asked from a gate that is not.
    let proc = a_process(47_195, "rlimit-hard");
    let was = limit_of(&proc, RLIMIT_NOFILE);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // The descriptor ceiling first, which is the one rule the
            // capability is no way past: root is refused it too.
            let too_many = RLimit {
                cur: 0,
                max: NR_OPEN + 1,
            };
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &too_many as *const RLimit as usize),
                Err(LxError::EPERM),
                "a hard limit past NR_OPEN was accepted from root"
            );

            let lp = sys.linux_process();
            lp.set_resuid(A_USER, A_USER, A_USER).unwrap();
            assert!(
                !lp.capable(CAP_SYS_RESOURCE),
                "the caller still holds the capability the rule is about"
            );

            let higher = RLimit {
                cur: was.cur,
                max: was.max + 4096,
            };
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &higher as *const RLimit as usize),
                Err(LxError::EPERM),
                "an unprivileged caller raised its own hard limit"
            );
            assert_eq!(
                limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                was,
                "a refused change moved the limit anyway"
            );

            // Down is always allowed, and it does not come back: that is
            // what makes the hard limit a limit rather than a preference.
            let lower = RLimit { cur: 128, max: 256 };
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &lower as *const RLimit as usize),
                Ok(0),
                "an unprivileged caller could not lower its own hard limit"
            );
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &was as *const RLimit as usize),
                Err(LxError::EPERM),
                "the hard limit it had just lowered was handed back to it"
            );
            assert_eq!(
                limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                lower,
                "the lowered limit did not stay down"
            );
        })
    });
}

#[test]
fn the_capability_asked_about_is_the_callers_and_never_the_targets() {
    // `prlimit64(pid, ...)` reads `capable(CAP_SYS_RESOURCE)` of whoever is
    // CALLING, and then hands it to the target's own `rlimit`. Asking it of
    // the target instead compiles, reads sensibly, and is wrong in both
    // directions -- it would refuse root raising an ordinary user's limit,
    // which is what `prlimit --pid` is for, and let an unprivileged process
    // raise a privileged one's.
    //
    // One root caller and one target who is not root separate the two
    // questions. The same asymmetry catches the two credentials arguments of
    // `may_touch_limits_of` exchanged: with the target's ids in the caller's
    // place, neither its id match nor its capability holds, and the call
    // would be `EPERM`.
    let caller = a_process(47_196, "rlimit-root");
    let target = a_process_of(47_197, "rlimit-theirs", A_USER);
    let was = limit_of(&target, RLIMIT_NOFILE);
    let mine_was = limit_of(&caller, RLIMIT_NOFILE);
    let theirs = target.clone();
    let pid = target.id() as usize;
    on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            assert!(
                sys.linux_process().capable(CAP_SYS_RESOURCE),
                "the caller is not the privileged half of this test"
            );
            assert!(
                !theirs.linux().capable(CAP_SYS_RESOURCE),
                "the target is not the unprivileged half of this test"
            );

            let want = RLimit {
                cur: was.cur + 1024,
                max: was.max + 4096,
            };
            let mut out = A_SEED;
            assert_eq!(
                prlimit(
                    sys,
                    pid,
                    RLIMIT_NOFILE,
                    &want as *const RLimit as usize,
                    &mut out as *mut RLimit as usize,
                ),
                Ok(0),
                "root could not raise another user's hard limit"
            );
            assert_eq!(out, was, "the limit reported was not the target's own");
            assert_eq!(
                limit_of(&theirs, RLIMIT_NOFILE),
                want,
                "the new limit did not reach the target"
            );
            assert_eq!(
                limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                mine_was,
                "the call landed on the caller as well, or instead"
            );
        })
    });
}

#[test]
fn another_users_process_is_eperm_and_one_with_the_same_ids_is_not() {
    // `may_touch_limits_of` through the syscall, from a caller with no
    // capability to fall back on, and the refusal arriving BEFORE anything
    // is changed: the stranger's limit is read afterwards and has to be
    // where it was.
    let caller = a_process_of(47_198, "rlimit-user", A_USER);
    let stranger = a_process_of(47_199, "rlimit-stranger", ANOTHER_USER);
    let same = a_process_of(47_200, "rlimit-sameuser", A_USER);
    let stranger_was = limit_of(&stranger, RLIMIT_NOFILE);
    let (theirs, ours) = (stranger.clone(), same.clone());
    let (stranger_pid, same_pid) = (stranger.id() as usize, same.id() as usize);
    on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let want = RLimit { cur: 128, max: 128 };
            let giving = &want as *const RLimit as usize;
            let mut out = A_SEED;
            let at = &mut out as *mut RLimit as usize;

            assert_eq!(
                prlimit(sys, stranger_pid, RLIMIT_NOFILE, giving, at),
                Err(LxError::EPERM),
                "another user's limits were the caller's to move"
            );
            assert_eq!(
                limit_of(&theirs, RLIMIT_NOFILE),
                stranger_was,
                "the refusal arrived after the limit had already moved"
            );
            assert_eq!(out, A_SEED, "a refused call wrote to the out-slot");
            assert_eq!(
                prlimit(sys, stranger_pid, RLIMIT_NOFILE, 0, at),
                Err(LxError::EPERM),
                "another user's limits could be read without moving them"
            );

            // Every one of the six ids matches here, which is the only thing
            // that has changed.
            assert_eq!(
                prlimit(sys, same_pid, RLIMIT_NOFILE, giving, at),
                Ok(0),
                "a process of the caller's own user was out of reach"
            );
            assert_eq!(
                limit_of(&ours, RLIMIT_NOFILE),
                want,
                "the new limit did not reach a process of the same user"
            );
        })
    });
}

#[test]
fn a_process_part_way_through_a_set_user_id_dance_still_reads_its_own_limits() {
    // `pid == 0 || pid == me` returns the caller without asking anything
    // else, and the shortcut is not a shortcut: the six id comparisons are
    // "this process holds no id I have not got", and a caller whose own
    // three uids disagree does not hold its own. Dropping the arm and
    // letting the lookup answer -- which finds the very same process -- turns
    // `getrlimit` into `EPERM` for every process between a `setresuid` and
    // the next, which is where a setuid binary spends its life.
    let proc = a_process(47_201, "rlimit-dance");
    let pid = proc.id() as usize;
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            lp.set_resuid(A_USER, ANOTHER_USER, ANOTHER_USER).unwrap();
            // The premise, which is what makes the arm load-bearing: this
            // process would not pass its own gate.
            let creds = lp.credentials();
            assert!(
                !LinuxProcess::may_touch_limits_of(&creds, &creds),
                "the caller's ids are not the asymmetric ones this test needs"
            );
            assert!(
                find_process(pid as KoID).is_some(),
                "the lookup the arm is a shortcut past would not have found it"
            );

            let mut by_zero = A_SEED;
            let mut by_name = A_SEED;
            assert_eq!(
                prlimit(
                    sys,
                    0,
                    RLIMIT_NOFILE,
                    0,
                    &mut by_zero as *mut RLimit as usize
                ),
                Ok(0),
                "a process mid-dance could not read its own limits"
            );
            assert_eq!(
                prlimit(
                    sys,
                    pid,
                    RLIMIT_NOFILE,
                    0,
                    &mut by_name as *mut RLimit as usize
                ),
                Ok(0),
                "a process mid-dance could not read its limits by its own pid"
            );
            assert_eq!(
                by_zero, by_name,
                "the two ways of naming the caller answered differently"
            );
        })
    });
}

#[test]
fn a_pid_that_names_no_process_is_esrch_and_a_negative_one_never_reaches_the_table() {
    // The two refusals before the gate, and the one thing both of them must
    // not do: fall back on the caller. That was the old behaviour for every
    // pid, so a `prlimit(gone, RLIMIT_NOFILE, &new)` reported success and
    // the limit it moved was the caller's own.
    let proc = a_process(47_202, "rlimit-esrch");
    let was = limit_of(&proc, RLIMIT_NOFILE);
    assert!(
        find_process(A_PID_OF_NOBODY as KoID).is_none(),
        "the pid this test needs free is in use"
    );
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let want = RLimit { cur: 1, max: 1 };
            let giving = &want as *const RLimit as usize;
            let mut out = A_SEED;
            let at = &mut out as *mut RLimit as usize;

            for (pid, what) in [
                (A_PID_OF_NOBODY, "a pid that names no process"),
                // A `pid_t` is the low 32 bits of the register, sign and
                // all, and a negative one is not looked up at all.
                (usize::MAX, "a negative pid"),
            ] {
                assert_eq!(
                    prlimit(sys, pid, RLIMIT_NOFILE, giving, at),
                    Err(LxError::ESRCH),
                    "{} was answered",
                    what
                );
                assert_eq!(
                    limit_of(sys.zircon_process(), RLIMIT_NOFILE),
                    was,
                    "{} moved the caller's own limit",
                    what
                );
                assert_eq!(out, A_SEED, "{} wrote to the out-slot", what);
            }
        })
    });
}

#[test]
fn the_soft_descriptor_limit_a_setrlimit_installs_refuses_the_next_descriptor() {
    // `RLIMIT_NOFILE`'s soft limit is the one limit of the sixteen this
    // kernel enforces, and the only one whose arrival can be read without
    // asking `getrlimit` -- which is the point: a `setrlimit` that stored
    // its argument somewhere nothing consults would pass every other test in
    // this file. `ulimit -n` is the command, and a shell that lowers it
    // expects the next `open` to fail.
    let proc = a_process(47_203, "rlimit-nofile");
    // The hard limit stays exactly where it was: `NR_OPEN` is the ceiling
    // over a descriptor hard limit, so `RLIM_INFINITY` in that field is
    // `EPERM` even for root, as the test two above says.
    let was = limit_of(&proc, RLIMIT_NOFILE);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let first = sys.sys_eventfd2(0, 0).expect("the first eventfd failed") as u64;

            // Room for everything up to `first` and nothing above it.
            let tight = RLimit {
                cur: first + 1,
                max: was.max,
            };
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &tight as *const RLimit as usize),
                Ok(0),
                "setrlimit to the descriptor in hand failed"
            );
            assert_eq!(
                sys.sys_eventfd2(0, 0),
                Err(LxError::EMFILE),
                "a descriptor past the soft limit was handed out"
            );

            // And room for one more, which says the refusal was the limit
            // and not the table.
            let roomier = RLimit {
                cur: first + 2,
                max: was.max,
            };
            assert_eq!(
                setrlimit(sys, RLIMIT_NOFILE, &roomier as *const RLimit as usize),
                Ok(0),
                "setrlimit raising the soft limit failed"
            );
            assert_eq!(
                sys.sys_eventfd2(0, 0),
                Ok(first as usize + 1),
                "the raised soft limit did not reach the descriptor table"
            );
        })
    });
}

#[test]
fn a_finite_cpu_limit_arms_the_watch_on_the_process_it_was_set_for() {
    // The one side effect in `prlimit64` beyond the row itself. A CPU limit
    // is not a timer the wheel can fire -- it counts the process's own CPU
    // clock, which only the process moves -- so what a `setrlimit(RLIMIT_CPU)`
    // has to do is arm the look that every return from user mode takes, and
    // arm it on the process whose limit it is. The three negatives are here
    // because an unconditional arming would pass a test that only set a CPU
    // limit and looked.
    let caller = a_process(47_204, "rlimit-cpu");
    let target = a_process_of(47_205, "rlimit-cpu-theirs", A_USER);
    let theirs = target.clone();
    let pid = target.id() as usize;
    on_a_thread(&caller, move |sys| {
        Box::pin(async move {
            let lp = sys.linux_process();
            assert!(
                !lp.itimers().cpu_armed(),
                "the watch was armed before anything was set"
            );

            let finite = RLimit {
                cur: 60,
                max: RLIM_INFINITY,
            };
            let unlimited = RLimit {
                cur: RLIM_INFINITY,
                max: RLIM_INFINITY,
            };

            // A finite limit on another resource is not a CPU limit.
            assert_eq!(
                setrlimit(sys, RLIMIT_FSIZE, &finite as *const RLimit as usize),
                Ok(0),
                "setrlimit on the file-size limit failed"
            );
            assert!(
                !lp.itimers().cpu_armed(),
                "a limit on another resource armed the CPU watch"
            );

            // And a CPU limit of infinity is not a limit.
            assert_eq!(
                setrlimit(sys, RLIMIT_CPU, &unlimited as *const RLimit as usize),
                Ok(0),
                "setrlimit of an unlimited CPU limit failed"
            );
            assert!(
                !lp.itimers().cpu_armed(),
                "an unlimited CPU limit armed the watch"
            );

            assert_eq!(
                setrlimit(sys, RLIMIT_CPU, &finite as *const RLimit as usize),
                Ok(0),
                "setrlimit of a finite CPU limit failed"
            );
            assert!(
                lp.itimers().cpu_armed(),
                "a finite CPU limit did not arm the watch"
            );

            // The target's, set from here, arms the target's watch.
            assert!(
                !theirs.linux().itimers().cpu_armed(),
                "the target's watch was armed before anything was set"
            );
            assert_eq!(
                prlimit(sys, pid, RLIMIT_CPU, &finite as *const RLimit as usize, 0),
                Ok(0),
                "a CPU limit on another process failed"
            );
            assert!(
                theirs.linux().itimers().cpu_armed(),
                "the watch of the process the limit was set for is not armed"
            );
        })
    });
}
