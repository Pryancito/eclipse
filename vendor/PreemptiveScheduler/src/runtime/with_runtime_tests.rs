//! The runtime probes, with a runtime behind them.
//!
//! [`super::probe_tests`] covers the other half: what each of these answers on
//! a CPU whose slot does not exist or is momentarily locked, which is the half
//! the real machine never reaches. This module is the one that was impossible
//! until `arch::pg_base_register` got a host stand-in, because every probe
//! here needs a live `Executor` with a real stack behind it, and building one
//! read CR3.
//!
//! All of them are called from hard-IRQ context or from the panic path, and
//! all of them are diagnostics whose job is to be right about a stack that is
//! about to be resumed, discarded or written past. A probe that says "yes"
//! without having looked sends a dead stack to be resumed; one that says "no"
//! when the fault was containable stops the machine for a task's bug.

use super::*;

/// A built runtime for this CPU, with a strong executor of its own, put back
/// the way it was found.
///
/// `GLOBAL_RUNTIME`'s slots are `Once`: once CPU 0's is built it exists for
/// the rest of the process, and `ExecutorRuntime` panics in its own `Drop` on
/// purpose, so there is no taking it away again. What this fixture does
/// instead is give every test a **fresh strong executor and no weak ones**, so
/// one test clobbering a canary cannot be read by the next -- the executors it
/// replaces are dropped properly, which is what returns their stacks to the
/// pool.
///
/// Both locks: `Executor::new` touches the stack pool, the live-stack
/// registries and the spine, and the ready mask these tests set is the same
/// per-CPU global the resched tests own.
pub(super) struct WithRuntime {
    _globals: std::sync::MutexGuard<'static, ()>,
    _resched: std::sync::MutexGuard<'static, ()>,
    saved_ready: u64,
}

impl Drop for WithRuntime {
    fn drop(&mut self) {
        {
            let mut rt = GLOBAL_RUNTIME.force(0).lock();
            rt.current_executor = None;
            rt.weak_executors.clear();
            let fresh = Arc::new(Executor::new(rt.task_collection.clone()));
            rt.strong_executor = fresh;
        }
        publish_weak_waiting(0);
        set_executor_ready_mask_for_test(self.saved_ready);
        crate::executor::wipe_stack_pool_for_test();
    }
}

/// `ready` is what `num_online_cpus` reads and what a stack retirement waits
/// for; `0` means "nobody else is online", which keeps every `Drop` in these
/// tests reclaiming its block instead of parking it.
fn with_runtime(ready: u64) -> WithRuntime {
    let globals = crate::executor::executor_globals_lock();
    let resched = resched_test_lock();
    let saved_ready = set_executor_ready_mask_for_test(ready);
    {
        let mut rt = GLOBAL_RUNTIME.force(0).lock();
        rt.current_executor = None;
        rt.weak_executors.clear();
        let fresh = Arc::new(Executor::new(rt.task_collection.clone()));
        rt.strong_executor = fresh;
    }
    publish_weak_waiting(0);
    WithRuntime {
        _globals: globals,
        _resched: resched,
        saved_ready,
    }
}

/// Clobber the soft-guard canary of the stack whose usable region starts at
/// `base`, which is what a kernel call chain running off the bottom of a
/// coroutine stack does.
///
/// The executors these tests build have soft guards -- no install hook is
/// registered here -- so the word below the usable base is a canary and not an
/// unmapped page.
fn smash_canary(base: usize) {
    unsafe {
        core::ptr::write_volatile((base - 8) as *mut u64, 0);
    }
}

/// Park a weak executor on this CPU and return its stack base.
fn park_a_weak() -> usize {
    let mut rt = GLOBAL_RUNTIME.force(0).lock();
    let collection = rt.task_collection.clone();
    let weak = Arc::new(Executor::new(collection));
    let base = weak.stack_base();
    rt.add_weak_executor(weak);
    base
}

// ── what the CPU answers about itself ──────────────────────────────────────

/// **`warm_runtimes` builds this CPU's slot, and only once.**
///
/// Bare-metal boot calls it right after the guard hooks are registered, which
/// is the whole point of it existing: the slot's first `Executor::new` has to
/// happen *after* the hooks, or the boot CPU's first stack gets a soft guard
/// for the life of the machine. Calling it twice must not build a second
/// runtime -- that would be another 2.6 MiB stack and a second queue nothing
/// polls.
#[test]
fn warming_the_runtime_builds_this_cpus_slot_once() {
    let _rt = with_runtime(0);
    warm_runtimes();
    let id = {
        let rt = get_current_runtime();
        rt.strong_executor.id()
    };
    warm_runtimes();
    let rt = get_current_runtime();
    assert_eq!(
        rt.strong_executor.id(),
        id,
        "una segunda llamada construyo otro runtime"
    );
    assert_eq!(rt.cpu_id(), 0);
}

/// **The current executor's id is 0 when there is none.**
///
/// Zero is "no executor" everywhere in this crate (`executor_alloc_id` starts
/// at 1 on purpose), and this reading goes into fault reports: a stale id in
/// one of those names the wrong stack as the one that died.
#[test]
fn the_current_executor_id_is_zero_while_nothing_is_running() {
    let _rt = with_runtime(0);
    assert_eq!(get_current_executor_id(), (0, 0));

    let running = {
        let mut rt = get_current_runtime();
        let ex = rt.strong_executor.clone();
        rt.current_executor = Some(ex);
        rt.strong_executor.id()
    };
    assert_eq!(get_current_executor_id(), (running, 0));
    assert!(!irq_on_idle_executor());

    get_current_runtime().current_executor = None;
    assert!(irq_on_idle_executor());
    assert_eq!(get_current_executor_id(), (0, 0));
}

// ── the canary sweep ───────────────────────────────────────────────────────

/// **A healthy CPU passes the canary sweep, idle and running.**
///
/// It is called from the timer tick, so the common case is every pass of it:
/// a sweep that panicked on a healthy stack would be a machine that cannot
/// boot.
#[test]
fn a_healthy_cpu_passes_the_canary_sweep() {
    let _rt = with_runtime(0);
    park_a_weak();
    check_current_executor_canary();

    let ex = get_current_runtime().strong_executor.clone();
    get_current_runtime().current_executor = Some(ex);
    check_current_executor_canary();
}

/// **Idle, the sweep still checks the strong executor.**
///
/// "Idle" is between polls, and the stack that was just used is still there
/// with whatever the last call chain did to it. Checking only the *current*
/// executor would mean the overflow is found on the next poll, by which time
/// the neighbouring heap has been written.
#[test]
#[should_panic(expected = "strong executor")]
fn an_idle_cpu_with_a_smashed_strong_canary_is_named() {
    let _rt = with_runtime(0);
    smash_canary(get_current_runtime().strong_executor.stack_base());
    check_current_executor_canary();
}

/// **And the parked weak ones, which nothing else is going to look at.**
///
/// A weak executor is a frame frozen mid-poll. Nobody runs on it until it gets
/// a turn, so if its canary is gone the only chance to say so is a sweep like
/// this one -- and the heap it overwrote is being used right now.
#[test]
#[should_panic(expected = "weak executor")]
fn an_idle_cpu_with_a_smashed_weak_canary_is_named() {
    let _rt = with_runtime(0);
    let base = park_a_weak();
    smash_canary(base);
    check_current_executor_canary();
}

/// **Running, the report names the task as well as the stack.**
///
/// The task id is what turns "a coroutine stack overflowed" into "this
/// future's call chain overflowed", which is the difference between a report
/// somebody can act on and a line in a log.
#[test]
#[should_panic(expected = "stack_base")]
fn a_running_executor_with_a_smashed_canary_names_its_task() {
    let _rt = with_runtime(0);
    let ex = get_current_runtime().strong_executor.clone();
    smash_canary(ex.stack_base());
    get_current_runtime().current_executor = Some(ex);
    check_current_executor_canary();
}

// ── the proximity check ────────────────────────────────────────────────────

/// **An RSP nowhere near any stack on this CPU is not a warning.**
#[test]
fn a_far_away_rsp_is_not_near_overflow() {
    let _rt = with_runtime(0);
    park_a_weak();
    check_current_executor_stack_proximity(0x1000);

    let ex = get_current_runtime().strong_executor.clone();
    get_current_runtime().current_executor = Some(ex);
    check_current_executor_stack_proximity(0x1000);
}

/// **An RSP in the last 128 KiB of the running executor's stack is.**
///
/// This is the early warning *before* the unmapped guard faults, and under a
/// hard guard it is the only one there is, because `canary_intact` always
/// answers true there. The threshold went 32 -> 128 KiB after labwc still
/// smashed past the old mark between two 4 ms timer samples.
#[test]
#[should_panic(expected = "NEAR OVERFLOW")]
fn an_rsp_in_the_low_water_zone_of_the_running_stack_panics() {
    let _rt = with_runtime(0);
    let ex = get_current_runtime().strong_executor.clone();
    let base = ex.stack_base();
    get_current_runtime().current_executor = Some(ex);
    check_current_executor_stack_proximity(base + 8);
}

/// **Idle, the check still covers the strong executor.**
///
/// An IRQ nesting after a yield leaves `current_executor` as `None` while the
/// RSP is still on the stack that was just parked.
#[test]
#[should_panic(expected = "NEAR OVERFLOW")]
fn an_idle_cpu_with_the_rsp_on_the_strong_stack_panics() {
    let _rt = with_runtime(0);
    let base = get_current_runtime().strong_executor.stack_base();
    check_current_executor_stack_proximity(base + 8);
}

/// **And the parked weak ones too.**
#[test]
#[should_panic(expected = "NEAR OVERFLOW")]
fn an_idle_cpu_with_the_rsp_on_a_parked_stack_panics() {
    let _rt = with_runtime(0);
    let base = park_a_weak();
    check_current_executor_stack_proximity(base + 8);
}

// ── skipping deep IRQ work ─────────────────────────────────────────────────

/// **With depth to spare, heavy IRQ work is not skipped.**
///
/// The real RSP of a host test thread is on no coroutine stack at all, which
/// is the "plenty of room" answer for every branch of this probe: current
/// executor set, idle with a strong executor, and idle with weak ones parked.
/// Skipping when there is room is a desktop that drops HID input for nothing.
#[test]
fn heavy_irq_work_is_not_skipped_when_the_stack_has_room() {
    let _rt = with_runtime(0);
    assert!(!irq_should_skip_heavy_work(), "idle, con sitio de sobra");
    park_a_weak();
    assert!(!irq_should_skip_heavy_work(), "con un debil aparcado");

    let ex = get_current_runtime().strong_executor.clone();
    get_current_runtime().current_executor = Some(ex);
    assert!(!irq_should_skip_heavy_work(), "con ejecutor corriendo");
}

// ── attributing a fault to a stack ─────────────────────────────────────────

/// **A fault's RSP and RBP are attributed to the executors they land on.**
///
/// This is what proves a soft-smash: a truncated `[rsp]` with hard guards
/// installed and no `[stack-guard] #PF` means either an in-stack buffer smash
/// (the pointer is still in the usable region) or heap/fn-ptr corruption (it
/// is on no stack at all). Telling those two apart is the whole report.
#[test]
fn a_fault_is_attributed_to_the_stack_its_pointers_land_on() {
    let _rt = with_runtime(0);
    let strong = get_current_runtime().strong_executor.stack_base();
    let weak = park_a_weak();

    // One in the middle of the running stack, one in the bottom guard of the
    // parked one: different executors, different regions.
    let attr = attribute_fault_stack_ptrs(strong + crate::STACK_SIZE / 2, weak - 8);
    let rsp = attr.rsp.expect("el rsp no se atribuyo a ninguna pila");
    let rbp = attr.rbp.expect("el rbp no se atribuyo a ninguna pila");
    assert_eq!(rsp.stack_base, strong);
    assert_eq!(rsp.region, StackPtrRegion::Usable);
    assert_eq!(rbp.stack_base, weak);
    assert_eq!(rbp.region, StackPtrRegion::BottomGuard);
    assert_eq!(attr.cpus_walked, 1);
    assert!(attr.executors_seen >= 2);

    // And a pair on no stack at all is the other verdict: heap or fn-ptr
    // corruption, not a stack overflow.
    let nowhere = attribute_fault_stack_ptrs(0x1000, 0x2000);
    assert!(nowhere.rsp.is_none() && nowhere.rbp.is_none());
    assert!(nowhere.executors_seen >= 2, "no se miro ningun ejecutor");
}

/// **A CPU whose slot is not built is counted as skipped, not walked.**
///
/// The walk is `try_lock` only because it runs from the panic path, where
/// waiting for a lock turns a diagnosable fault into a freeze with nothing on
/// screen. The count is how a reader of the report knows the answer is partial.
#[test]
fn the_cpus_the_walk_could_not_look_at_are_counted() {
    // Four CPUs "online", one of which has a runtime.
    let _rt = with_runtime(0b1111);
    let attr = attribute_fault_stack_ptrs(0x1000, 0x2000);
    assert_eq!(attr.cpus_walked, 1);
    assert_eq!(
        attr.cpus_skipped, 3,
        "no se contaron los huecos sin construir"
    );
}

// ── can this fault be contained? ───────────────────────────────────────────

/// **A fault SP on the running executor's own stack can be contained.**
///
/// And one anywhere else cannot: the caller is about to discard whichever
/// stack this says owns the fault, so saying yes about a stack the fault was
/// not on discards a live coroutine.
#[test]
fn a_fault_sp_is_only_abandonable_on_the_stack_it_belongs_to() {
    let _rt = with_runtime(0);
    let ex = get_current_runtime().strong_executor.clone();
    let base = ex.stack_base();
    assert!(
        !fault_sp_abandonable(base + 1024),
        "sin ejecutor corriendo no hay nada que abandonar"
    );

    get_current_runtime().current_executor = Some(ex);
    assert!(fault_sp_abandonable(base + crate::STACK_SIZE / 2));
    assert!(
        !fault_sp_abandonable(0x1000),
        "se dio por abandonable una pila que no era la del fallo"
    );

    // A clobbered canary disqualifies it: discarding a stack cannot repair
    // the heap it already overwrote.
    smash_canary(base);
    assert!(
        !fault_sp_abandonable(base + crate::STACK_SIZE / 2),
        "se abandono una pila cuyo canario ya estaba roto"
    );
}

/// **The two `current_*` probes read the real RSP, so on the host they refuse.**
///
/// Which is the answer that matters for them: both are panic-path entry points
/// and both have to fail closed. A host test thread is on no coroutine stack,
/// so "this CPU is standing on the executor's own stack" is false however the
/// runtime looks -- and that is what these assert, with a runtime and a
/// current executor in place, which is the state that would otherwise let a
/// sloppy implementation answer yes from the runtime alone.
#[test]
fn the_containment_probes_refuse_when_the_cpu_is_not_on_the_coroutine_stack() {
    let _rt = with_runtime(0);
    assert!(!current_task_abandonable());
    assert!(!current_executor_abandonable());

    let ex = get_current_runtime().strong_executor.clone();
    get_current_runtime().current_executor = Some(ex);
    assert!(!current_task_abandonable());
    assert!(!current_executor_abandonable());
}

// ── is a parked frame safe to resume? ──────────────────────────────────────

/// **Every way a parked frame can be unfit to resume, through the real gate.**
///
/// This is the pre-switch validation of the `[null-exec guard]`: a saved frame
/// whose `sp` does not name a whole frame on its own stack, or whose resume
/// CR3 or RIP came back wrong, is a frame that was written over, and `ret`ing
/// through it is the crash this check exists to replace with a fresh executor.
/// `classify_parked_frame` has its own tests for the rules; what this one
/// covers is the gate around them -- the report each verdict writes, including
/// the frame dump, which is the only thing a reader of that crash ever gets.
///
/// The fourth verdict, `Resumable`, is **not reachable here**: it needs the
/// resume address to be inside the kernel image, and the window that decides
/// that is only installed by a bare-metal boot path
/// (`lock::fn_slot::set_text_range` refuses a host address as implausible), so
/// on the host the fallback range is the kernel's own and a host function
/// pointer is never in it. The rule itself is covered by
/// `resume_address_in_text`'s own tests.
#[test]
fn a_parked_frame_that_cannot_be_resumed_is_refused_and_reported() {
    let _rt = with_runtime(0);
    let ex = get_current_runtime().strong_executor.clone();
    let base = ex.stack_base();

    // As built: a whole frame on its own stack with a live CR3, refused only
    // because its resume address is a host one. This is the arm that dumps
    // the frame.
    assert!(!executor_frame_resumable(&ex));
    let sp = ex.context.get_sp();
    assert!(sp > base && sp < base + crate::STACK_SIZE);

    // CR3 blanked where it sat: the frame was consumed or overwritten.
    unsafe { core::ptr::write_volatile(sp as *mut u64, 0) };
    assert!(!executor_frame_resumable(&ex));

    // And an `sp` that is not a whole frame on this stack is refused without
    // reading anything, which is why that check comes first.
    // SAFETY: `get_context` hands back the address of the saved-sp cell, which
    // is what `switch.S` writes through; this is the same write that module's
    // own tests make.
    unsafe {
        let cell = ex.context.get_context() as *mut usize;
        core::ptr::write_volatile(cell, base - 16);
        assert!(!executor_frame_resumable(&ex));
        core::ptr::write_volatile(cell, sp);
    }
}
