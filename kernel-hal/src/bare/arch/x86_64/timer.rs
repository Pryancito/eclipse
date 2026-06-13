use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use spin::Once;
use x86_64::instructions::port::Port;

/// Global monotonic floor in nanoseconds. Unsynchronized per-CPU TSCs can read
/// backwards across cores; smoltcp's TCP timers (and every sleep/timeout in the
/// kernel) require non-decreasing time, so clamp each reading to the highest
/// value observed on any CPU.
static MONO_NS: AtomicU64 = AtomicU64::new(0);

pub fn timer_now() -> Duration {
    let cycle = unsafe { core::arch::x86_64::_rdtsc() };
    let ns = cycle.wrapping_mul(1000) / super::cpu::cpu_frequency() as u64;
    // `fetch_max` returns the previous value; the effective clock is the larger
    // of the previous floor and this reading, guaranteeing it never goes back.
    let prev = MONO_NS.fetch_max(ns, Ordering::Relaxed);
    Duration::from_nanos(prev.max(ns))
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
// Without this the wall clock starts at the Unix epoch (1970), which makes
// every TLS certificate look "not yet valid" and breaks `wget https://...`
// for any client that validates certificates. Reading the RTC at boot gives
// a real date so `date` is no longer required before HTTPS.

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const RTC_SECONDS: u8 = 0x00;
const RTC_MINUTES: u8 = 0x02;
const RTC_HOURS: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_CENTURY: u8 = 0x32;
const RTC_STATUS_A: u8 = 0x0A;
const RTC_STATUS_B: u8 = 0x0B;

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

fn bcd_to_bin(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) * 10)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct RtcRaw {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    century: u8,
}

unsafe fn rtc_read_raw() -> RtcRaw {
    RtcRaw {
        sec: cmos_read(RTC_SECONDS),
        min: cmos_read(RTC_MINUTES),
        hour: cmos_read(RTC_HOURS),
        day: cmos_read(RTC_DAY),
        month: cmos_read(RTC_MONTH),
        year: cmos_read(RTC_YEAR),
        century: cmos_read(RTC_CENTURY),
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). Valid for `year >= 1970`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Read the CMOS RTC and return seconds since the Unix epoch, or `None` if the
/// values look invalid (no RTC, garbage, etc.).
fn read_rtc_epoch() -> Option<u64> {
    unsafe {
        // Wait out any in-progress update, then read twice until two reads
        // agree, to avoid catching the RTC mid-tick.
        let mut spins = 0u32;
        while rtc_update_in_progress() {
            spins += 1;
            if spins > 1_000_000 {
                break;
            }
        }
        let mut last = rtc_read_raw();
        loop {
            let mut s = 0u32;
            while rtc_update_in_progress() {
                s += 1;
                if s > 1_000_000 {
                    break;
                }
            }
            let cur = rtc_read_raw();
            if cur == last {
                break;
            }
            last = cur;
        }

        let status_b = cmos_read(RTC_STATUS_B);
        let is_bcd = status_b & 0x04 == 0;
        let is_12h = status_b & 0x02 == 0;

        let mut sec = last.sec;
        let mut min = last.min;
        // Preserve the PM flag (bit 7) before any BCD conversion strips it.
        let pm = last.hour & 0x80 != 0;
        let mut hour = last.hour & 0x7F;
        let mut day = last.day;
        let mut month = last.month;
        let mut year = last.year;
        let mut century = last.century;

        if is_bcd {
            sec = bcd_to_bin(sec);
            min = bcd_to_bin(min);
            hour = bcd_to_bin(hour);
            day = bcd_to_bin(day);
            month = bcd_to_bin(month);
            year = bcd_to_bin(year);
            century = bcd_to_bin(century);
        }

        if is_12h {
            if pm {
                hour = (hour % 12) + 12;
            } else {
                hour %= 12;
            }
        }

        // The century register is optional; fall back to 21st century.
        let full_year: i64 = if (19..=21).contains(&century) {
            century as i64 * 100 + year as i64
        } else {
            2000 + year as i64
        };

        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || min > 59
            || sec > 60
            || full_year < 1970
        {
            return None;
        }

        let days = days_from_civil(full_year, month as i64, day as i64);
        if days < 0 {
            return None;
        }
        let secs = days as u64 * 86_400 + hour as u64 * 3_600 + min as u64 * 60 + sec as u64;
        Some(secs)
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
