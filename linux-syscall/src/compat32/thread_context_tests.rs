//! What a **thread** of a 32-bit process starts with.
//!
//! `clone_tests` pins which argument the thread pointer comes out of and what
//! it means. This file pins the other half, the one the probe's step 91
//! measures end to end: the context the new thread is handed. Three things
//! have to be in it, and each of them is invisible from the caller until the
//! thread's first instruction runs:
//!
//! - **the stack it was given**, which is the whole difference between this
//!   call and the fork of step 88 (`newsp` was zero there);
//! - **`compat32`**, the bit `iret` reads to return to ring 3 in
//!   compatibility mode. A thread of a 32-bit process that lost it would
//!   decode its own first instruction as a 64-bit one;
//! - **its own thread area**, installed in the slot its `user_desc` names ---
//!   and NOT in the parent's context, the descriptor tables being per-thread.
//!
//! Asked of [`Syscall::thread_start_context`] rather than by starting a
//! thread: a started thread runs the caller's context on the host's executor,
//! which in a test is the harness itself. The running claim belongs to the
//! probe (`tools/i386-hello`, step 91), which does it for real on Eclipse.
//!
//! Fixed pids live in 48_62x.

use super::clone_tests::{a_user_desc, FIRST_ENTRY};
use super::*;
use crate::on_a_thread::on_a_thread;
use crate::task::CloneFlags;
use alloc::boxed::Box;
use kernel_hal::context::UserContextField;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};

/// i386's `set_thread_area`, which is how the PARENT gets its own area ---
/// the real path, so that what the child's is compared against is a
/// descriptor this kernel wrote and not one a test built by hand.
const I386_SET_THREAD_AREA: u32 = 243;

/// A stack a 32-bit thread could really be given: 16-aligned and below
/// 4 GiB, as every ABI wants one --- and **not** page-aligned, which it
/// never is in practice: `pthread_create` hands over the top of a mapping
/// minus the bytes it kept for itself. Page-aligned it was, and a mutation
/// that rounded the stack down to its page passed the test; a stack that
/// says what it is is the one that measures something.
const NEWSP: usize = 0x0805_0c40;

/// `CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_SETTLS`, which is what
/// `pthread_create` passes, minus the TID bookkeeping that does not reach the
/// context.
fn thread_flags() -> CloneFlags {
    CloneFlags::THREAD | CloneFlags::SIGHAND | CloneFlags::VM | CloneFlags::SETTLS
}

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "thread32", lp).unwrap()
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// The base address a GDT data descriptor carries, which is where a 32-bit
/// thread's `%gs:0` lands: bits 16..40 and 56..64 of the eight bytes.
///
/// Spelled out here because the point of the test is that the base the
/// descriptor was ASKED for is the base the child's slot holds --- comparing
/// the eight bytes against a constant would pass just as well for a kernel
/// that stored them somewhere else.
fn base_of(desc: u64) -> u32 {
    (((desc >> 16) & 0x00ff_ffff) | (((desc >> 56) & 0xff) << 24)) as u32
}

/// The base `a_user_desc` asks for, and another one for the parent, so that
/// "the child's area" and "the parent's area" cannot be confused.
const CHILD_BASE: u32 = 0x0804_f000;
const PARENT_BASE: u32 = 0x0804_e000;

/// `a_user_desc` with a base of its own.
fn a_user_desc_based(entry: u32, base: u32) -> [u8; 16] {
    let mut b = a_user_desc(entry);
    b[4..8].copy_from_slice(&base.to_ne_bytes());
    b
}

/// **A 32-bit thread starts on the stack it was given, in compatibility
/// mode, with its own thread area.**
#[test]
fn a_32_bit_thread_starts_with_its_stack_its_mode_and_its_own_area() {
    let parent_desc = a_user_desc_based(u32::MAX, PARENT_BASE); // -1: let the kernel pick
    let child_desc = a_user_desc_based(FIRST_ENTRY, CHILD_BASE);
    let parent_at = parent_desc.as_ptr() as usize;
    let child_at = child_desc.as_ptr() as usize;
    let proc = a_process(48_620, Abi::Linux32);
    let (mut child_ctx, parent_slot_after) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            // The parent is a 32-bit thread: in compatibility mode, with an
            // area of its own, both the way a real one gets them.
            sys.thread
                .with_context(|c| c.set_compat32(true))
                .expect("a context of its own");
            let installed = sys
                .compat32_syscall(I386_SET_THREAD_AREA, [parent_at, 0, 0, 0, 0, 0])
                .await;
            assert_eq!(installed, 0, "set_thread_area answered {}", installed);

            let child_ctx = as_mut(&sys)
                .thread_start_context(thread_flags(), NEWSP, child_at)
                .expect("a descriptor the kernel accepts");
            let parent_slot_after = sys
                .thread
                .with_context(|c| c.compat_tls(0))
                .expect("a context of its own");
            (child_ctx, parent_slot_after)
        })
    });

    assert_eq!(
        child_ctx.get_field(UserContextField::StackPointer),
        NEWSP,
        "the thread was not given the stack it asked for"
    );
    assert_eq!(
        child_ctx.get_field(UserContextField::ReturnValue),
        0,
        "a clone answers 0 in the child"
    );
    assert!(
        child_ctx.is_compat32(),
        "the thread of a 32-bit process lost `compat32`: it would come back \
         to ring 3 in 64-bit mode"
    );
    let child_slot = child_ctx.compat_tls(0).expect("slot zero exists");
    assert_eq!(
        base_of(child_slot),
        CHILD_BASE,
        "the thread's own slot does not hold the base its `user_desc` named"
    );
    // And the parent's area is still the parent's: the tables are per-thread,
    // which is what lets two threads of one process read different words
    // through the same selector.
    assert_eq!(
        base_of(parent_slot_after.expect("the parent has one too")),
        PARENT_BASE,
        "building the child's context changed the PARENT's thread area"
    );
    proc.exit(0);
}

/// **And a native thread is handed a native context**: no `compat32`, and the
/// thread pointer is the word itself.
///
/// The pair that makes the test above measure something: a kernel that had
/// simply turned compatibility mode on for everyone would pass it.
#[test]
fn a_native_thread_is_not_in_compatibility_mode() {
    let tp: u64 = 0;
    let at = &tp as *const u64 as usize;
    let proc = a_process(48_621, Abi::Linux);
    let mut ctx = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            as_mut(sys)
                .thread_start_context(thread_flags(), NEWSP, at)
                .expect("a thread pointer inside the user half")
        })
    });
    assert_eq!(ctx.get_field(UserContextField::StackPointer), NEWSP);
    assert!(
        !ctx.is_compat32(),
        "a native thread must not be started in compatibility mode"
    );
    assert_eq!(
        ctx.get_field(UserContextField::ThreadPointer),
        at,
        "a native `CLONE_SETTLS` word IS the thread pointer, not a pointer to \
         a descriptor"
    );
    proc.exit(0);
}
