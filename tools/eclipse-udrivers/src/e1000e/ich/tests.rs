//! The ICH8/9/10 bring-up against a card that only pretends: registers in
//! a map, a PHY at MDIO address 1, the SW/FW semaphore, and a reset that
//! hangs the card if anything is read too soon after it.

use super::*;
use crate::e1000e::pch::{
    ADVERTISE_1000FULL, ADVERTISE_ALL, BMCR_ANENABLE, BMCR_ANRESTART, BMSR_ESTATEN,
    ESTATUS_1000_TFULL, MII_ADVERTISE, MII_BMCR, MII_BMSR, MII_CTRL1000, MII_ESTATUS,
};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

const MDIC: usize = 0x0020;

#[derive(Default)]
struct Card {
    regs: RefCell<HashMap<usize, u32>>,
    phy: RefCell<HashMap<(u8, u32), u16>>,
    /// Microseconds waited since the reset, while it is in force.
    since_reset: Cell<Option<u64>>,
    /// A read came less than 20 ms after the reset.
    hung: Cell<bool>,
    swflag_at_reset: Cell<Option<bool>>,
}

impl Card {
    fn reg(&self, r: usize) -> u32 {
        *self.regs.borrow().get(&r).unwrap_or(&0)
    }
    fn set(&self, r: usize, v: u32) {
        self.regs.borrow_mut().insert(r, v);
    }
    fn phy(&self, reg: u32) -> u16 {
        *self.phy.borrow().get(&(PHY, reg)).unwrap_or(&0)
    }
    /// A gigabit PHY that offers less than it can.
    fn gigabit_phy(&self) {
        let mut phy = self.phy.borrow_mut();
        phy.insert((PHY, MII_BMCR), BMCR_ANENABLE);
        phy.insert((PHY, MII_BMSR), BMSR_ESTATEN);
        phy.insert((PHY, MII_ESTATUS), ESTATUS_1000_TFULL);
        phy.insert((PHY, MII_ADVERTISE), 0x01e0);
        phy.insert((PHY, MII_CTRL1000), 0);
    }
}

impl Hw for Card {
    fn rd(&self, r: usize) -> u32 {
        if self.since_reset.get().is_some_and(|t| t < 20_000) {
            self.hung.set(true);
        }
        self.reg(r)
    }

    fn wr(&self, r: usize, v: u32) {
        match r {
            CTRL if v & CTRL_RST != 0 => {
                self.swflag_at_reset
                    .set(Some(self.reg(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG != 0));
                self.since_reset.set(Some(0));
                self.set(CTRL, 0);
            }
            MDIC => {
                let reg = (v >> 16) & 0x1f;
                let addr = ((v >> 21) & 0x1f) as u8;
                if v & (1 << 26) != 0 {
                    self.phy.borrow_mut().insert((addr, reg), v as u16);
                    self.set(MDIC, v | (1 << 28));
                } else if let Some(&d) = self.phy.borrow().get(&(addr, reg)) {
                    self.set(MDIC, (v & !0xffff) | d as u32 | (1 << 28));
                } else {
                    // No PHY at that address: an error.
                    self.set(MDIC, v | (1 << 28) | (1 << 30));
                }
            }
            _ => self.set(r, v),
        }
    }

    fn delay_us(&self, us: u64) {
        if let Some(t) = self.since_reset.get() {
            self.since_reset
                .set(Some(t + us).filter(|&t| t < 1_000_000));
        }
    }
}

#[test]
fn every_ich8_ich9_and_ich10_id_is_a_part() {
    for id in [0x1049, 0x104a, 0x104b, 0x104c, 0x104d, 0x10c4, 0x10c5] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::Ich8), "{id:#x}");
    }
    for id in [
        0x10bd, 0x294c, 0x10bf, 0x10f5, 0x10cb, 0x10c0, 0x10c3, 0x10c2, 0x10e5, 0x10cc, 0x10cd,
        0x10ce, 0x10de, 0x10df, 0x1525,
    ] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::Ich9), "{id:#x}");
    }
}

#[test]
fn the_reset_is_under_the_semaphore_and_nothing_is_read_for_20_ms_after_it() {
    let card = Card::default();
    card.gigabit_phy();
    let report = reset(&card, Part::Ich9).unwrap();
    assert!(!card.hung.get(), "a read too soon after the reset");
    assert_eq!(card.swflag_at_reset.get(), Some(true));
    assert!(report.swflag);
    assert_eq!(
        card.reg(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG,
        0,
        "the semaphore left held"
    );
}

#[test]
fn ich8_gets_its_fifo_workaround_and_its_own_bits() {
    let card = Card::default();
    card.gigabit_phy();
    card.set(STATUS, 1 << 31);
    reset(&card, Part::Ich8).unwrap();
    assert_eq!((card.reg(PBA), card.reg(PBS)), (8, 16));
    assert_eq!(card.reg(TARC0) & (3 << 28), 3 << 28);
    assert_eq!(card.reg(STATUS) & (1 << 31), 0);
    assert_ne!(card.reg(RFCTL) & RFCTL_IPV6_EX_DIS, 0);
    assert_eq!(card.reg(GCR) & GCR_NO_SNOOP_ALL, GCR_NO_SNOOP_ALL);
    let (high, low) = watermarks(8);
    assert_eq!((card.reg(FCRTH), card.reg(FCRTL)), (high, low | FCRTL_XONE));
}

#[test]
fn ich9_has_a_10k_buffer_and_none_of_ich8s_workarounds() {
    let card = Card::default();
    card.gigabit_phy();
    reset(&card, Part::Ich9).unwrap();
    assert_eq!(card.reg(PBA), 10);
    assert_eq!(card.reg(PBS), 0);
    assert_eq!(card.reg(TARC0) & (3 << 28), 0);
    assert_eq!(card.reg(GCR), 0);
    assert_eq!(card.reg(RFCTL) & RFCTL_IPV6_EX_DIS, 0);
    assert_ne!(card.reg(RFCTL) & RFCTL_NFSW_DIS, 0);
    assert_ne!(card.reg(KABGTXD) & KABGTXD_BGSQLBIAS, 0);
}

#[test]
fn lplu_is_cleared_in_phy_ctrl_and_autoneg_restarted_at_address_1_with_gigabit() {
    let card = Card::default();
    card.gigabit_phy();
    card.set(PHY_CTRL, PHY_CTRL_LPLU_AND_GBE | 1);
    let report = reset(&card, Part::Ich9).unwrap();
    assert_eq!(card.reg(PHY_CTRL), 1);
    assert_eq!(report.autoneg, Some(PHY));
    assert_eq!(card.phy(MII_ADVERTISE), ADVERTISE_ALL);
    assert_eq!(card.phy(MII_CTRL1000), ADVERTISE_1000FULL);
    assert_eq!(
        card.phy(MII_BMCR) & (BMCR_ANENABLE | BMCR_ANRESTART),
        BMCR_ANENABLE | BMCR_ANRESTART
    );
}

#[test]
fn an_ife_phy_is_not_offered_gigabit() {
    let card = Card::default();
    card.phy.borrow_mut().insert((PHY, MII_BMCR), BMCR_ANENABLE);
    card.phy.borrow_mut().insert((PHY, MII_BMSR), 0);
    reset(&card, Part::Ich8).unwrap();
    assert_eq!(card.phy.borrow().get(&(PHY, MII_CTRL1000)), None);
}

#[test]
fn the_watermarks_leave_room_for_a_frame() {
    for kb in [8, 10] {
        let (high, low) = watermarks(kb);
        assert!(high + MAX_FRAME <= kb << 10);
        assert_eq!(high % 8, 0);
        assert_eq!(low, high - 8);
    }
}
