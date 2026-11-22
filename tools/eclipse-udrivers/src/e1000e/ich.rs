//! The e1000e MACs of the ICH8, ICH9 and ICH10 chipsets (Linux
//! `ich8lan.c`, mac types `ich8lan`, `ich9lan`, `ich10lan`): the MAC in the
//! south bridge, the PHY beside it (IGP3, IFE or BM, at MDIO address 1),
//! shared with the management engine behind the SW/FW semaphore as on the
//! PCH parts ([`super::pch`]).
//!
//! What is done is Linux's `e1000_reset_hw_ich8lan` and `e1000_init_hw_ich8lan`
//! without the PHY reset (as the kernel's driver skips it on the PCH parts:
//! the PHY comes out of power-on negotiating): the bus quiesced, ICH8's
//! FIFO workaround, the MAC reset under the semaphore with nothing read for
//! 20 ms after it (a read then hangs the hardware), the bits of
//! `e1000_initialize_hw_bits_ich8lan`, LPLU and gigabit-disable off in
//! `PHY_CTRL` (where the ICH keeps them), flow control, the Kumeran
//! timeouts, and auto-negotiation restarted with everything offered
//! (gigabit only where the PHY has it: an IFE is 10/100). The address is
//! the one the reset loaded from the NVM, whose flash is behind BAR 1.

use super::pch::{self, Hw, Swflag};
use super::Part;

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const CTRL_EXT: usize = 0x0018;
const FCTTV: usize = 0x0170;
const EXTCNF_CTRL: usize = 0x0f00;
const PHY_CTRL: usize = 0x0f10;
const PBA: usize = 0x1000;
const PBS: usize = 0x1008;
const FCRTL: usize = 0x2160;
const FCRTH: usize = 0x2168;
const KABGTXD: usize = 0x3004;
const TXDCTL: usize = 0x3828;
const TXDCTL1: usize = 0x3928;
const TARC0: usize = 0x3840;
const TARC1: usize = 0x3940;
const TCTL: usize = 0x0400;
const RFCTL: usize = 0x5008;
const GCR: usize = 0x5b00;

const CTRL_RST: u32 = 1 << 26;
const EXTCNF_CTRL_SWFLAG: u32 = 1 << 5;
const PHY_CTRL_LPLU_AND_GBE: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 6);
const KABGTXD_BGSQLBIAS: u32 = 0x0005_0000;
const TCTL_MULR: u32 = 1 << 28;
const RFCTL_NFSW_DIS: u32 = 1 << 6;
const RFCTL_NFSR_DIS: u32 = 1 << 7;
const RFCTL_IPV6_EX_DIS: u32 = 1 << 16;
const RFCTL_NEW_IPV6_EXT_DIS: u32 = 1 << 17;
/// `GCR`: no snooping on any of the card's DMA (Linux
/// `PCIE_ICH8_SNOOP_ALL`, ICH8 only).
const GCR_NO_SNOOP_ALL: u32 = 0x3f;
const FCRTL_XONE: u32 = 1 << 31;
const FC_PAUSE_TIME: u32 = 0x0680;
const MAX_FRAME: u32 = 1518;

/// The MDIO address of the PHY on these chipsets.
pub const PHY: u8 = 1;

/// The receive packet buffer, in KiB (Linux `e1000e_reset`): 8 on ICH8,
/// 10 on ICH9 and ICH10.
fn pba_kb(part: Part) -> u32 {
    if part == Part::Ich8 {
        8
    } else {
        10
    }
}

/// The flow-control watermarks for a packet buffer of `pba_kb` KiB: the
/// lower of 90 % of it and all of it but a frame, on the register's 8-byte
/// grain, and the low one 8 bytes below.
fn watermarks(pba_kb: u32) -> (u32, u32) {
    let bytes = pba_kb << 10;
    let high = (bytes / 10 * 9).min(bytes - MAX_FRAME) & 0xfff8;
    (high, high - 8)
}

/// What [`reset`] did that is worth a line in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub quiesced: bool,
    /// Whether the semaphore was had for the reset.
    pub swflag: bool,
    /// The MDIO address auto-negotiation was restarted at.
    pub autoneg: Option<u8>,
}

/// Reset an ICH8, ICH9 or ICH10 MAC and set it up, up to where the rings
/// are programmed. Interrupts masked, receive and transmit off.
pub fn reset<H: Hw>(hw: &H, part: Part) -> Result<Report, String> {
    let quiesced = pch::disable_pcie_master(hw);
    hw.wr(super::IMC, u32::MAX);
    hw.wr(super::RCTL, 0);
    hw.wr(TCTL, super::TCTL_PSP);
    pch::flush(hw);
    hw.delay_us(10_000);
    if part == Part::Ich8 {
        // ICH8's FIFO corruption workaround: 8 KiB receive, 16 KiB of
        // packet buffer in all.
        hw.wr(PBA, 8);
        hw.wr(PBS, 16);
    }
    // The semaphore is the reset's to clear: nothing may be read for a
    // while after it, so the guard is not dropped but let go of by hand.
    let flag = Swflag::take(hw);
    let swflag = flag.is_some();
    std::mem::forget(flag);
    hw.wr(CTRL, hw.rd(CTRL) | CTRL_RST);
    hw.delay_us(20_000);
    let mut out = false;
    for _ in 0..1000 {
        if hw.rd(CTRL) & CTRL_RST == 0 {
            out = true;
            break;
        }
        hw.delay_us(1_000);
    }
    hw.wr(EXTCNF_CTRL, hw.rd(EXTCNF_CTRL) & !EXTCNF_CTRL_SWFLAG);
    if !out {
        return Err("the card did not come out of its reset".into());
    }
    hw.wr(super::IMC, u32::MAX);
    let _ = hw.rd(super::ICR);
    hw.wr(KABGTXD, hw.rd(KABGTXD) | KABGTXD_BGSQLBIAS);

    let ext = (hw.rd(CTRL_EXT) | super::CTRL_EXT_RO_DIS | super::CTRL_EXT_DRV_LOAD)
        & !super::CTRL_EXT_IAME;
    hw.wr(CTRL_EXT, ext);
    hw.wr(super::IAM, 0);
    hw_bits(hw, part);
    if part == Part::Ich8 {
        hw.wr(GCR, hw.rd(GCR) | GCR_NO_SNOOP_ALL);
    }
    hw.wr(PHY_CTRL, hw.rd(PHY_CTRL) & !PHY_CTRL_LPLU_AND_GBE);
    let _ = hw.rd(PHY_CTRL);

    let ctrl = (hw.rd(CTRL) & !(super::CTRL_FRCSPD | super::CTRL_FRCDPX))
        | super::CTRL_SLU
        | super::CTRL_ASDE;
    hw.wr(CTRL, ctrl);
    let pba = pba_kb(part);
    let (high, low) = watermarks(pba);
    hw.wr(PBA, pba);
    hw.wr(FCTTV, FC_PAUSE_TIME);
    hw.wr(FCRTL, low | FCRTL_XONE);
    hw.wr(FCRTH, high);
    pch::flush(hw);
    pch::kmrn_copper_timeouts(hw);
    let autoneg = pch::restart_autoneg_at(hw, &[PHY]);
    Ok(Report {
        quiesced,
        swflag,
        autoneg,
    })
}

/// The bits of Linux `e1000_initialize_hw_bits_ich8lan`, for these three.
fn hw_bits<H: Hw>(hw: &H, part: Part) {
    hw.wr(CTRL_EXT, hw.rd(CTRL_EXT) | (1 << 22));
    hw.wr(TXDCTL, hw.rd(TXDCTL) | (1 << 22));
    hw.wr(TXDCTL1, hw.rd(TXDCTL1) | (1 << 22));
    let mut tarc0 = hw.rd(TARC0);
    if part == Part::Ich8 {
        tarc0 |= (1 << 28) | (1 << 29);
    }
    tarc0 |= (1 << 23) | (1 << 24) | (1 << 26) | (1 << 27);
    hw.wr(TARC0, tarc0);
    let mut tarc1 = hw.rd(TARC1);
    if hw.rd(TCTL) & TCTL_MULR != 0 {
        tarc1 &= !(1 << 28);
    } else {
        tarc1 |= 1 << 28;
    }
    tarc1 |= (1 << 24) | (1 << 26) | (1 << 30);
    hw.wr(TARC1, tarc1);
    if part == Part::Ich8 {
        hw.wr(STATUS, hw.rd(STATUS) & !(1 << 31));
    }
    // NFS filtering corrupts descriptors under NFSv2 over UDP; and on ICH8
    // a malformed IPv6 extension header can hang the receive unit.
    let mut rfctl = hw.rd(RFCTL) | RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
    if part == Part::Ich8 {
        rfctl |= RFCTL_IPV6_EX_DIS | RFCTL_NEW_IPV6_EXT_DIS;
    }
    hw.wr(RFCTL, rfctl);
}

/// On link up: flow control resolved against the partner, from the PHY at
/// address 1 (Linux `e1000e_config_fc_after_link_up`).
pub fn on_link_up<H: Hw>(hw: &H, status: u32) -> pch::Link {
    pch::resolve_pause(
        hw,
        &[PHY],
        pch::Link {
            speed: pch::speed_of(status),
            full_duplex: status & 1 != 0,
            pause: None,
            phy_says: None,
        },
    )
}

#[cfg(test)]
mod tests;
