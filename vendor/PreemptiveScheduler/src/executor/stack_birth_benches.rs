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
//! 4. **Envenena la zona utilizable entera, siempre**: [`STACK_SIZE`] bytes de
//!    `0xDEADBEEF_DEADBEEF`, venga el bloque del fondo o del monton.
//!
//! El cuarto paso es el que estas filas miden, porque es el unico que no tiene
//! ninguna salida rapida y el unico cuyo tamaño es la pila completa. Y la
//! forma en que esta escrito importaba tanto como el tamaño: era
//! `write_volatile` en un bucle, y una escritura volatil tiene que ocurrir
//! exactamente como esta escrita, asi que el compilador **no podia** juntarlas
//! ni vectorizarlas. Lo que corria era una tienda de 8 bytes por iteracion,
//! 262.144 veces, sobre 2 MiB que no caben en ninguna cache de datos de un
//! nucleo. **Esta tanda lo cambia por un `fill` con una barrera detras**, que
//! escribe los mismos bytes dejando elegir el ancho de la tienda.
//!
//! De ahi el trio de filas:
//!
//! - `poison_a_whole_stack_one_volatile_word_at_a_time` es el bucle **tal como
//!   estaba**.
//! - `poison_a_whole_stack_with_a_fill_and_a_fence` escribe exactamente los
//!   mismos bytes en la misma region, pero sin volatil y con una barrera de
//!   compilador detras, que es lo que el `volatile` estaba ahi para conseguir
//!   (que las escrituras no se eliminen por muertas). Lo que las separa es el
//!   ancho de la tienda, no el contenido, y eso lo piña el test
//!   `the_fill_and_the_volatile_loop_write_the_same_bytes`. **Es lo que
//!   `Executor::new` hace desde esta tanda.**
//! - `poison_only_the_deepest_sixty_four_kilobytes` y sus cuatro hermanas de
//!   la curva son la cota: lo que costaria envenenar solo la parte de la pila
//!   que un marco llega a tocar.
//!
//! # El hallazgo: el `volatile` costaba una cuarta parte de la preempcion
//!
//! Medido en este contenedor, mediana de cuatro vueltas:
//!
//! | fila | coste |
//! | --- | --- |
//! | `poison_a_whole_stack_one_volatile_word_at_a_time` | **105.100 ns** |
//! | `poison_a_whole_stack_with_a_fill_and_a_fence` | **76.100 ns** |
//!
//! Los mismos bytes en la misma region, **29.000 ns menos**: algo mas de una
//! cuarta parte de todo lo que cuesta una preempcion a mitad de poll, por
//! dejar que el compilador elija el ancho de la tienda. El `volatile` nunca
//! iba de orden ni de que otra CPU viera estas escrituras — un bloque llega
//! aqui despues de que el periodo de gracia lo haya retirado, asi que nadie
//! mas puede tocarlo —, iba de que las escrituras no se tiraran por muertas, y
//! eso lo dice la barrera de compilador una vez en vez de 262.144 veces.
//!
//! # La curva: acotar vale lo que su proporcion, ni mas ni menos
//!
//! Las filas de cota, todas sobre el mismo buffer de 2 MiB, mediana de cuatro
//! vueltas:
//!
//! | tamaño | palabras | coste | por palabra |
//! | --- | --- | --- | --- |
//! | 4 KiB | 512 | 190 ns | 0,37 ns |
//! | 32 KiB | 4.096 | 1.619 ns | 0,40 ns |
//! | 64 KiB | 8.192 | 3.160 ns | 0,39 ns |
//! | 256 KiB | 32.768 | 12.700 ns | 0,39 ns |
//! | 1 MiB | 131.072 | 49.300 ns | 0,38 ns |
//! | 2 MiB | 262.144 | 105.100 ns | 0,40 ns |
//!
//! **No hay rodilla.** El coste por palabra es el mismo, 0,37-0,40 ns, a lo
//! largo de un factor 512 de tamaño: ni la L1, ni la L2, ni el salto a RAM se
//! ven. Asi que una cota vale exactamente su proporcion y **el tamaño que se
//! elija es una cuestion de seguridad, no de rendimiento**: no hay un numero
//! magico que compre mas de lo que mide.
//!
//! Lo que si cambia con el tamaño es **cuanto costaba el `volatile`**: a 2 MiB
//! era 1,38x y a 64 KiB es 1,97x (`fill_only_the_deepest_sixty_four_kilobytes`,
//! 1.601 ns frente a 3.160). A 2 MiB las dos formas chocan con el ancho de
//! banda a RAM y el ancho de la tienda importa menos; cuanto mas cabe la region
//! en cache, mas se nota. O sea que si el envenenado se acotara alguna vez,
//! haberle quitado el `volatile` valdria **mas** alli que aqui.
//!
//! # Por que acotarlo NO sale gratis
//!
//! La cuenta invita a acotar: 2 MiB -> 64 KiB serian 105.100 ns -> 1.601 ns con
//! el `fill`, 66 veces menos. Lo que lo impide no es el coste, es lo que la
//! puerta `classify_parked_frame` contesta sobre las palabras que la cota
//! dejaria sin tocar, y son tres contenidos con tres respuestas (los tres
//! tests estan en `runtime::probe_tests::executor_gates`):
//!
//! - **envenenado**: `RipNotInText`, y `looks_like_a_smash()`. Rechazado y
//!   denunciado, hueco a hueco.
//! - **cero** (paginas de camaradas recien nacidas): `DeadPageTable`. Tambien
//!   rechazado y denunciado; para un bloque NUEVO el envenenado no cambia el
//!   veredicto, solo lo hace legible.
//! - **lo que dejo el ocupante anterior** (un bloque del fondo de pilas):
//!   marcos de verdad, con un `cr3` vivo y una `rip` dentro del texto del
//!   kernel. O sea **el unico contenido que la puerta acepta**. Un `ret` ahi no
//!   es un `rip` malo que se informa: es un salto a codigo del kernel con la
//!   pila de otro.
//!
//! Asi que una cota solo es segura si cubre todo lo que el ocupante anterior
//! escribio, y de eso el crate **no lleva cuenta por bloque**:
//! `stack_high_water()` es un solo global, se muestrea unicamente en
//! `sched_yield`, y una cadena que baje mas sin aparcar no lo mueve. Es un
//! diagnostico, no una cota. Dicho en la forma de las tres tandas anteriores:
//! el re-envenenado de un bloque del fondo es otro recorrido de capacidad en
//! vez de ocupacion, **pero esta vez no hay contador que lo acote, y ponerlo
//! es lo que habria que construir primero** (una profundidad apuntada por
//! bloque donde el bloque se retira).
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
//! | **envenenar los 2 MiB** | `poison_a_whole_stack_with_a_fill_and_a_fence` | **76.100 ns** |
//! | tres barridos de cuarentena | `sweep_the_retired_table_with_nothing_in_it` x3 | 26 ns |
//! | devolver el bloque al fondo | `give_a_stack_back_to_an_empty_pool` | 15 ns |
//!
//! Todo lo que no es el envenenado suma **unos 150 ns**: el **0,2 %**. Dicho
//! al derecho: **una preempcion a mitad de poll es el envenenado y nada mas**.
//! Las tres tandas anteriores encontraron barridos de tabla fija de verdad
//! (512 huecos en cada malloc, 256 en cada punto de planificacion, 128 RMW en
//! cada `Executor::new`) y los tres estaban bien arreglados, pero los tres
//! juntos caben en ese 0,2 %. Y `poison_only_the_deepest_sixty_four_kilobytes`
//! dice lo que costaria una preempcion si el envenenado se acotara a lo que un
//! marco llega a tocar: **unos 3 us en vez de 105** --- por que eso no sale
//! gratis esta arriba.
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

use super::{
    poison_usable_region, GUARD_SIZE, GUARD_WORDS, STACK_CANARY, STACK_POISON, STACK_SIZE,
};
use alloc::vec;
use test::{black_box, Bencher};

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

/// Una fila de la curva: las `bytes` ultimas palabras de un buffer de
/// `STACK_SIZE`, con el bucle volatil tal como esta en `Executor::new`.
fn poison_deepest(b: &mut Bencher, bytes: usize) {
    let total = STACK_SIZE / core::mem::size_of::<u64>();
    let n = bytes / core::mem::size_of::<u64>();
    let mut buf = words(total);
    let from = total - n;
    b.iter(|| {
        poison_volatile(
            black_box(unsafe { buf.as_mut_ptr().add(from) }),
            black_box(n),
        );
        black_box(buf[from])
    });
}

/// Los mismos bytes en la misma region, sin volatil y con la barrera detras:
/// [`poison_usable_region`], que es lo que `Executor::new` llama.
fn poison_filled(buf: &mut [u64]) {
    // SAFETY: `buf` es un trozo vivo y exclusivo del llamante, alineado a 8.
    unsafe { poison_usable_region(buf.as_mut_ptr() as usize, buf.len() * 8) };
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

// ── la curva: que se gana acotando el envenenado ──────────────────────
//
// `poison_only_the_deepest_sixty_four_kilobytes` da UN punto de la curva, y un
// punto no dice si acotar paga. Estas filas dan la forma: si el coste es lineal
// en el tamaño sin rodilla, cualquier cota vale lo que su proporcion y el
// numero que se elija es una cuestion de seguridad y no de rendimiento; si hay
// una rodilla donde la region deja de caber en la cache, una cota por debajo
// vale MAS que su proporcion.
//
// Todas escriben en el mismo buffer de 2 MiB, sus `n` ultimas palabras: la pila
// crece hacia abajo desde el tope, asi que la zona que un marco toca es el
// final del bloque, y darle a cada fila un buffer de su tamaño cambiaria
// tambien la huella de cache.

/// Cuatro KiB: una pagina, la cota mas agresiva que tiene sentido.
#[bench]
fn poison_only_the_deepest_four_kilobytes(b: &mut Bencher) {
    poison_deepest(b, 4 * 1024);
}

/// Treinta y dos KiB.
#[bench]
fn poison_only_the_deepest_thirty_two_kilobytes(b: &mut Bencher) {
    poison_deepest(b, 32 * 1024);
}

/// Doscientos cincuenta y seis KiB.
#[bench]
fn poison_only_the_deepest_two_hundred_fifty_six_kilobytes(b: &mut Bencher) {
    poison_deepest(b, 256 * 1024);
}

/// Un MiB: la mitad de la zona utilizable.
#[bench]
fn poison_only_the_deepest_one_mebibyte(b: &mut Bencher) {
    poison_deepest(b, 1024 * 1024);
}

/// Los mismos 64 KiB de la cota, pero con el `fill` en vez del bucle volatil.
/// Leida contra `poison_only_the_deepest_sixty_four_kilobytes` dice si lo que
/// cuesta el `volatile` es proporcional al tamaño o un peaje fijo: en la
/// pareja de 2 MiB son ~20 us de diferencia, asi que si aqui la diferencia es
/// ~1/32 de eso, el `volatile` se paga por palabra y no por llamada.
#[bench]
fn fill_only_the_deepest_sixty_four_kilobytes(b: &mut Bencher) {
    let total = STACK_SIZE / core::mem::size_of::<u64>();
    let n = 64 * 1024 / core::mem::size_of::<u64>();
    let mut buf = words(total);
    let from = total - n;
    b.iter(|| {
        poison_filled(black_box(&mut buf[from..]));
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
    ///
    /// Y es ademas la unica prueba que tiene el camino de produccion:
    /// `poison_usable_region` la llama `Executor::new`, que en el anfitrion no
    /// corre (lee `cr3`), asi que lo que se puede comprobar aqui es que la
    /// funcion escribe lo mismo que escribia el bucle volatil, palabra por
    /// palabra y en toda la region.
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

    /// Y la region entera, los dos extremos incluidos: una mutacion que
    /// escriba una palabra de menos por cualquiera de las dos puntas deja un
    /// hueco que la puerta aceptaria.
    #[test]
    fn the_poison_reaches_both_ends_of_the_region_and_stops_there() {
        const N: usize = 4096;
        // Dos palabras de centinela a cada lado, que no se deben tocar.
        let mut buf = words(N + 4);
        let region = buf[2..N + 2].as_mut_ptr() as usize;
        // SAFETY: `region` son N palabras vivas y exclusivas de este test.
        unsafe { poison_usable_region(region, N * 8) };
        assert_eq!(buf[2], STACK_POISON, "la primera palabra");
        assert_eq!(buf[N + 1], STACK_POISON, "la ultima palabra");
        assert!(
            buf[2..N + 2].iter().all(|w| *w == STACK_POISON),
            "quedo un hueco sin envenenar dentro de la region"
        );
        assert_eq!(
            (buf[0], buf[1], buf[N + 2], buf[N + 3]),
            (0, 0, 0, 0),
            "el envenenado se salio de su region"
        );
    }

    /// El patron sigue siendo uno que ninguna direccion de retorno del kernel
    /// puede ser. Ya no hace falta vigilar una copia --- las filas importan
    /// `STACK_POISON` de `executor` ---, pero si que el valor siga sirviendo
    /// para lo que esta: `classify_parked_frame` lo rechaza porque no cae en el
    /// texto del kernel, y lo rechaza como SOSPECHA DE ESCRITURA, no como
    /// puntero raro (ver `a_recycled_frame_is_the_one_content_the_gate_accepts`
    /// en `runtime::probe_tests`).
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
