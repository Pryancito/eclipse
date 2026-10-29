//! El mapa de registros, clavado contra lo que publican Linux y U-Boot.
//!
//! Estos tests **no prueban el silicio**: aqui no hay Raspberry Pi 5 y QEMU no
//! emula el GEM del RP1, asi que lo unico que se puede comprobar es que el
//! mapa no se copio mal, que es el error realista. Cada valor lleva apuntada
//! la constante de la que sale, de
//! `drivers/net/ethernet/cadence/macb.h` de Linux (v6.12) o de
//! `drivers/net/macb.c` de U-Boot.
//!
//! Lo que una placa podria desmentir es el mapa entero; lo que esto desmiente
//! es un digito transpuesto, que es lo que de verdad pasa.

use super::*;

/// Los offsets de los registros, uno a uno, con el nombre de Linux.
#[test]
fn every_register_offset_is_the_one_linux_publishes() {
    // macb.h, "Register offsets" (comunes a MACB y GEM).
    assert_eq!(NCR, 0x000, "MACB_NCR");
    assert_eq!(NCFGR, 0x004, "MACB_NCFGR");
    assert_eq!(NSR, 0x008, "MACB_NSR");
    assert_eq!(TSR, 0x014, "MACB_TSR");
    assert_eq!(RBQP, 0x018, "MACB_RBQP");
    assert_eq!(TBQP, 0x01c, "MACB_TBQP");
    assert_eq!(RSR, 0x020, "MACB_RSR");
    assert_eq!(ISR, 0x024, "MACB_ISR");
    assert_eq!(IER, 0x028, "MACB_IER");
    assert_eq!(IDR, 0x02c, "MACB_IDR");
    assert_eq!(IMR, 0x030, "MACB_IMR");
    assert_eq!(MAN, 0x034, "MACB_MAN");
    assert_eq!(MID, 0x0fc, "MACB_MID");
    assert_eq!(TBQPH, 0x4c8, "MACB_TBQPH");
    assert_eq!(RBQPH, 0x4d4, "MACB_RBQPH");

    // macb.h, "GEM register offsets".
    assert_eq!(GEM_USRIO, 0x00c, "GEM_USRIO");
    assert_eq!(GEM_DMACFG, 0x010, "GEM_DMACFG");
    assert_eq!(GEM_PBUFRXCUT, 0x044, "GEM_PBUFRXCUT");
    assert_eq!(GEM_JML, 0x048, "GEM_JML");
    assert_eq!(GEM_HRB, 0x080, "GEM_HRB");
    assert_eq!(GEM_HRT, 0x084, "GEM_HRT");
    assert_eq!(GEM_SA1B, 0x088, "GEM_SA1B");
    assert_eq!(GEM_SA1T, 0x08c, "GEM_SA1T");
    assert_eq!(GEM_DCFG1, 0x280, "GEM_DCFG1");
    assert_eq!(GEM_DCFG2, 0x284, "GEM_DCFG2");
    assert_eq!(GEM_DCFG6, 0x294, "GEM_DCFG6");

    // Y el del MACB viejo, que es el que hace falta para que la negativa de
    // `check_supported` tenga sentido.
    assert_eq!(MACB_SA1B, 0x098, "MACB_SA1B");
}

/// La razon de ser de este driver: el GEM **movio** la MAC y el hash, asi que
/// un MACB que no es GEM no se puede manejar con estos offsets.
///
/// Si estos dos coincidieran, la negativa de [`MacbMac::check_supported`]
/// sobraria, y nadie se enteraria de que sobra.
#[test]
fn the_gem_and_the_old_macb_do_not_keep_the_mac_in_the_same_place() {
    assert_ne!(GEM_SA1B, MACB_SA1B);
    // Y el tercer juego del GEM cae justo donde el MACB tiene el primero, que
    // es lo que convierte el error en una tarjeta que acepta tramas ajenas en
    // vez de en un fallo visible.
    assert_eq!(GEM_SA1B + 2 * SA_STRIDE, MACB_SA1B);
}

/// Los cuatro juegos de direccion especifica, con su paso.
#[test]
fn the_four_specific_address_sets_are_two_words_apart() {
    assert_eq!(SA_SETS, 4);
    assert_eq!(SA_STRIDE, 8);
    // macb.h: GEM_SA2B 0x0090, GEM_SA3B 0x0098, GEM_SA4B 0x00A0.
    assert_eq!(GEM_SA1B + SA_STRIDE, 0x090);
    assert_eq!(GEM_SA1B + 2 * SA_STRIDE, 0x098);
    assert_eq!(GEM_SA1B + 3 * SA_STRIDE, 0x0a0);
    // Y la parte alta de cada juego, una palabra despues de la baja.
    assert_eq!(GEM_SA1T, GEM_SA1B + 4);
    assert_eq!(GEM_SA1T + 3 * SA_STRIDE, 0x0a4, "GEM_SA4T");
    // El ultimo juego cabe en la ventana que declara el device tree.
    const { assert!(GEM_SA1T + 3 * SA_STRIDE < MACB_WINDOW_BYTES) };
}

/// Los bits de `NCR`, que son los `MACB_*_OFFSET` de macb.h.
#[test]
fn the_control_register_bits_are_where_linux_says() {
    assert_eq!(NCR_LLB, 1 << 1, "MACB_LLB_OFFSET 1");
    assert_eq!(NCR_RE, 1 << 2, "MACB_RE_OFFSET 2");
    assert_eq!(NCR_TE, 1 << 3, "MACB_TE_OFFSET 3");
    assert_eq!(NCR_MPE, 1 << 4, "MACB_MPE_OFFSET 4");
    assert_eq!(NCR_CLRSTAT, 1 << 5, "MACB_CLRSTAT_OFFSET 5");
    assert_eq!(NCR_INCSTAT, 1 << 6, "MACB_INCSTAT_OFFSET 6");
    assert_eq!(NCR_WESTAT, 1 << 7, "MACB_WESTAT_OFFSET 7");
    assert_eq!(NCR_TSTART, 1 << 9, "MACB_TSTART_OFFSET 9");
    assert_eq!(NCR_THALT, 1 << 10, "MACB_THALT_OFFSET 10");

    // Cada uno un bit y ninguno repetido: un bit de mas en una mascara de
    // `modify` apaga algo que nadie nombro.
    let todos = [
        NCR_LLB,
        NCR_RE,
        NCR_TE,
        NCR_MPE,
        NCR_CLRSTAT,
        NCR_INCSTAT,
        NCR_WESTAT,
        NCR_TSTART,
        NCR_THALT,
    ];
    let mut vistos = 0u32;
    for bit in todos {
        assert_eq!(bit.count_ones(), 1);
        assert_eq!(vistos & bit, 0);
        vistos |= bit;
    }
}

/// Los bits y los campos de `NCFGR`.
#[test]
fn the_config_register_fields_are_where_linux_says() {
    assert_eq!(NCFGR_SPD, 1 << 0, "MACB_SPD_OFFSET 0");
    assert_eq!(NCFGR_FD, 1 << 1, "MACB_FD_OFFSET 1");
    assert_eq!(NCFGR_JFRAME, 1 << 3, "MACB_JFRAME_OFFSET 3");
    assert_eq!(NCFGR_CAF, 1 << 4, "MACB_CAF_OFFSET 4");
    assert_eq!(NCFGR_NBC, 1 << 5, "MACB_NBC_OFFSET 5");
    assert_eq!(NCFGR_MTI, 1 << 6, "MACB_NCFGR_MTI_OFFSET 6");
    assert_eq!(NCFGR_UNI, 1 << 7, "MACB_UNI_OFFSET 7");
    assert_eq!(NCFGR_BIG, 1 << 8, "MACB_BIG_OFFSET 8");
    assert_eq!(NCFGR_GBE, 1 << 10, "GEM_GBE_OFFSET 10");
    assert_eq!(NCFGR_PCSSEL, 1 << 11, "GEM_PCSSEL_OFFSET 11");
    assert_eq!(NCFGR_PAE, 1 << 13, "GEM_PAE_OFFSET 13");
    assert_eq!(NCFGR_DRFCS, 1 << 17, "MACB_DRFCS_OFFSET 17");
    assert_eq!(NCFGR_RXCOEN, 1 << 24, "GEM_RXCOEN_OFFSET 24");
    assert_eq!(NCFGR_SGMIIEN, 1 << 27, "GEM_SGMIIEN_OFFSET 27");

    // Los campos: MACB_RBOF_OFFSET 14 tamaño 2, GEM_CLK_OFFSET 18 tamaño 3,
    // GEM_DBW_OFFSET 21 tamaño 2.
    assert_eq!((NCFGR_RBOF_SHIFT, NCFGR_RBOF_MASK), (14, 0x3));
    assert_eq!((NCFGR_CLK_SHIFT, NCFGR_CLK_MASK), (18, 0x7));
    assert_eq!((NCFGR_DBW_SHIFT, NCFGR_DBW_MASK), (21, 0x3));

    // Y ninguno pisa a otro. El que de verdad importa es `GBE` contra `CLK`:
    // en el MACB viejo el divisor del MDC esta en el bit 10 (MACB_CLK_OFFSET),
    // o sea justo donde el GEM pone el gigabit.
    let campos = [
        (NCFGR_RBOF_SHIFT, NCFGR_RBOF_MASK),
        (NCFGR_CLK_SHIFT, NCFGR_CLK_MASK),
        (NCFGR_DBW_SHIFT, NCFGR_DBW_MASK),
    ];
    let mut vistos = NCFGR_SPD
        | NCFGR_FD
        | NCFGR_JFRAME
        | NCFGR_CAF
        | NCFGR_NBC
        | NCFGR_MTI
        | NCFGR_UNI
        | NCFGR_BIG
        | NCFGR_GBE
        | NCFGR_PCSSEL
        | NCFGR_PAE
        | NCFGR_DRFCS
        | NCFGR_RXCOEN
        | NCFGR_SGMIIEN;
    for (shift, mask) in campos {
        let campo = mask << shift;
        assert_eq!(vistos & campo, 0, "el campo en {shift} pisa un bit ya dado");
        vistos |= campo;
    }
}

/// Los campos de `MAN`, el bus MDIO, y los valores fijos de la clausula 22.
#[test]
fn the_phy_maintenance_fields_are_where_linux_says() {
    // macb.h: DATA 0/16, CODE 16/2, REGA 18/5, PHYA 23/5, RW 28/2, SOF 30/2.
    assert_eq!((MAN_DATA_SHIFT, MAN_DATA_MASK), (0, 0xFFFF));
    assert_eq!((MAN_CODE_SHIFT, MAN_CODE_MASK), (16, 0x3));
    assert_eq!((MAN_REGA_SHIFT, MAN_REGA_MASK), (18, 0x1F));
    assert_eq!((MAN_PHYA_SHIFT, MAN_PHYA_MASK), (23, 0x1F));
    assert_eq!((MAN_RW_SHIFT, MAN_RW_MASK), (28, 0x3));
    assert_eq!((MAN_SOF_SHIFT, MAN_SOF_MASK), (30, 0x3));

    // Los campos cubren la palabra entera y no se pisan: `MAN` no tiene ni un
    // bit libre, asi que cualquier desplazamiento de mas se come un vecino.
    let mut vistos = 0u32;
    for (shift, mask) in [
        (MAN_DATA_SHIFT, MAN_DATA_MASK),
        (MAN_CODE_SHIFT, MAN_CODE_MASK),
        (MAN_REGA_SHIFT, MAN_REGA_MASK),
        (MAN_PHYA_SHIFT, MAN_PHYA_MASK),
        (MAN_RW_SHIFT, MAN_RW_MASK),
        (MAN_SOF_SHIFT, MAN_SOF_MASK),
    ] {
        let campo = mask << shift;
        assert_eq!(vistos & campo, 0);
        vistos |= campo;
    }
    assert_eq!(vistos, u32::MAX);

    // MACB_MAN_C22_SOF 1, _CODE 2, _WRITE 1, _READ 2.
    assert_eq!(MAN_C22_SOF, 1);
    assert_eq!(MAN_C22_CODE, 2);
    assert_eq!(MAN_C22_WRITE, 1);
    assert_eq!(MAN_C22_READ, 2);
    // Leer y escribir son valores distintos del mismo campo: si fueran el
    // mismo, toda lectura escribiria el PHY.
    assert_ne!(MAN_C22_READ, MAN_C22_WRITE);
}

/// `NSR`, `MID` y los `DCFG`.
#[test]
fn the_status_and_design_fields_are_where_linux_says() {
    assert_eq!(NSR_IDLE, 1 << 2, "MACB_IDLE_OFFSET 2");
    assert_eq!((MID_IDNUM_SHIFT, MID_IDNUM_MASK), (16, 0xFFF));
    assert_eq!((MID_REV_SHIFT, MID_REV_MASK), (0, 0xFFFF));
    assert_eq!(IDNUM_GEM_MIN, 2, "hw_is_gem: IDNUM >= 0x2");
    assert_eq!((DCFG1_DBWDEF_SHIFT, DCFG1_DBWDEF_MASK), (25, 0x7));
    assert_eq!(DCFG1_IRQCOR, 1 << 23, "GEM_IRQCOR_OFFSET 23");
    assert_eq!(DCFG6_DAW64, 1 << 23, "GEM_DAW64_OFFSET 23");
    // El ancho del bus, codificado para el campo `DBW`.
    assert_eq!((GEM_DBW32, GEM_DBW64, GEM_DBW128), (0, 1, 2));
    // El divisor del MDC, codificado para el campo `CLK`. Son ocho valores de
    // tres bits, en orden creciente de divisor.
    let divisores = [
        GEM_CLK_DIV8,
        GEM_CLK_DIV16,
        GEM_CLK_DIV32,
        GEM_CLK_DIV48,
        GEM_CLK_DIV64,
        GEM_CLK_DIV96,
        GEM_CLK_DIV128,
        GEM_CLK_DIV224,
    ];
    for (esperado, valor) in divisores.iter().enumerate() {
        assert_eq!(*valor, esperado as u32);
        assert!(*valor <= NCFGR_CLK_MASK);
    }
}

/// La ventana, que es la que declara el `reg` del nodo del RP1.
#[test]
fn the_window_is_the_one_the_rp1_node_declares() {
    // rp1.dtsi: reg = <0xc0 0x40100000  0x0 0x4000>.
    assert_eq!(MACB_WINDOW_BYTES, 0x4000);
    // Y todo lo que este driver toca cae dentro, incluidos los dos registros
    // mas altos: los `DCFG` en 0x280 y la mitad alta de las bases de las
    // colas en 0x4c8 y 0x4d4. O sea que la ventana que declara el arbol basta
    // y la tanda de los anillos no va a tener que pedir mas.
    for off in [GEM_DCFG6, TBQPH, RBQPH] {
        assert!(
            off + 4 <= MACB_WINDOW_BYTES,
            "{:#x} se sale de la ventana",
            off
        );
    }
}

/// Los dos bytes de relleno de recepcion caben en el campo que los lleva.
#[test]
fn the_receive_offset_fits_in_its_field() {
    assert_eq!(RX_IP_ALIGN, 2, "NET_IP_ALIGN");
    const { assert!(RX_IP_ALIGN <= NCFGR_RBOF_MASK) };
}
