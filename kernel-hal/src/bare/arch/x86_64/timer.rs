use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;
use spin::Once;
use x86_64::instructions::port::Port;

use crate::common::rtc::{self, RtcRegs};

/// Global monotonic floor in nanoseconds. Unsynchronized per-CPU TSCs can read
/// backwards across cores; smoltcp's TCP timers (and every sleep/timeout in the
/// kernel) require non-decreasing time, so clamp each reading to the highest
/// value observed on any CPU.
///
/// Only consulted on the slow path: with an invariant TSC (the norm on every
/// post-2008 x86 and on QEMU/KVM) the package synchronizes the counters at
/// reset, so the raw reading is already cross-CPU monotonic and the floor RMW
/// — a single cacheline every CPU would otherwise bounce on EVERY clock read
/// (syscall entry/exit, every tick, hunter's time source) — is skipped. A
/// per-tick watchdog (`mono_floor_tick`) keeps maintaining the floor at 250 Hz
/// and demotes to the clamped slow path if real skew is ever observed.
static MONO_NS: AtomicU64 = AtomicU64::new(0);

/// `true` once boot detected an invariant TSC (CPUID.80000007H:EDX[8]) — the
/// fast, RMW-free `timer_now` path. Cleared forever by `mono_floor_tick` if a
/// CPU observes the floor ahead of its own reading beyond the tolerance.
static TSC_INVARIANT: AtomicBool = AtomicBool::new(false);

/// Fixed-point multiplier: ns = (tsc * TSC_NS_MULT) >> 32, i.e.
/// `(1_000_000_000 << 32) / tsc_hz`. Replaces the per-read 64-bit division and,
/// via the 128-bit intermediate, fixes the `cycles * 1000` overflow that wrapped
/// the clock after ~71 days of uptime at 3 GHz. Built from Hz, not truncated
/// MHz, so a 3.312 GHz TSC is not paced as if it were 3.000 GHz. 0 = not yet
/// initialized.
static TSC_NS_MULT: AtomicU64 = AtomicU64::new(0);

/// Skew tolerance for the invariant-TSC watchdog. Same-package TSCs agree to
/// within nanoseconds; the floor a CPU compares against can be up to one tick
/// (4 ms) stale, so anything beyond a generous 1 ms means genuinely unsynced
/// counters (multi-socket, buggy firmware) and demotes to the clamped path.
const TSC_SKEW_TOLERANCE_NS: u64 = 1_000_000;

#[cold]
fn tsc_ns_mult_init() -> u64 {
    // `tsc_hz()` is Once-cached and never zero (falls back to 2 GHz).
    let hz = super::cpu::tsc_hz().max(1);
    let mult = ((1_000_000_000u128 << 32) / hz as u128) as u64;
    TSC_NS_MULT.store(mult, Ordering::Relaxed);
    // Invariant TSC: CPUID leaf 0x8000_0007, EDX bit 8. On such parts the TSC
    // runs at a constant rate and, on a single package, is reset-synchronized
    // across cores, so raw readings are already monotonic system-wide.
    let invariant = raw_cpuid::CpuId::new()
        .get_extended_function_info()
        .map(|info| info.has_invariant_tsc())
        .unwrap_or(false);
    TSC_INVARIANT.store(invariant, Ordering::Relaxed);
    mult
}

/// The TSC frequency was corrected (see `cpu::recalibrate_tsc_hz`): rebuild
/// the multiplier and let the monotonic floor follow the clock down.
///
/// `timer_now` stays `tsc * mult >> 32` with no base, so a smaller multiplier
/// makes the reading drop once, right here. This runs on the BSP at boot,
/// before the APs and the tick exist and before anything outside the kernel
/// can read time; deadlines armed before it simply fire a little later. The
/// floor is lowered to the new reading so the clamped path does not freeze
/// until the old reading is overtaken, and the invariant-path watchdog does
/// not mistake the drop for cross-CPU skew.
pub(super) fn tsc_hz_changed(hz: u64) {
    let mult = ((1_000_000_000u128 << 32) / hz.max(1) as u128) as u64;
    TSC_NS_MULT.store(mult, Ordering::Relaxed);
    let cycle = unsafe { core::arch::x86_64::_rdtsc() };
    MONO_NS.store(
        ((cycle as u128 * mult as u128) >> 32) as u64,
        Ordering::Relaxed,
    );
}

#[inline]
fn tsc_to_ns(cycle: u64) -> u64 {
    let mut mult = TSC_NS_MULT.load(Ordering::Relaxed);
    if mult == 0 {
        mult = tsc_ns_mult_init();
    }
    ((cycle as u128 * mult as u128) >> 32) as u64
}

pub fn timer_now() -> Duration {
    let cycle = unsafe { core::arch::x86_64::_rdtsc() };
    let ns = tsc_to_ns(cycle);
    if TSC_INVARIANT.load(Ordering::Relaxed) {
        // Fast path: no shared-cacheline RMW. The floor is still advanced at
        // tick rate by `mono_floor_tick`, so a later demotion to the slow path
        // stays (almost) seamless.
        return Duration::from_nanos(ns);
    }
    // `fetch_max` returns the previous value; the effective clock is the larger
    // of the previous floor and this reading, guaranteeing it never goes back.
    let prev = MONO_NS.fetch_max(ns, Ordering::Relaxed);
    Duration::from_nanos(prev.max(ns))
}

/// Per-tick watchdog for the invariant-TSC fast path, called from every CPU's
/// periodic tick (250 Hz × N CPUs — negligible RMW traffic). Keeps the floor
/// fresh and demotes to the clamped slow path if this CPU's TSC reading is
/// ever behind the floor by more than the tolerance, which would mean the
/// "invariant" TSCs are not actually synchronized on this machine.
pub fn mono_floor_tick(now_ns: u64) {
    if !TSC_INVARIANT.load(Ordering::Relaxed) {
        return; // slow path already maintains the floor on every read
    }
    let prev = MONO_NS.fetch_max(now_ns, Ordering::Relaxed);
    if prev > now_ns + TSC_SKEW_TOLERANCE_NS {
        TSC_INVARIANT.store(false, Ordering::Relaxed);
        warn!(
            "TSC skew detected ({} ns behind the cross-CPU floor); \
             falling back to the clamped monotonic clock",
            prev - now_ns
        );
        // The clamped path exists inside this kernel only. Userspace reading
        // the TSC through the vDSO has no way to participate in the floor, so
        // once the counters are known to disagree across CPUs the vDSO must
        // stop answering and send everyone back to the syscall — otherwise a
        // thread that migrates reads time going backwards.
        crate::timer::notify_clock_changed();
    }
}

/// The TSC→ns multiplier, but only when the TSC is fit to be read directly by
/// userspace. `None` means the vDSO must stay disabled.
///
/// Two conditions, and both are load-bearing. The multiplier must exist, which
/// it does not until the first `timer_now` calibrates it. And the TSC must be
/// invariant: constant-rate, so one multiplier is valid for all time, and
/// reset-synchronized across cores, so a thread that migrates mid-read cannot
/// see the clock go backwards. When it is, `timer_now` returns exactly
/// `tsc_to_ns(rdtsc())` with no floor applied — the same arithmetic on the same
/// inputs the vDSO performs, so the two clocks cannot disagree.
pub fn vdso_tsc_mult() -> Option<u64> {
    if !TSC_INVARIANT.load(Ordering::Relaxed) && !FORCE_TSC_INVARIANT.load(Ordering::Relaxed) {
        return None;
    }
    match TSC_NS_MULT.load(Ordering::Relaxed) {
        0 => None,
        mult => Some(mult),
    }
}

/// Set by `VDSOFORCE=1` on the kernel command line: treat the TSC as usable by
/// userspace even though CPUID does not say it is invariant.
///
/// This exists because QEMU cannot advertise an invariant TSC under TCG — the
/// feature word is not in its TCG-supported set, so `+invtsc` is dropped — and
/// TCG is the only substrate on which Eclipse and Linux can be compared on
/// equal terms. Without it the vDSO would be unmeasurable. See `zCore/main.rs`
/// for why it is sound there and unsound on real hardware.
///
/// Deliberately does NOT touch `TSC_INVARIANT`: the kernel's own monotonic
/// floor keeps working exactly as it did, so forcing this affects what
/// userspace is allowed to do and nothing else.
static FORCE_TSC_INVARIANT: AtomicBool = AtomicBool::new(false);

/// Enable the `VDSOFORCE=1` override. See [`FORCE_TSC_INVARIANT`].
pub fn set_force_tsc_invariant(force: bool) {
    FORCE_TSC_INVARIANT.store(force, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// LAPIC timer rate and tick programming
// ---------------------------------------------------------------------------
// Boot programs `TimerDivide::Div1`, so the initial-count register is a count
// of LAPIC timer clocks. What that clock is depends on the machine: on the
// hardware this kernel was first tuned on the timer counts core cycles, so
// the TSC rate was used as the count rate directly. Under KVM the emulated
// timer counts at a fixed 1 GHz whatever the TSC does, and QEMU's TCG is the
// same, so on a 4 GHz host the "4 ms" tick took 16 ms — every poll/select
// re-scan of a bus-less fd (PulseAudio's sink waiting on its PCM among them)
// was served four times late. The rate is therefore measured at boot
// (`calibrate_lapic_timer`) and everything below counts in it. We modulate
// the count to stretch the periodic tick when a CPU goes idle, then restore
// it on resume.

use super::super::timer::TICKS_PER_SEC;
use zcore_drivers::irq::x86::Apic;

/// LAPIC timer count rate in Hz as measured by [`calibrate_lapic_timer`];
/// 0 until then (or if the measurement was implausible), which
/// [`lapic_hz`] reads as "assume the TSC rate", the historical behaviour.
static LAPIC_HZ: AtomicU64 = AtomicU64::new(0);

/// The rate the LAPIC timer counts at.
pub fn lapic_hz() -> u64 {
    match LAPIC_HZ.load(Ordering::Relaxed) {
        0 => super::cpu::tsc_hz(),
        hz => hz,
    }
}

/// Measure the LAPIC timer's count rate against the (already calibrated)
/// TSC: load the maximum count with the interrupt masked — the LVT mask
/// stops the interrupt, not the countdown — spin a TSC-timed 20 ms, and read
/// back how far the count got. The same idea as Linux's
/// `calibrate_APIC_clock`, with the TSC as the reference.
///
/// BSP only, at boot, interrupts off, before the periodic tick is programmed
/// (`program_periodic_tick` then uses the result). Leaves the timer stopped
/// and masked.
pub fn calibrate_lapic_timer() {
    use x2apic::lapic::{TimerDivide, TimerMode};
    if !Apic::local_apic_ready() {
        return;
    }
    let tsc_hz = super::cpu::tsc_hz().max(1);
    let lapic = Apic::local_apic();
    lapic.disable_timer();
    lapic.set_timer_mode(TimerMode::OneShot);
    lapic.set_timer_divide(TimerDivide::Div1);
    lapic.set_timer_initial(u32::MAX);
    let t0 = unsafe { core::arch::x86_64::_rdtsc() };
    let c0 = lapic.timer_current();
    let window = tsc_hz / 50; // 20 ms
    while unsafe { core::arch::x86_64::_rdtsc() }.wrapping_sub(t0) < window {
        core::hint::spin_loop();
    }
    let c1 = lapic.timer_current();
    let t1 = unsafe { core::arch::x86_64::_rdtsc() };
    // A zero initial count stops the one-shot countdown.
    lapic.set_timer_initial(0);
    let counts = u64::from(c0.wrapping_sub(c1));
    let cycles = t1.wrapping_sub(t0).max(1);
    let hz = (counts as u128 * tsc_hz as u128 / cycles as u128) as u64;
    // Plausible: 1 MHz (a slow crystal) to 50 GHz. A count that never moved
    // (0) or a wrapped read is left at the TSC-rate assumption.
    let plausible = counts > 1000 && (1_000_000..=50_000_000_000).contains(&hz);
    if plausible {
        LAPIC_HZ.store(hz, Ordering::Relaxed);
    }
    // `warn!` (not `klog_warn!`): this belongs next to the `[tsc]` line in
    // the serial boot log, where a `make qemu` transcript shows it.
    warn!(
        "[lapic] timer counts at {} Hz (TSC {} Hz): {} Hz tick = {} counts{}",
        hz,
        tsc_hz,
        TICKS_PER_SEC,
        fast_tick_count(),
        if plausible {
            ""
        } else {
            " -- implausible, keeping the TSC rate"
        }
    );
}

/// LAPIC timer initial count for the normal full-rate scheduler tick (4 ms at
/// 250 Hz), in the measured count rate.
pub fn fast_tick_count() -> u32 {
    crate::deadline::counts_per_tick(lapic_hz(), TICKS_PER_SEC)
}

/// Period of the full-rate scheduler tick, in nanoseconds (4 ms at 250 Hz).
/// The upper bound on how far ahead the deadline timer is ever programmed:
/// preemption and the per-tick housekeeping must keep running regardless of
/// what the timer heap wants.
pub const fn fast_tick_ns() -> u64 {
    1_000_000_000 / TICKS_PER_SEC
}

/// Convert a now-relative nanosecond span to LAPIC timer counts, in the
/// measured count rate. Clamped to a non-zero `u32`: a count of 0 stops the
/// timer, and counts above `u32::MAX` are not representable.
pub fn ns_to_tick_count(ns: u64) -> u32 {
    crate::deadline::counts_for(lapic_hz(), ns)
}

/// Reprogram this CPU's LAPIC timer initial count (the period, in periodic
/// mode). Safe from any CPU: the LAPIC registers are per-CPU hardware reached
/// through the local MMIO window / MSRs.
pub fn set_tick_count(count: u32) {
    if Apic::local_apic_ready() {
        Apic::local_apic().set_timer_initial(count);
    }
}

/// Program *this* CPU's LAPIC timer for the periodic scheduler tick.
///
/// The BSP does this inline during primary init (`drivers.rs`). Each AP must
/// repeat it: the LAPIC timer's mode / divide / initial-count registers are
/// per-CPU hardware and are NOT inherited from the BSP — only the shared cached
/// config (vector) is. An AP that skips this is left with an initial count of 0,
/// i.e. a *stopped* timer, so it never takes the 250 Hz tick: no preemption, no
/// idle accounting, and the whole system's `naive_timer` heap ends up serviced
/// by the BSP alone — which shows up as a lopsided per-CPU busy split and an
/// inflated `/proc/perf/kernel` busy%. Leaves the timer masked; the unmask
/// happens later via `apic_timer_enable()` (same ordering as the BSP).
pub fn program_periodic_tick() {
    use x2apic::lapic::{TimerDivide, TimerMode};
    if Apic::local_apic_ready() {
        let lapic = Apic::local_apic();
        lapic.set_timer_mode(TimerMode::Periodic);
        lapic.set_timer_divide(TimerDivide::Div1);
        lapic.set_timer_initial(fast_tick_count());
    }
}

static WALL_CLOCK_INIT: Once = Once::new();

pub fn init() {
    let irq = crate::drivers::all_irq().first_unwrap();
    irq.apic_timer_enable();
    // RTC I/O ports (0x70/0x71) are not per-CPU — only the first caller reads
    // them to avoid concurrent port access corrupting the read under SMP.
    WALL_CLOCK_INIT.call_once(init_wall_clock_from_rtc);
}

// ---------------------------------------------------------------------------
// CMOS / MC146818 real-time clock
// ---------------------------------------------------------------------------
// The two I/O ports, and nothing else: what the bytes behind them mean lives
// in `crate::common::rtc`, where the host suite can reach it.

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const RTC_SECONDS: u8 = 0x00;
const RTC_MINUTES: u8 = 0x02;
const RTC_HOURS: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
/// Where the century register sits when the board has one. Nothing on this side
/// can tell whether it does -- see `crate::common::rtc`, which decides how much
/// to believe the byte.
const RTC_CENTURY: u8 = 0x32;
const RTC_STATUS_A: u8 = 0x0A;
const RTC_STATUS_B: u8 = 0x0B;

/// How long one attempt waits for an update cycle to finish before reporting
/// that it could not read. An update takes about 2 ms; this is a spin, so the
/// count is generous, and exhausting it is a result rather than a hang.
const UIP_SPINS: u32 = 1_000_000;

unsafe fn cmos_read(reg: u8) -> u8 {
    // Bit 7 of the index port controls NMI; keep it clear (NMI enabled).
    let mut addr = Port::<u8>::new(CMOS_ADDR);
    let mut data = Port::<u8>::new(CMOS_DATA);
    addr.write(reg & 0x7F);
    data.read()
}

unsafe fn rtc_update_in_progress() -> bool {
    cmos_read(RTC_STATUS_A) & 0x80 != 0
}

/// One attempt at a clean register set: wait out an update cycle, then read all
/// seven registers. `None` when the clock was still updating after [`UIP_SPINS`]
/// spins, which is what a clock that is broken or absent looks like.
unsafe fn rtc_sample() -> Option<RtcRegs> {
    let mut spins = 0u32;
    while rtc_update_in_progress() {
        spins += 1;
        if spins > UIP_SPINS {
            return None;
        }
    }
    Some(RtcRegs {
        sec: cmos_read(RTC_SECONDS),
        min: cmos_read(RTC_MINUTES),
        hour: cmos_read(RTC_HOURS),
        day: cmos_read(RTC_DAY),
        month: cmos_read(RTC_MONTH),
        year: cmos_read(RTC_YEAR),
        century: cmos_read(RTC_CENTURY),
    })
}

/// Read the CMOS clock and return seconds since the Unix epoch, or `None` when
/// it never settles or the bytes cannot be a date.
fn read_rtc_epoch() -> Option<u64> {
    unsafe {
        let regs = rtc::settle(|| rtc_sample())?;
        let status_b = cmos_read(RTC_STATUS_B);
        rtc::decode(regs, status_b)
    }
}

fn init_wall_clock_from_rtc() {
    match read_rtc_epoch() {
        Some(epoch) => {
            crate::timer::wall_clock_set(Duration::from_secs(epoch));
            info!("wall clock initialized from RTC: {} s since epoch", epoch);
        }
        None => {
            warn!("RTC read failed; wall clock stays at boot epoch (1970)");
        }
    }
}
