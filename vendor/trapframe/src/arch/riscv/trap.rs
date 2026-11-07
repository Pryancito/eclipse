use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};

#[cfg(target_arch = "riscv32")]
global_asm!(
    r"
    .equ XLENB, 4
    .equ FRAME_WORDS, 34
    .macro LOAD_SP a1, a2
        lw \a1, \a2*XLENB(sp)
    .endm
    .macro STORE_SP a1, a2
        sw \a1, \a2*XLENB(sp)
    .endm
"
);
#[cfg(target_arch = "riscv64")]
global_asm!(
    r"
    .equ XLENB, 8
    .equ FRAME_WORDS, 67
    # `.ifdef HAS_FP` in trap.S guards the floating-point save/restore. Only
    # defined here, for riscv64: the riscv32 block above leaves it undefined
    # and that path keeps the 34-word frame it always had.
    HAS_FP = 1
    .macro LOAD_SP a1, a2
        ld \a1, \a2*XLENB(sp)
    .endm
    .macro STORE_SP a1, a2
        sd \a1, \a2*XLENB(sp)
    .endm
    # The kernel itself is built for a soft-float target (no F/D), so the
    # assembler refuses `fsd`/`fld` without being told the hardware has them
    # (it says: instruction requires the following: D). `.option arch, +d` says so
    # for these two instructions only, pushed and popped so nothing else in
    # the file sees a different arch. The guest is the one using the unit;
    # the kernel never touches an `f` register except to save and restore it,
    # which is exactly why a soft-float kernel is the right choice and why
    # this override is safe.
    .macro LOAD_FP a1, a2
        .option push
        .option arch, +d
        fld \a1, \a2*XLENB(sp)
        .option pop
    .endm
    .macro STORE_FP a1, a2
        .option push
        .option arch, +d
        fsd \a1, \a2*XLENB(sp)
        .option pop
    .endm
"
);

global_asm!(include_str!("trap.S"));

/// Initialize interrupt handling for the current HART.
///
/// # Safety
///
/// This function will:
/// - Set `sscratch` to 0.
/// - Set `stvec` to internal exception vector.
///
/// You **MUST NOT** modify these registers later.
pub unsafe fn init() {
    // Set sscratch register to 0, indicating to exception vector that we are
    // presently executing in the kernel
    asm!("csrw sscratch, zero");
    // Set the exception vector address
    asm!("csrw stvec, {}", in(reg) trap_entry as *const () as usize);
}

#[no_mangle]
#[linkage = "weak"]
extern "C" fn trap_handler(tf: &mut TrapFrame) {
    unimplemented!("TRAP: tf={:#x?}", tf);
}

/// Trap frame of kernel interrupt
///
/// # Trap handler
///
/// You need to define a handler function like this:
///
/// ```no_run
/// use trapframe::TrapFrame;
///
/// #[no_mangle]
/// pub extern "C" fn trap_handler(tf: &mut TrapFrame) {
///     println!("TRAP! tf: {:#x?}", tf);
/// }
/// ```
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct TrapFrame {
    /// General registers
    pub general: GeneralRegs,
    /// Supervisor Status
    pub sstatus: usize,
    /// Supervisor Exception Program Counter
    pub sepc: usize,
    /// Floating-point registers and `fcsr`. See [`FpRegs`].
    #[cfg(target_arch = "riscv64")]
    pub fp: FpRegs,
}

/// Saved registers on a trap.
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct UserContext {
    /// General registers
    pub general: GeneralRegs,
    /// Supervisor Status
    pub sstatus: usize,
    /// Supervisor Exception Program Counter
    pub sepc: usize,
    /// Floating-point registers and `fcsr`. See [`FpRegs`].
    #[cfg(target_arch = "riscv64")]
    pub fp: FpRegs,
    /// The vector state, saved lazily by the kernel around `run`: see
    /// [`VectorState`](super::VectorState). Not part of the trap frame, so
    /// `trap.S` never touches it; it sits after everything the assembly
    /// addresses by slot.
    #[cfg(target_arch = "riscv64")]
    pub vector: super::VectorState,
}

impl UserContext {
    /// Go to user space with the context, and come back when a trap occurs.
    ///
    /// On return, the context will be reset to the status before the trap.
    /// Trap reason and error code will be returned.
    ///
    /// # Example
    /// ```no_run
    /// use trapframe::{UserContext, GeneralRegs};
    ///
    /// // init user space context
    /// let mut context = UserContext {
    ///     general: GeneralRegs {
    ///         sp: 0x10000,
    ///         ..Default::default()
    ///     },
    ///     sepc: 0x1000,
    ///     ..Default::default()
    /// };
    /// // go to user
    /// context.run();
    /// // back from user
    /// println!("back from user: {:#x?}", context);
    /// ```
    pub fn run(&mut self) {
        unsafe { run_user(self) }
    }
}

/// General registers
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct GeneralRegs {
    pub zero: usize,
    pub ra: usize,
    pub sp: usize,
    pub gp: usize,
    pub tp: usize,
    pub t0: usize,
    pub t1: usize,
    pub t2: usize,
    pub s0: usize,
    pub s1: usize,
    pub a0: usize,
    pub a1: usize,
    pub a2: usize,
    pub a3: usize,
    pub a4: usize,
    pub a5: usize,
    pub a6: usize,
    pub a7: usize,
    pub s2: usize,
    pub s3: usize,
    pub s4: usize,
    pub s5: usize,
    pub s6: usize,
    pub s7: usize,
    pub s8: usize,
    pub s9: usize,
    pub s10: usize,
    pub s11: usize,
    pub t3: usize,
    pub t4: usize,
    pub t5: usize,
    pub t6: usize,
}

/// The floating-point state a trap has to preserve on riscv64.
///
/// The thirty-two `f` registers plus `fcsr`, which carries the rounding mode
/// and the accrued exception flags. Thirty-three words, saved and restored
/// with the integer registers.
///
/// # Why this is saved at all
///
/// Nothing saved it before, and user mode runs with `sstatus.FS` set (see
/// `setup_uspace`), so the guest uses `f0`-`f31` freely. Two guest threads
/// multiplexed onto one hart therefore overwrote each other's floating-point
/// state, and so did any kernel code the compiler gave `f` registers to. The
/// measured symptom was `libc-test`'s `sin.exe` failing intermittently under
/// riscv64 with an `INVALID` flag the case never raised and a result whose
/// exponent was +378 where -1 was expected: not a precision bug, another
/// thread's `fcsr` and `f` registers.
///
/// # Divergence from Linux, deliberate
///
/// Linux switches this lazily: a thread starts with `FS = Off`, the first
/// floating-point instruction traps, and only a thread that has actually used
/// the unit pays for saving it. That is the optimisation, not the
/// correctness; it also needs a trap-to-enable path that this kernel does not
/// have yet. This saves eagerly, which costs thirty-three loads and
/// thirty-three stores on every trap and is right on all of them.
#[cfg(target_arch = "riscv64")]
#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
pub struct FpRegs {
    /// `f0`-`f31`, as raw doubles.
    pub f: [usize; 32],
    /// `fcsr`: rounding mode and accrued exception flags.
    pub fcsr: usize,
}

/// The contract between these structs and `trap.S`.
///
/// `trap_entry` saves the registers with `STORE_SP xN, N` -- the slot number
/// *is* the architectural register number -- and the frame is carved out with
/// `addi sp, sp, -FRAME_WORDS * XLENB`. So the whole layout is written into the
/// assembly, and `mv a0, sp` hands that same stack address to
/// `trap_handler(tf: &mut TrapFrame)`: the general registers have to start at
/// the frame base, and each one has to sit at its own register number.
///
/// The three slots the assembly names outright, rather than by register
/// number, are `STORE_SP t0, 2` (the saved sp, which is x2),
/// `STORE_SP t1, 32` (sstatus) and `STORE_SP t2, 33` (sepc).
///
/// `W` is `XLENB`: eight bytes on riscv64 and four on riscv32, which is why
/// these are written against `size_of::<usize>()` and not a literal.
///
/// Compile-time assertions rather than unit tests because this module only
/// exists for `target_arch = "riscv32"` / `"riscv64"`, which no host
/// `cargo test` builds.
const _: () = {
    const W: usize = size_of::<usize>();
    /// Words in the trap frame: 32 general registers, `sstatus`, `sepc`, and
    /// on riscv64 the 32 floating-point registers plus `fcsr`. Must match the
    /// `addi sp, sp, -FRAME_WORDS * XLENB` in `trap.S`, which gets it from the
    /// `.equ` that `global_asm!` emits next to `XLENB`.
    #[cfg(target_arch = "riscv64")]
    const FRAME_WORDS: usize = 67;
    #[cfg(not(target_arch = "riscv64"))]
    const FRAME_WORDS: usize = 34;

    assert!(offset_of!(GeneralRegs, zero) == 0 * W, "zero is x0");
    assert!(offset_of!(GeneralRegs, ra) == 1 * W, "ra is x1");
    assert!(offset_of!(GeneralRegs, sp) == 2 * W, "sp is x2");
    assert!(offset_of!(GeneralRegs, gp) == 3 * W, "gp is x3");
    assert!(offset_of!(GeneralRegs, tp) == 4 * W, "tp is x4");
    assert!(offset_of!(GeneralRegs, t0) == 5 * W, "t0 is x5");
    assert!(offset_of!(GeneralRegs, t1) == 6 * W, "t1 is x6");
    assert!(offset_of!(GeneralRegs, t2) == 7 * W, "t2 is x7");
    assert!(offset_of!(GeneralRegs, s0) == 8 * W, "s0 is x8");
    assert!(offset_of!(GeneralRegs, s1) == 9 * W, "s1 is x9");
    assert!(offset_of!(GeneralRegs, a0) == 10 * W, "a0 is x10");
    assert!(offset_of!(GeneralRegs, a1) == 11 * W, "a1 is x11");
    assert!(offset_of!(GeneralRegs, a2) == 12 * W, "a2 is x12");
    assert!(offset_of!(GeneralRegs, a3) == 13 * W, "a3 is x13");
    assert!(offset_of!(GeneralRegs, a4) == 14 * W, "a4 is x14");
    assert!(offset_of!(GeneralRegs, a5) == 15 * W, "a5 is x15");
    assert!(offset_of!(GeneralRegs, a6) == 16 * W, "a6 is x16");
    assert!(offset_of!(GeneralRegs, a7) == 17 * W, "a7 is x17");
    assert!(offset_of!(GeneralRegs, s2) == 18 * W, "s2 is x18");
    assert!(offset_of!(GeneralRegs, s3) == 19 * W, "s3 is x19");
    assert!(offset_of!(GeneralRegs, s4) == 20 * W, "s4 is x20");
    assert!(offset_of!(GeneralRegs, s5) == 21 * W, "s5 is x21");
    assert!(offset_of!(GeneralRegs, s6) == 22 * W, "s6 is x22");
    assert!(offset_of!(GeneralRegs, s7) == 23 * W, "s7 is x23");
    assert!(offset_of!(GeneralRegs, s8) == 24 * W, "s8 is x24");
    assert!(offset_of!(GeneralRegs, s9) == 25 * W, "s9 is x25");
    assert!(offset_of!(GeneralRegs, s10) == 26 * W, "s10 is x26");
    assert!(offset_of!(GeneralRegs, s11) == 27 * W, "s11 is x27");
    assert!(offset_of!(GeneralRegs, t3) == 28 * W, "t3 is x28");
    assert!(offset_of!(GeneralRegs, t4) == 29 * W, "t4 is x29");
    assert!(offset_of!(GeneralRegs, t5) == 30 * W, "t5 is x30");
    assert!(offset_of!(GeneralRegs, t6) == 31 * W, "t6 is x31");
    // x0 is hardwired to zero and `trap.S` deliberately never stores it, but
    // the slot has to exist so that register N stays at slot N.
    assert!(
        size_of::<GeneralRegs>() == 32 * W,
        "the general registers are no longer the 32 slots STORE_SP indexes"
    );

    // `mv a0, sp` makes the frame base the TrapFrame, so `general` comes first
    // here (unlike aarch64, where it comes last).
    assert!(
        offset_of!(TrapFrame, general) == 0,
        "the general registers start the frame"
    );
    assert!(
        offset_of!(TrapFrame, sstatus) == 32 * W,
        "the `STORE_SP t1, 32` writes sstatus"
    );
    assert!(
        offset_of!(TrapFrame, sepc) == 33 * W,
        "the `STORE_SP t2, 33` writes sepc"
    );
    // The floating-point area the `STORE_FP fN, 34 + N` run indexes, with
    // `fcsr` the word after it. Only riscv64 has it: the riscv32 path of this
    // file is untouched, because this tree never builds it and inventing
    // `fsw` sequences nothing exercises would be worse than leaving it alone.
    #[cfg(target_arch = "riscv64")]
    assert!(
        offset_of!(TrapFrame, fp) == 34 * W,
        "the `STORE_FP f0, 34` writes the first floating-point register"
    );
    #[cfg(target_arch = "riscv64")]
    assert!(
        offset_of!(FpRegs, f) == 0,
        "the floating-point registers start their area"
    );
    #[cfg(target_arch = "riscv64")]
    assert!(
        offset_of!(TrapFrame, fp) + offset_of!(FpRegs, fcsr) == 66 * W,
        "the `STORE_SP t0, 66` writes fcsr"
    );
    assert!(
        size_of::<TrapFrame>() == FRAME_WORDS * W,
        "the frame is no longer the words `addi sp, sp, -FRAME_WORDS * XLENB` carves out"
    );

    // `run_user` takes a `&mut UserContext` while the trap path builds a
    // `TrapFrame`, and both reach the same `trap_return`, so the two have to
    // agree word for word over the frame. Nothing but this makes them. The
    // `UserContext` carries the vector state after the frame, which the
    // assembly never addresses: the frame has to be exactly its prefix.
    #[cfg(target_arch = "riscv64")]
    assert!(
        offset_of!(UserContext, vector) == size_of::<TrapFrame>(),
        "TrapFrame is no longer the prefix of UserContext that trap.S addresses by slot"
    );
    #[cfg(not(target_arch = "riscv64"))]
    assert!(
        size_of::<TrapFrame>() == size_of::<UserContext>(),
        "TrapFrame and UserContext no longer agree, and trap.S uses them interchangeably"
    );
    assert!(
        offset_of!(UserContext, general) == offset_of!(TrapFrame, general),
        "general"
    );
    assert!(
        offset_of!(UserContext, sstatus) == offset_of!(TrapFrame, sstatus),
        "sstatus"
    );
    assert!(
        offset_of!(UserContext, sepc) == offset_of!(TrapFrame, sepc),
        "sepc"
    );
};

impl UserContext {
    /// Get number of syscall
    pub fn get_syscall_num(&self) -> usize {
        self.general.a7
    }

    /// Get return value of syscall
    pub fn get_syscall_ret(&self) -> usize {
        self.general.a0
    }

    /// Set return value of syscall
    pub fn set_syscall_ret(&mut self, ret: usize) {
        self.general.a0 = ret;
    }

    /// Get syscall args
    pub fn get_syscall_args(&self) -> [usize; 6] {
        [
            self.general.a0,
            self.general.a1,
            self.general.a2,
            self.general.a3,
            self.general.a4,
            self.general.a5,
        ]
    }

    /// Set instruction pointer
    pub fn set_ip(&mut self, ip: usize) {
        self.sepc = ip;
    }

    /// Set stack pointer
    pub fn set_sp(&mut self, sp: usize) {
        self.general.sp = sp;
    }

    /// Get stack pointer
    pub fn get_sp(&self) -> usize {
        self.general.sp
    }

    /// Set tls pointer
    pub fn set_tls(&mut self, tls: usize) {
        self.general.tp = tls;
    }
}

#[allow(improper_ctypes)]
extern "C" {
    fn trap_entry();
    fn run_user(regs: &mut UserContext);
}
