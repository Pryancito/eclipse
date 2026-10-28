//! `clone3(2)`, asked through the syscall and through its number.
//!
//! `clone3_tests` next door covers the two pure halves thoroughly:
//! `clone3_size` and `clone3_to_clone`, the second of which is where the
//! stack, the exit-signal byte, the unknown flag bits and the pidfd slot are
//! decided. What has never run is everything between them and `sys_clone`:
//!
//!  * the dispatch arm. `Sys::CLONE3` appears exactly once outside this
//!    file, in the table, and the implementation spent a while sitting
//!    UNDER a `Sys::CLONE3 => Err(LxError::ENOSYS)` stub with
//!    `#[allow(unreachable_patterns)]` keeping the build quiet: the arm
//!    answered every `clone3` and the code below it was decoration. Nothing
//!    would have noticed it coming back.
//!  * the order of the two refusals that live in the method itself: the size
//!    is judged before the struct is read, and the tail past version 0 is
//!    read before the struct is decoded.
//!  * that the eight words, once decoded, reach `sys_clone` as its
//!    arguments. `clone3_to_clone` returning the right `Clone3Args` says
//!    nothing about where they then go.
//!
//! The last one is the hard one, because `sys_clone` with nothing wrong
//! FORKS. One test here does exactly that and reaps the child, since nothing
//! else can show that the chain arrives at all; the rest leave exactly one
//! thing wrong, and pick a wrong thing whose errno no other stage can
//! answer:
//!
//!  * `CLONE_NEWUSER` is a bit this kernel NAMES, so `clone3_to_clone`
//!    hands it on, and REFUSES, in `sys_clone`'s namespace gate. A struct
//!    whose only fault is that bit separates "the flags word arrived" from
//!    "the flags word was decoded".
//!  * `CLONE_SETTLS` with a tls outside user space is **`EPERM`**, and
//!    `EPERM` is an errno nothing in `clone3`'s own decoding can produce.
//!    It pins the LAST of the eight words as well as the first.
//!
//! Between them the tests follow seven of the eight words to the end: the
//! flags, the pidfd slot (`CLONE_PIDFD`, where the descriptor lands), the
//! child_tid (`CLONE_CHILD_SETTID`, where the new task's tid lands), the
//! parent_tid (the slot the pidfd does NOT come from), the stack and its
//! length, and the tls. What is NOT pinned is `newsp` as an ARGUMENT: the
//! sum of words 5 and 6 only ever lands in a task's stack pointer, so the
//! mutation lot's `args.newsp` as a constant survives everything here, and
//! what would see it is a child that runs -- which this harness ends at
//! once, by design. What is pinned there is the sum being read and refused
//! when it overflows, and `newsp` not being confused with another argument.
//!
//! Every test asserts the decode as a premise first, so a failure says which
//! half it is in.
//!
//! Fixed pids live in 47_18x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// `CLONE_NEWUSER`, the namespace this kernel has not got.
const A_NAMESPACE: u64 = 1 << 28;

/// `CLONE_SETTLS`, without which `sys_clone` does not look at the tls
/// argument at all.
const SETTLS: u64 = 1 << 19;

/// `CLONE_PIDFD`, which is the one flag that makes `clone3` use its own
/// `pidfd` field instead of the `parent_tid` one.
const PIDFD: u64 = 1 << 12;

/// An exit signal in the low byte of the flags word, which is where legacy
/// `clone` carries it and where `clone3` refuses it.
const A_SIGNAL_IN_THE_FLAGS: u64 = 17;

/// What a slot holds before anything is written to it, so "left alone" cannot
/// pass for "written".
const A_SEED: i32 = -0x55;

/// `CLONE_THREAD` with the two flags Linux insists come with it, plus
/// `CLONE_CHILD_SETTID`. The one combination whose write this test can read
/// back, because a thread SHARES the address space it is written into, where
/// a fork gets a copy of it.
const A_THREAD_ASKING_FOR_ITS_TID: u64 = (1 << 16) | (1 << 8) | (1 << 11) | (1 << 24);

/// Something that is not zero, for the tail that has to be empty. It goes in
/// the LAST byte of the tail every time: a tail check that reads one byte, or
/// any number of bytes short of the whole, passes an empty-looking tail that
/// is not empty.
const A_TAIL_BYTE: u8 = 1;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A `struct clone_args` with room past version 0. `#[repr(C)]` so the tail
/// really does begin at byte 64 and nowhere else.
#[repr(C)]
struct Wide {
    args: [u64; 8],
    tail: [u8; 8],
}

impl Wide {
    /// A struct whose only fault is `flags`.
    fn asking(flags: u64) -> Self {
        Wide {
            args: [flags, 0, 0, 0, 0, 0, 0, 0],
            tail: [0; 8],
        }
    }

    fn asked(&mut self) -> UserInPtr<u64> {
        UserInPtr::from(self.args.as_mut_ptr() as usize)
    }
}

/// A pointer to nothing, built fresh each time: a `UserInPtr` is not `Copy`.
fn nowhere() -> UserInPtr<u64> {
    UserInPtr::from(0)
}

/// Make the call the way a program makes it: by number, through the table.
///
/// `Syscall::syscall` wants `&mut self` and the harness lends a shared one,
/// so this builds its own out of the same three parts. Same thread, so the
/// call lands in the same process.
async fn by_number(sys: &Syscall<'_>, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(crate::Sys::CLONE3 as u32, args).await
}

#[test]
fn the_size_is_judged_before_the_struct_is_read_and_out_of_range_answers_two_errnos() {
    // Three things at once, and the null pointer is what makes them
    // separable: a size this gate refuses must come back as the SIZE's
    // errno, and a size it accepts as the pointer's. Were the gate below
    // the read, every line here would be `EFAULT`.
    //
    // The two directions answer differently, and deliberately: too big is
    // `E2BIG` because the struct cannot be made sense of, too small is
    // `EINVAL` because it is not a `struct clone_args` at all.
    //
    // And there is NO zero quirk here. `sched_setattr`, which asks the very
    // same question of its own extensible struct, reads a zero as "version
    // 0" -- an ABI compatibility quirk it is stuck with. Copying that over
    // would make `clone3(NULL, 0)` a fork.
    let proc = a_process(47_180, "clone3-size");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_clone3(nowhere(), PAGE_SIZE + 1).await,
                Err(LxError::E2BIG),
                "a size past a page was not E2BIG, or the struct was read first"
            );
            assert_eq!(
                sys.sys_clone3(nowhere(), CLONE_ARGS_SIZE_VER0 - 1).await,
                Err(LxError::EINVAL),
                "a size short of version 0 was not EINVAL, or the struct was read first"
            );
            assert_eq!(
                sys.sys_clone3(nowhere(), 0).await,
                Err(LxError::EINVAL),
                "a size of zero was read as version 0, which is sched_setattr's quirk"
            );
            // The lower edge is inclusive: version 0 exactly is accepted, and
            // the EFAULT is the proof that the read was reached.
            assert_eq!(
                sys.sys_clone3(nowhere(), CLONE_ARGS_SIZE_VER0).await,
                Err(LxError::EFAULT),
                "version 0 exactly was refused as a size"
            );
        })
    });
}

#[test]
fn the_tail_past_version_zero_is_read_past_the_struct_and_has_to_be_empty() {
    // How an old kernel refuses a struct a new userspace filled in: a size
    // past version 0 is accepted only if every byte past version 0 is zero,
    // and `E2BIG` otherwise. Dropping the tail silently handed back a child
    // that was not the one asked for.
    //
    // The flags word is NOT zero in either case, which is what makes this
    // test say WHERE the tail is. Read from the start of the struct instead
    // of 64 bytes into it, the first eight bytes of the tail are the flags
    // word, so the empty tail would read as a full one and both lines below
    // would be `E2BIG`.
    //
    // The upper edge comes in at the end: a page exactly is a legal size,
    // with 4032 bytes of tail to be empty.
    let proc = a_process(47_181, "clone3-tail");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut empty = Wide::asking(A_NAMESPACE);
            assert_eq!(
                sys.sys_clone3(empty.asked(), CLONE_ARGS_SIZE_VER0 + 8)
                    .await,
                Err(LxError::EINVAL),
                "an empty tail stopped the call, or the tail was read over the struct"
            );

            let mut filled = Wide::asking(A_NAMESPACE);
            filled.tail[7] = A_TAIL_BYTE;
            assert_eq!(
                sys.sys_clone3(filled.asked(), CLONE_ARGS_SIZE_VER0 + 8)
                    .await,
                Err(LxError::E2BIG),
                "a field this kernel has never heard of was dropped silently"
            );

            let mut a_page = alloc::vec![0u64; PAGE_SIZE / 8];
            a_page[0] = A_NAMESPACE;
            let whole = UserInPtr::<u64>::from(a_page.as_mut_ptr() as usize);
            assert_eq!(
                sys.sys_clone3(whole, PAGE_SIZE).await,
                Err(LxError::EINVAL),
                "a page exactly was refused as a size, or its empty tail was not empty"
            );

            // And the whole of that tail is looked at: the one byte that is
            // not zero is the very last of the 4032.
            let last = a_page.len() - 1;
            a_page[last] = (A_TAIL_BYTE as u64) << 56;
            let whole = UserInPtr::<u64>::from(a_page.as_mut_ptr() as usize);
            assert_eq!(
                sys.sys_clone3(whole, PAGE_SIZE).await,
                Err(LxError::E2BIG),
                "the last byte of the tail was never read"
            );
        })
    });
}

#[test]
fn the_flags_word_reaches_sys_clone_which_refuses_a_namespace_this_kernel_has_not_got() {
    // The first of the eight words, followed to the end. `CLONE_NEWUSER` is
    // the one lever that tells the two halves apart: the decode NAMES the
    // bit, so it passes it on, and `sys_clone` refuses it, because this
    // kernel has no namespaces and answering success would be a lie about
    // isolation the child does not have.
    //
    // So the premise below and the answer after it cannot both come from the
    // same place. With the word lost on the way -- a `0` where `args.flags`
    // goes -- there is nothing left to refuse and the call FORKS.
    let words = [A_NAMESPACE, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        clone3_to_clone(words),
        Ok(Clone3Args {
            flags: A_NAMESPACE as usize,
            newsp: 0,
            parent_tid: 0,
            pidfd: 0,
            tls: 0,
            child_tid: 0,
        }),
        "the decode refuses CLONE_NEWUSER itself, so this proves nothing"
    );

    // And the other direction, because the decode's own refusals have to
    // arrive as well: an exit signal in the low byte of the flags word is
    // where legacy `clone` keeps it and `clone3` will not have it, so the
    // decode answers EINVAL -- and a swallowed error there FORKS.
    assert_eq!(
        clone3_to_clone([A_SIGNAL_IN_THE_FLAGS, 0, 0, 0, 0, 0, 0, 0]).err(),
        Some(LxError::EINVAL)
    );

    let proc = a_process(47_182, "clone3-flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut asking = Wide::asking(A_NAMESPACE);
            assert_eq!(
                sys.sys_clone3(asking.asked(), CLONE_ARGS_SIZE_VER0).await,
                Err(LxError::EINVAL),
                "the flags word did not reach the namespace gate"
            );

            let mut dirty = Wide::asking(A_SIGNAL_IN_THE_FLAGS);
            assert_eq!(
                sys.sys_clone3(dirty.asked(), CLONE_ARGS_SIZE_VER0).await,
                Err(LxError::EINVAL),
                "the decode refused the struct and the call forked anyway"
            );
        })
    });
}

/// `clone_tls_checked` is the x86_64 half of `copy_thread`; there is no such
/// check to reach on the other two.
#[cfg(target_arch = "x86_64")]
#[test]
fn the_tls_word_reaches_sys_clone_as_the_tls_and_answers_an_errno_only_it_can_give() {
    // The LAST of the eight words, which is the other thing a lost or
    // shuffled argument list would show. `CLONE_SETTLS` makes `sys_clone`
    // hold the tls to `arch_prctl`'s rule, and a pointer outside user space
    // fails the whole clone with `EPERM` -- an errno no part of `clone3`'s
    // own decoding answers, so it can only have come from there.
    //
    // It takes both ends of the array to produce: the flag is word 0 and the
    // address is word 7. Swap `args.tls` with `args.child_tid` in the call
    // and the tls is 0, which is inside user space, and this forks.
    use zircon_object::vm::{USER_ASPACE_BASE, USER_ASPACE_SIZE};
    let outside = (USER_ASPACE_BASE + USER_ASPACE_SIZE) as usize;
    let words = [SETTLS, 0, 0, 0, 0, 0, 0, outside as u64];
    assert_eq!(
        clone3_to_clone(words),
        Ok(Clone3Args {
            flags: SETTLS as usize,
            newsp: 0,
            parent_tid: 0,
            pidfd: 0,
            tls: outside,
            child_tid: 0,
        }),
        "the decode refuses this tls itself, so this proves nothing"
    );

    let proc = a_process(47_183, "clone3-tls");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut asking = Wide::asking(SETTLS);
            asking.args[7] = outside as u64;
            assert_eq!(
                sys.sys_clone3(asking.asked(), CLONE_ARGS_SIZE_VER0).await,
                Err(LxError::EPERM),
                "the tls word did not reach the clone's own check of it"
            );
        })
    });
}

#[test]
fn clone3_answers_from_its_own_number_with_the_pointer_first_and_the_size_second() {
    // What userspace actually has is a number. `Sys::CLONE3` is named in one
    // place in this crate outside this file, and until now no test went
    // through it: the arm could have gone back to answering `ENOSYS` -- which
    // it did for a while, from a stub ABOVE the live one -- and every test in
    // `clone3_tests` would still pass.
    //
    // The two lines also fix which argument is which, and they are a pair
    // for that reason: with `a0` and `a1` the other way round the first
    // becomes a size of 0 (`EINVAL`) and the second a size of some host
    // address (`E2BIG`), so each answers the other's errno.
    let proc = a_process(47_184, "clone3-number");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                by_number(sys, [0, PAGE_SIZE + 1, 0, 0, 0, 0]).await,
                -(LxError::E2BIG as isize),
                "clone3 by number did not answer its own size gate"
            );

            let mut asking = Wide::asking(A_NAMESPACE);
            let at = asking.asked().as_addr();
            assert_eq!(
                by_number(sys, [at, CLONE_ARGS_SIZE_VER0, 0, 0, 0, 0]).await,
                -(LxError::EINVAL as isize),
                "clone3 by number did not reach sys_clone with the struct"
            );
        })
    });
}

#[test]
fn a_clone_args_with_nothing_wrong_forks_and_the_child_is_the_callers() {
    // Every test above leaves one thing wrong, because that is the only way
    // to ask about the wiring without a child process at the end of it. This
    // one leaves nothing wrong, which is the one thing they cannot show: that
    // the chain ARRIVES. A `clone3` refused at every stage and a `clone3`
    // that never reaches the fork answer alike.
    //
    // The struct is all zeros, which for `clone3` is a plain fork asking for
    // no exit signal at all -- `exit_signal` has a field of its own here, and
    // a 0 in it means "tell the parent nothing", which legacy `clone` cannot
    // say, since a 0 in the low byte of its flags word is the same as not
    // asking. The zombie is still left for a `wait4` to find; only the
    // notification goes.
    //
    // The child's thread has nothing to run in this harness, so it ends as if
    // it had exited, and the `wait4` both proves the parentage and reaps it.
    let proc = a_process(47_185, "clone3-forks");
    let caller = proc.id();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut asking = Wide::asking(0);
            let pid = sys
                .sys_clone3(asking.asked(), CLONE_ARGS_SIZE_VER0)
                .await
                .expect("a clone_args with nothing wrong did not fork");
            assert_ne!(
                pid, 0,
                "clone3 answered the child's 0, not the parent's pid"
            );
            assert_ne!(pid as KoID, caller, "clone3 answered the caller's own pid");

            assert_eq!(
                sys.sys_wait4(pid as i32, UserOutPtr::from(0), 0, UserOutPtr::from(0))
                    .await,
                Ok(pid),
                "the pid clone3 answered is not a child of the caller"
            );
        })
    });
}

#[test]
fn the_pidfd_slot_is_its_own_field_and_the_descriptor_lands_in_it() {
    // The third difference `clone3` exists for, after the separate exit
    // signal and the stack given as base plus length: the pidfd gets a field
    // of its own, where legacy `clone` has to report it through the
    // `parent_tid` slot. So the decode carries word 1 apart from word 3, to
    // where the descriptor is handed over.
    //
    // Which makes this the one test that pins word 1, the `pidfd` argument:
    // taking word 3 instead -- the obvious mistake, since that is the word
    // legacy `clone` would have read -- is a write to a null pointer, i.e.
    // EFAULT instead of a child.
    let proc = a_process(47_186, "clone3-pidfd");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // The slot is built HERE and not outside the closure: a `move`
            // closure takes a copy of an `i32`, so an address taken outside
            // names the copy the test can no longer read.
            let mut slot: i32 = A_SEED;
            let at = &mut slot as *mut i32 as usize;
            assert_eq!(
                clone3_to_clone([PIDFD, at as u64, 0, 0, 0, 0, 0, 0]),
                Ok(Clone3Args {
                    flags: PIDFD as usize,
                    newsp: 0,
                    parent_tid: 0,
                    pidfd: at,
                    tls: 0,
                    child_tid: 0,
                }),
                "the decode does not move the pidfd field into the parent_tid slot"
            );

            let mut asking = Wide::asking(PIDFD);
            asking.args[1] = at as u64;
            let pid = sys
                .sys_clone3(asking.asked(), CLONE_ARGS_SIZE_VER0)
                .await
                .expect("a clone3 asking for a pidfd did not fork");

            // Seeded negative, so "never written" cannot pass for a
            // descriptor, and closable, so a number that is not one cannot
            // pass either.
            let fd = slot;
            assert_ne!(fd, A_SEED, "nothing was written to the pidfd slot");
            assert!(fd >= 0, "the pidfd slot got {} and not a descriptor", fd);
            assert_eq!(
                sys.sys_close((fd as usize).into()),
                Ok(0),
                "the number in the pidfd slot names no open file"
            );

            assert_eq!(
                sys.sys_wait4(pid as i32, UserOutPtr::from(0), 0, UserOutPtr::from(0))
                    .await,
                Ok(pid)
            );
        })
    });
}

#[test]
fn the_child_tid_word_reaches_sys_clone_and_the_new_task_writes_its_tid_there() {
    // Word 2, and the last of the eight this file can follow to the end.
    // `CLONE_CHILD_SETTID` is what makes `sys_clone` write to it at all, and
    // it has to write BEFORE the new task runs, because the parent's pthread
    // library reads the word immediately -- which is also what makes it
    // readable here.
    //
    // A thread and not a fork, because a fork writes into the CHILD's copy of
    // the page and there is no copy to read here: `fork_child_tid_tests` next
    // door asks that of `publish_child_tid` directly, with two address spaces
    // and a mapped page. A thread shares the one address space, so the word
    // the test holds IS the word the kernel writes.
    let proc = a_process(47_187, "clone3-ctid");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut word: i32 = A_SEED;
            let at = &mut word as *mut i32 as usize;
            assert_eq!(
                clone3_to_clone([A_THREAD_ASKING_FOR_ITS_TID, 0, at as u64, 0, 0, 0, 0, 0]),
                Ok(Clone3Args {
                    flags: A_THREAD_ASKING_FOR_ITS_TID as usize,
                    newsp: 0,
                    parent_tid: 0,
                    pidfd: 0,
                    tls: 0,
                    child_tid: at,
                }),
                "the decode does not carry word 2 as the child_tid"
            );

            let mut asking = Wide::asking(A_THREAD_ASKING_FOR_ITS_TID);
            asking.args[2] = at as u64;
            let tid = sys
                .sys_clone3(asking.asked(), CLONE_ARGS_SIZE_VER0)
                .await
                .expect("a clone3 asking for a thread did not make one");

            assert_ne!(
                tid as KoID,
                sys.thread.id(),
                "clone3 answered the caller's own tid"
            );
            assert_ne!(word, A_SEED, "nothing was written to the child_tid word");
            assert_eq!(
                word, tid as i32,
                "the child_tid word got something that is not the new thread's tid"
            );
        })
    });
}

#[test]
fn the_stack_words_are_read_and_a_sum_that_overflows_is_refused_and_not_a_panic() {
    // Words 5 and 6, and the reason they are worth a test of their own: the
    // child's stack pointer is `stack + stack_size`, both straight out of
    // userspace, and a plain `+` panics on a debug build. `clone3` with
    // `stack = u64::MAX` and `stack_size = 1` was a kernel panic any process
    // could ask for, and it sat there unreachable behind an `ENOSYS` stub
    // waiting for the day somebody deleted the stub -- which is since.
    //
    // `clone3_tests` fixes the arithmetic. What this adds is that the refusal
    // comes back as an errno through the call userspace makes, which is the
    // one thing a panic would not do.
    let proc = a_process(47_188, "clone3-stack");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut over = Wide::asking(0);
            over.args[5] = u64::MAX;
            over.args[6] = 1;
            assert_eq!(
                sys.sys_clone3(over.asked(), CLONE_ARGS_SIZE_VER0).await,
                Err(LxError::EINVAL),
                "stack + stack_size overflowed instead of being refused"
            );

            // And the other half of `clone3_stack_valid`: both zero, or
            // neither. A stack with no length is not a stack.
            let mut half = Wide::asking(0);
            half.args[5] = 0x7000_0000;
            assert_eq!(
                sys.sys_clone3(half.asked(), CLONE_ARGS_SIZE_VER0).await,
                Err(LxError::EINVAL),
                "a stack with no length was accepted"
            );
        })
    });
}
