use core::arch::{asm, global_asm};

mod context;

pub use context::*;

global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

extern "C" {
    pub fn switch(old: *const ContextData, new: *const ContextData);
    pub fn executor_entry();
}

pub(crate) fn cpu_id() -> u8 {
    // Dense logical id (0..NCPU), not the sparse Local APIC id — see `lock`.
    #[cfg(target_os = "none")]
    {
        lock::current_cpu_id()
    }
    // Hosted builds (libos) don't use this executor; async-std drives tasks.
    #[cfg(not(target_os = "none"))]
    {
        0
    }
}

// pub(crate) fn pg_base_addr() -> usize {
//     x86_64::registers::control::Cr3::read()
//         .0
//         .start_address()
//         .as_u64() as _
// }

pub(crate) fn pg_base_register() -> usize {
    let mut cr3;
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    cr3
}

use x86_64::instructions::interrupts;

extern "C" {
    /// Provided by `kernel-hal`: park the CPU until the next interrupt using the
    /// coolest available C-state (C1E via MONITOR/MWAIT, falling back to `hlt`)
    /// and account the idle time for `/proc/perf/kernel`. A bare `sti; hlt` here
    /// only reaches C1 and bypassed that power management, keeping the CPU
    /// warmer than necessary while idle.
    fn hal_cpu_idle();
}

pub(crate) fn wait_for_interrupt() {
    // `hal_cpu_idle` preserves the caller's interrupt-enable state itself, the
    // same contract as the previous `enable_and_hlt` + restore did.
    unsafe { hal_cpu_idle() }
}

pub(crate) fn intr_on() {
    interrupts::enable();
}

pub(crate) fn intr_off() {
    interrupts::disable();
}

pub(crate) fn intr_get() -> bool {
    interrupts::are_enabled()
}
