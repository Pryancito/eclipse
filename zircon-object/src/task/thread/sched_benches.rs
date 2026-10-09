//! Lo que un hilo de calculo paga en CADA tick del planificador.
//!
//! # Por que estas filas
//!
//! El hilo que mas se nota cuando el procesamiento va lento es el que no hace
//! nada mas que calcular: ni syscalls, ni esperas, ni E/S. Ese hilo no pasa
//! por la cola de tareas, no despierta a nadie y no se duerme, asi que casi
//! todo lo que el planificador le cobra se cobra en un solo sitio: el camino
//! de interrupcion de temporizador de `loader/src/linux.rs`, que en cada tick
//! pregunta tres cosas a este fichero.
//!
//! Las tres preguntas, en el orden en que las hace:
//!
//! 1. [`Thread::tick_should_preempt`] — se le acabo la rodaja?
//! 2. [`Thread::sched_may_preempt_on_wake`] — hay un despertar pendiente que
//!    pueda quitarle la CPU ya?
//! 3. [`Thread::sched_wake_preempt_floor_end`] — si se le ha denegado, cuando
//!    acaba el suelo de RUN_TO_PARITY?
//!
//! Y cada una de las tres lee el reloj por su cuenta y recalcula
//! [`credited_slice_end`] por su cuenta. De ahi la fila
//! `the_whole_tick_when_a_wake_is_pending_and_denied`, que es la unica que
//! mide la secuencia entera: lo que dice al compararla con
//! `ask_a_running_thread_whether_its_slice_expired` es cuanto de ese tick es
//! trabajo y cuanto es la misma cuenta hecha tres veces.
//!
//! Debajo de las tres hay dos capas que tambien se miden aparte, porque si la
//! secuencia sale caras hay que saber de cual de las dos viene:
//!
//! - **El reloj.** `timer_now()` es el suelo de todo lo que hay aqui: ninguna
//!   de las tres preguntas puede costar menos que una lectura de reloj.
//! - **La aritmetica.** `slice_verdict`, `credited_slice_end`,
//!   `wake_preempt_allowed`, `wake_preempt_floor_end` y `resume_slice_end` son
//!   funciones puras sobre seis `u64` como mucho, sin un acceso a memoria
//!   compartida entre ellas. Si una de estas sale por encima de unos pocos
//!   nanosegundos es que se ha colado algo que no es aritmetica.
//!
//! # Lo que estas filas NO dicen
//!
//! - **El reloj del anfitrion no es el de la maquina.** Bajo `libos`
//!   `timer_now()` es un `clock_gettime(CLOCK_MONOTONIC)`, que es una llamada
//!   al vDSO; en baremetal es un `rdtsc` escalado. Son del mismo orden, no la
//!   misma cifra. Lo que se traslada de aqui a la maquina es la RELACION entre
//!   las filas (una lectura de reloj frente a tres), no el valor absoluto.
//! - **No hay ejecutor.** `libos` no trae el planificador propio, asi que
//!   ninguna fila incluye el cambio de contexto que viene DESPUES de un
//!   veredicto de `preempt`. El coste del cambio se mide en el crate del
//!   planificador; aqui se mide lo que se paga en los ticks en los que NO se
//!   cambia de hilo, que en un bucle de calculo son casi todos.
//! - **Los contadores son globales.** `SLICE_CREDIT` lo escribe cualquier otro
//!   test del binario, asi que las filas que lo tocan miden el coste de la
//!   escritura y nunca se leen como un valor acumulado.
//! - **El perfil de test paga `debug_assert!`s** que el kernel no paga.
//!
//! Cada fila pasa sus ENTRADAS por `black_box`, no solo el resultado: son unas
//! pocas instrucciones sobre valores que el compilador ve, y marcar solo el
//! resultado le deja plegar la cuenta y dejar una cifra por debajo del
//! nanosegundo que es el bucle y no la medida.
//!
//! Correr:
//!
//! ```sh
//! cargo bench -p zircon-object --features libos,aspace-separate \
//!     -- task::thread::sched_benches
//! ```

use super::*;
use crate::task::*;
use test::{black_box, Bencher};

/// Un hilo recien nacido: `SCHED_NORMAL`, nice 0, sin rodaja en curso.
fn a_thread() -> Arc<Thread> {
    let root = Job::root();
    let proc = Process::create(&root, "bench").expect("un proceso");
    Thread::create(&proc, "bench").expect("un hilo")
}

/// Un hilo con una rodaja en curso que aun le queda tiempo, y con una
/// observacion de tick anterior: el estado en el que esta un hilo de calculo
/// en todos los ticks menos el ultimo de cada rodaja.
///
/// `last_tick_ns` se pone a un tick de distancia y no a cero: con cero,
/// [`credited_slice_end`] sale por la primera guarda y la fila mediria la
/// aritmetica que el kernel NO se salta.
fn a_thread_mid_slice() -> Arc<Thread> {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.sched
        .slice_end_ns
        .store(now + BASE_TIMESLICE_NS / 2, Ordering::Relaxed);
    t.sched
        .last_tick_ns
        .store(now.saturating_sub(SCHED_TICK_NS), Ordering::Relaxed);
    t
}

// ── el reloj, que es el suelo de todas las demas ───────────────────────────

/// Una lectura de reloj y nada mas. Ninguna de las tres preguntas del tick
/// puede bajar de aqui, y la secuencia entera hace tres de estas.
#[bench]
fn read_the_clock_once(b: &mut Bencher) {
    b.iter(|| black_box(kernel_hal::timer::timer_now().as_nanos() as u64));
}

/// El otro acceso que [`credited_slice_end`] hace fuera de sus argumentos: el
/// hueco de tick de esta CPU. Una carga relajada de un array por-CPU.
#[bench]
fn read_the_cpu_tick_gap(b: &mut Bencher) {
    b.iter(|| black_box(kernel_hal::kstats::last_busy_tick_gap_ns()));
}

// ── la aritmetica pura ─────────────────────────────────────────────────────

/// La tabla de pesos de Linux, que es lo primero que mira `timeslice_ns`.
#[bench]
fn weigh_a_nice_value(b: &mut Bencher) {
    b.iter(|| black_box(nice_to_weight(black_box(0))));
}

/// El veredicto de rodaja con tiempo por delante: el caso comun.
#[bench]
fn judge_a_slice_that_has_time_left(b: &mut Bencher) {
    let (now, slice) = (1_000_000_000u64, BASE_TIMESLICE_NS);
    b.iter(|| {
        black_box(slice_verdict(
            black_box(now),
            black_box(now + slice / 2),
            black_box(slice),
        ))
    });
}

/// Y el veredicto cuando se le acabo, que es el que devuelve `true` y manda al
/// hilo al cambio de contexto.
#[bench]
fn judge_a_slice_that_has_expired(b: &mut Bencher) {
    let (now, slice) = (1_000_000_000u64, BASE_TIMESLICE_NS);
    b.iter(|| {
        black_box(slice_verdict(
            black_box(now),
            black_box(now - 1),
            black_box(slice),
        ))
    });
}

/// El credito cuando no hay nada que devolver, que es lo que pasa en un tick
/// normal: el hueco cabe en un tick y se cobra como siempre.
#[bench]
fn credit_a_gap_that_needs_no_credit(b: &mut Bencher) {
    let (now, slice, tick) = (1_000_000_000u64, BASE_TIMESLICE_NS, SCHED_TICK_NS);
    b.iter(|| {
        black_box(credited_slice_end(
            black_box(now),
            black_box(now + slice / 2),
            black_box(now - tick / 2),
            black_box(slice),
            black_box(tick),
            black_box(0),
        ))
    });
}

/// Y el credito cuando el marco estuvo congelado: la rama que suma y recorta.
#[bench]
fn credit_a_gap_the_thread_was_frozen_for(b: &mut Bencher) {
    let (now, slice, tick) = (1_000_000_000u64, BASE_TIMESLICE_NS, SCHED_TICK_NS);
    b.iter(|| {
        black_box(credited_slice_end(
            black_box(now),
            black_box(now + slice / 2),
            black_box(now - 15 * tick),
            black_box(slice),
            black_box(tick),
            black_box(0),
        ))
    });
}

/// Cuanto dio el credito, que es la decision que los contadores cuentan.
#[bench]
fn ask_how_much_the_credit_gave(b: &mut Bencher) {
    b.iter(|| black_box(credit_given(black_box(1_000), black_box(1_000))));
}

/// Un despertar que ya pasa del suelo de RUN_TO_PARITY.
#[bench]
fn allow_a_wake_past_the_parity_floor(b: &mut Bencher) {
    let (now, slice) = (1_000_000_000u64, BASE_TIMESLICE_NS);
    b.iter(|| {
        black_box(wake_preempt_allowed(
            black_box(now),
            black_box(now + slice / 2),
            black_box(slice),
            black_box(BASE_SLICE_NS),
        ))
    });
}

/// Y uno que cae dentro del suelo, que es el que se deniega y obliga al camino
/// de trampa a hacer la tercera pregunta del tick.
#[bench]
fn deny_a_wake_inside_the_parity_floor(b: &mut Bencher) {
    let (now, slice) = (1_000_000_000u64, BASE_TIMESLICE_NS);
    b.iter(|| {
        black_box(wake_preempt_allowed(
            black_box(now),
            black_box(now + slice - BASE_SLICE_NS / 2),
            black_box(slice),
            black_box(BASE_SLICE_NS),
        ))
    });
}

/// La tercera pregunta: donde acaba el suelo. Vuelve a llamar a
/// `wake_preempt_allowed` por dentro, asi que esta fila es la de arriba mas la
/// aritmetica del limite.
#[bench]
fn find_where_the_parity_floor_ends(b: &mut Bencher) {
    let (now, slice) = (1_000_000_000u64, BASE_TIMESLICE_NS);
    b.iter(|| {
        black_box(wake_preempt_floor_end(
            black_box(now),
            black_box(now + slice - BASE_SLICE_NS / 2),
            black_box(slice),
            black_box(BASE_SLICE_NS),
        ))
    });
}

/// La regla de lag de una reanudacion: lo que paga un hilo al volver de una
/// espera, y la unica de las puras que no esta en el camino del tick.
#[bench]
fn settle_the_slice_of_a_resumption(b: &mut Bencher) {
    let (now, slice) = (1_000_000_000u64, BASE_TIMESLICE_NS);
    b.iter(|| {
        black_box(resume_slice_end(
            black_box(now),
            black_box(slice / 4),
            black_box(slice / 8),
            black_box(slice),
        ))
    });
}

// ── los contadores de /proc/perf/kernel ────────────────────────────────────

/// El incremento incondicional de `tick_should_preempt`: una linea compartida
/// por todas las CPUs, tocada una vez por tick y por CPU.
#[bench]
fn note_a_tick_observation(b: &mut Bencher) {
    let c = SliceCredit::new();
    b.iter(|| c.note_tick());
}

/// La anotacion cuando no hubo credito, que es el caso de casi todos los
/// ticks: sale por la guarda sin tocar ninguna de las tres lineas.
#[bench]
fn note_a_credit_that_was_not_given(b: &mut Bencher) {
    let c = SliceCredit::new();
    b.iter(|| c.note(black_box(1_000), black_box(1_000)));
}

/// Y cuando si lo hubo: tres RMW sobre tres lineas compartidas.
#[bench]
fn note_a_credit_that_was_given(b: &mut Bencher) {
    let c = SliceCredit::new();
    b.iter(|| c.note(black_box(1_000), black_box(2_000)));
}

// ── lo que el camino de trampa pregunta en cada tick ───────────────────────

/// La rodaja de un hilo `SCHED_NORMAL` con nice 0: la tabla de pesos y un
/// recorte.
#[bench]
fn work_out_the_slice_of_a_nice_zero_thread(b: &mut Bencher) {
    let t = a_thread();
    b.iter(|| black_box(t.timeslice_ns()));
}

/// La de un `SCHED_FIFO`, que es el `u64::MAX` por el que salen las tres
/// preguntas del tick antes de leer el reloj. Es el suelo de la familia de
/// abajo: lo que cuesta un tick al que no hay nada que preguntarle.
#[bench]
fn work_out_the_slice_of_an_untimesliced_thread(b: &mut Bencher) {
    let t = a_thread();
    t.set_sched(SCHED_FIFO, 0, 50);
    b.iter(|| black_box(t.timeslice_ns()));
}

/// **La primera pregunta del tick**, en el estado en el que esta un hilo de
/// calculo en casi todos los ticks: rodaja en curso, tiempo por delante.
/// Lectura de reloj, credito, veredicto, dos `store` y un contador.
#[bench]
fn ask_a_running_thread_whether_its_slice_expired(b: &mut Bencher) {
    let t = a_thread_mid_slice();
    b.iter(|| black_box(t.tick_should_preempt()));
}

/// La misma pregunta a un `SCHED_FIFO`, que sale antes de leer el reloj. La
/// diferencia con la fila de arriba es lo que cuesta el tick de un hilo al que
/// SI se le cuenta el tiempo.
#[bench]
fn ask_an_untimesliced_thread_whether_its_slice_expired(b: &mut Bencher) {
    let t = a_thread();
    t.set_sched(SCHED_FIFO, 0, 50);
    b.iter(|| black_box(t.tick_should_preempt()));
}

/// **La segunda pregunta del tick**: si hay un despertar pendiente, puede
/// quitarle la CPU ya? Segunda lectura de reloj y segundo `credited_slice_end`
/// del mismo tick.
#[bench]
fn ask_whether_a_pending_wake_may_preempt(b: &mut Bencher) {
    let t = a_thread_mid_slice();
    b.iter(|| black_box(t.sched_may_preempt_on_wake()));
}

/// **La tercera pregunta del tick**, la que solo se hace cuando la segunda
/// dijo no: tercera lectura de reloj y tercer `credited_slice_end`.
#[bench]
fn ask_where_the_parity_floor_of_a_thread_ends(b: &mut Bencher) {
    let t = a_thread_mid_slice();
    b.iter(|| black_box(t.sched_wake_preempt_floor_end()));
}

/// **El tick entero de un hilo en bucle de calculo sin nada que lo despierte**,
/// que es el caso de lejos mas frecuente: solo la primera pregunta, porque
/// `need_resched_pending()` corta las otras dos.
///
/// Aqui se mide como la primera pregunta sola a proposito: lo que el camino de
/// trampa añade encima es una carga relajada de una mascara global, que se mide
/// en el crate del planificador y no aqui.
#[bench]
fn the_whole_tick_of_a_thread_in_a_compute_loop(b: &mut Bencher) {
    let t = a_thread_mid_slice();
    b.iter(|| black_box(t.tick_should_preempt()));
}

/// **El tick entero cuando hay un despertar pendiente y el suelo lo deniega**:
/// las tres preguntas, tal como las hace `loader/src/linux.rs`.
///
/// Esta es la fila que hay que comparar con
/// `ask_a_running_thread_whether_its_slice_expired`: lo que la separa de ella
/// son dos lecturas de reloj y dos `credited_slice_end` mas, sobre un `now`
/// que el tick ya tenia en la mano.
#[bench]
fn the_whole_tick_when_a_wake_is_pending_and_denied(b: &mut Bencher) {
    let t = a_thread();
    // Rodaja recien empezada, asi que el despertar cae DENTRO del suelo de
    // RUN_TO_PARITY y se hacen las tres preguntas.
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    t.sched
        .slice_end_ns
        .store(now + BASE_TIMESLICE_NS, Ordering::Relaxed);
    t.sched
        .last_tick_ns
        .store(now.saturating_sub(SCHED_TICK_NS), Ordering::Relaxed);
    b.iter(|| {
        let expired = black_box(t.tick_should_preempt());
        let may = black_box(t.sched_may_preempt_on_wake());
        if !may {
            black_box(t.sched_wake_preempt_floor_end());
        }
        black_box(expired)
    });
}

/// Lo que paga una reanudacion: el `swap` del marcador de aparcado y, cuando
/// habia resto apuntado, una lectura de reloj y la regla de lag.
#[bench]
fn resume_a_thread_that_had_parked(b: &mut Bencher) {
    let t = a_thread();
    let now = kernel_hal::timer::timer_now().as_nanos() as u64;
    b.iter(|| {
        t.sched.parked.store(true, Ordering::Relaxed);
        t.sched
            .slice_left_ns
            .store(BASE_TIMESLICE_NS / 4, Ordering::Relaxed);
        t.sched
            .parked_at_ns
            .store(now.saturating_sub(BASE_TIMESLICE_NS / 8), Ordering::Relaxed);
        t.sched_note_resumed();
    });
}

/// Y la misma llamada sobre un hilo que no estaba aparcado, que es lo que pasa
/// en cada `poll` que no viene de una espera: un `swap` y nada mas.
#[bench]
fn resume_a_thread_that_had_not_parked(b: &mut Bencher) {
    let t = a_thread();
    b.iter(|| t.sched_note_resumed());
}
