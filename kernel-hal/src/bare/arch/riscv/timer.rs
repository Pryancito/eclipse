use core::convert::TryFrom;
use core::time::Duration;

fn get_cycle() -> u64 {
    riscv::register::time::read() as u64
}

pub(super) fn timer_set_next() {
    let cycles = crate::deadline::ticks_per_period(
        super::cpu::timebase_hz(),
        super::super::timer::TICKS_PER_SEC,
    );
    sbi_rt::set_timer(get_cycle() + cycles);
}

pub(super) fn init() {
    timer_set_next();
}

pub(crate) fn timer_now() -> Duration {
    crate::deadline::ticks_to_duration(get_cycle(), super::cpu::timebase_hz())
}

/// The scale the Zircon vDSO applies to `rdtime` to get the monotonic
/// nanoseconds the kernel reports, as the `(numerator, denominator)` pair it
/// divides by, plus the counter's frequency for `zx_ticks_per_second`.
///
/// The kernel's own clock is `ticks_to_duration(time, timebase_hz)`, which is
/// `floor(time * 1e9 / hz)` with no base subtracted: the `time` CSR counts
/// from reset and is the same on every hart. `1e9 / hz` reduced by its gcd
/// is the same rational, so the vDSO's `floor((time + 0) * num / den)` is the
/// same number for every `time` -- userspace and kernel on one clock, which
/// is what every deadline comparison in the core tests assumes.
///
/// `None` when the reduced pair does not fit the vDSO's two `u32` fields,
/// which no timebase in use comes near (QEMU's 10 MHz is `100 / 1`); the
/// caller then leaves the page as the blob shipped it, as before.
pub fn vdso_time_ratio() -> Option<(u32, u32, u64)> {
    let hz = super::cpu::timebase_hz();
    if hz == 0 {
        return None;
    }
    let g = gcd(1_000_000_000, hz);
    let numerator = u32::try_from(1_000_000_000 / g).ok()?;
    let denominator = u32::try_from(hz / g).ok()?;
    Some((numerator, denominator, hz))
}

/// Greatest common divisor, for reducing the ratio above.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}
