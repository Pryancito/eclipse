//! The `ucontext_t` the kernel builds on the user stack before it jumps into a
//! signal handler, and reads back on `sigreturn`.
//!
//! Every field here is part of an ABI a handler compiled by someone else
//! reads: musl's `ucontext_t` and `mcontext_t`, which are the kernel's
//! `struct ucontext` (`include/uapi/asm-generic/ucontext.h`) and each
//! architecture's `struct sigcontext`. Get an offset wrong and a handler reads
//! the wrong word; get the size wrong and the frame the kernel pushes does not
//! reach as far as the handler looks.
//!
//! The three layouts are **always compiled**, not selected by `cfg`, and the
//! active one is re-exported below. That is deliberate: a layout only one
//! architecture compiles is a layout no host test can check, and the aarch64
//! one spent its whole life as a `[usize; 274]` placeholder whose `get_pc` and
//! `set_pc` were `unimplemented!()` -- a kernel panic on the first signal
//! delivered to a handler, since `loader::linux` builds this struct for every
//! one. Nothing caught it because nothing on an x86_64 host could see it.

use super::{SignalStack, Sigset};
use kernel_hal::context::UserContext;

/// The x86_64 frame. `mcontext_t` comes BEFORE `uc_sigmask` here, unlike the
/// asm-generic layout the other two use: this is glibc/musl's
/// `struct __ucontext` for x86_64, which ends with the FPU save area.
pub mod x86_64 {
    use super::{SignalStack, Sigset};

    #[repr(C)]
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct FpregsMem {
        mem: [usize; 64],
    }

    impl Default for FpregsMem {
        fn default() -> Self {
            Self { mem: [0; 64] }
        }
    }

    /// See musl struct __ucontext
    #[repr(C)]
    #[derive(Clone, Default, Debug)]
    pub struct SignalUserContext {
        pub flags: usize,
        pub link: usize,
        pub stack: SignalStack,
        pub context: MachineContext,
        pub sig_mask: Sigset,
        pub _pad: [u64; 15], // very strange, maybe a bug of musl libc
        pub fpregs_mem: FpregsMem,
    }

    /// struct mcontext
    #[repr(C)]
    #[derive(Clone, Debug, Default, Eq, PartialEq)]
    pub struct MachineContext {
        // gregs
        pub r8: usize,
        pub r9: usize,
        pub r10: usize,
        pub r11: usize,
        pub r12: usize,
        pub r13: usize,
        pub r14: usize,
        pub r15: usize,
        pub rdi: usize,
        pub rsi: usize,
        pub rbp: usize,
        pub rbx: usize,
        pub rdx: usize,
        pub rax: usize,
        pub rcx: usize,
        pub rsp: usize,
        pub rip: usize,
        pub eflags: usize,
        pub cs: u16,
        pub gs: u16,
        pub fs: u16,
        pub _pad: u16,
        pub err: usize,
        pub trapno: usize,
        pub oldmask: usize,
        pub cr2: usize,
        // fpregs
        // TODO
        pub fpstate: usize,
        // reserved
        pub _reserved1: [usize; 8],
    }

    impl MachineContext {
        pub fn new(pc: usize) -> Self {
            Self {
                rip: pc,
                ..Default::default()
            }
        }

        pub fn get_pc(&self) -> usize {
            self.rip
        }

        pub fn set_pc(&mut self, pc: usize) {
            self.rip = pc;
        }
    }

    /// The `eflags` bits a handler may change through its own `ucontext`:
    /// Linux's `FIX_EFLAGS` (`arch/x86/kernel/signal.c`) minus `TF`.
    ///
    /// Everything outside this mask -- `IF`, `IOPL`, the system bits -- keeps
    /// the value the interrupted context had. A handler that could raise IOPL
    /// or clear IF by writing its own `uc_mcontext` would be setting kernel
    /// policy from ring 3, and `sigreturn` takes that struct straight from the
    /// user stack. `TF` is in Linux's set and not in this one because Eclipse
    /// has no path turning the resulting `#DB` into a `SIGTRAP`: honouring it
    /// would single-step a process into a trap nothing answers.
    pub const RESTORABLE_EFLAGS: usize = 0x0005_0CD5;

    /// The 64-bit user code selector, which is what Linux leaves in the low
    /// half of `gregs[REG_CSGSFS]` for a 64-bit process. `%gs` and `%fs` stay
    /// zero there: their bases live in `MSR_FS_BASE`/`MSR_KERNEL_GS_BASE`, not
    /// in the selectors, so a handler reading those two learns nothing either
    /// way.
    const USER_CS: u16 = 0x33;

    #[cfg(target_arch = "x86_64")]
    impl MachineContext {
        /// The registers the signal interrupted, as Linux's
        /// `setup_sigcontext()` copies them into the frame.
        ///
        /// This was `MachineContext::new(pc)`: the program counter and
        /// **sixteen zeros**. Every handler that reads its `ucontext` -- a JIT
        /// stepping past a faulting load, a crash reporter printing registers,
        /// a garbage collector asking where the thread was -- was handed those
        /// zeros and believed them.
        ///
        /// Go's asynchronous preemption is the sharpest case, because its
        /// whole mechanism is this struct: `runtime.doSigPreempt` reads
        /// `uc_mcontext.rsp` to decide whether the goroutine is at a safe
        /// point, and a zero is never inside any goroutine stack, so the
        /// answer was always "no". The runtime preempts a goroutine that has
        /// run for 10 ms by sending it `SIGURG`; with the answer always "no"
        /// the goroutine is never actually stopped, the `SIGURG` is resent
        /// forever, and anything that has to stop the world -- a garbage
        /// collection, above all -- waits for a thread that will never yield.
        pub fn from_context(ctx: &mut super::UserContext) -> Self {
            let g = ctx.general();
            Self {
                r8: g.r8,
                r9: g.r9,
                r10: g.r10,
                r11: g.r11,
                r12: g.r12,
                r13: g.r13,
                r14: g.r14,
                r15: g.r15,
                rdi: g.rdi,
                rsi: g.rsi,
                rbp: g.rbp,
                rbx: g.rbx,
                rdx: g.rdx,
                rax: g.rax,
                rcx: g.rcx,
                rsp: g.rsp,
                rip: g.rip,
                eflags: g.rflags,
                cs: USER_CS,
                ..Default::default()
            }
        }

        /// Put the frame's registers back on `sigreturn(2)` -- the other half
        /// of the same contract. Linux's `restore_sigcontext()` reads every
        /// general register out of the frame, which is what makes rewriting
        /// one inside a handler the documented way to change where, and in
        /// what state, the interrupted code resumes.
        ///
        /// Only the program counter was honoured here. `rsp` with it is what
        /// Go's `pushCall` needs: it writes a return address below the
        /// interrupted stack pointer, moves `rsp` down over it and points
        /// `rip` at `asyncPreempt`. With the old `rsp` restored and the new
        /// `rip` kept, `asyncPreempt` would return to whatever word the
        /// goroutine happened to be holding at its stack pointer.
        ///
        /// Every register here is userspace's own state, so letting a handler
        /// choose it grants nothing it could not do with ordinary
        /// instructions -- except `eflags`, filtered through
        /// [`RESTORABLE_EFLAGS`].
        pub fn restore_into(&self, ctx: &mut super::UserContext) {
            let kept_flags = ctx.general().rflags & !RESTORABLE_EFLAGS;
            let g = ctx.general_mut();
            g.r8 = self.r8;
            g.r9 = self.r9;
            g.r10 = self.r10;
            g.r11 = self.r11;
            g.r12 = self.r12;
            g.r13 = self.r13;
            g.r14 = self.r14;
            g.r15 = self.r15;
            g.rdi = self.rdi;
            g.rsi = self.rsi;
            g.rbp = self.rbp;
            g.rbx = self.rbx;
            g.rdx = self.rdx;
            g.rax = self.rax;
            g.rcx = self.rcx;
            g.rsp = self.rsp;
            g.rip = self.rip;
            g.rflags = kept_flags | (self.eflags & RESTORABLE_EFLAGS);
        }
    }
}

/// The riscv64 frame: the asm-generic `struct ucontext`, whose `mcontext_t` is
/// `struct user_regs_struct` (its first word is `pc`) followed by the FP save
/// union, whose largest member is the Q extension's 528 bytes.
pub mod riscv64 {
    use super::{SignalStack, Sigset};

    /// See musl struct __ucontext
    #[repr(C)]
    #[derive(Clone, Default, Debug)]
    pub struct SignalUserContext {
        pub flags: usize,
        pub link: usize,
        pub stack: SignalStack,
        pub sig_mask: Sigset,
        pub _pad: [u64; 15], // very strange, maybe a bug of musl libc
        pub context: MachineContext,
    }

    /// struct mcontext
    #[repr(C, align(16))]
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct MachineContext {
        // general regs, but only regs[0](namely `pc`) is used
        pub general_regs: [usize; 32],
        // fpregs
        pub fpstate: [usize; 66],
    }

    impl Default for MachineContext {
        fn default() -> Self {
            Self {
                general_regs: [0; 32],
                fpstate: [0; 66],
            }
        }
    }

    impl MachineContext {
        pub fn new(pc: usize) -> Self {
            let mut n = Self::default();
            n.general_regs[0] = pc;
            n
        }

        pub fn get_pc(&self) -> usize {
            self.general_regs[0]
        }

        pub fn set_pc(&mut self, pc: usize) {
            self.general_regs[0] = pc;
        }
    }

    /// `general_regs` is `struct user_regs_struct`, whose order is `pc`, `ra`,
    /// `sp`, `gp`, `tp`, `t0..t2`, `s0`, `s1`, `a0..a7`, `s2..s11`, `t3..t6` --
    /// the same order `trapframe`'s `GeneralRegs` has below `zero`, which is
    /// why this is two straight copies and not a field-by-field map.
    #[cfg(target_arch = "riscv64")]
    impl MachineContext {
        /// The registers the signal interrupted. The comment on the field
        /// above used to read "only regs[0] (namely `pc`) is used": everything
        /// else went to the handler as zero. See the x86_64 twin for what that
        /// costs a handler that reads its own `ucontext`.
        pub fn from_context(ctx: &mut super::UserContext) -> Self {
            let mut m = Self::default();
            let g = ctx.general();
            m.general_regs[0] = ctx.get_field(kernel_hal::context::UserContextField::InstrPointer);
            m.general_regs[1..].copy_from_slice(&[
                g.ra, g.sp, g.gp, g.tp, g.t0, g.t1, g.t2, g.s0, g.s1, g.a0, g.a1, g.a2, g.a3, g.a4,
                g.a5, g.a6, g.a7, g.s2, g.s3, g.s4, g.s5, g.s6, g.s7, g.s8, g.s9, g.s10, g.s11,
                g.t3, g.t4, g.t5, g.t6,
            ]);
            m
        }

        /// Put them back on `sigreturn(2)`. `sstatus` is not userspace's to
        /// choose and is not carried here, so it keeps the value the
        /// interrupted context had.
        pub fn restore_into(&self, ctx: &mut super::UserContext) {
            let r = &self.general_regs;
            ctx.set_field(kernel_hal::context::UserContextField::InstrPointer, r[0]);
            let g = ctx.general_mut();
            g.zero = 0;
            g.ra = r[1];
            g.sp = r[2];
            g.gp = r[3];
            g.tp = r[4];
            g.t0 = r[5];
            g.t1 = r[6];
            g.t2 = r[7];
            g.s0 = r[8];
            g.s1 = r[9];
            g.a0 = r[10];
            g.a1 = r[11];
            g.a2 = r[12];
            g.a3 = r[13];
            g.a4 = r[14];
            g.a5 = r[15];
            g.a6 = r[16];
            g.a7 = r[17];
            g.s2 = r[18];
            g.s3 = r[19];
            g.s4 = r[20];
            g.s5 = r[21];
            g.s6 = r[22];
            g.s7 = r[23];
            g.s8 = r[24];
            g.s9 = r[25];
            g.s10 = r[26];
            g.s11 = r[27];
            g.t3 = r[28];
            g.t4 = r[29];
            g.t5 = r[30];
            g.t6 = r[31];
        }
    }
}

/// The aarch64 frame: the asm-generic `struct ucontext` again, with arm64's
/// `struct sigcontext` (`arch/arm64/include/uapi/asm/sigcontext.h`).
pub mod aarch64 {
    use super::{SignalStack, Sigset};

    /// See musl struct __ucontext
    #[repr(C)]
    #[derive(Clone, Default, Debug)]
    pub struct SignalUserContext {
        pub flags: usize,
        pub link: usize,
        pub stack: SignalStack,
        pub sig_mask: Sigset,
        pub _pad: [u64; 15], // very strange, maybe a bug of musl libc
        pub context: MachineContext,
    }

    /// `struct sigcontext`, which is musl's `mcontext_t` on aarch64:
    ///
    /// ```c
    /// struct sigcontext {
    ///     __u64 fault_address;
    ///     __u64 regs[31];
    ///     __u64 sp, pc, pstate;
    ///     __u8 __reserved[4096] __attribute__((__aligned__(16)));
    /// };
    /// ```
    ///
    /// The 16-byte alignment is part of the ABI, not decoration: it is what
    /// pushes `uc_mcontext` from offset 168 to 176 inside the `ucontext`, and
    /// what the `_aarch64_ctx` records in `__reserved` are aligned to. The
    /// eight bytes of `_pad` are the padding a C compiler inserts for it.
    #[repr(C, align(16))]
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct MachineContext {
        pub fault_address: usize,
        pub regs: [usize; 31],
        pub sp: usize,
        pub pc: usize,
        pub pstate: usize,
        _pad: usize,
        /// Where the kernel writes the `_aarch64_ctx` records (FPSIMD, SVE,
        /// ESR) and the terminating null record. Left zeroed, which is a
        /// well-formed empty record list: a reader walking it sees magic 0,
        /// size 0, and stops.
        pub reserved: [u64; 512],
    }

    impl Default for MachineContext {
        fn default() -> Self {
            Self {
                fault_address: 0,
                regs: [0; 31],
                sp: 0,
                pc: 0,
                pstate: 0,
                _pad: 0,
                reserved: [0; 512],
            }
        }
    }

    impl MachineContext {
        pub fn new(pc: usize) -> Self {
            Self {
                pc,
                ..Default::default()
            }
        }

        pub fn get_pc(&self) -> usize {
            self.pc
        }

        pub fn set_pc(&mut self, pc: usize) {
            self.pc = pc;
        }
    }

    /// `regs` is `x0..x30` in order, which is not the order `trapframe` stores
    /// them in (`x0` and `x30` sit at the end of its `GeneralRegs`, put there
    /// for the trap entry assembly), so both directions spell the mapping out.
    #[cfg(target_arch = "aarch64")]
    impl MachineContext {
        /// The registers the signal interrupted. They were sixteen -- here
        /// thirty-three -- zeros; see the x86_64 twin for what that costs.
        ///
        /// `fault_address` stays zero: this is built from the thread's
        /// registers, which do not carry `FAR_EL1`, and a handler that wants
        /// the faulting address reads `si_addr` out of the `siginfo` it is
        /// handed alongside.
        pub fn from_context(ctx: &mut super::UserContext) -> Self {
            let g = *ctx.general();
            let mut m = Self {
                pc: ctx.get_field(kernel_hal::context::UserContextField::InstrPointer),
                sp: ctx.get_field(kernel_hal::context::UserContextField::StackPointer),
                ..Default::default()
            };
            m.regs = [
                g.x0, g.x1, g.x2, g.x3, g.x4, g.x5, g.x6, g.x7, g.x8, g.x9, g.x10, g.x11, g.x12,
                g.x13, g.x14, g.x15, g.x16, g.x17, g.x18, g.x19, g.x20, g.x21, g.x22, g.x23, g.x24,
                g.x25, g.x26, g.x27, g.x28, g.x29, g.x30,
            ];
            m
        }

        /// Put them back on `sigreturn(2)`. `pstate` is not carried back: it
        /// holds the exception-level and mask bits `spsr_el1` needs, which are
        /// the kernel's to set, not a handler's.
        pub fn restore_into(&self, ctx: &mut super::UserContext) {
            ctx.set_field(kernel_hal::context::UserContextField::InstrPointer, self.pc);
            ctx.set_field(kernel_hal::context::UserContextField::StackPointer, self.sp);
            let r = &self.regs;
            let g = ctx.general_mut();
            g.x0 = r[0];
            g.x1 = r[1];
            g.x2 = r[2];
            g.x3 = r[3];
            g.x4 = r[4];
            g.x5 = r[5];
            g.x6 = r[6];
            g.x7 = r[7];
            g.x8 = r[8];
            g.x9 = r[9];
            g.x10 = r[10];
            g.x11 = r[11];
            g.x12 = r[12];
            g.x13 = r[13];
            g.x14 = r[14];
            g.x15 = r[15];
            g.x16 = r[16];
            g.x17 = r[17];
            g.x18 = r[18];
            g.x19 = r[19];
            g.x20 = r[20];
            g.x21 = r[21];
            g.x22 = r[22];
            g.x23 = r[23];
            g.x24 = r[24];
            g.x25 = r[25];
            g.x26 = r[26];
            g.x27 = r[27];
            g.x28 = r[28];
            g.x29 = r[29];
            g.x30 = r[30];
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub use x86_64::{FpregsMem, MachineContext, SignalUserContext};

#[cfg(target_arch = "riscv64")]
pub use riscv64::{MachineContext, SignalUserContext};

#[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
pub use aarch64::{MachineContext, SignalUserContext};

#[cfg(test)]
mod ucontext_layout_tests {
    //! The frame the kernel pushes, measured against musl's headers and the
    //! kernel's `struct ucontext` / `struct sigcontext`.
    //!
    //! These run on ANY host, for all three architectures, which is the whole
    //! reason the layouts above are not behind a `cfg`. The aarch64 one was a
    //! `[usize; 274]` placeholder with `unimplemented!()` for its accessors
    //! and no host could see it: `loader::linux` calls `MachineContext::new`
    //! on every signal delivered to a handler, and `restore_after_handle_signal`
    //! calls `get_pc` on every `sigreturn`, so a handler on aarch64 took the
    //! kernel down twice over.

    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    /// The `ucontext` all three share below the `mcontext`: the asm-generic
    /// one, whose 128 bytes of signal mask are `Sigset` plus `_pad`.
    const UC_FLAGS: usize = 0;
    const UC_LINK: usize = 8;
    const UC_STACK: usize = 16;
    const UC_STACK_SIZE: usize = 24;
    const SIGSET_T: usize = 128;

    #[test]
    fn a_handler_on_every_architecture_is_told_where_it_was_interrupted() {
        // The one thing the frame must carry, and the one that was
        // `unimplemented!()` on aarch64 -- a kernel panic from a plain
        // `signal(SIGINT, handler)` and a Ctrl-C.
        const PC: usize = 0x7f00_1234_5678;
        assert_eq!(x86_64::MachineContext::new(PC).get_pc(), PC);
        assert_eq!(riscv64::MachineContext::new(PC).get_pc(), PC);
        assert_eq!(aarch64::MachineContext::new(PC).get_pc(), PC);

        let mut m = aarch64::MachineContext::new(PC);
        m.set_pc(0x4000_0000);
        assert_eq!(m.get_pc(), 0x4000_0000);
        assert_eq!(
            m.regs, [0; 31],
            "setting the pc must not disturb the saved registers"
        );
    }

    /// `arch/arm64/include/uapi/asm/sigcontext.h`, which is musl's
    /// `mcontext_t`: `fault_address`, `regs[31]`, `sp`, `pc`, `pstate`, then
    /// 4096 bytes of `_aarch64_ctx` records aligned to 16.
    #[test]
    fn the_aarch64_mcontext_is_the_kernels_struct_sigcontext() {
        type M = aarch64::MachineContext;
        assert_eq!(align_of::<M>(), 16, "__reserved is 16-byte aligned");
        assert_eq!(offset_of!(M, fault_address), 0);
        assert_eq!(offset_of!(M, regs), 8);
        assert_eq!(offset_of!(M, sp), 8 + 31 * 8);
        assert_eq!(offset_of!(M, pc), 8 + 32 * 8);
        assert_eq!(offset_of!(M, pstate), 8 + 33 * 8);
        // 280 bytes of fields, then the padding a C compiler inserts to put
        // `__reserved` on a 16-byte boundary.
        assert_eq!(offset_of!(M, reserved), 288);
        assert_eq!(size_of::<M>(), 288 + 4096);
    }

    /// `include/uapi/asm-generic/ucontext.h`. The 16-byte alignment of the
    /// `mcontext` is what moves it from 168 to 176; a `mcontext` that was
    /// merely 8-aligned would put every field a handler reads eight bytes
    /// early.
    #[test]
    fn the_aarch64_ucontext_puts_the_mcontext_where_a_handler_looks() {
        type U = aarch64::SignalUserContext;
        assert_eq!(offset_of!(U, flags), UC_FLAGS);
        assert_eq!(offset_of!(U, link), UC_LINK);
        assert_eq!(offset_of!(U, stack), UC_STACK);
        assert_eq!(offset_of!(U, sig_mask), UC_STACK + UC_STACK_SIZE);
        assert_eq!(size_of::<SignalStack>(), UC_STACK_SIZE);
        assert_eq!(
            size_of::<Sigset>() + size_of::<[u64; 15]>(),
            SIGSET_T,
            "the mask and its padding are one 128-byte sigset_t"
        );
        assert_eq!(offset_of!(U, context), 176);
        assert_eq!(size_of::<U>(), 176 + size_of::<aarch64::MachineContext>());
    }

    /// musl's x86_64 `__ucontext`: the `mcontext` comes BEFORE the mask here,
    /// and the frame ends with 512 bytes of FPU save area.
    #[test]
    fn the_x86_64_ucontext_is_musls() {
        type U = x86_64::SignalUserContext;
        type M = x86_64::MachineContext;
        assert_eq!(offset_of!(U, stack), UC_STACK);
        assert_eq!(offset_of!(U, context), UC_STACK + UC_STACK_SIZE);
        assert_eq!(size_of::<M>(), 256, "gregs + fpregs + __reserved1[8]");
        assert_eq!(offset_of!(M, rip), 16 * 8, "gregs[REG_RIP] is the 17th");
        assert_eq!(offset_of!(U, sig_mask), 40 + 256);
        assert_eq!(offset_of!(U, fpregs_mem), 40 + 256 + SIGSET_T);
        assert_eq!(size_of::<x86_64::FpregsMem>(), 512);
        assert_eq!(size_of::<U>(), 936);
    }

    /// riscv64: the asm-generic `ucontext` again, and a `mcontext` that is
    /// `struct user_regs_struct` (`pc` first) plus the widest member of the
    /// FP union, the Q extension's 528 bytes.
    #[test]
    fn the_riscv64_ucontext_starts_its_mcontext_at_the_program_counter() {
        type U = riscv64::SignalUserContext;
        type M = riscv64::MachineContext;
        assert_eq!(offset_of!(M, general_regs), 0, "sc_regs.pc is first");
        assert_eq!(offset_of!(M, fpstate), 32 * 8);
        assert_eq!(size_of::<M>(), (32 + 66) * 8);
        assert_eq!(align_of::<M>(), 16);
        assert_eq!(offset_of!(U, sig_mask), UC_STACK + UC_STACK_SIZE);
        assert_eq!(offset_of!(U, context), 176);
    }

    /// The frame is pushed as `size_of::<SignalUserContext>()` bytes of user
    /// stack, so a layout that is too short is a handler reading past it.
    /// Pin the three totals.
    #[test]
    fn the_frame_each_architecture_pushes_is_the_size_its_libc_expects() {
        assert_eq!(size_of::<x86_64::SignalUserContext>(), 936);
        assert_eq!(size_of::<riscv64::SignalUserContext>(), 176 + 784);
        assert_eq!(size_of::<aarch64::SignalUserContext>(), 176 + 4384);
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod ucontext_register_tests {
    //! The registers the frame carries, which are the half of the ABI the
    //! layout tests above cannot see: a `ucontext` of the right shape full of
    //! zeros passes every one of them.

    use super::x86_64::{MachineContext, RESTORABLE_EFLAGS};
    use kernel_hal::context::UserContext;

    /// A context with a distinct value in every general register, so a field
    /// copied from or into the wrong one shows up as the wrong number rather
    /// than as another zero.
    fn numbered_context() -> UserContext {
        let mut ctx = UserContext::default();
        let g = ctx.general_mut();
        g.rax = 0x01;
        g.rbx = 0x02;
        g.rcx = 0x03;
        g.rdx = 0x04;
        g.rsi = 0x05;
        g.rdi = 0x06;
        g.rbp = 0x07;
        g.rsp = 0x7fff_ffff_e000;
        g.r8 = 0x09;
        g.r9 = 0x0a;
        g.r10 = 0x0b;
        g.r11 = 0x0c;
        g.r12 = 0x0d;
        g.r13 = 0x0e;
        g.r14 = 0x0f;
        g.r15 = 0x10;
        g.rip = 0x4011_22;
        g.rflags = 0x3202; // IOPL=3, IF, CF, and the always-set bit 1.
        ctx
    }

    #[test]
    fn the_frame_carries_the_registers_the_signal_interrupted() {
        // It carried the program counter and sixteen zeros. A handler reading
        // its own `ucontext` -- Go's asynchronous preemption reads `rsp` out
        // of it to decide whether the goroutine is at a safe point -- believed
        // them.
        let mut ctx = numbered_context();
        let m = MachineContext::from_context(&mut ctx);
        assert_eq!(m.rsp, 0x7fff_ffff_e000, "a zero rsp is in nobody's stack");
        assert_eq!(m.rip, 0x4011_22);
        assert_eq!(m.eflags, 0x3202);
        assert_eq!(
            [
                m.rax, m.rbx, m.rcx, m.rdx, m.rsi, m.rdi, m.rbp, m.r8, m.r9, m.r10, m.r11, m.r12,
                m.r13, m.r14, m.r15
            ],
            [
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
                0x10
            ],
        );
        assert_eq!(m.cs, 0x33, "a 64-bit process's code selector");
    }

    #[test]
    fn sigreturn_puts_back_every_register_the_handler_left_in_the_frame() {
        // The other half: only the program counter was honoured, so a handler
        // that moved `rsp` -- which is exactly what Go's `pushCall` does to
        // inject a call to `asyncPreempt` -- resumed on the old stack with the
        // new entry point.
        let mut ctx = numbered_context();
        let mut m = MachineContext::from_context(&mut ctx);
        m.rsp = 0x7fff_ffff_dff8;
        m.rip = 0x4455_66;
        m.r12 = 0x1234;
        m.rax = 0xfeed;

        let mut resumed = UserContext::default();
        m.restore_into(&mut resumed);
        let g = resumed.general();
        assert_eq!(g.rsp, 0x7fff_ffff_dff8, "the handler's rsp was dropped");
        assert_eq!(g.rip, 0x4455_66);
        assert_eq!(g.r12, 0x1234);
        assert_eq!(g.rax, 0xfeed);
        assert_eq!(
            g.rbx, 0x02,
            "the untouched registers came back as they were"
        );
        assert_eq!(g.r15, 0x10);
    }

    #[test]
    fn a_handler_cannot_choose_the_flags_that_are_not_its_own() {
        // `sigreturn` reads this struct off the user stack, so `eflags` is
        // whatever the process wants it to be. The arithmetic flags are its
        // business; IF and IOPL are the kernel's, and a process that could
        // clear IF through its own frame would be disabling interrupts.
        let mut ctx = numbered_context();
        let mut m = MachineContext::from_context(&mut ctx);
        m.eflags = 0x0001; // CF alone: no IF, no IOPL, and bit 1 cleared.

        let mut resumed = UserContext::default();
        resumed.general_mut().rflags = 0x3202;
        m.restore_into(&mut resumed);

        let flags = resumed.general().rflags;
        assert_eq!(flags & 0x1, 0x1, "the handler's CF was dropped");
        assert_eq!(flags & 0x200, 0x200, "a handler cleared IF");
        assert_eq!(flags & 0x3000, 0x3000, "a handler changed IOPL");
        assert_eq!(flags & 0x2, 0x2, "the always-set bit was cleared");
        assert_eq!(
            RESTORABLE_EFLAGS & 0x100,
            0,
            "TF is not restorable: nothing here turns the #DB into a SIGTRAP"
        );
    }
}
