use core::arch::{asm, global_asm};

mod context;

pub use context::*;

global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

extern "C" {
    pub fn switch(old: *const ContextData, new: *const ContextData);
    pub fn executor_entry();
}

/// [null-exec guard] Written by `switch.S`'s last-instant dead-frame check:
/// bumped once per bounced restore, with the dead frame's rsp in
/// `SWITCH_BOUNCE_SP`. A bounce means the target frame's resume-rip (or cr3)
/// slot was ZERO *after* `executor_frame_resumable` had validated it — the
/// corruption (or double consumption) happened inside the validation->ret
/// window, which no earlier probe could see. Read via
/// [`switch_bounce_snapshot`].
#[no_mangle]
static SWITCH_BOUNCE_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[no_mangle]
static SWITCH_BOUNCE_SP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// (bounce_count, last dead frame sp). Cross-CPU racy by design — diagnostics.
pub fn switch_bounce_snapshot() -> (u64, u64) {
    use core::sync::atomic::Ordering::Relaxed;
    (
        SWITCH_BOUNCE_COUNT.load(Relaxed),
        SWITCH_BOUNCE_SP.load(Relaxed),
    )
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
    // SAFETY: reads rsp and nothing else.
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) sp, options(nostack, nomem, preserves_flags));
    }
    sp
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

/// Host stand-in for the symbol above.
///
/// `cargo test -p executor` links this crate on its own, with no `kernel-hal`
/// to provide the real one, and the linker demands the symbol as soon as any
/// test pulls in a path that so much as mentions `wait_for_interrupt` — which
/// is most of `Executor::run` and everything that reaches the run queue
/// through it. Without this the whole suite fails to link with `undefined
/// symbol: hal_cpu_idle`, whatever the test was actually about. Nothing on
/// the host ever parks a CPU, so an empty body is the honest one; `drivers`
/// carries the same kind of shim for the same reason.
/// Exported under the C name rather than declared as another `hal_cpu_idle`,
/// which would collide with the `extern` declaration above.
#[cfg(test)]
#[export_name = "hal_cpu_idle"]
extern "C" fn hal_cpu_idle_host_shim() {}

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
