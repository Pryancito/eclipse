//! Kernel fault containment: an *oops* instead of a dead machine.
//!
//! A defect in a user program — a null dereference, a divide by zero, an
//! illegal instruction — already costs only that program: `handle_user_trap`
//! turns the CPU exception into the matching Linux signal and the process dies
//! alone. What still took the whole machine down was the other direction: the
//! *kernel* faulting while serving that program. A `panic!`, an `unwrap` on an
//! argument nobody expected, a page fault in a driver path — each of those left
//! the kernel spinning in `loop { spin_loop() }`, killing every other process
//! with it, including the shell the failure could have been diagnosed from.
//!
//! This module applies the policy Linux calls an *oops*: if the fault happened
//! while serving one particular task, that task dies and the kernel lives.
//!
//! # What it costs
//!
//! Abandoning a coroutine mid-flight does **not** unwind it: not one destructor
//! of the aborted call chain runs, so its heap allocations, its `Arc`
//! references and its 2 MiB stack are all lost. That is bounded leakage per
//! contained fault, which is why there is a budget ([`MAX_CONTAINED`]): a
//! kernel that spends its day containing faults is not healthy, and past that
//! point a halt somebody can diagnose beats silent degradation.
//!
//! # Isolation-first, and when it still declines
//!
//! The policy is to keep the kernel up and *name the culprit* even when we
//! cannot be certain who it is: gather a heuristic (the interrupted process, and
//! — if the fault is in a timer callback — the firing callback's identity),
//! print it, kill the guilty process, and carry on. Halting is the last resort,
//! reserved for the few conditions under which isolation is genuinely unsafe or
//! impossible:
//!
//! - the CPU holds any kernel lock (`lock::lock_depth() != 0`): it was in the
//!   middle of mutating shared state, and that lock would never be released.
//!   This is the one hard, non-negotiable gate;
//! - the fault is not on a coroutine stack at all (neither
//!   `current_task_abandonable()` nor `current_executor_abandonable()`): there
//!   is literally nothing to switch off of, so isolation is impossible. A fault
//!   *between* polls, on an executor's own stack, IS isolated — the executor is
//!   discarded and rebuilt even though no task can be blamed;
//! - the executor's soft stack canary is damaged: replacing its stack cannot
//!   make the neighbouring heap safe again, even on the idle or IST path;
//! - this same CPU was already containing another fault: if the recovery
//!   machinery is what broke, there is nothing left to rescue;
//! - the fault budget is exhausted.
//!
//! Two conditions that USED to halt no longer do — they are now clues in the
//! report, not verdicts. A fault *in a timer callback* is isolated (the timer
//! EOI is sent before the callback runs, so abandoning it does not wedge this
//! CPU's timer); the interrupted process is **not** killed — it is coincidental
//! — and the firing callback is printed for when the true origin is whoever
//! armed it. A *suspected heap smash* is likewise isolated rather than halted —
//! that is exactly when staying up to name the culprit is most valuable, and
//! the leak is bounded by [`MAX_CONTAINED`].
//!
//! The `lock_depth` gate is necessary, not sufficient: a handful of scheduler
//! and console locks are external `spin::Mutex`es that do not count towards it.
//! The case that matters — the kernel objects (process, thread, VMAR, files) —
//! does use `lock::Mutex` and so is covered.
//!
//! `PANICONOOPS=1` on the kernel command line turns all of this off and
//! restores the previous behaviour (halt on the first fault), which is what one
//! wants while debugging a specific failure: the machine freezes on the spot.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use kernel_hal::console::{serial_write_fmt_spin, serial_write_str};

/// Print to the serial console AND append to the in-memory oops record that
/// `/proc/oops` exposes (drained to `/var/log/oops.log` by userspace).
///
/// The serial line is what a developer watching the console sees; the record is
/// what survives for everyone else, which matters precisely because a contained
/// fault leaves the machine RUNNING and the console scrolling on past it.
macro_rules! oops_report {
    ($($arg:tt)*) => {{
        serial_write_fmt_spin(format_args!($($arg)*));
        kernel_hal::oops_log::record(format_args!($($arg)*));
    }};
}
use zircon_object::object::KernelObject;
use zircon_object::task::Thread;

/// How many faults to contain before giving up and halting.
///
/// Not a resource limit (each leak is a couple of MiB out of a heap of
/// hundreds) but a credibility one: past this count the kernel is failing
/// systematically, and what helps then is a halt with its dump, not more
/// running.
const MAX_CONTAINED: u32 = 16;

/// Whether isolation should `kill -9` the thread that was current at the
/// fault. A timer callback runs on whoever the IRQ interrupted; that pid
/// is coincidental (`timer_cb={0,0}` in the capture that killed pid 1036).
fn should_kill_interrupted_pid(in_timer: bool) -> bool {
    !in_timer
}

/// Exit code for the victim: killed by SIGKILL, in the form the process
/// object carries a death by signal (`-signo`, see
/// `linux_object::process::exit_code_killed_by`; `linux-object` is optional
/// here, so the value is spelled out). The parent's `wait4` then reads it as
/// `WIFSIGNALED`/`WTERMSIG == 9`, and the shell's `$?` as 137. It used to be
/// the literal `128 + 9`, which `wait4` reported as an ordinary `exit(137)`.
const KILLED_BY_KERNEL: i64 = -9;

/// Faults contained since boot.
static CONTAINED: AtomicU32 = AtomicU32::new(0);

/// `PANICONOOPS=1`: contain nothing, halt as before.
static PANIC_ON_OOPS: AtomicBool = AtomicBool::new(false);

/// Mask of CPUs currently containing a fault, so a fault *inside* the recovery
/// path is recognised as such. One bit per logical CPU; the system's maximum
/// (`lock::MAX_CORE_NUM` = 64) is exactly what fits here.
static CONTAINING: AtomicU64 = AtomicU64::new(0);

/// Enable/disable halting on the first fault (`PANICONOOPS=1`).
pub fn set_panic_on_oops(enabled: bool) {
    PANIC_ON_OOPS.store(enabled, Ordering::SeqCst);
}

/// How many kernel faults have been contained since boot.
pub fn contained_count() -> u32 {
    CONTAINED.load(Ordering::Relaxed)
}

/// Take one slot out of the fault budget, or `None` when it is spent.
///
/// The count used to be a bare `fetch_add` read back against
/// [`MAX_CONTAINED`], which meant the increment happened whether or not the
/// fault was then contained. Two things followed. The budget check saw its own
/// increment, so the refusal came one fault after the limit rather than at it;
/// and every later fault kept incrementing a counter nobody would decrement, so
/// `contained_count` -- which is read back out of the kernel as *contained*
/// faults -- grew without bound past a limit it was supposed to be capped by.
/// A number that says 40 faults were contained out of a budget of 16 is not a
/// diagnostic.
///
/// A `compare_exchange` loop instead: the slot is claimed only when one is
/// there, so the counter never passes [`MAX_CONTAINED`] and counts exactly the
/// faults this kernel survived.
fn claim_fault_budget() -> Option<u32> {
    let mut cur = CONTAINED.load(Ordering::SeqCst);
    loop {
        if cur >= MAX_CONTAINED {
            return None;
        }
        match CONTAINED.compare_exchange(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(cur + 1),
            Err(seen) => cur = seen,
        }
    }
}

/// Try to contain the fault `what` by killing only whoever caused it.
///
/// **Does not return** if it succeeds: the CPU goes back to the scheduler and
/// keeps running the rest of the system. If it returns, containment was
/// declined and the caller must halt the machine as it did before; the reason
/// is already on the console.
///
/// `what` is the short label of the fault (`"kernel panic"`,
/// `"null-range #PF"`) that appears in the report.
///
/// `restore_kd` is the console mode the fault path forced to text so its dump
/// would be visible. Containing means putting it back, or a live compositor
/// would be left rendering into a buffer that is no longer presented and the
/// desktop would look frozen. `None` if the caller did not touch it.
///
/// Must be called with interrupts already disabled, from the fault path, having
/// touched nothing on the way.
pub fn try_contain(what: &str, restore_kd: Option<u32>) {
    if PANIC_ON_OOPS.load(Ordering::Relaxed) {
        serial_write_fmt_spin(format_args!(
            "\n[oops] {} NOT contained: PANICONOOPS=1 — halting\n",
            what
        ));
        return;
    }

    let cpu = lock::current_cpu_id_via_apic() as usize;
    if cpu >= 64 {
        serial_write_str("\n[oops] cpu id out of range — halting\n");
        return;
    }
    let bit = 1u64 << cpu;
    if CONTAINING.fetch_or(bit, Ordering::SeqCst) & bit != 0 {
        // Recovery itself faulted. No `format_args!` — if what broke was the
        // heap, formatting is exactly what faults again right here.
        serial_write_str("\n[oops] fault WHILE containing a fault — halting\n");
        return;
    }

    // Every refusal states its reason: that is the difference between "the
    // machine halted" and "the machine halted because the fault arrived with
    // two locks held", which is what says where to look next.
    let decline = |reason: core::fmt::Arguments| {
        oops_report!("\n[oops] {} NOT contained ({}) — halting\n", what, reason);
        CONTAINING.fetch_and(!bit, Ordering::SeqCst);
    };

    // HARD SAFETY GATE — the one condition that is never negotiable. A kernel
    // object lock held here (process/thread/VMAR/file `lock::Mutex`) would never
    // be released if we abandoned the coroutine, wedging every later waiter with
    // interrupts off. Halting is strictly better than that.
    let depth = lock::lock_depth();
    if depth != 0 {
        return decline(format_args!("the CPU holds {} kernel lock(s)", depth));
    }

    // ── Heuristic attribution ────────────────────────────────────────────────
    //
    // Isolation-first policy: gather every signal about WHO faulted and WHAT
    // they were doing, print it, and then isolate — kill the guilty process and
    // keep the kernel up — rather than halt. `in_timer_callback` and
    // `heap_smash_suspected` used to halt here; they are now just clues in the
    // report, not verdicts:
    //   * the timer EOI is sent BEFORE the callback runs (x86_apic::handle_irq),
    //     so abandoning from a timer callback does NOT leave the IRQ un-acked —
    //     this CPU's timer keeps firing;
    //   * a suspected heap smash is exactly WHEN we most want to stay up and
    //     name the culprit; the leak is bounded by `MAX_CONTAINED`.
    // The interrupted thread is the best-effort culprit. When the fault is in a
    // timer callback the true origin may instead be whoever armed that callback,
    // so the firing callback's `{data,vtable}` is printed too (symbolize the
    // vtable to get the closure's type/arming site).
    let in_timer = kernel_hal::timer::in_timer_callback();
    let smashed = executor::heap_smash_suspected();
    let (cb_data, cb_vtable) = kernel_hal::kstats::current_timer_cb();
    // Where the interrupted code was at the last timer tick — best-effort "what
    // it was doing". A low RIP is userspace (the thread's own code); a high one
    // is kernel code. Symbolize with addr2line against the ELF.
    let tick_rip = kernel_hal::kstats::current_cpu_tick_rip();
    let victim = kernel_hal::thread::get_current_thread()
        .and_then(|thread| thread.downcast::<Thread>().ok());

    // A per-CPU block pointer this CPU refused to dereference. Zero on a
    // healthy machine — and when it is not, it names the author of everything
    // printed above: the GS region was written by something that is not
    // bring-up, which is how "heap corruption with no culprit" starts.
    #[cfg(not(feature = "libos"))]
    {
        let (refused, last) = kernel_hal::percpu::bogus_percpu_ptrs();
        if refused != 0 {
            oops_report!(
                "\n[isolate] {} per-CPU block pointer(s) refused, last {:#x} — \
                 this CPU's GS region held something bring-up never published\n",
                refused,
                last,
            );
        }
    }

    // Printed BEFORE any kill/abandon so the heuristic survives even if the
    // corrupt heap re-faults the isolation path (the re-entrancy guard then
    // halts, but this line already escaped).
    match &victim {
        // Deliberately NO process NAME here. The name is a `String` in the very
        // heap that is smashed; formatting a corrupt one (invalid length /
        // non-UTF-8, e.g. `\u{1fffc0}`) sent `{:?}` into a wild read that HUNG
        // the isolation mid-print. `pid`/`tid` are plain integers read from the
        // object structs — safe to print. The pid is the reliable identifier;
        // map it to a name from the `[eclipse-init] respawn:` log if needed.
        Some(thread) => oops_report!(
            "\n[isolate] {} — culprit heuristic: pid={} tid={} \
             (in_timer_callback={} heap_smash={}); last_tick_rip={:#x} \
             timer_cb={{data:{:#x} vtable:{:#x}}}{}\n",
            what,
            thread.proc().id(),
            thread.id(),
            in_timer,
            smashed,
            tick_rip,
            cb_data,
            cb_vtable,
            if in_timer {
                " — fault in a timer callback: the interrupted pid may be \
                 coincidental; symbolize timer_cb vtable for the real origin"
            } else {
                ""
            },
        ),
        None => oops_report!(
            "\n[isolate] {} — no current thread (IRQ/idle/kernel coroutine); \
             (in_timer_callback={} heap_smash={}) last_tick_rip={:#x} \
             timer_cb={{data:{:#x} vtable:{:#x}}}\n",
            what,
            in_timer,
            smashed,
            tick_rip,
            cb_data,
            cb_vtable,
        ),
    }

    // FEASIBILITY GATE — isolation needs the fault to be standing on a
    // coroutine stack we can switch off. Two shapes qualify:
    //
    //  * a fault INSIDE a poll: retire that task (`abandon_current_task`);
    //  * a fault BETWEEN polls, on an executor's own stack: there is no task to
    //    blame, but the executor itself can be discarded and rebuilt
    //    (`abandon_current_executor`). This is the idle/scheduler case — an IRQ
    //    landing on the idle path, or a corrupted return slot in the scheduler's
    //    own frames — and it used to halt the machine for want of a victim even
    //    though nothing was in flight and the core was perfectly recoverable.
    let task_path = executor::current_task_abandonable();
    let executor_path = !task_path && executor::current_executor_abandonable();
    // Third shape: the fault was delivered on a DIFFERENT stack than the one
    // that died — a #DF or #GP arrives on its own IST stack, so the current SP
    // says nothing about which coroutine failed. The arch trap entry stashed
    // the active faulting frame, so ask which executor owns THAT stack. Without this a
    // double fault always halted the machine, which is exactly how the last
    // surviving crash ended.
    let fault_sp = kernel_hal::kstats::current_fault_rsp().unwrap_or(0) as usize;
    let ist_path =
        !task_path && !executor_path && fault_sp != 0 && executor::fault_sp_abandonable(fault_sp);
    if !task_path && !executor_path && !ist_path {
        return decline(format_args!(
            "fault is not on an abandonable coroutine stack (current or \
             faulting sp {:#x}) — cannot isolate",
            fault_sp
        ));
    }

    let Some(n) = claim_fault_budget() else {
        return decline(format_args!(
            "budget exhausted ({} faults already contained)",
            MAX_CONTAINED
        ));
    };

    match &victim {
        Some(thread) if !should_kill_interrupted_pid(in_timer) => {
            // The timer interrupted whoever was on this CPU. Killing them
            // treats a coincidental pid as the author of a smash inside
            // `timer_tick` (`timer_cb={0,0}` in the live capture).
            let report = format_args!(
                "[isolate] {} contained ({}/{}): not killing pid={} tid={} \
                 (fault in a timer callback; the interrupted pid is \
                 coincidental) — the kernel stays up\n",
                what,
                n,
                MAX_CONTAINED,
                thread.proc().id(),
                thread.id(),
            );
            serial_write_fmt_spin(report);
            kernel_hal::oops_log::record(report);
        }
        Some(thread) => {
            let report = format_args!(
                "[isolate] {} contained ({}/{}): killing pid={} tid={} — \
                 the kernel stays up\n",
                what,
                n,
                MAX_CONTAINED,
                thread.proc().id(),
                thread.id(),
            );
            serial_write_fmt_spin(report);
            kernel_hal::oops_log::record(report);
            // Serial ONLY. The graphic console writes through a `dyn
            // DisplayScheme` trait object, and dispatching through a fat pointer
            // whose vtable the smash may have zeroed is exactly the null-vtable
            // #PF this path is trying to survive — it would re-fault here and turn
            // a contained fault into a halt. The serial line above is the record.
            kill(thread);
        }
        None => {
            // No process to blame: the coroutine was the kernel's own (net
            // polling, a driver's deferred work...). Retired anyway — halting is
            // strictly worse — but say so loudly, since the symptom will be a
            // subsystem that quietly stops responding with nobody dead to explain
            // it.
            oops_report!(
                "[isolate] {} contained ({}/{}): kernel coroutine retired; the \
                 subsystem it served may stay dead until reboot\n",
                what,
                n,
                MAX_CONTAINED,
            );
        }
    }

    // Give the console back to whoever had it. The fault path forced it to text
    // so its dump would show; if the system is going to keep running, the
    // compositor has to own the screen again on its next frame.
    if let Some(mode) = restore_kd {
        kernel_hal::console::set_kd_mode(mode);
    }

    // Containment is about to succeed, so whatever stop screen the fault path
    // painted on its way here is void: the machine keeps running, the
    // compositor repaints over it, and a LATER stop must be free to claim the
    // screen from the top again rather than append under a report nobody can
    // see any more. (The stop screen is cumulative so that a panic, a kernel
    // #PF and the deadlock detector all fit in one photograph -- see
    // `kernel_hal::stop_screen`.)
    kernel_hal::stop_screen::reset();

    // The delicate part is done and only the context switch is left, so release
    // the re-entrancy guard: a later fault on this CPU should get its own
    // chance to be contained.
    CONTAINING.fetch_and(!bit, Ordering::SeqCst);

    // And the panic reporter's own guard, for the same reason and at the same
    // moment: nothing of the panic being contained runs after the call below,
    // so a later panic -- minutes on, about something else -- must get the full
    // report rather than the one line a report still in flight would earn it.
    // This is the only place it may be released, and it has to be *here*: a
    // release any earlier leaves a fault inside containment reading as a first
    // panic, and the handler would repaint over the very report it is about to
    // contain. See `crate::lang::release_panic_guard`.
    crate::lang::release_panic_guard();

    // SAFETY: we are on the fault path, standing on the coroutine's own stack
    // (proven just above, or on an IST with current trap provenance), with
    // interrupts disabled and no kernel lock held
    // (checked above). Nothing from the abandoned call chain is touched again.
    // Cleanup runs only after retirement succeeds. A skipped indirect timer
    // call returns normally and must keep its nesting depth; abandoning this
    // complete chain, including nested IRQs, will never run any of those exits.
    unsafe {
        executor::abandon_executor_chain(ist_path.then_some(fault_sp), || {
            kernel_hal::timer::abandon_timer_callbacks();
            kernel_hal::kstats::abandon_fault_scopes();
        });
    }
    serial_write_str("\n[oops] could not abandon the coroutine — halting\n");
}

/// Kill the process owning the faulted thread and take the thread off its list.
fn kill(thread: &alloc::sync::Arc<Thread>) {
    // Same as `CurrentThread::drop`: leave the dying process's page table
    // *before* its address space is torn down, so we are not left executing on
    // a CR3 whose tables are being freed.
    kernel_hal::vm::activate_kernel_paging();
    // This CPU's current thread points at the one just given up for dead. The
    // next coroutine to run here will set its own, but until then nobody should
    // find it.
    kernel_hal::thread::set_current_thread(None);

    let proc = thread.proc();
    // The same path a `kill -9` takes: publish the exit status, wake the
    // parent's `wait4`, and tell the process's other threads to die.
    proc.exit(KILLED_BY_KERNEL);
    // This thread's `CurrentThread` leaked with the abandoned coroutine, so its
    // `Drop` will never take it off the process's thread list. Doing it here is
    // what lets the process actually terminate and release its address space
    // when this was its last thread.
    thread.terminate_abandoned();
}

/// The fault-containment policy, on the host.
///
/// This module is `#[cfg(not(feature = "libos"))]` in the kernel build and
/// `cargo test -p zcore` runs with `--features libos`, so the code that decides
/// whether a kernel fault kills one process or the whole machine had never been
/// compiled by a test binary. `try_contain` itself cannot run here -- it reads
/// the local APIC's cpu id, the lock depth and the executor's stacks, and does
/// not return when it succeeds -- but the budget it spends and the exit code it
/// hands the victim are plain values, and both had a defect.
#[cfg(test)]
mod tests {
    use super::*;

    /// One test at a time through `CONTAINED`, a process-wide static that every
    /// test would otherwise believe is its own, saved and restored around the
    /// body so the count is left as it was found.
    fn alone_with_the_budget(body: impl FnOnce()) {
        extern crate std;
        use std::sync::Mutex;
        static TURNSTILE: Mutex<()> = Mutex::new(());
        let _guard = TURNSTILE.lock().unwrap_or_else(|e| e.into_inner());
        let saved = CONTAINED.load(Ordering::SeqCst);
        CONTAINED.store(0, Ordering::SeqCst);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        CONTAINED.store(saved, Ordering::SeqCst);
        if let Err(e) = r {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn the_budget_hands_out_exactly_the_faults_it_promises() {
        alone_with_the_budget(|| {
            for n in 1..=MAX_CONTAINED {
                assert_eq!(claim_fault_budget(), Some(n), "fault {n}");
            }
            assert_eq!(claim_fault_budget(), None, "the {}th", MAX_CONTAINED + 1);
        });
    }

    /// The defect: the count is read back out of the kernel as *contained*
    /// faults, and a `fetch_add` before the check kept moving it on every fault
    /// after the budget was spent. It reported more faults contained than the
    /// budget allows -- and it never stopped climbing.
    #[test]
    fn a_refused_fault_does_not_count_itself_as_contained() {
        alone_with_the_budget(|| {
            while claim_fault_budget().is_some() {}
            assert_eq!(contained_count(), MAX_CONTAINED);
            for _ in 0..50 {
                assert_eq!(claim_fault_budget(), None);
            }
            assert_eq!(
                contained_count(),
                MAX_CONTAINED,
                "declined faults were counted as contained"
            );
        });
    }

    /// The count is what `contained_count` publishes, so the two must be the
    /// same number and not merely agree at zero.
    #[test]
    fn the_published_count_is_the_number_of_faults_survived() {
        alone_with_the_budget(|| {
            assert_eq!(contained_count(), 0);
            claim_fault_budget().unwrap();
            claim_fault_budget().unwrap();
            assert_eq!(contained_count(), 2);
        });
    }

    /// The exit code the victim's process carries. It used to be the literal
    /// `128 + 9`, which `wait4` reports as an ordinary `exit(137)` rather than a
    /// death by signal, so a parent testing `WIFSIGNALED` was told its child
    /// exited normally. The value is spelled out here because `linux-object` is
    /// an optional dependency of this crate; where it IS linked, this test is
    /// what stops the two spellings from drifting.
    #[test]
    #[cfg(feature = "linux")]
    fn the_victim_dies_the_way_a_kill_9_does() {
        assert_eq!(
            KILLED_BY_KERNEL,
            linux_object::process::exit_code_killed_by(9)
        );
    }

    /// Without `linux-object` the constant can still be pinned to what `wait4`
    /// needs: negative, and the signal number rather than `128 + signo`.
    #[test]
    fn the_victims_exit_code_is_a_signal_death_and_not_an_exit_status() {
        assert_eq!(KILLED_BY_KERNEL, -9);
        assert_ne!(KILLED_BY_KERNEL, 128 + 9);
    }

    /// Isolation must not treat the interrupted pid as the author of a timer
    /// callback smash. The live capture killed pid=1036 while
    /// `in_timer_callback=true` and `timer_cb={0,0}`.
    #[test]
    fn a_timer_fault_does_not_blame_the_interrupted_pid() {
        assert!(!should_kill_interrupted_pid(true));
        assert!(should_kill_interrupted_pid(false));
    }

    /// `CONTAINING` is a bit per CPU in a `u64`, and `try_contain` refuses a cpu
    /// id it cannot represent rather than shifting out of range. 64 is the
    /// system's maximum, so the guard is at the edge of what fits, not inside
    /// it.
    #[test]
    fn every_cpu_the_guard_admits_has_a_bit_of_its_own() {
        let mut seen = 0u64;
        for cpu in 0..64usize {
            let bit = 1u64 << cpu;
            assert_eq!(seen & bit, 0, "cpu {cpu} shares a bit");
            seen |= bit;
        }
        assert_eq!(seen, u64::MAX);
        assert_eq!(lock::MAX_CORE_NUM, 64, "the mask is sized to this");
    }
}
