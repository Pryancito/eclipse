use core::arch::global_asm;

mod context;

pub use context::*;

global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

extern "C" {
    pub fn switch(old: *const ContextData, new: *const ContextData);
    pub fn executor_entry();
}

pub(crate) fn cpu_id() -> u8 {
    // Dense logical id via hart-id table populated during SMP bring-up.
    lock::current_cpu_id()
}

pub(crate) fn pg_base_addr() -> usize {
    riscv::register::satp::read().ppn() << 12
}

pub(crate) fn pg_base_register() -> usize {
    riscv::register::satp::read().bits()
}

use riscv::{asm, register::sstatus};

/// Park this hart until an interrupt arrives, leaving `sstatus.SIE` exactly as
/// the caller left it.
///
/// The caller's halt protocol (`Executor::run`) is: interrupts off, publish
/// "this CPU is sleeping", re-check the run queue, and only then come here.
/// Its whole purpose is that nothing can slip between the re-check and the
/// stall — a remote waker either sees the sleeping bit and sends the
/// reschedule IPI, or its notify is ordered before the re-check and we never
/// halt.
///
/// Enabling interrupts before the `wfi` broke exactly that, which is what the
/// FIXME this replaces was about: an IPI arriving between `set_sie` and `wfi`
/// is taken and retired **there**, and the `wfi` that follows has nothing left
/// pending to wake it. The CPU that was just told there is work then sleeps
/// until the next timer tick anyway — 250 Hz, so up to 4 ms — which is the
/// same cost, on the receiving side, as an architecture with no wake IPI at
/// all.
///
/// `wfi` does not need interrupts enabled. The privileged spec requires its
/// operation to be "unaffected by the global interrupt bits in mstatus" and to
/// honour only the individual enables, so a pending, individually-enabled
/// interrupt resumes the hart with `SIE` clear. Stall first, then open the
/// window in which the trap can be taken.
pub(crate) fn wait_for_interrupt() {
    let enable = sstatus::read().sie();
    unsafe {
        asm::wfi();
    }
    if !enable {
        // The hart is awake and the trap is pending; this is the instant it
        // gets taken. Then the caller has its interrupts-off state back.
        unsafe { sstatus::set_sie() };
        unsafe { sstatus::clear_sie() };
    }
}

pub(crate) fn intr_on() {
    unsafe { sstatus::set_sie() };
}

pub(crate) fn intr_off() {
    unsafe { sstatus::clear_sie() };
}

pub(crate) fn intr_get() -> bool {
    sstatus::read().sie()
}

// pub(crate) fn is_handling_intr() -> bool {
//     sstatus::read().spp() == sstatus::SPP::Supervisor
// }
