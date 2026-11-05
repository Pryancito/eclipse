//! The kernel's own `rt_sigreturn` trampoline, and the page it lives on.
//!
//! ## Why a kernel page and not libc's `sa_restorer`
//!
//! A signal handler is an ordinary function: it ends in a `ret`, and whatever
//! the kernel left in the return-address register is where the thread goes
//! next. That address has to be a stub that issues `rt_sigreturn`, because the
//! interrupted context lives in the kernel and only that call puts it back.
//!
//! On x86_64 the stub's address is pushed on the stack and `struct sigaction`
//! carries it in `sa_restorer`: the ABI *requires* `SA_RESTORER`, and Linux
//! answers `rt_sigaction` without it with `EINVAL`, so there is always one.
//!
//! The generic ABI — aarch64 and riscv64 — has no `sa_restorer` field at all,
//! and no `SA_RESTORER`. Linux there puts `rt_sigreturn` in its own vDSO and
//! writes *that* into the return-address register
//! (`setup_rt_frame` → `regs->ra = VDSO_SYMBOL(.., rt_sigreturn)`), ignoring
//! anything a libc might have passed. A C library on those architectures is
//! therefore entitled to send a zeroed restorer, and glibc does.
//!
//! This kernel wrote `action.restorer` into the register unconditionally. With
//! a zero restorer the handler's `ret` jumped to address 0 — measured in QEMU
//! riscv64 in this container, `/sbin/init` dying 60 seconds into every boot,
//! `pc=0 ra=0`, the moment its `SIGALRM` handler returned. PID 1, so the whole
//! guest went with it.
//!
//! The fix is Linux's: the kernel provides the trampoline. There is no Linux
//! vDSO on these architectures ([`crate::vdso`] builds an x86_64 image only),
//! so it gets a page of its own — which is also what Linux does on ARM, whose
//! `sigpage` is exactly this: one page, one trampoline, mapped read+execute
//! into every process.
//!
//! A *non-zero* restorer is still honoured. On x86_64 nothing here is used at
//! all, and on the generic ABI a libc that does supply a stub (musl, which
//! defines `SA_RESTORER` for riscv64 and points it at its own `__restore`)
//! keeps returning through it, so no program that worked before changes path.

use alloc::sync::Arc;

use kernel_hal::VirtAddr;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use zircon_object::vm::{MMUFlags, VmObject};
use zircon_object::vm::{VmAddressRegion, PAGE_SIZE};

/// A gap left below the mapping the sigpage is placed under, for the same
/// reason [`crate::vdso`] leaves one: an overflow faults instead of running
/// quietly into executable pages.
/// Compiled on every architecture, and used on two: see
/// [`riscv64_trampoline`] for why.
#[allow(dead_code)]
const GUARD: usize = PAGE_SIZE;

/// `rt_sigreturn` in the generic syscall ABI, which is the number both
/// architectures below use.
/// Compiled on every architecture, and used on two: see
/// [`riscv64_trampoline`] for why.
#[allow(dead_code)]
const NR_RT_SIGRETURN: u32 = 139;

/// The trampoline in RV64, as the bytes of the instructions that make the
/// call.
///
/// Two instructions: put the syscall number in the syscall-number register,
/// then trap. Nothing else — no prologue, no stack, no return.
/// `rt_sigreturn` does not come back.
///
/// Built, not pasted as hex, and compiled on every architecture rather than
/// only on its own, so that the encoding is checked by a unit test that the
/// CI actually runs (which is an x86_64 host).
#[allow(dead_code)]
const fn riscv64_trampoline() -> [u8; 8] {
    // addi a7, zero, 139   (`li a7, NR_rt_sigreturn`)
    let li = (NR_RT_SIGRETURN << 20) | (17 << 7) | 0b001_0011;
    // ecall
    let ecall: u32 = 0x0000_0073;
    let (a, b) = (li.to_le_bytes(), ecall.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]]
}

/// As [`riscv64_trampoline`], in A64.
#[allow(dead_code)]
const fn aarch64_trampoline() -> [u8; 8] {
    // mov x8, #139   (`movz x8, #NR_rt_sigreturn`)
    let movz = 0xd280_0000 | (NR_RT_SIGRETURN << 5) | 8;
    // svc #0
    let svc: u32 = 0xd400_0001;
    let (a, b) = (movz.to_le_bytes(), svc.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]]
}

/// Whichever of the two this kernel is being built for.
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
const fn trampoline() -> [u8; 8] {
    #[cfg(target_arch = "riscv64")]
    {
        riscv64_trampoline()
    }
    #[cfg(target_arch = "aarch64")]
    {
        aarch64_trampoline()
    }
}

/// Where the page goes: under `below`, with [`GUARD`] between them.
///
/// Returns the address and the offset into `vmar` to map it at. `None` when
/// the arithmetic does not fit, which is the only way this can fail and is
/// non-fatal: see [`map_into`].
#[allow(dead_code)]
fn placement(below: VirtAddr, vmar_addr: VirtAddr) -> Option<(VirtAddr, usize)> {
    let base = below.checked_sub(GUARD)?.checked_sub(PAGE_SIZE)?;
    let offset = base.checked_sub(vmar_addr)?;
    Some((base, offset))
}

/// Map a sigpage into a process, under `below`, and return the address of the
/// trampoline on it.
///
/// `None` on an architecture that does not need one (x86_64, whose ABI always
/// carries a restorer) and on a mapping failure. A `None` is not fatal: it
/// leaves delivery exactly as it was before this module existed, which is a
/// handler that must return through its own restorer.
#[allow(unused_variables)]
pub fn map_into(vmar: &Arc<VmAddressRegion>, below: VirtAddr) -> Option<VirtAddr> {
    #[cfg(not(any(target_arch = "riscv64", target_arch = "aarch64")))]
    {
        None
    }
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    {
        let (base, offset) = placement(below, vmar.addr())?;
        let vmo = VmObject::new_paged(1);
        if let Err(e) = vmo.write(0, &trampoline()) {
            warn!("sigpage: cannot write the trampoline: {:?}", e);
            return None;
        }
        // Read and execute, never write: a process that could write it could
        // only lie to itself, but there is no reason to allow even that.
        let flags = MMUFlags::READ | MMUFlags::EXECUTE | MMUFlags::USER;
        // Named rather than passed positionally: `map_ext` takes three
        // bare booleans in a row, and which one is which is not visible at
        // the call site.
        //
        // `map_range` installs the PTE now. The alternative is a page fault
        // on the way out of every first signal handler.
        let overwrite = false;
        let map_range = true;
        let allow_faults = false;
        match vmar.map_ext(
            Some(offset),
            vmo,
            0,
            PAGE_SIZE,
            flags,
            flags,
            overwrite,
            map_range,
            allow_faults,
        ) {
            Ok(addr) => {
                debug!("sigpage: mapped at {:#x}", addr);
                Some(addr)
            }
            Err(e) => {
                warn!("sigpage: cannot map at {:#x}: {:?}", base, e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
