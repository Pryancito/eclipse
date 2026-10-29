//! El protocolo del bus MDIO del puente: qué se escribe, en qué orden y qué se
//! espera de vuelta. Que el PHY de una Pi 5 conteste eso no se puede comprobar
//! aquí; lo que se comprueba es que se le habla como su driver de Linux le
//! habla.

use super::mdio_fake::{self, LEIDO};
use super::*;
use alloc::boxed::Box;

fn bus() -> MdioBus {
    let banco: &'static mut [u32] = Box::leak(alloc::vec![0u32; 0x1200 / 4].into_boxed_slice());
    unsafe { MdioBus::new(banco.as_mut_ptr() as usize) }
}

// --- el paquete -------------------------------------------------------------

/// Cada campo en su sitio: el puerto en los bits 16-19, el registro en los 16
/// de abajo y la orden en los 12 de arriba.
#[test]
fn each_field_of_an_mdio_packet_goes_where_the_phy_expects_it() {
    assert_eq!(form_pkt(0, 0, CMD_READ), 0x0010_0000, "la orden de leer");
    assert_eq!(form_pkt(0, 0, CMD_WRITE), 0, "la de escribir es un cero");
    assert_eq!(form_pkt(0, 0x1f, CMD_WRITE), 0x1f, "el registro, abajo");
    assert_eq!(
        form_pkt(3, 0, CMD_WRITE),
        0x0003_0000,
        "el puerto, a 16 bits"
    );
    assert_eq!(
        form_pkt(3, 0x1f, CMD_READ),
        0x0013_001f,
        "los tres a la vez"
    );
}

/// Un puerto que no cabe en sus cuatro bits se recorta en vez de desbordar
/// sobre la orden, que es el campo de al lado.
#[test]
fn a_port_that_does_not_fit_is_clipped_instead_of_spilling_into_the_command() {
    assert_eq!(
        form_pkt(0xff, 0, CMD_WRITE),
        0x000f_0000,
        "el puerto se queda en sus cuatro bits"
    );
}

/// La orden de leer es un **uno** y la de escribir un **cero**: un driver que
/// las intercambiase pediría una lectura al escribir y nunca vería el dato.
#[test]
fn reading_is_the_one_and_writing_is_the_zero() {
    assert_eq!(CMD_READ, 1);
    assert_eq!(CMD_WRITE, 0);
    assert_ne!(CMD_READ, CMD_WRITE);
}

// --- el acuse ---------------------------------------------------------------

/// **El acuse de una escritura es el mismo bit que el de una lectura y con la
/// polaridad al revés.** La lectura acaba cuando el bit 31 está puesto; la
/// escritura, cuando está limpio. El dato se escribe con ese bit a uno y es el
/// silicio quien lo baja.
#[test]
fn the_write_acknowledgement_is_the_same_bit_as_the_read_one_but_inverted() {
    let b = bus();

    let (r, phy) = mdio_fake::con_un_phy(|| b.write(PORT0, 0x16, 0x50b9));

    assert_eq!(r, Ok(()));
    let escrito = phy
        .traza
        .iter()
        .find(|(off, _)| *off == RC_DL_MDIO_WR_DATA)
        .expect("se escribió el dato");
    assert_eq!(
        escrito.1 & DATA_DONE,
        DATA_DONE,
        "el dato se manda CON el bit del acuse puesto"
    );
}

/// El valor de una lectura viene **sin** el bit del acuse: los datos son los 31
/// de abajo. Devolverlo entero daría un valor con el bit alto puesto en todas
/// las lecturas buenas.
#[test]
fn the_value_of_a_read_comes_back_without_the_acknowledgement_bit() {
    let b = bus();

    let (r, _) = mdio_fake::con_un_phy(|| {
        b.write(PORT0, SET_ADDR_OFFSET, 0x1600)?;
        b.write(PORT0, 0x17, 0xbda1)?;
        b.read(PORT0, 0x17)
    });

    assert_eq!(r, Ok(0xbda1), "ni 0x8000_bda1");
}

/// Un PHY que no acusa se da por perdido tras el plazo, en los dos sentidos, y
/// el plazo es el de Linux: diez vueltas de 10 µs.
#[test]
fn a_phy_that_never_acknowledges_times_out_in_both_directions() {
    let b = bus();

    let (r, _) = mdio_fake::mudo(|| b.read(PORT0, 0x16));
    assert_eq!(r, Err(MdioError::SinAcuse), "la lectura");

    let (r, _) = mdio_fake::mudo(|| b.write(PORT0, 0x16, 1));
    assert_eq!(r, Err(MdioError::SinAcuse), "la escritura");

    assert_eq!(POLL_TRIES, 10);
    assert_eq!(POLL_STEP_US, 10);
    assert_eq!(
        u64::from(POLL_TRIES) * POLL_STEP_US,
        100,
        "los 100 µs de readl_poll_timeout_atomic(..., 10, 100)"
    );
}

/// Tras dejar el paquete en el registro de dirección se **relee ese mismo
/// registro**, y no para mirar nada: es el vaciado de una escritura diferida.
/// Sin esa lectura el sondeo del acuse puede adelantarse a la escritura que lo
/// provoca.
#[test]
fn the_address_register_is_read_back_to_flush_the_posted_write() {
    let b = bus();

    let (_, phy) = mdio_fake::con_un_phy(|| b.write(PORT0, 0x16, 0x50b9));

    let i_escritura = phy
        .traza
        .iter()
        .position(|(off, _)| *off == RC_DL_MDIO_ADDR)
        .expect("se escribió la dirección");
    let i_relectura = phy
        .traza
        .iter()
        .position(|(off, _)| *off == RC_DL_MDIO_ADDR + LEIDO)
        .expect("no se releyó el registro de dirección");
    let i_dato = phy
        .traza
        .iter()
        .position(|(off, _)| *off == RC_DL_MDIO_WR_DATA)
        .expect("se escribió el dato");

    assert!(
        i_escritura < i_relectura && i_relectura < i_dato,
        "el vaciado va entre la dirección y el dato, traza {:x?}",
        phy.traza
    );
}

// --- el direccionamiento de dos pasos ---------------------------------------

/// El direccionamiento es de dos pasos: primero el **bloque** por
/// `SET_ADDR_OFFSET`, y luego los registros de dentro por su número. El mismo
/// número de registro en dos bloques son dos registros distintos.
#[test]
fn the_same_register_number_in_two_blocks_is_two_different_registers() {
    let b = bus();

    let (r, phy) = mdio_fake::con_un_phy(|| {
        b.write(PORT0, SET_ADDR_OFFSET, 0x1600)?;
        b.write(PORT0, 0x16, 0xaaaa)?;
        b.write(PORT0, SET_ADDR_OFFSET, SSC_REGS_ADDR)?;
        b.write(PORT0, 0x16, 0x5555)?;
        Ok::<(), MdioError>(())
    });

    assert_eq!(r, Ok(()));
    assert_eq!(phy.regs.get(&(0x1600, 0x16)), Some(&0xaaaa));
    assert_eq!(phy.regs.get(&(SSC_REGS_ADDR, 0x16)), Some(&0x5555));
}

/// `SSC_REGS_ADDR` vale `0x1100`, **el mismo número** que el offset del
/// registro de dirección del MDIO en la ventana del puente. Son dos cosas sin
/// relación: un offset de MMIO y una dirección de bloque dentro del PHY.
#[test]
fn the_ssc_block_address_happens_to_be_the_mmio_offset_of_the_address_register() {
    assert_eq!(SSC_REGS_ADDR, 0x1100);
    assert_eq!(RC_DL_MDIO_ADDR, 0x1100);
    assert_eq!(
        usize::from(SSC_REGS_ADDR),
        RC_DL_MDIO_ADDR,
        "y aun así no tienen nada que ver"
    );
}

// --- el ensanchado ----------------------------------------------------------

/// El ensanchado se pide poniendo los dos bits del control sobre lo que ya
/// hubiera, no escribiendo sólo esos dos.
#[test]
fn asking_for_spread_spectrum_adds_its_two_bits_to_what_was_already_there() {
    let b = bus();

    let (r, phy) = mdio_fake::con_un_phy(|| {
        // Lo que el PHY traía de antes en el control, y un estado bueno.
        prepara_phy(0x0021, SSC_STATUS_SSC | SSC_STATUS_PLL_LOCK);
        b.set_ssc()
    });

    assert_eq!(r, Ok(()));
    assert_eq!(
        phy.regs.get(&(SSC_REGS_ADDR, SSC_CNTL_OFFSET)),
        Some(&((SSC_CNTL_OVRD_EN | SSC_CNTL_OVRD_VAL) as u16 | 0x0021)),
        "los bits de antes se perdieron"
    );
}

/// Hacen falta **los dos** bits del estado: el ensanchado puesto y el PLL
/// enganchado. Con uno solo el reloj no sirve.
#[test]
fn spread_spectrum_needs_both_the_spread_and_the_pll_lock() {
    let b = bus();

    for (estado, nombre) in [
        (0, "ninguno de los dos"),
        (SSC_STATUS_SSC, "sólo el ensanchado"),
        (SSC_STATUS_PLL_LOCK, "sólo el enganche"),
    ] {
        let (r, _) = mdio_fake::con_un_phy(|| {
            prepara_phy(0, estado);
            b.set_ssc()
        });
        assert_eq!(r, Err(MdioError::SinEnsanchado), "{}", nombre);
    }

    let (r, _) = mdio_fake::con_un_phy(|| {
        prepara_phy(0, SSC_STATUS_SSC | SSC_STATUS_PLL_LOCK);
        b.set_ssc()
    });
    assert_eq!(r, Ok(()), "los dos");
}

/// Y entre pedirlo y preguntar si cuajó se espera: el PHY no engancha en el
/// mismo ciclo.
#[test]
fn there_is_a_wait_between_asking_for_the_spread_and_asking_whether_it_took() {
    let b = bus();

    let (_, esperado) = super::super::reset::reset_fake::esperando(|| {
        mdio_fake::con_un_phy(|| {
            prepara_phy(0, SSC_STATUS_SSC | SSC_STATUS_PLL_LOCK);
            b.set_ssc()
        })
    });

    assert!(
        esperado >= SSC_SETTLE_US,
        "se preguntó por el enganche sin esperar (esperó {} µs)",
        esperado
    );
}

// --- el PLL -----------------------------------------------------------------

/// Los siete registros del PLL, con los valores del **código** de Linux y no
/// los de su comentario: el comentario sólo nombra cuatro, y para el `0x17` da
/// `0xbd1a` donde el código escribe `0xbda1`.
#[test]
fn the_pll_registers_are_the_ones_the_code_writes_and_not_the_ones_its_comment_names() {
    let b = bus();

    let (fallos, phy) = mdio_fake::con_un_phy(|| b.munge_pll());

    assert_eq!(fallos, 0);
    assert_eq!(phy.bloque, 0x1600, "el bloque del PLL");
    let esperados: [(u8, u16); 7] = [
        (0x16, 0x50b9),
        (0x17, 0xbda1),
        (0x18, 0x0094),
        (0x19, 0x97b4),
        (0x1b, 0x5030),
        (0x1c, 0x5030),
        (0x1e, 0x0007),
    ];
    for (reg, val) in esperados {
        assert_eq!(
            phy.regs.get(&(0x1600, reg)),
            Some(&val),
            "el registro {:#x} del PLL",
            reg
        );
    }
    assert_eq!(
        phy.regs.get(&(0x1600, 0x17)),
        Some(&0xbda1),
        "0xbd1a es lo que dice el comentario de Linux, no lo que escribe"
    );
    assert_eq!(phy.regs.len(), 7, "no se tocó ningún registro de más");
}

/// Un fallo de MDIO a mitad del PLL **no aborta**: parar ahí dejaría el PLL
/// medio reprogramado, que es peor que no haberlo tocado. Lo que se hace es
/// contarlo.
#[test]
fn a_failed_access_halfway_through_the_pll_is_counted_and_not_a_reason_to_stop() {
    let b = bus();

    let (fallos, _) = mdio_fake::mudo(|| b.munge_pll());

    assert_eq!(
        fallos, 8,
        "los ocho accesos (el bloque y los siete registros) se intentaron todos"
    );
}

/// Tras reprogramar el PLL se espera: el enlace no se intenta en el mismo
/// ciclo.
#[test]
fn the_pll_is_given_time_to_settle_before_anything_else_happens() {
    let b = bus();

    let (_, esperado) =
        super::super::reset::reset_fake::esperando(|| mdio_fake::con_un_phy(|| b.munge_pll()));

    assert!(
        esperado >= PLL_SETTLE_US,
        "no se esperó tras reprogramar el PLL (esperó {} µs)",
        esperado
    );
}

/// Prepara el PHY con un control y un estado de partida, dentro del bloque de
/// SSC.
fn prepara_phy(cntl: u16, estado: u32) {
    mdio_fake::PHY.with(|p| {
        let mut p = p.borrow_mut();
        p.regs.insert((SSC_REGS_ADDR, SSC_CNTL_OFFSET), cntl);
        p.regs
            .insert((SSC_REGS_ADDR, SSC_STATUS_OFFSET), estado as u16);
    });
}
