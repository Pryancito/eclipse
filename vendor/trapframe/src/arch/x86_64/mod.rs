#[cfg(any(target_os = "linux", target_os = "macos"))]
mod fncall;
#[cfg(any(target_os = "none", target_os = "uefi"))]
mod gdt;
#[cfg(any(target_os = "none", target_os = "uefi"))]
mod idt;
#[cfg(feature = "ioport_bitmap")]
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub mod ioport;
#[cfg(any(target_os = "none", target_os = "uefi"))]
mod syscall;
#[cfg(any(target_os = "none", target_os = "uefi"))]
mod trap;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use fncall::syscall_fn_entry;
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub use gdt::{
    logical_cpu_id_valid, read_cpu_local, read_logical_cpu_id, write_cpu_local,
    write_logical_cpu_id,
};
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub use syscall::dbg_save_addr;
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub use trap::TrapFrame;

/// Initialize interrupt handling on x86_64.
///
/// # Safety
///
/// This function will:
///
/// - Disable interrupt.
/// - Switch to a new [GDT], extend 7 more entries from the current one.
/// - Switch to a new [TSS], set `GSBASE` to its base address.
/// - Switch to a new [IDT], override the current one.
/// - Enable [`syscall`] instruction.
///     - set `EFER::SYSTEM_CALL_EXTENSIONS`
///
/// [GDT]: https://wiki.osdev.org/GDT
/// [IDT]: https://wiki.osdev.org/IDT
/// [TSS]: https://wiki.osdev.org/Task_State_Segment
/// [`syscall`]: https://www.felixcloutier.com/x86/syscall
///
/// Non-zero once this CPU has enabled XSAVE + AVX state (CR4.OSXSAVE + XCR0);
/// the value is the XCR0 feature mask (low dword) that [`UserContext::run`]
/// hands to XSAVE/XRSTOR. Zero means the CPU has no XSAVE/AVX and the plain
/// FXSAVE/FXRSTOR path is used. Every CPU runs `init_fpu`, so all agree.
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub(crate) static XSAVE_MASK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Set by the kernel (before `init`) to force the FXSAVE path even on a CPU that
/// supports XSAVE + AVX. A boot-time escape hatch (e.g. `noavx` on the kernel
/// command line) for isolating AVX enablement while debugging.
#[cfg(any(target_os = "none", target_os = "uefi"))]
static AVX_DISABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Force the plain FXSAVE path (disable XSAVE/AVX enablement in `init_fpu`).
/// Must be called before `init`/`init_ap` run on any CPU.
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub fn set_avx_disabled(disabled: bool) {
    AVX_DISABLED.store(disabled, core::sync::atomic::Ordering::Relaxed);
}

/// Whether this CPU enabled XSAVE + AVX for user space (256-bit vector width for
/// llvmpipe and other AVX code). False when the CPU lacks it or `set_avx_disabled`
/// forced the FXSAVE path.
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub fn xsave_avx_enabled() -> bool {
    XSAVE_MASK.load(core::sync::atomic::Ordering::Relaxed) != 0
}

/// Enable x87 + SSE (and AVX, when the CPU has it) on this CPU.
///
/// The BSP inherits a usable FPU state from the firmware; APs arrive from the
/// INIT/SIPI trampoline with CR0.TS set, so the first SSE instruction in Rust
/// kernel code raises #NM → unhandled trap in `trap_handler`.
///
/// AVX needs one more step: the CPU advertising AVX in CPUID is not enough to
/// USE it — the OS must set CR4.OSXSAVE, enable the AVX bit in XCR0 (XSETBV),
/// and save/restore the YMM registers across the user/kernel boundary (see
/// `run`). Without it, LLVM/Mesa (llvmpipe) detect AVX as unusable and fall
/// back to 128-bit SSE. We enable x87+SSE+AVX here (not AVX-512, to keep the
/// XSAVE area small) and switch `run` to XSAVE/XRSTOR.
#[cfg(any(target_os = "none", target_os = "uefi"))]
fn init_fpu() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

    unsafe {
        Cr4::update(|cr4| {
            cr4.insert(Cr4Flags::OSFXSR);
            cr4.insert(Cr4Flags::OSXMMEXCPT_ENABLE);
        });
        Cr0::update(|cr0| {
            cr0.remove(Cr0Flags::EMULATE_COPROCESSOR);
            cr0.remove(Cr0Flags::TASK_SWITCHED);
        });
        core::arch::asm!("fninit", options(nostack, preserves_flags));
        const MXCSR_DEFAULT: u32 = 0x1F80;
        let mxcsr = MXCSR_DEFAULT;
        core::arch::asm!(
            "ldmxcsr [{mxcsr}]",
            mxcsr = in(reg) &mxcsr,
            options(nostack, preserves_flags),
        );

        // Enable XSAVE + AVX for user space when the CPU supports both. Order
        // matters: CR4.OSXSAVE must be set before XSETBV (XCr0::write), which
        // reads XCR0 first. Runs on the BSP and every AP, so every CPU ends up
        // with the same XCR0 and the same XSAVE_MASK.
        let leaf1 = core::arch::x86_64::__cpuid(1);
        let has_xsave = leaf1.ecx & (1 << 26) != 0;
        let has_avx = leaf1.ecx & (1 << 28) != 0;
        let disabled = AVX_DISABLED.load(core::sync::atomic::Ordering::Relaxed);
        if !disabled && has_xsave && has_avx {
            use x86_64::registers::xcontrol::{XCr0, XCr0Flags};
            Cr4::update(|cr4| cr4.insert(Cr4Flags::OSXSAVE));
            let flags = XCr0Flags::X87 | XCr0Flags::SSE | XCr0Flags::AVX;
            XCr0::write(flags);
            XSAVE_MASK.store(flags.bits() as u32, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Set up the BSP's GDT/TSS, IDT and FPU state and install the trap handlers.
///
/// # Safety
/// Must run once, early, with interrupts disabled, before any trap can be
/// taken on this CPU; it replaces the descriptor tables the CPU is using.
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub unsafe fn init() {
    x86_64::instructions::interrupts::disable();
    init_fpu();
    gdt::init();
    idt::init();
    syscall::init();
}

/// Per-AP counterpart of [`init`]: loads the shared IDT and this CPU's own
/// GDT/TSS.
///
/// # Safety
/// Same contract as [`init`], and only after `init` has run on the BSP so
/// the shared IDT exists.
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub unsafe fn init_ap() {
    x86_64::instructions::interrupts::disable();
    init_fpu();
    gdt::init_ap();
    // Load the shared IDT on this AP.  Each CPU's IDTR is a private register;
    // without this call the AP's IDTR is at its reset-default (base = 0),
    // causing any interrupt or exception to immediately triple-fault.
    idt::init_ap();
    // Configure syscall MSRs (EFER::SCE, LSTAR, SFMASK) — all per-CPU.
    syscall::init();
}

/// User space context
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
#[repr(C)]
pub struct UserContext {
    pub general: GeneralRegs,
    pub trap_num: usize,
    pub error_code: usize,
    /// FXSAVE area holding this thread's x87/SSE (FPU/MMX/XMM) state. The kernel
    /// is compiled with SSE2 (`+sse2`) and freely uses XMM (memcpy, etc.), so the
    /// per-thread user FPU state MUST be saved on every kernel entry and restored
    /// on every kernel exit, otherwise a user SSE computation (e.g. musl memcpy)
    /// that is preempted mid-way resumes with clobbered XMM -> corrupted data.
    pub fpstate: FpState,
}

/// Extended-state save area for this thread (see `UserContext::fpstate`).
///
/// 1024 bytes, 64-byte aligned: XSAVE requires 64-byte alignment, and 1024
/// covers the legacy x87/SSE region (512) + the XSAVE header (64) + the AVX
/// YMM_Hi component (256) with headroom. AVX-512 is intentionally not enabled
/// (see `init_fpu`), so its far larger area is not needed. The FXSAVE fallback
/// path (CPUs without XSAVE/AVX) uses only the first 512 bytes.
#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(C, align(64))]
pub struct FpState([u8; 1024]);

impl core::fmt::Debug for FpState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FpState(..)")
    }
}

impl Default for FpState {
    fn default() -> Self {
        let mut s = [0u8; 1024];
        // Legacy area (read by FXRSTOR, and the legacy half of the XSAVE area):
        // FCW = 0x037F (default x87 control word).
        s[0] = 0x7F;
        s[1] = 0x03;
        // MXCSR = 0x1F80 (default; all SSE exceptions masked) at offset 24.
        s[24] = 0x80;
        s[25] = 0x1F;
        // The XSAVE header (bytes 512..576) stays zero: XSTATE_BV = 0 (every
        // component in its init state) and XCOMP_BV = 0 (standard, non-compacted
        // format). A fresh thread's first XRSTOR then loads init x87/SSE/AVX.
        FpState(s)
    }
}

/// General registers
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
#[repr(C)]
pub struct GeneralRegs {
    pub rax: usize,
    pub rbx: usize,
    pub rcx: usize,
    pub rdx: usize,
    pub rsi: usize,
    pub rdi: usize,
    pub rbp: usize,
    pub rsp: usize,
    pub r8: usize,
    pub r9: usize,
    pub r10: usize,
    pub r11: usize,
    pub r12: usize,
    pub r13: usize,
    pub r14: usize,
    pub r15: usize,
    pub rip: usize,
    pub rflags: usize,
    pub fsbase: usize,
    pub gsbase: usize,
}

impl UserContext {
    /// Get number of syscall
    pub fn get_syscall_num(&self) -> usize {
        self.general.rax
    }

    /// Get return value of syscall
    pub fn get_syscall_ret(&self) -> usize {
        self.general.rax
    }

    /// Set return value of syscall
    pub fn set_syscall_ret(&mut self, ret: usize) {
        self.general.rax = ret;
    }

    /// Get syscall args
    pub fn get_syscall_args(&self) -> [usize; 6] {
        [
            self.general.rdi,
            self.general.rsi,
            self.general.rdx,
            self.general.r10,
            self.general.r8,
            self.general.r9,
        ]
    }

    /// Set instruction pointer
    pub fn set_ip(&mut self, ip: usize) {
        self.general.rip = ip;
    }

    /// Set stack pointer
    pub fn set_sp(&mut self, sp: usize) {
        self.general.rsp = sp;
    }

    /// Get stack pointer
    pub fn get_sp(&self) -> usize {
        self.general.rsp
    }

    /// Set tls pointer
    pub fn set_tls(&mut self, tls: usize) {
        self.general.fsbase = tls;
    }
}
