use core::arch::global_asm;
use cortex_a::registers::*;
use tock_registers::interfaces::Readable;

mod context;

pub use context::*;

global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

extern "C" {
    pub fn switch(old: *const ContextData, new: *const ContextData);
    pub fn executor_entry();
}

/// This CPU's current stack pointer.
///
/// Behind an arch shim because it used to be inline `x86_64` assembly in
/// `irq_should_skip_heavy_work`, with `return false` for everything else — so
/// the guard that refuses to dispatch another `Box<dyn Fn>` on a nearly
/// exhausted coroutine stack simply did not exist on riscv64 and aarch64. The
/// compiler had been saying so on every build (`unreachable statement`).
#[inline(always)]
pub(crate) fn stack_pointer() -> usize {
    let sp: usize;
    // SAFETY: reads sp and nothing else.
    unsafe {
        core::arch::asm!("mov {}, sp", out(reg) sp, options(nostack, nomem, preserves_flags));
    }
    sp
}

pub(crate) fn cpu_id() -> u8 {
    // Dense logical id in TPIDR_EL1 (not sparse MPIDR Aff0).
    lock::current_cpu_id()
}

pub(crate) fn pg_base_addr() -> usize {
    TTBR0_EL1.get() as usize
}

pub(crate) fn pg_base_register() -> usize {
    TTBR0_EL1.get() as usize
}

/// Park this PE until an interrupt arrives, leaving `PSTATE.I` exactly as the
/// caller left it.
///
/// The caller's halt protocol (`Executor::run`) is: interrupts off, publish
/// "this CPU is sleeping", re-check the run queue, and only then come here.
/// Its whole purpose is that nothing can slip between the re-check and the
/// stall — a remote waker either sees the sleeping bit and sends the
/// reschedule IPI, or its notify is ordered before the re-check and we never
/// halt.
///
/// Unmasking before the `wfi` broke exactly that: an IPI arriving between the
/// `daifclr` and the `wfi` is taken and retired **there**, and the `wfi` that
/// follows has nothing left pending to wake it. The CPU that was just told
/// there is work then sleeps until the next timer tick anyway — 250 Hz, so up
/// to 4 ms — which is the same cost, on the receiving side, as an
/// architecture with no wake IPI at all.
///
/// `wfi` does not need the interrupt unmasked: its wake-up events are not
/// masked by `PSTATE.{I,F}`, so a pending physical IRQ takes the PE out of the
/// low-power state whatever `DAIF` says. Stall first, then open the window in
/// which the interrupt can be taken.
pub(crate) fn wait_for_interrupt() {
    let enable = intr_get();
    cortex_a::asm::wfi();
    if !enable {
        // The PE is awake and the IRQ is pending; this is the instant it gets
        // taken. Then the caller has its interrupts-off state back.
        intr_on();
        intr_off();
    }
}

pub(crate) fn intr_on() {
    unsafe {
        core::arch::asm!("msr daifclr, #2");
    }
}

pub(crate) fn intr_off() {
    unsafe {
        core::arch::asm!("msr daifset, #2");
    }
}

pub(crate) fn intr_get() -> bool {
    !DAIF.is_set(DAIF::I)
}
