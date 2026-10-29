//! Qué registro se toca y con qué bits. El resto de un reset —— que el bloque
//! de detrás se enterase —— no se puede comprobar aquí: no hay Pi 5 y QEMU no
//! emula ninguno de los dos controladores.

use super::*;
use alloc::boxed::Box;

/// La ventana que declara el nodo `bcm_reset`: `0x30` bytes, dos bancos.
const SW_INIT_LEN: usize = 0x30;

fn sw_init() -> (BrcmstbReset, usize) {
    let banco: &'static mut [u32] =
        Box::leak(alloc::vec![0u32; SW_INIT_LEN / 4].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    (unsafe { BrcmstbReset::new(base, SW_INIT_LEN) }, base)
}

fn rescal() -> (PcieRescal, usize) {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; 0x10 / 4].into_boxed_slice());
    let base = banco.as_mut_ptr() as usize;
    (unsafe { PcieRescal::new(base) }, base)
}

fn reg(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}

fn poke(base: usize, off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + off) as *mut u32, val) }
}

// --- el reparto de líneas en bancos ----------------------------------------

/// Una línea es un bit de un banco: `id / 32` dice el banco e `id % 32` el
/// bit. Las dos del puente PCIe caen las dos en el banco 1.
#[test]
fn a_reset_line_is_a_bit_of_a_bank_and_both_of_the_bridges_are_in_the_second() {
    assert_eq!(sw_init_bank(RESET_SWINIT), 1, "la 32 abre el banco 1");
    assert_eq!(sw_init_bit(RESET_SWINIT), 1 << 0, "y es su bit 0");

    assert_eq!(sw_init_bank(RESET_BRIDGE), 1, "la 44 también");
    assert_eq!(sw_init_bit(RESET_BRIDGE), 1 << 12, "en el bit 12");

    assert_eq!(sw_init_bank(0), 0);
    assert_eq!(sw_init_bit(31), 1 << 31, "la última línea del banco 0");
    assert_eq!(
        sw_init_bit(32),
        1 << 0,
        "y la primera del 1 vuelve al bit 0"
    );
}

/// El paso entre bancos es `0x18` y no `0x0c`: el banco trae registros de más
/// que este driver no usa y que **cuentan como parte del banco**. Con un paso
/// de 0x0c, apretar una línea del banco 1 escribiría en el `STATUS` del banco
/// 0 —— un registro de sólo lectura en el chip, y en todo caso el equivocado.
#[test]
fn the_step_between_banks_counts_the_registers_the_driver_does_not_use() {
    assert_eq!(SW_INIT_BANK_SIZE, 0x18);

    let (rst, base) = sw_init();
    assert!(rst.assert_line(RESET_SWINIT));

    assert_eq!(
        reg(base, 0x18 + SW_INIT_SET),
        1 << 0,
        "la línea 32 va al SET del banco 1, en 0x18"
    );
    assert_eq!(
        reg(base, 0x0c),
        0,
        "y no a 0x0c, que es donde caería con un paso de tres registros"
    );
}

/// Y una línea de un banco que no está en la ventana se rechaza. La ventana
/// del BCM2712 son dos bancos; la línea 64 abriría un tercero.
#[test]
fn a_line_in_a_bank_outside_the_window_is_refused_rather_than_written() {
    let (rst, base) = sw_init();

    assert!(!rst.assert_line(64), "la línea 64 está en el banco 2");
    assert!(!rst.deassert_line(64));
    assert_eq!(rst.line_is_asserted(64), None);

    for off in (0..SW_INIT_LEN).step_by(4) {
        assert_eq!(reg(base, off), 0, "se escribió en {:#x}", off);
    }
}

// --- apretar y soltar -------------------------------------------------------

/// Apretar y soltar van a **registros distintos**. Soltar escribiendo un cero
/// en el `SET` no suelta nada: estos dos son registros de orden, y un cero no
/// dice nada.
#[test]
fn asserting_and_deasserting_are_two_different_registers_not_a_one_and_a_zero() {
    let (rst, base) = sw_init();

    assert!(rst.assert_line(RESET_BRIDGE));
    assert_eq!(reg(base, 0x18 + SW_INIT_SET), 1 << 12);
    assert_eq!(reg(base, 0x18 + SW_INIT_CLEAR), 0, "soltar aún no");

    assert!(rst.deassert_line(RESET_BRIDGE));
    assert_eq!(
        reg(base, 0x18 + SW_INIT_CLEAR),
        1 << 12,
        "soltar va al CLEAR"
    );
}

/// Se escribe **sólo** el bit de la línea. La máscara entera apretaría el
/// reset de treinta y una líneas más, y en este bloque las vecinas son otros
/// periféricos del SoC.
#[test]
fn only_the_bit_of_its_own_line_is_written_and_never_the_whole_mask() {
    let (rst, base) = sw_init();

    assert!(rst.assert_line(RESET_SWINIT));

    assert_eq!(
        reg(base, 0x18 + SW_INIT_SET),
        1 << 0,
        "un solo bit, no una máscara"
    );
}

/// Y no se lee antes para conservar lo que hubiera: en un registro de orden,
/// reescribir lo que se leyó volvería a dar la orden de las líneas que otro
/// apretó.
#[test]
fn a_command_register_is_not_read_back_before_being_written() {
    let (rst, base) = sw_init();
    poke(base, 0x18 + SW_INIT_SET, 0xffff_0000);

    assert!(rst.assert_line(RESET_SWINIT));

    assert_eq!(
        reg(base, 0x18 + SW_INIT_SET),
        1 << 0,
        "lo de antes no se arrastra: se da una orden, no se guarda un estado"
    );
}

/// El estado sí es un registro de estado, y se lee por el bit de la línea.
#[test]
fn the_status_register_answers_for_the_bit_of_the_line_it_was_asked_about() {
    let (rst, base) = sw_init();

    assert_eq!(rst.line_is_asserted(RESET_BRIDGE), Some(false));

    poke(base, 0x18 + SW_INIT_STATUS, 1 << 12);
    assert_eq!(rst.line_is_asserted(RESET_BRIDGE), Some(true));
    assert_eq!(
        rst.line_is_asserted(RESET_SWINIT),
        Some(false),
        "la 32 es otro bit del mismo registro"
    );
}

/// El margen tras soltar un reset es de 100 µs, con un peor caso medido de
/// ~14 µs. No es adorno: tocar los registros del bloque antes de eso lee
/// basura y no da ningún error.
#[test]
fn deasserting_leaves_the_slack_the_block_needs_to_come_back() {
    assert_eq!(DEASSERT_SETTLE_US, 100);
    const _: () = assert!(
        DEASSERT_SETTLE_US >= 14,
        "por debajo del peor caso del bloque"
    );

    let (rst, _) = sw_init();
    let (ok, esperado) = reset_fake::esperando(|| rst.deassert_line(RESET_BRIDGE));

    assert!(ok);
    assert_eq!(
        esperado, DEASSERT_SETTLE_US,
        "soltar el reset no esperó a que el bloque volviera"
    );
}

/// Y apretarlo no espera: el margen es para volver de un reset, no para
/// entrar en uno. Esperar aquí sería un retardo por nada en el camino de
/// arranque.
#[test]
fn asserting_a_reset_does_not_wait_because_the_slack_is_for_coming_back() {
    let (rst, _) = sw_init();

    let (ok, esperado) = reset_fake::esperando(|| rst.assert_line(RESET_BRIDGE));

    assert!(ok);
    assert_eq!(esperado, 0);
}

// --- el recalibrado del PHY -------------------------------------------------

/// El camino bueno: se pone el bit de arranque, se espera a que el estado diga
/// que acabó, y el bit **se vuelve a limpiar**. Es un pulso, no un pestillo:
/// dejarlo puesto deja el bloque pidiendo otro recalibrado.
#[test]
fn the_rescal_start_bit_is_a_pulse_and_is_taken_back_down_at_the_end() {
    let (res, base) = rescal();
    poke(base, RESCAL_STATUS, RESCAL_STATUS_BIT);

    assert_eq!(res.start(), Ok(()));

    assert_eq!(
        reg(base, RESCAL_START) & RESCAL_START_BIT,
        0,
        "el bit de arranque se quedó puesto"
    );
}

/// El bit de arranque se escribe **sin tocar** el resto del registro, en los
/// dos sentidos: ni al ponerlo ni al retirarlo.
#[test]
fn starting_the_rescal_leaves_the_rest_of_its_register_alone() {
    let (res, base) = rescal();
    poke(base, RESCAL_START, 0xa5a5_a5a0);
    poke(base, RESCAL_STATUS, RESCAL_STATUS_BIT);

    assert_eq!(res.start(), Ok(()));

    assert_eq!(reg(base, RESCAL_START), 0xa5a5_a5a0);
}

/// Si el bit de arranque no se queda puesto, el bloque está diciendo que no
/// arranca, y seguir adelante es esperar un recalibrado que no va a pasar.
/// Esto no es comprobar que la escritura llegó: es un acuse.
#[test]
fn a_start_bit_that_does_not_stick_is_the_block_refusing_and_not_a_lost_write() {
    let (res, base) = rescal();
    poke(base, RESCAL_STATUS, RESCAL_STATUS_BIT);
    reset_fake::RECHAZA_ARRANQUE.with(|c| c.set(true));

    let r = res.start();

    reset_fake::RECHAZA_ARRANQUE.with(|c| c.set(false));
    assert_eq!(r, Err(RescalError::NoArranca));
    assert_eq!(
        reg(base, RESCAL_START),
        0,
        "y no se queda nada escrito del intento"
    );
}

/// Si el estado no llega a decir que acabó, se agota el plazo —— y el bit de
/// arranque se retira **igualmente**. Dejarlo puesto tras un plazo agotado es
/// peor que soltarlo: el bloque se queda pidiendo.
#[test]
fn a_rescal_that_never_finishes_times_out_and_still_takes_the_pulse_back_down() {
    let (res, base) = rescal();
    // El estado se queda a cero: el recalibrado no acaba nunca.

    let (r, esperado) = reset_fake::esperando(|| res.start());

    assert_eq!(r, Err(RescalError::SeAgotoElPlazo));
    assert_eq!(
        reg(base, RESCAL_START) & RESCAL_START_BIT,
        0,
        "el pulso se quedó puesto tras agotarse el plazo"
    );
    assert_eq!(
        esperado,
        u64::from(RESCAL_POLL_TRIES) * RESCAL_POLL_STEP_US,
        "el plazo entero se agotó mirando, ni más vueltas ni menos"
    );
}

/// El plazo es el de Linux: diez vueltas de 100 µs, o sea el milisegundo de
/// `readl_poll_timeout(..., 100, 1000)`.
#[test]
fn the_rescal_timeout_is_the_millisecond_linux_gives_it() {
    assert_eq!(RESCAL_POLL_TRIES, 10);
    assert_eq!(RESCAL_POLL_STEP_US, 100);
    assert_eq!(
        u64::from(RESCAL_POLL_TRIES) * RESCAL_POLL_STEP_US,
        1000,
        "diez vueltas de cien microsegundos"
    );
}

/// El `assert` de este controlador no hace nada, y es a propósito: todo el
/// trabajo está en el arranque. Esto queda escrito para que la ausencia no
/// parezca un hueco por rellenar.
#[test]
fn the_rescal_assert_does_nothing_on_purpose() {
    let (res, base) = rescal();

    res.assert_noop();

    for off in (0..0x10).step_by(4) {
        assert_eq!(reg(base, off), 0, "tocó {:#x}", off);
    }
}

/// Los offsets de los dos controladores caben en las ventanas que sus nodos
/// declaran: `0x30` el de SW_INIT y `0x10` el del recalibrado.
#[test]
fn every_register_of_both_controllers_fits_inside_its_declared_window() {
    for off in [SW_INIT_SET, SW_INIT_CLEAR, SW_INIT_STATUS] {
        assert!(off + 4 <= SW_INIT_BANK_SIZE, "{:#x} sale del banco", off);
    }
    const _: () = assert!(
        SW_INIT_BANK_SIZE * 2 <= SW_INIT_LEN,
        "los dos bancos del BCM2712 no caben en su reg"
    );

    for off in [RESCAL_START, RESCAL_CTRL, RESCAL_STATUS] {
        assert!(off + 4 <= 0x10, "{:#x} sale de la ventana", off);
    }
}
