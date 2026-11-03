//! Where the pieces of an i386 signal frame land.
//!
//! Numbers only: no page table, no writes. What the frame CONTAINS is pinned
//! by `linux_object::signal::ucontext`'s own tests; what is pinned here is
//! that the handler's arguments are where a cdecl function looks for them,
//! that the alignment is the one an SSE handler needs, and that the
//! `sigreturn` is found by the stack pointer it is really made with.

use super::*;

/// A stack top well inside a 32-bit process's address space.
const TOP: usize = 0xffff_d000;

#[test]
fn the_frame_is_the_header_the_siginfo_and_the_ucontext() {
    assert_eq!(FRAME32_SIZE, 16 + 128 + 116);
    assert_eq!(
        FRAME32_SIZE, 260,
        "and 260 bytes is what the writable-memory check must ask about"
    );
}

#[test]
fn the_handlers_arguments_are_where_a_cdecl_function_looks() {
    let f = Frame32::at(TOP).unwrap();
    // `esp` points at the return address, so argument 1 is at `esp + 4`,
    // argument 2 at `esp + 8` and argument 3 at `esp + 12` -- which is what
    // the header's four words are.
    assert_eq!(f.siginfo_ptr, f.sp + 16, "the info follows the header");
    assert_eq!(f.uctx_ptr, f.siginfo_ptr + 128);
    assert_eq!(
        f.uctx_ptr + 116,
        f.sp + FRAME32_SIZE,
        "and nothing after it"
    );
}

#[test]
fn the_frame_is_aligned_so_that_the_handlers_own_stack_is() {
    // `((sp + 4) & -16) - 4`: what must be 16-byte aligned is `esp + 4`, the
    // stack as the handler's body sees it once its return address is pushed.
    // A handler built with SSE addresses its locals with `movaps`, which
    // faults on a misaligned address -- inside the handler, with the signal
    // already delivered and no way to report where it happened.
    for top in TOP - 15..=TOP {
        let f = Frame32::at(top).unwrap();
        assert_eq!((f.sp + 4) % 16, 0, "top={top:#x}");
        assert!(f.sp + FRAME32_SIZE <= top, "the frame stays below the top");
    }
}

#[test]
fn the_frame_never_reaches_above_the_top_it_was_given() {
    // The red zone below the interrupted stack pointer is the caller's to
    // skip; what this must not do is round UP over it.
    let f = Frame32::at(TOP).unwrap();
    assert!(f.sp < TOP);
    assert!(TOP - f.sp >= FRAME32_SIZE);
    assert!(
        TOP - f.sp < FRAME32_SIZE + 16,
        "and no more than one alignment step of slack"
    );
}

#[test]
fn a_stack_pointer_too_low_to_hold_a_frame_is_refused() {
    // Not wrapped round to the top of the address space, which would be the
    // kernel writing a signal frame wherever the arithmetic landed.
    assert_eq!(Frame32::at(0), None);
    assert_eq!(Frame32::at(FRAME32_SIZE - 1), None);
    // Exactly the frame's size is still not enough: the alignment step needs
    // twelve bytes of headroom under it, so this refuses rather than
    // wrapping to the top of the address space.
    assert_eq!(Frame32::at(FRAME32_SIZE), None);
    assert_eq!(Frame32::at(FRAME32_SIZE + 11), None);
    let f = Frame32::at(FRAME32_SIZE + 12).unwrap();
    assert_eq!((f.sp, f.sp + FRAME32_SIZE), (12, FRAME32_SIZE + 12));
}

#[test]
fn the_sigreturn_is_found_four_bytes_up_and_not_eight() {
    // The saved context is keyed on the stack pointer the `sigreturn` is
    // really made with. The i386 `__restore_rt` pops nothing, so only the
    // four bytes the handler's `ret` took are gone -- the native path's
    // eight would look the frame up at an address no `sigreturn` ever
    // arrives with, and every `sigreturn` would be EINVAL: a handler that
    // returns kills its process.
    let f = Frame32::at(TOP).unwrap();
    assert_eq!(f.sigreturn_sp(), f.sp + 4);
    assert_ne!(f.sigreturn_sp(), f.sp + 8);
}
