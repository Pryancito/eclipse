//! The vector (V extension) state of a user thread on riscv64, and the two
//! instructions sequences that move it in and out of the register file.
//!
//! # Why this exists
//!
//! Fuchsia's riscv64 libc uses vector instructions unconditionally in its
//! start-up (`__fuchsia_libc_StartupRelocateCreate` poisons memory with a
//! `vsetvli`/`vmv.v.x`/`vse8.v` memset before the first system call), so
//! `userboot` died with an illegal instruction on the Zircon riscv64 job
//! while `sstatus.VS` was Off. Turning it on without saving the state would
//! be the floating-point bug of [`FpRegs`](super::FpRegs) all over again,
//! with thirty-two registers of up to `VLENB` bytes each instead of eight.
//!
//! # Lazy, like Linux, by the hardware's own bit
//!
//! The hardware sets `sstatus.VS` to Dirty when user code writes any vector
//! register or CSR, and the trap entry stores that `sstatus` in the context.
//! So the kernel saves this state only after a trip to user mode that came
//! back Dirty, and restores it before the next trip only for a thread that
//! has saved state at all. A thread that never touches the unit pays
//! nothing, which matters: a full save is `32 * VLENB` bytes plus four CSRs.
//! The kernel itself is built without V and never executes a vector
//! instruction except these, so between the trap and the save nothing
//! disturbs the register file.
//!
//! # Sizing
//!
//! `VLENB` is a property of the hardware (`csr vlenb`), read at boot. The
//! state lives inline in the `UserContext` so that it is copied with the
//! rest of the thread on `fork`, sized for [`VLENB_MAX`] bytes per register:
//! a machine with a wider unit than that keeps `VS` Off, exactly as before,
//! and says so in the log.

/// The widest vector register this layout holds, in bytes: `VLEN = 256`.
/// QEMU's `virt` is run with `vlen=128`; real parts shipping today are 128
/// or 256.
pub const VLENB_MAX: usize = 32;

/// `sstatus.VS`: bits 9 and 10.
pub const SSTATUS_VS_SHIFT: usize = 9;
/// The `VS` field mask.
pub const SSTATUS_VS_MASK: usize = 0b11 << SSTATUS_VS_SHIFT;
/// `VS = Initial`: the unit is on, its state is in the reset condition.
pub const SSTATUS_VS_INITIAL: usize = 0b01 << SSTATUS_VS_SHIFT;
/// `VS = Clean`: on, and nothing has been written since the last save.
pub const SSTATUS_VS_CLEAN: usize = 0b10 << SSTATUS_VS_SHIFT;
/// `VS = Dirty`: on, and user code has written to it since.
pub const SSTATUS_VS_DIRTY: usize = 0b11 << SSTATUS_VS_SHIFT;

/// The vector state of one thread: the thirty-two `v` registers and the
/// four CSRs that `vsetvl` and the vector CSR instructions carry.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct VectorState {
    /// `v0`-`v31`, each `VLENB` bytes wide, packed as `vs8r.v` stores them.
    pub v: [u8; 32 * VLENB_MAX],
    /// `vstart`: the element a restarted instruction resumes at.
    pub vstart: usize,
    /// `vtype`: the `vsetvl` configuration, read-only except through `vsetvl`.
    pub vtype: usize,
    /// `vl`: the vector length `vsetvl` chose.
    pub vl: usize,
    /// `vcsr`: `vxrm` and `vxsat`, the fixed-point rounding mode and flag.
    pub vcsr: usize,
    /// Whether `v` holds a state this thread saved. Restores happen only
    /// when it does; a thread that never used the unit has nothing to put
    /// back.
    pub live: bool,
}

impl Default for VectorState {
    fn default() -> Self {
        Self {
            v: [0; 32 * VLENB_MAX],
            vstart: 0,
            vtype: 0,
            vl: 0,
            vcsr: 0,
            live: false,
        }
    }
}

impl core::fmt::Debug for VectorState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VectorState")
            .field("live", &self.live)
            .field("vstart", &self.vstart)
            .field("vtype", &self.vtype)
            .field("vl", &self.vl)
            .field("vcsr", &self.vcsr)
            .finish_non_exhaustive()
    }
}

/// `VLENB`, the width in bytes of one vector register on this hart.
///
/// # Safety
///
/// The hart must implement the V extension: on one that does not, `vlenb`
/// does not exist and the read is an illegal instruction. The caller learns
/// that from the device tree (`riscv,isa`), never by trying.
#[cfg(target_arch = "riscv64")]
pub unsafe fn vlenb() -> usize {
    let vlenb: usize;
    // SAFETY: a CSR read with no side effects, on a hart the caller vouches
    // has the register. `VS` is raised first because `vlenb`, like every
    // vector CSR, is only accessible while the unit is on.
    unsafe {
        core::arch::asm!(
            "csrs sstatus, {vs}",
            ".option push",
            ".option arch, +v",
            "csrr {out}, vlenb",
            ".option pop",
            vs = in(reg) SSTATUS_VS_INITIAL,
            out = out(reg) vlenb,
            options(nomem, nostack),
        );
    }
    vlenb
}

/// Copy the hart's vector register file and CSRs into `state` and mark it
/// live.
///
/// # Safety
///
/// The hart must implement V with `VLENB <= VLENB_MAX`, and the register
/// file must still hold the thread's values -- call this right after the
/// trip to user mode came back, before anything else could run on this
/// hart.
#[cfg(target_arch = "riscv64")]
pub unsafe fn vector_save(state: &mut VectorState) {
    let base = state.v.as_mut_ptr();
    // SAFETY: `vs8r.v` writes `8 * VLENB` bytes at each of the four
    // addresses, `32 * VLENB <= 32 * VLENB_MAX`, the size of `state.v`. The
    // unit is switched on for the kernel's own instructions here (`csrs`),
    // whatever the field said; the trap entry has already stored the
    // user's `sstatus` in the context, and a kernel-side `VS` is never read
    // back as the user's.
    unsafe {
        core::arch::asm!(
            "csrs sstatus, {vs}",
            ".option push",
            ".option arch, +v",
            "csrr {vstart}, vstart",
            "csrr {vtype}, vtype",
            "csrr {vl}, vl",
            "csrr {vcsr}, vcsr",
            "csrr {step}, vlenb",
            "slli {step}, {step}, 3",
            "vs8r.v v0, ({base})",
            "add {base}, {base}, {step}",
            "vs8r.v v8, ({base})",
            "add {base}, {base}, {step}",
            "vs8r.v v16, ({base})",
            "add {base}, {base}, {step}",
            "vs8r.v v24, ({base})",
            ".option pop",
            vs = in(reg) SSTATUS_VS_INITIAL,
            vstart = out(reg) state.vstart,
            vtype = out(reg) state.vtype,
            vl = out(reg) state.vl,
            vcsr = out(reg) state.vcsr,
            step = out(reg) _,
            base = inout(reg) base => _,
            options(nostack),
        );
    }
    state.live = true;
}

/// Load the hart's vector register file and CSRs from `state`.
///
/// # Safety
///
/// Same hardware contract as [`vector_save`], and `state.live` must be
/// true: restoring a state nobody saved loads zeros over whatever the unit
/// held, which is harmless but pointless.
#[cfg(target_arch = "riscv64")]
pub unsafe fn vector_restore(state: &VectorState) {
    let base = state.v.as_ptr();
    // SAFETY: four `vl8r.v` reads of `8 * VLENB` bytes from `state.v`, which
    // is `32 * VLENB_MAX` bytes long. `vsetvl` is the only way to write
    // `vtype`, and it takes `vl` with it; `vstart` goes last, because the
    // loads above reset it to zero.
    unsafe {
        core::arch::asm!(
            "csrs sstatus, {vs}",
            ".option push",
            ".option arch, +v",
            "csrr {step}, vlenb",
            "slli {step}, {step}, 3",
            "vl8r.v v0, ({base})",
            "add {base}, {base}, {step}",
            "vl8r.v v8, ({base})",
            "add {base}, {base}, {step}",
            "vl8r.v v16, ({base})",
            "add {base}, {base}, {step}",
            "vl8r.v v24, ({base})",
            "vsetvl x0, {vl}, {vtype}",
            "csrw vcsr, {vcsr}",
            "csrw vstart, {vstart}",
            ".option pop",
            vs = in(reg) SSTATUS_VS_INITIAL,
            vstart = in(reg) state.vstart,
            vtype = in(reg) state.vtype,
            vl = in(reg) state.vl,
            vcsr = in(reg) state.vcsr,
            step = out(reg) _,
            base = inout(reg) base => _,
            options(nostack, readonly),
        );
    }
}
