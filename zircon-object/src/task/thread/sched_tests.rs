//! The scheduling tests of `Thread`: slices, the wake-up preemption floor,
//! the lag a parked thread keeps, and the user-mode share that decides who
//! waits behind the woken (`Thread::sched_took_more_than_half`).

use super::*;
use crate::task::*;

fn a_thread() -> Arc<Thread> {
    let root = Job::root();
    let proc = Process::create(&root, "proc").expect("un proceso");
    Thread::create(&proc, "thread").expect("un hilo")
}

/// `sched_prio_to_weight[]` as Linux ships it (`kernel/sched/core.c`).
/// Copied out on purpose rather than computed: a typo in a table is exactly
/// what a table test is for, and a wrong entry is a thread that quietly
/// gets the CPU share of a different nice level.
const LINUX_WEIGHTS: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

#[test]
fn the_weight_table_is_the_one_linux_ships() {
    assert_eq!(
        SCHED_PRIO_TO_WEIGHT, LINUX_WEIGHTS,
        "la tabla de pesos no es la de Linux"
    );
    for (i, w) in LINUX_WEIGHTS.iter().enumerate() {
        let nice = i as i32 - 20;
        assert_eq!(
            nice_to_weight(nice as i8),
            *w,
            "el peso de nice {} no es el de Linux",
            nice
        );
    }
    assert_eq!(nice_to_weight(0), WEIGHT_NICE0, "nice 0 no pesa 1024");
    // Fuera del rango se recorta, no se sale de la tabla.
    assert_eq!(
        nice_to_weight(-100),
        nice_to_weight(MIN_NICE),
        "nice muy bajo no se recorta"
    );
    assert_eq!(
        nice_to_weight(100),
        nice_to_weight(MAX_NICE),
        "nice muy alto no se recorta"
    );
}

#[test]
fn a_nice_zero_fair_thread_gets_the_base_slice() {
    let t = a_thread();
    assert_eq!(
        t.sched_policy(),
        SCHED_NORMAL,
        "un hilo nuevo no nace SCHED_NORMAL"
    );
    assert_eq!(t.sched_nice(), 0, "un hilo nuevo no nace con nice 0");
    assert_eq!(
        t.sched_rt_priority(),
        0,
        "un hilo nuevo nace con prioridad de tiempo real"
    );
    assert!(!t.sched_is_realtime());
    assert_eq!(
        t.timeslice_ns(),
        BASE_TIMESLICE_NS,
        "nice 0 no da el slice base"
    );
}

/// One nice step is ~25% of load weight, i.e. ~10% of CPU: that is the whole
/// point of `nice`, and a slice that does not move with it makes `nice` a
/// number that is reported and ignored.
#[test]
fn one_nice_step_moves_the_slice_by_about_a_quarter() {
    let t = a_thread();
    for nice in -4..=4i8 {
        t.set_sched(SCHED_NORMAL, nice, 0);
        let lo = t.timeslice_ns();
        t.set_sched(SCHED_NORMAL, nice - 1, 0);
        let hi = t.timeslice_ns();
        assert!(
            hi > lo,
            "nice {} da un slice de {} ns y nice {} otro de {} ns: no baja al subir nice",
            nice - 1,
            hi,
            nice,
            lo
        );
        // 88/64 = 1.375 y 72/64 = 1.125 acotan el 25% de la tabla con holgura.
        assert!(
            hi * 64 > lo * 72 && hi * 64 < lo * 88,
            "de nice {} a nice {} el slice pasa de {} a {} ns, que no es el ~25% de la tabla",
            nice,
            nice - 1,
            lo,
            hi
        );
    }
}

#[test]
fn the_slice_is_clamped_at_both_ends() {
    let t = a_thread();
    t.set_sched(SCHED_NORMAL, MIN_NICE, 0);
    assert_eq!(
        t.timeslice_ns(),
        MAX_TIMESLICE_NS,
        "nice -20 no se recorta por arriba"
    );
    t.set_sched(SCHED_NORMAL, MAX_NICE, 0);
    assert_eq!(
        t.timeslice_ns(),
        MIN_TIMESLICE_NS,
        "nice 19 no se recorta por abajo"
    );
    assert!(MIN_TIMESLICE_NS < BASE_TIMESLICE_NS && BASE_TIMESLICE_NS < MAX_TIMESLICE_NS);
}

#[test]
fn every_policy_gets_the_slice_it_is_promised() {
    let t = a_thread();
    t.set_sched(SCHED_FIFO, 0, 50);
    assert_eq!(t.timeslice_ns(), u64::MAX, "un FIFO se reparte el tiempo");
    assert!(t.sched_is_realtime());
    assert!(
        !t.tick_should_preempt(),
        "un FIFO se desaloja por fin de slice"
    );

    t.set_sched(SCHED_RR, 0, 50);
    assert_eq!(
        t.timeslice_ns(),
        RR_TIMESLICE_NS,
        "un RR no da el cuanto de 100 ms"
    );
    assert!(t.sched_is_realtime());

    t.set_sched(SCHED_IDLE, 0, 0);
    assert_eq!(
        t.timeslice_ns(),
        MIN_TIMESLICE_NS,
        "un IDLE no da el slice mas corto"
    );
    assert!(!t.sched_is_realtime());

    // BATCH y DEADLINE se planifican como NORMAL: el nice cuenta.
    for policy in [SCHED_BATCH, SCHED_DEADLINE] {
        t.set_sched(policy, 0, 0);
        assert_eq!(
            t.timeslice_ns(),
            BASE_TIMESLICE_NS,
            "la politica {} no se planifica como NORMAL",
            policy
        );
        assert!(
            !t.sched_is_realtime(),
            "la politica {} sale como tiempo real",
            policy
        );
    }
}

#[test]
fn changing_the_policy_drops_the_slice_that_was_running() {
    let t = a_thread();
    t.sched.slice_end_ns.store(1_234_567, Ordering::Relaxed);
    t.set_sched(SCHED_NORMAL, 5, 0);
    assert_eq!(
        t.sched.slice_end_ns.load(Ordering::Relaxed),
        0,
        "cambiar la politica deja corriendo el slice de la anterior"
    );
    assert_eq!(t.sched_nice(), 5);
}

const SLICE: u64 = 20_000_000;

#[test]
fn the_first_tick_of_a_thread_starts_its_slice_instead_of_ending_it() {
    let now = 1_000_000_000;
    assert_eq!(
        slice_verdict(now, 0, SLICE),
        (false, now + SLICE),
        "el primer tick de un hilo lo desaloja"
    );
}

#[test]
fn a_tick_inside_the_slice_leaves_the_deadline_where_it_was() {
    let now = 1_000_000_000;
    let end = now + 5_000_000;
    assert_eq!(
        slice_verdict(now, end, SLICE),
        (false, end),
        "un tick a mitad de slice mueve la fecha limite o desaloja"
    );
}

#[test]
fn a_tick_past_the_deadline_preempts_and_starts_the_next_slice() {
    let now = 1_000_000_000;
    // Un tick de planificador (4 ms) despues de vencer: es como se ve de verdad.
    let end = now - 4_000_000;
    assert_eq!(
        slice_verdict(now, end, SLICE),
        (true, now + SLICE),
        "un slice vencido no desaloja"
    );
    // Y justo en la fecha limite, tambien.
    assert_eq!(slice_verdict(now, now, SLICE), (true, now + SLICE));
}

/// Where the line is. A tick that arrives a whole slice late is still a
/// thread that was running -- a CPU busy with interrupts delays the
/// scheduler tick -- so it is preempted. One nanosecond further back is a
/// span no running thread can have gone unticked, so it slept.
#[test]
fn a_tick_a_whole_slice_late_is_still_the_slice_running_out() {
    let now = 1_000_000_000;
    assert_eq!(
        slice_verdict(now, now - SLICE, SLICE),
        (true, now + SLICE),
        "un tick que llega un slice tarde se toma por un hilo que despierta"
    );
    assert_eq!(
        slice_verdict(now, now - SLICE - 1, SLICE),
        (false, now + SLICE),
        "un nanosegundo mas atras todavia se toma por un slice agotado"
    );
}

/// The bug. A thread that blocked keeps the deadline it had before it went
/// to sleep, so waking up looked exactly like running out of time.
#[test]
fn a_thread_that_slept_longer_than_its_slice_is_not_preempted_the_moment_it_wakes() {
    let now = 2_000_000_000;
    for slept in [SLICE + 1, 100_000_000, 1_000_000_000, now] {
        let end = now - slept;
        assert_eq!(
            slice_verdict(now, end, SLICE),
            (false, now + SLICE),
            "un hilo que durmio {} ns con un slice de {} ns se desaloja al despertar",
            slept,
            SLICE
        );
    }
}

#[test]
fn a_deadline_further_out_than_a_whole_slice_is_not_believed() {
    let now = 1_000_000_000;
    let end = now + SLICE + 1;
    assert_eq!(
        slice_verdict(now, end, SLICE),
        (false, now + SLICE),
        "una fecha limite de un reloj que fue hacia atras se cree"
    );
    // Justo un slice por delante si es creible: es lo que acabamos de poner.
    let end = now + SLICE;
    assert_eq!(slice_verdict(now, end, SLICE), (false, end));
}

#[test]
fn a_deadline_at_the_end_of_the_clock_does_not_wrap_around() {
    let now = u64::MAX - 1;
    let (preempt, next) = slice_verdict(now, now - 1, SLICE);
    assert!(preempt, "un slice vencido al final del reloj no desaloja");
    assert_eq!(
        next,
        u64::MAX,
        "la fecha limite da la vuelta en vez de saturar"
    );
    assert_eq!(
        slice_verdict(u64::MAX, 0, SLICE),
        (false, u64::MAX),
        "el primer slice al final del reloj da la vuelta"
    );
}

/// The fix the clock cannot do on its own: a wait SHORTER than a slice.
///
/// A media thread runs 15 ms of its 20 ms slice, waits 8 ms for its audio
/// callback and wakes. Its deadline is 3 ms in the past, which is exactly
/// what a late tick looks like, so `slice_verdict` preempts it --
/// correctly, on the evidence it has. The park is the evidence it does not
/// have.
///
/// In the real order the park happens while the thread is still the one
/// running, with 5 ms of slice still in hand, and that is the remainder the
/// resumption gets back (see [`resume_slice_end`]). What #1588 is about is
/// the assertion at the end: the first tick after waking does not preempt.
#[test]
fn a_wait_shorter_than_a_slice_does_not_cost_the_thread_its_first_tick() {
    let t = a_thread();
    let slice = t.timeslice_ns();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    // Sin la marca, una fecha limite 3 ms atras es un slice agotado y es lo
    // unico que el reloj puede ver.
    assert!(
        slice_verdict(now, now.saturating_sub(3_000_000), slice).0,
        "el reloj deberia ver esto como un slice agotado"
    );
    // El orden de produccion: aparca con 5 ms de slice por gastar.
    t.sched
        .slice_end_ns
        .store(now.saturating_add(5_000_000), Ordering::Relaxed);
    t.sched_note_parked();
    assert!(
        t.sched.slice_left_ns.load(Ordering::Relaxed) > 4_000_000,
        "el aparcado no se quedo con el resto del slice"
    );
    t.sched_note_resumed();
    let end = t.sched.slice_end_ns.load(Ordering::Relaxed);
    assert!(
        end > kernel_hal::timer::timer_now().as_nanos() as u64,
        "despertar de una espera corta no devuelve el resto que le quedaba"
    );
    assert!(
        !t.tick_should_preempt(),
        "y el primer tick tras despertar sigue quitandole la CPU"
    );
}

/// The renewal this closes: parking with the slice already spent does NOT
/// hand out a new one. A thread that burns 20 ms, parks on a `read` whose
/// data is already there and is polled again microseconds later used to
/// come back with a full fresh slice, and could repeat that forever.
#[test]
fn a_thread_that_spent_its_slice_cannot_renew_it_with_a_zero_length_park() {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    // Aparca con el slice recien agotado: no le queda nada.
    t.sched.slice_end_ns.store(now, Ordering::Relaxed);
    t.sched_note_parked();
    t.sched_note_resumed();
    assert!(
        t.tick_should_preempt(),
        "un park de duracion cero le renovo el slice: se queda la CPU para siempre"
    );
}

/// `set_sched` drops the carried remainder too: it was measured against the
/// slice the old nice gave.
#[test]
fn a_renice_while_parked_does_not_carry_the_old_slices_remainder() {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.sched
        .slice_end_ns
        .store(now.saturating_add(5_000_000), Ordering::Relaxed);
    t.sched_note_parked();
    t.set_sched(SCHED_NORMAL, 19, 0);
    assert_eq!(
        t.sched.slice_left_ns.load(Ordering::Relaxed),
        NO_SLICE_REMAINDER,
        "el resto del slice viejo sobrevivio al renice"
    );
    t.sched_note_resumed();
    assert_eq!(
        t.sched.slice_end_ns.load(Ordering::Relaxed),
        0,
        "tras un renice el despertar tiene que empezar slice nuevo"
    );
}

const LAG_SLICE: u64 = 20_000_000;

#[test]
fn a_park_with_nothing_recorded_still_gets_a_fresh_slice() {
    assert_eq!(
        resume_slice_end(1_000, NO_SLICE_REMAINDER, 0, LAG_SLICE),
        0,
        "sin resto apuntado el despertar no puede cobrar nada"
    );
}

#[test]
fn a_sleep_as_long_as_the_part_it_ran_pays_the_lag_off() {
    // Gasto 15 ms de 20, durmio 15 ms: a la par.
    let left = 5_000_000;
    assert_eq!(
        resume_slice_end(1_000_000_000, left, 15_000_000, LAG_SLICE),
        0,
        "dormir lo que corrio tiene que dar slice nuevo"
    );
    // Un nanosegundo menos y todavia debe.
    assert_ne!(
        resume_slice_end(1_000_000_000, left, 15_000_000 - 1, LAG_SLICE),
        0,
        "la frontera del lag esta en el nanosegundo equivocado"
    );
}

#[test]
fn a_sleep_shorter_than_the_part_it_ran_only_gets_the_remainder_back() {
    let now = 1_000_000_000;
    let left = 5_000_000;
    assert_eq!(
        resume_slice_end(now, left, 1_000, LAG_SLICE),
        now + left,
        "un park corto tiene que devolver el resto, no un slice entero"
    );
}

#[test]
fn a_park_that_consumed_nothing_is_free() {
    assert_eq!(
        resume_slice_end(1_000_000_000, LAG_SLICE, 0, LAG_SLICE),
        0,
        "un hilo que no gasto nada no debe nada"
    );
}

/// A remainder longer than the slice can only come from a slice that shrank
/// under it; it is clamped rather than handed out.
#[test]
fn a_remainder_longer_than_the_slice_is_clamped() {
    assert_eq!(
        resume_slice_end(1_000_000_000, LAG_SLICE * 4, 0, LAG_SLICE),
        0,
        "un resto mayor que el slice tendria que recortarse, no regalarse"
    );
}

/// Time zero with nothing left: 0 is the "no slice running" sentinel, so
/// the deadline has to come out as something else or the thread is handed a
/// fresh slice by accident.
#[test]
fn a_spent_slice_at_time_zero_is_not_read_as_a_fresh_one() {
    assert_ne!(
        resume_slice_end(0, 0, 0, LAG_SLICE),
        0,
        "un slice agotado en el instante 0 se confunde con no tener slice"
    );
}

/// The share `sched_took_more_than_half` reads: none for a window under
/// a tick, and never more than all of the window.
#[test]
fn a_share_is_read_only_from_a_window_a_tick_old() {
    let start = 1_000_000_000;
    assert_eq!(
        share_pm(start, 1_000, start + SCHED_TICK_NS - 1),
        None,
        "una ventana de menos de un tick ya da una cuota"
    );
    assert_eq!(share_pm(0, 1_000, start), None, "sin ventana hay cuota");
    assert_eq!(
        share_pm(start, SCHED_TICK_NS / 2, start + SCHED_TICK_NS),
        Some(500)
    );
    assert_eq!(
        share_pm(start, 3 * SCHED_TICK_NS, start + SCHED_TICK_NS),
        Some(1000),
        "una cuota de más de toda la ventana"
    );
}

#[test]
fn a_wake_inside_the_base_slice_does_not_take_the_cpu() {
    let now = 1_000_000_000;
    // Corrio 100 us de 20 ms: dentro del suelo.
    let end = now + LAG_SLICE - 100_000;
    assert!(
        !wake_preempt_allowed(now, end, LAG_SLICE, BASE_SLICE_NS),
        "un despertar a los 100 us le quita la CPU al que corre"
    );
}

#[test]
fn a_wake_past_the_base_slice_takes_the_cpu() {
    let now = 1_000_000_000;
    let end = now + LAG_SLICE - BASE_SLICE_NS;
    assert!(
        wake_preempt_allowed(now, end, LAG_SLICE, BASE_SLICE_NS),
        "justo en el suelo el despertar ya tiene que pasar"
    );
    assert!(
        !wake_preempt_allowed(now, end + 1, LAG_SLICE, BASE_SLICE_NS),
        "el suelo esta un nanosegundo torcido"
    );
}

/// Every case where the run length cannot be read off the deadline answers
/// `true`: an unknown must never become latency for the woken task.
#[test]
fn a_run_length_that_cannot_be_read_lets_the_wake_through() {
    let now = 1_000_000_000;
    assert!(
        wake_preempt_allowed(now, 0, LAG_SLICE, BASE_SLICE_NS),
        "sin slice apuntado el despertar tiene que pasar"
    );
    assert!(
        wake_preempt_allowed(now, now + 1, u64::MAX, BASE_SLICE_NS),
        "SCHED_FIFO no se mide con este suelo"
    );
    assert!(
        wake_preempt_allowed(now, now, LAG_SLICE, BASE_SLICE_NS),
        "un slice ya vencido no protege a nadie"
    );
    assert!(
        wake_preempt_allowed(now, now + LAG_SLICE + 1, LAG_SLICE, BASE_SLICE_NS),
        "una fecha limite mas lejos que un slice entero no es creible"
    );
}

/// A wake the floor lets through needs no timer: it is honoured on this
/// very trap, so there is nothing to come back for.
#[test]
fn a_wake_that_is_allowed_asks_for_no_timer() {
    let now = 1_000_000_000;
    let end = now + LAG_SLICE - BASE_SLICE_NS;
    assert_eq!(
        wake_preempt_floor_end(now, end, LAG_SLICE, BASE_SLICE_NS),
        None,
        "justo en el suelo el despertar pasa, no hay nada que armar"
    );
    assert_eq!(
        wake_preempt_floor_end(now, 0, LAG_SLICE, BASE_SLICE_NS),
        None
    );
    assert_eq!(
        wake_preempt_floor_end(now, now + 1, u64::MAX, BASE_SLICE_NS),
        None
    );
    assert_eq!(
        wake_preempt_floor_end(now, now, LAG_SLICE, BASE_SLICE_NS),
        None
    );
}

/// A wake the floor denies asks for a timer at the floor, which is
/// `BASE_SLICE_NS` after the slice started -- not at the next tick, which
/// is what a thread in a userspace loop used to wait for.
#[test]
fn a_wake_the_floor_denies_asks_for_a_timer_at_the_floor() {
    let now = 1_000_000_000;
    // Corrio 100 us de 20 ms: dentro del suelo, asi que el suelo acaba
    // 650 us mas adelante.
    let end = now + LAG_SLICE - 100_000;
    let started = end - LAG_SLICE;
    assert_eq!(
        wake_preempt_floor_end(now, end, LAG_SLICE, BASE_SLICE_NS),
        Some(started + BASE_SLICE_NS),
        "el temporizador tiene que caer en el suelo"
    );
    let floor = wake_preempt_floor_end(now, end, LAG_SLICE, BASE_SLICE_NS).unwrap();
    assert_eq!(
        floor - now,
        BASE_SLICE_NS - 100_000,
        "650 us desde ahora, no los 4 ms del tick"
    );
    assert!(floor > now, "un suelo detras del reloj no arma nada");
    assert!(floor < end, "el suelo no puede pasarse del fin del slice");
}

/// A thread that has consumed nothing yet asks for the whole floor, and
/// one a nanosecond short of it asks for that nanosecond.
#[test]
fn the_floor_is_measured_from_where_the_slice_started() {
    let now = 1_000_000_000;
    assert_eq!(
        wake_preempt_floor_end(now, now + LAG_SLICE, LAG_SLICE, BASE_SLICE_NS),
        Some(now + BASE_SLICE_NS),
        "sin consumir nada, el suelo entero"
    );
    let end = now + LAG_SLICE - BASE_SLICE_NS + 1;
    assert_eq!(
        wake_preempt_floor_end(now, end, LAG_SLICE, BASE_SLICE_NS),
        Some(now + 1),
        "a un nanosegundo del suelo, un nanosegundo"
    );
}

/// A floor wider than the whole slice must not be armed past the
/// preemption that is coming anyway.
#[test]
fn the_floor_never_lands_past_the_end_of_the_slice() {
    let now = 1_000_000_000;
    let slice = 500_000;
    let end = now + slice - 1;
    let floor = wake_preempt_floor_end(now, end, slice, BASE_SLICE_NS).unwrap();
    assert_eq!(floor, end, "un suelo mas ancho que el slice se recorta");
}

/// `SCHED_IDLE` yields to anything that wakes, as in Linux: the floor is
/// for threads whose CPU share is worth defending.
#[test]
fn an_idle_policy_thread_yields_to_any_wake() {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.set_sched(SCHED_IDLE, 0, 0);
    t.sched
        .slice_end_ns
        .store(now.saturating_add(t.timeslice_ns()), Ordering::Relaxed);
    assert!(
        t.sched_may_preempt_on_wake(),
        "un hilo SCHED_IDLE se queda la CPU ante un despertar"
    );
}

/// And a fair thread that has just started its slice keeps it.
#[test]
fn a_fair_thread_that_just_started_its_slice_keeps_it_through_a_wake() {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.sched
        .slice_end_ns
        .store(now.saturating_add(t.timeslice_ns()), Ordering::Relaxed);
    assert!(
        !t.sched_may_preempt_on_wake(),
        "el primer BASE_SLICE_NS del slice no es suyo"
    );
}

/// The wake that was denied gets a timer for when the floor ends, and only
/// a thread the floor applies to gets one: `SCHED_IDLE` is never denied,
/// and `SCHED_FIFO` has no slice for a floor to sit in.
#[test]
fn only_a_thread_the_floor_applies_to_gets_a_floor_end() {
    let just_started = |t: &Thread| {
        let now = kernel_hal::timer::timer_now().as_nanos() as u64;
        t.sched
            .slice_end_ns
            .store(now.saturating_add(t.timeslice_ns()), Ordering::Relaxed);
    };
    let fair = a_thread();
    just_started(&fair);
    assert!(
        fair.sched_wake_preempt_floor_end().is_some(),
        "al hilo justo se le deniega el despertar y nadie le pone temporizador"
    );

    let idle = a_thread();
    idle.set_sched(SCHED_IDLE, 0, 0);
    just_started(&idle);
    assert_eq!(
        idle.sched_wake_preempt_floor_end(),
        None,
        "a SCHED_IDLE nunca se le deniega nada"
    );

    let fifo = a_thread();
    fifo.set_sched(SCHED_FIFO, 0, 50);
    just_started(&fifo);
    assert_eq!(
        fifo.sched_wake_preempt_floor_end(),
        None,
        "SCHED_FIFO no tiene slice"
    );
}

#[test]
fn a_poll_that_never_parked_does_not_help_itself_to_a_fresh_slice() {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    let end = now.saturating_sub(4_000_000);
    t.sched.slice_end_ns.store(end, Ordering::Relaxed);
    // Un future que estaba listo a la primera no aparca, asi que resumed()
    // no tiene nada que perdonar.
    t.sched_note_resumed();
    assert_eq!(
        t.sched.slice_end_ns.load(Ordering::Relaxed),
        end,
        "un poll que no aparco se regala un slice"
    );
    assert!(
        t.tick_should_preempt(),
        "y ademas se queda la CPU con el slice agotado"
    );
}

#[test]
fn a_park_is_forgiven_once_and_not_twice() {
    let t = a_thread();
    t.sched_note_parked();
    t.sched_note_resumed();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    let end = now.saturating_sub(4_000_000);
    t.sched.slice_end_ns.store(end, Ordering::Relaxed);
    // El segundo resumed() sin un parked() por delante no hace nada: si lo
    // hiciera, un hilo que nunca se bloquea no se desalojaria jamas.
    t.sched_note_resumed();
    assert_eq!(
        t.sched.slice_end_ns.load(Ordering::Relaxed),
        end,
        "un solo aparcado perdona dos despertares"
    );
}

/// The whole thing through the thread, with the clock it really uses.
#[test]
fn a_thread_that_just_woke_keeps_the_cpu_and_one_that_ran_out_does_not() {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    // Lo que deja un hilo que se bloqueo un segundo.
    t.sched
        .slice_end_ns
        .store(now.saturating_sub(1_000_000_000), Ordering::Relaxed);
    assert!(
        !t.tick_should_preempt(),
        "un hilo que acaba de despertar pierde la CPU en su primer tick"
    );
    assert!(
        t.sched.slice_end_ns.load(Ordering::Relaxed) > now,
        "y no se le ha dado un slice nuevo"
    );
    // Lo que deja un hilo que si agoto su slice: vencio hace un tick.
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.sched
        .slice_end_ns
        .store(now.saturating_sub(4_000_000), Ordering::Relaxed);
    assert!(
        t.tick_should_preempt(),
        "un hilo que agoto su slice no lo suelta"
    );
}

const CRED_SLICE: u64 = 20_000_000;

/// A gap no longer than a tick is time the thread may have been running, so
/// it stays charged. This is the common case -- every tick of a thread that
/// holds its CPU -- and it must cost nothing.
#[test]
fn a_gap_within_one_tick_is_charged_as_time_on_the_cpu() {
    let now = 100_000_000;
    let end = now + 18_000_000;
    for gap in [1, 100_000, SCHED_TICK_NS - 1, SCHED_TICK_NS] {
        assert_eq!(
            credited_slice_end(now, end, now - gap, CRED_SLICE, SCHED_TICK_NS, 0),
            end,
            "un hueco de {} ns no es tiempo fuera de la CPU",
            gap
        );
    }
}

/// The frozen frame: the excess over a tick is time the thread was not on
/// the CPU, and it goes back on the deadline.
#[test]
fn the_part_of_the_gap_past_one_tick_goes_back_on_the_deadline() {
    // Un hilo que corrio 2 ms de sus 20 y se quedo congelado 19.
    let started = 100_000_000;
    let ran = 2_000_000;
    let frozen = 19_000_000;
    let end = started + CRED_SLICE;
    let last_seen = started + ran;
    let now = last_seen + frozen;
    assert_eq!(
        credited_slice_end(now, end, last_seen, CRED_SLICE, SCHED_TICK_NS, 0),
        end + (frozen - SCHED_TICK_NS),
        "el tiempo congelado no se le devuelve"
    );
    // Sin el credito el plazo esta vencido y el tick lo expulsa tras haber
    // corrido 2 ms de 20, que es el fallo. Con el, sigue dentro del slice.
    assert!(
        slice_verdict(now, end, CRED_SLICE).0,
        "sin el credito el tick no lo expulsaba y el caso no es el que creo"
    );
    let credited = credited_slice_end(now, end, last_seen, CRED_SLICE, SCHED_TICK_NS, 0);
    assert!(
        !slice_verdict(now, credited, CRED_SLICE).0,
        "con el credito todavia lo expulsa"
    );
}

/// The credit is capped at one whole slice in hand, whatever the gap. A core
/// with no periodic tick could otherwise hand out a deadline
/// `slice_verdict` reads as a backwards clock, and a long freeze must not
/// buy more CPU than the slice is worth.
#[test]
fn the_credit_never_leaves_more_than_one_slice_in_hand() {
    let now = 1_000_000_000;
    let end = now + 1;
    for frozen in [CRED_SLICE, 10 * CRED_SLICE, now] {
        let credited = credited_slice_end(now, end, now - frozen, CRED_SLICE, SCHED_TICK_NS, 0);
        assert!(
            credited <= now + CRED_SLICE,
            "un congelado de {} ns deja mas de un slice en mano",
            frozen
        );
        assert!(
            !slice_verdict(now, credited, CRED_SLICE).0,
            "y el plazo acreditado tiene que seguir siendo uno que este reloj pudo poner"
        );
    }
}

/// Nothing to compare against means no credit: a fresh slice (`end == 0`), a
/// resumption that already settled up (`last_seen == 0`), or a clock that
/// went backwards across a migration.
#[test]
fn with_nothing_to_compare_against_there_is_no_credit() {
    let now = 100_000_000;
    assert_eq!(
        credited_slice_end(now, 0, now - 50_000_000, CRED_SLICE, SCHED_TICK_NS, 0),
        0,
        "un slice sin empezar no se acredita"
    );
    let end = now + 1_000_000;
    assert_eq!(
        credited_slice_end(now, end, 0, CRED_SLICE, SCHED_TICK_NS, 0),
        end,
        "una reanudacion ya liquido la ausencia; pagarla otra vez es pagarla dos veces"
    );
    assert_eq!(
        credited_slice_end(now, end, now, CRED_SLICE, SCHED_TICK_NS, 0),
        end,
        "sin hueco no hay credito"
    );
    assert_eq!(
        credited_slice_end(now, end, now + 1, CRED_SLICE, SCHED_TICK_NS, 0),
        end,
        "una observacion en el futuro es un reloj que fue hacia atras, no un credito"
    );
}

/// A resumption clears the observation, so the sleep the lag rule just paid
/// for is not credited a second time at the next tick.
#[test]
fn a_resumption_clears_the_observation_it_already_paid_for() {
    let t = a_thread();
    t.sched.last_tick_ns.store(1, Ordering::Relaxed);
    t.sched.parked.store(true, Ordering::Relaxed);
    t.sched
        .slice_left_ns
        .store(NO_SLICE_REMAINDER, Ordering::Relaxed);
    t.sched_note_resumed();
    assert_eq!(
        t.sched.last_tick_ns.load(Ordering::Relaxed),
        0,
        "la reanudacion deja la observacion vieja en pie y el credito se paga dos veces"
    );
    // And a `set_sched` drops it with the slice it belonged to.
    t.sched.last_tick_ns.store(1, Ordering::Relaxed);
    t.set_sched(SCHED_NORMAL, 5, 0);
    assert_eq!(t.sched.last_tick_ns.load(Ordering::Relaxed), 0);
}

/// The tick that did not fire is not a thread that did not run. A busy CPU
/// whose tick is late makes every gap on it look like an absence, and
/// crediting that hands a thread that was running the whole time an extra
/// slice -- the opposite of a preemption, on a machine that is already slow.
#[test]
fn a_tick_that_did_not_fire_is_not_time_off_the_cpu() {
    // Un hilo que corrio 10 ms de sus 20 y entonces esta CPU, ocupada, se
    // quedo 15 ms sin tick. El hilo estuvo corriendo todo ese rato: la
    // rodaja se agoto y le toca soltar la CPU.
    let started = 100_000_000;
    let end = started + CRED_SLICE;
    let last_seen = started + 10_000_000;
    let cpu_gap = 15_000_000;
    let now = last_seen + cpu_gap;
    assert_eq!(
        credited_slice_end(now, end, last_seen, CRED_SLICE, SCHED_TICK_NS, cpu_gap),
        end,
        "el hueco lo explica el tick que falto, no una ausencia del hilo"
    );
    assert!(
        slice_verdict(now, end, CRED_SLICE).0,
        "y por tanto el tick si lo expulsa"
    );
    // Sin el dato de la CPU el plazo se iba por delante del reloj y el hilo
    // seguia corriendo: lo contrario de una expulsion, en una maquina que ya
    // va lenta. Es el regalo que este test existe para quitar, y cae en la
    // banda de 12 a 20 ms, que es donde la red de seguridad de
    // `slice_verdict` (pasarse de una rodaja entera) todavia no llega.
    let sin_dato = credited_slice_end(now, end, last_seen, CRED_SLICE, SCHED_TICK_NS, 0);
    assert!(
        !slice_verdict(now, sin_dato, CRED_SLICE).0,
        "sin el dato de la CPU el tick no lo expulsaba"
    );
    // Y lo que el hueco de la CPU NO explica se sigue devolviendo: eso es
    // una ausencia de verdad.
    let mas_tarde = now + 6_000_000;
    assert_eq!(
        credited_slice_end(
            mas_tarde,
            end,
            last_seen,
            CRED_SLICE,
            SCHED_TICK_NS,
            cpu_gap
        ),
        end + 6_000_000,
        "la parte del hueco que el tick no explica si es ausencia"
    );
}

/// What gets counted is the credit actually given, and nothing is counted
/// when none was. A zero here over a slow stretch is what rules this whole
/// path out, so it had better not count ticks that were never credited.
#[test]
fn only_a_credit_that_was_given_is_counted() {
    assert_eq!(credit_given(1_000, 1_007), Some(7));
    assert_eq!(
        credit_given(1_000, 1_000),
        None,
        "un tick sin credito no es un credito, y contarlo deja sin cero que leer"
    );
    assert_eq!(credit_given(1_000, 999), None, "un plazo recortado tampoco");
    // Cifras propias, no las globales del binario: si fueran las globales,
    // el incremento de otro test valdria por el que esta mutacion quita.
    let c = SliceCredit::new();
    c.note(1_000, 1_000 + 1_234_567);
    // Despues del mayor, a proposito: el peor congelado se queda, no se
    // sobreescribe con el ultimo.
    c.note(1_000, 1_000 + 7);
    c.note(1_000, 1_000);
    c.note(1_000, 999);
    c.note_tick();
    assert_eq!(
        c.read(),
        (1, 2, 1_234_567 + 7, 1_234_567),
        "ticks, creditos, nanosegundos dados y el peor congelado"
    );
}

/// The whole thing through the thread: a tick records where the clock
/// stood, so the next one can tell running from frozen.
#[test]
fn a_tick_records_the_observation_the_next_one_compares_against() {
    let t = a_thread();
    t.sched.last_tick_ns.store(0, Ordering::Relaxed);
    let before = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.tick_should_preempt();
    let seen = t.sched.last_tick_ns.load(Ordering::Relaxed);
    assert!(
        seen >= before,
        "el tick no apunta el reloj y el siguiente no puede distinguir congelado de corriendo"
    );
}
