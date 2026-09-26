//! The battery-backed clock on the board, and the arithmetic that turns its
//! registers into a Unix timestamp.
//!
//! Without this the wall clock starts at the epoch, which makes every TLS
//! certificate look "not yet valid" and breaks `wget https://...` for any
//! client that validates one; so this runs once, at boot, before anything can
//! ask what time it is.
//!
//! It lived in `bare/arch/x86_64/timer.rs`, welded to the two I/O ports it
//! reads, so nothing that could report a mistake in it ever compiled a line of
//! it. Only the ports are architecture-specific: the register layout is the
//! MC146818's, which every PC-compatible board still answers to, and every
//! failure here is the quiet kind -- a date a century out, or a boot that stops
//! before it can say why. The port I/O stays in the architecture; the decision
//! of what the bytes mean is here, where a test can reach it.
//!
//! Reference: Linux's `drivers/rtc/rtc-mc146818-lib.c`
//! (`mc146818_avoid_UIP`, `mc146818_get_time`).

/// Status register B, bit 2: the clock counts in plain binary rather than BCD.
const STATUS_B_BINARY: u8 = 0x04;
/// Status register B, bit 1: the hours register counts 0..23 rather than 1..12.
const STATUS_B_24_HOUR: u8 = 0x02;
/// The hours register's top bit in 12-hour mode: the afternoon.
const HOUR_PM: u8 = 0x80;

/// How many register sets [`settle`] will ask for before giving up.
///
/// Linux reads the same clock through `mc146818_avoid_UIP`, which tries 100
/// times and then reports `-EIO` on the grounds that a clock still mid-update
/// after that long "is apparently broken or not present". A board with no
/// RTC behind those ports answers every read with the same byte, so it settles
/// on the first two tries; one whose ports float never settles at all, and
/// without a bound the kernel would spin there forever, before it has printed
/// a line anyone could read.
pub const SETTLE_TRIES: u32 = 100;

/// The one century register value this kernel believes.
///
/// A real century register is named by the ACPI FADT, which is how Linux finds
/// it; there is no FADT here, so the address is hardcoded and on a board that
/// has no century register it is ordinary battery-backed CMOS RAM holding
/// whatever the firmware left there. Believing an arbitrary byte moves the
/// clock by whole centuries, so it is believed only when it says the century
/// this kernel is running in, which is the one case where believing it and
/// ignoring it give the same answer. Anything else -- a stale `0x19`, a stale
/// `0x21`, `0xff` from an absent register -- falls through to [`full_year`]'s
/// window. Revisit in 2100.
const TRUSTED_CENTURY: u8 = 20;

/// The seven clock registers, read as one set.
///
/// `PartialEq` is the point: [`settle`] compares two consecutive sets and only
/// believes them once they agree, because the clock updates them one at a time
/// and a read that straddles a tick returns a mixture of before and after.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct RtcRegs {
    pub sec: u8,
    pub min: u8,
    pub hour: u8,
    pub day: u8,
    pub month: u8,
    pub year: u8,
    pub century: u8,
}

/// One packed decimal byte as a number: `0x26` is twenty-six.
pub fn bcd_to_bin(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

/// Ask for register sets until two in a row agree, or give up.
///
/// `sample` reads all seven registers at once and returns `None` when it could
/// not get a clean set (an update was still in progress); that costs a try like
/// any other, so a clock that never finishes updating cannot hold the boot.
pub fn settle<S>(mut sample: S) -> Option<RtcRegs>
where
    S: FnMut() -> Option<RtcRegs>,
{
    let mut last: Option<RtcRegs> = None;
    for _ in 0..SETTLE_TRIES {
        match sample() {
            Some(cur) if Some(cur) == last => return Some(cur),
            Some(cur) => last = Some(cur),
            None => last = None,
        }
    }
    None
}

/// The year the year and century registers name, both already converted out of
/// BCD.
///
/// The year register holds two digits, so something has to say which century
/// they belong to. When the century register cannot be believed (see
/// [`TRUSTED_CENTURY`]) the rule is Linux's: two digits below 70 mean the
/// twenty-first century, 70 and up mean the twentieth. That window is what
/// userspace expects, and it is why a clock whose battery died and came up
/// reading `99` says 1999 rather than 2099 -- a date in the past breaks less
/// than a date forty years in the future, which makes every certificate look
/// expired and every build artifact newer than its source.
pub fn full_year(year_reg: u8, century_reg: u8) -> i64 {
    let mut year = year_reg as i64;
    if century_reg == TRUSTED_CENTURY {
        year += (TRUSTED_CENTURY as i64 - 19) * 100;
    }
    if year <= 69 {
        year += 100;
    }
    1900 + year
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). Valid for `year >= 1970`.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Seconds since the Unix epoch for one settled register set, or `None` when
/// the bytes cannot be a date.
///
/// `status_b` is status register B, which says how to read the others: packed
/// decimal or plain binary, and hours round the clock or hours with an
/// afternoon bit. A board can be in any of the four combinations; the emulator
/// is only ever in one of them.
///
/// What is deliberately *not* rejected: a day the month does not have. A clock
/// reading the 31st of February is wrong, but `None` here means the wall clock
/// stays at the epoch, and being a day or two out beats being fifty-six years
/// out.
pub fn decode(regs: RtcRegs, status_b: u8) -> Option<u64> {
    let is_bcd = status_b & STATUS_B_BINARY == 0;
    let is_12_hour = status_b & STATUS_B_24_HOUR == 0;

    // The afternoon bit rides in the hours register, so take it before any
    // conversion strips it.
    let pm = regs.hour & HOUR_PM != 0;

    let mut sec = regs.sec;
    let mut min = regs.min;
    let mut hour = regs.hour & !HOUR_PM;
    let mut day = regs.day;
    let mut month = regs.month;
    let mut year = regs.year;
    let mut century = regs.century;

    if is_bcd {
        sec = bcd_to_bin(sec);
        min = bcd_to_bin(min);
        hour = bcd_to_bin(hour);
        day = bcd_to_bin(day);
        month = bcd_to_bin(month);
        year = bcd_to_bin(year);
        century = bcd_to_bin(century);
    }

    if is_12_hour {
        // Noon is 12 with the bit set and midnight is 12 without it, so the
        // remainder has to come first in both directions.
        hour = if pm { (hour % 12) + 12 } else { hour % 12 };
    }

    let gregorian_year = full_year(year, century);

    // The last two are backstops: `full_year` cannot name a year before the
    // epoch, so no byte reaches them and a mutation that removes either
    // survives. The invariant they stand for has its own test instead.
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || min > 59
        || sec > 60
        || gregorian_year < 1970
    {
        return None;
    }

    let days = days_from_civil(gregorian_year, month as i64, day as i64);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hour as u64 * 3_600 + min as u64 * 60 + sec as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// Status register B as the emulator always reports it: packed decimal,
    /// hours round the clock. Three of the four combinations below never run
    /// there, which is why they are here.
    const BCD_24H: u8 = 0x02;
    const BINARY_24H: u8 = 0x02 | 0x04;
    const BCD_12H: u8 = 0x00;
    const BINARY_12H: u8 = 0x04;

    /// 2026-09-26 01:23:45 UTC.
    const MORNING: u64 = 1_790_385_825;
    /// The same day at 13:23:45 UTC.
    const AFTERNOON: u64 = 1_790_429_025;

    fn bcd(n: u8) -> u8 {
        ((n / 10) << 4) | (n % 10)
    }

    /// The registers a clock in packed decimal, 24-hour mode holds for a date.
    fn regs_bcd(year: u8, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> RtcRegs {
        RtcRegs {
            sec: bcd(sec),
            min: bcd(min),
            hour: bcd(hour),
            day: bcd(day),
            month: bcd(month),
            year: bcd(year),
            century: bcd(20),
        }
    }

    /// A sampler that hands out a canned sequence and then repeats the last
    /// entry, counting how many times it was asked.
    struct Sampler {
        seq: RefCell<alloc::vec::Vec<Option<RtcRegs>>>,
        calls: RefCell<u32>,
    }

    impl Sampler {
        fn new(seq: alloc::vec::Vec<Option<RtcRegs>>) -> Self {
            Self {
                seq: RefCell::new(seq),
                calls: RefCell::new(0),
            }
        }

        fn next(&self) -> Option<RtcRegs> {
            *self.calls.borrow_mut() += 1;
            let mut seq = self.seq.borrow_mut();
            if seq.len() > 1 {
                seq.remove(0)
            } else {
                seq[0]
            }
        }
    }

    fn a() -> RtcRegs {
        regs_bcd(26, 9, 26, 1, 23, 45)
    }

    fn b() -> RtcRegs {
        regs_bcd(26, 9, 26, 1, 23, 46)
    }

    // ── packed decimal ──────────────────────────────────────────────────────

    #[test]
    fn a_packed_decimal_byte_is_two_digits() {
        assert_eq!(bcd_to_bin(0x00), 0);
        assert_eq!(bcd_to_bin(0x09), 9);
        assert_eq!(bcd_to_bin(0x10), 10);
        assert_eq!(bcd_to_bin(0x26), 26);
        assert_eq!(bcd_to_bin(0x59), 59);
        assert_eq!(bcd_to_bin(0x99), 99);
    }

    #[test]
    fn a_byte_that_is_not_packed_decimal_still_comes_back_a_number() {
        // Every nibble pair has an answer -- what matters is that nothing here
        // panics or wraps, because these are the bytes an absent register gives.
        assert_eq!(bcd_to_bin(0xFF), 165);
        assert_eq!(bcd_to_bin(0x0F), 15);
    }

    // ── which century the two digits belong to ──────────────────────────────

    #[test]
    fn the_century_register_says_which_century_when_it_can_be_believed() {
        assert_eq!(full_year(26, 20), 2026);
        assert_eq!(full_year(0, 20), 2000);
        assert_eq!(full_year(99, 20), 2099);
    }

    #[test]
    fn without_a_century_register_two_digits_under_seventy_are_this_century() {
        assert_eq!(full_year(26, 0), 2026);
        assert_eq!(full_year(0, 0), 2000);
        assert_eq!(full_year(69, 0), 2069);
    }

    #[test]
    fn and_seventy_and_up_are_the_last_one() {
        // A clock whose battery died and came up reading 99 says 1999, not
        // 2099: a date in the past breaks less than one forty years ahead.
        assert_eq!(full_year(70, 0), 1970);
        assert_eq!(full_year(99, 0), 1999);
    }

    #[test]
    fn a_century_register_reading_nineteen_is_not_believed() {
        // 0x32 is ordinary CMOS RAM on a board with no century register, so a
        // stale byte there must not move the clock into the last century.
        assert_eq!(full_year(26, 19), 2026);
        assert_eq!(full_year(99, 19), 1999);
    }

    #[test]
    fn nor_is_one_reading_twenty_one() {
        assert_eq!(full_year(26, 21), 2026);
    }

    #[test]
    fn nor_the_ones_an_absent_register_gives() {
        // 0xff decoded out of packed decimal, and a register that reads zero.
        assert_eq!(full_year(26, 165), 2026);
        assert_eq!(full_year(26, 0xFF), 2026);
    }

    #[test]
    fn the_window_cannot_name_a_year_before_the_epoch() {
        // The guards in `decode` against a year before 1970 and a negative day
        // count are backstops: no byte reaches them. A mutation that removes
        // either survives, and that is why this test names the invariant they
        // stand for instead.
        for year_reg in 0..=u8::MAX {
            for century in [0u8, 19, 20, 21, 165, 0xFF] {
                let y = full_year(year_reg, century);
                assert!(
                    y >= 1970,
                    "year_reg {} century {} gave {}",
                    year_reg,
                    century,
                    y
                );
                assert!(
                    days_from_civil(y, 1, 1) >= 0,
                    "year_reg {} century {} gave {}",
                    year_reg,
                    century,
                    y
                );
            }
        }
    }

    // ── the calendar ────────────────────────────────────────────────────────

    #[test]
    fn the_epoch_itself_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn a_leap_day_is_a_day() {
        assert_eq!(
            days_from_civil(2024, 3, 1) - days_from_civil(2024, 2, 28),
            2
        );
        assert_eq!(
            days_from_civil(2023, 3, 1) - days_from_civil(2023, 2, 28),
            1
        );
        // 1900 was not a leap year and 2000 was.
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2
        );
    }

    // ── the four ways a board can hold the same instant ─────────────────────

    #[test]
    fn the_four_combinations_of_status_b_read_the_same_instant() {
        // The emulator is only ever in the first of these, so the other three
        // had never been run by anything.
        let bcd_24 = regs_bcd(26, 9, 26, 1, 23, 45);
        let binary_24 = RtcRegs {
            sec: 45,
            min: 23,
            hour: 1,
            day: 26,
            month: 9,
            year: 26,
            century: 20,
        };
        let mut bcd_12 = bcd_24;
        bcd_12.hour = bcd(1);
        let mut binary_12 = binary_24;
        binary_12.hour = 1;

        assert_eq!(decode(bcd_24, BCD_24H), Some(MORNING));
        assert_eq!(decode(binary_24, BINARY_24H), Some(MORNING));
        assert_eq!(decode(bcd_12, BCD_12H), Some(MORNING));
        assert_eq!(decode(binary_12, BINARY_12H), Some(MORNING));
    }

    #[test]
    fn an_afternoon_hour_comes_back_in_the_afternoon() {
        let mut bcd_12 = regs_bcd(26, 9, 26, 1, 23, 45);
        bcd_12.hour = bcd(1) | HOUR_PM;
        assert_eq!(decode(bcd_12, BCD_12H), Some(AFTERNOON));

        let mut binary_12 = RtcRegs {
            sec: 45,
            min: 23,
            hour: 1 | HOUR_PM,
            day: 26,
            month: 9,
            year: 26,
            century: 20,
        };
        assert_eq!(decode(binary_12, BINARY_12H), Some(AFTERNOON));
        binary_12.hour = 1;
        assert_eq!(decode(binary_12, BINARY_12H), Some(MORNING));
    }

    #[test]
    fn noon_and_midnight_are_the_two_a_twelve_hour_clock_gets_wrong() {
        // Both are hour 12 on the register; only the afternoon bit separates
        // them, and neither is hour 12 nor hour 0 after conversion by accident.
        let mut midnight = regs_bcd(26, 9, 26, 12, 7, 5);
        midnight.hour = bcd(12);
        assert_eq!(decode(midnight, BCD_12H), Some(1_790_381_225));

        let mut noon = regs_bcd(26, 9, 26, 12, 0, 0);
        noon.hour = bcd(12) | HOUR_PM;
        assert_eq!(decode(noon, BCD_12H), Some(1_790_424_000));
    }

    #[test]
    fn the_afternoon_bit_is_not_a_digit_of_the_hour() {
        // In 24-hour mode the bit is not there to read, and stripping it must
        // not change an hour that legitimately uses those bits.
        let regs = regs_bcd(26, 9, 26, 23, 59, 59);
        assert_eq!(
            decode(regs, BCD_24H),
            Some(MORNING + 22 * 3600 + 36 * 60 + 14)
        );
    }

    // ── bytes that cannot be a date ─────────────────────────────────────────

    #[test]
    fn a_month_the_year_does_not_have_is_not_a_date() {
        let mut regs = regs_bcd(26, 9, 26, 1, 23, 45);
        regs.month = bcd(0);
        assert_eq!(decode(regs, BCD_24H), None);
        regs.month = bcd(13);
        assert_eq!(decode(regs, BCD_24H), None);
    }

    #[test]
    fn a_day_no_month_has_is_not_a_date() {
        let mut regs = regs_bcd(26, 9, 26, 1, 23, 45);
        regs.day = bcd(0);
        assert_eq!(decode(regs, BCD_24H), None);
        regs.day = bcd(32);
        assert_eq!(decode(regs, BCD_24H), None);
    }

    #[test]
    fn an_hour_or_a_minute_past_the_clock_is_not_a_date() {
        let mut regs = regs_bcd(26, 9, 26, 1, 23, 45);
        regs.hour = bcd(24);
        assert_eq!(decode(regs, BCD_24H), None);
        regs = regs_bcd(26, 9, 26, 1, 23, 45);
        regs.min = bcd(60);
        assert_eq!(decode(regs, BCD_24H), None);
    }

    #[test]
    fn a_leap_second_is_a_second() {
        // The clock can read 60 seconds during one, and a date is what comes
        // back rather than nothing.
        let regs = regs_bcd(26, 9, 26, 1, 23, 60);
        assert_eq!(decode(regs, BCD_24H), Some(MORNING + 15));
        let mut past = regs;
        past.sec = bcd(61);
        assert_eq!(decode(past, BCD_24H), None);
    }

    #[test]
    fn a_day_the_month_is_too_short_for_is_still_a_date() {
        // Deliberate: `None` leaves the wall clock at the epoch, and being two
        // days out beats being fifty-six years out.
        let regs = regs_bcd(26, 2, 31, 1, 23, 45);
        assert!(decode(regs, BCD_24H).is_some());
    }

    #[test]
    fn every_register_reading_ones_is_not_a_date() {
        // What an absent clock behind the ports answers.
        let regs = RtcRegs {
            sec: 0xFF,
            min: 0xFF,
            hour: 0xFF,
            day: 0xFF,
            month: 0xFF,
            year: 0xFF,
            century: 0xFF,
        };
        assert_eq!(decode(regs, BCD_24H), None);
        assert_eq!(decode(regs, BINARY_24H), None);
        assert_eq!(decode(RtcRegs::default(), BCD_24H), None);
    }

    // ── settling, and the boot that used to stop here ───────────────────────

    #[test]
    fn two_register_sets_that_agree_are_the_time() {
        let s = Sampler::new(alloc::vec![Some(a())]);
        assert_eq!(settle(|| s.next()), Some(a()));
        assert_eq!(*s.calls.borrow(), 2);
    }

    #[test]
    fn a_set_read_across_a_tick_is_thrown_away() {
        let s = Sampler::new(alloc::vec![Some(a()), Some(b())]);
        assert_eq!(settle(|| s.next()), Some(b()));
    }

    #[test]
    fn a_clock_that_never_settles_does_not_hold_the_boot() {
        // Ports that float answer differently every time. Without a bound this
        // spun forever, before the kernel had printed a line anyone could read.
        let mut n = 0u8;
        let calls = RefCell::new(0u32);
        let out = settle(|| {
            *calls.borrow_mut() += 1;
            n = n.wrapping_add(1);
            let mut r = a();
            r.sec = n;
            Some(r)
        });
        assert_eq!(out, None);
        assert_eq!(*calls.borrow(), SETTLE_TRIES);
    }

    #[test]
    fn nor_does_one_that_never_finishes_updating() {
        let calls = RefCell::new(0u32);
        let out = settle(|| {
            *calls.borrow_mut() += 1;
            None
        });
        assert_eq!(out, None);
        assert_eq!(*calls.borrow(), SETTLE_TRIES);
    }

    #[test]
    fn an_update_between_two_agreeing_sets_starts_the_count_again() {
        // The set before the update and the set after it are not two
        // consecutive reads, whatever they say.
        let s = Sampler::new(alloc::vec![Some(a()), None, Some(a()), Some(a())]);
        assert_eq!(settle(|| s.next()), Some(a()));
        assert_eq!(*s.calls.borrow(), 4);
    }

    // ── end to end ──────────────────────────────────────────────────────────

    #[test]
    fn the_bytes_the_emulator_holds_now_decode_to_now() {
        let s = Sampler::new(alloc::vec![Some(regs_bcd(26, 9, 26, 1, 23, 45))]);
        let regs = settle(|| s.next()).expect("settles");
        assert_eq!(decode(regs, BCD_24H), Some(MORNING));
    }

    #[test]
    fn the_last_second_of_the_last_century_decodes_without_a_century_register() {
        let mut regs = regs_bcd(99, 12, 31, 23, 59, 59);
        regs.century = 0;
        assert_eq!(decode(regs, BCD_24H), Some(946_684_799));
    }

    #[test]
    fn a_leap_day_decodes() {
        assert_eq!(
            decode(regs_bcd(24, 2, 29, 23, 59, 59), BCD_24H),
            Some(1_709_251_199)
        );
    }
}
