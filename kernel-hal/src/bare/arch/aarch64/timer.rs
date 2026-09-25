//! ARM Generic Timer.

use crate::timer::TICKS_PER_SEC;
use core::time::Duration;
use cortex_a::{asm::barrier, registers::*};
use tock_registers::interfaces::{Readable, Writeable};

pub fn timer_now() -> Duration {
    unsafe { barrier::isb(barrier::SY) }
    // Fuchsia's vDSO reads CNTVCT_EL0 directly. Use the same counter in the
    // kernel so deadlines and vDSO timestamps share one time base.
    //
    // Through `ticks_to_duration` rather than `count * 1_000_000_000 / freq`,
    // which is the spelling this had and which wraps a `u64` at
    // `u64::MAX / 1_000_000_000` counts — 18.4 billion of them. On QEMU's
    // `virt` (CNTFRQ 62.5 MHz) that is **295 seconds of uptime**, after which
    // the monotonic clock jumps backwards and every deadline taken from it is
    // nonsense. x86_64 fixed the same overflow in its own copy and says so at
    // `TSC_NS_MULT`; this copy and riscv's were the ones that comment did not
    // reach.
    crate::deadline::ticks_to_duration(CNTVCT_EL0.get(), CNTFRQ_EL0.get())
}

pub fn set_next_trigger() {
    // Never zero: a tick period of zero counts is a timer already due the
    // instant it is armed, i.e. an interrupt that re-arms itself as fast as
    // the PE can take it.
    CNTP_TVAL_EL0.set(crate::deadline::ticks_per_period(
        CNTFRQ_EL0.get(),
        TICKS_PER_SEC,
    ));
}

pub fn init() {
    CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::SET);
    set_next_trigger();
}
