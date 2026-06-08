//! Intel e1000e NIC driver — simplified for I219 (PCH-SPT / 82574L / QEMU e1000)
//!
//! Based on the Onyx (heatd/Onyx) and LittleKernel e1000 reference drivers.
//! No AMT open sequence, no BM WUC filter management, no complex MDIO autoneg.
//! Reset → read MAC → init rings → enable RX/TX → done.

#![allow(unused_imports, dead_code)]

const E1000E_DRIVER_TAG: &str = "e1000e-simple";
const E1000E_WATCHDOG_PERIOD_US: u64 = 2_000_000;
const E1000E_WATCHDOG_FAST_US: u64 = 50_000;
const E1000E_LOG_VERBOSE: bool = false;

macro_rules! e1000e_vlog {
    ($($t:tt)*) => {
        if E1000E_LOG_VERBOSE { crate::klog_info!($($t)*); }
    };
}

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::size_of;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, fence, AtomicBool, Ordering};

use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities, Checksum};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result as SmolResult;

use crate::net::get_sockets;
use crate::scheme::{NetScheme, Scheme, SchemeUpcast, RouteInfo, NetStats};
use crate::{Device, DeviceError, DeviceResult};
use crate::bus::pci_drivers::PciDriver;
use crate::builder::IoMapper;
use crate::utils::dma::DmaRegion;
use crate::utils::dma_sync::{dma_sync_region, dma_sync_rx_desc_span, DmaSyncDir};
use pci::{PCIDevice, BAR, Location};
use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
use lock::Mutex;

use super::timer_now_as_micros;

// ---------------------------------------------------------------------------
// Register offsets (byte address / 4 → u32 index into MMIO array)
// ---------------------------------------------------------------------------
const E1000E_CTRL:      usize = 0x0000 / 4;
const E1000E_STATUS:    usize = 0x0008 / 4;
const E1000E_EECD:      usize = 0x0010 / 4;
const E1000E_EERD:      usize = 0x0014 / 4;
const E1000E_CTRL_EXT:  usize = 0x0018 / 4;
const E1000E_MDIC:      usize = 0x0020 / 4;
const E1000E_EXTCNF_CTRL: usize = 0x0F00 / 4;
const E1000E_PHY_CTRL:  usize = 0x00F10 / 4;
const E1000E_FEXTNVM6:  usize = 0x01014 / 4;
const E1000E_FEXTNVM7:  usize = 0x01018 / 4;
const E1000E_PBA:       usize = 0x01000 / 4;
const E1000E_ICR:       usize = 0x00C0 / 4;
const E1000E_ITR:       usize = 0x00C4 / 4;
const E1000E_IMS:       usize = 0x00D0 / 4;
const E1000E_IMC:       usize = 0x00D8 / 4;
const E1000E_RCTL:      usize = 0x0100 / 4;
const E1000E_TCTL:      usize = 0x0400 / 4;
const E1000E_TIPG:      usize = 0x0410 / 4;
const E1000E_RDBAL:     usize = 0x2800 / 4;
const E1000E_RDBAH:     usize = 0x2804 / 4;
const E1000E_RDLEN:     usize = 0x2808 / 4;
const E1000E_RDH:       usize = 0x2810 / 4;
const E1000E_RDT:       usize = 0x2818 / 4;
const E1000E_RDTR:      usize = 0x2820 / 4;
const E1000E_RXDCTL:    usize = 0x02828 / 4;
const E1000E_RADV:      usize = 0x282C / 4;
const E1000E_SRRCTL:    usize = 0x0280C / 4;
const E1000E_TDBAL:     usize = 0x3800 / 4;
const E1000E_TDBAH:     usize = 0x3804 / 4;
const E1000E_TDLEN:     usize = 0x3808 / 4;
const E1000E_TDH:       usize = 0x3810 / 4;
const E1000E_TDT:       usize = 0x3818 / 4;
const E1000E_TXDCTL:    usize = 0x03828 / 4;
const E1000E_TXDCTL1:   usize = E1000E_TXDCTL + (0x100 / 4);
const E1000E_TIDV:      usize = 0x03820 / 4;
const E1000E_TADV:      usize = 0x0382C / 4;
const E1000E_TARC0:     usize = 0x03840 / 4;
const E1000E_RAL0:      usize = 0x5400 / 4;
const E1000E_RAH0:      usize = 0x5404 / 4;
const E1000E_MTA_BASE:  usize = 0x5200 / 4;
const E1000E_RXCSUM:    usize = 0x5000 / 4;
const E1000E_RFCTL:     usize = 0x5008 / 4;
const E1000E_MRQC:      usize = 0x5818 / 4;
const E1000E_VET:       usize = 0x0038 / 4;
const E1000E_GPRC:      usize = 0x04074 / 4;
const E1000E_GPTC:      usize = 0x04080 / 4;
const E1000E_GORCL:     usize = 0x04088 / 4;
const E1000E_GORCH:     usize = 0x0408C / 4;
const E1000E_MPC:       usize = 0x04010 / 4;
const E1000E_WUC:       usize = 0x05800 / 4;
const E1000E_WUFC:      usize = 0x05808 / 4;
const E1000E_WUS:       usize = 0x05810 / 4;
const E1000E_MANC:      usize = 0x05820 / 4;
const E1000E_FWSM:      usize = 0x05B54 / 4;
const E1000E_H2ME:      usize = 0x05B50 / 4;
const E1000E_IOSFPC:    usize = 0x00F28 / 4;
const E1000E_VFTA_BASE: usize = 0x5600 / 4;

// CTRL register bits
const CTRL_FD:      u32 = 1 << 0;
const CTRL_ASDE:    u32 = 1 << 5;
const CTRL_SLU:     u32 = 1 << 6;
const CTRL_FRCSPD:  u32 = 1 << 12;
const CTRL_FRCDPX:  u32 = 1 << 11;
const CTRL_RST:     u32 = 1 << 26;
const CTRL_PHY_RST: u32 = 1 << 31;

// STATUS register bits
const STATUS_LU:    u32 = 1 << 1;
const STATUS_FD:    u32 = 1 << 0;

// CTRL_EXT bits
const CTRL_EXT_RO_DIS:   u32 = 1 << 17; // PCIe Relaxed Ordering Disable
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28; // Driver loaded (release ME)
const CTRL_EXT_IAME:     u32 = 1 << 27;

// PHY_CTRL (MAC-side register 0xF10) — LPLU bits for I219
const PHY_CTRL_D0A_LPLU:        u32 = 1 << 1;
const PHY_CTRL_NOND0A_LPLU:     u32 = 1 << 2;
const PHY_CTRL_NOND0A_GBE_DISABLE: u32 = 1 << 3;
const PHY_CTRL_GBE_DISABLE:     u32 = 1 << 6;

// MDIC register bits
const MDIC_REG_SHIFT:   u32 = 16;
const MDIC_PHYADD_SHIFT: u32 = 21;
const MDIC_OP_WRITE:    u32 = 1 << 26;
const MDIC_OP_READ:     u32 = 2 << 26;
const MDIC_READY:       u32 = 1 << 28;
const MDIC_ERROR:       u32 = 1 << 30;
const MDIC_POLL_TRIES:  u32 = 2000;

// PHY register 0 (BMCR)
const BMCR_RESET: u16 = 0x8000;

// EERD
const EERD_START:      u32 = 1 << 0;
const EERD_DONE_BIT4:  u32 = 1 << 4;
const EERD_DONE_BIT1:  u32 = 1 << 1;
const EERD_DATA_SHIFT: u32 = 16;

// ICR / IMS bits
const ICR_TXDW:   u32 = 1 << 0;
const ICR_LSC:    u32 = 1 << 2;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXT0:   u32 = 1 << 7;
const ICR_RX_ANY: u32 = ICR_RXT0 | ICR_RXDMT0;
const IMS_REARM:  u32 = ICR_TXDW | ICR_LSC | ICR_RXDMT0 | ICR_RXT0 | (1 << 6) | (1 << 8);

// RCTL bits
const RCTL_EN:    u32 = 1 << 1;
const RCTL_SBP:   u32 = 1 << 2;
const RCTL_UPE:   u32 = 1 << 3;
const RCTL_MPE:   u32 = 1 << 4;
const RCTL_BAM:   u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;

// TCTL bits
const TCTL_EN:   u32 = 1 << 1;
const TCTL_PSP:  u32 = 1 << 3;
const TCTL_RTLC: u32 = 1 << 24;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_CT_LINUX:   u32 = 15 << TCTL_CT_SHIFT;
const TCTL_COLD_LINUX: u32 = 63 << 12;

// TXDCTL / RXDCTL
const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const TXDCTL_FULL_TX_DESC_WB: u32 = 0x0101_0000;
const TXDCTL_DMA_BURST: u32 = (1 << 22) | (1 << 8) | 1; // wthresh=1, pthresh=1, hthresh=1

// RFCTL bits
const RFCTL_EXTEN:    u32 = 1 << 15;
const RFCTL_NFSW_DIS: u32 = 1 << 6;
const RFCTL_NFSR_DIS: u32 = 1 << 7;

// MANC
const MANC_EN_MNG2HOST: u32 = 1 << 21;

// TARC0
const TARC0_SPEED_MODE: u32 = 1 << 21;

// FWSM
const FWSM_FW_VALID: u32 = 1 << 14;

// ULP (Ultra Low Power) disable — i219/PCH-SPT only. On real hardware the ME
// firmware often leaves the PHY in ULP, so STATUS.LU never asserts. QEMU has no
// ME, which is why this is only needed on real hardware.
const ICH_FWSM_FW_VALID:          u32 = 0x0000_8000;
const FWSM_ULP_CFG_DONE:          u32 = 0x0000_0400;
const H2ME_ULP:                   u32 = 0x0000_0800;
const H2ME_ENFORCE_SETTINGS:      u32 = 0x0000_1000;
const FEXTNVM7_DISABLE_SMB_PERST: u32 = 0x0000_0020;
const CTRL_EXT_FORCE_SMBUS:       u32 = 0x0000_0800; // CTRL_EXT bit 11

// SW/FW semaphore (EXTCNF_CTRL)
const EXTCNF_CTRL_SWFLAG: u32 = 0x0000_0020; // bit 5

// HV PHY paged-register access: value at page-select reg = page << PHY_PAGE_SHIFT,
// then access (reg & MAX_PHY_REG_ADDRESS) via MDIC.
const PHY_PAGE_SHIFT:       u32 = 5;
const PHY_PAGE_SELECT_REG:  u32 = 0x1F; // IGP01E1000_PHY_PAGE_SELECT
const MAX_PHY_REG_ADDRESS:  u32 = 0x1F;
const MAX_PHY_MULTI_PAGE_REG: u32 = 0x0F;
const HV_PHY_ADDR:          u8  = 1;    // pages >= 768 live at PHY addr 1

// CV_SMB_CTRL = PHY_REG(769, 23)
const CV_SMB_CTRL_PAGE: u32 = 769;
const CV_SMB_CTRL_REG:  u32 = 23;
const CV_SMB_CTRL_FORCE_SMBUS: u16 = 0x0001;
// HV_PM_CTRL = PHY_REG(770, 17)
const HV_PM_CTRL_PAGE: u32 = 770;
const HV_PM_CTRL_REG:  u32 = 17;
const HV_PM_CTRL_K1_ENABLE: u16 = 0x4000;
// I218_ULP_CONFIG1 = PHY_REG(779, 16)
const ULP_CONFIG1_PAGE: u32 = 779;
const ULP_CONFIG1_REG:  u32 = 16;
const ULP_CONFIG1_START:             u16 = 0x0001;
const ULP_CONFIG1_IND:               u16 = 0x0004;
const ULP_CONFIG1_STICKY_ULP:        u16 = 0x0010;
const ULP_CONFIG1_INBAND_EXIT:       u16 = 0x0020;
const ULP_CONFIG1_WOL_HOST:          u16 = 0x0040;
const ULP_CONFIG1_RESET_TO_SMBUS:    u16 = 0x0100;
const ULP_CONFIG1_DISABLE_SMB_PERST: u16 = 0x1000;

// GIO master disable (quiesce DMA before CTRL_RST)
const CTRL_GIO_MASTER_DISABLE:  u32 = 0x0000_0004; // CTRL bit 2
const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000; // STATUS bit 19
const MASTER_DISABLE_TIMEOUT:   u32 = 800;

// Kumeran (KMRN) register access + K1 config
const E1000E_KMRNCTRLSTA:      usize = 0x0034 / 4;
const KMRNCTRLSTA_OFFSET_SHIFT: u32 = 16;
const KMRNCTRLSTA_OFFSET:       u32 = 0x001F_0000;
const KMRNCTRLSTA_REN:          u32 = 0x0020_0000;
const KMRNCTRLSTA_K1_CONFIG:    u32 = 0x7;
const KMRNCTRLSTA_K1_ENABLE:    u16 = 0x0002;
const CTRL_EXT_SPD_BYPS: u32 = 0x0000_8000;
const CTRL_SPD_1000:     u32 = 0x0000_0200;
const CTRL_SPD_100:      u32 = 0x0000_0100;

// LANPHYPC toggle — re-powers the PHY after it leaves ULP
const CTRL_LANPHYPC_OVERRIDE: u32 = 0x0001_0000; // CTRL bit 16
const CTRL_LANPHYPC_VALUE:    u32 = 0x0002_0000; // CTRL bit 17
const CTRL_EXT_LPCD:          u32 = 0x0000_0004; // CTRL_EXT bit 2 (link phy config done)

// MII BMCR (PHY register 0) — IEEE standard autoneg bits
const MII_CR_RESTART_AUTO_NEG: u16 = 0x0200;
const MII_CR_AUTO_NEG_EN:      u16 = 0x1000;

// Extended RX write-back layout (RFCTL_EXTEN=1):
//   +0:  addr u64 (driver writes)
//   +8:  staterr u32 (HW writes back: DD=bit0, EOP=bit1)
//   +12: length u16 (HW writes back)
const RXD_EXT_DD:  u32 = 1 << 0;
const RXD_EXT_EOP: u32 = 1 << 1;

// TX descriptor CMD bits
const TX_CMD_EOP:  u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS:   u8 = 1 << 3;

// DMA ring sizing
const NUM_RX: usize = 256;
const NUM_TX: usize = 256;
const BUF_SIZE: usize = 2048 + 128;
const DMA_RING_BYTES: usize    = NUM_RX * size_of::<RxDesc>();
const DMA_TX_RING_BYTES: usize = NUM_TX * size_of::<TxDesc>();
const DMA_DESC_ALIGN: usize = 16;
const CACHE_LINE_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// Descriptor layouts
// ---------------------------------------------------------------------------

/// Legacy RX descriptor (16 bytes). With RFCTL_EXTEN, HW writes extended WB
/// at +8 (staterr u32) and +12 (length u16).
#[repr(C, align(16))]
#[derive(Copy, Clone, Default)]
struct RxDesc {
    addr:     u64,  // buffer physical address (driver writes)
    reserved: u64,  // HW write-back: staterr[31:0] @ +8, length[15:0] @ +12
}
const _RX_DESC_SIZE: () = assert!(core::mem::size_of::<RxDesc>() == 16);

/// Legacy TX descriptor (16 bytes).
#[repr(C, align(16))]
#[derive(Copy, Clone, Default)]
struct TxDesc {
    addr:    u64,
    len:     u16,
    cso:     u8,
    cmd:     u8,
    status:  u8,
    css:     u8,
    special: u16,
}
const _TX_DESC_SIZE: () = assert!(core::mem::size_of::<TxDesc>() == 16);

// ---------------------------------------------------------------------------
// MMIO helpers
// ---------------------------------------------------------------------------

#[inline(always)]
unsafe fn mmio_read(base: usize, reg: usize) -> u32 {
    read_volatile((base + reg * 4) as *const u32)
}

#[inline(always)]
unsafe fn mmio_write(base: usize, reg: usize, val: u32) {
    write_volatile((base + reg * 4) as *mut u32, val);
}

// ---------------------------------------------------------------------------
// E1000eHw — hardware state
// ---------------------------------------------------------------------------

pub struct E1000eHw {
    base:    usize,
    pci_loc: Location,
    device_id: u16,

    mac: [u8; 6],

    rx_ring:         DmaRegion,
    rx_coherent:     bool,
    rx_buf_pool:     DmaRegion,
    rx_buf_coherent: bool,
    rx_next_to_clean: usize,
    rx_rdt_last_written: usize,

    tx_ring:     DmaRegion,
    tx_coherent:     bool,
    tx_buf_pool: DmaRegion,
    tx_buf_coherent: bool,
    tx_tail:     usize,

    pub stats: NetStats,

    rx_poll_budget: u8,
    link_up: bool,
    link_watchdog_next_us: u64,
}

impl E1000eHw {
    // -----------------------------------------------------------------------
    // Timing
    // -----------------------------------------------------------------------

    fn udelay(us: u64) {
        if us == 0 { return; }
        let t0 = timer_now_as_micros();
        const MAX_SPINS: u64 = 10_000_000;
        let mut n = 0u64;
        while timer_now_as_micros().wrapping_sub(t0) < us {
            core::hint::spin_loop();
            n += 1;
            if n >= MAX_SPINS { break; }
        }
    }

    // -----------------------------------------------------------------------
    // Buffer address helpers
    // -----------------------------------------------------------------------

    #[inline] fn rx_buf_paddr(&self, i: usize) -> u64 {
        (self.rx_buf_pool.paddr() + i * BUF_SIZE) as u64
    }
    #[inline] fn rx_buf_vaddr(&self, i: usize) -> usize {
        self.rx_buf_pool.vaddr() + i * BUF_SIZE
    }
    #[inline] fn tx_buf_paddr(&self, i: usize) -> u64 {
        (self.tx_buf_pool.paddr() + i * BUF_SIZE) as u64
    }
    #[inline] fn tx_buf_vaddr(&self, i: usize) -> usize {
        self.tx_buf_pool.vaddr() + i * BUF_SIZE
    }

    // -----------------------------------------------------------------------
    // Device family helpers
    // -----------------------------------------------------------------------

    fn is_pch(&self) -> bool {
        // I217 (PCH-LPT), I218, I219 (PCH-SPT+) — all PCH-integrated NICs
        matches!(self.device_id,
            0x1502 | 0x1503 |                       // I82579
            0x153a | 0x153b |                       // I217
            0x155a | 0x1559 |                       // I218-LM/V (PCH-LPT)
            0x15a0..=0x15a3 |                       // I218-x (PCH-LPT)
            0x156f | 0x1570 |                       // I219-LM/V (PCH-SPT step A)
            0x15b7..=0x15be |                       // I219-x (PCH-SPT / KBP)
            0x15d6..=0x15d8 |                       // I219-x (PCH-CNP)
            0x15e3 |
            0x0d4c..=0x0d4f |
            0x15f4..=0x15fc |
            0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 |
            0x550a..=0x5511 |
            0x57a0 | 0x57a1 |
            0x57b3..=0x57ba |
            0x15df..=0x15e2 |
            0x0d53 | 0x0d55
        )
    }

    fn is_pch_spt_or_later(&self) -> bool {
        // PCH-SPT (Sunrise Point, Kaby Lake, Coffee Lake, …) — device 0x156f+
        matches!(self.device_id,
            0x156f | 0x1570 |
            0x15b7..=0x15be |
            0x15d6..=0x15d8 |
            0x15e3 |
            0x0d4c..=0x0d4f |
            0x15f4..=0x15fc |
            0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 |
            0x550a..=0x5511 |
            0x57a0 | 0x57a1 |
            0x57b3..=0x57ba |
            0x15df..=0x15e2 |
            0x0d53 | 0x0d55
        )
    }

    // -----------------------------------------------------------------------
    // MDIC (MDIO) — used only for PHY soft reset
    // -----------------------------------------------------------------------

    unsafe fn mdic_write(&self, phy_addr: u8, reg: u32, val: u16) -> bool {
        let cmd = (val as u32)
            | (reg << MDIC_REG_SHIFT)
            | ((phy_addr as u32) << MDIC_PHYADD_SHIFT)
            | MDIC_OP_WRITE;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..MDIC_POLL_TRIES {
            Self::udelay(50);
            let v = mmio_read(self.base, E1000E_MDIC);
            if v & MDIC_READY != 0 {
                return v & MDIC_ERROR == 0;
            }
        }
        false
    }

    unsafe fn mdic_read(&self, phy_addr: u8, reg: u32) -> Option<u16> {
        let cmd = (reg << MDIC_REG_SHIFT)
            | ((phy_addr as u32) << MDIC_PHYADD_SHIFT)
            | MDIC_OP_READ;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..MDIC_POLL_TRIES {
            Self::udelay(50);
            let v = mmio_read(self.base, E1000E_MDIC);
            if v & MDIC_READY != 0 {
                if v & MDIC_ERROR != 0 { return None; }
                return Some(v as u16);
            }
        }
        None
    }

    // -----------------------------------------------------------------------
    // SW/FW semaphore (EXTCNF_CTRL.SWFLAG) — required before touching the PHY
    // on PCH parts while the ME firmware is active.
    // -----------------------------------------------------------------------

    unsafe fn acquire_swflag(&self) -> bool {
        let mut ext;
        let mut timeout = 100u32;
        loop {
            ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if ext & EXTCNF_CTRL_SWFLAG == 0 { break; }
            if timeout == 0 { return false; }
            Self::udelay(1_000);
            timeout -= 1;
        }
        ext |= EXTCNF_CTRL_SWFLAG;
        mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
        let mut timeout = 1_000u32;
        loop {
            ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if ext & EXTCNF_CTRL_SWFLAG != 0 { return true; }
            if timeout == 0 {
                ext &= !EXTCNF_CTRL_SWFLAG;
                mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
                return false;
            }
            Self::udelay(1_000);
            timeout -= 1;
        }
    }

    unsafe fn release_swflag(&self) {
        let ext = mmio_read(self.base, E1000E_EXTCNF_CTRL) & !EXTCNF_CTRL_SWFLAG;
        mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
    }

    // -----------------------------------------------------------------------
    // HV PHY paged register access (used only in the SW disable-ULP path).
    // Caller must hold the SW/FW semaphore.
    // -----------------------------------------------------------------------

    unsafe fn phy_read_hv(&self, page: u32, reg: u32) -> Option<u16> {
        if reg > MAX_PHY_MULTI_PAGE_REG
            && !self.mdic_write(HV_PHY_ADDR, PHY_PAGE_SELECT_REG, (page << PHY_PAGE_SHIFT) as u16)
        {
            return None;
        }
        self.mdic_read(HV_PHY_ADDR, reg & MAX_PHY_REG_ADDRESS)
    }

    unsafe fn phy_write_hv(&self, page: u32, reg: u32, val: u16) -> bool {
        if reg > MAX_PHY_MULTI_PAGE_REG
            && !self.mdic_write(HV_PHY_ADDR, PHY_PAGE_SELECT_REG, (page << PHY_PAGE_SHIFT) as u16)
        {
            return false;
        }
        self.mdic_write(HV_PHY_ADDR, reg & MAX_PHY_REG_ADDRESS, val)
    }

    // -----------------------------------------------------------------------
    // Disable ULP — port of Linux e1000_disable_ulp_lpt_lp().
    // On real i219 with active ME firmware the FW-handshake path runs (MMIO
    // only, no PHY access). The SW path is the fallback when no FW is present.
    // -----------------------------------------------------------------------

    unsafe fn disable_ulp(&self, force: bool) {
        if !self.is_pch_spt_or_later() { return; }

        let fwsm = mmio_read(self.base, E1000E_FWSM);
        if fwsm & ICH_FWSM_FW_VALID != 0 {
            // Firmware handshake path — ask the ME to un-configure ULP.
            if force {
                let mut h2me = mmio_read(self.base, E1000E_H2ME);
                h2me &= !H2ME_ULP;
                h2me |= H2ME_ENFORCE_SETTINGS;
                mmio_write(self.base, E1000E_H2ME, h2me);
            }
            // Poll up to ~400 ms for ME to clear ULP_CFG_DONE.
            let mut cleared = false;
            for _ in 0..40u32 {
                if mmio_read(self.base, E1000E_FWSM) & FWSM_ULP_CFG_DONE == 0 {
                    cleared = true;
                    break;
                }
                Self::udelay(10_000);
            }
            let mut h2me = mmio_read(self.base, E1000E_H2ME);
            if force { h2me &= !H2ME_ENFORCE_SETTINGS; } else { h2me &= !H2ME_ULP; }
            mmio_write(self.base, E1000E_H2ME, h2me);
            crate::klog_warn!("[e1000e] disable_ulp FW-path cfg_done_cleared={}\n", cleared);
            return;
        }

        // Software path — drive the PHY directly (no ME firmware present).
        if !self.acquire_swflag() {
            crate::klog_warn!("[e1000e] disable_ulp: SW/FW semaphore busy\n");
            return;
        }
        // Clear FORCE_SMBUS in the PHY.
        if let Some(mut p) = self.phy_read_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG) {
            p &= !CV_SMB_CTRL_FORCE_SMBUS;
            let _ = self.phy_write_hv(CV_SMB_CTRL_PAGE, CV_SMB_CTRL_REG, p);
        }
        // Unforce SMBus at the MAC.
        let ext = mmio_read(self.base, E1000E_CTRL_EXT) & !CTRL_EXT_FORCE_SMBUS;
        mmio_write(self.base, E1000E_CTRL_EXT, ext);
        // Re-enable K1 (ME disables it when entering ULP).
        if let Some(mut p) = self.phy_read_hv(HV_PM_CTRL_PAGE, HV_PM_CTRL_REG) {
            p |= HV_PM_CTRL_K1_ENABLE;
            let _ = self.phy_write_hv(HV_PM_CTRL_PAGE, HV_PM_CTRL_REG, p);
        }
        // Clear the ULP configuration and commit (START).
        if let Some(mut p) = self.phy_read_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG) {
            p &= !(ULP_CONFIG1_IND
                 | ULP_CONFIG1_STICKY_ULP
                 | ULP_CONFIG1_RESET_TO_SMBUS
                 | ULP_CONFIG1_WOL_HOST
                 | ULP_CONFIG1_INBAND_EXIT
                 | ULP_CONFIG1_DISABLE_SMB_PERST);
            let _ = self.phy_write_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG, p);
            p |= ULP_CONFIG1_START;
            let _ = self.phy_write_hv(ULP_CONFIG1_PAGE, ULP_CONFIG1_REG, p);
        }
        // Clear FEXTNVM7.DISABLE_SMB_PERST.
        let f7 = mmio_read(self.base, E1000E_FEXTNVM7) & !FEXTNVM7_DISABLE_SMB_PERST;
        mmio_write(self.base, E1000E_FEXTNVM7, f7);
        self.release_swflag();
        crate::klog_warn!("[e1000e] disable_ulp SW-path done\n");
    }

    // -----------------------------------------------------------------------
    // GIO master disable — quiesce in-flight DMA before CTRL_RST so the reset
    // doesn't fire while the device is mastering the bus (port of
    // e1000e_disable_pcie_master). Returns false if requests stay pending.
    // -----------------------------------------------------------------------

    unsafe fn disable_pcie_master(&self) -> bool {
        let ctrl = mmio_read(self.base, E1000E_CTRL) | CTRL_GIO_MASTER_DISABLE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        for _ in 0..MASTER_DISABLE_TIMEOUT {
            if mmio_read(self.base, E1000E_STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
                return true;
            }
            Self::udelay(100);
        }
        false
    }

    // -----------------------------------------------------------------------
    // Kumeran (KMRN) register access and K1 power-state config
    // (port of e1000_configure_k1_ich8lan). Caller need not hold the semaphore;
    // configure_k1 takes it internally.
    // -----------------------------------------------------------------------

    unsafe fn kmrn_read(&self, offset: u32) -> u16 {
        let cmd = ((offset << KMRNCTRLSTA_OFFSET_SHIFT) & KMRNCTRLSTA_OFFSET) | KMRNCTRLSTA_REN;
        mmio_write(self.base, E1000E_KMRNCTRLSTA, cmd);
        let _ = mmio_read(self.base, E1000E_STATUS); // flush
        Self::udelay(2);
        mmio_read(self.base, E1000E_KMRNCTRLSTA) as u16
    }

    unsafe fn kmrn_write(&self, offset: u32, data: u16) {
        let cmd = ((offset << KMRNCTRLSTA_OFFSET_SHIFT) & KMRNCTRLSTA_OFFSET) | data as u32;
        mmio_write(self.base, E1000E_KMRNCTRLSTA, cmd);
        let _ = mmio_read(self.base, E1000E_STATUS); // flush
        Self::udelay(2);
    }

    unsafe fn configure_k1(&self, enable: bool) {
        if !self.is_pch() { return; }
        if !self.acquire_swflag() {
            crate::klog_warn!("[e1000e] configure_k1: SW/FW semaphore busy\n");
            return;
        }
        let mut kmrn = self.kmrn_read(KMRNCTRLSTA_K1_CONFIG);
        if enable { kmrn |= KMRNCTRLSTA_K1_ENABLE; } else { kmrn &= !KMRNCTRLSTA_K1_ENABLE; }
        self.kmrn_write(KMRNCTRLSTA_K1_CONFIG, kmrn);
        Self::udelay(30);

        let ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        let ctrl_reg = mmio_read(self.base, E1000E_CTRL);
        let mut reg = ctrl_reg & !(CTRL_SPD_1000 | CTRL_SPD_100);
        reg |= CTRL_FRCSPD | CTRL_FRCDPX;
        mmio_write(self.base, E1000E_CTRL, reg);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext | CTRL_EXT_SPD_BYPS);
        let _ = mmio_read(self.base, E1000E_STATUS); // flush
        Self::udelay(30);
        // Restore CTRL / CTRL_EXT to the pre-K1 values (preserves our SLU+ASDE).
        mmio_write(self.base, E1000E_CTRL, ctrl_reg);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
        let _ = mmio_read(self.base, E1000E_STATUS); // flush
        Self::udelay(30);

        self.release_swflag();
    }

    // -----------------------------------------------------------------------
    // LANPHYPC toggle — force the PHY to re-run its power-up/config sequence
    // after leaving ULP (port of e1000_toggle_lanphypc_pch_lpt). MMIO only.
    // -----------------------------------------------------------------------

    unsafe fn toggle_lanphypc(&self) {
        if !self.is_pch() { return; }
        // Toggle LANPHYPC value bit with override asserted, then deasserted.
        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl |= CTRL_LANPHYPC_OVERRIDE;
        ctrl &= !CTRL_LANPHYPC_VALUE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_STATUS); // flush
        Self::udelay(20);
        ctrl &= !CTRL_LANPHYPC_OVERRIDE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_STATUS); // flush

        if self.is_pch_spt_or_later() {
            // PCH-LPT+: wait for the PHY config-done indication (LPCD), ~120 ms max.
            let mut count = 20u16;
            loop {
                Self::udelay(6_000);
                if mmio_read(self.base, E1000E_CTRL_EXT) & CTRL_EXT_LPCD != 0 { break; }
                if count == 0 { break; }
                count -= 1;
            }
            Self::udelay(30_000);
        } else {
            Self::udelay(50_000);
        }
    }

    // -----------------------------------------------------------------------
    // Restart auto-negotiation via the PHY BMCR (register 0). Tries both
    // possible PHY addresses and protects the access with the SW/FW semaphore.
    // -----------------------------------------------------------------------

    unsafe fn restart_autoneg(&self) {
        if !self.acquire_swflag() { return; }
        for phy_addr in [1u8, 2u8] {
            if let Some(bmcr) = self.mdic_read(phy_addr, 0) {
                if bmcr == 0xFFFF { continue; }
                let v = bmcr | MII_CR_AUTO_NEG_EN | MII_CR_RESTART_AUTO_NEG;
                if self.mdic_write(phy_addr, 0, v) {
                    crate::klog_warn!("[e1000e] restart autoneg on phy_addr={}\n", phy_addr);
                    break;
                }
            }
        }
        self.release_swflag();
    }

    // -----------------------------------------------------------------------
    // PHY soft reset — clears all PHY registers to power-on defaults
    // (including any BM WUC filters left by firmware)
    // -----------------------------------------------------------------------

    unsafe fn phy_soft_reset(&self) {
        // Try both possible PHY addresses
        for phy_addr in [1u8, 2u8] {
            // Read BMSR to check if PHY is present
            if self.mdic_read(phy_addr, 1).is_none() {
                continue;
            }
            // Write BMCR reset bit
            let _ = self.mdic_write(phy_addr, 0, BMCR_RESET);
            // Wait for reset to complete (up to 500ms)
            for _ in 0..500 {
                Self::udelay(1000);
                if let Some(bmcr) = self.mdic_read(phy_addr, 0) {
                    if bmcr & BMCR_RESET == 0 {
                        break;
                    }
                }
            }
        }
        // Allow PHY to settle
        Self::udelay(10_000);
    }

    // -----------------------------------------------------------------------
    // MAC address
    // -----------------------------------------------------------------------

    unsafe fn read_mac_from_hw(&mut self) {
        let ral = mmio_read(self.base, E1000E_RAL0);
        let rah = mmio_read(self.base, E1000E_RAH0);
        if ral == 0 && (rah & 0xFFFF) == 0 {
            // Try EERD as fallback
            self.read_mac_from_eeprom();
            return;
        }
        self.mac[0] = (ral & 0xFF) as u8;
        self.mac[1] = ((ral >> 8) & 0xFF) as u8;
        self.mac[2] = ((ral >> 16) & 0xFF) as u8;
        self.mac[3] = ((ral >> 24) & 0xFF) as u8;
        self.mac[4] = (rah & 0xFF) as u8;
        self.mac[5] = ((rah >> 8) & 0xFF) as u8;
    }

    unsafe fn read_mac_from_eeprom(&mut self) {
        for word in 0..3u16 {
            let w = self.eerd_read(word);
            if w == 0 || w == 0xFFFF {
                continue;
            }
            self.mac[(word as usize) * 2]     = (w & 0xFF) as u8;
            self.mac[(word as usize) * 2 + 1] = (w >> 8) as u8;
        }
    }

    unsafe fn eerd_read(&self, offset: u16) -> u16 {
        // Try shift-2 (most discrete e1000e)
        for shift in [2u32, 3u32] {
            let cmd = ((offset as u32) << shift) | EERD_START;
            mmio_write(self.base, E1000E_EERD, cmd);
            for _ in 0..2000u32 {
                Self::udelay(50);
                let v = mmio_read(self.base, E1000E_EERD);
                if v & (EERD_DONE_BIT4 | EERD_DONE_BIT1) != 0 {
                    return (v >> EERD_DATA_SHIFT) as u16;
                }
            }
        }
        0
    }

    fn is_valid_mac(&self) -> bool {
        let all_zeros = self.mac.iter().all(|&b| b == 0);
        let all_ff    = self.mac.iter().all(|&b| b == 0xFF);
        !all_zeros && !all_ff
    }

    // -----------------------------------------------------------------------
    // Main init — reset, configure, arm rings
    // -----------------------------------------------------------------------

    pub unsafe fn reset_and_init(&mut self) -> DeviceResult<()> {
        // 1. Mask all interrupts
        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
        let _ = mmio_read(self.base, E1000E_IMC);

        // 2. Disable RX / TX
        mmio_write(self.base, E1000E_RCTL, 0);
        mmio_write(self.base, E1000E_TCTL, TCTL_PSP);
        let _ = mmio_read(self.base, E1000E_STATUS);
        Self::udelay(10_000);

        // 3. Disable queue enables (I219 SPT must clear QUEUE_ENABLE before CTRL_RST)
        if self.is_pch_spt_or_later() {
            let rxdctl = mmio_read(self.base, E1000E_RXDCTL);
            mmio_write(self.base, E1000E_RXDCTL, rxdctl & !RXDCTL_QUEUE_ENABLE);
            let txdctl = mmio_read(self.base, E1000E_TXDCTL);
            mmio_write(self.base, E1000E_TXDCTL, txdctl & !TXDCTL_QUEUE_ENABLE);
            let _ = mmio_read(self.base, E1000E_STATUS);
            Self::udelay(1_000);
        }

        // 4. Clear WUC/WUFC so PHY WUC filter is disabled at the MAC level too
        mmio_write(self.base, E1000E_WUC,  0);
        mmio_write(self.base, E1000E_WUFC, 0);

        // 4.5 Disable ULP (i219 real hardware): bring the PHY out of Ultra Low
        //     Power mode so auto-negotiation can run and STATUS.LU can assert.
        //     No-op on QEMU/discrete parts (no ME firmware, not PCH-SPT).
        self.disable_ulp(true);

        // 4.6 Toggle LANPHYPC so the PHY re-runs its power-up/config sequence
        //     now that it is out of ULP.
        self.toggle_lanphypc();

        // 4.7 Quiesce in-flight DMA before resetting (GIO master disable).
        if !self.disable_pcie_master() {
            crate::klog_warn!("[e1000e] GIO master requests still pending before reset\n");
        }

        // 5. MAC reset (CTRL_RST)
        {
            let ctrl = mmio_read(self.base, E1000E_CTRL);
            mmio_write(self.base, E1000E_CTRL, ctrl | CTRL_RST);
        }
        // Wait for reset to self-clear
        for _ in 0..1000u32 {
            Self::udelay(1_000);
            if mmio_read(self.base, E1000E_CTRL) & CTRL_RST == 0 {
                break;
            }
        }
        Self::udelay(10_000);

        // 6. Mask interrupts again (reset clears IMC)
        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
        let _ = mmio_read(self.base, E1000E_ICR);

        // 7. Re-enable PCI bus master (CTRL_RST may disable it on I219)
        {
            let mut cmd = PCI_ACCESS.read16(&PortOpsImpl, self.pci_loc, 0x04);
            cmd |= 0x0004 | 0x0002; // Bus Master + Memory Space
            PCI_ACCESS.write16(&PortOpsImpl, self.pci_loc, 0x04, cmd);
        }

        // 8. CTRL_EXT: disable PCIe relaxed ordering, signal driver loaded
        {
            let mut ext = mmio_read(self.base, E1000E_CTRL_EXT);
            ext |= CTRL_EXT_RO_DIS | CTRL_EXT_DRV_LOAD;
            ext &= !CTRL_EXT_IAME;
            mmio_write(self.base, E1000E_CTRL_EXT, ext);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
        }

        // 9. FEXTNVM6/7 workarounds for PCH-SPT (Linux ich8lan.c)
        if self.is_pch_spt_or_later() {
            let fext6 = mmio_read(self.base, E1000E_FEXTNVM6);
            mmio_write(self.base, E1000E_FEXTNVM6, fext6 & !0x0000_0010); // clear bit 4
            let fext7 = mmio_read(self.base, E1000E_FEXTNVM7);
            mmio_write(self.base, E1000E_FEXTNVM7, fext7 | 0x0000_0001);  // set bit 0
        }

        // 10. Disable LPLU via MAC PHY_CTRL register (no MDIO needed)
        if self.is_pch() {
            let mut phy_ctrl = mmio_read(self.base, E1000E_PHY_CTRL);
            phy_ctrl &= !(PHY_CTRL_D0A_LPLU
                | PHY_CTRL_NOND0A_LPLU
                | PHY_CTRL_GBE_DISABLE
                | PHY_CTRL_NOND0A_GBE_DISABLE);
            mmio_write(self.base, E1000E_PHY_CTRL, phy_ctrl);
            let _ = mmio_read(self.base, E1000E_PHY_CTRL);
            Self::udelay(1_000);
        }

        // 11. Skip PHY soft reset — BMCR reset disrupts auto-negotiation (3-5 s) and may
        //     reload LPLU from NVM, permanently keeping the link down. The MAC-level
        //     CTRL_RST + PHY_CTRL LPLU clear is sufficient; the OSDev i219-V guide
        //     confirms this works without any PHY soft reset on real hardware.

        // 12. CTRL: SLU + ASDE, clear force-speed/duplex
        {
            let mut ctrl = mmio_read(self.base, E1000E_CTRL);
            ctrl &= !(CTRL_FRCSPD | CTRL_FRCDPX);
            ctrl |= CTRL_SLU | CTRL_ASDE;
            mmio_write(self.base, E1000E_CTRL, ctrl);
            let _ = mmio_read(self.base, E1000E_CTRL);
        }

        // 12.5 Configure K1 (Kumeran power state) to a known-good enabled state.
        //      Runs the FRCSPD/SPD_BYPS dance from Linux and restores CTRL.
        self.configure_k1(true);

        // 12.7 Kick off auto-negotiation explicitly via the PHY BMCR, in case
        //      SLU+ASDE alone didn't restart it after the ULP/LANPHYPC dance.
        self.restart_autoneg();

        // 13. Read MAC address
        self.read_mac_from_hw();
        if !self.is_valid_mac() {
            crate::klog_warn!("[e1000e] MAC all-zero/FF after reset — using placeholder\n");
            self.mac = [0x00, 0x0E, 0x10, 0xDE, 0xAD, 0x01];
        }

        // 14. Clear MTA (multicast table)
        for i in 0..128usize {
            mmio_write(self.base, E1000E_MTA_BASE + i, 0);
        }

        // 15. Clear VLAN filter table
        for i in 0..128usize {
            mmio_write(self.base, E1000E_VFTA_BASE + i, 0);
        }

        // 16. Disable VET (VLAN EtherType — use 0 for untagged)
        mmio_write(self.base, E1000E_VET, 0);

        // 17. Disable WUC at MAC level
        mmio_write(self.base, E1000E_WUC,  0);
        mmio_write(self.base, E1000E_WUFC, 0);

        // 18. Program TX ring
        self.init_tx();

        // 19. Program RX ring and enable
        self.init_rx();

        // 20. Enable interrupts
        compiler_fence(Ordering::SeqCst);
        mmio_write(self.base, E1000E_IMS, IMS_REARM);
        let _ = mmio_read(self.base, E1000E_IMS);

        // 21. Check link immediately
        let status = mmio_read(self.base, E1000E_STATUS);
        self.link_up = status & STATUS_LU != 0;
        crate::klog_warn!(
            "[e1000e] reset_and_init done: STATUS={:#010x} LU={} GPRC={} tag={}\n",
            status,
            self.link_up,
            mmio_read(self.base, E1000E_GPRC),
            E1000E_DRIVER_TAG
        );

        Ok(())
    }

    unsafe fn init_tx(&mut self) {
        // Program TX ring base, length, head, tail
        let tx_pa = self.tx_ring.paddr();
        mmio_write(self.base, E1000E_TDBAL, tx_pa as u32);
        mmio_write(self.base, E1000E_TDBAH, (tx_pa >> 32) as u32);
        mmio_write(self.base, E1000E_TDLEN, (NUM_TX * size_of::<TxDesc>()) as u32);
        mmio_write(self.base, E1000E_TDH, 0);
        mmio_write(self.base, E1000E_TDT, 0);
        self.tx_tail = 0;

        dma_sync_rx_desc_span(
            &self.tx_ring,
            self.tx_coherent,
            0,
            NUM_TX,
            size_of::<TxDesc>(),
            DmaSyncDir::ToDevice,
        );

        // Timers
        mmio_write(self.base, E1000E_TIDV, 0);
        mmio_write(self.base, E1000E_TADV, 0);

        // Inter-Packet Gap (GbE standard values from datasheet)
        mmio_write(self.base, E1000E_TIPG, 8 | (8 << 10) | (12 << 20));

        // TXDCTL
        if self.is_pch_spt_or_later() {
            // PCH-SPT: must set QUEUE_ENABLE (bit 25) and wait for it to latch
            let txdctl = TXDCTL_DMA_BURST | TXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
            for _ in 0..100u32 {
                Self::udelay(100);
                if mmio_read(self.base, E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE != 0 {
                    break;
                }
            }
            // Mirror to queue 1 (Linux e1000_configure_tx)
            mmio_write(self.base, E1000E_TXDCTL1, mmio_read(self.base, E1000E_TXDCTL));
            // IOSF PCIe compliance
            let iosfpc = mmio_read(self.base, E1000E_IOSFPC);
            mmio_write(self.base, E1000E_IOSFPC, iosfpc | 0x0001_0000);
            let _ = mmio_read(self.base, E1000E_IOSFPC);
        } else {
            mmio_write(self.base, E1000E_TXDCTL, TXDCTL_DMA_BURST | TXDCTL_FULL_TX_DESC_WB);
        }

        // TCTL: enable TX
        let tctl = TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT_LINUX | TCTL_COLD_LINUX;
        mmio_write(self.base, E1000E_TCTL, tctl);
        let _ = mmio_read(self.base, E1000E_TCTL);
    }

    unsafe fn init_rx(&mut self) {
        // Timers off
        mmio_write(self.base, E1000E_RDTR, 0);
        mmio_write(self.base, E1000E_RADV, 0);
        
        // Write ITR to throttle interrupts (e.g. 250 translates to ~15,625 interrupts/sec,
        // which is 250 * 256 ns = 64 us interval).
        mmio_write(self.base, E1000E_ITR, 250);

        // Program RX ring base, length, head
        let rx_pa = self.rx_ring.paddr();
        mmio_write(self.base, E1000E_RDBAL, rx_pa as u32);
        mmio_write(self.base, E1000E_RDBAH, (rx_pa >> 32) as u32);
        mmio_write(self.base, E1000E_RDLEN, (NUM_RX * size_of::<RxDesc>()) as u32);
        mmio_write(self.base, E1000E_RDH, 0);
        self.rx_next_to_clean = 0;

        // Fill all RX descriptors with buffer addresses (status = 0)
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        for i in 0..NUM_RX {
            write_volatile(&mut (*ring.add(i)).addr, self.rx_buf_paddr(i));
            write_volatile(&mut (*ring.add(i)).reserved, 0);
        }
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);

        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.rx_coherent,
            0,
            NUM_RX,
            size_of::<RxDesc>(),
            DmaSyncDir::ToDevice,
        );

        // Disable checksum offload
        mmio_write(self.base, E1000E_RXCSUM, 0);

        // RFCTL: use legacy write-back (no RFCTL_EXTEN) — matches OSDev i219-V guide.
        // Extended WB and legacy WB differ in descriptor layout; legacy is simpler and
        // confirmed to work on real i219-V hardware without RFCTL_EXTEN.
        let mut rfctl = mmio_read(self.base, E1000E_RFCTL);
        rfctl &= !RFCTL_EXTEN;  // legacy mode: status at +12 u8, length at +8 u16
        rfctl |= RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
        mmio_write(self.base, E1000E_RFCTL, rfctl);
        let _ = mmio_read(self.base, E1000E_RFCTL);

        // No multiqueue
        mmio_write(self.base, E1000E_MRQC, 0);

        // SRRCTL: 2 KB buffer size
        mmio_write(self.base, E1000E_SRRCTL, 2 | (1 << 31)); // 2 KB, Drop_En

        // Configure RXDCTL for DMA burst (Linux RXDCTL_DMA_BURST_ENABLE)
        let mut rxdctl = 0x0104_0420; // Granularity=1, WTHRESH=4, HTHRESH=4, PTHRESH=32
        if self.is_pch_spt_or_later() {
            rxdctl |= RXDCTL_QUEUE_ENABLE;
        }
        mmio_write(self.base, E1000E_RXDCTL, rxdctl);
        if self.is_pch_spt_or_later() {
            for _ in 0..100u32 {
                Self::udelay(100);
                if mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE != 0 {
                    break;
                }
            }
        }

        // Doorbell: give (NUM_RX - 1) descriptors to hardware
        // RDT = last descriptor index hardware can use
        mmio_write(self.base, E1000E_RDT, (NUM_RX - 1) as u32);
        let _ = mmio_read(self.base, E1000E_RDT); // flush
        self.rx_rdt_last_written = NUM_RX - 1;

        // Small settle before enabling RCTL
        Self::udelay(1_000);

        // RCTL: enable RX, promiscuous, broadcast, strip CRC, 2 KB buffers
        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SECRC;
        mmio_write(self.base, E1000E_RCTL, rctl);
        let _ = mmio_read(self.base, E1000E_RCTL); // flush

        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // RX data path
    // -----------------------------------------------------------------------

    pub unsafe fn flush_rx_ring(&mut self) {
        let rdt = (self.rx_next_to_clean + NUM_RX - 1) % NUM_RX;
        if rdt != self.rx_rdt_last_written {
            fence(Ordering::SeqCst);
            mmio_write(self.base, E1000E_RDT, rdt as u32);
            self.rx_rdt_last_written = rdt;
        }
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        if self.rx_poll_budget == 0 {
            unsafe { self.flush_rx_ring(); }
            return None;
        }

        let i = self.rx_next_to_clean;
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let desc_ptr = unsafe { ring.add(i) };

        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.rx_coherent,
            i,
            1,
            size_of::<RxDesc>(),
            DmaSyncDir::FromDevice,
        );

        // Legacy descriptor write-back layout (no RFCTL_EXTEN):
        //   +8  u16  length
        //   +12 u8   status  (DD=bit0, EOP=bit1)
        compiler_fence(Ordering::SeqCst);
        let status = unsafe { read_volatile((desc_ptr as usize + 12) as *const u8) };
        if status & 1 == 0 {
            // DD (Descriptor Done) not set — hardware hasn't written back yet
            unsafe { self.flush_rx_ring(); }
            return None;
        }

        let len = unsafe { read_volatile((desc_ptr as usize + 8) as *const u16) } as usize;

        self.rx_poll_budget = self.rx_poll_budget.saturating_sub(1);

        // EOP (End Of Packet) must be set; if not, this is a jumbo fragment we can't reassemble.
        if status & 2 == 0 || len == 0 || len > BUF_SIZE {
            unsafe { self.recycle_rx_slot(i); }
            self.rx_next_to_clean = (i + 1) % NUM_RX;
            unsafe { self.flush_rx_ring(); }
            return None;
        }

        dma_sync_region(
            &self.rx_buf_pool,
            self.rx_buf_coherent,
            i * BUF_SIZE,
            len,
            DmaSyncDir::FromDevice,
        );

        // Copy packet from buffer
        let packet = unsafe {
            core::slice::from_raw_parts(self.rx_buf_vaddr(i) as *const u8, len).to_vec()
        };

        self.stats.rx_packets += 1;
        self.stats.rx_bytes += len as u64;

        // Recycle: clear descriptor, post new buffer
        unsafe { self.recycle_rx_slot(i); }
        self.rx_next_to_clean = (i + 1) % NUM_RX;

        Some(packet)
    }

    unsafe fn recycle_rx_slot(&self, i: usize) {
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let desc_ptr = ring.add(i);
        // Clear the status byte (legacy WB: status at +12) so hardware can reuse slot.
        // Re-post buffer address (addr at +0).
        write_volatile((desc_ptr as usize + 12) as *mut u8, 0u8);
        compiler_fence(Ordering::SeqCst);
        write_volatile(&mut (*desc_ptr).addr, self.rx_buf_paddr(i));

        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.rx_coherent,
            i,
            1,
            size_of::<RxDesc>(),
            DmaSyncDir::ToDevice,
        );
    }

    // -----------------------------------------------------------------------
    // TX data path
    // -----------------------------------------------------------------------

    fn tx_slots_free(&self) -> usize {
        let head = unsafe { mmio_read(self.base, E1000E_TDH) as usize };
        let tail = self.tx_tail;
        if tail >= head {
            NUM_TX.saturating_sub(tail - head).saturating_sub(1)
        } else {
            head.saturating_sub(tail).saturating_sub(1)
        }
    }

    fn can_send(&self) -> bool {
        self.tx_slots_free() > 0
    }

    pub fn send(&mut self, data: &[u8]) -> DeviceResult {
        if data.is_empty() || data.len() > BUF_SIZE {
            return Err(DeviceError::InvalidParam);
        }

        // Check link via STATUS register
        if !self.link_up {
            let status = unsafe { mmio_read(self.base, E1000E_STATUS) };
            if status & STATUS_LU != 0 {
                self.link_up = true;
            } else {
                return Err(DeviceError::NotReady);
            }
        }

        if !self.can_send() {
            return Err(DeviceError::NotReady);
        }

        let idx = self.tx_tail;
        let ring = self.tx_ring.as_ptr::<TxDesc>();
        let desc = unsafe { &mut *ring.add(idx) };

        // Copy packet to TX buffer
        let buf = unsafe {
            core::slice::from_raw_parts_mut(self.tx_buf_vaddr(idx) as *mut u8, data.len())
        };
        buf.copy_from_slice(data);

        dma_sync_region(
            &self.tx_buf_pool,
            self.tx_buf_coherent,
            idx * BUF_SIZE,
            data.len(),
            DmaSyncDir::ToDevice,
        );

        self.stats.tx_packets += 1;
        self.stats.tx_bytes += data.len() as u64;

        // Write descriptor fields (cmd last so HW doesn't fetch partial descriptor)
        unsafe {
            write_volatile(&mut desc.addr,    self.tx_buf_paddr(idx));
            write_volatile(&mut desc.len,     data.len() as u16);
            write_volatile(&mut desc.cso,     0);
            write_volatile(&mut desc.status,  0);
            write_volatile(&mut desc.css,     0);
            write_volatile(&mut desc.special, 0);
        }
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
        unsafe { write_volatile(&mut desc.cmd, TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS); }
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);

        dma_sync_rx_desc_span(
            &self.tx_ring,
            self.tx_coherent,
            idx,
            1,
            size_of::<TxDesc>(),
            DmaSyncDir::ToDevice,
        );

        self.tx_tail = (idx + 1) % NUM_TX;
        unsafe { mmio_write(self.base, E1000E_TDT, self.tx_tail as u32); }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Watchdog — simple link check
    // -----------------------------------------------------------------------

    /// Returns `true` when carrier state changed (for Eclipse Pulse `PULSE_LINK`).
    pub unsafe fn watchdog_tick(&mut self) -> bool {
        let status = mmio_read(self.base, E1000E_STATUS);
        let link = status & STATUS_LU != 0;
        let mut link_changed = false;

        if link != self.link_up {
            link_changed = true;
            self.link_up = link;
            if link {
                crate::klog_warn!("[e1000e] link UP STATUS={:#010x}\n", status);
            } else {
                crate::klog_warn!("[e1000e] link DOWN\n");
            }
        }

        // Log GPRC/MPC periodically to diagnose RX issues.
        // GPRC>0 means the MAC received frames. MPC>0 means frames arrived but
        // were dropped (no free descriptors or DMA ring not armed).
        let gprc = mmio_read(self.base, E1000E_GPRC);
        let mpc  = mmio_read(self.base, E1000E_MPC);
        crate::klog_warn!("[e1000e] watchdog: link={} GPRC={} MPC={} rx_pkt={}\n",
            link, gprc, mpc, self.stats.rx_packets);
        link_changed
    }

    fn merged_stats(&self) -> NetStats {
        self.stats.clone()
    }
}

// ---------------------------------------------------------------------------
// Eclipse OS driver wrappers
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct E1000eDriver {
    pub hw: Arc<Mutex<E1000eHw>>,
}

#[derive(Clone)]
pub struct E1000eInterface {
    pub iface: Arc<Mutex<Interface<'static, E1000eDriver>>>,
    pub driver: E1000eDriver,
    pub name: String,
    pub irq: usize,
    pub base: usize,
    pub poll_pending: Arc<AtomicBool>,
    pub link_up_seen: Arc<AtomicBool>,
    watchdog_job_scheduled: Arc<AtomicBool>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl E1000eInterface {
    /// Notify Eclipse Pulse from thread/deferred context (never from raw IRQ).
    fn pulse(&self, bits: u32) {
        if bits != 0 {
            crate::pulse::pulse_signal(bits);
        }
    }

    pub fn schedule_watchdog(&self, fast: bool) {
        let now = timer_now_as_micros();
        {
            let mut hw = self.driver.hw.lock();
            if now < hw.link_watchdog_next_us && !fast {
                return;
            }
            if fast {
                hw.link_watchdog_next_us = now.saturating_add(E1000E_WATCHDOG_FAST_US);
            } else if now >= hw.link_watchdog_next_us {
                hw.link_watchdog_next_us = now.saturating_add(E1000E_WATCHDOG_PERIOD_US);
            }
        }
        if self.watchdog_job_scheduled.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            struct Guard(Arc<AtomicBool>);
            impl Drop for Guard { fn drop(&mut self) { self.0.store(false, Ordering::Release); } }
            let _g = Guard(Arc::clone(&me.watchdog_job_scheduled));
            me.watchdog_job_scheduled.store(false, Ordering::Release);
            let (link_changed, link_up) = {
                let mut hw = me.driver.hw.lock();
                let changed = unsafe { hw.watchdog_tick() };
                (changed, hw.link_up)
            };
            if link_changed {
                me.link_up_seen.store(link_up, Ordering::Release);
                me.pulse(crate::pulse::PULSE_LINK);
            }
            me.schedule_watchdog(false);
        });
    }

    fn ims_rearm(&self) {
        unsafe {
            compiler_fence(Ordering::SeqCst);
            mmio_write(self.base, E1000E_IMS, IMS_REARM);
            let _ = mmio_read(self.base, E1000E_IMS);
            fence(Ordering::SeqCst);
        }
    }

    fn queue_deferred_poll(&self) {
        if self.poll_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let poll_pending = self.poll_pending.clone();
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            let _ = me.poll_with_irq_hint(0);
            poll_pending.store(false, Ordering::SeqCst);
        });
    }

    /// NIC poll; `irq_icr` carries ICR bits when invoked from the deferred IRQ bottom-half.
    fn poll_with_irq_hint(&self, irq_icr: u32) -> DeviceResult {
        use crate::pulse::{PULSE_LINK, PULSE_NET_RX};

        let now = timer_now_as_micros();
        let due = self.driver.hw.lock().link_watchdog_next_us <= now;
        if due {
            self.schedule_watchdog(false);
        }

        let mut pulse = 0u32;
        if irq_icr & ICR_LSC != 0 {
            pulse |= PULSE_LINK;
        }
        if irq_icr & ICR_RX_ANY != 0 {
            pulse |= PULSE_NET_RX;
        }

        let ts = Instant::from_micros(now as i64);
        unsafe { self.driver.hw.lock().ensure_rx_armed_if_link_up(); }
        {
            let mut hw = self.driver.hw.lock();
            hw.rx_poll_budget = 32;
        }

        // Mutex::lock() uses push_off/pop_off — do not mix manual intr_off/intr_on here.
        let sockets = get_sockets();
        {
            let mut sockets = sockets.lock();
            match self.iface.lock().poll(&mut sockets, ts) {
                Ok(true) => pulse |= PULSE_NET_RX,
                Ok(false) => {}
                Err(e) => warn!("e1000e smoltcp poll: {:?}", e),
            }
        }
        super::net_flush_deferred_packets();

        if self.driver.hw.lock().rx_poll_budget == 0 {
            self.queue_deferred_poll();
        } else {
            self.ims_rearm();
        }

        self.pulse(pulse);
        if pulse & PULSE_NET_RX != 0 {
            super::wake_net_rx_waiters();
        }
        Ok(())
    }
}

impl Scheme for E1000eInterface {
    fn name(&self) -> &str { "e1000e" }

    /// Minimal IRQ top-half (same as [`e1000::E1000Interface`]): read ICR, mask IMS,
    /// queue one deferred poll. Pulse is signaled from [`E1000eInterface::poll_with_irq_hint`]
    /// in thread context — never here (avoids `RefCell already borrowed`).
    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        let icr = unsafe { mmio_read(self.base, E1000E_ICR) };
        if icr == 0 {
            if !self.poll_pending.load(Ordering::SeqCst) {
                self.ims_rearm();
            }
            return;
        }

        if self.poll_pending.load(Ordering::SeqCst) {
            self.ims_rearm();
            return;
        }

        self.poll_pending.store(true, Ordering::SeqCst);
        unsafe {
            mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
            let _ = mmio_read(self.base, E1000E_IMC);
            fence(Ordering::SeqCst);
        }

        let poll_pending = self.poll_pending.clone();
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            if icr & ICR_LSC != 0 {
                me.schedule_watchdog(true);
            }
            let _ = me.poll_with_irq_hint(icr);
            poll_pending.store(false, Ordering::SeqCst);
        });
    }
}

impl NetScheme for E1000eInterface {
    fn get_mac(&self) -> EthernetAddress {
        self.iface.lock().ethernet_addr()
    }
    fn get_ifname(&self) -> String {
        self.name.clone()
    }
    fn get_ip_address(&self) -> Vec<IpCidr> {
        self.ip_addrs.lock().clone()
    }
    fn set_ipv4_address(&self, cidr: Ipv4Cidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            let mut set_primary = false;
            for slot in addrs.iter_mut() {
                if let IpCidr::Ipv4(_) = slot {
                    if !set_primary {
                        *slot = IpCidr::Ipv4(cidr);
                        set_primary = true;
                    } else {
                        *slot = IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address::UNSPECIFIED, 0));
                    }
                }
            }
            if !set_primary {
                if let Some(slot) = addrs.iter_mut().next() {
                    *slot = IpCidr::Ipv4(cidr);
                }
            }
        });
        let addrs_vec = iface.ip_addrs().to_vec();
        *self.ip_addrs.lock() = addrs_vec;
        Ok(())
    }
    fn add_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            if addrs.contains(&cidr) { return; }
            for slot in addrs.iter_mut() {
                if slot.address().is_unspecified() && slot.prefix_len() == 0 {
                    *slot = cidr;
                    return;
                }
            }
            if let Some(slot) = addrs.iter_mut().last() {
                *slot = cidr;
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
    }
    fn remove_ip_address(&self, cidr: IpCidr) -> DeviceResult {
        let mut iface = self.iface.lock();
        iface.update_ip_addrs(|addrs| {
            for slot in addrs.iter_mut() {
                if *slot == cidr {
                    *slot = IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0);
                    return;
                }
            }
        });
        *self.ip_addrs.lock() = iface.ip_addrs().to_vec();
        Ok(())
    }
    fn seed_neighbor(
        &self,
        protocol: smoltcp::wire::IpAddress,
        hardware: smoltcp::wire::EthernetAddress,
    ) -> DeviceResult {
        let ts = Instant::from_micros(timer_now_as_micros() as i64);
        self.iface.lock().seed_neighbor(protocol, hardware, ts);
        Ok(())
    }
    fn refresh_link(&self) -> DeviceResult {
        {
            let mut hw = self.driver.hw.lock();
            hw.link_up = false;
        }
        self.schedule_watchdog(true);
        Ok(())
    }
    fn link_carrier_up(&self) -> bool {
        self.driver.hw.lock().link_up
            || unsafe { mmio_read(self.base, E1000E_STATUS) & STATUS_LU != 0 }
    }
    fn poll(&self) -> DeviceResult {
        self.poll_with_irq_hint(0)
    }
    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        if let Some(pkt) = self.driver.hw.lock().receive() {
            let n = pkt.len().min(buf.len());
            buf[..n].copy_from_slice(&pkt[..n]);
            Ok(n)
        } else {
            Err(DeviceError::NotReady)
        }
    }
    fn send(&self, data: &[u8]) -> DeviceResult<usize> {
        let mut hw = self.driver.hw.lock();
        hw.send(data)?;
        Ok(data.len())
    }
    fn can_recv(&self) -> bool { true }
    fn can_send(&self) -> bool { self.driver.hw.lock().can_send() }
    fn add_route(&self, cidr: IpCidr, gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        match gateway {
            Some(IpAddress::Ipv4(gw)) => {
                if cidr.prefix_len() == 0 {
                    iface.routes_mut().add_default_ipv4_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                }
                let mut routes = self.routes.lock();
                routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0));
                routes.push(RouteInfo { dst: cidr, gateway: Some(IpAddress::Ipv4(gw)) });
            }
            Some(IpAddress::Ipv6(gw)) => {
                if cidr.prefix_len() == 0 {
                    iface.routes_mut().add_default_ipv6_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                }
                let mut routes = self.routes.lock();
                routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0));
                routes.push(RouteInfo { dst: cidr, gateway: Some(IpAddress::Ipv6(gw)) });
            }
            None => { self.routes.lock().push(RouteInfo { dst: cidr, gateway }); }
            _ => {}
        }
        Ok(())
    }
    fn del_route(&self, cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        if cidr.prefix_len() == 0 {
            match cidr {
                IpCidr::Ipv4(_) => { let _ = iface.routes_mut().remove_default_ipv4_route(); }
                _ => {}
            }
        }
        self.routes.lock().retain(|r| r.dst != cidr);
        Ok(())
    }
    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        let mut res = self.routes.lock().clone();
        for cidr in iface.ip_addrs() {
            match cidr {
                IpCidr::Ipv4(v4) if v4.prefix_len() > 0 => {
                    res.push(RouteInfo { dst: IpCidr::Ipv4(v4.network()), gateway: None });
                }
                IpCidr::Ipv6(v6) if v6.prefix_len() > 0 => {
                    res.push(RouteInfo { dst: IpCidr::Ipv6(v6.network()), gateway: None });
                }
                _ => {}
            }
        }
        res
    }
    fn get_stats(&self) -> NetStats { self.driver.hw.lock().merged_stats() }
    fn get_mtu(&self) -> usize { 1500 }
}

// ---------------------------------------------------------------------------
// smoltcp Device impl
// ---------------------------------------------------------------------------

pub struct E1000eRxToken { data: Vec<u8> }
pub struct E1000eTxToken(E1000eDriver);

impl phy::Device<'_> for E1000eDriver {
    type RxToken = E1000eRxToken;
    type TxToken = E1000eTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let mut hw = self.hw.lock();
        hw.receive().map(|pkt| (E1000eRxToken { data: pkt }, E1000eTxToken(self.clone())))
    }
    fn transmit(&mut self) -> Option<Self::TxToken> {
        if self.hw.lock().can_send() {
            Some(E1000eTxToken(self.clone()))
        } else {
            None
        }
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(64);
        caps
    }
}

impl phy::RxToken for E1000eRxToken {
    fn consume<R, F>(self, _ts: Instant, f: F) -> SmolResult<R>
    where F: FnOnce(&mut [u8]) -> SmolResult<R> {
        let mut data = self.data;
        super::net_defer_packet(&data);
        f(&mut data)
    }
}

impl phy::TxToken for E1000eTxToken {
    fn consume<R, F>(self, _ts: Instant, len: usize, f: F) -> SmolResult<R>
    where F: FnOnce(&mut [u8]) -> SmolResult<R> {
        let len = len.min(65536);
        let mut buf = vec![0u8; len];
        let result = f(&mut buf)?;
        let mut hw = self.0.hw.lock();
        hw.send(&buf).map_err(|_| smoltcp::Error::Exhausted)?;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Helper: ensure RX ring is armed when link comes up
// ---------------------------------------------------------------------------

impl E1000eHw {
    pub unsafe fn ensure_rx_armed_if_link_up(&mut self) {
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU != 0 {
            self.link_up = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Public init — called from pci.rs
// ---------------------------------------------------------------------------

pub fn init(
    name: String,
    pci: &PCIDevice,
    irq: usize,
    vaddr: usize,
    _index: usize,
) -> DeviceResult<E1000eInterface> {
    crate::klog_warn!(
        "[e1000e] probing {} vaddr={:#x} irq={} device={:#x} tag={}\n",
        name, vaddr, irq, pci.id.device_id, E1000E_DRIVER_TAG
    );

    let (rx_ring, rx_coherent)      = DmaRegion::alloc_uninit_try_coherent(NUM_RX * size_of::<RxDesc>())
        .ok_or(DeviceError::DmaError)?;
    let (tx_ring, tx_coherent)      = DmaRegion::alloc_uninit_try_coherent(NUM_TX * size_of::<TxDesc>())
        .ok_or(DeviceError::DmaError)?;
    let (rx_buf_pool, rx_buf_coherent)  = DmaRegion::alloc_uninit_try_coherent(NUM_RX * BUF_SIZE)
        .ok_or(DeviceError::DmaError)?;
    let (tx_buf_pool, tx_buf_coherent)  = DmaRegion::alloc_uninit_try_coherent(NUM_TX * BUF_SIZE)
        .ok_or(DeviceError::DmaError)?;

    // Alignment checks
    for (label, region, align, span) in [
        ("rx_ring",    &rx_ring,    DMA_DESC_ALIGN, DMA_RING_BYTES),
        ("tx_ring",    &tx_ring,    DMA_DESC_ALIGN, DMA_TX_RING_BYTES),
        ("rx_buf_pool",&rx_buf_pool,64,             NUM_RX * BUF_SIZE),
        ("tx_buf_pool",&tx_buf_pool,64,             NUM_TX * BUF_SIZE),
    ] {
        if region.paddr() % align != 0 || region.vaddr() % align != 0 {
            crate::klog_err!("[e1000e] {} DMA misaligned\n", label);
            return Err(DeviceError::DmaError);
        }
        if region.byte_len() < span {
            crate::klog_err!("[e1000e] {} too small\n", label);
            return Err(DeviceError::DmaError);
        }
    }

    let mut hw = E1000eHw {
        base: vaddr,
        pci_loc: pci.loc,
        device_id: pci.id.device_id,
        mac: [0u8; 6],
        rx_ring,
        rx_coherent,
        rx_buf_pool,
        rx_buf_coherent,
        rx_next_to_clean: 0,
        rx_rdt_last_written: NUM_RX - 1,
        tx_ring,
        tx_coherent,
        tx_buf_pool,
        tx_buf_coherent,
        tx_tail: 0,
        stats: NetStats::default(),
        rx_poll_budget: 32,
        link_up: false,
        link_watchdog_next_us: 0,
    };

    unsafe { hw.reset_and_init()?; }

    let mac_bytes = hw.mac;
    crate::klog_warn!(
        "e1000e: {} {:#x}:{:#x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} tag={}\n",
        name,
        pci.id.vendor_id,
        pci.id.device_id,
        mac_bytes[0], mac_bytes[1], mac_bytes[2],
        mac_bytes[3], mac_bytes[4], mac_bytes[5],
        E1000E_DRIVER_TAG
    );

    let hw_arc = Arc::new(Mutex::new(hw));
    let driver = E1000eDriver { hw: hw_arc.clone() };

    let ethernet_addr = EthernetAddress::from_bytes(&mac_bytes);

    // IPv6 link-local from EUI-64
    let mut eui64 = [0u8; 8];
    eui64[0] = mac_bytes[0] ^ 2;
    eui64[1] = mac_bytes[1];
    eui64[2] = mac_bytes[2];
    eui64[3] = 0xff;
    eui64[4] = 0xfe;
    eui64[5] = mac_bytes[3];
    eui64[6] = mac_bytes[4];
    eui64[7] = mac_bytes[5];
    let link_local = Ipv6Address::new(
        0xfe80, 0, 0, 0,
        (eui64[0] as u16) << 8 | eui64[1] as u16,
        (eui64[2] as u16) << 8 | eui64[3] as u16,
        (eui64[4] as u16) << 8 | eui64[5] as u16,
        (eui64[6] as u16) << 8 | eui64[7] as u16,
    );

    let ip_addrs = vec![
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::Ipv6(Ipv6Cidr::new(link_local, 64)),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
        IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
    ];
    let default_v4_gw = Ipv4Address::new(0, 0, 0, 0);
    static mut ROUTES_STORAGE: [Option<(IpCidr, Route)>; 4] = [None; 4];
    let mut routes = unsafe { Routes::new(&mut ROUTES_STORAGE[..]) };
    routes.add_default_ipv4_route(default_v4_gw).unwrap();
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    let iface = InterfaceBuilder::new(driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs.clone())
        .routes(routes)
        .finalize();

    let link_up_seen = Arc::new(AtomicBool::new(
        unsafe { mmio_read(vaddr, E1000E_STATUS) & STATUS_LU != 0 },
    ));
    let e1000e_iface = E1000eInterface {
        iface: Arc::new(Mutex::new(iface)),
        driver,
        name,
        irq,
        base: vaddr,
        poll_pending: Arc::new(AtomicBool::new(false)),
        link_up_seen,
        watchdog_job_scheduled: Arc::new(AtomicBool::new(false)),
        routes: Arc::new(Mutex::new(vec![RouteInfo {
            dst: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            gateway: Some(IpAddress::Ipv4(default_v4_gw)),
        }])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    };

    Ok(e1000e_iface)
}

// ---------------------------------------------------------------------------
// PCI driver registration
// ---------------------------------------------------------------------------

pub struct E1000eDriverPci;

impl PciDriver for E1000eDriverPci {
    fn name(&self) -> &str { "e1000e" }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        if vendor_id != 0x8086 { return false; }
        matches!(
            device_id,
            0x10d3 | 0x10f5 | 0x150c |
            0x1533 | 0x1539 | 0x157b | 0x157c |
            0x1502..=0x1503 | 0x153a..=0x153b | 0x155a | 0x1559 |
            0x15a0..=0x15a3 | 0x156f..=0x1570 | 0x15b7..=0x15be |
            0x15d6..=0x15d8 | 0x15e3 |
            0x0d4c..=0x0d4f | 0x15f4..=0x15fc | 0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 | 0x550a..=0x5511 | 0x57a0..=0x57a1 |
            0x57b3..=0x57ba | 0x15df..=0x15e2 | 0x0d53 | 0x0d55
        )
    }

    fn init(&self, dev: &PCIDevice, mapper: &Option<Arc<dyn IoMapper>>, irq: Option<usize>) -> DeviceResult<Device> {
        crate::klog_warn!(
            "e1000e: probe PCI {:#x}:{:#x} tag={}\n",
            dev.id.vendor_id, dev.id.device_id, E1000E_DRIVER_TAG
        );
        let bar0_addr = if let Some(BAR::Memory(a, _, _, _)) = dev.bars[0] {
            a as usize
        } else {
            return Err(DeviceError::IoError);
        };

        if let Some(m) = mapper {
            m.query_or_map(bar0_addr, 128 * 1024);
        }

        let vaddr = crate::net::phys_to_virt(bar0_addr);
        let name = alloc::format!("eth{}", dev.loc.bus);

        unsafe {
            let mut cmd = PCI_ACCESS.read16(&PortOpsImpl, dev.loc, 0x04);
            cmd |= 0x0004 | 0x0002;
            PCI_ACCESS.write16(&PortOpsImpl, dev.loc, 0x04, cmd);
        }

        let vector = irq.map(|idx| idx + 32).unwrap_or(0);
        let iface = init(name, dev, vector, vaddr, 0)?;
        let iface_arc = Arc::new(iface);
        iface_arc.schedule_watchdog(true);
        if vector != 0 {
            crate::net::pci_note_pending_msi(vector, iface_arc.clone());
        }
        Ok(Device::Net(iface_arc))
    }
}