use core::arch::global_asm;
// Solo lo usa la lectura de `cr3`, que en una compilacion de test la
// sustituye el valor de anfitrion de mas abajo.
#[cfg(not(test))]
use core::arch::asm;

mod context;

pub use context::*;

global_asm!(include_str!("switch.S"));
global_asm!(include_str!("executor_entry.S"));

unsafe extern "C" {
    pub fn switch(old: *mut usize, new: *const usize);
    pub fn executor_entry();
}

/// [null-exec guard] Written by `switch.S`'s last-instant dead-frame check:
/// bumped once per bounced restore, with the dead frame's rsp in
/// `SWITCH_BOUNCE_SP`. A bounce means the target frame's resume-rip (or cr3)
/// slot was ZERO *after* `executor_frame_resumable` had validated it — the
/// corruption (or double consumption) happened inside the validation->ret
/// window, which no earlier probe could see. Read via
/// [`switch_bounce_snapshot`].
#[unsafe(no_mangle)]
static SWITCH_BOUNCE_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[unsafe(no_mangle)]
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

#[cfg(test)]
pub(crate) fn cpu_id() -> u8 {
    HOST_CPU_ID.with(core::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn set_cpu_id_for_test(cpu: u8) -> u8 {
    HOST_CPU_ID.with(|id| id.replace(cpu))
}

#[cfg(not(test))]
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

#[cfg(not(test))]
pub(crate) fn pg_base_register() -> usize {
    let mut cr3;
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    cr3
}

/// Host stand-in for the read above.
///
/// Same reason as `hal_cpu_idle_host_shim` and the `cli`/`sti` pair further
/// down: reading CR3 is privileged, so in a user-mode test process it is a
/// #GP -- SIGSEGV before the test reports anything (diagnosed 11-oct-2026,
/// see the note in `executor/stack_birth_benches.rs`). It is the one
/// instruction that kept `Executor::new` out of reach of `cargo test`, and
/// with it `ExecutorRuntime::new`, the stack registry, the saved-frame
/// validation and every caller of the three.
///
/// A fixed, page-aligned, non-zero value is the honest stand-in. Every reader
/// of this number either stores it in a `ContextData` for `switch.S` to load
/// -- and nothing on the host ever switches -- or compares it against zero to
/// spot a frame that was written over, and that reader wants "some address
/// space" and not "none".
#[cfg(test)]
pub(crate) fn pg_base_register() -> usize {
    HOST_PG_BASE
}

/// What [`pg_base_register`] reports in a test build.
#[cfg(test)]
pub(crate) const HOST_PG_BASE: usize = 0x1000;

#[cfg(not(test))]
use x86_64::instructions::interrupts;

unsafe extern "C" {
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

// Host stand-ins, same reason as `hal_cpu_idle_host_shim`: `cli`/`sti` are
// privileged and SIGSEGV a user-mode test process. Nothing on the host has
// interrupts to mask; model IF per host thread to exercise save/restore without
// touching privileged RFLAGS.IF.
#[cfg(test)]
std::thread_local! {
    static HOST_INTR_ENABLED: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
    static HOST_CPU_ID: core::cell::Cell<u8> = const { core::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn intr_on() {
    HOST_INTR_ENABLED.with(|enabled| enabled.set(true));
}
#[cfg(test)]
pub(crate) fn intr_off() {
    HOST_INTR_ENABLED.with(|enabled| enabled.set(false));
}
#[cfg(test)]
pub(crate) fn intr_get() -> bool {
    HOST_INTR_ENABLED.with(core::cell::Cell::get)
}

#[cfg(not(test))]
pub(crate) fn intr_on() {
    interrupts::enable();
}

#[cfg(not(test))]
pub(crate) fn intr_off() {
    interrupts::disable();
}

#[cfg(not(test))]
pub(crate) fn intr_get() -> bool {
    interrupts::are_enabled()
}

#[cfg(test)]
mod host_state_tests {
    /// La lectura de RSP detras del shim de arquitectura. Es la que usa
    /// `irq_should_skip_heavy_work` para decidir si queda pila: cuando era
    /// ensamblador en linea dentro de esa funcion, en riscv64 y aarch64 la
    /// guarda simplemente no existia.
    #[test]
    fn the_stack_pointer_shim_reads_the_caller_s_own_frame() {
        let local = 0u64;
        let sp = super::stack_pointer();
        let addr = &raw const local as usize;
        assert_ne!(sp, 0);
        assert_eq!(sp % 8, 0, "RSP tiene que estar alineado a palabra");
        assert!(
            addr >= sp && addr - sp < 4096,
            "un local de este marco ({:#x}) tiene que caer justo por encima del \
             RSP leido ({:#x})",
            addr,
            sp
        );
        // Y dos lecturas del mismo marco dan lo mismo: si no, lo que devuelve
        // no es el RSP sino algo que el compilador mueve.
        assert_eq!(sp, super::stack_pointer());
    }

    /// El contador de rebotes de `switch.S`: lo escribe el ensamblador por su
    /// nombre sin adornar, y lo lee el informe de `[switch-bounce]`. Lo que se
    /// fija aqui es que la pareja se lea entera y por su sitio -- un cero en
    /// un arranque limpio, no basura de otra variable.
    #[test]
    fn the_switch_bounce_counters_read_back_as_a_pair() {
        use core::sync::atomic::Ordering::Relaxed;
        let (count, sp) = super::switch_bounce_snapshot();
        assert_eq!(
            (count, sp),
            (0, 0),
            "en el anfitrion no hay `switch.S` que rebote nada"
        );

        super::SWITCH_BOUNCE_COUNT.store(3, Relaxed);
        super::SWITCH_BOUNCE_SP.store(0xdead_beef, Relaxed);
        assert_eq!(super::switch_bounce_snapshot(), (3, 0xdead_beef));

        super::SWITCH_BOUNCE_COUNT.store(count, Relaxed);
        super::SWITCH_BOUNCE_SP.store(sp, Relaxed);
    }

    #[test]
    fn cpu_identity_override_is_thread_local_and_restorable() {
        let previous = super::set_cpu_id_for_test(9);
        assert_eq!(super::cpu_id(), 9);
        std::thread::spawn(|| {
            assert_eq!(super::cpu_id(), 0);
            assert_eq!(super::set_cpu_id_for_test(3), 0);
            assert_eq!(super::cpu_id(), 3);
        })
        .join()
        .unwrap();
        assert_eq!(super::cpu_id(), 9);
        assert_eq!(super::set_cpu_id_for_test(previous), 9);
        assert_eq!(super::cpu_id(), previous);
    }
}
