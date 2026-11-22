//! What a PCH e1000e (82579, I217, I218, I219) needs before its rings can
//! be programmed, and when its link comes up: the bring-up the kernel's
//! driver (`drivers/src/net/e1000e.rs`) does on real I219s, which is a
//! subset of Linux's `ich8lan.c`, step for step.
//!
//! On these parts the PHY is a separate chip behind the MAC, shared with
//! the management engine (ME). Firmware or a previous OS can leave it in
//! Ultra Low Power (ULP: no link at all), with "low power link up" set
//! (LPLU: the lowest speed the switch offers, 10 Mb/s), or with K1 off. So
//! before the MAC reset the ME is asked to take the PHY out of ULP, and
//! LANPHYPC is toggled so the PHY runs its power-up again; after it, LPLU
//! and gigabit-disable are cleared in both copies (the MAC's `PHY_CTRL` and
//! the PHY's own `HV_OEM_BITS`), the Kumeran timeouts and K1 are set, flow
//! control is programmed and auto-negotiation restarted with everything
//! advertised. Each PHY access is under the SW/FW semaphore
//! (`EXTCNF_CTRL.SWFLAG`), as the ME requires.
//!
//! Everything is written against [`Hw`], so the sequences can be run on the
//! host against a card that only pretends ([`tests`]): nothing here can be
//! tried on one in QEMU, whose only e1000e is an 82574.

use super::Part;

/// What the sequences need of a card: its registers, by byte offset, and
/// a way to wait.
pub trait Hw {
    fn rd(&self, reg: usize) -> u32;
    fn wr(&self, reg: usize, value: u32);
    fn delay_us(&self, us: u64);
}

// Registers, by byte offset (Linux `regs.h`, `ich8lan.h`).
pub(super) const CTRL: usize = 0x0000;
pub(super) const STATUS: usize = 0x0008;
const STRAP: usize = 0x000c;
const FEXTNVM6: usize = 0x0010;
pub(super) const CTRL_EXT: usize = 0x0018;
const MDIC: usize = 0x0020;
const FEXTNVM4: usize = 0x0024;
const FEXTNVM: usize = 0x0028;
const KMRNCTRLSTA: usize = 0x0034;
pub(super) const VET: usize = 0x0038;
const FCTTV: usize = 0x0170;
pub(super) const TIPG: usize = 0x0410;
const FEXTNVM7: usize = 0x00e4;
const EXTCNF_CTRL: usize = 0x0f00;
const PHY_CTRL: usize = 0x0f10;
pub(super) const IOSFPC: usize = 0x0f28;
const PBA: usize = 0x1000;
const FCRTL: usize = 0x2160;
const FCRTH: usize = 0x2168;
pub(super) const SRRCTL: usize = 0x280c;
pub(super) const RXDCTL: usize = 0x2828;
pub(super) const TXDCTL: usize = 0x3828;
pub(super) const TXDCTL1: usize = 0x3928;
pub(super) const WUC: usize = 0x5800;
pub(super) const WUFC: usize = 0x5808;
const H2ME: usize = 0x5b50;
const FWSM: usize = 0x5b54;
const FCRTV_PCH: usize = 0x5f40;
/// The NVM's flash controller, inside BAR 0 from Sunrise Point on.
const FLASH: usize = 0xe000;
const HSFSTS: usize = FLASH + 0x04;
const FADDR: usize = FLASH + 0x08;
const FDATA0: usize = FLASH + 0x10;
/// The last register a PCH part's bring-up touches, and so how much of the
/// window it needs.
pub const REGS_SPAN: usize = FDATA0 + 4;

const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
const CTRL_FRCSPD: u32 = 1 << 11;
const CTRL_FRCDPX: u32 = 1 << 12;
const CTRL_SPD_100: u32 = 1 << 8;
const CTRL_SPD_1000: u32 = 1 << 9;
const CTRL_LANPHYPC_OVERRIDE: u32 = 1 << 16;
const CTRL_LANPHYPC_VALUE: u32 = 1 << 17;
pub(super) const CTRL_RST: u32 = 1 << 26;
const CTRL_RFCE: u32 = 1 << 27;
const CTRL_TFCE: u32 = 1 << 28;
const STATUS_FD: u32 = 1 << 0;
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;
const CTRL_EXT_LPCD: u32 = 1 << 2;
const CTRL_EXT_FORCE_SMBUS: u32 = 1 << 11;
const CTRL_EXT_SPD_BYPS: u32 = 1 << 15;

pub(super) const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
pub(super) const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
/// `SRRCTL`: 2 KiB receive buffers, and drop a frame when no descriptor is
/// free rather than stall the queue.
pub(super) const SRRCTL_VALUE: u32 = 2 | (1 << 31);
/// `IOSFPC`: the bit Linux sets for PCIe compliance on Sunrise Point.
pub(super) const IOSFPC_COMPLIANCE: u32 = 1 << 16;

const MDIC_REG_SHIFT: u32 = 16;
const MDIC_PHY_SHIFT: u32 = 21;
const MDIC_OP_WRITE: u32 = 1 << 26;
const MDIC_OP_READ: u32 = 2 << 26;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;
const MDIC_TRIES: u32 = 2000;

const EXTCNF_CTRL_SWFLAG: u32 = 1 << 5;
const FWSM_FW_VALID: u32 = 1 << 15;
const FWSM_ULP_CFG_DONE: u32 = 1 << 10;
const H2ME_ULP: u32 = 1 << 11;
const H2ME_ENFORCE_SETTINGS: u32 = 1 << 12;
const FEXTNVM_SW_CONFIG_ICH8M: u32 = 1 << 27;
const FEXTNVM4_BEACON_DURATION_MASK: u32 = 0x7;
const FEXTNVM4_BEACON_DURATION_8USEC: u32 = 0x7;
const FEXTNVM6_K1_OFF_ENABLE: u32 = 1 << 4;
const FEXTNVM7_SIDE_CLK_UNGATE: u32 = 1 << 0;
const FEXTNVM7_DISABLE_SMB_PERST: u32 = 1 << 5;

const PHY_CTRL_D0A_LPLU: u32 = 1 << 1;
const PHY_CTRL_NOND0A_LPLU: u32 = 1 << 2;
const PHY_CTRL_NOND0A_GBE_DISABLE: u32 = 1 << 3;
const PHY_CTRL_GBE_DISABLE: u32 = 1 << 6;

const KMRN_OFFSET_SHIFT: u32 = 16;
const KMRN_OFFSET: u32 = 0x001f_0000;
const KMRN_REN: u32 = 1 << 21;
const KMRN_TIMEOUTS: u32 = 0x4;
const KMRN_INBAND_PARAM: u32 = 0x9;
const KMRN_K1_CONFIG: u32 = 0x7;
const KMRN_K1_ENABLE: u16 = 0x0002;

// The PHY: the IEEE registers at MDIO address 2, the pages from 768 on (the
// ones Intel added) at address 1, reached through the page-select register.
const PHY_PAGE_SELECT: u32 = 0x1f;
const PHY_PAGE_SHIFT: u32 = 5;
const MULTI_PAGE_REG_MAX: u32 = 0x0f;
const HV_PHY: u8 = 1;
const HV_INTC_FC_PAGE: u32 = 768;
const HV_OEM_BITS: (u32, u32) = (768, 25);
const HV_OEM_BITS_LPLU: u16 = 1 << 2;
const HV_OEM_BITS_GBE_DIS: u16 = 1 << 6;
const HV_OEM_BITS_RESTART_AN: u16 = 1 << 10;
const BM_PORT_PAUSE_TIME: (u32, u32) = (769, 27);
const CV_SMB_CTRL: (u32, u32) = (769, 23);
const CV_SMB_CTRL_FORCE_SMBUS: u16 = 1 << 0;
const HV_PM_CTRL: (u32, u32) = (770, 17);
const HV_PM_CTRL_K1_ENABLE: u16 = 1 << 14;
const ULP_CONFIG1: (u32, u32) = (779, 16);
const ULP_CONFIG1_START: u16 = 1 << 0;
const ULP_CONFIG1_IND: u16 = 1 << 2;
const ULP_CONFIG1_STICKY_ULP: u16 = 1 << 4;
const ULP_CONFIG1_INBAND_EXIT: u16 = 1 << 5;
const ULP_CONFIG1_WOL_HOST: u16 = 1 << 6;
const ULP_CONFIG1_RESET_TO_SMBUS: u16 = 1 << 8;
const ULP_CONFIG1_DISABLE_SMB_PERST: u16 = 1 << 12;

pub(super) const MII_BMCR: u32 = 0x00;
pub(super) const MII_BMSR: u32 = 0x01;
pub(super) const MII_ADVERTISE: u32 = 0x04;
pub(super) const MII_LPA: u32 = 0x05;
pub(super) const MII_CTRL1000: u32 = 0x09;
pub(super) const MII_STAT1000: u32 = 0x0a;
pub(super) const MII_ESTATUS: u32 = 0x0f;
pub(super) const BMCR_ANENABLE: u16 = 1 << 12;
pub(super) const BMCR_ANRESTART: u16 = 1 << 9;
pub(super) const BMSR_LSTATUS: u16 = 1 << 2;
pub(super) const BMSR_ANEGCOMPLETE: u16 = 1 << 5;
pub(super) const BMSR_ESTATEN: u16 = 1 << 8;
pub(super) const ESTATUS_1000_TFULL: u16 = 1 << 13;
/// Every 10/100 mode, and both PAUSE bits (Linux `e1000_phy_setup_autoneg`
/// for `e1000_fc_full`).
pub(super) const ADVERTISE_ALL: u16 = 0x0020 | 0x0040 | 0x0080 | 0x0100 | (1 << 10) | (1 << 11);
const ADVERTISE_PAUSE_CAP: u16 = 1 << 10;
const ADVERTISE_PAUSE_ASYM: u16 = 1 << 11;
pub(super) const ADVERTISE_1000FULL: u16 = 1 << 9;
const ADVERTISE_1000HALF: u16 = 1 << 8;
const LPA_10HALF: u16 = 0x0020;
const LPA_10FULL: u16 = 0x0040;
const LPA_100HALF: u16 = 0x0080;
const LPA_100FULL: u16 = 0x0100;
pub(super) const LPA_1000FULL: u16 = 1 << 11;
const LPA_1000HALF: u16 = 1 << 10;

// Flow control at standard MTU on a PCH2 or later part (Linux
// `e1000e_reset`): fixed watermarks, the longest pause, a 26 KiB receive
// packet buffer.
const PBA_KB: u32 = 26;
const FC_HIGH_WATER: u32 = 0x5c20;
const FC_LOW_WATER: u32 = 0x5048;
const FC_PAUSE_TIME: u32 = 0xffff;
const FCRTL_XONE: u32 = 1 << 31;

const HSFSTS_FLCDONE: u32 = 1 << 0;
const HSFSTS_FLCERR: u32 = 1 << 1;
const HSFSTS_DAEL: u32 = 1 << 2;
const HSFSTS_FLCINPROG: u32 = 1 << 5;
const HSFSTS_FLDESVALID: u32 = 1 << 14;
const FLASH_LINEAR_ADDR_MASK: u32 = 0x00ff_ffff;

pub(super) fn flush<H: Hw>(hw: &H) {
    let _ = hw.rd(STATUS);
}

fn mdic<H: Hw>(hw: &H, cmd: u32) -> Option<u32> {
    hw.wr(MDIC, cmd);
    for _ in 0..MDIC_TRIES {
        hw.delay_us(50);
        let v = hw.rd(MDIC);
        if v & MDIC_READY != 0 {
            return (v & MDIC_ERROR == 0).then_some(v);
        }
    }
    None
}

pub(super) fn mdic_read<H: Hw>(hw: &H, phy: u8, reg: u32) -> Option<u16> {
    mdic(
        hw,
        (reg << MDIC_REG_SHIFT) | ((phy as u32) << MDIC_PHY_SHIFT) | MDIC_OP_READ,
    )
    .map(|v| v as u16)
}

pub(super) fn mdic_write<H: Hw>(hw: &H, phy: u8, reg: u32, value: u16) -> bool {
    mdic(
        hw,
        value as u32 | (reg << MDIC_REG_SHIFT) | ((phy as u32) << MDIC_PHY_SHIFT) | MDIC_OP_WRITE,
    )
    .is_some()
}

/// The SW/FW semaphore, held until dropped (Linux
/// `e1000_acquire_swflag_ich8lan`): wait up to 100 ms for the ME to let go
/// of it, then up to a second for the bit to stick.
pub(super) struct Swflag<'a, H: Hw>(&'a H);

impl<'a, H: Hw> Swflag<'a, H> {
    pub(super) fn take(hw: &'a H) -> Option<Self> {
        let mut free = false;
        for _ in 0..100 {
            if hw.rd(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG == 0 {
                free = true;
                break;
            }
            hw.delay_us(1000);
        }
        if !free {
            return None;
        }
        hw.wr(EXTCNF_CTRL, hw.rd(EXTCNF_CTRL) | EXTCNF_CTRL_SWFLAG);
        for _ in 0..1000 {
            if hw.rd(EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG != 0 {
                return Some(Swflag(hw));
            }
            hw.delay_us(1000);
        }
        hw.wr(EXTCNF_CTRL, hw.rd(EXTCNF_CTRL) & !EXTCNF_CTRL_SWFLAG);
        None
    }
}

impl<H: Hw> Drop for Swflag<'_, H> {
    fn drop(&mut self) {
        let hw = self.0;
        hw.wr(EXTCNF_CTRL, hw.rd(EXTCNF_CTRL) & !EXTCNF_CTRL_SWFLAG);
    }
}

/// What goes in the page-select register for HV page `page`. Page 768 is
/// the one not addressed by its number: it is page 0 at address 1 (Linux
/// `__e1000_read_phy_reg_hv`), and it is where `HV_OEM_BITS` lives.
fn hv_page(page: u32) -> u16 {
    let page = if page == HV_INTC_FC_PAGE { 0 } else { page };
    (page << PHY_PAGE_SHIFT) as u16
}

impl<H: Hw> Swflag<'_, H> {
    fn hv_read(&self, (page, reg): (u32, u32)) -> Option<u16> {
        if reg > MULTI_PAGE_REG_MAX && !mdic_write(self.0, HV_PHY, PHY_PAGE_SELECT, hv_page(page)) {
            return None;
        }
        mdic_read(self.0, HV_PHY, reg & 0x1f)
    }

    fn hv_write(&self, (page, reg): (u32, u32), value: u16) -> bool {
        if reg > MULTI_PAGE_REG_MAX && !mdic_write(self.0, HV_PHY, PHY_PAGE_SELECT, hv_page(page)) {
            return false;
        }
        mdic_write(self.0, HV_PHY, reg & 0x1f, value)
    }

    fn kmrn_read(&self, offset: u32) -> u16 {
        let hw = self.0;
        hw.wr(
            KMRNCTRLSTA,
            ((offset << KMRN_OFFSET_SHIFT) & KMRN_OFFSET) | KMRN_REN,
        );
        flush(hw);
        hw.delay_us(2);
        hw.rd(KMRNCTRLSTA) as u16
    }

    fn kmrn_write(&self, offset: u32, data: u16) {
        let hw = self.0;
        hw.wr(
            KMRNCTRLSTA,
            ((offset << KMRN_OFFSET_SHIFT) & KMRN_OFFSET) | data as u32,
        );
        flush(hw);
        hw.delay_us(2);
    }
}

/// How ULP was left (Linux `e1000_disable_ulp_lpt_lp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ulp {
    /// Not a Sunrise Point part: nothing to do.
    NotThere,
    /// The ME was asked, and said done (or did not, in 400 ms).
    Firmware { done: bool },
    /// No ME: undone in the PHY.
    Software,
    /// No ME, and the semaphore could not be had.
    Busy,
}

/// Take the PHY out of Ultra Low Power: through the ME's handshake when
/// its firmware is there (register writes only, no PHY access), else in
/// the PHY itself.
fn disable_ulp<H: Hw>(hw: &H, part: Part) -> Ulp {
    if part != Part::PchSpt {
        return Ulp::NotThere;
    }
    if hw.rd(FWSM) & FWSM_FW_VALID != 0 {
        // Forced: the driver is starting, whatever the ME thought.
        hw.wr(H2ME, (hw.rd(H2ME) & !H2ME_ULP) | H2ME_ENFORCE_SETTINGS);
        let mut done = false;
        for _ in 0..40 {
            if hw.rd(FWSM) & FWSM_ULP_CFG_DONE == 0 {
                done = true;
                break;
            }
            hw.delay_us(10_000);
        }
        hw.wr(H2ME, hw.rd(H2ME) & !H2ME_ENFORCE_SETTINGS);
        return Ulp::Firmware { done };
    }
    let Some(flag) = Swflag::take(hw) else {
        return Ulp::Busy;
    };
    if let Some(v) = flag.hv_read(CV_SMB_CTRL) {
        flag.hv_write(CV_SMB_CTRL, v & !CV_SMB_CTRL_FORCE_SMBUS);
    }
    hw.wr(CTRL_EXT, hw.rd(CTRL_EXT) & !CTRL_EXT_FORCE_SMBUS);
    // The ME turns K1 off on the way into ULP.
    if let Some(v) = flag.hv_read(HV_PM_CTRL) {
        flag.hv_write(HV_PM_CTRL, v | HV_PM_CTRL_K1_ENABLE);
    }
    if let Some(mut v) = flag.hv_read(ULP_CONFIG1) {
        v &= !(ULP_CONFIG1_IND
            | ULP_CONFIG1_STICKY_ULP
            | ULP_CONFIG1_RESET_TO_SMBUS
            | ULP_CONFIG1_WOL_HOST
            | ULP_CONFIG1_INBAND_EXIT
            | ULP_CONFIG1_DISABLE_SMB_PERST);
        flag.hv_write(ULP_CONFIG1, v);
        flag.hv_write(ULP_CONFIG1, v | ULP_CONFIG1_START);
    }
    hw.wr(FEXTNVM7, hw.rd(FEXTNVM7) & !FEXTNVM7_DISABLE_SMB_PERST);
    Ulp::Software
}

/// Make the PHY run its power-up and configuration again (Linux
/// `e1000_toggle_lanphypc_pch_lpt`): LANPHYPC low under override, then the
/// override let go; a Sunrise Point part says when it is done (`LPCD`).
fn toggle_lanphypc<H: Hw>(hw: &H, part: Part) {
    let mut ctrl = hw.rd(CTRL);
    ctrl |= CTRL_LANPHYPC_OVERRIDE;
    ctrl &= !CTRL_LANPHYPC_VALUE;
    hw.wr(CTRL, ctrl);
    flush(hw);
    hw.delay_us(20);
    ctrl &= !CTRL_LANPHYPC_OVERRIDE;
    hw.wr(CTRL, ctrl);
    flush(hw);
    if part == Part::PchSpt {
        for _ in 0..=20 {
            hw.delay_us(6_000);
            if hw.rd(CTRL_EXT) & CTRL_EXT_LPCD != 0 {
                break;
            }
        }
        hw.delay_us(30_000);
    } else {
        hw.delay_us(50_000);
    }
}

/// Stop the card mastering the bus before its reset (Linux
/// `e1000e_disable_pcie_master`): whether it stopped within 80 ms.
pub(super) fn disable_pcie_master<H: Hw>(hw: &H) -> bool {
    hw.wr(CTRL, hw.rd(CTRL) | CTRL_GIO_MASTER_DISABLE);
    for _ in 0..800 {
        if hw.rd(STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
            return true;
        }
        hw.delay_us(100);
    }
    false
}

/// What the PHY's `HV_OEM_BITS` should say for the MAC's `PHY_CTRL` while
/// the driver is up (Linux `e1000_oem_bits_config_ich8lan`, D0): LPLU and
/// gigabit-disable as the MAC's copy has them, and auto-negotiation
/// restarted.
fn oem_bits_for_d0(phy_ctrl: u32, oem: u16) -> u16 {
    let mut oem = oem & !(HV_OEM_BITS_GBE_DIS | HV_OEM_BITS_LPLU);
    if phy_ctrl & PHY_CTRL_GBE_DISABLE != 0 {
        oem |= HV_OEM_BITS_GBE_DIS;
    }
    if phy_ctrl & PHY_CTRL_D0A_LPLU != 0 {
        oem |= HV_OEM_BITS_LPLU;
    }
    oem | HV_OEM_BITS_RESTART_AN
}

/// Clear LPLU and gigabit-disable in the PHY's own copy, which survives the
/// MAC reset: with LPLU set the PHY negotiates the lowest speed there is.
/// Linux mirrors `PHY_CTRL` when the NVM allows it (`FEXTNVM.SW_CONFIG`) and
/// otherwise still clears LPLU (`e1000_set_lplu_state_pchlan`). The value
/// before and after, when they differ.
fn oem_bits_config<H: Hw>(hw: &H) -> Option<(u16, u16)> {
    let flag = Swflag::take(hw)?;
    let sw_config = hw.rd(FEXTNVM) & FEXTNVM_SW_CONFIG_ICH8M != 0;
    let phy_ctrl = hw.rd(PHY_CTRL);
    let oem = flag.hv_read(HV_OEM_BITS)?;
    if oem == 0xffff {
        return None;
    }
    let want = if sw_config {
        oem_bits_for_d0(phy_ctrl, oem)
    } else {
        (oem & !HV_OEM_BITS_LPLU) | HV_OEM_BITS_RESTART_AN
    };
    (want != oem && flag.hv_write(HV_OEM_BITS, want)).then_some((oem, want))
}

/// Set K1, the Kumeran interconnect's power state, and make the MAC take
/// it (Linux `e1000_configure_k1_ich8lan`): speed forced and bypassed for a
/// moment, then CTRL and CTRL_EXT as they were.
fn configure_k1<H: Hw>(hw: &H, enable: bool) -> bool {
    let Some(flag) = Swflag::take(hw) else {
        return false;
    };
    let k1 = flag.kmrn_read(KMRN_K1_CONFIG);
    flag.kmrn_write(
        KMRN_K1_CONFIG,
        if enable {
            k1 | KMRN_K1_ENABLE
        } else {
            k1 & !KMRN_K1_ENABLE
        },
    );
    hw.delay_us(30);
    let (ctrl_ext, ctrl) = (hw.rd(CTRL_EXT), hw.rd(CTRL));
    hw.wr(
        CTRL,
        (ctrl & !(CTRL_SPD_1000 | CTRL_SPD_100)) | CTRL_FRCSPD | CTRL_FRCDPX,
    );
    hw.wr(CTRL_EXT, ctrl_ext | CTRL_EXT_SPD_BYPS);
    flush(hw);
    hw.delay_us(30);
    hw.wr(CTRL, ctrl);
    hw.wr(CTRL_EXT, ctrl_ext);
    flush(hw);
    hw.delay_us(30);
    true
}

/// The Kumeran half of Linux `e1000_setup_copper_link_ich8lan`: the longest
/// wait between PHY polls and the in-band parameter floor ("fixes erroneous
/// timeouts at 10Mbps").
pub(super) fn kmrn_copper_timeouts<H: Hw>(hw: &H) {
    if let Some(flag) = Swflag::take(hw) {
        flag.kmrn_write(KMRN_TIMEOUTS, 0xffff);
        let inband = flag.kmrn_read(KMRN_INBAND_PARAM);
        flag.kmrn_write(KMRN_INBAND_PARAM, inband | 0x3f);
    }
}

/// 802.3x flow control's registers, and the PHY's copy of the pause time
/// (Linux `e1000e_reset`, `e1000e_set_fc_watermarks`). Which directions
/// are on is decided when the link comes up ([`on_link_up`]).
fn flow_control<H: Hw>(hw: &H, part: Part) {
    // The first PCH (82577/82578) has its own: lower watermarks "to work
    // around adapter hangs with certain network loads", the default
    // pause, a shorter refresh.
    let (high, low, pause, refresh) = if part.is_pch1() {
        (
            PCH1_HIGH_WATER,
            PCH1_LOW_WATER,
            PCH1_PAUSE_TIME,
            PCH1_REFRESH_TIME,
        )
    } else {
        (FC_HIGH_WATER, FC_LOW_WATER, FC_PAUSE_TIME, FC_PAUSE_TIME)
    };
    hw.wr(PBA, PBA_KB);
    hw.wr(FCTTV, pause);
    hw.wr(FCRTV_PCH, refresh);
    hw.wr(FCRTL, low | FCRTL_XONE);
    hw.wr(FCRTH, high);
    flush(hw);
    if let Some(flag) = Swflag::take(hw) {
        flag.hv_write(BM_PORT_PAUSE_TIME, pause as u16);
    }
}

// The first PCH's flow control (Linux `e1000e_reset`, `e1000_pchlan`).
const PCH1_HIGH_WATER: u32 = 0x5000;
const PCH1_LOW_WATER: u32 = 0x3000;
const PCH1_PAUSE_TIME: u32 = 0x0680;
const PCH1_REFRESH_TIME: u32 = 0x1000;

// The 82577/82578 PHY registers of Linux `e1000_hv_phy_workarounds_ich8lan`
// and `e1000_k1_gig_workaround_hv`.
const HV_KMRN_MODE_CTRL: (u32, u32) = (769, 16);
const HV_KMRN_MDIO_SLOW: u16 = 1 << 10;
const BM_PORT_GEN_CFG: (u32, u32) = (769, 17);
const HV_EARLY_PREAMBLE: (u32, u32) = (769, 25);
const HV_KMRN_FIFO_CTRLSTA: (u32, u32) = (770, 16);
const HV_LINK_STALL: (u32, u32) = (770, 19);
const HV_LINK_STALL_UP: u16 = 0x0100;
const HV_LINK_STALL_DOWN: u16 = 0x4100;
const MII_PHYSID2: u32 = 0x03;
const MII_BMCR_RESET: u16 = 1 << 15;
/// The PHY's page-0 registers at MDIO address 2: the EMI window.
const HV_PHY_PAGE0: u8 = 2;
const EMI_ADDR: u32 = 0x10;
const EMI_DATA: u32 = 0x11;
const I82577_MSE_THRESHOLD: u16 = 0x0887;

/// What [`pch1_workarounds`] did, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pch1 {
    /// The PHY's revision (`PHYSID2`'s low nibble), when it answered.
    pub revision: Option<u16>,
    /// Whether the early-preamble and SSC tuning of those revisions went in.
    pub preamble: bool,
    /// Whether an 82578 of revision 1 was reset to its defaults.
    pub soft_reset: bool,
}

/// What Linux does to an 82577/82578 PHY after every reset
/// (`e1000_hv_phy_workarounds_ich8lan`): MDIO slow mode on an 82577 before
/// anything else on the PHY; early preamble off and preamble tuning on the
/// first revisions; an 82578 of revision 1 back to defaults; the link
/// stall fix as if the link were up; no link drops on a busy half-duplex
/// hub; a higher MSE threshold so the link stays up in noise.
fn pch1_workarounds<H: Hw>(hw: &H, part: Part) -> Option<Pch1> {
    let flag = Swflag::take(hw)?;
    let m82578 = part == Part::Pch82578;
    if !m82578 {
        if let Some(v) = flag.hv_read(HV_KMRN_MODE_CTRL) {
            flag.hv_write(HV_KMRN_MODE_CTRL, v | HV_KMRN_MDIO_SLOW);
        }
    }
    let revision = mdic_read(hw, HV_PHY_PAGE0, MII_PHYSID2)
        .filter(|&id| id != 0xffff)
        .map(|id| id & 0xf);
    let preamble = match revision {
        Some(r) => (!m82578 && (r == 1 || r == 2)) || (m82578 && r == 1),
        None => false,
    };
    if preamble {
        flag.hv_write(HV_EARLY_PREAMBLE, 0x4431);
        flag.hv_write(HV_KMRN_FIFO_CTRLSTA, 0xa204);
    }
    let soft_reset = m82578 && revision.is_some_and(|r| r < 2);
    if soft_reset {
        if let Some(bmcr) = mdic_read(hw, HV_PHY_PAGE0, MII_BMCR) {
            mdic_write(hw, HV_PHY_PAGE0, MII_BMCR, bmcr | MII_BMCR_RESET);
            for _ in 0..500 {
                hw.delay_us(1_000);
                if mdic_read(hw, HV_PHY_PAGE0, MII_BMCR).is_some_and(|b| b & MII_BMCR_RESET == 0) {
                    break;
                }
            }
        }
        mdic_write(hw, HV_PHY_PAGE0, MII_BMCR, 0x3140);
    }
    mdic_write(hw, HV_PHY, PHY_PAGE_SELECT, 0);
    flag.hv_write(HV_LINK_STALL, HV_LINK_STALL_UP);
    if let Some(v) = flag.hv_read(BM_PORT_GEN_CFG) {
        flag.hv_write(BM_PORT_GEN_CFG, v & 0x00ff);
    }
    // EMI: page 0 at address 2, above the registers every page has.
    if mdic_write(hw, HV_PHY_PAGE0, PHY_PAGE_SELECT, 0) {
        mdic_write(hw, HV_PHY_PAGE0, EMI_ADDR, I82577_MSE_THRESHOLD);
        mdic_write(hw, HV_PHY_PAGE0, EMI_DATA, 0x0034);
    }
    Some(Pch1 {
        revision,
        preamble,
        soft_reset,
    })
}

/// The first PCH's K1 erratum on every link change (Linux
/// `e1000_k1_gig_workaround_hv`): K1 off while the link is at gigabit, the
/// link stall fix for up or down.
pub fn pch1_link_changed<H: Hw>(hw: &H, part: Part, up_at: Option<u32>) {
    if !part.is_pch1() {
        return;
    }
    if let Some(flag) = Swflag::take(hw) {
        flag.hv_write(
            HV_LINK_STALL,
            if up_at.is_some() {
                HV_LINK_STALL_UP
            } else {
                HV_LINK_STALL_DOWN
            },
        );
    }
    configure_k1(hw, up_at != Some(1000));
}

/// Offer everything the PHY at `phy` can do -- every 10/100 mode, PAUSE
/// both ways, gigabit full duplex if it has it -- and restart
/// auto-negotiation. Bits are only added: firmware or a previous OS may
/// have taken gigabit away, and a restart alone does not give it back.
fn restart_autoneg<H: Hw>(hw: &H) -> Option<u8> {
    restart_autoneg_at(hw, &[2, 1])
}

/// [`restart_autoneg`] at the first of `phys` that answers, under the
/// SW/FW semaphore.
pub(super) fn restart_autoneg_at<H: Hw>(hw: &H, phys: &[u8]) -> Option<u8> {
    let _flag = Swflag::take(hw)?;
    for &phy in phys {
        let Some(bmcr) = mdic_read(hw, phy, MII_BMCR) else {
            continue;
        };
        if bmcr == 0xffff {
            continue;
        }
        if let Some(adv) = mdic_read(hw, phy, MII_ADVERTISE) {
            if adv != 0xffff && adv & ADVERTISE_ALL != ADVERTISE_ALL {
                mdic_write(hw, phy, MII_ADVERTISE, adv | ADVERTISE_ALL);
            }
        }
        let gigabit = mdic_read(hw, phy, MII_BMSR)
            .filter(|&b| b != 0xffff && b & BMSR_ESTATEN != 0)
            .and_then(|_| mdic_read(hw, phy, MII_ESTATUS))
            .is_some_and(|e| e != 0xffff && e & ESTATUS_1000_TFULL != 0);
        if gigabit {
            if let Some(c) = mdic_read(hw, phy, MII_CTRL1000) {
                if c != 0xffff && c & ADVERTISE_1000FULL == 0 {
                    mdic_write(hw, phy, MII_CTRL1000, c | ADVERTISE_1000FULL);
                }
            }
        }
        if mdic_write(hw, phy, MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART) {
            return Some(phy);
        }
    }
    None
}

/// What [`reset`] did that is worth a line in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub ulp: Ulp,
    /// Whether the card stopped mastering the bus before its reset.
    pub quiesced: bool,
    /// `HV_OEM_BITS` before and after, when it had to change.
    pub oem_bits: Option<(u16, u16)>,
    pub k1: bool,
    /// The MDIO address auto-negotiation was restarted at.
    pub autoneg: Option<u8>,
    /// What was done to the PHY of an 82577/82578.
    pub pch1: Option<Pch1>,
}

/// Reset a PCH part and get its PHY ready, up to where the rings are
/// programmed: the kernel driver's `reset_and_init`, steps 1 to 12.7.
/// Interrupts masked, receive and transmit off, the link on its way up.
pub fn reset<H: Hw>(hw: &H, part: Part) -> Result<Report, String> {
    hw.wr(super::IMC, u32::MAX);
    hw.wr(super::RCTL, 0);
    hw.wr(super::TCTL, super::TCTL_PSP);
    flush(hw);
    hw.delay_us(10_000);
    // Sunrise Point: the queues off before the reset.
    if part == Part::PchSpt {
        hw.wr(RXDCTL, hw.rd(RXDCTL) & !RXDCTL_QUEUE_ENABLE);
        hw.wr(TXDCTL, hw.rd(TXDCTL) & !TXDCTL_QUEUE_ENABLE);
        flush(hw);
        hw.delay_us(1_000);
    }
    // No wake-up filters left by firmware.
    hw.wr(WUC, 0);
    hw.wr(WUFC, 0);
    let ulp = disable_ulp(hw, part);
    toggle_lanphypc(hw, part);
    let quiesced = disable_pcie_master(hw);

    hw.wr(CTRL, hw.rd(CTRL) | CTRL_RST);
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
    hw.delay_us(10_000);
    hw.wr(super::IMC, u32::MAX);
    let _ = hw.rd(super::ICR);
    // Before anything else talks to an 82577/82578's PHY.
    let pch1 = if part.is_pch1() {
        pch1_workarounds(hw, part)
    } else {
        None
    };

    // Interrupts are not masked by their own delivery (the card's thread
    // reads `ICR` and asks again), relaxed ordering off, the driver loaded.
    let ext = (hw.rd(CTRL_EXT) | super::CTRL_EXT_RO_DIS | super::CTRL_EXT_DRV_LOAD)
        & !super::CTRL_EXT_IAME;
    hw.wr(CTRL_EXT, ext);
    hw.wr(super::IAM, 0);
    if part == Part::PchSpt {
        hw.wr(FEXTNVM6, hw.rd(FEXTNVM6) & !FEXTNVM6_K1_OFF_ENABLE);
        hw.wr(FEXTNVM7, hw.rd(FEXTNVM7) | FEXTNVM7_SIDE_CLK_UNGATE);
    }
    // LPLU and gigabit-disable off in the MAC's copy, then in the PHY's.
    hw.wr(
        PHY_CTRL,
        hw.rd(PHY_CTRL)
            & !(PHY_CTRL_D0A_LPLU
                | PHY_CTRL_NOND0A_LPLU
                | PHY_CTRL_GBE_DISABLE
                | PHY_CTRL_NOND0A_GBE_DISABLE),
    );
    let _ = hw.rd(PHY_CTRL);
    hw.delay_us(1_000);
    let oem_bits = oem_bits_config(hw);

    let ctrl = (hw.rd(CTRL) & !(CTRL_FRCSPD | CTRL_FRCDPX)) | super::CTRL_SLU | super::CTRL_ASDE;
    hw.wr(CTRL, ctrl);
    let _ = hw.rd(CTRL);
    flow_control(hw, part);
    kmrn_copper_timeouts(hw);
    let k1 = configure_k1(hw, true);
    let autoneg = restart_autoneg(hw);
    Ok(Report {
        ulp,
        quiesced,
        oem_bits,
        k1,
        autoneg,
        pch1,
    })
}

/// Start a flash cycle, the controller idle and its errors cleared.
fn flash_cycle_init<H: Hw>(hw: &H) -> bool {
    let hs = hw.rd(HSFSTS) & 0xffff;
    if hs & HSFSTS_FLDESVALID == 0 {
        return false;
    }
    hw.wr(HSFSTS, hs | HSFSTS_FLCERR | HSFSTS_DAEL);
    for _ in 0..10_000 {
        let hs = hw.rd(HSFSTS) & 0xffff;
        if hs & HSFSTS_FLCINPROG == 0 {
            hw.wr(HSFSTS, hs | HSFSTS_FLCDONE);
            return true;
        }
        hw.delay_us(1);
    }
    false
}

/// One dword of the NVM, at byte `at` (Linux `e1000_read_flash_data32`, a
/// Sunrise Point part: `HSFCTL` is the high half of the dword at `HSFSTS`).
fn flash_read<H: Hw>(hw: &H, at: u32) -> Option<u32> {
    for _ in 0..10 {
        if !flash_cycle_init(hw) {
            return None;
        }
        let mut ctl = (hw.rd(HSFSTS) >> 16) & 0xffff;
        // A read (cycle 0) of four bytes (count 3).
        ctl &= !((3 << 1) | (3 << 8) | 1);
        ctl |= 3 << 8;
        hw.wr(HSFSTS, ctl << 16);
        hw.wr(FADDR, at & FLASH_LINEAR_ADDR_MASK);
        hw.wr(HSFSTS, (ctl | 1) << 16);
        for _ in 0..2000 {
            let hs = hw.rd(HSFSTS);
            if hs & HSFSTS_FLCDONE != 0 {
                if hs & HSFSTS_FLCERR != 0 {
                    break;
                }
                return Some(hw.rd(FDATA0));
            }
            hw.delay_us(1);
        }
    }
    None
}

/// Where the valid bank of the NVM starts, in words: the bank whose word
/// 0x13 has the signature (Linux `e1000_valid_nvm_bank_detect_ich8lan`).
fn flash_bank<H: Hw>(hw: &H) -> u32 {
    let size = (((hw.rd(STRAP) >> 1) & 0x1f) + 1) * 4096;
    let bank_words = size / 4;
    let signed =
        |words: u32| flash_read(hw, (words + 0x13) << 1).is_some_and(|d| (d >> 8) & 0xc0 == 0x80);
    if signed(0) {
        0
    } else if bank_words > 0 && signed(bank_words) {
        bank_words
    } else {
        0
    }
}

/// The card's address from the NVM's first three words, on a Sunrise Point
/// part (older ones keep their flash behind BAR 1, which is not ours).
pub fn mac_from_flash<H: Hw>(hw: &H, part: Part) -> Option<[u8; 6]> {
    if part != Part::PchSpt {
        return None;
    }
    let bank = flash_bank(hw);
    let d0 = flash_read(hw, bank << 1)?;
    let d1 = flash_read(hw, (bank + 2) << 1)?;
    let [a, b, c, d] = d0.to_le_bytes();
    let [e, f, _, _] = d1.to_le_bytes();
    Some([a, b, c, d, e, f])
}

/// Which directions of PAUSE are on (Linux `e1000_fc_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pause {
    None,
    /// Ours honoured, never sent.
    Rx,
    /// Sent, the partner's ignored.
    Tx,
    Full,
}

/// Resolve flow control from what each end advertised (IEEE 802.3 Annex
/// 28B, Linux `e1000e_config_fc_after_link_up` asking for full). None on
/// half duplex, where PAUSE does not exist.
fn pause_for(adv: u16, lpa: u16, full_duplex: bool) -> Pause {
    if !full_duplex {
        return Pause::None;
    }
    let (cap, asym) = (
        adv & ADVERTISE_PAUSE_CAP != 0,
        adv & ADVERTISE_PAUSE_ASYM != 0,
    );
    let (lp_cap, lp_asym) = (
        lpa & ADVERTISE_PAUSE_CAP != 0,
        lpa & ADVERTISE_PAUSE_ASYM != 0,
    );
    if cap && lp_cap {
        Pause::Full
    } else if !cap && asym && lp_cap && lp_asym {
        Pause::Tx
    } else if cap && asym && !lp_cap && lp_asym {
        Pause::Rx
    } else {
        Pause::None
    }
}

/// The speed `STATUS` says, in Mb/s.
pub fn speed_of(status: u32) -> u32 {
    match (status >> 6) & 3 {
        0 => 10,
        1 => 100,
        _ => 1000,
    }
}

/// What the PHY says the two ends agreed on: the highest mode both
/// advertised, once auto-negotiation is complete.
fn negotiated(bmsr: u16, adv: u16, lpa: u16, ctrl1000: u16, stat1000: u16) -> Option<(u32, bool)> {
    if bmsr & BMSR_LSTATUS == 0 || bmsr & BMSR_ANEGCOMPLETE == 0 {
        return None;
    }
    if ctrl1000 & ADVERTISE_1000FULL != 0 && stat1000 & LPA_1000FULL != 0 {
        return Some((1000, true));
    }
    if ctrl1000 & ADVERTISE_1000HALF != 0 && stat1000 & LPA_1000HALF != 0 {
        return Some((1000, false));
    }
    let both = adv & lpa;
    [
        (LPA_100FULL, 100, true),
        (LPA_100HALF, 100, false),
        (LPA_10FULL, 10, true),
        (LPA_10HALF, 10, false),
    ]
    .into_iter()
    .find(|(bit, _, _)| both & bit != 0)
    .map(|(_, speed, fd)| (speed, fd))
}

/// What [`on_link_up`] found, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub speed: u32,
    pub full_duplex: bool,
    /// `None` when no PHY answered: the pause bits are left as they were.
    pub pause: Option<Pause>,
    /// What the PHY itself says, when it differs from the MAC's `STATUS`
    /// (the MAC-PHY interconnect at fault, not the wire).
    pub phy_says: Option<Option<(u32, bool)>>,
}

/// What Linux does on a PCH part each time its link comes up
/// (`e1000_check_for_copper_link_ich8lan`): the inter-packet gap for the
/// speed, the I217 beacon duration of its packet-loss erratum, and flow
/// control resolved against the partner into `CTRL`.
pub fn on_link_up<H: Hw>(hw: &H, part: Part, status: u32) -> Link {
    let speed = speed_of(status);
    let full_duplex = status & STATUS_FD != 0;
    let ipgt = if !full_duplex && speed == 10 {
        0xff
    } else if part == Part::PchSpt && full_duplex && speed != 1000 {
        0x0c
    } else {
        0x08
    };
    let tipg = hw.rd(TIPG);
    if tipg & 0x3ff != ipgt {
        hw.wr(TIPG, (tipg & !0x3ff) | ipgt);
    }
    let f4 = hw.rd(FEXTNVM4);
    let want = (f4 & !FEXTNVM4_BEACON_DURATION_MASK) | FEXTNVM4_BEACON_DURATION_8USEC;
    if want != f4 {
        hw.wr(FEXTNVM4, want);
    }
    resolve_pause(
        hw,
        &[2, 1],
        Link {
            speed,
            full_duplex,
            pause: None,
            phy_says: None,
        },
    )
}

/// Flow control resolved against what the partner advertised, read from
/// the first of `phys` that answers, into `CTRL`; and what that PHY says
/// was negotiated, when it is not what `link` (the MAC's `STATUS`) says.
pub(super) fn resolve_pause<H: Hw>(hw: &H, phys: &[u8], mut link: Link) -> Link {
    let Some(_flag) = Swflag::take(hw) else {
        return link;
    };
    let (speed, full_duplex) = (link.speed, link.full_duplex);
    for &phy in phys {
        let Some(bmsr) = mdic_read(hw, phy, MII_BMSR).filter(|&b| b != 0xffff) else {
            continue;
        };
        let reg = |r| mdic_read(hw, phy, r).unwrap_or(0);
        let (adv, lpa) = (reg(MII_ADVERTISE), reg(MII_LPA));
        let phy = negotiated(bmsr, adv, lpa, reg(MII_CTRL1000), reg(MII_STAT1000));
        if phy != Some((speed, full_duplex)) {
            link.phy_says = Some(phy);
        }
        let pause = pause_for(adv, lpa, full_duplex);
        let bits = match pause {
            Pause::None => 0,
            Pause::Rx => CTRL_RFCE,
            Pause::Tx => CTRL_TFCE,
            Pause::Full => CTRL_RFCE | CTRL_TFCE,
        };
        let ctrl = hw.rd(CTRL);
        let want = (ctrl & !(CTRL_RFCE | CTRL_TFCE)) | bits;
        if want != ctrl {
            hw.wr(CTRL, want);
            flush(hw);
        }
        link.pause = Some(pause);
        break;
    }
    link
}

#[cfg(test)]
mod tests;
