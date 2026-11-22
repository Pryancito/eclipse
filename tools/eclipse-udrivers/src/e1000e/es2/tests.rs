//! The 80003ES2LAN bring-up against a card that only pretends: registers
//! in a map, `SWSM` and `SW_FW_SYNC`, the Kumeran registers behind
//! `KMRNCTRLSTA`, and a paged GG82563 at MDIO address 1 that notes any
//! access made without this port's bit held.

use super::*;
use crate::e1000e::Part;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

const MDIC: usize = 0x0020;
const SWSM: usize = 0x5b50;

#[derive(Default)]
struct Card {
    regs: RefCell<HashMap<usize, u32>>,
    /// GG82563 registers by `page << 5 | reg`.
    phy: RefCell<HashMap<u32, u16>>,
    page: Cell<u16>,
    kmrn: RefCell<HashMap<u32, u16>>,
    /// The firmware holds this port's PHY bit.
    fw_holds_phy: Cell<bool>,
    unsynced_phy: Cell<bool>,
    unsynced_kmrn: Cell<bool>,
    reset_owned: Cell<Option<bool>>,
}

impl Card {
    fn reg(&self, r: usize) -> u32 {
        *self.regs.borrow().get(&r).unwrap_or(&0)
    }
    fn set(&self, r: usize, v: u32) {
        self.regs.borrow_mut().insert(r, v);
    }
    fn phy(&self, offset: u32) -> u16 {
        *self.phy.borrow().get(&offset).unwrap_or(&0)
    }
    fn kmrn(&self, offset: u32) -> u16 {
        *self.kmrn.borrow().get(&offset).unwrap_or(&0)
    }
    fn port_mask(&self) -> u32 {
        if self.reg(STATUS) & STATUS_FUNC_1 != 0 {
            SWFW_PHY1
        } else {
            SWFW_PHY0
        }
    }
}

impl Hw for Card {
    fn rd(&self, r: usize) -> u32 {
        let v = self.reg(r);
        match r {
            SWSM => self.set(SWSM, v | 1),
            SW_FW_SYNC if self.fw_holds_phy.get() => return v | (self.port_mask() << 16),
            _ => {}
        }
        v
    }

    fn wr(&self, r: usize, v: u32) {
        match r {
            CTRL if v & CTRL_RST != 0 => {
                self.reset_owned
                    .set(Some(self.reg(SW_FW_SYNC) & self.port_mask() != 0));
                self.set(CTRL, 0);
                self.set(EECD, EECD_AUTO_RD);
            }
            KMRNCTRLSTA => {
                if self.reg(SW_FW_SYNC) & SWFW_CSR == 0 {
                    self.unsynced_kmrn.set(true);
                }
                let offset = (v >> KMRN_OFFSET_SHIFT) & 0x1f;
                if v & KMRN_REN != 0 {
                    self.set(KMRNCTRLSTA, v & !0xffff | self.kmrn(offset) as u32);
                } else {
                    self.kmrn.borrow_mut().insert(offset, v as u16);
                    self.set(KMRNCTRLSTA, v);
                }
            }
            MDIC => {
                if self.reg(SW_FW_SYNC) & self.port_mask() == 0 {
                    self.unsynced_phy.set(true);
                }
                let reg = (v >> 16) & 0x1f;
                let addr = (v >> 21) & 0x1f;
                if addr != PHY as u32 {
                    self.set(MDIC, v | (1 << 28) | (1 << 30));
                    return;
                }
                let full = ((self.page.get() as u32) << 5) | reg;
                if v & (1 << 26) != 0 {
                    if reg == GG_PAGE_SELECT || reg == GG_PAGE_SELECT_ALT {
                        self.page.set(v as u16);
                    } else {
                        // The reset bit clears itself.
                        let d = if reg == 0 {
                            v as u16 & !BMCR_RESET
                        } else {
                            v as u16
                        };
                        self.phy.borrow_mut().insert(full, d);
                    }
                    self.set(MDIC, v | (1 << 28));
                } else {
                    let d = if reg == GG_PAGE_SELECT || reg == GG_PAGE_SELECT_ALT {
                        self.page.get()
                    } else {
                        self.phy(full)
                    };
                    self.set(MDIC, (v & !0xffff) | d as u32 | (1 << 28));
                }
            }
            _ => self.set(r, v),
        }
    }

    fn delay_us(&self, _: u64) {}
}

fn card() -> Card {
    let card = Card::default();
    card.phy.borrow_mut().insert(MII_BMCR, BMCR_ANENABLE);
    card.phy.borrow_mut().insert(MII_ADVERTISE, 0x01e0);
    card.phy
        .borrow_mut()
        .insert(GG_SPEC_CTRL, GG_PSCR_POLARITY_REVERSAL_DISABLE);
    card.phy
        .borrow_mut()
        .insert(GG_KMRN_MODE_CTRL, GG_KMCR_PASS_FALSE_CARRIER);
    card.phy
        .borrow_mut()
        .insert(GG_SPEC_CTRL_2, GG_PSCR2_REVERSE_AUTO_NEG);
    card
}

#[test]
fn the_copper_80003es2lan_is_a_part_and_the_serdes_one_is_not() {
    for id in [0x1096, 0x10ba] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::Es2), "{id:#x}");
    }
    for id in [0x1098, 0x10bb] {
        assert_eq!(Part::of(0x8086_0000 | id), None, "{id:#x}");
    }
}

#[test]
fn a_port_is_reset_under_its_phy_bit_and_its_gg82563_set_up() {
    let card = card();
    card.set(TARC0, 0xf << 27);
    let report = reset(&card).unwrap();
    assert_eq!(card.reset_owned.get(), Some(true));
    assert!(report.owned && report.nvm_read && report.phy_setup && report.autoneg);
    assert!(report.mdic_wa);
    assert!(!card.unsynced_phy.get(), "the PHY without its bit");
    assert!(!card.unsynced_kmrn.get(), "Kumeran without its bit");
    assert_eq!(card.reg(SW_FW_SYNC), 0, "a bit left held");
    assert_eq!(card.reg(SWSM) & 3, 0, "SWSM left held");

    assert_eq!(
        card.phy(GG_SPEC_CTRL) & (GG_PSCR_CROSSOVER_MASK | GG_PSCR_POLARITY_REVERSAL_DISABLE),
        GG_PSCR_CROSSOVER_AUTO
    );
    assert_eq!(
        card.phy(GG_MAC_SPEC_CTRL),
        GG_MSCR_ASSERT_CRS_ON_TX | GG_MSCR_TX_CLK_1000MBPS_25
    );
    assert_eq!(card.phy(GG_SPEC_CTRL_2) & GG_PSCR2_REVERSE_AUTO_NEG, 0);
    assert_eq!(card.phy(GG_KMRN_MODE_CTRL) & GG_KMCR_PASS_FALSE_CARRIER, 0);
    assert_ne!(
        card.phy(GG_PWR_MGMT_CTRL) & GG_PMCR_ENABLE_ELECTRICAL_IDLE,
        0
    );
    assert_ne!(card.phy(GG_INBAND_CTRL) & GG_ICR_DIS_PADDING, 0);
    assert_eq!(card.phy(MII_ADVERTISE), ADVERTISE_ALL);
    assert_eq!(card.phy(MII_CTRL1000), ADVERTISE_1000FULL);
    assert_eq!(
        card.phy(MII_BMCR) & (BMCR_ANENABLE | BMCR_ANRESTART),
        BMCR_ANENABLE | BMCR_ANRESTART
    );

    assert_eq!(card.kmrn(KMRN_TIMEOUTS), 0xffff);
    assert_eq!(
        card.kmrn(KMRN_INBAND_PARAM),
        KMRN_IBIST_DISABLE | 0x3f,
        "no far-end loopback"
    );
    assert_eq!(card.kmrn(KMRN_INB_CTRL), KMRN_INB_DIS_PADDING);
    assert_eq!(
        card.kmrn(KMRN_FIFO_CTRL),
        KMRN_FIFO_RX_BYPASS | KMRN_FIFO_TX_BYPASS
    );
    assert_eq!(card.kmrn(KMRN_MAC2PHY_OPMODE), KMRN_OPMODE_E_IDLE);

    assert_eq!(card.reg(TARC0) >> 27 & 0xf, 0);
    assert_ne!(card.reg(TARC1) & (1 << 28), 0, "no MULR: bit 28 set");
    assert_ne!(card.reg(RFCTL) & RFCTL_IPV6_EX_DIS, 0);
    assert_eq!(card.reg(TCTL_EXT) & TCTL_EXT_GCEX_MASK, TCTL_EXT_GCEX);
}

#[test]
fn the_second_port_takes_the_second_phy_bit() {
    let card = card();
    card.set(STATUS, STATUS_FUNC_1);
    let report = reset(&card).unwrap();
    assert!(report.owned && report.autoneg);
    assert!(!card.unsynced_phy.get());
}

#[test]
fn a_phy_the_firmware_keeps_is_left_alone_but_the_card_comes_up() {
    let card = card();
    card.fw_holds_phy.set(true);
    let report = reset(&card).unwrap();
    assert!(!report.owned && !report.phy_setup && !report.autoneg);
    assert!(
        !card.unsynced_phy.get(),
        "the PHY was touched without its bit"
    );
}

#[test]
fn under_iamt_the_power_and_kumeran_mode_bits_are_the_firmwares() {
    let card = card();
    card.set(FWSM, FWSM_MODE_IAMT);
    reset(&card).unwrap();
    assert_eq!(card.phy(GG_PWR_MGMT_CTRL), 0);
    assert_ne!(card.phy(GG_KMRN_MODE_CTRL) & GG_KMCR_PASS_FALSE_CARRIER, 0);
}

#[test]
fn in_band_mdio_needs_no_waits() {
    let card = card();
    card.kmrn
        .borrow_mut()
        .insert(KMRN_MAC2PHY_OPMODE, KMRN_OPMODE_INBAND_MDIO);
    assert!(!reset(&card).unwrap().mdic_wa);
}

#[test]
fn link_up_sets_kumeran_and_the_gap_for_the_speed() {
    let card = card();
    card.set(TIPG, 8 | (8 << 10) | (6 << 20));
    assert!(on_link_up(&card, 100, false));
    assert_eq!(card.kmrn(KMRN_HD_CTRL), KMRN_HD_CTRL_10_100);
    assert_eq!(card.reg(TIPG), 9 | (8 << 10) | (6 << 20));
    assert_ne!(card.phy(GG_KMRN_MODE_CTRL) & GG_KMCR_PASS_FALSE_CARRIER, 0);

    assert!(on_link_up(&card, 1000, true));
    assert_eq!(card.kmrn(KMRN_HD_CTRL), KMRN_HD_CTRL_1000);
    assert_eq!(card.reg(TIPG) & TIPG_IPGT_MASK, 8);
    assert_eq!(card.phy(GG_KMRN_MODE_CTRL) & GG_KMCR_PASS_FALSE_CARRIER, 0);
    assert!(!card.unsynced_phy.get() && !card.unsynced_kmrn.get());
    assert_eq!(card.reg(SW_FW_SYNC), 0);
}
