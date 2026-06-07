//! Intel e1000e NIC driver (82574L / 82579 / I217 / I218 / I219 family)
//!
//! Reference drivers (behaviour checklist, not a line-by-line port):
//! - **Linux** `drivers/net/ethernet/intel/e1000e/` — `netdev.c` (open/NAPI/RX),
//!   `ich8lan.c` (PCH/AMT/I219), `defines.h` (interrupt masks).
//! - **FreeBSD** `if_em.c` / `em_rxtx.c` — `bus_dmamap_sync(POSTREAD|PREWRITE)`,
//!   filter + taskqueue instead of NAPI.
//!
//! Eclipse mapping:
//! | Concern        | Linux                         | FreeBSD              | Eclipse                    |
//! |----------------|-------------------------------|----------------------|----------------------------|
//! | DMA alloc      | `dma_alloc_coherent`          | `BUS_DMA_COHERENT`   | [`DmaRegion::map_coherent`] |
//! | CPU→device     | `dma_sync_single_for_device`  | `BUS_DMASYNC_PREWRITE` | [`DmaSyncDir::ToDevice`]   |
//! | device→CPU     | `dma_sync_single_for_cpu`     | `BUS_DMASYNC_POSTREAD` | [`DmaSyncDir::FromDevice`] |
//! | RX poll budget | NAPI `budget`                 | if_em filter         | Pulse + `rx_poll_budget`   |
//! | AMT open       | `e1000e_open` chunked         | em attach/open       | `schedule_amt_open`        |
//!
//! Set [`E1000E_CONVENTIONAL`] for a minimal profile: no checksum offload, no
//! no IAME auto-mask, no optional PCH tuning, short link-up wait. RX always uses extended
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
const E1000E_DRIVER_TAG: &str = "e1000e-osdev-rx";
/// Retry AMT open if SWFLAG/MDIO still blocked (CSME can take seconds after DRV_LOAD).
const E1000E_AMT_OPEN_RETRY_US: u64 = 30_000_000;
/// Per deferred job: wait at most this long for SWFLAG (chunked — PS/2/USB stay responsive).
const AMT_OPEN_SWFLAG_CHUNK_MS: u32 = 150;
const AMT_OPEN_SWFLAG_MAX_CHUNKS: u16 = 20;
/// [`amt_open_phase`] values for chunked Linux `e1000e_open` on I219+AMT.
const AMT_OPEN_IDLE: u8 = 0;
const AMT_OPEN_DRVLOAD: u8 = 1;
const AMT_OPEN_WAIT_SW: u8 = 2;
/// Linux `e1000_init_phy_workarounds_pchlan` (ULP, LANPHYPC, PHY reset) — before `reset_hw`.
const AMT_OPEN_PHY_WA: u8 = 3;
const AMT_OPEN_RESET: u8 = 4;
const AMT_OPEN_INIT: u8 = 5;
const AMT_OPEN_LINK: u8 = 6;

/// Trace/info — off unless [`E1000E_LOG_VERBOSE`].
macro_rules! e1000e_vlog {
    ($($t:tt)*) => {
        if E1000E_LOG_VERBOSE {
            crate::klog_info!($($t)*);
        }
    };
}

/// Diagnostic warnings (bringup steps, MDIO, SWFLAG) — off in normal dmesg.
macro_rules! e1000e_wlog {
    ($($t:tt)*) => {
        if E1000E_LOG_VERBOSE {
            crate::klog_warn!($($t)*);
        }
    };
}

const TARC0_CB_MULTIQ_3_REQ: u32 = 0x3000_0000;
const TARC0_CB_MULTIQ_2_REQ: u32 = 0x2000_0000;
/// Linux `SPEED_MODE_BIT` — must be clear at 10/100M or I219 TX may not complete (DD never set).
const TARC0_SPEED_MODE: u32 = 1 << 21;

/// Linux `mod_timer(&watchdog_timer, round_jiffies(jiffies + 2 * HZ))`.
const E1000E_WATCHDOG_PERIOD_US: u64 = 2_000_000;
/// Linux `mod_timer(..., jiffies + 1)` after LSC — quick follow-up check.
const E1000E_WATCHDOG_FAST_US: u64 = 10_000;
/// Min spacing between full PHY recovery attempts when MDIO stays silent.
const E1000E_PHY_RECOVERY_INTERVAL_US: u64 = 60_000_000;
/// After SWFLAG acquire fails, skip MDIO (Linux returns -E1000_ERR_CONFIG; no tight retry loop).
const E1000E_SWFLAG_BACKOFF_US: u64 = 60_000_000;
/// Linux `PHY_CFG_TIMEOUT` — wait for SWFLAG clear (mdelay(1) × 100).
const SWFLAG_WAIT_CLEAR_MS: u32 = 100;
/// Linux `SW_FLAG_TIMEOUT` — verify claim on MDIO hot paths (mdelay(1) × N).
const SWFLAG_VERIFY_SET_MS: u32 = 50;
/// First boot / probe: Linux `SW_FLAG_TIMEOUT` (mdelay(1) × 1000).
const SWFLAG_VERIFY_SET_INIT_MS: u32 = 1000;
/// Linux ME ULP_CFG_DONE poll: up to 2.5 s (`usleep_range(10000)` × 250).
const ME_ULP_CFG_DONE_MAX_ROUNDS: u32 = 250;
/// reg26 settle when `get_link_status` forces a PHY read (not on every poll).
const E1000E_LINK_CHECK_REG26_MS: u32 = 500;
/// Longer reg26 wait when arming RX / committing link speed (watchdog/deferred only).
const E1000E_REG26_COMMIT_MS: u32 = 3000;
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
use core::cell::Cell;
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
use crate::utils::dma_sync::{dma_sync_region, dma_sync_rx_desc_span, dma_sync_wb_from_device, DmaSyncDir};
use pci::{PCIDevice, BAR, Location};
use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
use lock::Mutex;


use super::timer_now_as_micros;

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
const LANPHYPC_POWERDOWN_HOLD_US: u64 = 20_000;
/// Analog PHY settle after releasing LANPHYPC (I219 real boards often need >100 ms).
const LANPHYPC_POWERUP_SETTLE_US: u64 = 200_000;
/// Spin iterations between clflush passes (PCIe WB settle on SKX+ / NUMA).
/// Minimum DMA base alignment for descriptor rings (Intel hardware requirement).
const DMA_DESC_ALIGN: usize = 16;
/// x86 cache line; four 16-byte RX descriptors share one line (false-sharing risk).
const CACHE_LINE_SIZE: usize = 64;
const RX_DESCS_PER_CACHE_LINE: usize = CACHE_LINE_SIZE / 16;
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
const CTRL_EXT_RO_DIS: u32 = 1 << 17; // Relaxation Order Disable (Linux: E1000_CTRL_EXT_RO_DIS = 0x00020000)
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
const BM_PORT_GEN_CFG: u32 = 27;
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
/// CSME/AMT can hold SWFLAG for seconds after warm boot — force-release only from PHY recovery.
const SWFLAG_FORCE_RELEASE_WAIT_MS: u32 = 200;

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
/// MDIC poll budget at runtime (400 × 50µs ≈ 20 ms).
const MDIC_POLL_TRIES: u32 = 400;
/// MDIC budget during PCI probe / deferred PHY init (must not block boot at 84%).
const MDIC_INIT_TRIES: u32 = 48;
/// Fast MDIO probe during PHY discovery (avoid 100 ms × 31 addresses).
const MDIC_PROBE_TRIES: u32 = 128;
const FWSM_FW_VALID: u32 = 0x8000;
const FWSM_ULP_CFG_DONE: u32 = 1 << 10;
const FWSM_RSPCIPHY: u32 = 0x40;
const CTRL_EXT_FORCE_SMBUS: u32 = 0x00000800;
const CTRL_EXT_LPCD: u32 = 1 << 2;
/// Linux `E1000_CTRL_EXT_DRV_LOAD` — tell CSME/AMT the driver owns the NIC (before SWFLAG/MDIO).
const CTRL_EXT_DRV_LOAD: u32 = 0x1000_0000;
const H2ME_ULP: u32 = 0x0000_0800;
const H2ME_ENFORCE_SETTINGS: u32 = 0x0000_1000;
const FEXTNVM3_PHY_CFG_COUNTER_MASK: u32 = 0x0C00_0000;
const FEXTNVM3_PHY_CFG_COUNTER_50MSEC: u32 = 0x0800_0000;
const MII_BMCR: u32 = 0x00;
const MII_BMSR: u32 = 0x01;
const MII_PHYSID1: u32 = 2;
const MII_PHYSID2: u32 = 3;
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
/// Sparse TX_CMD_RS (Linux): avoid saturating the chip WB FIFO on every slot.
const TX_RS_REPORT_INTERVAL: usize = 16;
const TX_RS_REPORT_LOW_WATER: usize = 8;

const NUM_RX: usize = 256;
/// Descriptor ring size (256×16 B = 4 KiB); must fit in one `DmaRegion` page.
const DMA_RING_BYTES: usize = NUM_RX * size_of::<RxDesc>();
const _RX_RING_ONE_PAGE: () = assert!(DMA_RING_BYTES <= 4096);
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
/// Max ring slots to spin waiting for SG continuation fragments per `receive()` call.
const RX_SG_SLOTS_PER_CALL: u8 = 8;
const NUM_TX: usize = 256;
const DMA_TX_RING_BYTES: usize = NUM_TX * size_of::<TxDesc>();
const BUF_SIZE: usize = 2048 + 128;

/// Intel indexes descriptor rings as `RDBAL + idx*16` in **physical** address space.
#[inline]
fn dma_span_within_one_phys_page(paddr: usize, span: usize) -> bool {
    let page_off = paddr & (crate::bus::PAGE_SIZE - 1);
    page_off + span <= crate::bus::PAGE_SIZE
}
/// Linux `E1000_RX_BUFFER_WRITE` — batch RDT doorbell every N descriptors.
const RX_BUFFER_WRITE: usize = 16;

// ---------------------------------------------------------------------------
// Descriptor layouts (§3.2.3 / §3.3.3 of 82574 datasheet)
// ---------------------------------------------------------------------------
// align(16): the NIC DMA engine requires 16-byte alignment. CRITICALLY, the
// hardware ALWAYS uses a fixed 16-byte descriptor stride (RDLEN/16 = slots).
// Padding to 64 bytes would make size_of::<RxDesc>()=64 → RDLEN=256×64=16384
// → NIC sees 1024 slots, with 768 entries having addr=0 (our padding) →
// DMA writes frames to physical address 0 → completely broken RX ring.
//
// Cache-line false-sharing: clflush/flush only on 64-byte-aligned spans (4 desc
// per line), never per 16-byte slot while the RX DMA engine is active.

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

/// Posted MMIO write + read-back (PCIe write posting flush). Use on RDT doorbells.
#[inline(always)]
unsafe fn mmio_write_flush(base: usize, reg: usize, val: u32) {
    mmio_write(base, reg, val);
    let _ = mmio_read(base, reg);
}

/// Drain PCIe posted-write buffers before enabling RCTL.EN (RDT must be visible first).
#[inline(always)]
unsafe fn mmio_pcie_posted_flush(base: usize) {
    let _ = mmio_read(base, E1000E_RDT);
    let _ = mmio_read(base, E1000E_STATUS);
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    fence(Ordering::SeqCst);
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
    /// Software accumulation of GPRC/MPC (hardware regs are read-on-clear).
    hw_roc_gprc_acc: u64,
    hw_roc_mpc_acc: u64,
    /// Last ROC deltas from [`E1000eHw::refresh_hw_stats_roc`] (for diag only).
    hw_roc_gprc_last: u32,
    hw_roc_mpc_last: u32,
    rx_diag_counter: u32,
    /// Watchdog ticks with link up but GPRC still zero — triggers RX re-arm.
    rx_stall_watchdogs: u8,
    /// Cap frames delivered per `poll()` so AF_PACKET `send()` does not drain the whole ring.
    rx_poll_budget: u8,
    /// PHY workarounds (ULP/MDIO/LANPHYPC) deferred past PCI scan (boot progress 84%).
    phy_init_pending: bool,
    /// Chunk index for [`E1000eHw::deferred_init_step`] (0..=2 while pending).
    deferred_init_step: u8,
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
    /// Linux watchdog: next [`E1000eHw::watchdog_tick`] (2 Hz default).
    link_watchdog_next_us: u64,
    /// Last [`pch_recover_phy_mdio`] attempt when MDIO silent and link down.
    last_phy_recovery_us: u64,
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
    /// Last cache line (idx/4) synced for DD read on WB fallback; 0xFF = none.
    rx_wb_sync_line: u8,
    /// MDIO/SWFLAG unavailable — trust STATUS.LU for link (common on I219 after ULP).
    mdio_degraded: bool,
    /// MDIO returned a valid PHY ID/BMSR during detect.
    phy_mdio_responding: bool,
    /// Logged once when detect fails (avoid dmesg flood on bringup retries).
    phy_detect_fail_logged: bool,
    /// Skip `pch_swflag_acquire` until this time (hot path — no 100+ ms spin per poll).
    swflag_backoff_until_us: Cell<u64>,
    /// One dmesg line when SWFLAG claim fails (FW/CSME owns EXTCNF).
    swflag_fail_logged: Cell<bool>,
    /// Linux `FLAG_HAS_AMT`: DRV_LOAD + PHY/link only after "interface open", not at PCI probe.
    nic_open_done: bool,
    /// Last [`E1000eHw::e1000e_open_bringup`] attempt (retry while MDIO blocked).
    last_amt_open_attempt_us: Cell<u64>,
    /// Suppress hot-path SWFLAG warnings while [`E1000eHw::e1000e_open_bringup_step`] runs.
    amt_open_active: Cell<bool>,
    /// Chunked AMT open state machine ([`E1000eHw::e1000e_open_bringup_step`]).
    amt_open_phase: u8,
    /// SWFLAG wait chunks consumed in current open attempt.
    amt_open_sw_chunks: u16,
}

impl E1000eHw {
    // -----------------------------------------------------------------------
    // Kumeran (KMRN) register access (ICH8/PCH specific)
    // -----------------------------------------------------------------------

    /// Busy-wait for `us` microseconds (timer-based; capped spin so HID/USB are not starved).
    fn udelay(us: u64) {
        if us == 0 {
            return;
        }
        let t0 = timer_now_as_micros();
        const MAX_SPINS: u64 = 10_000_000;
        let mut spins = 0u64;
        while timer_now_as_micros().wrapping_sub(t0) < us {
            core::hint::spin_loop();
            spins = spins.wrapping_add(1);
            if spins >= MAX_SPINS {
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
            e1000e_wlog!(
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
            e1000e_wlog!(
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
        if rx_base_p & (crate::bus::PAGE_SIZE - 1) != 0
            || !dma_span_within_one_phys_page(rx_base_p, DMA_RING_BYTES)
        {
            crate::klog_err!(
                "[e1000e] RX ring p={:#x} crosses 4 KiB page (RDLEN={})\n",
                rx_base_p,
                DMA_RING_BYTES
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
        if tx_base_p & (crate::bus::PAGE_SIZE - 1) != 0
            || !dma_span_within_one_phys_page(tx_base_p, DMA_TX_RING_BYTES)
        {
            crate::klog_err!(
                "[e1000e] TX ring p={:#x} crosses 4 KiB page (TDLEN={})\n",
                tx_base_p,
                DMA_TX_RING_BYTES
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
                e1000e_wlog!(
                    "[e1000e] RX buf slot {} paddr {:#x} not 64-byte aligned\n",
                    i,
                    self.rx_buf_paddr(i)
                );
            }
        }
        e1000e_vlog!(
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

    /// clflush RX payload only when DMA is still write-back (UC mapping needs no invalidate).
    fn rx_needs_cache_invalidation(&self) -> bool {
        !self.dma_uncached
    }

    /// Returns true when CPU cache maintenance is required before reading NIC write-back.
    fn rx_needs_cache_flush(&self) -> bool {
        !self.dma_uncached
    }

    /// Mark descriptor rings and packet pools uncacheable (Linux `dma_alloc_coherent` fallback).
    fn setup_dma_uncached(&mut self) {
        let ok = E1000eHw::mark_dma_region_uncached(&self.rx_ring, "rx_ring")
            & E1000eHw::mark_dma_region_uncached(&self.tx_ring, "tx_ring")
            & E1000eHw::mark_dma_region_uncached(&self.rx_buf_pool, "rx_buf_pool")
            & E1000eHw::mark_dma_region_uncached(&self.tx_buf_pool, "tx_buf_pool");
        self.dma_uncached = ok;
        if ok {
            e1000e_vlog!("[e1000e] DMA rings/pools mapped and verified uncacheable (PAT UC)\n");
            unsafe {
                for (region, len) in [
                    (&self.rx_ring, self.rx_ring.byte_len()),
                    (&self.tx_ring, self.tx_ring.byte_len()),
                    (&self.rx_buf_pool, self.rx_buf_pool.byte_len()),
                    (&self.tx_buf_pool, self.tx_buf_pool.byte_len()),
                ] {
                    dma_sync_region(region, false, 0, len, DmaSyncDir::ToDevice);
                }
            }
        } else {
            crate::klog_warn!(
                "[e1000e] UC DMA incomplete — RX uses line-batch sync on WB pages (tag={})\n",
                E1000E_DRIVER_TAG
            );
        }
        let _ = self.validate_dma_ring_layout();
    }

    /// Returns true for PCH-SPT (I219) and later silicon.
    /// These chips require explicit RXDCTL/TXDCTL QUEUE_ENABLE (bit 25) to
    /// activate the RX/TX DMA queues after RCTL_EN/TCTL_EN.
    /// Linux `FLAG_HAS_AMT` on `e1000_pch_spt_info` (I219 / CSME-managed LAN).
    fn has_amt(&self) -> bool {
        self.is_pch_spt_or_later()
    }

    fn is_pch_spt_or_later(&self) -> bool {
        matches!(self.device_id,
            0x156f..=0x1570 | 0x15b7..=0x15be | 0x15d6..=0x15d8 | 0x15e3 |
            0x0d4c..=0x0d4f | 0x15f4..=0x15fc | 0x1a1c..=0x1a1f |
            0x0dc5..=0x0dc8 | 0x550a..=0x5511 | 0x57a0..=0x57a1 | 0x57b3..=0x57ba |
            0x15df..=0x15e2 | 0x0d53 | 0x0d55 | 0x15f9 | 0x15fa
        )
    }

    /// Acquire SWFLAG for MDIO; force-release once if CSME/BIOS left it set.
    unsafe fn pch_acquire_swflag_for_mdio(&self) -> bool {
        if self.swflag_in_backoff() {
            return false;
        }
        if self.pch_swflag_acquire_init() {
            return true;
        }
        if self.swflag_in_backoff() {
            return false;
        }
        self.pch_swflag_force_release();
        Self::udelay(SWFLAG_FORCE_RELEASE_WAIT_MS as u64 * 1_000);
        self.pch_swflag_acquire_init()
    }

    /// Scan PHY addresses with SWFLAG held (PCH) or per-read acquire (discrete).
    unsafe fn scan_phy_addrs(&mut self, scan: &[u8]) -> bool {
        let try_addr = |hw: &Self, pa: u8| -> bool {
            if let Some(id1) = hw.mdic_read_swheld(pa, MII_PHYSID1, MDIC_PROBE_TRIES) {
                if id1 != 0 && id1 != 0xFFFF {
                    e1000e_vlog!("[e1000e] PHY addr {} ID1={:#x}\n", pa, id1);
                    return true;
                }
            }
            if let Some(bmsr) = hw.mdic_read_swheld(pa, MII_BMSR, MDIC_PROBE_TRIES) {
                if bmsr != 0 && bmsr != 0xFFFF {
                    e1000e_vlog!("[e1000e] PHY addr {} BMSR={:#x}\n", pa, bmsr);
                    return true;
                }
            }
            false
        };

        if self.is_pch_lpt_or_later() {
            if !self.pch_acquire_swflag_for_mdio() {
                return false;
            }
            self.pch_mdio_unlock_swheld();
            let mut found = false;
            for &pa in scan {
                if try_addr(self, pa) {
                    self.phy_addr = pa;
                    found = true;
                    break;
                }
            }
            self.pch_swflag_release();
            return found;
        }

        for &pa in scan {
            if let Some(id1) = self.mdic_read_init(pa, MII_PHYSID1) {
                if id1 != 0 && id1 != 0xFFFF {
                    e1000e_vlog!("[e1000e] PHY addr {} ID1={:#x}\n", pa, id1);
                    self.phy_addr = pa;
                    return true;
                }
            }
        }
        for &pa in scan {
            if let Some(bmsr) = self.mdic_read_init(pa, MII_BMSR) {
                if bmsr != 0 && bmsr != 0xFFFF {
                    e1000e_vlog!("[e1000e] PHY addr {} BMSR={:#x}\n", pa, bmsr);
                    self.phy_addr = pa;
                    return true;
                }
            }
        }
        for pa in 3u8..=31 {
            if let Some(id1) = self.mdic_read_init(pa, MII_PHYSID1) {
                if id1 != 0 && id1 != 0xFFFF {
                    e1000e_vlog!("[e1000e] PHY addr {} ID1={:#x}\n", pa, id1);
                    self.phy_addr = pa;
                    return true;
                }
            }
        }
        false
    }

    unsafe fn try_detect_phy_addr(&mut self) -> bool {
        let scan = if self.is_pch_lpt_or_later() {
            [self.phy_addr, 2u8, 1u8]
        } else {
            [1u8, 2, self.phy_addr]
        };
        self.scan_phy_addrs(&scan)
    }

    /// ULP off + LANPHYPC + MDIO unlock — used when PHY does not answer MDIC.
    unsafe fn pch_recover_phy_mdio(&self) {
        e1000e_vlog!("[e1000e] PHY MDIO recovery (ULP + LANPHYPC)\n");
        let _ = self.disable_ulp_lpt_lp(true);
        self.toggle_lanphypc();
        self.pch_mdio_prepare_after_power();
        let _ = self.wait_phy_mdio_ready(self.phy_addr, LANPHYPC_POWERUP_SETTLE_US);
    }

    /// Discover the responding PHY address via MDIO reads.
    unsafe fn detect_phy_addr(&mut self) {
        if self.try_detect_phy_addr() {
            self.phy_mdio_responding = true;
            return;
        }
        if self.is_pch_lpt_or_later() {
            self.pch_recover_phy_mdio();
            if self.try_detect_phy_addr() {
                self.phy_mdio_responding = true;
                return;
            }
        }
        self.phy_mdio_responding = false;
        if self.is_pch_lpt_or_later() {
            self.phy_addr = 2;
        } else {
            self.phy_addr = 1;
        }
        if !self.phy_detect_fail_logged {
            self.phy_detect_fail_logged = true;
            crate::klog_warn!(
                "[e1000e] MDIO silent — using PHY addr {} (set E1000E_LOG_VERBOSE for diag)\n",
                self.phy_addr
            );
            self.log_mdio_diag();
        }
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
        e1000e_vlog!(
            "[e1000e] MAC from NVM: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.mac[0], self.mac[1], self.mac[2], self.mac[3], self.mac[4], self.mac[5]
        );
    }

    unsafe fn toggle_lanphypc(&self) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        let mut fextnvm3 = mmio_read(self.base, E1000E_FEXTNVM3);
        fextnvm3 &= !FEXTNVM3_PHY_CFG_COUNTER_MASK;
        fextnvm3 |= FEXTNVM3_PHY_CFG_COUNTER_50MSEC;
        mmio_write(self.base, E1000E_FEXTNVM3, fextnvm3);
        let _ = mmio_read(self.base, E1000E_FEXTNVM3);

        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl |= CTRL_LANPHYPC_OVERRIDE;
        ctrl &= !CTRL_LANPHYPC_VALUE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL);
        Self::udelay(LANPHYPC_POWERDOWN_HOLD_US);

        ctrl = mmio_read(self.base, E1000E_CTRL);
        ctrl &= !CTRL_LANPHYPC_OVERRIDE;
        mmio_write(self.base, E1000E_CTRL, ctrl);
        let _ = mmio_read(self.base, E1000E_CTRL);

        if self.is_pch_lpt_or_later() {
            let mut lpc_wait = 40u32;
            while lpc_wait > 0 {
                if mmio_read(self.base, E1000E_CTRL_EXT) & CTRL_EXT_LPCD != 0 {
                    break;
                }
                Self::udelay(5_000);
                lpc_wait -= 1;
            }
        }
        Self::udelay(LANPHYPC_POWERUP_SETTLE_US);
    }

    /// Linux `e1000_gate_hw_phy_config_ich8lan`.
    unsafe fn pch_gate_hw_phy_config(&self, gate: bool) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        let mut ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if gate {
            ext |= EXTCNF_CTRL_GATE_PHY_CFG;
        } else {
            ext &= !EXTCNF_CTRL_GATE_PHY_CFG;
        }
        mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
        let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
    }

    /// Linux `e1000_check_reset_block_ich8lan` — true when ME blocks PHY reset.
    unsafe fn pch_phy_reset_blocked(&self) -> bool {
        let mut i = 0u32;
        while (mmio_read(self.base, E1000E_FWSM) & FWSM_RSPCIPHY) == 0 {
            if i >= 30 {
                return true;
            }
            i += 1;
            Self::udelay(10_000);
        }
        false
    }

    /// Linux `e1000_phy_is_accessible_pchlan` (caller holds SWFLAG).
    unsafe fn phy_is_accessible_pchlan_swheld(&self, phy_addr: u8) -> bool {
        let mut phy_id: u32 = 0;
        let mut phy_reg2: u16 = 0;
        for _ in 0..2 {
            let Some(id1) = self.mdic_read_swheld(phy_addr, MII_PHYSID1, MDIC_INIT_TRIES) else {
                continue;
            };
            if id1 == 0 || id1 == 0xFFFF {
                continue;
            }
            phy_id = (id1 as u32) << 16;
            let Some(id2) = self.mdic_read_swheld(phy_addr, MII_PHYSID2, MDIC_INIT_TRIES) else {
                phy_id = 0;
                continue;
            };
            if id2 == 0 || id2 == 0xFFFF {
                phy_id = 0;
                continue;
            }
            phy_reg2 = id2;
            phy_id |= (id2 & 0xFFF0) as u32;
            break;
        }
        if phy_id == 0 {
            return false;
        }

        if self.is_pch_lpt_or_later() && (mmio_read(self.base, E1000E_FWSM) & FWSM_FW_VALID) == 0 {
            let cv_smb = Self::phy_reg_paged(769, 23);
            if let Some(mut pr) = self.mdic_read_phy_swheld(phy_addr, cv_smb) {
                pr &= !0x0001;
                let _ = self.mdic_write_phy_swheld(phy_addr, cv_smb, pr);
            }
            let mut mac_reg = mmio_read(self.base, E1000E_CTRL_EXT);
            mac_reg &= !CTRL_EXT_FORCE_SMBUS;
            mmio_write(self.base, E1000E_CTRL_EXT, mac_reg);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
        }

        let _ = phy_reg2;
        true
    }

    /// Clear GATE_PHY_CFG + stuck MDIO; SWFLAG must already be held.
    unsafe fn pch_mdio_unlock_swheld(&self) {
        let mut ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if ext & EXTCNF_CTRL_GATE_PHY_CFG != 0 {
            ext &= !EXTCNF_CTRL_GATE_PHY_CFG;
            mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
            let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            Self::udelay(1_000);
        }
        self.mdic_clear_stuck(false);
    }

    /// After LANPHYPC: ungate MDIO bus (works even when SWFLAG is temporarily busy).
    unsafe fn pch_mdio_prepare_after_power(&self) {
        let mut ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if ext & EXTCNF_CTRL_GATE_PHY_CFG != 0 {
            ext &= !EXTCNF_CTRL_GATE_PHY_CFG;
            mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
            let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            Self::udelay(1_000);
        }
        self.mdic_clear_stuck(true);
        if self.pch_swflag_acquire() {
            self.pch_mdio_unlock_swheld();
            self.pch_swflag_release();
        } else {
            e1000e_wlog!("[e1000e] MDIO prep: SWFLAG unavailable (STATUS fallback may apply)\n");
        }
    }

    unsafe fn pch_try_h2me_ulp_clear(&self) -> bool {
        let mut h2me = mmio_read(self.base, E1000E_H2ME);
        h2me &= !(1 << 11);
        h2me |= 1 << 12;
        mmio_write(self.base, E1000E_H2ME, h2me);
        let _ = mmio_read(self.base, E1000E_H2ME);

        let mut i = 0;
        while (mmio_read(self.base, E1000E_FWSM) & FWSM_ULP_CFG_DONE) != 0 {
            if i >= 250 {
                return false;
            }
            i += 1;
            Self::udelay(10_000);
        }

        let mut h2me = mmio_read(self.base, E1000E_H2ME);
        h2me &= !(1 << 12);
        mmio_write(self.base, E1000E_H2ME, h2me);
        let _ = mmio_read(self.base, E1000E_H2ME);
        e1000e_vlog!("[e1000e] ULP ME/H2ME clear in {} ms\n", i * 10);
        true
    }

    /// Linux `e1000_disable_ulp_lpt_lp` software branch (SWFLAG already held).
    unsafe fn disable_ulp_software_path_swheld(&self, phy_addr: u8) -> bool {
        self.pch_mdio_unlock_swheld();

        let cv_smb_ctrl = Self::phy_reg_paged(769, 23);
        let mut ok = false;
        if let Some(mut phy_reg) = self.mdic_read_phy_swheld(phy_addr, cv_smb_ctrl) {
            phy_reg &= !0x0001;
            ok = self.mdic_write_phy_swheld(phy_addr, cv_smb_ctrl, phy_reg);
        } else {
            let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
            ctrl_ext |= CTRL_EXT_FORCE_SMBUS;
            mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
            Self::udelay(50_000);

            if let Some(mut phy_reg) = self.mdic_read_phy_swheld(phy_addr, cv_smb_ctrl) {
                phy_reg &= !0x0001;
                ok = self.mdic_write_phy_swheld(phy_addr, cv_smb_ctrl, phy_reg);
            }
        }

        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext &= !CTRL_EXT_FORCE_SMBUS;
        ctrl_ext &= !CTRL_EXT_PHYPDEN;
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
        let _ = mmio_read(self.base, E1000E_CTRL_EXT);

        let hv_pm_ctrl = Self::phy_reg_paged(770, 17);
        if let Some(mut phy_reg) = self.mdic_read_phy_swheld(phy_addr, hv_pm_ctrl) {
            phy_reg |= 0x4000;
            let _ = self.mdic_write_phy_swheld(phy_addr, hv_pm_ctrl, phy_reg);
        }

        let i218_ulp_config1 = Self::phy_reg_paged(779, 16);
        if let Some(mut phy_reg) = self.mdic_read_phy_swheld(phy_addr, i218_ulp_config1) {
            phy_reg &= !0x1D74;
            let _ = self.mdic_write_phy_swheld(phy_addr, i218_ulp_config1, phy_reg);
            phy_reg |= 0x0001;
            let _ = self.mdic_write_phy_swheld(phy_addr, i218_ulp_config1, phy_reg);
        }

        let mut fextnvm7 = mmio_read(self.base, E1000E_FEXTNVM7);
        fextnvm7 &= !FEXTNVM7_DISABLE_SMB_PERST;
        mmio_write(self.base, E1000E_FEXTNVM7, fextnvm7);

        match self.mdic_read_swheld(phy_addr, MII_BMSR, MDIC_INIT_TRIES) {
            Some(bmsr) => {
                e1000e_vlog!("[e1000e] ULP software: PHY BMSR={:#x}\n", bmsr);
                ok
            }
            None => {
                e1000e_wlog!("[e1000e] ULP software: MDIO still silent on PHY {}\n", phy_addr);
                false
            }
        }
    }

    /// Linux `e1000_disable_ulp_lpt_lp(hw, force)` at driver load.
    unsafe fn disable_ulp_lpt_lp(&self, force: bool) -> bool {
        if !self.is_pch_lpt_or_later() {
            return true;
        }

        let fwsm = mmio_read(self.base, E1000E_FWSM);
        e1000e_vlog!("[e1000e] disable_ulp_lpt_lp FWSM={:#x} force={}\n", fwsm, force);

        if (fwsm & FWSM_FW_VALID) != 0 {
            if force {
                let mut h2me = mmio_read(self.base, E1000E_H2ME);
                h2me &= !H2ME_ULP;
                h2me |= H2ME_ENFORCE_SETTINGS;
                mmio_write(self.base, E1000E_H2ME, h2me);
                let _ = mmio_read(self.base, E1000E_H2ME);
            }
            let mut i = 0u32;
            while (mmio_read(self.base, E1000E_FWSM) & FWSM_ULP_CFG_DONE) != 0 {
                if i >= ME_ULP_CFG_DONE_MAX_ROUNDS {
                    e1000e_wlog!("[e1000e] ME ULP_CFG_DONE timeout (~2.5 s)\n");
                    return false;
                }
                i += 1;
                Self::udelay(10_000);
            }
            if i > 100 {
                e1000e_wlog!("[e1000e] ME ULP_CFG_DONE slow ({} ms) — firmware\n", i * 10);
            } else if i > 0 {
                e1000e_vlog!("[e1000e] ME cleared ULP_CFG_DONE in {} ms\n", i * 10);
            }
            if force {
                let mut h2me = mmio_read(self.base, E1000E_H2ME);
                h2me &= !H2ME_ENFORCE_SETTINGS;
                mmio_write(self.base, E1000E_H2ME, h2me);
            } else {
                let mut h2me = mmio_read(self.base, E1000E_H2ME);
                h2me &= !H2ME_ULP;
                mmio_write(self.base, E1000E_H2ME, h2me);
            }
            return true;
        }

        if !self.pch_swflag_acquire_init() {
            e1000e_wlog!("[e1000e] disable_ulp_lpt_lp: no SWFLAG\n");
            self.log_mdio_diag();
            return false;
        }

        if force {
            self.toggle_lanphypc();
        }
        let phy_addr = self.phy_addr;
        let sw_ok = self.disable_ulp_software_path_swheld(phy_addr);
        self.pch_swflag_release();

        if force {
            self.pch_issue_phy_reset();
            Self::udelay(50_000);
        }

        if sw_ok {
            e1000e_vlog!("[e1000e] ULP disabled via software path (PHY {})\n", phy_addr);
        }
        sw_ok
    }

    /// Linux `e1000_init_phy_workarounds_pchlan` (load path).
    unsafe fn pch_init_phy_workarounds(&mut self) {
        if !self.is_pch_lpt_or_later() {
            return;
        }

        self.pch_gate_hw_phy_config(true);
        if !self.disable_ulp_lpt_lp(true) {
            e1000e_wlog!("[e1000e] disable_ulp_lpt_lp failed\n");
        }

        if !self.pch_acquire_swflag_for_mdio() {
            e1000e_wlog!("[e1000e] phy workarounds: SWFLAG unavailable — continuing gated\n");
            self.pch_gate_hw_phy_config(false);
            self.pch_mdio_prepare_after_power();
            self.detect_phy_addr();
            return;
        }

        let phy_addr = self.phy_addr;
        let mut accessible = self.phy_is_accessible_pchlan_swheld(phy_addr);

        if !accessible && self.is_pch_lpt_or_later() {
            let mut mac_reg = mmio_read(self.base, E1000E_CTRL_EXT);
            mac_reg |= CTRL_EXT_FORCE_SMBUS;
            mmio_write(self.base, E1000E_CTRL_EXT, mac_reg);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
            Self::udelay(50_000);
            accessible = self.phy_is_accessible_pchlan_swheld(phy_addr);
        }

        if !accessible && !self.pch_phy_reset_blocked() {
            e1000e_vlog!("[e1000e] PHY not accessible — LANPHYPC toggle\n");
            self.toggle_lanphypc();
            self.pch_mdio_unlock_swheld();
            let mut mac_reg = mmio_read(self.base, E1000E_CTRL_EXT);
            mac_reg &= !CTRL_EXT_FORCE_SMBUS;
            mmio_write(self.base, E1000E_CTRL_EXT, mac_reg);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
            accessible = self.phy_is_accessible_pchlan_swheld(phy_addr);
        } else if !accessible {
            e1000e_wlog!("[e1000e] PHY inaccessible and ME blocks LANPHYPC\n");
        }

        if accessible && !self.pch_phy_reset_blocked() {
            self.pch_swflag_release();
            self.pch_issue_phy_reset();
            if !self.pch_swflag_acquire_init() {
                e1000e_wlog!("[e1000e] phy workarounds: lost SWFLAG after PHY_RST\n");
                return;
            }
            let _ = self.phy_is_accessible_pchlan_swheld(phy_addr);
        }

        self.pch_swflag_release();

        if accessible {
            e1000e_vlog!("[e1000e] PHY accessible after workarounds (addr {})\n", phy_addr);
            self.phy_mdio_responding = true;
        } else {
            if !self.phy_detect_fail_logged {
                crate::klog_warn!("[e1000e] PHY still inaccessible after workarounds\n");
                self.log_mdio_diag();
            }
        }
        self.detect_phy_addr();
    }

    /// Quiesce DMA before reset (Linux `e1000_reset_hw_ich8lan` preamble).
    unsafe fn quiesce_dma_before_reset(&mut self) {
        if self.is_pch_spt_or_later() {
            let mut rxdctl = mmio_read(self.base, E1000E_RXDCTL);
            rxdctl &= !RXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_RXDCTL, rxdctl);
            let _ = mmio_read(self.base, E1000E_RXDCTL);

            let mut txdctl = mmio_read(self.base, E1000E_TXDCTL);
            txdctl &= !TXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
            let _ = mmio_read(self.base, E1000E_TXDCTL);
            Self::udelay(2_000);
        }

        self.stop_rx_tx_engines();
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;
        mmio_write(self.base, E1000E_RDH, 0);
        let _ = mmio_read(self.base, E1000E_RDH);
        mmio_write(self.base, E1000E_RDT, 0);
        let _ = mmio_read(self.base, E1000E_RDT);
        self.flush_desc_rings();
    }

    /// Linux `e1000_reset_hw_ich8lan` — global MAC reset (+ PHY_RST when ME allows).
    unsafe fn reset_hw_ich8lan_linux(&mut self, with_phy_rst: bool) {
        e1000e_wlog!("[e1000e] reset_hw_ich8lan (phy_rst={})\n", with_phy_rst);

        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
        mmio_write(self.base, E1000E_RCTL, 0);
        mmio_write(self.base, E1000E_TCTL, TCTL_PSP);
        let _ = mmio_read(self.base, E1000E_TCTL);
        Self::udelay(10_000);

        let mut ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(self.base, E1000E_CTRL, ctrl | CTRL_GIO_MASTER_DISABLE);
        let mut gio_wait = 500u32;
        while gio_wait > 0 {
            if mmio_read(self.base, E1000E_STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
                break;
            }
            Self::udelay(100);
            gio_wait -= 1;
        }

        ctrl = mmio_read(self.base, E1000E_CTRL);
        let phy_rst = with_phy_rst
            && self.is_pch_lpt_or_later()
            && !self.pch_phy_reset_blocked();
        if phy_rst {
            ctrl |= CTRL_PHY_RST;
        }

        let swflag_ok = if self.is_pch_lpt_or_later() {
            self.pch_swflag_acquire_init()
        } else {
            true
        };

        mmio_write(self.base, E1000E_CTRL, ctrl | CTRL_RST);
        Self::udelay(20_000);

        let mut rst_wait = 1_000u32;
        while rst_wait > 0 {
            if mmio_read(self.base, E1000E_CTRL) & CTRL_RST == 0 {
                break;
            }
            Self::udelay(100);
            rst_wait -= 1;
        }

        if swflag_ok && self.is_pch_lpt_or_later() {
            self.pch_swflag_release();
        }

        if phy_rst {
            self.pch_phy_reset_complete();
            Self::udelay(10_000);
            self.pch_clear_status_phyra_if_set();
        }

        self.invalidate_rx_sw_state();

        mmio_write(self.base, E1000E_WUC, 0);
        mmio_write(self.base, E1000E_WUFC, 0);
        mmio_write(self.base, E1000E_WUS, 0xFFFF_FFFF);

        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
        let _ = mmio_read(self.base, E1000E_ICR);

        let kabgtxd = mmio_read(self.base, E1000E_KABGTXD);
        mmio_write(self.base, E1000E_KABGTXD, kabgtxd | KABGTXD_BGSQLBIAS);

        if self.is_pch_lpt_or_later() {
            let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
            ctrl_ext &= !CTRL_EXT_DPG_EN;
            mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
        }

        ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(
            self.base,
            E1000E_CTRL,
            ctrl & !CTRL_GIO_MASTER_DISABLE,
        );
        let _ = mmio_read(self.base, E1000E_CTRL);
        self.restore_pci_command_bus_master();
        Self::udelay(50);
    }

    /// CTRL_RST (reset_hw_ich8lan) puede dejar deshabilitado el Bus Master y/o
    /// Memory Space en el registro PCI Command en algunos equipos reales.
    /// Tras reset, reaseguramos esos bits antes de reprogramar datapath/DMA.
    unsafe fn restore_pci_command_bus_master(&self) {
        let mut cmd = PCI_ACCESS.read16(&PortOpsImpl, self.pci_loc, 0x04);
        cmd |= 0x0004; // Bus Master
        cmd |= 0x0002; // Memory Space
        PCI_ACCESS.write16(&PortOpsImpl, self.pci_loc, 0x04, cmd);
        let _ = PCI_ACCESS.read16(&PortOpsImpl, self.pci_loc, 0x04);
    }

    /// Linux `e1000_setup_link_ich8lan` + `e1000_setup_copper_link_pch_lpt` (flow + copper).
    unsafe fn setup_link_ich8lan_linux(&self) {
        if self.pch_phy_reset_blocked() {
            return;
        }
        if self.is_pch_lpt_or_later() {
            mmio_write(self.base, E1000E_FCTTV, 0xFFFF);
            mmio_write(self.base, E1000E_FCRTV, 0xFFFF);
        }
        self.mac_setup_copper_link_linux();
        self.phy_setup_82577_copper(self.active_phy_addr());
    }

    /// Linux `e1000_init_hw_ich8lan` subset after reset.
    unsafe fn init_hw_ich8lan_linux(&mut self) {
        self.read_mac_from_hw();
        if self.is_valid_mac() {
            let mac_low =
                u32::from_le_bytes([self.mac[0], self.mac[1], self.mac[2], self.mac[3]]);
            let mac_high = u32::from_le_bytes([self.mac[4], self.mac[5], 0, 0]);
            mmio_write(self.base, E1000E_RAL0, mac_low);
            mmio_write(self.base, E1000E_RAH0, mac_high | 0x8000_0000);
        }

        if self.is_pch_lpt_or_later() {
            self.pch_disable_lplu_gbe();
            self.pch_disable_k1();
            let phy_addr = self.active_phy_addr();
            let pll_reg = Self::phy_reg_paged(772, 28);
            if let Some(mut phy_reg) = self.mdic_read_phy(phy_addr, pll_reg) {
                phy_reg &= !I217_PLL_CLOCK_GATE_MASK;
                phy_reg |= 0xFA;
                let _ = self.mdic_write_phy(phy_addr, pll_reg, phy_reg);
            }
            self.pch_setup_kmrn_copper_link();
            self.setup_link_ich8lan_linux();
        } else {
            self.mac_setup_copper_link_linux();
        }

        let status = mmio_read(self.base, E1000E_STATUS);
        e1000e_wlog!(
            "[e1000e] init_hw_ich8lan STATUS={:#x} link={}\n",
            status,
            status & STATUS_LU != 0
        );
        if self.is_pch_lpt_or_later() {
            self.pch_clear_bm_port_gen_host_wu();
        }
    }

    /// Reprogram MAC datapath + DMA rings after `reset_hw_ich8lan_linux`.
    /// Probe-time ring setup is invalidated by CTRL_RST; Linux re-runs configure_tx/rx on open.
    unsafe fn reapply_datapath_after_mac_reset(&mut self) {
        e1000e_wlog!("[e1000e] reapply datapath after MAC reset\n");
        self.invalidate_rx_sw_state();
        self.restore_pci_command_bus_master();
        Self::udelay(50);

        for i in 1usize..16 {
            mmio_write(self.base, E1000E_RAL0 + i * 2, 0);
            mmio_write(self.base, E1000E_RAL0 + i * 2 + 1, 0);
        }

        let ctrl = mmio_read(self.base, E1000E_CTRL);
        mmio_write(
            self.base,
            E1000E_CTRL,
            (ctrl | CTRL_SLU | CTRL_ASDE | CTRL_FD)
                & !(CTRL_TFCE | CTRL_RFCE | CTRL_VME | CTRL_GIO_MASTER_DISABLE),
        );

        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext |= 1 << 22; // PBA_CLR
        ctrl_ext |= 1 << 31; // PBA_SUPPORT (I219)
        ctrl_ext |= CTRL_EXT_DRV_LOAD;
        if self.is_pch_lpt_or_later() {
            ctrl_ext &= !CTRL_EXT_PHYPDEN;
        }
        ctrl_ext |= CTRL_EXT_RO_DIS;
        ctrl_ext &= !CTRL_EXT_DPG_EN;
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);

        if self.is_pch_lpt_or_later() {
            mmio_write(self.base, E1000E_PBA, 0x0012001A);
            self.pch_apply_silicon_workarounds();
        } else {
            mmio_write(self.base, E1000E_PBA, 0x00100030);
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

        for i in 0..E1000E_MTA_LEN {
            mmio_write(self.base, E1000E_MTA_BASE + i, 0);
        }

        let rx_ring = self.rx_ring.as_ptr::<RxDesc>();
        let tx_ring = self.tx_ring.as_ptr::<TxDesc>();
        core::ptr::write_bytes(rx_ring, 0, NUM_RX);
        core::ptr::write_bytes(tx_ring, 0, NUM_TX);
        for i in 0..NUM_RX {
            let desc = &mut *rx_ring.add(i);
            desc.addr = self.rx_buf_paddr(i);
        }
        self.flush_rx_ring_descriptor_span(0, NUM_RX);

        let tx_ring_pa = self.tx_ring.paddr();
        mmio_write(self.base, E1000E_TDBAL, tx_ring_pa as u32);
        mmio_write(self.base, E1000E_TDBAH, (tx_ring_pa >> 32) as u32);
        mmio_write(self.base, E1000E_TDLEN, (NUM_TX * size_of::<TxDesc>()) as u32);
        self.tx_tail = 0;
        mmio_write(self.base, E1000E_TDH, 0);
        mmio_write(self.base, E1000E_TDT, 0);
        mmio_write(self.base, E1000E_TIPG, 8 | (8 << 10) | (12 << 20));
        mmio_write(self.base, E1000E_TIDV, 0);
        mmio_write(self.base, E1000E_TADV, 0);

        if self.is_pch_lpt_or_later() {
            let txdctl = self.program_txdctl_linux();
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
        } else {
            mmio_write(
                self.base,
                E1000E_TXDCTL,
                TXDCTL_DMA_BURST | TXDCTL_FULL_TX_DESC_WB,
            );
        }
        if self.is_pch_spt_or_later() {
            let txdctl = mmio_read(self.base, E1000E_TXDCTL);
            mmio_write(self.base, E1000E_TXDCTL, txdctl | TXDCTL_QUEUE_ENABLE);
            let mut txq_wait = 100;
            while txq_wait > 0 && mmio_read(self.base, E1000E_TXDCTL) & TXDCTL_QUEUE_ENABLE == 0 {
                Self::udelay(100);
                txq_wait -= 1;
            }
            mmio_write(
                self.base,
                E1000E_TXDCTL1,
                mmio_read(self.base, E1000E_TXDCTL),
            );
        }
        {
            let mut tctl = mmio_read(self.base, E1000E_TCTL);
            if self.is_pch_spt_or_later() {
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

        mmio_write(self.base, E1000E_RDTR, 0);
        mmio_write(self.base, E1000E_RADV, 0);
        mmio_write(self.base, E1000E_ITR, 0);
        self.disable_iame_automask();

        let rx_ring_pa = self.rx_ring.paddr();
        mmio_write(self.base, E1000E_RDBAL, rx_ring_pa as u32);
        mmio_write(self.base, E1000E_RDBAH, (rx_ring_pa >> 32) as u32);
        mmio_write(self.base, E1000E_RDLEN, (NUM_RX * size_of::<RxDesc>()) as u32);
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;
        mmio_write(self.base, E1000E_RDH, 0);
        let _ = mmio_read(self.base, E1000E_RDH);

        mmio_write(self.base, E1000E_RXCSUM, 0);
        {
            let mut rfctl = mmio_read(self.base, E1000E_RFCTL);
            rfctl |= RFCTL_EXTEN | RFCTL_NFSW_DIS | RFCTL_NFSR_DIS;
            mmio_write(self.base, E1000E_RFCTL, rfctl);
            let rfctl_rd = mmio_read(self.base, E1000E_RFCTL);
            self.use_extended_descriptors = (rfctl_rd & RFCTL_EXTEN) != 0;
        }
        mmio_write(self.base, E1000E_MRQC, 0);
        mmio_write(self.base, E1000E_VET, 0);
        self.program_srrctl_rx_queue0();

        if self.is_pch_lpt_or_later() {
            mmio_write(self.base, E1000E_FCTTV, 0xFFFF);
            mmio_write(self.base, E1000E_FCRTV, 0xFFFF);
            mmio_write(self.base, E1000E_FCRTL, 0x05048);
            mmio_write(self.base, E1000E_FCRTH, 0x05C20);
        }

        mmio_write(self.base, E1000E_RCTL, self.rctl_rx_bits() & !RCTL_EN);
        mmio_write(self.base, E1000E_VET, 0);
        for i in 0..128 {
            mmio_write(self.base, E1000E_VFTA_BASE + i, 0);
        }
        let rctl_v = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl_v & !RCTL_VFE);

        mmio_write(self.base, 0x0E30 / 4, 0);
        self.irq_mask_then_clear_icr();
        mmio_write(self.base, E1000E_IMS, IMS_REARM_LINUX);

        e1000e_wlog!(
            "[e1000e] datapath reprogrammed RDBAL={:#x} TDBAL={:#x} RCTL={:#x} TCTL={:#x}\n",
            mmio_read(self.base, E1000E_RDBAL),
            mmio_read(self.base, E1000E_TDBAL),
            mmio_read(self.base, E1000E_RCTL),
            mmio_read(self.base, E1000E_TCTL)
        );
    }

    /// Arm RX when STATUS.LU is set but the ring was never posted (common on QEMU
    /// when link comes up after probe, or when send() sets link_up without RX).
    unsafe fn ensure_rx_armed_if_link_up(&mut self) {
        if self.rx_link_armed {
            return;
        }
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU == 0 {
            return;
        }
        if self.device_id == 0x10d3 {
            if !self.link_up {
                self.link_up = true;
                self.link_speed = Self::speed_mbps_from_status(status);
                self.link_duplex = status & STATUS_FD != 0;
                self.config_collision_dist_linux();
            }
            self.enable_rx_after_link();
            e1000e_wlog!(
                "[e1000e] ensure_rx: QEMU link up @ {} Mb/s RX armed={}\n",
                self.link_speed,
                self.rx_link_armed
            );
            return;
        }
        if self.is_pch_lpt_or_later() {
            if self.try_rx_arm_pending_amt(status) {
                return;
            }
            let _ = self.apply_link_from_status(status);
        }
    }

    fn invalidate_rx_sw_state(&mut self) {
        self.rx_link_armed = false;
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;
        self.rx_post_since_doorbell = 0;
        self.invalidate_rx_desc_wb_sync();
        self.rx_sg_reset();
    }

    /// I219+AMT: arm RX from STATUS.LU only when AMT open is not resetting the MAC.
    unsafe fn try_rx_arm_pending_amt(&mut self, status: u32) -> bool {
        if self.rx_link_armed || !self.has_amt() || self.nic_open_done || self.amt_open_active.get() {
            return false;
        }
        if status & STATUS_LU == 0 {
            return false;
        }
        crate::klog_warn!(
            "[e1000e] AMT open pending — arm RX from STATUS (LU=1 open_done={})\n",
            self.nic_open_done
        );
        let _ = self.apply_link_from_status(status);
        if !self.rx_link_armed {
            self.link_up = true;
            self.enable_rx_after_link();
        }
        self.rx_link_armed
    }

    #[inline]
    fn mdio_unavailable(&self) -> bool {
        self.mdio_degraded || self.swflag_in_backoff()
    }

    /// One chunk of deferred PCH init — returns `true` when another job is needed.
    /// Never blocks boot: each step is a separate deferred_job.
    unsafe fn deferred_init_step(&mut self) -> bool {
        if !self.phy_init_pending {
            return false;
        }
        match self.deferred_init_step {
            0 => {
                e1000e_wlog!(
                    "[e1000e] deferred init 1/3: PHY workarounds (tag={})\n",
                    E1000E_DRIVER_TAG
                );
                if self.is_pch_lpt_or_later() {
                    self.pch_init_phy_workarounds();
                }
                self.deferred_init_step = 1;
                true
            }
            1 => {
                e1000e_wlog!("[e1000e] deferred init 2/3: MAC reset + datapath\n");
                self.reset_hw_ich8lan_linux(self.is_pch_lpt_or_later());
                // CTRL_RST puede romper DMA al dejar Bus Master PCI apagado.
                unsafe { self.restore_pci_command_bus_master() };
                self.init_hw_ich8lan_linux();
                self.reapply_datapath_after_mac_reset();
                self.pch_mdio_prepare_after_power();
                self.deferred_init_step = 2;
                true
            }
            2 => {
                e1000e_wlog!("[e1000e] deferred init 3/3: link bringup\n");
                self.phy_init_pending = false;
                self.pch_mdio_prepare_after_power();
                let now = timer_now_as_micros();
                self.link_watchdog_next_us = now;
                self.get_link_status = true;
                self.link_up = false;
                self.link_speed = 0;
                self.link_duplex = false;
                self.rx_link_armed = false;
                self.pch_try_early_link();
                if self.link_up && !self.rx_link_armed {
                    e1000e_wlog!("[e1000e] deferred: RX arm after early link\n");
                    self.enable_rx_after_link();
                }
                false
            }
            _ => {
                self.phy_init_pending = false;
                false
            }
        }
    }

    /// Abort a stuck MDIC transaction (READY never set — common after BIOS/ME handoff).
    unsafe fn mdic_clear_stuck(&self, log: bool) {
        let mdic = mmio_read(self.base, E1000E_MDIC);
        if mdic & MDIC_READY != 0 {
            return;
        }
        if mdic == 0 {
            return;
        }
        if log {
            e1000e_wlog!("[e1000e] MDIC stuck {:#x} — clearing\n", mdic);
        }
        mmio_write(self.base, E1000E_MDIC, 0);
        let _ = mmio_read(self.base, E1000E_MDIC);
        Self::udelay(100);
        for _ in 0..100 {
            if mmio_read(self.base, E1000E_MDIC) & MDIC_READY != 0 {
                let _ = mmio_read(self.base, E1000E_MDIC);
                break;
            }
            Self::udelay(50);
        }
    }

    unsafe fn log_mdio_diag(&self) {
        let mdic = mmio_read(self.base, E1000E_MDIC);
        let phy = (mdic >> MDIC_PHY_SHIFT) & 0x1F;
        let reg = (mdic >> MDIC_REG_SHIFT) & 0x1F;
        e1000e_wlog!(
            "[e1000e] MDIO diag: EXTCNF={:#x} STATUS={:#x} CTRL_EXT={:#x} FWSM={:#x} \
             MDIC={:#x} (pending phy={} reg={} ready={} err={})\n",
            mmio_read(self.base, E1000E_EXTCNF_CTRL),
            mmio_read(self.base, E1000E_STATUS),
            mmio_read(self.base, E1000E_CTRL_EXT),
            mmio_read(self.base, E1000E_FWSM),
            mdic,
            phy,
            reg,
            mdic & MDIC_READY != 0,
            mdic & MDIC_ERROR != 0
        );
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

        e1000e_wlog!("[e1000e] I219 pre-reset flush (state={:#x}): setting FEXTNVM bits only", hang_state);

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
            Self::clflush_cache_line_for_device(p as *const u8);
            p += 64;
        }
        core::arch::x86_64::_mm_sfence();
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::SeqCst);
    }

    /// Bare-metal: do not use CTRL_EXT.IAME — mask via IMC before reading ICR.
    unsafe fn disable_iame_automask(&self) {
        let mut ce = mmio_read(self.base, E1000E_CTRL_EXT);
        ce &= !CTRL_EXT_IAME;
        mmio_write(self.base, E1000E_CTRL_EXT, ce);
        mmio_write(self.base, E1000E_IAM, 0);
        let _ = mmio_read(self.base, E1000E_CTRL_EXT);
    }

    /// Mask all sources, then read ICR (read-to-clear) without IAME/IMS races.
    unsafe fn irq_mask_then_clear_icr(&self) -> u32 {
        mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
        let _ = mmio_read(self.base, E1000E_IMC);
        fence(Ordering::SeqCst);
        mmio_read(self.base, E1000E_ICR)
    }

    /// Device → CPU ordering fence before reading NIC write-back fields.
    #[inline]
    unsafe fn dma_rmb_after_device() {
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::Acquire);
    }

    /// Drain CPU store buffers before an MMIO doorbell (TDT/RDT).
    #[inline]
    #[cfg(target_arch = "x86_64")]
    fn store_fence_before_device_mmio() {
        unsafe {
            core::arch::asm!("sfence", options(nostack, preserves_flags));
        }
    }

    /// Evict one cache line before a device write-back read (Skylake+ / I219 hosts).
    ///
    /// `clflushopt` is ordered with respect to subsequent loads only after `sfence`;
    /// plain `clflush` can lose to speculative fills on real silicon.
    #[inline]
    #[cfg(target_arch = "x86_64")]
    unsafe fn clflush_cache_line_for_device(p: *const u8) {
        // Operand must be a memory address (`byte ptr [reg]`). `clflushopt (reg)` is rejected by GAS.
        core::arch::asm!(
            "clflushopt byte ptr [{0}]",
            in(reg) p,
            options(nostack, preserves_flags),
        );
    }

    /// Flush cache lines covering `[vaddr, vaddr+len)` (64-byte stride).
    unsafe fn clflush_range(vaddr: usize, len: usize) {
        if len == 0 {
            return;
        }
        let mut p = vaddr & !63;
        let end = vaddr.saturating_add(len);
        while p < end {
            Self::clflush_cache_line_for_device(p as *const u8);
            p += 64;
        }
    }

    /// Drop stale CPU cache lines before reading device write-back (WB mapping only).
    unsafe fn invalidate_cpu_cache_for_read(vaddr: usize, len: usize) {
        dma_sync_wb_from_device(vaddr, len);
    }

    /// FreeBSD `BUS_DMASYNC_PREWRITE` / Linux `dma_sync_for_device` on RX ring span.
    unsafe fn flush_rx_ring_descriptor_span(&self, start_idx: usize, count: usize) {
        dma_sync_rx_desc_span(
            &self.rx_ring,
            self.dma_uncached,
            start_idx,
            count,
            size_of::<RxDesc>(),
            DmaSyncDir::ToDevice,
        );
    }

    /// Descriptor WB on WB pages: never clflush a single 16 B descriptor — 4 share a line.
    /// Sync the whole 64 B line once per clean sweep (see [`Self::prepare_rx_desc_wb_read`]).
    const RX_WB_SYNC_NONE: u8 = 0xFF;

    /// Before reading DD/length: UC needs only a fence; WB syncs one cache line at a time.
    unsafe fn prepare_rx_desc_wb_read(&mut self, desc_idx: usize) {
        if self.dma_uncached {
            core::arch::x86_64::_mm_mfence();
            return;
        }
        let line = (desc_idx / RX_DESCS_PER_CACHE_LINE) as u8;
        if self.rx_wb_sync_line == line {
            core::arch::x86_64::_mm_mfence();
            return;
        }
        self.rx_wb_sync_line = line;
        let line_start = desc_idx - (desc_idx % RX_DESCS_PER_CACHE_LINE);
        // device→CPU only: ToDevice here clflushes stale CPU lines over NIC write-back on WB pages.
        let byte_off = line_start * size_of::<RxDesc>();
        dma_sync_region(
            &self.rx_ring,
            self.dma_uncached,
            byte_off,
            CACHE_LINE_SIZE,
            DmaSyncDir::FromDevice,
        );
    }

    #[inline]
    fn invalidate_rx_desc_wb_sync(&mut self) {
        self.rx_wb_sync_line = Self::RX_WB_SYNC_NONE;
    }
    ///
    /// `lfence` must precede the load — not after — or Skylake+ may return speculative zeros.
    #[inline]
    unsafe fn read_rx_wb_u64(desc_ptr: *const RxDesc) -> u64 {
        core::arch::x86_64::_mm_lfence();
        fence(Ordering::Acquire);
        read_volatile(core::ptr::addr_of!((*desc_ptr).reserved))
    }

    /// Invalidate a received frame buffer before CPU peek (WB cache only).
    ///
    /// Uses `mfence` (inside [`invalidate_cpu_cache_for_read`]) so PCIe WB completes before
    /// `clflushopt`; `lfence` alone before flush can drop in-flight packet bytes on PCH hosts.
    unsafe fn invalidate_rx_frame_buffer_for_cpu(buf_vaddr: usize, len: usize) {
        if len == 0 {
            return;
        }
        Self::invalidate_cpu_cache_for_read(buf_vaddr, len);
    }

    /// Single 64-bit load of the RX write-back region (staterr + length in `reserved`).
    fn dma_path_ready(&self) -> bool {
        if self.phy_init_pending {
            return false;
        }
        unsafe {
            PCI_ACCESS.read16(&PortOpsImpl, self.pci_loc, 0x04) & 0x0004 != 0
        }
    }

    #[inline]
    unsafe fn clear_rx_desc_wb(desc_ptr: *mut RxDesc) {
        compiler_fence(Ordering::SeqCst);
        write_volatile(core::ptr::addr_of_mut!((*desc_ptr).reserved), 0);
        fence(Ordering::Release);
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

    /// Wait until the TX ring is idle (TDH == TDT) before touching TARC (I219 pipeline).
    unsafe fn wait_tx_dma_quiescent(&self) {
        for _ in 0..200 {
            let tdh = mmio_read(self.base, E1000E_TDH);
            let tdt = mmio_read(self.base, E1000E_TDT);
            if tdh == tdt {
                break;
            }
            Self::udelay(10);
        }
        Self::udelay(100);
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
            e1000e_wlog!(
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

    /// Linux `e1000e_get_hw_control` — required on I219 before SWFLAG/PHY (CSME releases EXTCNF).
    unsafe fn e1000e_get_hw_control(&self) {
        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        if ctrl_ext & CTRL_EXT_DRV_LOAD == 0 {
            ctrl_ext |= CTRL_EXT_DRV_LOAD;
            mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
            let _ = mmio_read(self.base, E1000E_CTRL_EXT);
            Self::udelay(100_000);
        }
    }

    /// Wait until EXTCNF SWFLAG is clear (CSME released MDIO semaphore).
    unsafe fn wait_extcnf_swflag_clear(&self, max_ms: u32) -> bool {
        for _ in 0..max_ms {
            if mmio_read(self.base, E1000E_EXTCNF_CTRL) & EXTCNF_CTRL_SWFLAG == 0 {
                return true;
            }
            Self::udelay(1_000);
        }
        false
    }

    fn amt_open_reset_state(&mut self) {
        self.amt_open_phase = AMT_OPEN_IDLE;
        self.amt_open_sw_chunks = 0;
        self.amt_open_active.set(false);
    }

    /// One deferred-job slice of Linux `e1000e_open` (never blocks for multiple seconds).
    unsafe fn e1000e_open_bringup_step(&mut self) -> bool {
        if self.nic_open_done || !self.has_amt() {
            return false;
        }
        let now = timer_now_as_micros();
        self.amt_open_active.set(true);

        match self.amt_open_phase {
            AMT_OPEN_IDLE => {
                self.last_amt_open_attempt_us.set(now);
                self.amt_open_sw_chunks = 0;
                self.swflag_fail_logged.set(false);
                e1000e_vlog!("[e1000e] AMT open: start (chunked, tag={})\n", E1000E_DRIVER_TAG);
                self.e1000e_get_hw_control();
                self.amt_open_phase = AMT_OPEN_DRVLOAD;
                return true;
            }
            AMT_OPEN_DRVLOAD => {
                if mmio_read(self.base, E1000E_FWSM) & FWSM_RSPCIPHY != 0 {
                    self.amt_open_sw_chunks = 0;
                    self.amt_open_phase = AMT_OPEN_WAIT_SW;
                    return true;
                }
                self.amt_open_sw_chunks = self.amt_open_sw_chunks.saturating_add(1);
                if self.amt_open_sw_chunks < 30 {
                    Self::udelay(10_000);
                    return true;
                }
                self.amt_open_sw_chunks = 0;
                self.amt_open_phase = AMT_OPEN_WAIT_SW;
                return true;
            }
            AMT_OPEN_WAIT_SW => {
                if self.wait_extcnf_swflag_clear(AMT_OPEN_SWFLAG_CHUNK_MS) {
                    self.swflag_backoff_until_us.set(0);
                    self.amt_open_phase = AMT_OPEN_PHY_WA;
                    return true;
                }
                self.amt_open_sw_chunks = self.amt_open_sw_chunks.saturating_add(1);
                if self.amt_open_sw_chunks >= AMT_OPEN_SWFLAG_MAX_CHUNKS {
                    self.pch_swflag_force_release();
                    if self.wait_extcnf_swflag_clear(AMT_OPEN_SWFLAG_CHUNK_MS) {
                        self.swflag_backoff_until_us.set(0);
                        self.amt_open_phase = AMT_OPEN_PHY_WA;
                        return true;
                    }
                    self.amt_open_active.set(false);
                    self.amt_open_phase = AMT_OPEN_IDLE;
                    if !self.swflag_fail_logged.get() {
                        crate::klog_warn!(
                            "[e1000e] AMT open: SWFLAG busy (FWSM={:#x} EXTCNF={:#x}) — retry in {} s\n",
                            mmio_read(self.base, E1000E_FWSM),
                            mmio_read(self.base, E1000E_EXTCNF_CTRL),
                            E1000E_AMT_OPEN_RETRY_US / 1_000_000
                        );
                        self.swflag_fail_logged.set(true);
                    }
                    return false;
                }
                return true;
            }
            AMT_OPEN_PHY_WA => {
                e1000e_vlog!("[e1000e] AMT open: pch_init_phy_workarounds (linux probe order)\n");
                self.pch_init_phy_workarounds();
                self.amt_open_phase = AMT_OPEN_RESET;
                return true;
            }
            AMT_OPEN_RESET => {
                self.reset_hw_ich8lan_linux(true);
                self.restore_pci_command_bus_master();
                self.amt_open_phase = AMT_OPEN_INIT;
                return true;
            }
            AMT_OPEN_INIT => {
                self.init_hw_ich8lan_linux();
                self.reapply_datapath_after_mac_reset();
                self.pch_mdio_prepare_after_power();
                self.detect_phy_addr();
                self.amt_open_phase = AMT_OPEN_LINK;
                return true;
            }
            AMT_OPEN_LINK => {
                let status = mmio_read(self.base, E1000E_STATUS);
                if self.phy_mdio_responding {
                    self.get_link_status = true;
                    self.pch_kick_autoneg_mdio();
                    let _ = self.check_for_link_linux(E1000E_LINK_CHECK_REG26_MS);
                } else if status & STATUS_LU != 0 {
                    self.mdio_degraded = true;
                    let _ = self.apply_link_from_status(status);
                } else {
                    self.mac_setup_copper_link_linux();
                }
                self.pch_try_early_link();
                unsafe { self.ensure_rx_armed_if_link_up() };
                self.amt_open_active.set(false);
                self.amt_open_phase = AMT_OPEN_IDLE;

                if self.link_up || self.phy_mdio_responding {
                    self.swflag_fail_logged.set(false);
                    self.nic_open_done = true;
                    self.link_watchdog_next_us = now;
                    return false;
                }
                if !self.swflag_fail_logged.get() {
                    crate::klog_warn!(
                        "[e1000e] AMT open: no link/MDIO STATUS={:#x} EXTCNF={:#x} FWSM={:#x} — retry in {} s (tag={})\n",
                        status,
                        mmio_read(self.base, E1000E_EXTCNF_CTRL),
                        mmio_read(self.base, E1000E_FWSM),
                        E1000E_AMT_OPEN_RETRY_US / 1_000_000,
                        E1000E_DRIVER_TAG
                    );
                    self.swflag_fail_logged.set(true);
                }
                return false;
            }
            _ => {
                self.amt_open_reset_state();
                return false;
            }
        }
    }

    /// Linux `e1000e_release_hw_control` (unload / sleep paths).
    unsafe fn e1000e_release_hw_control(&self) {
        let mut ctrl_ext = mmio_read(self.base, E1000E_CTRL_EXT);
        ctrl_ext &= !CTRL_EXT_DRV_LOAD;
        mmio_write(self.base, E1000E_CTRL_EXT, ctrl_ext);
        let _ = mmio_read(self.base, E1000E_CTRL_EXT);
    }

    /// After failed SWFLAG claim: backoff MDIO and log once (never spin from `poll()`).
    fn note_swflag_failure(&self) {
        if self.amt_open_active.get() {
            return;
        }
        let now = timer_now_as_micros();
        self.swflag_backoff_until_us
            .set(now.saturating_add(E1000E_SWFLAG_BACKOFF_US));
        if !self.swflag_fail_logged.replace(true) {
            crate::klog_warn!(
                "[e1000e] EXTCNF SWFLAG unavailable (FW/CSME?) — MDIO paused ~60s, STATUS.LU fallback\n"
            );
        }
    }

    /// Linux `e1000_check_reset_block_ich8lan`: wait until ME allows PHY access.
    unsafe fn wait_fw_phy_rspciphy(&self) {
        for _ in 0..30 {
            if mmio_read(self.base, E1000E_FWSM) & FWSM_RSPCIPHY != 0 {
                return;
            }
            Self::udelay(10_000);
        }
    }

    #[inline]
    fn swflag_in_backoff(&self) -> bool {
        timer_now_as_micros() < self.swflag_backoff_until_us.get()
    }

    /// Linux `e1000_swflag_phy_acquire` last resort: clear stuck SWFLAG so MDIO can run.
    unsafe fn pch_swflag_force_release(&self) {
        let v = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if v & EXTCNF_CTRL_SWFLAG != 0 {
            e1000e_wlog!(
                "[e1000e] EXTCNF SWFLAG force-release ({:#x}) — CSME/AMT may have held MDIO\n",
                v
            );
            mmio_write(self.base, E1000E_EXTCNF_CTRL, v & !EXTCNF_CTRL_SWFLAG);
            let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            Self::udelay(1_000);
        }
    }

    /// Linux `e1000_acquire_swflag_ich8lan`: wait for clear, claim SWFLAG, verify read-back.
    unsafe fn pch_swflag_acquire(&self) -> bool {
        self.pch_swflag_acquire_ms(SWFLAG_VERIFY_SET_MS)
    }

    /// Longer verify window for probe / PHY workarounds (Linux `SW_FLAG_TIMEOUT`).
    unsafe fn pch_swflag_acquire_init(&self) -> bool {
        self.pch_swflag_acquire_ms(SWFLAG_VERIFY_SET_INIT_MS)
    }

    unsafe fn pch_swflag_acquire_ms(&self, verify_ms: u32) -> bool {
        if self.swflag_in_backoff() {
            return false;
        }

        let mut timeout = SWFLAG_WAIT_CLEAR_MS;
        while timeout > 0 {
            let v = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if v & EXTCNF_CTRL_SWFLAG == 0 {
                break;
            }
            Self::udelay(1_000);
            timeout -= 1;
        }
        if timeout == 0 {
            e1000e_vlog!("[e1000e] SWFLAG busy after {} ms wait\n", SWFLAG_WAIT_CLEAR_MS);
            if verify_ms <= SWFLAG_VERIFY_SET_MS {
                self.note_swflag_failure();
            }
            return false;
        }

        let mut ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        ext |= EXTCNF_CTRL_SWFLAG;
        mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);

        let mut verify = verify_ms;
        while verify > 0 {
            ext = mmio_read(self.base, E1000E_EXTCNF_CTRL);
            if ext & EXTCNF_CTRL_SWFLAG != 0 {
                self.swflag_backoff_until_us.set(0);
                self.swflag_fail_logged.set(false);
                return true;
            }
            Self::udelay(1_000);
            verify -= 1;
        }

        e1000e_vlog!(
            "[e1000e] SWFLAG claim failed FWSM={:#x} EXTCNF={:#x}\n",
            mmio_read(self.base, E1000E_FWSM),
            ext
        );
        ext &= !EXTCNF_CTRL_SWFLAG;
        mmio_write(self.base, E1000E_EXTCNF_CTRL, ext);
        let _ = mmio_read(self.base, E1000E_EXTCNF_CTRL);
        if verify_ms <= SWFLAG_VERIFY_SET_MS {
            self.note_swflag_failure();
        }
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
            e1000e_wlog!("[e1000e] STATUS.LAN_INIT_DONE timeout after PHY_RST");
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
            e1000e_wlog!("[e1000e] PCH: PHY_RST skipped (no SWFLAG)");
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
            e1000e_wlog!("[e1000e] clearing STATUS.PHYRA (status was {:#x})", s);
            mmio_write(self.base, E1000E_STATUS, s & !STATUS_PHYRA);
            let _ = mmio_read(self.base, E1000E_STATUS);
        }
    }

    /// MDIO read; caller must already hold SWFLAG on PCH.
    unsafe fn mdic_read_swheld(&self, phy_addr: u8, reg: u32, tries: u32) -> Option<u16> {
        let cmd =
            (reg << MDIC_REG_SHIFT) | ((phy_addr as u32) << MDIC_PHY_SHIFT) | MDIC_OP_READ;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..tries {
            Self::udelay(50);
            let mdic = mmio_read(self.base, E1000E_MDIC);
            if mdic & MDIC_READY != 0 {
                if mdic & MDIC_ERROR == 0 {
                    return Some((mdic & 0xFFFF) as u16);
                }
                self.mdic_clear_stuck(false);
                return None;
            }
        }
        self.mdic_clear_stuck(false);
        None
    }

    /// MDIO write; caller must already hold SWFLAG on PCH.
    unsafe fn mdic_write_swheld(&self, phy_addr: u8, reg: u32, val: u16, tries: u32) -> bool {
        let cmd = (val as u32)
            | (reg << MDIC_REG_SHIFT)
            | ((phy_addr as u32) << MDIC_PHY_SHIFT)
            | MDIC_OP_WRITE;
        mmio_write(self.base, E1000E_MDIC, cmd);
        for _ in 0..tries {
            Self::udelay(50);
            let mdic = mmio_read(self.base, E1000E_MDIC);
            if mdic & MDIC_READY != 0 {
                if (mdic & MDIC_ERROR) == 0 {
                    return true;
                }
                self.mdic_clear_stuck(false);
                return false;
            }
        }
        self.mdic_clear_stuck(false);
        false
    }

    unsafe fn mdic_read_probe(&self, phy_addr: u8, reg: u32) -> Option<u16> {
        let is_pch = self.is_pch_lpt_or_later();
        if is_pch && !self.pch_swflag_acquire() {
            return None;
        }
        let res = self.mdic_read_swheld(phy_addr, reg, MDIC_PROBE_TRIES);
        if is_pch {
            self.pch_swflag_release();
        }
        res
    }

    unsafe fn mdic_read_with_tries(&self, phy_addr: u8, reg: u32, tries: u32) -> Option<u16> {
        let is_pch = self.is_pch_lpt_or_later();
        if is_pch {
            let acquired = if self.has_amt() && !self.nic_open_done {
                self.pch_swflag_acquire_init()
            } else {
                self.pch_swflag_acquire()
            };
            if !acquired {
                return None;
            }
        }

        let res = self.mdic_read_swheld(phy_addr, reg, tries);

        if is_pch {
            self.pch_swflag_release();
        }
        res
    }

    unsafe fn mdic_read(&self, phy_addr: u8, reg: u32) -> Option<u16> {
        self.mdic_read_with_tries(phy_addr, reg, MDIC_POLL_TRIES)
    }

    unsafe fn mdic_read_init(&self, phy_addr: u8, reg: u32) -> Option<u16> {
        self.mdic_read_with_tries(phy_addr, reg, MDIC_INIT_TRIES)
    }

    unsafe fn mdic_read_phy_swheld(&self, phy_addr: u8, offset: u32) -> Option<u16> {
        if offset > MAX_PHY_MULTI_PAGE_REG {
            if !self.mdic_write_swheld(phy_addr, IGP_PHY_PAGE_SELECT, offset as u16, MDIC_POLL_TRIES)
            {
                return None;
            }
        }
        self.mdic_read_swheld(phy_addr, offset & 0x1F, MDIC_POLL_TRIES)
    }

    unsafe fn mdic_write_phy_swheld(&self, phy_addr: u8, offset: u32, val: u16) -> bool {
        if offset > MAX_PHY_MULTI_PAGE_REG {
            if !self.mdic_write_swheld(phy_addr, IGP_PHY_PAGE_SELECT, offset as u16, MDIC_POLL_TRIES)
            {
                return false;
            }
        }
        self.mdic_write_swheld(phy_addr, offset & 0x1F, val, MDIC_POLL_TRIES)
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
        if is_pch {
            let acquired = if self.has_amt() && !self.nic_open_done {
                self.pch_swflag_acquire_init()
            } else {
                self.pch_swflag_acquire()
            };
            if !acquired {
                return false;
            }
        }

        let ok = self.mdic_write_swheld(phy_addr, reg, val, MDIC_POLL_TRIES);

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

    /// reg26 reports link + settled speed. 100/1000 one-hot bits are definitive on I219;
    /// 10M (speed bits clear) needs AUTONEG_DONE — otherwise autoneg is still ramping to 1G.
    fn phy_reg26_speed_resolved(st2: u16) -> bool {
        if st2 == 0 || st2 == 0xFFFF || st2 & PHY_STATUS2_LINK_UP == 0 {
            return false;
        }
        let bits = st2 & PHY_STATUS2_SPEED_MASK;
        if bits == PHY_STATUS2_SPEED_1000 || bits == PHY_STATUS2_SPEED_100 {
            return true;
        }
        if bits == 0 {
            return false;
        }
        false
    }

    /// Highest common speed from current autoneg registers.
    unsafe fn phy_mii_hcd_speed(&self, phy_addr: u8) -> u32 {
        let anar = self.mdic_read(phy_addr, MII_ADVERTISE).unwrap_or(0);
        let lpa = self.mdic_read(phy_addr, MII_LPA).unwrap_or(0);
        let stat1000 = self.mdic_read(phy_addr, MII_STAT1000).unwrap_or(0);
        let ctrl1000 = self.mdic_read(phy_addr, MII_CTRL1000).unwrap_or(0);
        Self::phy_mii_hcd_speed_mbps(anar, lpa, stat1000, ctrl1000)
    }

    /// reg26 resolved rate below MII HCD (caller supplies `hcd` from one MDIO pass).
    fn reg26_resolved_below_hcd(st2: u16, hcd: u32) -> Option<u32> {
        if st2 & PHY_STATUS2_AUTONEG_DONE == 0 {
            return None;
        }
        let resolved = Self::speed_mbps_from_phy_st2(st2);
        if resolved < hcd {
            Some(hcd)
        } else {
            None
        }
    }

    fn phy_reg26_valid_for_lock(st2: u16) -> bool {
        st2 != 0
            && st2 != 0xFFFF
            && Self::phy_resolve_speed_duplex_st2(st2).is_some()
    }

    /// On I219 + GbE partner, do not bring link up at 10/100M — locks MAC before reg26 reaches 1G.
    unsafe fn pch_defer_link_until_gig_ready(
        &mut self,
        phy: u8,
        speed: u32,
        st2: u16,
    ) -> bool {
        if !self.is_pch_lpt_or_later()
            || self.device_id == 0x10d3
            || self.link_10m_degraded
            || self.mdio_unavailable()
            || speed >= SPEED_1000
            || self.link_up
        {
            return false;
        }
        let hcd = self.phy_mii_hcd_speed(phy);
        if hcd < SPEED_1000 {
            return false;
        }
        let stat1000 = self.mdic_read(phy, MII_STAT1000).unwrap_or(0);
        if stat1000 & STAT1000_LP_1000FULL == 0 {
            return false;
        }
        if speed >= SPEED_100 {
            let anar = self.mdic_read(phy, MII_ADVERTISE).unwrap_or(0);
            let lpa = self.mdic_read(phy, MII_LPA).unwrap_or(0);
            if anar & lpa & (ADVERTISE_100FULL | ADVERTISE_100HALF) != 0 {
                return false;
            }
        }
        if Self::phy_reg26_speed_resolved(st2) && speed >= SPEED_100 && speed < SPEED_1000 {
            return false;
        }
        e1000e_vlog!(
            "[e1000e] link deferred: {} Mb/s reg26={:#x} ({}) MII_HCD={} Mb/s — wait 1G (tag={})\n",
            speed,
            st2,
            Self::hv_m_status_label(st2),
            hcd,
            E1000E_DRIVER_TAG
        );
        if !self.link_up {
            crate::klog_warn!(
                "[e1000e] waiting for 1G: reg26={:#x} ({}) MII_HCD={} Mb/s (tag={})\n",
                st2,
                Self::hv_m_status_label(st2),
                hcd,
                E1000E_DRIVER_TAG
            );
        }
        self.get_link_status = true;
        true
    }

    /// Copper L1 ready — wait for BMSR ANEG or settled reg26 (Linux ~29 s to 1G on I219).
    #[inline]
    unsafe fn phy_copper_ready_for_link_up(&self, bmsr: u16, status: u32, st2: u16) -> bool {
        if bmsr == 0 || bmsr == 0xFFFF || (bmsr & 0x0004) == 0 {
            return self.mdio_unavailable() && status & STATUS_LU != 0;
        }
        if (bmsr & BMSR_ANEG_COMPLETE) != 0 {
            return true;
        }
        if Self::phy_reg26_speed_resolved(st2) {
            return true;
        }
        self.mdio_unavailable() && status & STATUS_LU != 0
    }

    /// STATUS.LU cleared — cable out or partner down.
    unsafe fn pch_note_status_link_down(&mut self) {
        if !self.link_up {
            return;
        }
        crate::klog_warn!("[e1000e] link down (STATUS.LU clear)\n");
        self.link_up = false;
        self.link_speed = 0;
        self.link_duplex = false;
        self.rx_link_armed = false;
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

    /// STATUS-only link: reject transient garbage (I219 often shows 1000M half before ANEG).
    fn speed_duplex_from_status_sane(status: u32) -> Option<(u32, bool)> {
        if status & STATUS_LU == 0 {
            return None;
        }
        let speed = Self::speed_mbps_from_status(status);
        let mut duplex_full = status & STATUS_FD != 0;
        if speed == SPEED_1000 {
            return Some((speed, true));
        }
        Some((speed, duplex_full))
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
        // I219: trust reg26 when ANEG-done or link+speed bits are set (not only 0x1000).
        if self.is_pch_lpt_or_later()
            && !self.mdio_unavailable()
            && (st2 & PHY_STATUS2_AUTONEG_DONE != 0 || Self::phy_reg26_speed_resolved(st2))
        {
            if let Some((idx, dpx)) = Self::phy_resolve_speed_duplex_st2(st2) {
                let speed = match idx {
                    2 => SPEED_1000,
                    1 => SPEED_100,
                    _ => SPEED_10,
                };
                let mut duplex_full = dpx != 0;
                if let Some((cs, cs_fd)) = self.phy_cs17_resolved(phy_addr) {
                    if cs > speed {
                        return (cs, cs_fd, "reg17");
                    }
                }
                if speed == SPEED_1000 {
                    duplex_full = true;
                }
                return (speed, duplex_full, "reg26");
            }
        }

        let status = mmio_read(self.base, E1000E_STATUS);
        let mut speed = Self::speed_mbps_from_status(status);
        let mut duplex_full = status & STATUS_FD != 0;
        let mut src = "STATUS";
        if speed == SPEED_1000 {
            duplex_full = true;
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
        e1000e_wlog!(
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

    /// True when FRCSPD/FRCDPX lock is safe — not during transient 10/100M before MII HCD settles.
    fn mac_should_lock_ctrl_from_st2(&self, st2: u16, speed: u32, hcd: u32) -> bool {
        if !Self::phy_reg26_valid_for_lock(st2) {
            return false;
        }
        if self.link_10m_degraded {
            return true;
        }
        if speed >= SPEED_1000 {
            return true;
        }
        if let Some(h) = Self::reg26_resolved_below_hcd(st2, hcd) {
            if h > speed {
                return false;
            }
        }
        if speed == SPEED_10 && hcd > SPEED_10 {
            return false;
        }
        if speed == SPEED_100 && hcd >= SPEED_1000 {
            return false;
        }
        speed >= SPEED_100
    }

    /// Lock CTRL to reg26 at 1G+, or leave SLU+ASDE autoneg free while PHY ramps past 10M.
    unsafe fn mac_apply_ctrl_for_operational_link(&self, st2: u16, speed: u32) {
        if self.mdio_unavailable() {
            return;
        }
        let phy = self.active_phy_addr();
        let hcd = self.phy_mii_hcd_speed(phy);
        if self.mac_should_lock_ctrl_from_st2(st2, speed, hcd) {
            if self.mac_lock_ctrl_from_st2(st2) {
                return;
            }
        }
        if Self::phy_reg26_valid_for_lock(st2) {
            self.mac_setup_copper_link_linux();
            self.mac_speed_sync_pulse();
            e1000e_vlog!(
                "[e1000e] CTRL autoneg (no FRCSPD): {} Mb/s reg26={:#x} ({}) MII_HCD={} Mb/s\n",
                speed,
                st2,
                Self::hv_m_status_label(st2),
                hcd
            );
        } else if speed >= SPEED_100 {
            self.mac_sync_ctrl_speed_mbps(speed, self.link_duplex);
            e1000e_vlog!(
                "[e1000e] CTRL from STATUS: {} Mb/s (reg26 invalid {:#x}) MII_HCD={} Mb/s\n",
                speed,
                st2,
                hcd
            );
        } else {
            self.mac_apply_link_up_autoneg();
            e1000e_vlog!(
                "[e1000e] CTRL autoneg (no reg26): {} Mb/s MII_HCD={} Mb/s\n",
                speed,
                hcd
            );
        }
    }

    /// Extra reg26 polling when link is still at 10M but MII HCD allows faster.
    unsafe fn phy_wait_reg26_if_transient_10m(&self, phy: u8, st2: u16, max_ms: u32) -> u16 {
        if self.link_10m_degraded {
            return st2;
        }
        let (speed, _, _) = self.resolve_link_speed_duplex_linux(phy, st2);
        if speed != SPEED_10 {
            return st2;
        }
        let hcd = self.phy_mii_hcd_speed(phy);
        if Self::reg26_resolved_below_hcd(st2, hcd).is_some() || hcd > SPEED_10 {
            return self.phy_wait_reg26_settled(phy, max_ms);
        }
        st2
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
        e1000e_vlog!(
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
        let hcd = self.phy_mii_hcd_speed(phy_addr);
        Self::reg26_resolved_below_hcd(st2, hcd)
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

    /// I219 TX errata: TARC0_SPEED_MODE + K1 off at 10/100M before any post-link TX.
    unsafe fn configure_link_tx_path(&self, speed_mbps: u32) {
        if self.is_pch_lpt_or_later() && speed_mbps < SPEED_1000 {
            self.pch_disable_k1();
        }
        if self.is_pch_spt_or_later() {
            self.program_tarc_with_tctl_gate(speed_mbps);
        } else {
            self.program_tarc_for_speed(speed_mbps);
        }
    }

    /// Linux netdev link-up: program TARC while TCTL is off, then re-enable TCTL.
    unsafe fn program_tarc_with_tctl_gate(&self, speed_mbps: u32) {
        if !self.is_pch_spt_or_later() {
            self.program_tarc_for_speed(speed_mbps);
            return;
        }
        self.wait_tx_dma_quiescent();
        let mut tctl = mmio_read(self.base, E1000E_TCTL);
        let was_en = tctl & TCTL_EN != 0;
        if was_en {
            mmio_write(self.base, E1000E_TCTL, tctl & !TCTL_EN);
            let _ = mmio_read(self.base, E1000E_TCTL);
            self.wait_tx_dma_quiescent();
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
            e1000e_wlog!(
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

    /// Linux `e1000_desc_unused` (kernel `e1000_main.c`).
    fn rx_desc_unused(&self) -> usize {
        if self.rx_next_to_clean > self.rx_next_to_use {
            self.rx_next_to_clean - self.rx_next_to_use - 1
        } else {
            NUM_RX + self.rx_next_to_clean - self.rx_next_to_use - 1
        }
    }

    /// Descriptors posted to hardware (RDH..RDT span) — used for recycle doorbell pressure.
    fn rx_hw_posted_slots(&self) -> usize {
        unsafe {
            let rdh = mmio_read(self.base, E1000E_RDH) as usize;
            let rdt = mmio_read(self.base, E1000E_RDT) as usize;
            if rdt >= rdh {
                rdt - rdh
            } else {
                NUM_RX - rdh + rdt
            }
        }
    }

    /// Rewrite a cleaned RX descriptor in host memory only (RDT doorbell is batched).
    unsafe fn repost_rx_slot(&mut self, idx: usize) {
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let desc_ptr = ring.add(idx);
        write_volatile(
            core::ptr::addr_of_mut!((*desc_ptr).addr),
            self.rx_buf_paddr(idx),
        );
        Self::clear_rx_desc_wb(desc_ptr);
        self.flush_rx_ring_descriptor_span(idx, 1);
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// Ring RDT after batched recycle reposts (`last posted = rx_next_to_clean - 1`).
    unsafe fn rx_doorbell_recycle_if_needed(&mut self, force: bool) {
        if self.rx_post_since_doorbell == 0 {
            return;
        }
        let hw_posted = self.rx_hw_posted_slots();
        let critical = hw_posted <= RX_BUFFER_WRITE;
        if !force
            && self.rx_post_since_doorbell < RX_BUFFER_WRITE as u16
            && !critical
        {
            return;
        }
        let last_posted = if self.rx_next_to_clean == 0 {
            NUM_RX - 1
        } else {
            self.rx_next_to_clean - 1
        };
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
        mmio_write_flush(self.base, E1000E_RDT, last_posted as u32);
        self.rx_post_since_doorbell = 0;
    }

    /// Linux `e1000_alloc_rx_buffers` — post `count` descriptors; RDT doorbell is batched.
    unsafe fn post_one_rx_buffer(&mut self) -> bool {
        if self.rx_desc_unused() == 0 {
            return false;
        }
        let ring = self.rx_ring.as_ptr::<RxDesc>();
        let i = self.rx_next_to_use;
        let desc_ptr = unsafe { ring.add(i) };
        unsafe {
            write_volatile(core::ptr::addr_of_mut!((*desc_ptr).addr), self.rx_buf_paddr(i));
            Self::clear_rx_desc_wb(desc_ptr);
        }
        // Buffer sync on post is unnecessary for UC; descriptor flush is batched at RDT doorbell.
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
        let posted = self.rx_post_since_doorbell as usize;
        if posted > 0 {
            let tail = self.rx_next_to_use;
            let start = (tail + NUM_RX - posted) % NUM_RX;
            self.flush_rx_ring_descriptor_span(start, posted);
        }
        let i = self.rx_next_to_use;
        let last = if i == 0 { NUM_RX - 1 } else { i - 1 };
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);
        mmio_write_flush(self.base, E1000E_RDT, last as u32);
        self.rx_post_since_doorbell = 0;
    }

    unsafe fn flush_rx_post_queue(&mut self) {
        self.rx_doorbell_recycle_if_needed(true);
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
    ///   3. Write RDH = 0 while RCTL.EN is clear.
    ///   4. Re-post descriptors into the ring (reinit_rx_ring) — clflushopt+sfence
    ///      so physical RAM already contains valid buffer addresses and zero WB fields.
    ///   5. Program SRRCTL (extended single-buffer; trust write if readback is 0).
    ///   6. Configure RXDCTL burst parameters (no QUEUE_ENABLE yet).
    ///   7. Enable RXDCTL.QUEUE_ENABLE (PCH-SPT/later) and wait for it to latch.
    ///   8. Post buffers and doorbell RDT while RCTL.EN is still clear (ring not empty).
    ///   9. Enable RCTL.EN last — I219 PCH ignores post-EN RDT if EN latched on empty ring.
    /// Returns false if the ring was not posted or RCTL.EN did not latch (do not set rx_link_armed).
    unsafe fn arm_rx_unit_linux(&mut self) -> bool {
        // Step 0: Ensure ring base registers survived any MAC reset (CTRL_RST clears them).
        let rdbal = mmio_read(self.base, E1000E_RDBAL);
        if rdbal == 0 {
            let rx_ring_pa = self.rx_ring.paddr();
            mmio_write(self.base, E1000E_RDBAL, rx_ring_pa as u32);
            mmio_write(self.base, E1000E_RDBAH, (rx_ring_pa >> 32) as u32);
            mmio_write(self.base, E1000E_RDLEN, (NUM_RX * size_of::<RxDesc>()) as u32);
            e1000e_wlog!(
                "[e1000e] arm_rx: restored RDBAL={:#x}\n",
                mmio_read(self.base, E1000E_RDBAL)
            );
        }

        // Step 1: Ensure RX engine is disabled; drain in-flight PCIe DMA before touching rings.
        let rctl = mmio_read(self.base, E1000E_RCTL);
        mmio_write(self.base, E1000E_RCTL, rctl & !RCTL_EN);
        let _ = mmio_read(self.base, E1000E_RCTL);
        self.wait_rx_dma_quiescent();
        self.rx_sg_reset();
        self.rx_post_since_doorbell = 0;
        self.invalidate_rx_desc_wb_sync();

        // Step 2: Reset indices.
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;

        // Step 3: RDH=0 only while the engine is off.
        mmio_write(self.base, E1000E_RDH, 0);
        let _ = mmio_read(self.base, E1000E_RDH);

        // Step 4: Re-initialize and flush descriptors to RAM.
        // reinit_rx_ring writes buffer addresses + zeroes the WB fields for every
        // slot, then clflushes them into physical RAM. The RDT doorbell is NOT
        // touched here — the engine is still off.
        self.reinit_rx_ring();

        // Step 5: Program SRRCTL while RX is still disabled.
        self.program_srrctl_rx_queue0();

        // Step 6: Configure RXDCTL parameters (DMA burst) without enabling the queue yet.
        let mut rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        rxdctl &= 0xFFFF_C000;
        rxdctl |= RXDCTL_DMA_BURST;
        mmio_write(self.base, E1000E_RXDCTL, rxdctl);
        let _ = mmio_read(self.base, E1000E_RXDCTL);

        // Step 7: Enable RXDCTL.QUEUE_ENABLE (PCH-SPT or later) and wait for it to latch
        // BEFORE raising RCTL.EN, so the DMA queue is fully enabled when EN fires.
        if self.is_pch_spt_or_later() {
            let mut rxd = mmio_read(self.base, E1000E_RXDCTL);
            rxd |= RXDCTL_QUEUE_ENABLE;
            mmio_write(self.base, E1000E_RXDCTL, rxd);
            let mut rxq_wait = 200;
            while rxq_wait > 0 && mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE == 0 {
                Self::udelay(100);
                rxq_wait -= 1;
            }
            if mmio_read(self.base, E1000E_RXDCTL) & RXDCTL_QUEUE_ENABLE == 0 {
                crate::klog_warn!("[e1000e] arm_rx: RXDCTL.QUEUE_ENABLE timeout — aborting\n");
                return false;
            }
        }

        // Step 8: Post buffers + RDT doorbell with RX engine still off (non-empty ring).
        let n = self.rx_desc_unused().min(RX_BOOT_POST_MAX);
        if n == 0 {
            crate::klog_warn!("[e1000e] arm_rx: no free RX descriptors\n");
            return false;
        }
        self.alloc_rx_buffers(n, true);

        // I219: posted RDT may lag behind MMIO readback — wait before empty-ring check.
        Self::udelay(500);
        mmio_pcie_posted_flush(self.base);
        let _ = mmio_read(self.base, E1000E_RDT);
        Self::udelay(200);

        let rdh = mmio_read(self.base, E1000E_RDH);
        let rdt = mmio_read(self.base, E1000E_RDT);
        if rdh == rdt {
            crate::klog_warn!(
                "[e1000e] arm_rx: empty ring RDH=RDT={} after posting {} buffers (uc={})\n",
                rdh,
                n,
                self.dma_uncached
            );
            return false;
        }

        // Step 9: Enable RX engine after hardware has observed a non-empty ring.
        let rctl = self.rctl_rx_bits() | RCTL_EN;
        if rctl & RCTL_BAM == 0 {
            crate::klog_warn!("[e1000e] arm_rx: RCTL.BAM clear — DHCP/broadcast would fail\n");
        }
        mmio_write_flush(self.base, E1000E_RCTL, rctl);
        let rctl_rb = mmio_read(self.base, E1000E_RCTL);
        if rctl_rb & RCTL_EN == 0 {
            crate::klog_warn!(
                "[e1000e] RCTL.EN did not latch ({:#x}) — RX down\n",
                rctl_rb
            );
            return false;
        }

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
        true
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
        for phy_addr in [self.phy_addr, 2u8, 1u8] {
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
        e1000e_wlog!("[e1000e] MDIO: no PHY responding on addr 1 or 2\n");
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
    /// OSDev guide: program RCTL filters first; RCTL.EN is set separately in `arm_rx_unit_linux`.
    unsafe fn rctl_rx_bits(&self) -> u32 {
        let mut rctl = mmio_read(self.base, E1000E_RCTL);
        rctl &= !(RCTL_MO_MASK | 0xC0); // MO + loopback mode → LBM_NO
        rctl |= RCTL_BAM | RCTL_UPE | RCTL_MPE | RCTL_SECRC;
        // OSDev: clear SBP; SZ_2048 + SECRC + BAM
        rctl &= !(RCTL_SBP | RCTL_DTYP_PS | RCTL_BSEX | RCTL_RX_SZ_MASK | RCTL_EN);
        rctl |= RCTL_SZ_2048 | RCTL_LPE;
        rctl
    }

    /// Program queue-0 SRRCTL when the register exists (Linux e1000e uses RCTL_SZ_2048 for
    /// standard RX; SRRCTL is mainly for packet-split). On I219-V the MMIO readback is often 0.
    unsafe fn program_srrctl_rx_queue0(&mut self) {
        if self.srrctl_absent {
            return;
        }
        // SRRCTL programming must not tear down an already-enabled RX queue.
        // Callers should keep RX disabled (RCTL.EN=0) while touching SRRCTL.
        if mmio_read(self.base, E1000E_RCTL) & RCTL_EN != 0 {
            e1000e_wlog!("[e1000e] SRRCTL write skipped: RCTL.EN=1\n");
            return;
        }

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
            e1000e_vlog!(
                "[e1000e] SRRCTL no readback (wrote {:#x}, read 0) — keep RFCTL_EXTEN, trust write\n",
                v
            );
        } else if (rd & SRRCTL_DESCTYPE_MASK) != 0 {
            e1000e_wlog!(
                "[e1000e] SRRCTL DESCTYPE!=0 (packet-split?) (DESCTYPE={:#x}) — keeping RFCTL_EXTEN\n",
                rd & SRRCTL_DESCTYPE_MASK
            );
        } else if (rd & 0xF) != SRRCTL_BSIZE_2K {
            e1000e_wlog!(
                "[e1000e] SRRCTL bad readback: wrote {:#x} got {:#x}\n",
                v,
                rd
            );
        }
        // Leave RCTL.EN off; arm_rx_unit_linux() doorbells RDT then sets EN.
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

    /// Drop obviously broken L2/L3 frames; tolerate I219 short descriptor WB lengths.
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
            0x86dd => Self::rx_ipv6_deliverable(data),
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
        Self::scan_dhcp_magic_cookie_offset(data, l2, ihl).is_some()
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

    /// Do not require ip_tot_len bytes when the DMA buffer already holds a valid L3 header
    /// (I219 often under-reports length in the descriptor write-back).
    fn rx_ipv4_deliverable(data: &[u8]) -> bool {
        let Some((l2, ihl, frame_need, is_dhcp)) = Self::eth_ipv4_header_info(data) else {
            return data.len() >= 14 + 20 && (data[14] >> 4) == 4;
        };
        if data.len() < l2 + ihl {
            return false;
        }
        if data[l2 + 9] == 17 {
            if is_dhcp {
                return Self::dhcp_cookie_valid_in_frame(data)
                    || data.len() >= l2 + ihl + 28;
            }
            return true;
        }
        if data.len() >= frame_need {
            return true;
        }
        data.len() >= l2 + ihl + 4
    }

    fn rx_ipv6_deliverable(data: &[u8]) -> bool {
        let Some((l2, _)) = Self::eth_ipv6_header_info(data) else {
            return false;
        };
        data.len() >= l2 + 40 && (data[l2] >> 4) == 6
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

    /// Scan BOOTP vendor area for RFC 2131 magic cookie (I219 may DMA it off the fixed offset).
    fn scan_dhcp_magic_cookie_offset(buf: &[u8], l2: usize, ihl: usize) -> Option<usize> {
        const MAGIC: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
        let bootp = l2 + ihl + 8;
        if buf.len() < bootp + 240 {
            return None;
        }
        let search_start = bootp + 236;
        let search_end = buf.len().min(bootp + 576);
        if search_end <= search_start {
            return None;
        }
        for i in search_start..search_end.saturating_sub(3) {
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
        let needs_inv = self.rx_needs_cache_invalidation();
        let inv = desc_len.max(512).min(BUF_SIZE);
        if needs_inv {
            Self::invalidate_rx_frame_buffer_for_cpu(buf_vaddr, inv);
        }
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
                if needs_inv {
                    Self::invalidate_rx_frame_buffer_for_cpu(buf_vaddr, frame_need.min(BUF_SIZE));
                }
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
                e1000e_wlog!(
                    "[e1000e] RX len fix: desc {} → {} (cookie @236)\n",
                    desc_len,
                    frame_need
                );
            } else {
                if needs_inv {
                    Self::invalidate_rx_frame_buffer_for_cpu(buf_vaddr, BUF_SIZE);
                }
                let full = core::slice::from_raw_parts(buf_vaddr as *const u8, BUF_SIZE);
                if is_bootp && Self::peek_dhcp_magic_cookie(full, l2, ihl) {
                    copy_len = frame_need;
                    e1000e_wlog!(
                        "[e1000e] RX len fix: desc {} → {} (BOOTP cookie after reinvalidate)\n",
                        desc_len,
                        frame_need
                    );
                } else if frame_need <= full.len() && Self::eth_ipv4_frame_complete(&full[..frame_need]) {
                    copy_len = frame_need;
                    e1000e_wlog!(
                        "[e1000e] RX len fix: desc {} → {} (ip_tot_len, is_bootp={})\n",
                        desc_len,
                        frame_need,
                        is_bootp
                    );
                } else if is_bootp {
                    e1000e_vlog!(
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
        if !self.mdic_write_swheld(
            BM_PHY_MDIO_ADDR,
            BM_WUC_ADDRESS_OPCODE,
            reg as u16,
            MDIC_POLL_TRIES,
        ) {
            return None;
        }
        self.mdic_read_swheld(BM_PHY_MDIO_ADDR, BM_WUC_DATA_OPCODE, MDIC_POLL_TRIES)
    }

    unsafe fn pch_bm_wuc_reg_write(&self, reg: u8, val: u16) -> bool {
        if !self.mdic_write_swheld(
            BM_PHY_MDIO_ADDR,
            BM_WUC_ADDRESS_OPCODE,
            reg as u16,
            MDIC_POLL_TRIES,
        ) {
            return false;
        }
        self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_DATA_OPCODE, val, MDIC_POLL_TRIES)
    }

    /// Linux `e1000_enable_phy_wakeup_reg_access_bm` (always MDIO PHY address 1).
    unsafe fn pch_bm_wuc_access_begin(&self) -> Option<u16> {
        if !self.pch_swflag_acquire() {
            return None;
        }
        let page_port = (BM_PORT_CTRL_PAGE << IGP_PAGE_SHIFT) as u16;
        if !self.mdic_write_swheld(
            BM_PHY_MDIO_ADDR,
            IGP_PHY_PAGE_SELECT,
            page_port,
            MDIC_POLL_TRIES,
        ) {
            self.pch_swflag_release();
            return None;
        }
        let Some(saved) =
            self.mdic_read_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG, MDIC_POLL_TRIES)
        else {
            self.pch_swflag_release();
            return None;
        };
        let mut temp = saved | BM_WUC_ENABLE_BIT;
        temp &= !(BM_WUC_ME_WU_BIT | BM_WUC_HOST_WU_BIT);
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG, temp, MDIC_POLL_TRIES) {
            self.pch_swflag_release();
            return None;
        }
        // Linux: select BM_WUC_PAGE (800) for host wakeup register access.
        let wuc_page = (BM_WUC_PAGE << IGP_PAGE_SHIFT) as u16;
        if !self.mdic_write_swheld(BM_PHY_MDIO_ADDR, IGP_PHY_PAGE_SELECT, wuc_page, MDIC_POLL_TRIES)
        {
            let restore = saved & !(BM_WUC_ME_WU_BIT | BM_WUC_HOST_WU_BIT | BM_WUC_ENABLE_BIT);
            let _ = self.mdic_write_swheld(
                BM_PHY_MDIO_ADDR,
                BM_WUC_ENABLE_REG,
                restore,
                MDIC_POLL_TRIES,
            );
            self.pch_swflag_release();
            return None;
        }
        Some(saved)
    }

    unsafe fn pch_bm_wuc_access_end(&self, saved: u16) {
        let page_port = (BM_PORT_CTRL_PAGE << IGP_PAGE_SHIFT) as u16;
        let _ = self.mdic_write_swheld(
            BM_PHY_MDIO_ADDR,
            IGP_PHY_PAGE_SELECT,
            page_port,
            MDIC_POLL_TRIES,
        );
        // Never leave BM_WUC_ENABLE set after BM page-800 sync — firmware often has it
        // after S3; an active PHY BM filter steals frames before the MAC ring (GPRC=0).
        let restore = saved & !(BM_WUC_ME_WU_BIT | BM_WUC_HOST_WU_BIT | BM_WUC_ENABLE_BIT);
        let _ = self.mdic_write_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG, restore, MDIC_POLL_TRIES);
        self.pch_swflag_release();
    }

    /// Linux `e1000_init_hw_ich8lan`: clear BM_PORT_GEN_CFG HOST_WU after reset.
    unsafe fn pch_clear_bm_port_gen_host_wu(&self) {
        if !self.is_pch_lpt_or_later() || !self.pch_swflag_acquire() {
            return;
        }
        let page_port = (BM_PORT_CTRL_PAGE << IGP_PAGE_SHIFT) as u16;
        if !self.mdic_write_swheld(
            BM_PHY_MDIO_ADDR,
            IGP_PHY_PAGE_SELECT,
            page_port,
            MDIC_POLL_TRIES,
        ) {
            self.pch_swflag_release();
            return;
        }
        if let Some(reg) =
            self.mdic_read_swheld(BM_PHY_MDIO_ADDR, BM_PORT_GEN_CFG, MDIC_POLL_TRIES)
        {
            let cleared = reg & !BM_WUC_HOST_WU_BIT;
            if cleared != reg {
                let _ = self.mdic_write_swheld(
                    BM_PHY_MDIO_ADDR,
                    BM_PORT_GEN_CFG,
                    cleared,
                    MDIC_POLL_TRIES,
                );
                e1000e_vlog!(
                    "[e1000e] BM_PORT_GEN_CFG HOST_WU cleared ({:#x} -> {:#x})\n",
                    reg,
                    cleared
                );
            }
        }
        self.pch_swflag_release();
    }

    /// Ensure RAL0/RAH0 carry our MAC with AV before copying filters to BM page-800.
    unsafe fn ensure_mac_rar0_valid(&self) {
        if !self.is_valid_mac() {
            return;
        }
        let rah = mmio_read(self.base, E1000E_RAH0);
        if rah & RAH_AV != 0 {
            return;
        }
        let mac_low = u32::from_le_bytes([self.mac[0], self.mac[1], self.mac[2], self.mac[3]]);
        let mac_high = u32::from_le_bytes([self.mac[4], self.mac[5], 0, 0]);
        mmio_write(self.base, E1000E_RAL0, mac_low);
        mmio_write(self.base, E1000E_RAH0, mac_high | RAH_AV);
        let _ = mmio_read(self.base, E1000E_RAH0);
        crate::klog_warn!(
            "[e1000e] RAH0 AV restored {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (tag={})\n",
            self.mac[0],
            self.mac[1],
            self.mac[2],
            self.mac[3],
            self.mac[4],
            self.mac[5],
            E1000E_DRIVER_TAG
        );
    }

    /// Linux `disable_phy_wakeup_reg_access_bm`: ensure BM WUC page access is off for host RX.
    unsafe fn pch_ensure_bm_wuc_disabled(&self) {
        if !self.is_pch_lpt_or_later() || !self.pch_swflag_acquire() {
            return;
        }
        let page_port = (BM_PORT_CTRL_PAGE << IGP_PAGE_SHIFT) as u16;
        if !self.mdic_write_swheld(
            BM_PHY_MDIO_ADDR,
            IGP_PHY_PAGE_SELECT,
            page_port,
            MDIC_POLL_TRIES,
        ) {
            self.pch_swflag_release();
            return;
        }
        if let Some(reg) =
            self.mdic_read_swheld(BM_PHY_MDIO_ADDR, BM_WUC_ENABLE_REG, MDIC_POLL_TRIES)
        {
            let cleared = reg & !(BM_WUC_ENABLE_BIT | BM_WUC_ME_WU_BIT | BM_WUC_HOST_WU_BIT);
            if cleared != reg {
                let _ = self.mdic_write_swheld(
                    BM_PHY_MDIO_ADDR,
                    BM_WUC_ENABLE_REG,
                    cleared,
                    MDIC_POLL_TRIES,
                );
                e1000e_vlog!(
                    "[e1000e] BM_WUC_ENABLE cleared ({:#x} -> {:#x})\n",
                    reg,
                    cleared
                );
            }
        }
        self.pch_swflag_release();
    }

    /// Linux `e1000_copy_rx_addrs_to_phy_ich8lan` + `e1000_init_phy_wakeup` BM path.
    /// Linux does not set PHY_CTRL GBE_DISABLE here — doing so drops a 1G link.
    unsafe fn pch_sync_phy_rx_path(&self, link_phy: u8) {
        if !self.is_pch_lpt_or_later() {
            return;
        }
        self.ensure_mac_rar0_valid();
        let Some(saved) = self.pch_bm_wuc_access_begin() else {
            crate::klog_warn!(
                "[e1000e] BM PHY filter sync skipped (no MDIO) — enabling UPE/MPE/BAM on MAC\n"
            );
            let mut rctl = mmio_read(self.base, E1000E_RCTL);
            rctl |= RCTL_UPE | RCTL_MPE | RCTL_BAM;
            mmio_write(self.base, E1000E_RCTL, rctl);
            let _ = mmio_read(self.base, E1000E_RCTL);
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
        // Match MAC promisc filters used during bringup (BM only active while WUC enable set).
        bm_rctl |= BM_RCTL_BAM | BM_RCTL_UPE | BM_RCTL_MPE;
        let ok_rctl = self.pch_bm_wuc_reg_write(0, bm_rctl);

        let mac_ral = mmio_read(self.base, E1000E_RAL0);
        let mac_rah = mmio_read(self.base, E1000E_RAL0 + 1);
        let bm_ral_lo = self.pch_bm_wuc_reg_read(16).unwrap_or(0);
        let bm_ral_hi = self.pch_bm_wuc_reg_read(17).unwrap_or(0);
        let bm_rah_lo = self.pch_bm_wuc_reg_read(18).unwrap_or(0);
        let bm_rah_av = self.pch_bm_wuc_reg_read(19).unwrap_or(0);
        let bm_rar_ok = ok_rar
            && bm_ral_lo == (mac_ral as u16)
            && bm_ral_hi == ((mac_ral >> 16) as u16)
            && bm_rah_lo == (mac_rah as u16)
            && bm_rah_av & 0x8000 != 0;

        self.pch_bm_wuc_access_end(saved);

        if ok_rar && ok_mta && ok_rctl && bm_rar_ok {
            crate::klog_warn!(
                "[e1000e] BM page-800 sync OK BM_RCTL={:#x} BAM={} RAR0={:04x}{:04x}/{:04x}{:04x} AV={} (tag={})\n",
                bm_rctl,
                bm_rctl & BM_RCTL_BAM != 0,
                mac_ral as u16,
                (mac_ral >> 16) as u16,
                bm_ral_lo,
                bm_ral_hi,
                bm_rah_av & 0x8000 != 0,
                E1000E_DRIVER_TAG
            );
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
                "[e1000e] BM page-800 verify fail (rar={} mta={} rctl={} bm_rar={}) — UPE/MPE/BAM on MAC (tag={})\n",
                ok_rar,
                ok_mta,
                ok_rctl,
                bm_rar_ok,
                E1000E_DRIVER_TAG
            );
            e1000e_wlog!(
                "[e1000e] BM page-800 partial sync (rar={} mta={} rctl={} bm_rar={}) — UPE/MPE/BAM on MAC\n",
                ok_rar,
                ok_mta,
                ok_rctl,
                bm_rar_ok
            );
            let mut rctl = mmio_read(self.base, E1000E_RCTL);
            rctl |= RCTL_UPE | RCTL_MPE | RCTL_BAM;
            mmio_write(self.base, E1000E_RCTL, rctl);
            let _ = mmio_read(self.base, E1000E_RCTL);
        }
    }

    unsafe fn log_link_mib_snapshot(&mut self, st2: u16) {
        self.refresh_hw_stats_roc();
        crate::klog_warn!(
            "[e1000e] link MIB: reg26={:#x} ({}) GPRC+{} MPC+{} (tag={})\n",
            st2,
            Self::hv_m_status_label(st2),
            self.hw_roc_gprc_last,
            self.hw_roc_mpc_last,
            E1000E_DRIVER_TAG
        );
    }

    unsafe fn verify_rx_engine(&self) {
        let rctl = mmio_read(self.base, E1000E_RCTL);
        let rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        let rdh = mmio_read(self.base, E1000E_RDH);
        let rdt = mmio_read(self.base, E1000E_RDT);
        if rctl & RCTL_EN == 0 {
            e1000e_wlog!("[e1000e] RX engine: RCTL.EN clear (RCTL={:#x})\n", rctl);
        }
        if rctl & RCTL_BAM == 0 {
            e1000e_wlog!("[e1000e] RX engine: RCTL.BAM clear — no broadcast/DHCP\n");
        }
        if rdh == rdt && rctl & RCTL_EN != 0 {
            e1000e_wlog!(
                "[e1000e] RX engine: RDH==RDT={} with RCTL.EN — DMA paused\n",
                rdh
            );
        }
        if self.is_pch_spt_or_later() && rxdctl & RXDCTL_QUEUE_ENABLE == 0 {
            e1000e_wlog!("[e1000e] RX engine: RXDCTL.QUEUE_ENABLE clear ({:#x})\n", rxdctl);
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
        let rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        let status_spd = Self::speed_mbps_from_status(status);
        e1000e_vlog!(
            "e1000e: {} CTRL={:#x} mac_spd={} STATUS_spd={} FRC={} STATUS={:#x} RCTL={:#x} RXDCTL={:#x} GPRC_acc={} MPC_acc={} RDH={} RDT={}\n",
            tag,
            ctrl,
            mac_spd,
            status_spd,
            if frc { "BAD" } else { "ok" },
            status,
            mmio_read(self.base, E1000E_RCTL),
            rxdctl,
            self.hw_roc_gprc_acc,
            self.hw_roc_mpc_acc,
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
            e1000e_wlog!("[e1000e] link-stall PHY reg write failed\n");
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
        e1000e_wlog!("[e1000e] GIO master enable bit still clear in STATUS\n");
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
        if !self.dma_path_ready() {
            return false;
        }
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
            let _ = mmio_read(self.base, E1000E_IOSFPC);
            Self::udelay(10);
        } else {
            mmio_write(self.base, E1000E_TXDCTL, txdctl);
        }

        // Linux netdev: clear TARC speed-mode at 10/100M before TCTL_EN (I219 TX errata).
        let phy = self.active_phy_addr();
        let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        self.program_tarc_with_tctl_gate(Self::speed_mbps_from_phy_st2(st2));
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
        e1000e_vlog!(
            "[e1000e] {} GPRC_acc={} MPC_acc={} sw_rx={} RCTL={:#x} RXDCTL={:#x}\n",
            tag,
            self.hw_roc_gprc_acc,
            self.hw_roc_mpc_acc,
            self.stats.rx_packets,
            mmio_read(self.base, E1000E_RCTL),
            mmio_read(self.base, E1000E_RXDCTL)
        );
    }

    /// Wait for settled reg26, lock MAC CTRL and PHY TIPG/EMI to that speed.
    /// I219 often reports 10M then ramps to 100/1000 within ms — arming RX with stale
    /// 10M tuning while CTRL was resynced to 100M leaves GPRC at zero.
    unsafe fn pch_commit_operational_link_speed(&mut self) -> u16 {
        let phy = self.active_phy_addr();
        let mut wait_ms = E1000E_REG26_COMMIT_MS;
        if self.link_speed == SPEED_10 && !self.link_10m_degraded {
            wait_ms = wait_ms.saturating_add(1000);
        }
        let mut st2 = if self.phy_mdio_responding {
            self.phy_wait_reg26_settled(phy, wait_ms)
        } else {
            self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0)
        };
        if self.phy_mdio_responding && !self.link_10m_degraded {
            st2 = self.phy_wait_reg26_if_transient_10m(phy, st2, 2000);
        }
        if self.phy_mdio_responding && st2 & PHY_STATUS2_AUTONEG_DONE == 0 {
            let st2_retry = self.phy_wait_reg26_settled(phy, 500);
            if st2_retry & PHY_STATUS2_AUTONEG_DONE != 0 {
                st2 = st2_retry;
                e1000e_vlog!("[e1000e] reg26 ANEG done after extra wait ({:#x})\n", st2);
            }
        }
        let (speed, duplex_full, src) = self.resolve_link_speed_duplex_linux(phy, st2);
        if self.link_speed != speed || self.link_duplex != duplex_full {
            crate::klog_warn!(
                "[e1000e] link reconcile: {}→{} Mb/s {} reg26={:#x} ({}) tag={}\n",
                self.link_speed,
                speed,
                if duplex_full { "FD" } else { "HD" },
                st2,
                src,
                E1000E_DRIVER_TAG
            );
        }
        self.link_speed = speed;
        self.link_duplex = duplex_full;
        if !self.mdio_unavailable() {
            self.mac_apply_ctrl_for_operational_link(st2, speed);
            self.program_link_tipg_emi_linux(phy, speed, duplex_full);
            self.pch_kmrn_half_duplex_preamble(phy, duplex_full);
            self.phy_setup_82577_copper(phy);
            if self.is_pch_spt_or_later() {
                self.pch_spt_ptr_gap_workaround(phy, speed);
            }
            self.configure_link_tx_path(speed);
            self.config_collision_dist_linux();
        }
        st2
    }

    /// Arm RX rings and enable RCTL — call only when link is up and configured.
    unsafe fn enable_rx_after_link(&mut self) {
        self.enable_rx_after_link_setup(true);
    }

    /// Re-post RX ring only (no PHY/BM reprogram) — used by GPRC stall recovery.
    unsafe fn enable_rx_rearm_only(&mut self) {
        self.enable_rx_after_link_setup(false);
    }

    unsafe fn enable_rx_after_link_setup(&mut self, full_link_setup: bool) {
        if full_link_setup && self.is_pch_lpt_or_later() {
            self.pch_clear_bm_port_gen_host_wu();
            self.pch_ensure_bm_wuc_disabled();
            let _st2 = self.pch_commit_operational_link_speed();
            self.pch_post_link_phy_tune();
            self.restart_tx_datapath_linux();

            let tctl = mmio_read(self.base, E1000E_TCTL);
            if tctl & TCTL_EN == 0 {
                crate::klog_warn!(
                    "[e1000e] enable_rx: TCTL.EN still clear after restart_tx — forcing\n"
                );
                let tctl_fix =
                    tctl | TCTL_EN | TCTL_PSP | TCTL_RTLC | TCTL_CT_LINUX | TCTL_COLD_LINUX;
                mmio_write(self.base, E1000E_TCTL, tctl_fix);
                let _ = mmio_read(self.base, E1000E_TCTL);
                Self::udelay(150);
            }

            let phy = self.active_phy_addr();
            self.pch_sync_phy_rx_path(phy);
        } else if self.is_pch_lpt_or_later() {
            self.pch_ensure_bm_wuc_disabled();
        }

        self.ensure_gio_master();

        let status = mmio_read(self.base, E1000E_STATUS);
        let status_spd = Self::speed_mbps_from_status(status);
        if self.is_pch_lpt_or_later() {
            let phy = self.active_phy_addr();
            let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
            e1000e_vlog!(
                "[e1000e] RX enable: STATUS={} Mb/s PHY reg26={:#x} ({}) CTRL={:#x} RCTL={:#x} RXDCTL={:#x}\n",
                status_spd,
                st2,
                Self::hv_m_status_label(st2),
                mmio_read(self.base, E1000E_CTRL),
                mmio_read(self.base, E1000E_RCTL),
                mmio_read(self.base, E1000E_RXDCTL)
            );
        } else {
            e1000e_vlog!(
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
        if self.is_pch_lpt_or_later() {
            self.flush_rx_ring_toggle();
            mmio_write(self.base, E1000E_FCRTL, 0);
            mmio_write(self.base, E1000E_FCRTH, 0);
        }

        let arm_ok = self.arm_rx_unit_linux();

        if self.is_pch_lpt_or_later() {
            let rctl_post = mmio_read(self.base, E1000E_RCTL);
            if rctl_post & (RCTL_UPE | RCTL_MPE | RCTL_BAM) != (RCTL_UPE | RCTL_MPE | RCTL_BAM) {
                mmio_write(
                    self.base,
                    E1000E_RCTL,
                    rctl_post | RCTL_UPE | RCTL_MPE | RCTL_BAM,
                );
                let _ = mmio_read(self.base, E1000E_RCTL);
            }
        }

        self.hw_roc_gprc_last = 0;
        self.hw_roc_mpc_last = 0;
        let rctl = mmio_read(self.base, E1000E_RCTL);
        let rxdctl = mmio_read(self.base, E1000E_RXDCTL);
        let rdh = mmio_read(self.base, E1000E_RDH);
        let rdt = mmio_read(self.base, E1000E_RDT);
        let ctrl = mmio_read(self.base, E1000E_CTRL);
        let armed = arm_ok && (rctl & RCTL_EN != 0) && rdh != rdt;
        self.rx_link_armed = armed;
        if armed {
            crate::klog_warn!(
                "[e1000e] RX armed: {} Mb/s RCTL={:#x} RXDCTL={:#x} RDH={} RDT={} reg26={:#x} CTRL={:#x} tag={}\n",
                self.link_speed,
                rctl,
                rxdctl,
                rdh,
                rdt,
                if self.is_pch_lpt_or_later() {
                    self.mdic_read(self.active_phy_addr(), MII_PHY_STATUS_2).unwrap_or(0)
                } else {
                    0
                },
                ctrl,
                E1000E_DRIVER_TAG
            );
            self.log_rx_path_regs("RX armed");
            self.log_post_link_counters("post-RX-arm");
        } else {
            crate::klog_warn!(
                "[e1000e] RX arm FAILED: RCTL={:#x} RXDCTL={:#x} RDH={} RDT={} BAM={} uc={} CTRL={:#x} tag={}\n",
                rctl,
                rxdctl,
                rdh,
                rdt,
                rctl & RCTL_BAM != 0,
                self.dma_uncached,
                ctrl,
                E1000E_DRIVER_TAG
            );
        }
    }



    unsafe fn resolve_link_speed_duplex(&self) -> (u32, bool) {
        let phy = self.active_phy_addr();
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

    /// Bring MAC/RX up from STATUS when MDIO is silent (I219 ULP/SWFLAG handoff).
    unsafe fn apply_link_from_status(&mut self, status: u32) -> bool {
        let is_pch = self.is_pch_lpt_or_later();
        let phy = self.active_phy_addr();
        let st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);
        if is_pch && !self.mdio_unavailable() && !Self::phy_reg26_speed_resolved(st2) {
            let _ = self.mdic_read(phy, MII_BMSR);
            let bmsr = self.mdic_read(phy, MII_BMSR).unwrap_or(0);
            if bmsr & BMSR_ANEG_COMPLETE == 0 {
                e1000e_vlog!(
                    "[e1000e] link deferred: STATUS={:#x} reg26={:#x} BMSR={:#x} (autoneg pending)\n",
                    status,
                    st2,
                    bmsr
                );
                self.get_link_status = true;
                return false;
            }
        }
        let use_status_sane = self.mdio_unavailable();
        let (speed, duplex_full, src) = if use_status_sane {
            let Some((spd, dpx)) = Self::speed_duplex_from_status_sane(status) else {
                return false;
            };
            let src = if self.mdio_unavailable() {
                "STATUS-only"
            } else {
                "STATUS-fallback"
            };
            (spd, dpx, src)
        } else {
            let (spd, dpx, s) = self.resolve_link_speed_duplex_linux(phy, st2);
            (spd, dpx, s)
        };

        if is_pch && self.pch_defer_link_until_gig_ready(phy, speed, st2) {
            return false;
        }

        if !self.link_up || self.link_speed != speed || self.link_duplex != duplex_full {
            let _was_down = !self.link_up;
            crate::klog_warn!(
                "[e1000e] link up: {} Mb/s {} duplex ({}{})\n",
                speed,
                if duplex_full { "full" } else { "half" },
                src,
                if self.mdio_degraded { ", STATUS/MDIO degraded" } else { "" }
            );

            self.link_up = true;
            self.link_speed = speed;
            self.link_duplex = duplex_full;

            if is_pch {
                self.mac_apply_link_up_autoneg();
                if !self.mdio_unavailable() {
                    self.mac_apply_ctrl_for_operational_link(st2, speed);
                }
            } else {
                self.mac_sync_ctrl_speed_mbps(speed, duplex_full);
            }

            if is_pch {
                self.program_link_tipg_emi_linux(phy, speed, duplex_full);
                self.pch_kmrn_half_duplex_preamble(phy, duplex_full);
                if !self.mdio_unavailable() {
                    self.phy_setup_82577_copper(phy);
                    if self.is_pch_spt_or_later() {
                        self.pch_spt_ptr_gap_workaround(phy, speed);
                    }
                }
                self.configure_link_tx_path(speed);
                self.config_collision_dist_linux();
            } else {
                self.config_collision_dist_linux();
            }

            self.configure_flow_control_after_link(speed, duplex_full);

            // Always re-arm RX when link parameters change (STATUS flap 1000→10 broke RX before).
            self.enable_rx_after_link();
        }

        true
    }

    /// Versión optimizada basada en el watchdog in-tree de Linux:
    /// Resuelve de forma segura el enlace físico y evita bucles infinitos de AN.
    ///
    /// `reg26_wait_ms` caps PHY reg26 polling — never block poll/epoll for seconds.
    unsafe fn check_for_link_linux(&mut self, reg26_wait_ms: u32) -> bool {
        if !self.get_link_status {
            return self.link_up;
        }
        self.get_link_status = false;

        let is_pch = self.is_pch_lpt_or_later();
        let phy = self.active_phy_addr();
        if phy != self.phy_addr {
            e1000e_vlog!(
                "[e1000e] runtime PHY switch {} -> {}\n",
                self.phy_addr,
                phy
            );
            self.phy_addr = phy;
        }

        let status = mmio_read(self.base, E1000E_STATUS);
        if is_pch && status & STATUS_LU != 0 && self.mdio_unavailable() {
            return self.apply_link_from_status(status);
        }

        // 1. Leer BMSR dos veces para limpiar el bit pegajoso de "Latch-Low".
        let _ = self.mdic_read(phy, MII_BMSR);
        let bmsr = self.mdic_read(phy, MII_BMSR).unwrap_or(0);

        // MDIO silent but MAC reports carrier — common on I219 when SWFLAG/ULP blocks MDIO.
        if bmsr == 0 || bmsr == 0xFFFF {
            if is_pch && status & STATUS_LU != 0 {
                self.mdio_degraded = true;
                return self.apply_link_from_status(status);
            }
            if self.link_up {
                crate::klog_warn!("[e1000e] link down (PHY read failed)\n");
                self.link_up = false;
                self.link_speed = 0;
                self.link_duplex = false;
                self.rx_link_armed = false;
            }
            return false;
        }

        self.mdio_degraded = false;

        let link_up = (bmsr & 0x0004) != 0;

        if !link_up {
            if is_pch && self.link_up && status & STATUS_LU != 0 {
                e1000e_vlog!(
                    "[e1000e] BMSR link clear but STATUS.LU set — keep link (tag={})\n",
                    E1000E_DRIVER_TAG
                );
                self.get_link_status = true;
                return true;
            }
            if self.link_up {
                crate::klog_warn!("[e1000e] link down\n");
                self.link_up = false;
                self.link_speed = 0;
                self.link_duplex = false;
                self.rx_link_armed = false;
            }
            return false;
        }

        let mut st2 = self.mdic_read(phy, MII_PHY_STATUS_2).unwrap_or(0);

        if is_pch && !self.phy_copper_ready_for_link_up(bmsr, status, st2) {
            self.get_link_status = true;
            return false;
        }

        // 2. Prefer reg26 speed; wait briefly, then STATUS.LU fallback (sanitized).
        let mut reg26_ready = st2 & PHY_STATUS2_AUTONEG_DONE != 0
            || Self::phy_reg26_speed_resolved(st2);
        if is_pch && self.phy_mdio_responding && !reg26_ready && reg26_wait_ms > 0 {
            st2 = self.phy_wait_reg26_settled(phy, reg26_wait_ms);
            reg26_ready = st2 & PHY_STATUS2_AUTONEG_DONE != 0
                || Self::phy_reg26_speed_resolved(st2);
        }
        if is_pch && self.phy_mdio_responding && !reg26_ready {
            e1000e_vlog!(
                "[e1000e] link deferred: BMSR={:#x} STATUS={:#x} reg26={:#x} ({})\n",
                bmsr,
                status,
                st2,
                Self::hv_m_status_label(st2)
            );
            self.get_link_status = true;
            return false;
        }
        let (mut speed, mut duplex_full, mut src) =
            self.resolve_link_speed_duplex_linux(phy, st2);

        // 3. If PHY/STATUS stuck at 10M but MII HCD is higher, wait for reg26 to settle.
        if reg26_wait_ms > 0 && speed == SPEED_10 && !self.link_10m_degraded {
            if let Some(hcd_speed) = self.phy_reg26_below_mii_hcd(phy, st2) {
                if hcd_speed > SPEED_10 {
                    st2 = self.phy_wait_reg26_settled(phy, reg26_wait_ms);
                    (speed, duplex_full, src) =
                        self.resolve_link_speed_duplex_linux(phy, st2);
                    if speed == SPEED_10 && hcd_speed > SPEED_10 {
                        self.phy_accept_10m_degraded_mode(phy);
                        return false;
                    }
                }
            }
        }

        // 4. PCH: one reg26 poll pass — never block poll/deferred jobs for seconds.
        if is_pch && self.phy_mdio_responding && speed < SPEED_1000 && reg26_wait_ms > 0 {
            st2 = self.phy_wait_reg26_settled(phy, reg26_wait_ms);
            (speed, duplex_full, src) = self.resolve_link_speed_duplex_linux(phy, st2);
            if !self.link_up && speed < SPEED_1000 && bmsr & BMSR_ANEG_COMPLETE == 0 {
                self.get_link_status = true;
                return false;
            }
        }

        if is_pch && self.pch_defer_link_until_gig_ready(phy, speed, st2) {
            return false;
        }

        // Link already up at stale 10/100M while MII HCD allows faster — wait and upgrade.
        if self.link_up
            && is_pch
            && !self.link_10m_degraded
            && self.link_speed < SPEED_1000
            && speed <= self.link_speed
            && reg26_wait_ms > 0
        {
            let hcd = self.phy_mii_hcd_speed(phy);
            if hcd > self.link_speed {
                let st2_up = self.phy_wait_reg26_settled(phy, reg26_wait_ms);
                let (spd, dpx, up_src) = self.resolve_link_speed_duplex_linux(phy, st2_up);
                if spd > self.link_speed {
                    st2 = st2_up;
                    speed = spd;
                    duplex_full = dpx;
                    src = up_src;
                    e1000e_vlog!(
                        "[e1000e] link speed upgrade pending: {}→{} Mb/s reg26={:#x} ({}) HCD={} Mb/s\n",
                        self.link_speed,
                        speed,
                        st2,
                        Self::hv_m_status_label(st2),
                        hcd
                    );
                }
            }
        }

        if !self.link_up || self.link_speed != speed || self.link_duplex != duplex_full {
            if speed == SPEED_1000 && duplex_full {
                crate::klog_warn!(
                    "[e1000e] NIC Link is Up 1000 Mbps Full Duplex, Flow Control: None\n"
                );
            }
            crate::klog_warn!(
                "[e1000e] link up: {} Mb/s {} duplex ({})\n",
                speed,
                if duplex_full { "full" } else { "half" },
                src
            );

            self.link_up = true;
            self.link_speed = speed;
            self.link_duplex = duplex_full;

            // 5. MAC: autoneg path — ASDE, then lock CTRL to PHY when reg26 is settled.
            if is_pch {
                self.mac_apply_link_up_autoneg();
                if !self.mdio_unavailable() {
                    self.mac_apply_ctrl_for_operational_link(st2, speed);
                }
            } else {
                self.mac_sync_ctrl_speed_mbps(speed, duplex_full);
            }

            // 6. Aplicar parches específicos de silicio según la velocidad real negociada
            if is_pch {
                self.program_link_tipg_emi_linux(phy, speed, duplex_full);
                self.pch_kmrn_half_duplex_preamble(phy, duplex_full);
                
                self.configure_link_tx_path(speed);
                self.config_collision_dist_linux();

                if !self.mdio_unavailable() {
                    self.phy_setup_82577_copper(phy);

                    if self.is_pch_spt_or_later() {
                        self.pch_spt_ptr_gap_workaround(phy, speed);
                    }
                }
            } else {
                self.config_collision_dist_linux();
            }

            self.configure_flow_control_after_link(speed, duplex_full);

            self.enable_rx_after_link();
            self.log_link_mib_snapshot(st2);
        }

        true
    }

    /// Linux `e1000e_has_link` — MDIO only when `get_link_status` is set (LSC / open / watchdog kick).
    unsafe fn e1000e_has_link(&mut self) -> bool {
        if self.device_id == 0x10d3 {
            self.qemu_finish_link_if_up();
            return self.link_up;
        }
        if !self.get_link_status {
            return self.link_up;
        }
        if self.has_amt() && !self.phy_mdio_responding && self.swflag_in_backoff() {
            let status = mmio_read(self.base, E1000E_STATUS);
            self.get_link_status = false;
            if status & STATUS_LU != 0 {
                let _ = self.apply_link_from_status(status);
            }
            return self.link_up;
        }
        let _ = self.check_for_link_linux(E1000E_LINK_CHECK_REG26_MS);
        self.link_up
    }

    /// Linux `e1000_watchdog_task` — 2 Hz link check, no per-poll MDIO hammering.
    unsafe fn watchdog_tick(&mut self) {
        if self.phy_init_pending {
            return;
        }

        let now = timer_now_as_micros();
        self.link_watchdog_next_us = now.saturating_add(E1000E_WATCHDOG_PERIOD_US);

        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU == 0 {
            self.pch_note_status_link_down();
        } else if self.has_amt() && !self.nic_open_done && !self.rx_link_armed {
            let _ = self.try_rx_arm_pending_amt(status);
        }

        if self.link_up {
            if self.is_pch_lpt_or_later()
                && self.device_id != 0x10d3
                && self.phy_mdio_responding
                && self.link_speed < SPEED_1000
            {
                self.get_link_status = true;
                let _ = self.check_for_link_linux(E1000E_LINK_CHECK_REG26_MS);
            }
            if !self.rx_link_armed {
                unsafe { self.enable_rx_after_link() };
            } else {
                unsafe {
                    self.refresh_hw_stats_roc();
                }
                if self.hw_roc_gprc_acc == 0 && self.stats.rx_packets == 0 {
                    self.rx_stall_watchdogs = self.rx_stall_watchdogs.saturating_add(1);
                    if self.rx_stall_watchdogs >= 4 {
                        self.rx_stall_watchdogs = 0;
                        if self.link_speed <= SPEED_100 && !self.link_10m_degraded {
                            crate::klog_warn!(
                                "[e1000e] GPRC still 0 at {} Mb/s — reconcile link speed + re-arm RX (tag={})\n",
                                self.link_speed,
                                E1000E_DRIVER_TAG
                            );
                            self.get_link_status = true;
                            let prev_speed = self.link_speed;
                            let _ = self.check_for_link_linux(E1000E_LINK_CHECK_REG26_MS);
                            if self.link_speed == prev_speed {
                                unsafe { self.enable_rx_after_link() };
                            }
                        } else {
                            crate::klog_warn!(
                                "[e1000e] GPRC still 0 after link — re-arm RX ring (tag={})\n",
                                E1000E_DRIVER_TAG
                            );
                            unsafe { self.enable_rx_rearm_only() };
                        }
                    }
                } else {
                    self.rx_stall_watchdogs = 0;
                }
            }
            return;
        }

        if self.is_pch_lpt_or_later() {
            self.pch_try_early_link();
        }

        self.get_link_status = true;
        let _ = self.e1000e_has_link();

        if self.link_up && !self.rx_link_armed {
            self.enable_rx_after_link();
        }

        // AMT/I219: PHY recovery without successful open just spams SWFLAG; open path retries.
        if !self.link_up
            && self.is_pch_lpt_or_later()
            && !self.phy_mdio_responding
            && !self.has_amt()
            && now.wrapping_sub(self.last_phy_recovery_us) >= E1000E_PHY_RECOVERY_INTERVAL_US
        {
            self.last_phy_recovery_us = now;
            e1000e_wlog!("[e1000e] watchdog: PHY MDIO recovery\n");
            self.pch_recover_phy_mdio();
            self.detect_phy_addr();
            self.pch_disable_lplu_gbe();
            self.mac_setup_copper_link_linux();
            self.pch_kick_autoneg_mdio();
            self.get_link_status = true;
        }
    }

    fn maybe_log_rx_diag(&mut self) {
        // ROC counters: single read site — never touch GPRC/MPC elsewhere for logging.
        unsafe {
            self.refresh_hw_stats_roc();
        }
        if self.hw_roc_mpc_last > 0 {
            e1000e_wlog!(
                "[e1000e] MPC +{} GPRC_acc={} RDH={} RDT={} clean={} post={} uc={}\n",
                self.hw_roc_mpc_last,
                self.hw_roc_gprc_acc,
                unsafe { mmio_read(self.base, E1000E_RDH) },
                unsafe { mmio_read(self.base, E1000E_RDT) },
                self.rx_next_to_clean,
                self.rx_post_since_doorbell,
                self.dma_uncached
            );
        }

        if !E1000E_LOG_VERBOSE {
            return;
        }
        self.rx_diag_counter = self.rx_diag_counter.wrapping_add(1);
        if self.rx_diag_counter & 0x3F != 0 {
            return;
        }
        if self.hw_roc_gprc_last > 0 {
            log::debug!(
                "[e1000e] GPRC +{} (acc {}) RDH={} RDT={} clean={}",
                self.hw_roc_gprc_last,
                self.hw_roc_gprc_acc,
                unsafe { mmio_read(self.base, E1000E_RDH) },
                unsafe { mmio_read(self.base, E1000E_RDT) },
                self.rx_next_to_clean
            );
        }
        if self.rx_needs_cache_invalidation() && self.stats.rx_packets == 0 {
            let i = self.rx_next_to_clean;
            let ring = self.rx_ring.as_ptr::<RxDesc>();
            let desc_ptr = unsafe { ring.add(i) };
            let wb = unsafe { read_volatile(core::ptr::addr_of!((*desc_ptr).reserved)) as u32 };
            if wb != 0 {
                log::trace!(
                    "[e1000e] RX slot {} WB={:#x} but not consumed (clean={} uc={})",
                    i,
                    wb,
                    self.rx_next_to_clean,
                    self.dma_uncached
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
        self.read_mac_from_hw();
        let mut mac_found = self.is_valid_mac();

        self.phy_init_pending = false;
        self.quiesce_dma_before_reset();

        // Linux: init_phy_workarounds_pchlan → reset_hw_ich8lan → init_hw_ich8lan (before rings).
        if self.is_pch_lpt_or_later() && !self.has_amt() {
            e1000e_vlog!("[e1000e] PCH probe: workarounds + reset_hw + init_hw\n");
            self.e1000e_get_hw_control();
            self.pch_clear_status_phyra_if_set();
            self.pch_init_phy_workarounds();
            self.reset_hw_ich8lan_linux(true);
            self.restore_pci_command_bus_master();
            self.init_hw_ich8lan_linux();
            self.pch_mdio_prepare_after_power();
            self.detect_phy_addr();
            self.get_link_status = true;
            self.pch_kick_autoneg_mdio();
            let _ = self.check_for_link_linux(E1000E_LINK_CHECK_REG26_MS);
        } else if self.is_pch_lpt_or_later() {
            e1000e_vlog!(
                "[e1000e] PCH probe: minimal (AMT — full init in chunked open, tag={})\n",
                E1000E_DRIVER_TAG
            );
            self.pch_clear_status_phyra_if_set();
            self.amt_open_reset_state();
            self.pch_mdio_prepare_after_power();
            self.detect_phy_addr();
            if self.phy_mdio_responding {
                self.pch_kick_autoneg_mdio();
                self.get_link_status = true;
            }
        } else {
            self.reset_hw_ich8lan_linux(false);
            self.restore_pci_command_bus_master();
        }

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
            crate::klog_warn!(
                "[e1000e] MMIO not responding after {} ms\n",
                STATUS_POLL_US / 1000
            );
            return Err(DeviceError::IoError);
        }

        self.pch_clear_status_phyra_if_set();

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
            crate::klog_warn!(
                "[e1000e] using fallback MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
                self.mac[0],
                self.mac[1],
                self.mac[2],
                self.mac[3],
                self.mac[4],
                self.mac[5]
            );
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
        ctrl_ext |= CTRL_EXT_DRV_LOAD;
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
        }
        self.flush_rx_ring_descriptor_span(0, NUM_RX);

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
        
        if !self.has_amt() {
            self.e1000e_get_hw_control();
        }

        // 11. Configure RX — register order aligned with Linux e1000_configure_rx:
        // RDTR/RADV/ITR → RDBAL/RDBAH/RDLEN/RDH/RDT → RXCSUM/RFCTL/… (no IAME on bare-metal)
        mmio_write(self.base, E1000E_RDTR, 0);
        mmio_write(self.base, E1000E_RADV, 0);
        mmio_write(self.base, E1000E_ITR, 0);
        self.disable_iame_automask();

        let rx_ring_pa = self.rx_ring.paddr();
        mmio_write(self.base, E1000E_RDBAL, rx_ring_pa as u32);
        mmio_write(self.base, E1000E_RDBAH, (rx_ring_pa >> 32) as u32);
        mmio_write(self.base, E1000E_RDLEN, (NUM_RX * size_of::<RxDesc>()) as u32);
        self.rx_next_to_clean = 0;
        self.rx_next_to_use = 0;
        mmio_write(self.base, E1000E_RDH, 0);
        let _ = mmio_read(self.base, E1000E_RDH);
        // RDT is not written here; arm_rx_unit_linux doorbells RDT before RCTL.EN.

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
            e1000e_vlog!(
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

        self.irq_mask_then_clear_icr();
        mmio_write(self.base, E1000E_IMS, IMS_REARM_LINUX);

        // 13. Link — Linux: `hw->mac.get_link_status = true` at open; watchdog @ 2 Hz.
        self.get_link_status = true;
        self.link_up = false;
        self.link_speed = 0;
        self.link_duplex = false;
        self.rx_poll_budget = 32;
        self.rx_link_armed = false;
        let now = timer_now_as_micros();
        self.link_watchdog_next_us = now;
        unsafe {
            self.qemu_finish_link_if_up();
            if self.is_pch_lpt_or_later() && !self.has_amt() {
                self.pch_try_early_link();
                if self.link_up && !self.rx_link_armed {
                    self.enable_rx_after_link();
                }
            }
        }
        Ok(())
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
        e1000e_wlog!(
            "[e1000e] QEMU fast link: {} Mb/s {} duplex RX armed\n",
            self.link_speed,
            if self.link_duplex { "full" } else { "half" }
        );
    }

    /// PCH/I219: if cable is already up at probe time, finish link/RX/TX without waiting for bringup ticks.
    unsafe fn pch_try_early_link(&mut self) {
        if self.device_id == 0x10d3 || !self.is_pch_lpt_or_later() {
            return;
        }
        let status = mmio_read(self.base, E1000E_STATUS);
        if status & STATUS_LU == 0 {
            return;
        }
        self.get_link_status = true;
        let _ = self.check_for_link_linux(E1000E_LINK_CHECK_REG26_MS);
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
        let mut i = 0usize;
        while i < NUM_RX {
            let chunk = RX_DESCS_PER_CACHE_LINE.min(NUM_RX - i);
            for j in 0..chunk {
                let idx = i + j;
                let desc_ptr = ring.add(idx);
                write_volatile(core::ptr::addr_of_mut!((*desc_ptr).addr), self.rx_buf_paddr(idx));
                Self::clear_rx_desc_wb(desc_ptr);
            }
            self.flush_rx_ring_descriptor_span(i, chunk);
            i += chunk;
        }
    }



    /// Sole reader for GPRC/MPC (read-on-clear). Accumulates into software counters.
    unsafe fn refresh_hw_stats_roc(&mut self) {
        let gprc_delta = mmio_read(self.base, E1000E_GPRC);
        let mpc_delta = mmio_read(self.base, E1000E_MPC);
        self.hw_roc_gprc_last = gprc_delta;
        self.hw_roc_mpc_last = mpc_delta;
        self.hw_roc_gprc_acc = self.hw_roc_gprc_acc.wrapping_add(gprc_delta as u64);
        self.hw_roc_mpc_acc = self.hw_roc_mpc_acc.wrapping_add(mpc_delta as u64);
    }

    /// Hardware MIB counters (Linux `e1000e_update_stats`). Clear-on-read.
    unsafe fn read_hw_stats(&mut self) -> NetStats {
        self.refresh_hw_stats_roc();
        let tx_packets = mmio_read(self.base, E1000E_GPTC) as u64;
        let rx_bytes =
            (mmio_read(self.base, E1000E_GORCL) as u64) | ((mmio_read(self.base, E1000E_GORCH) as u64) << 32);
        let tx_bytes =
            (mmio_read(self.base, E1000E_GOTCL) as u64) | ((mmio_read(self.base, E1000E_GOTCH) as u64) << 32);
        NetStats {
            rx_bytes,
            rx_packets: self.hw_roc_gprc_acc,
            tx_bytes,
            tx_packets,
            rx_errors: 0,
            rx_dropped: self.hw_roc_mpc_acc,
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
    /// CRITICAL: On WB-mapped DMA, never clflush a single 16 B descriptor in the hot path
    /// (4 descriptors share one 64 B line). Use [`Self::prepare_rx_desc_wb_read`] instead.
    unsafe fn desc_done(&mut self, desc_ptr: *const RxDesc) -> Option<(u32, usize)> {
        if desc_ptr.is_null() {
            return None;
        }
        let desc_idx = (desc_ptr as usize - self.rx_ring.vaddr()) / size_of::<RxDesc>();
        for attempt in 0..RX_DESC_WB_SETTLE_TRIES {
            if attempt > 0 {
                self.invalidate_rx_desc_wb_sync();
            }
            self.prepare_rx_desc_wb_read(desc_idx);
            let wb = Self::read_rx_wb_u64(desc_ptr);
            let parsed = if self.use_extended_descriptors {
                Self::parse_rx_wb_ext_u64(wb)
            } else {
                Self::parse_rx_wb_legacy_u64(wb)
            };
            match parsed {
                None => {
                    if attempt + 1 < RX_DESC_WB_SETTLE_TRIES {
                        Self::udelay(RX_DESC_WB_SETTLE_US as u64);
                        continue;
                    }
                    return None;
                }
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

    /// Return descriptor to hardware after the frame is copied (RDT doorbell batched at flush).
    unsafe fn recycle_rx_descriptor(&mut self, i: usize) {
        self.repost_rx_slot(i);
        self.rx_next_to_clean = (i + 1) % NUM_RX;
        self.rx_post_since_doorbell = self.rx_post_since_doorbell.saturating_add(1);
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
        let desc_ptr = unsafe { ring.add(i) };
        let (staterr, len) = match unsafe { self.desc_done(desc_ptr) } {
            Some(v) => v,
            None => {
                self.invalidate_rx_desc_wb_sync();
                if let Some(v) = unsafe { self.desc_done(desc_ptr) } {
                    v
                } else {
                let wb = unsafe {
                    self.prepare_rx_desc_wb_read(i);
                    Self::read_rx_wb_u64(desc_ptr)
                };
                let parsed = unsafe {
                    if self.use_extended_descriptors {
                        Self::parse_rx_wb_ext_u64(wb)
                    } else {
                        Self::parse_rx_wb_legacy_u64(wb)
                    }
                };
                match parsed {
                    Some((staterr, len)) if len > 0 && len <= BUF_SIZE => {
                        log::trace!(
                            "[e1000e] RX slot {} late WB settle staterr={:#x} len={}",
                            i,
                            staterr,
                            len
                        );
                        (staterr, len)
                    }
                    Some((staterr, len)) => {
                        log::debug!(
                            "[e1000e] RX slot {} forcing recycle after unstable WB staterr={:#x} len={} clean={} RDH={} RDT={}",
                            i,
                            staterr,
                            len,
                            self.rx_next_to_clean,
                            unsafe { mmio_read(self.base, E1000E_RDH) },
                            unsafe { mmio_read(self.base, E1000E_RDT) }
                        );
                        self.rx_sg_reset();
                        unsafe { self.recycle_rx_descriptor(i) };
                        return None;
                    }
                    None if wb != 0 => {
                        log::debug!(
                            "[e1000e] RX slot {} forcing recycle on non-zero WB={:#x} clean={} RDH={} RDT={}",
                            i,
                            wb,
                            self.rx_next_to_clean,
                            unsafe { mmio_read(self.base, E1000E_RDH) },
                            unsafe { mmio_read(self.base, E1000E_RDT) }
                        );
                        self.rx_sg_reset();
                        unsafe { self.recycle_rx_descriptor(i) };
                        return None;
                    }
                    None => return None,
                }
                }
            }
        };
        fence(Ordering::Acquire);
        log::trace!("[e1000e] RX: slot={} staterr={:#x} len={} clean={} RDH={} RDT={}",
              i, staterr, len, self.rx_next_to_clean,
              unsafe { mmio_read(self.base, E1000E_RDH) },
              unsafe { mmio_read(self.base, E1000E_RDT) });

        if len == 0 || len > BUF_SIZE {
            // Descriptor is done but length is implausible — discard and recycle.
            log::debug!(
                "[e1000e] RX slot {} bad len={} staterr={:#x} — discarding",
                i, len, staterr
            );
            self.rx_sg_reset();
            unsafe { self.recycle_rx_descriptor(i) };
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
                    unsafe { self.recycle_rx_descriptor(i) };
                    return None;
                }
                unsafe { self.recycle_rx_descriptor(i) };
                return None;
            } else if !self.rx_sg_buf.is_empty() {
                log::trace!(
                    "[e1000e] RX slot {} continuation {} B EOP=0 — buffer",
                    i,
                    frag.len()
                );
                if !self.rx_sg_append(&frag) {
                    unsafe { self.recycle_rx_descriptor(i) };
                    return None;
                }
                unsafe { self.recycle_rx_descriptor(i) };
                return None;
            } else {
                unsafe { self.recycle_rx_descriptor(i) };
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
                    unsafe { self.recycle_rx_descriptor(i) };
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
            unsafe { self.recycle_rx_descriptor(i) };
            return None;
        }

        unsafe { self.recycle_rx_descriptor(i) };

        self.stats.rx_packets += 1;
        self.stats.rx_bytes += data.len() as u64;
        if self.stats.rx_packets == 1 {
            e1000e_wlog!(
                "[e1000e] first RX frame {} bytes staterr={:#x} sw_rx={} RDT={} RDH={}",
                data.len(),
                staterr,
                self.stats.rx_packets,
                unsafe { mmio_read(self.base, E1000E_RDT) },
                unsafe { mmio_read(self.base, E1000E_RDH) }
            );
        }
        Some(data)
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        if !self.rx_link_armed {
            unsafe { self.ensure_rx_armed_if_link_up() };
        }
        if self.rx_poll_budget == 0 {
            return None;
        }
        if self.is_pch_lpt_or_later() {
            unsafe { self.kick_rx_writeback() };
        }

        let budget = self.rx_poll_budget as usize;
        let max_frags = budget.min(NUM_RX);
        let mut sg_slots = RX_SG_SLOTS_PER_CALL;

        for _ in 0..max_frags {
            let i = self.rx_next_to_clean;
            let prev_clean = i;

            if let Some(frame) = self.receive_slot(i) {
                self.rx_poll_budget = self.rx_poll_budget.saturating_sub(1);
                unsafe { self.flush_rx_post_queue() };
                return Some(frame);
            }

            if self.rx_next_to_clean != prev_clean {
                unsafe { self.rx_doorbell_recycle_if_needed(false) };
                continue;
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

        e1000e_wlog!("[e1000e] TX: {} ({} bytes)", info, data.len());

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
                        let _ = self.check_for_link_linux(0);
                    } else {
                        self.link_up = true;
                        self.link_speed = Self::speed_mbps_from_status(status);
                        self.link_duplex = status & STATUS_FD != 0;
                        if !self.rx_link_armed {
                            self.ensure_rx_armed_if_link_up();
                        }
                    }
                }
            }
            if !self.link_up {
                return Err(DeviceError::NotReady);
            }
        }
        if !self.dma_path_ready() {
            return Err(DeviceError::NotReady);
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

        // IMPORTANT (real silicon): make `cmd` the last field visible to DMA.
        // If the NIC fetches while cmd is visible but addr/len are not yet in RAM,
        // TX can wedge (master abort / silent hang).
        unsafe {
            write_volatile(&mut desc.addr, self.tx_buf_paddr(idx));
            write_volatile(&mut desc.len, data.len() as u16);
            write_volatile(&mut desc.cso, 0);
            write_volatile(&mut desc.status, 0);
            write_volatile(&mut desc.css, 0);
            write_volatile(&mut desc.special, 0);
        }
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);

        if self.rx_needs_cache_flush() {
            unsafe {
                // Push packet bytes first.
                Self::dma_wbinv_range(self.tx_buf_vaddr(idx), data.len());
                // Push descriptor fields (without cmd) out to RAM.
                Self::dma_wbinv_range(desc as *const TxDesc as usize, core::mem::size_of::<TxDesc>());
                core::arch::x86_64::_mm_sfence();
            }
        } else {
            compiler_fence(Ordering::SeqCst);
        }

        // Publish cmd last. RS only on sparse slots (queue head uses TDH; RS floods WB FIFO).
        let report_rs = first_tx
            || idx % TX_RS_REPORT_INTERVAL == 0
            || self.tx_desc_unused() <= TX_RS_REPORT_LOW_WATER;
        let cmd = TX_CMD_EOP | TX_CMD_IFCS | if report_rs { TX_CMD_RS } else { 0 };
        unsafe {
            write_volatile(&mut desc.cmd, cmd);
        }
        compiler_fence(Ordering::SeqCst);
        fence(Ordering::SeqCst);

        if self.rx_needs_cache_flush() {
            unsafe {
                Self::dma_wbinv_range(desc as *const TxDesc as usize, core::mem::size_of::<TxDesc>());
            }
        }
        // Unconditional: MMIO TDT must not pass stores to the descriptor in RAM/UC.
        Self::store_fence_before_device_mmio();

        self.tx_tail = (idx + 1) % NUM_TX;
        compiler_fence(Ordering::SeqCst);
        unsafe {
            mmio_write(self.base, E1000E_TDT, self.tx_tail as u32);
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
                e1000e_vlog!("[e1000e] first TX {} ({} bytes) DD ok\n", info, data.len());
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
    /// Avoid queueing multiple watchdog jobs at once (Linux `watchdog_task` workqueue).
    watchdog_job_scheduled: Arc<AtomicBool>,
    amt_open_job_scheduled: Arc<AtomicBool>,
    pub routes: Arc<Mutex<Vec<RouteInfo>>>,
    pub ip_addrs: Arc<Mutex<Vec<IpCidr>>>,
}

impl E1000eInterface {
    /// Linux `e1000_watchdog` → deferred work (not in interrupt / not every `poll()`).
    /// Linux `e1000e_open` for CSME/AMT (I219): DRV_LOAD + reset + link when stack starts using eth0.
    pub fn schedule_amt_open(&self) {
        {
            let hw = self.driver.hw.lock();
            if hw.nic_open_done || !hw.has_amt() {
                return;
            }
            let now = timer_now_as_micros();
            if now.wrapping_sub(hw.last_amt_open_attempt_us.get())
                < E1000E_AMT_OPEN_RETRY_US
            {
                return;
            }
        }
        if self
            .amt_open_job_scheduled
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            me.amt_open_job_scheduled
                .store(false, Ordering::Release);
            let more = {
                let mut hw = me.driver.hw.lock();
                unsafe { hw.e1000e_open_bringup_step() }
            };
            if more {
                me.schedule_amt_open();
            } else if me.driver.hw.lock().nic_open_done {
                me.schedule_watchdog(true);
            }
        });
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
        if self
            .watchdog_job_scheduled
            .swap(true, Ordering::AcqRel)
        {
            return;
        }
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            // RAII guard: clear flag on eviction (job dropped without running)
            // so future watchdog calls can reschedule.
            struct ClearOnDrop(Arc<AtomicBool>);
            impl Drop for ClearOnDrop {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _guard = ClearOnDrop(Arc::clone(&me.watchdog_job_scheduled));
            me.watchdog_job_scheduled.store(false, Ordering::Release);
            {
                let mut hw = me.driver.hw.lock();
                unsafe { hw.watchdog_tick() };
            }
        });
    }

    /// Queue the next chunk of PCH PHY/MAC init (background — never at boot 84%/87%).
    pub fn schedule_deferred_phy_init(&self) {
        if !self.driver.hw.lock().phy_init_pending {
            return;
        }
        let me = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            let more = {
                let mut hw = me.driver.hw.lock();
                if hw.phy_init_pending {
                    unsafe { hw.deferred_init_step() }
                } else {
                    false
                }
            };
            if more {
                me.schedule_deferred_phy_init();
            } else {
                me.schedule_watchdog(true);
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

    fn queue_deferred_poll(&self) {
        if self.poll_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let poll_pending = self.poll_pending.clone();
        let self_clone = self.clone();
        crate::utils::deferred_job::push_deferred_job(move || {
            // Clear *before* polling so that any IRQ firing during poll() can
            // immediately re-queue a new deferred poll instead of being silently
            // dropped.  The RAII guard clears again on drop so that eviction
            // (job dropped without running) also releases the flag.
            struct ClearOnDrop(Arc<AtomicBool>);
            impl Drop for ClearOnDrop {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let _guard = ClearOnDrop(Arc::clone(&poll_pending));
            poll_pending.store(false, Ordering::Release);
            let _ = self_clone.poll();
        });
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

        // Mask in software (no IAME): IMC before ICR read-to-clear, IMS at end.
        let icr = unsafe {
            mmio_write(self.base, E1000E_IMC, 0xFFFF_FFFF);
            let _ = mmio_read(self.base, E1000E_IMC);
            fence(Ordering::SeqCst);
            mmio_read(self.base, E1000E_ICR)
        };
        log::trace!("[e1000e] handle_irq: irq={}, icr={:#x}", irq, icr);

        if icr == 0 {
            if let Some(mut hw) = self.driver.hw.try_lock() {
                hw.get_link_status = true;
            }
            crate::pulse::pulse_signal(crate::pulse::PULSE_NET_RX);
            self.queue_deferred_poll();
            self.ims_rearm();
            return;
        }

        if icr & ICR_LSC != 0 {
            if let Some(mut hw) = self.driver.hw.try_lock() {
                hw.get_link_status = true;
            }
            crate::pulse::pulse_signal(crate::pulse::PULSE_LINK);
            self.schedule_watchdog(true);
        }

        let needs_poll = icr & (ICR_RX_ANY | ICR_TXDW | ICR_LSC) != 0;
        if needs_poll {
            if icr & ICR_RX_ANY != 0 {
                crate::pulse::pulse_signal(crate::pulse::PULSE_NET_RX);
            }
            self.queue_deferred_poll();
        }

        // Linux order: consume ICR, queue NAPI/poll work, then e1000_irq_enable (IMS).
        // Early IMS re-arm before deferred work can lose MSI events on non-reentrant APIC paths.
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
    
    fn refresh_link(&self) -> DeviceResult {
        let status = unsafe { mmio_read(self.base, E1000E_STATUS) };
        crate::klog_warn!(
            "[e1000e] admin refresh {} STATUS={:#x} LU={} tag={}\n",
            self.name,
            status,
            status & STATUS_LU != 0,
            E1000E_DRIVER_TAG
        );
        {
            let mut hw = self.driver.hw.lock();
            hw.get_link_status = true;
        }
        self.schedule_amt_open();
        self.schedule_watchdog(true);
        Ok(())
    }

    fn link_carrier_up(&self) -> bool {
        let hw = self.driver.hw.lock();
        hw.link_up || unsafe { mmio_read(self.base, E1000E_STATUS) & STATUS_LU != 0 }
    }

    fn poll(&self) -> DeviceResult {
        if self.driver.hw.lock().phy_init_pending {
            self.schedule_deferred_phy_init();
        } else {
            let now = timer_now_as_micros();
            let due = self
                .driver
                .hw
                .lock()
                .link_watchdog_next_us
                <= now;
            if due {
                self.schedule_watchdog(false);
            }
        }
        let ts = Instant::from_micros(timer_now_as_micros() as i64);
        let sockets = get_sockets();

        {
            let mut hw = self.driver.hw.lock();
            unsafe { hw.ensure_rx_armed_if_link_up() };
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
        // If the budget was exhausted (32 frames consumed without emptying the ring),
        // there may be more frames waiting.  Queue another deferred poll so the ring
        // drains without waiting for the next IRQ or periodic poll_ifaces() tick.
        if self.driver.hw.lock().rx_poll_budget == 0 {
            self.queue_deferred_poll();
        }
        // Poll path drains RX without IRQ — re-arm IMS so the next packet can interrupt.
        if self.driver.hw.lock().rx_poll_budget < 32 {
            self.ims_rearm();
        }
        super::wake_net_rx_waiters();
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
            e1000e_wlog!("[e1000e] send: hardware not ready");
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
    e1000e_vlog!(
        "[e1000e] probing {} at vaddr={:#x} irq={}",
        name, vaddr, irq
    );

    // Allocate DMA rings — map UC at alloc time (Linux dma_alloc_coherent).
    let (rx_ring, rx_uc) =
        DmaRegion::alloc_uninit_try_coherent(NUM_RX * size_of::<RxDesc>()).ok_or(DeviceError::DmaError)?;
    let (tx_ring, tx_uc) =
        DmaRegion::alloc_uninit_try_coherent(NUM_TX * size_of::<TxDesc>()).ok_or(DeviceError::DmaError)?;

    let (rx_buf_pool, rx_pool_uc) =
        DmaRegion::alloc_uninit_try_coherent(NUM_RX * BUF_SIZE).ok_or(DeviceError::DmaError)?;
    let (tx_buf_pool, tx_pool_uc) =
        DmaRegion::alloc_uninit_try_coherent(NUM_TX * BUF_SIZE).ok_or(DeviceError::DmaError)?;
    let probe_dma_coherent = rx_uc && tx_uc && rx_pool_uc && tx_pool_uc;

    for (name, region, align, max_span, ring_single_page) in [
        ("rx_ring", &rx_ring, DMA_DESC_ALIGN, DMA_RING_BYTES, true),
        ("tx_ring", &tx_ring, DMA_DESC_ALIGN, DMA_TX_RING_BYTES, true),
        ("rx_buf_pool", &rx_buf_pool, 64, NUM_RX * BUF_SIZE, false),
        ("tx_buf_pool", &tx_buf_pool, 64, NUM_TX * BUF_SIZE, false),
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
        if region.paddr() & (crate::bus::PAGE_SIZE - 1) != 0 {
            crate::klog_err!(
                "[e1000e] {} paddr={:#x} not page-aligned — NIC ring DMA will straddle pages\n",
                name,
                region.paddr()
            );
            return Err(DeviceError::DmaError);
        }
        if max_span > region.byte_len() {
            crate::klog_err!(
                "[e1000e] {} spans {} B but alloc is {} B (ring must not cross pages)\n",
                name,
                max_span,
                region.byte_len()
            );
            return Err(DeviceError::DmaError);
        }
        if ring_single_page && !dma_span_within_one_phys_page(region.paddr(), max_span) {
            crate::klog_err!(
                "[e1000e] {} ring crosses 4 KiB phys page (p={:#x} span={} B)\n",
                name,
                region.paddr(),
                max_span
            );
            return Err(DeviceError::DmaError);
        }
        if region.vaddr() & 0xFFF != region.paddr() & 0xFFF {
            e1000e_wlog!(
                "[e1000e] {} v/p page offset mismatch v={:#x} p={:#x}\n",
                name,
                region.vaddr(),
                region.paddr()
            );
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
        phy_addr: if pci.id.device_id >= 0x15b7 && pci.id.device_id <= 0x15be {
            2
        } else {
            1
        },
        stats: NetStats::default(),
        hw_roc_gprc_acc: 0,
        hw_roc_mpc_acc: 0,
        hw_roc_gprc_last: 0,
        hw_roc_mpc_last: 0,
        rx_diag_counter: 0,
        rx_stall_watchdogs: 0,
        rx_poll_budget: 32,
        phy_init_pending: false,
        deferred_init_step: 0,
        rx_link_armed: false,
        link_10m_degraded: false,
        get_link_status: true,
        link_up: false,
        link_speed: 0,
        link_duplex: false,
        link_watchdog_next_us: 0,
        last_phy_recovery_us: 0,
        stage_start_us: 0,
        use_extended_descriptors: true,
        rx_sg_buf: Vec::new(),
        rx_sg_frag_count: 0,
        rx_post_since_doorbell: 0,
        srrctl_absent: false,
        dma_uncached: false,
        rx_wb_sync_line: 0xFF,
        mdio_degraded: false,
        phy_mdio_responding: false,
        phy_detect_fail_logged: false,
        swflag_backoff_until_us: Cell::new(0),
        swflag_fail_logged: Cell::new(false),
        nic_open_done: false,
        last_amt_open_attempt_us: Cell::new(0),
        amt_open_active: Cell::new(false),
        amt_open_phase: AMT_OPEN_IDLE,
        amt_open_sw_chunks: 0,
    };

    if probe_dma_coherent {
        hw.dma_uncached = true;
        crate::klog_warn!(
            "[e1000e] DMA coherent at alloc (PAT UC, tag={})\n",
            E1000E_DRIVER_TAG
        );
        let _ = hw.validate_dma_ring_layout();
    } else {
        hw.setup_dma_uncached();
    }

    unsafe {
        hw.reset_and_init()?;
    }
    hw.warn_mac_diagnostic();

    let mac_bytes = hw.mac;
    let link_note = if unsafe { mmio_read(vaddr, E1000E_STATUS) & STATUS_LU != 0 } || hw.link_up {
        "up"
    } else if hw.has_amt() && !hw.nic_open_done {
        "pending"
    } else {
        "down"
    };
    crate::klog_warn!(
        "e1000e: {} {:#x}:{:#x} link={} tag={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
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
        watchdog_job_scheduled: Arc::new(AtomicBool::new(false)),
        amt_open_job_scheduled: Arc::new(AtomicBool::new(false)),
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
        iface_arc.schedule_amt_open();
        iface_arc.schedule_watchdog(true);
        if vector != 0 {
            crate::net::pci_note_pending_msi(vector, iface_arc.clone());
        }
        Ok(Device::Net(iface_arc))
    }
}
