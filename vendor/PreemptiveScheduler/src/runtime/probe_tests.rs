//! Lo que las sondas del runtime contestan en una CPU que no tiene runtime.
//!
//! Casi todas las que hay aqui se llaman desde contexto de IRQ dura o desde el
//! camino del panico, y todas empiezan por lo mismo: el id de CPU y un
//! `try_lock_cpu`. Las dos salidas de esa cabecera (una CPU sin id logico, un
//! hueco sin construir o tomado) son las que nunca tienen pruebas, porque la
//! maquina de verdad siempre tiene su runtime; y son exactamente las que no
//! pueden ni bloquear ni mentir. Una sonda del camino del panico que contesta
//! «si» sin haber mirado nada manda a reanudar una pila muerta; una que se
//! queda esperando el cerrojo convierte un fallo diagnosticable en una
//! congelacion sin una linea en pantalla.
//!
//! En el anfitrion NINGUNA CPU tiene runtime, y no se puede construir uno:
//! `ExecutorRuntime::new` llama a `Executor::new`, que lee `cr3`. Asi que esta
//! es la unica mitad que se puede probar aqui, y es la que importa.
//!
//! Lo que queda deliberadamente fuera, por lo mismo: `get_current_runtime`
//! (fuerza la construccion del hueco), `get_current_executor_id`,
//! `weak_executor_num`, `add_weak_executor`, `reclaim_finished_weaks`,
//! `downgrade_strong_executor`, `handle_timeout`, `spawn_with_affinity` y los
//! dos `check_current_executor_*`: todos quieren un ejecutor de verdad detras.

use super::*;

/// Un id de CPU prestado al hilo del test, devuelto al salir.
///
/// `cpu_id()` es un `thread_local` en los tests (ver `arch::x86_64`), asi que
/// esto no se pisa con nadie; los arrays por CPU que se tocan aqui SI son
/// globales, y por eso cada test que escribe en uno usa un indice propio.
struct AsCpu(u8);

impl AsCpu {
    fn new(cpu: u8) -> Self {
        AsCpu(crate::arch::set_cpu_id_for_test(cpu))
    }
}

impl Drop for AsCpu {
    fn drop(&mut self) {
        crate::arch::set_cpu_id_for_test(self.0);
    }
}

/// Una CPU por encima de `MAX_CORE_NUM`: no tiene id logico denso, no puede
/// tomar un cerrojo del nucleo y no debe correr codigo del nucleo.
const NO_DENSE_ID: u8 = MAX_CORE_NUM as u8;

#[test]
fn an_unbuilt_slot_is_skipped_rather_than_built_or_waited_on() {
    assert!(
        GLOBAL_RUNTIME.try_lock_cpu(0).is_none(),
        "en el anfitrion no hay runtime construido en ninguna CPU"
    );
    assert!(GLOBAL_RUNTIME.try_lock_cpu(MAX_CORE_NUM).is_none());
    assert!(GLOBAL_RUNTIME.try_lock_cpu(usize::MAX).is_none());
}

/// El informe de la CPU sin hueco esta limitado a cuatro lineas: si suena,
/// suena en cada vuelta del bucle de idle del llamante, y el cerrojo de la
/// consola es lo ultimo por lo que esa CPU deberia pelear.
#[test]
fn the_cpu_without_a_slot_is_reported_without_a_runtime() {
    for _ in 0..8 {
        report_cpu_without_slot(MAX_CORE_NUM, "probe_tests");
    }
}

#[test]
fn every_containment_probe_fails_closed_without_a_runtime() {
    let _cpu = AsCpu::new(0);
    assert!(!current_task_abandonable());
    assert!(!current_executor_abandonable());
    assert!(!fault_sp_abandonable(current_sp()));
    assert!(!fault_sp_abandonable(0));
}

#[test]
fn the_containment_probes_refuse_a_cpu_with_no_dense_id() {
    let _cpu = AsCpu::new(NO_DENSE_ID);
    assert!(!current_task_abandonable());
    assert!(!current_executor_abandonable());
    assert!(!fault_sp_abandonable(current_sp()));
}

/// Las dos sondas de IRQ fallan hacia «no te saltes el trabajo»: saltarse el
/// despacho en una CPU de la que no se sabe nada seria apagar el teclado y el
/// present grafico por si acaso.
#[test]
fn the_irq_depth_guard_does_not_skip_what_it_cannot_measure() {
    let _cpu = AsCpu::new(0);
    assert!(!irq_should_skip_heavy_work());
    let _no_id = AsCpu::new(NO_DENSE_ID);
    assert!(!irq_should_skip_heavy_work());
}

/// Y la de idle falla al contrario: sin runtime que mirar, «no hay ejecutor
/// actual» es la verdad, y para una CPU sin id logico lo es siempre.
#[test]
fn a_cpu_with_no_dense_id_is_always_on_the_idle_path() {
    let _cpu = AsCpu::new(NO_DENSE_ID);
    assert!(irq_on_idle_executor());
}

/// Un hueco sin construir no es lo mismo que uno tomado, y aqui los dos
/// contestan `false`: «no se sabe, no fuerces el salto».
#[test]
fn an_unreadable_runtime_is_not_reported_as_idle() {
    let _cpu = AsCpu::new(0);
    assert!(!irq_on_idle_executor());
}

/// La heuristica del tope de pila esta retirada y contesta `false` siempre.
/// El RSP de una funcion de Rust apunta a locales o a relleno de alineacion:
/// un cero ahi no es corrupcion, y creerselo apagaba los callbacks del timer.
#[test]
fn the_retired_stack_top_heuristic_never_suppresses_anything() {
    assert!(!current_stack_top_looks_null());
    let _cpu = AsCpu::new(NO_DENSE_ID);
    assert!(!current_stack_top_looks_null());
}

/// La marca de smash es POR CPU: un smash en un nucleo no puede apagar el
/// despacho dinamico en todos (eso es el escritorio cojeando entero).
#[test]
fn the_heap_smash_mark_is_sticky_and_per_cpu() {
    const MINE: u8 = 61;
    const NEIGHBOUR: u8 = 62;
    let _cpu = AsCpu::new(MINE);
    assert!(!heap_smash_suspected(), "la CPU {} esta limpia", MINE);
    note_heap_smash_suspected();
    assert!(heap_smash_suspected());
    note_heap_smash_suspected();
    assert!(heap_smash_suspected(), "la marca no se quita este arranque");

    let _neighbour = AsCpu::new(NEIGHBOUR);
    assert!(
        !heap_smash_suspected(),
        "la CPU de al lado no hereda el smash: eso era el escritorio cojo"
    );
}

/// Una CPU sin id logico no tiene hueco en el array, asi que ni lo marca ni
/// lo lee: lo que no puede pasar es que escriba fuera.
#[test]
fn a_cpu_with_no_dense_id_has_no_smash_slot() {
    let _cpu = AsCpu::new(NO_DENSE_ID);
    note_heap_smash_suspected();
    assert!(!heap_smash_suspected());
}

#[test]
fn a_suspected_smash_stops_all_dyn_dispatch_not_only_the_idle_tick() {
    const MINE: u8 = 63;
    let _cpu = AsCpu::new(MINE);
    assert!(
        !irq_should_skip_dyn_dispatch(),
        "sin profundidad en riesgo y sin smash no hay nada que saltarse"
    );
    note_heap_smash_suspected();
    assert!(
        irq_should_skip_dyn_dispatch(),
        "seguir llamando por un monton medio pisado es como cascada el segundo #PF"
    );
}

#[test]
fn the_fault_walk_reports_what_it_could_not_look_at() {
    let _cpu = AsCpu::new(0);
    let attr = attribute_fault_stack_ptrs(current_sp(), 0);
    assert!(
        attr.rsp.is_none(),
        "ningun ejecutor puede reclamar este RSP"
    );
    assert!(attr.rbp.is_none());
    assert_eq!(attr.executors_seen, 0);
    assert_eq!(
        attr.cpus_walked, 0,
        "una CPU sin runtime no se cuenta como recorrida"
    );
    assert!(
        attr.cpus_skipped > 0,
        "un informe que dice «no esta en ninguna pila» sin decir cuantas CPUs \
         no pudo mirar es un informe que no se puede leer"
    );
}

#[test]
fn every_stack_region_has_a_name_for_the_report() {
    assert_eq!(StackPtrRegion::Usable.as_str(), "usable");
    assert_eq!(StackPtrRegion::BottomGuard.as_str(), "bottom-guard");
    assert_eq!(StackPtrRegion::TopGuard.as_str(), "top-guard");
}

/// `current_sp` es la unica lectura de RSP del modulo y entra en la cuenta de
/// profundidad de pila (`note_stack_depth`), asi que una lectura que no fuera
/// el RSP de verdad falsearia la marca de agua sin que nada mas se enterase.
#[test]
fn the_stack_pointer_read_is_the_caller_s_own_frame() {
    let local = 0u64;
    let sp = current_sp();
    let addr = &raw const local as usize;
    assert_ne!(sp, 0);
    assert_eq!(sp % 8, 0, "RSP tiene que estar alineado a palabra");
    assert!(
        addr >= sp && addr - sp < 4096,
        "un local de este marco ({:#x}) tiene que caer justo por encima del RSP \
         leido ({:#x})",
        addr,
        sp
    );
}

/// Sin rango de texto publicado por el HAL, el que contesta es el de reserva
/// de arranque temprano. Es la diferencia entre refusar un marco parado por
/// tener una rip imposible y refusarlos todos antes de que el HAL arranque.
#[test]
#[cfg(target_arch = "x86_64")]
fn the_resume_address_gate_falls_back_to_the_early_boot_text_range() {
    assert_eq!(
        lock::fn_slot::text_range(),
        (0, 0),
        "este test describe el caso sin publicar"
    );
    assert!(kernel_resume_address(KERNEL_TEXT.start));
    assert!(kernel_resume_address(KERNEL_TEXT.end - 1));
    assert!(!kernel_resume_address(0));
    assert!(!kernel_resume_address(KERNEL_TEXT.end));
    assert!(
        !kernel_resume_address(0x4000),
        "una rip de espacio de usuario no es una direccion de reanudacion"
    );
}

/// Las dos puertas de robo, con el runtime sin construir: no hay victima
/// posible y lo que no puede pasar es que entren a buscarla.
#[test]
fn nobody_steals_from_a_machine_with_no_runtimes() {
    let _lock = resched_test_lock();
    let _cpu = AsCpu::new(0);
    let saved_ready = set_executor_ready_mask_for_test(0b1111);
    let saved_stealable = STEALABLE.swap(0b1110, Ordering::SeqCst);

    assert!(steal_task_from_other_cpu().is_none());
    let before = STEAL_REBALANCE.load(Ordering::SeqCst);
    assert!(steal_for_balance().is_none());
    assert_eq!(
        STEAL_REBALANCE.load(Ordering::SeqCst),
        before,
        "un reparto que no movio nada no se cuenta como reparto"
    );

    STEALABLE.store(saved_stealable, Ordering::SeqCst);
    set_executor_ready_mask_for_test(saved_ready);
}

/// La pista de robo contesta sola, sin tomar un cerrojo: es la razon de ser
/// de `STEALABLE` y la unica fila de `sched_steal_stats` que la mide.
#[test]
fn an_empty_hint_answers_without_taking_a_lock() {
    let _lock = resched_test_lock();
    let _cpu = AsCpu::new(0);
    let saved_ready = set_executor_ready_mask_for_test(0b1111);
    let saved_stealable = STEALABLE.swap(0, Ordering::SeqCst);

    let before = STEAL_SKIPPED.load(Ordering::SeqCst);
    assert!(steal_task_from_other_cpu().is_none());
    assert_eq!(
        STEAL_SKIPPED.load(Ordering::SeqCst),
        before + 1,
        "el barrido no se salto, o no se conto"
    );

    STEALABLE.store(saved_stealable, Ordering::SeqCst);
    set_executor_ready_mask_for_test(saved_ready);
}

/// El rescate de un vecino varado: lo decide `stranded_peers`, que ya tiene
/// sus pruebas; lo que falta es que la sonda le pase los tres globales y no
/// otra cosa.
#[test]
fn a_stranded_peer_is_only_worth_a_scan_while_it_is_marked() {
    let _lock = resched_test_lock();
    let _cpu = AsCpu::new(0);
    let saved_ready = set_executor_ready_mask_for_test(0b0011);
    clear_stranded_for_test();
    assert!(!stranded_peer_exists(), "nadie marcado, nada que rescatar");

    note_stranded(1);
    assert!(stranded_peer_exists(), "la CPU 1 esta marcada y esta lista");

    // Un vecino marcado al que nadie dio por listo no vale un barrido: el
    // rescate le roba la tarea a una CPU que SI esta en su bucle de ejecutor.
    clear_stranded_for_test();
    note_stranded(5);
    assert!(
        !stranded_peer_exists(),
        "la mascara de listos es la que decide si el marcado cuenta"
    );

    // Y uno mismo no se rescata: el marcado es el que no puede con la tarea.
    clear_stranded_for_test();
    note_stranded(0);
    assert!(!stranded_peer_exists());

    clear_stranded_for_test();
    assert!(!stranded_peer_exists());
    set_executor_ready_mask_for_test(saved_ready);
}

/// El llamado de idle: sin registrar, «no hice nada», que es lo que manda al
/// ejecutor a pararse en vez de volver a mirar la cola para siempre.
#[test]
fn the_idle_callback_answers_no_progress_until_one_is_registered() {
    let _lock = resched_test_lock();
    let saved = IDLE_CALLBACK.swap(0, Ordering::SeqCst);
    assert!(!run_idle_callback());

    fn made_progress() -> bool {
        true
    }
    set_idle_callback(made_progress);
    assert!(
        run_idle_callback(),
        "un llamado que drena trabajo diferido tiene que poder decir que lo hizo"
    );

    IDLE_CALLBACK.store(saved, Ordering::SeqCst);
}

#[test]
fn wakeup_preemption_is_a_boot_switch_that_reads_back() {
    let _lock = resched_test_lock();
    let saved = wakeup_preempt_enabled();
    set_wakeup_preempt(false);
    assert!(!wakeup_preempt_enabled());
    set_wakeup_preempt(true);
    assert!(wakeup_preempt_enabled());
    set_wakeup_preempt(saved);
}

/// La cuenta de debiles aparcados la lee `Executor::run` en su cabecera, con
/// una carga relajada del hueco de su propia CPU. Un hueco compartido o un
/// indice fuera de rango seria una CPU cediendo el turno por los debiles de
/// otra.
#[test]
fn the_parked_weak_count_is_each_cpu_s_own() {
    const MINE: u8 = 58;
    const NEIGHBOUR: u8 = 59;
    let _cpu = AsCpu::new(MINE);
    publish_weak_waiting(0);
    assert_eq!(weak_waiting_here(), 0);
    publish_weak_waiting(3);
    assert_eq!(weak_waiting_here(), 3);

    {
        let _neighbour = AsCpu::new(NEIGHBOUR);
        publish_weak_waiting(0);
        assert_eq!(weak_waiting_here(), 0);
    }
    assert_eq!(
        weak_waiting_here(),
        3,
        "el vecino escribio en nuestro hueco"
    );

    publish_weak_waiting(0);
}

#[test]
fn a_cpu_with_no_dense_id_publishes_no_weak_count() {
    let _cpu = AsCpu::new(NO_DENSE_ID);
    publish_weak_waiting(7);
    assert_eq!(weak_waiting_here(), 0);
}

/// El pico de debiles vivos es una marca de agua: sube y no baja. Es la cifra
/// que dice si la rotacion de preempciones adelanta al drenaje de debiles.
#[test]
fn the_weak_peak_only_ever_rises() {
    let _lock = resched_test_lock();
    let (_, before, _) = sched_weak_stats();
    note_weak_live(before as usize + 5);
    let (_, after, _) = sched_weak_stats();
    assert_eq!(after, before + 5);
    note_weak_live(0);
    note_weak_live(1);
    let (_, still, _) = sched_weak_stats();
    assert_eq!(still, after, "el pico no es la cuenta actual");
}

/// Los dos informes del camino de reanudacion. No tienen nada observable (un
/// `AtomicUsize` limitador y un `error!`), pero se llaman desde el camino del
/// panico y desde la cabecera del bucle del planificador: lo que se fija es
/// que el limitador no atrape ni desborde.
#[test]
fn the_resume_reports_survive_being_called_past_their_own_limit() {
    for cpu in 0..20 {
        report_double_resume("strong", 7, 1, cpu);
    }
    for _ in 0..4 {
        report_switch_bounce_if_any();
    }
}

/// Las dos puertas que toman un `&Executor`, con una pila de verdad (del
/// monton del anfitrion) debajo.
///
/// Son las que deciden si se vuelve a entrar en un marco parado y si se
/// abandona una pila; `classify_parked_frame` y `stack_abandonable` tienen sus
/// pruebas, pero el CABLEADO -- que sp, que base, que tope salen del ejecutor
/// -- no tenia ninguna, y equivocarse ahi es exactamente el `ret` a un marco
/// muerto que todo esto existe para no hacer.
#[cfg(target_arch = "x86_64")]
mod executor_gates {
    use super::*;
    use crate::executor::StubExecutor;

    /// Una pila de corrutina del tamaño de verdad, en el monton del
    /// anfitrion: las dos puertas leen `[base, base + STACK_SIZE)`, asi que
    /// una mas pequeña se saldria del buffer.
    struct HostStack {
        words: alloc::vec::Vec<u64>,
    }

    impl HostStack {
        fn new() -> Self {
            HostStack {
                words: alloc::vec![0u64; crate::STACK_SIZE / 8],
            }
        }

        fn base(&self) -> usize {
            self.words.as_ptr() as usize
        }

        /// Deja un marco parado a `offset` palabras de la base y devuelve su
        /// direccion, que es lo que el ejecutor guarda como sp.
        fn park_frame(&mut self, offset: usize, cr3: usize, rip: usize) -> usize {
            let frame = crate::arch::ContextData {
                cr3,
                rip,
                ..crate::arch::ContextData::default()
            };
            let at = self.base() + offset * 8;
            // SAFETY: `offset * 8 + FRAME_SIZE` cabe en el buffer; quien llama
            // lo elige por debajo de STACK_SIZE / 8 - 8.
            unsafe { core::ptr::write(at as *mut crate::arch::ContextData, frame) };
            at
        }
    }

    fn with_parked_frame(sp: usize, base: usize) -> core::mem::ManuallyDrop<Executor> {
        StubExecutor {
            stack_base: base,
            parked_sp: Some(sp),
            ..StubExecutor::default()
        }
        .build()
    }

    #[test]
    fn a_whole_frame_on_its_own_stack_with_a_live_root_is_resumable() {
        let mut stack = HostStack::new();
        let base = stack.base();
        let sp = stack.park_frame(64, 0x1000, KERNEL_TEXT.start as usize);
        assert!(executor_frame_resumable(&with_parked_frame(sp, base)));
    }

    #[test]
    fn a_frame_with_a_dead_page_table_root_is_refused() {
        let mut stack = HostStack::new();
        let base = stack.base();
        let sp = stack.park_frame(64, 0, KERNEL_TEXT.start as usize);
        assert!(
            !executor_frame_resumable(&with_parked_frame(sp, base)),
            "cr3 = 0 es un marco pisado: reanudarlo es el `[null-exec]`"
        );
    }

    #[test]
    fn a_frame_whose_resume_address_is_not_kernel_text_is_refused() {
        let mut stack = HostStack::new();
        let base = stack.base();
        for rip in [0, 0x4000, KERNEL_TEXT.end as usize] {
            let sp = stack.park_frame(64, 0x1000, rip);
            assert!(
                !executor_frame_resumable(&with_parked_frame(sp, base)),
                "una rip de {:#x} no es una direccion de reanudacion",
                rip
            );
        }
    }

    /// El tope se calcula `base + STACK_SIZE` y el sp se compara con
    /// `saturating_add`: un marco con un sp altisimo hacia que la suma diera
    /// la vuelta, la cota pasara, y las lecturas se fueran a donde apuntara la
    /// basura. Eso es la puerta entregandole al camino de reanudacion el
    /// mismisimo fallo que esta ahi para cazar.
    #[test]
    fn a_frame_pointer_that_is_not_on_this_stack_is_refused_without_reading_it() {
        let stack = HostStack::new();
        let base = stack.base();
        for sp in [
            0,
            base - 8,
            base + crate::STACK_SIZE,
            0xffff_ffff_ffff_ffc0,
            usize::MAX - 7,
        ] {
            assert!(
                !executor_frame_resumable(&with_parked_frame(sp, base)),
                "{:#x} no nombra un marco entero dentro de esta pila",
                sp
            );
        }
        // Y un sp desalineado tampoco, aunque caiga dentro.
        assert!(!executor_frame_resumable(&with_parked_frame(
            base + 65,
            base
        )));
    }

    /// Un marco que empieza entero dentro pero cuyo ultimo cuarto se sale: la
    /// cota es `sp + FRAME_SIZE <= top`, no `sp < top`.
    #[test]
    fn a_frame_that_starts_inside_but_ends_outside_is_refused() {
        let mut stack = HostStack::new();
        let base = stack.base();
        let last = crate::STACK_SIZE / 8 - FRAME_SIZE / 8;
        let good = stack.park_frame(last, 0x1000, KERNEL_TEXT.start as usize);
        assert!(
            executor_frame_resumable(&with_parked_frame(good, base)),
            "el ultimo marco que cabe entero SI se reanuda"
        );
        assert!(!executor_frame_resumable(&with_parked_frame(
            good + 8,
            base
        )));
    }

    /// Cuantas palabras puede leer el informe de la negativa: nueve, y nunca
    /// por encima del tope, porque la banda de guarda de arriba esta
    /// desmapeada a proposito y un informe que falla dentro del manejador de
    /// fallos se lleva la maquina en el momento en que iba a decir por que.
    #[test]
    fn the_refusal_report_never_reads_past_the_top_of_the_stack() {
        const TOP: usize = 0x8000_0000;
        assert_eq!(frame_dump_words(TOP - 9 * 8, TOP), 9);
        assert_eq!(frame_dump_words(TOP - 100 * 8, TOP), 9, "nueve es el techo");
        assert_eq!(frame_dump_words(TOP - 8, TOP), 1);
        assert_eq!(frame_dump_words(TOP, TOP), 0);
        assert_eq!(
            frame_dump_words(TOP + 8, TOP),
            0,
            "un sp por encima del tope"
        );
    }

    /// La otra puerta: el ejecutor le pasa su propia base y su propio canario,
    /// y la seccion critica del planificador la cierra entera.
    #[test]
    fn a_fault_on_this_executors_own_stack_with_an_intact_canary_is_abandonable() {
        let stack = HostStack::new();
        let base = stack.base();
        // Guarda dura: `canary_intact()` contesta sin leer la banda, que aqui
        // no existe (el buffer empieza en `base`).
        let executor = StubExecutor {
            stack_base: base,
            hard_guard_bottom: true,
            ..StubExecutor::default()
        }
        .build();

        assert!(executor_stack_abandonable(&executor, base));
        assert!(executor_stack_abandonable(
            &executor,
            base + crate::STACK_SIZE - 1
        ));
        for sp in [0, base - 1, base + crate::STACK_SIZE, usize::MAX] {
            assert!(
                !executor_stack_abandonable(&executor, sp),
                "{:#x} no es esta pila: abandonarla no contiene ese fallo",
                sp
            );
        }

        let critical = StubExecutor {
            stack_base: base,
            hard_guard_bottom: true,
            scheduler_critical: true,
            ..StubExecutor::default()
        }
        .build();
        assert!(
            !executor_stack_abandonable(&critical, base),
            "abandonar la contabilidad del planificador puede dejar huerfano un \
             prestamo o un cerrojo de cola"
        );
    }

    /// Y sin guarda dura la banda se lee de verdad: un canario pisado
    /// descalifica el abandono, porque tirar una pila no repara el monton de
    /// al lado.
    #[test]
    fn a_smashed_soft_canary_disqualifies_the_abandon() {
        let mut band = alloc::vec![0u64; crate::GUARD_SIZE / 8 + crate::STACK_SIZE / 8];
        let base = band.as_ptr() as usize + crate::GUARD_SIZE;
        for (i, word) in band[..crate::GUARD_SIZE / 8].iter_mut().enumerate() {
            *word = crate::executor::stack_canary_word(i);
        }

        let executor = StubExecutor {
            stack_base: base,
            ..StubExecutor::default()
        }
        .build();
        assert!(executor_stack_abandonable(&executor, base));

        band[crate::GUARD_SIZE / 8 - 1] = 0;
        assert!(
            !executor_stack_abandonable(&executor, base),
            "un canario pisado es monton de al lado ya escrito: no hay \
             recuperacion a nivel de tarea que arregle eso"
        );
    }
}

/// Las dos entradas del camino del panico que no pasan por `*_abandonable`
/// antes: refusan solas, sin runtime que mirar.
#[test]
fn the_abandon_entry_points_refuse_without_a_runtime() {
    let _lock = resched_test_lock();
    let _cpu = AsCpu::new(0);
    assert!(!unsafe { abandon_current_executor() });
    assert!(!unsafe { abandon_current_task() });
    assert!(!unsafe { abandon_executor_for_sp(current_sp()) });
    assert!(!unsafe { abandon_executor_for_sp(0) });
}
