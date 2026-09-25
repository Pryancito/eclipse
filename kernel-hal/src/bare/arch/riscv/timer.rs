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
