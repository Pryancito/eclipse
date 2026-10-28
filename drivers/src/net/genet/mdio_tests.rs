//! El bus MDIO del GENET y el PHY que hay al otro lado.

use super::genet_fake::{Hw, DEFAULT_PHY_ADDR};
use super::*;
use crate::net::mii::*;

const PHY: u8 = DEFAULT_PHY_ADDR;

#[test]
fn a_read_brings_back_what_the_phy_has() {
    let hw = Hw::new();
    hw.set_phy(PHY, MII_BMCR, 0x1234);
    assert_eq!(hw.mac().mdio_read(PHY, MII_BMCR), Ok(0x1234));
}

#[test]
fn a_write_lands_in_the_phy_and_is_recorded() {
    let hw = Hw::new();
    hw.mac()
        .mdio_write(PHY, MII_ADVERTISE, 0xABCD)
        .expect("el PHY contesta");
    assert_eq!(hw.phy(PHY, MII_ADVERTISE), Some(0xABCD));
    assert_eq!(hw.phy_writes(), [(PHY, MII_ADVERTISE, 0xABCD)]);
}

#[test]
fn the_address_and_the_register_go_in_the_right_fields() {
    // Los dos campos estan pegados (21 y 16) y los dos son de 5 bits: cambiar
    // uno por el otro leeria el registro 3 del PHY 7 en vez del 7 del 3, y los
    // dos existen.
    let hw = Hw::new();
    hw.set_phy(3, 7, 0x0303);
    hw.set_phy(7, 3, 0x0707);
    assert_eq!(hw.mac().mdio_read(3, 7), Ok(0x0303));
    assert_eq!(hw.mac().mdio_read(7, 3), Ok(0x0707));
}

#[test]
fn the_transaction_is_loaded_first_and_started_after() {
    // El hardware no hace nada hasta que se le sube `START_BUSY`, y el driver
    // lo sube con read-modify-write para no tener que repetir la direccion y
    // el registro. Si alguna vez lo escribiera todo de una, la carga se
    // perderia: aqui se ve que hay una sola transaccion por lectura.
    let hw = Hw::new();
    hw.set_phy(PHY, MII_BMSR, 0x7849);
    assert_eq!(hw.mac().mdio_read(PHY, MII_BMSR), Ok(0x7849));
    assert_eq!(hw.mdio_starts(), 1);
}

#[test]
fn a_mac_that_never_finishes_the_transaction_times_out() {
    // `START_BUSY` que no baja es un MAC colgado: el driver tiene que salir
    // por el plazo y no quedarse ahi.
    let hw = Hw::new();
    hw.faults(|f| f.mdio_never_ready = true);
    assert_eq!(
        hw.mac().mdio_read(PHY, MII_BMSR),
        Err(DeviceError::NotReady)
    );
    assert_eq!(
        hw.mac().mdio_write(PHY, MII_BMCR, 0),
        Err(DeviceError::NotReady)
    );
}

#[test]
fn a_phy_that_is_not_there_is_an_io_error_not_a_zero() {
    // Lo que se ve cuando la direccion MDIO del device tree no es la del PHY
    // de la placa. Devolver el 0xffff del bus como dato haria creer que hay un
    // PHY con todos los bits puestos.
    let hw = Hw::new();
    hw.faults(|f| f.mdio_mute.push(9));
    assert_eq!(hw.mac().mdio_read(9, MII_BMSR), Err(DeviceError::IoError));
    // Y el PHY que si esta sigue contestando.
    assert!(hw.mac().mdio_read(PHY, MII_BMSR).is_ok());
}

#[test]
fn the_phy_id_is_the_two_halves_in_the_right_order() {
    // Cambiarlas de sitio daria 0x84a2600d, que no es el OUI de nadie.
    let hw = Hw::new();
    assert_eq!(hw.mac().phy_id(PHY), Ok(0x600D_84A2));
}

#[test]
fn the_pi_4_phy_is_recognised_whatever_its_revision_is() {
    // La revision va en el nibble bajo y cambia entre tiradas de la misma
    // placa: compararla haria que una Pi 4 de otra tirada no tuviera PHY.
    for rev in 0..16u32 {
        let id = BCM5421X_PHY_ID | rev;
        assert!(
            phy_is_bcm5421x(id),
            "{:#x} es de la familia del BCM54213PE",
            id
        );
    }
    // Y no vale cualquier cosa: el RTL8211F del D1, por ejemplo.
    assert!(!phy_is_bcm5421x(0x001C_C916));
}

#[test]
fn an_empty_bus_is_not_read_as_a_phy() {
    // El bus sin nadie deja las lineas arriba o abajo, y las dos cosas se leen
    // como un identificador.
    for id in [0u32, 0xFFFF_FFFF, 0x0000_FFFF, 0xFFFF_0000] {
        assert!(!phy_id_is_present(id), "{:#x} no es un PHY", id);
    }
    assert!(phy_id_is_present(0x600D_84A2));
}

#[test]
fn the_scan_finds_the_phy_the_device_tree_would_have_named() {
    let hw = Hw::new();
    assert_eq!(hw.mac().phy_scan(), Ok(PHY));
}

#[test]
fn a_bus_with_no_phy_at_all_is_not_ready() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_read_fail = true);
    assert_eq!(hw.mac().phy_scan(), Err(DeviceError::NotReady));
}

#[test]
fn the_scan_skips_the_addresses_that_do_not_answer() {
    // Un bus donde el PHY no esta en la direccion 0 obliga a seguir buscando
    // en vez de rendirse en la primera.
    let hw = Hw::new();
    hw.set_phy_present(17);
    hw.faults(|f| f.mdio_mute.extend(0..17u8));
    assert_eq!(hw.mac().phy_scan(), Ok(17));
}

#[test]
fn restarting_aneg_keeps_what_the_phy_already_had() {
    // El `BMCR` lleva tambien el duplex y la velocidad forzados: escribir solo
    // los dos bits de la autonegociacion los borraria.
    let hw = Hw::new();
    hw.set_phy(PHY, MII_BMCR, BMCR_FULLDPLX as u16);
    hw.mac().restart_aneg(PHY).expect("el PHY contesta");
    let bmcr = hw.phy(PHY, MII_BMCR).expect("el PHY tiene BMCR");
    assert_ne!(bmcr as u32 & BMCR_FULLDPLX, 0);
    assert_ne!(bmcr as u32 & BMCR_ANENABLE, 0);
    assert_ne!(bmcr as u32 & BMCR_ANRESTART, 0);
}

#[test]
fn a_gigabit_full_duplex_link_reads_as_such() {
    let hw = Hw::new();
    assert_eq!(
        hw.mac().read_link(PHY),
        Ok(Link {
            up: true,
            speed: LinkSpeed::Mbps1000,
            full_duplex: true,
        })
    );
}

#[test]
fn a_hundred_megabit_link_reads_its_duplex() {
    for full in [true, false] {
        let hw = Hw::new();
        hw.set_link_100(PHY, full);
        assert_eq!(
            hw.mac().read_link(PHY),
            Ok(Link {
                up: true,
                speed: LinkSpeed::Mbps100,
                full_duplex: full,
            })
        );
    }
}

#[test]
fn no_carrier_is_a_link_that_is_down() {
    let hw = Hw::new();
    hw.set_link_down(PHY);
    assert_eq!(hw.mac().read_link(PHY), Ok(Link::DOWN));
}

#[test]
fn a_negotiation_still_running_is_a_link_that_is_down() {
    // Hay portadora pero la autonegociacion no ha terminado: lo que digan
    // `STAT1000` y `LPA` ahora mismo es de la vuelta anterior, asi que un
    // driver que no mire `ANEGCOMPLETE` le contaria al MAC una velocidad que
    // no es la que va a quedar.
    let hw = Hw::new();
    hw.set_phy(PHY, MII_BMSR, BMSR_LSTATUS as u16);
    hw.set_phy(PHY, MII_STAT1000, LPA_1000FULL as u16);
    assert_eq!(hw.mac().read_link(PHY), Ok(Link::DOWN));
}

#[test]
fn the_latched_link_bit_is_read_twice() {
    // `BMSR` latcha `LSTATUS`: la primera lectura devuelve el estado desde la
    // lectura anterior. Un driver que lea una sola vez ve caido un enlace que
    // se cayo y volvio.
    let hw = Hw::new();
    let starts_before = hw.mdio_starts();
    hw.mac().read_link(PHY).expect("el PHY contesta");
    // Dos del BMSR mas CTRL1000, STAT1000, ADVERTISE y LPA.
    assert_eq!(hw.mdio_starts() - starts_before, 6);
}

#[test]
fn a_phy_that_stops_answering_mid_read_is_an_error_not_a_guess() {
    // La averia aparece despues de que el enlace se lea arriba: el resto de la
    // lectura tiene que fallar, no inventarse la velocidad.
    let hw = Hw::new();
    hw.faults(|f| f.mdio_never_ready = true);
    assert_eq!(hw.mac().read_link(PHY), Err(DeviceError::NotReady));
}

#[test]
fn gigabit_is_only_claimed_when_both_sides_advertised_it() {
    // `STAT1000` dice lo que anuncia el otro lado, no lo negociado: sin mirar
    // tambien lo que anunciamos nosotros, un PHY nuestro limitado a 100 se
    // leeria como gigabit.
    let hw = Hw::new();
    hw.set_phy(PHY, MII_CTRL1000, 0);
    hw.set_phy(PHY, MII_STAT1000, LPA_1000FULL as u16);
    hw.set_phy(PHY, MII_LPA, LPA_100FULL as u16);
    hw.set_phy(PHY, MII_ADVERTISE, ADVERTISE_100FULL as u16);
    assert_eq!(
        hw.mac().read_link(PHY),
        Ok(Link {
            up: true,
            speed: LinkSpeed::Mbps100,
            full_duplex: true,
        })
    );
}

#[test]
fn a_link_with_nothing_in_common_falls_back_to_ten_half() {
    // Lo que queda cuando ningun modo coincide: 10 megabits half duplex, que
    // es el minimo que todo Ethernet sabe hacer. Dejar la velocidad anterior
    // seria contarle al MAC un gigabit que el cable no da.
    let hw = Hw::new();
    hw.set_phy(PHY, MII_CTRL1000, 0);
    hw.set_phy(PHY, MII_STAT1000, 0);
    hw.set_phy(PHY, MII_ADVERTISE, ADVERTISE_CSMA as u16);
    hw.set_phy(PHY, MII_LPA, 0);
    assert_eq!(
        hw.mac().read_link(PHY),
        Ok(Link {
            up: true,
            speed: LinkSpeed::Mbps10,
            full_duplex: false,
        })
    );
}

#[test]
fn the_link_the_phy_negotiated_is_what_the_mac_gets_told() {
    // La cadena completa de la tanda: leer el enlace del PHY y ajustar el MAC.
    let hw = Hw::new();
    let mac = hw.mac();
    mac.set_port_mode_ext_gphy();
    mac.reset_umac();
    mac.write_mac_addr(&[0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33]);
    let link = mac.read_link(PHY).expect("el PHY contesta");
    mac.adjust_link(&link, true).expect("el enlace esta arriba");
    mac.set_rx_tx_enabled(true);

    let cmd = hw.peek(UMAC_CMD);
    assert_eq!((cmd >> CMD_SPEED_SHIFT) & CMD_SPEED_MASK, UMAC_SPEED_1000);
    assert_eq!(cmd & CMD_HD_EN, 0);
    assert_ne!(cmd & (CMD_TX_EN | CMD_RX_EN), 0);
    assert_eq!(mac.read_mac_addr(), [0xDC, 0xA6, 0x32, 0x11, 0x22, 0x33]);
}
