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
