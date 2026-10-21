//! `sched_setattr` and `sched_getattr`, asked through the syscall.
//!
//! The pure halves have three test modules of their own in `task.rs`
//! (`sched_attr_layout_tests`, `sched_attr_flag_tests`,
//! `extensible_struct_tests`) and `sched_validate_tests` besides. What
//! only this can see is the wiring, and this pair has three pieces of it
//! that a reader would not guess.
//!
//! The first is that **the same question gets two different errnos**. An
//! out-of-range size is `E2BIG` for `sched_setattr` and `EINVAL` for
//! `sched_getattr`, one function away. It is not the same question:
//! `setattr` reads the size out of the struct, where it describes what the
//! caller built, and `getattr` takes it as an argument, where it describes
//! the buffer. A zero is version 0 for the first and nothing at all for
//! the second.
//!
//! The second is the **extensible struct**: a size past the 56 bytes this
//! kernel knows is accepted only if everything past them is zero, and
//! `E2BIG` otherwise. That is how a kernel refuses a struct a newer
//! userspace filled in, instead of silently ignoring the half it does not
//! know.
//!
//! The third is that **`SCHED_FLAG_RESET_ON_FORK` lives in `sched_flags`
//! here**, while the very same bit reaches `sched_setscheduler` inside the
//! policy word. Two calls, one flag, two different places to carry it.
//!
//! Fixed pids live in 47_15x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `SCHED_DEADLINE`, which this scheduler has no runqueue for.
const A_DEADLINE_POLICY: u32 = 6;

/// A real-time priority inside `1..=99`, and another one.
const A_PRIORITY: u32 = 50;
const ANOTHER_PRIORITY: u32 = 60;

/// A nice value, and another one.
const A_NICE: i32 = 7;
const ANOTHER_NICE: i32 = 5;

/// What a buffer is seeded with, so that "wrote nothing" cannot pass for
/// "wrote zero".
const A_SEED: u8 = 0xaa;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A `struct sched_attr` with room past the version this kernel knows,
/// which is what an extensible struct needs to be asked about at all.
/// `#[repr(C)]` so the tail really does start at byte 56.
#[repr(C)]
struct Wide {
    attr: SchedAttr,
    tail: [u8; 16],
}

impl Wide {
    /// A version-0 request: `size` 0 means "the 48 bytes", the quirk the
    /// first `sched_setattr` users shipped with.
    fn asking(policy: u32, flags: u64, priority: u32, nice: i32) -> Self {
        Wide {
            attr: SchedAttr {
                size: 0,
                sched_policy: policy,
                sched_flags: flags,
                sched_nice: nice,
                sched_priority: priority,
                sched_runtime: 0,
                sched_deadline: 0,
                sched_period: 0,
                sched_util_min: 0,
                sched_util_max: 0,
            },
            tail: [0; 16],
        }
    }

    /// A buffer for an answer, with every byte recognizable.
    fn seeded() -> Self {
        Wide {
            attr: SchedAttr {
                size: u32::from_le_bytes([A_SEED; 4]),
                sched_policy: u32::from_le_bytes([A_SEED; 4]),
                sched_flags: u64::from_le_bytes([A_SEED; 8]),
                sched_nice: i32::from_le_bytes([A_SEED; 4]),
                sched_priority: u32::from_le_bytes([A_SEED; 4]),
                sched_runtime: u64::from_le_bytes([A_SEED; 8]),
                sched_deadline: u64::from_le_bytes([A_SEED; 8]),
                sched_period: u64::from_le_bytes([A_SEED; 8]),
                sched_util_min: u32::from_le_bytes([A_SEED; 4]),
                sched_util_max: u32::from_le_bytes([A_SEED; 4]),
            },
            tail: [A_SEED; 16],
        }
    }

    fn at(&mut self) -> usize {
        &mut self.attr as *mut SchedAttr as usize
    }

    fn asked(&mut self) -> UserInPtr<SchedAttr> {
        UserInPtr::from(self.at())
    }

    fn answered(&mut self) -> UserOutPtr<SchedAttr> {
        UserOutPtr::from(self.at())
    }
}

/// What the thread's attributes read as, through `sched_getattr`.
fn attributes_of(sys: &Syscall<'_>) -> (u32, u64, i32, u32) {
    let mut got = Wide::seeded();
    assert_eq!(
        sys.sys_sched_getattr(0, got.answered(), SCHED_ATTR_SIZE_VER0, 0),
        Ok(0)
    );
    (
        got.attr.sched_policy,
        got.attr.sched_flags,
        got.attr.sched_nice,
        got.attr.sched_priority,
    )
}

#[test]
fn an_out_of_range_size_is_e2big_asking_and_einval_answering() {
    // The asymmetry is the whole test: `setattr` reads the size out of the
    // struct, so a bad one means "I cannot make sense of what you built"
    // -- E2BIG -- and `getattr` takes it as the size of a buffer, so a bad
    // one is just a bad argument. A caller that told the two apart by
    // errno, which is all it has, would be told the wrong thing.
    let proc = a_process(47_150, "attr-size");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut short = Wide::asking(SCHED_NORMAL as u32, 0, 0, 0);
            short.attr.size = SCHED_ATTR_SIZE_VER0 as u32 - 1;
            assert_eq!(
                sys.sys_sched_setattr(0, short.asked(), 0),
                Err(LxError::E2BIG),
                "a struct smaller than version 0 was not E2BIG"
            );
            let mut huge = Wide::asking(SCHED_NORMAL as u32, 0, 0, 0);
            huge.attr.size = PAGE_SIZE as u32 + 1;
            assert_eq!(
                sys.sys_sched_setattr(0, huge.asked(), 0),
                Err(LxError::E2BIG)
            );

            // And a zero is version 0, not an error: the first users of
            // this call shipped before the field existed.
            let mut quirk = Wide::asking(SCHED_NORMAL as u32, 0, 0, A_NICE);
            assert_eq!(sys.sys_sched_setattr(0, quirk.asked(), 0), Ok(0));
            assert_eq!(attributes_of(sys).2, A_NICE, "a size of zero was ignored");

            let mut buffer = Wide::seeded();
            for bad in [0, SCHED_ATTR_SIZE_VER0 - 1, PAGE_SIZE + 1] {
                assert_eq!(
                    sys.sys_sched_getattr(0, buffer.answered(), bad, 0),
                    Err(LxError::EINVAL),
                    "a buffer size out of range was not EINVAL"
                );
            }
            assert_eq!(
                buffer.tail, [A_SEED; 16],
                "a refused sched_getattr wrote into the caller's buffer"
            );
        })
    });
}

#[test]
fn everything_past_the_struct_this_kernel_knows_has_to_be_zero() {
    // The extensible-struct rule, and the only way an old kernel can
    // refuse a request a new userspace built: a field it does not know is
    // zero (so nothing was asked for) or it is not (so something was, and
    // the kernel would be ignoring it). E2BIG is the refusal.
    let proc = a_process(47_151, "attr-tail");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut empty = Wide::asking(SCHED_NORMAL as u32, 0, 0, A_NICE);
            empty.attr.size = (SCHED_ATTR_SIZE_VER1 + 16) as u32;
            assert_eq!(
                sys.sys_sched_setattr(0, empty.asked(), 0),
                Ok(0),
                "a struct with an empty tail was refused"
            );
            assert_eq!(attributes_of(sys).2, A_NICE);

            // One byte, anywhere in the tail, and only that byte.
            for at in [0, 7, 15] {
                let mut filled = Wide::asking(SCHED_NORMAL as u32, 0, 0, ANOTHER_NICE);
                filled.attr.size = (SCHED_ATTR_SIZE_VER1 + 16) as u32;
                filled.tail[at] = 1;
                assert_eq!(
                    sys.sys_sched_setattr(0, filled.asked(), 0),
                    Err(LxError::E2BIG),
                    "a tail with something in it was accepted"
                );
            }
            assert_eq!(
                attributes_of(sys).2,
                A_NICE,
                "a refused sched_setattr moved the thread anyway"
            );
        })
    });
}

#[test]
fn a_flags_argument_and_a_null_struct_are_einval_in_both_calls() {
    // The `flags` argument of these two has no bits yet, so the only
    // correct answer to a non-zero one is EINVAL: accepting it would
    // promise a caller that whatever it asked for was done.
    let proc = a_process(47_152, "attr-args");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut asking = Wide::asking(SCHED_NORMAL as u32, 0, 0, 0);
            assert_eq!(
                sys.sys_sched_setattr(0, asking.asked(), 1),
                Err(LxError::EINVAL),
                "sched_setattr took a flag it does not have"
            );
            let mut buffer = Wide::seeded();
            assert_eq!(
                sys.sys_sched_getattr(0, buffer.answered(), SCHED_ATTR_SIZE_VER0, 1),
                Err(LxError::EINVAL),
                "sched_getattr took a flag it does not have"
            );

            assert_eq!(
                sys.sys_sched_setattr(0, UserInPtr::from(0usize), 0),
                Err(LxError::EINVAL),
                "sched_setattr read a null struct"
            );
            assert_eq!(
                sys.sys_sched_getattr(0, UserOutPtr::from(0usize), SCHED_ATTR_SIZE_VER0, 0),
                Err(LxError::EINVAL),
                "sched_getattr wrote through a null struct"
            );
        })
    });
}

#[test]
fn the_deadline_policy_is_refused_and_the_thread_stays_where_it_was() {
    // There is no deadline runqueue here, so the honest answer is EINVAL.
    // Accepting it and running the thread as anything else is the failure
    // mode that matters: a program that asked for a deadline class and was
    // told yes has no reason to check again.
    let proc = a_process(47_153, "attr-deadline");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut fair = Wide::asking(SCHED_NORMAL as u32, 0, 0, A_NICE);
            assert_eq!(sys.sys_sched_setattr(0, fair.asked(), 0), Ok(0));

            let mut deadline = Wide::asking(A_DEADLINE_POLICY, 0, 0, ANOTHER_NICE);
            deadline.attr.sched_runtime = 1_000_000;
            deadline.attr.sched_deadline = 2_000_000;
            deadline.attr.sched_period = 2_000_000;
            assert_eq!(
                sys.sys_sched_setattr(0, deadline.asked(), 0),
                Err(LxError::EINVAL),
                "the deadline policy was accepted"
            );
            assert_eq!(
                attributes_of(sys),
                (SCHED_NORMAL as u32, 0, A_NICE, 0),
                "a refused deadline request moved the thread anyway"
            );
        })
    });
}

#[test]
fn the_reset_on_fork_bit_travels_in_the_flags_here_and_in_the_policy_there() {
    // One bit, two calls, two different words. `sched_setattr` carries it
    // in `sched_flags`; `sched_setscheduler` carries it inside the policy
    // argument, and `sched_getscheduler` ORs it back in. A kernel that
    // read it from the policy word here would see `SCHED_NORMAL` and set
    // nothing, and one that reported it in the policy word of `sched_attr`
    // would answer a policy number no `SCHED_*` constant has.
    let proc = a_process(47_154, "attr-reset-on-fork");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut asking = Wide::asking(
                SCHED_NORMAL as u32,
                SCHED_FLAG_RESET_ON_FORK,
                0,
                ANOTHER_NICE,
            );
            assert_eq!(sys.sys_sched_setattr(0, asking.asked(), 0), Ok(0));
            assert_eq!(
                attributes_of(sys),
                (
                    SCHED_NORMAL as u32,
                    SCHED_FLAG_RESET_ON_FORK,
                    ANOTHER_NICE,
                    0
                ),
                "the reset-on-fork bit did not come back in the flags"
            );

            // The same thread, asked the other way: there the bit is part
            // of the policy number.
            assert_eq!(
                sys.sys_sched_getscheduler(0),
                Ok(SCHED_NORMAL as usize | SCHED_RESET_ON_FORK),
                "sched_getscheduler did not carry the bit in its policy word"
            );

            // And it goes down again from here, which is what tells a
            // stored bit from a constant.
            let mut without = Wide::asking(SCHED_NORMAL as u32, 0, 0, ANOTHER_NICE);
            assert_eq!(sys.sys_sched_setattr(0, without.asked(), 0), Ok(0));
            assert_eq!(attributes_of(sys).1, 0, "the bit could not be cleared");
            assert_eq!(sys.sys_sched_getscheduler(0), Ok(SCHED_NORMAL as usize));
        })
    });
}

#[test]
fn keep_params_keeps_the_numbers_and_lets_the_class_change_through() {
    // `SCHED_FLAG_KEEP_PARAMS` keeps the PARAMETERS, and that is all it
    // keeps. Skipping the store made
    // `sched_setattr(tid, {policy = SCHED_FIFO, flags = KEEP_PARAMS})` --
    // "move this thread to real time and leave its numbers alone", which
    // is what a program with a priority it has already tuned asks for --
    // a silent no-op that returned 0.
    let proc = a_process(47_155, "attr-keep-params");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut round_robin = Wide::asking(SCHED_RR as u32, 0, A_PRIORITY, A_NICE);
            assert_eq!(sys.sys_sched_setattr(0, round_robin.asked(), 0), Ok(0));
            assert_eq!(attributes_of(sys), (SCHED_RR as u32, 0, A_NICE, A_PRIORITY));

            // The numbers in the struct are the ones that must be
            // IGNORED, so they are both different from the thread's.
            let mut class_only = Wide::asking(
                SCHED_FIFO as u32,
                SCHED_FLAG_KEEP_PARAMS,
                ANOTHER_PRIORITY,
                ANOTHER_NICE,
            );
            assert_eq!(sys.sys_sched_setattr(0, class_only.asked(), 0), Ok(0));
            assert_eq!(
                attributes_of(sys),
                (SCHED_FIFO as u32, 0, A_NICE, A_PRIORITY),
                "keep_params either dropped the class change or took the numbers"
            );
        })
    });
}

#[test]
fn keep_policy_takes_the_threads_own_class_and_its_own_reset_on_fork() {
    // The mirror image, `SETPARAM_POLICY`: with `SCHED_FLAG_KEEP_POLICY`
    // the thread's own policy AND its own reset-on-fork bit stand in for
    // the struct's, so a `sched_setattr` that only wants to move a
    // priority cannot take the class down with it. The struct below asks
    // for the fair class with no flags, and neither may happen.
    let proc = a_process(47_156, "attr-keep-policy");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut round_robin = Wide::asking(
                SCHED_RR as u32,
                SCHED_FLAG_RESET_ON_FORK,
                A_PRIORITY,
                A_NICE,
            );
            assert_eq!(sys.sys_sched_setattr(0, round_robin.asked(), 0), Ok(0));

            let mut params_only = Wide::asking(
                SCHED_NORMAL as u32,
                SCHED_FLAG_KEEP_POLICY,
                ANOTHER_PRIORITY,
                ANOTHER_NICE,
            );
            assert_eq!(sys.sys_sched_setattr(0, params_only.asked(), 0), Ok(0));
            assert_eq!(
                attributes_of(sys),
                (
                    SCHED_RR as u32,
                    SCHED_FLAG_RESET_ON_FORK,
                    ANOTHER_NICE,
                    ANOTHER_PRIORITY
                ),
                "keep_policy took the class or the bit from the struct"
            );
        })
    });
}

#[test]
fn getattr_answers_with_its_own_version_and_zeroes_what_it_has_nothing_for() {
    // What a caller reads out of its buffer, field by field: the size is
    // the kernel's own version and not the one the caller asked with, the
    // three deadline fields are zero because this scheduler has no
    // deadline class, the two util clamps are zero because it has no
    // clamps either, and the part of a bigger buffer the kernel has
    // nothing to put in comes back zeroed and not untouched, which is what
    // `copy_struct_to_user` does: a caller reads the whole `size` it was
    // answered with, so the bytes beyond this version have to mean
    // something, and zero is "nothing here".
    let proc = a_process(47_157, "attr-answer");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut asking = Wide::asking(SCHED_RR as u32, 0, A_PRIORITY, A_NICE);
            assert_eq!(sys.sys_sched_setattr(0, asking.asked(), 0), Ok(0));

            let mut got = Wide::seeded();
            assert_eq!(
                sys.sys_sched_getattr(0, got.answered(), SCHED_ATTR_SIZE_VER1 + 16, 0),
                Ok(0)
            );
            assert_eq!(
                got.attr.size, SCHED_ATTR_SIZE_VER1 as u32,
                "sched_getattr did not report the version it answered with"
            );
            assert_eq!(
                (
                    got.attr.sched_runtime,
                    got.attr.sched_deadline,
                    got.attr.sched_period
                ),
                (0, 0, 0),
                "a scheduler with no deadline class reported deadline numbers"
            );
            assert_eq!(
                (got.attr.sched_util_min, got.attr.sched_util_max),
                (0, 0),
                "a scheduler with no util clamps reported clamp numbers"
            );
            assert_eq!(
                got.tail, [0; 16],
                "sched_getattr left the rest of a bigger buffer as it was"
            );
        })
    });
}
