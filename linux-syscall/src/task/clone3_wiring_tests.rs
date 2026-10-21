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
//! FORKS, and a unit test has no use for a child process. So each test here
//! leaves exactly one thing wrong, and picks a wrong thing whose errno no
//! other stage can answer:
//!
//!  * `CLONE_NEWUSER` is a bit this kernel NAMES, so `clone3_to_clone`
//!    hands it on, and REFUSES, in `sys_clone`'s namespace gate. A struct
//!    whose only fault is that bit separates "the flags word arrived" from
//!    "the flags word was decoded".
//!  * `CLONE_SETTLS` with a tls outside user space is **`EPERM`**, and
//!    `EPERM` is an errno nothing in `clone3`'s own decoding can produce.
//!    It pins the LAST of the eight words as well as the first.
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

/// Something that is not zero, for the tail that has to be empty.
const A_TAIL_WORD: u64 = 1;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// A `struct clone_args` with room past version 0. `#[repr(C)]` so the tail
/// really does begin at byte 64 and nowhere else.
#[repr(C)]
struct Wide {
    args: [u64; 8],
    tail: [u64; 1],
}

impl Wide {
    /// A struct whose only fault is `flags`.
    fn asking(flags: u64) -> Self {
        Wide {
            args: [flags, 0, 0, 0, 0, 0, 0, 0],
            tail: [0],
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
            filled.tail[0] = A_TAIL_WORD;
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
            parent_slot: 0,
            tls: 0,
            child_tid: 0,
        }),
        "the decode refuses CLONE_NEWUSER itself, so this proves nothing"
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
            parent_slot: 0,
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
