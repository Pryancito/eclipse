//! El teclado USB: la tabla de la página HID «Keyboard/Keypad» y el delta
//! entre dos informes, que es todo lo que hay entre los seis bytes que manda
//! el aparato y los eventos que lee un cliente.
//!
//! Los informes de ratón ya tienen su camino cubierto en [`super::tests`]; el
//! del teclado no tenía nada, y es el que decide si una tecla se queda
//! pulsada para siempre.

use super::*;
use crate::sync::Mutex;
use alloc::{boxed::Box, vec::Vec};

/// Un informe de arranque del teclado: modificadores y seis huecos.
fn keys(list: &[u8]) -> [u8; 6] {
    let mut k = [0u8; 6];
    for (slot, u) in k.iter_mut().zip(list) {
        *slot = *u;
    }
    k
}

/// El delta entre dos informes, como lo leería un cliente: los eventos y el
/// array que queda latcheado.
fn delta(prev_m: u8, new_m: u8, prev_k: &[u8], new_k: &[u8]) -> (Vec<(u16, u16, i32)>, [u8; 6]) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let lis = EventListener::<InputEvent>::new();
    let sink = seen.clone();
    lis.subscribe(
        Box::new(move |e: &InputEvent| sink.lock().push((e.event_type as u16, e.code, e.value))),
        false,
    );
    let latched = emit_keyboard_delta(&lis, prev_m, new_m, &keys(prev_k), &keys(new_k));
    let out = seen.lock().clone();
    (out, latched)
}

const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

fn key(code: u16, value: i32) -> (u16, u16, i32) {
    (EV_KEY, code, value)
}

// --- la tabla de la página ------------------------------------------------

#[test]
fn el_bloque_basico_de_la_pagina_no_tiene_huecos() {
    // De «a» (0x04) a «Volume Down» (0x81) la página HID no deja ningún
    // usage sin asignar, así que la tabla tampoco puede dejarlo: un hueco
    // ahí es una tecla que no escribe nada.
    for u in 0x04u8..=0x81 {
        assert!(
            hid_usage_to_linux(u).is_some(),
            "el usage {:#04x} del bloque básico no tiene tecla",
            u
        );
    }
}

#[test]
fn los_indicadores_de_error_y_los_huecos_no_son_teclas() {
    // 1..=3 son ErrorRollOver, POSTFail y ErrorUndefined: el aparato
    // informando de que no sabe qué hay pulsado.
    for u in 0x00u8..=0x03 {
        assert_eq!(hid_usage_to_linux(u), None, "usage {u:#04x}");
    }
    // Y los huecos de verdad de la página, que no se pueden inventar.
    for u in [
        0x82u8, 0x83, 0x84, 0x86, 0x8d, 0x8f, 0x94, 0xa0, 0xb5, 0xdf, 0xe8, 0xff,
    ] {
        assert_eq!(hid_usage_to_linux(u), None, "usage {u:#04x}");
    }
}

#[test]
fn solo_una_tecla_esta_duplicada_y_es_la_del_teclado_ansi() {
    // Dos usages distintos que dan la misma tecla son normalmente una
    // errata, y ya hubo una: 0x32 apuntaba a KEY_102ND y la tecla `#`/`~`
    // de los teclados europeos escribía la del ISO. La única duplicación
    // legítima es la que hace `hid-input.c`: 0x31 (Backslash) y 0x32
    // (Non-US # y ~) comparten KEY_BACKSLASH porque están en el mismo sitio
    // de un teclado ANSI.
    let mut pares: Vec<(u16, u8)> = (0u8..=0xff)
        .filter_map(|u| hid_usage_to_linux(u).map(|c| (c, u)))
        .collect();
    pares.sort();
    let duplicadas: Vec<_> = pares
        .windows(2)
        .filter(|w| w[0].0 == w[1].0)
        .map(|w| (w[0].0, w[0].1, w[1].1))
        .collect();
    assert_eq!(duplicadas, alloc::vec![(KEY_BACKSLASH, 0x31, 0x32)]);
}

#[test]
fn las_anclas_de_la_pagina_hid_estan_donde_dice_la_especificacion() {
    for (usage, code) in [
        (0x04u8, KEY_A),
        (0x1d, KEY_Z),
        (0x1e, KEY_1),
        (0x27, KEY_0),
        (0x28, KEY_ENTER),
        (0x29, KEY_ESC),
        (0x2a, KEY_BACKSPACE),
        (0x2b, KEY_TAB),
        (0x2c, KEY_SPACE),
        (0x39, KEY_CAPSLOCK),
        (0x3a, KEY_F1),
        (0x45, KEY_F12),
        (0x4f, KEY_RIGHT),
        (0x52, KEY_UP),
        (0x62, KEY_KP0),
        (0x64, KEY_102ND),
        (0x68, KEY_F13),
        (0x73, KEY_F24),
    ] {
        assert_eq!(hid_usage_to_linux(usage), Some(code), "usage {usage:#04x}");
    }
}

// --- el delta entre dos informes -----------------------------------------

#[test]
fn el_bitmap_de_modificadores_va_en_el_orden_de_la_pagina() {
    // El bit `n` del byte de modificadores es el usage 0xe0 + n. Si las dos
    // listas se desordenan por separado, un Ctrl escribe un Meta.
    for bit in 0u8..8 {
        let (eventos, _) = delta(0, 1 << bit, &[], &[]);
        let esperado = hid_usage_to_linux(0xe0 + bit).expect("modificador sin tecla");
        assert_eq!(eventos, alloc::vec![key(esperado, 1), SYN], "bit {bit}");
    }
}

#[test]
fn un_modificador_que_se_suelta_emite_su_cero() {
    let (eventos, _) = delta(0x02, 0x00, &[], &[]);
    assert_eq!(eventos, alloc::vec![key(KEY_LEFTSHIFT, 0), SYN]);
}

#[test]
fn un_informe_sin_cambios_es_solo_un_sync() {
    let (eventos, latched) = delta(0x02, 0x02, &[0x04], &[0x04]);
    assert_eq!(eventos, alloc::vec![SYN]);
    assert_eq!(latched, keys(&[0x04]));
}

#[test]
fn las_pulsaciones_van_antes_que_las_sueltas() {
    // Se suelta la «a» y se pulsa la «b» en el mismo informe: el orden
    // importa, porque un cliente que lee «b arriba» antes de «a abajo»
    // puede repetir la tecla equivocada.
    let (eventos, latched) = delta(0, 0, &[0x04], &[0x05]);
    assert_eq!(eventos, alloc::vec![key(KEY_B, 1), key(KEY_A, 0), SYN]);
    assert_eq!(latched, keys(&[0x05]));
}

#[test]
fn una_tecla_que_cambia_de_hueco_no_se_vuelve_a_pulsar() {
    // El array de seis huecos no tiene orden: el aparato puede mover una
    // tecla pulsada de sitio cuando otra se suelta, y eso no es un evento.
    let (eventos, _) = delta(0, 0, &[0x04, 0x05], &[0x05, 0x04]);
    assert_eq!(eventos, alloc::vec![SYN]);
}

#[test]
fn soltar_todo_suelta_todas_las_teclas() {
    let (eventos, latched) = delta(0, 0, &[0x04, 0x05, 0x06], &[]);
    assert_eq!(
        eventos,
        alloc::vec![key(KEY_A, 0), key(KEY_B, 0), key(KEY_C, 0), SYN]
    );
    assert_eq!(latched, keys(&[]));
}

#[test]
fn un_rollover_no_suelta_lo_que_estaba_pulsado() {
    // Cuando hay más teclas abajo de las que el informe puede llevar, el
    // teclado rellena los seis huecos con ErrorRollOver: está diciendo que
    // no sabe qué hay pulsado, no que se haya soltado todo. Tomárselo al pie
    // de la letra soltaba y volvía a pulsar las teclas de movimiento en
    // cuanto se levantaba una.
    let (eventos, latched) = delta(0, 0, &[0x04, 0x05], &[0x01; 6]);
    assert_eq!(eventos, alloc::vec![SYN]);
    assert_eq!(latched, keys(&[0x04, 0x05]));
}

#[test]
fn los_tres_indicadores_de_error_valen_como_rollover_y_el_cuatro_no() {
    for u in [0x01u8, 0x02, 0x03] {
        let (eventos, latched) = delta(0, 0, &[0x04], &[u]);
        assert_eq!(eventos, alloc::vec![SYN], "indicador {u:#04x}");
        assert_eq!(latched, keys(&[0x04]), "indicador {u:#04x}");
    }
    // 0x04 ya es la «a»: ese informe sí se cree.
    let (eventos, latched) = delta(0, 0, &[0x05], &[0x04]);
    assert_eq!(eventos, alloc::vec![key(KEY_A, 1), key(KEY_B, 0), SYN]);
    assert_eq!(latched, keys(&[0x04]));
}

#[test]
fn un_rollover_no_deja_un_shift_colgado() {
    // Los modificadores son un bitmap, no un array: son fiables siempre, y
    // el rollover no puede dejar un Shift pulsado para el resto de la
    // sesión.
    let (eventos, latched) = delta(0x02, 0x00, &[0x04], &[0x01; 6]);
    assert_eq!(eventos, alloc::vec![key(KEY_LEFTSHIFT, 0), SYN]);
    assert_eq!(latched, keys(&[0x04]));
}

#[test]
fn un_usage_sin_tecla_no_emite_nada_pero_cuenta_como_pulsado() {
    // 0xa0 es un hueco de la página. No hay evento que mandar, pero el
    // array se latchea igual: si no, el usage se trataría como nuevo en cada
    // informe.
    let (eventos, latched) = delta(0, 0, &[], &[0xa0]);
    assert_eq!(eventos, alloc::vec![SYN]);
    assert_eq!(latched, keys(&[0xa0]));
    // Y al soltarlo tampoco sale nada.
    let (eventos, _) = delta(0, 0, &[0xa0], &[]);
    assert_eq!(eventos, alloc::vec![SYN]);
}

#[test]
fn seis_teclas_a_la_vez_salen_las_seis() {
    let (eventos, latched) = delta(0, 0, &[], &[0x04, 0x05, 0x06, 0x07, 0x08, 0x09]);
    assert_eq!(
        eventos,
        alloc::vec![
            key(KEY_A, 1),
            key(KEY_B, 1),
            key(KEY_C, 1),
            key(KEY_D, 1),
            key(KEY_E, 1),
            key(KEY_F, 1),
            SYN
        ]
    );
    assert_eq!(latched, keys(&[0x04, 0x05, 0x06, 0x07, 0x08, 0x09]));
}

#[test]
fn los_modificadores_y_las_teclas_salen_en_el_mismo_informe() {
    // Ctrl+C: el modificador primero, que es el orden que espera un cliente
    // que compone atajos.
    let (eventos, latched) = delta(0, 0x01, &[], &[0x06]);
    assert_eq!(
        eventos,
        alloc::vec![key(KEY_LEFTCTRL, 1), key(KEY_C, 1), SYN]
    );
    assert_eq!(latched, keys(&[0x06]));
}
