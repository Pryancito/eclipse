//! CPU information.

use cortex_a::registers::*;
use tock_registers::interfaces::Readable;

use crate::common::cpu_topology::CpuTopology;

// ─── CPU topology: dense logical id  <->  MPIDR affinity ─────────────────────────
//
// MPIDR_EL1 affinity fields (Aff0..Aff3) are sparse: Aff0 repeats across clusters,
// so it cannot index per-CPU arrays. Each online CPU gets a dense logical id
// (0..NCPU, boot CPU = 0) which is stored in TPIDR_EL1 (read by `lock`/`cpu_id`).
// We keep the reverse map (logical -> affinity) for targeting GIC SGIs.

/// This machine's dense logical id <-> packed MPIDR affinity map (index 0 = the
/// boot CPU). Shared with x86_64 and riscv so the three cannot drift apart
/// again: this side used to count in a `u8` with no bound, and to answer "which
/// core is logical N?" with affinity 0 — the boot CPU — for an N nobody had
/// ever registered.
static TOPOLOGY: CpuTopology = CpuTopology::new();

/// Packed MPIDR affinity (Aff3<<24 | Aff2<<16 | Aff1<<8 | Aff0) of the current CPU.
///
/// Use this — not [`cpu_id`](crate::cpu::cpu_id) — whenever a *hardware* CPU
/// identifier is required (PSCI `CPU_ON` target, GIC affinity routing).
pub fn raw_affinity() -> u32 {
    let mpidr = MPIDR_EL1.get();
    let aff0 = (mpidr & 0xff) as u32;
    let aff1 = ((mpidr >> 8) & 0xff) as u32;
    let aff2 = ((mpidr >> 16) & 0xff) as u32;
    let aff3 = ((mpidr >> 32) & 0xff) as u32;
    (aff3 << 24) | (aff2 << 16) | (aff1 << 8) | aff0
}

/// Assign this CPU its dense logical id, publish it in TPIDR_EL1, and record the
/// reverse (logical -> affinity) map. Called once per CPU from `percpu::register`.
pub fn register_logical_id() -> u8 {
    let affinity = raw_affinity();
    // A core past what the per-CPU tables hold gets no id rather than one that
    // indexes nothing: the count used to be an `AtomicU8` incremented
    // unconditionally, so core 64 got id 64, no reverse-map entry (the
    // `.get()` dropped it silently), and a TPIDR_EL1 value that every per-CPU
    // array in the kernel would either refuse or alias onto another core.
    let Some(logical) = TOPOLOGY.register(affinity) else {
        warn!(
            "[smp] affinity {:#x} has no logical id left (max {}) — it must not run kernel code",
            affinity,
            crate::config::MAX_CORE_NUM
        );
        return u8::MAX;
    };
    // Register the affinity -> logical mapping BEFORE publishing the id in
    // TPIDR_EL1. TPIDR_EL1 is an ordinary writable system register that reads
    // whatever reset left in it on a core the kernel has not set up yet, so
    // `lock` cross-checks it against the ids bring-up actually registered
    // — exactly as it does with GS on x86_64 — and without this call every
    // core would look like a lie and resolve through the affinity instead.
    lock::set_logical_cpu_id(affinity, logical as u8);
    unsafe { core::arch::asm!("msr tpidr_el1, {0}", in(reg) logical as u64) };
    logical as u8
}

/// Translate a dense logical CPU id back to its packed MPIDR affinity, or
/// `None` when no core was ever given that id.
///
/// `None` rather than a fallback, for the reason x86_64's `logical_to_apic`
/// gives: affinity 0 is the boot CPU, so answering "unknown" with 0 does not
/// drop the message — it aims it at the BSP, while the core it was meant for
/// hears nothing.
pub fn logical_to_affinity(logical: usize) -> Option<u32> {
    TOPOLOGY.hw_id(logical)
}

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        fn cpu_id() -> u8 {
            // Dense logical id (from TPIDR_EL1 via `lock`); see module docs.
            lock::current_cpu_id()
        }

        fn cpu_frequency() -> u16 {
            0
        }

        fn cpu_brand() -> alloc::string::String {
            alloc::string::String::from("AArch64 CPU")
        }

        fn cpu_count() -> u8 {
            TOPOLOGY.count() as u8
        }

        fn reset() -> ! {
            info!("shutdown...");
            let psci_system_off = 0x8400_0008_usize;
            unsafe {
                core::arch::asm!(
                    "hvc #0",
                    in("x0") psci_system_off
                );
            }
            unreachable!()
        }
    }
}
