//! Lo que un `Executor` contesta y lo que sus trabas dejan dicho, sin pila
//! detras.
//!
//! `Executor::new` no se puede llamar aqui: reserva 2,6 MiB de pila y
//! `init_stack_and_context` lee `cr3`, una instruccion privilegiada que en un
//! proceso de usuario es un SIGSEGV antes de que el test compruebe nada (ver
//! las notas de `stack_birth_benches`). Asi que todo lo de este fichero sale
//! del literal [`super::metadata_only_executor`], que es la unica costura por
//! la que se pueden leer los observadores (`is_polling`, `stack_contains`,
//! `canary_intact`) y accionar las trabas de contencion
//! (`abandon_idle_executor`, `force_replace_executor`,
//! `abandon_current_task`).
//!
//! Son justo los que el camino del panico consulta para decidir si descarta
//! una tarea, un ejecutor entero, o nada: cada uno de ellos contestando de mas
//! es una pila muerta a la que se vuelve con un `ret`, y contestando de menos
//! es una maquina parada por un fallo que se podia contener.

use super::*;
use alloc::sync::Arc;

/// Un ejecutor con la geometria de pila que se le dice, y nada detras.
fn with_stack_base(base: usize) -> core::mem::ManuallyDrop<Executor> {
    let mut executor = metadata_only_executor();
    executor.stack_base = base;
    executor
}

#[test]
fn a_fault_between_polls_has_no_task_to_blame() {
    let executor = metadata_only_executor();
    assert!(
        !executor.is_polling(),
        "un ejecutor recien hecho no esta dentro de Task::poll"
    );
    assert!(!executor.is_running_future());

    // Lo que `Executor::run` publica justo antes de entrar en `Task::poll`.
    let mut task = 0u8;
    executor
        .current_task
        .store(&raw mut task as *mut Task, Ordering::Release);
    assert!(executor.is_polling());
    executor
        .current_task
        .store(core::ptr::null_mut(), Ordering::Release);
    assert!(!executor.is_polling());
}

/// `is_running_future` sigue siendo cierto el resto de la vuelta del bucle;
/// `is_polling` solo durante el poll. La diferencia es la que decide si hay un
/// futuro que retirar.
#[test]
fn being_in_a_poll_is_stricter_than_having_a_task_in_flight() {
    let executor = metadata_only_executor();
    executor.task_id.store(7, Ordering::Relaxed);
    assert!(executor.is_running_future());
    assert!(
        !executor.is_polling(),
        "entre polls no hay futuro que retirar aunque la tarea siga en vuelo"
    );
}

#[test]
fn only_the_usable_stack_counts_as_this_executors_own() {
    const BASE: usize = 0x4000_0000;
    let executor = with_stack_base(BASE);
    for sp in [BASE, BASE + 8, BASE + STACK_SIZE / 2, BASE + STACK_SIZE - 1] {
        assert!(
            executor.stack_contains(sp),
            "{:#x} esta en la pila usable",
            sp
        );
    }
    for sp in [
        0,
        BASE - 1,
        BASE - GUARD_SIZE,
        BASE + STACK_SIZE,
        BASE + STACK_SIZE + TOP_GUARD_SIZE,
        usize::MAX,
    ] {
        assert!(
            !executor.stack_contains(sp),
            "{:#x} NO es la pila usable: las bandas de guarda no se abandonan \
             como si fueran el marco de una tarea",
            sp
        );
    }
    assert_eq!(executor.stack_base(), BASE);
}

/// El pestillo de reanudacion lo prueba `resume_claim_tests` por dentro; esto
/// es que el ejecutor lo cablee al suyo y no a otro.
#[test]
fn the_executor_hands_its_own_latch_to_the_first_cpu_that_asks() {
    let executor = metadata_only_executor();
    assert_eq!(executor.try_claim_resume(0), Ok(()));
    assert_eq!(executor.try_claim_resume(1), Err(0));
    executor.release_resume(0);
    assert_eq!(executor.try_claim_resume(1), Ok(()));
    executor.release_resume(1);
}

/// Una suelta que no es del dueño se CUENTA (16 informes) pero limpia igual:
/// el pestillo no puede quedarse echado por un error de contabilidad.
#[test]
fn a_release_by_the_wrong_cpu_still_frees_the_stack() {
    let executor = metadata_only_executor();
    assert_eq!(executor.try_claim_resume(2), Ok(()));
    executor.release_resume(5);
    assert_eq!(
        executor.try_claim_resume(5),
        Ok(()),
        "la suelta ajena deja el pestillo abierto, aunque quede informada"
    );
    executor.release_resume(5);
}

#[test]
fn an_executor_faulting_between_polls_is_discarded_whole() {
    let executor = metadata_only_executor();
    assert!(!executor.killed());
    assert!(unsafe { executor.abandon_idle_executor() });
    assert!(
        executor.killed(),
        "el runtime tiene que tirarlo, no reanudar su pila"
    );
    assert!(
        executor.is_running_future(),
        "force_replace hace que run_until_idle lo REEMPLACE en vez de volver a \
         entrar en el marco que fallo"
    );
}

#[test]
fn an_executor_inside_a_poll_is_refused_the_idle_path() {
    let executor = metadata_only_executor();
    let mut task = 0u8;
    executor
        .current_task
        .store(&raw mut task as *mut Task, Ordering::Release);
    assert!(
        !unsafe { executor.abandon_idle_executor() },
        "con un poll en vuelo matar la tarea es estrictamente mejor que tirar \
         el ejecutor entero"
    );
    assert!(!executor.killed(), "la negativa no cambia nada");
    assert!(!executor.is_running_future());
}

#[test]
fn abandoning_a_poll_that_does_not_exist_changes_nothing() {
    let executor = metadata_only_executor();
    assert!(!unsafe { executor.abandon_current_task() });
    assert!(!executor.killed());
    assert!(!executor.is_running_future());
}

/// La traba de ultimo recurso: ni tarea retirada ni poll que mirar, pero la
/// pila esta muerta y no se puede reanudar.
#[test]
fn the_last_resort_latch_kills_the_executor_without_retiring_a_task() {
    let executor = metadata_only_executor();
    let mut task = 0u8;
    executor
        .current_task
        .store(&raw mut task as *mut Task, Ordering::Release);
    executor.force_replace_executor();
    assert!(executor.killed());
    assert!(executor.is_running_future());
    assert_eq!(
        executor.task_id(),
        0,
        "no retira nada: solo impide que se vuelva a entrar en la pila"
    );
}

/// El camino que de verdad corre el panico: el ejecutor retira la tarea que
/// esta poleando y el hueco del slab se recoge sin que el llamante toque el
/// waker.
///
/// `task_collection` ya prueba la secuencia a mano
/// (`abandoning_a_poll_still_reaps_the_slab_slot`); lo que falta es que el
/// ejecutor use el waker que publico y deje `killed()` cierto.
#[test]
fn the_executor_retires_the_task_it_is_polling_and_the_slot_is_reaped() {
    let collection = TaskCollection::new(0);
    collection.add_task(core::future::pending::<()>(), None);
    let (_key, task, waker) = collection.take_task().expect("una tarea nueva corre");
    assert_eq!(collection.task_num(), 1);

    // El estado que deja un panico dentro de `Task::poll`: el cerrojo del
    // futuro tomado y nadie que lo suelte.
    task.leak_future_lock_for_test();

    let executor = metadata_only_executor();
    executor
        .current_task
        .store(Arc::as_ptr(&task) as *mut Task, Ordering::Release);
    executor
        .current_waker
        .store(Arc::as_ptr(&waker) as *mut WakerRef, Ordering::Release);

    assert!(unsafe { executor.abandon_current_task() });
    assert!(executor.killed());
    assert!(
        collection.take_task().is_none(),
        "una tarea retirada se volvio a repartir"
    );
    assert_eq!(collection.task_num(), 0, "el hueco del slab no se recogio");
}

/// Sin waker publicado el ejecutor cae en `task.waker()`, que es el mismo
/// `Arc`. El camino existe porque `current_waker` se publica un instante
/// despues de `current_task`.
#[test]
fn without_a_published_waker_the_task_s_own_is_used() {
    let collection = TaskCollection::new(0);
    collection.add_task(core::future::pending::<()>(), None);
    let (_key, task, _waker) = collection.take_task().expect("una tarea nueva corre");
    task.leak_future_lock_for_test();

    let executor = metadata_only_executor();
    executor
        .current_task
        .store(Arc::as_ptr(&task) as *mut Task, Ordering::Release);

    assert!(unsafe { executor.abandon_current_task() });
    assert!(collection.take_task().is_none());
    assert_eq!(collection.task_num(), 0);
}

#[test]
fn a_hard_bottom_guard_reports_its_canary_intact_without_reading_it() {
    let mut executor = with_stack_base(0);
    executor.hard_guard_bottom = true;
    assert!(
        executor.canary_intact(),
        "con guarda dura el desbordamiento es un #PF: leer la banda (aqui, la \
         pagina cero) seria el fallo que esto evita"
    );
}

/// El canario blando se muestrea en los DOS extremos de la banda: ocho
/// palabras arriba (las primeras que pisa un desbordamiento) y cuatro abajo
/// (un salto largo que se salta el borde).
#[test]
fn the_soft_canary_is_sampled_at_both_ends_of_the_guard_band() {
    // La banda entera, mas una pagina por encima para que `stack_base` caiga
    // justo detras de ella.
    let mut band = alloc::vec![0u64; GUARD_WORDS + PAGE_SIZE / 8];
    let base = band.as_ptr() as usize + GUARD_SIZE;
    for (i, word) in band[..GUARD_WORDS].iter_mut().enumerate() {
        *word = STACK_CANARY ^ i as u64;
    }

    let executor = with_stack_base(base);
    assert!(
        executor.canary_intact(),
        "la banda recien pintada esta entera"
    );

    for smashed in [GUARD_WORDS - 1, GUARD_WORDS - 8, 0, 3] {
        let saved = band[smashed];
        band[smashed] = 0;
        assert!(
            !executor.canary_intact(),
            "la palabra {} esta muestreada y el desbordamiento pasa por ella",
            smashed
        );
        band[smashed] = saved;
        assert!(executor.canary_intact());
    }

    // Y lo que NO ve: el muestreo son doce palabras de 65.536, asi que un
    // pisotón en el medio de la banda no lo nota nadie hasta el proximo tick.
    // Se deja escrito porque es la razon de ser de la guarda dura.
    let middle = GUARD_WORDS / 2;
    band[middle] = 0;
    assert!(
        executor.canary_intact(),
        "el canario blando es un muestreo, no un barrido: la guarda dura es la \
         que no tiene huecos"
    );
}

/// La banda se compara con `STACK_CANARY ^ indice`, no con una constante: un
/// relleno uniforme con el valor base no cuenta como intacto.
#[test]
fn a_uniform_fill_of_the_magic_word_is_not_an_intact_band() {
    let band = alloc::vec![STACK_CANARY; GUARD_WORDS + PAGE_SIZE / 8];
    let executor = with_stack_base(band.as_ptr() as usize + GUARD_SIZE);
    assert!(
        !executor.canary_intact(),
        "el indice va dentro de cada palabra para que un memset del valor \
         magico no se cuele"
    );
}

#[test]
fn executor_ids_are_handed_out_once_each() {
    let first = executor_alloc_id();
    let second = executor_alloc_id();
    assert!(second > first, "los ids no se repiten ni bajan");

    const PER_THREAD: usize = 200;
    let threads: alloc::vec::Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                (0..PER_THREAD)
                    .map(|_| executor_alloc_id())
                    .collect::<alloc::vec::Vec<usize>>()
            })
        })
        .collect();
    let mut seen: alloc::vec::Vec<usize> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    seen.sort_unstable();
    let total = seen.len();
    seen.dedup();
    assert_eq!(
        seen.len(),
        total,
        "dos ejecutores con el mismo id hacen ilegible cualquier informe de \
         [stale-resume] o [double-resume]"
    );
}

/// Los dos informes de una vez. No tienen nada observable (son `#[cold]` con
/// un `AtomicBool` y un `error!`), asi que lo que se fija es que el pestillo
/// de una sola vez no atrape ni entre en bucle: se llaman desde
/// `Executor::new`, o sea desde el arranque de cada CPU.
#[test]
fn the_one_shot_reports_are_callable_more_than_once() {
    note_first_executor_created(true, true);
    note_first_executor_created(false, false);
    note_soft_guard_fallback("test");
    note_soft_guard_fallback("test otra vez");
}
