//! El UMAC del GENET: identificacion, reset, direccion MAC y lo que se le
//! cuenta del enlace.

use super::genet_fake::Hw;
use super::*;

#[test]
fn a_bcm2711_reads_as_genet_v5() {
    let hw = Hw::new();
    let mac = hw.mac();
    // El BCM2711 pone un 6 en el nibble de la generacion.
    assert_eq!(mac.hw_version(), 5);
    assert!(mac.check_supported().is_ok());
}

#[test]
fn the_version_nibble_is_translated_not_read_straight() {
    // Las tres traducciones que no son la identidad. Sin ellas un 6 seria una
    // generacion que no existe y un 0 tampoco.
    for (nibble, version) in [(6u32, 5u8), (5, 4), (0, 1)] {
        let hw = Hw::new();
        hw.poke(SYS_REV_CTRL, nibble << 24);
        assert_eq!(
            hw.mac().hw_version(),
            version,
            "el nibble {nibble} tiene que leerse como la generacion {version}"
        );
    }
}

#[test]
fn a_genet_that_is_not_v5_is_refused() {
    // Un v4 (nibble 5) tiene los bloques en otros offsets: seguir adelante
    // seria escribir registros al azar.
    let hw = Hw::new();
    hw.poke(SYS_REV_CTRL, 5 << 24);
    assert_eq!(hw.mac().check_supported(), Err(DeviceError::NotSupported));
}

#[test]
fn the_port_is_wired_to_an_external_gigabit_phy() {
    let hw = Hw::new();
    hw.mac().set_port_mode_ext_gphy();
    assert_eq!(hw.peek(SYS_PORT_CTRL), PORT_MODE_EXT_GPHY);
}

#[test]
fn the_rbuf_flush_goes_up_down_and_to_zero_in_that_order() {
    // Los tres pasos no son redundantes: el primero vacia el buffer de
    // recepcion y el ultimo suelta el reset del propio RBUF. Un reset que se
    // salte el tercero deja el RBUF parado con todos los registros sanos.
    let hw = Hw::new();
    hw.mac().reset_umac();
    let writes = hw.rbuf_flush_writes();
    assert_eq!(writes.len(), 3, "tres escrituras, no {}", writes.len());
    assert_ne!(writes[0] & (1 << 1), 0, "la primera sube el bit 1");
    assert_eq!(writes[1] & (1 << 1), 0, "la segunda lo baja");
    assert_eq!(writes[2], 0, "la tercera deja el registro a cero");
}

#[test]
fn the_umac_reset_holds_local_loopback_while_it_resets() {
    // Mientras se resetea, el MAC no puede escupir nada al cable: el
    // `CMD_SW_RESET` va acompañado del bucle local, y el registro vuelve a
    // cero despues.
    let hw = Hw::new();
    hw.mac().reset_umac();
    let writes = hw.umac_cmd_writes();
    assert_eq!(writes, [0, CMD_SW_RESET | CMD_LCL_LOOP_EN, 0]);
}

#[test]
fn the_reset_clears_the_mib_counters_and_lets_them_count_again() {
    // Los tres bits de reset se ponen y se quitan: dejarlos puestos seria un
    // MAC que no cuenta nada.
    let hw = Hw::new();
    hw.mac().reset_umac();
    assert_eq!(hw.peek(UMAC_MIB_CTRL), 0);
}

#[test]
fn the_reset_sets_the_frame_ceiling_and_the_two_byte_alignment() {
    let hw = Hw::new();
    // Basura de un arranque anterior en el RBUF_CTRL: la alineacion se añade,
    // no se escribe encima.
    hw.poke(RBUF_CTRL, 0x8000_0001);
    hw.mac().reset_umac();
    assert_eq!(hw.peek(UMAC_MAX_FRAME_LEN), ENET_MAX_MTU_SIZE);
    assert_eq!(hw.peek(RBUF_CTRL), 0x8000_0001 | RBUF_ALIGN_2B);
    assert_eq!(hw.peek(RBUF_TBUF_SIZE_CTRL), 1);
}

#[test]
fn the_mac_address_goes_out_big_endian_and_comes_back_the_same() {
    let hw = Hw::new();
    let mac = hw.mac();
    let addr = [0xDC, 0xA6, 0x32, 0x01, 0x23, 0x45];
    mac.write_mac_addr(&addr);
    // El primer byte del cable es el mas significativo de `UMAC_MAC0`.
    assert_eq!(hw.peek(UMAC_MAC0), 0xDCA6_3201);
    assert_eq!(hw.peek(UMAC_MAC1), 0x0000_2345);
    assert_eq!(mac.read_mac_addr(), addr);
}

#[test]
fn the_tx_flush_is_raised_and_lowered() {
    // Dejarlo puesto es una cola de transmision que se vacia sola para
    // siempre.
    let hw = Hw::new();
    hw.mac().flush_tx();
    assert_eq!(hw.peek(UMAC_TX_FLUSH), 0);
}

#[test]
fn adjusting_the_link_does_not_turn_the_mac_off() {
    // La divergencia 1 con U-Boot: U-Boot escribe la velocidad ENCIMA de
    // `UMAC_CMD`, lo que borra TX_EN/RX_EN. En U-Boot da igual porque enciende
    // despues; aqui apagaria la tarjeta en silencio en cada renegociacion.
    let hw = Hw::new();
    let mac = hw.mac();
    mac.set_rx_tx_enabled(true);
    mac.adjust_link(
        &Link {
            up: true,
            speed: LinkSpeed::Mbps1000,
            full_duplex: true,
        },
        true,
    )
    .expect("un enlace arriba se ajusta");
    let cmd = hw.peek(UMAC_CMD);
    assert_ne!(cmd & CMD_TX_EN, 0, "la transmision sigue encendida");
    assert_ne!(cmd & CMD_RX_EN, 0, "la recepcion sigue encendida");
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_1000);
}

#[test]
fn a_half_duplex_link_puts_the_mac_in_half_duplex() {
    // La divergencia 2 con U-Boot: U-Boot no toca el duplex, asi que un enlace
    // a half se queda con el MAC a full y el resultado son colisiones que el
    // contador de errores no explica.
    let hw = Hw::new();
    let mac = hw.mac();
    mac.adjust_link(
        &Link {
            up: true,
            speed: LinkSpeed::Mbps100,
            full_duplex: false,
        },
        true,
    )
    .expect("un enlace arriba se ajusta");
    assert_ne!(hw.peek(UMAC_CMD) & CMD_HD_EN, 0);

    // Y al volver a full duplex el bit se quita: es positivo, no se queda.
    mac.adjust_link(
        &Link {
            up: true,
            speed: LinkSpeed::Mbps100,
            full_duplex: true,
        },
        true,
    )
    .expect("un enlace arriba se ajusta");
    assert_eq!(hw.peek(UMAC_CMD) & CMD_HD_EN, 0);
}

#[test]
fn the_speed_field_is_replaced_not_accumulated() {
    // El campo son dos bits: pasar de gigabit (2) a 100 (1) sin limpiarlos
    // dejaria un 3, que es 2,5 gigabit y no existe en esta placa.
    let hw = Hw::new();
    let mac = hw.mac();
    for (speed, code) in [
        (LinkSpeed::Mbps1000, UMAC_SPEED_1000),
        (LinkSpeed::Mbps100, UMAC_SPEED_100),
        (LinkSpeed::Mbps10, UMAC_SPEED_10),
    ] {
        mac.adjust_link(
            &Link {
                up: true,
                speed,
                full_duplex: true,
            },
            true,
        )
        .expect("un enlace arriba se ajusta");
        assert_eq!(
            (hw.peek(UMAC_CMD) >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK,
            code,
            "la velocidad {} tiene que dejar el codigo {code}",
            speed.mbps()
        );
        assert_ne!(code, UMAC_SPEED_2500, "esta placa no hace 2,5 gigabit");
    }
}

#[test]
fn the_rgmii_is_told_the_link_is_up_and_the_oob_is_let_go() {
    let hw = Hw::new();
    // `OOB_DISABLE` puesto de antes: el ajuste tiene que quitarlo, o el MAC no
    // se cree el enlace que se le acaba de contar.
    hw.poke(EXT_RGMII_OOB_CTRL, OOB_DISABLE);
    hw.mac()
        .adjust_link(
            &Link {
                up: true,
                speed: LinkSpeed::Mbps1000,
                full_duplex: true,
            },
            true,
        )
        .expect("un enlace arriba se ajusta");
    let oob = hw.peek(EXT_RGMII_OOB_CTRL);
    assert_eq!(oob & OOB_DISABLE, 0);
    assert_ne!(oob & RGMII_LINK, 0);
    assert_ne!(oob & RGMII_MODE_EN, 0);
    assert_ne!(
        oob & ID_MODE_DIS,
        0,
        "con id_mode_dis el retardo lo pone la placa"
    );
}

#[test]
fn without_id_mode_dis_the_mac_keeps_its_internal_delay() {
    let hw = Hw::new();
    hw.mac()
        .adjust_link(
            &Link {
                up: true,
                speed: LinkSpeed::Mbps1000,
                full_duplex: true,
            },
            false,
        )
        .expect("un enlace arriba se ajusta");
    assert_eq!(hw.peek(EXT_RGMII_OOB_CTRL) & ID_MODE_DIS, 0);
}

#[test]
fn adjusting_a_link_that_is_down_is_refused_and_touches_nothing() {
    // Ajustar un enlace caido escribiria una velocidad inventada en el MAC.
    let hw = Hw::new();
    assert_eq!(
        hw.mac().adjust_link(&Link::DOWN, true),
        Err(DeviceError::NotReady)
    );
    assert_eq!(hw.peek(EXT_RGMII_OOB_CTRL), 0);
    assert_eq!(hw.peek(UMAC_CMD), 0);
}

#[test]
fn rx_and_tx_go_on_and_off_without_disturbing_the_rest() {
    let hw = Hw::new();
    let mac = hw.mac();
    // La velocidad ya puesta no se pierde al encender.
    hw.poke(UMAC_CMD, UMAC_SPEED_1000 << CMD_SPEED_SHIFT);
    mac.set_rx_tx_enabled(true);
    let cmd = hw.peek(UMAC_CMD);
    assert_ne!(cmd & (CMD_TX_EN | CMD_RX_EN), 0);
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_1000);

    mac.set_rx_tx_enabled(false);
    let cmd = hw.peek(UMAC_CMD);
    assert_eq!(cmd & (CMD_TX_EN | CMD_RX_EN), 0);
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_1000);
}

#[test]
fn promiscuous_mode_goes_on_and_off_without_touching_rx_tx() {
    let hw = Hw::new();
    let mac = hw.mac();
    mac.set_rx_tx_enabled(true);
    mac.set_promiscuous(true);
    assert_ne!(hw.peek(UMAC_CMD) & CMD_PROMISC, 0);
    assert_ne!(hw.peek(UMAC_CMD) & (CMD_TX_EN | CMD_RX_EN), 0);
    mac.set_promiscuous(false);
    assert_eq!(hw.peek(UMAC_CMD) & CMD_PROMISC, 0);
    assert_ne!(hw.peek(UMAC_CMD) & (CMD_TX_EN | CMD_RX_EN), 0);
}

#[test]
fn the_register_offsets_land_in_the_block_each_one_belongs_to() {
    // Un offset mal copiado de los drivers libres es un registro al azar, y
    // con el silicio sin hoja de datos publica nadie lo veria hasta tener la
    // placa delante. Lo que se puede comprobar aqui es que cada registro cae
    // dentro de su bloque.
    for (name, off, lo, hi) in [
        ("SYS_REV_CTRL", SYS_REV_CTRL, 0x0000, 0x0080),
        ("SYS_PORT_CTRL", SYS_PORT_CTRL, 0x0000, 0x0080),
        ("SYS_RBUF_FLUSH_CTRL", SYS_RBUF_FLUSH_CTRL, 0x0000, 0x0080),
        ("SYS_TBUF_FLUSH_CTRL", SYS_TBUF_FLUSH_CTRL, 0x0000, 0x0080),
        ("EXT_RGMII_OOB_CTRL", EXT_RGMII_OOB_CTRL, 0x0080, 0x0300),
        ("RBUF_CTRL", RBUF_CTRL, 0x0300, 0x0800),
        ("RBUF_TBUF_SIZE_CTRL", RBUF_TBUF_SIZE_CTRL, 0x0300, 0x0800),
        ("UMAC_CMD", UMAC_CMD, 0x0800, 0x2000),
        ("UMAC_MAC0", UMAC_MAC0, 0x0800, 0x2000),
        ("UMAC_MAC1", UMAC_MAC1, 0x0800, 0x2000),
        ("UMAC_MAX_FRAME_LEN", UMAC_MAX_FRAME_LEN, 0x0800, 0x2000),
        ("UMAC_TX_FLUSH", UMAC_TX_FLUSH, 0x0800, 0x2000),
        ("UMAC_MIB_CTRL", UMAC_MIB_CTRL, 0x0800, 0x2000),
        ("UMAC_MDIO_CMD", UMAC_MDIO_CMD, 0x0800, 0x2000),
    ] {
        assert!(
            off >= lo && off < hi,
            "{} ({:#x}) se ha salido de su bloque [{:#x}, {:#x})",
            name,
            off,
            lo,
            hi
        );
        assert_eq!(off % 4, 0, "{name} ({off:#x}) no esta alineado a 4");
    }
}
