//! aarch64 Symmetric Multi-Processing startup (PSCI `CPU_ON`).
//!
//! Brings up secondary cores via the PSCI `CPU_ON` SMC/HVC call. Secondaries
//! enter [`secondary_trampoline`] **with the MMU off**, at the trampoline's
//! *physical* address, with `x0` = physical pointer to a [`SecondaryContext`].
//! The trampoline restores the BSP's translation registers (so it shares the
//! kernel page table), enables the MMU, and jumps to the kernel's per-CPU entry
//! (`ap_fn`, i.e. `secondary_main`) on its own stack.
//!
//! MMU hand-off detail: while the MMU is being enabled, the PC still holds the
//! trampoline's *physical* address, so that address must be mapped. We therefore
//! build a TTBR0 table that identity-maps the trampoline page; after the `br` to
//! the high-half entry, execution runs purely out of the kernel (TTBR1) mapping.
//!
//! NOTE: this path has been validated to *compile* but not yet boot-tested on
//! QEMU; the MMU/PSCI hand-off is the likely place to debug if secondaries hang.

use core::sync::atomic::{AtomicUsize, Ordering};

use cortex_a::registers::*;
use tock_registers::interfaces::Readable;

use super::vm::PageTable;
use crate::common::affinity_walk::{self, AffinityWalk, Outcome, Step};
use crate::{vm::GenericPageTable, MMUFlags, KCONFIG};

const PAGE_SIZE: usize = 4096;
const STACK_SIZE: usize = 256 * 1024;
// AP stacks are allocated via `Layout::from_size_align(STACK_SIZE, PAGE_SIZE)`,
// which requires the size to be a multiple of the (page) alignment.
const _: () = assert!(STACK_SIZE.is_multiple_of(PAGE_SIZE));

/// PSCI `CPU_ON` (SMC64) function id.
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Number of secondary CPUs that have signalled they are running.
pub static AP_ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// State handed to a secondary core (read by [`secondary_trampoline`] with the
/// MMU off, so it lives in identity-readable physical memory). Field order is
/// load-bearing — the trampoline reads it by fixed byte offsets.
#[repr(C)]
struct SecondaryContext {
    ttbr0: u64,   // +0
    ttbr1: u64,   // +8
    tcr: u64,     // +16
    mair: u64,    // +24
    sctlr: u64,   // +32
    sp: u64,      // +40  (virtual stack top)
    entry: u64,   // +48  (virtual kernel entry, ap_fn)
    cpacr: u64,   // +56
    cntkctl: u64, // +64
}

/// The trampoline reads this struct by hand-written byte offsets in a
/// `naked_asm!` block that cannot see a single Rust name. That duplication was
/// held together by the `// +0`, `// +8` comments above and by nothing else;
/// reorder a field and the secondary loads its `SCTLR` out of the `MAIR` slot,
/// with the MMU off, before anything can print. Say the offsets once more in a
/// form the compiler checks -- this is the same thing x86_64's `smp.rs` does
/// for its trampoline slots.
const _: () = {
    use core::mem::{offset_of, size_of};
    assert!(offset_of!(SecondaryContext, ttbr0) == 0);
    assert!(offset_of!(SecondaryContext, ttbr1) == 8);
    assert!(offset_of!(SecondaryContext, tcr) == 16);
    assert!(offset_of!(SecondaryContext, mair) == 24);
    assert!(offset_of!(SecondaryContext, sctlr) == 32);
    assert!(offset_of!(SecondaryContext, sp) == 40);
    assert!(offset_of!(SecondaryContext, entry) == 48);
    assert!(offset_of!(SecondaryContext, cpacr) == 56);
    assert!(offset_of!(SecondaryContext, cntkctl) == 64);
    // And nothing past the last offset the trampoline knows about, so a field
    // added at the end is a field no secondary would ever read.
    assert!(size_of::<SecondaryContext>() == 72);
};

/// Physical address of a kernel virtual address.
fn virt_to_phys(va: usize) -> usize {
    va - KCONFIG.phys_to_virt_offset
}

/// Translation registers captured from the BSP, shared by every secondary.
struct TransRegs {
    ttbr0: u64,
    ttbr1: u64,
    tcr: u64,
    mair: u64,
    sctlr: u64,
    /// CPACR_EL1 and CNTKCTL_EL1 are banked per CPU and reset to "trap", so a
    /// core that does not get them traps on its first FP/SIMD instruction with
    /// `EC=0x07` -- and `trap_handler` contains one, so the handler takes the
    /// same trap again, forever, until the pushed frames run off the bottom of
    /// the stack and the loop settles into a write fault at
    /// `__vectors + 0x200`. That is what every aarch64 boot with more than one
    /// core did, silently: the one core still able to print was waiting on a
    /// frame-allocator lock a dead one held. They are set in the trampoline
    /// rather than in `secondary_init`, which is already several calls of
    /// compiled Rust too late.
    cpacr: u64,
    cntkctl: u64,
}

/// Build a TTBR0 page table that identity-maps the trampoline code page so the PC
/// remains valid across the MMU-enable step. Leaked: it must outlive the bring-up.
fn build_identity_ttbr0(tramp_phys: usize) -> u64 {
    let mut pt = PageTable::new();
    let base = tramp_phys & !(PAGE_SIZE - 1);
    // Two pages, in case the trampoline straddles a page boundary.
    pt.map_cont(
        base,
        PAGE_SIZE * 2,
        base,
        MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE,
    )
    .expect("[smp] identity-map trampoline failed");
    let token = pt.table_phys() as u64;
    core::mem::forget(pt); // keep the table alive for the lifetime of the APs
    token
}

/// Clean `[base, base + len)` out of this PE's data caches to the **point of
/// coherency**.
///
/// A secondary starts with the MMU off, and with the MMU off every data access
/// it makes is Device-nGnRnE: non-cacheable, and by the architecture's own
/// rules (ARM ARM, "Mismatched memory attributes") *not coherent* with the
/// cacheable writes the BSP made to the same bytes. The
/// [`SecondaryContext`] is built through an ordinary cacheable kernel mapping
/// moments before `CPU_ON`, so without this its stores can still be sitting in
/// the BSP's own L1 when the secondary reads the address -- and what the
/// secondary reads then is whatever the heap happened to hold before, i.e. a
/// junk `TTBR1`, a junk `SCTLR` and a junk entry point, taken in that order
/// with no way to report any of it.
///
/// QEMU does not model caches, so this costs nothing there and shows up
/// nowhere; on silicon it is the difference between a core that boots and a
/// core that never speaks again.
fn clean_dcache_to_poc(base: usize, len: usize) {
    let ctr: u64;
    // CTR_EL0 is readable at EL1 and reports the *minimum* line length over
    // every cache in the coherency domain, which is exactly the stride that is
    // safe to use for all of them.
    unsafe { core::arch::asm!("mrs {0}, ctr_el0", out(reg) ctr, options(nomem, nostack)) };
    let line = crate::common::cache_maint::dcache_line_size(ctr);
    crate::common::cache_maint::for_each_line(base, len, line, |addr| {
        // By virtual address, so this is the BSP's own mapping of the object,
        // not the physical address handed to PSCI.
        unsafe {
            core::arch::asm!("dc cvac, {0}", in(reg) addr, options(nostack, preserves_flags))
        };
    });
    // The clean must have landed before `CPU_ON` lets the other core look.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Start all secondary cores. Called once from the BSP `primary_init`, after the
/// kernel page table and GIC are initialised.
pub fn start_secondary_cores() {
    // `smp=off` is the escape hatch for bringing a suspect machine up
    // single-core without a rebuild, and it was honoured on exactly one of the
    // three architectures. `zCore`'s boot log prints "single-core boot forced
    // (smp=off)" on all three, and this side then started every core it could
    // find — a switch the kernel says it obeyed and did not.
    if !crate::common::ipi::smp_enabled() {
        crate::klog_warn!("[smp] secondary bring-up disabled by `smp=off` — single-core");
        return;
    }
    let max_aps = crate::config::MAX_CORE_NUM - 1;

    let tramp_phys = virt_to_phys(secondary_trampoline as *const () as usize);
    let regs = TransRegs {
        ttbr0: build_identity_ttbr0(tramp_phys),
        ttbr1: TTBR1_EL1.get(),
        tcr: TCR_EL1.get(),
        mair: MAIR_EL1.get(),
        sctlr: SCTLR_EL1.get(),
        cpacr: CPACR_EL1.get(),
        cntkctl: {
            let v: u64;
            unsafe { core::arch::asm!("mrs {0}, cntkctl_el1", out(reg) v) };
            v
        },
    };

    crate::klog_info!("[smp] starting secondary cores (PSCI CPU_ON)");

    // Which affinities to probe, and what PSCI's answer means, live in
    // `crate::affinity_walk` -- and are tested there, which nothing here can
    // be. What this loop used to be was `for aff in 1..=63`, stopping at the
    // first `INVALID_PARAMETERS`: it could not name a core outside cluster 0
    // (an affinity is `Aff1 << 8 | Aff0`, and `Aff1` is the cluster), and one
    // hole in the numbering cost every core behind it.
    let mut walk = AffinityWalk::new(super::cpu::raw_affinity(), max_aps);
    let mut failures = 0usize;
    // One stack, carried across probes until a core actually takes it. A
    // probing walk spends most of its calls finding the edges of clusters, and
    // a fresh 256 KiB allocation freed again for each of those is pure boot
    // churn -- the old walk paid it once because it stopped at the first miss;
    // this one tolerates holes, so it would pay it a dozen times.
    let mut spare: Option<usize> = None;
    while let Step::Probe(aff) = walk.next_candidate() {
        let stack_top = match spare.take().or_else(alloc_stack) {
            Some(top) => top,
            None => {
                crate::klog_warn!("[smp] out of memory allocating AP stack");
                break;
            }
        };

        let ctx = alloc::boxed::Box::new(SecondaryContext {
            ttbr0: regs.ttbr0,
            ttbr1: regs.ttbr1,
            tcr: regs.tcr,
            mair: regs.mair,
            sctlr: regs.sctlr,
            cpacr: regs.cpacr,
            cntkctl: regs.cntkctl,
            sp: stack_top as u64,
            entry: KCONFIG.ap_fn as usize as u64,
        });
        // Kept as a Box until CPU_ON is known to have succeeded: most probes
        // of a walk that discovers a machine by asking are answered "no such
        // core", and leaking the stack + context of each of those threw away a
        // 256 KiB stack apiece.
        let ctx_phys = virt_to_phys(ctx.as_ref() as *const _ as usize) as u64;
        // The secondary reads this with the MMU off, so the BSP's cacheable
        // stores have to be pushed out to memory first; see below.
        clean_dcache_to_poc(
            ctx.as_ref() as *const _ as usize,
            core::mem::size_of::<SecondaryContext>(),
        );

        // Fire CPU_ON and move on: the AP only proceeds past its `STARTED` gate
        // once the BSP finishes init, so waiting for it to come online here would
        // just stall boot. `ap_signal_online` tracks the real online count.
        let ret = unsafe { psci_cpu_on(aff as u64, tramp_phys as u64, ctx_phys) };
        let outcome = affinity_walk::outcome(ret);
        walk.saw(outcome);
        match outcome {
            Outcome::Started => {
                // The secondary reads this context with the MMU off, long after
                // we return: it must outlive us. Same for its stack.
                let _ = alloc::boxed::Box::leak(ctx);
                crate::klog_info!("[smp] CPU_ON affinity {:#x} -> ok", aff);
                continue;
            }
            // Expected, once per cluster edge: this is how a probing walk
            // finds out where a cluster ends. Saying it out loud printed a
            // line per absent core on any machine with more than one cluster.
            Outcome::Absent => {}
            Outcome::AlreadyOn => crate::klog_warn!("[smp] affinity {:#x} already on", aff),
            Outcome::Failed(code) => {
                failures += 1;
                crate::klog_warn!("[smp] CPU_ON affinity {:#x} failed: {}", aff, code);
            }
        }
        // Any non-success path: this core never started, so its stack is free
        // for the next candidate (the context Box drops here on its own).
        spare = Some(stack_top);
    }
    if let Some(top) = spare.take() {
        free_stack(top);
    }

    if failures > 0 {
        crate::klog_warn!(
            "[smp] {} core(s) exist and would not start — this machine is running with \
             fewer CPUs than it has",
            failures
        );
    }
    crate::klog_info!(
        "[smp] secondary bring-up done — {} CPU_ON issued",
        walk.started()
    );
}

/// Called by each secondary from `secondary_init` to announce it is running.
pub fn ap_signal_online() {
    // Join the online set, exactly as x86's `ap_signal_online` does. Without
    // this the mask stayed at "BSP only" forever on aarch64, so every consumer
    // of `cpu_online_mask()` / `online_cpu_count()` — the `/proc` accounting and
    // the affinity syscalls among them — reported a uniprocessor machine no
    // matter how many cores had actually come up.
    crate::common::ipi::mark_cpu_online(super::cpu::cpu_id() as usize);
    AP_ONLINE_COUNT.fetch_add(1, Ordering::Release);
}

fn stack_layout() -> alloc::alloc::Layout {
    alloc::alloc::Layout::from_size_align(STACK_SIZE, PAGE_SIZE).unwrap()
}

fn alloc_stack() -> Option<usize> {
    let base = unsafe { alloc::alloc::alloc_zeroed(stack_layout()) };
    if base.is_null() {
        None
    } else {
        Some(base as usize + STACK_SIZE)
    }
}

/// Give back a stack from [`alloc_stack`] whose core never started.
fn free_stack(stack_top: usize) {
    let base = (stack_top - STACK_SIZE) as *mut u8;
    unsafe { alloc::alloc::dealloc(base, stack_layout()) };
}

/// PSCI `CPU_ON` via the HVC conduit (QEMU `virt` uses HVC, as does `reset`).
unsafe fn psci_cpu_on(target: u64, entry: u64, context: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "hvc #0",
        inout("x0") PSCI_CPU_ON => ret,
        in("x1") target,
        in("x2") entry,
        in("x3") context,
        out("x4") _,
        out("x5") _,
        out("x6") _,
        out("x7") _,
        options(nostack),
    );
    ret
}

/// Secondary entry, reached via PSCI with the MMU **off** and `x0` = physical
/// `*const SecondaryContext`. Restores the BSP translation regs, enables the MMU,
/// and jumps to the kernel entry on the AP stack. Position-independent: it must
/// not reference any absolute (high-half) symbol before the MMU is on.
#[unsafe(naked)]
unsafe extern "C" fn secondary_trampoline() -> ! {
    core::arch::naked_asm!(
        "ldr x1, [x0, #0]", // ttbr0 (identity table)
        "msr ttbr0_el1, x1",
        "ldr x1, [x0, #8]", // ttbr1 (kernel table)
        "msr ttbr1_el1, x1",
        "ldr x1, [x0, #16]", // tcr
        "msr tcr_el1, x1",
        "ldr x1, [x0, #24]", // mair
        "msr mair_el1, x1",
        // Load stack/entry (virtual) into callee regs before the MMU is on, while
        // the physical context pointer in x0 is still valid.
        "ldr x9, [x0, #40]",  // sp
        "ldr x10, [x0, #48]", // entry
        "ldr x11, [x0, #56]", // cpacr (FP/SIMD not trapped)
        "ldr x12, [x0, #64]", // cntkctl (EL0 may read the counters)
        "dsb sy",
        "isb",
        "tlbi vmalle1",
        "dsb sy",
        "isb",
        "ldr x1, [x0, #32]", // sctlr (with M/C/I set) -> enable MMU
        "msr sctlr_el1, x1",
        "isb",
        // Per-CPU, and reset to "trap" on every core: see `TransRegs::cpacr`.
        "msr cpacr_el1, x11",
        "msr cntkctl_el1, x12",
        "isb",
        "mov sp, x9",
        "br x10",
    )
}
