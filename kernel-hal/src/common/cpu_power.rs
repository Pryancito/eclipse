//! What the CPU's power-management registers mean: the P-state bounds, the
//! word that asks for a new ceiling, the two thermal sensors, and the
//! governor's decision to walk the ceiling down or back up.
//!
//! All of it lived in `bare/arch/x86_64/power.rs`, welded to the MSR and
//! CPUID instructions it reads, so nothing outside a kernel build compiled a
//! line of it -- and that module says in its own first paragraph why the
//! emulator cannot stand in either: QEMU exposes neither HWP/CPPC nor real
//! C-states and does not model the silicon's voltage, frequency or
//! temperature, so **everything here only happens on physical silicon**. It
//! exists because an idle Eclipse ran at about 80 degrees on a real machine.
//!
//! The instructions stay in the architecture; the field layouts and the
//! arithmetic are here, where a test can reach them, the same way
//! `common/deadline.rs` and `common/rtc.rs` came out of their own modules.
//!
//! **The field order is not the same for the two vendors, on purpose.** Intel
//! puts the minimum first and AMD the maximum, and the request word is built
//! in three places -- once per vendor at bring-up and once more by the
//! governor every time it moves a ceiling -- so the three must agree or the
//! first governor tick quietly reprograms the CPU with the fields swapped.
//!
//! References: Intel SDM vol. 4 (IA32_HWP_CAPABILITIES, IA32_HWP_REQUEST,
//! IA32_THERM_STATUS, MSR_TEMPERATURE_TARGET), AMD PPR (MSR_AMD_CPPC_CAP1 /
//! _REQUEST), and Linux's `k10temp` for the Zen sensor.

/// The two bounds and the base point a vendor's capability register names,
/// in the abstract performance units both vendors use (0..255).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PerfCaps {
    /// The turbo/boost ceiling.
    pub highest: u8,
    /// The guaranteed (base/nominal) clock.
    pub base: u8,
    /// The lowest, coolest operating point.
    pub lowest: u8,
}

/// `IA32_HWP_CAPABILITIES`: Highest[7:0], Guaranteed[15:8], Lowest[31:24].
pub fn hwp_caps(caps: u64) -> PerfCaps {
    PerfCaps {
        highest: (caps & 0xff) as u8,
        base: ((caps >> 8) & 0xff) as u8,
        lowest: ((caps >> 24) & 0xff) as u8,
    }
}

/// `MSR_AMD_CPPC_CAP1`: Lowest[7:0], Nominal[23:16], Highest[31:24].
pub fn cppc_caps(cap1: u64) -> PerfCaps {
    PerfCaps {
        highest: ((cap1 >> 24) & 0xff) as u8,
        base: ((cap1 >> 16) & 0xff) as u8,
        lowest: (cap1 & 0xff) as u8,
    }
}

/// The ceiling to ask for, and whether turbo ended up capped.
///
/// `want_base` is the `noturbo` command line. The base point is only believed
/// when it is reported at all and is not below the floor: firmware that leaves
/// it at zero would otherwise pin every core to its coolest, slowest state for
/// the life of the boot.
pub fn ceiling(want_base: bool, caps: PerfCaps) -> (u8, bool) {
    let capped = want_base && caps.base >= caps.lowest && caps.base > 0;
    (if capped { caps.base } else { caps.highest }, capped)
}

/// `IA32_HWP_REQUEST`: Minimum[7:0], Maximum[15:8], Desired[23:16] = 0
/// (hardware chooses), Energy_Performance_Preference[31:24].
pub fn hwp_request(lowest: u8, max: u8, epp: u64) -> u64 {
    (lowest as u64) | ((max as u64) << 8) | (epp << 24)
}

/// `MSR_AMD_CPPC_REQUEST`: **Max**[7:0], **Min**[15:8], Desired[23:16] = 0,
/// Energy_Performance[31:24]. The first two are the other way round from
/// Intel's.
pub fn cppc_request(lowest: u8, max: u8, epp: u64) -> u64 {
    (max as u64) | ((lowest as u64) << 8) | (epp << 24)
}

// ── the two thermal sensors ─────────────────────────────────────────────────

/// `MSR_TEMPERATURE_TARGET`[23:16] in whole degrees, falling back to 100 when
/// the register reads back zero -- it is model-specific, and a part that does
/// not implement it must not make the governor believe the CPU is at its
/// throttle point already.
pub fn intel_tjmax_c(temp_target: u64) -> i32 {
    let t = ((temp_target >> 16) & 0xff) as i32;
    if t > 0 {
        t
    } else {
        100
    }
}

/// `IA32_THERM_STATUS` as a temperature in milli-degrees, or `None` while the
/// reading is not valid.
///
/// The digital sensor counts degrees **below** the throttle point, in
/// Reading[22:16], with Valid at bit 31.
pub fn intel_temp_mc(therm_status: u64, tjmax_c: i32) -> Option<i32> {
    if therm_status & (1 << 31) == 0 {
        return None;
    }
    let below_tjmax = ((therm_status >> 16) & 0x7f) as i32;
    Some((tjmax_c - below_tjmax) * 1000)
}

/// The family number `CPUID.01H:EAX` names, with the extended field folded in
/// the way both vendors specify. Zen is family `0x17`, which is base `0xf`
/// plus extended `0x8`.
pub fn cpu_family(cpuid1_eax: u32) -> u32 {
    let base = (cpuid1_eax >> 8) & 0xf;
    if base == 0xf {
        base + ((cpuid1_eax >> 20) & 0xff)
    } else {
        base
    }
}

/// `CurTmp` starts at bit 21 and each step is an eighth of a degree.
const ZEN_CUR_TEMP_SHIFT: u32 = 21;
/// With this bit set `CurTmp` uses the extended range, offset by -49 degrees.
const ZEN_CUR_TEMP_RANGE_SEL: u32 = 1 << 19;

/// AMD's reported-temperature register as `Tctl` in milli-degrees, the way
/// `k10temp` reads it. The per-model `Tdie` offset some parts apply is not
/// subtracted.
pub fn amd_tctl_mc(regval: u32) -> i32 {
    let mut temp = ((regval >> ZEN_CUR_TEMP_SHIFT) as i32) * 125;
    if regval & ZEN_CUR_TEMP_RANGE_SEL != 0 {
        temp -= 49_000;
    }
    temp
}

// ── the governor's decision ─────────────────────────────────────────────────

/// AMD `Tctl` throttle band in milli-degrees: Zen exposes no fixed throttle
/// point to read, so the band is fixed.
pub const AMD_COOL_MC: i32 = 78_000;
/// See [`AMD_COOL_MC`].
pub const AMD_HOT_MC: i32 = 88_000;
/// How far below the throttle point the Intel band sits, in whole degrees.
const INTEL_HOT_BELOW_TJMAX: i32 = 12;
/// See [`INTEL_HOT_BELOW_TJMAX`].
const INTEL_COOL_BELOW_TJMAX: i32 = 22;

/// `(cool, hot)` in milli-degrees for an Intel part with this throttle point.
///
/// The two floors stop a throttle point that reads far too low from putting
/// the band below room temperature, where the governor would hold every core
/// at its slowest for ever; they are ten degrees apart for the same reason the
/// offsets are, so the band can never invert.
pub fn intel_band_mc(tjmax_c: i32) -> (i32, i32) {
    let cool = (tjmax_c - INTEL_COOL_BELOW_TJMAX).max(40) * 1000;
    let hot = (tjmax_c - INTEL_HOT_BELOW_TJMAX).max(50) * 1000;
    (cool, hot)
}

/// One step of the ceiling walk, or `None` to hold where it is.
///
/// A step is an eighth of the range, so a full swing takes about eight
/// samples; at one sample a second that is gentle enough not to oscillate and
/// quick enough to act before the hardware's own throttle. `None` covers both
/// "inside the band" and "already at the end it is walking towards", so the
/// caller writes no MSR in either case.
pub fn next_ceiling(
    temp_mc: i32,
    band: (i32, i32),
    ceiling: u8,
    lowest: u8,
    ceil_max: u8,
) -> Option<u8> {
    let (cool, hot) = band;
    if ceil_max <= lowest {
        return None; // no room to scale
    }
    let step = core::cmp::max(1, (ceil_max - lowest) / 8);
    let next = if temp_mc >= hot {
        ceiling.saturating_sub(step).max(lowest)
    } else if temp_mc <= cool {
        core::cmp::min(ceil_max, ceiling.saturating_add(step))
    } else {
        return None; // inside the hysteresis band: hold
    };
    if next == ceiling {
        None
    } else {
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What an Intel part reports: turbo 42, base 30, floor 8.
    const HWP_CAPS_WORD: u64 = 0x0800_0000 | (30 << 8) | 42;
    /// The same three numbers as an AMD part reports them.
    const CPPC_CAPS_WORD: u64 = (42 << 24) | (30 << 16) | 8;

    fn caps() -> PerfCaps {
        PerfCaps {
            highest: 42,
            base: 30,
            lowest: 8,
        }
    }

    // ── the bounds each vendor publishes ────────────────────────────────────

    #[test]
    fn intel_publishes_its_bounds_turbo_first() {
        assert_eq!(hwp_caps(HWP_CAPS_WORD), caps());
    }

    #[test]
    fn amd_publishes_the_same_three_the_other_way_round() {
        assert_eq!(cppc_caps(CPPC_CAPS_WORD), caps());
    }

    #[test]
    fn neither_reads_a_byte_that_is_not_its_own() {
        // Every byte set: each field must still come back 0xff and not run
        // into its neighbour.
        let all = u64::MAX;
        assert_eq!(
            hwp_caps(all),
            PerfCaps {
                highest: 0xff,
                base: 0xff,
                lowest: 0xff
            }
        );
        assert_eq!(
            cppc_caps(all),
            PerfCaps {
                highest: 0xff,
                base: 0xff,
                lowest: 0xff
            }
        );
    }

    // ── the ceiling to ask for ──────────────────────────────────────────────

    #[test]
    fn without_noturbo_the_ceiling_is_the_turbo_clock() {
        assert_eq!(ceiling(false, caps()), (42, false));
    }

    #[test]
    fn with_noturbo_it_is_the_base_clock() {
        assert_eq!(ceiling(true, caps()), (30, true));
    }

    #[test]
    fn a_base_clock_the_firmware_never_reported_is_not_believed() {
        // Zero would pin every core to its coolest, slowest point for the life
        // of the boot.
        let mut c = caps();
        c.base = 0;
        assert_eq!(ceiling(true, c), (42, false));
    }

    #[test]
    fn nor_one_on_a_part_that_reports_no_bounds_at_all() {
        // With the floor at zero too, "not below the floor" is satisfied by
        // the same zero, and the ceiling would be asked for as performance 0.
        let c = PerfCaps {
            highest: 42,
            base: 0,
            lowest: 0,
        };
        assert_eq!(ceiling(true, c), (42, false));
    }

    #[test]
    fn nor_is_one_below_the_floor() {
        let mut c = caps();
        c.base = 4;
        assert_eq!(ceiling(true, c), (42, false));
        c.base = 8; // equal to the floor is still a point it can run at
        assert_eq!(ceiling(true, c), (8, true));
    }

    // ── the word that asks for a ceiling ────────────────────────────────────

    #[test]
    fn intel_asks_with_the_minimum_first() {
        // Minimum[7:0] = 8, Maximum[15:8] = 30, Desired[23:16] = 0, EPP = 0.
        assert_eq!(hwp_request(8, 30, 0), 0x0000_1e08);
        assert_eq!(hwp_request(8, 30, 0x80), 0x8000_1e08);
    }

    #[test]
    fn amd_asks_with_the_maximum_first() {
        // Max[7:0] = 30, Min[15:8] = 8, Desired[23:16] = 0, EPP = 0.
        assert_eq!(cppc_request(8, 30, 0), 0x0000_081e);
        assert_eq!(cppc_request(8, 30, 0x80), 0x8000_081e);
    }

    #[test]
    fn the_two_vendors_do_not_put_the_fields_in_the_same_place() {
        // If they ever agree, one of them is wrong: asking a CPU for a floor
        // where it reads a ceiling pins it at its slowest or lets it run flat
        // out, and nothing in this kernel would say so.
        assert_ne!(hwp_request(8, 30, 0), cppc_request(8, 30, 0));
    }

    #[test]
    fn what_the_request_asks_for_is_what_the_bounds_said() {
        // Read each field back out of the word by its own position, so the
        // decode and the encode are pinned as a pair: the three call sites
        // build this word from the caps, and a field that moved on one side
        // and not the other reads as correct code.
        let intel = hwp_caps(HWP_CAPS_WORD);
        let (max, capped) = ceiling(true, intel);
        assert!(capped);
        let w = hwp_request(intel.lowest, max, 0);
        assert_eq!(w & 0xff, intel.lowest as u64, "Minimum[7:0]");
        assert_eq!((w >> 8) & 0xff, max as u64, "Maximum[15:8]");
        assert_eq!((w >> 16) & 0xff, 0, "Desired[23:16]: hardware chooses");

        let amd = cppc_caps(CPPC_CAPS_WORD);
        let (amd_max, _) = ceiling(true, amd);
        assert_eq!(max, amd_max, "the two vendors' bounds decode alike");
        let w = cppc_request(amd.lowest, amd_max, 0);
        assert_eq!(w & 0xff, amd_max as u64, "Max[7:0]");
        assert_eq!((w >> 8) & 0xff, amd.lowest as u64, "Min[15:8]");
        assert_eq!((w >> 16) & 0xff, 0, "Desired[23:16]: hardware chooses");
    }

    // ── the throttle point and the sensor ───────────────────────────────────

    #[test]
    fn the_throttle_point_comes_out_of_bits_sixteen_to_twenty_three() {
        assert_eq!(intel_tjmax_c(100 << 16), 100);
        assert_eq!(intel_tjmax_c(105 << 16), 105);
        // Neighbouring bits must not leak in.
        assert_eq!(intel_tjmax_c((100 << 16) | 0xffff | (0xff << 24)), 100);
    }

    #[test]
    fn a_part_that_does_not_report_a_throttle_point_gets_a_hundred() {
        // Zero would read as "this core is already at its throttle point",
        // and the governor would walk every ceiling to the floor.
        assert_eq!(intel_tjmax_c(0), 100);
    }

    #[test]
    fn the_sensor_counts_degrees_below_the_throttle_point() {
        let valid = 1u64 << 31;
        assert_eq!(intel_temp_mc(valid | (30 << 16), 100), Some(70_000));
        assert_eq!(intel_temp_mc(valid, 100), Some(100_000));
        assert_eq!(intel_temp_mc(valid | (12 << 16), 105), Some(93_000));
    }

    #[test]
    fn a_reading_that_is_not_valid_is_no_reading() {
        assert_eq!(intel_temp_mc(30 << 16, 100), None);
    }

    #[test]
    fn the_reading_is_seven_bits_and_not_eight() {
        // Bit 23 is the resolution field, not part of the count.
        let valid = 1u64 << 31;
        assert_eq!(
            intel_temp_mc(valid | (1 << 23) | (30 << 16), 100),
            Some(70_000)
        );
    }

    // ── the AMD side ────────────────────────────────────────────────────────

    #[test]
    fn the_family_folds_in_the_extended_field_only_when_it_has_to() {
        assert_eq!(cpu_family(0x0080_0f00), 0x17); // Zen: base f, extended 8
        assert_eq!(cpu_family(0x00a0_0f00), 0x19); // Zen 3/4
        assert_eq!(cpu_family(0x0009_0600), 0x06); // Intel: base 6, extended ignored
                                                   // Both vendors say the extended field is only read when the base one
                                                   // reads 0xf, so a part that sets it anyway must not be added to.
        assert_eq!(cpu_family(0x00b0_0600), 0x06);
    }

    #[test]
    fn the_zen_sensor_counts_in_eighths_of_a_degree() {
        assert_eq!(amd_tctl_mc(500 << 21), 62_500);
        assert_eq!(amd_tctl_mc(0), 0);
    }

    #[test]
    fn and_shifts_down_by_forty_nine_in_the_extended_range() {
        assert_eq!(amd_tctl_mc((500 << 21) | (1 << 19)), 62_500 - 49_000);
    }

    #[test]
    fn the_bits_under_the_reading_are_not_part_of_it() {
        // Everything below bit 21 set, including the range bit.
        assert_eq!(amd_tctl_mc((500 << 21) | 0x001f_ffff), 62_500 - 49_000);
    }

    // ── the band ────────────────────────────────────────────────────────────

    #[test]
    fn the_intel_band_sits_below_the_throttle_point() {
        assert_eq!(intel_band_mc(100), (78_000, 88_000));
        assert_eq!(intel_band_mc(105), (83_000, 93_000));
    }

    #[test]
    fn a_throttle_point_that_reads_far_too_low_does_not_put_the_band_at_room_temperature() {
        // The governor would otherwise hold every core at its slowest for ever.
        assert_eq!(intel_band_mc(0), (40_000, 50_000));
        assert_eq!(intel_band_mc(55), (40_000, 50_000));
    }

    #[test]
    fn the_band_can_never_invert() {
        // Cool above hot would make both arms fire on the same reading.
        for tjmax in 0..=255 {
            let (cool, hot) = intel_band_mc(tjmax);
            assert!(cool < hot, "tjmax {} gave {}..{}", tjmax, cool, hot);
        }
        assert!(AMD_COOL_MC < AMD_HOT_MC);
    }

    // ── the walk ────────────────────────────────────────────────────────────

    const BAND: (i32, i32) = (78_000, 88_000);

    #[test]
    fn hot_walks_the_ceiling_down_by_one_step() {
        // (42 - 8) / 8 = 4.
        assert_eq!(next_ceiling(90_000, BAND, 42, 8, 42), Some(38));
    }

    #[test]
    fn cool_walks_it_back_up() {
        assert_eq!(next_ceiling(70_000, BAND, 38, 8, 42), Some(42));
        assert_eq!(next_ceiling(70_000, BAND, 20, 8, 42), Some(24));
    }

    #[test]
    fn inside_the_band_it_holds() {
        assert_eq!(next_ceiling(80_000, BAND, 30, 8, 42), None);
        // The edges belong to the arms, not to the band.
        assert_eq!(next_ceiling(88_000, BAND, 30, 8, 42), Some(26));
        assert_eq!(next_ceiling(78_000, BAND, 30, 8, 42), Some(34));
    }

    #[test]
    fn neither_end_is_passed() {
        assert_eq!(next_ceiling(90_000, BAND, 10, 8, 42), Some(8));
        assert_eq!(next_ceiling(90_000, BAND, 8, 8, 42), None);
        assert_eq!(next_ceiling(70_000, BAND, 40, 8, 42), Some(42));
        assert_eq!(next_ceiling(70_000, BAND, 42, 8, 42), None);
    }

    #[test]
    fn a_part_with_nowhere_to_go_is_left_alone() {
        assert_eq!(next_ceiling(90_000, BAND, 8, 8, 8), None);
        assert_eq!(next_ceiling(90_000, BAND, 8, 9, 8), None);
    }

    #[test]
    fn a_range_too_narrow_for_eight_steps_still_moves() {
        // (12 - 8) / 8 == 0, and a step of nothing is a governor that never
        // throttles.
        assert_eq!(next_ceiling(90_000, BAND, 12, 8, 12), Some(11));
    }

    #[test]
    fn a_full_swing_takes_about_eight_samples() {
        let (lowest, ceil_max) = (8u8, 42u8);
        let mut ceiling = ceil_max;
        let mut steps = 0;
        while let Some(next) = next_ceiling(90_000, BAND, ceiling, lowest, ceil_max) {
            ceiling = next;
            steps += 1;
            assert!(steps <= 16, "the walk down does not end");
        }
        assert_eq!(ceiling, lowest);
        assert_eq!(steps, 9);

        let mut up = 0;
        while let Some(next) = next_ceiling(70_000, BAND, ceiling, lowest, ceil_max) {
            ceiling = next;
            up += 1;
            assert!(up <= 16, "the walk up does not end");
        }
        assert_eq!(ceiling, ceil_max);
        assert_eq!(up, 9);
    }
}
