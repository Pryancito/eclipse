use super::*;
use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};

global_asm!(include_str!("trap.S"));

/// Initialize interrupt handling for the current HART.
///
/// # Safety
///
/// This function will:
/// - Set `vbar_el1` to internal exception vector.
///
/// You **MUST NOT** modify these registers later.
pub unsafe fn init() {
    // Set the exception vector address
    asm!("msr VBAR_EL1, {}", in(reg) __vectors as *const () as usize);
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
    /// Trap num: Source and Kind
    pub trap_num: usize,
    /// Reserved for internal use
    pub __reserved: usize,
    /// Exception Link Register, elr_el1
    pub elr: usize,
    /// Saved Process Status Register, spsr_el1
    pub spsr: usize,
    /// Stack Pointer, sp_el1
    pub sp: usize,
    /// Software Thread ID Register, tpidr_el1
    pub tpidr: usize,
    /// General registers
    /// Must be last in this struct
    pub general: GeneralRegs,
}

/// The contract between [`TrapFrame`] and `trap.S`.
///
/// `__alltraps` builds this frame with `stp` pairs and then reaches into it at
/// *literal* byte offsets: `[sp, #32]` for `sp`, `[sp, #40]` for `tpidr`,
/// `[sp, #8]` for the kernel stack pointer that `run_user` stashes in
/// `__reserved`, and `add x1, sp, #38*8` for the whole frame. Reorder two
/// fields and the assembler keeps assembling, while every trap starts reading
/// and writing the wrong words.
///
/// Compile-time assertions rather than unit tests because this module only
/// exists under `target_os = "none"` / `"uefi"`, so no host `cargo test`
/// compiles it: a test here would never run, and a mutation of this struct
/// comes out "survived" without anything having looked at it.
const _: () = {
    assert!(
        offset_of!(TrapFrame, trap_num) == 0,
        "the trap num is the bottom of the frame"
    );
    // `run_user` does `str x1, [sp, #8]` to stash the kernel sp here, and
    // `trap_from_user` reads it back with `ldr x2, [sp, #8]`. The field is
    // called `__reserved`, but that slot is load-bearing.
    assert!(
        offset_of!(TrapFrame, __reserved) == 8,
        "the kernel sp is stashed at byte 8"
    );
    assert!(
        offset_of!(TrapFrame, elr) == 16,
        "elr_el1 is restored from byte 16"
    );
    assert!(
        offset_of!(TrapFrame, spsr) == 24,
        "spsr_el1 is restored from byte 24"
    );
    // Both `stp x1, x2, [sp, #32]` stores write sp then tpidr as a pair.
    assert!(
        offset_of!(TrapFrame, sp) == 32,
        "the `[sp, #32]` stores write sp here"
    );
    assert!(
        offset_of!(TrapFrame, tpidr) == 40,
        "the `ldr x1, [sp, #40]` reads tpidr here"
    );
    assert!(
        offset_of!(TrapFrame, general) == 48,
        "the general registers start at byte 48"
    );

    // `add x1, sp, #38*8` computes the interrupted sp from the top of the
    // frame, so a field added anywhere makes it point into the frame instead.
    assert!(
        size_of::<TrapFrame>() == 38 * 8,
        "the frame is no longer the 38 words the `add x1, sp, #38*8` steps over"
    );
    // What the doc comment above asks for in prose, checked.
    assert!(
        offset_of!(TrapFrame, general) + size_of::<GeneralRegs>() == size_of::<TrapFrame>(),
        "the general registers are no longer last in the struct"
    );

    // `run_user` is declared here as taking `&mut UserContext`, while `trap.S`
    // says "x0 points to TrapFrame" and reaches `trap_return`, which both
    // paths share. So the two structs have to agree word for word, and nothing
    // but this assertion makes them.
    assert!(
        size_of::<TrapFrame>() == size_of::<UserContext>(),
        "TrapFrame and UserContext no longer agree, and trap.S uses them interchangeably"
    );
    assert!(
        offset_of!(TrapFrame, trap_num) == offset_of!(UserContext, trap_num),
        "trap_num"
    );
    assert!(
        offset_of!(TrapFrame, __reserved) == offset_of!(UserContext, __reserved),
        "__reserved"
    );
    assert!(
        offset_of!(TrapFrame, elr) == offset_of!(UserContext, elr),
        "elr"
    );
    assert!(
        offset_of!(TrapFrame, spsr) == offset_of!(UserContext, spsr),
        "spsr"
    );
    assert!(
        offset_of!(TrapFrame, sp) == offset_of!(UserContext, sp),
        "sp"
    );
    assert!(
        offset_of!(TrapFrame, tpidr) == offset_of!(UserContext, tpidr),
        "tpidr"
    );
    assert!(
        offset_of!(TrapFrame, general) == offset_of!(UserContext, general),
        "general"
    );
};

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
    ///         ..Default::default()
    ///     },
    ///     sp: 0x10000,
    ///     elr: 0x1000,
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

#[allow(improper_ctypes)]
extern "C" {
    fn __vectors();
    fn run_user(regs: &mut UserContext);
}
