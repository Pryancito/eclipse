//! La identificacion, el reset, la configuracion de red, la MAC y el enlace,
//! contra el GEM falso de [`macb_fake`].

use super::macb_fake::*;
use super::*;

// ---------------------------------------------------------------------------
// Identificacion
// ---------------------------------------------------------------------------

#[test]
fn a_gem_is_identified_by_the_module_id_and_accepted() {
    let hw = Hw::new();
    let mac = hw.mac();
    assert_eq!(mac.hw_id(), (GEM_IDNUM, 0x0107));
    assert!(mac.is_gem());
    assert!(mac.check_supported().is_ok());
}

/// El MACB viejo de Atmel se rechaza, porque tiene la MAC y el hash en otros
/// offsets: manejarlo con estos seria escribir registros al azar.
#[test]
fn an_old_macb_is_refused_rather_than_driven_with_the_wrong_offsets() {
    let hw = Hw::new();
    hw.poke(MID, 1 << MID_IDNUM_SHIFT);
    let mac = hw.mac();
    assert!(!mac.is_gem());
    assert_eq!(mac.check_supported().err(), Some(DeviceError::NotSupported));
}

/// Y el `IDNUM` es un umbral, no un valor: un GEM mas nuevo dice mas de 2 y
/// tiene que entrar igual.
#[test]
fn a_newer_gem_is_accepted_too_because_the_id_is_a_floor() {
    let hw = Hw::new();
    hw.poke(MID, 7 << MID_IDNUM_SHIFT);
    assert!(hw.mac().is_gem());
    assert!(hw.mac().check_supported().is_ok());
}

/// Una ventana que no contesta se rechaza **antes** de leer el `MID`.
///
/// Es lo unico que distingue un MAC de una direccion mal mapeada: un `MID`
/// leido de la nada puede dar cualquier `IDNUM`, incluido uno que valga.
#[test]
fn a_window_with_nothing_behind_it_is_refused_before_its_id_is_believed() {
    let hw = Hw::new();
    // Un `IDNUM` que pasaria por bueno si solo se mirara el `MID`.
    hw.poke(MID, GEM_IDNUM << MID_IDNUM_SHIFT);
    hw.faults(|f| f.dead_window = true);
    let mac = hw.mac();
    assert!(!mac.responds());
    assert_eq!(mac.check_supported().err(), Some(DeviceError::NotReady));
}

/// La prueba de la ventana deja `NCR` como estaba, o sea a cero: es de sondeo
/// y no se puede usar con el MAC en marcha.
#[test]
fn probing_the_window_leaves_the_control_register_at_zero() {
    let hw = Hw::new();
    let mac = hw.mac();
    assert!(mac.responds());
    assert_eq!(hw.peek(NCR), 0);
}

#[test]
fn what_the_design_registers_declare_is_read_not_assumed() {
    let hw = Hw::new();
    let mac = hw.mac();

    // `DCFG6` sin el bit: DMA de 32 bits.
    assert!(!mac.has_64bit_dma());
    hw.poke(GEM_DCFG6, DCFG6_DAW64);
    assert!(mac.has_64bit_dma());

    // `IRQCOR` a cero (lo que el falso trae) es el `ISR` que se limpia
    // escribiendolo; con el bit puesto, no.
    assert!(mac.isr_clear_on_write());
    hw.poke(GEM_DCFG1, DCFG1_IRQCOR);
    assert!(!mac.isr_clear_on_write());
}

// ---------------------------------------------------------------------------
// Reset
// ---------------------------------------------------------------------------

#[test]
fn the_reset_stops_the_mac_and_clears_everything_it_can_report() {
    let hw = Hw::new();
    let mac = hw.mac();
    hw.poke(NCR, NCR_RE | NCR_TE | NCR_MPE);
    hw.poke(TSR, 0x1234);
    hw.poke(RSR, 0x5678);
    hw.poke(IMR, 0xFFFF);
    hw.raise_irq(0xABC);
    hw.poke(GEM_PBUFRXCUT, 0xDEAD);

    mac.reset_hw();

    assert_eq!(hw.peek(NCR) & (NCR_RE | NCR_TE), 0, "sigue encendido");
    assert_eq!(hw.peek(TSR), 0);
    assert_eq!(hw.peek(RSR), 0);
    assert_eq!(hw.peek(IMR), 0, "quedo alguna interrupcion sin enmascarar");
    assert_eq!(hw.peek(ISR), 0, "quedo alguna interrupcion pendiente");
    assert_eq!(hw.peek(GEM_PBUFRXCUT), 0);
}

/// `CLRSTAT` se escribe. Es una orden, asi que lo unico que se puede
/// comprobar es que paso por el registro.
#[test]
fn the_reset_asks_for_the_counters_to_be_cleared() {
    let hw = Hw::new();
    let marca = hw.write_mark();
    hw.mac().reset_hw();
    assert!(
        hw.writes()[marca..]
            .iter()
            .any(|(off, val)| *off == NCR && val & NCR_CLRSTAT != 0),
        "nadie pidio poner a cero los contadores"
    );
}

/// El reset **modifica** `NCR`, no lo reescribe: esto mismo se llama al bajar
/// la interfaz, y un `write` completo borraria el `MPE` dejando el bus MDIO
/// muerto sin decir nada.
#[test]
fn the_reset_does_not_take_the_management_port_down_with_it() {
    let hw = Hw::new();
    hw.mac().set_mdio_enabled(true);
    hw.poke(NCR, hw.peek(NCR) | NCR_RE | NCR_TE);
    hw.mac().reset_hw();
    assert_ne!(hw.peek(NCR) & NCR_MPE, 0, "se llevo el MPE por delante");
}

/// Las interrupciones se apagan **antes** de escribir el `ISR`.
///
/// Lo que importa de verdad es que esten apagadas antes de **leerlo**, porque
/// al contrario el silicio puede poner un bit entre la lectura y el apagado y
/// ese bit se queda puesto con la linea levantada. Eso desde el anfitrion no
/// se ve: una lectura de la ventana es una lectura de memoria y no deja
/// rastro. Asi que esto prueba el orden de las dos **escrituras**, que es la
/// mitad observable.
#[test]
fn the_reset_masks_the_interrupts_before_it_writes_the_status_register() {
    let hw = Hw::new();
    let marca = hw.write_mark();
    hw.mac().reset_hw();
    let offsets = hw.writes_since(marca);
    let apagado = offsets.iter().position(|off| *off == IDR);
    let limpiado = offsets.iter().position(|off| *off == ISR);
    assert!(apagado.is_some(), "no se apagaron las interrupciones");
    if let Some(limpiado) = limpiado {
        assert!(
            apagado.unwrap() < limpiado,
            "se limpio el ISR antes de enmascarar"
        );
    }
}

/// Donde el `ISR` no se limpia escribiendolo, no se escribe: la escritura no
/// haria nada, y creerse que la hizo es un manejador dando vueltas sobre la
/// misma interrupcion.
#[test]
fn the_reset_does_not_write_a_status_register_that_does_not_clear_that_way() {
    let hw = Hw::new();
    hw.poke(GEM_DCFG1, DCFG1_IRQCOR);
    let marca = hw.write_mark();
    hw.mac().reset_hw();
    assert!(
        !hw.writes_since(marca).contains(&ISR),
        "escribio el ISR de un silicio que no se limpia asi"
    );
}

// ---------------------------------------------------------------------------
// Configuracion de red
// ---------------------------------------------------------------------------

#[test]
fn the_config_is_written_whole_with_what_the_board_and_the_silicon_say() {
    let hw = Hw::new();
    let cfg = MacbConfig {
        pclk_hz: 200_000_000,
        promiscuous: false,
    };
    hw.mac().init_config(&cfg);
    let ncfgr = hw.peek(NCFGR);

    // 200 MHz caen en el tramo de 240, o sea dividir por 96.
    assert_eq!(
        (ncfgr >> NCFGR_CLK_SHIFT) & NCFGR_CLK_MASK,
        GEM_CLK_DIV96,
        "el divisor del MDC no es el del pclk"
    );
    // El falso declara un bus de 64 bits en `DBWDEF`.
    assert_eq!((ncfgr >> NCFGR_DBW_SHIFT) & NCFGR_DBW_MASK, GEM_DBW64);
    // Dos bytes de relleno, para que la cabecera IP caiga alineada a cuatro.
    assert_eq!((ncfgr >> NCFGR_RBOF_SHIFT) & NCFGR_RBOF_MASK, RX_IP_ALIGN);
    assert_ne!(ncfgr & NCFGR_DRFCS, 0, "no quita el FCS");
    assert_ne!(ncfgr & NCFGR_BIG, 0, "no acepta tramas de 1536");
    // Nada de jumbo: el nodo del RP1 no lo declara, y pedirlo sin tenerlo
    // cambia el significado del campo de longitud del descriptor.
    assert_eq!(ncfgr & NCFGR_JFRAME, 0, "pidio jumbo");
    // Ni promiscuo, que no se pidio.
    assert_eq!(ncfgr & NCFGR_CAF, 0);
    // Ni gigabit ni 100: la velocidad es del enlace, no del arranque.
    assert_eq!(ncfgr & (NCFGR_GBE | NCFGR_SPD), 0);
}

#[test]
fn the_config_asks_for_promiscuous_mode_when_the_board_wants_it() {
    let hw = Hw::new();
    hw.mac().init_config(&MacbConfig {
        pclk_hz: 200_000_000,
        promiscuous: true,
    });
    assert_ne!(hw.peek(NCFGR) & NCFGR_CAF, 0);
}

/// El modo promiscuo se puede cambiar despues sin tocar el resto.
#[test]
fn promiscuous_mode_can_be_turned_on_and_off_without_losing_the_config() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.init_config(&MacbConfig::default());
    let antes = hw.peek(NCFGR);

    mac.set_promiscuous(true);
    assert_ne!(hw.peek(NCFGR) & NCFGR_CAF, 0);
    mac.set_promiscuous(false);
    assert_eq!(hw.peek(NCFGR), antes, "cambio algo mas que el CAF");
}

/// La tabla del divisor del MDC, en los bordes.
///
/// Los tramos son inclusivos por arriba, como los `<=` de Linux, y el techo
/// del MDC son 2,5 MHz: pasarse no da ningun error, da un PHY que contesta
/// basura.
#[test]
fn the_mdc_divider_is_the_table_linux_publishes_borders_included() {
    let campo = |pclk| (mdc_clk_div(pclk) >> NCFGR_CLK_SHIFT) & NCFGR_CLK_MASK;
    assert_eq!(campo(1), GEM_CLK_DIV8);
    assert_eq!(campo(20_000_000), GEM_CLK_DIV8, "el borde es inclusivo");
    assert_eq!(campo(20_000_001), GEM_CLK_DIV16);
    assert_eq!(campo(40_000_000), GEM_CLK_DIV16);
    assert_eq!(campo(40_000_001), GEM_CLK_DIV32);
    assert_eq!(campo(80_000_000), GEM_CLK_DIV32);
    assert_eq!(campo(80_000_001), GEM_CLK_DIV48);
    assert_eq!(campo(120_000_000), GEM_CLK_DIV48);
    assert_eq!(campo(120_000_001), GEM_CLK_DIV64);
    assert_eq!(campo(160_000_000), GEM_CLK_DIV64);
    assert_eq!(campo(160_000_001), GEM_CLK_DIV96);
    assert_eq!(campo(240_000_000), GEM_CLK_DIV96);
    assert_eq!(campo(240_000_001), GEM_CLK_DIV128);
    assert_eq!(campo(320_000_000), GEM_CLK_DIV128);
    assert_eq!(campo(320_000_001), GEM_CLK_DIV224);
    assert_eq!(campo(u32::MAX), GEM_CLK_DIV224);
    // Y el divisor sale en su campo, sin tocar nada mas.
    assert_eq!(mdc_clk_div(1), 0);
    assert_eq!(
        mdc_clk_div(u32::MAX),
        GEM_CLK_DIV224 << NCFGR_CLK_SHIFT,
        "el divisor se salio de su campo"
    );
}

/// El `clk_sys` del RP1 son 200 MHz, y a esa frecuencia el MDC tiene que
/// quedar por debajo de los 2,5 MHz que permite la clausula 22.
#[test]
fn the_divider_the_pi_5_gets_keeps_the_mdc_under_the_limit() {
    let cfg = MacbConfig::default();
    assert_eq!(cfg.pclk_hz, 200_000_000);
    let divisor = match (mdc_clk_div(cfg.pclk_hz) >> NCFGR_CLK_SHIFT) & NCFGR_CLK_MASK {
        GEM_CLK_DIV8 => 8,
        GEM_CLK_DIV16 => 16,
        GEM_CLK_DIV32 => 32,
        GEM_CLK_DIV48 => 48,
        GEM_CLK_DIV64 => 64,
        GEM_CLK_DIV96 => 96,
        GEM_CLK_DIV128 => 128,
        _ => 224,
    };
    assert!(
        cfg.pclk_hz / divisor <= 2_500_000,
        "el MDC se sale del limite de la clausula 22: {} Hz",
        cfg.pclk_hz / divisor
    );
}

/// El ancho del bus que el silicio declara **no** se codifica igual que el
/// campo que lo programa, y cualquier cosa que no se reconozca son 32 bits,
/// que es la unica suposicion que no puede partir una transferencia.
#[test]
fn the_bus_width_is_translated_not_copied() {
    let campo =
        |dbwdef: u32| (dbw_field(dbwdef << DCFG1_DBWDEF_SHIFT) >> NCFGR_DBW_SHIFT) & NCFGR_DBW_MASK;
    assert_eq!(campo(1), GEM_DBW32);
    assert_eq!(campo(2), GEM_DBW64);
    assert_eq!(campo(4), GEM_DBW128);
    // Lo que no esta en la tabla: 32 bits.
    for declarado in [0, 3, 5, 6, 7] {
        assert_eq!(campo(declarado), GEM_DBW32, "DBWDEF {declarado}");
    }
    // Y la traduccion no es la identidad, que es el error facil.
    assert_ne!(campo(4), 4);
    // El campo sale donde tiene que salir, sin tocar nada mas.
    assert_eq!(
        dbw_field(4 << DCFG1_DBWDEF_SHIFT),
        GEM_DBW128 << NCFGR_DBW_SHIFT
    );
}

// ---------------------------------------------------------------------------
// Direccion MAC
// ---------------------------------------------------------------------------

/// El orden de los bytes es el **contrario** al del GENET: el primer byte del
/// cable es el menos significativo de `SA1B`. Equivocarlo da una tarjeta que
/// no se reconoce en ninguna trama, sin un solo error en los contadores.
#[test]
fn the_mac_goes_into_the_registers_little_endian_first_byte_lowest() {
    let hw = Hw::new();
    let mac = hw.mac();
    let addr = [0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56];
    mac.write_mac_addr(&addr);

    assert_eq!(hw.peek(GEM_SA1B), 0x1267_cf2c);
    assert_eq!(hw.peek(GEM_SA1T), 0x0000_5634);
    assert_eq!(mac.read_mac_addr_set(0), addr);
}

/// Los otros tres juegos se ponen a cero: uno con basura filtra tramas ajenas
/// hacia dentro, y nadie se enteraria.
#[test]
fn writing_the_mac_clears_the_other_three_address_sets() {
    let hw = Hw::new();
    let mac = hw.mac();
    for set in 0..SA_SETS {
        hw.poke(GEM_SA1B + set * SA_STRIDE, 0xAAAA_AAAA);
        hw.poke(GEM_SA1T + set * SA_STRIDE, 0xBBBB);
    }
    mac.write_mac_addr(&[0x2c, 0xcf, 0x67, 1, 2, 3]);
    for set in 1..SA_SETS {
        assert_eq!(hw.peek(GEM_SA1B + set * SA_STRIDE), 0, "juego {set}");
        assert_eq!(hw.peek(GEM_SA1T + set * SA_STRIDE), 0, "juego {set}");
    }
}

/// La MAC que dejo la firmware se busca en los cuatro juegos, y vale el
/// primero que sea una MAC de verdad.
#[test]
fn the_firmware_mac_is_looked_for_in_every_address_set() {
    let hw = Hw::new();
    let mac = hw.mac();
    assert_eq!(mac.hw_mac_addr(), None, "una ventana a ceros no trae MAC");

    // En el tercer juego, que es el sitio donde el MACB viejo tiene el
    // primero: si los offsets se mezclaran, esto se leeria del juego 0.
    hw.poke(GEM_SA1B + 2 * SA_STRIDE, 0x1267_cf2c);
    hw.poke(GEM_SA1T + 2 * SA_STRIDE, 0x0000_5634);
    assert_eq!(
        mac.hw_mac_addr(),
        Some([0x2c, 0xcf, 0x67, 0x12, 0x34, 0x56])
    );
}

/// Y una MAC que no vale no se coge: ni los ceros de un registro que nadie
/// relleno ni una direccion de grupo, que una tarjeta no puede tener por suya.
#[test]
fn an_address_that_is_not_a_unicast_address_is_not_taken_as_the_cards_own() {
    assert!(!mac_addr_is_valid(&[0, 0, 0, 0, 0, 0]), "todo ceros");
    assert!(!mac_addr_is_valid(&[0xff; 6]), "broadcast");
    assert!(!mac_addr_is_valid(&[0x01, 0, 0x5e, 1, 2, 3]), "multicast");
    assert!(mac_addr_is_valid(&[0x2c, 0xcf, 0x67, 1, 2, 3]));
    // El bit de grupo es el bajo del PRIMER byte, no el de ningun otro: con
    // el bit mirado en el sitio equivocado, media tabla de MAC reales se
    // rechazaria.
    assert!(mac_addr_is_valid(&[0x2c, 0x01, 0x01, 0x01, 0x01, 0x01]));

    let hw = Hw::new();
    hw.poke(GEM_SA1B, 0x0000_0001);
    assert_eq!(hw.mac().hw_mac_addr(), None, "cogio una MAC de grupo");
}

// ---------------------------------------------------------------------------
// Enlace
// ---------------------------------------------------------------------------

/// La velocidad son **dos** campos: `SPD` dice 10 o 100 y `GBE` dice gigabit.
#[test]
fn the_speed_is_two_fields_and_gigabit_is_not_the_hundred_bit() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.init_config(&MacbConfig::default());

    mac.adjust_link(&Link {
        up: true,
        speed: LinkSpeed::Mbps1000,
        full_duplex: true,
    })
    .unwrap();
    let ncfgr = hw.peek(NCFGR);
    assert_ne!(ncfgr & NCFGR_GBE, 0, "gigabit sin GBE");
    assert_eq!(ncfgr & NCFGR_SPD, 0, "gigabit con el bit de 100 puesto");
    assert_ne!(ncfgr & NCFGR_FD, 0);

    mac.adjust_link(&Link {
        up: true,
        speed: LinkSpeed::Mbps100,
        full_duplex: true,
    })
    .unwrap();
    let ncfgr = hw.peek(NCFGR);
    assert_ne!(ncfgr & NCFGR_SPD, 0, "100 sin SPD");
    assert_eq!(
        ncfgr & NCFGR_GBE,
        0,
        "se dejo el gigabit puesto al bajar a 100"
    );

    mac.adjust_link(&Link {
        up: true,
        speed: LinkSpeed::Mbps10,
        full_duplex: false,
    })
    .unwrap();
    let ncfgr = hw.peek(NCFGR);
    assert_eq!(ncfgr & (NCFGR_SPD | NCFGR_GBE), 0, "10 no es la ausencia");
    assert_eq!(ncfgr & NCFGR_FD, 0, "half duplex con FD puesto");
}

/// Ajustar el enlace no toca el resto de `NCFGR`, que es del arranque: el
/// divisor del MDC, el ancho del bus y el relleno de recepcion.
#[test]
fn adjusting_the_link_leaves_the_rest_of_the_config_alone() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.init_config(&MacbConfig::default());
    let antes = hw.peek(NCFGR);

    mac.adjust_link(&Link {
        up: true,
        speed: LinkSpeed::Mbps1000,
        full_duplex: true,
    })
    .unwrap();

    let suyos = NCFGR_SPD | NCFGR_FD | NCFGR_GBE;
    assert_eq!(
        hw.peek(NCFGR) & !suyos,
        antes & !suyos,
        "se llevo por delante algo del arranque"
    );
}

/// Un enlace caido no se programa: lo que diga su velocidad es de la vuelta
/// anterior.
#[test]
fn a_link_that_is_down_is_refused_rather_than_programmed() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.init_config(&MacbConfig::default());
    let antes = hw.peek(NCFGR);
    assert_eq!(
        mac.adjust_link(&Link::DOWN).err(),
        Some(DeviceError::NotReady)
    );
    assert_eq!(hw.peek(NCFGR), antes);
}

#[test]
fn receive_and_transmit_are_turned_on_and_off_together_without_touching_anything_else() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.set_mdio_enabled(true);

    mac.set_rx_tx_enabled(true);
    assert_eq!(hw.peek(NCR) & (NCR_RE | NCR_TE), NCR_RE | NCR_TE);
    assert_ne!(
        hw.peek(NCR) & NCR_MPE,
        0,
        "apago el puerto de mantenimiento"
    );

    mac.set_rx_tx_enabled(false);
    assert_eq!(hw.peek(NCR) & (NCR_RE | NCR_TE), 0);
    assert_ne!(hw.peek(NCR) & NCR_MPE, 0);
}
