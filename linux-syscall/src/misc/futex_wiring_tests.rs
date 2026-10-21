//! The futex through the syscall: the park a thread goes into and the wake
//! that takes it out again.
//!
//! `sys_futex` is the largest function in this file and the one every
//! threaded program in the system rests on -- a contended
//! `pthread_mutex_lock`, every `pthread_cond_wait`, musl's `__synccall`, the
//! `xshmfence` handshake of every GL client -- and until now **no test named
//! it**. Its pure halves are covered next door: `futex_op_tests` owns the
//! `op`/`val3` verdict and `futex_deadline_tests` owns the arithmetic that
//! turns a timeout into a deadline. Neither can see whether the syscall
//! ASKS those questions, in what order, or what it does with the answers.
//!
//! Four pieces of wiring are what this file is for.
//!
//! The first is **order**. `op` is judged before `uaddr` is looked at, so an
//! operation this kernel does not serve is `ENOSYS` even when the word is a
//! null pointer; swap the two and a program probing for `FUTEX_WAIT_BITSET`
//! support learns `EFAULT` instead and glibc's fallback never fires. The
//! word is then checked on EVERY call rather than once when the `Futex` is
//! created, which is the fix for a kernel page fault at `sys_futex`
//! reproducible to the byte from PulseAudio. And a `FUTEX_WAIT` whose word
//! has already changed answers `EAGAIN` before the `timespec` is read at
//! all, so a hot contended mutex pays for neither.
//!
//! The second is **which clock an absolute deadline is on**. A plain
//! `FUTEX_WAIT` timeout is RELATIVE; a `FUTEX_WAIT_BITSET` one is a point on
//! the monotonic clock, or on the wall clock when `FUTEX_CLOCK_REALTIME`
//! says so. The three readings of one `timespec` differ by billions:
//! monotonic time here is the host's uptime and the wall clock is seconds
//! since 1970, so a deadline read on the wrong one of them either fires at
//! once or parks for a decade. No pure test can tell them apart, because
//! the function that does the arithmetic is handed the clock it is to use.
//!
//! The third is the **pair**: a thread parked with no deadline at all comes
//! back only because another thread's `FUTEX_WAKE` reached it, and the count
//! it asked for is how many it took. That needs two threads of one process,
//! so a wait and the wake that ends it are made from two host threads here.
//!
//! The fourth is the **argument order**, which a requeue is the only call to
//! make visible: `val` is how many to wake, `val2` how many to move and
//! `uaddr2` where to move them. All three are the same width and two of
//! them are counts, so nothing but a move that really happens says they
//! arrived in the right places.
//!
//! One trap measured while writing this: **every word gets its own
//! address**. `LinuxProcess::get_futex` keeps the queue it made for an
//! address for the life of the process, so a word reused across tests
//! carries the previous test's waiters -- real or tombstoned -- into this
//! one. Each word below is leaked for the life of the binary and used once.
//!
//! Fixed pids live in 47_26x.

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicI32, Ordering};
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;
use zircon_object::vm::{MMUFlags, VmObject, PAGE_SIZE};

/// `FUTEX_BITSET_MATCH_ANY`: the mask musl and glibc are the only callers to
/// send, and the only one this kernel honours. Zero is the empty set, which
/// is `EINVAL`.
const MATCH_ANY: u32 = 0xffff_ffff;

/// A command number above every one `<linux/futex.h>` defines.
const AN_OPERATION_OF_NOBODY: u32 = 0x20;

/// How long a wait that is meant to time out asks for.
const A_TIMEOUT: Duration = Duration::from_millis(1_500);

/// The floor a wait that really waited has to clear, and the ceiling one
/// that was already past has to stay under. One value for both, so the two
/// readings of the same `timespec` cannot both pass.
const HALFWAY: Duration = Duration::from_millis(1_000);

/// How long a wake retries for while the waiters park: 3 seconds, well
/// inside the 20 the harness gives the whole call.
const TRIES: usize = 300;

/// A gap between tries, long enough that the retries are not a spin.
const A_MOMENT: Duration = Duration::from_millis(10);

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A futex word of this binary's own, holding `value`.
///
/// Leaked on purpose. `LinuxProcess::get_futex` builds an `&'static
/// AtomicI32` out of the address and the per-process table keeps the queue
/// under that address for good, so a word that goes out of scope leaves the
/// kernel holding a pointer into freed memory, and a word used twice brings
/// the first test's queue into the second.
fn a_futex_word(value: i32) -> &'static AtomicI32 {
    Box::leak(Box::new(AtomicI32::new(value)))
}

/// Where a word is, as userspace names it.
fn addr_of(word: &AtomicI32) -> usize {
    word as *const AtomicI32 as usize
}

/// The `Syscall` the harness lends, as a `&mut` one, for the calls that want
/// one. Every field is `Copy`.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

async fn futex(
    sys: &Syscall<'_>,
    uaddr: usize,
    op: u32,
    val: u32,
    val2: usize,
    uaddr2: usize,
    val3: u32,
) -> SysResult {
    sys.sys_futex(uaddr, op, val, val2, uaddr2, val3).await
}

/// `sys_futex` reached through the syscall table instead of by name.
async fn by_number(sys: &Syscall<'_>, args: [usize; 6]) -> isize {
    let mut own = as_mut(sys);
    own.syscall(crate::Sys::FUTEX as u32, args).await
}

/// A thread of `proc` parked on `addr` with `op`, on its own host thread.
///
/// `on_a_thread` blocks until the call it makes comes back, so a wait and
/// the wake that ends it cannot both be made from the test's own thread.
/// Both threads belong to the SAME process deliberately: the per-process
/// futex table keys by address within one process, so a waiter and a waker
/// in two processes would queue on two different `Futex`es and the wake
/// would reach nobody.
fn a_waiter(
    proc: &Arc<Process>,
    addr: usize,
    op: u32,
    timeout: Option<Duration>,
) -> std::thread::JoinHandle<SysResult> {
    let proc = proc.clone();
    std::thread::spawn(move || {
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                // The `timespec` lives in the future, which is pinned, so
                // the address handed to the kernel stays put for the whole
                // call. A null one is "block indefinitely".
                let deadline = timeout.map(TimeSpec::from_duration);
                let at = deadline
                    .as_ref()
                    .map_or(0, |t| t as *const TimeSpec as usize);
                futex(sys, addr, op, 0, at, 0, MATCH_ANY).await
            })
        })
    })
}

/// Ask for `count` wakes on `addr` until one of them finds somebody, and
/// answer with that first non-zero count. `None` when the budget ran out
/// without anybody being there.
async fn a_wake_that_finds_somebody(
    sys: &Syscall<'_>,
    addr: usize,
    op: u32,
    count: u32,
) -> Option<usize> {
    for _ in 0..TRIES {
        match futex(sys, addr, op, count, 0, 0, 0).await {
            Ok(0) => async_std::task::sleep(A_MOMENT).await,
            other => return other.ok(),
        }
    }
    None
}

/// Give the waiters time to park, asking for ZERO wakes all the while.
///
/// A count of zero wakes nobody however many waiters are queued, so this
/// both settles the test and asserts something: a kernel that passed a
/// constant to `Futex::wake` instead of the count it was given answers 1
/// here long before the test gets to the call it is about.
async fn nobody_is_woken_while_the_count_is_zero(sys: &Syscall<'_>, addr: usize) {
    for _ in 0..60 {
        assert_eq!(
            futex(sys, addr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
            Ok(0),
            "a wake asking for nobody woke somebody"
        );
        async_std::task::sleep(A_MOMENT).await;
    }
}

/// `op` and `val3` are the whole of what Linux decides before it looks at
/// the address, and this kernel decides them in that order too: every
/// refusal below is answered with a null word, which would be `EFAULT` if
/// the address were looked at first.
///
/// What it costs to get this backwards is a program that cannot probe. The
/// requeue-PI pair is exactly that case: glibc's condvars ask for it, read
/// `ENOSYS` and fall back to a plain requeue. `EFAULT` is not a refusal it
/// knows how to retry.
#[test]
fn an_operation_this_kernel_does_not_serve_is_judged_before_the_word_is_looked_at() {
    let proc = a_process(47_260, "futex-op-order");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            for op in [
                FUTEX_WAIT_REQUEUE_PI,
                FUTEX_CMP_REQUEUE_PI,
                AN_OPERATION_OF_NOBODY,
            ] {
                assert_eq!(
                    futex(sys, 0, op, 0, 0, 0, MATCH_ANY).await,
                    Err(LxError::ENOSYS),
                    "op {:#x} with a null word",
                    op
                );
            }
            // The realtime bit on a command that has no absolute deadline
            // to put on a clock is the same answer, and the same order.
            assert_eq!(
                futex(sys, 0, FUTEX_WAKE | FUTEX_CLOCK_REALTIME, 1, 0, 0, 0).await,
                Err(LxError::ENOSYS),
                "a wake carrying FUTEX_CLOCK_REALTIME"
            );
            // An empty bitset is EINVAL, and that too is decided from `op`
            // and `val3` alone.
            assert_eq!(
                futex(sys, 0, FUTEX_WAKE_BITSET, 1, 0, 0, 0).await,
                Err(LxError::EINVAL),
                "a bitset wake with an empty mask"
            );
            assert_eq!(
                futex(sys, 0, FUTEX_WAIT_BITSET, 0, 0, 0, 0).await,
                Err(LxError::EINVAL),
                "a bitset wait with an empty mask"
            );
            // And an operation that IS served gets as far as the word, which
            // is where the null pointer is finally noticed.
            assert_eq!(
                futex(sys, 0, FUTEX_WAKE, 1, 0, 0, 0).await,
                Err(LxError::EFAULT),
                "a wake of a null word"
            );
        })
    });
}

/// The word is checked on every call, before any command does anything with
/// it, and a bad one is `EFAULT` rather than the `EINVAL` the futex table
/// would answer for the same address.
///
/// Both halves matter. The check being in the preamble rather than in each
/// arm is what keeps an unmapped word from taking the machine down through
/// whichever command happened to be asked, and the errno being `EFAULT` is
/// what tells a caller the pointer is the problem: `EINVAL` reads as "that
/// is not a futex operation" and sends musl down the wrong path.
#[test]
fn a_futex_word_that_is_null_or_unaligned_is_refused_whatever_the_operation_asked() {
    let proc = a_process(47_261, "futex-bad-word");
    let word = a_futex_word(0);
    let elsewhere = a_futex_word(0);
    let good = addr_of(word);
    let target = addr_of(elsewhere);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            for op in [
                FUTEX_WAIT,
                FUTEX_WAKE,
                FUTEX_REQUEUE,
                FUTEX_CMP_REQUEUE,
                FUTEX_LOCK_PI,
                FUTEX_LOCK_PI2,
                FUTEX_TRYLOCK_PI,
                FUTEX_UNLOCK_PI,
                FUTEX_WAIT_BITSET,
                FUTEX_WAKE_BITSET,
            ] {
                assert_eq!(
                    futex(sys, 0, op | FUTEX_PRIVATE_FLAG, 0, 0, target, MATCH_ANY).await,
                    Err(LxError::EFAULT),
                    "op {:#x} with a null word",
                    op
                );
                // One byte past a word that IS fine: the alignment a futex
                // word needs is the only thing wrong here.
                assert_eq!(
                    futex(
                        sys,
                        good + 1,
                        op | FUTEX_PRIVATE_FLAG,
                        0,
                        0,
                        target,
                        MATCH_ANY
                    )
                    .await,
                    Err(LxError::EFAULT),
                    "op {:#x} one byte off an aligned word",
                    op
                );
            }
            // And the word itself, aligned, is served.
            assert_eq!(
                futex(sys, good, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0).await,
                Ok(0),
                "a wake of a word nobody waits on"
            );
            assert_eq!(
                futex(
                    sys,
                    good,
                    FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG,
                    1,
                    0,
                    0,
                    MATCH_ANY
                )
                .await,
                Ok(0),
                "a bitset wake of a word nobody waits on"
            );
        })
    });
}

/// A `FUTEX_WAIT` whose word no longer holds the value the caller expected
/// is `EAGAIN`, and it gets there without reading the timeout.
///
/// That is the ordinary outcome of a contended `pthread_mutex_lock`: the
/// userspace compare-and-swap lost, the value moved again before the
/// syscall arrived, and the right answer is "look again" with nothing
/// allocated and nothing parked. The order is visible because a `timespec`
/// the kernel cannot read would be `EFAULT` if it were read first -- which
/// is also how this test knows the pointer really is unreadable.
#[test]
fn a_wait_on_a_word_that_has_already_changed_never_reads_the_timeout() {
    let proc = a_process(47_262, "futex-eagain");
    let moved = a_futex_word(7);
    let still = a_futex_word(0);
    let changed = addr_of(moved);
    let unchanged = addr_of(still);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let good = TimeSpec::from_duration(A_TIMEOUT);
            let at = &good as *const TimeSpec as usize;
            // The value is what it is judged on, and nothing else is read.
            assert_eq!(
                futex(
                    sys,
                    changed,
                    FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                    0,
                    at + 1,
                    0,
                    0
                )
                .await,
                Err(LxError::EAGAIN),
                "a wait on a word that had already moved"
            );
            // Same call on a word that did NOT move: now the timeout is
            // read, and the pointer above was unreadable all along.
            assert_eq!(
                futex(
                    sys,
                    unchanged,
                    FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                    0,
                    at + 1,
                    0,
                    0
                )
                .await,
                Err(LxError::EFAULT),
                "an unreadable timespec on a word that matched"
            );
            // The value compared is the one the caller GAVE, not zero: a
            // word holding seven and a wait expecting seven do match, and
            // the proof is that the unreadable timeout gets read after all.
            assert_eq!(
                futex(
                    sys,
                    changed,
                    FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                    7,
                    at + 1,
                    0,
                    0
                )
                .await,
                Err(LxError::EFAULT),
                "a wait whose expected value really is the word's"
            );
            // And a `timespec` that is readable but names no duration is
            // EINVAL, whether or not we would have ended up waiting.
            for bad in [
                TimeSpec {
                    sec: 1,
                    nsec: 1_000_000_000,
                },
                TimeSpec {
                    sec: usize::MAX,
                    nsec: 0,
                },
            ] {
                let at = &bad as *const TimeSpec as usize;
                assert_eq!(
                    futex(sys, unchanged, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, at, 0, 0).await,
                    Err(LxError::EINVAL),
                    "timespec {:?}",
                    bad
                );
                assert_eq!(
                    futex(
                        sys,
                        unchanged,
                        FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
                        0,
                        at,
                        0,
                        MATCH_ANY
                    )
                    .await,
                    Err(LxError::EINVAL),
                    "timespec {:?} on a bitset wait",
                    bad
                );
            }
        })
    });
}

/// A plain `FUTEX_WAIT` timeout is a duration counted from now; a
/// `FUTEX_WAIT_BITSET` one is a point on the monotonic clock.
///
/// The same `timespec` read the other way round is off by the host's whole
/// uptime: a relative 1.5 s read as an absolute monotonic deadline is
/// already hours past and comes back at once, and an absolute deadline read
/// as a relative one parks for as long as the machine has been up. Both
/// mistakes are invisible to the function that does the arithmetic, which
/// is handed the clock to use.
#[test]
fn a_relative_timeout_ends_an_untouched_wait_and_the_bitset_one_is_an_absolute_deadline() {
    let proc = a_process(47_263, "futex-timeout");
    let relative = a_futex_word(0);
    let absolute = a_futex_word(0);
    let counted = addr_of(relative);
    let dated = addr_of(absolute);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let soon = TimeSpec::from_duration(A_TIMEOUT);
            let at = &soon as *const TimeSpec as usize;
            let started = std::time::Instant::now();
            assert_eq!(
                futex(sys, counted, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, at, 0, 0).await,
                Err(LxError::ETIMEDOUT),
                "a wait nobody woke"
            );
            assert!(
                started.elapsed() >= HALFWAY,
                "the relative timeout was read as a deadline already past: back in {:?}",
                started.elapsed()
            );
            // The same wait as a point on the monotonic clock. Read as a
            // relative timeout it would park for the host's uptime and the
            // harness would give up on the call instead.
            let deadline =
                TimeSpec::from_duration(Duration::from(TimeSpec::now_monotonic()) + A_TIMEOUT);
            let at = &deadline as *const TimeSpec as usize;
            let started = std::time::Instant::now();
            assert_eq!(
                futex(
                    sys,
                    dated,
                    FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
                    0,
                    at,
                    0,
                    MATCH_ANY
                )
                .await,
                Err(LxError::ETIMEDOUT),
                "a bitset wait nobody woke"
            );
            assert!(
                started.elapsed() >= HALFWAY,
                "the monotonic deadline was read on another clock: back in {:?}",
                started.elapsed()
            );
        })
    });
}

/// `FUTEX_CLOCK_REALTIME` is which clock an absolute deadline is a point
/// on, and the two clocks here are nowhere near each other: monotonic time
/// is the host's uptime, a few thousand seconds, and the wall clock is
/// seconds since 1970.
///
/// So one deadline tells the two readings apart with no timing to measure.
/// A point a second and a half into the future of the monotonic clock is
/// decades past on the wall clock, and a kernel that reads it there answers
/// at once.
#[test]
fn the_realtime_bit_says_which_clock_an_absolute_deadline_is_on() {
    let proc = a_process(47_264, "futex-clock");
    let word = a_futex_word(0);
    let addr = addr_of(word);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let wall = Duration::from(TimeSpec::now());
            let mono = Duration::from(TimeSpec::now_monotonic());
            // The premise, stated rather than assumed: this test says
            // nothing unless the two clocks really are far apart.
            assert!(
                wall > mono + A_TIMEOUT,
                "the wall clock {:?} is not past the monotonic one {:?}",
                wall,
                mono
            );
            let deadline = TimeSpec::from_duration(mono + A_TIMEOUT);
            let at = &deadline as *const TimeSpec as usize;
            let started = std::time::Instant::now();
            assert_eq!(
                futex(
                    sys,
                    addr,
                    FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME,
                    0,
                    at,
                    0,
                    MATCH_ANY
                )
                .await,
                Err(LxError::ETIMEDOUT),
                "a wall-clock deadline in 1970"
            );
            assert!(
                started.elapsed() < HALFWAY,
                "a deadline long past on the wall clock waited anyway: {:?}",
                started.elapsed()
            );
        })
    });
}

/// Two threads parked with nothing to end the wait but a wake, and the
/// wakes that end them.
///
/// This is the pair the whole call exists for, and the only test here that
/// can see it: an untimed wait is what a contended `pthread_mutex_lock` and
/// a `pthread_cond_wait` come down to, and nothing in the kernel ends one
/// but a matching `FUTEX_WAKE`. The count asked for is how many are taken,
/// which is `pthread_cond_signal` against `pthread_cond_broadcast`: one
/// call takes one of the two and leaves the other where it was.
///
/// The second waiter asks WITHOUT `FUTEX_PRIVATE_FLAG` while both wakes
/// carry it. A word no two processes share belongs in the per-process table
/// either way, so the flag must not fork the queue; if it did, that waiter
/// would sit through its whole 10 seconds and answer `ETIMEDOUT`.
#[test]
fn a_wake_releases_the_waiters_one_at_a_time_and_a_count_of_zero_releases_none() {
    let proc = a_process(47_265, "futex-pair");
    let word = a_futex_word(0);
    let addr = addr_of(word);
    let untimed = a_waiter(&proc, addr, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, None);
    let timed = a_waiter(&proc, addr, FUTEX_WAIT, Some(Duration::from_secs(10)));
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            nobody_is_woken_while_the_count_is_zero(sys, addr).await;
            assert_eq!(
                a_wake_that_finds_somebody(sys, addr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1).await,
                Some(1),
                "the first wake did not take exactly one of the two waiters"
            );
            assert_eq!(
                a_wake_that_finds_somebody(sys, addr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1).await,
                Some(1),
                "the second wake did not reach the other waiter"
            );
            assert_eq!(
                futex(sys, addr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0).await,
                Ok(0),
                "a third wake found somebody still queued"
            );
        })
    });
    assert_eq!(
        untimed.join().unwrap(),
        Ok(0),
        "the waiter with no deadline"
    );
    assert_eq!(
        timed.join().unwrap(),
        Ok(0),
        "the waiter that asked without the private flag"
    );
}

/// An uncontended priority-inheritance lock takes the word and gives it
/// back, which is the whole of what musl probes for.
///
/// `pthread_mutexattr_setprotocol(PTHREAD_PRIO_INHERIT)` issues one
/// `FUTEX_LOCK_PI` on a zero word and passes the kernel's errno straight
/// out; PulseAudio asserts on it and aborts the process for anything but 0
/// or `ENOTSUP`, which is how supertux2 died on real hardware. So the test
/// is what the word holds afterwards, not merely that nothing failed: the
/// owner's tid goes in, the flag bits that were there stay, a second lock
/// by the owner is `EDEADLK`, somebody else's word is `EAGAIN` to try and
/// `EPERM` to unlock, and the unlock that does belong clears the word back
/// to a plain userspace compare-and-swap.
#[test]
fn an_uncontended_priority_lock_takes_the_word_and_gives_it_back() {
    let proc = a_process(47_266, "futex-pi");
    let free = a_futex_word(0);
    let flagged = a_futex_word(FUTEX_WAITERS | FUTEX_OWNER_DIED);
    let theirs = a_futex_word(0x1234);
    let second = a_futex_word(0);
    let mine = addr_of(free);
    let marked = addr_of(flagged);
    let somebody_elses = addr_of(theirs);
    let with_pi2 = addr_of(second);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let tid = (sys.thread.id() as i32) & FUTEX_TID_MASK;
            assert_ne!(tid, 0, "a thread with tid 0 cannot own a PI lock");
            assert_eq!(
                futex(sys, mine, FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Ok(0),
                "an uncontended PI lock"
            );
            assert_eq!(
                free.load(Ordering::SeqCst),
                tid,
                "the lock word does not name its owner"
            );
            // The owner asking again is a deadlock it is told about, and
            // that answer comes before the try/sleep split.
            assert_eq!(
                futex(sys, mine, FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Err(LxError::EDEADLK),
                "locking a PI mutex this thread already holds"
            );
            assert_eq!(
                futex(sys, mine, FUTEX_TRYLOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Err(LxError::EDEADLK),
                "trying for a PI mutex this thread already holds"
            );
            assert_eq!(
                futex(sys, mine, FUTEX_UNLOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Ok(0),
                "unlocking a PI mutex this thread holds"
            );
            assert_eq!(
                free.load(Ordering::SeqCst),
                0,
                "the unlock left something in the word with nobody queued"
            );
            assert_eq!(
                futex(sys, mine, FUTEX_UNLOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Err(LxError::EPERM),
                "unlocking a PI mutex nobody holds"
            );
            // A free word that still carries the robust-mutex marks keeps
            // them, so a later unlock still comes to the kernel and the
            // owner-death marker survives to the new owner.
            assert_eq!(
                futex(sys, marked, FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Ok(0),
                "a PI lock on a free word with the flag bits set"
            );
            assert_eq!(
                flagged.load(Ordering::SeqCst),
                tid | FUTEX_WAITERS | FUTEX_OWNER_DIED,
                "the flag bits did not survive the acquire"
            );
            // Somebody else's word: there is nothing to take and nothing to
            // give back.
            assert_eq!(
                futex(
                    sys,
                    somebody_elses,
                    FUTEX_TRYLOCK_PI | FUTEX_PRIVATE_FLAG,
                    0,
                    0,
                    0,
                    0
                )
                .await,
                Err(LxError::EAGAIN),
                "trying for a PI mutex another thread holds"
            );
            assert_eq!(
                futex(
                    sys,
                    somebody_elses,
                    FUTEX_UNLOCK_PI | FUTEX_PRIVATE_FLAG,
                    0,
                    0,
                    0,
                    0
                )
                .await,
                Err(LxError::EPERM),
                "unlocking a PI mutex another thread holds"
            );
            assert_eq!(
                theirs.load(Ordering::SeqCst),
                0x1234,
                "a refused PI operation wrote to the word anyway"
            );
            // `FUTEX_TRYLOCK_PI` never sleeps, so it never reads the
            // timeout either: a `timespec` that names no duration at all is
            // nothing to it.
            let nonsense = TimeSpec {
                sec: 0,
                nsec: 2_000_000_000,
            };
            let at = &nonsense as *const TimeSpec as usize;
            assert_eq!(
                futex(
                    sys,
                    somebody_elses,
                    FUTEX_TRYLOCK_PI | FUTEX_PRIVATE_FLAG,
                    0,
                    at,
                    0,
                    0
                )
                .await,
                Err(LxError::EAGAIN),
                "a try-lock read a timeout it never uses"
            );
            // `FUTEX_LOCK_PI2` is the same acquire on its own clock, and
            // the third name in the arm.
            assert_eq!(
                futex(
                    sys,
                    with_pi2,
                    FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG,
                    0,
                    0,
                    0,
                    0
                )
                .await,
                Ok(0),
                "an uncontended LOCK_PI2"
            );
            assert_eq!(
                second.load(Ordering::SeqCst),
                tid,
                "LOCK_PI2 did not take the word"
            );
        })
    });
}

/// `FUTEX_CMP_REQUEUE` reads the word and refuses if it has moved;
/// `FUTEX_REQUEUE` does not read it at all.
///
/// Which of the two was asked for is the only thing that tells the kernel
/// whether to check, and the check is what makes a `pthread_cond_broadcast`
/// safe against a mutex that changed hands underneath it. The target word
/// is resolved through the futex table rather than the bounds check, so a
/// target this kernel cannot key answers `EINVAL` where the source word
/// would have answered `EFAULT`.
#[test]
fn a_compare_requeue_checks_the_word_before_it_moves_anybody() {
    let proc = a_process(47_267, "futex-cmp");
    let word = a_futex_word(5);
    let elsewhere = a_futex_word(0);
    let from = addr_of(word);
    let to = addr_of(elsewhere);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                futex(
                    sys,
                    from,
                    FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                    1,
                    1,
                    to,
                    4
                )
                .await,
                Err(LxError::EAGAIN),
                "a compare-requeue against a value the word does not hold"
            );
            assert_eq!(
                futex(
                    sys,
                    from,
                    FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                    1,
                    1,
                    to,
                    5
                )
                .await,
                Ok(0),
                "a compare-requeue of a word nobody waits on"
            );
            // The plain requeue never reads the word, so the value it is
            // handed is nothing to it.
            assert_eq!(
                futex(sys, from, FUTEX_REQUEUE | FUTEX_PRIVATE_FLAG, 1, 1, to, 4).await,
                Ok(0),
                "a plain requeue read the value it was given"
            );
            // The target: not through the bounds check, so not EFAULT.
            for bad in [0, to + 1] {
                assert_eq!(
                    futex(
                        sys,
                        from,
                        FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                        1,
                        1,
                        bad,
                        5
                    )
                    .await,
                    Err(LxError::EINVAL),
                    "a requeue target at {:#x}",
                    bad
                );
            }
        })
    });
}

/// A requeue wakes `val` of the waiters and moves `val2` of the rest to the
/// word `uaddr2` names, and answers with how many it touched in all.
///
/// Three arguments of the same width, two of them counts, and nothing but a
/// move that really happens says which is which: swap the two counts and a
/// broadcast wakes the wrong number, put the target anywhere else and the
/// waiters it moved are on a queue nobody can reach and sleep for good.
/// Answering 0 instead of the total is its own bug -- it told every caller
/// that a broadcast had reached nobody.
#[test]
fn a_requeue_wakes_the_count_it_was_given_and_moves_the_rest_to_the_word_it_names() {
    let proc = a_process(47_268, "futex-requeue");
    let word = a_futex_word(0);
    let elsewhere = a_futex_word(0);
    let from = addr_of(word);
    let to = addr_of(elsewhere);
    // Three waiters and two DIFFERENT counts, so the number to wake and
    // the number to move cannot pass for one another.
    let untimed = a_waiter(&proc, from, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, None);
    let timed = [
        a_waiter(
            &proc,
            from,
            FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
            Some(Duration::from_secs(10)),
        ),
        a_waiter(
            &proc,
            from,
            FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
            Some(Duration::from_secs(10)),
        ),
    ];
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            nobody_is_woken_while_the_count_is_zero(sys, from).await;
            assert_eq!(
                futex(
                    sys,
                    from,
                    FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG,
                    1,
                    2,
                    to,
                    0
                )
                .await,
                Ok(3),
                "one woken and two moved is three waiters accounted for"
            );
            // The source queue is empty now: one left through the wake and
            // two through the move.
            assert_eq!(
                futex(sys, from, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 3, 0, 0, 0).await,
                Ok(0),
                "somebody was still on the word the requeue emptied"
            );
            // And the two that moved are both on the target word. The move
            // happens inside the requeue call, so they are there already
            // and there is nothing to wait for: a wake addressed to the
            // target finds exactly the two of them.
            assert_eq!(
                futex(sys, to, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 3, 0, 0, 0).await,
                Ok(2),
                "the waiters the requeue moved were not both on the target"
            );
        })
    });
    assert_eq!(untimed.join().unwrap(), Ok(0), "the untimed waiter");
    for (n, waiter) in IntoIterator::into_iter(timed).enumerate() {
        assert_eq!(waiter.join().unwrap(), Ok(0), "timed waiter {}", n);
    }
}

/// The six arguments reach `sys_futex` in the order the syscall table hands
/// them over, each one pinned by an answer only it can produce.
///
/// A dispatch table is a long column of calls whose arguments are all
/// `usize`, and the compiler has nothing to say about two of them swapped.
#[test]
fn the_arguments_arrive_in_the_order_the_syscall_table_names_them() {
    let proc = a_process(47_269, "futex-by-number");
    let word = a_futex_word(0);
    let moved = a_futex_word(3);
    let elsewhere = a_futex_word(0);
    let addr = addr_of(word);
    let changed = addr_of(moved);
    let to = addr_of(elsewhere);
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let private = (FUTEX_WAKE | FUTEX_PRIVATE_FLAG) as usize;
            // First: the word. One byte off and it is EFAULT.
            assert_eq!(
                by_number(sys, [addr + 1, private, 1, 0, 0, 0]).await,
                -(LxError::EFAULT as isize),
                "the word is not the first argument"
            );
            assert_eq!(
                by_number(sys, [addr, private, 1, 0, 0, 0]).await,
                0,
                "a wake of a word nobody waits on, by number"
            );
            // Second: the operation.
            assert_eq!(
                by_number(sys, [addr, FUTEX_WAIT_REQUEUE_PI as usize, 0, 0, 0, 0]).await,
                -(LxError::ENOSYS as isize),
                "the operation is not the second argument"
            );
            // Third: the value a wait compares the word against.
            assert_eq!(
                by_number(
                    sys,
                    [
                        changed,
                        (FUTEX_WAIT | FUTEX_PRIVATE_FLAG) as usize,
                        0,
                        0,
                        0,
                        0
                    ]
                )
                .await,
                -(LxError::EAGAIN as isize),
                "the expected value is not the third argument"
            );
            // Fourth: where the timeout is. A readable `timespec` that
            // names no duration can come from nowhere else.
            let bad = TimeSpec {
                sec: 0,
                nsec: 2_000_000_000,
            };
            let at = &bad as *const TimeSpec as usize;
            assert_eq!(
                by_number(
                    sys,
                    [
                        addr,
                        (FUTEX_WAIT | FUTEX_PRIVATE_FLAG) as usize,
                        0,
                        at,
                        0,
                        0
                    ]
                )
                .await,
                -(LxError::EINVAL as isize),
                "the timeout is not the fourth argument"
            );
            // Fifth: the requeue target. Were the fourth and fifth swapped,
            // the target would be the count 1 and that is no futex word.
            assert_eq!(
                by_number(
                    sys,
                    [
                        addr,
                        (FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG) as usize,
                        1,
                        1,
                        to,
                        0
                    ]
                )
                .await,
                0,
                "the requeue target is not the fifth argument"
            );
            // Sixth: the bitset. Empty is EINVAL and anything else is not.
            assert_eq!(
                by_number(
                    sys,
                    [
                        addr,
                        (FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG) as usize,
                        1,
                        0,
                        0,
                        0
                    ]
                )
                .await,
                -(LxError::EINVAL as isize),
                "the bitset is not the sixth argument"
            );
            assert_eq!(
                by_number(
                    sys,
                    [
                        addr,
                        (FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG) as usize,
                        1,
                        0,
                        0,
                        MATCH_ANY as usize
                    ]
                )
                .await,
                0,
                "a bitset wake with a full mask, by number"
            );
        })
    });
}

/// Every futex call whose operation was understood is counted, and the ones
/// that went looking for a word another process shares are counted apart.
///
/// Those counters are not decoration: the futex storm that `epoll_pwait`
/// and `tokio` turned into was found by subtracting them, and the cost of
/// the shared-word search -- a second descent of the address space on a
/// call that almost always answers "no" -- is only visible because the
/// probes and the descents are counted separately from the calls.
///
/// Every bound here is a floor, never an exact figure: the counters are
/// machine-wide and every other test in this binary adds to them while this
/// one runs. A floor is still enough, because a call that stopped counting
/// stops moving them at all.
#[test]
fn every_futex_call_whose_operation_was_understood_is_counted() {
    let proc = a_process(47_270, "futex-counted");
    let word = a_futex_word(0);
    let addr = addr_of(word);
    /// How many of each kind of call to make.
    const EACH: u64 = 32;
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let (ops, probes, _, walks) = kernel_hal::kstats::futex_stats();
            for _ in 0..EACH {
                // A private wake: one call, one bounds check, no search.
                assert_eq!(
                    futex(sys, addr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0).await,
                    Ok(0)
                );
                // And one without the flag, which has to go looking for a
                // word two processes share before it can answer.
                assert_eq!(futex(sys, addr, FUTEX_WAKE, 1, 0, 0, 0).await, Ok(0));
            }
            let (now_ops, now_probes, _, now_walks) = kernel_hal::kstats::futex_stats();
            assert!(
                now_ops >= ops + 2 * EACH,
                "{} calls counted {} of them",
                2 * EACH,
                now_ops - ops
            );
            assert!(
                now_probes >= probes + EACH,
                "{} calls without the private flag counted {} searches",
                EACH,
                now_probes - probes
            );
            // One descent for the bounds check on every call, and a second
            // one for the search the calls without the flag perform.
            assert!(
                now_walks >= walks + 3 * EACH,
                "{} calls counted {} descents of the address space",
                2 * EACH,
                now_walks - walks
            );
        })
    });
}

/// A thread of `proc` contending for the PI lock at `addr`, on its own host
/// thread, answering with what it got and which tid it is.
///
/// Its own tid is the point: what the word holds after the hand-off is the
/// claim this makes, and only the thread that took it knows the number.
fn a_pi_waiter(proc: &Arc<Process>, addr: usize) -> std::thread::JoinHandle<(SysResult, i32)> {
    let proc = proc.clone();
    std::thread::spawn(move || {
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let tid = (sys.thread.id() as i32) & FUTEX_TID_MASK;
                let got = futex(sys, addr, FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await;
                (got, tid)
            })
        })
    })
}

/// A contended PI lock publishes `FUTEX_WAITERS` in the word, and the
/// owner's unlock passes the word on with that mark still in it.
///
/// This is the half of the protocol a single thread cannot reach, and the
/// mark is the whole of why the unlock has to come to the kernel at all: a
/// word that does not carry it is released by a userspace compare-and-swap
/// and the thread queued behind it is never woken. After the hand-off the
/// word names its new owner AND still says somebody was queued, because the
/// acquire keeps the flag bits it found rather than overwriting the word.
///
/// The test knows the waiter is queued by watching for that mark rather
/// than by sleeping for a while: the bit appearing is the waiter's own
/// announcement that it is about to park.
#[test]
fn a_contended_priority_lock_passes_the_word_from_its_owner_to_the_waiter() {
    let proc = a_process(47_271, "futex-pi-pair");
    let word = a_futex_word(0);
    let addr = addr_of(word);
    let theirs = proc.clone();
    let waiter = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let tid = (sys.thread.id() as i32) & FUTEX_TID_MASK;
            assert_eq!(
                futex(sys, addr, FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Ok(0),
                "the uncontended acquire this test starts from"
            );
            // Only now that the word is held can anybody contend for it.
            let waiter = a_pi_waiter(&theirs, addr);
            let mut queued = false;
            for _ in 0..TRIES {
                if word.load(Ordering::SeqCst) & FUTEX_WAITERS != 0 {
                    queued = true;
                    break;
                }
                async_std::task::sleep(A_MOMENT).await;
            }
            assert!(
                queued,
                "the contending thread never marked the word: {:#x}",
                word.load(Ordering::SeqCst)
            );
            assert_eq!(
                word.load(Ordering::SeqCst),
                tid | FUTEX_WAITERS,
                "the mark did not go in beside the owner that is still there"
            );
            assert_eq!(
                futex(sys, addr, FUTEX_UNLOCK_PI | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0).await,
                Ok(0),
                "the owner's unlock with somebody queued"
            );
            waiter
        })
    });
    let (got, their_tid) = waiter.join().unwrap();
    assert_eq!(got, Ok(0), "the thread that waited for the lock");
    assert_ne!(their_tid, 0, "a thread with tid 0 cannot own a PI lock");
    assert_eq!(
        word.load(Ordering::SeqCst),
        their_tid | FUTEX_WAITERS,
        "the word did not pass to the waiter with its mark still on it"
    );
}

/// Which clock a PI lock's absolute deadline is a point on is decided by
/// the command, not by the caller: `FUTEX_LOCK_PI` always reads it on the
/// wall clock, `FUTEX_LOCK_PI2` on the monotonic one unless
/// `FUTEX_CLOCK_REALTIME` says otherwise.
///
/// `FUTEX_LOCK_PI` predates the flag and could never carry it, which is
/// exactly why its clock is not negotiable and why nothing but a timed
/// contended lock can show which one it used. One deadline tells all three
/// readings apart: a point a second and a half ahead of the monotonic clock
/// is decades past on the wall clock.
#[test]
fn a_contended_priority_lock_times_out_on_the_clock_its_command_chooses() {
    let proc = a_process(47_272, "futex-pi-clock");
    // One word per case: a lock that timed out leaves its mark behind.
    let on_the_wall = addr_of(a_futex_word(0x1234));
    let on_the_tick = addr_of(a_futex_word(0x1234));
    let told_otherwise = addr_of(a_futex_word(0x1234));
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let wall = Duration::from(TimeSpec::now());
            let mono = Duration::from(TimeSpec::now_monotonic());
            assert!(
                wall > mono + A_TIMEOUT,
                "the wall clock {:?} is not past the monotonic one {:?}",
                wall,
                mono
            );
            let deadline = TimeSpec::from_duration(mono + A_TIMEOUT);
            let at = &deadline as *const TimeSpec as usize;
            let started = std::time::Instant::now();
            assert_eq!(
                futex(
                    sys,
                    on_the_wall,
                    FUTEX_LOCK_PI | FUTEX_PRIVATE_FLAG,
                    0,
                    at,
                    0,
                    0
                )
                .await,
                Err(LxError::ETIMEDOUT),
                "a LOCK_PI on a word another thread holds"
            );
            assert!(
                started.elapsed() < HALFWAY,
                "LOCK_PI read its deadline on the monotonic clock: {:?}",
                started.elapsed()
            );
            let started = std::time::Instant::now();
            assert_eq!(
                futex(
                    sys,
                    on_the_tick,
                    FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG,
                    0,
                    at,
                    0,
                    0
                )
                .await,
                Err(LxError::ETIMEDOUT),
                "a LOCK_PI2 on a word another thread holds"
            );
            assert!(
                started.elapsed() >= HALFWAY,
                "LOCK_PI2 read its deadline on the wall clock: {:?}",
                started.elapsed()
            );
            let started = std::time::Instant::now();
            assert_eq!(
                futex(
                    sys,
                    told_otherwise,
                    FUTEX_LOCK_PI2 | FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME,
                    0,
                    at,
                    0,
                    0
                )
                .await,
                Err(LxError::ETIMEDOUT),
                "a LOCK_PI2 told to use the wall clock"
            );
            assert!(
                started.elapsed() < HALFWAY,
                "FUTEX_CLOCK_REALTIME did not change the clock: {:?}",
                started.elapsed()
            );
        })
    });
}

/// A page of this binary's own for a shared mapping to land on.
///
/// The hosted page table maps a frame with `MAP_FIXED` at the user address,
/// so that address has to be a page-aligned region this process already
/// owns: anywhere else the mapping lands on whatever the host keeps there.
/// Leaked, and never touched through this type again.
#[repr(align(4096))]
struct APage([u8; 4096]);

impl APage {
    /// Where this page begins, as userspace names it.
    fn at(&self) -> usize {
        self.0.as_ptr() as usize
    }
}

/// One page of a SHARED object mapped into `proc` read-write, and the user
/// address its futex word sits at.
///
/// This is the shape libxshmfence uses, and the reason shared futexes exist
/// at all: one `int32` in a `memfd` that two processes map, waited on by the
/// X client and woken by the X server, neither side carrying the private
/// flag.
fn a_shared_word_in(proc: &Arc<Process>) -> usize {
    assert_eq!(PAGE_SIZE, 4096, "the page this test leaks is 4 KiB");
    let vmo = VmObject::new_paged(1);
    vmo.set_share_on_fork();
    vmo.commit(0, PAGE_SIZE).unwrap();
    let user = Box::leak(Box::new(APage([0; 4096]))).at();
    let vmar = proc.vmar();
    vmar.map_at(
        user - vmar.addr(),
        vmo,
        0,
        PAGE_SIZE,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
    )
    .unwrap();
    // The mapping must never be undone: the frame now covers a page of this
    // binary's own heap, and tearing it down would leave that address
    // unmapped for the rest of the run.
    core::mem::forget(proc.clone());
    user
}

/// A word in a mapping two processes could share belongs to one queue, and
/// the per-process table is a different one: the private flag picks which,
/// and a wake sent to the wrong one reaches nobody at all.
///
/// This is the whole of what the shared-futex table is for, and the
/// asymmetry behind `eglgears_wayland` running while `glxgears` hung on the
/// same machine. libxshmfence waits and wakes on a word in a shared `memfd`
/// with no private flag; serving that from the per-process table, which
/// keys by the caller's own virtual address, gives each side its own queue,
/// so the X server's wake never reaches the client and the window freezes
/// with no way to tell why. Wayland clients do not use xshmfence, which is
/// why only one of the two hung.
///
/// Nothing in the test reads the word: it is a fresh committed page, so it
/// holds zero, which is the value the waiter expects.
#[test]
fn a_word_in_a_shared_mapping_is_a_different_queue_from_the_private_one() {
    let proc = a_process(47_273, "futex-shared");
    let addr = a_shared_word_in(&proc);
    let waiting = a_waiter(&proc, addr, FUTEX_WAIT, Some(Duration::from_secs(10)));
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // A second of wakes carrying the private flag, which go to the
            // per-process table: the waiter is not there to be found.
            for _ in 0..100 {
                assert_eq!(
                    futex(sys, addr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0).await,
                    Ok(0),
                    "a private wake reached a waiter queued on the shared word"
                );
                async_std::task::sleep(A_MOMENT).await;
            }
            // And one without the flag, which goes looking for the shared
            // word first and finds the queue the waiter is really on.
            assert_eq!(
                a_wake_that_finds_somebody(sys, addr, FUTEX_WAKE, 1).await,
                Some(1),
                "a shared wake did not reach the waiter on the shared word"
            );
        })
    });
    assert_eq!(
        waiting.join().unwrap(),
        Ok(0),
        "the waiter on the shared word"
    );
}
