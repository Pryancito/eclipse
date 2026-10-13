//! Lo que hace SEGURA la cota del envenenado de pila.
//!
//! El bloque de comentario de [`super::poison_bound`] lleva el argumento. Aqui
//! estan las cuatro cosas que, si se rompen, convierten la cota en el
//! `[null-exec]`:
//!
//! 1. **Sin gancho, se envenena todo.** La cota es una optimizacion con
//!    permiso del hardware, no el caso por omision.
//! 2. **Un `None` del gancho es «todo», no «nada».** Es el camino que toman un
//!    mapeo enorme, riscv64 y aarch64.
//! 3. **El envenenado va al FINAL de la region.** La pila crece hacia abajo
//!    desde el tope, asi que lo escrito por el ocupante anterior esta arriba:
//!    envenenar los primeros `bound` bytes en vez de los ultimos deja intactos
//!    justo los marcos que la cota existe para tapar, y no lo nota nadie
//!    porque las dos versiones escriben la misma cantidad.
//! 4. **La cota se redondea hacia ARRIBA a pagina y se recorta a la region.**
//!    El bit de sucio es por pagina de 4 KiB: una cota a media pagina dejaria
//!    la otra media sucia sin tocar.
//!
//! Lo que estos tests NO pueden comprobar es el gancho de verdad: leer un bit
//! `D` de un PTE necesita tablas de paginas, y `Executor::new` no corre en el
//! anfitrion de todas formas (lee `cr3`). Lo que se prueba aqui es **la
//! decision**, con ganchos de mentira que contestan lo que haga falta,
//! incluido lo imposible.

extern crate std;

use super::{
    poison_bound, poison_usable_region, poison_usable_region_bounded, with_dirty_depth_hook,
    PAGE_SIZE, STACK_POISON,
};
use alloc::vec;

/// Un bloque de `pages` paginas lleno de un valor que no es el veneno, para
/// poder ver exactamente que palabras se tocaron.
const CLEAN: u64 = 0x1111_1111_1111_1111;

fn block(pages: usize) -> vec::Vec<u64> {
    vec![CLEAN; pages * PAGE_SIZE / 8]
}

/// Cuantos bytes del final del bloque quedaron envenenados, y si el veneno es
/// un tramo contiguo que llega hasta el tope. Devuelve `None` cuando hay
/// veneno suelto por debajo de ese tramo, que es lo que delata un envenenado
/// escrito en el extremo equivocado.
fn poisoned_suffix(buf: &[u64]) -> Option<usize> {
    let first = buf.iter().position(|w| *w == STACK_POISON);
    match first {
        None => Some(0),
        Some(i) => {
            if buf[i..].iter().all(|w| *w == STACK_POISON) {
                Some((buf.len() - i) * 8)
            } else {
                None
            }
        }
    }
}

// ── 1 y 2: cuando no hay cota, se envenena todo ────────────────────────────

#[test]
fn with_no_hook_registered_the_whole_region_is_poisoned() {
    let _g = with_dirty_depth_hook(None);
    let size = 4 * PAGE_SIZE;
    assert_eq!(poison_bound(0x1000, size), size);
}

#[test]
fn a_hook_that_cannot_answer_means_the_whole_region() {
    fn cannot(_base: usize, _size: usize) -> Option<usize> {
        None
    }
    let _g = with_dirty_depth_hook(Some(cannot));
    let size = 4 * PAGE_SIZE;
    assert_eq!(
        poison_bound(0x1000, size),
        size,
        "un mapeo enorme, riscv64 o aarch64: `None` es «todo», no «nada»"
    );
}

// ── 3: el extremo por el que se envenena ───────────────────────────────────

/// El test que separa la cota correcta de la que parece correcta: con una cota
/// de una pagina, el veneno tiene que estar en la ULTIMA pagina.
#[test]
fn the_poison_lands_at_the_top_of_the_region_where_the_frames_are() {
    fn one_page(_base: usize, _size: usize) -> Option<usize> {
        Some(PAGE_SIZE)
    }
    let _g = with_dirty_depth_hook(Some(one_page));
    let mut buf = block(4);
    let base = buf.as_mut_ptr() as usize;
    let size = buf.len() * 8;
    // SAFETY: `[base, base + size)` es el buffer vivo de este test.
    unsafe { poison_usable_region_bounded(base, size) };

    assert_eq!(
        poisoned_suffix(&buf),
        Some(PAGE_SIZE),
        "la cota tiene que envenenar la ultima pagina, que es por donde \
         entra la pila"
    );
    assert_eq!(
        buf[0], CLEAN,
        "la primera palabra del bloque no es lo que un marco toca"
    );
    let words = PAGE_SIZE / 8;
    assert!(
        buf[buf.len() - words..].iter().all(|w| *w == STACK_POISON),
        "la ultima pagina entera"
    );
    assert_eq!(
        buf[buf.len() - words - 1],
        CLEAN,
        "y ni una palabra por debajo de la cota"
    );
}

/// Y con cota igual a la region, lo mismo que antes de todo esto: el bloque
/// entero, sin un hueco.
#[test]
fn a_bound_of_the_whole_region_poisons_every_word() {
    fn everything(_base: usize, size: usize) -> Option<usize> {
        Some(size)
    }
    let _g = with_dirty_depth_hook(Some(everything));
    let mut buf = block(3);
    let base = buf.as_mut_ptr() as usize;
    let size = buf.len() * 8;
    // SAFETY: como arriba.
    unsafe { poison_usable_region_bounded(base, size) };
    assert_eq!(poisoned_suffix(&buf), Some(size));
}

/// La cota acotada escribe lo MISMO que el envenenado sin cota en el tramo que
/// cubre: si no, la comparacion de coste entre las dos no diria nada.
#[test]
fn the_bounded_poison_writes_what_the_unbounded_one_writes() {
    fn two_pages(_base: usize, _size: usize) -> Option<usize> {
        Some(2 * PAGE_SIZE)
    }
    let _g = with_dirty_depth_hook(Some(two_pages));
    let mut bounded = block(4);
    let mut whole = block(4);
    let size = bounded.len() * 8;
    // SAFETY: los dos son buffers vivos y exclusivos de este test.
    unsafe {
        poison_usable_region_bounded(bounded.as_mut_ptr() as usize, size);
        let top = whole.as_mut_ptr() as usize + size - 2 * PAGE_SIZE;
        poison_usable_region(top, 2 * PAGE_SIZE);
    }
    assert_eq!(bounded, whole);
}

// ── 4: redondeo y recorte ──────────────────────────────────────────────────

/// El bit de sucio es por pagina: una cota a media pagina sube a la pagina
/// entera. Redondear hacia abajo dejaria la otra media sucia sin tocar.
#[test]
fn a_bound_inside_a_page_grows_to_the_whole_page() {
    fn half_a_page(_base: usize, _size: usize) -> Option<usize> {
        Some(PAGE_SIZE / 2)
    }
    let _g = with_dirty_depth_hook(Some(half_a_page));
    assert_eq!(poison_bound(0x1000, 8 * PAGE_SIZE), PAGE_SIZE);
}

#[test]
fn one_byte_of_dirt_still_costs_its_page() {
    fn one_byte(_base: usize, _size: usize) -> Option<usize> {
        Some(1)
    }
    let _g = with_dirty_depth_hook(Some(one_byte));
    assert_eq!(poison_bound(0x1000, 8 * PAGE_SIZE), PAGE_SIZE);
}

/// Nada sucio es una respuesta legitima --- un bloque nuevo del monton cuyas
/// paginas nadie ha escrito --- y entonces no hay nada que envenenar.
#[test]
fn nothing_dirty_poisons_nothing() {
    fn clean(_base: usize, _size: usize) -> Option<usize> {
        Some(0)
    }
    let _g = with_dirty_depth_hook(Some(clean));
    let size = 8 * PAGE_SIZE;
    assert_eq!(poison_bound(0x1000, size), 0);
    let mut buf = block(2);
    let base = buf.as_mut_ptr() as usize;
    let n = buf.len() * 8;
    // SAFETY: como arriba.
    unsafe { poison_usable_region_bounded(base, n) };
    assert!(
        buf.iter().all(|w| *w == CLEAN),
        "sin nada sucio no se escribe una palabra"
    );
}

/// Un gancho que conteste mas que la region es un gancho roto, y lo que se
/// hace con el es envenenar la region y **no una palabra mas**: pasarse es
/// escribir en la guarda de arriba, que esta desmapeada a proposito.
#[test]
fn a_hook_that_answers_more_than_the_region_is_clamped_not_believed() {
    fn too_much(_base: usize, size: usize) -> Option<usize> {
        Some(size * 4 + 7)
    }
    let _g = with_dirty_depth_hook(Some(too_much));
    let size = 4 * PAGE_SIZE;
    assert_eq!(poison_bound(0x1000, size), size);

    // Y de verdad, con centinelas detras del buffer.
    let mut buf = block(3);
    let pages = buf.len() * 8 / PAGE_SIZE;
    let region = pages * PAGE_SIZE;
    buf.extend_from_slice(&[CLEAN; 4]);
    let base = buf.as_mut_ptr() as usize;
    // SAFETY: `[base, base + region)` cae dentro de `buf`, que es mas grande.
    unsafe { poison_usable_region_bounded(base, region) };
    assert_eq!(
        &buf[region / 8..],
        &[CLEAN; 4],
        "el envenenado se salio de la region"
    );
}

// ── el gancho ve la region que se le pregunta ──────────────────────────────

/// Lo que se le pasa al gancho es la base y el tamaño de la zona utilizable,
/// no los del bloque entero: preguntarle por el bloque incluiria las dos
/// guardas, y la de abajo esta desmapeada.
#[test]
fn the_hook_is_asked_about_the_region_it_is_given() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static SEEN_BASE: AtomicUsize = AtomicUsize::new(0);
    static SEEN_SIZE: AtomicUsize = AtomicUsize::new(0);
    fn record(base: usize, size: usize) -> Option<usize> {
        SEEN_BASE.store(base, Ordering::SeqCst);
        SEEN_SIZE.store(size, Ordering::SeqCst);
        Some(0)
    }
    let _g = with_dirty_depth_hook(Some(record));
    let size = 5 * PAGE_SIZE;
    poison_bound(0xdead_0000, size);
    assert_eq!(SEEN_BASE.load(Ordering::SeqCst), 0xdead_0000);
    assert_eq!(SEEN_SIZE.load(Ordering::SeqCst), size);
}

// ── las cuentas ────────────────────────────────────────────────────────────

/// Los contadores separan los bloques que se acotaron de los que hubo que
/// envenenar enteros, que es lo unico que dira en la maquina si la cota esta
/// haciendo algo o el gancho contesta `None` siempre.
#[test]
fn the_counters_tell_a_bounded_block_from_a_whole_one() {
    fn one_page(_base: usize, _size: usize) -> Option<usize> {
        Some(PAGE_SIZE)
    }
    let _g = with_dirty_depth_hook(Some(one_page));
    let mut buf = block(4);
    let base = buf.as_mut_ptr() as usize;
    let size = buf.len() * 8;
    let before = super::poison_bound_counts();
    // SAFETY: como arriba.
    unsafe { poison_usable_region_bounded(base, size) };
    let after = super::poison_bound_counts();
    assert_eq!(after.0, before.0 + 1, "un bloque acotado");
    assert_eq!(after.1, before.1, "y ninguno entero");
    assert_eq!(
        after.2 - before.2,
        3,
        "tres de las cuatro paginas se ahorraron"
    );

    // El cerrojo del ayudante es un `Mutex` del anfitrion y no es reentrante:
    // pedirlo otra vez sin soltar el primero es un cuelgue, no un fallo.
    drop(_g);
    let _g2 = with_dirty_depth_hook(None);
    let before = super::poison_bound_counts();
    // SAFETY: como arriba.
    unsafe { poison_usable_region_bounded(base, size) };
    let after = super::poison_bound_counts();
    assert_eq!(after.1, before.1 + 1, "sin gancho, un bloque entero");
    assert_eq!(after.0, before.0);
    assert_eq!(after.2, before.2, "y cero paginas ahorradas");
}
