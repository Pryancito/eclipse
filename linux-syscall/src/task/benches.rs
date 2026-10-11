//! The process syscalls: `clone`, `clone3`, `wait4`, `waitid`,
//! `setpgid`, the `sched_*attr` pair, `prctl` and `nanosleep`'s
//! remainder.
//!
//! None of these can be benched end to end on the host -- a `clone` needs
//! a scheduler and `wait4` needs a child to reap -- so what is measured
//! is the part that is pure decision: the flag words, the pid arguments,
//! the extensible-struct sizes, and the two bits of real work in the
//! family (`comm_from_user_bytes`, which walks a name byte by byte, and
//! the `rusage` a reaped child hands back).
//!
//! That is not a consolation prize. `clone3_to_clone` is nine refusals
//! over a 64-byte struct and it runs on every `posix_spawn`; if it cost
//! anything, `fork` would show it. The rows say it does not.
//!
//! Every row black-boxes its inputs. Two cycles (~0.6 ns) is `b.iter`'s
//! own loop, so a row there with an opaque input is a real compare and
//! branch -- with a *constant* input it is a call that was folded away.

use super::*;
use test::{black_box, Bencher};

/// glibc's `arch_fork` flag word: `CLONE_CHILD_SETTID |
/// CLONE_CHILD_CLEARTID | SIGCHLD`, which is what every `fork(3)` sends.
const GLIBC_FORK: usize = 0x0120_0011;

/// `clone`'s flag word, as `fork(3)` sends it.
#[bench]
fn clone_flags_of_a_fork(b: &mut Bencher) {
    b.iter(|| black_box(clone_flags_from_word(black_box(GLIBC_FORK & !0xff))));
}

/// A flag word with a bit this kernel does not name, which must be
/// `EINVAL` so glibc falls back instead of being told it got something
/// it did not.
#[bench]
fn clone_flags_unknown_bit(b: &mut Bencher) {
    b.iter(|| black_box(clone_flags_from_word(black_box(1 << 40))));
}

/// `clone3`'s whole `struct clone_args`, decoded into the legacy
/// arguments: nine refusals over eight words, once per `posix_spawn`.
#[bench]
fn clone3_decode_a_fork(b: &mut Bencher) {
    let words: [u64; 8] = [
        (GLIBC_FORK & !0xff) as u64,
        0,
        0x7fff_0000_1000,
        0,
        17,
        0,
        0,
        0,
    ];
    b.iter(|| black_box(clone3_to_clone(black_box(words))));
}

/// `clone3` with a stack: the arm that adds `stack + stack_size`, which
/// is what a threading runtime sends.
#[bench]
fn clone3_decode_a_thread(b: &mut Bencher) {
    let words: [u64; 8] = [
        0x0003_d0f00,
        0,
        0x7fff_0000_1000,
        0,
        0,
        0x7fff_0010_0000,
        8 * 1024 * 1024,
        0x7fff_0000_2000,
    ];
    b.iter(|| black_box(clone3_to_clone(black_box(words))));
}

/// How many bytes of `struct clone_args` `clone3` will read.
#[bench]
fn clone3_struct_size(b: &mut Bencher) {
    b.iter(|| black_box(clone3_size(black_box(CLONE_ARGS_SIZE_VER0))));
}

/// The same question for `sched_setattr` and `sched_getattr`, which
/// answer it with different errnos on purpose.
#[bench]
fn sched_attr_struct_sizes(b: &mut Bencher) {
    b.iter(|| {
        black_box((
            sched_setattr_size(black_box(0)),
            sched_getattr_size(black_box(SCHED_ATTR_SIZE_VER0)),
        ))
    });
}

/// What `sched_setattr` was actually asked to change, from its flags and
/// its policy.
#[bench]
fn sched_setattr_decision(b: &mut Bencher) {
    b.iter(|| black_box(sched_setattr_plan(black_box(0), black_box(0))));
}

/// `wait4`'s option word.
#[bench]
fn wait4_option_word(b: &mut Bencher) {
    b.iter(|| black_box(wait4_options(black_box(1))));
}

/// `waitid`'s, which has one check more: a word naming nothing to wait
/// for is refused rather than blocked on.
#[bench]
fn waitid_option_word(b: &mut Bencher) {
    b.iter(|| black_box(waitid_options(black_box(4))));
}

/// The `rusage` a reaped child hands back, which `time(1)` prints.
#[bench]
fn child_rusage_build(b: &mut Bencher) {
    let cpu = linux_object::process::ChildCpu {
        utime_ns: 1_234_567_890,
        stime_ns: 98_765_432,
    };
    b.iter(|| black_box(child_rusage(black_box(cpu))));
}

/// `setpgid(0, 0)`: both arguments resolved, which is what a shell sends
/// per job.
#[bench]
fn setpgid_arguments(b: &mut Bencher) {
    b.iter(|| black_box(setpgid_args(black_box(4242), black_box(0), black_box(0))));
}

/// The `pid` of `getpgid`/`getsid`, where a negative one is `ESRCH` and
/// zero is the caller.
#[bench]
fn pid_argument_resolve(b: &mut Bencher) {
    b.iter(|| black_box(resolve_pid_arg(black_box(4242), black_box(0))));
}

/// `prctl(PR_SET_NAME, "name")`: fifteen ASCII bytes, truncated at the
/// NUL and at `TASK_COMM_LEN`. This one walks the bytes, so it is the
/// only row here that is not a handful of instructions.
#[bench]
fn comm_from_ascii_bytes(b: &mut Bencher) {
    let bytes = b"firefox\0\0\0\0\0\0\0\0";
    b.iter(|| black_box(comm_from_user_bytes(black_box(bytes))));
}

/// The same with bytes that are not UTF-8, which go through the
/// chunk-by-chunk path and come out as `?`. A process name arrives from
/// userspace, so this is not a corner case, it is an untrusted string.
#[bench]
fn comm_from_invalid_utf8_bytes(b: &mut Bencher) {
    let bytes = &[
        0xffu8, 0xfe, b'a', 0xc3, b'b', 0x80, b'c', 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    b.iter(|| black_box(comm_from_user_bytes(black_box(bytes))));
}

/// The name of the running program, taken off the end of its path.
#[bench]
fn comm_from_exec_path(b: &mut Bencher) {
    b.iter(|| black_box(comm_from_path(black_box("/usr/lib/firefox/firefox"))));
}

/// `prctl(PR_SET_KEEPCAPS)`, which takes 0 or 1 and nothing else, and
/// `PR_SET_TIMERSLACK`, which takes anything.
#[bench]
fn prctl_boolean_and_slack_args(b: &mut Bencher) {
    b.iter(|| {
        black_box((
            keepcaps_arg(black_box(1)),
            timerslack_arg(black_box(50_000)),
        ))
    });
}

/// `prctl(PR_SET_PDEATHSIG)`, whose argument used to be read as a byte.
#[bench]
fn prctl_pdeathsig_arg(b: &mut Bencher) {
    b.iter(|| black_box(pdeathsig_from_arg(black_box(15))));
}

/// What `nanosleep` writes into `rem` when a signal cut it short.
#[bench]
fn nanosleep_remainder(b: &mut Bencher) {
    let deadline = core::time::Duration::from_nanos(5_000_000_000);
    let now = core::time::Duration::from_nanos(1_234_567_890);
    b.iter(|| black_box(nanosleep_remaining(black_box(deadline), black_box(now))));
}
