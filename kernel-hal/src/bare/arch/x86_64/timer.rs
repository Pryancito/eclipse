use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use spin::Once;
use x86_64::instructions::port::Port;

/// Fixed-point factor: `nanos = (tsc * TSC_MULT) >> 32`, set once the real TSC
/// frequency is known (PIT calibration or CPUID 0x15). 0 = not calibrated yet.
static TSC_MULT: AtomicU64 = AtomicU64::new(0);

pub fn timer_now() -> Duration {
    let cycle = unsafe { core::arch::x86_64::_rdtsc() };
    let mult = TSC_MULT.load(Ordering::Relaxed);
    if mult != 0 {
        Duration::from_nanos(((cycle as u128 * mult as u128) >> 32) as u64)
    } else {
        // Early-boot fallback: CPUID base frequency (may be off by 20-30% on
        // real hardware, and 2 GHz default when leaf 0x16 is missing).
        Duration::from_nanos(cycle * 1000 / super::cpu::cpu_frequency() as u64)
    }
}

fn set_tsc_hz(hz: u64) {
    if hz != 0 {
        TSC_MULT.store(
            ((1_000_000_000u128 << 32) / hz as u128) as u64,
            Ordering::Relaxed,
        );
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
// LAPIC timer / TSC calibration against the i8254 PIT
// ---------------------------------------------------------------------------
// The LAPIC timer does NOT count at the CPU core frequency on real hardware:
// it counts at the bus/crystal clock (~25-38 MHz on modern Intel). Deriving
// the initial count from cpu_frequency() therefore makes the "10 ms" tick
// last seconds on bare metal (while looking fine under QEMU, whose virtual
// LAPIC runs near 1 GHz). Measure both the LAPIC timer and the TSC against
// the PIT, which ticks at a known 1.193182 MHz on every PC.

const PIT_FREQ_HZ: u64 = 1_193_182;
const CALIBRATE_MS: u64 = 20;

struct PitCalibration {
    lapic_hz: u64,
    tsc_hz: u64,
}

/// Measure the LAPIC-timer and TSC frequencies over a PIT channel-2 window.
///
/// Channel 2 is gated by port 0x61 bit 0 and raises OUT (port 0x61 bit 5) on
/// terminal count, so no interrupts are involved. The caller must have set
/// the LAPIC divide configuration; the timer is left stopped on return.
///
/// Returns `None` when the PIT is missing/fake (some modern boards drop the
/// legacy 8254) — detected by implausible measurements.
unsafe fn pit_calibrate() -> Option<PitCalibration> {
    use x2apic::lapic::TimerMode;
    use zcore_drivers::irq::x86::Apic;

    let mut port_gate = Port::<u8>::new(0x61);
    let mut port_cmd = Port::<u8>::new(0x43);
    let mut port_ch2 = Port::<u8>::new(0x42);

    let reload = (PIT_FREQ_HZ * CALIBRATE_MS / 1000) as u16;

    // Gate channel 2 off (and mute the speaker) while programming the count.
    let gate = port_gate.read();
    port_gate.write(gate & !0x03);
    // Channel 2, lobyte/hibyte, mode 0 (interrupt on terminal count), binary.
    port_cmd.write(0b1011_0000);
    port_ch2.write((reload & 0xff) as u8);
    port_ch2.write((reload >> 8) as u8);

    let lapic = Apic::local_apic();
    lapic.set_timer_mode(TimerMode::OneShot);
    lapic.set_timer_initial(u32::MAX);

    let tsc_start = core::arch::x86_64::_rdtsc();
    // Raising the gate starts the countdown.
    port_gate.write((gate & !0x02) | 0x01);

    // OUT (bit 5) goes high on terminal count. Bound the loop so a missing
    // PIT (reads as 0xFF, or never toggling) cannot hang the boot.
    let mut spins: u64 = 0;
    loop {
        let status = port_gate.read();
        if status & 0x20 != 0 {
            break;
        }
        if status == 0xFF || spins > 100_000_000 {
            port_gate.write(gate & !0x03);
            lapic.set_timer_initial(0);
            return None;
        }
        spins += 1;
        core::hint::spin_loop();
    }
    let tsc_end = core::arch::x86_64::_rdtsc();
    let lapic_remaining = lapic.timer_current();
    lapic.set_timer_initial(0);
    port_gate.write(gate & !0x03);

    let lapic_elapsed = (u32::MAX - lapic_remaining) as u64;
    let lapic_hz = lapic_elapsed * 1000 / CALIBRATE_MS;
    let tsc_hz = (tsc_end - tsc_start) * 1000 / CALIBRATE_MS;

    // Sanity windows: LAPIC timers run 1 MHz - 5 GHz; TSCs 200 MHz - 10 GHz.
    if !(1_000_000..=5_000_000_000).contains(&lapic_hz)
        || !(200_000_000..=10_000_000_000).contains(&tsc_hz)
    {
        return None;
    }
    Some(PitCalibration { lapic_hz, tsc_hz })
}

/// CPUID leaf 0x15 TSC frequency, when the firmware/CPU report it exactly.
fn cpuid_tsc_hz() -> Option<u64> {
    raw_cpuid::CpuId::new()
        .get_tsc_info()
        .and_then(|t| t.tsc_frequency())
        .filter(|&hz| (200_000_000..=10_000_000_000).contains(&hz))
}

/// Calibrate the LAPIC timer and TSC on the BSP. Returns the LAPIC initial
/// count for one scheduler tick (`TICKS_PER_SEC`). Falls back to the old
/// cpu_frequency() heuristic when no reference clock is usable.
///
/// Must be called with the local APIC initialized and its timer disabled;
/// leaves the timer stopped (mode/initial are reprogrammed by the caller).
pub(super) fn calibrate_apic_timer_bsp() -> u32 {
    let ticks_per_sec = super::super::timer::TICKS_PER_SEC;
    if let Some(cal) = unsafe { pit_calibrate() } {
        // Prefer the architecturally exact CPUID 0x15 value for the TSC when
        // present; the PIT measurement then only drives the LAPIC count.
        let tsc_hz = cpuid_tsc_hz().unwrap_or(cal.tsc_hz);
        set_tsc_hz(tsc_hz);
        let initial = (cal.lapic_hz / ticks_per_sec).max(1) as u32;
        crate::klog_info!(
            "[timer] PIT calibration: LAPIC {} Hz, TSC {} Hz, tick initial={}",
            cal.lapic_hz,
            tsc_hz,
            initial
        );
        initial
    } else {
        if let Some(tsc_hz) = cpuid_tsc_hz() {
            set_tsc_hz(tsc_hz);
        }
        let initial = (super::cpu::cpu_frequency() as u64 * 1_000_000 / ticks_per_sec) as u32;
        crate::klog_warn!(
            "[timer] PIT unavailable — falling back to cpu_frequency() LAPIC count {}",
            initial
        );
        initial
    }
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
        let secs = days as u64 * 86_400
            + hour as u64 * 3_600
            + min as u64 * 60
            + sec as u64;
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
