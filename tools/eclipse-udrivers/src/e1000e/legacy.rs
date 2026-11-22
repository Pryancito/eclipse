//! The discrete e1000e parts before the 82574: the 82571 and 82572 (an IGP
//! PHY, shared between the two ports of a dual card behind the SW/FW
//! semaphore `SWSM`) and the 82573 (an M88 PHY whose MDIO the firmware
//! hands over through `EXTCNF_CTRL`). Copper only: the fiber and SerDes
//! versions have no PHY to bring up, and are not taken.
//!
//! What is done is what Linux's `82571.c` does that is more than the
//! 82574's bring-up: the bus quiesced and the PHY's owner held around the
//! MAC reset, the NVM's automatic read waited for, the errata bits of
//! `e1000_initialize_hw_bits_82571`, LPLU off in an IGP PHY
//! (`e1000_set_d0_lplu_state_82571`), and auto-negotiation restarted with
//! everything offered. After the reset the card loads its own address from
//! the EEPROM, which `EERD` reads otherwise, as on an 82574.

use super::pch::{
    mdic_read, mdic_write, Hw, ADVERTISE_1000FULL, ADVERTISE_ALL, BMCR_ANENABLE, BMCR_ANRESTART,
    MII_ADVERTISE, MII_BMCR, MII_CTRL1000,
};
use super::Part;

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const EECD: usize = 0x0010;
const CTRL_EXT: usize = 0x0018;
pub(super) const TCTL: usize = 0x0400;
const EXTCNF_CTRL: usize = 0x0f00;
const PBA_ECC: usize = 0x1100;
pub(super) const TXDCTL: usize = 0x3828;
pub(super) const TXDCTL1: usize = 0x3928;
pub(super) const TARC0: usize = 0x3840;
pub(super) const TARC1: usize = 0x3940;
pub(super) const RFCTL: usize = 0x5008;
const SWSM: usize = 0x5b50;

const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
const CTRL_RST: u32 = 1 << 26;
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;
const EECD_AUTO_RD: u32 = 1 << 9;
const CTRL_EXT_DMA_DYN_CLK_EN: u32 = 1 << 19;
const EXTCNF_CTRL_MDIO_SW_OWNERSHIP: u32 = 1 << 5;
const PBA_ECC_CORR_EN: u32 = 1 << 0;
const TCTL_MULR: u32 = 1 << 28;
/// `TARC0`'s speed-mode bit, set while the link is at gigabit (Linux
/// `FLAG_TARC_SPEED_MODE_BIT`, the 82571 and 82572).
pub(super) const TARC0_SPEED_MODE: u32 = 1 << 21;
const RFCTL_IPV6_EX_DIS: u32 = 1 << 16;
const RFCTL_NEW_IPV6_EXT_DIS: u32 = 1 << 17;
const SWSM_SMBI: u32 = 1 << 0;
const SWSM_SWESMBI: u32 = 1 << 1;

/// The PHY's MDIO address on these parts.
const PHY: u8 = 1;
/// IGP's page-select register, and its power management register with
/// "low power link up" in D0.
const IGP_PAGE_SELECT: u32 = 0x1f;
const IGP02_POWER_MGMT: u32 = 0x19;
const IGP02_PM_D0_LPLU: u16 = 1 << 1;

/// Tries at the semaphores, 50 to 100 us apart in Linux: an EEPROM's worth
/// of words is its bound, 2048 a generous one.
const SWSM_TRIES: u32 = 2048;

/// Whoever owns the PHY, held until dropped: `SWSM` on an 82571/82572
/// (Linux `e1000_get_hw_semaphore_82571`), MDIO ownership on an 82573
/// (`e1000_get_hw_semaphore_82573`).
pub(super) struct Owner<'a, H: Hw> {
    hw: &'a H,
    part: Part,
}

impl<'a, H: Hw> Owner<'a, H> {
    pub(super) fn take(hw: &'a H, part: Part) -> Option<Self> {
        if part == Part::E82573 {
            for _ in 0..10 {
                hw.wr(
                    EXTCNF_CTRL,
                    hw.rd(EXTCNF_CTRL) | EXTCNF_CTRL_MDIO_SW_OWNERSHIP,
                );
                if hw.rd(EXTCNF_CTRL) & EXTCNF_CTRL_MDIO_SW_OWNERSHIP != 0 {
                    return Some(Owner { hw, part });
                }
                hw.delay_us(2_000);
            }
            hw.wr(
                EXTCNF_CTRL,
                hw.rd(EXTCNF_CTRL) & !EXTCNF_CTRL_MDIO_SW_OWNERSHIP,
            );
            return None;
        }
        // The software half: reading `SWSM` takes `SMBI` when it is clear.
        if !(0..SWSM_TRIES).any(|_| {
            let free = hw.rd(SWSM) & SWSM_SMBI == 0;
            if !free {
                hw.delay_us(50);
            }
            free
        }) {
            return None;
        }
        // The firmware half: `SWESMBI` latches when the firmware lets go.
        for _ in 0..SWSM_TRIES {
            hw.wr(SWSM, hw.rd(SWSM) | SWSM_SWESMBI);
            if hw.rd(SWSM) & SWSM_SWESMBI != 0 {
                return Some(Owner { hw, part });
            }
            hw.delay_us(50);
        }
        hw.wr(SWSM, hw.rd(SWSM) & !(SWSM_SMBI | SWSM_SWESMBI));
        None
    }
}

impl<H: Hw> Drop for Owner<'_, H> {
    fn drop(&mut self) {
        let hw = self.hw;
        if self.part == Part::E82573 {
            hw.wr(
                EXTCNF_CTRL,
                hw.rd(EXTCNF_CTRL) & !EXTCNF_CTRL_MDIO_SW_OWNERSHIP,
            );
        } else {
            hw.wr(SWSM, hw.rd(SWSM) & !(SWSM_SMBI | SWSM_SWESMBI));
        }
    }
}

/// What [`reset`] did that is worth a line in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// Whether the card stopped mastering the bus before its reset.
    pub quiesced: bool,
    /// Whether the PHY's owner could be had around the reset.
    pub owned: bool,
    /// Whether the NVM said its automatic read was done.
    pub nvm_read: bool,
    /// Whether LPLU had to be cleared in an IGP PHY.
    pub lplu_cleared: bool,
    /// Whether auto-negotiation was restarted.
    pub autoneg: bool,
}

/// Reset an 82571, 82572 or 82573 (Linux `e1000_reset_hw_82571`) and set
/// it up as `e1000_init_hw_82571` does, up to where the rings are
/// programmed. Interrupts masked, receive and transmit off.
pub fn reset<H: Hw>(hw: &H, part: Part) -> Result<Report, String> {
    hw.wr(CTRL, hw.rd(CTRL) | CTRL_GIO_MASTER_DISABLE);
    let quiesced = (0..800).any(|_| {
        let idle = hw.rd(STATUS) & STATUS_GIO_MASTER_ENABLE == 0;
        if !idle {
            hw.delay_us(100);
        }
        idle
    });
    hw.wr(super::IMC, u32::MAX);
    hw.wr(super::RCTL, 0);
    hw.wr(TCTL, super::TCTL_PSP);
    let _ = hw.rd(STATUS);
    hw.delay_us(10_000);

    // The PHY's owner is held across the reset: on a dual 82571 the other
    // port's driver may be talking to it.
    let owner = Owner::take(hw, part);
    let owned = owner.is_some();
    hw.wr(CTRL, hw.rd(CTRL) | CTRL_RST);
    drop(owner);
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
    let nvm_read = (0..10).any(|_| {
        let done = hw.rd(EECD) & EECD_AUTO_RD != 0;
        if !done {
            hw.delay_us(1_000);
        }
        done
    });
    if part == Part::E82573 {
        hw.delay_us(25_000);
    }
    hw.wr(super::IMC, u32::MAX);
    let _ = hw.rd(super::ICR);

    let ext = (hw.rd(CTRL_EXT) | super::CTRL_EXT_RO_DIS | super::CTRL_EXT_DRV_LOAD)
        & !super::CTRL_EXT_IAME;
    hw.wr(CTRL_EXT, ext);
    hw.wr(super::IAM, 0);
    hw_bits(hw, part);
    let ctrl = (hw.rd(CTRL) & !(super::CTRL_FRCSPD | super::CTRL_FRCDPX))
        | super::CTRL_SLU
        | super::CTRL_ASDE;
    hw.wr(CTRL, ctrl);

    let (lplu_cleared, autoneg) = match Owner::take(hw, part) {
        Some(_owner) => {
            let lplu = part == Part::E82571 && clear_igp_lplu(hw);
            (lplu, restart_autoneg(hw))
        }
        None => (false, false),
    };
    Ok(Report {
        quiesced,
        owned,
        nvm_read,
        lplu_cleared,
        autoneg,
    })
}

/// The errata and arbitration bits of Linux
/// `e1000_initialize_hw_bits_82571`, for these three parts.
fn hw_bits<H: Hw>(hw: &H, part: Part) {
    hw.wr(TXDCTL, hw.rd(TXDCTL) | (1 << 22));
    hw.wr(TXDCTL1, hw.rd(TXDCTL1) | (1 << 22));
    let mut tarc0 = hw.rd(TARC0) & !(0xf << 27);
    if part == Part::E82571 {
        tarc0 |= (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26);
    }
    hw.wr(TARC0, tarc0);
    if part == Part::E82571 {
        let mut tarc1 = hw.rd(TARC1) & !((1 << 29) | (1 << 30));
        tarc1 |= (1 << 22) | (1 << 24) | (1 << 25) | (1 << 26);
        if hw.rd(TCTL) & TCTL_MULR != 0 {
            tarc1 &= !(1 << 28);
        } else {
            tarc1 |= 1 << 28;
        }
        hw.wr(TARC1, tarc1);
        // Error correction in the packet buffer, and no dynamic DMA clock
        // gating (an erratum of both parts).
        hw.wr(PBA_ECC, hw.rd(PBA_ECC) | PBA_ECC_CORR_EN);
        hw.wr(CTRL_EXT, hw.rd(CTRL_EXT) & !CTRL_EXT_DMA_DYN_CLK_EN);
    } else {
        hw.wr(CTRL, hw.rd(CTRL) & !(1 << 29));
        hw.wr(CTRL_EXT, (hw.rd(CTRL_EXT) & !(1 << 23)) | (1 << 22));
    }
    // A malformed IPv6 extension header can hang the receive unit.
    hw.wr(
        RFCTL,
        hw.rd(RFCTL) | RFCTL_IPV6_EX_DIS | RFCTL_NEW_IPV6_EXT_DIS,
    );
}

/// Clear "low power link up" in D0 in an IGP PHY: set, it negotiates the
/// lowest speed there is. Whether it had to be.
fn clear_igp_lplu<H: Hw>(hw: &H) -> bool {
    if !mdic_write(hw, PHY, IGP_PAGE_SELECT, IGP02_POWER_MGMT as u16) {
        return false;
    }
    match mdic_read(hw, PHY, IGP02_POWER_MGMT) {
        Some(pm) if pm != 0xffff && pm & IGP02_PM_D0_LPLU != 0 => {
            mdic_write(hw, PHY, IGP02_POWER_MGMT, pm & !IGP02_PM_D0_LPLU)
        }
        _ => false,
    }
}

/// Offer every 10/100 mode, PAUSE and gigabit full duplex (all of these
/// PHYs have it) and restart auto-negotiation.
fn restart_autoneg<H: Hw>(hw: &H) -> bool {
    let Some(bmcr) = mdic_read(hw, PHY, MII_BMCR).filter(|&b| b != 0xffff) else {
        return false;
    };
    if let Some(adv) = mdic_read(hw, PHY, MII_ADVERTISE).filter(|&a| a != 0xffff) {
        if adv & ADVERTISE_ALL != ADVERTISE_ALL {
            mdic_write(hw, PHY, MII_ADVERTISE, adv | ADVERTISE_ALL);
        }
    }
    if let Some(c) = mdic_read(hw, PHY, MII_CTRL1000).filter(|&c| c != 0xffff) {
        if c & ADVERTISE_1000FULL == 0 {
            mdic_write(hw, PHY, MII_CTRL1000, c | ADVERTISE_1000FULL);
        }
    }
    mdic_write(hw, PHY, MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART)
}

/// On link up (Linux's watchdog, `FLAG_TARC_SPEED_MODE_BIT`): an 82571 or
/// 82572 keeps `TARC0`'s speed-mode bit only at gigabit.
pub fn on_link_up<H: Hw>(hw: &H, part: Part, speed: u32) {
    if part != Part::E82571 {
        return;
    }
    let tarc0 = hw.rd(TARC0);
    let want = if speed == 1000 {
        tarc0 | TARC0_SPEED_MODE
    } else {
        tarc0 & !TARC0_SPEED_MODE
    };
    if want != tarc0 {
        hw.wr(TARC0, want);
    }
}

#[cfg(test)]
mod tests;
