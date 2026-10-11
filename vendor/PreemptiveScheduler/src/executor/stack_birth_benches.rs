//! Lo que cuesta nacer una pila de corrutina, que es lo que paga **cada
//! preempcion a mitad de poll**.
//!
//! # Por que estas filas
//!
//! Un hilo al que el temporizador interrumpe dentro de un poll no vuelve por
//! donde entro: su marco se congela como ejecutor debil y la CPU sigue en uno
//! nuevo. Cada uno de esos ejecutores nuevos es un [`Executor::new`], y
//! `Executor::new` hace, en este orden, cuatro cosas de coste muy distinto:
//!
//! 1. Pide un bloque: una ranura del fondo de pilas, o `ALLOC_SIZE` (~2,6 MiB)
//!    del monton.
//! 2. Instala las dos guardas duras, que son particiones de tabla de paginas
//!    con su derribo de TLB. **Solo si el bloque es nuevo**: uno del fondo ya
//!    las trae puestas.
//! 3. Si no hay guardas duras, escribe el canario blando: [`GUARD_WORDS`]
//!    escrituras volatiles, una por palabra de los 512 KiB de la guarda baja.
//! 4. **Envenena la zona utilizable entera, siempre**: [`STACK_SIZE`] / 8
//!    escrituras volatiles de `0xDEADBEEF_DEADBEEF`, venga el bloque del fondo
//!    o del monton.
//!
//! El cuarto paso es el que estas filas miden, porque es el unico que no tiene
//! ninguna salida rapida y el unico cuyo tamaño es la pila completa. Y la
//! forma en que esta escrito importa tanto como el tamaño: son
//! `write_volatile` en un bucle, y una escritura volatil tiene que ocurrir
//! exactamente como esta escrita, asi que el compilador **no puede** juntarlas
//! ni convertirlas en un `memset`. Lo que corre es una tienda de 8 bytes por
//! iteracion, 262.144 veces, sobre 2 MiB que no caben en ninguna cache de
//! datos de un nucleo.
//!
//! De ahi el trio de filas:
//!
//! - `poison_a_whole_stack_one_volatile_word_at_a_time` es el bucle tal como
//!   esta.
//! - `poison_a_whole_stack_with_a_fill_and_a_fence` escribe exactamente los
//!   mismos bytes en la misma region, pero sin volatil y con una barrera de
//!   compilador detras, que es lo que el `volatile` esta ahi para conseguir
//!   (que las escrituras no se eliminen por muertas). Lo que las separa es el
//!   ancho de la tienda, no el contenido.
//! - `poison_only_the_deepest_sixty_four_kilobytes` es la cota: lo que
//!   costaria envenenar solo la parte de la pila que un marco llega a tocar.
//!   El crate ya sabe cuanto es eso — `stack_high_water()` lo apunta — asi que
//!   la fila esta aqui para poder leer la cuenta, no para proponerla.
//!
//! # Lo que estas filas NO dicen
//!
//! - **No hay guardas duras ni derribos de TLB.** El anfitrion no instala
//!   guardas, asi que el paso 2 no se mide en ninguna fila. En la maquina ese
//!   paso cuesta ademas dos particiones de tabla de paginas y sus derribos; lo
//!   que estas filas acotan es lo que se paga **incluso cuando el bloque viene
//!   del fondo** y el paso 2 se salta entero.
//! - **La cache del anfitrion no es la de la maquina.** El numero absoluto se
//!   mueve con el tamaño de la L2 y con el ancho de banda a RAM. Lo que se
//!   traslada es la RELACION entre las tres filas, y que la primera no cabe en
//!   cache en ninguna maquina.
//! - **No se mide un `Executor::new` entero**, y no se puede: la razon esta
//!   abajo, en «Por que no hay una fila que mida `Executor::new` entero». Las
//!   filas de aqui son puras: un buffer propio, ningun global.
//!
//! # El denominador: con el fondo caliente, el envenenado es TODA la preempcion
//!
//! Las filas de arriba dan el coste del cuarto paso. Las de los otros modulos
//! de este crate dan el de todo lo demas que hace `Executor::new` cuando el
//! bloque viene del fondo de pilas, que es el caso normal del nucleo en
//! regimen (crear y liberar se compensan, asi que el fondo casi nunca esta
//! vacio). Medidas el 11-oct en el mismo contenedor, a minutos unas de
//! otras:
//!
//! | lo que hace una preempcion | fila | coste |
//! | --- | --- | --- |
//! | pedir el bloque al fondo | `take_a_stack_from_a_pool_holding_one_in_the_last_slot` | 50 ns |
//! | publicarlo en los dos registros | `publish_a_stack_into_registries_holding_four` | 55 ns |
//! | **envenenar los 2 MiB** | `poison_a_whole_stack_one_volatile_word_at_a_time` | **94.844 ns** |
//! | tres barridos de cuarentena | `sweep_the_retired_table_with_nothing_in_it` x3 | 26 ns |
//! | devolver el bloque al fondo | `give_a_stack_back_to_an_empty_pool` | 15 ns |
//!
//! Todo lo que no es el envenenado suma **unos 150 ns**: el **0,16 %**. Dicho
//! al derecho: **una preempcion a mitad de poll es el envenenado y nada mas**.
//! Las tres tandas anteriores encontraron barridos de tabla fija de verdad
//! (512 huecos en cada malloc, 256 en cada punto de planificacion, 128 RMW en
//! cada `Executor::new`) y los tres estaban bien arreglados, pero los tres
//! juntos caben en ese 0,16 %. Y `poison_only_the_deepest_sixty_four_kilobytes`
//! dice lo que costaria una preempcion si el envenenado se acotara a lo que un
//! marco llega a tocar: **unos 3 us en vez de 95**.
//!
//! Lo unico que el nucleo paga aqui y el anfitrion no es el paso 2, y **en una
//! preempcion con el fondo caliente el paso 2 no corre**: el bloque trae las
//! guardas puestas. Asi que para ese caso la suma de arriba no tiene hueco.
//!
//! ## Por que no hay una fila que mida `Executor::new` entero
//!
//! No es pureza, es que no se puede: `init_stack_and_context` llama a
//! `crate::arch::pg_base_register()`, que en x86_64 es `mov reg, cr3`. Una
//! instruccion privilegiada en un test del anfitrion es un #GP, o sea
//! SIGSEGV, antes de que la fila mida nada. (Probado, 11-oct.) Por eso el
//! denominador se arma sumando filas y no cronometrando la funcion. Y por eso
//! la reserva del monton en un fallo de fondo tampoco tiene fila: ahi `Global`
//! es el `malloc` del anfitrion, no el asignador de camaradas del nucleo, y la
//! cifra no se trasladaria.
//!
//! Correr:
//!
//! ```sh
//! cargo bench -p executor -- executor::stack_birth_benches
//! ```

use super::{GUARD_SIZE, GUARD_WORDS, STACK_CANARY, STACK_SIZE};
use alloc::vec;
use core::sync::atomic::{compiler_fence, Ordering};
use test::{black_box, Bencher};

/// El valor que `Executor::new` escribe por la zona utilizable. Copiado y no
/// importado porque alla es un `const` dentro del bloque `unsafe`: si alli
/// cambia y aqui no, lo que falla es la igualdad que comprueba
/// `the_fill_and_the_volatile_loop_write_the_same_bytes`.
const STACK_POISON: u64 = 0xDEAD_BEEF_DEAD_BEEF;

/// Un bloque alineado a 8 del tamaño que se le pida, en palabras.
fn words(n: usize) -> vec::Vec<u64> {
    vec![0u64; n]
}

/// El bucle tal como esta en `Executor::new`, sobre `n` palabras.
fn poison_volatile(p: *mut u64, n: usize) {
    // SAFETY: `p` apunta a `n` palabras de un `Vec` vivo del llamante.
    unsafe {
        for i in 0..n {
            core::ptr::write_volatile(p.add(i), STACK_POISON);
        }
    }
}

/// Los mismos bytes en la misma region, sin volatil y con la barrera detras.
fn poison_filled(buf: &mut [u64]) {
    buf.fill(STACK_POISON);
    // Lo que el `volatile` consigue de verdad: que nadie decida que estas
    // escrituras estan muertas porque nada en este hilo las vuelve a leer.
    compiler_fence(Ordering::SeqCst);
}

// ── el envenenado de la zona utilizable, que se paga siempre ───────────────

/// **El paso 4 tal como esta**: `STACK_SIZE / 8` escrituras volatiles de 8
/// bytes, una por iteracion, sobre 2 MiB. Esto es lo que cuesta una preempcion
/// a mitad de poll antes de que el ejecutor nuevo ejecute su primera
/// instruccion.
#[bench]
fn poison_a_whole_stack_one_volatile_word_at_a_time(b: &mut Bencher) {
    let n = STACK_SIZE / core::mem::size_of::<u64>();
    let mut buf = words(n);
    b.iter(|| {
        poison_volatile(black_box(buf.as_mut_ptr()), black_box(n));
        black_box(buf[0])
    });
}

/// Los mismos bytes, dejando que el compilador elija el ancho de la tienda.
/// La diferencia con la fila de arriba es lo que cuesta el `volatile`, no lo
/// que cuesta envenenar.
#[bench]
fn poison_a_whole_stack_with_a_fill_and_a_fence(b: &mut Bencher) {
    let n = STACK_SIZE / core::mem::size_of::<u64>();
    let mut buf = words(n);
    b.iter(|| {
        poison_filled(black_box(&mut buf[..]));
        black_box(buf[0])
    });
}

/// La cota: los 64 KiB mas profundos, que es el orden de lo que un marco de
/// kernel llega a usar de verdad (`stack_high_water()` lleva la cifra exacta).
/// Esta aqui para poder leer la cuenta de la fila de arriba, no como propuesta.
#[bench]
fn poison_only_the_deepest_sixty_four_kilobytes(b: &mut Bencher) {
    let n = 64 * 1024 / core::mem::size_of::<u64>();
    let mut buf = words(STACK_SIZE / core::mem::size_of::<u64>());
    // Lo profundo es el final del bloque: la pila crece hacia abajo desde
    // `stack_base + STACK_SIZE`.
    let from = buf.len() - n;
    b.iter(|| {
        poison_volatile(
            black_box(unsafe { buf.as_mut_ptr().add(from) }),
            black_box(n),
        );
        black_box(buf[from])
    });
}

// ── el canario blando, que solo se paga sin guardas duras ──────────────────

/// El paso 3: [`GUARD_WORDS`] escrituras volatiles por los 512 KiB de la
/// guarda baja, cada una con su `xor` del indice. Solo corre cuando la
/// instalacion de guardas duras no esta disponible o la rechaza, que en
/// baremetal es antes de `stack_guard::init` y en una PTE enorme.
#[bench]
fn write_the_soft_guard_canary(b: &mut Bencher) {
    let mut buf = words(GUARD_WORDS);
    b.iter(|| {
        let p = black_box(buf.as_mut_ptr());
        // SAFETY: `p` apunta a `GUARD_WORDS` palabras del `Vec` de arriba.
        unsafe {
            for i in 0..GUARD_WORDS {
                core::ptr::write_volatile(p.add(i), STACK_CANARY ^ i as u64);
            }
        }
        black_box(buf[0])
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Las dos formas de envenenar escriben los mismos bytes. Es lo que
    /// convierte la comparacion de las dos filas en una comparacion de coste y
    /// no de trabajo: si una escribiera menos, la cifra mas baja no diria nada.
    #[test]
    fn the_fill_and_the_volatile_loop_write_the_same_bytes() {
        const N: usize = 4096;
        let mut a = words(N);
        let mut b = words(N);
        poison_volatile(a.as_mut_ptr(), N);
        poison_filled(&mut b[..]);
        assert_eq!(a, b, "las dos formas de envenenar no escriben lo mismo");
        assert!(
            a.iter().all(|w| *w == STACK_POISON),
            "el envenenado dejo alguna palabra sin escribir"
        );
    }

    /// El valor copiado es el que `Executor::new` escribe. Una fila que midiera
    /// el coste de escribir OTRO patron seguiria siendo valida como coste, pero
    /// la prueba de igualdad de arriba dejaria de hablar del kernel.
    #[test]
    fn the_poison_here_is_the_poison_the_kernel_writes() {
        assert_eq!(
            STACK_POISON, 0xDEAD_BEEF_DEAD_BEEF,
            "el patron de envenenado no es el de Executor::new"
        );
    }

    /// Y las tres filas se leen unas contra otras solo si los tamaños son los
    /// del kernel: 2 MiB de zona utilizable y 512 KiB de guarda baja.
    #[test]
    fn the_sizes_the_rows_use_are_the_kernels() {
        assert_eq!(STACK_SIZE, 4096 * 512, "STACK_SIZE ya no son 2 MiB");
        assert_eq!(GUARD_SIZE, 4096 * 128, "GUARD_SIZE ya no son 512 KiB");
        assert_eq!(
            GUARD_WORDS,
            GUARD_SIZE / core::mem::size_of::<u64>(),
            "GUARD_WORDS no cuenta las palabras de GUARD_SIZE"
        );
    }
}
