//! The PCH sequences against a card that only pretends: registers in a
//! map, a PHY behind `MDIC` with its pages, the SW/FW semaphore, the ME's
//! ULP handshake, the reset that clears itself and the NVM's flash
//! controller. What it checks is what costs most to get wrong on a real
//! I219 and cannot be seen in QEMU: an LPLU bit not cleared is 10 Mb/s on
//! a gigabit switch, a PHY left in ULP is no link at all.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

const I219_V: u32 = 0x8086_15b8;

/// The card.
#[derive(Default)]
struct Card {
    regs: RefCell<HashMap<usize, u32>>,
    /// The PHY: `(address, page, register)`. The IEEE registers are page 0.
    phy: RefCell<HashMap<(u8, u32, u32), u16>>,
    /// The page each address has selected.
    page: RefCell<HashMap<u8, u32>>,
    /// Every PHY write, in order.
    phy_writes: RefCell<Vec<(u8, u32, u32, u16)>>,
    /// An address nobody answers at.
    mute: Cell<Option<u8>>,
    /// The ME holds the semaphore and never lets go.
    me_holds_swflag: Cell<bool>,
    /// The ME clears `ULP_CFG_DONE` once asked.
    me_answers_ulp: Cell<bool>,
    /// The NVM, by dword.
    flash: RefCell<HashMap<u32, u32>>,
    waited_us: Cell<u64>,
    /// Whether the semaphore was held at every PHY access.
    phy_unlocked: Cell<bool>,
}

impl Card {
    fn new() -> Card {
        let card = Card::default();
        card.me_answers_ulp.set(true);
        card
    }

    fn reg(&self, reg: usize) -> u32 {
        *self.regs.borrow().get(&reg).unwrap_or(&0)
    }

    fn set(&self, reg: usize, v: u32) {
        self.regs.borrow_mut().insert(reg, v);
    }

    fn set_phy(&self, addr: u8, (page, reg): (u32, u32), v: u16) {
        self.phy.borrow_mut().insert((addr, page, reg), v);
    }

    fn phy(&self, addr: u8, (page, reg): (u32, u32)) -> u16 {
        *self.phy.borrow().get(&(addr, page, reg)).unwrap_or(&0)
    }

    /// A PHY that has just negotiated gigabit full duplex with PAUSE both
    /// ways, at the IEEE address.
    fn healthy_phy(&self) {
        self.set_phy(
            2,
            (0, MII_BMSR),
            BMSR_LSTATUS | BMSR_ANEGCOMPLETE | BMSR_ESTATEN,
        );
        self.set_phy(2, (0, MII_ESTATUS), ESTATUS_1000_TFULL);
        self.set_phy(2, (0, MII_ADVERTISE), ADVERTISE_ALL);
        self.set_phy(2, (0, MII_LPA), 0x01e0 | (1 << 10));
        self.set_phy(2, (0, MII_CTRL1000), ADVERTISE_1000FULL);
        self.set_phy(2, (0, MII_STAT1000), LPA_1000FULL);
    }

    fn mdio(&self, cmd: u32) {
        let reg = (cmd >> MDIC_REG_SHIFT) & 0x1f;
        let addr = ((cmd >> MDIC_PHY_SHIFT) & 0x1f) as u8;
        let op = cmd & (3 << 26);
        if self.mute.get() == Some(addr) {
            self.set(MDIC, cmd & !MDIC_READY);
            return;
        }
        if self.reg(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG == 0 {
            self.phy_unlocked.set(true);
        }
        let page = if reg <= MULTI_PAGE_REG_MAX {
            0
        } else {
            *self.page.borrow().get(&addr).unwrap_or(&0)
        };
        // Page 0 at address 1 is page 768 (`hv_page`).
        let page = if addr == HV_PHY && page == 0 && reg > MULTI_PAGE_REG_MAX {
            HV_INTC_FC_PAGE
        } else {
            page
        };
        if op == MDIC_OP_WRITE {
            let v = cmd as u16;
            if reg == PHY_PAGE_SELECT {
                self.page
                    .borrow_mut()
                    .insert(addr, (v >> PHY_PAGE_SHIFT) as u32);
            } else {
                self.phy.borrow_mut().insert((addr, page, reg), v);
                self.phy_writes.borrow_mut().push((addr, page, reg, v));
            }
            self.set(MDIC, cmd | MDIC_READY);
        } else {
            let v = self.phy(addr, (page, reg));
            self.set(MDIC, (cmd & !0xffff) | v as u32 | MDIC_READY);
        }
    }
}

impl Hw for Card {
    fn rd(&self, reg: usize) -> u32 {
        let v = self.reg(reg);
        match reg {
            // Out of ULP as soon as the ME is asked.
            FWSM if self.reg(H2ME) & H2ME_ENFORCE_SETTINGS != 0 && self.me_answers_ulp.get() => {
                v & !FWSM_ULP_CFG_DONE
            }
            _ => v,
        }
    }

    fn wr(&self, reg: usize, v: u32) {
        match reg {
            // The reset is over at once, and clears CTRL.
            CTRL if v & CTRL_RST != 0 => self.set(CTRL, 0),
            CTRL if v & CTRL_GIO_MASTER_DISABLE != 0 => {
                self.set(CTRL, v);
                self.set(STATUS, self.reg(STATUS) & !STATUS_GIO_MASTER_ENABLE);
            }
            EXTCNF_CTRL if self.me_holds_swflag.get() => {}
            MDIC => self.mdio(v),
            HSFSTS => {
                // The status half: write one to clear.
                let hs = self.reg(HSFSTS);
                let status = (hs & 0xffff) & !(v & (HSFSTS_FLCDONE | HSFSTS_FLCERR | HSFSTS_DAEL));
                let ctl = v >> 16;
                if ctl & 1 != 0 {
                    // Go: a read of the dword at FADDR.
                    let at = self.reg(FADDR);
                    let d = *self.flash.borrow().get(&at).unwrap_or(&0xffff_ffff);
                    self.set(FDATA0, d);
                    self.set(HSFSTS, status | HSFSTS_FLCDONE);
                } else {
                    self.set(HSFSTS, status | (ctl << 16));
                }
            }
            _ => self.set(reg, v),
        }
    }

    fn delay_us(&self, us: u64) {
        self.waited_us.set(self.waited_us.get() + us);
    }
}

fn ready_card() -> Card {
    let card = Card::new();
    card.set(STATUS, STATUS_GIO_MASTER_ENABLE);
    card.set(
        EXTCNF_CTRL,
        if card.me_holds_swflag.get() {
            EXTCNF_CTRL_SWFLAG
        } else {
            0
        },
    );
    card.healthy_phy();
    card
}

#[test]
fn every_i219_and_pch_id_the_kernel_takes_is_a_part_here() {
    assert_eq!(Part::of(I219_V), Some(Part::PchSpt));
    for id in [
        0x156f, 0x1570, 0x15b7, 0x15d8, 0x15e3, 0x0d4f, 0x15fc, 0x1a1f, 0x550b, 0x57ba,
    ] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::PchSpt), "{id:#x}");
    }
    for id in [0x1502, 0x1503, 0x153a, 0x153b, 0x155a, 0x1559, 0x15a3] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::Pch), "{id:#x}");
    }
    for id in [0x10d3, 0x10f6, 0x150c] {
        assert_eq!(Part::of(0x8086_0000 | id), Some(Part::Discrete), "{id:#x}");
    }
    // A kernel that does not say only ever left the 82574.
    assert_eq!(Part::of(0), Some(Part::Discrete));
    // An I210 (igb), and the right device from another vendor.
    assert_eq!(Part::of(0x8086_1533), None);
    assert_eq!(Part::of(0x10ec_15b8), None);
}

#[test]
fn an_i219_comes_out_of_reset_with_lplu_and_gigabit_disable_cleared_in_both_copies() {
    let card = ready_card();
    // Left by the ME: LPLU in D0 and gigabit off, in the MAC and the PHY.
    card.set(
        PHY_CTRL,
        PHY_CTRL_D0A_LPLU | PHY_CTRL_GBE_DISABLE | PHY_CTRL_NOND0A_LPLU,
    );
    card.set(FEXTNVM, FEXTNVM_SW_CONFIG_ICH8M);
    card.set_phy(HV_PHY, HV_OEM_BITS, HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS);
    let report = reset(&card, Part::PchSpt).unwrap();
    assert_eq!(
        card.reg(PHY_CTRL) & 0x4e,
        0,
        "PHY_CTRL {:#x}",
        card.reg(PHY_CTRL)
    );
    let oem = card.phy(HV_PHY, HV_OEM_BITS);
    assert_eq!(oem & (HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS), 0);
    assert_ne!(oem & HV_OEM_BITS_RESTART_AN, 0);
    assert_eq!(
        report.oem_bits,
        Some((
            HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS,
            HV_OEM_BITS_RESTART_AN
        ))
    );
    assert!(
        !card.phy_unlocked.get(),
        "a PHY access without the semaphore"
    );
    assert_eq!(
        card.reg(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG,
        0,
        "the semaphore left held"
    );
}

#[test]
fn with_the_nvm_gate_closed_lplu_is_still_cleared_and_nothing_else_touched() {
    let card = ready_card();
    card.set_phy(HV_PHY, HV_OEM_BITS, HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS);
    reset(&card, Part::PchSpt).unwrap();
    assert_eq!(
        card.phy(HV_PHY, HV_OEM_BITS),
        HV_OEM_BITS_GBE_DIS | HV_OEM_BITS_RESTART_AN
    );
}

#[test]
fn the_me_is_asked_to_take_the_phy_out_of_ulp_and_its_answer_waited_for() {
    let card = ready_card();
    card.set(FWSM, FWSM_FW_VALID | FWSM_ULP_CFG_DONE);
    card.set(H2ME, H2ME_ULP);
    let report = reset(&card, Part::PchSpt).unwrap();
    assert_eq!(report.ulp, Ulp::Firmware { done: true });
    // ULP asked off, the enforcement let go after.
    assert_eq!(card.reg(H2ME) & (H2ME_ULP | H2ME_ENFORCE_SETTINGS), 0);
    // Firmware's path is register writes only: no ULP page written.
    assert!(!card
        .phy_writes
        .borrow()
        .iter()
        .any(|w| (w.1, w.2) == ULP_CONFIG1));
}

#[test]
fn an_me_that_does_not_answer_costs_400_ms_and_the_bring_up_goes_on() {
    let card = ready_card();
    card.me_answers_ulp.set(false);
    card.set(FWSM, FWSM_FW_VALID | FWSM_ULP_CFG_DONE);
    let before = card.waited_us.get();
    let report = reset(&card, Part::PchSpt).unwrap();
    assert_eq!(report.ulp, Ulp::Firmware { done: false });
    assert!(card.waited_us.get() - before >= 400_000);
    assert!(report.autoneg.is_some());
}

#[test]
fn with_no_me_ulp_is_undone_in_the_phy() {
    let card = ready_card();
    card.set(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
    card.set(FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
    card.set_phy(HV_PHY, CV_SMB_CTRL, CV_SMB_CTRL_FORCE_SMBUS);
    card.set_phy(
        HV_PHY,
        ULP_CONFIG1,
        ULP_CONFIG1_IND | ULP_CONFIG1_STICKY_ULP | ULP_CONFIG1_WOL_HOST,
    );
    let report = reset(&card, Part::PchSpt).unwrap();
    assert_eq!(report.ulp, Ulp::Software);
    assert_eq!(card.phy(HV_PHY, CV_SMB_CTRL) & CV_SMB_CTRL_FORCE_SMBUS, 0);
    assert_ne!(card.phy(HV_PHY, HV_PM_CTRL) & HV_PM_CTRL_K1_ENABLE, 0);
    assert_eq!(card.phy(HV_PHY, ULP_CONFIG1), ULP_CONFIG1_START);
    assert_eq!(card.reg(FEXTNVM7) & FEXTNVM7_DISABLE_SMB_PERST, 0);
    assert_eq!(card.reg(CTRL_EXT) & CTRL_EXT_FORCE_SMBUS, 0);
    assert!(!card.phy_unlocked.get());
}

#[test]
fn an_i217_has_no_ulp_and_no_queues_to_enable() {
    let card = ready_card();
    card.set(FWSM, FWSM_FW_VALID | FWSM_ULP_CFG_DONE);
    let report = reset(&card, Part::Pch).unwrap();
    assert_eq!(report.ulp, Ulp::NotThere);
    assert_eq!(card.reg(H2ME), 0, "the ME was asked on a part without ULP");
    // FEXTNVM6/7 are Sunrise Point's workarounds.
    assert_eq!(card.reg(FEXTNVM7) & FEXTNVM7_SIDE_CLK_UNGATE, 0);
}

#[test]
fn the_mac_ends_up_linked_auto_speed_with_flow_control_and_k1_on() {
    let card = ready_card();
    card.set(CTRL, CTRL_FRCSPD | CTRL_FRCDPX);
    let report = reset(&card, Part::PchSpt).unwrap();
    let ctrl = card.reg(CTRL);
    assert_eq!(ctrl & (CTRL_FRCSPD | CTRL_FRCDPX), 0, "CTRL {ctrl:#x}");
    assert_ne!(ctrl & super::super::CTRL_SLU, 0);
    assert_ne!(ctrl & super::super::CTRL_ASDE, 0);
    // K1's dance put CTRL and CTRL_EXT back as they were.
    assert_eq!(card.reg(CTRL_EXT) & CTRL_EXT_SPD_BYPS, 0);
    assert!(report.k1);
    assert_ne!(card.reg(KMRNCTRLSTA) as u16 & KMRN_K1_ENABLE, 0);
    assert_eq!(
        card.reg(super::super::CTRL_EXT) & super::super::CTRL_EXT_IAME,
        0
    );
    assert_ne!(card.reg(CTRL_EXT) & super::super::CTRL_EXT_DRV_LOAD, 0);
    assert_eq!(
        (card.reg(PBA), card.reg(FCRTH), card.reg(FCRTL)),
        (PBA_KB, FC_HIGH_WATER, FC_LOW_WATER | FCRTL_XONE)
    );
    assert_eq!(card.phy(HV_PHY, BM_PORT_PAUSE_TIME), 0xffff);
    assert_eq!(card.reg(FEXTNVM6) & FEXTNVM6_K1_OFF_ENABLE, 0);
    assert!(report.quiesced);
}

#[test]
fn autoneg_is_restarted_at_the_ieee_address_with_gigabit_and_pause_offered() {
    let card = ready_card();
    // A previous OS took gigabit and PAUSE away.
    card.set_phy(2, (0, MII_ADVERTISE), 0x01e0);
    card.set_phy(2, (0, MII_CTRL1000), 0);
    let report = reset(&card, Part::PchSpt).unwrap();
    assert_eq!(report.autoneg, Some(2));
    assert_eq!(card.phy(2, (0, MII_ADVERTISE)), ADVERTISE_ALL);
    assert_eq!(card.phy(2, (0, MII_CTRL1000)), ADVERTISE_1000FULL);
    let bmcr = card.phy(2, (0, MII_BMCR));
    assert_eq!(
        bmcr & (BMCR_ANENABLE | BMCR_ANRESTART),
        BMCR_ANENABLE | BMCR_ANRESTART
    );
    assert_eq!(
        card.phy(1, (0, MII_BMCR)),
        0,
        "address 1 is not the IEEE PHY"
    );
}

#[test]
fn a_phy_that_does_not_answer_at_2_is_tried_at_1() {
    let card = ready_card();
    card.mute.set(Some(2));
    card.set_phy(1, (0, MII_BMCR), BMCR_ANENABLE);
    let report = reset(&card, Part::Pch).unwrap();
    assert_eq!(report.autoneg, Some(1));
}

#[test]
fn an_me_that_keeps_the_semaphore_leaves_the_phy_alone_but_the_card_comes_up() {
    let card = Card::new();
    card.set(EXTCNF_CTRL, EXTCNF_CTRL_SWFLAG);
    card.me_holds_swflag.set(true);
    card.set_phy(HV_PHY, HV_OEM_BITS, HV_OEM_BITS_LPLU);
    let report = reset(&card, Part::PchSpt).unwrap();
    assert!(!report.k1);
    assert_eq!(report.autoneg, None);
    assert!(card.phy_writes.borrow().is_empty());
}

#[test]
fn a_reset_that_never_ends_is_an_error() {
    struct Stuck(Card);
    impl Hw for Stuck {
        fn rd(&self, reg: usize) -> u32 {
            self.0.rd(reg)
        }
        fn wr(&self, reg: usize, v: u32) {
            // CTRL keeps whatever is written, RST too.
            if reg == CTRL {
                self.0.set(CTRL, v);
            } else {
                self.0.wr(reg, v)
            }
        }
        fn delay_us(&self, us: u64) {
            self.0.delay_us(us)
        }
    }
    let card = Stuck(ready_card());
    assert!(reset(&card, Part::PchSpt).is_err());
}

#[test]
fn the_address_comes_from_the_valid_bank_of_the_nvm() {
    let card = Card::new();
    card.set(HSFSTS, HSFSTS_FLDESVALID);
    // An 8 KiB NVM: banks of 2048 words. Bank 0 unsigned, bank 1 signed.
    card.set(STRAP, 1 << 1);
    let bank = 2048u32;
    card.flash.borrow_mut().insert(0x13 << 1, 0);
    card.flash
        .borrow_mut()
        .insert((bank + 0x13) << 1, 0x80 << 8);
    card.flash
        .borrow_mut()
        .insert(bank << 1, u32::from_le_bytes([0x00, 0x1b, 0x21, 0xaa]));
    card.flash.borrow_mut().insert(
        (bank + 2) << 1,
        u32::from_le_bytes([0xbb, 0xcc, 0xff, 0xff]),
    );
    assert_eq!(
        mac_from_flash(&card, Part::PchSpt),
        Some([0x00, 0x1b, 0x21, 0xaa, 0xbb, 0xcc])
    );
    // Before Sunrise Point the flash is behind BAR 1.
    assert_eq!(mac_from_flash(&card, Part::Pch), None);
}

#[test]
fn a_flash_with_no_valid_descriptor_gives_no_address() {
    let card = Card::new();
    assert_eq!(mac_from_flash(&card, Part::PchSpt), None);
}

#[test]
fn hv_page_768_is_selected_as_page_0() {
    assert_eq!(hv_page(768), 0);
    assert_eq!(hv_page(769), 769 << 5);
    assert_eq!(hv_page(779), 779 << 5);
}

#[test]
fn link_up_at_gigabit_resolves_full_pause_and_the_normal_gap() {
    let card = ready_card();
    card.set(TIPG, 8 | (8 << 10) | (6 << 20));
    let status = 1 | (2 << 6) | 2; // full duplex, 1000, link
    let link = on_link_up(&card, Part::PchSpt, status);
    assert_eq!((link.speed, link.full_duplex), (1000, true));
    assert_eq!(link.pause, Some(Pause::Full));
    assert_eq!(link.phy_says, None);
    assert_eq!(
        card.reg(CTRL) & (CTRL_RFCE | CTRL_TFCE),
        CTRL_RFCE | CTRL_TFCE
    );
    assert_eq!(card.reg(TIPG) & 0x3ff, 8);
    assert_eq!(
        card.reg(FEXTNVM4) & FEXTNVM4_BEACON_DURATION_MASK,
        FEXTNVM4_BEACON_DURATION_8USEC
    );
    assert_eq!(card.reg(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG, 0);
}

#[test]
fn link_up_at_100_on_an_i219_widens_the_gap_and_says_when_the_phy_disagrees() {
    let card = ready_card();
    let status = 1 | (1 << 6) | 2; // full duplex, 100
    let link = on_link_up(&card, Part::PchSpt, status);
    assert_eq!(card.reg(TIPG) & 0x3ff, 0x0c);
    // The PHY negotiated gigabit: the MAC-PHY interconnect is at fault.
    assert_eq!(link.phy_says, Some(Some((1000, true))));
    // An I217 keeps the normal gap at 100.
    on_link_up(&card, Part::Pch, status);
    assert_eq!(card.reg(TIPG) & 0x3ff, 8);
}

#[test]
fn half_duplex_has_no_pause_and_10_half_the_widest_gap() {
    let card = ready_card();
    card.set(CTRL, CTRL_RFCE | CTRL_TFCE);
    let link = on_link_up(&card, Part::PchSpt, 2); // 10 Mb/s, half duplex
    assert_eq!(link.pause, Some(Pause::None));
    assert_eq!(card.reg(CTRL) & (CTRL_RFCE | CTRL_TFCE), 0);
    assert_eq!(card.reg(TIPG) & 0x3ff, 0xff);
}

#[test]
fn the_pause_table_is_ieee_annex_28b() {
    let (c, a) = (ADVERTISE_PAUSE_CAP, ADVERTISE_PAUSE_ASYM);
    assert_eq!(pause_for(c | a, c, true), Pause::Full);
    assert_eq!(pause_for(c | a, c | a, true), Pause::Full);
    assert_eq!(pause_for(a, c | a, true), Pause::Tx);
    assert_eq!(pause_for(c | a, a, true), Pause::Rx);
    assert_eq!(pause_for(c | a, 0, true), Pause::None);
    assert_eq!(pause_for(c, a, true), Pause::None);
}

#[test]
fn the_first_pch_ids_are_its_two_phys() {
    assert_eq!(Part::of(0x8086_10ea), Some(Part::Pch82577));
    assert_eq!(Part::of(0x8086_10eb), Some(Part::Pch82577));
    assert_eq!(Part::of(0x8086_10ef), Some(Part::Pch82578));
    assert_eq!(Part::of(0x8086_10f0), Some(Part::Pch82578));
}

#[test]
fn an_82577_of_revision_1_gets_slow_mdio_first_and_every_workaround() {
    let card = ready_card();
    card.set_phy(2, (0, MII_PHYSID2), 0x15a1);
    card.set_phy(HV_PHY, BM_PORT_GEN_CFG, 0xab12);
    let report = reset(&card, Part::Pch82577).unwrap();
    let writes = card.phy_writes.borrow();
    assert_eq!(
        (writes[0].1, writes[0].2),
        HV_KMRN_MODE_CTRL,
        "slow mode is not the first PHY write: {:x?}",
        &writes[..3]
    );
    assert_ne!(card.phy(HV_PHY, HV_KMRN_MODE_CTRL) & HV_KMRN_MDIO_SLOW, 0);
    assert_eq!(card.phy(HV_PHY, HV_EARLY_PREAMBLE), 0x4431);
    assert_eq!(card.phy(HV_PHY, HV_KMRN_FIFO_CTRLSTA), 0xa204);
    assert_eq!(card.phy(HV_PHY, HV_LINK_STALL), HV_LINK_STALL_UP);
    assert_eq!(card.phy(HV_PHY, BM_PORT_GEN_CFG), 0x0012);
    assert_eq!(card.phy(2, (0, EMI_ADDR)), I82577_MSE_THRESHOLD);
    assert_eq!(card.phy(2, (0, EMI_DATA)), 0x0034);
    assert_eq!(
        report.pch1,
        Some(Pch1 {
            revision: Some(1),
            preamble: true,
            soft_reset: false
        })
    );
    drop(writes);
    // Its own flow control.
    assert_eq!(
        (
            card.reg(FCRTH),
            card.reg(FCRTL),
            card.reg(FCTTV),
            card.reg(FCRTV_PCH)
        ),
        (
            PCH1_HIGH_WATER,
            PCH1_LOW_WATER | FCRTL_XONE,
            PCH1_PAUSE_TIME,
            PCH1_REFRESH_TIME
        )
    );
    assert!(!card.phy_unlocked.get());
}

#[test]
fn an_82578_of_revision_1_is_reset_to_its_defaults_and_gets_no_slow_mode() {
    let card = ready_card();
    card.set_phy(2, (0, MII_PHYSID2), 0x15a1);
    let report = reset(&card, Part::Pch82578).unwrap();
    assert!(report.pch1.unwrap().soft_reset);
    assert!(card
        .phy_writes
        .borrow()
        .iter()
        .any(|w| (w.0, w.2, w.3) == (2, MII_BMCR, 0x3140)));
    assert_eq!(card.phy(HV_PHY, HV_KMRN_MODE_CTRL), 0);
}

#[test]
fn a_later_revision_gets_no_preamble_tuning_and_other_parts_none_of_it() {
    let card = ready_card();
    card.set_phy(2, (0, MII_PHYSID2), 0x15a3);
    let report = reset(&card, Part::Pch82577).unwrap();
    assert!(!report.pch1.unwrap().preamble);
    assert_eq!(card.phy(HV_PHY, HV_EARLY_PREAMBLE), 0);
    let card = ready_card();
    let report = reset(&card, Part::Pch).unwrap();
    assert_eq!(report.pch1, None);
    assert_eq!(card.phy(HV_PHY, HV_LINK_STALL), 0);
    assert_eq!(card.reg(FCRTH), FC_HIGH_WATER);
}

#[test]
fn on_the_first_pch_k1_is_off_at_gigabit_and_back_on_below_or_down() {
    let card = ready_card();
    let k1 = |card: &Card| card.reg(KMRNCTRLSTA) as u16 & KMRN_K1_ENABLE != 0;
    pch1_link_changed(&card, Part::Pch82577, Some(1000));
    assert!(!k1(&card));
    assert_eq!(card.phy(HV_PHY, HV_LINK_STALL), HV_LINK_STALL_UP);
    pch1_link_changed(&card, Part::Pch82577, None);
    assert!(k1(&card));
    assert_eq!(card.phy(HV_PHY, HV_LINK_STALL), HV_LINK_STALL_DOWN);
    pch1_link_changed(&card, Part::Pch82578, Some(100));
    assert!(k1(&card));
    // Not the first PCH: nothing.
    card.set(KMRNCTRLSTA, 0);
    pch1_link_changed(&card, Part::PchSpt, Some(1000));
    assert_eq!(card.reg(KMRNCTRLSTA), 0);
}
