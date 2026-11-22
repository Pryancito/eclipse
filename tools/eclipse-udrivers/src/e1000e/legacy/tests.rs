//! The 82571/82572/82573 bring-up against a card that only pretends:
//! registers in a map, a PHY at MDIO address 1 (with IGP's page select),
//! `SWSM` and MDIO ownership, the reset that clears itself and the NVM's
//! automatic read.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

const MDIC: usize = 0x0020;

#[derive(Default)]
struct Card {
    regs: RefCell<HashMap<usize, u32>>,
    phy: RefCell<HashMap<u32, u16>>,
    page: Cell<u16>,
    /// The other port's driver, or the firmware, holds `SWSM`.
    swsm_held: Cell<bool>,
    /// Whether the PHY was touched without its owner held.
    unowned_phy: Cell<bool>,
    /// Whether the reset happened with the owner held.
    reset_owned: Cell<Option<bool>>,
}

impl Card {
    fn reg(&self, r: usize) -> u32 {
        *self.regs.borrow().get(&r).unwrap_or(&0)
    }
    fn set(&self, r: usize, v: u32) {
        self.regs.borrow_mut().insert(r, v);
    }
    fn owned(&self) -> bool {
        self.reg(SWSM) & SWSM_SWESMBI != 0
            || self.reg(EXTCNF_CTRL) & EXTCNF_CTRL_MDIO_SW_OWNERSHIP != 0
    }
    fn phy(&self, r: u32) -> u16 {
        *self.phy.borrow().get(&r).unwrap_or(&0)
    }
}

impl Hw for Card {
    fn rd(&self, r: usize) -> u32 {
        let v = self.reg(r);
        if r == SWSM && !self.swsm_held.get() {
            // A read takes SMBI when it is clear, and answers what it was.
            self.set(SWSM, v | SWSM_SMBI);
        }
        if r == SWSM && self.swsm_held.get() {
            return v | SWSM_SMBI;
        }
        v
    }

    fn wr(&self, r: usize, v: u32) {
        match r {
            CTRL if v & CTRL_RST != 0 => {
                self.reset_owned.set(Some(self.owned()));
                self.set(CTRL, 0);
                self.set(EECD, EECD_AUTO_RD | EECD_REQ | EECD_GNT);
            }
            CTRL if v & CTRL_GIO_MASTER_DISABLE != 0 => {
                self.set(CTRL, v);
                self.set(STATUS, self.reg(STATUS) & !STATUS_GIO_MASTER_ENABLE);
            }
            MDIC => {
                if !self.owned() {
                    self.unowned_phy.set(true);
                }
                let reg = (v >> 16) & 0x1f;
                let full = if reg > 0x0f {
                    self.page.get() as u32
                } else {
                    reg
                };
                if v & (1 << 26) != 0 {
                    if reg == IGP_PAGE_SELECT {
                        self.page.set(v as u16);
                    } else {
                        self.phy.borrow_mut().insert(full, v as u16);
                    }
                    self.set(MDIC, v | (1 << 28));
                } else {
                    self.set(MDIC, (v & !0xffff) | self.phy(full) as u32 | (1 << 28));
                }
            }
            _ => self.set(r, v),
        }
    }

    fn delay_us(&self, _: u64) {}
}

fn card() -> Card {
    let card = Card::default();
    card.set(STATUS, STATUS_GIO_MASTER_ENABLE);
    card.phy.borrow_mut().insert(MII_BMCR, BMCR_ANENABLE);
    card.phy.borrow_mut().insert(MII_ADVERTISE, 0x01e0);
    card
}

#[test]
fn the_copper_82571_82572_and_82573_are_parts_and_the_fiber_ones_are_not() {
    for id in [0x105e, 0x10a4, 0x10bc, 0x10d5, 0x107d, 0x10b9] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::E82571), "{id:#x}");
    }
    for id in [0x108b, 0x108c, 0x109a] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::E82573), "{id:#x}");
    }
    // 82571EB fiber and SerDes, 82572PI fiber and SerDes.
    for id in [0x105f, 0x1060, 0x10a5, 0x10d9, 0x10da, 0x107e, 0x107f] {
        assert_eq!(Part::of(0x8086_0000 | id), None, "{id:#x}");
    }
}

#[test]
fn an_82571_is_reset_without_a_semaphore_with_its_errata_bits_and_lplu_cleared() {
    let card = card();
    card.phy
        .borrow_mut()
        .insert(IGP02_POWER_MGMT, IGP02_PM_D0_LPLU | 1);
    card.set(TARC0, 0xf << 27);
    card.set(CTRL_EXT, CTRL_EXT_DMA_DYN_CLK_EN);
    let report = reset(&card, Part::E82571).unwrap();
    assert_eq!(
        card.reset_owned.get(),
        Some(false),
        "Linux takes no semaphore for an 82571's reset"
    );
    assert_eq!(card.reg(EECD) & (EECD_REQ | EECD_GNT), 0);
    assert!(report.quiesced && report.owned && report.nvm_read);
    assert!(report.lplu_cleared && report.autoneg);
    assert_eq!(card.phy(IGP02_POWER_MGMT), 1);
    assert_eq!(card.reg(TARC0) >> 27 & 0xf, 0);
    assert_eq!(card.reg(TARC0) & (0xf << 23), 0xf << 23);
    assert_ne!(card.reg(TARC1) & (1 << 28), 0, "no MULR: bit 28 set");
    assert_ne!(card.reg(PBA_ECC) & PBA_ECC_CORR_EN, 0);
    assert_eq!(card.reg(CTRL_EXT) & CTRL_EXT_DMA_DYN_CLK_EN, 0);
    assert_ne!(card.reg(RFCTL) & RFCTL_IPV6_EX_DIS, 0);
    assert_eq!(card.phy(MII_ADVERTISE), ADVERTISE_ALL);
    assert_eq!(card.phy(MII_CTRL1000), ADVERTISE_1000FULL);
    assert_eq!(
        card.phy(MII_BMCR) & (BMCR_ANENABLE | BMCR_ANRESTART),
        BMCR_ANENABLE | BMCR_ANRESTART
    );
    assert!(!card.unowned_phy.get());
    assert_eq!(
        card.reg(SWSM) & (SWSM_SMBI | SWSM_SWESMBI),
        0,
        "SWSM left held"
    );
}

#[test]
fn an_82573_takes_mdio_ownership_and_has_no_igp_lplu() {
    let card = card();
    let report = reset(&card, Part::E82573).unwrap();
    assert_eq!(card.reset_owned.get(), Some(true));
    assert!(!report.lplu_cleared && report.autoneg);
    assert_eq!(card.reg(SWSM), 0, "SWSM is the 82571's");
    assert_eq!(card.reg(EXTCNF_CTRL) & EXTCNF_CTRL_MDIO_SW_OWNERSHIP, 0);
    assert_ne!(card.reg(CTRL_EXT) & (1 << 22), 0);
    assert!(!card.unowned_phy.get());
}

#[test]
fn a_held_swsm_leaves_the_phy_alone_but_the_card_comes_up() {
    let card = card();
    card.swsm_held.set(true);
    let report = reset(&card, Part::E82571).unwrap();
    assert!(
        report.owned && !report.autoneg,
        "the reset needs no SWSM, the PHY does"
    );
    assert!(!card.unowned_phy.get(), "the PHY was touched without SWSM");
}

#[test]
fn the_speed_mode_bit_stays_only_at_gigabit() {
    let card = card();
    card.set(TARC0, TARC0_SPEED_MODE | 1);
    on_link_up(&card, Part::E82571, 100);
    assert_eq!(card.reg(TARC0), 1);
    on_link_up(&card, Part::E82571, 1000);
    assert_eq!(card.reg(TARC0), 1 | TARC0_SPEED_MODE);
    card.set(TARC0, 0);
    on_link_up(&card, Part::E82573, 1000);
    assert_eq!(card.reg(TARC0), 0, "not an 82573's bit");
}
