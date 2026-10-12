use core::mem::{offset_of, size_of};

#[cfg(target_os = "linux")]
mod fncall;
#[cfg(any(target_os = "none", target_os = "uefi"))]
mod trap;

#[cfg(target_os = "linux")]
pub use fncall::*;
#[cfg(any(target_os = "none", target_os = "uefi"))]
pub use trap::*;

/// Saved registers on a trap.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
#[repr(C)]
pub struct UserContext {
    /// Trap num: Source and Kind
    pub trap_num: usize,
    /// Reserved for internal use
    pub __reserved: usize,
    /// Exception Link Register, elr_el1
    pub elr: usize,
    /// Saved Process Status Register, spsr_el1
    pub spsr: usize,
    /// Stack Pointer, sp_el0
    pub sp: usize,
    /// Software Thread ID Register, tpidr_el0
    pub tpidr: usize,
    /// General registers
    /// Must be last in this struct
    pub general: GeneralRegs,
}

/// General registers
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
#[repr(C)]
pub struct GeneralRegs {
    pub x1: usize,
    pub x2: usize,
    pub x3: usize,
    pub x4: usize,
    pub x5: usize,
    pub x6: usize,
    pub x7: usize,
    pub x8: usize,
    pub x9: usize,
    pub x10: usize,
    pub x11: usize,
    pub x12: usize,
    pub x13: usize,
    pub x14: usize,
    pub x15: usize,
    pub x16: usize,
    pub x17: usize,
    pub x18: usize,
    pub x19: usize,
    pub x20: usize,
    pub x21: usize,
    pub x22: usize,
    pub x23: usize,
    pub x24: usize,
    pub x25: usize,
    pub x26: usize,
    pub x27: usize,
    pub x28: usize,
    pub x29: usize,
    pub __reserved: usize, // for alignment
    pub x30: usize,
    // put here deliberately for ease of asm
    pub x0: usize,
    // x31 means special
}

/// The contract between these two structs and `trap.S`.
///
/// `__alltraps` saves the general registers with `stp` *pairs* and
/// `trap_return` restores them with `ldp` pairs in the mirror order, so each
/// register's slot is written into the assembly. The odd tail is deliberate
/// and the comments above say so, but nothing checked it: `x29` is loaded
/// alone (`ldr x29, [sp], #16`, which steps over the alignment filler next to
/// it) and `x30` and `x0` come last because `__vectors` pushes them together
/// as the frame's first `stp` and `trap_return` pops them as its last `ldp`.
///
/// [`UserContext`] carries the same literal offsets as [`TrapFrame`], because
/// `run_user` reaches into it with `str x1, [sp, #8]` and
/// `ldp x1, x2, [sp, #32]`.
///
/// Compile-time assertions rather than unit tests because this module only
/// exists for `target_arch = "aarch64"`, which no host `cargo test` builds.
const _: () = {
    assert!(offset_of!(GeneralRegs, x1) == 0 * 8, "x1 is slot 0");
    assert!(offset_of!(GeneralRegs, x2) == 1 * 8, "x2 is slot 1");
    assert!(offset_of!(GeneralRegs, x3) == 2 * 8, "x3 is slot 2");
    assert!(offset_of!(GeneralRegs, x4) == 3 * 8, "x4 is slot 3");
    assert!(offset_of!(GeneralRegs, x5) == 4 * 8, "x5 is slot 4");
    assert!(offset_of!(GeneralRegs, x6) == 5 * 8, "x6 is slot 5");
    assert!(offset_of!(GeneralRegs, x7) == 6 * 8, "x7 is slot 6");
    assert!(offset_of!(GeneralRegs, x8) == 7 * 8, "x8 is slot 7");
    assert!(offset_of!(GeneralRegs, x9) == 8 * 8, "x9 is slot 8");
    assert!(offset_of!(GeneralRegs, x10) == 9 * 8, "x10 is slot 9");
    assert!(offset_of!(GeneralRegs, x11) == 10 * 8, "x11 is slot 10");
    assert!(offset_of!(GeneralRegs, x12) == 11 * 8, "x12 is slot 11");
    assert!(offset_of!(GeneralRegs, x13) == 12 * 8, "x13 is slot 12");
    assert!(offset_of!(GeneralRegs, x14) == 13 * 8, "x14 is slot 13");
    assert!(offset_of!(GeneralRegs, x15) == 14 * 8, "x15 is slot 14");
    assert!(offset_of!(GeneralRegs, x16) == 15 * 8, "x16 is slot 15");
    assert!(offset_of!(GeneralRegs, x17) == 16 * 8, "x17 is slot 16");
    assert!(offset_of!(GeneralRegs, x18) == 17 * 8, "x18 is slot 17");
    assert!(offset_of!(GeneralRegs, x19) == 18 * 8, "x19 is slot 18");
    assert!(offset_of!(GeneralRegs, x20) == 19 * 8, "x20 is slot 19");
    assert!(offset_of!(GeneralRegs, x21) == 20 * 8, "x21 is slot 20");
    assert!(offset_of!(GeneralRegs, x22) == 21 * 8, "x22 is slot 21");
    assert!(offset_of!(GeneralRegs, x23) == 22 * 8, "x23 is slot 22");
    assert!(offset_of!(GeneralRegs, x24) == 23 * 8, "x24 is slot 23");
    assert!(offset_of!(GeneralRegs, x25) == 24 * 8, "x25 is slot 24");
    assert!(offset_of!(GeneralRegs, x26) == 25 * 8, "x26 is slot 25");
    assert!(offset_of!(GeneralRegs, x27) == 26 * 8, "x27 is slot 26");
    assert!(offset_of!(GeneralRegs, x28) == 27 * 8, "x28 is slot 27");
    assert!(offset_of!(GeneralRegs, x29) == 28 * 8, "x29 is slot 28");
    // The alignment filler `ldr x29, [sp], #16` steps over, then the pair
    // `ldp lr, x0` restores last.
    assert!(
        offset_of!(GeneralRegs, __reserved) == 29 * 8,
        "the alignment filler is slot 29"
    );
    assert!(
        offset_of!(GeneralRegs, x30) == 30 * 8,
        "x30 is slot 30, restored as `ldp lr, x0`"
    );
    assert!(
        offset_of!(GeneralRegs, x0) == 31 * 8,
        "x0 is the last slot, saved first by __vectors"
    );
    assert!(
        size_of::<GeneralRegs>() == 32 * 8,
        "the general registers are no longer the 32 words trap.S saves and restores"
    );

    // The same literal offsets `run_user` uses, on the struct it is declared
    // to take.
    assert!(
        offset_of!(UserContext, trap_num) == 0,
        "the trap num is the bottom of the frame"
    );
    assert!(
        offset_of!(UserContext, __reserved) == 8,
        "the kernel sp is stashed at byte 8"
    );
    assert!(
        offset_of!(UserContext, elr) == 16,
        "elr is restored from byte 16"
    );
    assert!(
        offset_of!(UserContext, spsr) == 24,
        "spsr is restored from byte 24"
    );
    assert!(
        offset_of!(UserContext, sp) == 32,
        "the `[sp, #32]` pair writes sp here"
    );
    assert!(
        offset_of!(UserContext, tpidr) == 40,
        "the `[sp, #32]` pair writes tpidr here"
    );
    assert!(
        offset_of!(UserContext, general) == 48,
        "the general registers start at byte 48"
    );
    assert!(
        offset_of!(UserContext, general) + size_of::<GeneralRegs>() == size_of::<UserContext>(),
        "the general registers are no longer last in the struct"
    );
};

impl UserContext {
    /// Get number of syscall
    pub fn get_syscall_num(&self) -> usize {
        self.general.x8
    }

    /// Get return value of syscall
    pub fn get_syscall_ret(&self) -> usize {
        self.general.x0
    }

    /// Set return value of syscall
    pub fn set_syscall_ret(&mut self, ret: usize) {
        self.general.x0 = ret;
    }

    /// Get syscall args
    pub fn get_syscall_args(&self) -> [usize; 6] {
        [
            self.general.x0,
            self.general.x1,
            self.general.x2,
            self.general.x3,
            self.general.x4,
            self.general.x5,
        ]
    }

    /// Set instruction pointer
    pub fn set_ip(&mut self, ip: usize) {
        self.elr = ip;
    }

    /// Set stack pointer
    pub fn set_sp(&mut self, sp: usize) {
        self.sp = sp;
    }

    /// Get stack pointer
    pub fn get_sp(&self) -> usize {
        self.sp
    }

    /// Set tls pointer
    pub fn set_tls(&mut self, tls: usize) {
        self.tpidr = tls;
    }
}
