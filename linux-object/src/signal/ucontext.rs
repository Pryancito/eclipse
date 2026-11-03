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

use super::{SignalStack, SignalStackFlags, Sigset};
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

    #[cfg(test)]
    impl FpregsMem {
        /// A save area that is not the zeroed one, for the test that asks
        /// whether `unmodified_check` notices the FPU area changing. The
        /// field is private and there is no other way to build a non-default
        /// one; nothing about the comparison itself is bypassed, since that
        /// compares the whole struct.
        pub(crate) fn scribbled() -> Self {
            let mut mem = [0usize; 64];
            mem[63] = 1;
            Self { mem }
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
    #[cfg(target_arch = "x86_64")]
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

/// The i386 frame, which a 32-bit process on this 64-bit kernel gets instead
/// of the x86_64 one above: Linux's `CONFIG_COMPAT` frame,
/// `struct rt_sigframe_ia32` and the `struct ucontext_ia32` inside it.
///
/// It is not the x86_64 layout with its words halved. It is a different
/// structure with different fields in a different order --- `struct
/// sigcontext_32` keeps the segment selectors a 32-bit program can still see
/// (`gs`, `fs`, `es`, `ds`, `ss`), which the 64-bit one does not have at all,
/// and its general registers are the eight i386 ones in the i386 order. So
/// nothing in [`crate::signal`]'s narrowing machinery applies: this is a
/// layout of its own, which is why it is a module of its own next to the
/// other three.
///
/// ## Why the frame has a header
///
/// The three arguments of a `SA_SIGINFO` handler reach it in registers on
/// x86_64 (`rdi`, `rsi`, `rdx`) and **on the stack** on i386, because that is
/// what i386's calling convention is. So the i386 frame does not start at the
/// `siginfo`: it starts at four words the handler reads as its own return
/// address and arguments,
///
/// ```text
///  esp ->  pretcode   the restorer, which the handler's `ret` returns to
///          sig        argument 1
///          pinfo      argument 2, pointing at the `info` below
///          puc        argument 3, pointing at the `uc` below
///          info       compat_siginfo, 128 bytes
///          uc         struct ucontext_ia32, 116 bytes
/// ```
///
/// and `esp` points at `pretcode`, so `sig` is at `esp+4` where a cdecl
/// function looks for its first argument. Push the x86_64 frame to a 32-bit
/// handler instead and it reads its signal number out of whatever the
/// `ucontext` happens to have at that offset.
pub mod i386 {
    use super::Sigset;

    /// `compat_stack_t`: `uc_stack` at 32 bits.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
    pub struct SignalStack32 {
        pub sp: u32,
        pub flags: u32,
        pub size: u32,
    }

    impl SignalStack32 {
        /// The alternate stack as the kernel puts it in the frame
        /// (`__save_altstack`), from the native one it keeps.
        ///
        /// A 32-bit process lives below 4 GiB, so neither the pointer nor the
        /// size can lose anything here --- but it is truncated rather than
        /// asserted, because `ss_sp` comes out of the kernel's own record and
        /// a panic in the middle of delivering a signal is not a diagnosis.
        pub fn from_native(alt: &super::SignalStack) -> Self {
            Self {
                sp: alt.sp as u32,
                flags: alt.flags.bits(),
                size: alt.size as u32,
            }
        }

        /// And back, for the `sigreturn` that honours what the handler left
        /// in `uc_stack` (`restore_altstack`).
        pub fn to_native(self) -> super::SignalStack {
            super::SignalStack {
                sp: self.sp as usize,
                flags: super::SignalStackFlags::from_bits_truncate(self.flags),
                size: self.size as usize,
            }
        }
    }

    /// `struct sigcontext_32` (`arch/x86/include/uapi/asm/sigcontext.h`),
    /// 88 bytes, which is what i386's `mcontext_t` is.
    ///
    /// The `__gsh`/`__fsh`/`__esh`/`__dsh`/`__csh`/`__ssh` halves are the high
    /// sixteen bits of what were 32-bit slots in the original i386 frame and
    /// are now unused. They are named rather than left as padding for the
    /// reason [`crate::signal::SigInfo`]'s `_pad` is: the frame is copied to
    /// userspace as bytes, and padding a struct literal does not write is
    /// padding whose bytes are whatever the kernel stack held.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
    pub struct SigContext32 {
        pub gs: u16,
        pub gsh: u16,
        pub fs: u16,
        pub fsh: u16,
        pub es: u16,
        pub esh: u16,
        pub ds: u16,
        pub dsh: u16,
        pub di: u32,
        pub si: u32,
        pub bp: u32,
        pub sp: u32,
        pub bx: u32,
        pub dx: u32,
        pub cx: u32,
        pub ax: u32,
        pub trapno: u32,
        pub err: u32,
        pub ip: u32,
        pub cs: u16,
        pub csh: u16,
        pub flags: u32,
        pub sp_at_signal: u32,
        pub ss: u16,
        pub ssh: u16,
        /// `really (struct _fpstate_32 *)`: a pointer to the FPU save area,
        /// and `NULL` here because this kernel saves no FPU state in a frame
        /// (the x86_64 twin's `fpstate` is zero for the same reason).
        pub fpstate: u32,
        pub oldmask: u32,
        pub cr2: u32,
    }

    impl SigContext32 {
        pub fn get_pc(&self) -> usize {
            self.ip as usize
        }

        pub fn set_pc(&mut self, pc: usize) {
            self.ip = pc as u32;
        }
    }

    /// `struct ucontext_ia32`, 116 bytes. The mask is **last**, as the
    /// asm-generic layout has it and unlike the x86_64 one, and the comment
    /// in the kernel's header says why: "mask last for extensibility".
    ///
    /// `uc_sigmask` is spelled as two `u32` rather than as a [`Sigset`]
    /// because `compat_sigset_t` is `compat_sigset_word sig[2]`, and because
    /// a `Sigset` is eight-byte aligned: put one here and the compiler moves
    /// it from byte 108 to byte 112 to satisfy an alignment i386 does not
    /// have, and every handler reading its own mask reads four bytes past
    /// where the kernel wrote it.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
    pub struct SignalUserContext32 {
        pub flags: u32,
        pub link: u32,
        pub stack: SignalStack32,
        pub context: SigContext32,
        pub sig_mask: [u32; 2],
    }

    impl SignalUserContext32 {
        /// `uc_sigmask` as the kernel's own 64-bit bitmap.
        pub fn mask(&self) -> Sigset {
            Sigset::new(self.sig_mask[0] as u64 | ((self.sig_mask[1] as u64) << 32))
        }

        /// Put a mask into the frame as the two words a 32-bit program reads.
        pub fn set_mask(&mut self, mask: Sigset) {
            let v = mask.val();
            self.sig_mask = [v as u32, (v >> 32) as u32];
        }
    }

    /// The four words below the `siginfo` in `struct rt_sigframe_ia32`: the
    /// handler's return address and its three arguments, which i386 passes on
    /// the stack. See this module's own documentation.
    #[repr(C)]
    #[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
    pub struct RtSigFrameHeader32 {
        pub pretcode: u32,
        pub sig: u32,
        pub pinfo: u32,
        pub puc: u32,
    }

    #[cfg(target_arch = "x86_64")]
    impl SigContext32 {
        /// The registers the signal interrupted, as Linux's
        /// `ia32_setup_sigcontext()` copies them into the frame.
        ///
        /// A 32-bit process runs on the same hardware registers as any other,
        /// so each field here is the low half of one of them --- `eax` is
        /// `rax & 0xffff_ffff`. That is what the process itself sees, and
        /// truncating is not losing anything it could have put there.
        pub fn from_context(ctx: &mut super::UserContext) -> Self {
            let g = ctx.general();
            Self {
                di: g.rdi as u32,
                si: g.rsi as u32,
                bp: g.rbp as u32,
                sp: g.rsp as u32,
                bx: g.rbx as u32,
                dx: g.rdx as u32,
                cx: g.rcx as u32,
                ax: g.rax as u32,
                ip: g.rip as u32,
                flags: g.rflags as u32,
                sp_at_signal: g.rsp as u32,
                cs: USER32_CS,
                ss: USER32_DS,
                ds: USER32_DS,
                es: USER32_DS,
                ..Default::default()
            }
        }

        /// Put them back on `sigreturn(2)`, the other half of the contract,
        /// as `ia32_restore_sigcontext()` does.
        ///
        /// Only the eight general registers, the instruction pointer and the
        /// arithmetic flags: the selectors are not a process's to choose, and
        /// the registers a 32-bit program has no name for (`r8`--`r15`) keep
        /// what the interrupted context had rather than being zeroed, since
        /// the frame never carried them.
        ///
        /// Writing the low half of each register, not the whole of it: a
        /// 32-bit value written as a 64-bit one would leave a handler that
        /// changed nothing resuming with the high halves cleared, which for
        /// `rsp` is a stack pointer of zero.
        pub fn restore_into(&self, ctx: &mut super::UserContext) {
            let kept_flags = ctx.general().rflags & !super::x86_64::RESTORABLE_EFLAGS;
            let g = ctx.general_mut();
            g.rdi = self.di as usize;
            g.rsi = self.si as usize;
            g.rbp = self.bp as usize;
            g.rsp = self.sp as usize;
            g.rbx = self.bx as usize;
            g.rdx = self.dx as usize;
            g.rcx = self.cx as usize;
            g.rax = self.ax as usize;
            g.rip = self.ip as usize;
            g.rflags = kept_flags | (self.flags as usize & super::x86_64::RESTORABLE_EFLAGS);
        }
    }

    /// The selectors Linux leaves in a compat process's frame: `USER32_CS`
    /// and `USER_DS` (`arch/x86/include/asm/segment.h`).
    ///
    /// They are reported, not read off the CPU. This kernel runs a 32-bit
    /// process's instructions with the 64-bit selectors loaded --- nothing
    /// here switches descriptors --- so the live `cs` is `0x33`, which is a
    /// number no 32-bit handler can make sense of. What the ABI means by
    /// `uc_mcontext.gregs[REG_CS]` is "the code selector of a 32-bit user
    /// process", and that is `0x23`. Nothing is restored from these on
    /// `sigreturn`, here or in Linux: a process does not choose its own
    /// descriptors.
    pub const USER32_CS: u16 = 0x23;
    pub const USER32_DS: u16 = 0x2b;
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
            // Copied out, not borrowed: `get_field` below takes `&mut ctx`,
            // and `GeneralRegs` is `Copy`.
            let g = *ctx.general();
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

    /// The `pstate` bits a handler may change through its own `ucontext`: the
    /// condition flags `N`, `Z`, `C` and `V`, which are userspace's own. The
    /// rest of `spsr_el1` -- the exception level, the `DAIF` interrupt masks,
    /// the execution state -- is the kernel's, and `sigreturn` reads this
    /// struct straight off the user stack. The x86_64 twin is
    /// [`super::x86_64::RESTORABLE_EFLAGS`].
    pub const RESTORABLE_PSTATE: usize = 0xf000_0000;

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
                pstate: ctx.get_field(kernel_hal::context::UserContextField::CpuFlags),
                ..Default::default()
            };
            m.regs = [
                g.x0, g.x1, g.x2, g.x3, g.x4, g.x5, g.x6, g.x7, g.x8, g.x9, g.x10, g.x11, g.x12,
                g.x13, g.x14, g.x15, g.x16, g.x17, g.x18, g.x19, g.x20, g.x21, g.x22, g.x23, g.x24,
                g.x25, g.x26, g.x27, g.x28, g.x29, g.x30,
            ];
            m
        }

        /// Put them back on `sigreturn(2)`. Of `pstate` only
        /// [`RESTORABLE_PSTATE`] comes back, the arm64 half of what
        /// `RESTORABLE_EFLAGS` does for x86_64: the rest of `spsr_el1` picks
        /// the exception level and which interrupts are masked, and this
        /// struct is read off the user stack.
        pub fn restore_into(&self, ctx: &mut super::UserContext) {
            let kept =
                ctx.get_field(kernel_hal::context::UserContextField::CpuFlags) & !RESTORABLE_PSTATE;
            ctx.set_field(
                kernel_hal::context::UserContextField::CpuFlags,
                kept | (self.pstate & RESTORABLE_PSTATE),
            );
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

    /// `struct sigcontext_32`, field by field, because it is the one layout
    /// here whose fields are not a straight list of words: six of them are
    /// 16-bit selectors with a named 16-bit half beside them, and a compiler
    /// left to pad would put the first general register four bytes early.
    #[test]
    fn the_i386_sigcontext_is_the_kernels_struct_sigcontext_32() {
        type C = i386::SigContext32;
        assert_eq!(align_of::<C>(), 4, "nothing in i386 is 8-byte aligned");
        assert_eq!(offset_of!(C, gs), 0);
        assert_eq!(offset_of!(C, fs), 4);
        assert_eq!(offset_of!(C, es), 8);
        assert_eq!(offset_of!(C, ds), 12);
        // The eight general registers, in the i386 frame's order, which is
        // NOT the order of the x86_64 one above.
        assert_eq!(offset_of!(C, di), 16);
        assert_eq!(offset_of!(C, si), 20);
        assert_eq!(offset_of!(C, bp), 24);
        assert_eq!(offset_of!(C, sp), 28);
        assert_eq!(offset_of!(C, bx), 32);
        assert_eq!(offset_of!(C, dx), 36);
        assert_eq!(offset_of!(C, cx), 40);
        assert_eq!(offset_of!(C, ax), 44);
        assert_eq!(offset_of!(C, trapno), 48);
        assert_eq!(offset_of!(C, err), 52);
        assert_eq!(offset_of!(C, ip), 56, "where a crash reporter looks");
        assert_eq!(offset_of!(C, cs), 60);
        assert_eq!(offset_of!(C, flags), 64);
        assert_eq!(offset_of!(C, sp_at_signal), 68);
        assert_eq!(offset_of!(C, ss), 72);
        assert_eq!(offset_of!(C, fpstate), 76);
        assert_eq!(offset_of!(C, oldmask), 80);
        assert_eq!(offset_of!(C, cr2), 84);
        assert_eq!(size_of::<C>(), 88);
    }

    /// `struct ucontext_ia32`. The mask is last here and the `mcontext`
    /// before it, the opposite way round from musl's x86_64 `__ucontext`.
    #[test]
    fn the_i386_ucontext_puts_its_mask_last_and_at_four_byte_alignment() {
        type U = i386::SignalUserContext32;
        assert_eq!(offset_of!(U, flags), 0);
        assert_eq!(offset_of!(U, link), 4);
        assert_eq!(offset_of!(U, stack), 8);
        assert_eq!(size_of::<i386::SignalStack32>(), 12, "compat_stack_t");
        assert_eq!(offset_of!(U, context), 20);
        // The whole reason `uc_sigmask` is `[u32; 2]` and not a `Sigset`: a
        // `Sigset` is 8-byte aligned, which would move this to 112 and put
        // every handler's mask four bytes past where the kernel wrote it.
        assert_eq!(offset_of!(U, sig_mask), 108);
        assert_eq!(align_of::<U>(), 4);
        assert_eq!(size_of::<U>(), 116);
    }

    /// `uc_stack` at either width is the same three fields, but the native
    /// one has four bytes of padding after `ss_flags` that i386 does not.
    #[test]
    fn the_alternate_stack_crosses_the_width_both_ways() {
        let native = SignalStack {
            sp: 0xdead_0000,
            flags: SignalStackFlags::AUTODISARM,
            size: 0x4000,
        };
        let narrow = i386::SignalStack32::from_native(&native);
        assert_eq!(narrow.sp, 0xdead_0000);
        assert_eq!(narrow.size, 0x4000);
        assert_eq!(narrow.flags, SignalStackFlags::AUTODISARM.bits());
        assert_eq!(
            narrow.to_native(),
            native,
            "a 32-bit process lives below 4 GiB, so nothing is lost either way"
        );
        assert_eq!(
            offset_of!(SignalStack, size) - offset_of!(SignalStack, flags),
            8,
            "the native ss_flags has padding after it; the i386 one does not"
        );
    }

    /// `uc_sigmask` is two words, low half first, and the 64-bit bitmap the
    /// kernel keeps goes through them unchanged. A signal above 32 --- every
    /// realtime signal --- lives in the SECOND word, so the two being swapped
    /// is a mask that holds off the wrong signals and no test of SIGINT alone
    /// would see it.
    #[test]
    fn the_two_words_of_the_mask_are_the_low_half_first() {
        let mut uc = i386::SignalUserContext32::default();
        // SIGINT (bit 1) and SIGRTMIN+2 (bit 36).
        let set = Sigset::new((1 << 1) | (1u64 << 36));
        uc.set_mask(set);
        assert_eq!(uc.sig_mask[0], 1 << 1);
        assert_eq!(uc.sig_mask[1], 1 << 4);
        assert_eq!(uc.mask().val(), set.val(), "and back again");
    }

    /// The header the x86_64 frame has no equivalent of: four words that are
    /// the handler's return address and its three arguments, because i386
    /// passes arguments on the stack.
    #[test]
    fn the_i386_frame_starts_with_the_handlers_arguments() {
        type H = i386::RtSigFrameHeader32;
        assert_eq!(offset_of!(H, pretcode), 0, "esp points here");
        assert_eq!(offset_of!(H, sig), 4, "argument 1, at esp+4 as cdecl wants");
        assert_eq!(offset_of!(H, pinfo), 8);
        assert_eq!(offset_of!(H, puc), 12);
        assert_eq!(size_of::<H>(), 16);
    }

    /// What the whole `rt_sigframe_ia32` adds up to, since the frame is
    /// pushed as that many bytes of user stack.
    #[test]
    fn the_i386_frame_is_the_size_a_32_bit_libc_expects() {
        assert_eq!(
            size_of::<i386::RtSigFrameHeader32>() + 128 + size_of::<i386::SignalUserContext32>(),
            16 + 128 + 116,
        );
        assert_ne!(
            size_of::<i386::SignalUserContext32>(),
            size_of::<x86_64::SignalUserContext>() / 2,
            "it is not the 64-bit frame with its words halved: it is another structure",
        );
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
mod ucontext_i386_register_tests {
    //! The same half of the ABI for the compat frame: a `sigcontext_32` of
    //! the right shape full of zeros passes every layout test above.

    use super::i386::{SigContext32, USER32_CS, USER32_DS};
    use kernel_hal::context::UserContext;

    /// A context as a 32-bit process leaves it: every register with a
    /// distinct value, and a high half in each one that the frame has no
    /// room for and the restore must not clear.
    fn a_32_bit_context() -> UserContext {
        let mut ctx = UserContext::default();
        let g = ctx.general_mut();
        g.rax = 0x1111_1111;
        g.rbx = 0x2222_2222;
        g.rcx = 0x3333_3333;
        g.rdx = 0x4444_4444;
        g.rsi = 0x5555_5555;
        g.rdi = 0x6666_6666;
        g.rbp = 0x7777_7777;
        g.rsp = 0xffff_d000;
        g.rip = 0x0804_8123;
        g.rflags = 0x3202; // IOPL=3, IF, CF, and the always-set bit 1.
        ctx
    }

    #[test]
    fn the_frame_carries_the_eight_registers_a_32_bit_handler_knows_about() {
        let mut ctx = a_32_bit_context();
        let c = SigContext32::from_context(&mut ctx);
        assert_eq!(c.ax, 0x1111_1111);
        assert_eq!(c.bx, 0x2222_2222);
        assert_eq!(c.cx, 0x3333_3333);
        assert_eq!(c.dx, 0x4444_4444);
        assert_eq!(c.si, 0x5555_5555);
        assert_eq!(c.di, 0x6666_6666);
        assert_eq!(c.bp, 0x7777_7777);
        assert_eq!(c.sp, 0xffff_d000);
        assert_eq!(c.ip, 0x0804_8123, "where the signal interrupted it");
        assert_eq!(c.get_pc(), 0x0804_8123);
        assert_eq!(c.flags, 0x3202);
        assert_eq!(
            c.sp_at_signal, c.sp,
            "the i386 frame keeps the stack pointer twice over"
        );
    }

    #[test]
    fn the_frame_reports_the_selectors_of_a_32_bit_process() {
        // Not the live ones: this kernel runs 32-bit code with the 64-bit
        // selectors loaded, so `cs` on the CPU is 0x33, which is a number a
        // 32-bit handler cannot make sense of.
        let mut ctx = a_32_bit_context();
        let c = SigContext32::from_context(&mut ctx);
        // The literals, not the constants: comparing a field against the
        // constant it was built from is an assertion that passes whatever
        // either of them is.
        assert_eq!(c.cs, 0x23, "USER32_CS");
        assert_eq!([c.ss, c.ds, c.es], [0x2b; 3], "USER_DS");
        assert_eq!((USER32_CS, USER32_DS), (0x23, 0x2b));
        assert_eq!(
            [c.gs, c.fs],
            [0, 0],
            "their bases are in the MSRs, not in the selectors"
        );
        assert_eq!(
            [c.gsh, c.fsh, c.esh, c.dsh, c.csh, c.ssh],
            [0; 6],
            "the unused halves are written, not left as padding"
        );
        assert_eq!(c.fpstate, 0, "no FPU save area is pushed");
    }

    #[test]
    fn sigreturn_keeps_the_high_halves_the_frame_never_carried() {
        // The sharp edge of the whole conversion: the frame has 32-bit
        // fields and the machine has 64-bit registers. A restore that writes
        // each field as a full register resumes the process with every high
        // half cleared -- and for `rsp` that is a stack pointer of zero on
        // the next push, from a handler that changed nothing.
        let mut ctx = a_32_bit_context();
        let c = SigContext32::from_context(&mut ctx);

        let mut resumed = UserContext::default();
        {
            let g = resumed.general_mut();
            // The registers a 32-bit program has no name for: they are not in
            // the frame, so they must come back as the interrupted context
            // had them rather than as zeros.
            g.r12 = 0xcafe;
            g.r15 = 0xbeef;
        }
        c.restore_into(&mut resumed);
        let g = resumed.general();
        assert_eq!(g.rsp, 0xffff_d000);
        assert_eq!(g.rip, 0x0804_8123);
        assert_eq!(g.rax, 0x1111_1111);
        assert_eq!(g.r12, 0xcafe, "r12 is not in a 32-bit frame");
        assert_eq!(g.r15, 0xbeef);
    }

    #[test]
    fn a_handler_that_moves_the_32_bit_stack_pointer_is_obeyed() {
        // What the native twin's test is about, and the reason `restore_into`
        // exists at all: a handler rewrites its own `ucontext` to change
        // where the interrupted code resumes.
        let mut ctx = a_32_bit_context();
        let mut c = SigContext32::from_context(&mut ctx);
        c.sp = 0xffff_cff8;
        c.set_pc(0x0804_9000);
        c.ax = 0;

        let mut resumed = a_32_bit_context();
        c.restore_into(&mut resumed);
        let g = resumed.general();
        assert_eq!(g.rsp, 0xffff_cff8);
        assert_eq!(g.rip, 0x0804_9000);
        assert_eq!(g.rax, 0, "a handler setting a register to zero is obeyed");
    }

    #[test]
    fn a_32_bit_handler_cannot_choose_the_flags_that_are_not_its_own() {
        // The same filter as the native frame, through the same mask: the
        // frame comes off the user stack, so `eflags` is whatever the process
        // wants it to be, and IF and IOPL are the kernel's.
        let mut ctx = a_32_bit_context();
        let mut c = SigContext32::from_context(&mut ctx);
        c.flags = 0x0001; // CF alone: no IF, no IOPL.

        let mut resumed = UserContext::default();
        resumed.general_mut().rflags = 0x3202;
        c.restore_into(&mut resumed);

        let flags = resumed.general().rflags;
        assert_eq!(flags & 0x1, 0x1, "the handler's CF was dropped");
        assert_eq!(flags & 0x200, 0x200, "a handler cleared IF");
        assert_eq!(flags & 0x3000, 0x3000, "a handler changed IOPL");
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
