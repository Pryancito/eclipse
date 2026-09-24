//! CPU information.
use crate::common::cpu_topology::CpuTopology;
use crate::utils::init_once::InitOnce;

pub(super) static CPU_FREQ_MHZ: InitOnce<u16> = InitOnce::new_with_default(1000); // 1GHz

// ─── CPU topology: dense logical id  <->  hart id ───────────────────────────────
//
// Hart ids (in `tp`) may be sparse (some boards reserve hart 0), so they cannot be
// used directly to index per-CPU arrays. Each online hart gets a dense logical id
// (0..NCPU, boot hart = 0). The forward map (hart -> logical) lives in `lock` so
// the lock crate and the kernel share one id space; here we keep the reverse map
// (logical -> hart) needed to target SBI IPIs.

/// This machine's dense logical id <-> hart id map. Shared with x86_64 and
/// aarch64 so the three cannot drift apart again: this side used to count in a
/// `u8` with no bound, and to answer "which hart is logical N?" with hart 0 —
/// the boot hart — for an N nobody had ever registered.
static TOPOLOGY: CpuTopology = CpuTopology::new();

/// Raw hart id of the current CPU (kernel convention: stored in `tp`).
///
/// Use this — not [`cpu_id`](crate::cpu::cpu_id) — whenever a *hardware* hart id
/// is required (device-tree `riscv-intc-cpuN` nodes, PLIC contexts, SBI hart masks).
pub fn raw_hart_id() -> usize {
    let hart_id: usize;
    unsafe { core::arch::asm!("mv {0}, tp", out(reg) hart_id) };
    hart_id
}

/// Assign this hart its dense logical id and register the hart<->logical maps.
/// Called once per hart from `percpu::register`, before any lock-taking code.
pub fn register_logical_id() -> u8 {
    let hart_id = raw_hart_id() as u32;
    // A hart past what the per-CPU tables hold keeps the id it booted with
    // rather than being given one that indexes nothing: the count used to be
    // an `AtomicU8` incremented unconditionally, so hart 64 got id 64, no
    // reverse-map entry (the `.get()` dropped it silently), and a `lock`
    // forward-map entry pointing at a per-CPU slot it shares with somebody
    // else. There is nothing sound to do for such a hart here, but reporting
    // it is better than letting it corrupt another CPU's state in silence.
    let Some(logical) = TOPOLOGY.register(hart_id) else {
        warn!(
            "[smp] hart {} has no logical id left (max {}) — it must not run kernel code",
            hart_id,
            crate::config::MAX_CORE_NUM
        );
        return u8::MAX;
    };
    lock::set_logical_cpu_id(hart_id, logical as u8);
    logical as u8
}

/// Translate a dense logical CPU id back to its hart id (for SBI IPI delivery),
/// or `None` when no hart was ever given that id.
///
/// `None` rather than a fallback, for the reason x86_64's `logical_to_apic`
/// gives: hart 0 is the boot hart, so answering "unknown" with 0 does not drop
/// the IPI — it rings the BSP, which flushes a TLB nobody asked it about, while
/// the CPU the shootdown was for hears nothing and its initiator waits for an
/// acknowledgement in a loop that has no timeout.
pub fn logical_to_hart(logical: usize) -> Option<usize> {
    TOPOLOGY.hw_id(logical).map(|h| h as usize)
}

hal_fn_impl! {
    impl mod crate::hal_fn::cpu {
        fn cpu_id() -> u8 {
            // Dense logical id (0..NCPU), resolved from the sparse hart id via the
            // table in `lock`. Hart ids are not contiguous on all boards, so they
            // must not be used directly to index per-CPU arrays.
            lock::current_cpu_id()
        }

        fn cpu_frequency() -> u16 {
            *CPU_FREQ_MHZ
        }

        fn cpu_brand() -> alloc::string::String {
            alloc::string::String::from("RISC-V CPU")
        }

        fn cpu_count() -> u8 {
            TOPOLOGY.count() as u8
        }

        fn reset() -> ! {
            info!("shutdown...");
            sbi_rt::system_reset(sbi_rt::Shutdown, sbi_rt::NoReason);
            unreachable!()
        }
    }
}
