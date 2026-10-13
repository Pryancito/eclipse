//! Lo que cuesta preguntarle a un hilo en que estado esta, que es lo que el
//! retorno a usuario hace TRES veces por syscall.
//!
//! # Por que estas filas
//!
//! El bucle de `loader/src/linux.rs::run_user` es el camino de salida de toda
//! syscall de todo proceso Linux, y pregunta `thread.state() ==
//! ThreadState::Dying` tres veces por vuelta: al volver de `wait_for_run`,
//! despues de mirar las senales, y despues de atender el trap. Las tandas 6 y
//! 7 de este encargo sacaron de ese mismo bucle las dos preguntas que tomaban
//! el cerrojo gordo del PROCESO (34,2 ns -> 3,05); estas tres toman el del
//! HILO, que es [`Thread::inner`], el que guarda tambien el contexto de
//! usuario, la excepcion en curso y el waker.
//!
//! [`Thread::state`] no es un campo: es la pregunta derivada de tres
//! ([`ThreadInner::state`], que mira `suspend_count`, si hay contexto y el
//! campo `state`), y hacerla cuesta el cerrojo. De ahi las filas: una
//! pregunta, las tres de una vuelta, y la derivacion sola con el cerrojo ya
//! tomado, que es lo que dice cuanto de la cifra es la cuenta y cuanto es
//! pedir permiso para hacerla.
//!
//! Lo que salio, antes y despues de publicar el estado al soltar el cerrojo
//! (ver [`super::published_state`]):
//!
//! | fila | antes | despues |
//! |---|---|---|
//! | una pregunta | 8,96 | 1,24 |
//! | las tres de una vuelta | 25,68 | 1,82 |
//! | una con un hilo al lado | 110,75 | 2,72 |
//! | una con dos | 731,54 | 2,77 |
//! | una con tres | 1603,79 | 1,38 |
//! | las tres con dos hilos al lado | 1295,74 | 3,86 |
//!
//! Una columna curva contra una recta: es lo que dice que el coste era el
//! cerrojo y no algo que solo crecia a la vez que el.
//!
//! # Las filas con hilos al lado
//!
//! El cerrojo de un hilo no lo comparten los hermanos como el del proceso,
//! pero tampoco es suyo: lo toman `/proc/<pid>/task/<tid>/stat`, la entrega
//! de senales, `zx_object_get_info` y el propio `set_blocked` de cada syscall
//! que se duerme. El trabajo de fondo de estas filas es
//! [`Thread::get_thread_info`], que es un lector de verdad de ese cerrojo y
//! **sigue tomandolo** pase lo que pase con `state()`: una fila de fondo que
//! dejara de tomar el cerrojo con el arreglo mediria el arreglo contra si
//! mismo.
//!
//! # Lo que estas filas NO dicen
//!
//! - **Las cifras con disputa son puntos de saturacion**, no predicciones: el
//!   contenedor tiene cuatro nucleos compartidos. Lo que establecen es que el
//!   coste es EL CERROJO, porque nada mas de este camino crece con el numero
//!   de hilos que miran al mismo hilo.
//! - **`libos` no tiene IRQs que apagar.** En baremetal `kernel_hal::sync::
//!   Mutex` apaga interrupciones mientras lo tiene, asi que alla la cifra sin
//!   disputa es mayor que aqui y la de disputa, peor.
//!
//! Correr:
//!
//! ```sh
//! cargo bench -p zircon-object --features libos,aspace-separate \
//!     -- task::thread::return_path_benches
//! ```

use super::*;
use crate::object::KernelObject;
use crate::task::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use test::{black_box, Bencher};

extern crate std;

/// Un hilo corriendo, que es el estado en el que esta el que vuelve de una
/// syscall.
fn a_running_thread() -> Arc<Thread> {
    let root = Job::root();
    let proc = Process::create(&root, "bench").expect("un proceso");
    let t = Thread::create(&proc, "bench").expect("un hilo");
    t.lock_inner().state = ThreadState::Running;
    t
}

/// La fila con `n` hilos de verdad preguntandole cosas al MISMO hilo por el
/// mismo cerrojo, que es lo que hacen un barrido de `/proc` o una senal.
fn with_n_others_asking(b: &mut Bencher, t: &Arc<Thread>, others: usize, body: impl Fn()) {
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..others {
        let (t, stop) = (t.clone(), stop.clone());
        handles.push(std::thread::spawn(move || {
            // `Acquire`/`Release` y no `Relaxed`: una carga relajada en un
            // bucle de espera no tiene garantia de progreso en el modelo de
            // memoria, y este bucle no hace nada mas.
            while !stop.load(Ordering::Acquire) {
                for _ in 0..256 {
                    black_box(t.get_thread_info());
                }
                core::hint::spin_loop();
            }
        }));
    }
    b.iter(&body);
    stop.store(true, Ordering::Release);
    for h in handles {
        h.join().expect("un hilo de fondo");
    }
}

// ── una pregunta ───────────────────────────────────────────────────────────

/// Una sola pregunta, sin nadie al lado.
#[bench]
fn ask_a_thread_for_its_state(b: &mut Bencher) {
    let t = a_running_thread();
    b.iter(|| black_box(t.state()));
}

/// La derivacion sola, con el cerrojo ya tomado: tres comparaciones y un
/// `Option::is_none`. Lo que no esta aqui y si en la fila de arriba es el
/// cerrojo.
#[bench]
fn derive_the_state_with_the_lock_already_held(b: &mut Bencher) {
    let t = a_running_thread();
    let inner = t.inner.lock();
    b.iter(|| black_box(inner.state()));
}

/// El cerrojo y nada mas, para poder restar.
#[bench]
fn lock_the_thread_and_drop(b: &mut Bencher) {
    let t = a_running_thread();
    b.iter(|| {
        black_box(t.inner.lock());
    });
}

/// El cerrojo por el guard que publica al soltarse, que es lo que pagan los
/// once mutadores para que la pregunta sea gratis: un `store` relajado y la
/// derivacion, contra la fila de arriba.
#[bench]
fn lock_the_thread_through_the_guard_and_drop(b: &mut Bencher) {
    let t = a_running_thread();
    b.iter(|| {
        black_box(t.lock_inner());
    });
}

/// El control con forma: una llamada que no computa nada, porque el bucle de
/// `b.iter` no es cero.
#[bench]
fn the_shaped_control(b: &mut Bencher) {
    let t = a_running_thread();
    #[inline(never)]
    fn computes_nothing(t: &Arc<Thread>) -> KoID {
        t.id()
    }
    b.iter(|| black_box(computes_nothing(&t)));
}

// ── la vuelta entera del bucle ─────────────────────────────────────────────

/// Las tres preguntas de una vuelta de `run_user`, que es lo que paga una
/// syscall. Las tres son la misma pregunta.
#[bench]
fn ask_three_times_as_one_return_does(b: &mut Bencher) {
    let t = a_running_thread();
    b.iter(|| {
        black_box(t.state() == ThreadState::Dying);
        black_box(t.state() == ThreadState::Dying);
        black_box(t.state() == ThreadState::Dying);
    });
}

// ── con hilos al lado ──────────────────────────────────────────────────────

/// Una pregunta con un hilo mirando al mismo hilo.
#[bench]
fn ask_for_the_state_with_one_other(b: &mut Bencher) {
    let t = a_running_thread();
    let t2 = t.clone();
    with_n_others_asking(b, &t2, 1, move || {
        black_box(t.state());
    });
}

/// Con dos. Es la fila que decide: la de saturacion no.
#[bench]
fn ask_for_the_state_with_two_others(b: &mut Bencher) {
    let t = a_running_thread();
    let t2 = t.clone();
    with_n_others_asking(b, &t2, 2, move || {
        black_box(t.state());
    });
}

/// Con tres, que en este contenedor de cuatro nucleos es la saturacion.
#[bench]
fn ask_for_the_state_with_three_others(b: &mut Bencher) {
    let t = a_running_thread();
    let t2 = t.clone();
    with_n_others_asking(b, &t2, 3, move || {
        black_box(t.state());
    });
}

/// Las tres preguntas de la vuelta, con dos hilos al lado: la fila que dice
/// lo que una syscall paga en una maquina con algo mas corriendo.
#[bench]
fn ask_three_times_with_two_others(b: &mut Bencher) {
    let t = a_running_thread();
    let t2 = t.clone();
    with_n_others_asking(b, &t2, 2, move || {
        black_box(t.state() == ThreadState::Dying);
        black_box(t.state() == ThreadState::Dying);
        black_box(t.state() == ThreadState::Dying);
    });
}
