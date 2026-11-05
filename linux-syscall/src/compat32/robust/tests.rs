//! The two numbers, against the table in the module documentation --- which
//! was measured on this container's Linux with an i386 program, not read off
//! a header.
//!
//! Every call goes through `compat32_syscall` **by number** so that deleting
//! the arm fails a test here instead of quietly restoring the `ENOSYS` that
//! was there before.
//!
//! Fixed pids live in 48_63x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::vec::Vec;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::thread::{robust_word_bytes, CurrentThreadExt, RobustList};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};
use zircon_object::vm::{MMUFlags, VmObject};

const EINVAL: isize = -(LxError::EINVAL as isize);

const USER_AT: usize = 0x3200_0000;
const USER_PAGES: usize = 2;
const PAGE_SIZE: usize = 0x1000;

/// A head a 32-bit libc could really register: inside the user half and
/// 4-aligned.
const HEAD_AT: usize = USER_AT + 0x40;
/// And the same thing at 1 modulo 4, which Linux accepts: nothing reads the
/// head at registration time. Measured.
const HEAD_ODD: usize = USER_AT + 0x141;
/// Where `get_robust_list` writes, with a known word after each so that
/// writing eight bytes where four belong is visible. `0xdeadbeef` is what the
/// measurement on Linux left there untouched.
const OUT_HEAD: usize = USER_AT + 0x200;
const OUT_LEN: usize = USER_AT + 0x240;
const GUARD: u32 = 0xdead_beef;

/// And where the list a thread DIES holding goes: the head, one node, and
/// the lock word four bytes before that node, which is what the negative
/// `futex_offset` means.
///
/// 4-aligned and NOT 8-aligned, which is how a 32-bit allocator hands one
/// out: the exit walk used to refuse any word that was not aligned for THIS
/// kernel's `usize`, which would have skipped the walk in silence.
const HELD_HEAD: usize = USER_AT + 0x304;
const HELD_NODE: usize = HELD_HEAD + 0x40;
const HELD_WORD: usize = HELD_NODE - 4;

/// `FUTEX_OWNER_DIED`, bit 30 of a lock word (`<linux/futex.h>`).
const OWNER_DIED: u32 = 0x4000_0000;

/// Lay out a list holding one lock this thread owns, and register it.
fn hold_one_lock(tid: u32) {
    put(HELD_HEAD, &(HELD_NODE as u32).to_ne_bytes());
    // `-4` as a 32-bit `long`, which is what both libcs write.
    put(HELD_HEAD + 4, &(-4i32).to_ne_bytes());
    put(HELD_HEAD + 8, &0u32.to_ne_bytes());
    // The last node, so its `next` comes back to the head.
    put(HELD_NODE, &(HELD_HEAD as u32).to_ne_bytes());
    // The word `pthread_mutex_lock` leaves holding the owner's tid.
    put(HELD_WORD, &tid.to_ne_bytes());
}

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    let proc = Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "robust32", lp).unwrap();
    proc.vmar()
        .map_at(
            USER_AT,
            VmObject::new_paged(USER_PAGES),
            0,
            USER_PAGES * PAGE_SIZE,
            MMUFlags::READ | MMUFlags::WRITE | MMUFlags::USER,
        )
        .expect("pages of the process's own, at an address a u32 can hold");
    proc
}

fn put(at: usize, bytes: &[u8]) {
    let mut p: kernel_hal::user::UserOutPtr<u8> = at.into();
    p.write_array(bytes).expect("mapped and writable");
}

fn get(at: usize, len: usize) -> Vec<u8> {
    let p: kernel_hal::user::UserInPtr<u8> = at.into();
    p.read_array(len).expect("mapped and readable")
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// **The narrow head is twelve bytes and the native one twenty-four**, which
/// is what makes the length an argument the two calls refuse each other over.
#[test]
fn the_two_heads_are_three_words_each_at_their_own_width() {
    assert_eq!(ROBUST_LIST32_SIZE, 12);
    assert_eq!(core::mem::size_of::<RobustList>(), 24);
    // And the length is what the exit walk reads the width off, because it
    // is the only thing that records which ABI registered the list.
    assert_eq!(robust_word_bytes(ROBUST_LIST32_SIZE), Some(4));
    assert_eq!(robust_word_bytes(24), Some(8));
    for refused in [0, 8, 13, 23, 25, usize::MAX] {
        assert_eq!(
            robust_word_bytes(refused),
            None,
            "{} is a length no set_robust_list accepted",
            refused
        );
    }
}

/// **Twelve is accepted and twenty-four is refused**: the measured table, and
/// the reason the number cannot be translated to the native one, which
/// refuses exactly the other way round.
#[test]
fn number_311_takes_twelve_and_refuses_the_native_length() {
    let proc = a_process(48_630, Abi::Linux32);
    let answers = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let mut answers = Vec::new();
            for len in [12usize, 24, 0, 13] {
                answers.push(
                    sys.compat32_syscall(I386_SET_ROBUST_LIST, [HEAD_AT, len, 0, 0, 0, 0])
                        .await,
                );
            }
            // And a null head with the right length, which is how a libc
            // says "forget the list I gave you". Measured: 0.
            answers.push(
                sys.compat32_syscall(I386_SET_ROBUST_LIST, [0, 12, 0, 0, 0, 0])
                    .await,
            );
            // And an unaligned one, which Linux also accepts: nothing reads
            // the head here. Measured: 0 for a head at 1 modulo 4.
            answers.push(
                sys.compat32_syscall(I386_SET_ROBUST_LIST, [HEAD_ODD, 12, 0, 0, 0, 0])
                    .await,
            );
            answers
        })
    });
    assert_eq!(answers[0], 0, "len=12 is this ABI's sizeof(*head)");
    assert_eq!(answers[1], EINVAL, "len=24 is the NATIVE head's size");
    assert_eq!(answers[2], EINVAL, "len=0");
    assert_eq!(answers[3], EINVAL, "len=13");
    assert_eq!(answers[4], 0, "a null head is how a list is forgotten");
    assert_eq!(answers[5], 0, "an unaligned head is accepted");
    proc.exit(0);
}

/// **`get_robust_list` writes two 4-byte words and not two 8-byte ones**, and
/// what it answers is the head that was registered.
///
/// The guard word after each output is the measurement: on Linux the word
/// past the length came back untouched, so a handler writing a native
/// `usize` there is writing over the caller's next variable.
#[test]
fn number_312_answers_the_registered_head_in_four_bytes() {
    let proc = a_process(48_631, Abi::Linux32);
    let (set, got, out) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(OUT_HEAD, &GUARD.to_ne_bytes());
            put(OUT_HEAD + 4, &GUARD.to_ne_bytes());
            put(OUT_LEN, &GUARD.to_ne_bytes());
            put(OUT_LEN + 4, &GUARD.to_ne_bytes());
            let set = sys
                .compat32_syscall(I386_SET_ROBUST_LIST, [HEAD_AT, 12, 0, 0, 0, 0])
                .await;
            let got = sys
                .compat32_syscall(I386_GET_ROBUST_LIST, [0, OUT_HEAD, OUT_LEN, 0, 0, 0])
                .await;
            // Read inside the closure: outside it another test's address
            // space may be the current one.
            let out = (get(OUT_HEAD, 8), get(OUT_LEN, 8));
            (set, got, out)
        })
    });
    assert_eq!(set, 0);
    assert_eq!(got, 0);
    let (head, len) = out;
    assert_eq!(
        word(&head, 0),
        HEAD_AT as u32,
        "312 did not answer the head 311 registered"
    );
    assert_eq!(
        word(&head, 4),
        GUARD,
        "312 wrote eight bytes where a 32-bit pointer takes four"
    );
    assert_eq!(word(&len, 0), 12, "the length is this ABI's sizeof(*head)");
    assert_eq!(
        word(&len, 4),
        GUARD,
        "312 wrote eight bytes where a 32-bit size_t takes four"
    );
    proc.exit(0);
}

/// And a thread that never registered one is answered too, with a null head
/// and the same length: measured, Linux answers `sizeof(*head)` whatever the
/// thread did.
#[test]
fn a_thread_that_never_registered_a_list_is_answered_with_a_null_head() {
    let proc = a_process(48_632, Abi::Linux32);
    let (got, out) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            put(OUT_HEAD, &GUARD.to_ne_bytes());
            put(OUT_LEN, &GUARD.to_ne_bytes());
            let got = sys
                .compat32_syscall(I386_GET_ROBUST_LIST, [0, OUT_HEAD, OUT_LEN, 0, 0, 0])
                .await;
            (got, (get(OUT_HEAD, 4), get(OUT_LEN, 4)))
        })
    });
    assert_eq!(got, 0);
    assert_eq!(word(&out.0, 0), 0, "no list was ever registered");
    assert_eq!(word(&out.1, 0), 12);
    proc.exit(0);
}

/// **The two numbers are answered here and not by the table**, which is what
/// keeps the native handler --- whose length check is the other one --- out
/// of a 32-bit caller's way.
#[test]
fn the_two_numbers_are_the_32_bit_entry_and_not_a_row_in_the_table() {
    use super::super::{name, native_number};
    for (num, called) in [
        (I386_SET_ROBUST_LIST, "set_robust_list"),
        (I386_GET_ROBUST_LIST, "get_robust_list"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) must not reach the native handler: its length check \
             refuses the only length a 32-bit libc passes",
            num,
            called
        );
    }
    // The native numbers they would have been translated into, for the
    // record: the table is default-deny, so these being absent is a
    // decision and not an omission.
    assert_eq!(I386_SET_ROBUST_LIST, 311);
    assert_eq!(I386_GET_ROBUST_LIST, 312);
}

/// **`rseq` (386) answers `ENOSYS`, and that is the right answer.**
///
/// The other of the two numbers a real dynamic i386 program's startup asks
/// for that this kernel does not implement --- glibc's loader calls
/// `set_robust_list` and then `rseq` on every thread, and the native
/// dispatch answers `ENOSYS` to `rseq` ON PURPOSE (see `Sys::RSEQ` in
/// `lib.rs`): restartable sequences are not implemented, glibc probes them
/// once per thread and falls back in silence, and advertising them would be
/// a lie the C library would then trust.
///
/// So the 32-bit side needs nothing, and this pins that it needs nothing:
/// adding `rseq` to the allowlist would translate the number onto a handler
/// whose whole body is that `ENOSYS`, which is the same answer through more
/// code. Here so that the pair this batch came from is both measured and
/// both written down.
#[test]
fn rseq_needs_nothing_from_this_side_because_the_native_answer_is_enosys() {
    use super::super::{name, native_number};
    const I386_RSEQ: u32 = 386;
    assert_eq!(name(I386_RSEQ), Some("rseq"));
    assert_eq!(
        native_number(I386_RSEQ),
        None,
        "rseq is not in the allowlist, so a 32-bit caller gets the ENOSYS \
         the native call gives --- which is the answer, not a gap"
    );
    let proc = a_process(48_634, Abi::Linux32);
    let answer = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            as_mut(sys)
                .compat32_syscall(I386_RSEQ, [USER_AT, 0x20, 0, 0x5305_3053, 0, 0])
                .await
        })
    });
    assert_eq!(
        answer,
        -(LxError::ENOSYS as isize),
        "the arguments glibc really passes: a 32-byte struct rseq and its \
         signature"
    );
    proc.exit(0);
}

/// **A 32-bit thread that dies holding a lock gets it marked.**
///
/// The other half of the pair, and the one no system call reaches: the list
/// is read by the KERNEL at thread exit, and every word of it is four bytes
/// wide. This is Linux's `compat_exit_robust_list`, and it runs at the exit
/// of every thread of every 32-bit program, because both libcs register a
/// list per thread.
///
/// Driven through [`CurrentThreadExt::release_robust_locks`], which is the
/// call the run loop makes on every way a thread dies --- and from inside
/// the thread, because the walk reads the process's own memory and needs its
/// address space to be the current one.
#[test]
fn a_32_bit_thread_s_lock_is_marked_when_it_dies() {
    let proc = a_process(48_635, Abi::Linux32);
    let (set, before, after) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let tid = sys.thread.id() as u32;
            hold_one_lock(tid);
            let set = sys
                .compat32_syscall(I386_SET_ROBUST_LIST, [HELD_HEAD, 12, 0, 0, 0, 0])
                .await;
            let before = get(HELD_WORD, 4);
            sys.thread.release_robust_locks();
            (set, before, get(HELD_WORD, 4))
        })
    });
    assert_eq!(set, 0);
    assert_ne!(word(&before, 0), 0, "the fixture must hold the lock first");
    assert_eq!(
        word(&after, 0),
        OWNER_DIED,
        "the lock of a 32-bit thread was left owned by a thread that is gone: \
         the next pthread_mutex_lock on it blocks for ever"
    );
    proc.exit(0);
}

/// And a length neither call would have accepted releases nothing: the walk
/// has no width to read the list at, and guessing one would mark words at
/// addresses computed from a stride the list was never written with.
#[test]
fn a_list_registered_with_an_impossible_length_is_not_walked() {
    let proc = a_process(48_636, Abi::Linux32);
    let (held, after) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let sys = as_mut(sys);
            let tid = sys.thread.id() as u32;
            hold_one_lock(tid);
            // 16 is neither head's size, so no `set_robust_list` answered 0
            // for it: registered behind the call's back, on purpose.
            sys.thread.set_robust_list(HELD_HEAD.into(), 16);
            let held = get(HELD_WORD, 4);
            sys.thread.release_robust_locks();
            (held, get(HELD_WORD, 4))
        })
    });
    assert_eq!(
        word(&after, 0),
        word(&held, 0),
        "a list of an impossible width was walked anyway"
    );
    proc.exit(0);
}
