//! Per-CPU interrupt-disable bookkeeping, and the answer to "which CPU is
//! this?" that picks the slot it lives in.
//!
//! The architecture-specific part is small and is confined to the `interrupts`
//! module below: read this CPU's *hardware* id, read the logical id this CPU
//! *published* about itself (if the architecture has such a thing), and turn
//! interrupts on and off. Everything built on top of that — the id map, the
//! cross-check, the nesting counter — is [`crate::cpuid`], which compiles and
//! is tested on the host.

use core::cell::UnsafeCell;

use crate::cpuid::{LogicalIdMap, PopError};

cfg_if::cfg_if! {
    if #[cfg(all(target_os = "none", any(target_arch = "riscv32", target_arch = "riscv64")))] {
        mod interrupts {
            use riscv::register::sstatus;

            /// Raw hart id of the current CPU (kernel convention: stored in `tp`).
            ///
            /// The full width, not `as u8`: hart ids are sparse by definition
            /// (boards reserve hart 0, and cluster numbering leaves gaps), and
            /// truncating hart 256 to a byte used to land it on hart 0 — the
            /// boot hart, whose per-CPU slot it would then share.
            pub(super) fn raw_hw_id() -> u32 {
                let hart_id: usize;
                unsafe {
                    core::arch::asm!("mv {0}, tp", out(reg) hart_id);
                }
                hart_id as u32
            }

            /// riscv publishes no logical id: `tp` holds the *hart* id, which
            /// is a hardware id and goes through the map like any other.
            pub(super) fn published_cpu_id() -> Option<u8> {
                None
            }

            pub(crate) fn intr_on() {
                unsafe { sstatus::set_sie() };
            }
            pub(crate) fn intr_off() {
                unsafe { sstatus::clear_sie() };
            }
            pub(crate) fn intr_get() -> bool {
                sstatus::read().sie()
            }
        }
    } else if #[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))] {
        mod interrupts {
            use core::sync::atomic::{AtomicU64, Ordering};
            use x86_64::instructions::interrupts;

            /// `IA32_APIC_BASE`. Bit 11 = APIC global enable, bit 10 = x2APIC mode.
            const IA32_APIC_BASE: u32 = 0x1B;
            const APIC_BASE_ENABLE: u64 = 1 << 11;
            const APIC_BASE_EXTD: u64 = 1 << 10;
            /// `IA32_X2APIC_APICID` — the APIC ID register in x2APIC mode.
            const IA32_X2APIC_APICID: u32 = 0x802;

            /// `phys + offset` virtual mapping for the LAPIC MMIO page (set by HAL at boot).
            static PHYS_VIRT_OFFSET: AtomicU64 = AtomicU64::new(0);

            /// Register the kernel's phys→virt linear map offset (from UEFI/boot config).
            pub fn set_phys_virt_offset(offset: u64) {
                PHYS_VIRT_OFFSET.store(offset, Ordering::Release);
            }

            /// Whether this CPU's Local APIC is in x2APIC mode.
            ///
            /// Load-bearing: in x2APIC mode the LAPIC **stops decoding its MMIO
            /// page**, so the register window used below reads whatever the
            /// (unclaimed) bus returns — typically all-ones. Every APIC register
            /// must go through the MSR interface once this is set.
            fn x2apic_active() -> bool {
                let base = unsafe { x86_64::registers::model_specific::Msr::new(IA32_APIC_BASE).read() };
                base & (APIC_BASE_ENABLE | APIC_BASE_EXTD) == (APIC_BASE_ENABLE | APIC_BASE_EXTD)
            }

            /// Read the Local APIC ID from the MMIO register (xAPIC mode only).
            fn read_lapic_id_mmio() -> Option<u32> {
                use x86_64::registers::model_specific::Msr;
                let offset = PHYS_VIRT_OFFSET.load(Ordering::Acquire);
                if offset == 0 {
                    return None;
                }
                let base = unsafe { Msr::new(IA32_APIC_BASE).read() };
                if base & APIC_BASE_ENABLE == 0 || base & APIC_BASE_EXTD != 0 {
                    // Disabled, or x2APIC: the MMIO window is not readable.
                    return None;
                }
                let page_phys = (base & 0xFFFF_F000) as u64;
                let id_ptr = (page_phys.wrapping_add(offset) + 0x20) as *const u32;
                let id_reg = unsafe { core::ptr::read_volatile(id_ptr) };
                // xAPIC keeps the id in bits 31:24 of the ID register.
                Some(id_reg >> 24)
            }

            /// Initial APIC ID from CPUID, used when the LAPIC itself cannot be
            /// queried yet. Leaf 0x0B (x2APIC topology) reports the full 32-bit
            /// id; the legacy leaf 1 field is only 8 bits wide.
            fn cpuid_apic_id() -> u32 {
                use core::arch::x86_64::{__cpuid, __cpuid_count};
                if __cpuid(0).eax >= 0x0B {
                    let leaf = __cpuid_count(0x0B, 0);
                    // EBX[15:0] == 0 means the leaf is not valid on this CPU.
                    if leaf.ebx & 0xFFFF != 0 {
                        return leaf.edx;
                    }
                }
                __cpuid(1).ebx >> 24
            }

            /// Raw Local APIC ID of the current CPU (hardware id, sparse and — in
            /// x2APIC mode — up to 32 bits wide).
            pub(super) fn raw_hw_id() -> u32 {
                if x2apic_active() {
                    return unsafe {
                        x86_64::registers::model_specific::Msr::new(IA32_X2APIC_APICID).read() as u32
                    };
                }
                read_lapic_id_mmio().unwrap_or_else(cpuid_apic_id)
            }

            /// The logical id this CPU published about itself, in the per-CPU
            /// area GS points at. One register-relative read, which is what
            /// keeps `cpu_id()` cheap enough to sit on every lock acquire —
            /// and a *corruptible* one, which is why the caller cross-checks it.
            #[cfg(target_arch = "x86_64")]
            pub(super) fn published_cpu_id() -> Option<u8> {
                if trapframe::logical_cpu_id_valid() {
                    Some(trapframe::read_logical_cpu_id())
                } else {
                    None
                }
            }

            #[cfg(not(target_arch = "x86_64"))]
            pub(super) fn published_cpu_id() -> Option<u8> {
                None
            }

            pub(crate) fn intr_on() {
                interrupts::enable();
            }
            pub(crate) fn intr_off() {
                interrupts::disable();
            }
            pub(crate) fn intr_get() -> bool {
                interrupts::are_enabled()
            }
        }
    } else if #[cfg(all(target_os = "none", target_arch = "aarch64"))] {
        mod interrupts {
            /// Packed MPIDR_EL1 affinity (Aff3<<24 | Aff2<<16 | Aff1<<8 | Aff0)
            /// of the current CPU — the hardware id, matching what
            /// `kernel-hal`'s `cpu::raw_affinity` registers.
            ///
            /// Sparse: Aff0 repeats across clusters, so it cannot index a
            /// per-CPU array; that is what the logical id is for.
            pub(super) fn raw_hw_id() -> u32 {
                use cortex_a::registers::MPIDR_EL1;
                use tock_registers::interfaces::Readable;
                let mpidr = MPIDR_EL1.get();
                let aff0 = (mpidr & 0xff) as u32;
                let aff1 = ((mpidr >> 8) & 0xff) as u32;
                let aff2 = ((mpidr >> 16) & 0xff) as u32;
                let aff3 = ((mpidr >> 32) & 0xff) as u32;
                (aff3 << 24) | (aff2 << 16) | (aff1 << 8) | aff0
            }

            /// The logical id this CPU published about itself, in TPIDR_EL1.
            ///
            /// aarch64's twin of x86's GS, and corruptible the same way: it is
            /// an ordinary writable system register, it reads whatever reset
            /// left in it on a core the kernel has not set up yet, and 0 —
            /// which is what an unset one usually reads — is the boot CPU.
            /// So it goes through the same cross-check.
            pub(super) fn published_cpu_id() -> Option<u8> {
                let id: u64;
                unsafe { core::arch::asm!("mrs {0}, tpidr_el1", out(reg) id) };
                Some(id as u8)
            }

            pub(crate) fn intr_on() {
                unsafe {
                    core::arch::asm!("msr daifclr, #2");
                }
            }
            pub(crate) fn intr_off() {
                unsafe {
                    core::arch::asm!("msr daifset, #2");
                }
            }
            pub(crate) fn intr_get() -> bool {
                use cortex_a::registers::DAIF;
                use tock_registers::interfaces::Readable;
                !DAIF.is_set(DAIF::I)
            }
        }
    } else if #[cfg(test)] {
        /// Host backend for `cargo test`: one simulated CPU per test thread.
        ///
        /// See `KERNEL_LOCKS_ON_HOST` in `lib.rs`. There is no such thing as
        /// "this CPU" on a hosted target, so the test build hands each thread
        /// its own hardware id and its own interrupt flag; that is enough for
        /// the question every lock in this crate turns on — whether a guard's
        /// `push_off` and `pop_off` come in pairs, on one slot.
        mod interrupts {
            use core::cell::Cell;

            std::thread_local! {
                /// This thread's simulated interrupt-enable flag. Starts on,
                /// like a CPU running ordinary kernel code.
                static IRQ_ON: Cell<bool> = const { Cell::new(true) };
                /// This thread's simulated hardware CPU id.
                static HW_ID: Cell<u32> = const { Cell::new(0) };
                /// What this thread publishes about itself, if anything.
                static PUBLISHED: Cell<Option<u8>> = const { Cell::new(None) };
                /// Stands in for an interrupt arriving the instant this thread
                /// re-enables them, which is the only way to observe what a
                /// guard's `Drop` did *before* its `pop_off` -- and every guard
                /// in this crate releases its lock first on purpose, so that an
                /// arriving handler finds the lock takeable rather than
                /// deadlocking against a guard that has not let go yet.
                static ON_IRQ_ENABLE: Cell<Option<fn()>> = const { Cell::new(None) };
            }

            pub(super) fn raw_hw_id() -> u32 {
                HW_ID.with(|c| c.get())
            }

            pub(super) fn published_cpu_id() -> Option<u8> {
                PUBLISHED.with(|c| c.get())
            }

            pub(crate) fn intr_on() {
                IRQ_ON.with(|c| c.set(true));
                // Taken, not read: the stand-in interrupt fires once, like a
                // real one, and cannot recurse through a handler that enables
                // interrupts itself.
                if let Some(handler) = ON_IRQ_ENABLE.with(|c| c.take()) {
                    handler();
                }
            }
            pub(crate) fn intr_off() {
                IRQ_ON.with(|c| c.set(false));
            }
            pub(crate) fn intr_get() -> bool {
                IRQ_ON.with(|c| c.get())
            }

            /// Make this test thread be hardware CPU `hw`.
            pub(crate) fn set_test_hw_id(hw: u32) {
                HW_ID.with(|c| c.set(hw));
            }

            /// Make this test thread publish `id` about itself (the twin of
            /// writing GS / TPIDR_EL1).
            pub(crate) fn set_test_published(id: Option<u8>) {
                PUBLISHED.with(|c| c.set(id));
            }

            /// Arm one stand-in interrupt for the next `intr_on` on this thread.
            pub(crate) fn arm_irq_on_enable(handler: fn()) {
                ON_IRQ_ENABLE.with(|c| c.set(Some(handler)));
            }

            /// Disarm it, whether or not it fired.
            pub(crate) fn disarm_irq_on_enable() {
                ON_IRQ_ENABLE.with(|c| c.take());
            }
        }
    } else {
        mod interrupts {
            pub(super) fn raw_hw_id() -> u32 {
                unimplemented!();
            }
            pub(super) fn published_cpu_id() -> Option<u8> {
                unimplemented!();
            }
            pub(crate) fn intr_on() { unimplemented!(); }
            pub(crate) fn intr_off() { unimplemented!(); }
            pub(crate) fn intr_get() -> bool {
                unimplemented!();
            }
        }
    }
}

use interrupts::*;
#[cfg(test)]
pub(crate) use interrupts::{set_test_hw_id, set_test_published};

/// This machine's hardware-id <-> dense-logical-id map.
///
/// One per machine, shared by all three architectures so they cannot drift
/// apart: each used to keep its own, and two of them answered "the boot CPU"
/// for a hardware id nobody had ever registered. See [`crate::cpuid`].
static LOGICAL_IDS: LogicalIdMap = LogicalIdMap::new();

/// Current CPU's dense logical id (0..NCPU), or [`NO_CPU`].
///
/// Three sources, in order of how much they can be trusted:
///
/// 1. an open AP-boot window, which is the only thing that knows who a CPU is
///    before its per-CPU publisher exists;
/// 2. the publisher itself (x86 `GS`, aarch64 `TPIDR_EL1`) — one register read,
///    and the reason this function is cheap enough to sit on every lock
///    acquire — *cross-checked* against the ids bring-up actually registered,
///    because a `swapgs` imbalance or a wild write makes it name a CPU that
///    does not exist;
/// 3. the hardware id (Local APIC ID, hart id, MPIDR affinity), which no
///    memory corruption can reach.
///
/// Answering [`NO_CPU`] rather than 0 is the point: 0 is the boot CPU
/// everywhere, so a wrong 0 does not lose the answer, it silently nests this
/// CPU's interrupt-disable depth in the BSP's slot.
pub fn current_cpu_id() -> u8 {
    // The relaxed mask load short-circuits this in the steady state. It
    // matters: this runs on every `push_off`/`pop_off`, i.e. on every kernel
    // lock acquire and release, and reading a hardware id costs an RDMSR plus
    // (in xAPIC mode) an uncached MMIO read.
    if LOGICAL_IDS.ap_boot_any() {
        if let Some(logical) = LOGICAL_IDS.ap_boot_logical(raw_hw_id()) {
            return logical;
        }
    }
    if let Some(published) = published_cpu_id() {
        if LOGICAL_IDS.accepts_published(published) {
            return published;
        }
        // No printing from here: every console writer takes a lock and would
        // re-enter this very function. Record it for the panic reporter and
        // fall through to the hardware id.
        LOGICAL_IDS.note_bogus(published);
    }
    LOGICAL_IDS.resolve(raw_hw_id())
}

pub(crate) use current_cpu_id as cpu_id;

/// Dense logical id resolved **only** from the hardware id — never from the
/// per-CPU publisher.
///
/// Use this from the NMI path. `syscall_return` does `swapgs` then WRMSR of the
/// user gsbase while CS is still ring 0; an NMI in that window takes the
/// `__from_kernel` trampoline (no swapgs) and would otherwise read `cpu_id`
/// out of user GS. A non-zero byte at `logical_cpu_valid`'s offset in that
/// user mapping looks "valid" and publishes the TLB-shootdown watermark into
/// the **wrong** `SHOOTDOWN_SEQ` slot — the surviving rival hypothesis for
/// "NMI ran, nmi_rip fresh, watermark never moved".
///
/// Which is why this one must not fall back to 0 either: a hardware id that
/// resolves to nothing used to come back as the boot CPU, publishing that
/// watermark into slot 0 — the same corruption by a different door.
pub fn current_cpu_id_via_apic() -> u8 {
    LOGICAL_IDS.resolve(raw_hw_id())
}

/// `(last bogus id, count)` for logical cpu ids read out of a per-CPU
/// publisher that name no CPU SMP bring-up ever registered.
///
/// A non-zero count is not a warning, it is a diagnosis: this CPU ran with a
/// publisher that was lying about who it is, so every `push_off`/`pop_off` in
/// that window nested its IRQ-disable depth on a foreign per-CPU slot. The
/// kernel's fault and panic reporters print it, because it explains classes of
/// damage (locks released with interrupts on, re-entrant acquires, scribbled
/// per-CPU state) that otherwise look impossible from the backtrace alone.
///
/// Reading it is allocation- and lock-free, so it is safe from a fault path.
pub fn bogus_cpu_id_events() -> (u32, u32) {
    LOGICAL_IDS.bogus_events()
}

/// Raw hardware Local APIC ID (x86). Sparse, and up to 32 bits wide in x2APIC
/// mode; use [`current_cpu_id`] to index arrays.
#[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
pub fn hardware_apic_id() -> u32 {
    interrupts::raw_hw_id()
}

/// Register the dense logical id assigned to a hardware CPU id (Local APIC ID
/// on x86, hart id on riscv, packed MPIDR affinity on aarch64).
///
/// Must be called once per CPU (including the BSP) before that CPU executes any
/// code that takes a lock, so that `cpu_id()` never returns a stale/colliding
/// id. Returns `false` for a logical id no per-CPU array can hold, rather than
/// writing an entry that indexes nothing.
pub fn set_logical_cpu_id(hw_id: u32, logical_id: u8) -> bool {
    LOGICAL_IDS.register(hw_id, logical_id)
}

/// Undo [`set_logical_cpu_id`] for `logical_id`: after this, the hardware CPU
/// it named resolves to [`cpuid::NO_CPU`] rather than to an id whose per-CPU
/// slots belong to nobody. Returns whether anything was registered.
///
/// Pairs with `set_logical_cpu_id`, and the pair must stay together: the SMP
/// bring-up keeps the reverse map (logical -> hardware) of its own, and a CPU
/// dropped from one map and not the other is worse than a CPU in both.
pub fn clear_logical_cpu_id(logical_id: u8) -> bool {
    LOGICAL_IDS.unregister(logical_id)
}

/// The hardware id registered for a logical id, or `None`.
pub fn hardware_id_of(logical_id: u8) -> Option<u32> {
    LOGICAL_IDS.hw_of(logical_id)
}

/// Register phys→virt linear map offset for LAPIC MMIO reads on x86.
#[cfg(all(target_os = "none", any(target_arch = "x86", target_arch = "x86_64")))]
pub fn set_phys_virt_offset(offset: u64) {
    interrupts::set_phys_virt_offset(offset)
}

/// Run `f` while [`current_cpu_id`] returns `logical` for the calling CPU.
///
/// For the stretch of AP bring-up that runs before the per-CPU publisher is
/// set up: during `init_ap` on x86, GSBASE is still 0 and reading the
/// published id would fault or invent one.
pub fn with_ap_boot_logical<R>(logical: u8, f: impl FnOnce() -> R) -> R {
    if !LOGICAL_IDS.ap_boot_enter(logical, raw_hw_id()) {
        return f();
    }
    let ret = f();
    LOGICAL_IDS.ap_boot_leave(logical);
    ret
}

/// Per-CPU interrupt-disable depth, in its own cache line.
pub use crate::cpuid::IrqDepth as Cpu;

#[repr(align(64))]
pub struct CpuStorage(UnsafeCell<Cpu>);

// SAFETY: each CPU only ever accesses CPUS[cpu_id()]; wrong ids are fixed at AP boot.
unsafe impl Sync for CpuStorage {}

impl CpuStorage {
    const fn new() -> Self {
        Self(UnsafeCell::new(Cpu::new()))
    }

    // Deliberate: hands out a `&mut Cpu` view of an `UnsafeCell` from `&self`.
    // Each slot is per-CPU and only touched by its owning core, so the standard
    // `mut_from_ref` guard does not apply.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut Cpu {
        // SAFETY: caller ensures this slot is owned by the current CPU.
        unsafe { &mut *self.0.get() }
    }
}

// Avoid hard code
#[allow(clippy::declare_interior_mutable_const)]
const DEFAULT_CPU: CpuStorage = CpuStorage::new();

// Tamaño único de los arrays per-CPU del sistema (id lógico denso); lo
// reutilizan el scheduler (vendor/PreemptiveScheduler) y kernel-hal lo
// verifica en compilación contra su `config::MAX_CORE_NUM`.
use crate::MAX_CORE_NUM;

static CPUS: [CpuStorage; MAX_CORE_NUM] = [DEFAULT_CPU; MAX_CORE_NUM];

#[inline]
pub fn mycpu() -> &'static mut Cpu {
    let id = cpu_id() as usize;
    // Not a bounds check for its own sake: this is the one place that turns
    // "we do not know which CPU this is" into a stop, instead of letting it
    // become "slot 0", which is the boot CPU's.
    assert!(
        id < MAX_CORE_NUM,
        "cpu {} has no logical id: it is running kernel code without being registered",
        id
    );
    CPUS[id].get()
}

/// How many kernel lock guards this CPU currently holds.
///
/// Every guard handed out by this crate (`Mutex`, `TicketMutex`, `RwLock`, …)
/// brackets its lifetime with `push_off`/`pop_off`, so `noff == 0` means the
/// core holds **no** kernel lock: nothing is half-updated behind a lock and no
/// later acquisition of one can deadlock against this context. That is the
/// precondition the panic-recovery path (`zcore::oops`) tests before it dares
/// to run recovery code — which itself takes locks — from inside a panic.
///
/// Unlike [`mycpu`] this never asserts: it is called from the panic handler,
/// where a second panic (out-of-range cpu id) would abort the machine. An
/// unknown cpu id reports "locks held", i.e. the conservative answer.
#[inline]
pub fn lock_depth() -> i32 {
    let id = cpu_id() as usize;
    if id >= MAX_CORE_NUM {
        return i32::MAX;
    }
    CPUS[id].get().noff
}

// push_off/pop_off are like intr_off()/intr_on() except that they are matched:
// it takes two pop_off()s to undo two push_off()s.  Also, if interrupts
// are initially off, then push_off, pop_off leaves them off.
pub(crate) fn push_off() {
    let old = intr_get();
    intr_off();
    mycpu().push(old);
}

pub(crate) fn pop_off() {
    // NOTICE: intr_on() may lead to an immediate interrupt, so the Cpu borrow
    // must end before enabling IRQs — it ends with this statement.
    let should_enable = match mycpu().pop(intr_get()) {
        Ok(enable) => enable,
        // Two diagnoses, not one. Both mean the pair came apart, but they come
        // apart for different reasons and a bare "pop_off" names neither.
        Err(PopError::InterruptsEnabled) => panic!(
            "pop_off on cpu {}: interrupts are already on inside a critical section",
            cpu_id()
        ),
        Err(PopError::Underflow) => panic!(
            "pop_off on cpu {}: more lock releases than acquires on this slot",
            cpu_id()
        ),
    };
    if should_enable {
        intr_on();
    }
}

// Test-only windows onto the host backend, so the lock tests can put this
// thread on a given CPU and watch the interrupt flag the guards move.
#[cfg(test)]
pub(crate) fn intr_on_for_test() {
    intr_on()
}

#[cfg(test)]
pub(crate) fn intr_off_for_test() {
    intr_off()
}

#[cfg(test)]
pub(crate) fn intr_get_for_test() -> bool {
    intr_get()
}

/// Arm one stand-in interrupt to fire the next time this thread re-enables
/// interrupts — i.e. inside the outermost `pop_off`, which is the only vantage
/// point from which what a guard's `Drop` did before it can be seen.
#[cfg(test)]
pub(crate) fn arm_irq_on_enable_for_test(handler: fn()) {
    arm_irq_on_enable(handler)
}

#[cfg(test)]
pub(crate) fn disarm_irq_on_enable_for_test() {
    disarm_irq_on_enable()
}
