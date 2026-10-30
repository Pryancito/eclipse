//! Where a new task is born, and who comes to take it away.
//!
//! [`super::with_runtime_tests`] covers the diagnostics that read a runtime;
//! this module covers the two decisions that *use* one. Both were impossible
//! to test before `arch::pg_base_register` got a host stand-in, because both
//! end in a real `ExecutorRuntime` with a real `Executor` behind it.
//!
//! The decisions are not bookkeeping either. `spawn_task` picking the wrong
//! CPU is a `fork`ed child that waits out a timeslice before its first
//! instruction; picking a CPU the affinity mask forbids is a task parked in a
//! queue whose generator refuses it on every pass. And `steal_task_inner` is
//! the only reason an idle core ever gets work at all -- its ranking by
//! `ready_num_for` rather than `ready_num` is what keeps a victim full of
//! foreign-affinity work from absorbing every idle core's scan.

use super::*;
use core::sync::atomic::Ordering as O;

/// CPU 0's runtime, with an **empty** queue and a fresh strong executor, put
/// back the way it was found.
///
/// Unlike [`super::with_runtime_tests::WithRuntime`] this also replaces the
/// task collection: every test here counts tasks, and `GLOBAL_RUNTIME`'s
/// slots are `Once` -- a task one test spawned and never polled would be in
/// the next test's `task_num`.
///
/// It also owns the three steal globals (`STEALABLE`, `STRANDED` and the
/// per-CPU rescue latches) and the host CPU id, because a steal test has to
/// stand on a CPU that is *not* the victim.
struct Placed {
    _globals: std::sync::MutexGuard<'static, ()>,
    _resched: std::sync::MutexGuard<'static, ()>,
    saved_ready: u64,
    saved_cpu: u8,
    saved_stealable: u64,
    saved_stranded: u64,
}

/// Give `cpu`'s runtime a brand-new collection and a strong executor built
/// from it, building the slot if it does not exist yet.
fn reset_cpu(cpu: usize) {
    let mut rt = GLOBAL_RUNTIME.force(cpu).lock();
    rt.current_executor = None;
    rt.weak_executors.clear();
    rt.task_collection = TaskCollection::new(cpu as u8);
    let fresh = Arc::new(Executor::new(rt.task_collection.clone()));
    rt.strong_executor = fresh;
}

/// Empty every slot that exists, which is CPU 0 plus whichever victims a
/// ranking test asked for.
///
/// The slots are `Once`: a victim one test built is there for the rest of the
/// process, and a task left in its queue would be in the next test's count --
/// including the counts of `probe_tests`, which takes the same lock and whose
/// premise is that the peers hold nothing.
fn reset_built_slots() {
    reset_cpu(0);
    for cpu in 1..MAX_CORE_NUM {
        if GLOBAL_RUNTIME.get(cpu).is_some() {
            reset_cpu(cpu);
        }
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        crate::arch::set_cpu_id_for_test(self.saved_cpu);
        // Back to "nobody else is online" first, so the executors this test
        // built reclaim their blocks at `Drop` instead of being parked in the
        // retired-stack table waiting for CPUs this host does not have.
        set_executor_ready_mask_for_test(0);
        reset_built_slots();
        publish_weak_waiting(0);
        STEALABLE.store(self.saved_stealable, O::SeqCst);
        STRANDED.store(self.saved_stranded, O::SeqCst);
        for latch in RESCUE_KICK.iter() {
            latch.0.store(0, O::SeqCst);
        }
        set_executor_ready_mask_for_test(self.saved_ready);
        crate::executor::wipe_stack_pool_for_test();
    }
}

/// `ready` is the executor-ready mask the decision reads; `cpu` is the CPU the
/// caller is standing on.
fn placed(ready: u64, cpu: u8) -> Placed {
    let globals = crate::executor::executor_globals_lock();
    let resched = resched_test_lock();
    let saved_stealable = STEALABLE.swap(0, O::SeqCst);
    let saved_stranded = STRANDED.swap(0, O::SeqCst);
    for latch in RESCUE_KICK.iter() {
        latch.0.store(0, O::SeqCst);
    }
    // A request another test left pending would make the first
    // `take_need_resched` here answer `true` before anything asked for one.
    NEED_RESCHED.store(0, O::SeqCst);
    // Build this test's executor with nobody else online, so the one it
    // replaces reclaims its block at `Drop` instead of being parked in the
    // retired-stack table waiting for CPUs this host does not have. The mask
    // the suite found is the one captured for the way out.
    let saved_ready = set_executor_ready_mask_for_test(0);
    reset_built_slots();
    publish_weak_waiting(0);
    set_executor_ready_mask_for_test(ready);
    let saved_cpu = crate::arch::set_cpu_id_for_test(cpu);
    Placed {
        _globals: globals,
        _resched: resched,
        saved_ready,
        saved_cpu,
        saved_stealable,
        saved_stranded,
    }
}

/// A future that never finishes, so the task it carries stays in the queue to
/// be counted and stolen.
fn parked() -> impl Future<Output = ()> + Send + 'static {
    core::future::pending()
}

fn cpu0_task_num() -> usize {
    GLOBAL_RUNTIME.force(0).lock().task_num()
}

// ── where a new task is born ───────────────────────────────────────────────

/// **A task spawned for a named CPU lands in that CPU's queue, and that CPU
/// is kicked.**
///
/// `spawn_task` *forces* the slot for a named CPU rather than skipping a cold
/// one: the task needs a queue to land in even if its CPU has not entered the
/// scheduler loop yet. And the kick is not optional -- a new task is born
/// `notified`, which is the same situation as a wake, so a halted target
/// would not notice it until its next tick and a busy one not until the
/// occupying thread's timeslice expired.
#[test]
fn una_tarea_con_cpu_pedida_nace_en_la_cola_de_esa_cpu_y_la_despierta() {
    let _p = placed(0b1, 0);
    let (requested_before, _, _) = wakeup_preempt_stats();
    spawn_task(parked(), None, Some(0), None);
    assert_eq!(
        cpu0_task_num(),
        1,
        "la tarea no llego a la cola de la CPU 0"
    );
    assert!(
        wakeup_preempt_stats().0 > requested_before,
        "una tarea recien nacida esta notificada: sin el aviso su CPU no la \
         mira hasta el siguiente tick"
    );
}

/// **A named CPU that is not in its executor loop is warned about and used
/// anyway.**
///
/// The warning is the whole value of the branch: the task will sit there
/// until someone steals it, and that is a stall worth a line in the log. The
/// placement still has to happen -- refusing it would lose the task.
#[test]
fn una_cpu_pedida_que_no_ha_entrado_en_su_bucle_se_avisa_y_se_usa_igual() {
    let _p = placed(0, 0);
    assert!(
        !is_executor_ready(0),
        "la mascara de este test tiene que dejar a la CPU 0 fuera del bucle"
    );
    spawn_task(parked(), None, Some(0), None);
    assert_eq!(cpu0_task_num(), 1);
}

/// **With no CPU named, the scan picks the least-loaded ready CPU.**
///
/// The scan is `try_lock`-only on purpose: a locked runtime is by definition
/// in use, and spinning on it would make every spawn wait on whoever is
/// polling. It ranks by `placement_load` -- *runnable* tasks -- because
/// counting parked ones made a CPU full of sleeping daemons look busier than
/// a CPU saturated with spinners, which is backwards.
#[test]
fn sin_cpu_pedida_la_tarea_va_a_la_cpu_menos_cargada() {
    let _p = placed(0b1, 0);
    spawn_task(parked(), None, None, None);
    spawn_task(parked(), None, None, None);
    assert_eq!(
        cpu0_task_num(),
        2,
        "con una sola CPU lista el barrido solo puede elegirla a ella"
    );
}

/// **The affinity mask decides which CPUs the scan even looks at.**
///
/// A mask that allows nobody who is ready skips every candidate and falls
/// back to `affinity_home`, then to `park_cpu_if_cold` -- a CPU that *is*
/// looping, so a generator can see the task and forward it. The old fallback
/// was CPU 0 outright, and CPU 0 is in general a CPU the mask forbids: the
/// task sat in the queue of a CPU not allowed to poll it, skipped on every
/// single pass.
#[test]
fn la_mascara_de_afinidad_saca_a_la_cpu_del_barrido_y_se_aparca_en_una_viva() {
    let _p = placed(0b1, 0);
    // Bit 0 clear: the only ready CPU is one the mask forbids.
    let mask = Arc::new(AtomicU64::new(0b10));
    spawn_task(parked(), None, None, Some(mask));
    assert_eq!(
        cpu0_task_num(),
        1,
        "la tarea se aparca en la unica CPU viva aunque su mascara no la \
         permita: alli hay un generador que la reenvia, y la CPU de su \
         mascara la roba al entrar"
    );
}

/// **`spawn` and `spawn_with_affinity` are the same placement with interrupts
/// off.**
///
/// They exist as separate entry points because the affinity mask is shared
/// (`Arc`) with the owning thread, which updates it later through
/// `sched_setaffinity`; the scheduler re-reads it on every placement and
/// steal decision rather than copying it once at birth.
#[test]
fn las_dos_puertas_publicas_del_spawn_colocan_igual() {
    let _p = placed(0b1, 0);
    spawn(parked());
    assert_eq!(cpu0_task_num(), 1);
    spawn_with_affinity(parked(), Arc::new(AtomicU64::new(u64::MAX)));
    assert_eq!(cpu0_task_num(), 2);
    assert!(
        crate::arch::intr_get(),
        "las dos restauran el estado de interrupciones que encontraron"
    );
}

// ── what a runtime says about its own load ─────────────────────────────────

/// **The three load figures a runtime reports, and the two ways to change
/// them.**
///
/// `task_num` counts everything in the slab; `ready_num` only what is
/// runnable; `ready_num_for` only what *this* CPU may poll. The distinction is
/// the one steal ranking lives on, and `placement_load` is the figure spawn
/// placement uses.
#[test]
fn el_runtime_cuenta_sus_tareas_de_tres_maneras_distintas() {
    let _p = placed(0b1, 0);
    let rt = GLOBAL_RUNTIME.force(0).lock();
    assert_eq!(rt.task_num(), 0);
    assert_eq!(rt.ready_num(), Some(0));
    assert_eq!(rt.ready_num_for(0), Some(0));
    assert_eq!(rt.placement_load(), Some(0));
    assert_eq!(placement_load(&rt), 0);
    assert_eq!(rt.weak_executor_num(), 0);
    assert_ne!(rt.get_context(), 0, "el marco guardado del runtime existe");

    let key = rt.add_task(DEFAULT_PRIORITY, parked(), None);
    assert_eq!(rt.task_num(), 1);
    assert_eq!(rt.ready_num(), Some(1));
    assert_eq!(rt.ready_num_for(0), Some(1));
    assert_eq!(placement_load(&rt), 1);

    rt.remove_task(key);
    assert_eq!(
        rt.task_num(),
        0,
        "quitar la tarea por su llave la saca de la cuenta"
    );
}

/// **A task whose mask forbids this CPU counts as load, but not as work this
/// CPU may run.**
///
/// This is the whole reason `ready_num_for` exists beside `ready_num`: a
/// victim full of foreign-affinity work looked richest under plain
/// `ready_num` and burned every idle core's scan on affinity kicks.
#[test]
fn una_tarea_de_otra_cpu_cuenta_como_carga_pero_no_como_trabajo_nuestro() {
    let _p = placed(0b11, 0);
    let rt = GLOBAL_RUNTIME.force(0).lock();
    // Allowed on CPU 0 only.
    rt.add_task(
        DEFAULT_PRIORITY,
        parked(),
        Some(Arc::new(AtomicU64::new(1))),
    );
    assert_eq!(rt.ready_num(), Some(1), "la tarea esta lista");
    assert_eq!(
        rt.ready_num_for(1),
        Some(0),
        "pero la CPU 1 no puede correrla"
    );
}

/// **The whole run queue of the machine, summed.**
///
/// `runnable_task_count` is the Linux-style `nr_running` the load average
/// wants, and it skips a momentarily-locked CPU rather than spinning: an EWMA
/// consumer tolerates the occasional low sample, and this must stay safe from
/// any context.
#[test]
fn la_cola_de_toda_la_maquina_es_la_suma_de_las_colas_vivas() {
    let _p = placed(0b1, 0);
    assert_eq!(runnable_task_count(), 0);
    spawn_task(parked(), None, Some(0), None);
    assert_eq!(runnable_task_count(), 1);
}

// ── who comes to take it away ──────────────────────────────────────────────

/// **A scan with no peer offering anything touches no lock at all.**
///
/// `STEALABLE | STRANDED` is the gate, and it is two plain loads. This is the
/// pass an idle CPU used to pay for on every single interrupt: two `try_lock`
/// compare-exchanges per empty peer, on every peer, to discover that there
/// was nothing anywhere.
#[test]
fn un_barrido_sin_iguales_que_ofrezcan_nada_no_mira_ninguna_cola() {
    let _p = placed(0b11, 1);
    let (scans_before, _, _, _, _, skipped_before, _) = sched_steal_stats();
    assert!(steal_task_from_other_cpu().is_none());
    let (scans, _, _, _, _, skipped, _) = sched_steal_stats();
    assert_eq!(
        scans, scans_before,
        "la puerta tiene que cortar antes del barrido, no despues"
    );
    assert_eq!(skipped, skipped_before + 1, "y el corte se cuenta");
}

/// **The thief takes the task off the peer that holds it.**
///
/// Both locks are `try_lock`, on both the ranking pass and the probe pass.
/// That is not an optimization: the thief may hold the victim's runtime lock
/// while stealing, but it must never *spin* while holding it. The AB-BA
/// otherwise is the >8 s DEADLOCK banner -- thief holds `RUNTIME[victim]` and
/// spins on the victim's generator; the victim's executor holds that
/// generator mid-`take_task` and is interrupted by a timer whose
/// `sched_yield` spins on `RUNTIME[victim]` with interrupts off.
#[test]
fn el_ladron_se_lleva_la_tarea_del_igual_que_la_tiene() {
    let _p = placed(0b11, 1);
    spawn_task(parked(), None, Some(0), None);
    note_stealable(0);
    let (_, probed_before, ok_before, _, _, _, _) = sched_steal_stats();

    let stolen = steal_task_from_other_cpu();

    assert!(stolen.is_some(), "la CPU 1 tenia que poder robarle a la 0");
    let (_, probed, ok, _, _, _, _) = sched_steal_stats();
    assert_eq!(probed, probed_before + 1);
    assert_eq!(ok, ok_before + 1);
}

/// **A peer full of work this CPU may not run is counted apart from an empty
/// one.**
///
/// `ready_num_for` says `Some(0)` for both, and the two call for opposite
/// fixes: an empty peer means the machine has no work, a peer full of
/// foreign-affinity work means affinity is starving steal. `/proc/perf` has to
/// be able to tell them apart.
#[test]
fn un_igual_lleno_de_trabajo_ajeno_no_es_lo_mismo_que_un_igual_vacio() {
    let _p = placed(0b11, 1);
    {
        let rt = GLOBAL_RUNTIME.force(0).lock();
        rt.add_task(
            DEFAULT_PRIORITY,
            parked(),
            Some(Arc::new(AtomicU64::new(1))),
        );
    }
    note_stealable(0);
    let (_, _, _, affinity_before, _, _, _) = sched_steal_stats();

    assert!(
        steal_task_from_other_cpu().is_none(),
        "la tarea esta clavada en la CPU 0: la 1 no puede llevarsela"
    );

    let (_, _, _, affinity, _, _, _) = sched_steal_stats();
    assert_eq!(
        affinity,
        affinity_before + 1,
        "un igual lleno de trabajo que no podemos correr se apunta aparte"
    );
}

/// **The rebalance pull is the same scan with a higher bar, and it is counted
/// separately.**
///
/// The caller has already checked `rebalance_due` and that its own queue holds
/// exactly one runnable task, so this pull only fires against a *clearly*
/// richer peer -- `1 + REBALANCE_MARGIN`, not `1`. A rebalance scan also does
/// not mix into the affinity-empty counter: those scans are rare and gated,
/// and counting them would blur the figure that says whether affinity is
/// starving idle steal.
#[test]
fn el_tiron_de_reparto_es_el_mismo_barrido_con_el_listo_mas_alto() {
    let _p = placed(0b11, 1);
    note_stealable(0);
    {
        let rt = GLOBAL_RUNTIME.force(0).lock();
        for _ in 0..(1 + REBALANCE_MARGIN) {
            rt.add_task(DEFAULT_PRIORITY, parked(), None);
        }
    }
    let (_, _, _, _, rebalance_before, _, _) = sched_steal_stats();
    assert!(
        steal_for_balance().is_some(),
        "con {} tareas el igual es claramente mas rico",
        1 + REBALANCE_MARGIN
    );
    let (_, _, _, _, rebalance, _, _) = sched_steal_stats();
    assert_eq!(rebalance, rebalance_before + 1);
}

/// **A rescue pull is the idle scan, run by a CPU that is not idle.**
///
/// The only thing that changes is who may call it. `STEALABLE` answers "is
/// there anything worth taking"; `STRANDED` answers the narrower "is there
/// anything nobody else will ever come for" -- a task its owner's mask
/// forbids, which the owner's generator refuses on every pass and which no
/// idle CPU comes for because an allowed CPU that never goes idle never comes
/// at all. Two threads in a tight `sched_yield` loop pinned to one CPU were
/// enough: a third, born elsewhere, got one timeslice in twenty seconds
/// against its peer's 544.590.
#[test]
fn un_rescate_es_el_barrido_de_siempre_hecho_por_una_cpu_ocupada() {
    let _p = placed(0b11, 1);
    spawn_task(parked(), None, Some(0), None);
    // The owner says so: its generator refused the task, and `STEALABLE` is
    // NOT set -- a halted owner clears it while still holding the task.
    note_stranded(0);
    assert!(
        stranded_peer_exists(),
        "la CPU 1 tiene a quien ir a rescatar"
    );
    let (_, _, _, _, _, _, rescue_before) = sched_steal_stats();
    assert!(rescue_stranded_task().is_some());
    let (_, _, _, _, _, _, rescue) = sched_steal_stats();
    assert_eq!(rescue, rescue_before + 1);
}

/// **A rescue kick is latched once, taken once, and the empty outcome is
/// counted on its own.**
///
/// Three counters and not one, because `STEAL_RESCUE` alone cannot say which
/// link of the forward a lost wake fell out of: a pinned waker that runs once
/// in 32 wakes shows the same low rescue count whether the owner never
/// kicked, the kicked CPU never made a pass, or the pass came and the scan
/// missed -- and each of those is a different fix.
#[test]
fn un_aviso_de_rescate_se_apunta_una_vez_se_consume_una_vez_y_se_cuenta() {
    let _p = placed(0b11, 1);
    let (sent_before, taken_before, empty_before) = sched_rescue_kick_stats();

    note_rescue_kick(1);
    note_rescue_kick(1);
    let (sent, _, _) = sched_rescue_kick_stats();
    assert_eq!(sent, sent_before + 2, "los dos avisos se cuentan");

    assert!(take_rescue_kick(), "el primer paso se lleva el aviso");
    assert!(
        !take_rescue_kick(),
        "y el pestillo es de un solo uso: si no, la CPU rescataria en cada \
         paso para siempre"
    );
    let (_, taken, _) = sched_rescue_kick_stats();
    assert_eq!(taken, taken_before + 1);

    note_rescue_kick_empty();
    let (_, _, empty) = sched_rescue_kick_stats();
    assert_eq!(empty, empty_before + 1);
}

/// **A kick for a CPU with no run queue is dropped, and a CPU with no logical
/// id takes none.**
///
/// `lock::current_cpu_id()` answers `NO_CPU` (255) for a CPU whose hardware id
/// never got a dense logical one, and `RESCUE_KICK` is 64 elements: the index
/// is the whole reason both ends check.
#[test]
fn un_aviso_para_una_cpu_que_no_existe_no_entra_en_el_registro() {
    let _p = placed(0b11, u8::MAX);
    let (sent_before, taken_before, _) = sched_rescue_kick_stats();
    note_rescue_kick(MAX_CORE_NUM);
    assert_eq!(
        sched_rescue_kick_stats().0,
        sent_before,
        "ni se cuenta: no hay pestillo donde ponerlo"
    );
    assert!(!take_rescue_kick(), "y esta CPU no tiene pestillo que leer");
    assert_eq!(sched_rescue_kick_stats().1, taken_before);
}

/// **A CPU with no logical id gets no runtime slot built for it either.**
///
/// `warm_runtimes` is called from bare-metal boot right after the guard hooks
/// are registered. `self.slots[255]` on a 64-element array is an index panic
/// -- raised on a CPU that `lock` will not hand a lock slot to either, so the
/// panic path itself is on thin ice. It reports and idles instead.
#[test]
fn una_cpu_sin_id_logico_no_construye_runtime_y_se_informa() {
    let _p = placed(0b1, u8::MAX);
    warm_runtimes();
    assert!(
        GLOBAL_RUNTIME.get(MAX_CORE_NUM).is_none(),
        "no hay hueco para una CPU fuera del array"
    );
}

// ── the counters and latches the trap path reads ───────────────────────────

/// **A CPU marks itself ready when it enters its loop, and that is what
/// `num_online_cpus` reads.**
///
/// The mask is the authority, not a count: a count reported holes as online
/// when a higher AP started before a lower one.
#[test]
fn una_cpu_se_apunta_en_la_mascara_al_entrar_en_su_bucle() {
    let _p = placed(0b1, 0);
    assert!(!is_executor_ready(3));
    mark_executor_ready(3);
    assert!(is_executor_ready(3));
    assert_eq!(num_online_cpus(), 4, "una por encima de la mas alta lista");
    mark_executor_ready(MAX_CORE_NUM);
    assert_eq!(
        executor_ready_mask(),
        0b1001,
        "una CPU fuera del array no cabe en la mascara"
    );
}

/// **The voluntary-yield marker is per CPU and is cleared by hand.**
///
/// It carries the address of the `WakerRef` behind the `Waker` about to be
/// woken, which is what `is_voluntary_yield_wake` compares: a `sched_yield`'s
/// own re-wake must not be counted as a wake that found the CPU busy.
#[test]
fn la_marca_de_cesion_voluntaria_se_pone_y_se_quita_por_cpu() {
    let _p = placed(0b1, 0);
    let waker_id = 0xdead_beef_usize;
    begin_voluntary_yield(waker_id);
    assert!(is_voluntary_yield_wake(waker_id));
    end_voluntary_yield();
    assert!(!is_voluntary_yield_wake(waker_id));
}

/// **A CPU with no logical id has no marker to set either.**
#[test]
fn una_cpu_sin_id_logico_no_tiene_marca_de_cesion() {
    let _p = placed(0b1, u8::MAX);
    begin_voluntary_yield(1);
    end_voluntary_yield();
    assert!(
        !is_voluntary_yield_wake(1),
        "no hay ranura donde escribirla, y leerla contesta que no"
    );
}

/// **A reschedule request is taken exactly once, and the take is counted.**
///
/// Returning `true` twice would have two callers yield for one request. The
/// relaxed pre-check is what keeps this -- which runs on every user trap --
/// from being an RMW on a shared word on the overwhelming majority of traps
/// where the flag is clear.
#[test]
fn una_peticion_de_replanificacion_se_consume_una_sola_vez() {
    let _p = placed(0b1, 0);
    let (_, taken_before, _) = wakeup_preempt_stats();
    assert!(!take_need_resched(), "no hay nada pendiente al empezar");
    request_resched(0);
    assert!(need_resched_pending(), "la peticion esta puesta");
    assert!(take_need_resched());
    assert!(!take_need_resched(), "y ya no esta");
    let (_, taken, _) = wakeup_preempt_stats();
    assert_eq!(taken, taken_before + 1);
}

/// **The IPI counter and the clock hook are the two things the probes cannot
/// invent.**
///
/// Without a clock the wake-latency probe records *nothing at all*, which is
/// the right answer for a build whose HAL never offered one -- never a zero
/// latency, which would read as a scheduler that hands out CPUs instantly.
#[test]
fn el_reloj_se_instala_y_el_contador_de_ipis_se_puede_leer() {
    let _p = placed(0b1, 0);
    fn frozen() -> u64 {
        4_000
    }
    let _ = resched_ipis_sent();
    let saved = NOW_NS_HOOK.load(O::SeqCst);
    set_now_ns_hook(frozen);
    assert_eq!(now_ns(), Some(4_000));
    NOW_NS_HOOK.store(saved, O::SeqCst);
}

/// **A wake serviced on the CPU that published it is counted apart from one
/// serviced across CPUs.**
///
/// The split is the point of the probe: a cross-CPU wake pays an IPI and a
/// cache transfer that a same-CPU wake does not, and a mean that mixes them
/// cannot say which one the tail belongs to.
#[test]
fn un_despertar_servido_en_su_propia_cpu_se_cuenta_aparte() {
    let _p = placed(0b1, 0);
    fn clock() -> u64 {
        9_000
    }
    let saved = NOW_NS_HOOK.swap(clock as *const () as usize, O::SeqCst);
    let (_, _, _, same_before, cross_before, _) = wake_latency_stats();

    note_wake_serviced(pack_wake_stamp(0, 8_000));
    let (_, _, _, same, cross, _) = wake_latency_stats();
    assert_eq!(same, same_before + 1, "publicada y servida en la CPU 0");
    assert_eq!(cross, cross_before);

    note_wake_serviced(pack_wake_stamp(1, 8_000));
    let (_, _, _, same_after, cross_after, _) = wake_latency_stats();
    assert_eq!(same_after, same, "esta la publico otra CPU");
    assert_eq!(cross_after, cross + 1);

    NOW_NS_HOOK.store(saved, O::SeqCst);
}

/// **The dyn-dispatch gate refuses on a suspected heap smash even when the
/// stack is fine.**
///
/// `irq_should_skip_heavy_work` only looks at the current coroutine's depth.
/// Continuing timer `Box<dyn Fn>` work after `[rsp]=0x13446` is how a second
/// `#PF` cascades inside the same callback pass, so once a smash is suspected
/// this skips every tick, not only the idle ones.
#[test]
fn la_puerta_del_despacho_dinamico_cierra_con_el_monton_sospechoso() {
    let _p = placed(0b1, 0);
    assert!(
        !irq_should_skip_dyn_dispatch(),
        "sin sospecha y con pila sana, pasa"
    );
    note_heap_smash_suspected();
    assert!(irq_should_skip_dyn_dispatch());
    HEAP_SMASH_SUSPECTED[0].store(false, O::Relaxed);
    assert!(!irq_should_skip_dyn_dispatch());
}

/// **A per-CPU counter is born at zero and on its own cache line.**
///
/// The alignment is the whole type: `NEED_RESCHED` as a plain array made the
/// most contended line in the kernel out of a counter no CPU ever reads but
/// its owner.
#[test]
fn un_contador_por_cpu_nace_a_cero() {
    let counter = CacheAligned::new();
    assert_eq!(counter.0.load(O::SeqCst), 0);
    assert_eq!(
        core::mem::align_of::<CacheAligned>(),
        64,
        "una linea de cache para cada uno, que es para lo que existe el tipo"
    );
}

/// **A runtime is never dropped, and says so.**
///
/// `GLOBAL_RUNTIME`'s slots are `Once` and nothing ever takes one away: the
/// strong executor's stack is the stack a CPU is standing on. Dropping one
/// would free it under its owner, so the destructor is a deliberate panic
/// rather than a silent unmap.
#[test]
#[should_panic(expected = "drop executor runtime")]
fn tirar_un_runtime_es_un_error_que_se_anuncia() {
    let _g = crate::executor::executor_globals_lock();
    let saved = set_executor_ready_mask_for_test(0);
    let runtime = ExecutorRuntime::new(0);
    set_executor_ready_mask_for_test(saved);
    drop(runtime);
}

/// **A CPU with no logical id steals nothing, and does not index the scratch
/// array it has no slot in.**
///
/// `STEAL_CANDIDATES` is one element per CPU and the victim list lives in this
/// CPU's own, deliberately OFF this stack frame: it used to be a 1 KiB array
/// on the scan's frame whose zero-init blanked a return address a parked
/// executor's saved frame shared with it, which is every captured
/// `[null-exec]`. `cpu_id()` under `MAX_CORE_NUM` is a scheduler invariant, so
/// this never fires in practice; bailing to "no steal this pass" is what it
/// does if it ever did.
#[test]
fn una_cpu_sin_id_logico_no_roba_y_no_indexa_el_bloc_que_no_tiene() {
    let _p = placed(0b11, u8::MAX);
    note_stealable(0);
    assert!(
        steal_task_from_other_cpu().is_none(),
        "sin ranura propia en el bloc del barrido no hay barrido"
    );
}

/// **A peer with work, but less than the bar the caller set, is not a
/// candidate.**
///
/// The rebalance pull asks for `1 + REBALANCE_MARGIN`: its caller already has
/// exactly one runnable task of its own, so pulling off a peer that holds one
/// too would just hand the queue back and forth on every rebalance tick.
#[test]
fn un_igual_con_menos_trabajo_del_que_se_pide_no_es_candidato() {
    let _p = placed(0b11, 1);
    note_stealable(0);
    {
        let rt = GLOBAL_RUNTIME.force(0).lock();
        rt.add_task(DEFAULT_PRIORITY, parked(), None);
    }
    let (_, probed_before, _, _, _, _, _) = sched_steal_stats();
    assert!(
        steal_for_balance().is_none(),
        "una tarea no es «claramente mas rico» que ninguna"
    );
    assert_eq!(
        sched_steal_stats().1,
        probed_before,
        "y al no ser candidato no se le sonda la cola: el reparto no toca su \
         cerrojo de coleccion"
    );
}

/// **With two victims the thief goes to the richer one first.**
///
/// Ranking is on *runnable tasks we may poll*, descending. A plain ascending
/// sweep by cpu id sent every idle core at the same lowest-id victim first,
/// and all but one of them lost the `try_lock` and walked away empty; and
/// ranking by total tasks put a CPU full of blocked daemons ahead of the
/// genuinely backlogged one.
#[test]
fn con_dos_victimas_el_ladron_va_primero_a_la_mas_rica() {
    let _p = placed(0b111, 2);
    reset_cpu(1);
    {
        let poor = GLOBAL_RUNTIME.force(0).lock();
        poor.add_task(DEFAULT_PRIORITY, parked(), None);
        let rich = GLOBAL_RUNTIME.force(1).lock();
        for _ in 0..4 {
            rich.add_task(DEFAULT_PRIORITY, parked(), None);
        }
    }
    note_stealable(0);
    note_stealable(1);

    assert!(steal_task_from_other_cpu().is_some());

    assert_eq!(
        GLOBAL_RUNTIME.force(0).lock().ready_num(),
        Some(1),
        "la victima pobre sigue con la suya: no se la toco"
    );
    assert_eq!(
        GLOBAL_RUNTIME.force(1).lock().ready_num(),
        Some(3),
        "la tarea salio de la rica, que es a la que se fue primero"
    );
}

/// **With no CPU named and two CPUs ready, the task goes to the emptier
/// one.**
///
/// The scan keeps the best seen so far, so a later candidate that is not
/// better has to leave it alone -- the half of the comparison a single-CPU
/// machine can never exercise.
#[test]
fn con_dos_cpus_listas_la_tarea_nace_en_la_menos_cargada() {
    let _p = placed(0b11, 0);
    reset_cpu(1);
    {
        let busy = GLOBAL_RUNTIME.force(0).lock();
        for _ in 0..3 {
            busy.add_task(DEFAULT_PRIORITY, parked(), None);
        }
    }
    spawn_task(parked(), None, None, None);
    assert_eq!(
        GLOBAL_RUNTIME.force(1).lock().task_num(),
        1,
        "la CPU 1 estaba vacia y es donde tenia que caer"
    );
    assert_eq!(
        GLOBAL_RUNTIME.force(0).lock().task_num(),
        3,
        "la 0 no crecio"
    );
}
