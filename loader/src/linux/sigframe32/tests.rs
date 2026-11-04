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

/// **The old frame is the one Linux's `struct sigframe_ia32` is**, member by
/// member.
///
/// The kernel is the only thing that knows where `extramask` is --- the
/// trampoline on the other side only does `esp - 8` arithmetic --- so getting
/// the `_fpstate_32` size wrong would not show up as a crash: the high half
/// of every restored mask would come out of the middle of a hole, and a
/// program that blocks a real-time signal around a handler would silently
/// come back with it unblocked.
#[test]
fn the_old_frame_is_the_sigframe_ia32_linux_pushes() {
    assert_eq!(
        OLD_FRAME32_SIZE,
        8 + 88 + 624 + 4 + 8,
        "pretcode and sig, the sigcontext_32, the _fpstate_32 nobody reads, \
         extramask, retcode"
    );
    assert_eq!(OLD_FRAME32_SIZE, 732);
    let f = OldFrame32::at(TOP).unwrap();
    assert_eq!(f.sc_ptr, f.sp + 8, "the sigcontext follows the two words");
    assert_eq!(
        f.extramask_ptr(),
        f.sc_ptr + 88 + 624,
        "and the high half of the mask follows the fpstate"
    );
    assert_eq!(
        f.extramask_ptr() + 4 + 8,
        f.sp + OLD_FRAME32_SIZE,
        "retcode is the last member"
    );
}

/// **It is the BIGGER of the two frames**, which is what the writable-memory
/// check has to be told.
///
/// Asking about the `rt_` frame's 260 bytes and then writing 732 would put
/// the `_fpstate_32` and the `extramask` of a frame 472 bytes past the last
/// address that was checked --- off the end of a stack mapping that has just
/// enough room for the frame the kernel asked about.
#[test]
fn the_old_frame_is_the_larger_of_the_two() {
    assert!(OLD_FRAME32_SIZE > FRAME32_SIZE);
    assert_eq!(OLD_FRAME32_SIZE - FRAME32_SIZE, 472);
}

/// **An old frame's `sigreturn` is found EIGHT bytes up, not four.**
///
/// The two trampolines differ by one instruction: `__restore_rt` pops
/// nothing, `__restore` pops the signal number before the `int $0x80`. So the
/// two frames' `sigreturn` stack pointers are four bytes apart, and the saved
/// context is keyed on that pointer: getting this one wrong by four bytes
/// makes every `sigreturn` from an old frame `EINVAL`, which is a handler
/// that kills its process on the way out.
#[test]
fn the_old_sigreturn_is_found_eight_bytes_up() {
    let f = OldFrame32::at(TOP).unwrap();
    assert_eq!(f.sigreturn_sp(), f.sp + 8);
    assert_eq!(
        f.sigreturn_sp(),
        f.sc_ptr,
        "which is &frame->sc, where Linux's regs->sp - 8 puts the frame base"
    );
    let rt = Frame32::at(TOP).unwrap();
    assert_ne!(
        rt.sigreturn_sp() - rt.sp,
        f.sigreturn_sp() - f.sp,
        "and the two cannot be confused for each other"
    );
}

/// The alignment and the refusal, the same two properties the `rt_` frame's
/// own tests pin, for the same two reasons.
#[test]
fn the_old_frame_is_aligned_and_a_stack_too_low_is_refused() {
    for top in TOP - 15..=TOP {
        let f = OldFrame32::at(top).unwrap();
        assert_eq!((f.sp + 4) % 16, 0, "top={top:#x}");
        assert!(f.sp + OLD_FRAME32_SIZE <= top);
    }
    assert_eq!(OldFrame32::at(0), None);
    assert_eq!(OldFrame32::at(OLD_FRAME32_SIZE), None);
    assert_eq!(OldFrame32::at(OLD_FRAME32_SIZE + 11), None);
    assert!(OldFrame32::at(OLD_FRAME32_SIZE + 12).is_some());
}

/// **The mask goes over in two pieces**, and `write_old_frame` is where it is
/// split.
///
/// Into a buffer of this process's own, which is all the frame writer needs:
/// it takes addresses, and a host test can give it ones it owns.
#[test]
fn the_mask_is_split_between_oldmask_and_extramask() {
    use linux_object::signal::Sigset;
    // One real-time signal (bit 40) and one ordinary one, so both halves
    // carry something and neither can pass for the other.
    let mask = Sigset::new(0x0000_0100_0000_0002);
    let mut buf = alloc::vec![0u8; OLD_FRAME32_SIZE + 16];
    let base = (buf.as_mut_ptr() as usize + 15) & !0xf;
    let frame = OldFrame32 {
        sp: base,
        sc_ptr: base + 8,
    };
    let sc = SigContext32::default();
    // SAFETY: `base` is the start of a buffer of this process's own with
    // room for a whole frame above it.
    #[allow(unsafe_code)]
    unsafe {
        write_old_frame(frame, 0xdead_beef, 10, &sc, mask);
    }
    #[allow(unsafe_code)]
    let (pretcode, sig, oldmask, extramask, retcode) = unsafe {
        (
            *(frame.sp as *const u32),
            *((frame.sp + 4) as *const u32),
            (*(frame.sc_ptr as *const SigContext32)).oldmask,
            *(frame.extramask_ptr() as *const u32),
            *((frame.extramask_ptr() + 4) as *const [u8; 8]),
        )
    };
    assert_eq!(pretcode, 0xdead_beef, "the handler's return address");
    assert_eq!(sig, 10, "its one argument");
    assert_eq!(oldmask, 0x0000_0002, "the low 32 bits of the mask");
    assert_eq!(extramask, 0x0000_0100, "and the high 32 behind the fpstate");
    assert_eq!(
        retcode, OLD_RETCODE,
        "popl %eax; movl $119, %eax; int $0x80"
    );
}

/// **An action without `SA_SIGINFO` is sized for the frame it is going to
/// get.**
///
/// The writable-memory check and the write read the size from the same
/// function, which is the point: a check made about 260 bytes followed by a
/// write of 732 puts the `_fpstate_32` and the `extramask` off the end of a
/// stack mapping that had just enough room for the frame the kernel asked
/// about -- and the kernel is the one writing there.
#[test]
fn the_size_asked_about_is_the_size_of_the_frame_that_will_be_written() {
    assert_eq!(frame32_size(true), FRAME32_SIZE);
    assert_eq!(frame32_size(false), OLD_FRAME32_SIZE);
    assert!(
        frame32_size(false) > frame32_size(true),
        "and the one a plain handler gets is the bigger: a check made about \
         the other size is short by 472 bytes"
    );
}
