//! El bus MDIO del GEM y el PHY que hay detras.

use super::macb_fake::*;
use super::*;
use alloc::vec;

/// La palabra que se escribe en `MAN` es la de la clausula 22, con cada cosa
/// en su campo.
#[test]
fn a_read_puts_every_part_of_the_transaction_in_its_own_field() {
    let hw = Hw::new();
    hw.mac().mdio_read(DEFAULT_PHY_ADDR, MII_BMSR).unwrap();
    let man = hw
        .writes()
        .into_iter()
        .rev()
        .find(|(off, _)| *off == MAN)
        .expect("no se escribio MAN")
        .1;

    assert_eq!((man >> MAN_SOF_SHIFT) & MAN_SOF_MASK, MAN_C22_SOF);
    assert_eq!((man >> MAN_RW_SHIFT) & MAN_RW_MASK, MAN_C22_READ);
    assert_eq!(
        (man >> MAN_PHYA_SHIFT) & MAN_PHYA_MASK,
        DEFAULT_PHY_ADDR as u32
    );
    assert_eq!((man >> MAN_REGA_SHIFT) & MAN_REGA_MASK, MII_BMSR);
    assert_eq!((man >> MAN_CODE_SHIFT) & MAN_CODE_MASK, MAN_C22_CODE);
    // Una lectura no lleva dato: un dato ahi seria una escritura disfrazada.
    assert_eq!((man >> MAN_DATA_SHIFT) & MAN_DATA_MASK, 0);
}

#[test]
fn a_write_carries_the_data_and_the_write_opcode() {
    let hw = Hw::new();
    hw.mac().mdio_write(3, MII_BMCR, 0xBEEF).unwrap();
    let man = hw
        .writes()
        .into_iter()
        .rev()
        .find(|(off, _)| *off == MAN)
        .expect("no se escribio MAN")
        .1;

    assert_eq!((man >> MAN_RW_SHIFT) & MAN_RW_MASK, MAN_C22_WRITE);
    assert_eq!((man >> MAN_PHYA_SHIFT) & MAN_PHYA_MASK, 3);
    assert_eq!((man >> MAN_DATA_SHIFT) & MAN_DATA_MASK, 0xBEEF);
    assert_eq!(hw.phy(3, MII_BMCR), Some(0xBEEF));
    assert_eq!(hw.phy_writes(), vec![(3, MII_BMCR, 0xBEEF)]);
}

/// Lo que se lee del PHY es lo que el PHY contesta, no lo que se le pidio.
#[test]
fn what_comes_back_from_a_read_is_the_phys_answer() {
    let hw = Hw::new();
    hw.set_phy(DEFAULT_PHY_ADDR, MII_BMSR, 0x796d);
    assert_eq!(hw.mac().mdio_read(DEFAULT_PHY_ADDR, MII_BMSR), Ok(0x796d));
}

/// La direccion del PHY y el registro son de cinco bits: un numero que no cabe
/// no puede derramarse al campo de al lado, que es la direccion de otro PHY.
#[test]
fn an_address_that_does_not_fit_cannot_spill_into_the_neighbouring_field() {
    let hw = Hw::new();
    hw.mac().mdio_write(0xFF, 0xFF, 0).unwrap();
    let man = hw
        .writes()
        .into_iter()
        .rev()
        .find(|(off, _)| *off == MAN)
        .unwrap()
        .1;
    assert_eq!((man >> MAN_PHYA_SHIFT) & MAN_PHYA_MASK, 0x1F);
    assert_eq!((man >> MAN_REGA_SHIFT) & MAN_REGA_MASK, 0x1F);
    // Y los campos fijos siguen siendo los suyos.
    assert_eq!((man >> MAN_SOF_SHIFT) & MAN_SOF_MASK, MAN_C22_SOF);
    assert_eq!((man >> MAN_RW_SHIFT) & MAN_RW_MASK, MAN_C22_WRITE);
}

/// Se espera a que la logica quede quieta **antes y despues**: antes porque
/// `MAN` escrito mientras hay una transaccion en marcha la pisa, y despues
/// porque hasta que acaba el dato que hay en `MAN` es el de la anterior.
#[test]
fn a_transaction_waits_for_the_bus_on_both_sides() {
    let hw = Hw::new();
    let marca = hw.write_mark();
    hw.set_phy(DEFAULT_PHY_ADDR, MII_PHYSID1, 0x600D);
    assert_eq!(
        hw.mac().mdio_read(DEFAULT_PHY_ADDR, MII_PHYSID1),
        Ok(0x600D)
    );
    // La espera no escribe nada, asi que lo que se puede comprobar es que el
    // dato que vuelve es el de ESTA transaccion: si no se esperara al final,
    // seria el de la anterior.
    assert_eq!(hw.writes_since(marca), vec![MAN]);
    assert_ne!(hw.peek(NSR) & NSR_IDLE, 0, "el bus quedo ocupado");
}

/// Una logica que no vuelve a reposo se va por el plazo en vez de colgar el
/// arranque.
#[test]
fn a_bus_that_never_goes_idle_times_out_instead_of_hanging() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_never_idle = true);
    assert_eq!(
        hw.mac().mdio_read(DEFAULT_PHY_ADDR, MII_BMSR).err(),
        Some(DeviceError::NotReady)
    );
    assert_eq!(
        hw.mac().mdio_write(DEFAULT_PHY_ADDR, MII_BMCR, 0).err(),
        Some(DeviceError::NotReady)
    );
}

/// Y si ya estaba ocupada antes de empezar, no se arranca nada.
#[test]
fn a_bus_that_is_busy_before_it_starts_is_not_stamped_on() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_never_idle = true);
    hw.poke(NSR, 0);
    let arrancadas = hw.mdio_starts();
    assert!(hw.mac().mdio_read(DEFAULT_PHY_ADDR, MII_BMSR).is_err());
    assert_eq!(
        hw.mdio_starts(),
        arrancadas,
        "piso una transaccion en marcha"
    );
}

/// El identificador del PHY son dos registros, el alto primero.
#[test]
fn the_phy_id_is_the_two_identification_registers_high_half_first() {
    let hw = Hw::new();
    assert_eq!(hw.mac().phy_id(DEFAULT_PHY_ADDR), Ok(0x600D_84A2));
}

/// **El GEM no tiene bit de error**: una direccion sin PHY devuelve todo unos,
/// que es lo que hace un bus en reposo, asi que quien busque un PHY tiene que
/// mirar el dato y no el resultado.
#[test]
fn an_address_with_no_phy_answers_all_ones_and_that_is_not_a_phy() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_mute = (0..32).collect());
    // La transaccion termina bien: no hay nada que falle.
    assert_eq!(hw.mac().phy_id(7), Ok(0xFFFF_FFFF));
    assert!(!phy_id_is_present(0xFFFF_FFFF));
    // Y por eso el barrido no encuentra nada en vez de quedarse con el 0.
    assert_eq!(hw.mac().phy_scan().err(), Some(DeviceError::NotReady));
}

/// El barrido encuentra el PHY donde este, que es la red de seguridad para
/// cuando el device tree no trae su direccion.
#[test]
fn the_scan_finds_the_phy_wherever_it_is() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_mute = (0..32).filter(|pmd| *pmd != 9).collect());
    hw.set_phy_present(9);
    assert_eq!(hw.mac().phy_scan(), Ok(9));
}

/// Reiniciar la autonegociacion es un read-modify-write del `BMCR`: lo que ya
/// hubiera ahi (el bit de autonegociacion encendida) no se puede perder.
#[test]
fn restarting_the_negotiation_keeps_what_the_control_register_already_said() {
    let hw = Hw::new();
    hw.set_phy(
        DEFAULT_PHY_ADDR,
        MII_BMCR,
        (BMCR_ANENABLE | BMCR_FULLDPLX) as u16,
    );
    hw.mac().restart_aneg(DEFAULT_PHY_ADDR).unwrap();
    let bmcr = hw.phy(DEFAULT_PHY_ADDR, MII_BMCR).unwrap() as u32;
    assert_ne!(bmcr & BMCR_ANRESTART, 0, "no reinicio la negociacion");
    assert_ne!(bmcr & BMCR_ANENABLE, 0);
    assert_ne!(bmcr & BMCR_FULLDPLX, 0, "perdio lo que ya estaba puesto");
    // Y no reseteo el PHY, que es el bit de al lado y tira el enlace.
    assert_eq!(bmcr & BMCR_RESET, 0);
}

/// El enlace se lee por este bus, con la misma decision que el GENET: la
/// logica es de [`super::super::mii`], lo que cambia es el acceso.
#[test]
fn the_link_is_read_through_this_bus_and_decided_the_same_way_everywhere() {
    let hw = Hw::new();
    let mac = hw.mac();

    assert_eq!(
        mac.read_link(DEFAULT_PHY_ADDR),
        Ok(Link {
            up: true,
            speed: LinkSpeed::Mbps1000,
            full_duplex: true,
        })
    );

    hw.set_link_100(DEFAULT_PHY_ADDR, false);
    assert_eq!(
        mac.read_link(DEFAULT_PHY_ADDR),
        Ok(Link {
            up: true,
            speed: LinkSpeed::Mbps100,
            full_duplex: false,
        })
    );

    hw.set_link_down(DEFAULT_PHY_ADDR);
    assert_eq!(mac.read_link(DEFAULT_PHY_ADDR), Ok(Link::DOWN));
}

/// Un bus que no contesta es un enlace que no se sabe, no un enlace caido:
/// darlo por caido es apagar una tarjeta que esta bien.
#[test]
fn a_bus_that_does_not_answer_is_an_unknown_link_not_a_link_that_is_down() {
    let hw = Hw::new();
    hw.faults(|f| f.mdio_never_idle = true);
    assert_eq!(
        hw.mac().read_link(DEFAULT_PHY_ADDR).err(),
        Some(DeviceError::NotReady)
    );
}
