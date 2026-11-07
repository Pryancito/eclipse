//! Switch context by function call within the same privilege level, on a
//! Linux host.
//!
//! # The trip out
//!
//! The guest's registers go out through the host's `rt_sigreturn`:
//! [`UserContext::run_fncall`] lays a signal frame on the kernel stack with
//! the guest's general registers, stack pointer, program counter, flags and
//! floating-point state in it, and `syscall_fn_return` asks the host to
//! return from a signal into it. The host loads the lot in one go and the
//! guest is running, exactly as it would be after a real signal handler
//! returned.
//!
//! # The way back
//!
//! There is none the guest can take itself. Every transition back is a host
//! signal -- `SIGSYS` for a `svc` the seccomp filter trapped, `SIGSEGV`,
//! `SIGBUS`, `SIGILL`, `SIGTRAP`, `SIGFPE` for a fault, `SIGALRM` for the end
//! of a time slice -- and the signal handler (`kernel_hal::fault_bridge`)
//! calls [`UserContext::take_from_host`] on the host's signal context: the
//! guest's registers are copied out of it and the context is pointed at
//! `fault_fn_entry` on the kernel stack `syscall_fn_return` left, so the
//! host's own return from the handler lands in the kernel. See `fncall.S`.
//!
//! # Why not the old way
//!
//! The previous version was the rCore scheme: the guest called
//! `syscall_fn_entry` with a `bl`, and the entry found the kernel's stack
//! through `tpidr_el0 + 48`. That trusted a register the guest can write
//! with one unprivileged `msr`, and it had nothing at all for a guest that
//! faulted: the host signal killed kernel, guest and harness together. It
//! also needed a vDSO rebuilt with `bl` in place of `svc`, whose stubs are
//! not the real vDSO's size, which breaks the Zircon core tests that find
//! internal syscall stubs by their distance from a public one.

use super::{FpRegs, GeneralRegs, UserContext};
use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};

global_asm!(include_str!("fncall.S"));

extern "C" {
    /// The old entry for a guest that `bl`s into the kernel instead of
    /// executing `svc`: a `brk` the host reports as `SIGTRAP` with the
    /// program counter exactly here, which the fault bridge translates into
    /// a system call with the return address for its `elr`. Only the hosted
    /// Linux personality still reaches it, through the `rcore_syscall_entry`
    /// it patches into musl; the Zircon guests boot the real vDSO.
    pub fn syscall_fn_entry();

    /// Out to the guest. `frame` is the signal frame `run_fncall` laid out.
    fn syscall_fn_return(regs: &mut UserContext, frame: *const u8);

    /// The way back into the kernel for a guest that trapped.
    ///
    /// Not callable from Rust: it is where [`UserContext::take_from_host`]
    /// points the host's signal context, with the stack pointer at the
    /// callee-saved registers `syscall_fn_return` parked.
    pub fn fault_fn_entry();
}

/// `rt_sigreturn`, `sigaltstack` and `rt_sigprocmask` on aarch64 Linux: the
/// generic syscall table.
const SYS_SIGALTSTACK: usize = 132;
const SYS_RT_SIGPROCMASK: usize = 135;

/// The magic numbers of the records in a signal context's `__reserved`
/// area (`arch/arm64/include/uapi/asm/sigcontext.h`).
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const ESR_MAGIC: u32 = 0x4553_5201;

/// The bits of `pstate` a user context may carry through `rt_sigreturn`:
/// the condition flags. The host refuses a frame with any of `DAIF`, the
/// exception level or the execution state set (`valid_user_regs`), and a
/// guest never has them.
const PSTATE_NZCV: u64 = 0xf000_0000;

/// The host's `struct sigcontext`, which is also glibc's `mcontext_t`: the
/// registers a signal handler is handed and `rt_sigreturn` reads back, in
/// the order the host lays them. The `__reserved` area holds a chain of
/// tagged records, each a 16-byte header (`magic`, `size`) and its payload,
/// ended by a header of zeros; the floating-point state is the
/// [`FPSIMD_MAGIC`] record, the syndrome of a fault the [`ESR_MAGIC`] one.
#[repr(C, align(16))]
pub struct HostSigContext {
    /// The faulting address, for a `SIGSEGV` or `SIGBUS` the host took as a
    /// memory fault.
    pub fault_address: u64,
    /// `x0`-`x30`.
    pub regs: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub pstate: u64,
    /// The host aligns `__reserved` to 16 bytes as a FIELD (`__aligned__(16)`
    /// on the array), and a byte array has no alignment of its own in Rust:
    /// without this word the records started eight bytes early and the host
    /// read a header of zeros where the first record's should be, so every
    /// `rt_sigreturn` was a bad frame (`SIGSEGV`, `SEGV_ACCERR`, at the
    /// guest's own `sp`, with `x0` zero).
    _pad: u64,
    pub reserved: [u8; 4096],
}

/// The `fpsimd_context` record: `fpsr`, `fpcr` and the thirty-two vector
/// registers, as the host writes it first in `__reserved`.
#[repr(C, align(16))]
struct FpsimdRecord {
    magic: u32,
    size: u32,
    fpsr: u32,
    fpcr: u32,
    vregs: [u64; 64],
}

/// The `esr_context` record.
#[repr(C, align(16))]
struct EsrRecord {
    magic: u32,
    size: u32,
    esr: u64,
}

/// The host's `struct rt_sigframe`, which is what `rt_sigreturn` wants the
/// stack pointer at: the `siginfo` (unused on the way back), then the
/// `ucontext` with the registers in its `uc_mcontext`.
#[repr(C, align(16))]
struct SigFrame {
    info: [u8; 128],
    uc_flags: u64,
    uc_link: u64,
    /// `stack_t`: `ss_sp`, `ss_flags` (and its padding), `ss_size`. The host
    /// restores the thread's alternate stack from it, and refuses the frame
    /// if it is not a stack it would accept.
    uc_stack: [u64; 3],
    /// The signal mask to put back: the host's own `sigset_t` is one word.
    uc_sigmask: u64,
    unused: [u8; 120],
    mcontext: HostSigContext,
}

/// The offset of `uc_mcontext` inside the host's `rt_sigframe`:
/// `sizeof(siginfo_t)` plus the `ucontext` header rounded up to the
/// alignment of `sigcontext`.
const MCONTEXT_OFFSET: usize = 128 + 176;
const _: () = assert!(offset_of!(SigFrame, mcontext) == MCONTEXT_OFFSET);
const _: () = assert!(size_of::<HostSigContext>() == 4384);
const _: () = assert!(offset_of!(HostSigContext, reserved) == 288);
const _: () = assert!(size_of::<FpsimdRecord>() == 528);
const _: () = assert!(size_of::<EsrRecord>() == 16);

/// Raw syscall: nothing of libc's here, since this runs on a thread whose
/// `tpidr_el0` may be the guest's, and libc's `errno` is a thread-local.
unsafe fn syscall4(nr: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    asm!(
        "svc #0",
        in("x8") nr,
        inout("x0") a0 => ret,
        in("x1") a1,
        in("x2") a2,
        in("x3") a3,
        options(nostack),
    );
    ret
}

/// This host thread's alternate signal stack, as `sigaltstack` reports it,
/// in the three words of a `stack_t`. All zero when the thread has none.
///
/// The host also knows the stack by name: it is what [`fault_fn_entry`]'s
/// caller runs its handler on, and `rt_sigreturn` must be handed it
/// unchanged, or the frame is refused.
unsafe fn current_altstack() -> [u64; 3] {
    let mut stack = [0u64; 3];
    if syscall4(SYS_SIGALTSTACK, 0, stack.as_mut_ptr() as usize, 0, 0) != 0 {
        // `SS_DISABLE` in `ss_flags`: the host accepts it as "no stack".
        stack = [0, 2, 0];
    }
    stack
}

/// This host thread's signal mask, to be put back by `rt_sigreturn`.
unsafe fn current_sigmask() -> u64 {
    let mut mask: u64 = 0;
    syscall4(
        SYS_RT_SIGPROCMASK,
        0, // SIG_BLOCK with no set: a read
        0,
        &mut mask as *mut u64 as usize,
        8,
    );
    mask
}

/// The guest's thread pointer, which the host keeps nowhere in a signal
/// context: it is whatever is in `tpidr_el0` when the handler runs.
unsafe fn read_tpidr() -> usize {
    let tp: usize;
    asm!("mrs {}, tpidr_el0", out(reg) tp, options(nomem, nostack, preserves_flags));
    tp
}

impl UserContext {
    /// Go to user context by function return, within the same privilege
    /// level.
    ///
    /// The guest comes back through a host signal and
    /// [`take_from_host`](Self::take_from_host); see the module
    /// documentation. `trap_num` and the syndrome fields are whatever that
    /// writes: nothing here sets them.
    pub fn run_fncall(&mut self) {
        // The frame goes on the kernel stack, under the callee-saved
        // registers `syscall_fn_return` parks: `rt_sigreturn` reads it
        // before the guest runs and never again, so the guest cannot touch
        // it and the kernel need not keep it.
        //
        // SAFETY: an all-zero frame is a valid one until it is filled in
        // below; every field the host checks is set.
        let mut frame: SigFrame = unsafe { core::mem::zeroed() };
        frame.uc_stack = unsafe { current_altstack() };
        frame.uc_sigmask = unsafe { current_sigmask() };
        let mc = &mut frame.mcontext;
        let g = &self.general;
        mc.regs = [
            g.x0 as u64,
            g.x1 as u64,
            g.x2 as u64,
            g.x3 as u64,
            g.x4 as u64,
            g.x5 as u64,
            g.x6 as u64,
            g.x7 as u64,
            g.x8 as u64,
            g.x9 as u64,
            g.x10 as u64,
            g.x11 as u64,
            g.x12 as u64,
            g.x13 as u64,
            g.x14 as u64,
            g.x15 as u64,
            g.x16 as u64,
            g.x17 as u64,
            g.x18 as u64,
            g.x19 as u64,
            g.x20 as u64,
            g.x21 as u64,
            g.x22 as u64,
            g.x23 as u64,
            g.x24 as u64,
            g.x25 as u64,
            g.x26 as u64,
            g.x27 as u64,
            g.x28 as u64,
            g.x29 as u64,
            g.x30 as u64,
        ];
        mc.sp = self.sp as u64;
        mc.pc = self.elr as u64;
        mc.pstate = self.spsr as u64 & PSTATE_NZCV;
        let fp = FpsimdRecord {
            magic: FPSIMD_MAGIC,
            size: size_of::<FpsimdRecord>() as u32,
            fpsr: self.fp.fpsr as u32,
            fpcr: self.fp.fpcr as u32,
            vregs: self.fp.q,
        };
        // SAFETY: the record and its zero terminator fit in `reserved` with
        // room to spare, and `reserved` is 16-aligned like the record.
        unsafe {
            (mc.reserved.as_mut_ptr() as *mut FpsimdRecord).write(fp);
            // The terminator is already zero.
            syscall_fn_return(self, &frame as *const SigFrame as *const u8);
        }
    }

    /// Take the guest's registers out of the host's signal context and point
    /// that context back into the kernel, so that the handler's own return
    /// lands in [`fault_fn_entry`] with the kernel's callee-saved registers
    /// restored and `syscall_fn_return` returns to its caller.
    ///
    /// Fills everything but `trap_num`, `esr` and `far`, which the caller
    /// decides from the signal, and returns the syndrome the host recorded
    /// (`esr_context`), if it recorded one. The guest's thread pointer is
    /// read from `tpidr_el0`, which is the guest's until the caller puts the
    /// kernel's back.
    ///
    /// # Safety
    ///
    /// `mc` is the context of a signal taken while this `UserContext` was
    /// out in the guest through [`run_fncall`](Self::run_fncall): its
    /// `__reserved` is the kernel stack `syscall_fn_return` left. Signal
    /// handler context: nothing here allocates, locks or touches a thread
    /// local.
    pub unsafe fn take_from_host(&mut self, mc: &mut HostSigContext) -> Option<u64> {
        let r = &mc.regs;
        self.general = GeneralRegs {
            x0: r[0] as usize,
            x1: r[1] as usize,
            x2: r[2] as usize,
            x3: r[3] as usize,
            x4: r[4] as usize,
            x5: r[5] as usize,
            x6: r[6] as usize,
            x7: r[7] as usize,
            x8: r[8] as usize,
            x9: r[9] as usize,
            x10: r[10] as usize,
            x11: r[11] as usize,
            x12: r[12] as usize,
            x13: r[13] as usize,
            x14: r[14] as usize,
            x15: r[15] as usize,
            x16: r[16] as usize,
            x17: r[17] as usize,
            x18: r[18] as usize,
            x19: r[19] as usize,
            x20: r[20] as usize,
            x21: r[21] as usize,
            x22: r[22] as usize,
            x23: r[23] as usize,
            x24: r[24] as usize,
            x25: r[25] as usize,
            x26: r[26] as usize,
            x27: r[27] as usize,
            x28: r[28] as usize,
            x29: r[29] as usize,
            __reserved: 0,
            x30: r[30] as usize,
        };
        self.sp = mc.sp as usize;
        self.elr = mc.pc as usize;
        self.spsr = mc.pstate as usize;
        self.tpidr = read_tpidr();

        let mut esr = None;
        let mut fp = FpRegs::default();
        let base = mc.reserved.as_ptr();
        let mut at = 0usize;
        // Walk the records: `(magic, size)` headers until a zero one. The
        // host wrote them, so they are well formed, but the bounds are kept
        // all the same: a handler that walks off the end dies silently.
        while at + 16 <= mc.reserved.len() {
            let magic = (base.add(at) as *const u32).read_unaligned();
            let size = (base.add(at + 4) as *const u32).read_unaligned() as usize;
            if magic == 0 || size < 16 || at + size > mc.reserved.len() {
                break;
            }
            match magic {
                FPSIMD_MAGIC if size >= size_of::<FpsimdRecord>() => {
                    let record = &*(base.add(at) as *const FpsimdRecord);
                    fp.q = record.vregs;
                    fp.fpsr = record.fpsr as usize;
                    fp.fpcr = record.fpcr as usize;
                }
                ESR_MAGIC if size >= size_of::<EsrRecord>() => {
                    esr = Some((*(base.add(at) as *const EsrRecord)).esr);
                }
                _ => {}
            }
            at += size;
        }
        self.fp = fp;

        // The way back: the host returns from the handler into the kernel
        // stack `syscall_fn_return` parked its registers on, at the entry
        // that unparks them. The guest's general registers it also restores
        // are dead there; `fault_fn_entry` overwrites the callee-saved ones
        // and the caller of `syscall_fn_return` expects nothing else.
        mc.pc = fault_fn_entry as *const () as usize as u64;
        mc.sp = self.__reserved as u64;
        mc.pstate &= PSTATE_NZCV;
        esr
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    // Mock user program: dump the registers at the stack, add 100 to each,
    // and trap back into the kernel with a `brk`.
    global_asm!(
        r#"
dump_registers:
    stp     x30, x0, [sp, #-16]!
    str     x29, [sp, #-16]!
    stp     x27, x28, [sp, #-16]!
    stp     x25, x26, [sp, #-16]!
    stp     x23, x24, [sp, #-16]!
    stp     x21, x22, [sp, #-16]!
    stp     x19, x20, [sp, #-16]!
    stp     x17, x18, [sp, #-16]!
    stp     x15, x16, [sp, #-16]!
    stp     x13, x14, [sp, #-16]!
    stp     x11, x12, [sp, #-16]!
    stp     x9, x10, [sp, #-16]!
    stp     x7, x8, [sp, #-16]!
    stp     x5, x6, [sp, #-16]!
    stp     x3, x4, [sp, #-16]!
    stp     x1, x2, [sp, #-16]!

    add     x0, x0, #100
    add     x1, x1, #100
    add     x2, x2, #100
    add     x3, x3, #100
    add     x4, x4, #100
    add     x5, x5, #100
    add     x6, x6, #100
    add     x7, x7, #100
    add     x8, x8, #100
    add     x9, x9, #100
    add     x10, x10, #100
    add     x11, x11, #100
    add     x12, x12, #100
    add     x13, x13, #100
    add     x14, x14, #100
    add     x15, x15, #100
    add     x16, x16, #100
    add     x17, x17, #100
    add     x18, x18, #100
    add     x19, x19, #100
    add     x20, x20, #100
    add     x21, x21, #100
    add     x22, x22, #100
    add     x23, x23, #100
    add     x24, x24, #100
    add     x25, x25, #100
    add     x26, x26, #100
    add     x27, x27, #100
    add     x28, x28, #100
    add     x29, x29, #100
    add     x30, x30, #100

    # The guest's thread pointer and a vector register, for the record.
    mrs     x9, tpidr_el0
    movi    v3.16b, #0x5a

.global brk_location
brk_location:
    brk     #0x1234
"#
    );

    /// What the test's own catcher knows: the context that is out, and the
    /// kernel's thread pointer to put back. One test, one guest at a time.
    static OUT: AtomicUsize = AtomicUsize::new(0);
    static KERNEL_TP: AtomicUsize = AtomicUsize::new(0);
    static GUEST_TP: AtomicUsize = AtomicUsize::new(0);

    /// The catcher the kernel's fault bridge stands in for here: `SIGTRAP`
    /// for the mock guest's `brk`.
    extern "C" fn on_trap(_signal: i32, _info: *mut u8, context: *mut u8) {
        unsafe {
            let mc = &mut *(context.add(MCONTEXT_OFFSET - 128) as *mut HostSigContext);
            let ctx = &mut *(OUT.load(Ordering::Acquire) as *mut UserContext);
            GUEST_TP.store(read_tpidr(), Ordering::Release);
            ctx.take_from_host(mc);
            ctx.trap_num = 2;
            asm!("msr tpidr_el0, {}", in(reg) KERNEL_TP.load(Ordering::Acquire));
        }
    }

    #[repr(C)]
    struct SigAction {
        handler: usize,
        flags: u64,
        restorer: usize,
        mask: u64,
    }

    /// `rt_sigaction(SIGTRAP, SA_SIGINFO | SA_ONSTACK | SA_NODEFER)`, with
    /// no restorer: the host supplies its own on aarch64. On an alternate
    /// stack as the kernel's bridge does, because the handler must not run
    /// on the guest's stack: a signal frame is 4.7 KiB plus the handler's
    /// own, and the mock guest's stack below is one page, over the kernel's
    /// parked registers. Measured: the pops in `fault_fn_entry` read the
    /// handler's leftovers and `ret` went to zero.
    unsafe fn catch_sigtrap() {
        const SIGTRAP: usize = 5;
        const SA_SIGINFO: u64 = 4;
        const SA_ONSTACK: u64 = 0x0800_0000;
        const SA_NODEFER: u64 = 0x4000_0000;
        static mut ALTSTACK: [u64; 8192] = [0; 8192];
        let altstack = [
            core::ptr::addr_of_mut!(ALTSTACK) as usize as u64,
            0,
            size_of::<[u64; 8192]>() as u64,
        ];
        assert_eq!(
            syscall4(SYS_SIGALTSTACK, altstack.as_ptr() as usize, 0, 0, 0),
            0,
            "sigaltstack"
        );
        let action = SigAction {
            handler: on_trap as *const () as usize,
            flags: SA_SIGINFO | SA_ONSTACK | SA_NODEFER,
            restorer: 0,
            mask: 0,
        };
        let ret = syscall4(134, SIGTRAP, &action as *const _ as usize, 0, 8);
        assert_eq!(ret, 0, "rt_sigaction");
    }

    #[test]
    fn run_fncall() {
        extern "C" {
            fn dump_registers();
            fn brk_location();
        }
        unsafe { catch_sigtrap() };
        let mut stack = [0u8; 0x1000];
        let general = GeneralRegs {
            x0: 0,
            x1: 1,
            x2: 2,
            x3: 3,
            x4: 4,
            x5: 5,
            x6: 6,
            x7: 7,
            x8: 8,
            x9: 9,
            x10: 10,
            x11: 11,
            x12: 12,
            x13: 13,
            x14: 14,
            x15: 15,
            x16: 16,
            x17: 17,
            x18: 18,
            x19: 19,
            x20: 20,
            x21: 21,
            x22: 22,
            x23: 23,
            x24: 24,
            x25: 25,
            x26: 26,
            x27: 27,
            x28: 28,
            x29: 29,
            x30: 30,
            ..Default::default()
        };
        let mut fp = FpRegs::default();
        fp.q[0] = 0x1111_2222_3333_4444;
        fp.q[1] = 0x5555_6666_7777_8888;
        fp.q[62] = 0xdead_beef;
        let mut cx = UserContext {
            general,
            sp: stack.as_mut_ptr() as usize + 0x1000,
            elr: dump_registers as *const () as usize,
            tpidr: 0x7777_0000,
            fp,
            ..Default::default()
        };
        let kernel_tp = unsafe { read_tpidr() };
        KERNEL_TP.store(kernel_tp, Ordering::Release);
        OUT.store(&mut cx as *mut UserContext as usize, Ordering::Release);
        cx.run_fncall();
        // The kernel's thread pointer is back.
        assert_eq!(unsafe { read_tpidr() }, kernel_tp);
        // The guest ran with its own.
        assert_eq!(GUEST_TP.load(Ordering::Acquire), 0x7777_0000);
        // check restored registers
        let general_dump = unsafe { *(cx.sp as *const GeneralRegs) };
        assert_eq!(general_dump, GeneralRegs { x30: 30, ..general });
        // check saved registers
        assert_eq!(
            cx.general,
            GeneralRegs {
                x0: 100 + 0,
                x1: 100 + 1,
                x2: 100 + 2,
                x3: 100 + 3,
                x4: 100 + 4,
                x5: 100 + 5,
                x6: 100 + 6,
                x7: 100 + 7,
                x8: 100 + 8,
                x9: 0x7777_0000,
                x10: 100 + 10,
                x11: 100 + 11,
                x12: 100 + 12,
                x13: 100 + 13,
                x14: 100 + 14,
                x15: 100 + 15,
                x16: 100 + 16,
                x17: 100 + 17,
                x18: 100 + 18,
                x19: 100 + 19,
                x20: 100 + 20,
                x21: 100 + 21,
                x22: 100 + 22,
                x23: 100 + 23,
                x24: 100 + 24,
                x25: 100 + 25,
                x26: 100 + 26,
                x27: 100 + 27,
                x28: 100 + 28,
                x29: 100 + 29,
                x30: 100 + 30,
                ..cx.general
            }
        );
        assert_eq!(cx.elr, brk_location as *const () as usize);
        assert_eq!(cx.tpidr, 0x7777_0000);
        assert_eq!(cx.trap_num, 2);
        // The vector registers went out and came back: q0 and q31 as given,
        // q3 as the guest left it.
        assert_eq!(cx.fp.q[0], 0x1111_2222_3333_4444);
        assert_eq!(cx.fp.q[1], 0x5555_6666_7777_8888);
        assert_eq!(cx.fp.q[62], 0xdead_beef);
        assert_eq!(cx.fp.q[6], 0x5a5a_5a5a_5a5a_5a5a);
        assert_eq!(cx.fp.q[7], 0x5a5a_5a5a_5a5a_5a5a);
    }

    #[test]
    fn the_signal_frame_is_laid_as_the_host_lays_it() {
        assert_eq!(offset_of!(SigFrame, uc_stack), 128 + 16);
        assert_eq!(offset_of!(SigFrame, uc_sigmask), 128 + 40);
        assert_eq!(offset_of!(SigFrame, mcontext), 304);
        assert_eq!(offset_of!(HostSigContext, regs), 8);
        assert_eq!(offset_of!(HostSigContext, sp), 256);
        assert_eq!(offset_of!(HostSigContext, pc), 264);
        assert_eq!(offset_of!(HostSigContext, pstate), 272);
        assert_eq!(offset_of!(HostSigContext, reserved), 288);
        assert_eq!(offset_of!(FpsimdRecord, vregs), 16);
    }
}
