//! Lo que se puede comprobar del bloque de interrupciones del RP1 sin tener
//! una Pi 5: en qué alias de la ventana se escribe cada bit, que no se lee
//! para modificar, y que el acuse de después del manejador sólo sale en una
//! línea de nivel. Aquí no hay RP1 y QEMU no lo emula, así que el banco de
//! registros es de mentira.

use super::*;
use alloc::boxed::Box;

/// La ventana del bloque: los tres alias, el último en 0xc00.
const VENTANA: usize = (REG_CLR + 0x400) / 4;

fn bloque() -> (Rp1IrqCfg, usize) {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; VENTANA].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    (unsafe { Rp1IrqCfg::new(base) }, base)
}

fn reg(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn poke(base: usize, off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}

/// Las escrituras de la traza, en orden.
fn escrituras(traza: &[rp1_fake::Entrada]) -> alloc::vec::Vec<(usize, u32)> {
    traza
        .iter()
        .filter_map(|(off, val)| val.map(|v| (*off, v)))
        .collect()
}

// --- la tabla de líneas ----------------------------------------------------

/// La tabla es una permutación densa de `0..61`. El aserto de verdad es de
/// compilación (`tabla_densa`); esto comprueba que lo que dice de la tabla es
/// de ESTA tabla, y de paso fija las dos líneas que importan.
#[test]
fn the_line_table_covers_every_number_once() {
    assert_eq!(LINEAS.len(), 61, "RP1_INT_END");
    let mut vistos = [false; 61];
    for (nombre, n) in LINEAS {
        assert!(n < 61, "{} está fuera de rango: {}", nombre, n);
        assert!(!vistos[n], "{} repite el número {}", nombre, n);
        vistos[n] = true;
    }
    assert!(vistos.iter().all(|v| *v), "falta algún número");
}

/// El 57 está **fuera de sitio** en la lista de Linux: `RP1_INT_PROC_MISC` va
/// escrito detrás del 60. Que el orden del fichero no sea el de los números es
/// exactamente la forma que tiene un número de salir mal copiado, así que
/// queda fijado qué nombre lleva cada uno de los cuatro del final.
#[test]
fn the_four_numbers_at_the_end_are_the_ones_linux_lists_out_of_order() {
    let nombre = |n: usize| LINEAS.iter().find(|(_, m)| *m == n).unwrap().0;
    assert_eq!(nombre(57), "proc_misc");
    assert_eq!(nombre(58), "syscfg");
    assert_eq!(nombre(59), "clocks_default");
    assert_eq!(nombre(60), "vbusctrl");
}

/// Y la que esto existe para poder levantar.
#[test]
fn the_ethernet_line_is_the_sixth() {
    assert_eq!(INT_ETH, 6);
    assert_eq!(LINEAS.iter().find(|(_, n)| *n == 6).unwrap().0, "eth");
}

// --- la traducción ---------------------------------------------------------

/// El `interrupt-controller` del RP1 no tiene tabla: la línea N **es** el
/// vector N. Si no fuese la identidad habría que leerla de algún sitio, y no
/// hay sitio.
#[test]
fn a_line_is_its_own_msix_vector_with_no_table_in_between() {
    for (_, n) in LINEAS {
        assert_eq!(linea_a_vector(n), n);
        assert_eq!(vector_a_linea(n as u32), Some(n));
    }
}

/// La vuelta se queda con **seis** bits, que es lo que hace el manejador
/// encadenado de Linux (`hwirq & 0x3f`): 64 vectores, aunque sólo existan 61.
/// Los tres que sobran no son líneas.
#[test]
fn the_reverse_keeps_six_bits_and_the_three_spare_vectors_are_not_lines() {
    assert_eq!(VECTOR_MASK, 0x3f);
    assert_eq!(vector_a_linea(60), Some(60), "el último que existe");
    assert_eq!(vector_a_linea(61), None);
    assert_eq!(vector_a_linea(63), None);
    // Y un vector de más de seis bits se recorta, no se descarta.
    assert_eq!(vector_a_linea(64), Some(0), "0x40 recortado es 0");
    assert_eq!(vector_a_linea(0x46), Some(6), "y aquí sale el Ethernet");
}

// --- el offset del registro ------------------------------------------------

/// El registro de configuración del vector 0 está en **8**, no en 0. Así que
/// su alias de poner es `0x808` y no `0x800`, que es donde caería quien
/// sumase sólo el alias.
#[test]
fn the_config_register_starts_at_eight_and_not_at_zero() {
    assert_eq!(msix_cfg(0), 0x8);
    assert_eq!(REG_SET + msix_cfg(0), 0x808);
    assert_eq!(REG_CLR + msix_cfg(0), 0xc08);
    assert_eq!(msix_cfg(6), 0x20, "el del Ethernet");
    assert_eq!(msix_cfg(60), 0xf8, "el del último");
}

/// Los tres alias no se pisan: el registro del último vector cae muy por
/// debajo de los 0x400 que separan un alias del siguiente.
#[test]
fn the_three_aliases_do_not_overlap() {
    assert!(msix_cfg(60) < 0x800, "{:#x}", msix_cfg(60));
    assert_eq!(REG_RW, 0x000);
    assert_eq!(REG_SET, 0x800);
    assert_eq!(REG_CLR, 0xc00);
}

// --- poner y quitar --------------------------------------------------------

/// Una línea de nivel se declara **poniendo** `IACK_EN`, y se pone escribiendo
/// en el alias de poner: **nunca leyendo para modificar**. Con tres alias, un
/// read-modify-write sobre el alias directo se llevaría por delante la
/// configuración de ese mismo vector que otro hilo acabase de poner.
#[test]
fn declaring_a_level_line_sets_a_bit_without_reading_anything_first() {
    let (cfg, base) = bloque();
    let (ok, traza) = rp1_fake::trazando(|| cfg.set_disparo(INT_ETH, Disparo::Nivel));
    assert!(ok);
    assert_eq!(
        escrituras(&traza),
        alloc::vec![(REG_SET + msix_cfg(INT_ETH), MSIX_CFG_IACK_EN)]
    );
    assert!(
        traza.iter().all(|(_, v)| v.is_some()),
        "se leyó algo para modificarlo: {:?}",
        traza
    );
    assert_eq!(reg(base, REG_SET + msix_cfg(INT_ETH)), 1 << 3);
}

/// Y una de flanco **quitándolo**, en el alias de quitar. Un cero escrito en
/// el alias de poner no dice nada, así que esto no se puede hacer con un solo
/// alias.
#[test]
fn declaring_an_edge_line_clears_the_same_bit_through_the_other_alias() {
    let (cfg, base) = bloque();
    let (_, traza) = rp1_fake::trazando(|| cfg.set_disparo(INT_ETH, Disparo::Flanco));
    assert_eq!(
        escrituras(&traza),
        alloc::vec![(REG_CLR + msix_cfg(INT_ETH), MSIX_CFG_IACK_EN)]
    );
    assert_eq!(
        reg(base, REG_SET + msix_cfg(INT_ETH)),
        0,
        "no se tocó el alias de poner"
    );
}

/// La puerta de este lado es otro bit del mismo registro, y se abre y se
/// cierra por los mismos dos alias.
#[test]
fn the_enable_bit_is_another_bit_of_the_same_register() {
    let (cfg, base) = bloque();
    assert!(cfg.habilita(INT_ETH));
    assert_eq!(reg(base, REG_SET + msix_cfg(INT_ETH)), MSIX_CFG_ENABLE);
    assert!(cfg.deshabilita(INT_ETH));
    assert_eq!(reg(base, REG_CLR + msix_cfg(INT_ETH)), MSIX_CFG_ENABLE);
    assert_eq!(MSIX_CFG_ENABLE, 1, "el bit 0");
}

/// Los cuatro bits son cuatro bits distintos, y en el sitio que dice Linux.
#[test]
fn the_four_bits_are_the_ones_linux_names() {
    assert_eq!(MSIX_CFG_ENABLE, 1 << 0);
    assert_eq!(MSIX_CFG_TEST, 1 << 1);
    assert_eq!(MSIX_CFG_IACK, 1 << 2);
    assert_eq!(MSIX_CFG_IACK_EN, 1 << 3);
}

/// Una línea que no existe no se programa, y no se toca nada al intentarlo.
#[test]
fn a_line_that_does_not_exist_is_refused_without_touching_the_block() {
    let (cfg, _) = bloque();
    let (_, traza) = rp1_fake::trazando(|| {
        assert!(!cfg.set_disparo(61, Disparo::Nivel));
        assert!(!cfg.habilita(61));
        assert!(!cfg.deshabilita(INT_END));
        assert!(!cfg.ack(100));
    });
    assert!(
        escrituras(&traza).is_empty(),
        "se escribió para una línea que no existe: {:?}",
        traza
    );
}

// --- el acuse --------------------------------------------------------------

/// El acuse de después del manejador sale **sólo** en una línea de nivel. Y
/// eso el registro no lo dice: `IACK_EN` es lo que el chip hace, no lo que el
/// driver tiene que acordarse de hacer, así que lo tiene que recordar el
/// driver.
#[test]
fn only_a_level_line_is_acknowledged_after_its_handler() {
    let (cfg, base) = bloque();
    cfg.set_disparo(INT_ETH, Disparo::Nivel);
    let (ack, traza) = rp1_fake::trazando(|| cfg.ack(INT_ETH));
    assert!(ack);
    assert_eq!(
        escrituras(&traza),
        alloc::vec![(REG_SET + msix_cfg(INT_ETH), MSIX_CFG_IACK)]
    );
    // Qué bits quedan puestos en el chip no se puede comprobar aquí: un banco
    // de palabras no hace de alias de poner, se queda con lo último que se le
    // escribe. Eso es justo por lo que hay traza, y lo que la traza fija es lo
    // que de verdad decide el comportamiento: **en qué alias** se escribió.
    assert_eq!(
        reg(base, REG_SET + msix_cfg(INT_ETH)),
        MSIX_CFG_IACK,
        "lo último escrito en el alias de poner"
    );
}

/// En una de flanco, no: y no es que el acuse sobre, es que sería escribir un
/// bit que el chip no espera.
#[test]
fn an_edge_line_is_not_acknowledged_at_all() {
    let (cfg, _) = bloque();
    cfg.set_disparo(INT_ETH, Disparo::Flanco);
    let (ack, traza) = rp1_fake::trazando(|| cfg.ack(INT_ETH));
    assert!(!ack);
    assert!(
        escrituras(&traza).is_empty(),
        "se acusó una línea de flanco: {:?}",
        traza
    );
}

/// Una línea que no se ha declarado tampoco se acusa. El defecto es «no
/// acusar», que deja la interrupción colgada una vez; el contrario escribiría
/// el acuse en una línea de flanco cada vez.
#[test]
fn a_line_never_declared_is_not_acknowledged_either() {
    let (cfg, _) = bloque();
    assert!(!cfg.es_de_nivel(INT_ETH));
    let (_, traza) = rp1_fake::trazando(|| cfg.ack(INT_ETH));
    assert!(escrituras(&traza).is_empty(), "{:?}", traza);
}

/// Declarar una línea de nivel y luego de flanco **la desapunta**. Si no, un
/// cambio de disparo dejaría acusando para siempre una línea que ya no lo
/// necesita.
#[test]
fn changing_a_line_from_level_to_edge_stops_the_acknowledgement() {
    let (cfg, _) = bloque();
    cfg.set_disparo(INT_ETH, Disparo::Nivel);
    assert!(cfg.es_de_nivel(INT_ETH));
    cfg.set_disparo(INT_ETH, Disparo::Flanco);
    assert!(!cfg.es_de_nivel(INT_ETH));
    assert!(!cfg.ack(INT_ETH));
}

/// Cada línea lleva su propia cuenta: declarar la 6 de nivel no hace de nivel
/// a la 7. Con un solo booleano para todas, el Ethernet le pondría el acuse al
/// I2C0.
#[test]
fn each_line_remembers_its_own_trigger() {
    let (cfg, _) = bloque();
    cfg.set_disparo(INT_ETH, Disparo::Nivel);
    assert!(cfg.es_de_nivel(INT_ETH));
    assert!(!cfg.es_de_nivel(7), "la 7 se contagió de la 6");
    assert!(!cfg.es_de_nivel(60));
    cfg.set_disparo(60, Disparo::Nivel);
    assert!(cfg.es_de_nivel(60));
    assert!(cfg.es_de_nivel(INT_ETH), "la 6 se perdió al declarar la 60");
}

/// Y el mapa de las de nivel cabe en 64 bits para las 61 líneas, incluida la
/// última: un `1 << 60` tiene que caber donde se guarda.
#[test]
fn the_level_map_reaches_the_last_line() {
    let (cfg, _) = bloque();
    for (_, n) in LINEAS {
        cfg.set_disparo(n, Disparo::Nivel);
    }
    for (nombre, n) in LINEAS {
        assert!(cfg.es_de_nivel(n), "{} ({}) se perdió", nombre, n);
    }
}

// --- lo que no se usa ------------------------------------------------------

/// `INTSTATL`/`INTSTATH` son defines que el driver de Linux **no lee nunca**,
/// y con un vector MSI-X por línea no hay nada que repartir. Están declarados
/// para que quede dicho; este test existe para que nadie los «arregle»
/// escribiendo el bucle de despacho que no hace falta.
#[test]
fn the_status_registers_exist_only_to_be_left_alone() {
    assert_eq!(INTSTATL, 0x108);
    assert_eq!(INTSTATH, 0x10c);
    let (cfg, base) = bloque();
    poke(base, INTSTATL, 0xffff_ffff);
    poke(base, INTSTATH, 0xffff_ffff);
    let (_, traza) = rp1_fake::trazando(|| {
        cfg.set_disparo(INT_ETH, Disparo::Nivel);
        cfg.habilita(INT_ETH);
        cfg.ack(INT_ETH);
        cfg.deshabilita(INT_ETH);
    });
    assert!(
        !traza
            .iter()
            .any(|(off, _)| *off == INTSTATL || *off == INTSTATH),
        "alguien miró el estado: {:?}",
        traza
    );
}

/// Y el bit de disparo a mano tampoco se usa en ningún camino.
#[test]
fn the_self_test_bit_is_never_written() {
    let (cfg, _) = bloque();
    let (_, traza) = rp1_fake::trazando(|| {
        cfg.set_disparo(INT_ETH, Disparo::Nivel);
        cfg.set_disparo(7, Disparo::Flanco);
        cfg.habilita(INT_ETH);
        cfg.ack(INT_ETH);
        cfg.deshabilita(INT_ETH);
    });
    for (off, val) in escrituras(&traza) {
        assert_eq!(
            val & MSIX_CFG_TEST,
            0,
            "se disparó una línea a mano en {:#x}",
            off
        );
    }
}

// --- de dónde cuelga el bloque ---------------------------------------------

/// Los dos offsets dentro de BAR1, y el mínimo que delata que el firmware de
/// la Pi no ha arrancado el chip. `0x1_0000` no es un tamaño mínimo
/// arbitrario: es la comprobación con la que el driver de Linux se niega a
/// seguir.
#[test]
fn the_block_hangs_off_bar_one_and_a_short_bar_means_no_firmware() {
    assert_eq!(PCIE_APBS_BASE, 0x10_8000);
    assert_eq!(SYSINFO_BASE, 0x00_0000);
    assert_eq!(BAR1_MIN_LEN, 0x1_0000);
    // El bloque entero cae dentro de una BAR1 que haya pasado la criba.
    assert!(
        (PCIE_APBS_BASE + REG_CLR + 0x400) as u64 > BAR1_MIN_LEN,
        "el bloque está por encima del mínimo, así que el mínimo no basta \
         para alcanzarlo: la criba dice «hay firmware», no «cabe el bloque»"
    );
}

/// Los dos identificadores de chip son distintos y sólo el C0 vale: el driver
/// de Linux declara el B0 y luego lo rechaza.
#[test]
fn only_the_c_zero_chip_id_is_accepted_even_though_the_b_zero_is_declared() {
    assert_ne!(CHIP_ID_B0, CHIP_ID_C0);
    assert_eq!(CHIP_ID_C0, 0x2000_1927);
    assert_eq!(CHIP_ID_B0, 0x1000_1927);
    // Los dos comparten los 16 bits de abajo: lo que cambia es la revisión.
    assert_eq!(CHIP_ID_B0 & 0xffff, CHIP_ID_C0 & 0xffff, "0x1927");
    assert_eq!(PLATFORM_FPGA, 1);
}
