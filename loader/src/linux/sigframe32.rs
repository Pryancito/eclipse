//! Putting a `struct rt_sigframe_ia32` on a 32-bit process's stack.
//!
//! The native frame is three pushes: the `siginfo`, the `ucontext`, and the
//! restorer's address as the handler's return address. The i386 frame is one
//! structure, because the handler's arguments are **in** it --- i386 passes
//! arguments on the stack --- so the four words of the header have to end up
//! immediately below the `siginfo`, and `esp` has to point at the first of
//! them. See [`linux_object::signal::ucontext::i386`] for the layout itself.
//!
//! The arithmetic is here, in functions that take and return numbers, so that
//! a host test can check where every piece lands without a user page table or
//! a write to address 0x0804_8000. [`write_frame`] is the only part that
//! touches memory, and it only places the pieces [`Frame32::at`] worked out.

use core::mem::size_of;
use linux_object::signal::ucontext::i386::{RtSigFrameHeader32, SignalUserContext32};
use linux_object::signal::{SigInfo, SIGINFO_LEN};

/// How many bytes of stack one frame takes: the header, the `compat_siginfo`
/// and the `ucontext_ia32`.
pub const FRAME32_SIZE: usize =
    size_of::<RtSigFrameHeader32>() + SIGINFO_LEN + size_of::<SignalUserContext32>();

/// Where each piece of one frame goes, and the two addresses the kernel has
/// to remember afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame32 {
    /// What `esp` is when the handler starts: the first word of the header,
    /// which the handler reads as its own return address.
    pub sp: usize,
    /// `&frame->info`, the handler's second argument.
    pub siginfo_ptr: usize,
    /// `&frame->uc`, its third.
    pub uctx_ptr: usize,
}

impl Frame32 {
    /// Lay a frame out below `frame_top`.
    ///
    /// The alignment is Linux's `get_sigframe()` for ia32:
    /// `sp = ((sp + 4) & -16) - 4`. Not "align the frame to 16": align it so
    /// that **`esp + 4` is**, because the i386 ABI's promise is that a
    /// function's stack is 16-byte aligned *after* its return address has
    /// been pushed. A handler compiled with SSE does `movaps` to its own
    /// locals off that, and a frame aligned the other way faults on the
    /// first one --- inside the handler, with the signal already delivered.
    ///
    /// `None` when there is not that much stack below `frame_top` to begin
    /// with, which the caller answers the way Linux does: a forced SIGSEGV.
    pub fn at(frame_top: usize) -> Option<Self> {
        let base = frame_top.checked_sub(FRAME32_SIZE)?;
        let sp = (base.checked_add(4)? & !0xfusize).checked_sub(4)?;
        Some(Self {
            sp,
            siginfo_ptr: sp + size_of::<RtSigFrameHeader32>(),
            uctx_ptr: sp + size_of::<RtSigFrameHeader32>() + SIGINFO_LEN,
        })
    }

    /// The stack pointer the `sigreturn` of this frame is made with, which is
    /// how the saved context is found again.
    ///
    /// The i386 `__restore_rt` trampoline is `movl $173, %eax; int $0x80` and
    /// pops nothing, so by the time the kernel sees the call the only thing
    /// off the frame is the `pretcode` the handler's `ret` took: four bytes,
    /// not eight. Linux reads the frame back at `regs->sp - 4` for exactly
    /// this reason.
    pub fn sigreturn_sp(self) -> usize {
        self.sp + size_of::<u32>()
    }
}

/// Write one frame into the process's memory.
///
/// # Safety
///
/// The user page table must be live and `[frame.sp, frame.sp + FRAME32_SIZE)`
/// must be inside one writable user mapping --- which is what the caller's
/// `frame_fits_writable` check is for. Everything written is a plain
/// `repr(C)` structure with no padding of its own.
#[allow(unsafe_code)]
pub unsafe fn write_frame(
    frame: Frame32,
    restorer: usize,
    signo: u32,
    info: &SigInfo,
    uc: &SignalUserContext32,
) {
    let header = RtSigFrameHeader32 {
        // The handler's `ret` goes here, which is the whole reason libc
        // passes `SA_RESTORER`: there is no other way back into the kernel.
        pretcode: restorer as u32,
        sig: signo,
        pinfo: frame.siginfo_ptr as u32,
        puc: frame.uctx_ptr as u32,
    };
    (frame.sp as *mut RtSigFrameHeader32).write(header);
    // The `siginfo` goes over as the 128 bytes a 32-bit program reads, which
    // is not the native struct: its union starts four bytes earlier and its
    // pointer members are half as wide.
    (frame.siginfo_ptr as *mut [u8; SIGINFO_LEN]).write(info.to_i386_bytes());
    (frame.uctx_ptr as *mut SignalUserContext32).write(*uc);
}

#[cfg(test)]
mod tests;
