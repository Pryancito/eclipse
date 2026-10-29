//! Lo que el `mip` decide, con un banco de registros en memoria del host.
//!
//! Lo que **no** se prueba aquí: que un mensaje escrito a `0xff_fffff000`
//! levante una SPI. Eso sólo lo contesta una Pi 5.

use super::*;
use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

/// Un banco de registros del tamaño de la ventana, y un `Mip` encima.
fn trasto(params: MipParams) -> (Mip, &'static mut [u32]) {
    let banco: &'static mut [u32] = Box::leak(vec![0u32; WIN_LEN / 4].into_boxed_slice());
    let base = banco.as_ptr() as usize;
    (unsafe { Mip::new(base, params) }, banco)
}

fn palabra(banco: &[u32], off: usize) -> u32 {
    banco[off / 4]
}

/// Los offsets que escribió una traza, en orden.
fn escrituras(traza: &[mip_fake::Entrada]) -> Vec<usize> {
    traza
        .iter()
        .filter(|(_, v)| v.is_some())
        .map(|(o, _)| *o)
        .collect()
}

// --- el mapa de registros --------------------------------------------------

#[test]
fn the_registers_are_sixteen_bytes_apart_and_not_four() {
    // El error fácil al teclear el mapa: doce registros de 32 bits seguidos
    // acabarían en 0x30, y la ventana mide 0xc0.
    let todos = [
        INT_RAISED,
        INT_CLEARED,
        INT_CFGL_HOST,
        INT_CFGH_HOST,
        INT_MASKL_HOST,
        INT_MASKH_HOST,
        INT_MASKL_VPU,
        INT_MASKH_VPU,
        INT_STATUSL_HOST,
        INT_STATUSH_HOST,
        INT_STATUSL_VPU,
        INT_STATUSH_VPU,
    ];
    for (i, off) in todos.iter().enumerate() {
        assert_eq!(*off, i * 0x10, "registro {}", i);
        assert!(off + 4 <= WIN_LEN, "registro {} se sale", i);
    }
}

#[test]
fn the_high_half_always_follows_its_low_half() {
    assert_eq!(INT_CFGH_HOST - INT_CFGL_HOST, 0x10);
    assert_eq!(INT_MASKH_HOST - INT_MASKL_HOST, 0x10);
    assert_eq!(INT_MASKH_VPU - INT_MASKL_VPU, 0x10);
    assert_eq!(INT_STATUSH_HOST - INT_STATUSL_HOST, 0x10);
    assert_eq!(INT_STATUSH_VPU - INT_STATUSL_VPU, 0x10);
}

// --- el arranque -----------------------------------------------------------

#[test]
fn bringing_it_up_leaves_the_host_open_and_the_vpu_shut() {
    // Un bit a UNO tapa, así que el host a cero es todo destapado y el VPU a
    // unos es todo tapado. Es lo contrario de la escotilla por línea del RP1,
    // donde hay que ENCENDER cada una.
    let (mip, banco) = trasto(MIP0);
    mip.init();
    assert_eq!(palabra(banco, INT_MASKL_HOST), 0);
    assert_eq!(palabra(banco, INT_MASKH_HOST), 0);
    assert_eq!(palabra(banco, INT_MASKL_VPU), 0xffff_ffff);
    assert_eq!(palabra(banco, INT_MASKH_VPU), 0xffff_ffff);
    assert_eq!(palabra(banco, INT_CFGL_HOST), 0xffff_ffff);
    assert_eq!(palabra(banco, INT_CFGH_HOST), 0xffff_ffff);
}

#[test]
fn bringing_it_up_is_six_writes_in_linuxs_order_and_not_one_read() {
    let (mip, _banco) = trasto(MIP0);
    let (_, traza) = mip_fake::trazando(|| mip.init());
    assert_eq!(
        escrituras(&traza),
        vec![
            INT_MASKL_HOST,
            INT_MASKH_HOST,
            INT_MASKL_VPU,
            INT_MASKH_VPU,
            INT_CFGL_HOST,
            INT_CFGH_HOST,
        ]
    );
    assert!(
        traza.iter().all(|(_, v)| v.is_some()),
        "el arranque no consulta el estado inicial, lo impone: {:?}",
        traza
    );
}

#[test]
fn the_registers_linux_defines_and_never_touches_stay_untouched() {
    // `RAISED`, `CLEARED` y los cuatro `STATUS`. Igual que `INTSTATL`/`H` del
    // RP1: están para quien escriba un bucle de «mira el estado y reparte»,
    // que no es como esto funciona.
    let (mip, banco) = trasto(MIP0);
    let (_, traza) = mip_fake::trazando(|| {
        mip.init();
        let _ = mip.alloc();
        let _ = mip.spi(0);
        let _ = mip.msg(0);
    });
    for off in [
        INT_RAISED,
        INT_CLEARED,
        INT_STATUSL_HOST,
        INT_STATUSH_HOST,
        INT_STATUSL_VPU,
        INT_STATUSH_VPU,
    ] {
        assert!(
            !traza.iter().any(|(o, _)| *o == off),
            "se tocó {:#x}, que Linux no toca nunca",
            off
        );
        assert_eq!(palabra(banco, off), 0, "{:#x} cambió", off);
    }
}

// --- el reparto de vectores ------------------------------------------------

#[test]
fn the_first_vector_of_mip_zero_is_zero_and_of_mip_one_is_eight() {
    // Ésta es la trampa del módulo en una línea.
    let (m0, _) = trasto(MIP0);
    let (m1, _) = trasto(MIP1);
    assert_eq!(m0.alloc(), Some(0));
    assert_eq!(m1.alloc(), Some(8));
}

#[test]
fn a_legal_vector_of_mip_one_is_bigger_than_its_vector_count() {
    // `num_msis` es ocho y los vectores legales son 8..=15: la comprobación
    // natural, `hwirq < num_msis`, rechazaría TODOS.
    let (m1, _) = trasto(MIP1);
    for hwirq in 8..16 {
        assert!(m1.hwirq_valido(hwirq), "{} tenía que valer", hwirq);
        assert!(
            hwirq >= MIP1.num_msis,
            "{} es mayor que num_msis, que es el punto",
            hwirq
        );
    }
    for hwirq in [0, 7, 16, 100] {
        assert!(!m1.hwirq_valido(hwirq), "{} no tenía que valer", hwirq);
    }
}

#[test]
fn every_vector_comes_out_once_and_then_they_run_out() {
    let (m1, _) = trasto(MIP1);
    let mut salidos = Vec::new();
    while let Some(v) = m1.alloc() {
        assert!(!salidos.contains(&v), "{} salió dos veces", v);
        salidos.push(v);
    }
    assert_eq!(salidos, (8..16).collect::<Vec<_>>());
    assert_eq!(m1.alloc(), None, "y no hay un noveno");
}

#[test]
fn a_freed_vector_comes_back_out() {
    let (m0, _) = trasto(MIP0);
    assert_eq!(m0.alloc(), Some(0));
    assert_eq!(m0.alloc(), Some(1));
    assert!(m0.free(0));
    assert_eq!(m0.alloc(), Some(0), "el hueco liberado es el que se reusa");
}

#[test]
fn freeing_a_vector_of_mip_one_takes_the_offset_off_again() {
    // Si `free` no restase el desplazamiento, liberar el 8 marcaría libre el
    // hueco 8, que en el `mip1` no existe, y el hueco 0 se quedaría cogido
    // para siempre.
    let (m1, _) = trasto(MIP1);
    assert_eq!(m1.alloc(), Some(8));
    assert!(m1.free(8));
    assert_eq!(m1.alloc(), Some(8), "el mismo vector vuelve a salir");
}

#[test]
fn freeing_something_that_is_not_ours_is_refused_and_frees_nothing() {
    // Un cero en el `mip1` restaría por debajo de cero. Se rechaza en vez de
    // liberar un hueco ajeno.
    let (m1, _) = trasto(MIP1);
    assert_eq!(m1.alloc(), Some(8));
    assert!(!m1.free(0));
    assert!(!m1.free(16));
    assert_eq!(m1.alloc(), Some(9), "el 8 sigue cogido");
}

#[test]
fn all_sixty_four_vectors_of_mip_zero_fit() {
    let (m0, _) = trasto(MIP0);
    for esperado in 0..64 {
        assert_eq!(m0.alloc(), Some(esperado));
    }
    assert_eq!(m0.alloc(), None);
}

// --- la traducción al GIC y al mensaje -------------------------------------

#[test]
fn the_spi_is_the_vector_plus_the_base_and_the_two_mips_do_not_overlap() {
    let (m0, _) = trasto(MIP0);
    let (m1, _) = trasto(MIP1);
    assert_eq!(m0.spi(0), Some(128));
    assert_eq!(m0.spi(63), Some(191));
    // El `mip1` empieza en 247 y su primer vector es el 8, así que su primera
    // SPI es 255 -- no 247, que es lo que dice el árbol y lo que se leería mal.
    assert_eq!(m1.spi(8), Some(255));
    assert_eq!(m1.spi(15), Some(262));
    // Y ninguna SPI del uno cae dentro del rango del cero.
    assert!(m1.spi(8).unwrap() > m0.spi(63).unwrap());
}

#[test]
fn a_vector_that_is_not_ours_has_no_spi_and_no_message() {
    let (m0, _) = trasto(MIP0);
    assert_eq!(m0.spi(64), None);
    assert_eq!(m0.msg(64), None);
    let (m1, _) = trasto(MIP1);
    assert_eq!(m1.spi(0), None);
    assert_eq!(m1.msg(7), None);
}

#[test]
fn every_vector_of_one_mip_shares_the_same_address_and_differs_in_the_data() {
    // No hay una dirección por vector: el vector va en el DATO. Es lo
    // contrario del camino de x86, donde la dirección lleva la CPU.
    let (m0, _) = trasto(MIP0);
    let a = m0.msg(0).unwrap();
    let b = m0.msg(63).unwrap();
    assert_eq!(a.0, b.0, "la dirección es la misma");
    assert_eq!(a.0, 0xff_fffff000);
    assert_eq!(a.1, 0);
    assert_eq!(b.1, 63);
}

#[test]
fn the_two_mips_have_different_addresses_and_they_are_a_page_apart() {
    let (m0, _) = trasto(MIP0);
    let (m1, _) = trasto(MIP1);
    assert_eq!(m0.msg(0).unwrap().0, 0xff_fffff000);
    assert_eq!(m1.msg(8).unwrap().0, 0xff_ffffe000);
    assert_eq!(
        m0.msg(0).unwrap().0 - m1.msg(8).unwrap().0,
        0x1000,
        "una página, que es lo que una dirección de MSI puede distinguir"
    );
}

#[test]
fn the_message_address_does_not_fit_in_thirty_two_bits() {
    // `0xff_fffff000` son 40 bits. Un dispositivo que sólo pueda escribir 32
    // no alcanza el reparto, y la mitad alta del mensaje NO es cero -- que es
    // el descuido que ya tuvo el camino de MSI de este kernel.
    for p in [MIP0, MIP1] {
        assert!(p.msg_addr > u64::from(u32::MAX));
        assert_eq!((p.msg_addr >> 32) as u32, 0xff);
    }
}

// --- el disparo ------------------------------------------------------------

#[test]
fn every_vector_is_edge_triggered_and_there_is_no_choice() {
    // Lo que alguien que venga del RP1 -- donde las líneas son de nivel y hay
    // que acusarlas -- va a dar por configurable.
    for hwirq in [0, 8, 63] {
        assert!(es_de_flanco(hwirq));
    }
}

// --- la máscara que no se usa ----------------------------------------------

#[test]
fn the_mask_bit_crosses_into_the_high_register_at_thirty_two() {
    let (m0, _) = trasto(MIP0);
    assert_eq!(m0.mask_reg(0), Some(INT_MASKL_HOST));
    assert_eq!(m0.mask_reg(31), Some(INT_MASKL_HOST));
    assert_eq!(m0.mask_reg(32), Some(INT_MASKH_HOST));
    assert_eq!(m0.mask_reg(63), Some(INT_MASKH_HOST));
    assert_eq!(m0.mask_bit(0), Some(1));
    assert_eq!(m0.mask_bit(31), Some(1 << 31));
    // Y el bit del 32 es el 0 del registro alto, no el 32 de nada.
    assert_eq!(m0.mask_bit(32), Some(1));
    assert_eq!(m0.mask_bit(63), Some(1 << 31));
}

#[test]
fn computing_the_mask_does_not_write_it() {
    // Expuesto sin usar, a propósito: las dos funciones de Linux que lo
    // tocaban están desactivadas y no coinciden entre sí.
    let (m0, banco) = trasto(MIP0);
    let (_, traza) = mip_fake::trazando(|| {
        let _ = m0.mask_reg(5);
        let _ = m0.mask_bit(5);
    });
    assert!(traza.is_empty(), "no toca el dispositivo: {:?}", traza);
    assert_eq!(palabra(banco, INT_MASKL_HOST), 0);
}

#[test]
fn the_mask_of_a_vector_that_is_not_ours_is_not_a_register() {
    let (m1, _) = trasto(MIP1);
    assert_eq!(m1.mask_reg(0), None);
    assert_eq!(m1.mask_bit(0), None);
    assert_eq!(m1.mask_reg(16), None);
}

// --- los parámetros del árbol ----------------------------------------------

#[test]
fn the_declared_parameters_are_the_ones_in_the_device_tree() {
    assert_eq!(MIP0.msi_base, 128);
    assert_eq!(MIP0.num_msis, 64);
    assert_eq!(MIP0.msi_offset, 0);
    assert_eq!(MIP1.msi_base, 247);
    // Ocho, no veinte: el nodo dice que son veinte en total pero que los otros
    // son dispersos y no consecutivos, y declara ocho.
    assert_eq!(MIP1.num_msis, 8);
    assert_eq!(MIP1.msi_offset, 8);
}

#[test]
fn neither_mip_can_ask_for_a_vector_the_map_cannot_hold() {
    // El mapa son 64 bits y el vector se apunta desplazado, así que el techo
    // no es `num_msis` sino `num_msis + msi_offset`.
    for p in [MIP0, MIP1] {
        assert!(p.num_msis + p.msi_offset <= MAX_VECTORES);
    }
}

#[test]
fn the_window_reaches_the_last_register_and_no_further() {
    assert_eq!(WIN_LEN, 0xc0);
    assert_eq!(INT_STATUSH_VPU + 0x10, WIN_LEN);
}
