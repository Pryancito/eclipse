//! x86_64 CPU power management: P-state scaling (Intel HWP / EPB, AMD CPPC) and
//! a deeper idle entry (MONITOR/MWAIT).
//!
//! ## Why this module exists
//!
//! On real hardware an otherwise-idle Eclipse OS ran hot (~80 °C). The
//! scheduler already halts the CPU correctly when it runs out of work (see
//! [`super::interrupt`]'s `wait_for_interrupt`), which is exactly why **under
//! QEMU the host CPU use stays low** — the guest really does execute a halt and
//! the vCPU thread sleeps. The heat is bare-metal-only.
//!
//! It comes from two things the kernel never did:
//!
//! 1. **No P-state control.** With no OS-directed power management the firmware
//!    leaves each core at a high fixed performance point (often the maximum
//!    non-turbo ratio, sometimes turbo). High core voltage/frequency burns
//!    power — and therefore generates heat — even at 0 % load.
//! 2. **Shallow idle.** A plain `hlt` only reaches C1, which gates the core
//!    clock but keeps the voltage up.
//!
//! QEMU exposes neither HWP/CPPC nor real C-states to the guest and does not
//! model the silicon's voltage/frequency/thermals (the host OS governs the
//! physical CPU), so neither problem is observable there — both bite only on
//! physical silicon. This module addresses both, once per logical CPU at
//! bring-up:
//!
//! * **Hardware-autonomous P-states.** On Intel via HWP ("Speed Shift"), on AMD
//!   via CPPC. Enable the feature and let the CPU range from its *lowest* to its
//!   *highest* performance on its own, biased by the Energy-Performance
//!   Preference (EPP). An idle core then settles at its lowest voltage/frequency
//!   (cool); a busy core still ramps to full speed by itself — with no per-tick
//!   MSR pokes from the kernel.
//! * **Energy-Performance Bias.** The legacy pre-HWP Intel hint (Sandy Bridge …
//!   Broadwell, and HWP parts that lack the EPP field) nudges the package's
//!   internal P-state and turbo decisions toward efficiency.
//! * **MWAIT idle.** Where supported, park in C1E via MONITOR/MWAIT instead of
//!   `hlt`, shedding a little more idle voltage. C1/C1E never stop the LAPIC
//!   timer, so this stays correct with the kernel's tickless-idle scheduler
//!   tick (which relies on the LAPIC timer to wake an idle CPU).
//!
//! Everything is gated on CPUID and the CPU vendor, and is **bare-metal only**:
//! under any hypervisor the host owns the physical power state, so the whole
//! module no-ops. That keeps the already-correct QEMU idle behaviour untouched
//! and avoids a #GP on a VMM that advertises a feature in CPUID but does not
//! implement the backing MSR.

use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use x86_64::registers::model_specific::Msr;

// ── Intel HWP / EPB MSRs ────────────────────────────────────────────────────
/// IA32_PM_ENABLE — bit 0 turns HWP on (a write-once latch, sticky to reset).
const IA32_PM_ENABLE: u32 = 0x770;
/// IA32_HWP_CAPABILITIES — read-only performance bounds of this part.
const IA32_HWP_CAPABILITIES: u32 = 0x771;
/// IA32_HWP_REQUEST — the OS's per-logical-CPU P-state request to the hardware.
const IA32_HWP_REQUEST: u32 = 0x774;
/// IA32_ENERGY_PERF_BIAS — legacy 4-bit efficiency hint (pre-HWP).
const IA32_ENERGY_PERF_BIAS: u32 = 0x1B0;
/// IA32_PM_ENABLE.HWP_ENABLE.
const HWP_ENABLE: u64 = 1 << 0;

// ── AMD CPPC MSRs (modelled on Linux's `amd_pstate` EPP driver) ──────────────
/// MSR_AMD_CPPC_CAP1 — read-only performance bounds (Highest[31:24] … Lowest[7:0]).
const MSR_AMD_CPPC_CAP1: u32 = 0xC001_02B0;
/// MSR_AMD_CPPC_ENABLE — bit 0 turns CPPC on.
const MSR_AMD_CPPC_ENABLE: u32 = 0xC001_02B1;
/// MSR_AMD_CPPC_REQUEST — Max[7:0] | Min[15:8] | Desired[23:16] | EPP[31:24].
const MSR_AMD_CPPC_REQUEST: u32 = 0xC001_02B2;

// ── Tunables ────────────────────────────────────────────────────────────────
//
// Energy-Performance Preference written into the HWP/CPPC request [31:24]:
//   0x00 = maximum performance … 0xFF = maximum power saving.
// 0x80 is the "balanced" value used by most operating systems. The idle-heat
// win does NOT come from EPP — at idle the core parks at the *minimum*
// performance set below (its lowest P-state), independent of EPP — so 0x80
// captures essentially all of the cooling while keeping interactive ramp-up
// snappy. Raise toward 0xC0/0xFF for a more aggressive power/heat bias at the
// cost of some throughput under sustained load; lower toward 0x00 for more
// performance.
const EPP_BALANCED: u64 = 0x80;

// IA32_ENERGY_PERF_BIAS[3:0]: 0 = performance … 15 = power saving; 7 = balanced.
const EPB_BALANCED: u64 = 7;

// MWAIT idle hints (EAX). Bits [7:4] select the C-state, [3:0] the sub-state.
// 0x00 = C1, 0x01 = C1E. We deliberately go no deeper than C1E: C3+ can gate
// the LAPIC timer on parts without ARAT, and this kernel wakes idle CPUs from
// that timer, so a deeper state risks an over-long sleep / missed tick.
const MWAIT_HINT_C1: usize = 0x00;
const MWAIT_HINT_C1E: usize = 0x01;

/// Which P-state interface was enabled, for the boot log.
#[derive(Clone, Copy)]
enum PStateMech {
    IntelHwp,
    AmdCppc,
}

impl PStateMech {
    fn label(self) -> &'static str {
        match self {
            PStateMech::IntelHwp => "Intel HWP",
            PStateMech::AmdCppc => "AMD CPPC",
        }
    }
}

// ── Idle state shared with `wait_for_interrupt` ─────────────────────────────
/// Whether to idle via MONITOR/MWAIT (`true`) or fall back to `hlt` (`false`).
static IDLE_USE_MWAIT: AtomicBool = AtomicBool::new(false);
/// The MWAIT hint (C-state) chosen at init.
static IDLE_MWAIT_HINT: AtomicUsize = AtomicUsize::new(MWAIT_HINT_C1);

/// A cacheline that idle CPUs arm their MONITOR on. It is **never written**: we
/// rely solely on interrupts to break MWAIT, so this just gives MONITOR a valid
/// write-back address to watch. Aligned and padded to its own line so unrelated
/// stores never spuriously trip the monitor. Many CPUs may monitor it at once;
/// with no writes there are no store-wakeups, only the interrupt-wakeups we want.
#[repr(align(64))]
struct MonitorLine(AtomicU64);
static IDLE_MONITOR: MonitorLine = MonitorLine(AtomicU64::new(0));

/// Latches after the first CPU logs the power-management summary, so the APs
/// (which all run identical hardware) don't repeat it N times.
static SUMMARY_LOGGED: AtomicBool = AtomicBool::new(false);

// ── CPUID helpers ───────────────────────────────────────────────────────────
/// `true` when running under a hypervisor (CPUID.01H:ECX[31]). Reliably set by
/// QEMU/KVM and clear on bare metal, so it cleanly scopes this module to the
/// physical-hardware case it is meant for.
fn hypervisor_present() -> bool {
    __cpuid(1).ecx & (1 << 31) != 0
}

/// `true` on a "GenuineIntel" part.
fn is_intel() -> bool {
    let r = __cpuid(0);
    // "Genu" / "ineI" / "ntel" packed little-endian into EBX / EDX / ECX.
    r.ebx == 0x756e_6547 && r.edx == 0x4965_6e69 && r.ecx == 0x6c65_746e
}

/// `true` on an "AuthenticAMD" part.
fn is_amd() -> bool {
    let r = __cpuid(0);
    // "Auth" / "enti" / "cAMD" packed little-endian into EBX / EDX / ECX.
    r.ebx == 0x6874_7541 && r.edx == 0x6974_6e65 && r.ecx == 0x444d_4163
}

/// `true` if AMD Collaborative Processor Performance Control is present
/// (CPUID Fn8000_0008_EBX[27]). Guards access to the MSR_AMD_CPPC_* registers.
fn amd_has_cppc() -> bool {
    if __cpuid(0x8000_0000).eax < 0x8000_0008 {
        return false;
    }
    __cpuid(0x8000_0008).ebx & (1 << 27) != 0
}

// ── P-state programming (per logical CPU) ───────────────────────────────────
/// Enable Intel HWP on this CPU and request hardware-autonomous scaling across
/// the full [lowest, highest] range. Returns `(lowest, highest)` for logging.
///
/// SAFETY: writes IA32_PM_ENABLE / IA32_HWP_REQUEST, valid only when HWP is
/// supported (checked by the caller).
unsafe fn enable_hwp(has_epp: bool) -> (u8, u8) {
    // Turn HWP on (idempotent / sticky); IA32_HWP_REQUEST is only meaningful
    // once this bit is set.
    Msr::new(IA32_PM_ENABLE).write(HWP_ENABLE);

    let caps = Msr::new(IA32_HWP_CAPABILITIES).read();
    let highest = (caps & 0xff) as u8; // [7:0]   Highest_Performance
    let lowest = ((caps >> 24) & 0xff) as u8; // [31:24] Lowest_Performance

    // Minimum = lowest  → an idle core may drop to its lowest P-state (coolest).
    // Maximum = highest → a busy core may still reach full/turbo speed.
    // Desired = 0       → hardware chooses the operating point autonomously.
    // EPP only exists when CPUID.06H:EAX[10] is set; otherwise bits [31:24] are
    // reserved-zero and IA32_ENERGY_PERF_BIAS provides the bias instead.
    let epp = if has_epp { EPP_BALANCED } else { 0 };
    let request = (lowest as u64)
        | ((highest as u64) << 8)
        | (0u64 << 16)
        | (epp << 24);
    Msr::new(IA32_HWP_REQUEST).write(request);

    (lowest, highest)
}

/// Enable AMD CPPC on this CPU and request hardware-autonomous scaling across
/// the full [lowest, highest] range. Returns `(lowest, highest)` for logging.
///
/// SAFETY: writes the MSR_AMD_CPPC_* registers, valid only when CPPC is present
/// (checked by the caller). Note the field order differs from Intel HWP.
unsafe fn enable_amd_cppc() -> (u8, u8) {
    Msr::new(MSR_AMD_CPPC_ENABLE).write(1);

    let cap1 = Msr::new(MSR_AMD_CPPC_CAP1).read();
    let highest = ((cap1 >> 24) & 0xff) as u8; // [31:24] Highest_Performance
    let lowest = (cap1 & 0xff) as u8; // [7:0]    Lowest_Performance

    // REQUEST: Max[7:0]=highest, Min[15:8]=lowest, Desired[23:16]=0 (autonomous),
    // EPP[31:24]. Out-of-range fields are clamped by hardware to [lowest,highest].
    let request = (highest as u64)
        | ((lowest as u64) << 8)
        | (0u64 << 16)
        | (EPP_BALANCED << 24);
    Msr::new(MSR_AMD_CPPC_REQUEST).write(request);

    (lowest, highest)
}

/// Set the legacy Intel Energy-Performance Bias hint, preserving reserved bits.
///
/// SAFETY: writes IA32_ENERGY_PERF_BIAS, valid only when CPUID.06H:ECX[3] is
/// set (checked by the caller).
unsafe fn set_energy_perf_bias() {
    let mut msr = Msr::new(IA32_ENERGY_PERF_BIAS);
    let value = (msr.read() & !0xf) | EPB_BALANCED;
    msr.write(value);
}

// ── Public entry points ─────────────────────────────────────────────────────
/// Configure CPU power management for the calling logical CPU. Safe to call on
/// the BSP and on every AP — the HWP/CPPC/EPB MSRs are per-logical-CPU, so each
/// core must run it; the MWAIT-idle decision is a global latched once. No-ops on
/// a hypervisor or where CPUID reports the features absent.
pub(super) fn init() {
    if hypervisor_present() {
        log_summary_once(|| {
            info!("power: under hypervisor — leaving CPU power management to the host");
        });
        return;
    }

    let intel = is_intel();
    let amd = !intel && is_amd();
    let leaf1 = __cpuid(1);
    let leaf6 = __cpuid(6);

    // --- Hardware-autonomous P-states ---
    // Intel exposes HWP; AMD exposes the equivalent via CPPC. Both, once enabled,
    // scale the core autonomously across [lowest, highest], so an idle core drops
    // to its lowest voltage/frequency. EPB is an Intel-only legacy hint for parts
    // predating (or lacking the EPP field of) HWP.
    let has_hwp = intel && (leaf6.eax & (1 << 7)) != 0;
    let has_hwp_epp = (leaf6.eax & (1 << 10)) != 0;
    let has_epb = intel && (leaf6.ecx & (1 << 3)) != 0;

    let pstate = if has_hwp {
        Some((PStateMech::IntelHwp, unsafe { enable_hwp(has_hwp_epp) }))
    } else if amd && amd_has_cppc() {
        Some((PStateMech::AmdCppc, unsafe { enable_amd_cppc() }))
    } else {
        None
    };
    if has_epb {
        unsafe { set_energy_perf_bias() };
    }

    // --- Idle C-state via MONITOR/MWAIT (cross-vendor) ---
    let has_monitor = (leaf1.ecx & (1 << 3)) != 0;
    let leaf5 = __cpuid(5);
    let mwait_pm_ext = (leaf5.ecx & (1 << 0)) != 0; // MWAIT power-mgmt hints usable
    let mwait_on = has_monitor && mwait_pm_ext;
    if mwait_on {
        // C1E lives as sub-state ≥1 of C1: only use it when CPUID.05H reports
        // more than one C1 sub-state (EDX[7:4]); otherwise stick to plain C1.
        let c1_substates = (leaf5.edx >> 4) & 0xf;
        let hint = if c1_substates >= 2 {
            MWAIT_HINT_C1E
        } else {
            MWAIT_HINT_C1
        };
        IDLE_MWAIT_HINT.store(hint, Ordering::Relaxed);
        IDLE_USE_MWAIT.store(true, Ordering::Relaxed);
    }

    log_summary_once(|| {
        match pstate {
            Some((mech, (lo, hi))) => info!(
                "power: {} enabled — hardware-autonomous P-state, perf range {}..={}, EPP=balanced{}",
                mech.label(),
                lo,
                hi,
                if has_epb { " (+EPB)" } else { "" },
            ),
            None => info!(
                "power: no OS P-state control ({}); left to firmware{}",
                if intel {
                    "Intel without HWP"
                } else if amd {
                    "AMD without CPPC"
                } else {
                    "unknown CPU vendor"
                },
                if has_epb { ", EPB=balanced" } else { "" },
            ),
        }
        if mwait_on {
            let hint = IDLE_MWAIT_HINT.load(Ordering::Relaxed);
            info!(
                "power: idle via MONITOR/MWAIT ({})",
                if hint == MWAIT_HINT_C1E { "C1E" } else { "C1" },
            );
        } else {
            info!("power: idle via HLT (C1)");
        }
    });
}

/// Idle the calling CPU until the next interrupt. Replaces a bare `sti; hlt`:
/// parks in C1E via MONITOR/MWAIT when available (cooler), else halts. Restores
/// the caller's interrupt-enable state, matching `enable_and_hlt` semantics.
pub(super) fn cpu_idle() {
    use x86_64::instructions::interrupts;

    let was_enabled = interrupts::are_enabled();

    if IDLE_USE_MWAIT.load(Ordering::Relaxed) {
        let hint = IDLE_MWAIT_HINT.load(Ordering::Relaxed);
        // Arm the monitor with interrupts masked so nothing slips between the
        // MONITOR and the MWAIT, then `sti; mwait`: the one-instruction shadow
        // after STI guarantees MWAIT is entered before any interrupt is taken,
        // and a pending/new interrupt then breaks MWAIT — so no wake is ever
        // lost (the same guarantee `enable_and_hlt`'s `sti; hlt` relies on).
        interrupts::disable();
        let monitor_addr = &IDLE_MONITOR as *const _ as usize;
        unsafe {
            core::arch::asm!(
                "monitor",
                in("rax") monitor_addr,
                in("rcx") 0,
                in("rdx") 0,
                options(nostack, preserves_flags),
            );
            core::arch::asm!(
                "sti; mwait",
                in("rax") hint,
                in("rcx") 0,
                options(nostack),
            );
        }
    } else {
        interrupts::enable_and_hlt();
    }

    if !was_enabled {
        interrupts::disable();
    }
}

/// Run `f` only on the first CPU to reach it; the APs run identical hardware, so
/// the summary need only be logged once.
fn log_summary_once(f: impl FnOnce()) {
    if SUMMARY_LOGGED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        f();
    }
}
