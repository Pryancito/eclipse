//! What `Executor::new` builds, and what happens to its stack when the
//! executor is built, reused and retired.
//!
//! This module could not exist until now. `init_stack_and_context` reads CR3,
//! a privileged instruction that in a user-mode test process is a #GP --
//! SIGSEGV before the test reports anything -- so every test of this file's
//! machinery had to go through the `metadata_only_executor` literal and could
//! never watch the real constructor do its work. `arch::pg_base_register` now
//! has a host stand-in beside the `cli`/`sti` and `hal_cpu_idle` ones, which
//! makes `Executor::new` callable here.
//!
//! What that constructor does is not bookkeeping. It carves a 2.6 MiB block,
//! installs (or fails to install) a guard page at each end, poisons the usable
//! 2 MiB so a `ret` into a never-used slot is a distinctive bad-RIP fault
//! instead of `rip = 0`, publishes the block in two live-stack registries so
//! neither the frame allocator nor the `GlobalAlloc` hook can hand out memory
//! that aliases it, and writes the saved frame `switch.S` will resume. Every
//! one of those steps has a crash class behind it, named in the comments of
//! `Executor::new` and of `Drop for Executor`.

use super::*;
use alloc::sync::Arc;
use core::sync::atomic::Ordering;

/// An empty run queue for an executor that is never going to poll it.
fn collection() -> Arc<crate::task_collection::TaskCollection> {
    crate::task_collection::TaskCollection::new(0)
}

/// Every global this file reads, emptied, and put back on the way out.
///
/// The stack pool, the overflow retention list, the retired-stack table, the
/// guard hooks and the quarantine hooks are all module statics, and
/// `Executor::new` touches the lot in one call. The lock is
/// [`super::executor_globals_lock`], shared with the three other modules that
/// empty any of them.
struct Fresh {
    _guard: std::sync::MutexGuard<'static, ()>,
    saved_ready: u64,
}

impl Drop for Fresh {
    fn drop(&mut self) {
        crate::runtime::set_executor_ready_mask_for_test(self.saved_ready);
        wipe();
    }
}

fn wipe() {
    *RETIRED_STACKS.lock() = [RetiredStack::EMPTY; RETIRED_STACKS_CAP];
    RETIRED_OCCUPIED.store(0, Ordering::SeqCst);
    for slot in STACK_POOL.iter() {
        slot.store(0, Ordering::SeqCst);
    }
    STACK_OVERFLOW.lock().clear();
    *STACK_GUARD_INSTALL.lock() = None;
    *STACK_GUARD_REMOVE.lock() = None;
    *STACK_QUAR_PROTECT.lock() = None;
    *STACK_QUAR_UNPROTECT.lock() = None;
}

/// `ready` is the set of CPUs a retirement waits for. `0` means "nobody else
/// is online", which is what makes a `Drop` reclaim its block immediately
/// instead of parking it in the retired-stack table.
fn fresh(ready: u64) -> Fresh {
    let guard = super::executor_globals_lock();
    let saved_ready = crate::runtime::set_executor_ready_mask_for_test(ready);
    wipe();
    Fresh {
        _guard: guard,
        saved_ready,
    }
}

/// A guard-install hook that always succeeds, and the count of its calls.
mod hooks {
    use core::sync::atomic::{AtomicUsize, Ordering};

    pub static INSTALLED: AtomicUsize = AtomicUsize::new(0);
    pub static REMOVED: AtomicUsize = AtomicUsize::new(0);

    pub fn install_ok(_base: usize, _size: usize) -> bool {
        INSTALLED.fetch_add(1, Ordering::SeqCst);
        true
    }

    pub fn install_refused(_base: usize, _size: usize) -> bool {
        INSTALLED.fetch_add(1, Ordering::SeqCst);
        false
    }

    pub fn remove(_base: usize, _size: usize) {
        REMOVED.fetch_add(1, Ordering::SeqCst);
    }

    pub fn reset() {
        INSTALLED.store(0, Ordering::SeqCst);
        REMOVED.store(0, Ordering::SeqCst);
    }
}

/// **The geometry, the poison and the saved frame, from the real constructor.**
///
/// `[bottom guard][usable STACK_SIZE][top guard]`, the block page-aligned
/// because the guard installs are page-table operations, and the saved frame
/// written at the very top of the usable region: `switch.S` pops it from
/// there, so `sp` landing anywhere else is a resume onto someone else's
/// memory. `pc` is `executor_entry` -- the assembly stub, not `run` -- because
/// the stub is what consumes the argument slot below it.
#[test]
fn a_fresh_executor_is_a_guarded_poisoned_block_with_a_frame_on_top() {
    let _f = fresh(0);
    let executor = Executor::new(collection());

    let alloc_base = executor.stack_base - GUARD_SIZE;
    assert_eq!(alloc_base % PAGE_SIZE, 0, "el bloque no esta alineado");
    assert_eq!(executor.stack_base, alloc_base + GUARD_SIZE);

    let sp = executor.context.get_sp();
    assert!(
        sp > executor.stack_base && sp <= executor.stack_base + STACK_SIZE,
        "el marco guardado no cae dentro de la region usable: {:#x}",
        sp
    );
    assert_eq!(sp % 8, 0);
    assert_eq!(
        executor.context.get_pc(),
        executor_entry as *const () as usize
    );
    assert_eq!(executor.context.get_pgbr(), crate::arch::HOST_PG_BASE);
    assert!(executor.stack_contains(executor.stack_base));
    assert!(executor.stack_contains(executor.stack_base + STACK_SIZE - 1));
    assert!(!executor.stack_contains(alloc_base));

    // The poison, read where nothing has been written over it: the bottom of
    // the usable region. The saved frame lives at the far end.
    unsafe {
        let p = executor.stack_base as *const u64;
        for i in 0..8 {
            assert_eq!(
                core::ptr::read_volatile(p.add(i)),
                STACK_POISON,
                "la pila no se envenenó en la palabra {}",
                i
            );
        }
    }
}

/// **Both live-stack registries get the block, and from the first instant.**
///
/// They hold the same set and answer different callers: the frame allocator
/// asks `overlapping_live_stack`, the `GlobalAlloc` hook asks
/// `alloc_overlaps_live_stack`, and coroutine stacks are allocated through the
/// second one. A block missing from either is a block the corresponding
/// allocator will happily hand out again -- the `[double-alloc]` that let a
/// `Vec` land on a live coroutine stack and zero its saved return slots.
#[test]
fn a_live_stack_is_published_in_both_registries_and_retracted_at_drop() {
    let _f = fresh(0);
    let executor = Executor::new(collection());
    let alloc_base = executor.stack_base - GUARD_SIZE;

    assert_eq!(
        overlapping_live_stack(executor.stack_base + STACK_SIZE / 2, 8),
        Some(alloc_base),
        "el asignador de marcos no ve la pila viva"
    );
    assert_eq!(
        alloc_overlaps_live_stack(alloc_base, 8),
        Some(alloc_base),
        "el gancho de GlobalAlloc no ve el bloque vivo"
    );
    // And nothing outside the block is claimed by it.
    assert_eq!(
        overlapping_live_stack(alloc_base - PAGE_SIZE, PAGE_SIZE),
        None
    );

    drop(executor);
    assert_eq!(
        overlapping_live_stack(alloc_base + GUARD_SIZE, 8),
        None,
        "la pila retirada sigue anotada como viva"
    );
    assert_eq!(alloc_overlaps_live_stack(alloc_base, 8), None);
}

/// **No guard hooks is the soft path: a canary instead of an unmapped page.**
///
/// Bare metal registers the hooks right after boot, but the executors created
/// before that -- and any block whose guard install a huge PTE refuses -- have
/// to fall back to something. The fallback is a known pattern written over the
/// bottom guard, which `canary_intact` reads afterwards: with a hard guard an
/// overflow faults before it reaches the heap, without one the canary is the
/// only evidence it happened.
#[test]
fn with_no_hooks_the_guard_is_a_canary_and_canary_intact_can_read_it() {
    let _f = fresh(0);
    let (hard_before, soft_before) = hard_guard_executor_counts();
    let executor = Executor::new(collection());

    assert!(!executor.hard_guard_bottom && !executor.hard_guard_top);
    assert_eq!(
        hard_guard_executor_counts(),
        (hard_before, soft_before + 1),
        "el ejecutor blando no se conto como blando"
    );
    assert!(executor.canary_intact());

    // And an overflow past the edge of the usable region is what it reports:
    // the first words an overflow smashes are the high end of the guard.
    unsafe {
        let high = (executor.stack_base - 8) as *mut u64;
        core::ptr::write_volatile(high, 0);
    }
    assert!(
        !executor.canary_intact(),
        "el canario no vio una escritura por debajo del borde"
    );
}

/// **With the hooks registered the guards are hard, and there is no canary.**
///
/// Two installs, not one: the hook API takes a single range per call and
/// `stack_guard` stores each base independently, so the bottom and the top go
/// in separately. A single install would leave one end of every stack open.
#[test]
fn with_hooks_registered_both_ends_are_installed_separately() {
    let _f = fresh(0);
    hooks::reset();
    unsafe { set_stack_guard_hooks(hooks::install_ok, hooks::remove) };
    assert!(stack_guard_hooks_registered());

    let (hard_before, soft_before) = hard_guard_executor_counts();
    let executor = Executor::new(collection());

    assert!(executor.hard_guard_bottom && executor.hard_guard_top);
    assert_eq!(
        hooks::INSTALLED.load(Ordering::SeqCst),
        2,
        "los dos extremos no se instalaron por separado"
    );
    assert_eq!(hard_guard_executor_counts(), (hard_before + 1, soft_before));
    assert!(
        executor.canary_intact(),
        "con guarda dura el canario contesta intacto sin leer nada"
    );
}

/// **An install the hooks refuse falls back to the canary, end by end.**
///
/// A huge-page leaf cannot be split into a guard, and the honest answer is a
/// soft guard and a count, not a stack with a guard it does not have.
#[test]
fn an_install_the_hooks_refuse_falls_back_to_the_canary() {
    let _f = fresh(0);
    hooks::reset();
    unsafe { set_stack_guard_hooks(hooks::install_refused, hooks::remove) };

    let (_, soft_before) = hard_guard_executor_counts();
    let executor = Executor::new(collection());

    assert!(!executor.hard_guard_bottom && !executor.hard_guard_top);
    assert_eq!(hooks::INSTALLED.load(Ordering::SeqCst), 2);
    assert_eq!(hard_guard_executor_counts().1, soft_before + 1);
    assert!(executor.canary_intact(), "el canario no se escribio");
}

/// **A retired hard-guarded block goes to the pool and comes back out of it.**
///
/// It must never go back to `Global`: that arena also backs userspace frames,
/// and a retirement one grace period early would let a stale kernel write
/// corrupt whatever page reused the block -- a fault the live-stack registry
/// cannot catch, because the block left the registry at `Drop`. So the block
/// stays in coroutine-stack land, with its guards still installed, and the
/// next `Executor::new` re-uses it: same `alloc_base`, and **no further
/// install**, which is the half of the contract that says the pooled block
/// still has the guards it was given.
#[test]
fn a_retired_hard_guarded_block_is_pooled_and_handed_back_without_reinstalling() {
    let _f = fresh(0);
    hooks::reset();
    unsafe { set_stack_guard_hooks(hooks::install_ok, hooks::remove) };

    let first = Executor::new(collection());
    let alloc_base = first.stack_base - GUARD_SIZE;
    assert_eq!(stack_pool_stats(), (0, 0));
    drop(first);
    assert_eq!(
        stack_pool_stats(),
        (1, 0),
        "el bloque retirado no quedo en la reserva"
    );

    let installs = hooks::INSTALLED.load(Ordering::SeqCst);
    let second = Executor::new(collection());
    assert_eq!(
        second.stack_base - GUARD_SIZE,
        alloc_base,
        "no se reutilizo el bloque de la reserva"
    );
    assert!(second.hard_guard_bottom && second.hard_guard_top);
    assert_eq!(
        hooks::INSTALLED.load(Ordering::SeqCst),
        installs,
        "se volvio a instalar la guarda de un bloque que ya la tenia"
    );
    assert_eq!(stack_pool_stats(), (0, 0), "la reserva no se vacio");
}

/// **With the pool full, the block goes to the overflow list, not to `Global`.**
///
/// Same reason as above: the pool is a fixed array and churn is unbounded, so
/// there has to be somewhere else for a freed block to go that is not the
/// shared arena.
#[test]
fn a_retired_block_with_the_pool_full_is_retained_in_the_overflow_list() {
    let _f = fresh(0);
    hooks::reset();
    unsafe { set_stack_guard_hooks(hooks::install_ok, hooks::remove) };
    // The block is carved first, with the pool empty: filling it beforehand
    // would hand `Executor::new` one of the fake addresses below and the
    // poison loop would write to it.
    let executor = Executor::new(collection());
    let alloc_base = executor.stack_base - GUARD_SIZE;
    // And now every slot is taken, by addresses nothing will hand out.
    for (i, slot) in STACK_POOL.iter().enumerate() {
        slot.store((i + 1) * PAGE_SIZE, Ordering::SeqCst);
    }
    drop(executor);

    let (pool, overflow) = stack_pool_stats();
    assert_eq!(pool, STACK_POOL_CAP);
    assert_eq!(overflow, 1, "el bloque no se retuvo");
    assert_eq!(STACK_OVERFLOW.lock()[0], alloc_base);
}

/// **A soft-guarded block is leaked rather than pooled or freed.**
///
/// `Executor::new`'s reuse path assumes the pooled block already has its hard
/// guards, so a soft one cannot be pooled; and returning it to `Global` is the
/// aliasing above. Leaking it is the only remaining answer, and it is bounded
/// in practice because it only happens before the hooks exist.
#[test]
fn a_retired_soft_guarded_block_is_leaked_and_not_pooled() {
    let _f = fresh(0);
    let executor = Executor::new(collection());
    assert!(!executor.hard_guard_bottom);
    drop(executor);
    assert_eq!(
        stack_pool_stats(),
        (0, 0),
        "un bloque con guarda blanda entro en la reserva"
    );
}

/// **A block retired while other CPUs are online waits for all of them.**
///
/// A dropped executor may still have an SMP-late parked frame, so neither the
/// pool nor the heap may see the block until every online CPU has run on its
/// own runtime stack since the retirement. The table is where it waits, and
/// `note_cpu_quiescent` is each CPU saying it has.
#[test]
fn a_block_retired_with_peers_online_waits_for_every_one_of_them() {
    let _f = fresh(0b111);
    hooks::reset();
    unsafe { set_stack_guard_hooks(hooks::install_ok, hooks::remove) };

    let executor = Executor::new(collection());
    let alloc_base = executor.stack_base - GUARD_SIZE;
    drop(executor);
    assert_eq!(
        stack_pool_stats(),
        (0, 0),
        "se reclamo sin esperar a las otras CPUs"
    );
    assert_eq!(RETIRED_OCCUPIED.load(Ordering::SeqCst), 1);

    // CPU 0 is us and was already discounted at retirement; 1 and 2 have to
    // say so, and only the last one reclaims.
    crate::executor::note_cpu_quiescent(1);
    assert_eq!(stack_pool_stats(), (0, 0));
    crate::executor::note_cpu_quiescent(2);
    assert_eq!(
        stack_pool_stats(),
        (1, 0),
        "el bloque no se reclamo al pasar la ultima CPU"
    );
    assert_eq!(RETIRED_OCCUPIED.load(Ordering::SeqCst), 0);
    assert_eq!(
        STACK_POOL
            .iter()
            .find(|s| s.load(Ordering::SeqCst) != 0)
            .map(|s| s.load(Ordering::SeqCst)),
        Some(alloc_base)
    );
}

/// **An executor dropped while a CPU still stands on its stack leaks it.**
///
/// This is the free-while-live race, contained. The resume claim is parked
/// only once control is off the executor's stack, so a claim still held at
/// `Drop` means some CPU is executing on that block right now: returning it to
/// the heap is precisely the `[double-alloc]` that zeroed saved return slots
/// under a live coroutine (the labwc `wl_list` NULL-deref class). The tracking
/// entries go, the memory does not.
#[test]
fn an_executor_dropped_while_a_cpu_stands_on_it_leaks_the_block() {
    let _f = fresh(0);
    hooks::reset();
    unsafe { set_stack_guard_hooks(hooks::install_ok, hooks::remove) };

    let executor = Executor::new(collection());
    let alloc_base = executor.stack_base - GUARD_SIZE;
    executor.try_claim_resume(3).expect("nadie lo tenia");
    drop(executor);

    assert_eq!(
        stack_pool_stats(),
        (0, 0),
        "se reciclo una pila sobre la que una CPU estaba"
    );
    assert_eq!(RETIRED_OCCUPIED.load(Ordering::SeqCst), 0);
    // Pero las dos anotaciones si se retiran: el bloque es inalcanzable, y
    // dejarlo anotado haria que cualquier reutilizacion legitima de ese rango
    // se denunciara como doble reserva para siempre.
    assert_eq!(overlapping_live_stack(alloc_base + GUARD_SIZE, 8), None);
    assert_eq!(alloc_overlaps_live_stack(alloc_base, 8), None);
}

/// **The quarantine freezes the usable region of a retired block.**
///
/// It is the diagnostic fast path: with the protect hook available, a stale
/// writer to a parked frame faults as `[stack-uaf]` during the grace period
/// instead of corrupting whatever reused the block. The unprotect must restore
/// it before the block is handed out again, which is what the pair of counts
/// here pins.
#[test]
fn the_quarantine_protects_a_retired_block_and_unprotects_it_before_reuse() {
    use core::sync::atomic::AtomicUsize;
    static PROTECTED: AtomicUsize = AtomicUsize::new(0);
    static UNPROTECTED: AtomicUsize = AtomicUsize::new(0);
    fn protect(_base: usize, _size: usize) -> bool {
        PROTECTED.fetch_add(1, Ordering::SeqCst);
        true
    }
    fn unprotect(_base: usize, _size: usize) {
        UNPROTECTED.fetch_add(1, Ordering::SeqCst);
    }

    let _f = fresh(0b11);
    hooks::reset();
    PROTECTED.store(0, Ordering::SeqCst);
    UNPROTECTED.store(0, Ordering::SeqCst);
    unsafe { set_stack_guard_hooks(hooks::install_ok, hooks::remove) };
    unsafe { set_stack_quarantine_hooks(protect, unprotect) };

    let executor = Executor::new(collection());
    drop(executor);
    assert_eq!(
        PROTECTED.load(Ordering::SeqCst),
        1,
        "la region usable no se congelo al retirarla"
    );
    assert_eq!(UNPROTECTED.load(Ordering::SeqCst), 0);

    crate::executor::note_cpu_quiescent(1);
    assert_eq!(
        UNPROTECTED.load(Ordering::SeqCst),
        1,
        "el bloque entro en la reserva congelado"
    );
    assert_eq!(stack_pool_stats(), (1, 0));
}
