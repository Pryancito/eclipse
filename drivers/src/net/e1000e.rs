//! Intel e1000e NIC driver — simplified for I219 (PCH-SPT / 82574L / QEMU e1000)
//!
//! Based on the Onyx (heatd/Onyx) and LittleKernel e1000 reference drivers.
//! No AMT open sequence, no BM WUC filter management, no complex MDIO autoneg.
//! Reset → read MAC → init rings → enable RX/TX → done.

#![allow(unused_imports, dead_code)]

const E1000E_DRIVER_TAG: &str = "e1000e-lk-rx3";
const E1000E_WATCHDOG_PERIOD_US: u64 = 2_000_000;
const E1000E_WATCHDOG_FAST_US: u64 = 50_000;
const E1000E_WATCHDOG_LOG_US: u64 = 5_000_000;
const E1000E_LOG_VERBOSE: bool = false;
const E1000E_ITR_LOW_LATENCY: u32 = 98;
const E1000E_ITR_BALANCED: u32 = 195;
/// ITR register units are 256 ns. 2000 → ~512 µs (~2 k interrupts/s), the
/// Linux e1000e bulk-transfer ballpark. The previous 512 (~131 µs) still
/// interrupted too often once a download was in THROUGHPUT mode.
const E1000E_ITR_THROUGHPUT: u32 = 2000;
/// Period between full ITR window samples. Shorter than the old 250 ms so
/// apk/wget bursts climb into THROUGHPUT coalescing sooner without waiting
/// a quarter-second on BALANCED.
const E1000E_ITR_TUNE_PERIOD_US: u64 = 100_000;
/// Packets drained in one poll that force an immediate THROUGHPUT upgrade.
const E1000E_ITR_BURST_THROUGHPUT: u64 = 8;
/// Packets per tune window that enter / reaffirm THROUGHPUT.
const E1000E_ITR_WINDOW_THROUGHPUT: u64 = 32;
/// Quiet window → LOW_LATENCY (interactive / ACK-sensitive).
const E1000E_ITR_WINDOW_LOW: u64 = 4;
/// Hysteresis floor: stay on THROUGHPUT until the window falls below this.
const E1000E_ITR_WINDOW_HOLD: u64 = 16;
/// If `poll_pending` (see [`E1000eInterface`]) has stayed true this long, the
/// IRQ bottom-half that owns clearing it was evicted from the shared
/// deferred-job queue and will never run — self-heal instead of staying
/// interrupt-deaf forever. Deferred jobs normally drain within a scheduler
/// tick or two, so this is comfortably above any legitimate latency.
const POLL_PENDING_STUCK_US: u64 = 500_000;

// ---------------------------------------------------------------------------
// PCI device identity — single source of truth for matched / is_pch / SPT+
// ---------------------------------------------------------------------------
// Scope 1A: QEMU 82574L/LA (+ 82583V) and PCH-integrated I217/I218/I219
// (Linux board_pch_lpt … board_pch_ptp). Explicitly excludes igb (I210/I211)
// and ICH9 0x10F5.

const E1000E_MTA_REG_COUNT: usize = 128;

/// Discrete copper parts this driver owns (Linux board_82574 / board_82583).
#[inline]
fn e1000e_is_discrete(device_id: u16) -> bool {
    matches!(device_id, 0x10d3 | 0x10f6 | 0x150c) // 82574L, 82574LA, 82583V
}

/// PCH-integrated copper: I217 / I218 / I219 (+ 82579 already used on some boards).
#[inline]
fn e1000e_is_pch(device_id: u16) -> bool {
    matches!(
        device_id,
        0x1502 | 0x1503 | // 82579 (pch2lan)
            0x153a | 0x153b | // I217
            0x155a | 0x1559 | // I218-LM/V
            0x15a0..=0x15a3 | // I218-x
            0x156f | 0x1570 | // I219 SPT
            0x15b7..=0x15be | // I219 SPT-H / CNP
            0x15d6..=0x15d8 | 0x15e3 | // I219 SPT later
            0x15df..=0x15e2 | // I219 ICP
            0x0d4c..=0x0d4f | 0x0d53 | 0x0d55 | // I219 CMP
            0x15f4..=0x15fc | // I219 TGP
            0x1a1c..=0x1a1f | // I219 ADP
            0x0dc5..=0x0dc8 | // I219 RPL
            0x550a..=0x5511 | // I219 MTP/LNP/ADP
            0x57a0 | 0x57a1 | // I219 ARL
            0x57b3..=0x57ba // I219 PTP/NVL
    )
}

/// Sunrise Point and later — need RXDCTL.QUEUE_ENABLE and flash@BAR0+0xE000.
#[inline]
fn e1000e_is_pch_spt_or_later(device_id: u16) -> bool {
    matches!(
        device_id,
        0x156f | 0x1570 |
            0x15b7..=0x15be |
            0x15d6..=0x15d8 | 0x15e3 |
            0x15df..=0x15e2 |
            0x0d4c..=0x0d4f | 0x0d53 | 0x0d55 |
            0x15f4..=0x15fc |
            0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 |
            0x550a..=0x5511 |
            0x57a0 | 0x57a1 |
            0x57b3..=0x57ba
    )
}

#[inline]
fn e1000e_device_matched(device_id: u16) -> bool {
    e1000e_is_discrete(device_id) || e1000e_is_pch(device_id)
}

/// Linux `e1000_hash_mc_addr` with `mc_filter_type == 0` and 128 MTA regs.
#[inline]
fn e1000e_hash_mc_addr(mc_addr: &[u8; 6]) -> u32 {
    let hash_mask = (E1000E_MTA_REG_COUNT as u32 * 32) - 1;
    let mut bit_shift = 0u8;
    while hash_mask >> bit_shift != 0xFF {
        bit_shift += 1;
    }
    hash_mask & (((mc_addr[4] as u32) >> (8 - bit_shift)) | ((mc_addr[5] as u32) << bit_shift))
}

/// Pick the next ITR setting from traffic samples (pure; unit-tested).
///
/// `rx_burst` is packets observed in the current poll; `rx_delta` is packets
/// over the completed tune window. Burst upgrades take priority so a single
/// heavy drain under download load does not wait for the next period.
fn choose_itr(current: u32, rx_delta: u64, rx_burst: u64) -> u32 {
    if rx_burst >= E1000E_ITR_BURST_THROUGHPUT {
        return E1000E_ITR_THROUGHPUT;
    }
    if rx_delta >= E1000E_ITR_WINDOW_THROUGHPUT {
        return E1000E_ITR_THROUGHPUT;
    }
    if current == E1000E_ITR_THROUGHPUT && rx_delta >= E1000E_ITR_WINDOW_HOLD {
        return E1000E_ITR_THROUGHPUT;
    }
    if rx_delta <= E1000E_ITR_WINDOW_LOW {
        return E1000E_ITR_LOW_LATENCY;
    }
    E1000E_ITR_BALANCED
}

macro_rules! e1000e_vlog {
    ($($t:tt)*) => {
        if E1000E_LOG_VERBOSE { crate::klog_info!($($t)*); }
    };
}

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{size_of, MaybeUninit};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, fence, AtomicBool, AtomicU32, AtomicU64, Ordering};

use smoltcp::iface::*;
use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::*;
use smoltcp::Result as SmolResult;

use crate::builder::IoMapper;
use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
use crate::bus::pci_drivers::PciDriver;
use crate::net::get_sockets;
use crate::scheme::{NetScheme, NetStats, RouteInfo, Scheme, SchemeUpcast};
use crate::utils::dma::DmaRegion;
use crate::utils::dma_sync::{dma_sync_region, dma_sync_rx_desc_span, DmaSyncDir};
use crate::{Device, DeviceError, DeviceResult};
use lock::Mutex;
use pci::{Location, PCIDevice, BAR};

use super::timer_now_as_micros;

// ---------------------------------------------------------------------------
// Register offsets (byte address / 4 → u32 index into MMIO array)
// ---------------------------------------------------------------------------
const E1000E_CTRL: usize = 0x0000 / 4;
const E1000E_STATUS: usize = 0x0008 / 4;
const E1000E_STRAP: usize = 0x0000C / 4;
/// SPT+ flash register window relative to BAR0 (Linux E1000_FLASH_BASE_ADDR).
const E1000E_FLASH_BASE_OFF: usize = 0xE000;
const ICH_FLASH_HSFSTS: usize = 0x04;
const ICH_FLASH_FADDR: usize = 0x08;
const ICH_FLASH_FDATA0: usize = 0x10;
const ICH_FLASH_LINEAR_ADDR_MASK: u32 = 0x00FF_FFFF;
const HSFSTS_FLCDONE: u16 = 1 << 0;
const HSFSTS_FLCERR: u16 = 1 << 1;
const HSFSTS_DAEL: u16 = 1 << 2;
const HSFSTS_FLCINPROG: u16 = 1 << 5;
const HSFSTS_FLDESVALID: u16 = 1 << 14;
const E1000E_EERD: usize = 0x0014 / 4;
const E1000E_CTRL_EXT: usize = 0x0018 / 4;
const E1000E_MDIC: usize = 0x0020 / 4;
const E1000E_EXTCNF_CTRL: usize = 0x0F00 / 4;
const E1000E_PHY_CTRL: usize = 0x00F10 / 4;
// Real offsets per Linux e1000e's regs.h — 0x01014/0x01018 (this file's
// previous values) land in reserved MMIO next to PBA, not on FEXTNVM6/7, so
// the ULP/SPT workarounds that RMW them were silently no-ops.
const E1000E_FEXTNVM: usize = 0x00028 / 4;
const E1000E_FEXTNVM4: usize = 0x00024 / 4;
/// FEXTNVM4[2:0] — beacon duration. The I217 packet-loss erratum wants 8 usec.
const FEXTNVM4_BEACON_DURATION_MASK: u32 = 0x7;
const FEXTNVM4_BEACON_DURATION_8USEC: u32 = 0x7;
/// FEXTNVM bit 27 — software configuration of the LAN Connected Device is
/// enabled. Linux only rewrites the OEM bits when this is set.
const FEXTNVM_SW_CONFIG_ICH8M: u32 = 1 << 27;
const E1000E_FEXTNVM6: usize = 0x00010 / 4;
const E1000E_FEXTNVM7: usize = 0x000E4 / 4;
const E1000E_PBA: usize = 0x01000 / 4;
const E1000E_ICR: usize = 0x00C0 / 4;
const E1000E_ITR: usize = 0x00C4 / 4;
const E1000E_IMS: usize = 0x00D0 / 4;
const E1000E_IAM: usize = 0x00E0 / 4;
const E1000E_IMC: usize = 0x00D8 / 4;
const E1000E_RCTL: usize = 0x0100 / 4;
const E1000E_TCTL: usize = 0x0400 / 4;
const E1000E_TIPG: usize = 0x0410 / 4;
/// TIPG[9:0] — IPGT, the transmit inter-packet gap (Linux `E1000_TIPG_IPGT_MASK`).
const TIPG_IPGT_MASK: u32 = 0x0000_03FF;
const E1000E_RDBAL: usize = 0x2800 / 4;
const E1000E_RDBAH: usize = 0x2804 / 4;
const E1000E_RDLEN: usize = 0x2808 / 4;
const E1000E_RDH: usize = 0x2810 / 4;
const E1000E_RDT: usize = 0x2818 / 4;
const E1000E_RDTR: usize = 0x2820 / 4;
const E1000E_RXDCTL: usize = 0x02828 / 4;
const E1000E_RADV: usize = 0x282C / 4;
const E1000E_SRRCTL: usize = 0x0280C / 4;
const E1000E_TDBAL: usize = 0x3800 / 4;
const E1000E_TDBAH: usize = 0x3804 / 4;
const E1000E_TDLEN: usize = 0x3808 / 4;
const E1000E_TDH: usize = 0x3810 / 4;
const E1000E_TDT: usize = 0x3818 / 4;
const E1000E_TXDCTL: usize = 0x03828 / 4;
const E1000E_TXDCTL1: usize = E1000E_TXDCTL + (0x100 / 4);
const E1000E_TIDV: usize = 0x03820 / 4;
const E1000E_TADV: usize = 0x0382C / 4;
const E1000E_TARC0: usize = 0x03840 / 4;
const E1000E_RAL0: usize = 0x5400 / 4;
const E1000E_RAH0: usize = 0x5404 / 4;
const E1000E_MTA_BASE: usize = 0x5200 / 4;
const E1000E_RXCSUM: usize = 0x5000 / 4;
const E1000E_RFCTL: usize = 0x5008 / 4;
const E1000E_MRQC: usize = 0x5818 / 4;
const E1000E_VET: usize = 0x0038 / 4;
const E1000E_GPRC: usize = 0x04074 / 4;
const E1000E_GPTC: usize = 0x04080 / 4;
const E1000E_GORCL: usize = 0x04088 / 4;
const E1000E_GORCH: usize = 0x0408C / 4;
const E1000E_GOTCL: usize = 0x04090 / 4;
const E1000E_GOTCH: usize = 0x04094 / 4;
/// Receive No Buffers Count — frames the MAC had to hold because the driver
/// had not returned descriptors fast enough.
const E1000E_RNBC: usize = 0x040A0 / 4;
const E1000E_MPC: usize = 0x04010 / 4;
const E1000E_WUC: usize = 0x05800 / 4;
const E1000E_WUFC: usize = 0x05808 / 4;
const E1000E_WUS: usize = 0x05810 / 4;
const E1000E_MANC: usize = 0x05820 / 4;
const E1000E_FWSM: usize = 0x05B54 / 4;
const E1000E_H2ME: usize = 0x05B50 / 4;
const E1000E_IOSFPC: usize = 0x00F28 / 4;
const E1000E_VFTA_BASE: usize = 0x5600 / 4;

// CTRL register bits
const CTRL_FD: u32 = 1 << 0;
const CTRL_ASDE: u32 = 1 << 5;
const CTRL_SLU: u32 = 1 << 6;
// Per the Intel datasheet / Linux e1000_hw.h, FRCSPD is bit 11 (0x800) and
// FRCDPX is bit 12 (0x1000) — these were previously swapped. Harmless while
// both are only ever set/cleared together (as a pair), but wrong for any
// code that toggles one alone.
const CTRL_FRCSPD: u32 = 1 << 11;
const CTRL_FRCDPX: u32 = 1 << 12;
const CTRL_RST: u32 = 1 << 26;
const CTRL_PHY_RST: u32 = 1 << 31;

// STATUS register bits
const STATUS_LU: u32 = 1 << 1;
const STATUS_FD: u32 = 1 << 0;
/// STATUS[7:6] — negotiated link speed (00 = 10, 01 = 100, 1x = 1000 Mb/s).
const STATUS_SPEED_MASK: u32 = 0x0000_00C0;
const STATUS_SPEED_SHIFT: u32 = 6;

/// Negotiated link speed in Mb/s, decoded from a STATUS snapshot.
fn status_speed_mbps(status: u32) -> u32 {
    match (status & STATUS_SPEED_MASK) >> STATUS_SPEED_SHIFT {
        0 => 10,
        1 => 100,
        _ => 1000,
    }
}

/// Value to write into the PHY page-select register for an HV paged access.
///
/// Port of the `if (page == HV_INTC_FC_PAGE_START) page = 0;` line in Linux's
/// `__e1000_read_phy_reg_hv` / `__e1000_write_phy_reg_hv`. Page 768 is the one
/// page that is *not* addressed by its own number: it is the PHY's page 0 seen
/// at MDIO address 1, so selecting 768 lands somewhere else entirely and every
/// read comes back as another page's register. That matters for `HV_OEM_BITS`
/// (768, 25), which is where LPLU actually lives.
fn hv_page_select(page: u32) -> u16 {
    let page = if page == HV_INTC_FC_PAGE_START {
        0
    } else {
        page
    };
    (page << PHY_PAGE_SHIFT) as u16
}

/// Port of Linux `e1000_oem_bits_config_ich8lan`: the value to write back into
/// the PHY's `HV_OEM_BITS` given the MAC-side `PHY_CTRL` register.
///
/// `d0_state` is true while the driver is up (D0a); false is the suspend path,
/// where the non-D0a copies of the bits apply as well. `reset_blocked` mirrors
/// `check_reset_block` — when the ME forbids resetting the PHY, the
/// restart-auto-negotiation bit is left alone.
fn oem_bits_for_d0(phy_ctrl: u32, oem_reg: u16, d0_state: bool, reset_blocked: bool) -> u16 {
    let mut oem = oem_reg & !(HV_OEM_BITS_GBE_DIS | HV_OEM_BITS_LPLU);
    let (gbe_mask, lplu_mask) = if d0_state {
        (PHY_CTRL_GBE_DISABLE, PHY_CTRL_D0A_LPLU)
    } else {
        (
            PHY_CTRL_GBE_DISABLE | PHY_CTRL_NOND0A_GBE_DISABLE,
            PHY_CTRL_D0A_LPLU | PHY_CTRL_NOND0A_LPLU,
        )
    };
    if phy_ctrl & gbe_mask != 0 {
        oem |= HV_OEM_BITS_GBE_DIS;
    }
    if phy_ctrl & lplu_mask != 0 {
        oem |= HV_OEM_BITS_LPLU;
    }
    if !reset_blocked {
        oem |= HV_OEM_BITS_RESTART_AN;
    }
    oem
}

/// Resolve what auto-negotiation settled on from the PHY's own registers, the
/// way `ethtool` does: the highest-priority mode both ends advertised.
///
/// `bmsr` decides whether there is anything to resolve at all (link up AND
/// auto-negotiation complete). Returns `(speed in Mb/s, full duplex)`.
///
/// IEEE 802.3 priority: 1000-full, 1000-half, 100-full, 100-half, 10-full,
/// 10-half. This is the PHY's truth; the MAC's `STATUS` register is a separate
/// path (auto-speed detection over the MAC-PHY interconnect), so the two
/// disagreeing is itself a diagnosis.
fn phy_negotiated_link(
    bmsr: u16,
    adv: u16,
    lpa: u16,
    ctrl1000: u16,
    stat1000: u16,
) -> Option<(u32, bool)> {
    if bmsr & BMSR_LSTATUS == 0 || bmsr & BMSR_ANEGCOMPLETE == 0 {
        return None;
    }
    if ctrl1000 & ADVERTISE_1000FULL != 0 && stat1000 & LPA_1000FULL != 0 {
        return Some((1000, true));
    }
    if ctrl1000 & ADVERTISE_1000HALF != 0 && stat1000 & LPA_1000HALF != 0 {
        return Some((1000, false));
    }
    let common = adv & lpa;
    if common & LPA_100FULL != 0 {
        return Some((100, true));
    }
    if common & LPA_100HALF != 0 {
        return Some((100, false));
    }
    if common & LPA_10FULL != 0 {
        return Some((10, true));
    }
    if common & LPA_10HALF != 0 {
        return Some((10, false));
    }
    None
}

/// Port of the TIPG half of Linux `e1000_check_for_copper_link_ich8lan`.
///
/// 10 Mb/s half duplex needs a much larger inter-packet gap (some parts are so
/// aggressive they collide constantly); PCH-SPT and later also want a wider gap
/// at 100 Mb/s full duplex. Returns the whole TIPG value, IPGT field replaced.
fn tipg_for_link(tipg: u32, is_spt_or_later: bool, speed: u32, full_duplex: bool) -> u32 {
    let ipgt = if !full_duplex && speed == 10 {
        0xFF
    } else if is_spt_or_later && full_duplex && speed != 1000 {
        0x0C
    } else {
        0x08
    };
    (tipg & !TIPG_IPGT_MASK) | ipgt
}

// CTRL_EXT bits
const CTRL_EXT_RO_DIS: u32 = 1 << 17; // PCIe Relaxed Ordering Disable
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28; // Driver loaded (release ME)
const CTRL_EXT_IAME: u32 = 1 << 27;

// PHY_CTRL (MAC-side register 0xF10) — LPLU bits for I219
const PHY_CTRL_D0A_LPLU: u32 = 1 << 1;
const PHY_CTRL_NOND0A_LPLU: u32 = 1 << 2;
const PHY_CTRL_NOND0A_GBE_DISABLE: u32 = 1 << 3;
const PHY_CTRL_GBE_DISABLE: u32 = 1 << 6;

// MDIC register bits
const MDIC_REG_SHIFT: u32 = 16;
const MDIC_PHYADD_SHIFT: u32 = 21;
const MDIC_OP_WRITE: u32 = 1 << 26;
const MDIC_OP_READ: u32 = 2 << 26;
const MDIC_READY: u32 = 1 << 28;
const MDIC_ERROR: u32 = 1 << 30;
const MDIC_POLL_TRIES: u32 = 2000;

// PHY register 0 (BMCR)
const BMCR_RESET: u16 = 0x8000;

// EERD
const EERD_START: u32 = 1 << 0;
const EERD_DONE_BIT4: u32 = 1 << 4;
const EERD_DONE_BIT1: u32 = 1 << 1;
const EERD_DATA_SHIFT: u32 = 16;

// ICR / IMS bits (LK e1000_hw.h)
const ICR_TXDW: u32 = 1 << 0;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXO: u32 = 1 << 6;
const ICR_RXT0: u32 = 1 << 7;
const ICR_RXTO: u32 = ICR_RXT0;
const ICR_RX_WORK: u32 = ICR_RXTO | ICR_RXO | ICR_RXDMT0 | ICR_RXT0;
const ICR_RX_ANY: u32 = ICR_RX_WORK;
const IMS_REARM: u32 = ICR_TXDW | ICR_LSC | ICR_RX_WORK | (1 << 8);

// RCTL bits
const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;

// TCTL bits
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_RTLC: u32 = 1 << 24;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_CT_LINUX: u32 = 15 << TCTL_CT_SHIFT;
const TCTL_COLD_LINUX: u32 = 63 << 12;

// TXDCTL / RXDCTL — v0.5.0 values. Linux FLAG2_DMA_BURST (GRAN + PTHRESH=0x1f)
// sped up QEMU and some discrete parts, but on PCH/I219 it delayed descriptor
// write-back enough that DHCP DISCOVER never completed DD before udhcpc
// retried, and RX DMA burst produced checksum-garbage that the software
// verifier then dropped. QEMU ignores these fields, which is why DHCP still
// worked in the VM.
const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const TXDCTL_FULL_TX_DESC_WB: u32 = 0x0101_0000;
const TXDCTL_DMA_BURST: u32 = (1 << 22) | (1 << 8) | 1; // COUNT_DESC | hthresh=1 | pthresh=1

// RFCTL bits
const RFCTL_EXTEN: u32 = 1 << 15;
const RFCTL_NFSW_DIS: u32 = 1 << 6;
const RFCTL_NFSR_DIS: u32 = 1 << 7;

// MANC
const MANC_EN_MNG2HOST: u32 = 1 << 21;

// TARC0
const TARC0_SPEED_MODE: u32 = 1 << 21;

// ULP (Ultra Low Power) disable — i219/PCH-SPT only. On real hardware the ME
// firmware often leaves the PHY in ULP, so STATUS.LU never asserts. QEMU has no
// ME, which is why this is only needed on real hardware.
// FWSM.FW_VALID is bit 15 (0x8000); there used to be a second, wrongly-valued
// `FWSM_FW_VALID = 1 << 14` constant here that nothing referenced — removed
// rather than fixed in place, since a dead duplicate is a landmine for the
// next person who reaches for the "obviously right" name instead of this one.
const ICH_FWSM_FW_VALID: u32 = 0x0000_8000;
const FWSM_ULP_CFG_DONE: u32 = 0x0000_0400;
const H2ME_ULP: u32 = 0x0000_0800;
const H2ME_ENFORCE_SETTINGS: u32 = 0x0000_1000;
const FEXTNVM7_DISABLE_SMB_PERST: u32 = 0x0000_0020;
const CTRL_EXT_FORCE_SMBUS: u32 = 0x0000_0800; // CTRL_EXT bit 11

// SW/FW semaphore (EXTCNF_CTRL)
const EXTCNF_CTRL_SWFLAG: u32 = 0x0000_0020; // bit 5

// HV PHY paged-register access: value at page-select reg = page << PHY_PAGE_SHIFT,
// then access (reg & MAX_PHY_REG_ADDRESS) via MDIC.
const PHY_PAGE_SHIFT: u32 = 5;
const PHY_PAGE_SELECT_REG: u32 = 0x1F; // IGP01E1000_PHY_PAGE_SELECT
const MAX_PHY_REG_ADDRESS: u32 = 0x1F;
const MAX_PHY_MULTI_PAGE_REG: u32 = 0x0F;
const HV_PHY_ADDR: u8 = 1; // pages >= 768 live at PHY addr 1

// HV_OEM_BITS = PHY_REG(768, 25) — the PHY's own copy of the "link up at the
// lowest speed to save power" (LPLU) and "no gigabit" (GBE_DIS) knobs. Linux
// keeps it in sync with the MAC-side PHY_CTRL register in
// `e1000_oem_bits_config_ich8lan`; see [`oem_bits_for_d0`].
const HV_OEM_BITS_PAGE: u32 = 768;
const HV_OEM_BITS_REG: u32 = 25;
const HV_OEM_BITS_LPLU: u16 = 0x0004;
const HV_OEM_BITS_GBE_DIS: u16 = 0x0040;
const HV_OEM_BITS_RESTART_AN: u16 = 0x0400;
/// Linux `HV_INTC_FC_PAGE_START`: the first HV page reachable at PHY address 1,
/// and the one page whose select value is written as 0 (see [`hv_page_select`]).
const HV_INTC_FC_PAGE_START: u32 = 768;
// CV_SMB_CTRL = PHY_REG(769, 23)
const CV_SMB_CTRL_PAGE: u32 = 769;
const CV_SMB_CTRL_REG: u32 = 23;
const CV_SMB_CTRL_FORCE_SMBUS: u16 = 0x0001;
// HV_PM_CTRL = PHY_REG(770, 17)
const HV_PM_CTRL_PAGE: u32 = 770;
const HV_PM_CTRL_REG: u32 = 17;
const HV_PM_CTRL_K1_ENABLE: u16 = 0x4000;
// I218_ULP_CONFIG1 = PHY_REG(779, 16)
const ULP_CONFIG1_PAGE: u32 = 779;
const ULP_CONFIG1_REG: u32 = 16;
const ULP_CONFIG1_START: u16 = 0x0001;
const ULP_CONFIG1_IND: u16 = 0x0004;
const ULP_CONFIG1_STICKY_ULP: u16 = 0x0010;
const ULP_CONFIG1_INBAND_EXIT: u16 = 0x0020;
const ULP_CONFIG1_WOL_HOST: u16 = 0x0040;
const ULP_CONFIG1_RESET_TO_SMBUS: u16 = 0x0100;
const ULP_CONFIG1_DISABLE_SMB_PERST: u16 = 0x1000;

// GIO master disable (quiesce DMA before CTRL_RST)
const CTRL_GIO_MASTER_DISABLE: u32 = 0x0000_0004; // CTRL bit 2
const STATUS_GIO_MASTER_ENABLE: u32 = 0x0008_0000; // STATUS bit 19
const MASTER_DISABLE_TIMEOUT: u32 = 800;

// Kumeran (KMRN) register access + K1 config
const E1000E_KMRNCTRLSTA: usize = 0x0034 / 4;
const KMRNCTRLSTA_OFFSET_SHIFT: u32 = 16;
const KMRNCTRLSTA_OFFSET: u32 = 0x001F_0000;
const KMRNCTRLSTA_REN: u32 = 0x0020_0000;
const KMRNCTRLSTA_K1_CONFIG: u32 = 0x7;
const KMRNCTRLSTA_K1_ENABLE: u16 = 0x0002;
/// Kumeran sub-registers Linux programs in `e1000_setup_copper_link_ich8lan`
/// with the comment "this fixes erroneous timeouts at 10Mbps".
const KMRNCTRLSTA_TIMEOUTS: u32 = 0x4;
const KMRNCTRLSTA_INBAND_PARAM: u32 = 0x9;
const CTRL_EXT_SPD_BYPS: u32 = 0x0000_8000;
const CTRL_SPD_1000: u32 = 0x0000_0200;
const CTRL_SPD_100: u32 = 0x0000_0100;

// LANPHYPC toggle — re-powers the PHY after it leaves ULP
const CTRL_LANPHYPC_OVERRIDE: u32 = 0x0001_0000; // CTRL bit 16
const CTRL_LANPHYPC_VALUE: u32 = 0x0002_0000; // CTRL bit 17
const CTRL_EXT_LPCD: u32 = 0x0000_0004; // CTRL_EXT bit 2 (link phy config done)

// MII BMCR (PHY register 0) — IEEE standard autoneg bits
const MII_CR_RESTART_AUTO_NEG: u16 = 0x0200;
const MII_CR_AUTO_NEG_EN: u16 = 0x1000;

// IEEE 802.3 MII registers used for auto-negotiation advertisement.
const MII_BMSR: u32 = 0x01;
const MII_ADVERTISE: u32 = 0x04;
const MII_CTRL1000: u32 = 0x09;
const MII_ESTATUS: u32 = 0x0F;
/// BMSR bit 8: register 15 (ESTATUS) is implemented.
const BMSR_ESTATEN: u16 = 0x0100;
/// ESTATUS bits 12/13: the PHY can do 1000BASE-T half / full duplex. Half is
/// never advertised (Linux refuses it too) but is kept here because the
/// register bit is part of the documented layout.
#[allow(dead_code)]
const ESTATUS_1000_THALF: u16 = 0x1000;
const ESTATUS_1000_TFULL: u16 = 0x2000;
/// ADVERTISE bits 5..8: 10/100, half and full duplex.
const ADVERTISE_10HALF: u16 = 0x0020;
const ADVERTISE_10FULL: u16 = 0x0040;
const ADVERTISE_100HALF: u16 = 0x0080;
const ADVERTISE_100FULL: u16 = 0x0100;
const ADVERTISE_ALL_10_100: u16 =
    ADVERTISE_10HALF | ADVERTISE_10FULL | ADVERTISE_100HALF | ADVERTISE_100FULL;
/// CTRL1000 bits 8/9: advertise 1000BASE-T half / full duplex.
const ADVERTISE_1000HALF: u16 = 0x0100;
const ADVERTISE_1000FULL: u16 = 0x0200;
/// MII registers needed to read back what auto-negotiation actually settled on.
const MII_BMCR: u32 = 0x00;
const MII_LPA: u32 = 0x05;
const MII_STAT1000: u32 = 0x0A;
/// BMSR bit 2 / bit 5: link is up (latched low) and auto-negotiation finished.
const BMSR_LSTATUS: u16 = 0x0004;
const BMSR_ANEGCOMPLETE: u16 = 0x0020;
/// LPA (register 5) — what the link partner advertised for 10/100.
const LPA_10HALF: u16 = 0x0020;
const LPA_10FULL: u16 = 0x0040;
const LPA_100HALF: u16 = 0x0080;
const LPA_100FULL: u16 = 0x0100;
/// STAT1000 (register 10) bits 10/11 — the partner's 1000BASE-T abilities.
const LPA_1000HALF: u16 = 0x0400;
const LPA_1000FULL: u16 = 0x0800;

// Legacy RX descriptor status (LK / 8254x §3.2.3.1)
const RXD_STAT_DD: u8 = 1 << 0;
const RXD_STAT_EOP: u8 = 1 << 1;
/// Ignore checksum indication: the NIC did not validate this frame.
#[allow(dead_code)]
const RXD_STAT_IXSM: u8 = 1 << 2;
/// UDP checksum was calculated (and, absent `errors.TCPE`, verified).
#[allow(dead_code)]
const RXD_STAT_UDPCS: u8 = 1 << 4;
/// TCP (or UDP on parts that fold both) checksum was calculated.
#[allow(dead_code)]
const RXD_STAT_TCPCS: u8 = 1 << 5;
/// IPv4 header checksum was calculated.
#[allow(dead_code)]
const RXD_STAT_IPCS: u8 = 1 << 6;

// Legacy RX descriptor errors byte (8254x §3.2.3.1.3): CE, SE, SEQ, CXE and
// RXE mean the frame was damaged on the wire and is always dropped. TCPE and
// IPE are only the NIC's *checksum verdict* — see `process_rx_slot`.
const RXD_ERR_TCPE: u8 = 1 << 5;
const RXD_ERR_IPE: u8 = 1 << 6;
const RXD_ERR_CSUM_VERDICT: u8 = RXD_ERR_TCPE | RXD_ERR_IPE;

// TX descriptor CMD bits
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;
/// Interrupt Delay Enable — without this, TIDV/TADV are ignored and every
/// RS descriptor fires TXDW immediately.
const TX_CMD_IDE: u8 = 1 << 7;
const TX_CMD_POST: u8 = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS | TX_CMD_IDE;

// TX descriptor STATUS bits (written back by hardware when CMD.RS is set)
const TX_STAT_DD: u8 = 1 << 0;

// DMA ring sizing
const NUM_RX: usize = 256;
const NUM_TX: usize = 256;
/// LK uses 2048-byte RX buffers; one slot holds a full MTU frame.
const BUF_SIZE: usize = 2048;
/// Cap on a reassembled multi-descriptor RX frame. Each non-EOP fragment
/// fills a full `BUF_SIZE` buffer, so the cap must be a multiple of it large
/// enough to hold several fragments — capping at `BUF_SIZE` itself (the
/// previous value) made the merge branch in `process_rx_slot` unreachable: a
/// second fragment's length always overflowed the already-accumulated first
/// buffer, so every multi-descriptor frame was silently dropped. 16 buffers
/// covers well past a 9K jumbo frame, should RCTL.LPE / SRRCTL buffer size
/// ever change from today's single-descriptor (<=1522B) configuration.
const MAX_RX_FRAME_BYTES: usize = BUF_SIZE * 16;
const RX_DRAIN_BUDGET: usize = 128;
/// Soft cap on completed frames staged in [`E1000eHw::rx_ready`] during one
/// drain. Sized to match a smoltcp ingress burst so a single RDH snapshot
/// feeds the whole poll without re-walking the ring every 16 packets.
const RX_READY_CAP: usize = 64;
/// Recycled `Vec<u8>` frames so a steady TCP stream does not heap-alloc
/// ~1500 B on every packet. Capped so an idle NIC does not pin a large pool.
const RX_FRAME_POOL_CAP: usize = 32;
/// Descriptors per cache line (64 / 16). Used when batching FromDevice syncs
/// on a non-coherent (WB) ring so we round the invalidate span up to a line.
const RX_DESCS_PER_CACHE_LINE: usize = CACHE_LINE_SIZE / 16;
/// Bounded retry budget for [`E1000eTxToken::consume`] when the TX ring is
/// momentarily full. Completion is gated on the descriptor DD bit (not TDH):
/// the NIC drains autonomously via DMA write-back, so cheap DD re-reads let an
/// in-flight frame free a slot before we give up. A single 1514-byte frame
/// clears the wire in ~12 µs at 1 Gbps; this bounds the wait to a few
/// milliseconds — enough to never drop a pure ACK / window-update under an RX
/// burst, without spinning unboundedly if TX is genuinely wedged.
const TX_SEND_SPIN_LIMIT: usize = 4096;
/// On write-back TX rings, only re-`dma_sync` the tail descriptor every N spin
/// iterations. Between syncs we just `pause` — a stale cached DD=0 is a safe
/// false-negative (we keep waiting). UC rings skip sync entirely and only
/// `read_volatile` the status byte.
const TX_SPIN_SYNC_INTERVAL: usize = 16;
/// Stack scratch for [`E1000eTxToken::consume`]. Covers the advertised MTU
/// (1514) plus headroom; avoids a heap `vec![0; len]` zero-fill per frame.
const TX_SCRATCH_LEN: usize = 1536;
const DMA_RING_BYTES: usize = NUM_RX * size_of::<RxDesc>();
const DMA_TX_RING_BYTES: usize = NUM_TX * size_of::<TxDesc>();
const DMA_DESC_ALIGN: usize = 16;
const CACHE_LINE_SIZE: usize = 64;

// ---------------------------------------------------------------------------
// Descriptor layouts
// ---------------------------------------------------------------------------

/// Legacy RX descriptor (LK `rdesc` / Eclipse `E1000RecvDesc`).
#[repr(C, align(16))]
#[derive(Copy, Clone, Default)]
struct RxDesc {
    addr: u64,
    len: u16,
    chksum: u16,
    status: u8,
    errors: u8,
    vlan: u16,
}
const _RX_DESC_SIZE: () = assert!(core::mem::size_of::<RxDesc>() == 16);

/// Legacy TX descriptor (16 bytes).
#[repr(C, align(16))]
#[derive(Copy, Clone, Default)]
struct TxDesc {
    addr: u64,
    len: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
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

/// True if `frame` is an IPv4 Ethernet frame whose IP *header* checksum is wrong
/// (the one-complement sum of the header 16-bit words must be 0xffff). Returns
/// false for non-IPv4 frames (ARP/IPv6) so they are not counted as corrupt.
#[allow(dead_code)] // unit tests; the RX hot path no longer drops on checksum
fn rx_ipv4_hdr_csum_bad(frame: &[u8]) -> bool {
    if frame.len() < 14 + 20 || frame[12] != 0x08 || frame[13] != 0x00 {
        return false; // too short or not IPv4 (EtherType != 0x0800)
    }
    let ihl = (frame[14] & 0x0f) as usize * 4;
    if ihl < 20 || frame.len() < 14 + ihl {
        return false;
    }
    let mut sum: u32 = 0;
    let mut i = 14;
    while i < 14 + ihl {
        sum += ((frame[i] as u32) << 8) | frame[i + 1] as u32;
        i += 2;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum != 0xffff
}

/// One's-complement accumulate of `data` as big-endian 16-bit words (an odd
/// trailing byte is zero-padded on the right, per RFC 1071).
#[allow(dead_code)]
fn csum_add(mut sum: u32, data: &[u8]) -> u32 {
    let mut i = 0;
    while i + 1 < data.len() {
        sum += ((data[i] as u32) << 8) | data[i + 1] as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    sum
}

/// Does the descriptor say the NIC skipped (part of) checksum validation?
///
/// With `RXCSUM.IPOFLD|TUOFLD` the driver tells smoltcp not to verify IPv4 /
/// TCP / UDP checksums on receive (`Checksum::Tx`). That is only sound for
/// frames the hardware actually validated, which it reports per-descriptor:
/// `IPCS` / `TCPCS` / `UDPCS` mean "calculated" (a failure then shows up in
/// `errors.IPE/TCPE` and the frame is dropped), and `IXSM` means "ignore my
/// checksum indication". Frames without those bits — IPv6 TCP/UDP on parts
/// without IPv6 offload, anything the NIC flags IXSM — reach smoltcp with
/// NO checksum verification at all, so a corrupted segment would be accepted
/// and delivered to userspace as good data. Linux's `e1000_rx_checksum`
/// makes exactly this per-packet decision; smoltcp's capabilities are static,
/// so the driver has to do the fallback verification itself.
#[allow(dead_code)]
#[inline]
fn rx_csum_needs_sw_check(status: u8) -> bool {
    status & RXD_STAT_IXSM != 0
        || status & RXD_STAT_IPCS == 0
        || status & (RXD_STAT_TCPCS | RXD_STAT_UDPCS) == 0
}

/// Software fallback for the checksums the NIC did not validate (see
/// [`rx_csum_needs_sw_check`]). Returns `true` only when the frame is an
/// IPv4 / IPv6 TCP or UDP packet whose checksum is definitively wrong.
/// Anything this cannot parse or verify (ARP, ICMP, fragments, unusual IPv6
/// extension headers, truncated frames) returns `false` and is left to
/// smoltcp, exactly as before.
#[allow(dead_code)]
fn rx_sw_csum_bad(frame: &[u8], status: u8) -> bool {
    if frame.len() < 14 {
        return false;
    }
    let l4_trusted = status & (RXD_STAT_TCPCS | RXD_STAT_UDPCS) != 0 && status & RXD_STAT_IXSM == 0;
    match u16::from_be_bytes([frame[12], frame[13]]) {
        0x0800 => {
            let ip = &frame[14..];
            if ip.len() < 20 || ip[0] >> 4 != 4 {
                return false;
            }
            if (status & RXD_STAT_IPCS == 0 || status & RXD_STAT_IXSM != 0)
                && rx_ipv4_hdr_csum_bad(frame)
            {
                return true;
            }
            if l4_trusted {
                return false;
            }
            let ihl = (ip[0] & 0x0f) as usize * 4;
            let total_len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
            if ihl < 20 || total_len < ihl || total_len > ip.len() {
                return false;
            }
            // Fragments carry no verifiable L4 checksum (MF set or offset != 0).
            if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
                return false;
            }
            l4_csum_bad(ip[9], &ip[12..16], &ip[16..20], &ip[ihl..total_len], false)
        }
        0x86dd => {
            if l4_trusted {
                return false;
            }
            let ip = &frame[14..];
            if ip.len() < 40 || ip[0] >> 4 != 6 {
                return false;
            }
            let end = 40 + u16::from_be_bytes([ip[4], ip[5]]) as usize;
            if end > ip.len() {
                return false;
            }
            let mut next = ip[6];
            let mut off = 40;
            loop {
                match next {
                    // Hop-by-hop, routing, destination options: skip.
                    0 | 43 | 60 => {
                        if off + 8 > end {
                            return false;
                        }
                        next = ip[off];
                        off += (ip[off + 1] as usize + 1) * 8;
                        if off > end {
                            return false;
                        }
                    }
                    6 | 17 => break,
                    // Fragment / ESP / AH / unknown: cannot verify here.
                    _ => return false,
                }
            }
            l4_csum_bad(next, &ip[8..24], &ip[24..40], &ip[off..end], true)
        }
        _ => false,
    }
}

/// TCP/UDP checksum over the pseudo-header + segment. `l4` must be exactly
/// the L4 segment as bounded by the IP header (IPv4 total length / IPv6
/// payload length), so Ethernet padding is never summed.
#[allow(dead_code)]
fn l4_csum_bad(proto: u8, src: &[u8], dst: &[u8], l4: &[u8], ipv6: bool) -> bool {
    let seg: &[u8] = match proto {
        6 if l4.len() >= 20 => l4,
        17 if l4.len() >= 8 => {
            let udp_len = u16::from_be_bytes([l4[4], l4[5]]) as usize;
            if udp_len < 8 || udp_len > l4.len() {
                return false;
            }
            // IPv4 UDP may legitimately carry no checksum (0); IPv6 may not.
            if !ipv6 && l4[6] == 0 && l4[7] == 0 {
                return false;
            }
            &l4[..udp_len]
        }
        _ => return false,
    };
    let mut sum = csum_add(0, src);
    sum = csum_add(sum, dst);
    // Pseudo-header protocol + length. IPv6 spells the length as 32 bits but
    // its one's-complement contribution is identical for lengths < 64 KiB.
    sum += proto as u32;
    sum += seg.len() as u32;
    sum = csum_add(sum, seg);
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    sum != 0xffff
}

// ---------------------------------------------------------------------------
// E1000eHw — hardware state
// ---------------------------------------------------------------------------

pub struct E1000eHw {
    base: usize,
    pci_loc: Location,
    device_id: u16,

    mac: [u8; 6],

    rx_ring: DmaRegion,
    rx_buf_pool: DmaRegion,
    rx_ring_coherent: bool,
    rx_buf_coherent: bool,
    /// LK `rx_last_head_`: next RX descriptor to inspect.
    rx_next_to_clean: usize,
    /// Multi-descriptor frame being reassembled (LK `rx_pending_pkt_`).
    rx_pending: Option<Vec<u8>>,
    /// Set when reassembly of a multi-descriptor frame was abandoned part way
    /// through (oversized, bad descriptor). The remaining fragments of that
    /// frame are still queued in the ring and must be swallowed up to and
    /// including the one carrying EOP. Without this the very next fragment —
    /// the middle or the tail of a frame we already gave up on — starts a
    /// *new* reassembly and, if it happens to carry EOP, is handed to smoltcp
    /// and to any AF_PACKET tap as though it were a complete Ethernet frame.
    rx_discard_until_eop: bool,
    /// Completed frames staged by a prior drain so back-to-back
    /// [`receive`](Self::receive) calls (smoltcp burst) only pop — no RDH
    /// re-read, no per-slot descriptor sync.
    rx_ready: VecDeque<Vec<u8>>,
    /// True when one or more descriptors have been recycled since the last
    /// [`flush_rx_doorbell`](Self::flush_rx_doorbell) call. Ringing the RDT
    /// doorbell is an MMIO write (plus, previously, a synchronous readback);
    /// batching it across a whole receive burst instead of once per packet
    /// is what actually matters under load — see `flush_rx_doorbell`.
    rx_doorbell_dirty: bool,

    tx_ring: DmaRegion,
    tx_buf_pool: DmaRegion,
    tx_ring_coherent: bool,
    tx_buf_coherent: bool,
    tx_tail: usize,
    /// True when one or more TX descriptors have been posted since the last
    /// [`flush_tx_doorbell`](Self::flush_tx_doorbell). Deferring the TDT MMIO
    /// (and the ToDevice descriptor sync) across a smoltcp TX burst avoids one
    /// doorbell round-trip per frame — see `flush_tx_doorbell`.
    tx_doorbell_dirty: bool,
    /// First posted slot not yet covered by a batched ToDevice descriptor sync.
    tx_desc_dirty_start: Option<usize>,
    /// Contiguous posted slots pending that batched sync (no ring wrap).
    tx_desc_dirty_count: usize,
    /// Recycled RX frame allocations. See [`RX_FRAME_POOL_CAP`].
    rx_frame_pool: Vec<Vec<u8>>,

    pub stats: NetStats,
    /// Count of received frames dropped by the driver's own checksum check:
    /// the software fallback for frames the NIC reported it did not validate
    /// (see [`rx_csum_needs_sw_check`]), plus — under `E1000E_LOG_VERBOSE` —
    /// IPv4 headers that failed the diagnostic probe. Surfaced in the
    /// watchdog. Per-instance (not a file-level static) so two e1000e NICs
    /// don't conflate each other's diagnostics.
    rx_csum_bad: u64,
    /// Frames the NIC flagged `TCPE`/`IPE` that then PASSED the software
    /// checksum check — i.e. hardware checksum false positives. Non-zero on
    /// real hardware is the signature of the "TCP works, DNS/UDP doesn't"
    /// failure the old unconditional drop produced. Watchdog-visible.
    rx_csum_hw_false_positive: u64,
    /// Count of outgoing frames smoltcp handed us that we had to DROP because
    /// the TX ring stayed full past [`TX_SEND_SPIN_LIMIT`]. Any non-zero
    /// value means a pure ACK / window-update was lost — the exact cause of
    /// the silent download deadlock — so the watchdog surfaces it. Should
    /// stay 0 once TX completes. Per-instance for the same reason as above.
    tx_dropped: u64,

    link_up: bool,
    link_watchdog_next_us: u64,
    watchdog_log_next_us: u64,
    itr_setting: u32,
    itr_last_rx_packets: u64,
    itr_tune_next_us: u64,
    /// Timestamp of the last watchdog throughput sample, so the rate is
    /// computed over the interval that actually elapsed rather than the
    /// nominal log period.
    throughput_last_us: u64,

    /// False after [`Self::hw_down`]; TX/RX refuse until reinit/`reset_and_init`.
    hw_running: bool,
    /// Software view of RCTL.UPE (unicast promiscuous).
    rx_promisc: bool,
    /// Software view of RCTL.MPE (multicast promiscuous / allmulti).
    rx_allmulti: bool,
    /// Multicast addresses programmed into the MTA when not allmulti.
    mc_list: Vec<[u8; 6]>,
}

impl E1000eHw {
    // -----------------------------------------------------------------------
    // Timing
    // -----------------------------------------------------------------------

    fn udelay(us: u64) {
        if us == 0 {
            return;
        }
        let t0 = timer_now_as_micros();
        const MAX_SPINS: u64 = 10_000_000;
        let mut n = 0u64;
        while timer_now_as_micros().wrapping_sub(t0) < us {
            core::hint::spin_loop();
            n += 1;
            if n >= MAX_SPINS {
                break;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Buffer address helpers
    // -----------------------------------------------------------------------

    #[inline]
    fn rx_buf_paddr(&self, i: usize) -> u64 {
        (self.rx_buf_pool.paddr() + i * BUF_SIZE) as u64
    }
    #[inline]
    fn rx_buf_vaddr(&self, i: usize) -> usize {
        self.rx_buf_pool.vaddr() + i * BUF_SIZE
    }
    #[inline]
    fn tx_buf_paddr(&self, i: usize) -> u64 {
        (self.tx_buf_pool.paddr() + i * BUF_SIZE) as u64
    }
    #[inline]
    fn tx_buf_vaddr(&self, i: usize) -> usize {
        self.tx_buf_pool.vaddr() + i * BUF_SIZE
    }

    fn take_rx_frame(&mut self, len: usize) -> Vec<u8> {
        match self.rx_frame_pool.pop() {
            Some(mut v) => {
                v.clear();
                if v.capacity() < len {
                    v.reserve(len);
                }
                v
            }
            None => Vec::with_capacity(len),
        }
    }

    fn recycle_rx_frame(&mut self, mut buf: Vec<u8>) {
        if self.rx_frame_pool.len() >= RX_FRAME_POOL_CAP {
            return;
        }
        let cap = buf.capacity();
        if !(64..=BUF_SIZE).contains(&cap) {
            return;
        }
        buf.clear();
        self.rx_frame_pool.push(buf);
    }

    // -----------------------------------------------------------------------
    // Device family helpers
    // -----------------------------------------------------------------------

    fn is_pch(&self) -> bool {
        e1000e_is_pch(self.device_id)
    }

    fn is_pch_spt_or_later(&self) -> bool {
        e1000e_is_pch_spt_or_later(self.device_id)
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
        let cmd = (reg << MDIC_REG_SHIFT) | ((phy_addr as u32) << MDIC_PHYADD_SHIFT) | MDIC_OP_READ;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..MDIC_POLL_TRIES {
            Self::udelay(50);
            let v = mmio_read(self.base, E1000E_MDIC);
            if v & MDIC_READY != 0 {
                if v & MDIC_ERROR != 0 {
                    return None;
                }
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
            if ext & EXTCNF_CTRL_SWFLAG == 0 {
                break;
            }
            if timeout == 0 {
                return false;
            }
            Self::udelay(1_000);
            timeout -= 1;
        }
        ext |= EXTCNF_CTRL_SWFLAG;
        mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
        let mut timeout = 1_000u32;
        loop {
            ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if ext & EXTCNF_CTRL_SWFLAG != 0 {
                return true;
            }
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
            && !self.mdic_write(HV_PHY_ADDR, PHY_PAGE_SELECT_REG, hv_page_select(page))
        {
            return None;
        }
        self.mdic_read(HV_PHY_ADDR, reg & MAX_PHY_REG_ADDRESS)
    }

    unsafe fn phy_write_hv(&self, page: u32, reg: u32, val: u16) -> bool {
        if reg > MAX_PHY_MULTI_PAGE_REG
            && !self.mdic_write(HV_PHY_ADDR, PHY_PAGE_SELECT_REG, hv_page_select(page))
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
        if !self.is_pch_spt_or_later() {
            return;
        }

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
            if force {
                h2me &= !H2ME_ENFORCE_SETTINGS;
            } else {
                h2me &= !H2ME_ULP;
            }
            mmio_write(self.base, E1000E_H2ME, h2me);
            crate::klog_warn!(
                "[e1000e] disable_ulp FW-path cfg_done_cleared={}\n",
                cleared
            );
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
        if !self.is_pch() {
            return;
        }
        if !self.acquire_swflag() {
            crate::klog_warn!("[e1000e] configure_k1: SW/FW semaphore busy\n");
            return;
        }
        let mut kmrn = self.kmrn_read(KMRNCTRLSTA_K1_CONFIG);
        if enable {
            kmrn |= KMRNCTRLSTA_K1_ENABLE;
        } else {
            kmrn &= !KMRNCTRLSTA_K1_ENABLE;
        }
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
        if !self.is_pch() {
            return;
        }
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
                if mmio_read(self.base, E1000E_CTRL_EXT) & CTRL_EXT_LPCD != 0 {
                    break;
                }
                if count == 0 {
                    break;
                }
                count -= 1;
            }
            Self::udelay(30_000);
        } else {
            Self::udelay(50_000);
        }
    }

    // -----------------------------------------------------------------------
    // LPLU / gigabit-disable in the PHY (port of e1000_oem_bits_config_ich8lan
    // and e1000_set_lplu_state_pchlan), plus the Kumeran and MAC tuning that
    // Linux hangs off the copper-link setup and the link-change check.
    // -----------------------------------------------------------------------

    /// Clear "low power link up" and "gigabit disabled" **in the PHY**.
    ///
    /// Clearing them in the MAC-side `PHY_CTRL` register (step 10 of
    /// `reset_and_init`) is not enough on a PCH part: the bits that actually
    /// steer auto-negotiation live in the PHY's own `HV_OEM_BITS`, they survive
    /// a MAC reset, and the ME leaves LPLU set when it has been holding the
    /// link up for manageability. LPLU means "negotiate the lowest speed the
    /// link supports", i.e. **10 Mb/s** — which is exactly what a host that
    /// never clears it sees on a gigabit switch.
    ///
    /// Linux gates the full `PHY_CTRL` -> `HV_OEM_BITS` mirror on
    /// `FEXTNVM.SW_CONFIG`; when that gate is closed it still has a narrower
    /// entry point (`phy.ops.set_d0_lplu_state`, i.e.
    /// `e1000_set_lplu_state_pchlan`) that only touches LPLU and restarts
    /// auto-negotiation. We run whichever of the two applies, so the link is
    /// never left at 10 Mb/s just because the NVM gate is closed.
    unsafe fn oem_bits_config(&self, d0_state: bool) {
        if !self.is_pch() {
            return;
        }
        if !self.acquire_swflag() {
            crate::klog_warn!("[e1000e] oem_bits_config: SW/FW semaphore busy\n");
            return;
        }
        let fextnvm = mmio_read(self.base, E1000E_FEXTNVM);
        let phy_ctrl = mmio_read(self.base, E1000E_PHY_CTRL);
        let sw_config = fextnvm & FEXTNVM_SW_CONFIG_ICH8M != 0;
        if let Some(oem) = self.phy_read_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG) {
            if oem != 0xFFFF {
                let want = if sw_config {
                    oem_bits_for_d0(phy_ctrl, oem, d0_state, false)
                } else {
                    // Narrow path: LPLU only, as e1000_set_lplu_state_pchlan does.
                    (oem & !HV_OEM_BITS_LPLU) | HV_OEM_BITS_RESTART_AN
                };
                if want != oem {
                    let ok = self.phy_write_hv(HV_OEM_BITS_PAGE, HV_OEM_BITS_REG, want);
                    crate::klog_warn!(
                        "[e1000e] HV_OEM_BITS {:#06x} -> {:#06x} (lplu={} gbe_dis={} sw_config={} ok={})\n",
                        oem,
                        want,
                        oem & HV_OEM_BITS_LPLU != 0,
                        oem & HV_OEM_BITS_GBE_DIS != 0,
                        sw_config,
                        ok
                    );
                }
            }
        } else {
            crate::klog_warn!(
                "[e1000e] HV_OEM_BITS unreadable (PHY_CTRL={:#010x} FEXTNVM={:#010x})\n",
                phy_ctrl,
                fextnvm
            );
        }
        self.release_swflag();
    }

    /// Port of the Kumeran half of Linux `e1000_setup_copper_link_ich8lan`:
    /// maximum wait between PHY polls and the in-band parameter floor. Linux's
    /// own comment is "this fixes erroneous timeouts at 10Mbps".
    unsafe fn setup_kmrn_copper_timeouts(&self) {
        if !self.is_pch() {
            return;
        }
        if !self.acquire_swflag() {
            return;
        }
        self.kmrn_write(KMRNCTRLSTA_TIMEOUTS, 0xFFFF);
        let inband = self.kmrn_read(KMRNCTRLSTA_INBAND_PARAM);
        self.kmrn_write(KMRNCTRLSTA_INBAND_PARAM, inband | 0x3F);
        self.release_swflag();
    }

    /// Read back what auto-negotiation actually resolved to, straight from the
    /// PHY. Used only for reporting: the MAC's `STATUS` stays the value the
    /// driver acts on, exactly as in Linux.
    unsafe fn read_phy_negotiated(&self) -> Option<(u32, bool)> {
        if !self.acquire_swflag() {
            return None;
        }
        let out = self.read_phy_negotiated_locked();
        self.release_swflag();
        out
    }

    unsafe fn read_phy_negotiated_locked(&self) -> Option<(u32, bool)> {
        let phy_addrs: [u8; 2] = if self.is_pch() { [2, 1] } else { [1, 2] };
        for phy_addr in phy_addrs {
            let Some(bmsr) = self.mdic_read(phy_addr, MII_BMSR) else {
                continue;
            };
            if bmsr == 0xFFFF {
                continue;
            }
            let adv = self.mdic_read(phy_addr, MII_ADVERTISE).unwrap_or(0);
            let lpa = self.mdic_read(phy_addr, MII_LPA).unwrap_or(0);
            let ctrl1000 = self.mdic_read(phy_addr, MII_CTRL1000).unwrap_or(0);
            let stat1000 = self.mdic_read(phy_addr, MII_STAT1000).unwrap_or(0);
            return phy_negotiated_link(bmsr, adv, lpa, ctrl1000, stat1000);
        }
        None
    }

    /// Speed-dependent MAC tuning Linux applies every time the link changes
    /// (`e1000_check_for_copper_link_ich8lan`): the transmit inter-packet gap,
    /// and the I217 beacon duration that its packet-loss erratum calls for.
    unsafe fn apply_link_speed_tuning(&self, status: u32) {
        if !self.is_pch() {
            return;
        }
        let speed = status_speed_mbps(status);
        let full_duplex = status & STATUS_FD != 0;
        let tipg = mmio_read(self.base, E1000E_TIPG);
        let want = tipg_for_link(tipg, self.is_pch_spt_or_later(), speed, full_duplex);
        if want != tipg {
            mmio_write(self.base, E1000E_TIPG, want);
        }
        let f4 = mmio_read(self.base, E1000E_FEXTNVM4);
        let want4 = (f4 & !FEXTNVM4_BEACON_DURATION_MASK) | FEXTNVM4_BEACON_DURATION_8USEC;
        if want4 != f4 {
            mmio_write(self.base, E1000E_FEXTNVM4, want4);
        }
    }

    // -----------------------------------------------------------------------
    // Restart auto-negotiation via the PHY BMCR (register 0). Tries both
    // possible PHY addresses and protects the access with the SW/FW semaphore.
    // -----------------------------------------------------------------------

    unsafe fn restart_autoneg(&self) {
        if !self.acquire_swflag() {
            return;
        }
        // On PCH (82577/8/9, I217/I218/I219) the IEEE MII registers live at
        // MDIO address 2; address 1 is the MAC-side / wakeup register block
        // (Linux `e1000_get_phy_addr_for_hv_page`: pages < 768 → 2). Trying
        // address 1 first on those parts "succeeded" (MDIC_READY, no error)
        // and broke out of the loop, so the BMCR autoneg restart never
        // reached the real PHY. Discrete 82574 keeps its PHY at address 1.
        let phy_addrs: [u8; 2] = if self.is_pch() { [2, 1] } else { [1, 2] };
        for phy_addr in phy_addrs {
            if let Some(bmcr) = self.mdic_read(phy_addr, MII_BMCR) {
                if bmcr == 0xFFFF {
                    continue;
                }
                self.widen_autoneg_advertisement(phy_addr);
                let v = bmcr | MII_CR_AUTO_NEG_EN | MII_CR_RESTART_AUTO_NEG;
                if self.mdic_write(phy_addr, MII_BMCR, v) {
                    crate::klog_warn!("[e1000e] restart autoneg on phy_addr={}\n", phy_addr);
                    break;
                }
            }
        }
        self.release_swflag();
    }

    /// Make sure the PHY advertises everything it is capable of before
    /// auto-negotiation is restarted.
    ///
    /// Restarting autoneg only re-runs the exchange; it does not touch what is
    /// being offered. Firmware, a previous OS, or the ULP/LPLU exit sequence
    /// can leave `ADVERTISE` / `CTRL1000` with gigabit (or full duplex) turned
    /// off, and the link then negotiates 100 Mb/s — or half duplex — on a
    /// gigabit switch, with nothing in the log saying why. Linux programs the
    /// full advertisement in `e1000_phy_setup_autoneg` before every restart.
    ///
    /// Bits are only ever ADDED here, never cleared, so this can widen a
    /// negotiation but never narrow one.
    unsafe fn widen_autoneg_advertisement(&self, phy_addr: u8) {
        if let Some(adv) = self.mdic_read(phy_addr, MII_ADVERTISE) {
            if adv != 0xFFFF && adv & ADVERTISE_ALL_10_100 != ADVERTISE_ALL_10_100 {
                let want = adv | ADVERTISE_ALL_10_100;
                if self.mdic_write(phy_addr, MII_ADVERTISE, want) {
                    crate::klog_warn!(
                        "[e1000e] widened 10/100 autoneg advertisement {:#06x} -> {:#06x}\n",
                        adv,
                        want
                    );
                }
            }
        }

        // Gigabit lives in CTRL1000 and only exists when BMSR says ESTATUS is
        // implemented and ESTATUS reports 1000BASE-T.
        let Some(bmsr) = self.mdic_read(phy_addr, MII_BMSR) else {
            return;
        };
        if bmsr == 0xFFFF || bmsr & BMSR_ESTATEN == 0 {
            return;
        }
        let Some(estatus) = self.mdic_read(phy_addr, MII_ESTATUS) else {
            return;
        };
        if estatus == 0xFFFF {
            return;
        }
        // Only full duplex: Linux's e1000_phy_setup_autoneg refuses to
        // advertise 1000BASE-T half duplex outright ("Advertise 1000mb Half
        // duplex request denied!"). Offering it can only ever resolve to a
        // worse link than the one we already asked for.
        let capable = if estatus & ESTATUS_1000_TFULL != 0 {
            ADVERTISE_1000FULL
        } else {
            return;
        };
        if let Some(ctrl1000) = self.mdic_read(phy_addr, MII_CTRL1000) {
            if ctrl1000 != 0xFFFF && ctrl1000 & capable != capable {
                let want = ctrl1000 | capable;
                if self.mdic_write(phy_addr, MII_CTRL1000, want) {
                    crate::klog_warn!(
                        "[e1000e] widened 1000BASE-T autoneg advertisement {:#06x} -> {:#06x}\n",
                        ctrl1000,
                        want
                    );
                }
            }
        }
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
            // Try EERD as fallback — but ONLY on discrete parts. On ICH/PCH
            // silicon there is no EERD: Linux's regs.h maps the same offset
            // 0x00014 to FEXTNVM5, and `e1000_read_nvm_ich8lan` goes through
            // the flash interface instead. Probing "EERD" on an I217/I218/I219
            // therefore does not read the MAC at all — it writes an arbitrary
            // value into a power-management workaround register and then reads
            // that register's top half back as if it were EEPROM data.
            if self.is_pch() {
                if self.read_mac_from_ich_flash() {
                    return;
                }
            } else {
                self.read_mac_from_eeprom();
            }
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
            self.mac[(word as usize) * 2] = (w & 0xFF) as u8;
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
        let all_ff = self.mac.iter().all(|&b| b == 0xFF);
        !all_zeros && !all_ff
    }

    // -----------------------------------------------------------------------
    // ICH/SPT flash NVM — minimal MAC read (Linux e1000_read_nvm_spt subset)
    // -----------------------------------------------------------------------

    #[inline]
    unsafe fn flash_read32(&self, off: usize) -> u32 {
        read_volatile((self.base + E1000E_FLASH_BASE_OFF + off) as *const u32)
    }

    #[inline]
    unsafe fn flash_write32(&self, off: usize, val: u32) {
        write_volatile((self.base + E1000E_FLASH_BASE_OFF + off) as *mut u32, val);
    }

    /// SPT-style flash cycle init (HSFSTS via dword at offset 0x4).
    unsafe fn flash_cycle_init_spt(&self) -> bool {
        let mut hs = self.flash_read32(ICH_FLASH_HSFSTS) as u16;
        if hs & HSFSTS_FLDESVALID == 0 {
            return false;
        }
        // W1C flcerr | dael
        hs |= HSFSTS_FLCERR | HSFSTS_DAEL;
        self.flash_write32(ICH_FLASH_HSFSTS, hs as u32);
        for _ in 0..10_000u32 {
            hs = self.flash_read32(ICH_FLASH_HSFSTS) as u16;
            if hs & HSFSTS_FLCINPROG == 0 {
                hs |= HSFSTS_FLCDONE;
                self.flash_write32(ICH_FLASH_HSFSTS, hs as u32);
                return true;
            }
            Self::udelay(1);
        }
        false
    }

    /// Read one dword from SPT+ flash window (Linux `e1000_read_flash_data32`).
    unsafe fn flash_read_dword_spt(&self, byte_off: u32) -> Option<u32> {
        for _ in 0..10u32 {
            if !self.flash_cycle_init_spt() {
                return None;
            }
            // HSFCTL lives in the upper 16 bits of the dword at HSFSTS (SPT).
            let mut ctl = (self.flash_read32(ICH_FLASH_HSFSTS) >> 16) as u16;
            ctl &= !((3 << 1) | (3 << 8) | 1); // clear flcycle, fldbcount, flcgo
            ctl |= 3 << 8; // fldbcount = 3 → 4-byte read; flcycle = READ (0)
            self.flash_write32(ICH_FLASH_HSFSTS, (ctl as u32) << 16);
            self.flash_write32(ICH_FLASH_FADDR, byte_off & ICH_FLASH_LINEAR_ADDR_MASK);
            ctl |= 1; // flcgo
            self.flash_write32(ICH_FLASH_HSFSTS, (ctl as u32) << 16);
            for _ in 0..2000u32 {
                let hs = self.flash_read32(ICH_FLASH_HSFSTS) as u16;
                if hs & HSFSTS_FLCDONE != 0 {
                    if hs & HSFSTS_FLCERR != 0 {
                        break;
                    }
                    return Some(self.flash_read32(ICH_FLASH_FDATA0));
                }
                Self::udelay(1);
            }
        }
        None
    }

    /// Bank detect via signature word 0x13 (Linux `e1000_valid_nvm_bank_detect_ich8lan` SPT).
    unsafe fn ich_flash_active_bank_words(&self) -> u32 {
        let strap = mmio_read(self.base, E1000E_STRAP);
        let nvm_size = (((strap >> 1) & 0x1F) + 1) * 4096;
        let bank_words = (nvm_size / 2) / 2; // words per bank

        // Signature at word offset 0x13 of each bank; valid if (sig & 0xC0) == 0x80.
        if let Some(d) = self.flash_read_dword_spt(0x13 << 1) {
            let sig = ((d >> 8) & 0xFF) as u16;
            if sig & 0xC0 == 0x80 {
                return 0;
            }
        }
        if bank_words > 0 {
            if let Some(d) = self.flash_read_dword_spt((bank_words + 0x13) << 1) {
                let sig = ((d >> 8) & 0xFF) as u16;
                if sig & 0xC0 == 0x80 {
                    return bank_words;
                }
            }
        }
        0
    }

    /// Read MAC words 0..2 from ICH/SPT flash. Returns true if a plausible MAC was loaded.
    unsafe fn read_mac_from_ich_flash(&mut self) -> bool {
        if !self.is_pch_spt_or_later() {
            // Pre-SPT needs BAR1; we only map BAR0. Leave MAC unset → placeholder.
            return false;
        }
        let bank = self.ich_flash_active_bank_words();
        let Some(d0) = self.flash_read_dword_spt(bank << 1) else {
            return false;
        };
        let Some(d1) = self.flash_read_dword_spt((bank + 2) << 1) else {
            return false;
        };
        let words = [
            (d0 & 0xFFFF) as u16,
            (d0 >> 16) as u16,
            (d1 & 0xFFFF) as u16,
        ];
        for (i, w) in words.iter().enumerate() {
            self.mac[i * 2] = (w & 0xFF) as u8;
            self.mac[i * 2 + 1] = (w >> 8) as u8;
        }
        if self.is_valid_mac() {
            crate::klog_warn!(
                "[e1000e] MAC from ICH flash bank_words={}: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                bank,
                self.mac[0],
                self.mac[1],
                self.mac[2],
                self.mac[3],
                self.mac[4],
                self.mac[5]
            );
            true
        } else {
            false
        }
    }

    // -----------------------------------------------------------------------
    // RX mode (promisc / allmulti / MTA) — Linux e1000e_set_rx_mode subset
    // -----------------------------------------------------------------------

    /// Program the multicast hash table from `mc_list` (or clear it).
    unsafe fn write_mta(&self, addrs: &[[u8; 6]]) {
        let mut shadow = [0u32; E1000E_MTA_REG_COUNT];
        for addr in addrs {
            let hash = e1000e_hash_mc_addr(addr);
            let reg = ((hash >> 5) as usize) & (E1000E_MTA_REG_COUNT - 1);
            let bit = hash & 0x1F;
            shadow[reg] |= 1u32 << bit;
        }
        for i in (0..E1000E_MTA_REG_COUNT).rev() {
            mmio_write(self.base, E1000E_MTA_BASE + i, shadow[i]);
        }
        let _ = mmio_read(self.base, E1000E_STATUS);
    }

    /// Apply RCTL UPE/MPE + MTA from the software rx-mode flags.
    ///
    /// Keeps EN/BAM/SECRC (and any other bits) from the current RCTL value.
    unsafe fn apply_rx_mode_rctl(&mut self) {
        let mut rctl = mmio_read(self.base, E1000E_RCTL);
        rctl &= !(RCTL_UPE | RCTL_MPE);
        if self.rx_promisc {
            rctl |= RCTL_UPE | RCTL_MPE;
            self.write_mta(&[]);
        } else {
            if self.rx_allmulti {
                rctl |= RCTL_MPE;
                self.write_mta(&[]);
            } else {
                let list = self.mc_list.clone();
                self.write_mta(&list);
            }
        }
        // Preserve EN if already running; init_rx ORs EN before calling us.
        mmio_write(self.base, E1000E_RCTL, rctl);
        let _ = mmio_read(self.base, E1000E_RCTL);
    }

    pub unsafe fn set_rx_mode(&mut self, promisc: bool, allmulti: bool, mc_addrs: &[[u8; 6]]) {
        self.rx_promisc = promisc;
        self.rx_allmulti = allmulti;
        self.mc_list.clear();
        self.mc_list.extend_from_slice(mc_addrs);
        if self.hw_running {
            self.apply_rx_mode_rctl();
        }
    }

    // -----------------------------------------------------------------------
    // Down / reinit — Linux e1000e_down subset (no PHY soft-reset)
    // -----------------------------------------------------------------------

    /// Quiesce RX/TX and mask IRQs. Soft state is retained for a later reinit.
    pub unsafe fn hw_down(&mut self) {
        self.hw_running = false;
        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
        let _ = mmio_read(self.base, E1000E_ICR);

        let rctl = mmio_read(self.base, E1000E_RCTL) & !RCTL_EN;
        mmio_write(self.base, E1000E_RCTL, rctl);
        let tctl = mmio_read(self.base, E1000E_TCTL) & !TCTL_EN;
        mmio_write(self.base, E1000E_TCTL, tctl);
        let _ = mmio_read(self.base, E1000E_STATUS);
        Self::udelay(10_000);

        self.rx_ready.clear();
        self.rx_pending = None;
        self.rx_discard_until_eop = false;
        self.rx_doorbell_dirty = false;
        self.tx_doorbell_dirty = false;
        self.tx_desc_dirty_start = None;
        self.tx_desc_dirty_count = 0;
        self.link_up = false;
    }

    /// Down then full `reset_and_init` (rings already allocated).
    pub unsafe fn reinit_locked(&mut self) -> DeviceResult<()> {
        self.hw_down();
        self.reset_and_init()
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
        mmio_write(self.base, E1000E_WUC, 0);
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

        // 8. CTRL_EXT: disable PCIe relaxed ordering, signal driver loaded.
        //    LK enables IAME on e1000e (QEMU 82574); keep it off on PCH i219.
        {
            let mut ext = mmio_read(self.base, E1000E_CTRL_EXT);
            ext |= CTRL_EXT_RO_DIS | CTRL_EXT_DRV_LOAD;
            if self.is_pch() {
                ext &= !CTRL_EXT_IAME;
            } else {
                ext |= CTRL_EXT_IAME;
            }
            mmio_write(self.base, E1000E_CTRL_EXT, ext);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
            if !self.is_pch() {
                mmio_write(self.base, E1000E_IAM, 0);
            }
        }

        // 9. FEXTNVM6/7 workarounds for PCH-SPT (Linux ich8lan.c)
        if self.is_pch_spt_or_later() {
            let fext6 = mmio_read(self.base, E1000E_FEXTNVM6);
            mmio_write(self.base, E1000E_FEXTNVM6, fext6 & !0x0000_0010); // clear bit 4
            let fext7 = mmio_read(self.base, E1000E_FEXTNVM7);
            mmio_write(self.base, E1000E_FEXTNVM7, fext7 | 0x0000_0001); // set bit 0
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

        // 10.5 Mirror those bits into the PHY's own HV_OEM_BITS. The MAC-side
        //      register above is only the host's copy; the PHY keeps its own,
        //      it survives CTRL_RST, and while its LPLU bit is set the PHY
        //      negotiates the *lowest* speed the link supports — 10 Mb/s on a
        //      gigabit switch. Linux does this in every PHY reset path
        //      (e1000_post_phy_reset_ich8lan -> e1000_oem_bits_config_ich8lan).
        self.oem_bits_config(true);

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

        // 12.3 Kumeran poll timeouts — Linux's e1000_setup_copper_link_ich8lan
        //      sets these right after SLU/ASDE, to stop the MAC giving up on
        //      the PHY early ("fixes erroneous timeouts at 10Mbps").
        self.setup_kmrn_copper_timeouts();

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
        mmio_write(self.base, E1000E_WUC, 0);
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

        self.hw_running = true;
        Ok(())
    }

    unsafe fn init_tx(&mut self) {
        // Program TX ring base, length, head, tail
        let tx_pa = self.tx_ring.paddr();
        mmio_write(self.base, E1000E_TDBAL, tx_pa as u32);
        mmio_write(self.base, E1000E_TDBAH, (tx_pa >> 32) as u32);
        mmio_write(
            self.base,
            E1000E_TDLEN,
            (NUM_TX * size_of::<TxDesc>()) as u32,
        );
        mmio_write(self.base, E1000E_TDH, 0);
        mmio_write(self.base, E1000E_TDT, 0);
        self.tx_tail = 0;

        // Pre-mark every descriptor DD (done/free) so `can_send`/`send` can gate
        // on the real hardware-ownership bit from the very first transmit, the
        // same way `e1000.rs` does. Without this, freshly allocated (possibly
        // uninitialized) descriptor memory could read DD=0 and permanently look
        // "owned by hardware" even though it was never posted.
        {
            let ring = self.tx_ring.as_ptr::<TxDesc>();
            for i in 0..NUM_TX {
                let desc = unsafe { &mut *ring.add(i) };
                write_volatile(&mut desc.addr, self.tx_buf_paddr(i));
                write_volatile(&mut desc.len, 0);
                write_volatile(&mut desc.cso, 0);
                write_volatile(&mut desc.cmd, 0);
                write_volatile(&mut desc.status, TX_STAT_DD);
                write_volatile(&mut desc.css, 0);
                write_volatile(&mut desc.special, 0);
            }
            dma_sync_rx_desc_span(
                &self.tx_ring,
                self.tx_ring_coherent,
                0,
                NUM_TX,
                size_of::<TxDesc>(),
                DmaSyncDir::ToDevice,
            );
        }

        // Timers off (v0.5.0). Absolute delay coalescing made the first
        // DHCPOFFER wait for a later packet on real hardware; QEMU injects
        // the reply immediately so it never showed up in the VM.
        mmio_write(self.base, E1000E_TIDV, 0);
        mmio_write(self.base, E1000E_TADV, 0);

        // Inter-Packet Gap: IPGT=8, IPGR1=8, IPGR2=6 — the 8254x/e1000e
        // datasheet copper default (0x00602008). IPGR2 was previously 12,
        // doubling the half-duplex carrier-sense/defer window versus spec.
        mmio_write(self.base, E1000E_TIPG, 8 | (8 << 10) | (6 << 20));

        // TXDCTL
        if self.is_pch_spt_or_later() {
            // PCH-SPT: must set QUEUE_ENABLE (bit 25) and wait for it to latch
            let txdctl = TXDCTL_DMA_BURST | TXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
            let mut latched = false;
            for _ in 0..100u32 {
                Self::udelay(100);
                if mmio_read(self.base, E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE != 0 {
                    latched = true;
                    break;
                }
            }
            if !latched {
                // TDH will never advance and every send() will spin-fail: this
                // was previously silent, so a dead TX queue looked identical
                // to a healthy but idle one until traffic was expected.
                crate::klog_warn!(
                    "[e1000e] TXDCTL.QUEUE_ENABLE did not latch within 10ms — TX queue may be dead\n"
                );
            }
            // Mirror to queue 1 (Linux e1000_configure_tx)
            mmio_write(
                self.base,
                E1000E_TXDCTL1,
                mmio_read(self.base, E1000E_TXDCTL),
            );
            // IOSF PCIe compliance
            let iosfpc = mmio_read(self.base, E1000E_IOSFPC);
            mmio_write(self.base, E1000E_IOSFPC, iosfpc | 0x0001_0000);
            let _ = mmio_read(self.base, E1000E_IOSFPC);
        } else {
            mmio_write(
                self.base,
                E1000E_TXDCTL,
                TXDCTL_DMA_BURST | TXDCTL_FULL_TX_DESC_WB,
            );
        }

        // TARC0 bit 0 is required for correct TX arbitration on 82574/ICH.
        {
            let tarc = mmio_read(self.base, E1000E_TARC0) | 1;
            mmio_write(self.base, E1000E_TARC0, tarc);
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
        self.program_itr(E1000E_ITR_BALANCED);

        // Program RX ring base, length, head
        let rx_pa = self.rx_ring.paddr();
        mmio_write(self.base, E1000E_RDBAL, rx_pa as u32);
        mmio_write(self.base, E1000E_RDBAH, (rx_pa >> 32) as u32);
        mmio_write(
            self.base,
            E1000E_RDLEN,
            (NUM_RX * size_of::<RxDesc>()) as u32,
        );
        mmio_write(self.base, E1000E_RDH, 0);
        self.rx_next_to_clean = 0;
        self.rx_pending = None;

        // Fill RX ring (LK: post buffer per slot, legacy descriptor layout).
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        for i in 0..NUM_RX {
            let desc = unsafe { &mut *ring.add(i) };
            write_volatile(&mut desc.addr, self.rx_buf_paddr(i));
            write_volatile(&mut desc.len, 0);
            write_volatile(&mut desc.chksum, 0);
            write_volatile(&mut desc.status, 0);
            write_volatile(&mut desc.errors, 0);
            write_volatile(&mut desc.vlan, 0);
        }
        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.rx_ring_coherent,
            0,
            NUM_RX,
            size_of::<RxDesc>(),
            DmaSyncDir::ToDevice,
        );
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);

        // Legacy write-back only (LK / QEMU). Linux extended WB is for PCH+RFCTL_EXTEN.
        let mut rfctl = mmio_read(self.base, E1000E_RFCTL);
        rfctl &= !RFCTL_EXTEN;
        rfctl |= RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
        mmio_write(self.base, E1000E_RFCTL, rfctl);
        let _ = mmio_read(self.base, E1000E_RFCTL);

        // No multiqueue
        mmio_write(self.base, E1000E_MRQC, 0);

        // PCH: SRRCTL 2 KB + Drop_En. Discrete/QEMU: RCTL buffer size alone (LK).
        if self.is_pch() {
            mmio_write(self.base, E1000E_SRRCTL, 2 | (1 << 31));
        }

        // PCH-SPT: must set RXDCTL.QUEUE_ENABLE (bit 25) before RCTL.EN.
        // Do not program Linux FLAG2_DMA_BURST here: on I219 it produced
        // descriptor write-backs that the software checksum path then
        // treated as corrupt, dropping DHCPOFFER on real hardware only.
        if self.is_pch_spt_or_later() {
            let rxdctl = mmio_read(self.base, E1000E_RXDCTL) | RXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_RXDCTL, rxdctl);
            for _ in 0..100u32 {
                Self::udelay(100);
                if mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE != 0 {
                    break;
                }
            }
        }

        // Explicitly clear leftover RXCSUM from firmware / a previous OS.
        // CTRL_RST does not always zero it on PCH; a stale IPOFLD|TUOFLD is
        // exactly the hardware-only DHCP drop (QEMU starts from a clean model).
        mmio_write(self.base, E1000E_RXCSUM, 0);

        // Do NOT program RXCSUM. v0.5.0 left it off: the NIC then does not
        // fill IPE/TCPE, and we do not run a software verifier on every
        // frame. Enabling IPOFLD|TUOFLD + Checksum::Tx made QEMU (which
        // barely implements offload) keep working while real I219 dropped
        // UDP/DHCP whose hardware verdict disagreed with our fallback.

        // Doorbell: give (NUM_RX - 1) descriptors to hardware
        // RDT = last descriptor index hardware can use
        mmio_write(self.base, E1000E_RDT, (NUM_RX - 1) as u32);
        let _ = mmio_read(self.base, E1000E_RDT); // flush

        // Small settle before enabling RCTL
        Self::udelay(1_000);

        // RCTL: EN + BAM + SECRC; UPE/MPE come from set_rx_mode defaults
        // (promisc+allmulti at bring-up for DHCP/bridge compatibility).
        let mut rctl = RCTL_EN | RCTL_BAM | RCTL_SECRC;
        if self.rx_promisc {
            rctl |= RCTL_UPE | RCTL_MPE;
        } else if self.rx_allmulti {
            rctl |= RCTL_MPE;
        }
        mmio_write(self.base, E1000E_RCTL, rctl);
        let _ = mmio_read(self.base, E1000E_RCTL);
        if !self.rx_promisc && !self.rx_allmulti {
            let list = self.mc_list.clone();
            self.write_mta(&list);
        }

        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    // -----------------------------------------------------------------------
    // RX data path (LK e1000: drain by RDH, legacy rdesc, fragment reassembly)
    // -----------------------------------------------------------------------

    fn rx_rdh(&self) -> usize {
        unsafe { mmio_read(self.base, E1000E_RDH) as usize }
    }

    fn clear_rx_pending(&mut self) {
        self.rx_pending = None;
    }

    /// Give up on the frame currently being reassembled. `saw_eop` says
    /// whether the descriptor that triggered the abort was the last of its
    /// frame; when it was not, the rest of the chain is still on its way and
    /// [`rx_discard_until_eop`](Self::rx_discard_until_eop) swallows it.
    fn abort_rx_frame(&mut self, saw_eop: bool) {
        self.rx_pending = None;
        self.rx_discard_until_eop = !saw_eop;
        self.stats.rx_dropped += 1;
    }

    /// LK `irq_handler` RXO path: drop any in-flight multi-descriptor frame.
    pub fn handle_rx_irq(&mut self, icr: u32) {
        if icr & ICR_RXO != 0 {
            if self.rx_pending.is_some() {
                self.clear_rx_pending();
                self.stats.rx_dropped += 1;
            }
            crate::klog_warn!("[e1000e] RX overrun (ICR_RXO)\n");
        }
    }

    /// How many descriptors sit between `rx_next_to_clean` and a snapshot of
    /// RDH (exclusive of RDH itself — that slot is still owned by HW).
    fn rx_avail_to_rdh(&self, rdh: usize) -> usize {
        (rdh + NUM_RX - self.rx_next_to_clean) % NUM_RX
    }

    /// Invalidate a contiguous (or wrap-around) descriptor span for CPU read.
    /// On UC/coherent rings this is a single fence — the production I219 path.
    /// On WB fallback it rounds up to cache-line boundaries so one clflush
    /// covers several 16-byte descriptors instead of one sync call per slot.
    fn sync_rx_descs_from_device(&self, start: usize, count: usize) {
        if count == 0 {
            return;
        }
        if self.rx_ring_coherent {
            fence(Ordering::SeqCst);
            return;
        }
        // Round the invalidate window up to whole cache lines so adjacent
        // slots that share a line are covered by one clflush_span rather than
        // N overlapping ones. Safe for FromDevice: we only invalidate, never
        // write back a stale line onto a neighbour the NIC just stamped.
        let aligned_start = start - (start % RX_DESCS_PER_CACHE_LINE);
        let end = start + count;
        let aligned_end = (end + RX_DESCS_PER_CACHE_LINE - 1) & !(RX_DESCS_PER_CACHE_LINE - 1);
        let aligned_count = (aligned_end - aligned_start).min(NUM_RX);
        self.sync_rx_desc_span_wrapping(
            aligned_start % NUM_RX,
            aligned_count,
            DmaSyncDir::FromDevice,
        );
    }

    fn sync_rx_desc_span_wrapping(&self, start: usize, count: usize, dir: DmaSyncDir) {
        if count == 0 {
            return;
        }
        if start + count <= NUM_RX {
            dma_sync_rx_desc_span(
                &self.rx_ring,
                self.rx_ring_coherent,
                start,
                count,
                size_of::<RxDesc>(),
                dir,
            );
        } else {
            let first = NUM_RX - start;
            dma_sync_rx_desc_span(
                &self.rx_ring,
                self.rx_ring_coherent,
                start,
                first,
                size_of::<RxDesc>(),
                dir,
            );
            dma_sync_rx_desc_span(
                &self.rx_ring,
                self.rx_ring_coherent,
                0,
                count - first,
                size_of::<RxDesc>(),
                dir,
            );
        }
    }

    /// Process one RX slot. Caller has already invalidated the descriptor
    /// (batched in [`drain_rx_into_ready`](Self::drain_rx_into_ready)); this
    /// does not re-sync the descriptor itself.
    fn process_rx_slot(&mut self) -> Option<Vec<u8>> {
        let head = self.rx_next_to_clean;

        // Read the status byte on its own first and only pull the rest of the
        // descriptor once DD is set, with an acquire fence in between. The
        // NIC writes all 16 bytes back in one PCIe transaction, but the CPU's
        // loads are independent: a single `read_volatile` of the whole struct
        // plus a *trailing* fence lets `len` / `errors` be satisfied from a
        // load issued before the DD store became visible, i.e. from the
        // previous occupant of the slot.
        let desc_ptr = unsafe { self.rx_ring.as_ptr::<RxDesc>().add(head) };

        // Never advance past a slot HW owns without DD — skipping desyncs the ring.
        if unsafe { read_volatile(&(*desc_ptr).status) } & RXD_STAT_DD == 0 {
            return None;
        }

        fence(Ordering::Acquire);

        // Copy descriptor locally (LK: consistent snapshot after dma_sync).
        let rxd = unsafe { read_volatile(desc_ptr) };

        let len = rxd.len as usize;
        let eop = rxd.status & RXD_STAT_EOP != 0;
        let expected_addr = self.rx_buf_paddr(head);

        // Swallow the leftovers of a chain we already abandoned (see
        // `rx_discard_until_eop`). Must come before any delivery decision.
        if self.rx_discard_until_eop {
            if eop {
                self.rx_discard_until_eop = false;
            }
            self.rx_next_to_clean = (head + 1) % NUM_RX;
            unsafe {
                self.recycle_rx_slot(head);
            }
            return None;
        }

        if rxd.addr != expected_addr {
            crate::klog_warn!(
                "[e1000e] RX addr mismatch slot={} desc={:#x} expected={:#x}\n",
                head,
                rxd.addr,
                expected_addr
            );
            self.abort_rx_frame(eop);
            self.rx_next_to_clean = (head + 1) % NUM_RX;
            unsafe {
                self.recycle_rx_slot(head);
            }
            return None;
        }

        // Linux never drops on IPE/TCPE (checksum opinion, often wrong for
        // UDP checksum 0 / DHCP). Only wire-level damage is fatal. Do not
        // run a software verifier here: without RXCSUM the IPCS/UDPCS bits
        // are clear, so every IPv4 frame would be checksummed against a
        // cache-line that on real hardware is not always coherent with the
        // DMA write — QEMU is, which is why DHCP lived in the VM and died
        // on I219. AF_PACKET (udhcpc) must see the frame; smoltcp still
        // verifies TCP/UDP itself (Checksum::Both).
        if rxd.errors & !RXD_ERR_CSUM_VERDICT != 0 || len == 0 || len > BUF_SIZE {
            self.abort_rx_frame(eop);
            self.rx_next_to_clean = (head + 1) % NUM_RX;
            unsafe {
                self.recycle_rx_slot(head);
            }
            return None;
        }

        // Invalidate exactly the bytes we're about to read (`len`, rounded up
        // to a cache line by clflush_span), not the whole BUF_SIZE (2048)
        // buffer: the slice built below is `..len`, so nothing past it is
        // ever read. Flushing the full buffer on every packet regardless of
        // its actual size — up to 32 cache lines to read a 64-byte ACK — was
        // pure per-packet overhead with no correctness benefit.
        dma_sync_region(
            &self.rx_buf_pool,
            self.rx_buf_coherent,
            head * BUF_SIZE,
            len,
            DmaSyncDir::FromDevice,
        );
        let frag =
            unsafe { core::slice::from_raw_parts(self.rx_buf_vaddr(head) as *const u8, len) };

        let complete = if let Some(ref mut pending) = self.rx_pending {
            if pending.len().saturating_add(len) > MAX_RX_FRAME_BYTES {
                self.abort_rx_frame(eop);
                None
            } else {
                pending.extend_from_slice(frag);
                if eop {
                    Some(self.rx_pending.take().unwrap())
                } else {
                    None
                }
            }
        } else if eop {
            // Recycled Vec when possible: a bulk download otherwise heap-allocs
            // one ~1500 B buffer per frame.
            let mut pkt = self.take_rx_frame(len);
            pkt.extend_from_slice(frag);
            Some(pkt)
        } else {
            let mut pending = Vec::with_capacity(BUF_SIZE.min(MAX_RX_FRAME_BYTES));
            pending.extend_from_slice(frag);
            self.rx_pending = Some(pending);
            None
        };

        if let Some(ref pkt) = complete {
            self.stats.rx_packets += 1;
            self.stats.rx_bytes += pkt.len() as u64;
        }

        self.rx_next_to_clean = (head + 1) % NUM_RX;
        unsafe {
            self.recycle_rx_slot(head);
        }
        complete
    }

    /// Drain up to [`RX_DRAIN_BUDGET`] slots into [`rx_ready`](Self::rx_ready),
    /// batching the descriptor FromDevice sync once for the whole window.
    fn drain_rx_into_ready(&mut self) {
        if self.rx_ready.len() >= RX_READY_CAP {
            return;
        }
        let rdh = self.rx_rdh();
        let avail = self.rx_avail_to_rdh(rdh);
        if avail == 0 {
            return;
        }
        let budget = avail
            .min(RX_DRAIN_BUDGET)
            .min(RX_READY_CAP - self.rx_ready.len());
        // One invalidate for the whole window we may walk — not one per slot.
        self.sync_rx_descs_from_device(self.rx_next_to_clean, budget);

        for _ in 0..budget {
            if self.rx_next_to_clean == rdh {
                break;
            }
            let head_before = self.rx_next_to_clean;
            if let Some(pkt) = self.process_rx_slot() {
                // Corruption probe is diagnostic-only: walking every IPv4
                // header on the hot path costs measurable cycles per frame.
                // Keep it behind the existing verbose gate so production
                // throughput is not taxed for a watchdog counter.
                if E1000E_LOG_VERBOSE && rx_ipv4_hdr_csum_bad(&pkt) {
                    self.rx_csum_bad += 1;
                }
                self.rx_ready.push_back(pkt);
                if self.rx_ready.len() >= RX_READY_CAP {
                    break;
                }
            } else if self.rx_next_to_clean == head_before {
                // Slot not ready (DD clear) — wait for next poll.
                break;
            }
        }
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        if !self.hw_running {
            return None;
        }
        if let Some(pkt) = self.rx_ready.pop_front() {
            return Some(pkt);
        }
        self.drain_rx_into_ready();
        self.rx_ready.pop_front()
    }

    /// LK `add_pktbuf_to_rxring_locked`, minus the doorbell: refill the
    /// descriptor and mark the RDT doorbell dirty. Ringing it is deferred to
    /// [`flush_rx_doorbell`](Self::flush_rx_doorbell) — see there for why.
    ///
    /// ToDevice descriptor sync: on UC/coherent rings (production I219 path)
    /// we skip the per-slot fence and publish once in `flush_rx_doorbell`. On
    /// WB fallback we keep a per-slot sync — clflush is already line-sized, and
    /// batching a longer ToDevice span would only widen the false-sharing
    /// window that UC was introduced to eliminate.
    unsafe fn recycle_rx_slot(&mut self, i: usize) {
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let desc = &mut *ring.add(i);
        write_volatile(&mut desc.status, 0);
        write_volatile(&mut desc.errors, 0);
        write_volatile(&mut desc.len, 0);
        write_volatile(&mut desc.chksum, 0);
        write_volatile(&mut desc.vlan, 0);
        write_volatile(&mut desc.addr, self.rx_buf_paddr(i));
        if !self.rx_ring_coherent {
            dma_sync_rx_desc_span(
                &self.rx_ring,
                self.rx_ring_coherent,
                i,
                1,
                size_of::<RxDesc>(),
                DmaSyncDir::ToDevice,
            );
        }
        self.rx_doorbell_dirty = true;
    }

    /// Ring the RDT doorbell once for every descriptor recycled since the
    /// last flush, instead of on every single packet. This used to be a
    /// `mmio_write` immediately followed by a synchronous `mmio_read` on
    /// every recycled slot — one MMIO round trip per packet, plus a second
    /// live read of RDH per slot in the old `receive()` loop. On real
    /// hardware an MMIO read forces the CPU to stall for a PCIe completion;
    /// under emulation (QEMU) each MMIO access typically costs a full VM
    /// exit. Coalescing the doorbell across a whole receive burst — flushed
    /// once per poll cycle or `recv()` call by the caller, not per packet —
    /// is the difference between one doorbell ring per *packet* and one per
    /// *batch*. Deferring is safe: it only delays telling hardware about
    /// freed buffers, it never blocks forward progress on anything.
    pub fn flush_rx_doorbell(&mut self) {
        if !self.rx_doorbell_dirty {
            return;
        }
        self.rx_doorbell_dirty = false;
        let rdt = (self.rx_next_to_clean + NUM_RX - 1) % NUM_RX;
        unsafe {
            // Publish any recycled descriptor stores (UC: this is the only
            // fence for the whole recycle batch; WB already synced per slot).
            fence(Ordering::SeqCst);
            mmio_write(self.base, E1000E_RDT, rdt as u32);
        }
    }

    // -----------------------------------------------------------------------
    // TX data path
    // -----------------------------------------------------------------------

    /// Is the slot at `tx_tail` free to post into? Intel documents that TDH is
    /// NOT a reliable completion signal — it reflects descriptors the NIC has
    /// prefetched into its internal FIFO, which can run ahead of the actual
    /// DMA write-back. The only reliable signal is the descriptor's own DD
    /// (Descriptor Done) status bit, written back by hardware because every
    /// posted descriptor carries CMD.RS (Report Status). `e1000.rs` uses the
    /// same DD-bit check; this mirrors it instead of trusting TDH.
    fn can_send(&self) -> bool {
        self.hw_running && self.tx_can_post(/* sync */ true)
    }

    /// Can a frame be posted at `tx_tail`? True when the tail slot itself is
    /// free (DD) AND the slot after it is free too — the latter is the guard
    /// slot the hardware needs.
    ///
    /// The NIC has no ownership bit of its own: it works descriptors from TDH
    /// up to (but excluding) TDT and treats `TDH == TDT` as an EMPTY ring.
    /// Deciding "free" purely per-slot by DD let software post all `NUM_TX`
    /// slots; the tail then wrapped onto the head and the TDT write handed
    /// the NIC a value equal to TDH. From its side that ring was empty: the
    /// posted frames were never fetched, their DD bits never came back, and
    /// every later send spun to the [`TX_SEND_SPIN_LIMIT`] and dropped — TX
    /// dead for good. It needs the NIC to be stalled while software posts a
    /// full lap (link flapping under an upload, flow-control pause), which
    /// QEMU's synchronous TX never does, so it only bit real hardware.
    /// Linux's `e1000_desc_unused` bounds in-flight descriptors to
    /// `count - 1` for the same reason. Requiring `tail + 1` to be free
    /// guarantees at most `NUM_TX - 1` descriptors are ever outstanding, so
    /// TDT can never catch TDH from behind.
    #[inline]
    fn tx_can_post(&self, sync: bool) -> bool {
        self.tx_dd_at(self.tx_tail, sync) && self.tx_dd_at((self.tx_tail + 1) % NUM_TX, sync)
    }

    /// Read DD at slot `idx`. When `sync` is true and the ring is write-back,
    /// invalidate the descriptor line first. UC rings skip the sync entirely —
    /// uncached loads already see hardware write-back, so a spin loop can
    /// re-check with a single volatile load and no fence/clflush.
    #[inline]
    fn tx_dd_at(&self, idx: usize, sync: bool) -> bool {
        if sync && !self.tx_ring_coherent {
            dma_sync_rx_desc_span(
                &self.tx_ring,
                self.tx_ring_coherent,
                idx,
                1,
                size_of::<TxDesc>(),
                DmaSyncDir::FromDevice,
            );
        }
        let ring = self.tx_ring.as_ptr::<TxDesc>();
        let status = unsafe { read_volatile(&(*ring.add(idx)).status) };
        status & TX_STAT_DD != 0
    }

    /// Sync any descriptors posted since the last flush (single contiguous
    /// span; wrap closes the batch early in [`post_tx_frame`]).
    fn sync_pending_tx_descs(&mut self) {
        if let Some(start) = self.tx_desc_dirty_start.take() {
            let count = self.tx_desc_dirty_count;
            self.tx_desc_dirty_count = 0;
            if count > 0 {
                dma_sync_rx_desc_span(
                    &self.tx_ring,
                    self.tx_ring_coherent,
                    start,
                    count,
                    size_of::<TxDesc>(),
                    DmaSyncDir::ToDevice,
                );
            }
        }
    }

    /// Ring TDT once for every descriptor posted since the last flush. Safe to
    /// call when clean (no-op). Must run before spinning on a full ring so the
    /// NIC can drain previously-posted frames, and after a smoltcp TX burst so
    /// one MMIO write covers the whole batch.
    pub fn flush_tx_doorbell(&mut self) {
        if !self.tx_doorbell_dirty {
            return;
        }
        self.sync_pending_tx_descs();
        unsafe {
            fence(Ordering::SeqCst);
            mmio_write(self.base, E1000E_TDT, self.tx_tail as u32);
        }
        self.tx_doorbell_dirty = false;
    }

    /// Copy `data` into the next free TX slot and advance the software tail
    /// without ringing TDT. Caller must [`flush_tx_doorbell`](Self::flush_tx_doorbell)
    /// before the NIC will transmit (and before waiting on DD for a full ring).
    fn post_tx_frame(&mut self, data: &[u8]) -> DeviceResult {
        if data.is_empty() || data.len() > BUF_SIZE {
            return Err(DeviceError::InvalidParam);
        }

        if !self.link_up_refreshed() {
            return Err(DeviceError::NotReady);
        }

        if !self.tx_can_post(/* sync */ true) {
            return Err(DeviceError::NotReady);
        }

        let idx = self.tx_tail;
        let ring = self.tx_ring.as_ptr::<TxDesc>();
        let desc = unsafe { &mut *ring.add(idx) };

        let buf = unsafe {
            core::slice::from_raw_parts_mut(self.tx_buf_vaddr(idx) as *mut u8, data.len())
        };
        buf.copy_from_slice(data);

        self.stats.tx_packets += 1;
        self.stats.tx_bytes += data.len() as u64;

        // Write descriptor fields (cmd last so HW doesn't fetch a partial
        // descriptor). ToDevice sync of the descriptor itself is deferred to
        // `flush_tx_doorbell` so a burst pays one sync/fence for many slots.
        unsafe {
            write_volatile(&mut desc.addr, self.tx_buf_paddr(idx));
            write_volatile(&mut desc.len, data.len() as u16);
            write_volatile(&mut desc.cso, 0);
            write_volatile(&mut desc.status, 0);
            write_volatile(&mut desc.css, 0);
            write_volatile(&mut desc.special, 0);
        }
        compiler_fence(Ordering::SeqCst);
        unsafe {
            write_volatile(&mut desc.cmd, TX_CMD_POST);
        }
        // Payload must hit RAM before TDT; buffers are WB+clflush, not UC.
        dma_sync_region(
            &self.tx_buf_pool,
            self.tx_buf_coherent,
            idx * BUF_SIZE,
            data.len(),
            DmaSyncDir::ToDevice,
        );

        // Track a contiguous dirty descriptor span for the batched ToDevice sync.
        match self.tx_desc_dirty_start {
            None => {
                self.tx_desc_dirty_start = Some(idx);
                self.tx_desc_dirty_count = 1;
            }
            Some(start) => {
                let count = self.tx_desc_dirty_count;
                let next_expected = (start + count) % NUM_TX;
                if idx == next_expected && start + count < NUM_TX {
                    self.tx_desc_dirty_count = count + 1;
                } else {
                    // Gap or ring wrap: close the current span, start a new one.
                    self.sync_pending_tx_descs();
                    self.tx_desc_dirty_start = Some(idx);
                    self.tx_desc_dirty_count = 1;
                }
            }
        }

        self.tx_tail = (idx + 1) % NUM_TX;
        self.tx_doorbell_dirty = true;
        Ok(())
    }

    pub fn send(&mut self, data: &[u8]) -> DeviceResult {
        if !self.hw_running {
            return Err(DeviceError::NotReady);
        }
        self.post_tx_frame(data)?;
        self.flush_tx_doorbell();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Watchdog — simple link check
    // -----------------------------------------------------------------------

    /// Returns `true` when carrier state changed.
    pub unsafe fn watchdog_tick(&mut self) -> bool {
        let now = timer_now_as_micros();
        let status = mmio_read(self.base, E1000E_STATUS);
        let link = status & STATUS_LU != 0;
        let mut link_changed = false;

        if link != self.link_up {
            link_changed = true;
            self.link_up = link;
            if link {
                // Linux re-tunes the inter-packet gap on every link change,
                // because the right value depends on the speed we just got.
                self.apply_link_speed_tuning(status);
                // Report what auto-negotiation actually settled on. Nothing
                // else in the system surfaces link speed (there is no ethtool
                // and no /sys/class/net/*/speed), so without this a link that
                // negotiated 100 Mb/s half duplex on a gigabit switch looks
                // exactly like a healthy one — and explains a 10x throughput
                // shortfall that would otherwise be blamed on the stack.
                crate::klog_warn!(
                    "[e1000e] link UP {}Mb/s {} STATUS={:#010x}\n",
                    status_speed_mbps(status),
                    if status & STATUS_FD != 0 {
                        "full-duplex"
                    } else {
                        "HALF-duplex"
                    },
                    status
                );
                // Second opinion, straight from the PHY. STATUS is what the
                // MAC's auto-speed detection made of the MAC-PHY interconnect;
                // the MII registers are what the two link partners agreed on.
                // When they disagree the problem is between MAC and PHY (the
                // interconnect stuck in SMBus mode, say), not on the wire.
                match self.read_phy_negotiated() {
                    Some((phy_speed, phy_fd)) => {
                        if phy_speed != status_speed_mbps(status)
                            || phy_fd != (status & STATUS_FD != 0)
                        {
                            crate::klog_warn!(
                                "[e1000e] PHY says {}Mb/s fd={} but MAC STATUS says {}Mb/s fd={} — MAC-PHY interconnect mismatch\n",
                                phy_speed,
                                phy_fd,
                                status_speed_mbps(status),
                                status & STATUS_FD != 0
                            );
                        }
                    }
                    None => {
                        crate::klog_warn!(
                            "[e1000e] link UP but the PHY reports auto-negotiation incomplete\n"
                        );
                    }
                }
            } else {
                crate::klog_warn!("[e1000e] link DOWN\n");
            }
        }

        if link_changed || now >= self.watchdog_log_next_us {
            self.watchdog_log_next_us = now.saturating_add(E1000E_WATCHDOG_LOG_US);
            // GPRC>0 means the MAC received frames. MPC>0 means frames arrived but
            // were dropped (no free descriptors or DMA ring not armed).
            let gprc = mmio_read(self.base, E1000E_GPRC);
            let mpc = mmio_read(self.base, E1000E_MPC);
            // Good octets in/out since the last sample. These are the MAC's
            // own counters and clear on read, so they measure what actually
            // crossed the wire — independent of anything the driver or the
            // stack thinks it did. Pair them with MPC / RNBC: throughput well
            // under the negotiated link speed WITH those at zero points at
            // the stack (software checksums, window, poll latency), whereas
            // non-zero MPC/RNBC means the driver is not returning RX
            // descriptors fast enough.
            let gorc = mmio_read(self.base, E1000E_GORCL) as u64
                | ((mmio_read(self.base, E1000E_GORCH) as u64) << 32);
            let gotc = mmio_read(self.base, E1000E_GOTCL) as u64
                | ((mmio_read(self.base, E1000E_GOTCH) as u64) << 32);
            let rnbc = mmio_read(self.base, E1000E_RNBC);
            let elapsed_us = now.saturating_sub(self.throughput_last_us);
            self.throughput_last_us = now;
            // Skip the very first sample (no baseline) and any interval
            // shorter than 100 ms, where rounding noise swamps the result.
            // Octets/us * 8000 = kbit/s. Mbit/s truncated every rate under a
            // megabit to a useless "0"; a gigabit link still fits easily.
            let (rx_kbps, tx_kbps) = if elapsed_us >= 100_000 {
                (
                    gorc.saturating_mul(8_000) / elapsed_us,
                    gotc.saturating_mul(8_000) / elapsed_us,
                )
            } else {
                (0, 0)
            };
            // GPTC>0 means the MAC actually transmitted frames in this interval.
            // If a download stalls with GPRC stuck but GPTC still climbing, the
            // NIC is still sending our ACKs/window updates and the peer has gone
            // silent (TCP window / server side); if GPTC also stalls and TDH ==
            // TDT, our TX ring drained and we stopped queueing ACKs (TX side).
            let gptc = mmio_read(self.base, E1000E_GPTC);
            let tdh = mmio_read(self.base, E1000E_TDH);
            let tdt = mmio_read(self.base, E1000E_TDT);
            let csum_bad = self.rx_csum_bad;
            let csum_fp = self.rx_csum_hw_false_positive;
            let tx_dropped = self.tx_dropped;
            crate::klog_info!(
                "[e1000e] watchdog: link={} {}Mb/s fd={} rx={}kb/s tx={}kb/s GPRC={} MPC={} RNBC={} rx_pkt={} rx_drop={} rx_csum_bad={} hw_csum_fp={} GPTC={} tx_pkt={} tx_drop={} TDH={} TDT={} itr={}\n",
                link,
                status_speed_mbps(status),
                status & STATUS_FD != 0,
                rx_kbps,
                tx_kbps,
                gprc,
                mpc,
                rnbc,
                self.stats.rx_packets,
                self.stats.rx_dropped,
                csum_bad,
                csum_fp,
                gptc,
                self.stats.tx_packets,
                tx_dropped,
                tdh,
                tdt,
                self.itr_setting
            );
        }
        link_changed
    }

    fn program_itr(&mut self, itr: u32) {
        self.itr_setting = itr;
        unsafe { mmio_write(self.base, E1000E_ITR, itr) };
    }

    /// Adaptive interrupt throttling.
    ///
    /// Previous logic retuned on every `rx_event`, resetting the packet window
    /// each poll. Under a steady download that meant `rx_delta` stayed tiny
    /// (a few packets per IRQ) and the driver stuck on LOW_LATENCY — maximum
    /// interrupt rate, worst throughput. Now:
    /// - heavy single-poll bursts upgrade to THROUGHPUT immediately
    /// - full window samples only run on the tune period (no early reset)
    /// - hysteresis holds THROUGHPUT until traffic clearly quiets
    /// - quiet windows drop to LOW_LATENCY so ACKs stay snappy
    fn tune_itr(&mut self, now_us: u64, rx_burst: u64) {
        if rx_burst >= E1000E_ITR_BURST_THROUGHPUT {
            if self.itr_setting != E1000E_ITR_THROUGHPUT {
                self.program_itr(E1000E_ITR_THROUGHPUT);
            }
            // Stretch the next window sample; do not clear `itr_last_rx_packets`
            // so the eventual period delta still reflects sustained load.
            self.itr_tune_next_us = now_us.saturating_add(E1000E_ITR_TUNE_PERIOD_US);
            return;
        }

        if now_us < self.itr_tune_next_us {
            return;
        }
        self.itr_tune_next_us = now_us.saturating_add(E1000E_ITR_TUNE_PERIOD_US);
        let rx_now = self.stats.rx_packets;
        let rx_delta = rx_now.saturating_sub(self.itr_last_rx_packets);
        self.itr_last_rx_packets = rx_now;
        let target = choose_itr(self.itr_setting, rx_delta, 0);
        if target != self.itr_setting {
            self.program_itr(target);
        }
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
    /// Micros timestamp of the last time `poll_pending` was set true. Lets
    /// `heal_stuck_poll_pending` detect a bottom-half that was evicted from
    /// the deferred-job queue instead of one merely awaiting its turn.
    poll_pending_set_us: Arc<AtomicU64>,
    pub link_up_seen: Arc<AtomicBool>,
    /// ICR bits read by [`Scheme::handle_irq`] that it could not hand to a
    /// bottom-half because one was already pending. Reading ICR clears it in
    /// hardware, so without this the causes are simply lost — most visibly
    /// `ICR_LSC` (a carrier change then waits up to a full watchdog period)
    /// and `ICR_RXO` (the in-flight reassembly is never reset). Merged into
    /// the next [`poll_with_irq_hint`](E1000eInterface::poll_with_irq_hint),
    /// IRQ-driven or periodic.
    pending_icr: Arc<AtomicU32>,
    watchdog_job_scheduled: Arc<AtomicBool>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

/// Unmask the NIC interrupt sources the driver services.
fn ims_rearm_at(base: usize) {
    unsafe {
        compiler_fence(Ordering::SeqCst);
        mmio_write(base, E1000E_IMS, IMS_REARM);
        let _ = mmio_read(base, E1000E_IMS);
        fence(Ordering::SeqCst);
    }
}

/// Owns the IRQ bottom-half's "poll pending, IMS masked" state. Dropping it
/// clears `poll_pending` and then re-arms IMS — in that order, so an IRQ that
/// fires right after the unmask finds the flag clear and queues a fresh
/// bottom-half instead of taking `handle_irq`'s "already pending" fast path.
///
/// It is moved into the deferred closure so the same release happens whether
/// the job runs to completion or is dropped unexecuted (evicted from the
/// deferred-job queue under pressure). Without that, an evicted bottom-half
/// left the NIC interrupt-masked and `poll_pending` stuck until
/// `heal_stuck_poll_pending` noticed half a second later.
struct PollPendingGuard {
    pending: Arc<AtomicBool>,
    base: usize,
}

impl Drop for PollPendingGuard {
    fn drop(&mut self) {
        self.pending.store(false, Ordering::SeqCst);
        ims_rearm_at(self.base);
    }
}

impl E1000eInterface {
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
        // The guard is created OUTSIDE the closure and moved in, so it is
        // dropped — and the flag cleared — even when the job never runs.
        // `deferred_job.rs` caps its queue at 256 and evicts (drops) entries
        // unexecuted under pressure; a guard built inside the closure body
        // only ever ran when the body did, so an evicted watchdog left
        // `watchdog_job_scheduled` true forever and link supervision dead
        // for the life of the kernel.
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let guard = Guard(Arc::clone(&self.watchdog_job_scheduled));
        crate::utils::deferred_job::push_deferred_job(move || {
            let _g = guard;
            let (link_changed, link_up) = {
                let mut hw = me.driver.hw.lock();
                let changed = unsafe { hw.watchdog_tick() };
                (changed, hw.link_up)
            };
            if link_changed {
                me.link_up_seen.store(link_up, Ordering::Release);
            }
            // Drop the Guard (clears watchdog_job_scheduled to false) BEFORE
            // calling schedule_watchdog, so the re-schedule can set the flag
            // back to true and push a new job without the Guard clobbering it.
            drop(_g);
            me.schedule_watchdog(false);
        });
    }

    /// If `poll_pending` has been stuck true far longer than any legitimate
    /// deferred-job latency, the IRQ bottom-half that owns clearing it (see
    /// `handle_irq`) was evicted from the shared deferred-job queue —
    /// `deferred_job.rs` caps it at 256 entries and silently drops the oldest
    /// without running it. Without this, every future interrupt would take
    /// `handle_irq`'s "already pending" fast path forever (re-arm and return)
    /// and `poll_with_irq_hint` would never run again except via periodic
    /// external polling (`poll_ifaces_throttled`). Called from `poll()`,
    /// which is reached by that periodic polling regardless of IRQ state, so
    /// this reliably self-heals even on an interface that stopped receiving
    /// interrupts entirely.
    fn heal_stuck_poll_pending(&self) {
        if !self.poll_pending.load(Ordering::SeqCst) {
            return;
        }
        let set_at = self.poll_pending_set_us.load(Ordering::Relaxed);
        let now = timer_now_as_micros();
        if set_at != 0 && now.saturating_sub(set_at) > POLL_PENDING_STUCK_US {
            crate::klog_warn!(
                "[e1000e] poll_pending stuck for >{}us, self-healing (deferred job likely evicted)\n",
                POLL_PENDING_STUCK_US
            );
            self.poll_pending.store(false, Ordering::SeqCst);
        }
    }

    fn ims_rearm(&self) {
        ims_rearm_at(self.base);
    }

    /// NIC poll; `irq_icr` carries ICR bits when invoked from the deferred IRQ bottom-half.
    fn poll_with_irq_hint(&self, irq_icr: u32) -> DeviceResult {
        // Pick up any causes a previous `handle_irq` read out of ICR but had
        // to drop because a bottom-half was already pending (see `pending_icr`).
        let irq_icr = irq_icr | self.pending_icr.swap(0, Ordering::AcqRel);
        let now = timer_now_as_micros();
        let ts = Instant::from_micros(now as i64);
        // One hw lock for watchdog-due check + RXO + link-arm (skips STATUS
        // MMIO when already link_up). Avoids three separate Mutex acquires on
        // every IRQ bottom-half / periodic poll.
        let (due, rx_baseline) = {
            let mut hw = self.driver.hw.lock();
            let due = hw.link_watchdog_next_us <= now;
            hw.handle_rx_irq(irq_icr);
            unsafe {
                hw.ensure_rx_armed_if_link_up();
            }
            (due, hw.stats.rx_packets)
        };
        if due {
            self.schedule_watchdog(false);
        }

        // Do NOT manually toggle interrupts around the SOCKETS + iface locks.
        // `super::intr_off/on` are raw hardware toggles (drivers_intr_*) that
        // bypass the kernel-sync Mutex's push_off/pop_off noff accounting. The
        // Mutex already keeps interrupts disabled for the whole locked critical
        // section, so a NIC IRQ cannot reenter and deadlock on SOCKETS while we
        // hold it. Force-toggling the raw flag here (as this code previously did,
        // mislabeled the "rtlx / e1000 pattern") desyncs pop_off and re-enables
        // interrupts mid-section under an outer lock holder — panicking
        // ("RefCell already borrowed") or deadlocking on SOCKETS under SMP.
        // e1000.rs and rtlx.rs document this exact hazard; rely on the Mutex alone.
        let sockets = get_sockets();
        let mut had_rx = (irq_icr & ICR_RX_ANY) != 0;
        // Acquire the socket set + iface with try_lock, exactly as the loopback
        // driver does. A blocking `lock()` here DEADLOCKS: this poll runs from a
        // deferred job that a socket syscall drains WHILE it holds the socket
        // set (e.g. a read/write draining the NIC to make progress), so on this
        // IRQ-off single-CPU kernel the acquire spins forever on a lock the same
        // CPU already owns — the >8s spinlock the red banner reported at this
        // line during a large `apk`/`wget` download. If either lock is held,
        // skip the smoltcp poll: the frame that owns the socket set drives it on
        // release, and `handle_rx_irq` above already serviced the RX-overrun bit
        // so nothing is lost.
        if let (Some(mut sockets), Some(mut iface)) = (sockets.try_lock(), self.iface.try_lock()) {
            match iface.poll(&mut sockets, ts) {
                Ok(true) => had_rx = true,
                Ok(false) => {}
                Err(e) => warn!("e1000e smoltcp poll: {:?}", e),
            }
        }

        super::net_flush_deferred_packets();
        {
            let mut hw = self.driver.hw.lock();
            // Ring the RDT doorbell once for however many packets `iface.poll`
            // just drained above, instead of once per packet inside the
            // receive loop — see `flush_rx_doorbell` for why this matters.
            hw.flush_rx_doorbell();
            // Same idea as RDT: one TDT write for the whole smoltcp TX burst.
            hw.flush_tx_doorbell();
            let rx_burst = hw.stats.rx_packets.saturating_sub(rx_baseline);
            hw.tune_itr(now, rx_burst);
        }

        if had_rx {
            super::wake_net_rx_waiters();
        }
        Ok(())
    }
}

impl Scheme for E1000eInterface {
    fn name(&self) -> &str {
        "e1000e"
    }

    /// Minimal IRQ top-half (same as [`e1000::E1000Interface`]): read ICR, mask IMS,
    /// queue one deferred poll. RX waiters are woken from [`E1000eInterface::poll_with_irq_hint`]
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

        if self
            .poll_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // A bottom-half is already queued and the ICR read above cleared
            // these causes in hardware. Stash them so the poll still sees
            // them instead of dropping a link-state change or an RX overrun.
            self.pending_icr.fetch_or(icr, Ordering::AcqRel);
            self.ims_rearm();
            return;
        }
        self.poll_pending_set_us
            .store(timer_now_as_micros(), Ordering::Relaxed);
        unsafe {
            mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
            let _ = mmio_read(self.base, E1000E_IMC);
            fence(Ordering::SeqCst);
        }

        let guard = PollPendingGuard {
            pending: self.poll_pending.clone(),
            base: self.base,
        };
        let me = self.clone();
        // Front of the deferred queue: NIC bottom-half must run before a
        // backlog of lower-urgency jobs can age it out of the 256-cap FIFO.
        // If it is aged out anyway, dropping `guard` with the closure still
        // clears `poll_pending` and re-arms IMS (see `PollPendingGuard`);
        // `heal_stuck_poll_pending` remains as the belt-and-braces fallback.
        crate::utils::deferred_job::push_deferred_job_front(move || {
            let guard = guard;
            if icr & ICR_LSC != 0 {
                me.schedule_watchdog(true);
            }
            let _ = me.poll_with_irq_hint(icr);
            // Clears poll_pending BEFORE re-arming IMS so that any IRQ that
            // fires after the unmask finds poll_pending=false and properly
            // queues a new deferred job. With IMS masked throughout the poll,
            // new packets accumulate in ICR; re-arming causes the NIC to
            // re-assert the IRQ for those accumulated bits.
            drop(guard);
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
            if addrs.contains(&cidr) {
                return;
            }
            for slot in addrs.iter_mut() {
                if (slot.address().is_unspecified() && slot.prefix_len() == 0)
                    || (slot.address() == IpAddress::v4(240, 0, 0, 0) && slot.prefix_len() == 32)
                {
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
            if !hw.hw_running {
                // Admin-up after down: bring rings/RCTL back without PHY soft-reset.
                unsafe {
                    hw.reinit_locked()?;
                }
            } else {
                hw.link_up = false;
            }
        }
        self.schedule_watchdog(true);
        Ok(())
    }
    fn admin_down(&self) -> DeviceResult {
        unsafe {
            self.driver.hw.lock().hw_down();
        }
        Ok(())
    }
    fn set_promiscuous(&self, on: bool) -> DeviceResult {
        let mut hw = self.driver.hw.lock();
        let allmulti = hw.rx_allmulti;
        let list = hw.mc_list.clone();
        unsafe {
            hw.set_rx_mode(on, allmulti, &list);
        }
        Ok(())
    }
    fn set_allmulti(&self, on: bool) -> DeviceResult {
        let mut hw = self.driver.hw.lock();
        let promisc = hw.rx_promisc;
        let list = hw.mc_list.clone();
        unsafe {
            hw.set_rx_mode(promisc, on, &list);
        }
        Ok(())
    }
    fn set_multicast_list(&self, addrs: &[[u8; 6]]) -> DeviceResult {
        let mut hw = self.driver.hw.lock();
        let promisc = hw.rx_promisc;
        let allmulti = hw.rx_allmulti;
        unsafe {
            hw.set_rx_mode(promisc, allmulti, addrs);
        }
        Ok(())
    }
    fn link_carrier_up(&self) -> bool {
        self.driver.hw.lock().link_up
            || unsafe { mmio_read(self.base, E1000E_STATUS) & STATUS_LU != 0 }
    }
    fn poll(&self) -> DeviceResult {
        self.heal_stuck_poll_pending();
        self.poll_with_irq_hint(0)?;
        self.ims_rearm();
        Ok(())
    }
    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        let mut hw = self.driver.hw.lock();
        let pkt = hw.receive();
        // This path doesn't go through `poll_with_irq_hint` (which flushes
        // after draining smoltcp), so it must flush its own doorbell here.
        hw.flush_rx_doorbell();
        drop(hw);
        if let Some(pkt) = pkt {
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
    fn can_recv(&self) -> bool {
        true
    }
    fn can_send(&self) -> bool {
        self.driver.hw.lock().can_send()
    }
    fn add_route(&self, cidr: IpCidr, gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        match gateway {
            Some(IpAddress::Ipv4(gw)) => {
                if cidr.prefix_len() == 0 {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                    iface
                        .routes_mut()
                        .add_default_ipv4_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                }
                let mut routes = self.routes.lock();
                routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv4(_)) && r.dst.prefix_len() == 0));
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv4(gw)),
                });
            }
            Some(IpAddress::Ipv6(gw)) => {
                if cidr.prefix_len() == 0 {
                    let _ = iface.routes_mut().remove_default_ipv6_route();
                    iface
                        .routes_mut()
                        .add_default_ipv6_route(gw)
                        .map_err(|_| DeviceError::IoError)?;
                }
                let mut routes = self.routes.lock();
                routes.retain(|r| !(matches!(r.dst, IpCidr::Ipv6(_)) && r.dst.prefix_len() == 0));
                routes.push(RouteInfo {
                    dst: cidr,
                    gateway: Some(IpAddress::Ipv6(gw)),
                });
            }
            None => {
                self.routes.lock().push(RouteInfo { dst: cidr, gateway });
            }
            _ => {}
        }
        Ok(())
    }
    fn del_route(&self, cidr: IpCidr, _gateway: Option<smoltcp::wire::IpAddress>) -> DeviceResult {
        let mut iface = self.iface.lock();
        if cidr.prefix_len() == 0 {
            match cidr {
                IpCidr::Ipv4(_) => {
                    let _ = iface.routes_mut().remove_default_ipv4_route();
                }
                IpCidr::Ipv6(_) => {
                    let _ = iface.routes_mut().remove_default_ipv6_route();
                }
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
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv4(v4.network()),
                        gateway: None,
                    });
                }
                IpCidr::Ipv6(v6) if v6.prefix_len() > 0 => {
                    res.push(RouteInfo {
                        dst: IpCidr::Ipv6(v6.network()),
                        gateway: None,
                    });
                }
                _ => {}
            }
        }
        res
    }
    fn get_stats(&self) -> NetStats {
        self.driver.hw.lock().merged_stats()
    }
    fn get_mtu(&self) -> usize {
        1500
    }
}

// ---------------------------------------------------------------------------
// smoltcp Device impl
// ---------------------------------------------------------------------------

pub struct E1000eRxToken {
    data: Vec<u8>,
    driver: E1000eDriver,
}
pub struct E1000eTxToken(E1000eDriver);

impl phy::Device<'_> for E1000eDriver {
    type RxToken = E1000eRxToken;
    type TxToken = E1000eTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let mut hw = self.hw.lock();
        hw.receive().map(|pkt| {
            (
                E1000eRxToken {
                    data: pkt,
                    driver: self.clone(),
                },
                E1000eTxToken(self.clone()),
            )
        })
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
        // Do NOT set max_burst_size. smoltcp clamps the TCP window to
        // `burst * MSS` and then stores it in a u16: with burst=64 that is
        // 64*1474=94336, which wraps to 28800. Unscaled, that is a ~28 KiB
        // window — at a 10 ms delayed-ACK / poll RTT, 1 MSS/10 ms = 1.2 Mbps.
        // We have 256 RX descriptors (~384 KiB of NIC buffering) and a 512 KiB
        // TCP receive buffer; the clamp was written for a 4-buffer MCU NIC.
        caps.max_burst_size = None;
        // RXCSUM is not programmed (v0.5.0). TX inserts only FCS (IFCS), so
        // smoltcp must compute and verify IP/TCP/UDP checksums itself.
        caps
    }
}

impl phy::RxToken for E1000eRxToken {
    fn consume<R, F>(self, _ts: Instant, f: F) -> SmolResult<R>
    where
        F: FnOnce(&mut [u8]) -> SmolResult<R>,
    {
        let mut data = self.data;
        // AF_PACKET tap: only copies when a callback is registered (see
        // `net_defer_packet`). Without a tap this used to allocate+copy every
        // frame into a queue that was immediately discarded on flush.
        super::net_defer_packet(&data);
        let result = f(&mut data);
        if let Some(mut hw) = self.driver.hw.try_lock() {
            hw.recycle_rx_frame(data);
        }
        result
    }
}

impl phy::TxToken for E1000eTxToken {
    fn consume<R, F>(self, _ts: Instant, len: usize, f: F) -> SmolResult<R>
    where
        F: FnOnce(&mut [u8]) -> SmolResult<R>,
    {
        // Stack scratch instead of `vec![0; len]` for the common MTU-sized
        // case: a 1536-byte stack memset is far cheaper than a heap
        // allocation per frame. Do NOT silently clamp oversized requests
        // though: the closure expects a slice of exactly `len`, and a
        // truncated slice would either panic in `copy_from_slice` or emit a
        // shorter/corrupt frame. Fall back to a heap buffer for the rare jumbo
        // request instead.
        //
        // The scratch is a real `[u8; N]`, not a `MaybeUninit` reinterpreted
        // through `from_raw_parts_mut`: handing out a `&mut [u8]` over
        // uninitialised memory is undefined behaviour (the bytes are `poison`
        // until written), which LLVM is free to exploit however it likes.
        let mut scratch = [0u8; TX_SCRATCH_LEN];
        let mut heap_buf = Vec::new();
        let buf: &mut [u8] = if len <= TX_SCRATCH_LEN {
            &mut scratch[..len]
        } else {
            heap_buf.resize(len, 0);
            heap_buf.as_mut_slice()
        };
        let result = f(buf)?;

        let mut hw = self.0.hw.lock();
        // NEVER silently drop a frame smoltcp handed us. The ingress path
        // (`socket_ingress`) dispatches the ACK / window-update generated in
        // direct response to a received segment through the TxToken paired with
        // the RxToken — and if that send fails, smoltcp only logs it and moves
        // on, having ALREADY advanced its remote_last_ack/remote_last_win state.
        // The dropped ACK is never re-emitted, so the peer keeps waiting on a
        // window it thinks is closed and the whole transfer deadlocks. On real
        // hardware the 256-deep TX ring transiently fills under an RX burst
        // (ITR throttles TX completion), losing exactly these ACKs — the silent
        // mid-download stall seen only on real hardware, never under QEMU (which
        // completes TX synchronously, so the ring never fills). The NIC drains
        // the ring autonomously via DMA, so spin briefly on a free slot instead
        // of dropping.
        //
        // Posted-but-unflushed frames from earlier tokens in this poll must
        // reach the NIC before we wait on DD, or a full ring never drains.
        hw.flush_tx_doorbell();

        // Link down: nothing will complete, so waiting is pointless. Bail now
        // rather than spinning the full TX_SEND_SPIN_LIMIT with the hw lock
        // (IRQs off) held — `post_tx_frame` re-read STATUS by MMIO on every
        // one of those iterations, several ms per attempted frame under QEMU.
        // Not counted in `tx_dropped`: that counter means "lost with the link
        // up", the deadlock signature the watchdog reports.
        if !hw.link_up_refreshed() {
            return Err(smoltcp::Error::Exhausted);
        }

        let mut tries = 0usize;
        loop {
            // Cheap DD poll: UC rings skip dma_sync; WB re-syncs only every
            // TX_SPIN_SYNC_INTERVAL iterations (stale DD=0 is a safe miss).
            let sync = !hw.tx_ring_coherent && tries.is_multiple_of(TX_SPIN_SYNC_INTERVAL);
            if hw.tx_can_post(sync) {
                match hw.post_tx_frame(buf) {
                    Ok(()) => {
                        // Leave TDT dirty so subsequent tokens in this burst
                        // coalesce into one doorbell at poll end.
                        return Ok(result);
                    }
                    Err(DeviceError::NotReady) => {}
                    Err(_) => {
                        hw.tx_dropped += 1;
                        return Err(smoltcp::Error::Exhausted);
                    }
                }
            }
            if tries >= TX_SEND_SPIN_LIMIT {
                hw.tx_dropped += 1;
                return Err(smoltcp::Error::Exhausted);
            }
            tries += 1;
            core::hint::spin_loop();
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: ensure RX ring is armed when link comes up
// ---------------------------------------------------------------------------

impl E1000eHw {
    /// `link_up`, refreshed from STATUS.LU only while it is still false (a
    /// link-down transition is the watchdog's job). Shared by the TX paths so
    /// a frame handed to us with the link down is refused up front instead
    /// of spinning on a ring that will never drain.
    fn link_up_refreshed(&mut self) -> bool {
        unsafe {
            self.ensure_rx_armed_if_link_up();
        }
        self.link_up
    }

    pub unsafe fn ensure_rx_armed_if_link_up(&mut self) {
        // Once link is already known up, this can only ever set link_up=true
        // again — a no-op — so skip the MMIO STATUS read. This runs on every
        // single poll (IRQ-driven or periodic), so during steady-state
        // traffic (the common case) it was an unconditional register read
        // that accomplished nothing. Link-DOWN transitions are still caught
        // by `watchdog_tick`'s own STATUS read on its normal cadence.
        if self.link_up {
            return;
        }
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU != 0 {
            self.link_up = true;
            // Report it HERE as well as in the watchdog. This runs on every
            // poll and on every transmit, so it almost always wins the race
            // to observe carrier — and by flipping `link_up` behind the
            // watchdog's back it made `watchdog_tick`'s `link != link_up`
            // test false, so the "link UP" line was in practice never
            // printed at all. The one message that says what speed and
            // duplex auto-negotiation settled on was dead code.
            crate::klog_warn!(
                "[e1000e] link UP {}Mb/s {} STATUS={:#010x}\n",
                status_speed_mbps(status),
                if status & STATUS_FD != 0 {
                    "full-duplex"
                } else {
                    "HALF-duplex"
                },
                status
            );
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
        name,
        vaddr,
        irq,
        pci.id.device_id,
        E1000E_DRIVER_TAG
    );

    // Split the DMA memory by access pattern, fixing both the UC-slowness AND
    // the WB false-sharing failure modes (QEMU, coherent with no real cache,
    // never exposes either):
    //
    // Descriptor RINGS -> uncached (try_coherent). A descriptor is 16 bytes, so
    // four pack into one 64-byte cache line. With WB+clflush, recycling one
    // descriptor dirties its line and the ToDevice clflush writes the WHOLE line
    // back — including neighbours the NIC may have just stamped DD into (in RAM)
    // a moment earlier. That stale write-back erases their DD bits, the driver
    // never sees those completions, and RX wedges: a deterministic mid-transfer
    // stall on real hardware (the race cannot happen under QEMU's synchronous
    // device model). UC sidesteps it entirely — and the rings are tiny, so the
    // uncached cost is negligible.
    //
    // Packet BUFFERS -> write-back + clflush (alloc_uninit). These carry the
    // bulk RX/TX payload, where uncached reads are ~100x slower and starve the
    // RX drain on a multi-MB stream. They are safe as WB: BUF_SIZE is a multiple
    // of the cache line and the pool is page-aligned, so buffers never share a
    // line (no false sharing), RX buffers are only read by the CPU (FromDevice
    // clflush only invalidates, never writes a stale line back), and
    // alloc_uninit now evicts each region's lines at allocation so it starts
    // clean.
    let (rx_ring, rx_ring_coherent) =
        DmaRegion::alloc_uninit_try_coherent(NUM_RX * size_of::<RxDesc>())
            .ok_or(DeviceError::DmaError)?;
    let (tx_ring, tx_ring_coherent) =
        DmaRegion::alloc_uninit_try_coherent(NUM_TX * size_of::<TxDesc>())
            .ok_or(DeviceError::DmaError)?;
    let rx_buf_pool = DmaRegion::alloc_uninit(NUM_RX * BUF_SIZE).ok_or(DeviceError::DmaError)?;
    let tx_buf_pool = DmaRegion::alloc_uninit(NUM_TX * BUF_SIZE).ok_or(DeviceError::DmaError)?;
    let rx_buf_coherent = false;
    let tx_buf_coherent = false;
    crate::klog_warn!(
        "[e1000e] LK-RX path: rings UC(rx={} tx={}), buffers write-back+clflush\n",
        rx_ring_coherent,
        tx_ring_coherent
    );

    // Alignment checks
    for (label, region, align, span) in [
        ("rx_ring", &rx_ring, DMA_DESC_ALIGN, DMA_RING_BYTES),
        ("tx_ring", &tx_ring, DMA_DESC_ALIGN, DMA_TX_RING_BYTES),
        ("rx_buf_pool", &rx_buf_pool, 64, NUM_RX * BUF_SIZE),
        ("tx_buf_pool", &tx_buf_pool, 64, NUM_TX * BUF_SIZE),
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
        rx_buf_pool,
        rx_ring_coherent,
        rx_buf_coherent,
        rx_next_to_clean: 0,
        rx_pending: None,
        rx_discard_until_eop: false,
        rx_ready: VecDeque::new(),
        rx_doorbell_dirty: false,
        tx_ring,
        tx_buf_pool,
        tx_ring_coherent,
        tx_buf_coherent,
        tx_tail: 0,
        tx_doorbell_dirty: false,
        tx_desc_dirty_start: None,
        tx_desc_dirty_count: 0,
        rx_frame_pool: Vec::new(),
        stats: NetStats::default(),
        rx_csum_bad: 0,
        rx_csum_hw_false_positive: 0,
        tx_dropped: 0,
        link_up: false,
        link_watchdog_next_us: 0,
        watchdog_log_next_us: 0,
        itr_setting: E1000E_ITR_BALANCED,
        itr_last_rx_packets: 0,
        itr_tune_next_us: 0,
        throughput_last_us: 0,
        hw_running: false,
        // Start promiscuous like the historical LK path (DHCP/bridges); userspace
        // can clear via NetScheme::set_promiscuous / set_allmulti.
        rx_promisc: true,
        rx_allmulti: true,
        mc_list: Vec::new(),
    };

    unsafe {
        hw.reset_and_init()?;
    }

    let mac_bytes = hw.mac;
    crate::klog_warn!(
        "e1000e: {} {:#x}:{:#x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} tag={}\n",
        name,
        pci.id.vendor_id,
        pci.id.device_id,
        mac_bytes[0],
        mac_bytes[1],
        mac_bytes[2],
        mac_bytes[3],
        mac_bytes[4],
        mac_bytes[5],
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
        0xfe80,
        0,
        0,
        0,
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
    // No dummy default via 0.0.0.0 — smoltcp would ARP 0.0.0.0 for every
    // off-link dest until DHCP overwrote it, and a full 4-slot table made
    // `add_default_ipv4_route` return Exhausted so the real gateway never
    // installed. DHCP/`ip route` fills this in.
    let routes_storage: &'static mut [Option<(IpCidr, Route)>] =
        Box::leak(vec![None; 4].into_boxed_slice());
    let routes = Routes::new(routes_storage);
    let neighbor_cache = NeighborCache::new(BTreeMap::new());

    let iface = InterfaceBuilder::new(driver.clone())
        .ethernet_addr(ethernet_addr)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(ip_addrs.clone())
        .routes(routes)
        .finalize();

    let link_up_seen = Arc::new(AtomicBool::new(unsafe {
        mmio_read(vaddr, E1000E_STATUS) & STATUS_LU != 0
    }));
    let e1000e_iface = E1000eInterface {
        iface: Arc::new(Mutex::new(iface)),
        driver,
        name,
        irq,
        base: vaddr,
        poll_pending: Arc::new(AtomicBool::new(false)),
        poll_pending_set_us: Arc::new(AtomicU64::new(0)),
        link_up_seen,
        pending_icr: Arc::new(AtomicU32::new(0)),
        watchdog_job_scheduled: Arc::new(AtomicBool::new(false)),
        routes: Arc::new(Mutex::new(vec![])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    };

    Ok(e1000e_iface)
}

// ---------------------------------------------------------------------------
// PCI driver registration
// ---------------------------------------------------------------------------

pub struct E1000eDriverPci;

impl PciDriver for E1000eDriverPci {
    fn name(&self) -> &str {
        "e1000e"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        vendor_id == 0x8086 && e1000e_device_matched(device_id)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        irq: Option<usize>,
    ) -> DeviceResult<Device> {
        crate::klog_warn!(
            "e1000e: probe PCI {:#x}:{:#x} tag={}\n",
            dev.id.vendor_id,
            dev.id.device_id,
            E1000E_DRIVER_TAG
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
        let name = crate::net::next_eth_ifname();

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

#[cfg(test)]
mod rx_ring_tests {
    //! Host bench for the e1000e RX descriptor-ring state machine.
    //!
    //! Drives the *real* `receive`/`process_rx_slot`/`recycle_rx_slot` against a
    //! simulated NIC: host memory stands in for the descriptor ring, the packet
    //! buffers and the MMIO register file (RDH/RDT), with phys==virt identity
    //! mapping. We play the hardware (fill descriptors, advance RDH) and assert
    //! the driver hands every frame back intact, recycles slots, advances RDT,
    //! and keeps working across a full-ring fill and ring wrap-around — the
    //! conditions a large download exercises and where a wedge would hide.

    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    extern crate std;
    use std::alloc::{alloc_zeroed, Layout};

    // --- mock kernel hooks: identity-mapped host memory, no-op the rest ---
    #[no_mangle]
    extern "C" fn drivers_dma_alloc(pages: usize) -> usize {
        let layout = Layout::from_size_align(pages * 4096, 4096).unwrap();
        unsafe { alloc_zeroed(layout) as usize }
    }
    #[no_mangle]
    extern "C" fn drivers_dma_dealloc(_p: usize, _pages: usize) -> i32 {
        0
    }
    #[no_mangle]
    extern "C" fn drivers_phys_to_virt(p: usize) -> usize {
        p
    }
    #[no_mangle]
    extern "C" fn drivers_virt_to_phys(v: usize) -> usize {
        v
    }
    #[no_mangle]
    extern "C" fn drivers_dma_mark_uncached(_p: usize, _pages: usize) -> i32 {
        0
    }
    #[no_mangle]
    extern "C" fn drivers_dma_verify_uncached(_p: usize, _pages: usize) -> i32 {
        0
    }
    #[no_mangle]
    extern "C" fn drivers_timer_now_as_micros() -> u64 {
        crate::nvme::nvme_queue::test_clock::now()
    }
    #[no_mangle]
    extern "C" fn drivers_klog_emit(_priority: u8, _msg: *const u8, _len: usize) {}
    #[no_mangle]
    extern "C" fn drivers_intr_on() {}
    #[no_mangle]
    extern "C" fn drivers_intr_off() {}
    #[no_mangle]
    extern "C" fn drivers_intr_get() -> bool {
        false
    }
    #[no_mangle]
    extern "C" fn drivers_wake_net_rx_waiters() {}
    #[no_mangle]
    extern "C" fn drivers_net_drain() {}

    fn reg_read(base: usize, reg: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }
    }
    fn reg_write(base: usize, reg: usize, val: u32) {
        unsafe { core::ptr::write_volatile((base + reg * 4) as *mut u32, val) };
    }

    /// Build an `E1000eHw` over host memory, with the RX ring initialized exactly
    /// like `init_rx` (each descriptor points at its buffer, RDH=0, RDT=NUM_RX-1).
    pub(super) fn make_hw() -> E1000eHw {
        let regs = Box::leak(vec![0u32; 0x4000].into_boxed_slice());
        let base = regs.as_ptr() as usize;
        let rx_ring = DmaRegion::alloc(NUM_RX * core::mem::size_of::<RxDesc>()).unwrap();
        let rx_buf_pool = DmaRegion::alloc_uninit(NUM_RX * BUF_SIZE).unwrap();
        let tx_ring = DmaRegion::alloc(NUM_TX * core::mem::size_of::<TxDesc>()).unwrap();
        let tx_buf_pool = DmaRegion::alloc(NUM_TX * BUF_SIZE).unwrap();

        let mut hw = E1000eHw {
            base,
            pci_loc: Location {
                bus: 0,
                device: 0,
                function: 0,
            },
            device_id: 0x10d3,
            mac: [0x52, 0x54, 0, 0, 0, 1],
            rx_ring,
            rx_buf_pool,
            rx_ring_coherent: false,
            rx_buf_coherent: false,
            rx_next_to_clean: 0,
            rx_pending: None,
            rx_discard_until_eop: false,
            rx_ready: VecDeque::new(),
            rx_doorbell_dirty: false,
            tx_ring,
            tx_buf_pool,
            tx_ring_coherent: false,
            tx_buf_coherent: false,
            tx_tail: 0,
            tx_doorbell_dirty: false,
            tx_desc_dirty_start: None,
            tx_desc_dirty_count: 0,
            rx_frame_pool: Vec::new(),
            stats: NetStats::default(),
            rx_csum_bad: 0,
            rx_csum_hw_false_positive: 0,
            tx_dropped: 0,
            link_up: true,
            link_watchdog_next_us: 0,
            watchdog_log_next_us: 0,
            itr_setting: 0,
            itr_last_rx_packets: 0,
            itr_tune_next_us: 0,
            throughput_last_us: 0,
            hw_running: true,
            rx_promisc: true,
            rx_allmulti: true,
            mc_list: Vec::new(),
        };
        // Initialize the descriptor ring (mirror of init_rx).
        let ring = hw.rx_ring.as_ptr::<RxDesc>();
        for i in 0..NUM_RX {
            unsafe {
                let d = &mut *ring.add(i);
                d.addr = hw.rx_buf_paddr(i);
                d.len = 0;
                d.chksum = 0;
                d.status = 0;
                d.errors = 0;
                d.vlan = 0;
            }
        }
        reg_write(base, E1000E_RDH, 0);
        reg_write(base, E1000E_RDT, (NUM_RX - 1) as u32);

        // Initialize the TX descriptor ring (mirror of init_tx): every slot
        // starts DD (done/free) so `can_send`/`send` can post from slot 0.
        let tx_ring_ptr = hw.tx_ring.as_ptr::<TxDesc>();
        for i in 0..NUM_TX {
            unsafe {
                let d = &mut *tx_ring_ptr.add(i);
                d.addr = hw.tx_buf_paddr(i);
                d.len = 0;
                d.cso = 0;
                d.cmd = 0;
                d.status = TX_STAT_DD;
                d.css = 0;
                d.special = 0;
            }
        }
        reg_write(base, E1000E_TDH, 0);
        reg_write(base, E1000E_TDT, 0);
        hw
    }

    /// Play the hardware: DMA `data` into slot `slot`, mark it DD|EOP, advance RDH.
    fn hw_deliver(hw: &E1000eHw, slot: usize, data: &[u8]) {
        assert!(data.len() <= BUF_SIZE);
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                hw.rx_buf_vaddr(slot) as *mut u8,
                data.len(),
            );
            let d = &mut *hw.rx_ring.as_ptr::<RxDesc>().add(slot);
            d.addr = hw.rx_buf_paddr(slot);
            d.len = data.len() as u16;
            d.status = RXD_STAT_DD | RXD_STAT_EOP;
            d.errors = 0;
        }
        // HW head now points one past the slot it just filled.
        reg_write(hw.base, E1000E_RDH, ((slot + 1) % NUM_RX) as u32);
    }

    /// Like `hw_deliver`, but with explicit checksum-indication status bits
    /// (IXSM / IPCS / TCPCS / UDPCS) or'd in, to model what the NIC did or
    /// did not validate for this frame.
    fn hw_deliver_with_status(hw: &E1000eHw, slot: usize, data: &[u8], status: u8) {
        hw_deliver(hw, slot, data);
        unsafe {
            let d = &mut *hw.rx_ring.as_ptr::<RxDesc>().add(slot);
            d.status |= status;
        }
    }

    /// Like `hw_deliver_with_status`, plus an explicit `errors` byte.
    fn hw_deliver_with_errors(hw: &E1000eHw, slot: usize, data: &[u8], status: u8, errors: u8) {
        hw_deliver_with_status(hw, slot, data, status);
        unsafe {
            let d = &mut *hw.rx_ring.as_ptr::<RxDesc>().add(slot);
            d.errors = errors;
        }
    }

    #[test]
    fn rx_ipe_tcpe_are_delivered_crc_is_dropped() {
        // IPE/TCPE are the NIC's checksum opinion, not wire damage. DHCP/DNS
        // UDP with checksum 0 is the classic false positive; dropping it is
        // why real-hardware DHCP died while QEMU (no offload) kept working.
        let mut hw = make_hw();
        let good = ipv4_udp_frame(b"dns reply the NIC disliked");
        let mut zero_csum = ipv4_udp_frame(b"router reply, no udp checksum");
        zero_csum[14 + 26] = 0;
        zero_csum[14 + 27] = 0;
        let mut corrupt = ipv4_udp_frame(b"actually corrupt");
        corrupt[14 + 28 + 4] ^= 0x10;
        let crc_err = ipv4_udp_frame(b"wire-damaged");
        hw_deliver_with_errors(&hw, 0, &good, RXD_STAT_IPCS | RXD_STAT_UDPCS, RXD_ERR_TCPE);
        hw_deliver_with_errors(&hw, 1, &zero_csum, RXD_STAT_IPCS, RXD_ERR_TCPE);
        hw_deliver_with_errors(
            &hw,
            2,
            &corrupt,
            RXD_STAT_IPCS | RXD_STAT_UDPCS,
            RXD_ERR_TCPE,
        );
        hw_deliver_with_errors(&hw, 3, &good, RXD_STAT_UDPCS, RXD_ERR_IPE);
        hw_deliver_with_errors(&hw, 4, &crc_err, RXD_STAT_IPCS | RXD_STAT_UDPCS, 0x01);

        assert_eq!(hw.receive().expect("slot 0"), good);
        assert_eq!(hw.receive().expect("slot 1"), zero_csum);
        assert_eq!(hw.receive().expect("slot 2"), corrupt);
        assert_eq!(hw.receive().expect("slot 3"), good);
        assert!(hw.receive().is_none());
        assert_eq!(hw.stats.rx_packets, 4);
        assert_eq!(hw.stats.rx_dropped, 1, "only CRC/wire error");
        hw.flush_rx_doorbell();
        assert_eq!(reg_read(hw.base, E1000E_RDT) as usize, 4);
    }

    /// Ethernet + IPv4 + UDP frame with a correct UDP and IP header checksum.
    fn ipv4_udp_frame(payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 14 + 20 + 8 + payload.len()];
        f[0..6].copy_from_slice(&[0x52, 0x54, 0, 0, 0, 1]);
        f[6..12].copy_from_slice(&[0x52, 0x54, 0, 0, 0, 2]);
        f[12..14].copy_from_slice(&[0x08, 0x00]);
        let ip = &mut f[14..];
        ip[0] = 0x45;
        let total = (20 + 8 + payload.len()) as u16;
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        ip[8] = 64;
        ip[9] = 17;
        ip[12..16].copy_from_slice(&[10, 0, 2, 2]);
        ip[16..20].copy_from_slice(&[10, 0, 2, 15]);
        let udp_len = (8 + payload.len()) as u16;
        ip[20..22].copy_from_slice(&1234u16.to_be_bytes());
        ip[22..24].copy_from_slice(&5678u16.to_be_bytes());
        ip[24..26].copy_from_slice(&udp_len.to_be_bytes());
        ip[28..].copy_from_slice(payload);
        // UDP checksum
        let mut sum = csum_add(0, &ip[12..20]);
        sum += 17 + udp_len as u32;
        sum = csum_add(sum, &ip[20..]);
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        let mut c = !(sum as u16);
        if c == 0 {
            c = 0xffff;
        }
        ip[26..28].copy_from_slice(&c.to_be_bytes());
        // IPv4 header checksum
        let mut sum = csum_add(0, &ip[..20]);
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        let c = !(sum as u16);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        f
    }

    /// Ethernet + IPv6 + TCP frame (no options) with a correct TCP checksum.
    fn ipv6_tcp_frame(payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 14 + 40 + 20 + payload.len()];
        f[0..6].copy_from_slice(&[0x52, 0x54, 0, 0, 0, 1]);
        f[6..12].copy_from_slice(&[0x52, 0x54, 0, 0, 0, 2]);
        f[12..14].copy_from_slice(&[0x86, 0xdd]);
        let ip = &mut f[14..];
        ip[0] = 0x60;
        let plen = (20 + payload.len()) as u16;
        ip[4..6].copy_from_slice(&plen.to_be_bytes());
        ip[6] = 6;
        ip[7] = 64;
        ip[8] = 0xfe;
        ip[9] = 0x80;
        ip[23] = 1;
        ip[24] = 0xfe;
        ip[25] = 0x80;
        ip[39] = 2;
        let tcp = &mut ip[40..];
        tcp[0..2].copy_from_slice(&443u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&40000u16.to_be_bytes());
        tcp[12] = 0x50; // data offset 5
        tcp[13] = 0x18; // PSH|ACK
        tcp[14..16].copy_from_slice(&1024u16.to_be_bytes());
        tcp[20..].copy_from_slice(payload);
        let mut sum = csum_add(0, &ip[8..40]);
        sum += 6 + plen as u32;
        sum = csum_add(sum, &ip[40..]);
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        let c = !(sum as u16);
        ip[56..58].copy_from_slice(&c.to_be_bytes());
        f
    }

    #[test]
    fn rx_sw_csum_accepts_good_and_rejects_corrupt_frames() {
        let payload = b"hello e1000e checksum offload";
        let v4 = ipv4_udp_frame(payload);
        let v6 = ipv6_tcp_frame(payload);
        // NIC validated nothing (bare DD|EOP): software must verify.
        assert!(!rx_sw_csum_bad(&v4, 0), "good IPv4/UDP must pass");
        assert!(!rx_sw_csum_bad(&v6, 0), "good IPv6/TCP must pass");
        let mut bad4 = v4.clone();
        bad4[14 + 28 + 3] ^= 0x40; // UDP payload byte
        assert!(
            rx_sw_csum_bad(&bad4, 0),
            "corrupt IPv4/UDP payload must fail"
        );
        let mut bad4h = v4.clone();
        bad4h[14 + 8] = 1; // TTL: breaks the IP header checksum only
        assert!(rx_sw_csum_bad(&bad4h, 0), "corrupt IPv4 header must fail");
        assert!(
            !rx_sw_csum_bad(&bad4h, RXD_STAT_IPCS | RXD_STAT_UDPCS),
            "a frame the NIC reports as validated is trusted (errors byte would have dropped it)"
        );
        let mut bad6 = v6.clone();
        bad6[14 + 40 + 20 + 5] ^= 0x01;
        assert!(
            rx_sw_csum_bad(&bad6, 0),
            "corrupt IPv6/TCP payload must fail"
        );
        assert!(
            !rx_sw_csum_bad(&bad6, RXD_STAT_TCPCS),
            "TCPCS set: the NIC already verified it"
        );
        assert!(
            rx_sw_csum_bad(&bad6, RXD_STAT_TCPCS | RXD_STAT_IXSM),
            "IXSM overrides TCPCS: the NIC says ignore its indication"
        );
        // IPv4 UDP with checksum 0 (no checksum) is legal and must not be dropped.
        let mut nocsum = v4.clone();
        nocsum[14 + 26] = 0;
        nocsum[14 + 27] = 0;
        assert!(!rx_sw_csum_bad(&nocsum, 0));
        // Fragments and non-IP frames are never rejected here.
        let mut frag = v4.clone();
        frag[14 + 6] = 0x20; // MF (header checksum now stale: say the NIC validated it)
        assert!(!rx_sw_csum_bad(&frag, RXD_STAT_IPCS));
        let mut arp = v4.clone();
        arp[12] = 0x08;
        arp[13] = 0x06;
        assert!(!rx_sw_csum_bad(&arp, 0));
        // Short / truncated frames must not panic.
        assert!(!rx_sw_csum_bad(&v4[..30], 0));
        assert!(!rx_sw_csum_bad(&v6[..50], 0));
        assert!(!rx_sw_csum_bad(&[], 0));
    }

    #[test]
    fn rx_does_not_drop_unvalidated_frames() {
        // process_rx_slot must not software-checksum-drop: udhcpc's AF_PACKET
        // path needs DHCPOFFER even when the NIC left IPCS/TCPCS clear.
        let mut hw = make_hw();
        let good = ipv6_tcp_frame(b"payload-0");
        let mut corrupt = ipv6_tcp_frame(b"payload-1");
        corrupt[14 + 40 + 20 + 2] ^= 0x80;
        hw_deliver_with_status(&hw, 0, &corrupt, 0);
        hw_deliver_with_status(&hw, 1, &corrupt, RXD_STAT_TCPCS);
        hw_deliver_with_status(&hw, 2, &good, 0);
        assert_eq!(hw.receive().expect("slot 0"), corrupt);
        assert_eq!(hw.receive().expect("slot 1"), corrupt);
        assert_eq!(hw.receive().expect("slot 2"), good);
        assert!(hw.receive().is_none());
        assert_eq!(hw.stats.rx_packets, 3);
        assert_eq!(hw.stats.rx_dropped, 0);
        hw.flush_rx_doorbell();
        assert_eq!(reg_read(hw.base, E1000E_RDT) as usize, 2);
    }

    fn pkt(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add(i as u8).wrapping_mul(31))
            .collect()
    }

    #[test]
    fn rx_single_packet_roundtrips() {
        let mut hw = make_hw();
        let p = pkt(7, 512);
        hw_deliver(&hw, 0, &p);
        let got = hw.receive().expect("expected a frame");
        assert_eq!(got, p, "frame payload mismatch");
        assert_eq!(hw.stats.rx_packets, 1);
        // The RDT doorbell is deferred (batched across a receive burst, see
        // `flush_rx_doorbell`) rather than rung on every single packet, so
        // the caller must flush explicitly before the slot is handed back to
        // HW via RDT.
        hw.flush_rx_doorbell();
        assert_eq!(
            reg_read(hw.base, E1000E_RDT) as usize,
            0,
            "RDT should point at recycled slot 0"
        );
    }

    #[test]
    fn rx_ready_queues_burst_for_subsequent_pops() {
        let mut hw = make_hw();
        // Deliver more than one frame so the first receive() drain fills
        // rx_ready; the next pops must not need a fresh RDH-driven walk of
        // empty slots.
        for i in 0..8usize {
            hw_deliver(&hw, i, &pkt(i as u8, 64));
        }
        let first = hw.receive().expect("first frame");
        assert_eq!(first, pkt(0, 64));
        assert!(
            !hw.rx_ready.is_empty(),
            "drain should stage remaining frames in rx_ready"
        );
        assert!(
            hw.rx_ready.len() <= RX_READY_CAP,
            "rx_ready must respect RX_READY_CAP"
        );
        for i in 1..8usize {
            let got = hw
                .receive()
                .unwrap_or_else(|| panic!("missing staged frame {}", i));
            assert_eq!(got, pkt(i as u8, 64), "staged frame {} mismatch", i);
        }
        assert!(hw.rx_ready.is_empty());
        assert!(hw.receive().is_none());
        assert_eq!(hw.stats.rx_packets, 8);
    }

    #[test]
    fn rx_desc_batch_sync_covers_avail_window() {
        // Smoke: with a non-coherent ring (make_hw default), draining a burst
        // must still deliver every frame — the batched FromDevice span must
        // cover every slot we walk, including cache-line rounding.
        let mut hw = make_hw();
        assert!(!hw.rx_ring_coherent);
        let n = RX_DESCS_PER_CACHE_LINE * 3 + 1; // crosses several lines
        for i in 0..n {
            hw_deliver(&hw, i, &pkt(i as u8, 96));
        }
        for i in 0..n {
            let got = hw.receive().unwrap_or_else(|| panic!("missing {}", i));
            assert_eq!(got, pkt(i as u8, 96));
        }
        hw.flush_rx_doorbell();
        assert_eq!(reg_read(hw.base, E1000E_RDT) as usize, (n - 1) % NUM_RX);
    }

    #[test]
    fn rx_doorbell_is_batched_not_rung_per_packet() {
        let mut hw = make_hw();
        // Deliver several packets up front, draining each one via receive()
        // WITHOUT flushing in between — mirroring how a real poll burst
        // drains many packets before the caller flushes once at the end.
        for i in 0..5usize {
            hw_deliver(&hw, i, &pkt(i as u8, 64));
        }
        for i in 0..5usize {
            let got = hw
                .receive()
                .unwrap_or_else(|| panic!("missing frame {}", i));
            assert_eq!(got, pkt(i as u8, 64), "frame {i} mismatch");
            // The doorbell must NOT move until flush_rx_doorbell is called —
            // recycling a slot only marks it dirty, it doesn't ring RDT.
            assert_eq!(
                reg_read(hw.base, E1000E_RDT) as usize,
                NUM_RX - 1,
                "RDT must stay at its initial value until flushed (packet {i})"
            );
        }
        // One flush must catch up the whole batch in a single write, jumping
        // straight to the last recycled slot instead of replaying each one.
        hw.flush_rx_doorbell();
        assert_eq!(
            reg_read(hw.base, E1000E_RDT) as usize,
            4,
            "flush should jump RDT straight to the last recycled slot"
        );
        // A second flush with nothing new recycled since must be a no-op.
        hw.flush_rx_doorbell();
        assert_eq!(reg_read(hw.base, E1000E_RDT) as usize, 4);
    }

    #[test]
    fn rx_frame_pool_reuses_capacity() {
        let mut hw = make_hw();
        let mut buf = hw.take_rx_frame(1200);
        buf.resize(1200, 0xab);
        hw.recycle_rx_frame(buf);
        assert_eq!(hw.rx_frame_pool.len(), 1);
        let reused = hw.take_rx_frame(64);
        assert!(hw.rx_frame_pool.is_empty());
        assert!(
            reused.capacity() >= 1200,
            "recycled Vec should keep its capacity"
        );
        assert!(reused.is_empty());
    }

    #[test]
    fn rx_burst_in_order() {
        let mut hw = make_hw();
        let n = 200usize;
        for i in 0..n {
            hw_deliver(&hw, i, &pkt(i as u8, 64 + i % 900));
        }
        for i in 0..n {
            let got = hw
                .receive()
                .unwrap_or_else(|| panic!("missing frame {}", i));
            assert_eq!(got, pkt(i as u8, 64 + i % 900), "frame {i} mismatch");
        }
        assert!(hw.receive().is_none(), "no more frames expected");
        assert_eq!(hw.stats.rx_packets, n as u64);
    }

    #[test]
    fn rx_full_ring_then_recover() {
        let mut hw = make_hw();
        // Fill every usable slot (HW leaves one guard between RDH and RDT).
        let fill = NUM_RX - 1;
        for i in 0..fill {
            hw_deliver(&hw, i, &pkt(i as u8, 128));
        }
        let mut drained = 0;
        while let Some(got) = hw.receive() {
            assert_eq!(got, pkt(drained as u8, 128), "frame {drained} mismatch");
            drained += 1;
        }
        assert_eq!(drained, fill, "should drain the whole ring");
        // After draining, RDT must have advanced so HW can use slots again:
        // deliver one more past the wrap and confirm it is received.
        let next = fill % NUM_RX;
        hw_deliver(&hw, next, &pkt(0xAB, 256));
        let got = hw.receive().expect("ring did not recover after full drain");
        assert_eq!(got, pkt(0xAB, 256));
    }

    /// Play the hardware for one fragment of a multi-descriptor frame: DD is
    /// set but EOP is not, except on the last one.
    fn hw_deliver_fragment(hw: &E1000eHw, slot: usize, data: &[u8], eop: bool) {
        assert!(data.len() <= BUF_SIZE);
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                hw.rx_buf_vaddr(slot) as *mut u8,
                data.len(),
            );
            let d = &mut *hw.rx_ring.as_ptr::<RxDesc>().add(slot);
            d.addr = hw.rx_buf_paddr(slot);
            d.len = data.len() as u16;
            d.status = RXD_STAT_DD | if eop { RXD_STAT_EOP } else { 0 };
            d.errors = 0;
        }
        reg_write(hw.base, E1000E_RDH, ((slot + 1) % NUM_RX) as u32);
    }

    #[test]
    fn an_aborted_fragment_chain_does_not_surface_as_a_frame() {
        // A chain the driver gives up on (here: one fragment flagged with a
        // wire error) must be swallowed up to and including its EOP
        // descriptor. Delivering the tail as if it were a whole Ethernet
        // frame hands smoltcp — and any AF_PACKET tap — a packet that never
        // existed on the wire, assembled from the middle of another one.
        let mut hw = make_hw();
        let head = pkt(0x11, BUF_SIZE);
        let middle = pkt(0x22, BUF_SIZE);
        let tail = pkt(0x33, 512);

        hw_deliver_fragment(&hw, 0, &head, false);
        // Wire error (not a checksum verdict) on the middle fragment.
        hw_deliver_fragment(&hw, 1, &middle, false);
        unsafe {
            (*hw.rx_ring.as_ptr::<RxDesc>().add(1)).errors = 0x01;
        }
        hw_deliver_fragment(&hw, 2, &tail, true);

        assert!(
            hw.receive().is_none(),
            "the tail of an aborted chain must not be delivered as a frame"
        );
        assert_eq!(hw.stats.rx_packets, 0);
        assert!(!hw.rx_discard_until_eop, "EOP must end the discard");

        // The ring keeps working: the next whole frame is delivered normally.
        let good = pkt(0x44, 300);
        hw_deliver(&hw, 3, &good);
        assert_eq!(hw.receive().expect("ring wedged after aborted chain"), good);
        hw.flush_rx_doorbell();
        assert_eq!(reg_read(hw.base, E1000E_RDT) as usize, 3);
    }

    #[test]
    fn a_complete_fragment_chain_is_reassembled() {
        let mut hw = make_hw();
        let a = pkt(0x55, BUF_SIZE);
        let b = pkt(0x66, 700);
        hw_deliver_fragment(&hw, 0, &a, false);
        hw_deliver_fragment(&hw, 1, &b, true);

        let mut expected = a.clone();
        expected.extend_from_slice(&b);
        assert_eq!(hw.receive().expect("chain not reassembled"), expected);
        assert_eq!(hw.stats.rx_packets, 1);
        assert_eq!(hw.stats.rx_dropped, 0);
    }

    #[test]
    fn rx_wraps_around_ring() {
        let mut hw = make_hw();
        // Push well over NUM_RX packets, draining as we go, to wrap rx_next_to_clean.
        let total = NUM_RX * 3 + 17;
        let mut produced = 0usize;
        let mut consumed = 0usize;
        while consumed < total {
            // keep the ring partly filled
            while produced < total && (produced - consumed) < NUM_RX - 1 {
                hw_deliver(&hw, produced % NUM_RX, &pkt(produced as u8, 100));
                produced += 1;
            }
            while let Some(got) = hw.receive() {
                assert_eq!(
                    got,
                    pkt(consumed as u8, 100),
                    "wrap frame {consumed} mismatch"
                );
                consumed += 1;
            }
        }
        assert_eq!(consumed, total);
        assert_eq!(hw.stats.rx_packets, total as u64);
    }
}

#[cfg(test)]
mod tx_ring_tests {
    //! Host bench for the e1000e TX descriptor-ring DD-bit ownership state
    //! machine (`can_send`/`send`), covering the fix for using a live TDH
    //! MMIO read as the completion signal (which Intel documents as
    //! unreliable — TDH reflects prefetch, not write-back) instead of the
    //! descriptor's own DD status bit that hardware sets on completion
    //! because every posted descriptor carries CMD.RS.

    use super::rx_ring_tests::make_hw;
    use alloc::vec::Vec;

    fn reg_read(base: usize, reg: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }
    }

    fn pkt(seed: u8, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add(i as u8).wrapping_mul(31))
            .collect()
    }

    /// Play the hardware completing a TX descriptor: write DD back into its
    /// status byte, exactly like a real NIC's write-back after CMD.RS.
    fn hw_complete_tx(hw: &super::E1000eHw, slot: usize) {
        unsafe {
            let d = &mut *hw.tx_ring.as_ptr::<super::TxDesc>().add(slot);
            d.status = super::TX_STAT_DD;
        }
    }

    #[test]
    fn tx_starts_free_and_posts_from_slot_zero() {
        let mut hw = make_hw();
        assert!(hw.can_send(), "every descriptor starts DD (free)");
        hw.send(&pkt(1, 64)).expect("first send must succeed");
        assert_eq!(reg_read(hw.base, super::E1000E_TDT), 1, "TDT advanced");
    }

    #[test]
    fn tx_slot_stays_owned_by_hw_until_dd_write_back() {
        let mut hw = make_hw();
        // Post every slot but the guard, so tx_tail sits on the last free
        // descriptor and the slot after it (0) is the oldest in flight.
        for i in 0..super::NUM_TX - 1 {
            hw.send(&pkt(i as u8, 64))
                .unwrap_or_else(|_| panic!("slot {} should be free on the first lap", i));
        }
        // Hardware has not completed its DMA yet: slot 0's DD bit is still
        // clear (send() clears it before posting), so the ring is full and a
        // post must be refused — a stale TDH read would have let this
        // through even though the NIC might still be reading the buffer.
        assert!(
            !hw.can_send(),
            "ring must stay full until slot 0's DD is written back"
        );
        assert!(hw.send(&pkt(0xFF, 64)).is_err());

        // Hardware finishes the DMA and writes DD back.
        hw_complete_tx(&hw, 0);
        assert!(hw.can_send(), "DD write-back must free a slot");
        hw.send(&pkt(0xEE, 64)).expect("post possible after DD");
        // tx_tail wrapped onto slot 0 (free), but slot 1 is still in flight:
        // it is now the guard, so the ring is full again.
        assert!(
            !hw.can_send(),
            "slot 1 still owned by hardware — guard holds"
        );
    }

    #[test]
    fn tx_keeps_one_guard_slot_so_tdt_never_meets_tdh() {
        // The NIC works descriptors from TDH up to (excluding) TDT and reads
        // TDH == TDT as "ring empty". Per-slot DD ownership alone would let
        // software post all NUM_TX slots and then write TDT == TDH: the NIC
        // would see nothing to send, those DD bits would never come back and
        // TX would be dead for good. At most NUM_TX - 1 may be outstanding.
        let mut hw = make_hw();
        for i in 0..super::NUM_TX - 1 {
            hw.send(&pkt(i as u8, 32))
                .unwrap_or_else(|_| panic!("slot {} should be free", i));
        }
        assert!(!hw.can_send(), "only the guard slot is left");
        assert!(hw.send(&pkt(0xAA, 32)).is_err());
        assert_eq!(
            reg_read(hw.base, super::E1000E_TDT) as usize,
            super::NUM_TX - 1,
            "TDT stops one short of wrapping onto TDH"
        );
        assert_ne!(
            reg_read(hw.base, super::E1000E_TDT),
            reg_read(hw.base, super::E1000E_TDH),
            "a full ring must never look empty to the NIC"
        );

        // Complete slot 0 (the oldest) and confirm the ring recovers: the
        // guard moves to slot 1.
        hw_complete_tx(&hw, 0);
        assert!(hw.can_send(), "completing the oldest slot frees one up");
        hw.send(&pkt(0xBB, 32))
            .expect("ring recovers after completion");
        assert!(!hw.can_send());
        hw_complete_tx(&hw, 1);
        assert!(hw.can_send());
    }

    #[test]
    fn tx_wraps_around_ring_interleaved_with_completions() {
        let mut hw = make_hw();
        let total = super::NUM_TX * 3 + 11;
        for i in 0..total {
            let slot = i % super::NUM_TX;
            // Simulate the NIC keeping up: before posting into `slot`, the
            // NIC has completed the slot after it (the oldest still in
            // flight), so the guard requirement is met.
            if i + 1 >= super::NUM_TX {
                hw_complete_tx(&hw, (i + 1) % super::NUM_TX);
            }
            hw.send(&pkt(i as u8, 40))
                .unwrap_or_else(|_| panic!("send {} (slot {}) should succeed", i, slot));
        }
        assert_eq!(hw.stats.tx_packets, total as u64);
    }

    #[test]
    fn tx_doorbell_batches_until_flush() {
        let mut hw = make_hw();
        hw.post_tx_frame(&pkt(1, 64)).expect("post");
        assert_eq!(
            reg_read(hw.base, super::E1000E_TDT),
            0,
            "TDT must stay put until flush"
        );
        assert!(hw.tx_doorbell_dirty);
        assert_eq!(hw.tx_desc_dirty_count, 1);
        hw.post_tx_frame(&pkt(2, 64)).expect("second post");
        assert_eq!(reg_read(hw.base, super::E1000E_TDT), 0);
        assert_eq!(hw.tx_desc_dirty_count, 2, "contiguous posts coalesce");
        hw.flush_tx_doorbell();
        assert_eq!(reg_read(hw.base, super::E1000E_TDT), 2);
        assert!(!hw.tx_doorbell_dirty);
        assert!(hw.tx_desc_dirty_start.is_none());
    }

    #[test]
    fn tx_uc_dd_recheck_skips_sync_path() {
        // Coherent (UC) rings must still see DD via volatile load without
        // requiring a FromDevice sync — the spin path relies on this.
        let mut hw = make_hw();
        hw.tx_ring_coherent = true;
        assert!(hw.tx_can_post(false), "init DD visible without sync");
        hw.send(&pkt(1, 32)).unwrap();
        // Tail advanced to slot 1, which still has the init-time DD bit.
        assert!(hw.can_send());
        assert!(hw.tx_can_post(false), "UC free slot visible without sync");
    }
}

#[cfg(test)]
mod coherency_bench {
    //! Host reproduction of the real-hardware-only RX wedge on a large download.
    //!
    //! A coherent host (or QEMU) cannot expose a DMA cache bug, so we model it:
    //! `ram` is what the NIC sees (it DMAs straight to RAM), and the driver
    //! accesses the descriptor ring through a simulated write-back cache. The
    //! NIC and the driver run interleaved — crucially the NIC fills the *next*
    //! descriptor while the driver is mid-processing the current one (real
    //! concurrent DMA), which QEMU's synchronous model never does.
    //!
    //! With write-back descriptor rings this reproduces the deterministic stall:
    //! a 16-byte descriptor shares a 64-byte cache line with three neighbours,
    //! so recycling one and flushing its line writes the stale neighbours back
    //! over DD bits the NIC just set -> lost completion -> RX wedges partway
    //! through a >100 MB stream. With uncached rings (the fix) the whole stream
    //! is delivered. The bench fails until the driver's ring is coherent.

    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    const N: usize = 256; // ring slots
    const PL: usize = 4; // 16-byte descriptors per 64-byte cache line

    #[derive(Clone, Copy, Default, PartialEq)]
    struct D {
        dd: bool,
        seq: u32,
    }

    struct Model {
        ram: Vec<D>,                             // NIC's view (DMA target)
        cache: BTreeMap<usize, ([D; PL], bool)>, // driver WB cache: line -> (data, dirty)
        uc: bool,                                // true => uncached ring (no cache)
        rdh: usize,                              // NIC head
    }
    impl Model {
        fn line(i: usize) -> usize {
            i / PL
        }
        fn load_line(&mut self, l: usize) {
            if !self.cache.contains_key(&l) {
                let mut a = [D::default(); PL];
                for k in 0..PL {
                    a[k] = self.ram[l * PL + k];
                }
                self.cache.insert(l, (a, false));
            }
        }
        // NIC DMAs a completed descriptor straight into RAM, advancing head.
        fn nic_fill(&mut self, seq: u32) {
            let s = self.rdh;
            self.ram[s] = D { dd: true, seq };
            self.rdh = (self.rdh + 1) % N;
        }
        fn clflush_inval(&mut self, i: usize) {
            if !self.uc {
                self.cache.remove(&Self::line(i));
            }
        }
        fn read(&mut self, i: usize) -> D {
            if self.uc {
                return self.ram[i];
            }
            let l = Self::line(i);
            self.load_line(l);
            self.cache.get(&l).unwrap().0[i % PL]
        }
        fn write(&mut self, i: usize, d: D) {
            if self.uc {
                self.ram[i] = d;
                return;
            }
            let l = Self::line(i);
            self.load_line(l);
            let e = self.cache.get_mut(&l).unwrap();
            e.0[i % PL] = d;
            e.1 = true;
        }
        // ToDevice flush: write the WHOLE 64-byte line back (this is the bug:
        // stale neighbours clobber DD bits the NIC set after the line loaded).
        fn clflush_wb(&mut self, i: usize) {
            if self.uc {
                return;
            }
            let l = Self::line(i);
            if let Some((a, dirty)) = self.cache.remove(&l) {
                if dirty {
                    for k in 0..PL {
                        self.ram[l * PL + k] = a[k];
                    }
                }
            }
        }
    }

    fn run(uc: bool, total: u32) -> Result<(), String> {
        let mut m = Model {
            ram: vec![D::default(); N],
            cache: BTreeMap::new(),
            uc,
            rdh: 0,
        };
        let mut next = 0usize; // driver next_to_clean
        let mut rdt = N - 1; // last slot handed to NIC
        let mut produced = 0u32;
        let mut received = 0u32;

        // Keep the NIC ~1 descriptor ahead of the driver so it fills same-line
        // neighbours while the driver works — the concurrent-DMA condition.
        let nic_can_fill = |rdh: usize, rdt: usize| (rdh + 1) % N != rdt;

        let mut guard = 0u64;
        while received < total {
            guard += 1;
            if guard > total as u64 * 100 + 1_000_000 {
                return Err(format!(
                    "livelock: received {} of {} (next={}, rdh={})",
                    received, total, next, m.rdh
                ));
            }

            // NIC produces until it is one ahead of the driver (or ring full).
            while produced < total && nic_can_fill(m.rdh, rdt) && (m.rdh + N - next) % N < 2 {
                m.nic_fill(produced);
                produced += 1;
            }

            if next == m.rdh {
                if produced >= total {
                    break;
                }
                continue;
            }

            // Driver processes slot `next`, mirroring the real sequence:
            m.clflush_inval(next); // dma_sync FromDevice
            let d = m.read(next);

            // *** concurrent DMA: NIC fills the next descriptor mid-process ***
            if produced < total && nic_can_fill(m.rdh, rdt) {
                m.nic_fill(produced);
                produced += 1;
            }

            if !d.dd {
                return Err(format!(
                    "RX WEDGED at slot {} after {} of {} packets (DD lost to false sharing)",
                    next, received, total
                ));
            }
            if d.seq != received {
                return Err(format!(
                    "corruption/reorder at slot {}: got seq {}, expected {}",
                    next, d.seq, received
                ));
            }
            received += 1;

            // recycle: clear descriptor and flush its line ToDevice
            m.write(next, D { dd: false, seq: 0 });
            m.clflush_wb(next);
            rdt = next;
            next = (next + 1) % N;
        }

        if received != total {
            return Err(format!("incomplete: {} of {}", received, total));
        }
        Ok(())
    }

    /// >100 MB at 1460 B/packet ~= 72k packets; use 100k. The fix (uncached
    /// descriptor rings) must deliver every packet with no wedge.
    #[test]
    fn uncached_rings_deliver_full_large_download() {
        run(true, 100_000).expect("uncached descriptor rings must not wedge");
    }

    /// Proof the bench actually reproduces the bug: write-back descriptor rings
    /// wedge/corrupt partway through, so this asserts an error is produced.
    #[test]
    fn writeback_rings_reproduce_the_wedge() {
        let r = run(false, 100_000);
        assert!(r.is_err(), "WB rings should reproduce the wedge but passed");
    }

    /// Batch recycle on *write-back* rings: defer recycling until a whole cache
    /// line's PL descriptors are all consumed, then write+flush the line once.
    /// By then the NIC has moved past that line (it fills sequentially and the
    /// line was not returned via RDT yet, so it cannot wrap back), so the
    /// write-back can never clobber a DD the NIC is still setting. This makes
    /// write-back rings safe WITHOUT relying on an uncached remap that may not
    /// take effect on real hardware.
    fn run_batch(total: u32) -> Result<(), String> {
        let mut m = Model {
            ram: vec![D::default(); N],
            cache: BTreeMap::new(),
            uc: false,
            rdh: 0,
        };
        let mut next = 0usize;
        let mut rdt = N - 1;
        let mut produced = 0u32;
        let mut received = 0u32;
        let nic_can_fill = |rdh: usize, rdt: usize| (rdh + 1) % N != rdt;
        let mut guard = 0u64;
        while received < total {
            guard += 1;
            if guard > total as u64 * 100 + 1_000_000 {
                return Err(format!(
                    "livelock: received {} of {} (next={}, rdh={})",
                    received, total, next, m.rdh
                ));
            }
            while produced < total && nic_can_fill(m.rdh, rdt) && (m.rdh + N - next) % N < 2 {
                m.nic_fill(produced);
                produced += 1;
            }
            if next == m.rdh {
                if produced >= total {
                    break;
                }
                continue;
            }
            m.clflush_inval(next);
            let d = m.read(next);
            if produced < total && nic_can_fill(m.rdh, rdt) {
                m.nic_fill(produced);
                produced += 1;
            }
            if !d.dd {
                return Err(format!(
                    "RX WEDGED (batch) at slot {} after {} of {} packets",
                    next, received, total
                ));
            }
            if d.seq != received {
                return Err(format!(
                    "corruption (batch) at slot {}: got seq {}, expected {}",
                    next, d.seq, received
                ));
            }
            received += 1;
            // Recycle the whole cache line only once its last descriptor is done.
            if next % PL == PL - 1 {
                let l0 = next - (PL - 1);
                for s in l0..=next {
                    m.write(s, D { dd: false, seq: 0 });
                }
                m.clflush_wb(next);
                rdt = next;
            }
            next = (next + 1) % N;
        }
        if received != total {
            return Err(format!("incomplete: {} of {}", received, total));
        }
        Ok(())
    }

    /// The write-back-safe fix: cache-line batch recycle delivers the whole
    /// >100 MB stream with no wedge, even though the rings are write-back and
    /// the NIC DMAs concurrently. This does not depend on an uncached remap.
    #[test]
    fn batch_recycle_survives_large_download_on_writeback_rings() {
        run_batch(100_000).expect("batch recycle must not wedge on write-back rings");
    }
}

#[cfg(test)]
mod itr_tune_tests {
    use super::*;

    #[test]
    fn burst_upgrades_to_throughput_immediately() {
        assert_eq!(
            choose_itr(E1000E_ITR_LOW_LATENCY, 0, E1000E_ITR_BURST_THROUGHPUT),
            E1000E_ITR_THROUGHPUT
        );
    }

    #[test]
    fn window_throughput_and_hysteresis() {
        assert_eq!(
            choose_itr(E1000E_ITR_BALANCED, E1000E_ITR_WINDOW_THROUGHPUT, 0),
            E1000E_ITR_THROUGHPUT
        );
        assert_eq!(
            choose_itr(E1000E_ITR_THROUGHPUT, E1000E_ITR_WINDOW_HOLD, 0),
            E1000E_ITR_THROUGHPUT
        );
        assert_eq!(
            choose_itr(E1000E_ITR_THROUGHPUT, E1000E_ITR_WINDOW_HOLD - 1, 0),
            E1000E_ITR_BALANCED
        );
    }

    #[test]
    fn quiet_window_prefers_low_latency_for_acks() {
        assert_eq!(
            choose_itr(E1000E_ITR_THROUGHPUT, E1000E_ITR_WINDOW_LOW, 0),
            E1000E_ITR_LOW_LATENCY
        );
        assert_eq!(
            choose_itr(E1000E_ITR_BALANCED, 0, 0),
            E1000E_ITR_LOW_LATENCY
        );
    }

    #[test]
    fn moderate_load_stays_balanced() {
        assert_eq!(
            choose_itr(E1000E_ITR_LOW_LATENCY, E1000E_ITR_WINDOW_LOW + 1, 0),
            E1000E_ITR_BALANCED
        );
    }
}

#[cfg(test)]
mod rx_mode_and_id_tests {
    use super::rx_ring_tests::make_hw;
    use super::*;

    fn reg_read(base: usize, reg: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }
    }

    #[test]
    fn pci_ids_match_82574_la_and_i219_reject_ich9_and_igb() {
        assert!(e1000e_device_matched(0x10d3)); // 82574L
        assert!(e1000e_device_matched(0x10f6)); // 82574LA
        assert!(e1000e_device_matched(0x15b8)); // I219-V
        assert!(e1000e_is_pch(0x15b8));
        assert!(e1000e_is_pch_spt_or_later(0x15b8));
        assert!(!e1000e_is_pch(0x10d3));
        assert!(!e1000e_device_matched(0x10f5)); // ICH9 — out of scope
        assert!(!e1000e_device_matched(0x1533)); // I210 igb
        assert!(!e1000e_device_matched(0x1539)); // I211 igb
    }

    #[test]
    fn mta_hash_matches_linux_case0_example() {
        // Linux mac.c comment: 01:AA:00:12:34:56 → hash 0x563 with 128 MTA regs.
        let addr = [0x01, 0xAA, 0x00, 0x12, 0x34, 0x56];
        assert_eq!(e1000e_hash_mc_addr(&addr), 0x563);
    }

    #[test]
    fn set_rx_mode_clears_promisc_and_programs_mta() {
        let mut hw = make_hw();
        // Seed RCTL like init_rx would after EN.
        unsafe {
            mmio_write(
                hw.base,
                E1000E_RCTL,
                RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SECRC,
            );
            hw.set_rx_mode(false, false, &[[0x01, 0xAA, 0x00, 0x12, 0x34, 0x56]]);
        }
        let rctl = reg_read(hw.base, E1000E_RCTL);
        assert_eq!(rctl & RCTL_UPE, 0, "UPE cleared when not promisc");
        assert_eq!(rctl & RCTL_MPE, 0, "MPE cleared when not allmulti");
        assert_ne!(rctl & RCTL_EN, 0);
        let hash = e1000e_hash_mc_addr(&[0x01, 0xAA, 0x00, 0x12, 0x34, 0x56]);
        let reg = ((hash >> 5) as usize) & (E1000E_MTA_REG_COUNT - 1);
        let bit = hash & 0x1F;
        let mta = reg_read(hw.base, E1000E_MTA_BASE + reg);
        assert_ne!(mta & (1u32 << bit), 0, "MTA bit set for hashed address");
    }

    #[test]
    fn hw_down_blocks_tx_until_flags_restored() {
        let mut hw = make_hw();
        assert!(hw.can_send());
        unsafe {
            // Seed RCTL/TCTL bits so hw_down has something to clear.
            mmio_write(hw.base, E1000E_RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
            mmio_write(hw.base, E1000E_TCTL, TCTL_EN | TCTL_PSP);
            hw.hw_down();
        }
        assert!(!hw.can_send());
        assert_eq!(reg_read(hw.base, E1000E_RCTL) & RCTL_EN, 0);
        assert_eq!(reg_read(hw.base, E1000E_TCTL) & TCTL_EN, 0);
        // Rings still allocated — flip running for unit-test recovery without
        // a full CTRL_RST against mock MMIO.
        hw.hw_running = true;
        assert!(hw.can_send());
    }
}

#[cfg(test)]
mod link_speed_tests {
    //! Auto-negotiation: what we ask the PHY for, what we read back, and what
    //! the MAC is tuned to afterwards.
    //!
    //! None of this needs a NIC — every decision below is a pure function of
    //! register contents, which is the whole reason it is split out that way.
    //! The path these cover (a PCH part with a live Management Engine) is
    //! never executed in CI: QEMU's e1000e has no ME, no ULP and no HV pages.

    use super::rx_ring_tests::make_hw;
    use super::*;

    /// I219-V, i.e. what a PCH-SPT-or-later part looks like to `is_pch()`.
    const I219: u16 = 0x15b8;

    fn reg_read(base: usize, reg: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + reg * 4) as *const u32) }
    }
    fn reg_write(base: usize, reg: usize, v: u32) {
        unsafe { core::ptr::write_volatile((base + reg * 4) as *mut u32, v) }
    }

    // ---------------------------------------------------------------- pages

    #[test]
    fn hv_page_768_is_selected_as_page_zero() {
        // Linux: `if (page == HV_INTC_FC_PAGE_START) page = 0;`. Page 768 is
        // the PHY's page 0 seen at MDIO address 1. Selecting it by number
        // writes 768 << 5 = 0x6000, which is a different page entirely — and
        // HV_OEM_BITS (768, 25) is where LPLU lives, so getting this wrong
        // means reading and writing someone else's register.
        assert_eq!(hv_page_select(768), 0);
        assert_ne!(hv_page_select(768), (768u32 << PHY_PAGE_SHIFT) as u16);
    }

    #[test]
    fn other_hv_pages_are_selected_by_number_shifted_left_five() {
        assert_eq!(hv_page_select(769), 0x6020); // CV_SMB_CTRL
        assert_eq!(hv_page_select(770), 0x6040); // HV_PM_CTRL
        assert_eq!(hv_page_select(779), 0x6160); // I218_ULP_CONFIG1
    }

    // ------------------------------------------------------------- OEM bits

    #[test]
    fn oem_bits_clear_lplu_and_gbe_disable_when_the_mac_copy_is_clear() {
        // This is the fix: the ME leaves LPLU set in the PHY, the driver
        // clears the MAC-side PHY_CTRL and thinks it is done, and the PHY goes
        // on negotiating the lowest speed the link supports — 10 Mb/s.
        let oem = HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS;
        let got = oem_bits_for_d0(0, oem, true, false);
        assert_eq!(got & HV_OEM_BITS_LPLU, 0);
        assert_eq!(got & HV_OEM_BITS_GBE_DIS, 0);
        assert_ne!(got & HV_OEM_BITS_RESTART_AN, 0);
    }

    #[test]
    fn oem_bits_mirror_the_d0_copies_of_the_mac_register() {
        let phy_ctrl = PHY_CTRL_D0A_LPLU | PHY_CTRL_GBE_DISABLE;
        let got = oem_bits_for_d0(phy_ctrl, 0, true, false);
        assert_ne!(got & HV_OEM_BITS_LPLU, 0);
        assert_ne!(got & HV_OEM_BITS_GBE_DIS, 0);
    }

    #[test]
    fn oem_bits_in_d0_ignore_the_non_d0_copies() {
        // In D0a only the D0a bits apply; the suspend path (d0_state = false)
        // takes both. Folding the two together would disable gigabit on a
        // machine whose firmware only asked for it while suspended.
        let phy_ctrl = PHY_CTRL_NOND0A_LPLU | PHY_CTRL_NOND0A_GBE_DISABLE;
        let d0 = oem_bits_for_d0(phy_ctrl, 0, true, false);
        assert_eq!(d0 & (HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS), 0);
        let dx = oem_bits_for_d0(phy_ctrl, 0, false, false);
        assert_eq!(
            dx & (HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS),
            HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS
        );
    }

    #[test]
    fn oem_bits_do_not_restart_autoneg_when_the_me_blocks_phy_resets() {
        let got = oem_bits_for_d0(0, HV_OEM_BITS_LPLU, true, true);
        assert_eq!(got & HV_OEM_BITS_RESTART_AN, 0);
    }

    #[test]
    fn oem_bits_leave_every_other_bit_of_the_register_alone() {
        // The register also carries LED and other OEM configuration we have no
        // business rewriting.
        let oem = 0x1289 | HV_OEM_BITS_LPLU;
        let got = oem_bits_for_d0(0, oem, true, false);
        assert_eq!(got & 0x1289, 0x1289);
    }

    // --------------------------------------------------- negotiation result

    #[test]
    fn phy_result_needs_both_link_and_autoneg_complete() {
        let up_and_done = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
        assert!(phy_negotiated_link(up_and_done, ADVERTISE_100FULL, LPA_100FULL, 0, 0).is_some());
        assert!(phy_negotiated_link(BMSR_LSTATUS, ADVERTISE_100FULL, LPA_100FULL, 0, 0).is_none());
        assert!(
            phy_negotiated_link(BMSR_ANEGCOMPLETE, ADVERTISE_100FULL, LPA_100FULL, 0, 0).is_none()
        );
    }

    #[test]
    fn phy_result_prefers_gigabit_full_duplex() {
        let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
        let adv = ADVERTISE_ALL_10_100;
        let lpa = LPA_10HALF | LPA_10FULL | LPA_100HALF | LPA_100FULL;
        let got = phy_negotiated_link(bmsr, adv, lpa, ADVERTISE_1000FULL, LPA_1000FULL);
        assert_eq!(got, Some((1000, true)));
    }

    #[test]
    fn phy_result_ignores_gigabit_the_partner_never_offered() {
        // We advertise 1000; the switch port is 100-only. Resolution has to
        // fall through to 100 full, not claim the gigabit we asked for.
        let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
        let got = phy_negotiated_link(
            bmsr,
            ADVERTISE_ALL_10_100,
            LPA_100FULL | LPA_100HALF,
            ADVERTISE_1000FULL,
            0,
        );
        assert_eq!(got, Some((100, true)));
    }

    #[test]
    fn phy_result_walks_down_the_ieee_priority_list() {
        let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
        let cases: [(u16, u16, (u32, bool)); 4] = [
            (ADVERTISE_100FULL, LPA_100FULL, (100, true)),
            (ADVERTISE_100HALF, LPA_100HALF, (100, false)),
            (ADVERTISE_10FULL, LPA_10FULL, (10, true)),
            (ADVERTISE_10HALF, LPA_10HALF, (10, false)),
        ];
        for (adv, lpa, want) in cases {
            assert_eq!(phy_negotiated_link(bmsr, adv, lpa, 0, 0), Some(want));
        }
        // 1000 half only ever resolves when both sides offered it.
        assert_eq!(
            phy_negotiated_link(bmsr, 0, 0, ADVERTISE_1000HALF, LPA_1000HALF),
            Some((1000, false))
        );
    }

    #[test]
    fn phy_result_is_none_when_the_two_sides_share_nothing() {
        let bmsr = BMSR_LSTATUS | BMSR_ANEGCOMPLETE;
        assert_eq!(
            phy_negotiated_link(bmsr, ADVERTISE_100FULL, LPA_10HALF, 0, 0),
            None
        );
    }

    // ------------------------------------------------------- MAC STATUS bits

    #[test]
    fn status_decodes_all_four_speed_encodings() {
        assert_eq!(status_speed_mbps(0), 10);
        assert_eq!(status_speed_mbps(0b01 << STATUS_SPEED_SHIFT), 100);
        assert_eq!(status_speed_mbps(0b10 << STATUS_SPEED_SHIFT), 1000);
        assert_eq!(status_speed_mbps(0b11 << STATUS_SPEED_SHIFT), 1000);
        // Bits outside [7:6] must not leak into the decode.
        assert_eq!(status_speed_mbps(0xFFFF_FF3F), 10);
    }

    // ------------------------------------------------------------ TIPG / IPG

    #[test]
    fn tipg_widens_the_gap_only_at_10_megabit_half_duplex() {
        let base = 8 | (8 << 10) | (6 << 20);
        assert_eq!(tipg_for_link(base, true, 10, false) & TIPG_IPGT_MASK, 0xFF);
        assert_eq!(tipg_for_link(base, true, 10, true) & TIPG_IPGT_MASK, 0x0C);
        assert_eq!(tipg_for_link(base, false, 10, true) & TIPG_IPGT_MASK, 0x08);
    }

    #[test]
    fn tipg_uses_the_intermediate_gap_below_gigabit_on_spt_and_later() {
        let base = 8 | (8 << 10) | (6 << 20);
        assert_eq!(tipg_for_link(base, true, 100, true) & TIPG_IPGT_MASK, 0x0C);
        assert_eq!(tipg_for_link(base, true, 1000, true) & TIPG_IPGT_MASK, 0x08);
        assert_eq!(tipg_for_link(base, false, 100, true) & TIPG_IPGT_MASK, 0x08);
    }

    #[test]
    fn tipg_keeps_the_ipgr1_and_ipgr2_fields() {
        let base = 8 | (8 << 10) | (6 << 20);
        let got = tipg_for_link(base, true, 10, false);
        assert_eq!(got & !TIPG_IPGT_MASK, base & !TIPG_IPGT_MASK);
    }

    #[test]
    fn link_tuning_writes_tipg_and_the_i217_beacon_duration() {
        let mut hw = make_hw();
        hw.device_id = I219;
        reg_write(hw.base, E1000E_TIPG, 8 | (8 << 10) | (6 << 20));
        reg_write(hw.base, E1000E_FEXTNVM4, 0xDEAD_BEE0);

        // 10 Mb/s half duplex: STATUS speed bits 00, FD clear.
        unsafe { hw.apply_link_speed_tuning(0) };
        assert_eq!(reg_read(hw.base, E1000E_TIPG) & TIPG_IPGT_MASK, 0xFF);
        assert_eq!(
            reg_read(hw.base, E1000E_FEXTNVM4) & FEXTNVM4_BEACON_DURATION_MASK,
            FEXTNVM4_BEACON_DURATION_8USEC
        );
        // The rest of FEXTNVM4 is left as it was.
        assert_eq!(
            reg_read(hw.base, E1000E_FEXTNVM4) & !0x7,
            0xDEAD_BEE0 & !0x7
        );

        // Gigabit full duplex rolls the gap back to the default.
        unsafe { hw.apply_link_speed_tuning((0b10 << STATUS_SPEED_SHIFT) | STATUS_FD) };
        assert_eq!(reg_read(hw.base, E1000E_TIPG) & TIPG_IPGT_MASK, 0x08);
    }

    #[test]
    fn link_tuning_leaves_discrete_parts_alone() {
        // The TIPG erratum and the beacon duration are PCH-only; an 82574 in
        // QEMU must come out of this untouched.
        let hw = make_hw();
        assert!(!hw.is_pch());
        reg_write(hw.base, E1000E_TIPG, 0x1234_5678);
        unsafe { hw.apply_link_speed_tuning(0) };
        assert_eq!(reg_read(hw.base, E1000E_TIPG), 0x1234_5678);
    }
}
