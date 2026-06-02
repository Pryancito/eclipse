//! Intel e1000e NIC driver (82574L / 82579 / I217 / I218 / I219 family)
//!
//! Register semantics and hot paths are aligned with the in-tree Linux driver
//! in this repo (`e1000e/*.c`, especially `netdev.c` ring setup, `hw.h` RX
//! extended descriptors, and `defines.h` interrupt masks). A full line-by-line
//! port of all MAC/PHY/NVM paths is not the goal; behaviour-critical pieces are
//! matched so bare-metal hardware matches QEMU/Linux expectations.
//!
//! Set [`E1000E_CONVENTIONAL`] for a minimal profile: no checksum offload, no
//! IAME, no optional PCH tuning, short link-up wait. RX always uses extended
//! descriptors (`RFCTL_EXTEN`) — I219/PCH ignore legacy layout on real silicon.
//!
//! Link speed/duplex follow Intel e1000e 3.8.7 (ich8lan.c / phy.c), as packaged in
//! koljah-de/e1000e-dkms-debian: autoneg + ASDE on PCH, STATUS as primary speed,
//! reg26/reg17 when ahead of STATUS, TIPG/EMI/PLL/TARC tuned after link-up.

/// Minimal NIC profile for bare-metal bring-up (fewer moving parts).
const E1000E_CONVENTIONAL: bool = false;
/// PHY/register bring-up traces (link stages, GPRC polls, BM sync). Keep false for quiet dmesg.
const E1000E_LOG_VERBOSE: bool = false;
/// Bump when changing init/RX paths — grep dmesg for this tag to verify the ISO.
const E1000E_DRIVER_TAG: &str = "e1000e-rev-20250601-metal-irq-lfence";

/// Optional `klog_info!` — compiled out when [`E1000E_LOG_VERBOSE`] is false.
macro_rules! e1000e_vlog {
    ($($t:tt)*) => {
        if E1000E_LOG_VERBOSE {
            crate::klog_info!($($t)*);
        }
    };
}

const TARC0_CB_MULTIQ_3_REQ: u32 = 0x3000_0000;
const TARC0_CB_MULTIQ_2_REQ: u32 = 0x2000_0000;
/// Linux `SPEED_MODE_BIT` — must be clear at 10/100M or I219 TX may not complete (DD never set).
const TARC0_SPEED_MODE: u32 = 1 << 21;

/// PHY wait budget per background deferred job (~1 s); job reschedules until link up.
const LINK_BRINGUP_SLICES_PER_JOB: u32 = 10;

/// Max PHY polling per `poll()` — avoids holding `hw` lock for seconds (deadlocks
/// NIC IRQ on single-CPU and starves USB/HID when `poll_ifaces` runs from epoll).
const LINK_BRINGUP_SLICE_MS: u32 = 100;
#[inline]
const fn e1000e_profile() -> &'static str {
    if E1000E_CONVENTIONAL {
        "conventional"
    } else {
        "extended"
    }
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
use pci::{PCIDevice, BAR, Location};
use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
use lock::Mutex;


use super::{timer_now_as_micros, intr_on, intr_off, intr_get};

// ---------------------------------------------------------------------------
// Register offsets (byte addresses / 4 → u32 index)
// ---------------------------------------------------------------------------
const E1000E_CTRL: usize = 0x0000 / 4;
const E1000E_STATUS: usize = 0x0008 / 4;
const E1000E_MDIC: usize = 0x0020 / 4;
const E1000E_EXTCNF_CTRL: usize = 0x0F00 / 4;
const E1000E_PHY_CTRL: usize = 0x00F10 / 4;
const PHY_CTRL_D0A_LPLU: u32 = 0x0000_0002;
const PHY_CTRL_NOND0A_LPLU: u32 = 0x0000_0004;
const PHY_CTRL_NOND0A_GBE_DISABLE: u32 = 0x0000_0008;
const PHY_CTRL_GBE_DISABLE: u32 = 0x0000_0040;
/// Linux `HV_OEM_BITS` — page 768 reg 25 on I217/I219.
const HV_OEM_BITS_PHY: u32 = (768 << 5) | 25;
const HV_OEM_BITS_LPLU: u16 = 0x0004;
const HV_OEM_BITS_GBE_DIS: u16 = 0x0040;
const HV_OEM_BITS_RESTART_AN: u16 = 0x0400;
const E1000E_EECD: usize = 0x0010 / 4;
const E1000E_VFTA_BASE: usize = 0x5600 / 4; // VLAN Filter Table Array (128 × u32)
const E1000E_EERD: usize = 0x0014 / 4;
const E1000E_ICR: usize = 0x00C0 / 4;
const E1000E_ITR: usize = 0x00C4 / 4; // Interrupt Throttling Rate
const E1000E_IMS: usize = 0x00D0 / 4;
const E1000E_IMC: usize = 0x00D8 / 4;
const E1000E_IAM: usize = 0x00E0 / 4; // Interrupt Acknowledge Auto Mask (Linux e1000_configure_rx)
const E1000E_RCTL: usize = 0x0100 / 4;
const E1000E_TCTL: usize = 0x0400 / 4;
const E1000E_TIPG: usize = 0x0410 / 4;
// Receive descriptor ring queue 0.  0x2800 is correct for all e1000e silicon;
// queues ≥4 use 0xC400 + 0x100*(n-4), but this driver only uses queue 0.
const E1000E_RDBAL: usize = 0x2800 / 4;
const E1000E_RDBAH: usize = 0x2804 / 4;
const E1000E_RDLEN: usize = 0x2808 / 4;
const E1000E_RDTR: usize = 0x2820 / 4; // RX Delay Timer
const E1000E_RADV: usize = 0x282C / 4; // RX Absolute Delay Timer
const E1000E_RDH: usize = 0x2810 / 4;
const E1000E_RDT: usize = 0x2818 / 4;
// Transmit descriptor ring
const E1000E_TDBAL: usize = 0x3800 / 4;
const E1000E_TDBAH: usize = 0x3804 / 4;
const E1000E_TDLEN: usize = 0x3808 / 4;
const E1000E_TDH: usize = 0x3810 / 4;
const E1000E_TDT: usize = 0x3818 / 4;
// Receive address
const E1000E_RAL0: usize = 0x5400 / 4;
const E1000E_RAH0: usize = 0x5404 / 4;
// Multicast table (128 × u32)
const E1000E_MTA_BASE: usize = 0x5200 / 4;
const E1000E_MTA_LEN: usize = 128;
// Statistics (regs.h) — clear-on-read; used for ifconfig /proc/net/dev.
const E1000E_GPRC: usize = 0x04074 / 4;
const E1000E_GPTC: usize = 0x04080 / 4;
const E1000E_GORCL: usize = 0x04088 / 4;
const E1000E_GORCH: usize = 0x0408C / 4;
const E1000E_GOTCL: usize = 0x04090 / 4;
const E1000E_GOTCH: usize = 0x04094 / 4;
const E1000E_MPC: usize = 0x04010 / 4;

// Additional registers for offloading/filtering
const E1000E_VET: usize = 0x0038 / 4;
const E1000E_RXCSUM: usize = 0x5000 / 4;
const E1000E_RFCTL: usize = 0x5008 / 4;
const E1000E_MRQC: usize = 0x5818 / 4;
const E1000E_FEXTNVM6: usize = 0x01014 / 4;
const E1000E_FEXTNVM7: usize = 0x01018 / 4;
const E1000E_FEXTNVM11: usize = 0x05BBC / 4;
const E1000E_KMRNCTRLSTA: usize = 0x00034 / 4;
const E1000E_PBA: usize = 0x01000 / 4;
const E1000E_WUC: usize = 0x05800 / 4;
const E1000E_FCTTV: usize = 0x00170 / 4;
const E1000E_FCRTV: usize = 0x05F40 / 4;
const E1000E_FCRTL: usize = 0x02160 / 4;
const E1000E_FCRTH: usize = 0x02168 / 4;
const E1000E_TARC0: usize = 0x03840 / 4;
const E1000E_TARC1: usize = 0x03940 / 4;
const E1000E_TXDCTL: usize = 0x03828 / 4;
/// Linux `e1000_configure_tx`: mirror TXDCTL(0) to queue 1 (`ew32(TXDCTL(1), er32(TXDCTL(0)))`).
const E1000E_TXDCTL1: usize = E1000E_TXDCTL + (0x100 / 4);
const E1000E_TIDV: usize = 0x03820 / 4;
const E1000E_TADV: usize = 0x0382C / 4;
const E1000E_RXDCTL: usize = 0x02828 / 4;
/// SRRCTL queue 0: Linux `E1000_SRRCTL(_n)` = `0x0280C + (_n)*0x100` for n < 4.
/// The previous wrong value 0x02100 left the real SRRCTL at its hardware reset
/// default (BSIZEPACKET=0), which limits DMA to the 60-byte Ethernet minimum
/// frame size — exactly the truncation seen in the DHCP debug log.
const E1000E_SRRCTL: usize = 0x0280C / 4;
/// BSIZEPACKET field: 2 = 2 KB RX buffers (Linux default for e1000e).
const SRRCTL_BSIZE_2K: u32 = 2;
const SRRCTL_DROP_EN: u32 = 1 << 31;
/// DESCTYPE field (bits 25–27) must be 0 for legacy extended WB (RFCTL_EXTEN).
const SRRCTL_DESCTYPE_MASK: u32 = 0x0E00_0000;
/// Linux `E1000_RCTL_SZ_2048` — 2 KB RX buffers when not using jumbo/SRRCTL alone.
const RCTL_SZ_2048: u32 = 0x0000_0000;
const E1000E_FEXTNVM4: usize = 0x000E0 / 4;
const E1000E_FEXTNVM9: usize = 0x05BB4 / 4;
const E1000E_PBECCSTS: usize = 0x0100C / 4;
const E1000E_CTRL_EXT: usize = 0x00018 / 4;
const E1000E_CRC_OFFSET: usize = 0x05F50 / 4;
const E1000E_KABGTXD: usize = 0x03004 / 4;
const E1000E_IOSFPC: usize = 0x00F28 / 4;
const E1000E_FWSM: usize = 0x05B54 / 4;
const E1000E_WUFC: usize = 0x05808 / 4;
const E1000E_WUS: usize = 0x05810 / 4;
const E1000E_FEXTNVM3: usize = 0x0003C / 4;
const E1000E_H2ME: usize = 0x05B50 / 4;
const E1000E_MANC: usize = 0x05820 / 4;


// RFCTL (Linux e1000e/defines.h): EXTEN enables extended RX descriptor write-back
// (union e1000_rx_desc_extended in hw.h — DD/status in u32 @ +8, length u16 @ +12).
const RFCTL_EXTEN: u32 = 1 << 15; // E1000_RFCTL_EXTEN
// E1000_RXD_STAT_* apply to the low byte of wb.upper.status_error (full dword in staterr).
const RXD_EXT_DD: u32 = 0x01;
const RXD_EXT_EOP: u32 = 0x02;
/// Linux `E1000_CTRL_EXT_IAME` — reading ICR masks until IMS is written again.
const CTRL_EXT_IAME: u32 = 1 << 27;
/// ICR cause bits (Intel 8257x / e1000e).
const ICR_TXDW: u32 = 1 << 0;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXT0: u32 = 1 << 7;
const ICR_RX_ANY: u32 = ICR_RXT0 | ICR_RXDMT0;
/// Hold LANPHYPC low long enough for PHY rail discharge on warm/cold boot.
const LANPHYPC_POWERDOWN_HOLD_US: u64 = 10_000;
/// Analog PHY settle after releasing LANPHYPC (some boards need >50 ms).
const LANPHYPC_POWERUP_SETTLE_US: u64 = 100_000;
/// Minimum DMA base alignment for descriptor rings (Intel hardware requirement).
const DMA_DESC_ALIGN: usize = 16;
/// Linux `e1000_irq_enable` (PCH): `IMS_ENABLE_MASK | E1000_IMS_ECCER`.
const IMS_REARM_LINUX: u32 = (1 << 0)   // TXDW
    | (1 << 2)   // LSC
    | (1 << 3)   // RXSEQ
    | (1 << 4)   // RXDMT0
    | (1 << 7)   // RXT0
    | (1 << 22); // ECCER
/// Conventional mode: RX + link-change only (no auto-mask via IAME).
const IMS_CONVENTIONAL: u32 = (1 << 7) | (1 << 2); // RXT0 | LSC

// KABGTXD bits
const KABGTXD_BGSQLBIAS: u32 = 0x00050000;

// FEXTNVM6 bits
const FEXTNVM6_K1_OFF_EN: u32 = 1 << 31;
const FEXTNVM6_DIS_ELDW: u32 = 1 << 5; // Disable Early Link Down Window

// KMRNCTRLSTA bits for ICH8/PCH
const KMRNCTRLSTA_OFFSET_SHIFT: u32 = 16;
const KMRNCTRLSTA_REN: u32 = 1 << 21;
const KMRNCTRLSTA_WEN: u32 = 1 << 22;
const KMRNCTRLSTA_K1_CONFIG: u16 = 0x1F; // Index 0x1F
const KMRNCTRLSTA_K1_ENABLE: u16 = 1 << 13;
const KMRNCTRLSTA_TIMEOUTS: u16 = 0x4;
const KMRNCTRLSTA_INBAND_PARAM: u16 = 0x9;
const E1000E_GCR: usize = 0x05B00 / 4;
const E1000E_FFLT_DBG: usize = 0x05F04 / 4;
/// Linux `PCIE_NO_SNOOP_ALL` — GCR bits 0..5.
const GCR_PCIE_NO_SNOOP_ALL: u32 = 0x3F;
const FFLT_DBG_DONT_GATE_WAKE_DMA_CLK: u32 = 1 << 12;
const I217_PLL_CLOCK_GATE_MASK: u16 = 0x07FF;
/// Linux `SPEED_*` (mbps) for TIPG/EMI paths in ich8lan.c.
const SPEED_10: u32 = 10;
const SPEED_100: u32 = 100;
const SPEED_1000: u32 = 1000;
const PHY_EMI_ADDR: u32 = 0x10;
const PHY_EMI_DATA: u32 = 0x11;
const I217_RX_CONFIG_EMI: u16 = 0xB20C;
const I82577_CFG_REG: u32 = 22;
const I82577_PHY_CTRL_2: u32 = 18;
const I82577_CFG_ASSERT_CRS_ON_TX: u16 = 1 << 15;
const I82577_CFG_ENABLE_DOWNSHIFT: u16 = 3 << 10;
const I82577_PHY_CTRL2_AUTO_MDI_MDIX: u16 = 0x0400;
/// ich8lan.h `HV_PM_CTRL` — page 770 reg 17.
const HV_PM_CTRL: u32 = (770 << 5) | 17;
const HV_PM_CTRL_K1_CLK_REQ: u16 = 0x0200;
/// Linux `PHY_AUTO_NEG_LIMIT` (100 ms steps).
const PHY_AUTO_NEG_LIMIT: u16 = 50;
const MANC_EN_MNG2HOST: u32 = 1 << 21;

// CTRL bits
const CTRL_FD: u32 = 1 << 0; // full duplex
const CTRL_SLU: u32 = 1 << 6; // set link up
const CTRL_ASDE: u32 = 1 << 5; // auto-speed detection enable
const CTRL_RST: u32 = 1 << 26; // full MAC + PHY reset
const CTRL_PHY_RST: u32 = 1 << 31; // PHY-only reset (Linux E1000_CTRL_PHY_RST)
const CTRL_TFCE: u32 = 1 << 27; // Transmit Flow Control Enable
const CTRL_RFCE: u32 = 1 << 28; // Receive Flow Control Enable
const CTRL_VME: u32 = 1 << 30; // VLAN Mode Enable
const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2; // GIO Master Disable
const CTRL_LANPHYPC_OVERRIDE: u32 = 0x00010000;
const CTRL_LANPHYPC_VALUE: u32 = 0x00020000;
const CTRL_SPD_1000: u32 = 1 << 9;
const CTRL_SPD_100: u32 = 1 << 8;
const CTRL_FRCSPD: u32 = 1 << 12;
const CTRL_FRCDPX: u32 = 1 << 11;
const CTRL_ILOS: u32 = 1 << 7; // Invert Loss of Signal


// CTRL_EXT bits
const CTRL_EXT_RO_DIS: u32 = 1 << 2; // Relaxation Order Disable
const CTRL_EXT_PHYPDEN: u32 = 1 << 20; // PHY Power Down Enable
const CTRL_EXT_DPG_EN: u32 = 1 << 3; // Dynamic Power Gating Enable
const CTRL_EXT_SPD_BYPS: u32 = 1 << 15; // Speed-select bypass (Linux k1/speed pulse)
const IGP_PHY_PAGE_SELECT: u32 = 31;
const IGP_PAGE_SHIFT: u32 = 5;
const MAX_PHY_MULTI_PAGE_REG: u32 = 0xF;
const PHY_REG_770_19: u32 = (770 << 5) | 19; // IGP3_KMRN_DIAG — link stall fix

// FEXTNVM4 bits
const FEXTNVM4_BEACON_DURATION_8USEC: u32 = 0x7;
const FEXTNVM4_BEACON_DURATION_MASK: u32 = 0x7;

// FEXTNVM7 bits
const FEXTNVM7_SIDE_CLK_UNGATE: u32 = 1 << 2;
const FEXTNVM7_DISABLE_SMB_PERST: u32 = 1 << 5;
const FEXTNVM7_NEED_DESCR_RING_FLUSH: u32 = 1 << 16;
// Do NOT set bit 28 here — not in Linux e1000e; on I219 it breaks broadcast RX (DHCP).
const RFCTL_NFSW_DIS: u32 = 1 << 6; // Linux ich8: disable NFS write filter
const RFCTL_NFSR_DIS: u32 = 1 << 7; // Linux ich8: disable NFS read filter

// FEXTNVM9 bits
const FEXTNVM9_IOSFSB_CLKGATE_DIS: u32 = 1 << 11;
const FEXTNVM9_IOSFSB_CLKREQ_DIS: u32 = 1 << 12;

// FEXTNVM11 bits
const FEXTNVM11_DISABLE_L1_2: u32 = 1 << 1;
const FEXTNVM11_DISABLE_MULR_FIX: u32 = 1 << 13;

// TXDCTL bits (e1000e/defines.h)
const TXDCTL_PTHRESH_MASK: u32 = 0x0000_003F;
const TXDCTL_HTHRESH_MASK: u32 = 0x0000_3F00;
const TXDCTL_WTHRESH_MASK: u32 = 0x003F_0000;
const TXDCTL_GRAN: u32 = 1 << 24; // 0=cache lines, 1=descriptors
/// Linux `E1000_TXDCTL_FULL_TX_DESC_WB` — per-descriptor DD write-back (not bit 26).
const TXDCTL_FULL_TX_DESC_WB: u32 = 0x0101_0000;
const TXDCTL_MAX_TX_DESC_PREFETCH: u32 = 0x0100_001F;
const TXDCTL_COUNT_DESC: u32 = 1 << 22; // bit 22 must be 1 on ICH8+
const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25; // PCH-SPT (I219) and later: must be set to enable TX DMA
/// Linux `E1000_TXDCTL_DMA_BURST_ENABLE` (wthresh=1 avoids TX stalls on PCH).
const TXDCTL_DMA_BURST: u32 =
    TXDCTL_GRAN | TXDCTL_COUNT_DESC | (1 << 16) | (1 << 8) | 0x1F;

// RXDCTL bits
const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25; // PCH-SPT (I219) and later: must be set to enable RX DMA
/// Linux `E1000_RXDCTL_DMA_BURST_ENABLE` (netdev.c / e1000.h).
const RXDCTL_DMA_BURST: u32 = 0x0100_0000 | (4 << 16) | (4 << 8) | 0x20;
const RDTR_FPD: u32 = 1 << 31;
/// Linux `BM_WUC_PAGE` — PHY wakeup/filter page (ich8lan.h).
const BM_WUC_PAGE: u32 = 800;
const BM_PORT_CTRL_PAGE: u32 = 769;
const BM_WUC_ENABLE_REG: u32 = 17;
const BM_WUC_ENABLE_BIT: u16 = 1 << 2;
const BM_WUC_HOST_WU_BIT: u16 = 1 << 4;
const BM_WUC_ME_WU_BIT: u16 = 1 << 5;
const BM_WUC_ADDRESS_OPCODE: u32 = 0x11;
const BM_WUC_DATA_OPCODE: u32 = 0x12;
/// Linux: BM WUC/port registers are on MDIO address 1, not the link PHY (2).
const BM_PHY_MDIO_ADDR: u8 = 1;
const BM_RCTL_UPE: u16 = 0x0001;
const BM_RCTL_MPE: u16 = 0x0002;
const BM_RCTL_MO_SHIFT: u16 = 3;
const BM_RCTL_MO_MASK: u16 = 0x0018;
const BM_RCTL_BAM: u16 = 0x0020;
const BM_RCTL_PMCF: u16 = 0x0040;
const BM_RCTL_RFCE: u16 = 0x0080;
/// Linux `E1000_PCH_LPT_RAR_ENTRIES` (I219).
const PCH_LPT_RAR_ENTRIES: usize = 12;
const ICH_MTA_REG_COUNT: usize = 32;
const RAH_AV: u32 = 0x8000_0000;
const RCTL_MO_3: u32 = 0x0000_3000;
const RCTL_PMCF: u32 = 0x0080_0000;
/// Linux `E1000_WUC_PHY_WAKE | E1000_WUC_PME_STATUS` — PHY filter path on PCH.
const WUC_PHY_WAKE: u32 = 0x0000_0100;
const WUC_PME_STATUS: u32 = 0x0000_0004;
const CTRL_MEHE: u32 = 1 << 19;
const PBECCSTS_ECC_ENABLE: u32 = 1 << 16;
const RFCTL_IPV6_EX_DIS: u32 = 1 << 16;
const RFCTL_NEW_IPV6_EXT_DIS: u32 = 1 << 17;

// STATUS bits
const STATUS_LU: u32 = 1 << 1; // link up
const STATUS_LAN_INIT_DONE: u32 = 1 << 9; // LAN init from NVM completed (ICH10+)
const STATUS_PHYRA: u32 = 1 << 10; // PHY Reset Asserted — must clear after PHY_RST
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19; // GIO Master Enable Status
const STATUS_SPEED_MASK: u32 = 0x000000C0;
const STATUS_SPEED_1000: u32 = 0x00000080;
const STATUS_SPEED_100: u32 = 0x00000040;
const STATUS_FD: u32 = 1 << 0;

// EXTCNF_CTRL (PCH PHY / NVM shared access)
const EXTCNF_CTRL_SWFLAG: u32 = 0x20;
const EXTCNF_CTRL_GATE_PHY_CFG: u32 = 0x80;

// EERD bits (discrete e1000e like 82574L use bit 4 for DONE; PCH-integrated like I219 use bit 1)
const EERD_START: u32 = 1 << 0;
const EERD_DONE_BIT4: u32 = 1 << 4;
const EERD_DONE_BIT1: u32 = 1 << 1;
const EERD_DATA_SHIFT: u32 = 16;

const PCICFG_DESC_RING_STATUS: u16 = 0xE4;
const FLUSH_DESC_REQUIRED: u16 = 0x100;

// MDIO / MDIC (Clause 22 access to integrated PHY)
const MDIC_REG_SHIFT: u32 = 16;
const MDIC_PHY_SHIFT: u32 = 21;
const MDIC_OP_READ: u32 = 0x0800_0000;
const MDIC_OP_WRITE: u32 = 0x0400_0000;
const MDIC_READY: u32 = 0x1000_0000;
const MDIC_ERROR: u32 = 0x4000_0000;
const MII_BMCR: u32 = 0x00;
const MII_BMSR: u32 = 0x01;
/// Marvell BM copper specific status (`BM_CS_STATUS`, Linux `phy.h`).
const BM_CS_STATUS: u32 = 17;
const BM_CS_STATUS_LINK_UP: u16 = 0x0400;
const BM_CS_STATUS_RESOLVED: u16 = 0x0800;
const BM_CS_STATUS_SPEED_MASK: u16 = 0xC000;
const MII_ADVERTISE: u32 = 0x04;
const MII_LPA: u32 = 0x05;
const MII_CTRL1000: u32 = 0x09;
const MII_STAT1000: u32 = 0x0A;
const LPA_1000FULL: u16 = 0x0800;
const STAT1000_LP_1000FULL: u16 = 0x0800;
const BMCR_ANENABLE: u16 = 0x1000;
const BMCR_ANRESTART: u16 = 0x0200;
const BMSR_ANEG_COMPLETE: u16 = 0x0020;
const ADVERTISE_CSMA: u16 = 0x0001;
const ADVERTISE_10HALF: u16 = 0x0020;
const ADVERTISE_10FULL: u16 = 0x0040;
const ADVERTISE_100HALF: u16 = 0x0080;
const ADVERTISE_100FULL: u16 = 0x0100;
const ADVERTISE_PAUSE_CAP: u16 = 0x0400;
const ADVERTISE_PAUSE_ASYM: u16 = 0x0800;
const ADVERTISE_ALL_COPPER: u16 = ADVERTISE_CSMA
    | ADVERTISE_10HALF
    | ADVERTISE_10FULL
    | ADVERTISE_100HALF
    | ADVERTISE_100FULL
    | ADVERTISE_PAUSE_CAP
    | ADVERTISE_PAUSE_ASYM;
const ADVERTISE_1000FULL: u16 = 0x0200;
const CTL1000_AS_MASTER: u16 = 0x0800;
const CTL1000_ENABLE_MASTER: u16 = 0x1000;
/// I82577/I217/I219 PHY status 2 (Linux `I82577_PHY_STATUS_2`).
const MII_PHY_STATUS_2: u32 = 26;
const PHY_STATUS2_SPEED_MASK: u16 = 0x0300;
const PHY_STATUS2_SPEED_1000: u16 = 0x0200;
const PHY_STATUS2_SPEED_100: u16 = 0x0100;
const PHY_STATUS2_AUTONEG_DONE: u16 = 0x1000;
const PHY_STATUS2_LINK_UP: u16 = 0x0040;

// STATUS-ready poll: 150 ms covers PCH-based NICs (I217/I218/I219).
const STATUS_POLL_US: u64 = 150_000;
// NVM EERD-done poll: 10 ms is more than enough for any e1000e silicon.
const NVM_POLL_US: u64 = 10_000;

// RCTL bits (e1000e/defines.h, e1000_setup_rctl in netdev.c)
const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2; // store bad packets
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_LPE: u32 = 1 << 5; // long packet enable (jumbo)
const RCTL_DTYP_PS: u32 = 1 << 10; // packet-split descriptor type
const RCTL_MO_MASK: u32 = 0x3 << 12; // multicast offset
const RCTL_BAM: u32 = 1 << 15; // broadcast accept
const RCTL_RX_SZ_MASK: u32 = 0x3 << 16; // buffer size field when BSEX=0
const RCTL_VFE: u32 = 1 << 18; // VLAN Filter Enable
const RCTL_BSEX: u32 = 1 << 25; // buffer size extension
const RCTL_SECRC: u32 = 1 << 26; // strip CRC

// TCTL bits
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_RTLC: u32 = 1 << 24; // E1000_TCTL_RTLC — Linux e1000_configure_tx
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_CT_LINUX: u32 = 15 << TCTL_CT_SHIFT; // E1000_COLLISION_THRESHOLD
const TCTL_COLD_LINUX: u32 = 63 << 12; // E1000_COLLISION_DISTANCE << E1000_COLD_SHIFT

const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;

const NUM_RX: usize = 256;
/// Leave a descriptor cushion at RX bring-up (RDH==RDT means empty; I219 races on full ring).
const RX_BOOT_POST_MAX: usize = NUM_RX - 2;
/// Microseconds to wait after RCTL.EN=0 before rewriting the RX descriptor ring.
const RX_DMA_DRAIN_US: u32 = 100;
/// Max PCIe write-back settle attempts when DD is set but length is not yet valid.
const RX_DESC_WB_SETTLE_TRIES: u32 = 12;
const RX_DESC_WB_SETTLE_US: u32 = 2;
/// Scatter-gather reassembly limits (non-EOP fragments without EOP=1).
const RX_SG_MAX_BYTES: usize = 9216;
const RX_SG_MAX_FRAGS: u8 = 24;
const NUM_TX: usize = 256;
const BUF_SIZE: usize = 2048 + 128;
/// Linux `E1000_RX_BUFFER_WRITE` — batch RDT doorbell every N descriptors.
const RX_BUFFER_WRITE: usize = 16;
/// Conservative TSC cycle budget per requested microsecond (safe through ~6 GHz turbo).
const UDELAY_TSC_PER_US: u64 = 7000;
/// Spin-loop floor per microsecond when the APIC/HPET timer is stuck.
const UDELAY_SPINS_PER_US: u64 = 3500;
/// Max RX slots consumed per `receive()` call while assembling SG (EOP=0) fragments.
const RX_SG_SLOTS_PER_CALL: u8 = 8;

// ---------------------------------------------------------------------------
// Descriptor layouts (§3.2.3 / §3.3.3 of 82574 datasheet)
// ---------------------------------------------------------------------------
// align(16): the NIC DMA engine requires 16-byte alignment. CRITICALLY, the
// hardware ALWAYS uses a fixed 16-byte descriptor stride (RDLEN/16 = slots).
// Padding to 64 bytes would make size_of::<RxDesc>()=64 → RDLEN=256×64=16384
// → NIC sees 1024 slots, with 768 entries having addr=0 (our padding) →
// DMA writes frames to physical address 0 → completely broken RX ring.
//
// Cache-line false-sharing is mitigated by flushing ALL descriptors before
// the single RDT doorbell write, preventing CPU/NIC races on the same line.

/// Legacy RX descriptor (16 bytes). Hardware writes back in extended format
/// when RFCTL_EXTEN is set; we only write the buffer address field.
#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, Default)]
struct RxDesc {
    addr:     u64,  // buffer physical address (written by driver)
    reserved: u64,  // written back by HW: staterr[31:0] @ +8, length[47:32] @ +12
}
const _RX_DESC_SIZE_OK: () = assert!(core::mem::size_of::<RxDesc>() == 16);

// Extended RX write-back layout (RFCTL_EXTEN=1, I219 default):
//   +0  addr     (driver → HW)
//   +8  staterr  (HW → driver: DD=bit0, EOP=bit1)
//   +12 length   (HW → driver: byte count in buffer)
//   +14 vlan     (ignored)

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
const _TX_DESC_SIZE_OK: () = assert!(core::mem::size_of::<TxDesc>() == 16);

/// Intel placeholder MAC when NVM/RAL reads fail — link may still work but filters are wrong.
const E1000E_PLACEHOLDER_MAC: [u8; 6] = [0x00, 0x0E, 0x10, 0x00, 0x0E, 0x00];

// ---------------------------------------------------------------------------
// DMA allocation helpers (thin wrappers over the kernel C FFI)
// ---------------------------------------------------------------------------
extern "C" {
    fn drivers_dma_alloc(pages: usize) -> usize; // returns phys addr
    fn drivers_dma_dealloc(paddr: usize, pages: usize) -> i32;
    fn drivers_phys_to_virt(paddr: usize) -> usize;
    fn drivers_virt_to_phys(vaddr: usize) -> usize;
}

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
// E1000eHw — raw hardware state
// ---------------------------------------------------------------------------
pub struct E1000eHw {
    base: usize, // MMIO virtual base
    pci_loc: Location,
    device_id: u16,

    mac: [u8; 6],

    rx_ring: DmaRegion,
    /// Single contiguous DMA block for all RX packet buffers (NUM_RX × BUF_SIZE).
    rx_buf_pool: DmaRegion,
    /// Linux `next_to_clean` — next descriptor to check for DD.
    rx_next_to_clean: usize,
    /// Linux `next_to_use` — next descriptor to post to hardware via RDT.
    rx_next_to_use: usize,

    tx_ring: DmaRegion,
    /// Single contiguous DMA block for all TX packet buffers (NUM_TX × BUF_SIZE).
    tx_buf_pool: DmaRegion,
    tx_tail: usize,
    phy_addr: u8,
    pub stats: NetStats,
    /// Last GPRC snapshot (clear-on-read); used to detect HW RX without DD.
    last_hw_rx_packets: u32,
    rx_diag_counter: u32,
    /// Cap frames delivered per `poll()` so AF_PACKET `send()` does not drain the whole ring.
    rx_poll_budget: u8,
    /// Full PHY/link tuning deferred to `poll()` so PCI probe does not block the shell.
    link_bringup_pending: bool,
    link_bringup_attempts: u8,
    /// State machine for chunked bringup (see `deferred_link_bringup_tick`).
    link_bringup_stage: u8,
    /// Set after full PHY tune + RX arm in `enable_rx_after_link`.
    rx_link_armed: bool,
    /// reg26 at 10M while MII HCD is higher — stop 100/1000 autoneg restarts.
    link_10m_degraded: bool,
    /// Aligned with Linux `mac.get_link_status`: set on LSC or when link is down to force PHY reads.
    get_link_status: bool,
    /// Currently resolved link status
    link_up: bool,
    /// Currently resolved speed (in Mbps)
    link_speed: u32,
    /// Currently resolved duplex (true = full)
    link_duplex: bool,
    /// Last microsecond timestamp we checked the link state (periodic poll).
    last_link_check_us: u64,
    /// Start of current bring-up stage in microseconds.
    stage_start_us: u64,
    /// Whether the hardware supports and is using extended write-back descriptors.
    use_extended_descriptors: bool,
    /// Scatter-gather assembly buffer for multi-descriptor (non-EOP) frames.
    /// Fragments with EOP=0 are appended here; the final EOP=1 fragment delivers
    /// the assembled frame. Reset to empty after delivery or on error.
    rx_sg_buf: Vec<u8>,
    /// Non-EOP fragments buffered without a completed frame yet.
    rx_sg_frag_count: u8,
    /// Descriptors posted in RAM since the last E1000E_RDT doorbell (batched refill).
    rx_post_since_doorbell: u16,
    /// SRRCTL at 0x280C does not read back on some steppings (e.g. 15b8); use RCTL only.
    srrctl_absent: bool,
    /// RX/TX rings mapped UC — skip clflush on descriptor WB paths.
    dma_uncached: bool,
}

impl E1000eHw {
    // -----------------------------------------------------------------------
    // Kumeran (KMRN) register access (ICH8/PCH specific)
    // -----------------------------------------------------------------------

    /// Busy-wait for `us` microseconds — APIC timer when live, TSC+spin floor otherwise.
    fn udelay(us: u64) {
        if us == 0 {
            return;
        }
        const MAX_SPINS: u64 = 800_000_000;
        let min_spins = us.saturating_mul(UDELAY_SPINS_PER_US);
        #[cfg(target_arch = "x86_64")]
        let tsc_start = unsafe { core::arch::x86_64::_rdtsc() };
        #[cfg(target_arch = "x86_64")]
        let tsc_budget = us.saturating_mul(UDELAY_TSC_PER_US);
        let t0 = timer_now_as_micros();
        let mut spins = 0u64;
        loop {
            core::hint::spin_loop();
            spins = spins.wrapping_add(1);
            let elapsed = timer_now_as_micros().wrapping_sub(t0);
            if elapsed >= us {
                break;
            }
            #[cfg(target_arch = "x86_64")]
            {
                let tsc_elapsed = unsafe { core::arch::x86_64::_rdtsc().wrapping_sub(tsc_start) };
                if tsc_elapsed >= tsc_budget && spins >= min_spins {
                    if elapsed == 0 && us > 10 {
                        log::trace!(
                            "[e1000e] udelay({}us) via TSC floor ({} cycles, timer stuck)",
                            us,
                            tsc_elapsed
                        );
                    }
                    break;
                }
            }
            #[cfg(not(target_arch = "x86_64"))]
            if spins >= min_spins {
                break;
            }
            if spins >= MAX_SPINS {
                warn!(
                    "[e1000e] udelay cap hit ({}us, {} spins, elapsed {}us)",
                    us,
                    spins,
                    elapsed
                );
                break;
            }
        }
    }

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

    fn mark_dma_region_uncached(region: &DmaRegion, label: &str) -> bool {
        if !region.mark_uncached() {
            crate::klog_warn!("[e1000e] {}: mark_uncached() failed (PAT remap rejected)\n", label);
            return false;
        }
        if !region.verify_uncached() {
            crate::klog_warn!(
                "[e1000e] {}: PTE verify failed — page still WB (PAT/MTRR not applied)\n",
                label
            );
            return false;
        }
        true
    }

    unsafe fn kmrn_read(&self, offset: u16) -> u16 {
        let cmd = ((offset as u32) << KMRNCTRLSTA_OFFSET_SHIFT) | KMRNCTRLSTA_REN;
        mmio_write(self.base, E1000E_KMRNCTRLSTA, cmd);
        let _ = mmio_read(self.base, E1000E_KMRNCTRLSTA); // flush write
        Self::udelay(2); // Linux uses udelay(2) between write and read
        (mmio_read(self.base, E1000E_KMRNCTRLSTA) & 0xFFFF) as u16
    }

    unsafe fn kmrn_write(&self, offset: u16, data: u16) {
        let cmd = ((offset as u32) << KMRNCTRLSTA_OFFSET_SHIFT) | KMRNCTRLSTA_WEN | (data as u32);
        mmio_write(self.base, E1000E_KMRNCTRLSTA, cmd);
        let _ = mmio_read(self.base, E1000E_KMRNCTRLSTA); // flush write
        Self::udelay(2); // Linux uses udelay(2) after write
    }

    // -----------------------------------------------------------------------
    // NVM word read via EERD (works on all e1000e silicon)
    // -----------------------------------------------------------------------
    unsafe fn nvm_read_word(&self, offset: u16) -> u16 {
        // Try Address Shift 2 first (82574L and most discrete e1000e)
        let cmd = ((offset as u32) << 2) | EERD_START;
        mmio_write(self.base, E1000E_EERD, cmd);
        let mut tries = (NVM_POLL_US / 50).max(1);
        while tries > 0 {
            let v = mmio_read(self.base, E1000E_EERD);
            if v & (EERD_DONE_BIT4 | EERD_DONE_BIT1) != 0 {
                return (v >> EERD_DATA_SHIFT) as u16;
            }
            Self::udelay(50); // C6: allow timer to tick on bare-metal
            tries -= 1;
        }

        // Try Address Shift 3 (PCH-integrated NICs like I217/I218/I219)
        let cmd = ((offset as u32) << 3) | EERD_START;
        mmio_write(self.base, E1000E_EERD, cmd);
        let mut tries = (NVM_POLL_US / 50).max(1);
        while tries > 0 {
            let v = mmio_read(self.base, E1000E_EERD);
            if v & (EERD_DONE_BIT4 | EERD_DONE_BIT1) != 0 {
                return (v >> EERD_DATA_SHIFT) as u16;
            }
            Self::udelay(50); // C6: allow timer to tick on bare-metal
            tries -= 1;
        }
        0
    }

    // -----------------------------------------------------------------------
    // Read MAC address from RAL0/RAH0 registers (usually set by BIOS)
    // -----------------------------------------------------------------------
    unsafe fn read_mac_from_hw(&mut self) {
        let ral = mmio_read(self.base, E1000E_RAL0);
        let rah = mmio_read(self.base, E1000E_RAH0);

        if ral == 0 && (rah & 0xFFFF) == 0 {
            return;
        }
        self.mac[0] = (ral & 0xFF) as u8;
        self.mac[1] = ((ral >> 8) & 0xFF) as u8;
        self.mac[2] = ((ral >> 16) & 0xFF) as u8;
        self.mac[3] = ((ral >> 24) & 0xFF) as u8;
        self.mac[4] = (rah & 0xFF) as u8;
        self.mac[5] = ((rah >> 8) & 0xFF) as u8;
    }

    fn is_valid_mac(&self) -> bool {
        let all_zeros = self.mac.iter().all(|&b| b == 0);
        let all_fs = self.mac.iter().all(|&b| b == 0xff);
        !all_zeros && !all_fs
    }

    fn is_placeholder_mac(&self) -> bool {
        self.mac == E1000E_PLACEHOLDER_MAC
    }

    fn warn_mac_diagnostic(&self) {
        if self.is_placeholder_mac() {
            crate::klog_warn!(
                "[e1000e] MAC is {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} — NVM/RAL read failed; \
                 check ULP/ME timeout and mdic timing\n",
                self.mac[0],
                self.mac[1],
                self.mac[2],
                self.mac[3],
                self.mac[4],
                self.mac[5]
            );
        }
    }

    /// Verify descriptor stride and ring geometry before programming RDLEN/TDLEN.
    fn validate_dma_ring_layout(&self) -> bool {
        const DESC_BYTES: usize = 16;
        if size_of::<RxDesc>() != DESC_BYTES || size_of::<TxDesc>() != DESC_BYTES {
            crate::klog_err!(
                "[e1000e] descriptor size mismatch Rx={} Tx={} (expected {})\n",
                size_of::<RxDesc>(),
                size_of::<TxDesc>(),
                DESC_BYTES
            );
            return false;
        }

        let rx_ring = self.rx_ring.as_ptr::<RxDesc>();
        let rx_base_v = rx_ring as usize;
        let rx_base_p = self.rx_ring.paddr();
        if (rx_base_v | rx_base_p) & (DMA_DESC_ALIGN - 1) != 0 {
            crate::klog_err!(
                "[e1000e] RX ring base v={:#x} p={:#x} not {}-byte aligned\n",
                rx_base_v,
                rx_base_p,
                DMA_DESC_ALIGN
            );
            return false;
        }

        let tx_ring = self.tx_ring.as_ptr::<TxDesc>();
        let tx_base_v = tx_ring as usize;
        let tx_base_p = self.tx_ring.paddr();
        if (tx_base_v | tx_base_p) & (DMA_DESC_ALIGN - 1) != 0 {
            crate::klog_err!(
                "[e1000e] TX ring base v={:#x} p={:#x} not {}-byte aligned\n",
                tx_base_v,
                tx_base_p,
                DMA_DESC_ALIGN
            );
            return false;
        }

        for i in 1..NUM_RX {
            let prev = unsafe { rx_ring.add(i - 1) as usize };
            let curr = unsafe { rx_ring.add(i) as usize };
            if curr - prev != DESC_BYTES {
                crate::klog_err!(
                    "[e1000e] RX ring stride {} != {} at slot {} — RDLEN would corrupt DMA\n",
                    curr - prev,
                    DESC_BYTES,
                    i
                );
                return false;
            }
        }

        let rdlen = (NUM_RX * DESC_BYTES) as u32;
        let tdlen = (NUM_TX * DESC_BYTES) as u32;
        for i in 1..NUM_RX {
            if self.rx_buf_paddr(i) != self.rx_buf_paddr(i - 1) + BUF_SIZE as u64 {
                crate::klog_err!(
                    "[e1000e] RX buf pool not contiguous at slot {} — DMA aliasing risk\n",
                    i
                );
                return false;
            }
            if self.rx_buf_paddr(i) & 63 != 0 {
                crate::klog_warn!(
                    "[e1000e] RX buf slot {} paddr {:#x} not 64-byte aligned\n",
                    i,
                    self.rx_buf_paddr(i)
                );
            }
        }
        crate::klog_info!(
            "[e1000e] DMA layout: NUM_RX={} RDLEN={} rx_paddr={:#x} rx_pool={:#x}+{} \
             NUM_TX={} TDLEN={} tx_paddr={:#x} uc={}\n",
            NUM_RX,
            rdlen,
            rx_base_p,
            self.rx_buf_pool.paddr(),
            self.rx_buf_pool.byte_len(),
            NUM_TX,
            tdlen,
            self.tx_ring.paddr(),
            self.dma_uncached
        );
        true
    }

    /// Returns true for PCH-LPT (I217/I218) and later integrated NICs.
    fn is_pch_lpt_or_later(&self) -> bool {
        self.is_pch_spt_or_later() || matches!(self.device_id,
            0x1502..=0x1503 | 0x153a..=0x153b | 0x155a | 0x1559 | 0x15a0..=0x15a3
        )
    }

    /// I219 / PCH-SPT paths that break QEMU 82574 (coherent DMA — no clflush before DD).
    fn is_i219_metal_rx_hacks(&self) -> bool {
        self.is_pch_lpt_or_later()
    }

    /// Returns true for bare-metal devices (not QEMU 82574 coherent DMA).
    fn rx_needs_cache_invalidation(&self) -> bool {
        self.device_id != 0x10d3
    }

    /// Returns true when CPU cache maintenance is required before reading NIC write-back.
    fn rx_needs_cache_flush(&self) -> bool {
        self.rx_needs_cache_invalidation() && !self.dma_uncached
    }

    /// Mark descriptor rings and packet pools uncacheable; verify PTE cache bits on bare metal.
    fn setup_dma_uncached(&mut self) {
        if !self.rx_needs_cache_invalidation() {
            self.dma_uncached = false;
            return;
        }
        let ok = Self::mark_dma_region_uncached(&self.rx_ring, "rx_ring")
            & Self::mark_dma_region_uncached(&self.tx_ring, "tx_ring")
            & Self::mark_dma_region_uncached(&self.rx_buf_pool, "rx_buf_pool")
            & Self::mark_dma_region_uncached(&self.tx_buf_pool, "tx_buf_pool");
        self.dma_uncached = ok;
        if ok {
            crate::klog_info!("[e1000e] DMA rings/pools mapped and verified uncacheable (PAT UC)\n");
        } else {
            crate::klog_warn!(
                "[e1000e] UC DMA incomplete — RX/TX will use clflush+mfence+lfence on WB pages\n"
            );
        }
        let _ = self.validate_dma_ring_layout();
    }

    /// Returns true for PCH-SPT (I219) and later silicon.
    /// These chips require explicit RXDCTL/TXDCTL QUEUE_ENABLE (bit 25) to
    /// activate the RX/TX DMA queues after RCTL_EN/TCTL_EN.
    fn is_pch_spt_or_later(&self) -> bool {
        matches!(self.device_id,
            0x156f..=0x1570 | 0x15b7..=0x15be | 0x15d6..=0x15d8 | 0x15e3 |
            0x0d4c..=0x0d4f | 0x15f4..=0x15fc | 0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 | 0x550a..=0x5511 | 0x57a0..=0x57a1 | 0x57b3..=0x57ba |
            0x15df..=0x15e2 | 0x0d53 | 0x0d55 | 0x15f9 | 0x15fa
        )
    }

    /// Discover the responding PHY address via MDIO reads.
    unsafe fn detect_phy_addr(&mut self) {
        if self.try_detect_phy_addr() {
            return;
        }
        if self.is_pch_lpt_or_later() {
            crate::klog_warn!("[e1000e] MDIO silent during PHY detect — LANPHYPC power cycle\n");
            self.toggle_lanphypc();
            if self.try_detect_phy_addr() {
                return;
            }
        }
        crate::klog_warn!("[e1000e] no PHY detected, falling back to address 1\n");
        self.phy_addr = 1;
    }

    unsafe fn try_detect_phy_addr(&mut self) -> bool {
        // Standard PHY discovery: read MII_PHYSID1 on addresses 1..=31
        for pa in 1u8..=31 {
            if let Some(id1) = self.mdic_read(pa, 2) {
                if id1 != 0 && id1 != 0xFFFF {
                    crate::klog_info!("[e1000e] detected PHY at address {} (ID1={:#x})\n", pa, id1);
                    self.phy_addr = pa;
                    return true;
                }
            }
        }
        // Fallback: read MII_BMSR
        for pa in 1u8..=31 {
            if let Some(bmsr) = self.mdic_read(pa, MII_BMSR) {
                if bmsr != 0 && bmsr != 0xFFFF {
                    crate::klog_info!(
                        "[e1000e] detected PHY at address {} via BMSR ({:#x})\n",
                        pa,
                        bmsr
                    );
                    self.phy_addr = pa;
                    return true;
                }
            }
        }
        false
    }

    /// Poll until MDIO responds after a LANPHYPC transition.
    unsafe fn wait_phy_mdio_ready(&self, phy_addr: u8, budget_us: u64) -> bool {
        let t0 = timer_now_as_micros();
        while timer_now_as_micros().wrapping_sub(t0) < budget_us {
            if self.mdic_read(phy_addr, MII_BMSR).is_some() {
                return true;
            }
            Self::udelay(1_000);
        }
        false
    }

    // -----------------------------------------------------------------------
    // Read MAC address from NVM (3 words at offsets 0, 1, 2)
    // -----------------------------------------------------------------------
    unsafe fn read_mac_from_nvm(&mut self) {
        let w0 = self.nvm_read_word(0);
        let w1 = self.nvm_read_word(1);
        let w2 = self.nvm_read_word(2);
        self.mac[0] = (w0 & 0xFF) as u8;
        self.mac[1] = (w0 >> 8) as u8;
        self.mac[2] = (w1 & 0xFF) as u8;
        self.mac[3] = (w1 >> 8) as u8;
        self.mac[4] = (w2 & 0xFF) as u8;
        self.mac[5] = (w2 >> 8) as u8;
        info!(
            "[e1000e] MAC from NVM: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.mac[0], self.mac[1], self.mac[2], self.mac[3], self.mac[4], self.mac[5]
        );
    }

    unsafe fn toggle_lanphypc(&self) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        let mut fextnvm3 = mmio_read(self.base, E1000E_FEXTNVM3);
        fextnvm3 &= !0x3F; // PHY_CFG_COUNTER_MASK
        fextnvm3 |= 0x20; // 50 msec counter
        mmio_write(self.base, E1000E_FEXTNVM3, fextnvm3);
        let _ = mmio_read(self.base, E1000E_FEXTNVM3); // flush posted write (M8)

        // Phase 1: assert OVERRIDE, deassert VALUE (drive LANPHYPC low)
        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl |= CTRL_LANPHYPC_OVERRIDE;
        ctrl &= !CTRL_LANPHYPC_VALUE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL); // flush posted write
        Self::udelay(LANPHYPC_POWERDOWN_HOLD_US);

        // Phase 2: deassert OVERRIDE (release LANPHYPC back to hardware control).
        // Re-read CTRL here so we don't accidentally clear bits that were set
        // by the PCH between phase 1 and phase 2 (e.g. GIO_MASTER state).
        ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl &= !CTRL_LANPHYPC_OVERRIDE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL); // flush
        Self::udelay(LANPHYPC_POWERUP_SETTLE_US);

        let phy_addr = self.active_phy_addr();
        if !self.wait_phy_mdio_ready(phy_addr, 200_000) {
            crate::klog_warn!(
                "[e1000e] LANPHYPC: MDIO still silent after {} ms settle\n",
                LANPHYPC_POWERUP_SETTLE_US / 1000
            );
        }
    }

    unsafe fn disable_ulp_software(&self) {
        // Release PHY power-down before any MDIO paged access.
        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext &= !CTRL_EXT_PHYPDEN;
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
        let _ = mmio_read(self.base, E1000E_CTRL_EXT);

        let phy_addr = self.active_phy_addr();
        if self.mdic_read(phy_addr, MII_BMSR).is_none() {
            self.toggle_lanphypc();
        } else {
            crate::klog_info!("[e1000e] ULP: MDIO alive — skipping initial LANPHYPC pulse\n");
        }
        let cv_smb_ctrl = Self::phy_reg_paged(769, 23);
        if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, cv_smb_ctrl) {
            phy_reg &= !0x0001; // Clear CV_SMB_CTRL_FORCE_SMBUS
            self.mdic_write_phy(phy_addr, cv_smb_ctrl, phy_reg);
        } else {
            // MAC might be in PCIe mode. Force to SMBus mode in MAC:
            let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
            ctrl_ext |= 0x00000800; // E1000_CTRL_EXT_FORCE_SMBUS (bit 11)
            mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
            Self::udelay(50_000); // 50 ms

            if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, cv_smb_ctrl) {
                phy_reg &= !0x0001;
                self.mdic_write_phy(phy_addr, cv_smb_ctrl, phy_reg);
            }
        }

        // Unforce SMBus mode in MAC
        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext &= !0x00000800; // Clear E1000_CTRL_EXT_FORCE_SMBUS
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);

        // Re-Enable K1 in PHY:
        let hv_pm_ctrl = Self::phy_reg_paged(770, 17);
        if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, hv_pm_ctrl) {
            phy_reg |= 0x4000; // Set HV_PM_CTRL_K1_ENABLE
            self.mdic_write_phy(phy_addr, hv_pm_ctrl, phy_reg);
        }

        // Clear ULP enabled configuration
        let i218_ulp_config1 = Self::phy_reg_paged(779, 16);
        if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, i218_ulp_config1) {
            phy_reg &= !0x1D74;
            self.mdic_write_phy(phy_addr, i218_ulp_config1, phy_reg);

            // Commit ULP changes by starting auto ULP configuration:
            phy_reg |= 0x0001; // I218_ULP_CONFIG1_START
            self.mdic_write_phy(phy_addr, i218_ulp_config1, phy_reg);
        }

        // Clear Disable SMBus Release on PERST# in MAC:
        let mut fextnvm7 = mmio_read(self.base, E1000E_FEXTNVM7);
        fextnvm7 &= !FEXTNVM7_DISABLE_SMB_PERST;
        mmio_write(self.base, E1000E_FEXTNVM7, fextnvm7);

        // Soft reset the PHY
        if let Some(bmcr) = self.mdic_read(phy_addr, MII_BMCR) {
            self.mdic_write(phy_addr, MII_BMCR, bmcr | 0x8000); // Set BMCR_RESET
            Self::udelay(50_000);
        }

        // If ME left MDIO dead, pulse LANPHYPC again (common after warm boot / Windows).
        if self.mdic_read(phy_addr, MII_BMSR).is_none() {
            crate::klog_warn!("[e1000e] ULP software: MDIO silent — retry toggle_lanphypc\n");
            self.toggle_lanphypc();
        }

        match self.mdic_read(phy_addr, MII_BMSR) {
            Some(bmsr) => crate::klog_info!("[e1000e] ULP software: PHY BMSR={:#x}\n", bmsr),
            None => crate::klog_warn!(
                "[e1000e] ULP software: PHY still not responding — check cable LEDs / ME lock\n"
            ),
        }
    }

    unsafe fn disable_ulp(&self) {
        if !self.is_pch_lpt_or_later() {
            return;
        }

        let fwsm = mmio_read(self.base, E1000E_FWSM);
        if (fwsm & 0x8000) != 0 {
            // Intel ME firmware is active: request ME to unconfigure ULP
            let mut h2me = mmio_read(self.base, E1000E_H2ME);
            h2me &= !(1 << 11); // Clear E1000_H2ME_ULP (bit 11)
            h2me |= 1 << 12;    // Set E1000_H2ME_ENFORCE_SETTINGS (bit 12)
            mmio_write(self.base, E1000E_H2ME, h2me);
            let _ = mmio_read(self.base, E1000E_H2ME);

            // Poll up to 2.5 seconds (250 ticks of 10ms) for ME to clear ULP_CFG_DONE (FWSM bit 10, 0x400)
            let mut i = 0;
            let mut me_ok = false;
            while (mmio_read(self.base, E1000E_FWSM) & 0x0400) != 0 {
                if i >= 250 {
                    crate::klog_warn!(
                        "[e1000e] ULP ME timeout (2.5s) — ME may be locked; running software ULP path\n"
                    );
                    break;
                }
                i += 1;
                Self::udelay(10_000);
            }
            me_ok = i < 250;

            // Clear ENFORCE_SETTINGS
            let mut h2me = mmio_read(self.base, E1000E_H2ME);
            h2me &= !(1 << 12); // Clear E1000_H2ME_ENFORCE_SETTINGS
            mmio_write(self.base, E1000E_H2ME, h2me);
            let _ = mmio_read(self.base, E1000E_H2ME);

            if me_ok {
                crate::klog_info!("[e1000e] ULP disabled via Intel ME in {} ms\n", i * 10);
            } else {
                self.disable_ulp_software();
            }
        } else {
            // No ME firmware active: software ULP disable path
            self.disable_ulp_software();
        }
    }

    // -----------------------------------------------------------------------
    // Flush descriptor rings (I219 workaround)
    // -----------------------------------------------------------------------
    unsafe fn flush_desc_rings(&self) {
        // Only SPT (I219) and later require this.
        if !self.is_pch_spt_or_later() {
            return;
        }

        // Check if flush is required via PCI config space.
        let hang_state = PCI_ACCESS.read16(&PortOpsImpl, self.pci_loc, PCICFG_DESC_RING_STATUS);
        if (hang_state & FLUSH_DESC_REQUIRED) == 0 {
            return;
        }

        warn!("[e1000e] I219 pre-reset flush (state={:#x}): setting FEXTNVM bits only", hang_state);

        // SAFE path: only write FEXTNVM7/FEXTNVM11 status bits.
        // DO NOT enable TX/RX DMA here — the NIC is still in its BIOS-handed
        // state and activating the DMA engine before CTRL_RST can trigger a
        // PCIe fatal error (completion timeout / unsupported request) that
        // freezes the entire system, requiring a power cycle to recover.
        // The hardware reset (CTRL_RST, issued next) will clear the ring-hang
        // condition without needing us to pump dummy descriptors through DMA.
        let mut fextnvm7 = mmio_read(self.base, E1000E_FEXTNVM7);
        fextnvm7 |= FEXTNVM7_NEED_DESCR_RING_FLUSH;
        mmio_write(self.base, E1000E_FEXTNVM7, fextnvm7);
        let _ = mmio_read(self.base, E1000E_FEXTNVM7); // flush posted write

        let mut fextnvm11 = mmio_read(self.base, E1000E_FEXTNVM11);
        fextnvm11 |= FEXTNVM11_DISABLE_MULR_FIX;
        mmio_write(self.base, E1000E_FEXTNVM11, fextnvm11);
        let _ = mmio_read(self.base, E1000E_FEXTNVM11); // flush posted write
    }

    /// Push CPU-written DMA data to RAM for the device (WB cache fallback only).
    unsafe fn dma_wbinv_range(vaddr: usize, len: usize) {
        if len == 0 {
            return;
        }
        let mut p = vaddr & !63;
        let end = vaddr.saturating_add(len);
        while p < end {
            core::arch::x86_64::_mm_clflush(p as *const u8);
            p += 64;
        }
        core::arch::x86_64::_mm_mfence();
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::SeqCst);
    }

    /// Device → CPU ordering fence before reading NIC write-back fields.
    #[inline]
    unsafe fn dma_rmb_after_device() {
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::Acquire);
    }

    /// Drop stale CPU cache lines before reading device write-back (WB mapping only).
    unsafe fn invalidate_cpu_cache_for_read(vaddr: usize, len: usize) {
        if len == 0 {
            return;
        }
        core::arch::x86_64::_mm_mfence();
        let mut p = vaddr & !63;
        let end = vaddr.saturating_add(len);
        while p < end {
            core::arch::x86_64::_mm_clflush(p as *const u8);
            p += 64;
        }
        core::arch::x86_64::_mm_mfence();
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::SeqCst);
    }

    /// Single 64-bit load of the RX write-back region (+8 staterr, +12 length).
    /// lfence immediately before read_volatile drains load buffers after clflush.
    #[inline]
    unsafe fn read_rx_wb_u64(desc_addr: usize) -> u64 {
        core::arch::x86_64::_mm_lfence();
        read_volatile((desc_addr + 8) as *const u64)
    }

    /// Invalidate descriptor WB, fence load buffers, then read — no speculative window.
    unsafe fn read_rx_wb_after_sync(&self, desc_addr: usize) -> u64 {
        if self.rx_needs_cache_flush() {
            Self::invalidate_cpu_cache_for_read(desc_addr, 16);
        } else if !self.dma_uncached {
            core::arch::x86_64::_mm_mfence();
            core::arch::x86_64::_mm_lfence();
            fence(Ordering::Acquire);
        } else {
            Self::dma_rmb_after_device();
        }
        Self::read_rx_wb_u64(desc_addr)
    }

    #[inline]
    unsafe fn parse_rx_wb_ext_u64(wb: u64) -> Option<(u32, usize)> {
        let staterr = wb as u32;
        if staterr & RXD_EXT_DD == 0 {
            return None;
        }
        let len = (wb >> 32) as u16 as usize;
        Some((staterr, len))
    }

    /// Legacy RX write-back: len @ +8, DD status @ +12 — one u64 covers both.
    #[inline]
    unsafe fn parse_rx_wb_legacy_u64(wb: u64) -> Option<(u32, usize)> {
        let len = wb as u16 as usize;
        let status = (wb >> 32) as u8;
        if status & 0x01 == 0 {
            return None;
        }
        Some((status as u32, len))
    }

    /// Read a range the device wrote into WB memory.
    unsafe fn dma_copy_in(dst: &mut Vec<u8>, vaddr: usize, len: usize) {
        dst.clear();
        dst.reserve(len);
        core::arch::x86_64::_mm_lfence();
        for i in 0..len {
            dst.push(core::ptr::read_volatile((vaddr + i) as *const u8));
        }
    }

    /// RX payload after device DMA — invalidate only when rings stay WB-mapped.
    unsafe fn dma_copy_in_rx_buffer(&self, dst: &mut Vec<u8>, vaddr: usize, len: usize) {
        if self.rx_needs_cache_flush() {
            Self::invalidate_cpu_cache_for_read(vaddr, len);
        } else {
            Self::dma_rmb_after_device();
        }
        Self::dma_copy_in(dst, vaddr, len);
    }

    unsafe fn wait_rx_dma_quiescent(&mut self) {
        if self.is_pch_spt_or_later() {
            let mut rxd = mmio_read(self.base, E1000E_RXDCTL);
            if rxd & RXDCTL_QUEUE_ENABLE != 0 {
                rxd &= !RXDCTL_QUEUE_ENABLE;
                mmio_write(self.base, E1000E_RXDCTL, rxd);
                let _ = mmio_read(self.base, E1000E_RXDCTL);
                for _ in 0..50 {
                    if mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE == 0 {
                        break;
                    }
                    Self::udelay(10);
                }
            }
        }
        // Mandatory on real silicon — do not touch descriptor RAM before this elapses.
        Self::udelay(10);
        Self::udelay(RX_DMA_DRAIN_US as u64);
        for _ in 0..10 {
            let rdh = mmio_read(self.base, E1000E_RDH);
            let rdt = mmio_read(self.base, E1000E_RDT);
            if rdh == rdt {
                break;
            }
            Self::udelay(10);
        }
    }

    fn rx_sg_reset(&mut self) {
        self.rx_sg_buf.clear();
        self.rx_sg_frag_count = 0;
    }

    fn rx_sg_append(&mut self, frag: &[u8]) -> bool {
        if self.rx_sg_frag_count >= RX_SG_MAX_FRAGS
            || self.rx_sg_buf.len().saturating_add(frag.len()) > RX_SG_MAX_BYTES
        {
            log::warn!(
                "[e1000e] RX SG overflow ({} frags, {} B) — reset",
                self.rx_sg_frag_count,
                self.rx_sg_buf.len()
            );
            self.rx_sg_reset();
            return false;
        }
        self.rx_sg_frag_count = self.rx_sg_frag_count.saturating_add(1);
        self.rx_sg_buf.extend_from_slice(frag);
        true
    }

    unsafe fn stop_rx_tx_engines(&self) {
        let rctl = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl & !RCTL_EN);
        let tctl = mmio_read(self.base, E1000E_TCTL);
        mmio_write(self.base, E1000E_TCTL, tctl & !TCTL_EN);
        Self::udelay(100);
    }

    /// Linux `e1000_acquire_swflag_ich8lan`: software PHY/MDIO ownership.
    unsafe fn pch_swflag_acquire(&self) -> bool {
        for _ in 0..200 {
            let v = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if v & EXTCNF_CTRL_SWFLAG == 0 {
                break;
            }
            Self::udelay(1_000);
        }
        let mut v = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if v & EXTCNF_CTRL_SWFLAG != 0 {
            warn!("[e1000e] EXTCNF_CTRL SWFLAG held by FW/HW");
            return false;
        }
        v |= EXTCNF_CTRL_SWFLAG;
        mmio_write(self.base, E1000E_EXTCNF_CTRL, v);
        for _ in 0..200 {
            let r = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if r & EXTCNF_CTRL_SWFLAG != 0 {
                return true;
            }
            Self::udelay(1_000);
        }
        warn!("[e1000e] failed to set EXTCNF_CTRL SWFLAG");
        let r = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        mmio_write(
            self.base,
            E1000E_EXTCNF_CTRL,
            r & !EXTCNF_CTRL_SWFLAG,
        );
        false
    }

    unsafe fn pch_swflag_release(&self) {
        let mut v = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if v & EXTCNF_CTRL_SWFLAG != 0 {
            v &= !EXTCNF_CTRL_SWFLAG;
            mmio_write(self.base, E1000E_EXTCNF_CTRL, v);
            let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        }
    }

    /// Linux `e1000_get_cfg_done_ich8lan` / `e1000_lan_init_done_ich8lan` after PHY_RST.
    unsafe fn pch_phy_reset_complete(&self) {
        Self::udelay(10_000);
        let mut loops = 1500u32;
        while loops > 0 {
            let s = mmio_read(self.base, E1000E_STATUS);
            if s & STATUS_LAN_INIT_DONE != 0 {
                break;
            }
            Self::udelay(150);
            loops -= 1;
        }
        if loops == 0 {
            warn!("[e1000e] STATUS.LAN_INIT_DONE timeout after PHY_RST");
        }
        let mut s = mmio_read(self.base, E1000E_STATUS);
        if s & STATUS_LAN_INIT_DONE != 0 {
            mmio_write(
                self.base,
                E1000E_STATUS,
                s & !STATUS_LAN_INIT_DONE,
            );
            let _ = mmio_read(self.base, E1000E_STATUS);
        }
        s = mmio_read(self.base, E1000E_STATUS);
        if s & STATUS_PHYRA != 0 {
            mmio_write(
                self.base,
                E1000E_STATUS,
                s & !STATUS_PHYRA,
            );
            let _ = mmio_read(self.base, E1000E_STATUS);
        }
    }

    unsafe fn pch_issue_phy_reset(&self) {
        let ext_saved = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        mmio_write(
            self.base,
            E1000E_EXTCNF_CTRL,
            ext_saved | EXTCNF_CTRL_GATE_PHY_CFG,
        );
        let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);

        if !self.pch_swflag_acquire() {
            mmio_write(self.base, E1000E_EXTCNF_CTRL, ext_saved);
            warn!("[e1000e] PCH: PHY_RST skipped (no SWFLAG)");
            return;
        }

        let ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(self.base, E1000E_CTRL, ctrl | CTRL_PHY_RST);
        let _ = mmio_read(self.base, E1000E_CTRL);
        Self::udelay(100);
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL);
        Self::udelay(300);

        self.pch_phy_reset_complete();

        self.pch_swflag_release();

        let ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        mmio_write(
            self.base,
            E1000E_EXTCNF_CTRL,
            ext & !EXTCNF_CTRL_GATE_PHY_CFG,
        );
        let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
    }

    /// Clear STATUS.PHYRA if set (Linux `e1000_get_cfg_done_ich8lan`). Safe when
    /// PHY_RST was skipped because firmware may leave this bit asserted.
    unsafe fn pch_clear_status_phyra_if_set(&self) {
        let s = mmio_read(self.base, E1000E_STATUS);
        if s & STATUS_PHYRA != 0 {
            warn!("[e1000e] clearing STATUS.PHYRA (status was {:#x})", s);
            mmio_write(self.base, E1000E_STATUS, s & !STATUS_PHYRA);
            let _ = mmio_read(self.base, E1000E_STATUS);
        }
    }

    /// MDIO read; caller must already hold SWFLAG on PCH.
    unsafe fn mdic_read_swheld(&self, phy_addr: u8, reg: u32) -> Option<u16> {
        let cmd =
            (reg << MDIC_REG_SHIFT) | ((phy_addr as u32) << MDIC_PHY_SHIFT) | MDIC_OP_READ;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..400 {
            Self::udelay(50);
            let mdic = mmio_read(self.base, E1000E_MDIC);
            if mdic & MDIC_READY != 0 {
                if mdic & MDIC_ERROR == 0 {
                    return Some((mdic & 0xFFFF) as u16);
                }
                return None;
            }
        }
        None
    }

    /// MDIO write; caller must already hold SWFLAG on PCH.
    unsafe fn mdic_write_swheld(&self, phy_addr: u8, reg: u32, val: u16) -> bool {
        let cmd = (val as u32)
            | (reg << MDIC_REG_SHIFT)
            | ((phy_addr as u32) << MDIC_PHY_SHIFT)
            | MDIC_OP_WRITE;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..400 {
            Self::udelay(50);
            let mdic = mmio_read(self.base, E1000E_MDIC);
            if mdic & MDIC_READY != 0 {
                return (mdic & MDIC_ERROR) == 0;
            }
        }
        false
    }

    unsafe fn mdic_read(&self, phy_addr: u8, reg: u32) -> Option<u16> {
        let is_pch = self.is_pch_lpt_or_later();
        if is_pch && !self.pch_swflag_acquire() {
            return None;
        }

        let res = self.mdic_read_swheld(phy_addr, reg);

        if is_pch {
            self.pch_swflag_release();
        }
        res
    }

    /// Paged PHY access (Linux `__e1000e_read_phy_reg_igp`).
    unsafe fn mdic_read_phy(&self, phy_addr: u8, offset: u32) -> Option<u16> {
        if offset > MAX_PHY_MULTI_PAGE_REG {
            if !self.mdic_write(phy_addr, IGP_PHY_PAGE_SELECT, offset as u16) {
                return None;
            }
        }
        self.mdic_read(phy_addr, offset & 0x1F)
    }

    unsafe fn mdic_write_phy(&self, phy_addr: u8, offset: u32, val: u16) -> bool {
        if offset > MAX_PHY_MULTI_PAGE_REG {
            if !self.mdic_write(phy_addr, IGP_PHY_PAGE_SELECT, offset as u16) {
                return false;
            }
        }
        self.mdic_write(phy_addr, offset & 0x1F, val)
    }

    unsafe fn mdic_write(&self, phy_addr: u8, reg: u32, val: u16) -> bool {
        let is_pch = self.is_pch_lpt_or_later();
        if is_pch && !self.pch_swflag_acquire() {
            return false;
        }

        let ok = self.mdic_write_swheld(phy_addr, reg, val);

        if is_pch {
            self.pch_swflag_release();
        }
        ok
    }

    unsafe fn phy_bmsr_link_up(&self, phy_addr: u8) -> bool {
        self.mdic_read(phy_addr, MII_BMSR)
            .map(|b| b != 0 && b != 0xFFFF && (b & 0x0004) != 0)
            .unwrap_or(false)
    }

    /// Decode reg26 / `HV_M_STATUS` for logs (I219 PHY status).
    fn hv_m_status_label(st2: u16) -> &'static str {
        let spd = match st2 & PHY_STATUS2_SPEED_MASK {
            PHY_STATUS2_SPEED_1000 => "1000",
            PHY_STATUS2_SPEED_100 => "100",
            0 => "10",
            _ => "speed-conflict",
        };
        if st2 & 0x0040 == 0 {
            return "down";
        }
        if st2 & 0x1000 == 0 {
            return "link-no-an";
        }
        spd
    }

    /// Speed/duplex from cached PHY reg 26 (Linux HV_M_STATUS / I82577 one-hot 0x300).
    fn phy_resolve_speed_duplex_st2(st2: u16) -> Option<(u32, u32)> {
        if st2 == 0 || st2 == 0xFFFF {
            return None;
        }
        let bits = st2 & PHY_STATUS2_SPEED_MASK;
        if bits == PHY_STATUS2_SPEED_1000 {
            return Some((2, 1));
        }
        if bits == PHY_STATUS2_SPEED_100 {
            return Some((1, 1));
        }
        if bits == 0 {
            return Some((0, 1));
        }
        None
    }

    /// Speed/duplex from PHY reg 26, then PSS reg 17, else 1000/full.
    unsafe fn phy_resolve_speed_duplex(&self, phy_addr: u8) -> (u32, u32) {
        for _ in 0..3 {
            if let Some(st2) = self.mdic_read(phy_addr, MII_PHY_STATUS_2) {
                if let Some(sd) = Self::phy_resolve_speed_duplex_st2(st2) {
                    return sd;
                }
            }
            Self::udelay(200);
        }
        let _ = self.mdic_read(phy_addr, 17);
        Self::udelay(500);
        if let Some(pss) = self.mdic_read(phy_addr, 17) {
            if pss != 0 && pss != 0xFFFF {
                return (((pss >> 14) & 0x3) as u32, ((pss >> 13) & 0x1) as u32);
            }
        }
        (2, 1)
    }

    /// Brief FRCSPD+SPD_BYPS pulse then restore (Linux `e1000_configure_k1_ich8lan`).
    unsafe fn mac_speed_sync_pulse(&self) {
        let ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        let ctrl_saved = mmio_read(self.base, E1000E_CTRL);
        let mut pulse = ctrl_saved & !(CTRL_SPD_1000 | CTRL_SPD_100);
        pulse |= CTRL_FRCSPD;
        mmio_write(self.base, E1000E_CTRL, pulse);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext | CTRL_EXT_SPD_BYPS);
        Self::udelay(40);
        mmio_write(self.base, E1000E_CTRL, ctrl_saved);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
        let _ = mmio_read(self.base, E1000E_CTRL);
    }

    fn phy_speed_label(speed: u32) -> &'static str {
        match speed {
            2 => "1000",
            1 => "100",
            _ => "10",
        }
    }

    #[inline]
    fn phy_reg_paged(page: u32, reg: u32) -> u32 {
        (page << 5) | reg
    }

    /// Linux `e1000e_get_speed_and_duplex_copper` (STATUS bits 6:7 + FD).
    fn speed_mbps_from_status(status: u32) -> u32 {
        if status & STATUS_SPEED_1000 != 0 {
            SPEED_1000
        } else if status & STATUS_SPEED_100 != 0 {
            SPEED_100
        } else {
            SPEED_10
        }
    }

    fn speed_mbps_from_phy_st2(st2: u16) -> u32 {
        match Self::phy_resolve_speed_duplex_st2(st2) {
            Some((2, _)) => SPEED_1000,
            Some((1, _)) => SPEED_100,
            Some((_, _)) => SPEED_10,
            None => SPEED_10,
        }
    }

    fn speed_idx_from_status(status: u32) -> u32 {
        if status & STATUS_SPEED_1000 != 0 {
            2
        } else if status & STATUS_SPEED_100 != 0 {
            1
        } else {
            0
        }
    }

    unsafe fn active_phy_addr(&self) -> u8 {
        for pa in [self.phy_addr, 1u8, 2u8] {
            if self.phy_bmsr_link_up(pa) {
                return pa;
            }
        }
        self.phy_addr
    }

    /// Program CTRL speed bits without FRCSPD (I219: avoid locking MAC at stale 10M).
    unsafe fn mac_sync_ctrl_speed_mbps(&self, speed_mbps: u32, duplex_full: bool) {
        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl &= !(CTRL_FRCSPD | CTRL_FRCDPX | CTRL_SPD_1000 | CTRL_SPD_100 | CTRL_FD | CTRL_ASDE);
        ctrl |= CTRL_SLU;
        if speed_mbps == SPEED_1000 {
            ctrl |= CTRL_SPD_1000 | CTRL_ASDE;
        } else {
            ctrl |= CTRL_FRCSPD | CTRL_FRCDPX;
            if speed_mbps == SPEED_100 {
                ctrl |= CTRL_SPD_100;
            }
        }
        if duplex_full {
            ctrl |= CTRL_FD;
        }
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL);

        self.mac_speed_sync_pulse();

        e1000e_vlog!(
            "[e1000e] CTRL -> {} Mb/s {} duplex (FRC={}) CTRL={:#x}\n",
            speed_mbps,
            if duplex_full { "full" } else { "half" },
            speed_mbps < 1000,
            ctrl
        );
    }

    /// Reg17 resolved speed when `BM_CS_STATUS_RESOLVED` is set.
    unsafe fn phy_cs17_resolved(&self, phy_addr: u8) -> Option<(u32, bool)> {
        let pss = self.mdic_read(phy_addr, BM_CS_STATUS)?;
        if pss == 0 || pss == 0xFFFF {
            return None;
        }
        if pss & (BM_CS_STATUS_LINK_UP | BM_CS_STATUS_RESOLVED)
            != (BM_CS_STATUS_LINK_UP | BM_CS_STATUS_RESOLVED)
        {
            return None;
        }
        let speed = match (pss >> 14) & 0x3 {
            2 => SPEED_1000,
            1 => SPEED_100,
            _ => SPEED_10,
        };
        let duplex_full = (pss >> 13) & 1 != 0;
        Some((speed, duplex_full))
    }

    /// Resolved link rate: PHY reg26, or reg17 when BM_CS_STATUS_RESOLVED (not MII HCD).
    unsafe fn phy_operational_speed(&self, phy_addr: u8, st2: u16) -> (u32, bool, &'static str) {
        let mut speed = Self::speed_mbps_from_phy_st2(st2);
        let mut duplex_full = Self::phy_resolve_speed_duplex_st2(st2)
            .map(|(_, d)| d != 0)
            .unwrap_or(true);
        let mut src = "reg26";

        if let Some((cs, cs_fd)) = self.phy_cs17_resolved(phy_addr) {
            if cs > speed {
                speed = cs;
                duplex_full = cs_fd;
                src = "reg17";
            }
        }

        // On real hardware, some discrete cards or integrated PHYs do not support/report
        // resolved speed on PHY reg26 (MII_PHY_STATUS_2) or reg17, returning 0 (SPEED_10).
        // Therefore, if the MAC STATUS register (which is hardware-updated when Link Up is set)
        // reports a higher speed, we must trust it and override the PHY-resolved speed.
        // Otherwise, forcing the MAC to 10 Mbps disables auto-speed detection and locks the NIC at 10 Mbps.
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU != 0 {
            let status_speed = Self::speed_mbps_from_status(status);
            let status_fd = status & STATUS_FD != 0;
            if status_speed > speed {
                speed = status_speed;
                duplex_full = status_fd;
                src = "STATUS(override)";
            } else if speed == SPEED_10 && status_speed == SPEED_10 {
                // PHY and STATUS agree on 10M: use STATUS for duplex sense.
                duplex_full = status_fd;
                src = if self.link_10m_degraded {
                    "reg26+STATUS(10m-deg)"
                } else {
                    "reg26+STATUS"
                };
            }
        }

        (speed, duplex_full, src)
    }

    /// Intel `e1000e_get_speed_and_duplex_copper` + I219 reg26/reg17 when ahead of STATUS.
    unsafe fn resolve_link_speed_duplex_linux(
        &self,
        phy_addr: u8,
        st2: u16,
    ) -> (u32, bool, &'static str) {
        let status = mmio_read(self.base, E1000E_STATUS);
        let mut speed = Self::speed_mbps_from_status(status);
        let mut duplex_full = status & STATUS_FD != 0;
        let mut src = "STATUS";

        if self.is_pch_lpt_or_later() && st2 & PHY_STATUS2_AUTONEG_DONE != 0 {
            let reg26_spd = Self::speed_mbps_from_phy_st2(st2);
            if reg26_spd > speed {
                speed = reg26_spd;
                duplex_full = Self::phy_resolve_speed_duplex_st2(st2)
                    .map(|(_, d)| d != 0)
                    .unwrap_or(duplex_full);
                src = "reg26";
            }
            if let Some((cs, cs_fd)) = self.phy_cs17_resolved(phy_addr) {
                if cs > speed {
                    speed = cs;
                    duplex_full = cs_fd;
                    src = "reg17";
                }
            }
        }

        (speed, duplex_full, src)
    }

    /// Linux autoneg link-up: SLU+ASDE only — do not FRCSPD at 10/100 (locks I219 at 10M).
    unsafe fn mac_apply_link_up_autoneg(&self) {
        self.mac_setup_copper_link_linux();
        self.mac_speed_sync_pulse();
    }

    /// Linux `e1000_wait_autoneg` — poll BMSR until ANEG complete or timeout.
    unsafe fn phy_wait_autoneg_linux(&self, phy_addr: u8) -> bool {
        let mut i = PHY_AUTO_NEG_LIMIT;
        while i > 0 {
            let _ = self.mdic_read(phy_addr, MII_BMSR);
            let bmsr = self.mdic_read(phy_addr, MII_BMSR).unwrap_or(0);
            if bmsr & BMSR_ANEG_COMPLETE != 0 {
                return true;
            }
            Self::udelay(100_000);
            i -= 1;
        }
        false
    }

    /// Last resort when both PHY reg26 and MAC STATUS say 10M but MII HCD is higher.
    /// Retry full copper autoneg once; do not force 10-only advertisement.
    unsafe fn phy_accept_10m_degraded_mode(&mut self, phy_addr: u8) {
        if self.link_10m_degraded {
            return;
        }
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU != 0 && Self::speed_mbps_from_status(status) > SPEED_10 {
            e1000e_vlog!(
                "[e1000e] PHY{} reg26=10M but STATUS={} Mb/s — skip 10-only autoneg\n",
                phy_addr,
                Self::speed_mbps_from_status(status)
            );
            return;
        }
        self.link_10m_degraded = true;
        crate::klog_warn!(
            "[e1000e] PHY{} reg26/STATUS at 10 Mb/s while MII HCD is higher — retry full autoneg\n",
            phy_addr
        );
        let _ = self.phy_copper_autoneg_restart_adv(
            phy_addr,
            ADVERTISE_ALL_COPPER,
            Self::ctrl1000_for_ms(ADVERTISE_1000FULL, None),
        );
        let _ = self.phy_wait_reg26_settled(phy_addr, 4000);
    }

    /// Set negotiated speed in CTRL from PHY reg 26 and resolved duplex.
    unsafe fn mac_sync_ctrl_speed_from_st2(&self, st2: u16, duplex_full: bool) -> bool {
        let Some((speed_idx, duplex)) = Self::phy_resolve_speed_duplex_st2(st2) else {
            return false;
        };
        let speed_mbps = match speed_idx {
            2 => SPEED_1000,
            1 => SPEED_100,
            _ => SPEED_10,
        };
        let fd = if speed_mbps == SPEED_10 {
            duplex_full
        } else {
            duplex != 0
        };
        self.mac_sync_ctrl_speed_mbps(speed_mbps, fd);
        true
    }

    /// Lock MAC speed/duplex to PHY reg26 (I219 when STATUS speed bits lie).
    unsafe fn mac_lock_ctrl_from_st2(&self, st2: u16) -> bool {
        let Some((speed, duplex)) = Self::phy_resolve_speed_duplex_st2(st2) else {
            return false;
        };
        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl &= !(CTRL_FRCSPD | CTRL_FRCDPX | CTRL_SPD_1000 | CTRL_SPD_100 | CTRL_FD);
        ctrl |= CTRL_SLU | CTRL_ASDE | CTRL_FRCSPD | CTRL_FRCDPX;
        if speed == 2 {
            ctrl |= CTRL_SPD_1000;
        } else if speed == 1 {
            ctrl |= CTRL_SPD_100;
        }
        if duplex != 0 {
            ctrl |= CTRL_FD;
        }
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL);
        self.mac_speed_sync_pulse();
        e1000e_vlog!(
            "[e1000e] CTRL locked from reg26={:#x} ({}) -> {} Mb/s FD={} CTRL={:#x}\n",
            st2,
            Self::hv_m_status_label(st2),
            Self::phy_speed_label(speed),
            duplex != 0,
            ctrl
        );
        true
    }

    /// Read PHY reg26 twice; return value if stable and link-up bit set.
    unsafe fn phy_reg26_stable(&self, phy_addr: u8) -> Option<u16> {
        let a = self.mdic_read(phy_addr, MII_PHY_STATUS_2)?;
        Self::udelay(20_000);
        let b = self.mdic_read(phy_addr, MII_PHY_STATUS_2)?;
        if a == b
            && a != 0
            && a != 0xFFFF
            && a & PHY_STATUS2_LINK_UP != 0
        {
            Some(a)
        } else {
            None
        }
    }

    /// Wait for autoneg to finish; prefer the highest stable speed (1000 > 100 > 10).
    /// Do not stop at the first transient 100M while the partner is still ramping to 1G.
    unsafe fn phy_wait_reg26_settled(&self, phy_addr: u8, max_ms: u32) -> u16 {
        let steps = (max_ms as u64 * 1000 / 50_000).max(1);
        let mut best = self.mdic_read(phy_addr, MII_PHY_STATUS_2).unwrap_or(0);
        let mut best_rank = Self::speed_mbps_from_phy_st2(best);
        for _ in 0..steps {
            if let Some(s) = self.phy_reg26_stable(phy_addr) {
                if s & PHY_STATUS2_AUTONEG_DONE != 0
                    && Self::phy_resolve_speed_duplex_st2(s).is_some()
                {
                    let rank = Self::speed_mbps_from_phy_st2(s);
                    if rank > best_rank {
                        best = s;
                        best_rank = rank;
                    }
                    if rank == SPEED_1000 {
                        return s;
                    }
                }
            }
            Self::udelay(50_000);
        }
        if let Some(s) = self.phy_reg26_stable(phy_addr) {
            let rank = Self::speed_mbps_from_phy_st2(s);
            if rank >= best_rank && s & PHY_STATUS2_AUTONEG_DONE != 0 {
                best = s;
            }
        }
        best
    }

    /// Linux `e1000_phy_setup_autoneg` + restart BMCR.
    unsafe fn phy_copper_autoneg_restart_adv(
        &self,
        phy_addr: u8,
        anar: u16,
        ctrl1000: u16,
    ) -> bool {
        if !self.mdic_write(phy_addr, MII_ADVERTISE, anar) {
            return false;
        }
        if !self.mdic_write(phy_addr, MII_CTRL1000, ctrl1000) {
            return false;
        }
        let Some(bmcr) = self.mdic_read(phy_addr, MII_BMCR) else {
            return false;
        };
        if bmcr == 0 || bmcr == 0xFFFF {
            return false;
        }
        if !self.mdic_write(phy_addr, MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART) {
            return false;
        }
        let anar_rd = self.mdic_read(phy_addr, MII_ADVERTISE).unwrap_or(0);
        let c1000_rd = self.mdic_read(phy_addr, MII_CTRL1000).unwrap_or(0);
        e1000e_vlog!(
            "[e1000e] PHY{} autoneg restart ANAR={:#x} CTRL1000={:#x}\n",
            phy_addr,
            anar_rd,
            c1000_rd
        );
        true
    }

    fn ctrl1000_for_ms(advertise_1000: u16, ms: Option<bool>) -> u16 {
        let mut c = advertise_1000;
        match ms {
            None => c &= !(CTL1000_ENABLE_MASTER | CTL1000_AS_MASTER),
            Some(true) => c |= CTL1000_ENABLE_MASTER | CTL1000_AS_MASTER,
            Some(false) => {
                c |= CTL1000_ENABLE_MASTER;
                c &= !CTL1000_AS_MASTER;
            }
        }
        c
    }

    unsafe fn phy_copper_autoneg_restart(&self, phy_addr: u8) -> bool {
        self.phy_copper_autoneg_restart_adv(
            phy_addr,
            ADVERTISE_ALL_COPPER,
            Self::ctrl1000_for_ms(ADVERTISE_1000FULL, None),
        )
    }

    unsafe fn phy_reg26_link_up(st2: u16) -> bool {
        st2 != 0 && st2 != 0xFFFF && st2 & PHY_STATUS2_LINK_UP != 0
    }

    unsafe fn phy_disable_downshift(&self, phy_addr: u8) {
        if let Some(mut cfg) = self.mdic_read(phy_addr, I82577_CFG_REG) {
            if cfg != 0 && cfg != 0xFFFF && cfg & I82577_CFG_ENABLE_DOWNSHIFT != 0 {
                cfg &= !I82577_CFG_ENABLE_DOWNSHIFT;
                let _ = self.mdic_write(phy_addr, I82577_CFG_REG, cfg);
                e1000e_vlog!("[e1000e] PHY{} downshift disabled (cfg reg22)\n", phy_addr);
            }
        }
    }

    unsafe fn phy_log_partner_abilities(&self, phy_addr: u8) {
        if !E1000E_LOG_VERBOSE {
            return;
        }
        let bmsr = self.mdic_read(phy_addr, MII_BMSR).unwrap_or(0);
        let anar = self.mdic_read(phy_addr, MII_ADVERTISE).unwrap_or(0);
        let lpa = self.mdic_read(phy_addr, MII_LPA).unwrap_or(0);
        let stat1000 = self.mdic_read(phy_addr, MII_STAT1000).unwrap_or(0);
        let ctrl1000 = self.mdic_read(phy_addr, MII_CTRL1000).unwrap_or(0);
        let cs17 = self.mdic_read(phy_addr, BM_CS_STATUS).unwrap_or(0);
        let gbe_lp = stat1000 & STAT1000_LP_1000FULL != 0;
        let hcd = Self::phy_mii_hcd_speed_mbps(anar, lpa, stat1000, ctrl1000);
        crate::klog_info!(
            "[e1000e] PHY{} BMSR={:#x} aneg_done={} link={} ANAR={:#x} LPA={:#x} STAT1000={:#x} CS17={:#x} lp_1g={} MII_HCD={} Mb/s\n",
            phy_addr,
            bmsr,
            bmsr & BMSR_ANEG_COMPLETE != 0,
            bmsr & 0x0004 != 0,
            anar,
            lpa,
            stat1000,
            cs17,
            gbe_lp,
            hcd
        );
    }

    /// Highest common speed from IEEE 802.3 Clause 28/40 autoneg registers (not software).
    fn phy_mii_hcd_speed_mbps(anar: u16, lpa: u16, stat1000: u16, ctrl1000: u16) -> u32 {
        if (ctrl1000 & ADVERTISE_1000FULL) != 0 && (stat1000 & STAT1000_LP_1000FULL) != 0 {
            return SPEED_1000;
        }
        let tech = anar & lpa;
        if tech & (ADVERTISE_100FULL | ADVERTISE_100HALF) != 0 {
            return SPEED_100;
        }
        if tech & (ADVERTISE_10FULL | ADVERTISE_10HALF) != 0 {
            return SPEED_10;
        }
        SPEED_10
    }

    /// `reg26` resolved rate is below what ANAR∩LPA (and 1000BASE-T) allow — autoneg not finished.
    unsafe fn phy_reg26_below_mii_hcd(&self, phy_addr: u8, st2: u16) -> Option<u32> {
        if st2 & PHY_STATUS2_AUTONEG_DONE == 0 {
            return None;
        }
        let resolved = Self::speed_mbps_from_phy_st2(st2);
        let anar = self.mdic_read(phy_addr, MII_ADVERTISE).unwrap_or(0);
        let lpa = self.mdic_read(phy_addr, MII_LPA).unwrap_or(0);
        let stat1000 = self.mdic_read(phy_addr, MII_STAT1000).unwrap_or(0);
        let ctrl1000 = self.mdic_read(phy_addr, MII_CTRL1000).unwrap_or(0);
        let hcd = Self::phy_mii_hcd_speed_mbps(anar, lpa, stat1000, ctrl1000);
        if resolved < hcd {
            Some(hcd)
        } else {
            None
        }
    }

    /// Linux `set_d0_lplu_state(false)` + clear OEM GbE-disable (I219 real HW).
    unsafe fn pch_disable_lplu_gbe(&self) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        let phy_ctrl_before = mmio_read(self.base, E1000E_PHY_CTRL);
        let mut phy_ctrl = phy_ctrl_before;
        phy_ctrl &= !(
            PHY_CTRL_D0A_LPLU
                | PHY_CTRL_NOND0A_LPLU
                | PHY_CTRL_GBE_DISABLE
                | PHY_CTRL_NOND0A_GBE_DISABLE
        );
        if phy_ctrl != phy_ctrl_before {
            mmio_write(self.base, E1000E_PHY_CTRL, phy_ctrl);
            let _ = mmio_read(self.base, E1000E_PHY_CTRL);
        }
        for phy_addr in [self.phy_addr, 1u8, 2u8] {
            let Some(mut oem) = self.mdic_read_phy(phy_addr, HV_OEM_BITS_PHY) else {
                continue;
            };
            if oem == 0 || oem == 0xFFFF {
                continue;
            }
            let prev = oem;
            oem &= !(HV_OEM_BITS_LPLU | HV_OEM_BITS_GBE_DIS);
            if oem != prev {
                oem |= HV_OEM_BITS_RESTART_AN;
                let _ = self.mdic_write_phy(phy_addr, HV_OEM_BITS_PHY, oem);
            }
        }
        if phy_ctrl_before
            & (PHY_CTRL_D0A_LPLU
                | PHY_CTRL_NOND0A_LPLU
                | PHY_CTRL_GBE_DISABLE
                | PHY_CTRL_NOND0A_GBE_DISABLE)
            != 0
        {
            e1000e_vlog!(
                "[e1000e] cleared MAC PHY_CTRL LPLU/GBE-dis (was {:#x} now {:#x})\n",
                phy_ctrl_before,
                phy_ctrl
            );
        }
    }

    /// Linux `e1000e_config_collision_dist_generic` — refresh COLD after link speed change.
    unsafe fn config_collision_dist_linux(&self) {
        let mut tctl = mmio_read(self.base, E1000E_TCTL);
        tctl &= !0x003FF000;
        tctl |= TCTL_COLD_LINUX;
        mmio_write(self.base, E1000E_TCTL, tctl);
        let _ = mmio_read(self.base, E1000E_TCTL);
    }

    /// Linux netdev link-up: clear TARC speed-mode at 10/100M, set at 1000M (I219 TX errata).
    unsafe fn program_tarc_for_speed(&self, speed_mbps: u32) {
        if !self.is_pch_spt_or_later() {
            return;
        }
        let mut tarc0 = mmio_read(self.base, E1000E_TARC0);
        if speed_mbps == SPEED_1000 {
            tarc0 |= TARC0_SPEED_MODE;
        } else {
            tarc0 &= !TARC0_SPEED_MODE;
        }
        tarc0 &= !TARC0_CB_MULTIQ_3_REQ;
        tarc0 |= TARC0_CB_MULTIQ_2_REQ;
        mmio_write(self.base, E1000E_TARC0, tarc0);
        let verify = mmio_read(self.base, E1000E_TARC0);
        e1000e_vlog!(
            "[e1000e] TARC0={:#x} for {} Mb/s (speed_mode={})\n",
            verify,
            speed_mbps,
            if verify & TARC0_SPEED_MODE != 0 { "on" } else { "off" }
        );
    }

    /// Linux netdev link-up: program TARC while TCTL is off, then re-enable TCTL.
    unsafe fn program_tarc_with_tctl_gate(&self, speed_mbps: u32) {
        if !self.is_pch_spt_or_later() {
            self.program_tarc_for_speed(speed_mbps);
            return;
        }
        let mut tctl = mmio_read(self.base, E1000E_TCTL);
        let was_en = tctl & TCTL_EN != 0;
        if was_en {
            mmio_write(self.base, E1000E_TCTL, tctl & !TCTL_EN);
            Self::udelay(150);
        }
        self.program_tarc_for_speed(speed_mbps);
        if was_en {
            tctl = mmio_read(self.base, E1000E_TCTL);
            tctl |= TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT_LINUX | TCTL_COLD_LINUX;
            mmio_write(self.base, E1000E_TCTL, tctl);
            let _ = mmio_read(self.base, E1000E_TCTL);
        }
    }

    unsafe fn program_tarc_from_phy(&self, phy_addr: u8) {
        let st2 = self.mdic_read(phy_addr, MII_PHY_STATUS_2).unwrap_or(0);
        let speed = Self::speed_mbps_from_phy_st2(st2);
        if self.is_pch_spt_or_later() {
            self.program_tarc_with_tctl_gate(speed);
        } else {
            self.program_tarc_for_speed(speed);
        }
    }

    /// Linux `e1000_setup_copper_link_ich8lan` half-duplex preamble workaround (PCH2+).
    unsafe fn pch_kmrn_half_duplex_preamble(&self, phy_addr: u8, duplex_full: bool) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        const HV_KMRN_FIFO_CTRLSTA: u32 = (770 << 5) | 16;
        const PREAMBLE_MASK: u16 = 0x7000;
        const PREAMBLE_SHIFT: u16 = 12;
        if let Some(mut reg) = self.mdic_read_phy(phy_addr, HV_KMRN_FIFO_CTRLSTA) {
            reg &= !PREAMBLE_MASK;
            if !duplex_full {
                reg |= 1 << PREAMBLE_SHIFT;
            }
            let _ = self.mdic_write_phy(phy_addr, HV_KMRN_FIFO_CTRLSTA, reg);
        }
    }

    /// Linux `e1000_check_for_copper_link_ich8lan` TIPG + I217_RX_CONFIG EMI + PLL gate.
    /// Use PHY reg26 speed — I219 often leaves STATUS at 10M while PHY is at 100M.
    unsafe fn program_link_tipg_emi_linux(&self, phy_addr: u8, speed: u32, duplex_full: bool) {
        if !self.is_pch_lpt_or_later() {
            return;
        }

        let mut tipg = mmio_read(self.base, E1000E_TIPG);
        tipg &= !0x3FF;
        let emi_val = if !duplex_full && speed == SPEED_10 {
            tipg |= 0xFF;
            0u16
        } else if self.is_pch_spt_or_later() && duplex_full && speed != SPEED_1000 {
            tipg |= 0x0C;
            1u16
        } else {
            tipg |= 0x08;
            1u16
        };
        mmio_write(self.base, E1000E_TIPG, tipg);

        if !self.phy_write_emi(phy_addr, I217_RX_CONFIG_EMI, emi_val) {
            crate::klog_warn!(
                "[e1000e] I217_RX_CONFIG EMI={} failed PHY{}\n",
                emi_val,
                phy_addr
            );
        }

        if self.is_pch_lpt_or_later() {
            let pll_reg = Self::phy_reg_paged(772, 28);
            if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, pll_reg) {
                phy_reg &= !I217_PLL_CLOCK_GATE_MASK;
                if speed == SPEED_100 || speed == SPEED_10 {
                    phy_reg |= 0x3E8;
                } else {
                    phy_reg |= 0xFA;
                }
                let _ = self.mdic_write_phy(phy_addr, pll_reg, phy_reg);
            }
            if self.is_pch_spt_or_later() && speed == SPEED_1000 {
                if let Some(mut pm) = self.mdic_read_phy(phy_addr, HV_PM_CTRL) {
                    pm |= HV_PM_CTRL_K1_CLK_REQ;
                    let _ = self.mdic_write_phy(phy_addr, HV_PM_CTRL, pm);
                }
            }
        }
    }

    /// Linux `e1000e_set_pcie_no_snoop` for PCH2+ (clear GCR no-snoop bits).
    unsafe fn pch_setup_pcie_no_snoop(&self) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        let mut gcr = mmio_read(self.base, E1000E_GCR);
        gcr &= !GCR_PCIE_NO_SNOOP_ALL;
        mmio_write(self.base, E1000E_GCR, gcr);
        let _ = mmio_read(self.base, E1000E_GCR);
    }

    /// Linux `e1000_setup_copper_link_ich8lan` KMRN + inband parameters.
    unsafe fn pch_setup_kmrn_copper_link(&self) {
        self.kmrn_write(KMRNCTRLSTA_TIMEOUTS, 0xFFFF);
        let mut inband = self.kmrn_read(KMRNCTRLSTA_INBAND_PARAM);
        inband |= 0x3F;
        self.kmrn_write(KMRNCTRLSTA_INBAND_PARAM, inband);
    }

    /// Linux `e1000_setup_copper_link_ich8lan`: SLU, clear FRCSPD/FRCDPX only.
    unsafe fn mac_setup_copper_link_linux(&self) {
        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl |= CTRL_SLU | CTRL_ASDE | CTRL_FD;
        ctrl &= !(CTRL_FRCSPD | CTRL_FRCDPX);
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL);
    }

    /// After link up: STATUS/reg26 speed + TIPG/TARC (Intel check_for_copper_link_ich8lan).
    unsafe fn finish_copper_link_up(&self, phy_addr: u8, st2: u16) {
        let (phy_speed, duplex_full, src) = self.resolve_link_speed_duplex_linux(phy_addr, st2);
        e1000e_vlog!(
            "[e1000e] finish_copper_link_up reg26={:#x} -> {} Mb/s {} ({})",
            st2,
            phy_speed,
            if duplex_full { "FD" } else { "HD" },
            src
        );

        if self.is_pch_lpt_or_later() {
            self.mac_apply_link_up_autoneg();
        } else {
            self.mac_sync_ctrl_speed_mbps(phy_speed, duplex_full);
        }

        if self.is_pch_lpt_or_later() {
            self.program_link_tipg_emi_linux(phy_addr, phy_speed, duplex_full);
            self.pch_kmrn_half_duplex_preamble(phy_addr, duplex_full);
            self.program_tarc_with_tctl_gate(phy_speed);
            self.config_collision_dist_linux();
        }
    }

    /// Linux `e1000_copper_link_setup_82577` (CRS + MDI/MDIX). No downshift on I219
    /// metal — re-enabling it in post-link tune was pinning links at 100M.
    unsafe fn phy_setup_82577_copper(&self, phy_addr: u8) {
        if let Some(mut cfg) = self.mdic_read(phy_addr, I82577_CFG_REG) {
            if cfg != 0 && cfg != 0xFFFF {
                cfg |= I82577_CFG_ASSERT_CRS_ON_TX;
                cfg &= !I82577_CFG_ENABLE_DOWNSHIFT;
                let _ = self.mdic_write(phy_addr, I82577_CFG_REG, cfg);
            }
        }
        if let Some(mut ctrl2) = self.mdic_read(phy_addr, I82577_PHY_CTRL_2) {
            if ctrl2 != 0 && ctrl2 != 0xFFFF {
                ctrl2 &= !0x0600;
                ctrl2 |= I82577_PHY_CTRL2_AUTO_MDI_MDIX;
                let _ = self.mdic_write(phy_addr, I82577_PHY_CTRL_2, ctrl2);
            }
        }
    }

    /// Linux `e1000_desc_unused`.
    fn rx_desc_unused(&self) -> usize {
        if self.rx_next_to_clean > self.rx_next_to_use {
            self.rx_next_to_clean - self.rx_next_to_use - 1
        } else {
            NUM_RX + self.rx_next_to_clean - self.rx_next_to_use - 1
        }
    }

    /// Linux `e1000_alloc_rx_buffers` — post `count` descriptors; RDT doorbell is batched.
    unsafe fn post_one_rx_buffer(&mut self) -> bool {
        if self.rx_desc_unused() == 0 {
            return false;
        }
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let i = self.rx_next_to_use;
        let desc = &mut *ring.add(i);
        write_volatile(&mut desc.addr, self.rx_buf_paddr(i));
        write_volatile((desc as *mut RxDesc as usize + 8) as *mut u64, 0);
        if self.rx_needs_cache_flush() {
            Self::invalidate_cpu_cache_for_read(self.rx_buf_vaddr(i), BUF_SIZE);
            core::arch::x86_64::_mm_clflush(desc as *const RxDesc as *const u8);
        }
        self.rx_next_to_use = (i + 1) % NUM_RX;
        self.rx_post_since_doorbell = self.rx_post_since_doorbell.saturating_add(1);
        true
    }

    /// Ring the RDT doorbell when enough descriptors are posted or the ring is low.
    unsafe fn rx_doorbell_if_needed(&mut self, force: bool) {
        if self.rx_post_since_doorbell == 0 {
            return;
        }
        let unused = self.rx_desc_unused();
        let critical = unused <= RX_BUFFER_WRITE;
        if !force
            && self.rx_post_since_doorbell < RX_BUFFER_WRITE as u16
            && !critical
        {
            return;
        }
        let i = self.rx_next_to_use;
        let last = if i == 0 { NUM_RX - 1 } else { i - 1 };
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
        mmio_write(self.base, E1000E_RDT, last as u32);
        let _ = mmio_read(self.base, E1000E_RDT);
        self.rx_post_since_doorbell = 0;
    }

    unsafe fn flush_rx_post_queue(&mut self) {
        self.rx_doorbell_if_needed(true);
    }

    unsafe fn alloc_rx_buffers(&mut self, count: usize, force_doorbell: bool) {
        if count == 0 {
            return;
        }
        let mut posted = 0usize;
        while posted < count {
            if !self.post_one_rx_buffer() {
                break;
            }
            posted += 1;
        }
        self.rx_doorbell_if_needed(force_doorbell);
    }

    /// Linux `e1000_configure_rx` final step: RCTL with EN already set in setup_rctl.
    ///
    /// Correct silicon initialization sequence (Intel-mandated order):
    ///   1. Ensure the RX engine is disabled (RCTL.EN = 0) so the hardware is quiet.
    ///   2. Reset software descriptor indices (rx_next_to_clean/use = 0).
    ///   3. Write RDH = 0 and RDT = 0 while RCTL.EN is clear.
    ///   4. Re-post descriptors into the ring (reinit_rx_ring) — flushes via clflush
    ///      so physical RAM already contains valid buffer addresses and zero WB fields.
    ///   5. Configure RXDCTL burst parameters (no QUEUE_ENABLE yet).
    ///   6. Enable RXDCTL.QUEUE_ENABLE (PCH-SPT/later) and wait for it to latch.
    ///   7. Enable RCTL.EN — the DMA engine is now fully armed.
    ///   8. ONLY THEN advance RDT via alloc_rx_buffers. The shadow register inside
    ///      the silicon only latches the doorbell correctly once RCTL.EN + QUEUE_ENABLE
    ///      are both set. Writing RDT while the engine is off desynchronises the
    ///      internal shadow pointer from the MMIO-visible value, causing the chip to
    ///      wake up in an undefined state ("no valid descriptors" or instant overflow).
    unsafe fn arm_rx_unit_linux(&mut self) {
        // Step 1: Ensure RX engine is disabled; drain in-flight PCIe DMA before touching rings.
        let rctl = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl & !RCTL_EN);
        let _ = mmio_read(self.base, E1000E_RCTL);
        self.wait_rx_dma_quiescent();
        self.rx_sg_reset();
        self.rx_post_since_doorbell = 0;

        // Step 2: Reset indices.
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;

        // Step 3: Write RDH=0, RDT=0 while DMA engine is completely disabled.
        // Do NOT write a non-zero RDT here — doing so while RCTL.EN=0 desynchronises
        // the NIC's internal shadow register from the MMIO-visible value.
        mmio_write(self.base, E1000E_RDH, 0);
        let _ = mmio_read(self.base, E1000E_RDH);
        mmio_write(self.base, E1000E_RDT, 0);
        let _ = mmio_read(self.base, E1000E_RDT);

        // Step 4: Re-initialize and flush descriptors to RAM.
        // reinit_rx_ring writes buffer addresses + zeroes the WB fields for every
        // slot, then clflushes them into physical RAM. The RDT doorbell is NOT
        // touched here — the engine is still off.
        self.reinit_rx_ring();

        // Step 5: Configure RXDCTL parameters (DMA burst) without enabling the queue yet.
        let mut rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        rxdctl &= 0xFFFF_C000;
        rxdctl |= RXDCTL_DMA_BURST;
        mmio_write(self.base, E1000E_RXDCTL, rxdctl);
        let _ = mmio_read(self.base, E1000E_RXDCTL);

        // Step 6: Enable RXDCTL.QUEUE_ENABLE (PCH-SPT or later) and wait for it to latch
        // BEFORE raising RCTL.EN, so the DMA queue is fully enabled when EN fires.
        if self.is_pch_spt_or_later() {
            let mut rxd = mmio_read(self.base, E1000E_RXDCTL);
            rxd |= RXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_RXDCTL, rxd);
            let mut rxq_wait = 100;
            while rxq_wait > 0 && mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE == 0 {
                Self::udelay(100);
                rxq_wait -= 1;
            }
        }

        // Step 7: Enable RX engine (RCTL.EN). The MAC is now running.
        let rctl = self.rctl_rx_bits() | RCTL_EN;
        mmio_write(self.base, E1000E_RCTL, rctl);
        // Read back to flush the posted write and confirm the bit has latched
        // in silicon before we advance the RDT doorbell below.
        let _ = mmio_read(self.base, E1000E_RCTL);

        // Step 8: Post buffers conservatively — never doorbell RDT to NUM_RX-1 on first arm.
        let n = self.rx_desc_unused().min(RX_BOOT_POST_MAX);
        unsafe { self.alloc_rx_buffers(n, true) };

        self.kick_rx_writeback();
        self.verify_rx_engine();
        e1000e_vlog!(
            "[e1000e] RX ring armed clean={} use={} unused={} RDT={} RDH={}\n",
            self.rx_next_to_clean,
            self.rx_next_to_use,
            self.rx_desc_unused(),
            mmio_read(self.base, E1000E_RDT),
            mmio_read(self.base, E1000E_RDH)
        );
    }



    /// Re-sync MAC/TIPG if CTRL speed bits disagree with settled reg26 (I219).
    unsafe fn resync_mac_if_phy_changed(&self, phy_addr: u8) -> bool {
        let Some(st2) = self.phy_reg26_stable(phy_addr) else {
            return false;
        };
        if st2 & PHY_STATUS2_AUTONEG_DONE == 0 {
            return false;
        }
        if st2 & PHY_STATUS2_SPEED_MASK == 0 {
            return false;
        }
        let ctrl = mmio_read(self.base, E1000E_CTRL);
        let mac_spd = if ctrl & CTRL_SPD_1000 != 0 {
            SPEED_1000
        } else if ctrl & CTRL_SPD_100 != 0 {
            SPEED_100
        } else {
            SPEED_10
        };
        let phy_spd = Self::speed_mbps_from_phy_st2(st2);
        if mac_spd == phy_spd {
            return false;
        }

        if mac_spd > phy_spd {
            e1000e_vlog!(
                "[e1000e] MAC {} Mb/s > PHY {} Mb/s (reg26={:#x}) — resync to PHY\n",
                mac_spd,
                phy_spd,
                st2
            );
            self.finish_copper_link_up(phy_addr, st2);
            return true;
        }
        e1000e_vlog!(
            "[e1000e] MAC {} Mb/s != PHY {} Mb/s (reg26={:#x} spd_bits={:#x}) — resync\n",
            mac_spd,
            phy_spd,
            st2,
            st2 & PHY_STATUS2_SPEED_MASK
        );
        self.finish_copper_link_up(phy_addr, st2);
        true
    }

    unsafe fn pch_disable_k1(&self) {
        let mut kmrn = self.kmrn_read(KMRNCTRLSTA_K1_CONFIG);
        kmrn &= !KMRNCTRLSTA_K1_ENABLE;
        self.kmrn_write(KMRNCTRLSTA_K1_CONFIG, kmrn);
    }

    /// Alias kept for call sites — matches Linux copper link CTRL programming.
    unsafe fn mac_allow_autoneg(&self) {
        self.mac_setup_copper_link_linux();
    }



    /// Full copper autoneg (PHY 1 first — matches Linux ethtool PHYAD on I219).
    unsafe fn pch_kick_autoneg_mdio(&self) {
        for phy_addr in [1u8, 2u8] {
            if let Some(bmcr) = self.mdic_read(phy_addr, MII_BMCR) {
                if bmcr == 0 || bmcr == 0xFFFF {
                    continue;
                }
                if self.phy_copper_autoneg_restart(phy_addr) {
                    let _ = self.phy_wait_autoneg_linux(phy_addr);
                    return;
                }
            }
        }
        crate::klog_warn!("[e1000e] MDIO: no PHY responding on addr 1 or 2\n");
    }

    /// Linux `e1000_init_hw_ich8lan` / link workarounds needed on I217/I219 real silicon.
    unsafe fn pch_apply_silicon_workarounds(&self) {
        let mut rfctl = mmio_read(self.base, E1000E_RFCTL);
        rfctl |= RFCTL_NFSW_DIS | RFCTL_NFSR_DIS | RFCTL_IPV6_EX_DIS | RFCTL_NEW_IPV6_EXT_DIS;
        mmio_write(self.base, E1000E_RFCTL, rfctl);

        let mut pbeccsts = mmio_read(self.base, E1000E_PBECCSTS);
        pbeccsts |= PBECCSTS_ECC_ENABLE;
        mmio_write(self.base, E1000E_PBECCSTS, pbeccsts);

        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl |= CTRL_MEHE;
        mmio_write(self.base, E1000E_CTRL, ctrl);

        // I217/I219 packet-loss fix (Linux e1000_setup_link_ich8lan).
        let mut fextnvm4 = mmio_read(self.base, E1000E_FEXTNVM4);
        fextnvm4 &= !FEXTNVM4_BEACON_DURATION_MASK;
        fextnvm4 |= FEXTNVM4_BEACON_DURATION_8USEC;
        mmio_write(self.base, E1000E_FEXTNVM4, fextnvm4);

        let mut fextnvm7 = mmio_read(self.base, E1000E_FEXTNVM7);
        fextnvm7 |= FEXTNVM7_SIDE_CLK_UNGATE
            | FEXTNVM7_DISABLE_SMB_PERST
            | FEXTNVM7_NEED_DESCR_RING_FLUSH;
        mmio_write(self.base, E1000E_FEXTNVM7, fextnvm7);

        if matches!(self.device_id, 0x156f..=0x1570 | 0x15b7..=0x15be) {
            let iosfpc = mmio_read(self.base, E1000E_IOSFPC);
            mmio_write(self.base, E1000E_IOSFPC, iosfpc | 0x0001_0000);
        }

        let mut fextnvm11 = mmio_read(self.base, E1000E_FEXTNVM11);
        fextnvm11 |= FEXTNVM11_DISABLE_L1_2 | FEXTNVM11_DISABLE_MULR_FIX;
        mmio_write(self.base, E1000E_FEXTNVM11, fextnvm11);

        if self.is_pch_spt_or_later() {
            let mut fflt = mmio_read(self.base, E1000E_FFLT_DBG);
            fflt |= FFLT_DBG_DONT_GATE_WAKE_DMA_CLK;
            mmio_write(self.base, E1000E_FFLT_DBG, fflt);
        }

        self.kmrn_write(KMRNCTRLSTA_TIMEOUTS, 0xFFFF);

        self.pch_setup_pcie_no_snoop();
        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext |= CTRL_EXT_RO_DIS;
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);

        // TXDCTL bit 22 (Linux initialize_hw_bits_ich8lan).
        for txdctl_reg in [E1000E_TXDCTL, E1000E_TXDCTL1] {
            let mut txdctl = mmio_read(self.base, txdctl_reg);
            txdctl |= 1 << 22;
            mmio_write(self.base, txdctl_reg, txdctl);
        }
    }

    unsafe fn program_rxdctl(&self) {
        let mut rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        rxdctl &= 0xFFFF_C000;
        rxdctl |= RXDCTL_DMA_BURST;
        if self.is_pch_spt_or_later() {
            rxdctl |= RXDCTL_QUEUE_ENABLE;
        }
        mmio_write(self.base, E1000E_RXDCTL, rxdctl);
        if self.is_pch_spt_or_later() {
            let mut rxq_wait = 100;
            while rxq_wait > 0 && mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE == 0 {
                Self::udelay(100);
                rxq_wait -= 1;
            }
        }
    }

    /// Linux `e1000_setup_rctl` for 2 KB buffers + `RFCTL_EXTEN` extended descriptors.
    unsafe fn rctl_rx_bits(&self) -> u32 {
        let mut rctl = mmio_read(self.base, E1000E_RCTL);
        rctl &= !(RCTL_MO_MASK | 0xC0); // MO + loopback mode → LBM_NO
        rctl |= RCTL_EN | RCTL_BAM | RCTL_SECRC;
        // Standard MTU: LPE on (enabling long packet support to avoid packet drops with 802.1Q tags or extra padding)
        rctl &= !(RCTL_SBP | RCTL_DTYP_PS | RCTL_BSEX | RCTL_RX_SZ_MASK);
        rctl |= RCTL_SZ_2048 | RCTL_LPE;
        rctl &= !(RCTL_UPE | RCTL_MPE);
        rctl
    }

    /// Program queue-0 SRRCTL when the register exists (Linux e1000e uses RCTL_SZ_2048 for
    /// standard RX; SRRCTL is mainly for packet-split). On I219-V the MMIO readback is often 0.
    unsafe fn program_srrctl_rx_queue0(&mut self) {
        if self.srrctl_absent {
            return;
        }
        let rctl_saved = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl_saved & !RCTL_EN);
        for _ in 0..200 {
            if mmio_read(self.base, E1000E_RCTL) & RCTL_EN == 0 {
                break;
            }
            Self::udelay(10);
        }
        let _ = mmio_read(self.base, E1000E_RCTL);

        let mut v = mmio_read(self.base, E1000E_SRRCTL);
        v &= !SRRCTL_DESCTYPE_MASK;
        v &= !0xF;
        v |= SRRCTL_BSIZE_2K | SRRCTL_DROP_EN;
        mmio_write(self.base, E1000E_SRRCTL, v);
        let _ = mmio_read(self.base, E1000E_SRRCTL);
        Self::udelay(50);
        let rd = mmio_read(self.base, E1000E_SRRCTL);
        if rd == 0 && v != 0 {
            self.srrctl_absent = true;
            crate::klog_info!(
                "[e1000e] SRRCTL not present (wrote {:#x}, read 0) — using RCTL_SZ_2048 only\n",
                v
            );
        } else if (rd & 0xF) != SRRCTL_BSIZE_2K || (rd & SRRCTL_DESCTYPE_MASK) != 0 {
            crate::klog_warn!(
                "[e1000e] SRRCTL bad readback: wrote {:#x} got {:#x}\n",
                v,
                rd
            );
        }
        if rctl_saved & RCTL_EN != 0 {
            mmio_write(self.base, E1000E_RCTL, rctl_saved);
        }
    }

    /// L2 length of a complete IPv4 frame (supports 802.1Q).
    fn eth_ipv4_frame_length(data: &[u8]) -> Option<usize> {
        if data.len() < 14 + 20 {
            return None;
        }
        let mut l2 = 14usize;
        let mut et = u16::from_be_bytes([data[12], data[13]]);
        if et == 0x8100 {
            if data.len() < 18 + 20 {
                return None;
            }
            l2 = 18;
            et = u16::from_be_bytes([data[16], data[17]]);
        }
        if et != 0x0800 {
            return None;
        }
        let ihl = ((data[l2] & 0x0f) as usize) * 4;
        if ihl < 20 || data.len() < l2 + ihl {
            return None;
        }
        let ip_tot = u16::from_be_bytes([data[l2 + 2], data[l2 + 3]]) as usize;
        if ip_tot < ihl || ip_tot > 9000 {
            return None;
        }
        Some(l2 + ip_tot)
    }

    fn eth_ipv4_frame_complete(data: &[u8]) -> bool {
        Self::eth_ipv4_frame_length(data)
            .map(|need| data.len() >= need)
            .unwrap_or(false)
    }

    /// Full L3 frame length for Ethernet (+ optional VLAN) + IPv6 (40-byte header + payload).
    fn eth_ipv6_frame_length(data: &[u8]) -> Option<usize> {
        Self::eth_ipv6_header_info(data).map(|(_, need)| need)
    }

    fn eth_ipv6_header_info(data: &[u8]) -> Option<(usize, usize)> {
        if data.len() < 14 + 40 {
            return None;
        }
        let mut l2 = 14usize;
        let mut et = u16::from_be_bytes([data[12], data[13]]);
        if et == 0x8100 {
            if data.len() < 18 + 40 {
                return None;
            }
            l2 = 18;
            et = u16::from_be_bytes([data[16], data[17]]);
        }
        if et != 0x86dd || data.len() < l2 + 40 {
            return None;
        }
        if (data[l2] >> 4) != 6 {
            return None;
        }
        let payload_len = u16::from_be_bytes([data[l2 + 4], data[l2 + 5]]) as usize;
        if payload_len > 9000 {
            return None;
        }
        Some((l2, l2 + 40 + payload_len))
    }

    fn eth_ipv6_frame_complete(data: &[u8]) -> bool {
        Self::eth_ipv6_frame_length(data)
            .map(|need| data.len() >= need)
            .unwrap_or(false)
    }

    fn trim_to_ip_frame(mut data: Vec<u8>) -> Vec<u8> {
        if let Some(need) = Self::eth_ipv4_frame_length(&data) {
            if data.len() > need {
                data.truncate(need);
            }
        } else if let Some(need) = Self::eth_ipv6_frame_length(&data) {
            if data.len() > need {
                data.truncate(need);
            }
        }
        data
    }

    /// Drop truncated L3 frames before they reach smoltcp / AF_PACKET / icmp_rx.
    fn rx_frame_deliverable(data: &[u8]) -> bool {
        if data.len() < 14 {
            return false;
        }
        let mut et = u16::from_be_bytes([data[12], data[13]]);
        if et == 0x8100 {
            if data.len() < 18 {
                return false;
            }
            et = u16::from_be_bytes([data[16], data[17]]);
        }
        match et {
            0x0800 => Self::rx_ipv4_deliverable(data),
            0x86dd => Self::eth_ipv6_frame_complete(data),
            _ => true,
        }
    }

    /// True if `data` begins with a plausible Ethernet (+ optional VLAN) + IPv4 header.
    /// Continuation fragments of a split RX descriptor do not pass this test.
    fn is_eth_ipv4_header_start(data: &[u8]) -> bool {
        if data.len() < 14 + 20 {
            return false;
        }
        let mut l2 = 14usize;
        let mut et = u16::from_be_bytes([data[12], data[13]]);
        if et == 0x8100 {
            if data.len() < 18 + 20 {
                return false;
            }
            l2 = 18;
            et = u16::from_be_bytes([data[16], data[17]]);
        }
        et == 0x0800 && (data[l2] >> 4) == 4
    }

    /// `(l2, ihl, frame_need, is_dhcp)` when `data` holds a valid IPv4 frame header.
    fn eth_ipv4_header_info(data: &[u8]) -> Option<(usize, usize, usize, bool)> {
        if data.len() < 14 + 20 {
            return None;
        }
        let mut l2 = 14usize;
        let mut et = u16::from_be_bytes([data[12], data[13]]);
        if et == 0x8100 {
            if data.len() < 18 + 20 {
                return None;
            }
            l2 = 18;
            et = u16::from_be_bytes([data[16], data[17]]);
        }
        if et != 0x0800 || data.len() < l2 + 20 {
            return None;
        }
        let ihl = ((data[l2] & 0x0f) as usize) * 4;
        if ihl < 20 || data.len() < l2 + ihl + 8 {
            return None;
        }
        let ip_tot = u16::from_be_bytes([data[l2 + 2], data[l2 + 3]]) as usize;
        if ip_tot < ihl || ip_tot > 9000 {
            return None;
        }
        let udp = l2 + ihl;
        let sport = u16::from_be_bytes([data[udp], data[udp + 1]]);
        let dport = u16::from_be_bytes([data[udp + 2], data[udp + 3]]);
        let is_dhcp = (sport == 67 && dport == 68) || (sport == 68 && dport == 67);
        Some((l2, ihl, l2 + ip_tot, is_dhcp))
    }

    fn dhcp_cookie_valid_in_frame(data: &[u8]) -> bool {
        let Some((l2, ihl, _, _)) = Self::eth_ipv4_header_info(data) else {
            return false;
        };
        if !Self::is_bootp_udp(data, l2, ihl) {
            return true;
        }
        if Self::peek_dhcp_magic_cookie(data, l2, ihl) {
            return true;
        }
        // I219 may place the cookie away from the fixed BOOTP offset.
        Self::scan_dhcp_magic_cookie_offset(data).is_some()
    }

    fn is_bootp_udp(peek: &[u8], l2: usize, ihl: usize) -> bool {
        if peek.len() < l2 + ihl + 8 {
            return false;
        }
        // BOOTP is UDP-only; at the L4 offset ICMP uses type/code, not UDP ports.
        if peek[l2 + 9] != 17 {
            return false;
        }
        let udp = l2 + ihl;
        let sport = u16::from_be_bytes([peek[udp], peek[udp + 1]]);
        let dport = u16::from_be_bytes([peek[udp + 2], peek[udp + 3]]);
        (sport == 67 && dport == 68) || (sport == 68 && dport == 67)
    }

    /// Do not deliver a frame that only satisfies ip_tot while the DHCP cookie is still fill bytes.
    fn rx_ipv4_deliverable(data: &[u8]) -> bool {
        if !Self::eth_ipv4_frame_complete(data) {
            return false;
        }
        let Some((l2, _, _, _)) = Self::eth_ipv4_header_info(data) else {
            return false;
        };
        // ICMP/ARP/etc. must reach smoltcp (ping to 10.0.2.2); cookie gate is DHCP-only.
        if data[l2 + 9] != 17 {
            return true;
        }
        Self::dhcp_cookie_valid_in_frame(data)
    }

    fn trim_to_ipv4_frame(data: Vec<u8>) -> Vec<u8> {
        Self::trim_to_ip_frame(data)
    }

    fn peek_dhcp_magic_cookie(peek: &[u8], l2: usize, ihl: usize) -> bool {
        const DHCP_COOKIE: u32 = 0x6382_5363;
        let off = l2 + ihl + 8 + 236;
        peek.len() >= off + 4
            && u32::from_be_bytes([peek[off], peek[off + 1], peek[off + 2], peek[off + 3]])
                == DHCP_COOKIE
    }

    /// Scan for RFC 2131 magic cookie anywhere in a received frame (I219 may DMA it in
    /// while the descriptor WB length / ip_tot lie about the tail).
    fn scan_dhcp_magic_cookie_offset(buf: &[u8]) -> Option<usize> {
        const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
        if buf.len() < 4 {
            return None;
        }
        for i in 0..=buf.len() - 4 {
            if buf[i..i + 4] == MAGIC {
                return Some(i);
            }
        }
        None
    }

    /// Descriptor WB length can be shorter than the frame in the DMA buffer (I219 quirk).
    /// Also extend when desc_len already matches ip_tot but the DHCP magic cookie is missing
    /// (stale tail / false length in WB).
    unsafe fn recover_rx_copy_len(&self, buf_vaddr: usize, desc_len: usize) -> usize {
        if !self.rx_needs_cache_invalidation() {
            return desc_len;
        }
        let inv = desc_len.max(512).min(BUF_SIZE);
        Self::invalidate_cpu_cache_for_read(buf_vaddr, inv);
        let peek = core::slice::from_raw_parts(buf_vaddr as *const u8, inv);
        if peek.len() < 34 {
            return desc_len;
        }
        let mut l2 = 14usize;
        let mut et = u16::from_be_bytes([peek[12], peek[13]]);
        if et == 0x8100 {
            if peek.len() < 38 {
                return desc_len;
            }
            l2 = 18;
            et = u16::from_be_bytes([peek[16], peek[17]]);
        }
        if et == 0x86dd {
            if peek.len() < l2 + 40 || (peek[l2] >> 4) != 6 {
                return desc_len;
            }
            let payload_len = u16::from_be_bytes([peek[l2 + 4], peek[l2 + 5]]) as usize;
            if payload_len > 9000 {
                return desc_len;
            }
            let frame_need = l2 + 40 + payload_len;
            if frame_need > BUF_SIZE {
                return desc_len;
            }
            if frame_need > desc_len {
                Self::invalidate_cpu_cache_for_read(buf_vaddr, frame_need.min(BUF_SIZE));
                if Self::eth_ipv6_frame_complete(core::slice::from_raw_parts(
                    buf_vaddr as *const u8,
                    frame_need.min(BUF_SIZE),
                )) {
                    return frame_need;
                }
            }
            return desc_len;
        }
        if et != 0x0800 {
            return desc_len;
        }
        let ihl = ((peek[l2] & 0x0f) as usize) * 4;
        if ihl < 20 || peek.len() < l2 + ihl + 8 {
            return desc_len;
        }
        let ip_tot = u16::from_be_bytes([peek[l2 + 2], peek[l2 + 3]]) as usize;
        if ip_tot < ihl || ip_tot > 9000 {
            return desc_len;
        }
        let frame_need = l2 + ip_tot;
        if frame_need > BUF_SIZE {
            return desc_len;
        }

        let is_bootp = Self::is_bootp_udp(peek, l2, ihl);
        let cookie_ok = Self::peek_dhcp_magic_cookie(peek, l2, ihl);
        let mut copy_len = desc_len;

        if frame_need > copy_len {
            if cookie_ok {
                copy_len = frame_need;
                crate::klog_warn!(
                    "[e1000e] RX len fix: desc {} → {} (cookie @236)\n",
                    desc_len,
                    frame_need
                );
            } else {
                Self::invalidate_cpu_cache_for_read(buf_vaddr, BUF_SIZE);
                let full = core::slice::from_raw_parts(buf_vaddr as *const u8, BUF_SIZE);
                if is_bootp && Self::peek_dhcp_magic_cookie(full, l2, ihl) {
                    copy_len = frame_need;
                    crate::klog_warn!(
                        "[e1000e] RX len fix: desc {} → {} (BOOTP cookie after reinvalidate)\n",
                        desc_len,
                        frame_need
                    );
                } else if frame_need <= full.len() && Self::eth_ipv4_frame_complete(&full[..frame_need]) {
                    copy_len = frame_need;
                    crate::klog_warn!(
                        "[e1000e] RX len fix: desc {} → {} (ip_tot_len, is_bootp={})\n",
                        desc_len,
                        frame_need,
                        is_bootp
                    );
                } else if is_bootp {
                    crate::klog_info!(
                        "[e1000e] RX BOOTP desc {} ip_tot {} B, no cookie @236 and incomplete — keep {} B (SG)\n",
                        desc_len,
                        frame_need,
                        copy_len
                    );
                }
            }
        }
        copy_len
    }

    /// Linux `e1000_access_phy_wakeup_reg_bm` — page-800 regs use opcodes 0x11/0x12 on PHY1.
    /// Caller must hold SWFLAG (via `pch_bm_wuc_access_begin`).
    unsafe fn pch_bm_wuc_reg_read(&self, reg: u8) -> Option<u16> {
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ADDRESS_OPCODE, reg as u16) {
            return None;
        }
        self.mdic_read_swheld(BM_PHY_MDIO_ADDR, BM_WUC_DATA_OPCODE)
    }

    unsafe fn pch_bm_wuc_reg_write(&self, reg: u8, val: u16) -> bool {
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ADDRESS_OPCODE, reg as u16) {
            return false;
        }
        self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_DATA_OPCODE, val)
    }

    /// Linux `e1000_enable_phy_wakeup_reg_access_bm` (always MDIO PHY address 1).
    unsafe fn pch_bm_wuc_access_begin(&self) -> Option<u16> {
        if !self.pch_swflag_acquire() {
            return None;
        }
        let page_port = (BM_PORT_CTRL_PAGE << IGP_PAGE_SHIFT) as u16;
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, IGP_PHY_PAGE_SELECT, page_port) {
            self.pch_swflag_release();
            return None;
        }
        let Some(saved) = self.mdic_read_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG) else {
            self.pch_swflag_release();
            return None;
        };
        let mut temp = saved | BM_WUC_ENABLE_BIT;
        temp &= !(BM_WUC_ME_WU_BIT | BM_WUC_HOST_WU_BIT);
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG, temp) {
            self.pch_swflag_release();
            return None;
        }
        // Linux: select BM_WUC_PAGE (800) for host wakeup register access.
        let wuc_page = (BM_WUC_PAGE << IGP_PAGE_SHIFT) as u16;
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, IGP_PHY_PAGE_SELECT, wuc_page) {
            self.pch_swflag_release();
            return None;
        }
        Some(saved)
    }

    unsafe fn pch_bm_wuc_access_end(&self, saved: u16) {
        let page_port = (BM_PORT_CTRL_PAGE << IGP_PAGE_SHIFT) as u16;
        let _ = self.mdic_write_swheld(BM_PHY_MDIO_ADDR, IGP_PHY_PAGE_SELECT, page_port);
        let _ = self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG, saved);
        self.pch_swflag_release();
    }

    /// Linux `e1000_copy_rx_addrs_to_phy_ich8lan` + `e1000_init_phy_wakeup` BM path.
    unsafe fn pch_sync_phy_rx_path(&self, link_phy: u8) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        // Linux: reliable BM page-800 MDIO on PCH when gigabit path is gated off briefly.
        let phy_ctrl_saved = if self.is_i219_metal_rx_hacks() {
            let s = mmio_read(self.base, E1000E_PHY_CTRL);
            mmio_write(
                self.base,
                E1000E_PHY_CTRL,
                s | PHY_CTRL_GBE_DISABLE | PHY_CTRL_NOND0A_GBE_DISABLE,
            );
            let _ = mmio_read(self.base, E1000E_PHY_CTRL);
            Some(s)
        } else {
            None
        };
        let Some(saved) = self.pch_bm_wuc_access_begin() else {
            if let Some(s) = phy_ctrl_saved {
                mmio_write(self.base, E1000E_PHY_CTRL, s);
            }
            crate::klog_warn!(
                "[e1000e] BM WUC access failed (link PHY{}, MDIO PHY{})\n",
                link_phy,
                BM_PHY_MDIO_ADDR
            );
            return;
        };

        let mut ok_rar = true;
        for i in 0..PCH_LPT_RAR_ENTRIES {
            let ral = mmio_read(self.base, E1000E_RAL0 + i * 2);
            let rah = mmio_read(self.base, E1000E_RAL0 + i * 2 + 1);
            let base = 16 + (i << 2);
            ok_rar = ok_rar
                && self.pch_bm_wuc_reg_write(base as u8, ral as u16)
                && self.pch_bm_wuc_reg_write((base + 1) as u8, (ral >> 16) as u16)
                && self.pch_bm_wuc_reg_write((base + 2) as u8, rah as u16)
                && self.pch_bm_wuc_reg_write((base + 3) as u8, ((rah & RAH_AV) >> 16) as u16);
        }

        let mut ok_mta = true;
        for i in 0..ICH_MTA_REG_COUNT {
            let mac = mmio_read(self.base, E1000E_MTA_BASE + i);
            let bm = 128 + (i << 1);
            ok_mta = ok_mta
                && self.pch_bm_wuc_reg_write(bm as u8, mac as u16)
                && self.pch_bm_wuc_reg_write((bm + 1) as u8, (mac >> 16) as u16);
        }

        let rctl = mmio_read(self.base, E1000E_RCTL);
        let ctrl = mmio_read(self.base, E1000E_CTRL);
        let mut bm_rctl = self.pch_bm_wuc_reg_read(0).unwrap_or(0);
        bm_rctl &= !(BM_RCTL_UPE
            | BM_RCTL_MPE
            | BM_RCTL_MO_MASK
            | BM_RCTL_BAM
            | BM_RCTL_PMCF
            | BM_RCTL_RFCE);
        if rctl & RCTL_UPE != 0 {
            bm_rctl |= BM_RCTL_UPE;
        }
        if rctl & RCTL_MPE != 0 {
            bm_rctl |= BM_RCTL_MPE;
        }
        if rctl & RCTL_MO_3 == RCTL_MO_3 {
            bm_rctl |= 3 << BM_RCTL_MO_SHIFT;
        }
        if rctl & RCTL_BAM != 0 {
            bm_rctl |= BM_RCTL_BAM;
        }
        if rctl & RCTL_PMCF != 0 {
            bm_rctl |= BM_RCTL_PMCF;
        }
        if ctrl & CTRL_RFCE != 0 {
            bm_rctl |= BM_RCTL_RFCE;
        }
        let ok_rctl = self.pch_bm_wuc_reg_write(0, bm_rctl);

        self.pch_bm_wuc_access_end(saved);
        if let Some(s) = phy_ctrl_saved {
            mmio_write(self.base, E1000E_PHY_CTRL, s);
            let _ = mmio_read(self.base, E1000E_PHY_CTRL);
        }

        if self.is_i219_metal_rx_hacks() {
            mmio_write(self.base, E1000E_WUC, WUC_PHY_WAKE | WUC_PME_STATUS);
        }

        if ok_rar && ok_mta && ok_rctl {
            e1000e_vlog!(
                "[e1000e] BM page-800 full sync ({} RAR, {} MTA, PHY{} link PHY{}, BM_RCTL={:#x})\n",
                PCH_LPT_RAR_ENTRIES,
                ICH_MTA_REG_COUNT,
                BM_PHY_MDIO_ADDR,
                link_phy,
                bm_rctl
            );
        } else {
            crate::klog_warn!(
                "[e1000e] BM page-800 partial sync (rar={} mta={} rctl={})\n",
                ok_rar,
                ok_mta,
                ok_rctl
            );
        }
    }

    unsafe fn verify_rx_engine(&self) {
        let rctl = mmio_read(self.base, E1000E_RCTL);
        let rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        if rctl & RCTL_EN == 0 {
            crate::klog_warn!("[e1000e] RX engine: RCTL.EN clear (RCTL={:#x})\n", rctl);
        }
        if self.is_pch_spt_or_later() && rxdctl & RXDCTL_QUEUE_ENABLE == 0 {
            crate::klog_warn!("[e1000e] RX engine: RXDCTL.QUEUE_ENABLE clear ({:#x})\n", rxdctl);
        }
    }

    /// Linux `e1000_flush_rx_ring`: disable RCTL.EN, reset RXDCTL, leave RCTL
    /// disabled. arm_rx_unit_linux re-enables RCTL.EN only after descriptors
    /// are posted. Previously this re-enabled RCTL.EN here (before the ring was
    /// armed), which could put the I219 DMA engine into an undefined state.
    unsafe fn flush_rx_ring_toggle(&self) {
        let rctl_saved = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl_saved & !RCTL_EN);
        let _ = mmio_read(self.base, E1000E_RCTL);
        Self::udelay(150);
        // Reset RXDCTL burst params (clears QUEUE_ENABLE too on SPT).
        // arm_rx_unit_linux will re-enable QUEUE_ENABLE after ring is posted.
        let mut rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        rxdctl &= !RXDCTL_QUEUE_ENABLE;
        mmio_write(self.base, E1000E_RXDCTL, rxdctl);
        let _ = mmio_read(self.base, E1000E_RXDCTL);
        Self::udelay(100);
    }

    unsafe fn log_rx_path_regs(&self, tag: &str) {
        if !E1000E_LOG_VERBOSE {
            return;
        }
        let ctrl = mmio_read(self.base, E1000E_CTRL);
        let status = mmio_read(self.base, E1000E_STATUS);
        let frc = (ctrl & (CTRL_FRCSPD | CTRL_FRCDPX)) != 0;
        let mac_spd = if ctrl & CTRL_SPD_1000 != 0 {
            "1000"
        } else if ctrl & CTRL_SPD_100 != 0 {
            "100"
        } else {
            "10"
        };
        let gprc = mmio_read(self.base, E1000E_GPRC);
        let mpc = mmio_read(self.base, E1000E_MPC);
        let rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        let status_spd = Self::speed_mbps_from_status(status);
        e1000e_vlog!(
            "e1000e: {} CTRL={:#x} mac_spd={} STATUS_spd={} FRC={} STATUS={:#x} RCTL={:#x} RXDCTL={:#x} GPRC={} MPC={} RDH={} RDT={}\n",
            tag,
            ctrl,
            mac_spd,
            status_spd,
            if frc { "BAD" } else { "ok" },
            status,
            mmio_read(self.base, E1000E_RCTL),
            rxdctl,
            gprc,
            mpc,
            mmio_read(self.base, E1000E_RDH),
            mmio_read(self.base, E1000E_RDT)
        );
    }

    unsafe fn wait_for_speed_status(&self, max_ms: u32) -> u32 {
        let steps = (max_ms as u64 * 1000 / 50).max(1);
        for _ in 0..steps {
            let status = mmio_read(self.base, E1000E_STATUS);
            if status & STATUS_LU != 0 {
                // SPEED_MASK == 0 is valid: it means 10 Mb/s (bits[7:6] = 00).
                // Only keep waiting if STATUS.LU itself is not yet set.
                return status;
            }
            Self::udelay(50);
        }
        mmio_read(self.base, E1000E_STATUS)
    }

    unsafe fn phy_write_emi(&self, phy_addr: u8, emi_addr: u16, data: u16) -> bool {
        self.mdic_write(phy_addr, PHY_EMI_ADDR, emi_addr)
            && self.mdic_write(phy_addr, PHY_EMI_DATA, data)
    }

    /// Linux `e1000_setup_copper_link_ich8lan` + post-link RX PHY tuning (I217/I219).
    /// Returns PHY reg 26 read before MDIO tuning writes (used for MAC speed).
    unsafe fn pch_post_link_phy_tune(&self) -> u16 {
        if !self.is_pch_lpt_or_later() {
            return 0;
        }
        self.kmrn_write(KMRNCTRLSTA_TIMEOUTS, 0xFFFF);
        self.pch_disable_k1();

        let phy = self.active_phy_addr();
        let st2_cached = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        self.phy_setup_82577_copper(phy);

        if !self.mdic_write_phy(phy, PHY_REG_770_19, 0x0100) {
            crate::klog_warn!("[e1000e] link-stall PHY reg write failed\n");
        }

        let st2_after = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        // Post-tune read reflects negotiated speed; pre-tune can be stale (10M).
        let st2 = if st2_after != 0 && st2_after != 0xFFFF {
            st2_after
        } else {
            st2_cached
        };
        e1000e_vlog!(
            "[e1000e] post-link PHY{} reg26={:#x} ({}) cached={:#x}\n",
            phy,
            st2,
            Self::hv_m_status_label(st2),
            st2_cached
        );

        st2
    }

    /// Ensure GIO master is enabled (DMA will not run otherwise on PCH).
    unsafe fn ensure_gio_master(&self) {
        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        if ctrl & CTRL_GIO_MASTER_DISABLE != 0 {
            ctrl &= !CTRL_GIO_MASTER_DISABLE;
            mmio_write(self.base, E1000E_CTRL, ctrl);
            let _ = mmio_read(self.base, E1000E_CTRL);
        }
        for _ in 0..50 {
            if mmio_read(self.base, E1000E_STATUS) & STATUS_GIO_MASTER_ENABLE != 0 {
                return;
            }
            Self::udelay(100);
        }
        crate::klog_warn!("[e1000e] GIO master enable bit still clear in STATUS\n");
    }

    /// Linux `e1000_init_hw_ich8lan` + `e1000_configure_tx` TXDCTL for PCH/I219.
    unsafe fn program_txdctl_linux(&self) -> u32 {
        let mut txdctl = mmio_read(self.base, E1000E_TXDCTL);
        txdctl &= 0xFFFF_C000;
        txdctl |= TXDCTL_COUNT_DESC;
        txdctl = (txdctl & !TXDCTL_WTHRESH_MASK) | TXDCTL_FULL_TX_DESC_WB;
        txdctl = (txdctl & !TXDCTL_PTHRESH_MASK) | TXDCTL_MAX_TX_DESC_PREFETCH;
        txdctl |= 1 << 22; // Linux initialize_hw_bits_ich8lan always sets bit 22
        txdctl
    }

    unsafe fn ensure_tx_engine_ready(&self) -> bool {
        self.ensure_gio_master();
        let tctl = mmio_read(self.base, E1000E_TCTL);
        if tctl & TCTL_EN == 0 {
            return false;
        }
        if self.is_pch_spt_or_later() {
            let txdctl = mmio_read(self.base, E1000E_TXDCTL);
            if txdctl & TXDCTL_QUEUE_ENABLE == 0 {
                return false;
            }
        }
        true
    }

    /// Linux `e1000_configure_tx` + I219 SPT errata — call after link/PHY is stable.
    unsafe fn restart_tx_datapath_linux(&mut self) {
        self.ensure_gio_master();
        let mut tctl = mmio_read(self.base, E1000E_TCTL);
        mmio_write(self.base, E1000E_TCTL, tctl & !TCTL_EN);
        Self::udelay(150);

        mmio_write(self.base, E1000E_TDH, 0);
        mmio_write(self.base, E1000E_TDT, 0);
        self.tx_tail = 0;

        fence(Ordering::SeqCst);

        let mut txdctl = if self.is_pch_lpt_or_later() {
            self.program_txdctl_linux()
        } else {
            let mut t = mmio_read(self.base, E1000E_TXDCTL);
            t &= 0xFFFF_C000;
            t |= TXDCTL_DMA_BURST;
            t
        };
        if self.is_pch_spt_or_later() {
            txdctl |= TXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
            let mut txq_wait = 100;
            while txq_wait > 0 && mmio_read(self.base, E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE == 0 {
                Self::udelay(100);
                txq_wait -= 1;
            }
            mmio_write(self.base, E1000E_TXDCTL1, mmio_read(self.base, E1000E_TXDCTL));
            let iosfpc = mmio_read(self.base, E1000E_IOSFPC);
            mmio_write(self.base, E1000E_IOSFPC, iosfpc | 0x0001_0000);
        } else {
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
        }

        // Linux netdev: clear TARC speed-mode at 10/100M before TCTL_EN (I219 TX errata).
        let phy = self.active_phy_addr();
        let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        self.program_tarc_for_speed(Self::speed_mbps_from_phy_st2(st2));
        self.config_collision_dist_linux();

        tctl = mmio_read(self.base, E1000E_TCTL);
        tctl &= !(0xFF0u32 | 0x003FF000u32);
        tctl |= TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT_LINUX | TCTL_COLD_LINUX;
        mmio_write(self.base, E1000E_TCTL, tctl);
        let _ = mmio_read(self.base, E1000E_TCTL);
    }

    unsafe fn log_post_link_counters(&self, tag: &str) {
        if !E1000E_LOG_VERBOSE {
            return;
        }
        let gprc = mmio_read(self.base, E1000E_GPRC);
        let mpc = mmio_read(self.base, E1000E_MPC);
        let gptc = mmio_read(self.base, E1000E_GPTC);
        e1000e_vlog!(
            "[e1000e] {} GPRC={} GPTC={} MPC={} RCTL={:#x} RXDCTL={:#x}\n",
            tag,
            gprc,
            gptc,
            mpc,
            mmio_read(self.base, E1000E_RCTL),
            mmio_read(self.base, E1000E_RXDCTL)
        );
    }

    /// Arm RX rings and enable RCTL — call only when link is up and configured.
    unsafe fn enable_rx_after_link(&mut self) {
        if self.is_pch_lpt_or_later() {
            let phy = self.active_phy_addr();
            let _ = self.resync_mac_if_phy_changed(phy);
            self.restart_tx_datapath_linux();
        }

        let status = mmio_read(self.base, E1000E_STATUS);
        let status_spd = Self::speed_mbps_from_status(status);
        if self.is_pch_lpt_or_later() {
            let phy = self.active_phy_addr();
            let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
            crate::klog_info!(
                "[e1000e] RX enable: STATUS={} Mb/s PHY reg26={:#x} ({}) CTRL={:#x} RCTL={:#x} RXDCTL={:#x}\n",
                status_spd,
                st2,
                Self::hv_m_status_label(st2),
                mmio_read(self.base, E1000E_CTRL),
                mmio_read(self.base, E1000E_RCTL),
                mmio_read(self.base, E1000E_RXDCTL)
            );
        } else {
            crate::klog_info!(
                "[e1000e] RX enable: STATUS={} Mb/s (discrete) CTRL={:#x}\n",
                status_spd,
                mmio_read(self.base, E1000E_CTRL)
            );
        }

        // 1. Re-arm RFCTL before touching the ring so extended WB is active.
        let mut rfctl = mmio_read(self.base, E1000E_RFCTL);
        rfctl |= RFCTL_EXTEN | RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
        mmio_write(self.base, E1000E_RFCTL, rfctl);
        let rfctl_rd = mmio_read(self.base, E1000E_RFCTL);
        self.use_extended_descriptors = (rfctl_rd & RFCTL_EXTEN) != 0;

        unsafe { self.program_srrctl_rx_queue0() };

        // I219: toggle RXDCTL/RCTL like Linux before posting a fresh ring after link events.
        if self.is_i219_metal_rx_hacks() {
            self.flush_rx_ring_toggle();
            mmio_write(self.base, E1000E_FCRTL, 0);
            mmio_write(self.base, E1000E_FCRTH, 0);
        } else if self.is_pch_lpt_or_later() {
            self.flush_rx_ring_toggle();
        }

        self.arm_rx_unit_linux();

        // Sync MAC RAR/MTA/RCTL → PHY BM filters after RCTL.EN (Linux init_phy_wakeup path).
        if self.is_pch_lpt_or_later() {
            let phy = self.active_phy_addr();
            self.pch_sync_phy_rx_path(phy);
        }

        self.last_hw_rx_packets = 0;
        self.rx_link_armed = true;
        self.log_rx_path_regs("RX armed");
        self.log_post_link_counters("post-RX-arm");
    }



    unsafe fn resolve_link_speed_duplex(&self) -> (u32, bool) {
        let phy = self.phy_addr;
        let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        let (speed, duplex_full, _src) = self.phy_operational_speed(phy, st2);
        (speed, duplex_full)
    }

    unsafe fn pch_spt_ptr_gap_workaround(&self, phy: u8, speed: u32) {
        if speed == SPEED_1000 {
            if let Some(mut data) = self.mdic_read(phy, Self::phy_reg_paged(776, 20)) {
                let ptr_gap = (data & (0x3FF << 2)) >> 2;
                if ptr_gap < 0x18 {
                    data &= !(0x3FF << 2);
                    data |= 0x18 << 2;
                    let _ = self.mdic_write(phy, Self::phy_reg_paged(776, 20), data);
                }
            }
        } else {
            let _ = self.mdic_write(phy, Self::phy_reg_paged(776, 20), 0xC023);
        }
    }

    unsafe fn configure_flow_control_after_link(&self, _speed: u32, _duplex_full: bool) {
        // Disabling flow control to avoid transmission drops on simple networks
        mmio_write(self.base, E1000E_FCRTL, 0);
        mmio_write(self.base, E1000E_FCRTH, 0);
    }

    /// Versión optimizada basada en el watchdog in-tree de Linux:
    /// Resuelve de forma segura el enlace físico y evita bucles infinitos de AN.
    unsafe fn check_for_link_linux(&mut self) -> bool {
        let is_pch = self.is_pch_lpt_or_later();
        let phy = self.phy_addr;

        // 1. Leer BMSR dos veces para limpiar el bit pegajoso de "Latch-Low".
        let _ = self.mdic_read(phy, MII_BMSR);
        let bmsr = self.mdic_read(phy, MII_BMSR).unwrap_or(0);
        
        // Manejo de cable desconectado o hardware ausente
        if bmsr == 0 || bmsr == 0xFFFF {
            if self.link_up {
                crate::klog_warn!("[e1000e] Link is Down (PHY read failure or detached)\n");
                self.link_up = false;
                self.link_speed = 0;
                self.link_duplex = false;
                self.rx_link_armed = false;
            }
            return false;
        }

        let link_up = (bmsr & 0x0004) != 0;

        if !link_up {
            if self.link_up {
                crate::klog_warn!("[e1000e] Link is Down\n");
                self.link_up = false;
                self.link_speed = 0;
                self.link_duplex = false;
                self.rx_link_armed = false;
            }
            return false;
        }

        // Intel: do not treat link as negotiated until ANEG completes.
        if is_pch && (bmsr & BMSR_ANEG_COMPLETE) == 0 {
            return false;
        }

        // 2. Resolve speed: STATUS first (e1000e_get_speed_and_duplex_copper), then reg26/reg17.
        let mut st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        let (mut speed, mut duplex_full, mut src) =
            self.resolve_link_speed_duplex_linux(phy, st2);

        // 3. If PHY/STATUS stuck at 10M but MII HCD is higher, wait for reg26 to settle.
        if speed == SPEED_10 && !self.link_10m_degraded {
            if let Some(hcd_speed) = self.phy_reg26_below_mii_hcd(phy, st2) {
                if hcd_speed > SPEED_10 {
                    st2 = self.phy_wait_reg26_settled(phy, 3000);
                    (speed, duplex_full, src) =
                        self.resolve_link_speed_duplex_linux(phy, st2);
                    if speed == SPEED_10 && hcd_speed > SPEED_10 {
                        self.phy_accept_10m_degraded_mode(phy);
                        return false;
                    }
                }
            }
        }

        // 4. Si el enlace ha cambiado o acaba de levantarse:
        if !self.link_up || self.link_speed != speed || self.link_duplex != duplex_full {
            let was_down = !self.link_up;
            crate::klog_info!(
                "[e1000e] Link is Up @ {} Mbps {} Duplex (Validated via {})\n",
                speed,
                if duplex_full { "Full" } else { "Half" },
                src
            );

            self.link_up = true;
            self.link_speed = speed;
            self.link_duplex = duplex_full;

            // 5. MAC: autoneg path — ASDE, no FRCSPD at 10/100 (Intel ich8lan link-up).
            if is_pch {
                self.mac_apply_link_up_autoneg();
            } else {
                self.mac_sync_ctrl_speed_mbps(speed, duplex_full);
            }

            // 6. Aplicar parches específicos de silicio según la velocidad real negociada
            if is_pch {
                self.program_link_tipg_emi_linux(phy, speed, duplex_full);
                self.pch_kmrn_half_duplex_preamble(phy, duplex_full);
                
                // Errata Intel crucial: El bit TARC0_SPEED_MODE debe apagarse a 10/100M, 
                // de lo contrario el motor de transmisión se congela (TX DMA lockup).
                self.program_tarc_with_tctl_gate(speed);
                self.config_collision_dist_linux();

                // Deshabilitar K1 (Power Management) a 10/100M para evitar oscilaciones cíclicas del enlace.
                if speed < SPEED_1000 {
                    self.pch_disable_k1();
                }
                
                self.phy_setup_82577_copper(phy);

                if self.is_pch_spt_or_later() {
                    self.pch_spt_ptr_gap_workaround(phy, speed);
                }
            } else {
                self.config_collision_dist_linux();
            }

            self.configure_flow_control_after_link(speed, duplex_full);

            // 7. Arm RX when link comes up or the ring was never posted (flap / partial init).
            if was_down || !self.rx_link_armed {
                self.enable_rx_after_link();
            }
        }

        true
    }

    /// Máquina de estados asíncrona optimizada para Bringup.
    /// No bloquea el hilo principal y respeta los tiempos del firmware del PHY.
    unsafe fn deferred_link_bringup_tick(&mut self) {
        if !self.link_bringup_pending {
            return;
        }

        if self.device_id == 0x10d3 {
            self.qemu_finish_link_if_up();
            return;
        }

        if self.is_pch_lpt_or_later() {
            self.pch_try_early_link();
            if !self.link_bringup_pending {
                return;
            }
        }

        let now = timer_now_as_micros();
        let elapsed_us = now.wrapping_sub(self.stage_start_us);

        // State 4: PHY link + autoneg done — bring up MAC/RX/TX immediately, then retry briefly.
        if self.link_bringup_stage == 4 {
            if self.check_for_link_linux() {
                e1000e_vlog!("[e1000e] Bringup: Link resuelto y estable.\n");
                self.link_bringup_pending = false;
                self.link_bringup_stage = 0;
                self.link_bringup_attempts = 0;
            } else if elapsed_us >= 1_500_000 {
                crate::klog_warn!("[e1000e] Bringup: El enlace se cayó antes de estabilizarse. Reintentando...\n");
                self.link_bringup_stage = 0;
                self.stage_start_us = now;
            }
            return;
        }

        // Lectura de control del PHY antes de evaluar timeouts de estados
        let phy = self.phy_addr;
        let _ = self.mdic_read(phy, MII_BMSR);
        let bmsr = self.mdic_read(phy, MII_BMSR).unwrap_or(0);
        let phy_link_up = bmsr != 0 && bmsr != 0xFFFF && (bmsr & 0x0004) != 0;
        let aneg_complete = (bmsr & BMSR_ANEG_COMPLETE) != 0;

        // If PHY reports link + autoneg, try full MAC bring-up now (don't wait 1.5 s idle).
        if phy_link_up && aneg_complete {
            let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
            e1000e_vlog!(
                "[e1000e] Bringup: Link de capa 1 detectado (Reg26={:#x}, Stage {}). Pasando a estabilización.\n",
                st2, self.link_bringup_stage
            );
            if self.check_for_link_linux() {
                self.link_bringup_pending = false;
                self.link_bringup_stage = 0;
                self.link_bringup_attempts = 0;
                return;
            }
            self.link_bringup_stage = 4;
            self.stage_start_us = now;
            return;
        }

        // Si excedemos el número máximo de intentos duros sin portadora, apagamos la bandera para no saturar el kernel
        if self.link_bringup_stage == 0 && self.link_bringup_attempts >= 3 {
            self.link_bringup_pending = false;
            self.link_bringup_stage = 0;
            self.link_up = false;
            crate::klog_warn!("[e1000e] Bringup: No se detectó portadora física tras 3 ciclos de hardware.\n");
            return;
        }

        // Máquina de estados para dispositivos integrados (PCH)
        match self.link_bringup_stage {
            0 => {
                self.link_bringup_attempts = self.link_bringup_attempts.saturating_add(1);
                e1000e_vlog!("[e1000e] Bringup PCH: Ciclo de inicialización {}/3\n", self.link_bringup_attempts);
                
                self.pch_disable_lplu_gbe();
                self.mac_allow_autoneg();
                self.pch_kick_autoneg_mdio();
                
                self.link_bringup_stage = 1;
                self.stage_start_us = now;
            }
            1 => {
                // Timeout del Estado 1 (4 segundos esperando portadora inicial con configuraciones base)
                if elapsed_us >= 4_000_000 {
                    e1000e_vlog!("[e1000e] Bringup PCH: Timeout en Stage 1 (4s). Forzando sincronización de registros MAC/PHY.\n");
                    self.mac_setup_copper_link_linux();
                    self.pch_kick_autoneg_mdio();
                    self.link_bringup_stage = 2;
                    self.stage_start_us = now;
                }
            }
            2 => {
                // Timeout del Estado 2 (2 segundos adicionales). Si sigue colapsado, disparamos un reset eléctrico del PHY vía LANPHYPC.
                if elapsed_us >= 2_000_000 {
                    crate::klog_warn!("[e1000e] Bringup PCH: Timeout en Stage 2. Disparando reset físico por hardware (LANPHYPC).\n");
                    self.toggle_lanphypc();
                    self.pch_disable_lplu_gbe();
                    self.pch_kick_autoneg_mdio();
                    self.link_bringup_stage = 3;
                    self.stage_start_us = now;
                }
            }
            3 => {
                // Timeout del Estado 3 (4 segundos esperando estabilización tras el reset eléctrico). Si falla, reinicia el ciclo completo.
                if elapsed_us >= 4_000_000 {
                    e1000e_vlog!("[e1000e] Bringup PCH: Reset de hardware sin respuesta. Reiniciando secuencia...\n");
                    self.link_bringup_stage = 0;
                    self.stage_start_us = now;
                }
            }
            _ => {
                self.link_bringup_stage = 0;
                self.stage_start_us = now;
            }
        }
    }

    fn maybe_log_rx_diag(&mut self) {
        // Read Missed Packet Count (MPC). This register is clear-on-read.
        // A non-zero delta indicates packet drop at the MAC layer.
        let mpc_delta = unsafe { mmio_read(self.base, E1000E_MPC) };
        if mpc_delta > 0 {
            let gprc = unsafe { mmio_read(self.base, E1000E_GPRC) };
            if gprc == 0 {
                warn!(
                    "[e1000e] MPC +{} but GPRC=0 — CPU cache/RDT race or IMS mute; \
                     RDH={} RDT={} clean={} post={} uc={}",
                    mpc_delta,
                    unsafe { mmio_read(self.base, E1000E_RDH) },
                    unsafe { mmio_read(self.base, E1000E_RDT) },
                    self.rx_next_to_clean,
                    self.rx_post_since_doorbell,
                    self.dma_uncached
                );
            } else {
                warn!(
                    "[e1000e] MPC +{} — ring underrun; GPRC={} RDH={} RDT={} clean={} uc={}",
                    mpc_delta,
                    gprc,
                    unsafe { mmio_read(self.base, E1000E_RDH) },
                    unsafe { mmio_read(self.base, E1000E_RDT) },
                    self.rx_next_to_clean,
                    self.dma_uncached
                );
            }
        }

        if !E1000E_LOG_VERBOSE {
            return;
        }
        self.rx_diag_counter = self.rx_diag_counter.wrapping_add(1);
        if self.rx_diag_counter & 0x3F != 0 {
            return;
        }
        // GPRC is clear-on-read: one read per diag tick, accumulate in software.
        let gprc_delta = unsafe { mmio_read(self.base, E1000E_GPRC) };
        if gprc_delta > 0 {
            self.last_hw_rx_packets = self.last_hw_rx_packets.wrapping_add(gprc_delta);
            log::debug!(
                "[e1000e] GPRC +{} (total {}) RDH={} RDT={} clean={}",
                gprc_delta,
                self.last_hw_rx_packets,
                unsafe { mmio_read(self.base, E1000E_RDH) },
                unsafe { mmio_read(self.base, E1000E_RDT) },
                self.rx_next_to_clean
            );
        }
        if self.rx_needs_cache_invalidation() && self.stats.rx_packets == 0 {
            let i = self.rx_next_to_clean;
            let ring = self.rx_ring.as_ptr::<RxDesc>();
            let desc_addr = unsafe { ring.add(i) as usize };
            unsafe {
                Self::invalidate_cpu_cache_for_read(desc_addr, core::mem::size_of::<RxDesc>());
            }
            let wb = unsafe { read_volatile((desc_addr + 8) as *const u32) };
            if wb != 0 {
                log::trace!(
                    "[e1000e] RX slot {} WB={:#x} but not consumed (clean={})",
                    i,
                    wb,
                    self.rx_next_to_clean
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Full hardware reset + init
    // -----------------------------------------------------------------------
    unsafe fn reset_and_init(&mut self) -> DeviceResult {
        e1000e_vlog!(
            "e1000e: reset_and_init tag={} profile={}\n",
            E1000E_DRIVER_TAG,
            e1000e_profile()
        );
        // Always disable ULP on PCH — conventional mode used to skip this and left RX dead.
        if self.is_pch_lpt_or_later() {
            self.disable_ulp();
        }

        self.read_mac_from_hw();
        let mut mac_found = self.is_valid_mac();

        // PCH I219+: a full CTRL_RST after UEFI can wedge the PCIe bus if the
        // BIOS left DMA descriptors active (completion-timeout → fatal PCIe error).
        // Instead we:
        //   1. Disable RXDCTL/TXDCTL QUEUE_ENABLE (stop DMA queues gracefully).
        //   2. Disable RCTL.EN / TCTL.EN.
        //   3. Wait for STATUS.GIO_MASTER_ENABLE to clear (no pending PCIe TLPs).
        //   4. Then proceed without CTRL_RST, leaving the PHY clocks running.
        // This mirrors Linux e1000_reset_hw_ich8lan's safe-path for I219.
        let skip_hw_reset = self.is_pch_spt_or_later();

        self.flush_desc_rings();

        if skip_hw_reset {
            // Step 1: Disable DMA queue-enable bits BEFORE stopping RCTL/TCTL
            // so the hardware can drain any in-flight descriptors cleanly.
            if self.is_pch_spt_or_later() {
                let mut rxdctl = mmio_read(self.base, E1000E_RXDCTL);
                rxdctl &= !RXDCTL_QUEUE_ENABLE;
                mmio_write(self.base, E1000E_RXDCTL, rxdctl);
                let _ = mmio_read(self.base, E1000E_RXDCTL);

                let mut txdctl = mmio_read(self.base, E1000E_TXDCTL);
                txdctl &= !TXDCTL_QUEUE_ENABLE;
                mmio_write(self.base, E1000E_TXDCTL, txdctl);
                let _ = mmio_read(self.base, E1000E_TXDCTL);
                Self::udelay(1_000); // let queues drain
            }

            // Step 2: Stop RX/TX engines (RCTL.EN / TCTL.EN).
            self.stop_rx_tx_engines();

            // Step 3: Wait for GIO master to go idle (no PCIe transactions pending).
            // Linux budget: 100 × 100 µs = 10 ms.
            mmio_write(self.base, E1000E_CTRL,
                mmio_read(self.base, E1000E_CTRL) | CTRL_GIO_MASTER_DISABLE);
            let mut gio_wait = 100u32;
            while gio_wait > 0 {
                if mmio_read(self.base, E1000E_STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
                    break;
                }
                Self::udelay(100);
                gio_wait -= 1;
            }
            if gio_wait == 0 {
                crate::klog_warn!("[e1000e] I219 skip-reset: GIO master still active after 10ms\n");
            }
            // Re-clear GIO_MASTER_DISABLE so normal DMA can resume after init.
            mmio_write(self.base, E1000E_CTRL,
                mmio_read(self.base, E1000E_CTRL) & !CTRL_GIO_MASTER_DISABLE);
            let _ = mmio_read(self.base, E1000E_CTRL);

            mmio_write(self.base, E1000E_WUC, 0);
            mmio_write(self.base, E1000E_WUFC, 0);
            mmio_write(self.base, E1000E_WUS, 0xFFFF_FFFF);
            let ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
            mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext | (1 << 28));
            // C8: Skipping PHY_RST as well. Linux only does it if link is down or on errors.
            // Keeping BIOS PHY state often results in a faster and more reliable link-up.
            self.pch_clear_status_phyra_if_set();
        } else {
        let ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext | (1 << 28));

        // 3. Issue global reset (RST bit in CTRL).

        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(self.base, E1000E_CTRL, ctrl | CTRL_GIO_MASTER_DISABLE);
        let mut master_wait = 500; // 500 * 100us = 50ms budget
        while master_wait > 0 {
            if mmio_read(self.base, E1000E_STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
                break;
            }
            Self::udelay(100); // C1: allow LAPIC timer to advance on bare-metal
            master_wait -= 1;
        }


        ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(self.base, E1000E_CTRL, ctrl | CTRL_RST);

        // Wait for RST self-clear BEFORE touching any other register.
        let mut rst_wait = 1_000; // 1000 * 100us = 100ms budget
        while rst_wait > 0 {
            if mmio_read(self.base, E1000E_CTRL) & CTRL_RST == 0 {
                break;
            }
            Self::udelay(100); // C2: allow LAPIC timer to advance on bare-metal
            rst_wait -= 1;
        }
        // Minimum post-reset silence before any MMIO (datasheet §4.6.3)
        Self::udelay(10_000);

        // Disable Wake-on-LAN now that the reset has completed.
        mmio_write(self.base, E1000E_WUC, 0);
        mmio_write(self.base, E1000E_WUFC, 0);
        mmio_write(self.base, E1000E_WUS, 0xFFFF_FFFF); // W1C: clear any pending WUS bits



        // 3. Poll STATUS until the device is ready.
        // 0xFFFF_FFFF means the PCIe config space is not responding (device
        // absent or bus error). Any other value — including 0 — means the
        // MAC register file is accessible and we can proceed.
        // STATUS_POLL_US = 150ms is the budget for PCH-based NICs (I219).
        let mut ready = false;
        let mut status_poll_tries = (STATUS_POLL_US / 1_000).max(1);
        while status_poll_tries > 0 {
            let s = mmio_read(self.base, E1000E_STATUS);
            if s != 0xFFFF_FFFF {
                ready = true;
                break;
            }
            Self::udelay(1_000);
            status_poll_tries -= 1;
        }
        if !ready {
            warn!("[e1000e] STATUS still 0xFFFFFFFF after {}ms — device not responding", STATUS_POLL_US / 1000);
            return Err(DeviceError::IoError);
        }

        let ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext | (1 << 28));
        }

        // Detect the active PHY address on the MDIO bus.
        self.detect_phy_addr();

        // 4. Linux Workarounds for I219-V (SPT)
        // NOTE: The is_pch_lpt_or_later() block below (step 6) already applies
        // all necessary FEXTNVM6/FEXTNVM7 workarounds with the correct named
        // constants. The old SPT-only block used raw offsets that mapped to
        // EECD (0x0010) and FEXTNVM4 (0x00E4) — corrupting those registers on
        // real hardware. It has been intentionally removed.

        // 5. Disable interrupts (RST clears IMC, re-enable needed explicitly later)
        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);

        // 5a. Recover the real MAC address FIRST (NVM or HW), THEN write RAL/RAH.
        // Writing RAL/RAH with a still-zero mac[] (from before NVM read) and
        // then overwriting it again later is harmless on QEMU but causes the
        // MAC filter to briefly accept everything on real hardware, which can
        // cause stray frames to fill the RX ring before init finishes.
        if !mac_found {
            // PCH-SPT (I219) and later do NOT use EERD for NVM reads — they use a
            // proprietary flash/firmware mechanism that requires acquiring a firmware
            // semaphore and talking to the CSME. Calling nvm_read_word() on these
            // chips always times out (200 iter × 50µs × 2 attempts = 20ms wasted)
            // and returns 0, which is not the real MAC.
            // For these chips, the BIOS always programs RAL0/RAH0, so the pre-reset
            // read_mac_from_hw() above should already have mac_found = true.
            // We only fall back to EERD-based NVM for discrete silicon (82574L etc.).
            if !self.is_pch_spt_or_later() {
                self.read_mac_from_nvm();
                mac_found = self.is_valid_mac();
            } else {
                // For I219 and later: re-read RAL0/RAH0 post-reset.
                // The reset may have reloaded the NVM shadow registers from flash.
                self.read_mac_from_hw();
                mac_found = self.is_valid_mac();
            }
        }
        if !mac_found {
            self.read_mac_from_hw();
            mac_found = self.is_valid_mac();
        }
        if !mac_found {
            self.mac = E1000E_PLACEHOLDER_MAC;
            warn!("[e1000e] using fallback MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                self.mac[0], self.mac[1], self.mac[2], self.mac[3], self.mac[4], self.mac[5]);
            self.warn_mac_diagnostic();
        }

        // 5b. Write the resolved MAC into RAL0/RAH0 with AV bit.
        let mac_low = u32::from_le_bytes([self.mac[0], self.mac[1], self.mac[2], self.mac[3]]);
        let mac_high = u32::from_le_bytes([self.mac[4], self.mac[5], 0, 0]);
        mmio_write(self.base, E1000E_RAL0, mac_low);
        mmio_write(self.base, E1000E_RAH0, mac_high | 0x80000000); // AV bit
        // Clear all other receive address slots.
        // Each slot is 8 bytes = 2 u32 dwords: RAL[i] @ RAL0+i*2, RAH[i] @ RAL0+i*2+1.
        // C5: Using RAH0+i*2 was incorrect — RAH0 = RAL0+1, so RAH0+i*2 != RAL0+i*2+1
        // for i>0. The correct stride keeps RAL and RAH within the same 8-byte slot.
        for i in 1usize..16 {
            mmio_write(self.base, E1000E_RAL0 + i * 2,     0); // RAL[i]
            mmio_write(self.base, E1000E_RAL0 + i * 2 + 1, 0); // RAH[i] = RAL[i]+1
        }

        // 6. Basic MAC configuration
        let ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(
            self.base,
            E1000E_CTRL,
            (ctrl | CTRL_SLU | CTRL_ASDE | CTRL_FD) & !(CTRL_TFCE | CTRL_RFCE | CTRL_VME | CTRL_GIO_MASTER_DISABLE),
        );

        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext |= 1 << 22; // PBA_CLR
        ctrl_ext |= 1 << 31; // PBA_SUPPORT (I219)
        ctrl_ext |= 1 << 28; // INT_TIMER_CLR
        if self.is_pch_lpt_or_later() {
            // Linux sets PHYPDEN for D3 low-power; on several I219 systems that
            // keeps STATUS.LU cleared after driver init without a full MAC reset.
            ctrl_ext &= !CTRL_EXT_PHYPDEN;
        }
        ctrl_ext |= CTRL_EXT_RO_DIS;
        ctrl_ext &= !CTRL_EXT_DPG_EN;
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);

        if self.is_pch_lpt_or_later() {
            // PBA: 26K RX, 18K TX (PCH default)
            mmio_write(self.base, E1000E_PBA, 0x0012001A);
        } else {
            mmio_write(self.base, E1000E_PBA, 0x00100030);
        }

        // PCH workarounds required on real I219 even in conventional profile (Linux ich8lan).
        if self.is_pch_lpt_or_later() {
            self.pch_apply_silicon_workarounds();
            self.pch_disable_lplu_gbe();

            // Force K1 disabled and lock PLL clock gating to GbE active from the first millisecond of bring-up
            self.pch_disable_k1();
            let phy_addr = self.active_phy_addr();
            let pll_reg = Self::phy_reg_paged(772, 28);
            if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, pll_reg) {
                phy_reg &= !I217_PLL_CLOCK_GATE_MASK;
                phy_reg |= 0xFA; // Force GbE clock active
                let _ = self.mdic_write_phy(phy_addr, pll_reg, phy_reg);
            }
        }

        if !E1000E_CONVENTIONAL && self.is_pch_lpt_or_later() {
            mmio_write(self.base, E1000E_CRC_OFFSET, 0x65656565);
            let kabgtxd = mmio_read(self.base, E1000E_KABGTXD);
            mmio_write(self.base, E1000E_KABGTXD, kabgtxd | KABGTXD_BGSQLBIAS);

            let mut fextnvm6 = mmio_read(self.base, E1000E_FEXTNVM6);
            fextnvm6 |= FEXTNVM6_K1_OFF_EN | FEXTNVM6_DIS_ELDW;
            mmio_write(self.base, E1000E_FEXTNVM6, fextnvm6);

            let mut kmrn = self.kmrn_read(KMRNCTRLSTA_K1_CONFIG);
            kmrn &= !KMRNCTRLSTA_K1_ENABLE;
            self.kmrn_write(KMRNCTRLSTA_K1_CONFIG, kmrn);

            let mut fextnvm11 = mmio_read(self.base, E1000E_FEXTNVM11);
            fextnvm11 |= FEXTNVM11_DISABLE_L1_2;
            mmio_write(self.base, E1000E_FEXTNVM11, fextnvm11);

            let mut fextnvm4 = mmio_read(self.base, E1000E_FEXTNVM4);
            fextnvm4 &= !FEXTNVM4_BEACON_DURATION_MASK;
            fextnvm4 |= FEXTNVM4_BEACON_DURATION_8USEC;
            mmio_write(self.base, E1000E_FEXTNVM4, fextnvm4);

            let mut fextnvm7 = mmio_read(self.base, E1000E_FEXTNVM7);
            fextnvm7 |= FEXTNVM7_SIDE_CLK_UNGATE
                | FEXTNVM7_DISABLE_SMB_PERST
                | FEXTNVM7_NEED_DESCR_RING_FLUSH;
            mmio_write(self.base, E1000E_FEXTNVM7, fextnvm7);

            let mut fextnvm9 = mmio_read(self.base, E1000E_FEXTNVM9);
            fextnvm9 |= FEXTNVM9_IOSFSB_CLKGATE_DIS | FEXTNVM9_IOSFSB_CLKREQ_DIS;
            mmio_write(self.base, E1000E_FEXTNVM9, fextnvm9);

            if matches!(self.device_id, 0x156f..=0x1570 | 0x15b7..=0x15be) {
                let iosfpc = mmio_read(self.base, E1000E_IOSFPC);
                mmio_write(self.base, E1000E_IOSFPC, iosfpc | 0x00010000);
            }
        }

        if !E1000E_CONVENTIONAL {
            let mut tarc0 = mmio_read(self.base, E1000E_TARC0);
            tarc0 |= (1 << 23) | (1 << 24) | (1 << 26) | (1 << 27);
            mmio_write(self.base, E1000E_TARC0, tarc0);

            let mut tarc1 = mmio_read(self.base, E1000E_TARC1);
            tarc1 |= (1 << 24) | (1 << 26) | (1 << 30) | (1 << 28);
            mmio_write(self.base, E1000E_TARC1, tarc1);
        }

        // 7. RAL0/RAH0 already written above (step 5b) with the correct MAC.
        // No second write needed; keeping the block for reference only.
        let _ = mac_found; // suppress unused-variable warning

        // 8. Initialize MTA — Linux clears to 0 (hash-based multicast filter disabled).
        // Do not fill 0xFFFF_FFFF on I219: it accepts all multicast and floods the 256-slot
        // RX ring under real LAN noise (mDNS/SSDP), driving MPC up and starving DHCP/TCP.
        for i in 0..E1000E_MTA_LEN {
            mmio_write(self.base, E1000E_MTA_BASE + i, 0);
        }

        // 9. Initialize Rings
        let rx_ring = self.rx_ring.as_ptr::<RxDesc>();
        let tx_ring = self.tx_ring.as_ptr::<TxDesc>();
        core::ptr::write_bytes(rx_ring, 0, NUM_RX);
        core::ptr::write_bytes(tx_ring, 0, NUM_TX);
        for i in 0..NUM_RX {
            let desc = &mut *rx_ring.add(i);
            desc.addr = self.rx_buf_paddr(i);
            core::arch::x86_64::_mm_clflush(desc as *const RxDesc as *const u8);
        }
        core::arch::x86_64::_mm_sfence();
        fence(Ordering::SeqCst);

        // 10. Configure TX
        let tx_ring_pa = self.tx_ring.paddr();
        mmio_write(self.base, E1000E_TDBAL, tx_ring_pa as u32);
        mmio_write(self.base, E1000E_TDBAH, (tx_ring_pa >> 32) as u32);
        mmio_write(self.base, E1000E_TDLEN, (NUM_TX * size_of::<TxDesc>()) as u32);
        mmio_write(self.base, E1000E_TDH, 0);
        mmio_write(self.base, E1000E_TDT, 0);
        mmio_write(self.base, E1000E_TIPG, 8 | (8 << 10) | (12 << 20));
        mmio_write(self.base, E1000E_TIDV, 0);
        mmio_write(self.base, E1000E_TADV, 0);

        // TXDCTL and Queue Enable
        if self.is_pch_lpt_or_later() {
            let txdctl = self.program_txdctl_linux();
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
        } else {
            // 82574/QEMU: FULL_TX_DESC_WB so status.DD is written back (Linux e1000_txdctl).
            mmio_write(
                self.base,
                E1000E_TXDCTL,
                TXDCTL_DMA_BURST | TXDCTL_FULL_TX_DESC_WB,
            );
        }
        if self.is_pch_spt_or_later() {
            // PCH-SPT (I219+) requires explicit QUEUE_ENABLE (bit 25) to start TX DMA.
            let txdctl = mmio_read(self.base, E1000E_TXDCTL);
            mmio_write(self.base, E1000E_TXDCTL, txdctl | TXDCTL_QUEUE_ENABLE);
            let mut txq_wait = 100; // 100 * 100us = 10ms
            while txq_wait > 0 && mmio_read(self.base, E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE == 0 {
                Self::udelay(100); // C3: allow timer to tick on bare-metal
                txq_wait -= 1;
            }
        }
        if self.is_pch_spt_or_later() {
            mmio_write(
                self.base,
                E1000E_TXDCTL1,
                mmio_read(self.base, E1000E_TXDCTL),
            );
        }
        {
            let mut tctl = mmio_read(self.base, E1000E_TCTL);
            if self.is_pch_spt_or_later() {
                // Leave TCTL_EN off until link-up + restart_tx_datapath_linux (Linux order).
                tctl &= !(0xFF0u32 | 0x003FF000u32 | TCTL_EN);
                tctl |= TCTL_PSP | TCTL_RTLC | TCTL_CT_LINUX | TCTL_COLD_LINUX;
            } else if E1000E_CONVENTIONAL {
                tctl |= TCTL_EN | TCTL_PSP;
            } else {
                tctl &= !(0xFF0u32 | 0x003FF000u32);
                tctl |= TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT_LINUX | TCTL_COLD_LINUX;
            }
            mmio_write(self.base, E1000E_TCTL, tctl);
            let _ = mmio_read(self.base, E1000E_TCTL);
        }

        // M5: MTA was already cleared above (step 8). Remove duplicate.
        
        // Signal driver loaded
        let ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext | (1 << 28));

        // 11. Configure RX — register order aligned with Linux e1000_configure_rx:
        // RDTR/RADV/ITR → IAME/IAM → RDBAL/RDBAH/RDLEN/RDH/RDT → RXCSUM/RFCTL/…
        mmio_write(self.base, E1000E_RDTR, 0);
        mmio_write(self.base, E1000E_RADV, 0);
        mmio_write(self.base, E1000E_ITR, 0);
        // Linux e1000_configure_rx: IAME + IAM mask all sources.
        {
            let mut ce = mmio_read(self.base, E1000E_CTRL_EXT);
            ce |= CTRL_EXT_IAME;
            mmio_write(self.base, E1000E_IAM, 0xFFFF_FFFF);
            mmio_write(self.base, E1000E_CTRL_EXT, ce);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
        }

        let rx_ring_pa = self.rx_ring.paddr();
        mmio_write(self.base, E1000E_RDBAL, rx_ring_pa as u32);
        mmio_write(self.base, E1000E_RDBAH, (rx_ring_pa >> 32) as u32);
        mmio_write(self.base, E1000E_RDLEN, (NUM_RX * size_of::<RxDesc>()) as u32);
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;

        mmio_write(self.base, E1000E_RXCSUM, 0);
        // Linux e1000_setup_rctl: EXTEN on all e1000e (required on I219 real HW).
        {
            let mut rfctl = mmio_read(self.base, E1000E_RFCTL);
            rfctl |= RFCTL_EXTEN | RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
            mmio_write(self.base, E1000E_RFCTL, rfctl);
            let rfctl_rd = mmio_read(self.base, E1000E_RFCTL);
            if rfctl_rd & RFCTL_EXTEN == 0 {
                crate::klog_warn!("[e1000e] RFCTL EXTEN missing after write! ({:#x})\n", rfctl_rd);
                self.use_extended_descriptors = false;
            } else {
                self.use_extended_descriptors = true;
            }
        }
        mmio_write(self.base, E1000E_MRQC, 0);
        mmio_write(self.base, E1000E_VET, 0);
        
        unsafe {
            self.program_srrctl_rx_queue0();
            crate::klog_info!(
                "[e1000e] SRRCTL queue 0 = {:#x} (2 KB, ext-desc, Drop_En)\n",
                mmio_read(self.base, E1000E_SRRCTL)
            );
        }

        if self.is_pch_lpt_or_later() {
            mmio_write(self.base, E1000E_FCTTV, 0xFFFF);
            mmio_write(self.base, E1000E_FCRTV, 0xFFFF);
            mmio_write(self.base, E1000E_FCRTL, 0x05048);
            mmio_write(self.base, E1000E_FCRTH, 0x05C20);
        }

        if self.is_pch_lpt_or_later() {
            self.pch_setup_kmrn_copper_link();
        }

        // Linux enables RCTL.EN in setup_rctl before link is up.
        {
            let mut manc = mmio_read(self.base, E1000E_MANC);
            manc |= MANC_EN_MNG2HOST;
            mmio_write(self.base, E1000E_MANC, manc);
        }

        // Program RCTL filters but leave RCTL.EN off until arm_rx_unit_linux() posts the ring.
        mmio_write(self.base, E1000E_RCTL, self.rctl_rx_bits() & !RCTL_EN);

        // Disable VLAN filtering
        mmio_write(self.base, E1000E_VET, 0);
        for i in 0..128 { mmio_write(self.base, E1000E_VFTA_BASE + i, 0); } // Clear VFTA table
        let rctl_v = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl_v & !RCTL_VFE);

        self.mac_setup_copper_link_linux();

        // Disable EEE
        mmio_write(self.base, 0x0E30 / 4, 0);

        let _ = mmio_read(self.base, E1000E_ICR);
        mmio_write(self.base, E1000E_IMS, IMS_REARM_LINUX);

        // 13. Link — do not block boot on multi-second PHY settle (BusyBox shell).
        self.link_bringup_pending = true;
        self.link_bringup_stage = 0;
        self.stage_start_us = timer_now_as_micros();
        self.link_bringup_attempts = 0;
        self.get_link_status = true;
        self.link_up = false;
        self.link_speed = 0;
        self.link_duplex = false;
        self.rx_poll_budget = 32;
        unsafe {
            self.qemu_finish_link_if_up();
            self.pch_try_early_link();
        }
        Ok(())
    }

    /// PCH/I219: if cable is already up at probe time, finish link/RX/TX without waiting for bringup ticks.
    unsafe fn pch_try_early_link(&mut self) {
        if self.device_id == 0x10d3 || !self.is_pch_lpt_or_later() {
            return;
        }
        if !self.link_bringup_pending {
            return;
        }
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU == 0 {
            return;
        }
        let phy = self.phy_addr;
        let _ = self.mdic_read(phy, MII_BMSR);
        let bmsr = self.mdic_read(phy, MII_BMSR).unwrap_or(0);
        if bmsr == 0 || bmsr == 0xFFFF || (bmsr & 0x0004) == 0 || (bmsr & BMSR_ANEG_COMPLETE) == 0 {
            return;
        }
        if self.check_for_link_linux() {
            self.link_bringup_pending = false;
            self.link_bringup_stage = 0;
            self.link_bringup_attempts = 0;
            crate::klog_info!(
                "[e1000e] PCH early link: {} Mb/s {} duplex\n",
                self.link_speed,
                if self.link_duplex { "full" } else { "half" }
            );
        }
    }

    /// QEMU e1000 (0x10d3): trust STATUS.LU, skip multi-second PHY MDIO bringup.
    unsafe fn qemu_finish_link_if_up(&mut self) {
        if self.device_id != 0x10d3 {
            return;
        }
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU == 0 {
            return;
        }
        self.link_bringup_pending = false;
        if self.link_up && self.rx_link_armed {
            return;
        }
        self.link_up = true;
        self.link_speed = Self::speed_mbps_from_status(status);
        self.link_duplex = status & STATUS_FD != 0;
        self.config_collision_dist_linux();
        if !self.rx_link_armed {
            self.enable_rx_after_link();
        }
        crate::klog_info!(
            "[e1000e] QEMU fast link: {} Mb/s {} duplex\n",
            self.link_speed,
            if self.link_duplex { "full" } else { "half" }
        );
    }

    unsafe fn restore_ctrl_autoneg_after_link(&self) {
        self.mac_setup_copper_link_linux();
    }

    /// Re-post RX descriptors (addr valid, write-back region cleared).
    ///
    /// On Write-Back memory, clflush is mandatory so the CPU evicts the cache lines
    /// containing the new buffer addresses and zeroed status to physical RAM.
    /// Without clflush, the NIC's descriptor fetch DMA reads stale values from RAM.
    unsafe fn reinit_rx_ring(&mut self) {
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        for i in 0..NUM_RX {
            let desc = &mut *ring.add(i);
            // Write buffer address (CPU→HW field).
            write_volatile(&mut desc.addr, self.rx_buf_paddr(i));
            // Zero the WB region (HW→CPU field) so old DD bits don't linger.
            write_volatile((desc as *mut RxDesc as usize + 8) as *mut u64, 0);

            // Invalidate the cache for the packet buffer on physical hardware.
            if self.rx_needs_cache_flush() {
                Self::invalidate_cpu_cache_for_read(self.rx_buf_vaddr(i), BUF_SIZE);
                core::arch::x86_64::_mm_clflush(desc as *const RxDesc as *const u8);
            }
        }
        if self.rx_needs_cache_flush() {
            core::arch::x86_64::_mm_sfence();
            fence(Ordering::SeqCst);
        }
    }



    /// Hardware MIB counters (Linux `e1000e_update_stats`). Clear-on-read.
    unsafe fn read_hw_stats(&self) -> NetStats {
        let rx_packets = mmio_read(self.base, E1000E_GPRC) as u64;
        let tx_packets = mmio_read(self.base, E1000E_GPTC) as u64;
        let rx_bytes =
            (mmio_read(self.base, E1000E_GORCL) as u64) | ((mmio_read(self.base, E1000E_GORCH) as u64) << 32);
        let tx_bytes =
            (mmio_read(self.base, E1000E_GOTCL) as u64) | ((mmio_read(self.base, E1000E_GOTCH) as u64) << 32);
        let mpc = mmio_read(self.base, E1000E_MPC) as u64;
        NetStats {
            rx_bytes,
            rx_packets,
            tx_bytes,
            tx_packets,
            rx_errors: 0,
            rx_dropped: mpc,
            tx_errors: 0,
            tx_dropped: 0,
        }
    }

    fn merged_stats(&self) -> NetStats {
        self.stats.clone()
    }

    /// Returns (staterr, len) if an extended write-back descriptor is done.
    /// We always use RFCTL_EXTEN on I219, so only check the extended layout:
    ///   +8  u32 staterr  (DD=bit0, EOP=bit1)
    ///   +12 u16 length
    ///
    /// CRITICAL: On I219 bare-metal the descriptor region is Write-Back cached.
    /// The NIC writes via DMA into physical RAM; the CPU L1/L2 cache still holds
    /// the stale zero from when the driver posted the descriptor.  We MUST call
    /// clflush (invalidate_cpu_cache_for_read) BEFORE the first read of the WB
    /// region so the CPU fetches from RAM, not from its own stale cache line.
    /// The old order (read → clflush → read) left a window where the first read
    /// always returned 0 from cache, so packets were silently dropped even when
    /// the NIC had already written DD=1.  QEMU is coherent and does not expose
    /// this bug; real silicon does.
    unsafe fn desc_done(&self, desc_addr: usize) -> Option<(u32, usize)> {
        for attempt in 0..RX_DESC_WB_SETTLE_TRIES {
            let wb = unsafe { self.read_rx_wb_after_sync(desc_addr) };
            let parsed = if self.use_extended_descriptors {
                Self::parse_rx_wb_ext_u64(wb)
            } else {
                Self::parse_rx_wb_legacy_u64(wb)
            };
            match parsed {
                None => return None,
                Some((staterr, len)) if len > 0 && len <= BUF_SIZE => return Some((staterr, len)),
                Some((staterr, len)) if attempt + 1 < RX_DESC_WB_SETTLE_TRIES => {
                    log::trace!(
                        "[e1000e] desc_done WB settling staterr={:#x} len={} try={}",
                        staterr,
                        len,
                        attempt + 1
                    );
                    Self::udelay(RX_DESC_WB_SETTLE_US as u64);
                }
                Some((staterr, len)) => {
                    log::trace!(
                        "[e1000e] desc_done unstable WB staterr={:#x} len={} — skip slot",
                        staterr,
                        len
                    );
                    return None;
                }
            }
        }
        None
    }

    /// Nudge the MAC to write back completed RX descriptors (Linux RDTR_FPD path).
    unsafe fn kick_rx_writeback(&self) {
        mmio_write(self.base, E1000E_RDTR, RDTR_FPD);
        let _ = mmio_read(self.base, E1000E_RDTR);
        mmio_write(self.base, E1000E_RDTR, 0);
        let _ = mmio_read(self.base, E1000E_RDTR);
    }

    fn receive_slot(&mut self, i: usize) -> Option<Vec<u8>> {
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let desc_addr = unsafe { ring.add(i) as usize };
        let (staterr, len) = unsafe { self.desc_done(desc_addr)? };
        fence(Ordering::Acquire);
        log::trace!("[e1000e] RX: slot={} staterr={:#x} len={} clean={} RDH={} RDT={}",
              i, staterr, len, self.rx_next_to_clean,
              unsafe { mmio_read(self.base, E1000E_RDH) },
              unsafe { mmio_read(self.base, E1000E_RDT) });

        // Recycle descriptor regardless of EOP/error status.
        let recycle = |hw: &mut Self| {
            unsafe {
                write_volatile((desc_addr + 8) as *mut u64, 0);
                if hw.rx_needs_cache_flush() {
                    Self::dma_wbinv_range(desc_addr, core::mem::size_of::<RxDesc>());
                }
            }
            hw.rx_next_to_clean = (i + 1) % NUM_RX;
            unsafe {
                hw.alloc_rx_buffers(1, false);
            }
        };

        if len == 0 || len > BUF_SIZE {
            // Descriptor is done but length is implausible — discard and recycle.
            log::debug!(
                "[e1000e] RX slot {} bad len={} staterr={:#x} — discarding",
                i, len, staterr
            );
            self.rx_sg_reset();
            recycle(self);
            return None;
        }

        // Copy this fragment out of the DMA buffer before recycling the descriptor.
        let buf_vaddr = self.rx_buf_vaddr(i);
        let copy_len = unsafe { self.recover_rx_copy_len(buf_vaddr, len) };
        let mut frag = Vec::new();
        unsafe { self.dma_copy_in_rx_buffer(&mut frag, buf_vaddr, copy_len) };

        // Unified split-RX (QEMU + bare metal): buffer only real continuations, never glue
        // a new Ethernet header onto a stale partial (the old 60+381 B DHCP bug).
        let data = if staterr & RXD_EXT_EOP == 0 {
            if Self::rx_ipv4_deliverable(&frag) {
                log::trace!(
                    "[e1000e] RX slot {} EOP=0 deliverable ({} B)",
                    i,
                    frag.len()
                );
                self.rx_sg_reset();
                Self::trim_to_ipv4_frame(frag)
            } else if Self::is_eth_ipv4_header_start(&frag) {
                if !self.rx_sg_buf.is_empty() {
                    log::trace!(
                        "[e1000e] RX slot {} new frame start — drop partial ({} B)",
                        i,
                        self.rx_sg_buf.len()
                    );
                    self.rx_sg_reset();
                }
                log::trace!(
                    "[e1000e] RX slot {} fragment {} B EOP=0 — buffer head",
                    i,
                    frag.len()
                );
                if !self.rx_sg_append(&frag) {
                    recycle(self);
                    return None;
                }
                recycle(self);
                return None;
            } else if !self.rx_sg_buf.is_empty() {
                log::trace!(
                    "[e1000e] RX slot {} continuation {} B EOP=0 — buffer",
                    i,
                    frag.len()
                );
                if !self.rx_sg_append(&frag) {
                    recycle(self);
                    return None;
                }
                recycle(self);
                return None;
            } else {
                recycle(self);
                return None;
            }
        } else if !self.rx_sg_buf.is_empty() {
            if Self::is_eth_ipv4_header_start(&frag) {
                log::trace!(
                    "[e1000e] RX slot {} EOP=1 new frame — drop partial ({} B), deliver {} B",
                    i,
                    self.rx_sg_buf.len(),
                    frag.len()
                );
                self.rx_sg_reset();
                Self::trim_to_ipv4_frame(frag)
            } else {
                let mut assembled = core::mem::take(&mut self.rx_sg_buf);
                self.rx_sg_frag_count = 0;
                assembled.extend_from_slice(&frag);
                if Self::rx_ipv4_deliverable(&assembled) {
                    log::trace!(
                        "[e1000e] RX slot {} EOP: assembled {} B",
                        i,
                        assembled.len()
                    );
                    Self::trim_to_ipv4_frame(assembled)
                } else {
                    log::debug!(
                        "[e1000e] RX slot {} assembled {} B still incomplete — discard",
                        i,
                        assembled.len()
                    );
                    recycle(self);
                    return None;
                }
            }
        } else {
            Self::trim_to_ipv4_frame(frag)
        };

        if data.len() >= 20 {
            let ethertype = u16::from_be_bytes([data[12], data[13]]);
            if ethertype == 0x0800 {
                let b = &data;
                let ver_ihl = b[14];
                let ihl = ((ver_ihl & 0x0f) * 4) as usize;
                if ihl >= 20 && data.len() >= 14 + ihl {
                    let ip_tot_len = u16::from_be_bytes([b[16], b[17]]) as usize;
                    let expected = 14 + ip_tot_len;
                    if ip_tot_len >= ihl && expected > data.len() {
                        log::trace!(
                            "[e1000e] RX TRUNCATED slot={} desc_len={} frame={} ip_tot_len={} (need {} B) RCTL={:#x} SRRCTL={:#x}",
                            i,
                            len,
                            data.len(),
                            ip_tot_len,
                            expected,
                            unsafe { mmio_read(self.base, E1000E_RCTL) },
                            unsafe { mmio_read(self.base, E1000E_SRRCTL) }
                        );
                    }
                }
            } else if ethertype == 0x86dd && data.len() >= 14 + 40 {
                let payload_len = u16::from_be_bytes([data[16], data[17]]) as usize;
                let expected = 14 + 40 + payload_len;
                if expected > data.len() {
                    log::trace!(
                        "[e1000e] RX TRUNCATED IPv6 slot={} desc_len={} frame={} payload_len={} (need {} B)",
                        i,
                        len,
                        data.len(),
                        payload_len,
                        expected
                    );
                }
            }
        }

        if !Self::rx_frame_deliverable(&data) {
            log::trace!(
                "[e1000e] RX slot {} incomplete L3 frame ({} B) — discard",
                i,
                data.len()
            );
            recycle(self);
            return None;
        }

        recycle(self);

        self.stats.rx_packets += 1;
        self.stats.rx_bytes += data.len() as u64;
        if self.stats.rx_packets == 1 {
            let gprc = unsafe { mmio_read(self.base, E1000E_GPRC) };
            self.last_hw_rx_packets = self.last_hw_rx_packets.wrapping_add(gprc);
            log::info!(
                "[e1000e] first RX frame {} bytes staterr={:#x} GPRC={} RDT={} RDH={}",
                data.len(),
                staterr,
                gprc,
                unsafe { mmio_read(self.base, E1000E_RDT) },
                unsafe { mmio_read(self.base, E1000E_RDH) }
            );
        }
        Some(data)
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        if self.rx_poll_budget == 0 {
            return None;
        }
        if self.is_i219_metal_rx_hacks() {
            unsafe { self.kick_rx_writeback() };
        }

        // Loop to collect SG fragments; cap work per call so a stuck EOP=0 storm
        // cannot burn the whole ring and starve the scheduler.
        let max_frags = NUM_RX;
        let mut sg_slots = RX_SG_SLOTS_PER_CALL;
        for _ in 0..max_frags {
            let i = self.rx_next_to_clean;
            if let Some(frame) = self.receive_slot(i) {
                self.rx_poll_budget = self.rx_poll_budget.saturating_sub(1);
                unsafe { self.flush_rx_post_queue() };
                return Some(frame);
            }
            if self.rx_sg_buf.is_empty() {
                break;
            }
            sg_slots = sg_slots.saturating_sub(1);
            self.rx_poll_budget = self.rx_poll_budget.saturating_sub(1);
            if sg_slots == 0 {
                log::trace!(
                    "[e1000e] RX SG budget exhausted ({} B partial) — resume next poll",
                    self.rx_sg_buf.len()
                );
                break;
            }
        }
        unsafe { self.flush_rx_post_queue() };
        None
    }

    /// Free TX ring slots (Linux `e1000_desc_unused` using TDH vs next_to_use).
    fn tx_desc_unused(&self) -> usize {
        let head = unsafe { mmio_read(self.base, E1000E_TDH) as usize };
        let tail = self.tx_tail;
        if tail >= head {
            NUM_TX.saturating_sub(tail - head).saturating_sub(1)
        } else {
            head.saturating_sub(tail).saturating_sub(1)
        }
    }

    // -----------------------------------------------------------------------
    // Check if a TX slot is available
    // -----------------------------------------------------------------------
    fn can_send(&self) -> bool {
        self.tx_desc_unused() > 0
    }

    // -----------------------------------------------------------------------
    // Send one frame
    // -----------------------------------------------------------------------
    pub fn send(&mut self, data: &[u8]) -> DeviceResult {
        if data.len() < 14 {
            return Err(DeviceError::InvalidParam);
        }
        let eth_type = u16::from_be_bytes([data[12], data[13]]);
        let info = match eth_type {
            0x0806 => "ARP",
            0x0800 => {
                let proto = if data.len() >= 24 { data[23] } else { 0 };
                match proto {
                    1 => "IPv4-ICMP",
                    6 => "IPv4-TCP",
                    17 => "IPv4-UDP",
                    _ => "IPv4-Other",
                }
            }
            0x86dd => "IPv6",
            _ => "Other",
        };

        warn!("[e1000e] TX: {} ({} bytes)", info, data.len());

        if !self.can_send() {
            return Err(DeviceError::NotReady);
        }
        if data.is_empty() || data.len() > BUF_SIZE {
            return Err(DeviceError::InvalidParam);
        }
        if !self.link_up {
            unsafe {
                let status = mmio_read(self.base, E1000E_STATUS);
                if status & STATUS_LU != 0 {
                    if self.is_pch_lpt_or_later() {
                        let _ = self.check_for_link_linux();
                    } else {
                        self.link_up = true;
                        self.link_speed = Self::speed_mbps_from_status(status);
                        self.link_duplex = status & STATUS_FD != 0;
                    }
                }
            }
            if !self.link_up {
                return Err(DeviceError::NotReady);
            }
        }
        if !unsafe { self.ensure_tx_engine_ready() } {
            crate::klog_warn!("[e1000e] send: TX engine not ready (TCTL/TXDCTL/GIO) despite link up\n");
            return Err(DeviceError::NotReady);
        }

        let ring = self.tx_ring.as_ptr::<TxDesc>();
        let idx = self.tx_tail;
        let desc = unsafe { &mut *ring.add(idx) };

        let buf =
            unsafe { core::slice::from_raw_parts_mut(self.tx_buf_vaddr(idx) as *mut u8, data.len()) };
        buf.copy_from_slice(data);


        let first_tx = self.stats.tx_packets == 0;
        self.stats.tx_packets += 1;
        self.stats.tx_bytes += data.len() as u64;

        unsafe {
            write_volatile(&mut desc.addr, self.tx_buf_paddr(idx));
            write_volatile(&mut desc.len, data.len() as u16);
            write_volatile(&mut desc.cso, 0);
            write_volatile(&mut desc.cmd, TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS);
            write_volatile(&mut desc.status, 0);
            write_volatile(&mut desc.css, 0);
            write_volatile(&mut desc.special, 0);
        }
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);

        if self.rx_needs_cache_flush() {
            unsafe {
                Self::dma_wbinv_range(self.tx_buf_vaddr(idx), data.len());
                Self::dma_wbinv_range(desc as *const TxDesc as usize, core::mem::size_of::<TxDesc>());
                core::arch::x86_64::_mm_sfence();
            }
        } else {
            compiler_fence(Ordering::SeqCst);
        }

        self.tx_tail = (idx + 1) % NUM_TX;
        compiler_fence(Ordering::SeqCst);
        unsafe {
            mmio_write(self.base, E1000E_TDT, self.tx_tail as u32);
            let _ = mmio_read(self.base, E1000E_TDT);
        }

        // Hot path: TX completion is driven by TXDW interrupt / later poll — do not spin here.
        if first_tx {
            let mut tx_dd = false;
            for _ in 0..100 {
                unsafe {
                    Self::dma_rmb_after_device();
                    if self.rx_needs_cache_flush() {
                        Self::invalidate_cpu_cache_for_read(
                            ring.add(idx) as usize,
                            core::mem::size_of::<TxDesc>(),
                        );
                    }
                    let status = read_volatile(&(*ring.add(idx)).status);
                    if status & 0x01 != 0 {
                        tx_dd = true;
                        break;
                    }
                }
                Self::udelay(20);
            }
            if tx_dd {
                crate::klog_info!("[e1000e] first TX {} ({} bytes) DD ok\n", info, data.len());
            } else {
                e1000e_vlog!(
                    "[e1000e] first TX {} ({} bytes) queued — DD pending (async TXDW)\n",
                    info,
                    data.len()
                );
            }
        }

        Ok(())
    }

}

impl Drop for E1000eHw {
    fn drop(&mut self) {
        // DmaRegion handles its own deallocation
    }
}

// ---------------------------------------------------------------------------
// Public driver wrapper
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
    /// Avoid queueing multiple background link-bringup jobs at once.
    bringup_job_scheduled: Arc<AtomicBool>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl E1000eInterface {
    /// Runs chunked PHY recovery without requiring userspace `poll_ifaces()`.
    fn run_link_bringup_slices(&self, max_slices: u32) {
        for _ in 0..max_slices {
            if !self.driver.hw.lock().link_bringup_pending {
                break;
            }
            let irq_was_on = intr_get();
            if irq_was_on {
                intr_off();
            }
            {
                let mut hw = self.driver.hw.lock();
                if hw.link_bringup_pending {
                    unsafe { hw.deferred_link_bringup_tick() };
                }
            }
            if irq_was_on {
                intr_on();
            }
        }
    }

    /// Queue background link recovery (idle loop / deferred_job drain).
    pub fn schedule_link_bringup_poll(&self) {
        if !self.driver.hw.lock().link_bringup_pending {
            return;
        }
        if self
            .bringup_job_scheduled
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            me.bringup_job_scheduled
                .store(false, Ordering::Release);
            me.run_link_bringup_slices(LINK_BRINGUP_SLICES_PER_JOB);
            if me.driver.hw.lock().link_bringup_pending {
                me.schedule_link_bringup_poll();
            }
        });
    }

    fn ims_rearm(&self) {
        unsafe {
            compiler_fence(Ordering::SeqCst);
            mmio_write(self.base, E1000E_IMS, IMS_REARM_LINUX);
            let _ = mmio_read(self.base, E1000E_IMS);
            fence(Ordering::SeqCst);
        }
    }
}

impl Scheme for E1000eInterface {
    fn name(&self) -> &str {
        "e1000e"
    }

    fn handle_irq(&self, irq: usize) {
        if irq != self.irq {
            return;
        }

        // ICR is read-to-clear; with CTRL_EXT.IAME a read auto-masks IMS even when icr==0
        // (shared/spurious IRQ). Re-arm IMS immediately to shrink the interrupt mute window.
        let icr = unsafe { mmio_read(self.base, E1000E_ICR) };
        log::trace!("[e1000e] handle_irq: irq={}, icr={:#x}", irq, icr);
        self.ims_rearm();

        if icr == 0 {
            return;
        }

        if icr & ICR_LSC != 0 {
            if let Some(mut hw) = self.driver.hw.try_lock() {
                hw.get_link_status = true;
            }
        }

        let needs_poll = icr & (ICR_RX_ANY | ICR_TXDW) != 0;
        if needs_poll && !self.poll_pending.load(Ordering::Acquire) {
            self.poll_pending.store(true, Ordering::SeqCst);
            let poll_pending = self.poll_pending.clone();
            let self_clone = self.clone();
            crate::utils::deferred_job::push_deferred_job(move || {
                let _ = self_clone.poll();
                poll_pending.store(false, Ordering::SeqCst);
            });
        }

        // Belt-and-suspenders: TXDW/RXT0 may assert while deferred poll is still queued.
        self.ims_rearm();
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
        e1000e_vlog!("{}: IPv4 address set to {}", self.name, cidr);
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
        self.iface
            .lock()
            .seed_neighbor(protocol, hardware, ts);
        Ok(())
    }
    
    fn poll(&self) -> DeviceResult {
        let ts = Instant::from_micros(timer_now_as_micros() as i64);
        let sockets = get_sockets();

        let mut trigger_check = false;
        {
            let mut hw = self.driver.hw.lock();
            if !hw.link_bringup_pending {
                let now = timer_now_as_micros();
                if hw.get_link_status || now.wrapping_sub(hw.last_link_check_us) >= 2_000_000 {
                    hw.last_link_check_us = now;
                    trigger_check = true;
                }
            }
        }

        if trigger_check {
            let mut hw = self.driver.hw.lock();
            unsafe {
                let prev_up = hw.link_up;
                let is_up = hw.check_for_link_linux();
                if prev_up && !is_up {
                    // Link transitioned DOWN! Start bringup recovery
                    hw.link_bringup_pending = true;
                    hw.link_bringup_stage = 0;
                    hw.stage_start_us = timer_now_as_micros();
                    hw.link_bringup_attempts = 0;
                    self.link_up_seen.store(false, Ordering::SeqCst);
                } else if !prev_up && is_up {
                    self.link_up_seen.store(true, Ordering::SeqCst);
                }
            }
        }

        self.run_link_bringup_slices(1);
        if self.driver.hw.lock().link_bringup_pending {
            self.schedule_link_bringup_poll();
        }
        crate::utils::deferred_job::drain_deferred_jobs();

        {
            let mut hw = self.driver.hw.lock();
            if hw.is_i219_metal_rx_hacks() {
                unsafe { hw.kick_rx_writeback() };
            }
        }

        // RX/TX: smoltcp (ping/ARP) + AF_PACKET dispatch in RxToken::consume.
        {
            let mut hw = self.driver.hw.lock();
            hw.rx_poll_budget = 32;
        }
        {
            let mut sockets = sockets.lock();
            let _ = self.iface.lock().poll(&mut sockets, ts);
        }
        // Wake any TCP read tasks that were sleeping waiting for RX data.
        // This replaces the 5 ms timer as the primary wake-up mechanism.
        super::wake_net_rx_waiters();
        self.driver.hw.lock().maybe_log_rx_diag();
        self.ims_rearm();
        Ok(())
    }
    fn recv(&self, buf: &mut [u8]) -> DeviceResult<usize> {
        let pkt = self.driver.hw.lock().receive();
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
        if hw.can_send() {
            hw.send(data)?;
            Ok(data.len())
        } else {
            warn!("[e1000e] send: hardware not ready");
            Err(DeviceError::NotReady)
        }
    }

    fn can_recv(&self) -> bool {
        // Return true so callers always attempt recv(); actual receive will return NotReady if nothing.
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
                IpCidr::Ipv4(_) => { let _ = iface.routes_mut().remove_default_ipv4_route(); }
                IpCidr::Ipv6(_) => { /* no simple remove_default_ipv6_route in smoltcp but tracked in routes */ }
                _ => {}
            }
        }
        self.routes.lock().retain(|r| r.dst != cidr);
        Ok(())
    }

    fn get_routes(&self) -> Vec<RouteInfo> {
        let iface = self.iface.lock();
        let mut res = Vec::new();
        
        // 1. Add tracked routes (including default gateway)
        res.extend(self.routes.lock().clone());
        
        // 2. Add direct routes for each assigned IP address
        for cidr in iface.ip_addrs() {
            match cidr {
                IpCidr::Ipv4(v4) => {
                    if v4.prefix_len() > 0 && v4.address().0[0] != 240 {
                        res.push(RouteInfo {
                            dst: IpCidr::Ipv4(v4.network()),
                            gateway: None,
                        });
                    }
                }
                IpCidr::Ipv6(v6) => {
                    if v6.prefix_len() > 0 {
                        res.push(RouteInfo {
                            dst: IpCidr::Ipv6(v6.network()),
                            gateway: None,
                        });
                    }
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
}

pub struct E1000eTxToken(E1000eDriver);

impl phy::Device<'_> for E1000eDriver {
    type RxToken = E1000eRxToken;
    type TxToken = E1000eTxToken;

    fn receive(&mut self) -> Option<(Self::RxToken, Self::TxToken)> {
        let mut hw = self.hw.lock();
        if let Some(pkt) = hw.receive() {
            // NOTE: net_dispatch_packet is intentionally NOT called here.
            // smoltcp owns this packet via the RxToken; calling dispatch would
            // send the same bytes to a raw-packet callback before smoltcp has
            // parsed/acknowledged them, causing DHCP/ARP processing races.
            // Raw-socket dispatch (if needed) should happen after smoltcp
            // processes the frame inside RxToken::consume.
            Some((
                E1000eRxToken { data: pkt },
                E1000eTxToken(self.clone()),
            ))
        } else {
            None
        }
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
    where
        F: FnOnce(&mut [u8]) -> SmolResult<R>,
    {
        let mut data = self.data;
        // Dispatch the raw Ethernet frame to AF_PACKET sockets (udhcpc, tcpdump, ping…)
        // BEFORE smoltcp processes it. smoltcp only mutates the slice in-place for
        // checksums on some TX paths; RX frames are never modified by smoltcp 0.8.
        // Dispatching here (not in Device::receive) ensures the bytes are available
        // to raw-socket readers regardless of whether smoltcp accepts or drops the frame.
        super::net_dispatch_packet(&data);
        f(&mut data)
    }
}

impl phy::TxToken for E1000eTxToken {
    fn consume<R, F>(self, _ts: Instant, len: usize, f: F) -> SmolResult<R>
    where
        F: FnOnce(&mut [u8]) -> SmolResult<R>,
    {
        const MAX_TX_COPY: usize = 65536;
        let len = len.min(MAX_TX_COPY);
        let mut buf = vec![0u8; len];
        // NOTE: do NOT call net_dispatch_packet here. The buffer is empty at this point
        // (smoltcp fills it via the closure below). Dispatching it as a received packet
        // would inject garbage frames into AF_PACKET sockets.
        let result = f(&mut buf)?;
        let mut hw = self.0.hw.lock();
        hw.send(&buf).map_err(|_| smoltcp::Error::Exhausted)?;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Public init — called from pci.rs
// ---------------------------------------------------------------------------
pub fn init(
    name: String,
    pci: &PCIDevice,
    irq: usize,
    vaddr: usize,  // MMIO virtual base
    _index: usize, // card index for IP suffix
) -> DeviceResult<E1000eInterface> {
    info!(
        "[e1000e] probing {} at vaddr={:#x} irq={}",
        name, vaddr, irq
    );

    // Allocate DMA rings (page-aligned contiguous frames → also 16-byte aligned bases).
    let rx_ring = DmaRegion::alloc(NUM_RX * size_of::<RxDesc>()).ok_or(DeviceError::DmaError)?;
    let tx_ring = DmaRegion::alloc(NUM_TX * size_of::<TxDesc>()).ok_or(DeviceError::DmaError)?;

    let rx_buf_pool = DmaRegion::alloc_uninit(NUM_RX * BUF_SIZE).ok_or(DeviceError::DmaError)?;
    let tx_buf_pool = DmaRegion::alloc(NUM_TX * BUF_SIZE).ok_or(DeviceError::DmaError)?;

    for (name, region, align) in [
        ("rx_ring", &rx_ring, DMA_DESC_ALIGN),
        ("tx_ring", &tx_ring, DMA_DESC_ALIGN),
        ("rx_buf_pool", &rx_buf_pool, 64),
        ("tx_buf_pool", &tx_buf_pool, 64),
    ] {
        if region.vaddr() % align != 0 || region.paddr() % align != 0 {
            crate::klog_err!(
                "[e1000e] {} DMA misaligned v={:#x} p={:#x} (need {} B)\n",
                name,
                region.vaddr(),
                region.paddr(),
                align
            );
            return Err(DeviceError::DmaError);
        }
    }

    let mut hw = E1000eHw {
        base: vaddr,
        pci_loc: pci.loc,
        device_id: pci.id.device_id,
        mac: [0u8; 6], // Read from hardware during reset
        rx_ring,
        rx_buf_pool,
        rx_next_to_clean: 0,
        rx_next_to_use: 0,
        tx_ring,
        tx_buf_pool,
        tx_tail: 0,
        phy_addr: 1, // Default to 1, updated during probe
        stats: NetStats::default(),
        last_hw_rx_packets: 0,
        rx_diag_counter: 0,
        rx_poll_budget: 32,
        link_bringup_pending: false,
        link_bringup_attempts: 0,
        link_bringup_stage: 0,
        rx_link_armed: false,
        link_10m_degraded: false,
        get_link_status: true,
        link_up: false,
        link_speed: 0,
        link_duplex: false,
        last_link_check_us: 0,
        stage_start_us: 0,
        use_extended_descriptors: true,
        rx_sg_buf: Vec::new(),
        rx_sg_frag_count: 0,
        rx_post_since_doorbell: 0,
        srrctl_absent: false,
        dma_uncached: false,
    };

    hw.setup_dma_uncached();

    unsafe {
        hw.reset_and_init()?;
    }
    hw.warn_mac_diagnostic();

    let mac_bytes = hw.mac;
    let link_note = if hw.link_bringup_pending {
        "pending"
    } else if unsafe { mmio_read(vaddr, E1000E_STATUS) & STATUS_LU != 0 } {
        "up"
    } else {
        "down"
    };
    crate::klog_info!(
        "e1000e: {} {:#x}:{:#x} {} tag={} MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        name,
        pci.id.vendor_id,
        pci.id.device_id,
        link_note,
        E1000E_DRIVER_TAG,
        mac_bytes[0],
        mac_bytes[1],
        mac_bytes[2],
        mac_bytes[3],
        mac_bytes[4],
        mac_bytes[5]
    );
    let hw_arc = Arc::new(Mutex::new(hw));
    let driver = E1000eDriver { hw: hw_arc.clone() };

    let ethernet_addr = EthernetAddress::from_bytes(&mac_bytes);
    // Start with unspecified address (0.0.0.0/0) so smoltcp accepts all ARP
    // probes and DHCP can assign the real address without routing conflicts.
    // A /24 here would make smoltcp reject ARP for IPs outside that subnet.
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


    let link_up_seen = Arc::new(core::sync::atomic::AtomicBool::new(
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
        bringup_job_scheduled: Arc::new(AtomicBool::new(false)),
        routes: Arc::new(Mutex::new(vec![RouteInfo {
            dst: IpCidr::new(IpAddress::v4(0, 0, 0, 0), 0),
            gateway: Some(IpAddress::Ipv4(default_v4_gw)),
        }])),
        ip_addrs: Arc::new(Mutex::new(ip_addrs)),
    };

    Ok(e1000e_iface)
}

pub struct E1000eDriverPci;

impl PciDriver for E1000eDriverPci {
    fn name(&self) -> &str {
        "e1000e"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        if vendor_id != 0x8086 {
            return false;
        }
        matches!(
            device_id,
            // 82574L, 82583V
            0x10d3 | 0x10f5 | 0x150c |
            // I210/I211 (sometimes handled by e1000e)
            0x1533 | 0x1539 | 0x157b | 0x157c |
            // I217, I218, I219 (PCH-LPT or later)
            0x1502..=0x1503 | 0x153a..=0x153b | 0x155a | 0x1559 | 0x15a0..=0x15a3 |
            0x156f..=0x1570 | 0x15b7..=0x15be | 0x15d6..=0x15d8 | 0x15e3 |
            0x0d4c..=0x0d4f | 0x15f4..=0x15fc | 0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 | 0x550a..=0x5511 | 0x57a0..=0x57a1 | 0x57b3..=0x57ba |
            0x15df..=0x15e2 | 0x0d53 | 0x0d55
        )
    }

    fn init(&self, dev: &PCIDevice, mapper: &Option<Arc<dyn IoMapper>>, irq: Option<usize>) -> DeviceResult<Device> {
        e1000e_vlog!(
            "e1000e: probing PCI {:#x}:{:#x} tag={}\n",
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
        let name = alloc::format!("eth{}", dev.loc.bus);
        
        // Ensure BUS MASTER is enabled in PCI command register
        unsafe {
            let mut cmd = PCI_ACCESS.read16(&PortOpsImpl, dev.loc, 0x04);
            cmd |= 0x0004; // Bus Master
            cmd |= 0x0002; // Memory Space
            PCI_ACCESS.write16(&PortOpsImpl, dev.loc, 0x04, cmd);
        }

        let vector = irq.map(|idx| idx + 32).unwrap_or(0);
        let iface = init(name, dev, vector, vaddr, 0)?;
        let iface_arc = Arc::new(iface);
        if iface_arc.driver.hw.lock().link_bringup_pending {
            iface_arc.schedule_link_bringup_poll();
        }
        if vector != 0 {
            crate::net::pci_note_pending_msi(vector, iface_arc.clone());
        }
        Ok(Device::Net(iface_arc))
    }
}
