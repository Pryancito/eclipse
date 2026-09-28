//! What TSC calibration decides, apart from the ports it reads.
//!
//! The kernel's whole notion of a second comes from one number: how fast the
//! TSC counts. Every timeout, every `nanosleep`, the vDSO's `tsc * mult`, the
//! vblank pacing and the `INIT`->`SIPI` gaps that start the other cores are
//! derived from it. It is obtained in `bare/arch/x86_64/cpu.rs` three
//! different ways -- CPUID leaf 15H, the PIT channel-2 gate, and the ACPI PM
//! timer -- and every one of them is a measurement against a piece of hardware
//! no host test can touch, in a file no host build compiles.
//!
//! What is *not* hardware is the arithmetic and the waiting, and those are
//! where the failures have been:
//!
//! * a wait for a counter to advance **with no bound on it**. The PIT
//!   calibrator learned this the hard way twice (its own comment records both
//!   rounds) and ends up counting stale reads; the PM-timer calibrator bounds
//!   its *first* read and then spins on the measurement loop for ever, so a
//!   counter that stops -- an SMI, firmware that parks it -- hangs the boot
//!   before anything can be printed.
//! * the answer being believed. "Is this a frequency a CPU could have" was
//!   written out three separate times, once per calibrator, as a literal
//!   range; three copies of one rule is how two of them end up different.
//! * a counter that wraps. The PM timer is 24 bits on most machines and 32 on
//!   the rest, and which one it is comes out of a FADT flag.

use core::time::Duration;

/// PIT (8254) channel reference frequency. Fixed by the spec: every x86 PC
/// and every emulator clocks it at 1.193182 MHz.
pub const PIT_REF_HZ: u64 = 1_193_182;

/// ACPI power-management timer frequency. Fixed by the ACPI spec.
pub const PM_TIMER_HZ: u64 = 3_579_545;

/// The slowest and fastest a TSC this kernel will believe can tick.
///
/// One rule, in one place. Each calibrator carried its own copy of this range
/// and they all have to agree, because the whole point of having three is that
/// a value one of them rejects falls through to the next: a range that is
/// wider in one calibrator than in another silently decides which measurement
/// wins.
pub const MIN_TSC_HZ: u64 = 100_000_000;
/// See [`MIN_TSC_HZ`].
pub const MAX_TSC_HZ: u64 = 20_000_000_000;

/// Whether `hz` is a frequency a TSC could actually be running at.
pub fn plausible_tsc_hz(hz: u64) -> bool {
    (MIN_TSC_HZ..=MAX_TSC_HZ).contains(&hz)
}

/// `cycles` TSC ticks measured over `ticks` ticks of a `ref_hz` reference.
///
/// In `u128`, because the product is not small: fifty milliseconds of a 5 GHz
/// TSC against the PM timer is already 8.9e14, and a longer window or a faster
/// part reaches the top of a `u64`. A saturating `u64` multiply does not
/// announce itself -- it just answers with a frequency that is not the one it
/// measured.
pub fn hz_from_window(cycles: u64, ref_hz: u64, ticks: u64) -> Option<u64> {
    if ticks == 0 {
        // Not a division by zero waiting to happen on a boot path: a window of
        // no ticks measured nothing, and there is no answer to give.
        return None;
    }
    let hz = (cycles as u128 * ref_hz as u128) / ticks as u128;
    if hz > MAX_TSC_HZ as u128 {
        return None;
    }
    let hz = hz as u64;
    plausible_tsc_hz(hz).then_some(hz)
}

/// CPUID leaf 15H: the TSC runs at the core crystal times a ratio.
///
/// `eax` is the denominator of the ratio, `ebx` the numerator and `ecx` the
/// crystal's frequency in Hz. All three have to be present: `ecx` is zero on
/// plenty of parts that still report the ratio, and the SDM's answer there is
/// a per-model crystal this kernel does not carry a table for, so the honest
/// result is "no answer" and the next calibrator measures instead.
pub fn tsc_hz_from_15h(eax: u32, ebx: u32, ecx: u32) -> Option<u64> {
    if eax == 0 || ebx == 0 || ecx == 0 {
        return None;
    }
    let hz = (ecx as u128 * ebx as u128) / eax as u128;
    if hz > MAX_TSC_HZ as u128 {
        return None;
    }
    let hz = hz as u64;
    plausible_tsc_hz(hz).then_some(hz)
}

/// CPUID leaf 16H: the processor's nominal base frequency, in MHz.
///
/// A declaration, not a measurement, and on modern Intel it is not what the
/// TSC counts at -- which is why this is the last thing tried and why the
/// answer is checked against the PM timer afterwards.
pub fn tsc_hz_from_base_mhz(mhz: u32) -> Option<u64> {
    let hz = (mhz as u64).checked_mul(1_000_000)?;
    plausible_tsc_hz(hz).then_some(hz)
}

/// How far a counter that wraps at `mask + 1` has advanced from `first`.
///
/// The PM timer is 24 bits wide on most machines and 32 on the rest, and its
/// top byte is *reserved* rather than guaranteed zero, so the difference has
/// to be taken modulo the counter's own width. The **trailing** mask is what
/// does that: masking the two reads first changes nothing, because two's
/// complement subtraction agrees modulo any power of two. It is written out
/// anyway so the rule reads as "both of these are `mask`-wide numbers" rather
/// than as a trick, and a test below pins the part that is not free.
pub fn advanced(first: u32, now: u32, mask: u32) -> u32 {
    (now & mask).wrapping_sub(first & mask) & mask
}

/// Why a wait for a hardware counter ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnd {
    /// The counter advanced by at least the number of ticks asked for.
    Reached(u32),
    /// The budget ran out first: the counter is stopped, or slower than the
    /// caller can wait for. Never a hang -- a boot path that spins for ever
    /// on a dead counter prints nothing and says nothing.
    Stalled(u32),
}

/// Wait until a counter has advanced `target` ticks, reading it at most
/// `budget` times.
///
/// `read` is the port; everything else is the decision. The budget is a count
/// of reads and deliberately not a time, because on this path the time is the
/// very thing being measured.
pub fn wait_for_ticks(
    target: u32,
    mask: u32,
    budget: u32,
    mut read: impl FnMut() -> u32,
) -> WaitEnd {
    let first = read();
    let mut elapsed = 0;
    for _ in 0..budget {
        elapsed = advanced(first, read(), mask);
        if elapsed >= target {
            return WaitEnd::Reached(elapsed);
        }
    }
    WaitEnd::Stalled(elapsed)
}

/// What a fresh measurement says about the frequency already in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Within tolerance; keep what is installed rather than jolting a clock
    /// other code has already read.
    Confirmed,
    /// Out of tolerance; the installed value is wrong and has to go.
    Corrected,
}

/// Half a percent, either way.
///
/// A machine that fell through to the 2 GHz guess with a 3.7 GHz TSC ran its
/// clock 1.84 times fast: every timeout short, every benchmark and every audio
/// stream measured against something that was not counting seconds. The
/// tolerance has to be tight enough to catch that and loose enough that a
/// noisy 50 ms window does not move a clock that is already right.
pub fn verdict(provisional: u64, measured: u64) -> Verdict {
    // In `u128`: `diff * 200` on a 20 GHz ceiling is 4e12, which fits, but the
    // shape is one multiplication away from not fitting and this is not a
    // place to be clever.
    let diff = provisional.abs_diff(measured) as u128;
    if diff * 200 <= provisional as u128 {
        Verdict::Confirmed
    } else {
        Verdict::Corrected
    }
}

/// Fixed-point multiplier for a TSC of `hz` Hz: `ns = (cycles * mult) >> 32`,
/// i.e. `(1_000_000_000 << 32) / hz`.
///
/// Built from Hz and not from truncated MHz, so a 3.312 GHz TSC is not paced
/// as if it were 3.000 GHz, and returned as one number both the kernel clock
/// and the vDSO scale by — two spellings of this division is two clocks.
pub fn ns_mult(hz: u64) -> u64 {
    ((1_000_000_000u128 << 32) / hz.max(1) as u128) as u64
}

/// Monotonic nanoseconds since `base` for a raw counter reading of `cycle`.
///
/// `base` is the reading the first clock read of the boot latched, and
/// subtracting it is the whole of this function's reason to exist. The TSC is
/// **not** zero when the kernel takes over: firmware runs before it, and on a
/// warm reboot the counter is never reset at all, so it carries however long
/// the machine has been powered. Scaling the raw reading therefore reported an
/// uptime of whatever the firmware and the previous boot had spent -- 711231 s,
/// 8.2 days, on Moebius's machine, from the very first line of `dmesg` -- with
/// time advancing correctly on top of it. `/proc/uptime`, `uptime(1)`, the
/// `dmesg` timestamps and `/proc/stat`'s `btime` all read that offset.
///
/// Saturating, so a reading below the base (a counter that a sibling CPU is
/// slightly behind on) is zero rather than an enormous wrapped span. In
/// `u128`, because the product of a multi-day cycle count and the multiplier
/// overflows a `u64` -- the wrap this shape was chosen to avoid.
pub fn mono_ns(cycle: u64, base: u64, mult: u64) -> u64 {
    let ticks = cycle.saturating_sub(base) as u128;
    ((ticks * mult as u128) >> 32) as u64
}

/// How long `ticks` of a `ref_hz` counter takes -- the window a calibrator is
/// asking the machine to sit through, so it can be stated in the boot log and
/// bounded in a test rather than guessed at from a constant.
pub fn window_duration(ticks: u32, ref_hz: u64) -> Duration {
    Duration::from_nanos((ticks as u64).saturating_mul(1_000_000_000) / ref_hz.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASK24: u32 = 0x00ff_ffff;

    /// The multiplier is one division in one place, and both clocks that use it
    /// -- `timer_now` and the vDSO -- scale by it, so the number it produces is
    /// what a second is worth on this machine.
    #[test]
    fn the_multiplier_turns_a_seconds_worth_of_cycles_into_a_second() {
        for hz in [
            1_000_000_000u64,
            2_000_000_000,
            3_312_000_000,
            3_700_000_000,
        ] {
            let ns = mono_ns(hz, 0, ns_mult(hz));
            // Fixed point: within a microsecond over a whole second.
            assert!(
                ns.abs_diff(1_000_000_000) < 1_000,
                "{} Hz: un segundo de ciclos dio {} ns",
                hz,
                ns
            );
        }
        // A 1 GHz TSC is exactly 1 ns per cycle, which is the calibration every
        // hand-checked expectation elsewhere is written against.
        assert_eq!(ns_mult(1_000_000_000), 1u64 << 32);
        // And zero is not a frequency to divide by.
        assert_eq!(ns_mult(0), ns_mult(1));
    }

    /// The bug this base exists for, in the arithmetic that had it.
    ///
    /// Moebius's machine reported 711231 s of uptime from the first line of
    /// `dmesg` -- 8.2 days, the time since it was last powered on, because the
    /// TSC is not reset when a kernel takes over and a warm reboot does not
    /// reset it at all. Scaling the absolute reading is what produced that;
    /// subtracting the first reading of the boot is what fixes it.
    #[test]
    fn the_base_is_what_makes_the_answer_an_uptime() {
        const HZ: u64 = 3_700_000_000;
        let mult = ns_mult(HZ);
        // The machine from the report: 8.2 days of counter before Eclipse ran.
        let base = 711_231 * HZ;

        // What the absolute reading said, and what the bug looked like. Within
        // a second: the multiplier truncates, so a span of days loses a tick's
        // worth of rounding per second of it -- about 9 ms over these 8.2 days.
        assert!(
            (mono_ns(base, 0, mult) / 1_000_000_000).abs_diff(711_231) <= 1,
            "el bug daba {} s",
            mono_ns(base, 0, mult) / 1_000_000_000
        );

        // Time zero, and then one second of it.
        assert_eq!(mono_ns(base, base, mult), 0);
        assert!(mono_ns(base + HZ, base, mult).abs_diff(1_000_000_000) < 1_000);
    }

    /// A reading below the base saturates. The base is latched by the first
    /// clock read of the boot, so a lower reading means a sibling CPU a few
    /// cycles behind; wrapping there would answer ~2^64 ticks -- five centuries
    /// -- instead of a clock a hair early, and every deadline taken from it
    /// would be measured against that.
    #[test]
    fn a_reading_below_the_base_saturates_instead_of_wrapping() {
        let mult = ns_mult(1_000_000_000);
        assert_eq!(mono_ns(0, 1, mult), 0);
        assert_eq!(mono_ns(1_000, 1_000_000, mult), 0);
        assert_eq!(mono_ns(0, u64::MAX, mult), 0);
    }

    /// The `u128` product, which is the other half of why this function exists:
    /// a 3.7 GHz counter multiplied by its own multiplier passes 2^64 within
    /// seconds, and a `u64` multiply there wraps the clock silently.
    #[test]
    fn the_product_is_taken_wide_enough_not_to_wrap() {
        const HZ: u64 = 3_700_000_000;
        let mult = ns_mult(HZ);
        // A hundred days of counter: far past where a 64-bit product wraps.
        let cycles = 100 * 86_400 * HZ;
        assert!(cycles as u128 * mult as u128 > u64::MAX as u128);
        assert_ne!(
            cycles.wrapping_mul(mult) >> 32,
            mono_ns(cycles, 0, mult),
            "la aritmetica de 64 bits daria lo mismo: caso mal elegido"
        );
        // Within a second of a hundred days, which is the fixed-point
        // multiplier's own truncation and not this function's business.
        assert!(
            (mono_ns(cycles, 0, mult) / 1_000_000_000).abs_diff(100 * 86_400) <= 1,
            "cien dias de contador dieron {} s",
            mono_ns(cycles, 0, mult) / 1_000_000_000
        );
    }

    #[test]
    fn every_calibrator_believes_the_same_frequencies() {
        // One rule. A 3.7 GHz part is a frequency; 50 MHz and 40 GHz are not.
        assert!(plausible_tsc_hz(3_700_000_000));
        assert!(plausible_tsc_hz(MIN_TSC_HZ));
        assert!(plausible_tsc_hz(MAX_TSC_HZ));
        assert!(!plausible_tsc_hz(MIN_TSC_HZ - 1));
        assert!(!plausible_tsc_hz(MAX_TSC_HZ + 1));
        assert!(!plausible_tsc_hz(0));
        // And the numbers themselves, not just the ends of whatever the
        // constants happen to say: 50 MHz is a mismeasurement and 40 GHz is
        // not a part that exists, and a window widened to admit either of
        // them stops the next calibrator from ever being tried.
        assert!(!plausible_tsc_hz(50_000_000));
        assert!(!plausible_tsc_hz(25_000_000_000));
        assert!(!plausible_tsc_hz(40_000_000_000));
    }

    #[test]
    fn a_window_is_cycles_over_reference_ticks() {
        // 50 ms of a 3.7 GHz TSC against the PM timer.
        let ticks = (PM_TIMER_HZ / 20) as u64;
        let cycles = 3_700_000_000u64 / 20;
        let hz = hz_from_window(cycles, PM_TIMER_HZ, ticks).unwrap();
        assert!(hz.abs_diff(3_700_000_000) < 1_000_000, "{}", hz);
    }

    #[test]
    fn the_product_does_not_have_to_fit_in_a_word() {
        // A saturating `u64` multiply answers with a frequency it did not
        // measure. 2e12 cycles against the PM timer overflows a u64 product
        // by three orders of magnitude, and the right answer is still an
        // ordinary frequency.
        let cycles = 10_000_000_000_000u64;
        let ticks = 35_795_450_000u64;
        let hz = hz_from_window(cycles, PM_TIMER_HZ, ticks);
        assert_eq!(hz, Some(1_000_000_000));
        assert!(
            cycles.checked_mul(PM_TIMER_HZ).is_none(),
            "the point of the test is that this product does not fit"
        );
    }

    #[test]
    fn a_window_of_no_ticks_has_no_answer() {
        // And in particular is not a division by zero on the boot path.
        assert_eq!(hz_from_window(1_000_000, PM_TIMER_HZ, 0), None);
    }

    #[test]
    fn a_window_that_measures_nonsense_is_refused() {
        assert_eq!(hz_from_window(1, PM_TIMER_HZ, 1_000_000), None);
        assert_eq!(hz_from_window(u64::MAX, PM_TIMER_HZ, 1), None);
    }

    #[test]
    fn the_crystal_ratio_is_crystal_times_numerator_over_denominator() {
        // Tiger Lake: 38.4 MHz crystal, ratio 2/172 inverted -- eax is the
        // denominator and ebx the numerator, and swapping them gives an
        // answer three orders of magnitude out.
        assert_eq!(tsc_hz_from_15h(2, 172, 38_400_000), Some(3_302_400_000));
        assert_eq!(tsc_hz_from_15h(172, 2, 38_400_000), None);
    }

    #[test]
    fn a_leaf_that_does_not_name_the_crystal_has_no_answer() {
        // `ecx` is zero on plenty of parts that do report the ratio. Treating
        // it as a frequency gives zero; guessing one gives a clock that is
        // wrong by whatever the guess was off by.
        assert_eq!(tsc_hz_from_15h(2, 172, 0), None);
        assert_eq!(tsc_hz_from_15h(0, 172, 38_400_000), None);
        assert_eq!(tsc_hz_from_15h(2, 0, 38_400_000), None);
    }

    #[test]
    fn a_base_frequency_is_megahertz() {
        assert_eq!(tsc_hz_from_base_mhz(3700), Some(3_700_000_000));
        assert_eq!(tsc_hz_from_base_mhz(0), None);
        assert_eq!(tsc_hz_from_base_mhz(50), None);
        assert_eq!(tsc_hz_from_base_mhz(u32::MAX), None);
    }

    #[test]
    fn a_counter_that_wrapped_still_advanced() {
        assert_eq!(advanced(MASK24 - 5, 4, MASK24), 10);
        assert_eq!(advanced(0, 10, MASK24), 10);
        assert_eq!(advanced(10, 10, MASK24), 0);
        assert_eq!(advanced(u32::MAX - 5, 4, u32::MAX), 10);
    }

    #[test]
    fn the_reserved_top_byte_of_a_24_bit_counter_is_not_part_of_the_count() {
        // The PM timer's upper eight bits are reserved, not guaranteed zero,
        // and the elapsed count is the denominator of the frequency: eight
        // bits of firmware's choosing in it is a clock that is wrong by
        // whatever they happened to be.
        let dirty = 0xAB00_0010u32;
        assert_eq!(advanced(0, dirty, MASK24), 0x10);
        assert_eq!(advanced(dirty, 0x20, MASK24), 0x10);
        // The rule, rather than the example: nothing above the counter's own
        // width ever comes out, whatever went in.
        for (a, b) in [(0u32, u32::MAX), (u32::MAX, 0), (0xDEAD_BEEF, 0x1234_5678)] {
            assert!(advanced(a, b, MASK24) <= MASK24);
        }
    }

    #[test]
    fn a_counter_that_stops_does_not_hang_the_boot() {
        // The failure this exists for: the measurement loop had no bound at
        // all, so a PM timer parked by an SMI or by firmware spun the boot CPU
        // for ever, before there was anything on screen to say so.
        let end = wait_for_ticks(1000, MASK24, 50, || 7);
        assert_eq!(end, WaitEnd::Stalled(0));
    }

    #[test]
    fn a_counter_that_moves_is_waited_out() {
        let mut t = 0u32;
        let end = wait_for_ticks(100, MASK24, 1000, || {
            t += 3;
            t
        });
        match end {
            WaitEnd::Reached(n) => assert!((100..103).contains(&n), "{}", n),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn the_budget_is_reads_and_not_ticks() {
        // A counter moving one tick per read needs `target` reads, and one
        // fewer than that is a stall -- which is what makes the budget
        // something a caller can reason about without knowing the frequency
        // it is trying to measure.
        let mut t = 0u32;
        assert_eq!(
            wait_for_ticks(10, MASK24, 9, || {
                t += 1;
                t
            }),
            WaitEnd::Stalled(9)
        );
        let mut t = 0u32;
        assert_eq!(
            wait_for_ticks(10, MASK24, 10, || {
                t += 1;
                t
            }),
            WaitEnd::Reached(10)
        );
    }

    #[test]
    fn a_wait_that_wraps_mid_window_still_reaches_its_target() {
        let mut t = MASK24 - 4;
        let end = wait_for_ticks(10, MASK24, 100, || {
            t = (t + 1) & MASK24;
            t
        });
        assert_eq!(end, WaitEnd::Reached(10));
    }

    #[test]
    fn half_a_percent_either_way_is_the_same_clock() {
        let hz = 3_700_000_000u64;
        assert_eq!(verdict(hz, hz), Verdict::Confirmed);
        assert_eq!(verdict(hz, hz + hz / 200), Verdict::Confirmed);
        assert_eq!(verdict(hz, hz - hz / 200), Verdict::Confirmed);
        assert_eq!(verdict(hz, hz + hz / 100), Verdict::Corrected);
    }

    #[test]
    fn the_guess_that_ran_a_clock_one_point_eight_times_fast_is_corrected() {
        // The i9-10900X: a 2 GHz guess installed against a 3.7 GHz TSC.
        assert_eq!(verdict(2_000_000_000, 3_700_000_000), Verdict::Corrected);
    }

    #[test]
    fn a_window_says_how_long_it_will_sit_there() {
        // 1/20th of the PM timer's ticks is a twentieth of a second, to the
        // nanosecond the integer division leaves behind.
        assert_eq!(
            window_duration((PM_TIMER_HZ / 20) as u32, PM_TIMER_HZ),
            Duration::from_nanos(49_999_930)
        );
        // And the PIT's full 16-bit count is the 54.9 ms its own comment
        // names -- the number the old TSC-cycle cap was measured against and
        // got wrong above 3.64 GHz.
        assert_eq!(
            window_duration(0xFFFF, PIT_REF_HZ),
            Duration::from_nanos(54_924_563)
        );
        // A reference of zero is not a division by zero either.
        assert_eq!(window_duration(1, 0), Duration::from_nanos(1_000_000_000));
    }
}
