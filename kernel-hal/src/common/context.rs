//! User context.

use crate::{MMUFlags, VirtAddr};
use core::fmt;
use trapframe::UserContext as UserContextInner;

pub use trapframe::GeneralRegs;

cfg_if! {
    if #[cfg(feature = "libos")] {
        pub use trapframe::syscall_fn_entry as syscall_entry;
    } else {
        pub use dummpy_syscall_entry as syscall_entry;
        pub fn dummpy_syscall_entry() {
            unreachable!("dummpy_syscall_entry")
        }
    }
}

/// For reading and writing fields in [`UserContext`].
#[derive(Debug)]
pub enum UserContextField {
    InstrPointer,
    StackPointer,
    ThreadPointer,
    AbiRegister,
    ReturnValue,
}

/// Reason of the trap.
#[derive(Debug, PartialEq, Eq)]
pub enum TrapReason {
    Syscall,
    Interrupt(usize),
    PageFault(VirtAddr, MMUFlags),
    UndefinedInstruction,
    SoftwareBreakpoint,
    HardwareBreakpoint,
    UnalignedAccess,
    GernelFault(usize),
}

#[cfg(not(feature = "libos"))]
pub const TIMER_INTERRUPT_VEC: usize = crate::timer_interrupt_vector();

impl TrapReason {
    /// Decode an x86 trap from the vector and error code the trap frame
    /// carries.
    ///
    /// `fault_vaddr` is asked for `CR2` -- the faulting linear address -- and
    /// only on the vector that has one, so no trap that is not a page fault
    /// pays for the read. It answers `None` when the register cannot be read
    /// back as an address.
    #[cfg(target_arch = "x86_64")]
    pub fn from_x86(
        trap_num: usize,
        error_code: usize,
        fault_vaddr: impl FnOnce() -> Option<VirtAddr>,
    ) -> Self {
        use x86::irq::*;
        const X86_INT_BASE: u8 = 0x20;
        const X86_INT_MAX: u8 = 0xff;

        // See https://github.com/rcore-os/trapframe-rs/blob/25cb5282aca8ceb4f7fc4dcd61e7e73b67d9ae00/src/arch/x86_64/syscall.S#L117
        if trap_num == 0x100 {
            return Self::Syscall;
        }
        // Everything below reads the vector as a `u8`, and 0x100 is the only
        // number above 0xff the trap frame ever carries. Say so rather than
        // truncating: 0x10e is not vector 14, and answering "page fault" for
        // it would send the kernel to read `CR2` about a fault that never
        // happened.
        if trap_num > X86_INT_MAX as usize {
            return Self::GernelFault(trap_num);
        }
        match trap_num as u8 {
            DEBUG_VECTOR => Self::HardwareBreakpoint,
            BREAKPOINT_VECTOR => Self::SoftwareBreakpoint,
            INVALID_OPCODE_VECTOR => Self::UndefinedInstruction,
            ALIGNMENT_CHECK_VECTOR => Self::UnalignedAccess,
            PAGE_FAULT_VECTOR => {
                bitflags::bitflags! {
                    struct PageFaultErrorCode: u32 {
                        const PRESENT =     1 << 0;
                        const WRITE =       1 << 1;
                        const USER =        1 << 2;
                        const RESERVED =    1 << 3;
                        const INST =        1 << 4;
                    }
                }
                let code = PageFaultErrorCode::from_bits_truncate(error_code as u32);
                let mut flags = MMUFlags::empty();
                if code.contains(PageFaultErrorCode::WRITE) {
                    flags |= MMUFlags::WRITE
                } else if !code.contains(PageFaultErrorCode::INST) {
                    // Instruction-fetch faults (INST=1) require EXECUTE
                    // permission, not READ. Only set READ for genuine data
                    // reads (WRITE=0 and INST=0).  Mixing in READ for
                    // INST faults overly restricts the vmar permission check
                    // (which tests `mapping_flags.contains(access_flags)`)
                    // and produces a misleading "flags=READ | EXECUTE" in
                    // the null-range #PF diagnostic.
                    flags |= MMUFlags::READ
                }
                if code.contains(PageFaultErrorCode::USER) {
                    flags |= MMUFlags::USER
                }
                if code.contains(PageFaultErrorCode::INST) {
                    flags |= MMUFlags::EXECUTE
                }
                if code.contains(PageFaultErrorCode::RESERVED) {
                    error!("page table entry has reserved bits set!");
                }
                match fault_vaddr() {
                    Some(vaddr) => Self::PageFault(vaddr, flags),
                    // `CR2` is the only thing that says where the fault was,
                    // so a fault whose address cannot be read is not one this
                    // can report as a page fault. It used to `expect()` here,
                    // which is a panic raised from inside the page-fault
                    // handler, where there is no handler left to take it.
                    None => {
                        error!("#PF with a fault address that is not an address");
                        Self::GernelFault(trap_num)
                    }
                }
            }
            vec @ X86_INT_BASE..=X86_INT_MAX => Self::Interrupt(vec as usize),
            _ => Self::GernelFault(trap_num),
        }
    }

    /// Get [`TrapReason`] from `trap_num` and `error_code` in trap frame for x86.
    #[cfg(target_arch = "x86_64")]
    pub fn from(trap_num: usize, error_code: usize) -> Self {
        Self::from_x86(trap_num, error_code, || {
            x86_64::registers::control::Cr2::read()
                .ok()
                .map(|addr| addr.as_u64() as VirtAddr)
        })
    }

    /// Decode a RISC-V trap from `scause`, split into its interrupt bit and
    /// its code, together with `stval`.
    ///
    /// Not gated on the architecture: there is no RISC-V register access left
    /// in here, and the numbers are the ones the privileged specification
    /// assigns (table "Supervisor cause register after trap"), so a host of
    /// any architecture can check the decision.
    pub fn from_riscv(is_interrupt: bool, code: usize, stval: VirtAddr) -> Self {
        const INSTRUCTION_MISALIGNED: usize = 0;
        const ILLEGAL_INSTRUCTION: usize = 2;
        const BREAKPOINT: usize = 3;
        const LOAD_MISALIGNED: usize = 4;
        const STORE_MISALIGNED: usize = 6;
        const USER_ENV_CALL: usize = 8;
        const INSTRUCTION_PAGE_FAULT: usize = 12;
        const LOAD_PAGE_FAULT: usize = 13;
        const STORE_PAGE_FAULT: usize = 15;

        if is_interrupt {
            return Self::Interrupt(code);
        }
        match code {
            USER_ENV_CALL => Self::Syscall,
            BREAKPOINT => Self::SoftwareBreakpoint,
            ILLEGAL_INSTRUCTION => Self::UndefinedInstruction,
            // There are three misaligned-access exceptions and this knew two
            // of them: a misaligned *load* was reported as a generic kernel
            // fault, so the one of the three a program is most likely to
            // cause was the one that could not be told apart from a fault the
            // kernel itself took.
            INSTRUCTION_MISALIGNED | LOAD_MISALIGNED | STORE_MISALIGNED => Self::UnalignedAccess,
            LOAD_PAGE_FAULT => Self::PageFault(stval, MMUFlags::READ),
            STORE_PAGE_FAULT => Self::PageFault(stval, MMUFlags::WRITE),
            INSTRUCTION_PAGE_FAULT => Self::PageFault(stval, MMUFlags::EXECUTE),
            _ => Self::GernelFault(code),
        }
    }

    /// Get [`TrapReason`] from `scause` in trap frame for RISC-V.
    #[cfg(target_arch = "riscv64")]
    pub fn from(scause: riscv::register::scause::Scause) -> Self {
        use riscv::register::scause::Trap;
        Self::from_riscv(
            matches!(scause.cause(), Trap::Interrupt(_)),
            scause.code(),
            riscv::register::stval::read(),
        )
    }

    /// Decode an AArch64 exception from the vector-table entry the trap frame
    /// carries (`trap_num`, which is the source and the kind, not the
    /// syndrome) plus `ESR_EL1` and `FAR_EL1`, which the caller reads.
    ///
    /// `irq_num` is asked only for an IRQ, because answering it means going to
    /// the interrupt controller.
    ///
    /// Not gated on the architecture: with the registers passed in there is
    /// nothing target-specific left, and the decision it makes -- what
    /// permission a fault needs, and whether user code took it -- is exactly
    /// what decides whether a process gets its page or a fatal fault.
    pub fn from_aarch64(
        trap_num: usize,
        esr: u32,
        fault_vaddr: VirtAddr,
        irq_num: impl FnOnce() -> usize,
    ) -> Self {
        use crate::{Fault, Kind, Source, Syndrome};

        /// ISS bit 6 of a data abort: set for a write, clear for a read.
        const WNR: u32 = 1 << 6;

        let source = Source::from_num(trap_num & 0xffff);
        // An exception from a lower exception level is one user code took;
        // anything else the kernel took itself. The permission check a page
        // fault ends in is `mapping_flags.contains(access_flags)`, so calling
        // every abort a user one made the kernel's own touch of a
        // kernel-only mapping a fatal fault.
        let user = if source.is_some_and(Source::is_user) {
            MMUFlags::USER
        } else {
            MMUFlags::empty()
        };
        // Translation, access-flag and permission faults are the three the
        // address space can answer -- map the page, set its accessed bit,
        // or refuse. Anything else (an address-size fault, an external abort
        // off the bus, a TLB conflict) is not a question about a mapping, and
        // handing it to the fault handler turns a hardware error into a
        // retry loop. Linux splits them the same way (`fault_info` in
        // `arch/arm64/mm/fault.c`).
        let addressable = |kind: Fault| {
            matches!(
                kind,
                Fault::Translation | Fault::AccessFlag | Fault::Permission
            )
        };
        match Kind::from_num((trap_num >> 16) & 0xffff) {
            Some(Kind::Synchronous) => match Syndrome::from(esr) {
                Syndrome::Breakpoint => Self::SoftwareBreakpoint,
                Syndrome::Svc(_) => Self::Syscall,
                // It used to ask for READ | WRITE on every data abort, which
                // no read-only mapping can satisfy: reading a page mapped
                // `PROT_READ` was a fatal fault rather than a page to map.
                // The architecture says which it was, in one bit.
                Syndrome::DataAbort { kind, level: _ } if addressable(kind) => {
                    let access = if esr & WNR != 0 {
                        MMUFlags::WRITE
                    } else {
                        MMUFlags::READ
                    };
                    Self::PageFault(fault_vaddr, access | user)
                }
                // And this knew only the permission fault, so an instruction
                // fetch from a page that was merely not mapped yet -- demand
                // paging, the ordinary case -- never reached the handler that
                // would have mapped it.
                Syndrome::InstructionAbort { kind, level: _ } if addressable(kind) => {
                    Self::PageFault(fault_vaddr, MMUFlags::EXECUTE | user)
                }
                Syndrome::PCAlignmentFault | Syndrome::SpAlignmentFault => Self::UnalignedAccess,
                _ => Self::GernelFault(esr as usize),
            },
            Some(Kind::Irq) => Self::Interrupt(irq_num()),
            _ => Self::GernelFault(esr as usize),
        }
    }

    /// Get [`TrapReason`] from `trap_num` in trap frame for AArch64.
    #[cfg(target_arch = "aarch64")]
    pub fn from(trap_num: usize) -> Self {
        use cortex_a::registers::{ESR_EL1, FAR_EL1};
        use tock_registers::interfaces::Readable;
        Self::from_aarch64(trap_num, ESR_EL1.get() as u32, FAR_EL1.get() as _, || {
            #[cfg(not(feature = "libos"))]
            {
                use crate::hal_fn::mem::phys_to_virt;
                use crate::KCONFIG;
                zcore_drivers::irq::gic_400::get_irq_num(
                    phys_to_virt(KCONFIG.gic_base + 0x1_0000),
                    phys_to_virt(KCONFIG.gic_base),
                )
            }
            #[cfg(feature = "libos")]
            {
                // TODO: interrupt in libOS
                usize::MAX
            }
        })
    }
}

/// User context saved on trap.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct UserContext(UserContextInner);

/// DEBUG: dirección donde el asm de trap guardó el último `GeneralRegs` (x86_64
/// bare). Se compara con [`UserContext::dbg_ctx_addr`].
#[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
pub fn dbg_asm_save_addr() -> usize {
    trapframe::dbg_save_addr()
}
#[cfg(not(all(target_arch = "x86_64", not(feature = "libos"))))]
pub fn dbg_asm_save_addr() -> usize {
    0
}

impl UserContext {
    /// Create an empty user context.
    pub fn new() -> Self {
        let context = UserContextInner::default();
        Self(context)
    }

    /// Initialize the context for entry into userspace.
    /// Note: if the number of args < 3, please fill with zeros
    /// Eg: ctx.setup_uspace(pc_, sp_, &[arg1, arg2, 0])
    pub fn setup_uspace(&mut self, pc: usize, sp: usize, args: &[usize; 3]) {
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                self.0.general.rip = pc;
                self.0.general.rsp = sp;
                self.0.general.rdi = args[0];
                self.0.general.rsi = args[1];
                self.0.general.rdx = args[2];
                // IOPL = 3, IF = 1
                // FIXME: set IOPL = 0 when IO port bitmap is supporte
                self.0.general.rflags = 0x3000 | 0x200 | 0x2;
            } else if #[cfg(target_arch = "aarch64")] {
                self.0.elr = pc;
                self.0.sp = sp;
                self.0.general.x0 = args[0];
                self.0.general.x1 = args[1];
                self.0.general.x2 = args[2];
                // Mask SError exceptions (currently unhandled).
                // TODO
                self.0.spsr = 1 << 8;
            } else if #[cfg(target_arch = "riscv64")] {
                self.0.sepc = pc;
                self.0.general.sp = sp;
                self.0.general.a0 = args[0];
                self.0.general.a1 = args[1];
                self.0.general.a2 = args[2];
                // SUM = 1, FS = 0b11, SPIE = 1
                self.0.sstatus = 1 << 18 | 0b11 << 13 | 1 << 5;
            }
        }
    }

    /// Setup return addr
    pub fn set_ra(&mut self, _ra: usize) {
        cfg_if! {
            if #[cfg(target_arch = "riscv64")] {
                self.0.general.ra = _ra;
            } else if #[cfg(target_arch = "x86_64")] {
                error!("Please set return addr via stack!");
            } else if #[cfg(target_arch = "aarch64")] {
                self.0.general.x30 = _ra;
            } else {
                unimplemented!("Unsupported arch!");
            }
        }
    }

    pub fn enable_extended_state(&mut self) {}

    /// Switch to user mode.
    pub fn enter_uspace(&mut self) {
        cfg_if! {
            if #[cfg(feature = "libos")] {
                self.0.run_fncall()
            } else {
                self.dbg_validate_user_ctx("before enter_uspace");
                // There used to be a `[rbpfix]` here that rewrote a user `rbp`
                // found inside the kernel physmap window (0xffff_8000_...) on
                // the theory that "ring 3 can never legitimately hold it". It
                // can. Alpine builds Firefox without frame pointers, so `rbp`
                // is an ordinary GPR there, and SpiderMonkey's NaN-boxing tag
                // mask is exactly 0xffff_8000_0000_0000 (`JSVAL_TAG_MASK`).
                // Every timer/IPI landing in libxul code that held that mask
                // in `rbp` came back with `rbp = 0`, silently breaking the
                // Value type tests the code was in the middle of -- that is a
                // content process crashing "at random" in JS. The kernel never
                // dereferences a user `rbp`; it must not edit it either.
                //
                // NMIs are the kernel's own business and must never reach the
                // thread: swallow them here and re-enter, exactly as the
                // kernel-mode `trap_handler` records and returns. The kernel
                // sends NMIs to its peers as a "where are you stuck?" probe and
                // to kick a CPU that is starving a TLB shootdown
                // (`nmi_kick_pending_targets`), and vector 2 is not in the
                // maskable-interrupt range, so `TrapReason::from` classified it
                // as `GernelFault(2)` -- which the Linux path turns into
                // SIGSEGV. Every user process running on another core when the
                // kernel broadcast an NMI died on the spot, at whatever
                // instruction it happened to be executing: lunarbg killed
                // mid-`subss` (a register-only SSE op that cannot fault),
                // labwc, the bar and PulseAudio going down together in the same
                // second, with a fault report naming innocent code.
                #[cfg(target_arch = "x86_64")]
                loop {
                    self.0.run();
                    if self.0.trap_num != 2 {
                        break;
                    }
                    // Same two jobs as the kernel-mode handler: record where
                    // this CPU was (a user RIP is a true answer to "stuck
                    // where?" -- it says the CPU is in userspace, not wedged in
                    // the kernel), then service a shootdown the peer escalated
                    // to an NMI because no maskable IPI could land. Both are
                    // NMI-safe: no locks, no allocation, nothing printed.
                    crate::kstats::note_nmi_rip(self.0.general.rip as u64);
                    crate::common::ipi::tlb_shootdown_ack_nmi();
                    self.dbg_validate_user_ctx("after NMI from user");
                }
                #[cfg(not(target_arch = "x86_64"))]
                self.0.run();
                self.dbg_validate_user_ctx("after trap from user");
            }
        }
    }

    /// DEBUG instrumentation: catch a corrupted saved user context (rip/rsp/rbp
    /// pointing into the kernel half) right before we (re)enter user mode and
    /// right after a trap returns. A "before" hit means the saved `GeneralRegs`
    /// were corrupted while sitting in kernel memory (or at save time); pairing
    /// it with the "after" hit localizes the intermittent register-state
    /// corruption behind the `apk` SIGSEGV / "BAD signature".
    #[inline]
    fn dbg_validate_user_ctx(&self, when: &str) {
        #[cfg(all(target_arch = "x86_64", not(feature = "libos")))]
        {
            use core::sync::atomic::{AtomicUsize, Ordering};
            static HB: AtomicUsize = AtomicUsize::new(0);
            let n = HB.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(200_000) {
                warn!("[ctxcheck] heartbeat n={} ({})", n, when);
            }
            const USER_MAX: usize = 0x0000_8000_0000_0000;
            // Do NOT treat a low rip/rsp/rbp as corruption: this loader maps PIE
            // executables (apk and the musl dynamic linker `ld-musl`) at base 0
            // (see `Loader::load_impl`: `app_base` is the start of an empty
            // address space, i.e. 0), so genuinely-valid user code, stack and
            // frame pointers legitimately live at very low virtual addresses —
            // a running PIE has rip well below the old 0x1_0000 floor. The
            // earlier `rip < USER_PC_MIN` heuristic therefore mis-fired on every
            // asynchronous trap taken while such a process ran low. The dump that
            // exposed this had trap_num=0xf3 (the IPI vector — an *interrupt*,
            // not a page fault), which proves the CPU was executing normally at
            // that low rip when the IPI arrived; a wild jump to an unmapped low
            // address would have raised #PF (trap_num=0xe) instead.
            //
            // The only reliable corruption signal is a user register pointing
            // into the *kernel* half (>= USER_MAX) — e.g. a user rbp that ended
            // up inside the physmap window (0xffff_8000_…) — so keep just that.
            let g = &self.0.general;
            // `rip` and `rsp` are consumed by the CPU on `sysret`/`iret`, so a
            // kernel-half value there is genuinely fatal. `rbp` (and every other
            // GPR) is never dereferenced by the kernel and may legitimately hold
            // anything: the System V outermost-frame marker `rbp = -1`, or --
            // in frame-pointer-less code such as Alpine's Firefox -- plain data
            // like SpiderMonkey's tag mask 0xffff_8000_0000_0000, which sits
            // exactly at the physmap base. Flagging `rbp` in that window fired
            // on every interrupt taken inside libxul and was the trigger for
            // the (since removed) `[rbpfix]` rewrite that zeroed it. So: rip
            // and rsp only.
            if g.rip >= USER_MAX || g.rsp >= USER_MAX {
                error!(
                    "[ctxcheck] {} CORRUPT user ctx cpu={} ctx_addr={:#x}: rip={:#x} rsp={:#x} rbp={:#x} fsbase={:#x} \
                     rax={:#x} rbx={:#x} rcx={:#x} rdx={:#x} rsi={:#x} rdi={:#x} \
                     r8={:#x} r9={:#x} r10={:#x} r11={:#x} r12={:#x} r13={:#x} r14={:#x} r15={:#x} \
                     rflags={:#x} trap_num={:#x} err={:#x}",
                    when,
                    crate::cpu::cpu_id(),
                    core::ptr::addr_of!(self.0.general) as usize,
                    g.rip, g.rsp, g.rbp, g.fsbase,
                    g.rax, g.rbx, g.rcx, g.rdx, g.rsi, g.rdi,
                    g.r8, g.r9, g.r10, g.r11, g.r12, g.r13, g.r14, g.r15,
                    g.rflags, self.0.trap_num, self.0.error_code,
                );
            }
        }
        let _ = when;
    }

    /// DEBUG: leer el `rbp` del contexto de usuario guardado (frame pointer en
    /// x86_64). Usado por el handler de page-fault para comparar el `rbp` guardado
    /// con la dirección que falló.
    pub fn dbg_general_rbp(&self) -> usize {
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                self.0.general.rbp
            } else {
                0
            }
        }
    }

    /// DEBUG: dirección en memoria del `GeneralRegs` de este `UserContext`. Se
    /// compara con [`dbg_asm_save_addr`] (donde el asm guardó de verdad) para
    /// detectar si el save/restore usan memorias distintas.
    pub fn dbg_ctx_addr(&self) -> usize {
        core::ptr::addr_of!(self.0.general) as usize
    }

    /// DEBUG: registros del bucle de histograma de apk para ver cuál sostiene el
    /// puntero corrupto `physmap`. Devuelve (rsi, rdi, r8, r9, r10, r11).
    pub fn dbg_loop_regs(&self) -> (usize, usize, usize, usize, usize, usize) {
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                let g = &self.0.general;
                (g.rsi, g.rdi, g.r8, g.r9, g.r10, g.r11)
            } else {
                (0, 0, 0, 0, 0, 0)
            }
        }
    }

    /// Returns the `error_code` field of the context.
    #[cfg(any(target_arch = "x86_64", doc))]
    #[doc(cfg(target_arch = "x86_64"))]
    pub fn error_code(&self) -> usize {
        self.0.error_code
    }

    /// Returns the saved process status register, `spsr_el1`.
    ///
    /// `zx_thread_read_state`/`write_state` report and restore AArch64's CPSR
    /// from it; the inner context's field is private to this wrapper.
    #[cfg(any(target_arch = "aarch64", doc))]
    #[doc(cfg(target_arch = "aarch64"))]
    pub fn status_register(&self) -> usize {
        self.0.spsr
    }

    /// Sets the saved process status register, `spsr_el1`.
    #[cfg(any(target_arch = "aarch64", doc))]
    #[doc(cfg(target_arch = "aarch64"))]
    pub fn set_status_register(&mut self, spsr: usize) {
        self.0.spsr = spsr;
    }

    /// Returns [`TrapReason`] according to the context.
    pub fn trap_reason(&self) -> TrapReason {
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                TrapReason::from(self.0.trap_num, self.0.error_code)
            } else if #[cfg(target_arch = "aarch64")] {
                TrapReason::from(self.0.trap_num)
            } else if #[cfg(target_arch = "riscv64")] {
                TrapReason::from(riscv::register::scause::read())
            } else {
                unimplemented!()
            }
        }
    }
    /// Returns a `usize` representing the trap reason. (i.e., IDT vector for x86, `scause` for RISC-V)
    pub fn raw_trap_reason(&self) -> usize {
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                self.0.trap_num
            } else if #[cfg(target_arch = "aarch64")] {
                // `trap_num` is the vector table entry -- source and kind --
                // not the syndrome, so this is the same read `TrapReason::from`
                // makes two functions up, for the same reason: the register is
                // what says *which* fault this was. It was `unimplemented!()`,
                // and `ExceptionContext::from_user_context` reaches it on every
                // page fault that becomes an exception report, so the first
                // unhandled fault in a Zircon process panicked the kernel.
                use cortex_a::registers::ESR_EL1;
                use tock_registers::interfaces::Readable;
                ESR_EL1.get() as usize
            } else if #[cfg(target_arch = "riscv64")] {
                riscv::register::scause::read().bits()
            } else {
                unimplemented!()
            }
        }
    }

    /// Returns the reference of general registers.
    pub fn general(&self) -> &GeneralRegs {
        &self.0.general
    }

    /// Returns the mutable reference of general registers.
    pub fn general_mut(&mut self) -> &mut GeneralRegs {
        &mut self.0.general
    }

    fn field_ref(&mut self, which: UserContextField) -> &mut usize {
        cfg_if! {
            if #[cfg(target_arch = "x86_64")] {
                match which {
                    UserContextField::InstrPointer => &mut self.0.general.rip,
                    UserContextField::StackPointer => &mut self.0.general.rsp,
                    UserContextField::ThreadPointer => &mut self.0.general.fsbase,
                    UserContextField::AbiRegister => &mut self.0.general.r10,
                    UserContextField::ReturnValue => &mut self.0.general.rax,
                }
            } else if #[cfg(target_arch = "aarch64")] {
                match which {
                    UserContextField::InstrPointer => &mut self.0.elr,
                    UserContextField::StackPointer => &mut self.0.sp,
                    UserContextField::ThreadPointer => &mut self.0.tpidr,
                    UserContextField::AbiRegister => &mut self.0.general.x18,
                    UserContextField::ReturnValue => &mut self.0.general.x0,
                }
            } else if #[cfg(target_arch = "riscv64")] {
                match which {
                    UserContextField::InstrPointer => &mut self.0.sepc,
                    UserContextField::StackPointer => &mut self.0.general.sp,
                    UserContextField::ThreadPointer => &mut self.0.general.tp,
                    UserContextField::AbiRegister => &mut self.0.general.a7,
                    UserContextField::ReturnValue => &mut self.0.general.a0,
                }
            } else {
                unimplemented!()
            }
        }
    }

    /// Read a field of the context.
    pub fn get_field(&mut self, which: UserContextField) -> usize {
        *self.field_ref(which)
    }

    /// Write a field of the context.
    pub fn set_field(&mut self, which: UserContextField, value: usize) {
        *self.field_ref(which) = value;
    }

    /// Advance the instruction pointer in trap handler on some architecture.
    pub fn advance_pc(&mut self, reason: TrapReason) {
        cfg_if! {
            if #[cfg(target_arch = "riscv64")] {
                if let TrapReason::Syscall = reason { self.0.sepc += 4 }
            } else {
                let _ = reason;
            }
        }
    }
}

impl Default for UserContext {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for UserContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        /// X86 vector registers.
        #[repr(C, align(16))]
        #[derive(Debug, Copy, Clone)]
        pub struct VectorRegs {
            pub fcw: u16,
            pub fsw: u16,
            pub ftw: u8,
            pub _pad0: u8,
            pub fop: u16,
            pub fip: u32,
            pub fcs: u16,
            pub _pad1: u16,

            pub fdp: u32,
            pub fds: u16,
            pub _pad2: u16,
            pub mxcsr: u32,
            pub mxcsr_mask: u32,

            pub mm: [U128; 8],
            pub xmm: [U128; 16],
            pub reserved: [U128; 3],
            pub available: [U128; 3],
        }

        // https://xem.github.io/minix86/manual/intel-x86-and-64-manual-vol1/o_7281d5ea06a5b67a-274.html
        impl Default for VectorRegs {
            fn default() -> Self {
                VectorRegs {
                    fcw: 0x37f,
                    mxcsr: 0x1f80,
                    ..unsafe { core::mem::zeroed() }
                }
            }
        }

        // workaround: libcore has bug on Debug print u128 ??
        #[derive(Default, Clone, Copy)]
        #[repr(C, align(16))]
        pub struct U128(pub [u64; 2]);

        impl fmt::Debug for U128 {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "{:#016x}_{:016x}", self.0[1], self.0[0])
            }
        }
    }
}

/// Host tests for the trap decoders.
///
/// What a trap *was* is the first thing the kernel decides about it and the
/// last thing anything else can second-guess: a page fault that is read as a
/// fatal fault kills the process, and one read as the wrong kind of access is
/// refused by the permission check that comes after. Until now the decision
/// could only be observed on the machine that produced the trap, and two of
/// the three architectures are ones nobody here has.
///
/// With the registers passed in rather than read, all three decode on any
/// host. The AArch64 and RISC-V halves are the point: the CI runs neither,
/// so every divergence below was one only a real board would ever have
/// reported, by failing.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Kind, Source};

    // ---- AArch64 ----------------------------------------------------------

    /// EC 0b100100: data abort from a lower exception level.
    const EC_DATA_ABORT_LOWER: u32 = 0b10_0100;
    /// EC 0b100101: data abort taken in the current exception level.
    const EC_DATA_ABORT_CURRENT: u32 = 0b10_0101;
    /// EC 0b100000: instruction abort from a lower exception level.
    const EC_INSN_ABORT_LOWER: u32 = 0b10_0000;
    /// EC 0b010101: `SVC` from AArch64.
    const EC_SVC64: u32 = 0b01_0101;
    /// EC 0b110000: breakpoint from a lower exception level.
    const EC_BREAKPOINT: u32 = 0b11_0000;
    /// EC 0b100010: PC alignment fault.
    const EC_PC_ALIGNMENT: u32 = 0b10_0010;
    /// EC 0b100110: SP alignment fault.
    const EC_SP_ALIGNMENT: u32 = 0b10_0110;

    /// DFSC/IFSC 0b0001LL: translation fault, here at level 3. This is what a
    /// page that has not been mapped yet reports -- demand paging.
    const FSC_TRANSLATION_L3: u32 = 0b00_0111;
    /// DFSC/IFSC 0b0011LL: permission fault at level 3.
    const FSC_PERMISSION_L3: u32 = 0b00_1111;
    /// DFSC/IFSC 0b0010LL: access-flag fault at level 3.
    const FSC_ACCESS_FLAG_L3: u32 = 0b00_1011;
    /// DFSC 0b010000: synchronous external abort, not on a table walk. A bus
    /// error, not a question about a mapping.
    const FSC_EXTERNAL_ABORT: u32 = 0b01_0000;
    /// ISS bit 6 of a data abort: the access was a write.
    const WNR: u32 = 1 << 6;

    fn esr(ec: u32, iss: u32) -> u32 {
        (ec << 26) | (iss & 0x1ff_ffff)
    }

    /// The vector-table entry number the trap frame carries: the source in the
    /// low half, the kind in the high half.
    fn vector(source: Source, kind: Kind) -> usize {
        (kind as usize) << 16 | source as usize
    }

    fn from_el0(kind: Kind) -> usize {
        vector(Source::LowerAArch64, kind)
    }

    const FAR: VirtAddr = 0x7fff_dead_b000;

    fn no_irq() -> usize {
        panic!("the interrupt controller was asked about an exception that is not an IRQ")
    }

    #[test]
    fn a_read_of_a_read_only_page_asks_for_read_and_nothing_else() {
        // Every data abort used to ask for READ | WRITE, and the check it ends
        // in is `mapping_flags.contains(access_flags)`. No read-only mapping
        // contains WRITE, so reading a page mapped `PROT_READ` -- a string in
        // `.rodata`, a read-only file mapping -- was a fatal fault instead of
        // a page to map. The architecture says which it was, in ISS bit 6.
        let reason = TrapReason::from_aarch64(
            from_el0(Kind::Synchronous),
            esr(EC_DATA_ABORT_LOWER, FSC_TRANSLATION_L3),
            FAR,
            no_irq,
        );
        assert_eq!(
            reason,
            TrapReason::PageFault(FAR, MMUFlags::READ | MMUFlags::USER)
        );
    }

    #[test]
    fn a_write_asks_for_write_and_nothing_else() {
        let reason = TrapReason::from_aarch64(
            from_el0(Kind::Synchronous),
            esr(EC_DATA_ABORT_LOWER, FSC_PERMISSION_L3 | WNR),
            FAR,
            no_irq,
        );
        assert_eq!(
            reason,
            TrapReason::PageFault(FAR, MMUFlags::WRITE | MMUFlags::USER)
        );
    }

    #[test]
    fn a_fault_the_kernel_took_itself_is_not_a_user_access() {
        // USER used to be set on every abort. A kernel-only mapping does not
        // carry USER, so `contains` failed and the kernel's own touch of its
        // own memory was a fatal fault.
        let reason = TrapReason::from_aarch64(
            vector(Source::CurrentSpElx, Kind::Synchronous),
            esr(EC_DATA_ABORT_CURRENT, FSC_TRANSLATION_L3),
            FAR,
            no_irq,
        );
        assert_eq!(reason, TrapReason::PageFault(FAR, MMUFlags::READ));
    }

    #[test]
    fn an_instruction_fetch_from_an_unmapped_page_is_a_page_fault() {
        // Only a *permission* instruction abort was one. A translation fault
        // is what an instruction fetch from a page that is simply not mapped
        // yet reports, which is the ordinary case under demand paging, and it
        // fell through to a fatal kernel fault -- so the page that would have
        // been mapped never was.
        for fsc in [FSC_TRANSLATION_L3, FSC_ACCESS_FLAG_L3, FSC_PERMISSION_L3] {
            let reason = TrapReason::from_aarch64(
                from_el0(Kind::Synchronous),
                esr(EC_INSN_ABORT_LOWER, fsc),
                FAR,
                no_irq,
            );
            assert_eq!(
                reason,
                TrapReason::PageFault(FAR, MMUFlags::EXECUTE | MMUFlags::USER),
                "instruction abort with fault status {fsc:#08b}"
            );
        }
    }

    #[test]
    fn a_bus_error_is_not_a_page_to_map() {
        // An external abort is the hardware saying the access never reached
        // memory. Handing it to the fault handler asks it to map a page that
        // would fault again on the next instruction, for as long as it takes.
        let faulty = esr(EC_DATA_ABORT_LOWER, FSC_EXTERNAL_ABORT);
        assert_eq!(
            TrapReason::from_aarch64(from_el0(Kind::Synchronous), faulty, FAR, no_irq),
            TrapReason::GernelFault(faulty as usize)
        );
    }

    #[test]
    fn a_supervisor_call_is_a_syscall() {
        assert_eq!(
            TrapReason::from_aarch64(from_el0(Kind::Synchronous), esr(EC_SVC64, 0), FAR, no_irq),
            TrapReason::Syscall
        );
    }

    #[test]
    fn breakpoints_and_alignment_faults_keep_their_names() {
        let cases = [
            (EC_BREAKPOINT, TrapReason::SoftwareBreakpoint),
            (EC_PC_ALIGNMENT, TrapReason::UnalignedAccess),
            (EC_SP_ALIGNMENT, TrapReason::UnalignedAccess),
        ];
        for (ec, expected) in cases {
            assert_eq!(
                TrapReason::from_aarch64(from_el0(Kind::Synchronous), esr(ec, 0), FAR, no_irq),
                expected,
                "EC {ec:#08b}"
            );
        }
    }

    #[test]
    fn only_an_irq_asks_the_interrupt_controller() {
        // Answering it means a read of the GIC, so an exception that is not an
        // interrupt must not get there. `no_irq` panics if it does, which is
        // what every other test in here relies on too.
        assert_eq!(
            TrapReason::from_aarch64(from_el0(Kind::Irq), esr(EC_DATA_ABORT_LOWER, 0), FAR, || 42),
            TrapReason::Interrupt(42)
        );
    }

    #[test]
    fn a_vector_that_names_no_kind_is_reported_not_panicked() {
        // `Kind::from` used to `panic!("bad kind")`, from inside the trap
        // handler, where a panic has no handler left to take it.
        let faulty = esr(EC_DATA_ABORT_LOWER, FSC_TRANSLATION_L3);
        let reason = TrapReason::from_aarch64(9 << 16, faulty, FAR, no_irq);
        assert_eq!(reason, TrapReason::GernelFault(faulty as usize));
        assert_eq!(Kind::from_num(4), None);
        assert_eq!(Source::from_num(4), None);
        assert_eq!(Kind::from_num(usize::MAX), None);
    }

    #[test]
    fn the_four_kinds_and_the_four_sources_round_trip() {
        for (n, kind) in [
            (0, Kind::Synchronous),
            (1, Kind::Irq),
            (2, Kind::Fiq),
            (3, Kind::SError),
        ] {
            assert_eq!(Kind::from_num(n), Some(kind));
        }
        for (n, source, is_user) in [
            (0, Source::CurrentSpEl0, false),
            (1, Source::CurrentSpElx, false),
            (2, Source::LowerAArch64, true),
            (3, Source::LowerAArch32, true),
        ] {
            assert_eq!(Source::from_num(n), Some(source));
            assert_eq!(source.is_user(), is_user);
        }
    }

    // ---- RISC-V -----------------------------------------------------------

    const STVAL: VirtAddr = 0x2_0000_1000;

    #[test]
    fn all_three_misaligned_accesses_are_unaligned_accesses() {
        // Instruction (0) and store (6) were named; load (4) was not, so the
        // one of the three a program is likeliest to cause came back as a
        // fault the kernel itself had taken.
        for code in [0usize, 4, 6] {
            assert_eq!(
                TrapReason::from_riscv(false, code, STVAL),
                TrapReason::UnalignedAccess,
                "exception code {code}"
            );
        }
    }

    #[test]
    fn each_page_fault_asks_for_the_access_that_caused_it() {
        let cases = [
            (13usize, MMUFlags::READ),
            (15, MMUFlags::WRITE),
            (12, MMUFlags::EXECUTE),
        ];
        for (code, flags) in cases {
            assert_eq!(
                TrapReason::from_riscv(false, code, STVAL),
                TrapReason::PageFault(STVAL, flags),
                "exception code {code}"
            );
        }
    }

    #[test]
    fn an_environment_call_from_user_mode_is_a_syscall() {
        assert_eq!(TrapReason::from_riscv(false, 8, STVAL), TrapReason::Syscall);
        // From supervisor mode it is not: the kernel does not call itself.
        assert_eq!(
            TrapReason::from_riscv(false, 9, STVAL),
            TrapReason::GernelFault(9)
        );
    }

    #[test]
    fn the_interrupt_bit_is_what_makes_it_an_interrupt() {
        // The codes overlap: 8 is an environment call as an exception and the
        // supervisor software interrupt as an interrupt.
        assert_eq!(
            TrapReason::from_riscv(true, 8, STVAL),
            TrapReason::Interrupt(8)
        );
        assert_eq!(
            TrapReason::from_riscv(true, 5, STVAL),
            TrapReason::Interrupt(5)
        );
    }

    #[test]
    fn a_breakpoint_and_an_illegal_instruction_keep_their_names() {
        assert_eq!(
            TrapReason::from_riscv(false, 3, STVAL),
            TrapReason::SoftwareBreakpoint
        );
        assert_eq!(
            TrapReason::from_riscv(false, 2, STVAL),
            TrapReason::UndefinedInstruction
        );
        // An access fault is not a page fault: there is no mapping to make.
        assert_eq!(
            TrapReason::from_riscv(false, 5, STVAL),
            TrapReason::GernelFault(5)
        );
    }

    // ---- x86_64 -----------------------------------------------------------

    #[cfg(target_arch = "x86_64")]
    mod x86 {
        use super::*;
        use core::cell::Cell;

        const CR2: VirtAddr = 0x7fff_0000_1000;

        fn reads_cr2() -> Option<VirtAddr> {
            Some(CR2)
        }

        fn no_cr2() -> Option<VirtAddr> {
            panic!("CR2 was read for a trap that is not a page fault")
        }

        #[test]
        fn the_syscall_vector_is_not_a_vector() {
            assert_eq!(TrapReason::from_x86(0x100, 0, no_cr2), TrapReason::Syscall);
        }

        #[test]
        fn a_trap_number_above_the_vector_space_is_not_truncated() {
            // `0x10e as u8` is 14, the page-fault vector, which would send the
            // kernel to read CR2 about a fault that never happened.
            assert_eq!(
                TrapReason::from_x86(0x10e, 0, no_cr2),
                TrapReason::GernelFault(0x10e)
            );
            assert_eq!(
                TrapReason::from_x86(usize::MAX, 0, no_cr2),
                TrapReason::GernelFault(usize::MAX)
            );
        }

        #[test]
        fn the_named_vectors_keep_their_names() {
            let cases = [
                (1usize, TrapReason::HardwareBreakpoint),
                (3, TrapReason::SoftwareBreakpoint),
                (6, TrapReason::UndefinedInstruction),
                (17, TrapReason::UnalignedAccess),
                (0, TrapReason::GernelFault(0)),
                (13, TrapReason::GernelFault(13)),
            ];
            for (vec, expected) in cases {
                assert_eq!(
                    TrapReason::from_x86(vec, 0, no_cr2),
                    expected,
                    "vector {vec}"
                );
            }
        }

        #[test]
        fn a_device_vector_is_an_interrupt() {
            assert_eq!(
                TrapReason::from_x86(0x20, 0, no_cr2),
                TrapReason::Interrupt(0x20)
            );
            assert_eq!(
                TrapReason::from_x86(0xff, 0, no_cr2),
                TrapReason::Interrupt(0xff)
            );
            // One below the base is not one: the vectors under 0x20 are the
            // processor's own exceptions.
            assert_eq!(
                TrapReason::from_x86(0x1f, 0, no_cr2),
                TrapReason::GernelFault(0x1f)
            );
        }

        #[test]
        fn a_page_fault_asks_for_the_access_that_caused_it() {
            const PRESENT: usize = 1 << 0;
            const WRITE: usize = 1 << 1;
            const USER: usize = 1 << 2;
            const INST: usize = 1 << 4;
            let cases = [
                (0, MMUFlags::READ),
                (WRITE, MMUFlags::WRITE),
                (INST, MMUFlags::EXECUTE),
                (USER, MMUFlags::READ | MMUFlags::USER),
                (WRITE | USER | PRESENT, MMUFlags::WRITE | MMUFlags::USER),
                (INST | USER, MMUFlags::EXECUTE | MMUFlags::USER),
            ];
            for (code, flags) in cases {
                assert_eq!(
                    TrapReason::from_x86(14, code, reads_cr2),
                    TrapReason::PageFault(CR2, flags),
                    "error code {code:#07b}"
                );
            }
        }

        #[test]
        fn a_fault_address_that_cannot_be_read_is_reported_not_panicked() {
            // `Cr2::read()` answers an error for a value that is not a
            // canonical address. It used to be `expect`ed -- a panic raised
            // from inside the page-fault handler.
            assert_eq!(
                TrapReason::from_x86(14, 0, || None),
                TrapReason::GernelFault(14)
            );
        }

        #[test]
        fn cr2_is_read_once_and_only_for_a_page_fault() {
            // Reading it is a privileged register access on the trap path;
            // every other vector must not pay for it. `no_cr2` panics, which
            // is what the rest of these tests lean on.
            let reads = Cell::new(0);
            let count = || {
                reads.set(reads.get() + 1);
                Some(CR2)
            };
            assert_eq!(
                TrapReason::from_x86(14, 0, count),
                TrapReason::PageFault(CR2, MMUFlags::READ)
            );
            assert_eq!(reads.get(), 1);
        }
    }
}
