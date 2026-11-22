//! The 80003ES2LAN (Linux `80003es2lan.c`), the MAC in the 631xESB and
//! 632xESB south bridges: two ports, each with a GG82563 PHY at MDIO
//! address 1 reached across the Kumeran interface. Copper only: the SerDes
//! versions have no PHY to bring up, and are not taken.
//!
//! The PHY, the Kumeran registers and the NVM are shared with the other
//! port's driver and the management firmware through `SW_FW_SYNC`, one bit
//! per resource for software and the same one 16 up for the firmware, held
//! under the hardware semaphore `SWSM` only while it is changed. Every PHY
//! and Kumeran access takes its bit, as Linux's does.
//!
//! What is done is Linux's `e1000_reset_hw_80003es2lan` and
//! `e1000_init_hw_80003es2lan`: the bus quiesced, the MAC reset with the
//! port's PHY bit held, far-end loopback off, the NVM's automatic read
//! waited for, the bits of `e1000_initialize_hw_bits_80003es2lan`, the
//! Kumeran set-up and the GG82563's (`e1000_copper_link_setup_gg82563`,
//! automatic crossover, then a PHY reset to apply it), auto-negotiation
//! restarted with everything offered, and gigabit carry extension. On each
//! link up the Kumeran half-duplex control, the inter-packet gap and the
//! false-carrier bit are set for the speed (`e1000_cfg_on_link_up`).

use super::legacy::{swsm_put, swsm_take};
use super::pch::{
    self, mdic_read, mdic_write, Hw, ADVERTISE_1000FULL, ADVERTISE_ALL, BMCR_ANENABLE,
    BMCR_ANRESTART, MII_ADVERTISE, MII_BMCR, MII_CTRL1000,
};

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EECD: usize = 0x0010;
const CTRL_EXT: usize = 0x0018;
const KMRNCTRLSTA: usize = 0x0034;
const TCTL_EXT: usize = 0x0404;
const TIPG: usize = 0x0410;
const TXDCTL: usize = 0x3828;
const TXDCTL1: usize = 0x3928;
const TARC0: usize = 0x3840;
pub(super) const TARC1: usize = 0x3940;
const TCTL: usize = 0x0400;
const RFCTL: usize = 0x5008;
const FWSM: usize = 0x5b54;
const SW_FW_SYNC: usize = 0x5b5c;
/// The second entry of the flexible filter length table.
const FFLT1: usize = 0x5f08;

const CTRL_RST: u32 = 1 << 26;
const STATUS_FUNC_1: u32 = 1 << 2;
const EECD_AUTO_RD: u32 = 1 << 9;
const CTRL_EXT_LINK_MODE_MASK: u32 = 3 << 22;
const TCTL_MULR: u32 = 1 << 28;
const TCTL_EXT_GCEX_MASK: u32 = 0x000f_fc00;
const TCTL_EXT_GCEX: u32 = 0x0001_0000;
const TIPG_IPGT_MASK: u32 = 0x3ff;
const TIPG_IPGT_1000: u32 = 8;
const TIPG_IPGT_10_100: u32 = 9;
const RFCTL_IPV6_EX_DIS: u32 = 1 << 16;
const RFCTL_NEW_IPV6_EXT_DIS: u32 = 1 << 17;
/// `FWSM`'s mode field, and its value when the management engine runs
/// IAMT (Linux `e1000e_check_mng_mode_generic`).
const FWSM_MODE_MASK: u32 = 0xe;
const FWSM_MODE_IAMT: u32 = 3 << 1;

/// `SW_FW_SYNC` bits: the Kumeran registers, and each port's PHY.
const SWFW_PHY0: u32 = 0x2;
const SWFW_PHY1: u32 = 0x4;
const SWFW_CSR: u32 = 0x8;

/// Kumeran registers, and what goes in them.
const KMRN_OFFSET_SHIFT: u32 = 16;
const KMRN_REN: u32 = 1 << 21;
const KMRN_FIFO_CTRL: u32 = 0x00;
const KMRN_FIFO_RX_BYPASS: u16 = 0x0008;
const KMRN_FIFO_TX_BYPASS: u16 = 0x0800;
const KMRN_INB_CTRL: u32 = 0x02;
const KMRN_INB_DIS_PADDING: u16 = 0x0010;
const KMRN_TIMEOUTS: u32 = 0x04;
const KMRN_INBAND_PARAM: u32 = 0x09;
const KMRN_IBIST_DISABLE: u16 = 0x0200;
const KMRN_HD_CTRL: u32 = 0x10;
const KMRN_HD_CTRL_10_100: u16 = 0x0004;
const KMRN_HD_CTRL_1000: u16 = 0x0000;
const KMRN_MAC2PHY_OPMODE: u32 = 0x1f;
const KMRN_OPMODE_E_IDLE: u16 = 0x2000;
const KMRN_OPMODE_MASK: u16 = 0x000c;
const KMRN_OPMODE_INBAND_MDIO: u16 = 0x0004;

/// The PHY's MDIO address.
const PHY: u8 = 1;

/// GG82563 registers, as `page << 5 | reg`.
const fn gg(page: u32, reg: u32) -> u32 {
    (page << 5) | (reg & 0x1f)
}
const GG_SPEC_CTRL: u32 = gg(0, 16);
const GG_PAGE_SELECT: u32 = 22;
const GG_SPEC_CTRL_2: u32 = gg(0, 26);
const GG_PAGE_SELECT_ALT: u32 = 29;
const GG_MAC_SPEC_CTRL: u32 = gg(2, 21);
const GG_KMRN_MODE_CTRL: u32 = gg(193, 16);
const GG_PWR_MGMT_CTRL: u32 = gg(193, 20);
const GG_INBAND_CTRL: u32 = gg(194, 18);
/// Registers from here up are paged by the alternative page select.
const GG_MIN_ALT_REG: u32 = 30;

const GG_PSCR_POLARITY_REVERSAL_DISABLE: u16 = 0x0002;
const GG_PSCR_CROSSOVER_MASK: u16 = 0x0060;
const GG_PSCR_CROSSOVER_AUTO: u16 = 0x0060;
const GG_PSCR2_REVERSE_AUTO_NEG: u16 = 0x2000;
const GG_MSCR_ASSERT_CRS_ON_TX: u16 = 0x0010;
const GG_MSCR_TX_CLK_1000MBPS_25: u16 = 0x0007;
const GG_KMCR_PASS_FALSE_CARRIER: u16 = 0x0800;
const GG_PMCR_ENABLE_ELECTRICAL_IDLE: u16 = 0x0001;
const GG_ICR_DIS_PADDING: u16 = 0x0010;
const GG_MAX_KMRN_RETRY: u32 = 5;
const BMCR_RESET: u16 = 1 << 15;

/// A bit of `SW_FW_SYNC`, held until dropped (Linux
/// `e1000_acquire_swfw_sync_80003es2lan`): 50 tries 5 ms apart while the
/// firmware or the other port has it.
struct Sync<'a, H: Hw> {
    hw: &'a H,
    mask: u32,
}

impl<'a, H: Hw> Sync<'a, H> {
    fn take(hw: &'a H, mask: u32) -> Option<Self> {
        for _ in 0..50 {
            if !swsm_take(hw) {
                return None;
            }
            let sync = hw.rd(SW_FW_SYNC);
            if sync & (mask | mask << 16) == 0 {
                hw.wr(SW_FW_SYNC, sync | mask);
                swsm_put(hw);
                return Some(Sync { hw, mask });
            }
            swsm_put(hw);
            hw.delay_us(5_000);
        }
        None
    }
}

impl<H: Hw> Drop for Sync<'_, H> {
    fn drop(&mut self) {
        let hw = self.hw;
        // Linux waits for SWSM without bound here; a bound, and the bit
        // let go of anyway, is kinder to a card that never answers.
        let had = swsm_take(hw);
        hw.wr(SW_FW_SYNC, hw.rd(SW_FW_SYNC) & !self.mask);
        if had {
            swsm_put(hw);
        }
    }
}

/// The card's two ways to its PHY and to the Kumeran interface, each under
/// its `SW_FW_SYNC` bit.
struct Es2<'a, H: Hw> {
    hw: &'a H,
    /// This port's PHY bit.
    phy_mask: u32,
    /// Whether every MDIO command is waited on and its page checked (Linux
    /// `mdic_wa_enable`: the ready bit can come before the page select is
    /// done), which is all but in in-band MDIO mode.
    mdic_wa: bool,
}

impl<'a, H: Hw> Es2<'a, H> {
    fn new(hw: &'a H) -> Self {
        let phy_mask = if hw.rd(STATUS) & STATUS_FUNC_1 != 0 {
            SWFW_PHY1
        } else {
            SWFW_PHY0
        };
        Es2 {
            hw,
            phy_mask,
            mdic_wa: true,
        }
    }

    fn kmrn_read(&self, offset: u32) -> Option<u16> {
        let hw = self.hw;
        let _sync = Sync::take(hw, SWFW_CSR)?;
        hw.wr(KMRNCTRLSTA, (offset << KMRN_OFFSET_SHIFT) | KMRN_REN);
        pch::flush(hw);
        hw.delay_us(2);
        Some(hw.rd(KMRNCTRLSTA) as u16)
    }

    fn kmrn_write(&self, offset: u32, data: u16) -> bool {
        let hw = self.hw;
        let Some(_sync) = Sync::take(hw, SWFW_CSR) else {
            return false;
        };
        hw.wr(KMRNCTRLSTA, (offset << KMRN_OFFSET_SHIFT) | data as u32);
        pch::flush(hw);
        hw.delay_us(2);
        true
    }

    fn kmrn_or(&self, offset: u32, bits: u16) -> bool {
        self.kmrn_read(offset)
            .is_some_and(|v| self.kmrn_write(offset, v | bits))
    }

    /// Select the page of GG82563 register `offset`, under the PHY's bit.
    fn page(&self, offset: u32) -> Option<Sync<'a, H>> {
        let hw = self.hw;
        let sync = Sync::take(hw, self.phy_mask)?;
        let select = if offset & 0x1f < GG_MIN_ALT_REG {
            GG_PAGE_SELECT
        } else {
            GG_PAGE_SELECT_ALT
        };
        let page = (offset >> 5) as u16;
        if !mdic_write(hw, PHY, select, page) {
            return None;
        }
        if self.mdic_wa {
            hw.delay_us(200);
            if mdic_read(hw, PHY, select) != Some(page) {
                return None;
            }
            hw.delay_us(200);
        }
        Some(sync)
    }

    fn read(&self, offset: u32) -> Option<u16> {
        let _sync = self.page(offset)?;
        let v = mdic_read(self.hw, PHY, offset & 0x1f);
        if self.mdic_wa {
            self.hw.delay_us(200);
        }
        v.filter(|&v| v != 0xffff)
    }

    fn write(&self, offset: u32, data: u16) -> bool {
        let Some(_sync) = self.page(offset) else {
            return false;
        };
        let done = mdic_write(self.hw, PHY, offset & 0x1f, data);
        if self.mdic_wa {
            self.hw.delay_us(200);
        }
        done
    }

    fn update(&self, offset: u32, f: impl FnOnce(u16) -> u16) -> bool {
        self.read(offset).is_some_and(|v| self.write(offset, f(v)))
    }

    /// The Kumeran mode control register, read until two reads agree
    /// (Linux reads it so, up to five times).
    fn kmrn_mode_ctrl(&self) -> Option<u16> {
        let mut v = self.read(GG_KMRN_MODE_CTRL)?;
        for _ in 1..GG_MAX_KMRN_RETRY {
            let again = self.read(GG_KMRN_MODE_CTRL)?;
            if again == v {
                break;
            }
            v = again;
        }
        Some(v)
    }
}

/// What [`reset`] did that is worth a line in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub quiesced: bool,
    /// Whether this port's PHY bit was had for the reset.
    pub owned: bool,
    /// Whether the NVM said its automatic read was done.
    pub nvm_read: bool,
    /// Whether the GG82563 took its set-up.
    pub phy_setup: bool,
    /// Whether auto-negotiation was restarted.
    pub autoneg: bool,
    /// Whether the MDIO commands are waited on (all but in-band MDIO).
    pub mdic_wa: bool,
}

/// Reset an 80003ES2LAN port and set it up, up to where the rings are
/// programmed. Interrupts masked, receive and transmit off.
pub fn reset<H: Hw>(hw: &H) -> Result<Report, String> {
    let quiesced = pch::disable_pcie_master(hw);
    hw.wr(super::IMC, u32::MAX);
    hw.wr(super::RCTL, 0);
    hw.wr(TCTL, super::TCTL_PSP);
    pch::flush(hw);
    hw.delay_us(10_000);

    let mut es2 = Es2::new(hw);
    let sync = Sync::take(hw, es2.phy_mask);
    let owned = sync.is_some();
    hw.wr(CTRL, hw.rd(CTRL) | CTRL_RST);
    drop(sync);
    let mut out = false;
    for _ in 0..1000 {
        hw.delay_us(1_000);
        if hw.rd(CTRL) & CTRL_RST == 0 {
            out = true;
            break;
        }
    }
    if !out {
        return Err("the card did not come out of its reset".into());
    }
    // No far-end loopback.
    es2.kmrn_or(KMRN_INBAND_PARAM, KMRN_IBIST_DISABLE);
    let nvm_read = (0..10).any(|_| {
        let done = hw.rd(EECD) & EECD_AUTO_RD != 0;
        if !done {
            hw.delay_us(1_000);
        }
        done
    });
    if !nvm_read {
        // Linux stops here: the MAC is not to be touched further.
        return Err("the NVM's automatic read did not finish".into());
    }
    hw.wr(super::IMC, u32::MAX);
    let _ = hw.rd(super::ICR);

    let ext = (hw.rd(CTRL_EXT) | super::CTRL_EXT_RO_DIS | super::CTRL_EXT_DRV_LOAD)
        & !super::CTRL_EXT_IAME;
    hw.wr(CTRL_EXT, ext);
    hw.wr(super::IAM, 0);
    hw_bits(hw);

    // In-band MDIO needs no waiting on (Linux sets this after the link
    // set-up, which on a first start runs without the waits; with them
    // from the start is the safe side).
    es2.mdic_wa = es2
        .kmrn_read(KMRN_MAC2PHY_OPMODE)
        .is_none_or(|m| m & KMRN_OPMODE_MASK != KMRN_OPMODE_INBAND_MDIO);

    let ctrl = (hw.rd(CTRL) & !(super::CTRL_FRCSPD | super::CTRL_FRCDPX)) | super::CTRL_SLU;
    hw.wr(CTRL, ctrl);
    // The longest Kumeran timeouts; and "these next three accesses were
    // always meant to use page 0x34" (Linux).
    es2.kmrn_write(KMRN_TIMEOUTS, 0xffff);
    es2.kmrn_or(KMRN_INBAND_PARAM, 0x3f);
    es2.kmrn_or(KMRN_INB_CTRL, KMRN_INB_DIS_PADDING);
    let phy_setup = gg82563_setup(&es2);
    let autoneg = restart_autoneg(&es2);
    es2.kmrn_or(KMRN_INBAND_PARAM, KMRN_IBIST_DISABLE);

    hw.wr(
        TCTL_EXT,
        (hw.rd(TCTL_EXT) & !TCTL_EXT_GCEX_MASK) | TCTL_EXT_GCEX,
    );
    hw.wr(FFLT1, hw.rd(FFLT1) & !0x0010_0000);
    Ok(Report {
        quiesced,
        owned,
        nvm_read,
        phy_setup,
        autoneg,
        mdic_wa: es2.mdic_wa,
    })
}

/// The bits of Linux `e1000_initialize_hw_bits_80003es2lan`, for copper.
fn hw_bits<H: Hw>(hw: &H) {
    hw.wr(TXDCTL, hw.rd(TXDCTL) | (1 << 22));
    hw.wr(TXDCTL1, hw.rd(TXDCTL1) | (1 << 22));
    hw.wr(TARC0, hw.rd(TARC0) & !(0xf << 27));
    let tarc1 = hw.rd(TARC1);
    hw.wr(
        TARC1,
        if hw.rd(TCTL) & TCTL_MULR != 0 {
            tarc1 & !(1 << 28)
        } else {
            tarc1 | (1 << 28)
        },
    );
    // A malformed IPv6 extension header can hang the receive unit.
    hw.wr(
        RFCTL,
        hw.rd(RFCTL) | RFCTL_IPV6_EX_DIS | RFCTL_NEW_IPV6_EXT_DIS,
    );
}

/// Linux `e1000_copper_link_setup_gg82563_80003es2lan`, with automatic
/// crossover and polarity correction. Whether every step took.
fn gg82563_setup<H: Hw>(es2: &Es2<'_, H>) -> bool {
    let hw = es2.hw;
    let mut ok = es2.update(GG_MAC_SPEC_CTRL, |v| {
        v | GG_MSCR_ASSERT_CRS_ON_TX | GG_MSCR_TX_CLK_1000MBPS_25
    });
    ok &= es2.update(GG_SPEC_CTRL, |v| {
        (v & !(GG_PSCR_CROSSOVER_MASK | GG_PSCR_POLARITY_REVERSAL_DISABLE)) | GG_PSCR_CROSSOVER_AUTO
    });
    // A PHY reset for the crossover to take.
    ok &= es2.update(MII_BMCR, |v| v | BMCR_RESET);
    hw.delay_us(1);
    ok &= es2.kmrn_write(KMRN_FIFO_CTRL, KMRN_FIFO_RX_BYPASS | KMRN_FIFO_TX_BYPASS);
    ok &= es2.kmrn_or(KMRN_MAC2PHY_OPMODE, KMRN_OPMODE_E_IDLE);
    ok &= es2.update(GG_SPEC_CTRL_2, |v| v & !GG_PSCR2_REVERSE_AUTO_NEG);
    hw.wr(CTRL_EXT, hw.rd(CTRL_EXT) & !CTRL_EXT_LINK_MODE_MASK);
    // Under IAMT the firmware has set these itself.
    if hw.rd(FWSM) & FWSM_MODE_MASK != FWSM_MODE_IAMT {
        ok &= es2.update(GG_PWR_MGMT_CTRL, |v| v | GG_PMCR_ENABLE_ELECTRICAL_IDLE);
        ok &= es2.update(GG_KMRN_MODE_CTRL, |v| v & !GG_KMCR_PASS_FALSE_CARRIER);
    }
    // No padding on the Kumeran interface in the MAC: the PHY pads.
    ok &= es2.update(GG_INBAND_CTRL, |v| v | GG_ICR_DIS_PADDING);
    ok
}

/// Offer every 10/100 mode, PAUSE and gigabit full duplex and restart
/// auto-negotiation.
fn restart_autoneg<H: Hw>(es2: &Es2<'_, H>) -> bool {
    let Some(bmcr) = es2.read(MII_BMCR) else {
        return false;
    };
    es2.update(MII_ADVERTISE, |v| v | ADVERTISE_ALL);
    es2.update(MII_CTRL1000, |v| v | ADVERTISE_1000FULL);
    es2.write(MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART)
}

/// On link up (Linux `e1000_cfg_on_link_up_80003es2lan`): the Kumeran
/// half-duplex control and the inter-packet gap for the speed, and the
/// PHY's false-carrier pass-through on only in half duplex below gigabit.
pub fn on_link_up<H: Hw>(hw: &H, speed: u32, full_duplex: bool) -> bool {
    let mut es2 = Es2::new(hw);
    es2.mdic_wa = es2
        .kmrn_read(KMRN_MAC2PHY_OPMODE)
        .is_none_or(|m| m & KMRN_OPMODE_MASK != KMRN_OPMODE_INBAND_MDIO);
    let gigabit = speed == 1000;
    if !es2.kmrn_write(
        KMRN_HD_CTRL,
        if gigabit {
            KMRN_HD_CTRL_1000
        } else {
            KMRN_HD_CTRL_10_100
        },
    ) {
        return false;
    }
    let ipgt = if gigabit {
        TIPG_IPGT_1000
    } else {
        TIPG_IPGT_10_100
    };
    hw.wr(TIPG, (hw.rd(TIPG) & !TIPG_IPGT_MASK) | ipgt);
    let Some(mode) = es2.kmrn_mode_ctrl() else {
        return false;
    };
    let mode = if !gigabit && !full_duplex {
        mode | GG_KMCR_PASS_FALSE_CARRIER
    } else {
        mode & !GG_KMCR_PASS_FALSE_CARRIER
    };
    es2.write(GG_KMRN_MODE_CTRL, mode)
}

#[cfg(test)]
mod tests;
