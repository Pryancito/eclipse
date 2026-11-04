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
use linux_object::signal::ucontext::i386::{
    OldSigFrameHeader32, RtSigFrameHeader32, SigContext32, SignalUserContext32, FPSTATE32_LEN,
};
use linux_object::signal::{SigInfo, Sigset, SIGINFO_LEN};

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

/// How many bytes of stack an **old** frame takes: the two header words, the
/// `sigcontext_32`, the `_fpstate_32` nobody uses, the high half of the mask,
/// and the eight bytes of `retcode`.
///
/// It is the BIGGER of the two frames --- 732 bytes against 260 --- which is
/// only surprising until you remember what the `rt_` one does with its
/// floating-point state: it keeps a pointer to it, while this one carries a
/// whole `struct _fpstate_32` it then ignores.
pub const OLD_FRAME32_SIZE: usize = size_of::<OldSigFrameHeader32>()
    + size_of::<SigContext32>()
    + FPSTATE32_LEN
    + size_of::<u32>()
    + 8;

/// Where the pieces of one **old** (non-`SA_SIGINFO`) frame go.
///
/// `struct sigframe_ia32`, which is the frame Linux pushes for an action
/// without `SA_SIGINFO` --- and that is most of them: musl's i386
/// `__libc_sigaction` points `sa_restorer` at `__restore`, a plain
/// `sigreturn`, for every handler that did not ask for a `siginfo`. So this
/// is the frame a 32-bit program gets from `signal(3)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldFrame32 {
    /// What `esp` is when the handler starts: its return address, with the
    /// signal number as its one argument just above.
    pub sp: usize,
    /// `&frame->sc`, which is **not** a pointer the handler is given: an old
    /// frame has no `ucontext` and no third argument. It is where the kernel
    /// writes the interrupted registers and where its own `sigreturn` reads
    /// them back.
    pub sc_ptr: usize,
}

impl OldFrame32 {
    /// Lay an old frame out below `frame_top`, aligned as [`Frame32::at`]
    /// aligns the other one, and for the same reason.
    pub fn at(frame_top: usize) -> Option<Self> {
        let base = frame_top.checked_sub(OLD_FRAME32_SIZE)?;
        let sp = (base.checked_add(4)? & !0xfusize).checked_sub(4)?;
        Some(Self {
            sp,
            sc_ptr: sp + size_of::<OldSigFrameHeader32>(),
        })
    }

    /// Where the high half of the mask sits: behind the `_fpstate_32`.
    pub fn extramask_ptr(self) -> usize {
        self.sc_ptr + size_of::<SigContext32>() + FPSTATE32_LEN
    }

    /// The stack pointer this frame's `sigreturn` is made with.
    ///
    /// **Eight** bytes above `sp`, not four: musl's `__restore` is
    /// `popl %eax; movl $119, %eax; int $0x80`, so the handler's `ret` takes
    /// the `pretcode` and the trampoline's `popl` takes the signal number.
    /// Linux reads the frame back at `regs->sp - 8` for exactly this reason,
    /// and that difference from the `rt_` frame's four is what makes a
    /// `sigreturn` aimed at the wrong frame miss its saved context instead of
    /// restoring one read at the wrong offsets.
    pub fn sigreturn_sp(self) -> usize {
        self.sp + size_of::<OldSigFrameHeader32>()
    }
}

/// Write one **old** frame into the process's memory.
///
/// The mask is split the way `sys_sigreturn` reads it back: its low 32 bits
/// into `sc.oldmask` and its high 32 into `extramask`.
///
/// # Safety
///
/// As [`write_frame`]: the user page table must be live and
/// `[frame.sp, frame.sp + OLD_FRAME32_SIZE)` must lie inside one writable
/// user mapping.
#[allow(unsafe_code)]
pub unsafe fn write_old_frame(
    frame: OldFrame32,
    restorer: usize,
    signo: u32,
    sc: &SigContext32,
    mask: Sigset,
) {
    (frame.sp as *mut OldSigFrameHeader32).write(OldSigFrameHeader32 {
        pretcode: restorer as u32,
        sig: signo,
    });
    let mut sc = *sc;
    sc.oldmask = mask.val() as u32;
    (frame.sc_ptr as *mut SigContext32).write(sc);
    // The `_fpstate_32` Linux calls unused really is: `sc.fpstate` is zero,
    // which is how a frame says it carries none, and a handler that reads the
    // member rather than the pointer gets the same zeros Linux leaves there.
    ((frame.sc_ptr + size_of::<SigContext32>()) as *mut [u8; FPSTATE32_LEN])
        .write([0u8; FPSTATE32_LEN]);
    (frame.extramask_ptr() as *mut u32).write((mask.val() >> 32) as u32);
    // And the trampoline Linux puts in every old frame for a program that
    // passed no `SA_RESTORER`: `popl %eax; movl $119, %eax; int $0x80`, which
    // is eight bytes to the byte. Nothing here jumps to it --- `pretcode` is
    // the restorer, as on the `rt_` path --- but a frame without it would not
    // be the structure it claims to be, and `retcode` is the last member.
    ((frame.extramask_ptr() + size_of::<u32>()) as *mut [u8; 8]).write(OLD_RETCODE);
}

/// How much stack the frame an action asks for takes.
///
/// The one place the choice between i386's two frames is made, so that the
/// writable-memory check and the write itself cannot disagree about it: they
/// read it from here. Asking about the `rt_` frame's 260 bytes and then
/// writing the old frame's 732 would put its `_fpstate_32` and its
/// `extramask` 472 bytes past the last address that was checked.
pub fn frame32_size(siginfo: bool) -> usize {
    if siginfo {
        FRAME32_SIZE
    } else {
        OLD_FRAME32_SIZE
    }
}

/// `popl %eax; movl $119, %eax; int $0x80`: the `sigreturn` trampoline Linux
/// lays in an old frame's `retcode`, and the sequence musl's `__restore` is
/// made of.
const OLD_RETCODE: [u8; 8] = [0x58, 0xb8, 0x77, 0x00, 0x00, 0x00, 0xcd, 0x80];

#[cfg(test)]
mod tests;
