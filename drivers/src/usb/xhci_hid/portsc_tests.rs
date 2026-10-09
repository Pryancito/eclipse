use super::*;

/// Every bit xHCI marks RW1C or RW1S in PORTSC, with the name it goes by.
/// `portsc_writeback` must strip all of them from the sample, because
/// carrying any one of them over from a read *acts*.
const WRITE_ONE_DOES_SOMETHING: &[(u32, &str)] = &[
    (1, "PED (deshabilita el puerto)"),
    (4, "PR (relanza un reset)"),
    (17, "CSC (reconoce una conexion)"),
    (18, "PEC (reconoce un cambio de habilitacion)"),
    (19, "WRC (reconoce un warm reset)"),
    (20, "OCC (reconoce una sobrecorriente)"),
    (21, "PRC (reconoce un reset)"),
    (22, "PLC (reconoce un cambio de enlace)"),
    (23, "CEC (reconoce un error de configuracion)"),
    (31, "WPR (relanza un warm reset)"),
];

#[test]
fn a_write_back_carries_over_no_bit_that_writing_one_to_would_act_on() {
    // The worst sample there is: every bit set.
    let out = portsc_writeback(u32::MAX, 0);
    for (bit, name) in WRITE_ONE_DOES_SOMETHING {
        assert_eq!(
            out & (1 << bit),
            0,
            "el bit {} {} viaja de la muestra a la escritura",
            bit,
            name
        );
    }
}

#[test]
fn a_write_back_sets_exactly_what_it_was_asked_to_and_keeps_the_rest() {
    // PP (9), PLS (5..8) and the speed field (10..13) are RWS or RO: they
    // have to survive, or powering a port would also reset its link state.
    let sc = (1 << 9) | (0x5 << 5) | (0x3 << 10) | 1 /* CCS */;
    let out = portsc_writeback(sc, 1 << 4);
    assert_eq!(out & (1 << 9), 1 << 9, "PP no sobrevive a la reescritura");
    assert_eq!(out & (0xf << 5), 0x5 << 5, "PLS no sobrevive");
    assert_eq!(out & (0xf << 10), 0x3 << 10, "la velocidad no sobrevive");
    assert_eq!(out & (1 << 4), 1 << 4, "PR no se pone");
    assert_eq!(
        out & !((1 << 9) | (0xf << 5) | (0xf << 10) | (1 << 4) | 1),
        0,
        "la reescritura pone bits que nadie pidio"
    );
}

/// The bug: a Port Config Error was cleared by every unrelated write.
#[test]
fn a_port_config_error_is_a_change_like_the_other_six() {
    assert_ne!(
        PORTSC_CHANGE_BITS & (1 << 23),
        0,
        "CEC no cuenta como cambio de puerto, asi que nada reexamina el puerto"
    );
    // A sample where CEC is the only thing set, written back to power the
    // port: CEC must still be there afterwards.
    let out = portsc_writeback(1 << 23, 1 << 9);
    assert_eq!(
        out & (1 << 23),
        0,
        "encender el puerto le escribe un 1 a CEC y borra el error"
    );
    // And acknowledged on purpose, it is.
    let acked = 1u32 << 23 & PORTSC_CHANGE_BITS;
    assert_eq!(
        portsc_writeback(1 << 23, acked) & (1 << 23),
        1 << 23,
        "un CEC reconocido a proposito no se escribe"
    );
}

#[test]
fn the_change_mask_is_the_seven_change_bits_and_nothing_else() {
    assert_eq!(
        PORTSC_CHANGE_BITS, 0x00fe_0000,
        "la mascara de cambios no son exactamente los bits 17..=23"
    );
}
