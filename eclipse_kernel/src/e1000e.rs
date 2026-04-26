//! Intel e1000e Ethernet Driver
//!
//! Supports Intel GbE controllers of the e1000e family, including:
//! - 82540EM / 82545EM / 82574L (legacy desktop / QEMU emulation)
//! - 82577LM / 82567LM          (Ibex Peak / Lynx Point)
//! - I217-LM / I217-V           (Haswell, 4th gen Core)
//! - I218-LM / I218-V           (Broadwell, 5th gen Core)
//! - I219-LM / I219-V           (Skylake through Lunar Lake, 6th–24th gen Core)
//!
//! ## Features
//! - Full PCI device-ID table for all I219 silicon generations
//!   (gen 1 through gen 24, Ice Lake through Lunar Lake / Raptor Lake)
//! - Legacy 16-byte Tx/Rx descriptor rings (compatible with all variants)
//! - IEEE 802.3x symmetric flow control (PAUSE frames)
//! - Interrupt coalescing timers (ITR/RDTR/RADV/TIDV/TADV)
//! - Link speed and duplex detection via STATUS register
//! - PHY access via MDI Control (MDIC) interface
//! - Hardware statistics register read (GPRC/GPTC/GORC/GOTC/MPC, …)
//! - Software packet/byte/error counters accumulated across calls
//!
//! ## References
//! - Intel 82574L GbE Controller Datasheet (external to PCH variants)
//! - Intel I217/I218/I219 Ethernet Connection Datasheets
//! - Linux kernel `drivers/net/ethernet/intel/e1000e/` (GPL-2.0)
//! - Redox OS `drivers/e1000d/` (MIT)
//! - OSDev Wiki: Intel 8254x Family

use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use core::ptr::{read_volatile, write_volatile};

use crate::serial;
use crate::memory;

// ───────────────────────────────────────────────────────────────────────────
// PCI device IDs for the e1000e / I21x family (vendor = 0x8086 / Intel)
// ───────────────────────────────────────────────────────────────────────────
const INTEL_VENDOR_ID: u16 = 0x8086;

/// All PCI device IDs belonging to the I217/I218/I219 (e1000e PCH-based) family.
const E1000E_DEVICE_IDS: &[u16] = &[
    // Legacy e1000
    0x100E, // 82540EM (QEMU e1000)
    0x100F, // 82545EM
    0x1015, // 82540EM Qemu
    // PCIe e1000e
    0x10D3, // 82574L (QEMU e1000e)
    0x10EA, // 82577LM
    0x10F5, // 82567LM
    // I217
    0x153A, // I217-LM
    0x153B, // I217-V
    // I218
    0x155A, // I218-LM
    0x1559, // I218-V
    0x15A0, // I218-LM (2)
    0x15A1, // I218-V  (2)
    0x15A2, // I218-LM (3)
    0x15A3, // I218-V  (3)
    // I219  — PCH-integrated Ethernet, Skylake and later
    //         'gen N' below refers to Intel's I219 silicon revision
    //         (i.e. the Nth variant of I219), not the CPU generation.
    //         Multiple revisions may exist for the same CPU family.
    0x156F, // I219-LM rev 1  (Skylake / Sunrise Point SPT)
    0x1570, // I219-V  rev 1  (Skylake / Sunrise Point SPT)
    0x15B7, // I219-LM rev 2  (Kaby Lake / Skylake SPT2)
    0x15B8, // I219-V  rev 2  (Kaby Lake / Skylake SPT2)  ← exact card from issue
    0x15BB, // I219-LM rev 3  (Lewisburg / Skylake LBG server)
    0x15BC, // I219-V  rev 3  (Lewisburg / Skylake LBG server)
    0x15D7, // I219-LM rev 4  (Kaby Lake SPT3)
    0x15D8, // I219-V  rev 4  (Kaby Lake SPT3)
    0x15E3, // I219-LM rev 5  (Coffee Lake SPT4)
    0x15D6, // I219-V  rev 5  (Coffee Lake SPT4)
    0x15BD, // I219-LM rev 6  (Ice Lake ICP)
    0x15BE, // I219-V  rev 6  (Ice Lake ICP)
    0x15DF, // I219-LM rev 7  (Ice Lake ICP2)
    0x15E0, // I219-V  rev 7  (Ice Lake ICP2)
    0x15E1, // I219-LM rev 8  (Ice Lake ICP3)
    0x15E2, // I219-V  rev 8  (Ice Lake ICP3)
    0x0DC5, // I219-LM rev 9  (Comet Lake CMP)
    0x0DC6, // I219-V  rev 9  (Comet Lake CMP)
    0x0DC7, // I219-LM rev 10 (Comet Lake CMP2)
    0x0DC8, // I219-V  rev 10 (Comet Lake CMP2)
    0x15F9, // I219-LM rev 11 (Tiger Lake TGP)
    0x15FA, // I219-V  rev 11 (Tiger Lake TGP)
    0x15FB, // I219-LM rev 12 (Elkhart Lake TGP2)
    0x15FC, // I219-V  rev 12 (Elkhart Lake TGP2)
    0x1DC2, // I219-LM rev 13 (Meteor Lake MTP)
    0x1DC3, // I219-V  rev 14 (Meteor Lake MTP)
    0x1A1C, // I219-LM rev 15 (Alder Lake ADP-P)
    0x1A1D, // I219-V  rev 16 (Alder Lake ADP-P)
    0x1A1E, // I219-LM rev 17 (Alder Lake ADP-S)
    0x1A1F, // I219-V  rev 18 (Alder Lake ADP-S)
    0x0D9F, // I219-LM rev 19 (Alder Lake ADP-L)
    0x1DC5, // I219-LM rev 21 (Lunar Lake LNL)
    0x1DC6, // I219-V  rev 22 (Lunar Lake LNL)
    0x0D4E, // I219-LM rev 23 (Raptor Lake RPL)
    0x0D4F, // I219-V  rev 23 (Raptor Lake RPL)
    0x0D53, // I219-LM rev 24 (Raptor Lake RPL-P)
    0x0D55, // I219-V  rev 24 (Raptor Lake RPL-P)
];

// ───────────────────────────────────────────────────────────────────────────
// Register offsets (relative to BAR0 virtual base)
// ───────────────────────────────────────────────────────────────────────────
const REG_CTRL:     u32 = 0x0000_0; // Device control
const REG_STATUS:   u32 = 0x0000_8; // Device status
const REG_FEXTNVM6: u32 = 0x0001_0; // Future Extended NVM 6 (I21x)
const REG_EERD:     u32 = 0x0001_4; // EEPROM read
const REG_CTRL_EXT: u32 = 0x0001_8; // Extended device control
const REG_MDIC:     u32 = 0x0002_0; // MDI (PHY) control
const REG_FCAL:     u32 = 0x0002_8; // Flow Control Address Low
const REG_FCAH:     u32 = 0x0002_C; // Flow Control Address High
const REG_FCT:      u32 = 0x0003_0; // Flow Control Type (EtherType = 0x8808)
const REG_ICR:      u32 = 0x000C_0; // Interrupt Cause Read (auto-cleared on read)
const REG_ITR:      u32 = 0x000C_4; // Interrupt Throttling Rate
#[allow(dead_code)]
const REG_ICS:      u32 = 0x000C_8; // Interrupt Cause Set
#[allow(dead_code)]
const REG_IMS:      u32 = 0x000D_0; // Interrupt Mask Set/Read
const REG_IMC:      u32 = 0x000D_8; // Interrupt Mask Clear
const REG_RCTL:     u32 = 0x0010_0; // RX control
const REG_FCTTV:    u32 = 0x0017_0; // Flow Control Transmit Timer Value
const REG_TCTL:     u32 = 0x0040_0; // TX control
const REG_TIPG:     u32 = 0x0041_0; // TX Inter-Packet Gap
const REG_TADV:     u32 = 0x0382_C; // TX Absolute Interrupt Delay Value
const REG_TIDV:     u32 = 0x0382_0; // TX Interrupt Delay Value
const REG_RDBAL:    u32 = 0x0280_0; // RX Desc Base Address Low
const REG_RDBAH:    u32 = 0x0280_4; // RX Desc Base Address High
const REG_RDLEN:    u32 = 0x0280_8; // RX Descriptor Length
const REG_RDH:      u32 = 0x0281_0; // RX Descriptor Head
const REG_RDT:      u32 = 0x0281_8; // RX Descriptor Tail
const REG_RDTR:     u32 = 0x0282_0; // RX Delay Timer (interrupt coalescing)
const REG_RADV:     u32 = 0x0282_C; // RX Absolute Delay Timer
const REG_TDBAL:    u32 = 0x0380_0; // TX Desc Base Address Low
const REG_TDBAH:    u32 = 0x0380_4; // TX Desc Base Address High
const REG_TDLEN:    u32 = 0x0380_8; // TX Descriptor Length
const REG_TDH:      u32 = 0x0381_0; // TX Descriptor Head
const REG_TDT:      u32 = 0x0381_8; // TX Descriptor Tail
const REG_FCRTL:    u32 = 0x0292_0; // Flow Control Receive Threshold Low
const REG_FCRTH:    u32 = 0x0292_4; // Flow Control Receive Threshold High
const REG_MTA:      u32 = 0x0520_0; // Multicast Table Array (128 × u32)
const REG_RFCTL:    u32 = 0x0500_8; // Receive Filter Control (I21x)
const REG_RAL0:     u32 = 0x0540_0; // Receive Address Low  (filter 0)
const REG_RAH0:     u32 = 0x0540_4; // Receive Address High (filter 0)
const REG_IPCNFG:   u32 = 0x0E38;   // Internal PHY Configuration (EEE)
const REG_EEER:     u32 = 0x0E30;   // EEE Register
const REG_WUC:      u32 = 0x5800;   // Wake-up Control
const REG_WUFC:     u32 = 0x5808;   // Wake-up Filter Control
const REG_WUS:      u32 = 0x5810;   // Wake-up Status
const REG_FEXTNVM7: u32 = 0x5BB4;   // Future Extended NVM 7
const REG_RXCSUM:   u32 = 0x5000;   // Receive Checksum Offload Control
// Statistics registers (read-on-clear, 32-bit unless noted)
const REG_CRCERRS:  u32 = 0x4000;   // CRC Error Count
const REG_MPC:      u32 = 0x4010;   // Missed Packet Count
const REG_GPRC:     u32 = 0x4074;   // Good Packets Received Count
const REG_BPRC:     u32 = 0x4078;   // Broadcast Packets Received Count
const REG_MPRC:     u32 = 0x407C;   // Multicast Packets Received Count
const REG_GPTC:     u32 = 0x4080;   // Good Packets Transmitted Count
const REG_GORCL:    u32 = 0x4088;   // Good Octets Received Low
const REG_GORCH:    u32 = 0x408C;   // Good Octets Received High
const REG_GOTCL:    u32 = 0x4090;   // Good Octets Transmitted Low
const REG_GOTCH:    u32 = 0x4094;   // Good Octets Transmitted High
const REG_RNBC:     u32 = 0x40A0;   // Receive No Buffer Count
const REG_RUC:      u32 = 0x40A4;   // Receive Undersize Count
const REG_RFC:      u32 = 0x40A8;   // Receive Fragment Count
const REG_ROC:      u32 = 0x40AC;   // Receive Oversize Count
const REG_RJC:      u32 = 0x40B0;   // Receive Jabber Count
const REG_TORL:     u32 = 0x40C0;   // Total Octets Received Low
const REG_TORH:     u32 = 0x40C4;   // Total Octets Received High
const REG_TOTL:     u32 = 0x40C8;   // Total Octets Transmitted Low
const REG_TOTH:     u32 = 0x40CC;   // Total Octets Transmitted High
const REG_TPR:      u32 = 0x40D0;   // Total Packets Received
const REG_TPT:      u32 = 0x40D4;   // Total Packets Transmitted
const REG_MPTC:     u32 = 0x40F0;   // Multicast Packets Transmitted Count
const REG_BPTC:     u32 = 0x40F4;   // Broadcast Packets Transmitted Count

const REG_SWSM:      u32 = 0x05B50; // Software Semaphore
const SWSM_SMBI:     u32 = 1 << 0;  // Semaphore Bit
const SWSM_SWESMBI:  u32 = 1 << 1;  // Software EEPROM Semaphore Bit

const REG_MANC:      u32 = 0x05820; // Management Control

// ───────────────────────────────────────────────────────────────────────────
// CTRL register bits
// ───────────────────────────────────────────────────────────────────────────
const CTRL_FD:      u32 = 1 << 0;  // Full-duplex
const CTRL_ASDE:    u32 = 1 << 5;  // Auto-speed detection enable
const CTRL_SLU:     u32 = 1 << 6;  // Set link up
const CTRL_RST:     u32 = 1 << 26; // Device reset
const CTRL_PHY_RST: u32 = 1 << 31; // PHY reset
const CTRL_RFCE:    u32 = 1 << 27; // RX flow control enable
const CTRL_TFCE:    u32 = 1 << 28; // TX flow control enable

// ───────────────────────────────────────────────────────────────────────────
// STATUS register bits
// ───────────────────────────────────────────────────────────────────────────
const STATUS_FD:          u32 = 1 << 0; // Full-duplex
const STATUS_LU:          u32 = 1 << 1; // Link Up
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19; // GIO Master Enable Status
const STATUS_SPEED_MASK:  u32 = 3 << 6; // Speed bits [7:6]
const STATUS_SPEED_10:    u32 = 0 << 6; // 10 Mb/s
const STATUS_SPEED_100:   u32 = 1 << 6; // 100 Mb/s
const STATUS_SPEED_1000:  u32 = 2 << 6; // 1000 Mb/s (GbE)

// ───────────────────────────────────────────────────────────────────────────
// CTRL_EXT register bits
// ───────────────────────────────────────────────────────────────────────────
const CTRL_EXT_GIO_MASTER_DISABLE: u32 = 1 << 2; // Disable GIO master
/// PHY Power-Down Enable — when set, the MAC holds the PHY in power-down.
/// Must be CLEARED to allow the I219-V's internal PHY to power up.
const CTRL_EXT_PHYPDEN: u32 = 1 << 30;
/// Driver Loaded — signals to the PCH Management Engine (Intel ME) that the
/// OS Ethernet driver has taken control of the device.  Without this, on
/// I217/I218/I219 (PCH-based) controllers the ME may still intercept or
/// redirect certain receive traffic, causing DHCP to silently fail.
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;

// ───────────────────────────────────────────────────────────────────────────
// MDIC (MDI Control) register fields — used to access the PHY via MII
// ───────────────────────────────────────────────────────────────────────────
const MDIC_OP_WRITE: u32 = 1 << 26; // Opcode 01 = write
const MDIC_OP_READ:  u32 = 1 << 27; // Opcode 10 = read
const MDIC_READY:    u32 = 1 << 28; // Transaction complete
const MDIC_ERROR:    u32 = 1 << 30; // Error flag

// MII/MDIO register index 0 = Basic Mode Control Register (BMCR)
const PHY_REG_BMCR:    u32 = 0;
const BMCR_POWER_DOWN: u16 = 1 << 11; // PHY power-down bit in BMCR
#[allow(dead_code)]
const BMCR_RESET:      u16 = 1 << 15; // Software reset

// MII register 1 = Basic Mode Status Register (BMSR)
#[allow(dead_code)]
const PHY_REG_BMSR:    u32 = 1;
#[allow(dead_code)]
const BMSR_LINK_STATUS: u16 = 1 << 2;  // Link status (1 = link up)
#[allow(dead_code)]
const BMSR_ANEG_COMPL:  u16 = 1 << 5;  // Auto-negotiation complete

// MII register 4 = Auto-Negotiation Advertisement Register (ANAR)
#[allow(dead_code)]
const PHY_REG_ANAR:    u32 = 4;
#[allow(dead_code)]
const ANAR_10_HDX:     u16 = 1 << 5;   // 10BASE-T half-duplex
#[allow(dead_code)]
const ANAR_10_FDX:     u16 = 1 << 6;   // 10BASE-T full-duplex
#[allow(dead_code)]
const ANAR_100_HDX:    u16 = 1 << 7;   // 100BASE-TX half-duplex
#[allow(dead_code)]
const ANAR_100_FDX:    u16 = 1 << 8;   // 100BASE-TX full-duplex
#[allow(dead_code)]
const ANAR_PAUSE:      u16 = 1 << 10;  // PAUSE capability (symmetric flow control)
#[allow(dead_code)]
const ANAR_ASM_DIR:    u16 = 1 << 11;  // Asymmetric PAUSE direction
#[allow(dead_code)]
const ANAR_SELECTOR:   u16 = 1;        // IEEE 802.3 selector field

// MII register 9 = 1000BASE-T Control Register
#[allow(dead_code)]
const PHY_REG_1KTCTL:  u32 = 9;
#[allow(dead_code)]
const TCTL_1KT_FDX:    u16 = 1 << 9;   // Advertise 1000BASE-T full-duplex
#[allow(dead_code)]
const TCTL_1KT_HDX:    u16 = 1 << 8;   // Advertise 1000BASE-T half-duplex

// MII register 16 = PHY Specific Control Register
const PHY_REG_PSCR:    u32 = 16;
const PSCR_LPLU_NON_D0: u16 = 1 << 10; // Low Power Link Up in non-D0 states
const PSCR_LPLU_D0:     u16 = 1 << 11; // Low Power Link Up in D0 state
const PSCR_SPD:        u16 = 1 << 12;  // Smart Power Down

// ───────────────────────────────────────────────────────────────────────────
// RCTL register bits
// ───────────────────────────────────────────────────────────────────────────
const RCTL_EN:      u32 = 1 << 1;  // RX enable
#[allow(dead_code)]
const RCTL_SBP:     u32 = 1 << 2;  // Store bad packets
const RCTL_UPE:     u32 = 1 << 3;  // Unicast promiscuous (accept all unicast)
const RCTL_MPE:     u32 = 1 << 4;  // Multicast promiscuous
#[allow(dead_code)]
const RCTL_LPE:     u32 = 1 << 5;  // Long packet reception enable (jumbo frames)
#[allow(dead_code)]
const RCTL_LBM_NO:  u32 = 0 << 6;  // No loopback (normal operation)
#[allow(dead_code)]
const RCTL_LBM_MAC: u32 = 1 << 6;  // MAC loopback
#[allow(dead_code)]
const RCTL_RDMTS_HALF: u32 = 0 << 8; // RX desc min threshold = 1/2 ring
#[allow(dead_code)]
const RCTL_RDMTS_QUAR: u32 = 1 << 8; // RX desc min threshold = 1/4 ring
const RCTL_BAM:     u32 = 1 << 15; // Broadcast accept
#[allow(dead_code)]
const RCTL_BSIZE_2048: u32 = 0;    // Buffer size = 2048 (BSEX=0, BSIZE=00)
#[allow(dead_code)]
const RCTL_VFE:     u32 = 1 << 18; // VLAN filter enable
#[allow(dead_code)]
const RCTL_CFIEN:   u32 = 1 << 19; // Canonical form indicator enable
#[allow(dead_code)]
const RCTL_CFI:     u32 = 1 << 20; // Canonical form indicator bit value
#[allow(dead_code)]
const RCTL_DPF:     u32 = 1 << 22; // Discard PAUSE frames
#[allow(dead_code)]
const RCTL_PMCF:    u32 = 1 << 23; // Pass MAC control frames
const RCTL_SECRC:   u32 = 1 << 26; // Strip Ethernet CRC

// ───────────────────────────────────────────────────────────────────────────
// TCTL register bits
// ───────────────────────────────────────────────────────────────────────────
const TCTL_EN:   u32 = 1 << 1; // TX enable
const TCTL_PSP:  u32 = 1 << 3; // Pad short packets
const TCTL_CT:   u32 = 0x0F << 4;  // Collision threshold (standard = 15)
const TCTL_COLD: u32 = 0x40 << 12; // Collision distance (full-duplex = 64)
#[allow(dead_code)]
const TCTL_RTLC: u32 = 1 << 24; // Re-transmit on late collision

// TIPG: TX Inter-Packet Gap for 802.3 GbE (standard = 0x0060_200A)
// Fields: IPGT[9:0]=10, IPGR1[19:10]=8, IPGR2[29:20]=6
const TIPG_IPGT_GBE: u32 = 0x0060_200A;
// For 10/100 Mbps: IPGT=10, IPGR1=10, IPGR2=10.
// The e1000e Linux driver uses 0x00602008 for 10/100.
#[allow(dead_code)]
const TIPG_IPGT_10_100: u32 = 0x0060_2008;

// ───────────────────────────────────────────────────────────────────────────
// Flow control constants
// ───────────────────────────────────────────────────────────────────────────
/// IEEE 802.3x PAUSE frame multicast destination address bytes 0–3, stored
/// little-endian in FCAL: MAC 01:80:C2:00:00:01 → bytes {01,80,C2,00} →
/// little-endian u32 = 0x00C28001.  Matches Linux E1000_FCAL_DEF.
const FLOW_CTRL_ADDR_LO: u32 = 0x00C28001;
/// IEEE 802.3x PAUSE frame multicast destination address bytes 4–5, stored
/// little-endian in FCAH: MAC[4..6] = {00,01} → little-endian u32 = 0x0100.
const FLOW_CTRL_ADDR_HI: u32 = 0x0100;
/// EtherType for IEEE 802.3x PAUSE frames.
const FLOW_CTRL_TYPE:    u32 = 0x8808;
/// FCT transmit timer: pause for ~33 ms (at GbE, 1 unit ≈ 512 bit times).
const FCTTV_DEFAULT:     u32 = 0x0100;
/// FCRTH: start sending PAUSE frames when RX FIFO reaches this threshold.
/// Bits [15:3] = threshold in 8-byte units (0x8000 >> 3 × 8 = 32 KiB).
/// Matches Linux E1000_FC_HIGH_THRESH default.
const FCRTH_DEFAULT:     u32 = 0x8000;
/// FCRTL: stop sending PAUSE frames when RX FIFO drops to this threshold.
/// Bits [15:3] = threshold in 8-byte units (0x4000 >> 3 × 8 = 16 KiB).
/// Matches Linux E1000_FC_LOW_THRESH default.
const FCRTL_DEFAULT:     u32 = 0x4000;

// ───────────────────────────────────────────────────────────────────────────
// Interrupt mask bits (for IMS/IMC registers)
// ───────────────────────────────────────────────────────────────────────────
#[allow(dead_code)]
const IMS_RXT0:  u32 = 1 << 7;  // RX timer interrupt (RDTR expired)
#[allow(dead_code)]
const IMS_RXO:   u32 = 1 << 6;  // RX overrun
#[allow(dead_code)]
const IMS_RXDMT: u32 = 1 << 4;  // RX descriptor minimum threshold reached
#[allow(dead_code)]
const IMS_TXDW:  u32 = 1 << 0;  // TX descriptor written back
#[allow(dead_code)]
const IMS_LSC:   u32 = 1 << 2;  // Link status change

// ───────────────────────────────────────────────────────────────────────────
// TX/RX descriptor command/status bits
// ───────────────────────────────────────────────────────────────────────────
const TXD_CMD_EOP:  u8 = 1 << 0; // End of packet
const TXD_CMD_IFCS: u8 = 1 << 1; // Insert FCS
const TXD_CMD_RS:   u8 = 1 << 3; // Report status
const TXD_STA_DD:   u8 = 1 << 0; // Descriptor done
const RXD_STA_DD:   u8 = 1 << 0; // Descriptor done
const RXD_STA_EOP:  u8 = 1 << 1; // End of Packet (last descriptor for this frame)

/// Error bits in the legacy RX descriptor errors byte that indicate the
/// received frame data is corrupt and must be discarded.
///
/// * Bit 0 (CE)  — CRC Error or Alignment Error: frame CRC does not match.
/// * Bit 1 (SE)  — Symbol Error: 8B/10B code violation on the link.
/// * Bit 7 (RXE) — Rx Data Error: PCI bus or DMA error during write.
///
/// Bits 5 (TCPE) and 6 (IPE) are TCP/UDP and IP checksum offload results;
/// they are only valid when checksum offload is enabled in RXCSUM. We always
/// write RXCSUM = 0, so those bits will never be set by the hardware.
/// Bits 2 (SEQ), 3 (reserved), and 4 (Carrier Extension) are either transient
/// or not fatal to the frame data and are handled by the upper-layer stack.
const RXD_ERR_FATAL: u8 = (1 << 0) | (1 << 1) | (1 << 7); // CE | SE | RXE

// ───────────────────────────────────────────────────────────────────────────
// Descriptor ring sizes — powers of two so modulo is cheap
// ───────────────────────────────────────────────────────────────────────────
const RX_RING_SIZE: usize = 128;
const TX_RING_SIZE: usize = 128;
const PACKET_BUF_SIZE: usize = 2048;

// ───────────────────────────────────────────────────────────────────────────
// Legacy RX descriptor (16 bytes, little-endian)
// ───────────────────────────────────────────────────────────────────────────
#[repr(C, packed)]
struct RxDesc {
    buffer_addr: u64,
    length:      u16,
    checksum:    u16,
    status:      u8,
    errors:      u8,
    special:     u16,
}

// ───────────────────────────────────────────────────────────────────────────
// Legacy TX descriptor (16 bytes, little-endian)
// ───────────────────────────────────────────────────────────────────────────
#[repr(C, packed)]
struct TxDesc {
    buffer_addr: u64,
    length:      u16,
    cso:         u8,
    cmd:         u8,
    status:      u8,
    css:         u8,
    special:     u16,
}

// ───────────────────────────────────────────────────────────────────────────
// Device state (behind a Mutex so the outer type is Sync)
// ───────────────────────────────────────────────────────────────────────────
struct E1000EInner {
    /// Virtual base address of the MMIO BAR
    mmio_base: u64,
    device_id: u16,
    bus: u8,
    dev: u8,
    func: u8,
    mac: [u8; 6],
    phy_addr: u8, // Dynamically discovered MDIO address of the PHY

    // RX ring
    rx_descs_virt: u64,             // virtual address of descriptor ring
    rx_descs_phys: u64,             // physical address of descriptor ring
    rx_bufs: [(u64, u64); RX_RING_SIZE], // (virt, phys) per slot
    rx_tail: usize,                 // software's read cursor

    // TX ring
    tx_descs_virt: u64,
    tx_descs_phys: u64,
    tx_bufs: [(u64, u64); TX_RING_SIZE],
    tx_tail: usize,

    // Link state (updated after init and on link-change events)
    link_up:     bool,
    link_speed:  u32,  // 10 / 100 / 1000 (Mb/s), 0 if unknown
    full_duplex: bool,

    // Software packet/byte counters (updated in send/receive paths)
    rx_packets: u64,
    tx_packets: u64,
    rx_bytes:   u64,
    tx_bytes:   u64,
    rx_errors:  u64,
    tx_errors:  u64,
}

/// Public handle to an Intel e1000e Ethernet device.
pub struct E1000EDevice {
    inner: Mutex<E1000EInner>,
}

// ───────────────────────────────────────────────────────────────────────────
// Module-level device registry (populated during init)
// ───────────────────────────────────────────────────────────────────────────
static E1000E_DEVICES: Mutex<Vec<Arc<E1000EDevice>>> = Mutex::new(Vec::new());

// ───────────────────────────────────────────────────────────────────────────
// Register read/write and memory helpers
// ───────────────────────────────────────────────────────────────────────────
impl E1000EInner {
    #[inline]
    fn clflush(&self, addr: u64) {
        unsafe {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack, preserves_flags));
        }
    }

    /// Flush a range of memory from the CPU cache to RAM.
    /// Operates in 64-byte chunks (standard x86 cache line size).
    fn flush_cache_range(&self, start: u64, len: usize) {
        let end = start + len as u64;
        let mut cur = start & !63;
        while cur < end {
            self.clflush(cur);
            cur += 64;
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }

    #[inline]
    fn read32(&self, reg: u32) -> u32 {
        unsafe { read_volatile((self.mmio_base + reg as u64) as *const u32) }
    }

    #[inline]
    fn write32(&self, reg: u32, val: u32) {
        unsafe { write_volatile((self.mmio_base + reg as u64) as *mut u32, val) }
    }

    /// Read the MAC address stored in the Receive Address register 0.
    fn mac_from_rar(&self) -> [u8; 6] {
        let ral = self.read32(REG_RAL0);
        let rah = self.read32(REG_RAH0);
        [
            (ral         & 0xFF) as u8,
            ((ral >>  8) & 0xFF) as u8,
            ((ral >> 16) & 0xFF) as u8,
            ((ral >> 24) & 0xFF) as u8,
            (rah         & 0xFF) as u8,
            ((rah >>  8) & 0xFF) as u8,
        ]
    }

    /// Attempt to read a 16-bit word from the NVM via the EERD register.
    /// Returns `None` if the NVM read times out.
    fn nvm_read(&self, word_addr: u16) -> Option<u16> {
        // Start the read: address << 2 | START
        self.write32(REG_EERD, ((word_addr as u32) << 2) | 0x1);

        // Poll for completion; the DONE bit is bit 1 for most e1000e variants
        // and bit 4 for a few older ones — check both.
        for _ in 0..20_000 {
            let v = self.read32(REG_EERD);
            if (v & (1 << 1)) != 0 || (v & (1 << 4)) != 0 {
                return Some((v >> 16) as u16);
            }
            for _ in 0..200 { core::hint::spin_loop(); }
        }
        None
    }

    /// Returns true if this is a PCH-family NIC (I217/I218/I219).
    fn is_pch(&self) -> bool {
        // PCH-integrated Ethernet IDs are not strictly sequential (e.g. Comet Lake/Raptor Lake
        // have IDs < 0x153A). We check against our exhaustive PCH subset of E1000E_DEVICE_IDS.
        match self.device_id {
            0x153A | 0x153B | // I217
            0x155A | 0x1559 | 0x15A0 | 0x15A1 | 0x15A2 | 0x15A3 | // I218
            0x156F | 0x1570 | 0x15B7 | 0x15B8 | 0x15BB | 0x15BC | 0x15D7 | 0x15D8 | // I219 gen 1-4
            0x15E3 | 0x15D6 | 0x15BD | 0x15BE | 0x15DF | 0x15E0 | 0x15E1 | 0x15E2 | // I219 gen 5-8
            0x0DC5 | 0x0DC6 | 0x0DC7 | 0x0DC8 | 0x15F9 | 0x15FA | 0x15FB | 0x15FC | // I219 gen 9-12
            0x1DC2 | 0x1DC3 | 0x1A1C | 0x1A1D | 0x1A1E | 0x1A1F | 0x0D9F | 0x1DC5 | // I219 gen 13-21
            0x1DC6 | 0x0D4E | 0x0D4F | 0x0D53 | 0x0D55 => true, // I219 gen 22-24
            _ => false,
        }
    }

    /// Returns true if this is an I218 or I219 (LPT/SPT and later PCH).
    fn is_pch_lpt(&self) -> bool {
        self.device_id >= 0x155A // Minimal I218-LM ID
    }

    /// Acquire the software semaphore for PHY/EEPROM access on PCH systems.
    /// This is necessary because the Management Engine (ME) may also be
    /// accessing the PHY.
    fn acquire_phy_semaphore(&self) -> bool {
        if !self.is_pch() { return true; }

        // 1. Wait for SMBI (Semaphore Bit) to be clear.
        //    This bit is used for BIOS/OS handoff.
        for _ in 0..10_000 {
            let swsm = self.read32(REG_SWSM);
            if swsm & SWSM_SMBI == 0 {
                // Try to take it
                self.write32(REG_SWSM, swsm | SWSM_SMBI);
                if self.read32(REG_SWSM) & SWSM_SMBI != 0 {
                    break;
                }
            }
            core::hint::spin_loop();
        }

        // 2. Now wait for SWESMBI (Software EEPROM Semaphore Bit).
        //    This bit controls access to the PHY/MDIO interface.
        let mut acquired = false;
        for _ in 0..10_000 {
            let swsm = self.read32(REG_SWSM);
            self.write32(REG_SWSM, swsm | SWSM_SWESMBI);
            if self.read32(REG_SWSM) & SWSM_SWESMBI != 0 {
                acquired = true;
                break;
            }
            core::hint::spin_loop();
        }

        if !acquired {
            // Release SMBI if we failed to get SWESMBI
            let swsm = self.read32(REG_SWSM);
            self.write32(REG_SWSM, swsm & !SWSM_SMBI);
        }
        acquired
    }

    /// Release the software semaphore.
    fn release_phy_semaphore(&self) {
        if !self.is_pch() { return; }
        let swsm = self.read32(REG_SWSM);
        self.write32(REG_SWSM, swsm & !(SWSM_SWESMBI | SWSM_SMBI));
    }

    fn mdic_read(&self, phy_reg: u32) -> Option<u16> {
        if !self.acquire_phy_semaphore() { return None; }
        let res = self.mdic_read_raw(self.phy_addr, phy_reg);
        self.release_phy_semaphore();
        res
    }

    fn mdic_write(&self, phy_reg: u32, data: u16) -> bool {
        if !self.acquire_phy_semaphore() { return false; }
        let res = self.mdic_write_raw(self.phy_addr, phy_reg, data);
        self.release_phy_semaphore();
        res
    }

    /// Read a PHY register directly given a specific PHY address
    fn mdic_read_raw(&self, phy_addr: u8, phy_reg: u32) -> Option<u16> {
        self.write32(REG_MDIC,
            ((phy_addr as u32 & 0x1F) << 21) | ((phy_reg & 0x1F) << 16) | MDIC_OP_READ);
        for _ in 0..100_000 {
            let v = self.read32(REG_MDIC);
            if v & MDIC_READY != 0 {
                return if v & MDIC_ERROR != 0 { None } else { Some((v & 0xFFFF) as u16) };
            }
            for _ in 0..100 { core::hint::spin_loop(); }
        }
        None // timeout
    }

    /// Write a PHY register directly given a specific PHY address
    fn mdic_write_raw(&self, phy_addr: u8, phy_reg: u32, data: u16) -> bool {
        self.write32(REG_MDIC,
            ((phy_addr as u32 & 0x1F) << 21) | ((phy_reg & 0x1F) << 16) | MDIC_OP_WRITE | (data as u32));
        for _ in 0..100_000 {
            let v = self.read32(REG_MDIC);
            if v & MDIC_READY != 0 {
                return v & MDIC_ERROR == 0;
            }
            for _ in 0..100 { core::hint::spin_loop(); }
        }
        false // timeout
    }

    /// Scan MDIO addresses to discover the PHY
    fn detect_phy_addr(&self) -> Option<u8> {
        if !self.acquire_phy_semaphore() { return None; }
        for addr in 1..=32 {
            let phy_addr = (addr % 32) as u8;
            if let Some(val) = self.mdic_read_raw(phy_addr, PHY_REG_BMCR) {
                if val != 0xFFFF && val != 0 {
                    self.release_phy_semaphore();
                    return Some(phy_addr);
                }
            }
        }
        self.release_phy_semaphore();
        None
    }

    /// Detect link speed and duplex from the STATUS register and update the
    /// `link_up`, `link_speed`, and `full_duplex` fields.
    fn detect_link_state(&mut self) {
        let status = self.read32(REG_STATUS);
        self.link_up = status & STATUS_LU != 0;
        self.full_duplex = status & STATUS_FD != 0;
        self.link_speed = if self.link_up {
            match status & STATUS_SPEED_MASK {
                STATUS_SPEED_10   => 10,
                STATUS_SPEED_100  => 100,
                STATUS_SPEED_1000 => 1000,
                _                 => 1000, // treat unknown as 1000
            }
        } else {
            0
        };

        // Fallback: if STATUS_LU is clear, check PHY BMSR as a backup.
        // On some I219-V revisions, the MAC link bit can be transiently clear
        // during auto-negotiation transitions even if the PHY has carrier.
        if !self.link_up {
            if let Some(bmsr) = self.mdic_read(PHY_REG_BMSR) {
                if bmsr & BMSR_LINK_STATUS != 0 {
                    self.link_up = true;
                    // Duplex/speed still come from STATUS or we assume defaults
                }
            }
        }
    }

    /// Read and clear the hardware statistics registers, accumulating their
    /// values into the software counters.  On e1000e-family hardware the
    /// statistics registers are read-on-clear (RoC): each read returns the
    /// count since the last read and then resets the register to zero.
    /// Callers should therefore call this method periodically (or before
    /// reporting statistics) to avoid counter overflow.
    fn update_hw_stats(&mut self) {
        // Receive counters
        let gprc  = self.read32(REG_GPRC) as u64;
        let gorcl = self.read32(REG_GORCL) as u64;
        let gorch = self.read32(REG_GORCH) as u64;
        let mpc   = self.read32(REG_MPC)  as u64; // missed (no buffer) = RX error

        self.rx_packets += gprc;
        self.rx_bytes   += gorcl | (gorch << 32);
        self.rx_errors  += mpc;

        // Transmit counters
        let gptc  = self.read32(REG_GPTC) as u64;
        let gotcl = self.read32(REG_GOTCL) as u64;
        let gotch = self.read32(REG_GOTCH) as u64;

        self.tx_packets += gptc;
        self.tx_bytes   += gotcl | (gotch << 32);

        // Read-and-discard additional stats to prevent 32-bit overflow
        let _ = self.read32(REG_CRCERRS);
        let _ = self.read32(REG_RNBC);
        let _ = self.read32(REG_RUC);
        let _ = self.read32(REG_RFC);
        let _ = self.read32(REG_ROC);
        let _ = self.read32(REG_RJC);
        let _ = self.read32(REG_BPRC);
        let _ = self.read32(REG_MPRC);
        let _ = self.read32(REG_TORL);
        let _ = self.read32(REG_TORH);
        let _ = self.read32(REG_TOTL);
        let _ = self.read32(REG_TOTH);
        let _ = self.read32(REG_TPR);
        let _ = self.read32(REG_TPT);
        let _ = self.read32(REG_MPTC);
        let _ = self.read32(REG_BPTC);
    }

    /// Full hardware initialisation.  Returns `true` on success.
    unsafe fn init(&mut self) -> bool {
        // 1a. Disable PCIe ASPM (Active State Power Management) L1.1 and L1.2.
        //     On Intel I219-V, ASPM can cause severe DMA instability and "fatal RX errors"
        //     when the CPU enters low-power C-states. Disabling it in the PCIe Link
        //     Control register is required for stable bare-metal operation.
        if self.is_pch() {
            serial::serial_print("[e1000e] Disabling PCIe ASPM L1.1/L1.2...\n");
            // PCIe Capability ID = 0x10
            let pci_dev = crate::pci::PciDevice {
                bus: self.bus,
                device: self.dev,
                function: self.func,
                vendor_id: INTEL_VENDOR_ID,
                device_id: self.device_id,
                class_code: 0, subclass: 0, prog_if: 0, header_type: 0, bar0: 0, interrupt_line: 0,
            };
            let cap_pos = crate::pci::pci_find_capability(&pci_dev, 0x10);
            if cap_pos != 0 {
                // Link Control Register is at offset 0x10 from capability base.
                // ASPM Control is bits 1:0. We clear them to disable ASPM.
                let link_ctrl = crate::pci::pci_config_read_u16(self.bus, self.dev, self.func, cap_pos + 0x10);
                if link_ctrl & 0x3 != 0 {
                    serial::serial_print("[e1000e]   Current Link Control: 0x");
                    serial::serial_print_hex(link_ctrl as u64);
                    serial::serial_print(" -> Disabling ASPM\n");
                    crate::pci::pci_config_write_u16(self.bus, self.dev, self.func, cap_pos + 0x10, link_ctrl & !0x3);
                }
            }
        }

        self.write32(REG_IMC, 0xFFFF_FFFF);
        let _ = self.read32(REG_ICR); // clear any pending causes

        // 1b. GIO Master Disable Handshake (recommended for all Intel NICs).
        //     This ensures any pending DMA transactions from a previous OS or 
        //     UEFI session are terminated before we global-reset the card.
        //     Failing to do this can cause PCIe bus hangs or corrupted init.
        serial::serial_print("[e1000e] Disabling GIO Master...\n");
        let ctrl_ext = self.read32(REG_CTRL_EXT);
        self.write32(REG_CTRL_EXT, ctrl_ext | CTRL_EXT_GIO_MASTER_DISABLE);
        let mut master_disabled = false;
        for _ in 0..20_000 {
            if self.read32(REG_STATUS) & STATUS_GIO_MASTER_ENABLE == 0 {
                master_disabled = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !master_disabled {
            serial::serial_print("[e1000e] WARN: GIO Master Disable bit did not clear, forcing reset anyway\n");
        } else {
            serial::serial_print("[e1000e] GIO Master disabled\n");
        }

        // 2. Issue a device reset and wait for it to clear.
        //    For I217/I218/I219 we also set PHY_RST to ensure the integrated
        //    PHY is power-cycled and any UEFI-stale state is cleared.
        let mut ctrl = self.read32(REG_CTRL);
        if self.is_pch() {
            ctrl |= CTRL_PHY_RST;
        }
        self.write32(REG_CTRL, ctrl | CTRL_RST);
        
        // Wait for RST to clear
        for _ in 0..200_000 { core::hint::spin_loop(); }
        let mut waited = 0u32;
        loop {
            if self.read32(REG_CTRL) & CTRL_RST == 0 { break; }
            core::hint::spin_loop();
            waited += 1;
            if waited > 500_000 {
                serial::serial_print("[e1000e] WARN: RST bit did not clear, continuing anyway\n");
                break;
            }
        }

        // Allow the NVM autoload and PLL to fully stabilise after the Global
        // Reset.  Intel I217/I218/I219 datasheets recommend a minimum of 20 ms
        // before accessing any device registers after RST clears.
        for _ in 0..3_000_000u32 { core::hint::spin_loop(); }

        // After reset, verify GIO master is re-enabled for DMA
        let mut master_enabled = false;
        for _ in 0..10_000 {
            if self.read32(REG_STATUS) & STATUS_GIO_MASTER_ENABLE != 0 {
                master_enabled = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !master_enabled {
            serial::serial_print("[e1000e] WARN: GIO Master still disabled after reset!\n");
        }

        // 3. Disable interrupts again (reset re-enables them) and clear ICR
        self.write32(REG_IMC, 0xFFFF_FFFF);
        let _ = self.read32(REG_ICR);

        // 3a. PCH / I219-V specific hardware quirks.
        if self.is_pch() {
            // Disable K1 power state (Future Extended NVM 6 register).
            let fextnvm6 = self.read32(REG_FEXTNVM6);
            if fextnvm6 & (1 << 11) == 0 {
                serial::serial_print("[e1000e] Disabling K1 power state in FEXTNVM6\n");
                self.write32(REG_FEXTNVM6, fextnvm6 | (1 << 11));
            }

            // I219-V Quirk: Clear all Wake-up filters.
            // These filters can be left active by a previous boot session (e.g. Windows)
            // and can silently drop broadcast DHCP traffic.
            serial::serial_print("[e1000e] Clearing Wake-up and Management filters\n");
            self.write32(REG_WUC, 0);
            self.write32(REG_WUFC, 0);
            self.write32(REG_WUS, 0);

            // I219-V Quirk: Set bit 18 of FEXTNVM7 (Side clock ungate).
            // Prevents link drops and DMA hangs on some Silicon revisions.
            let fextnvm7 = self.read32(REG_FEXTNVM7);
            if fextnvm7 & (1 << 18) == 0 {
                serial::serial_print("[e1000e] Setting bit 18 in FEXTNVM7 (Side clock ungate)\n");
                self.write32(REG_FEXTNVM7, fextnvm7 | (1 << 18));
            }
        }

        // 3b. PCH / I219-V PHY power-up sequence.
        //     UEFI firmware may leave CTRL_EXT.PHYPDEN set (bit 30), which
        //     forces the integrated PHY into power-down and prevents link.
        //     Clear it unconditionally so the PHY can auto-negotiate.
        let ctrl_ext = self.read32(REG_CTRL_EXT);
        if ctrl_ext & CTRL_EXT_PHYPDEN != 0 {
            serial::serial_print("[e1000e] CTRL_EXT.PHYPDEN was set — powering up PHY\n");
            self.write32(REG_CTRL_EXT, ctrl_ext & !CTRL_EXT_PHYPDEN);
            // Allow the PHY a moment to wake from power-down (~500 µs on a
            // 3 GHz CPU where PAUSE ≈ 10 ns → 50 000 × 10 ns = 500 µs).
            for _ in 0..50_000 { core::hint::spin_loop(); }
        } else {
            // Always clear the bit on PCH-family (harmless if already clear)
            self.write32(REG_CTRL_EXT, ctrl_ext & !CTRL_EXT_PHYPDEN);
        }

        // 3b. Signal to the Intel PCH Management Engine (ME) that the OS
        //     driver has loaded and is taking ownership of the device.
        //     Setting CTRL_EXT.DRV_LOAD (bit 28) prevents the ME from
        //     intercepting or re-directing received traffic, which on
        //     I217/I218/I219 controllers can silently block DHCP replies.
        //
        //     After setting DRV_LOAD we wait ~10 ms to give the ME firmware
        //     time to complete the handoff before we continue.  Without
        //     this delay, on some real I219-V systems the ME may still
        //     intercept the very first received frames (including DHCP
        //     OFFERs) even though DRV_LOAD has been written.
        {
            serial::serial_print("[e1000e] Synchronizing DRV_LOAD with Intel ME...\n");
            // Toggle DRV_LOAD to ensure the ME notices the transition
            let ctrl_ext_tmp = self.read32(REG_CTRL_EXT);
            self.write32(REG_CTRL_EXT, ctrl_ext_tmp & !CTRL_EXT_DRV_LOAD);
            for _ in 0..10_000 { core::hint::spin_loop(); }
            
            self.write32(REG_CTRL_EXT, ctrl_ext_tmp | CTRL_EXT_DRV_LOAD);
            // Reduced settlement time for QEMU (100_000 × PAUSE)
            for _ in 0..100_000 { core::hint::spin_loop(); }
        }

        // 3c. Scan for the PHY address, then check and clear the Power-Down
        //     bit in its BMCR register (MII register 0). Some firmware leaves
        //     the I219-V PHY in power-down mode after the OS hand-off.
        if let Some(addr) = self.detect_phy_addr() {
            self.phy_addr = addr;
            serial::serial_print("[e1000e] Found PHY at address: ");
            serial::serial_print_dec(addr as u64);
            serial::serial_print("\n");

            if let Some(bmcr) = self.mdic_read(PHY_REG_BMCR) {
                // Issue a PHY Software Reset (bit 15) to ensure a clean state
                serial::serial_print("[e1000e] Resetting PHY via BMCR...\n");
                self.mdic_write(PHY_REG_BMCR, bmcr | BMCR_RESET);
                // PHY reset takes ~10ms; 100_000 iterations is plenty
                for _ in 0..100_000 { core::hint::spin_loop(); }

                if bmcr & BMCR_POWER_DOWN != 0 {
                    serial::serial_print("[e1000e] PHY BMCR power-down bit set — clearing\n");
                    self.mdic_write(PHY_REG_BMCR, (bmcr & !BMCR_POWER_DOWN) | BMCR_RESET);
                    // Give the PHY ~500 µs to exit power-down before configuring
                    for _ in 0..50_000 { core::hint::spin_loop(); }
                }

                // I219-V Quirk: Disable Smart Power Down (SPD)
                if let Some(pscr) = self.mdic_read(PHY_REG_PSCR) {
                    if pscr & PSCR_SPD != 0 {
                        serial::serial_print("[e1000e] Disabling PHY Smart Power Down (SPD)\n");
                        self.mdic_write(PHY_REG_PSCR, pscr & !PSCR_SPD);
                    }
                }

                // I219-V Quirk: Clear bit 2 of PHY register 18 (0x12) - Configuration Register 1.
                // This is a documented fix in Intel/Linux drivers for I219-V link instability.
                if let Some(reg18) = self.mdic_read(0x12) {
                    if reg18 & (1 << 2) != 0 {
                        serial::serial_print("[e1000e] Clearing bit 2 in PHY register 18 (I219 quirk)\n");
                        self.mdic_write(0x12, reg18 & !(1 << 2));
                    }
                }

                // I219-V Quirk: Set bit 10 of PHY register 26 (0x1A).
                // Required for stable DMA operation on some PCH-family NICs to prevent RX hangs.
                if let Some(reg26) = self.mdic_read(0x1A) {
                    if reg26 & (1 << 10) == 0 {
                        serial::serial_print("[e1000e] Setting bit 10 in PHY register 26 (I219 quirk)\n");
                        self.mdic_write(0x1A, reg26 | (1 << 10));
                    }
                }

                // I219-V Quirk: Ensure PHY power management doesn't aggressively power-down
                // the PHY in non-D0 states if bit 11 of register 17 is set.
                if let Some(reg17) = self.mdic_read(0x11) {
                    if reg17 & (1 << 11) == 0 {
                        serial::serial_print("[e1000e] Setting bit 11 in PHY register 17 (I219 power quirk)\n");
                        self.mdic_write(0x11, reg17 | (1 << 11));
                    }
                }

                // I219-V Quirk: Set bit 14 of PHY register 25 (0x19).
                // Required for reliable clock gating during link transitions on PCH-based NICs.
                if let Some(reg25) = self.mdic_read(0x19) {
                    if reg25 & (1 << 14) == 0 {
                        serial::serial_print("[e1000e] Setting bit 14 in PHY register 25 (I219 quirk)\n");
                        self.mdic_write(0x19, reg25 | (1 << 14));
                    }
                }

                // Explicit Auto-Negotiation Advertisement.
                // Ensure we advertise all speeds: 10/100/1000 Full Duplex.
                // Register 4: ANAR
                if let Some(anar) = self.mdic_read(PHY_REG_ANAR) {
                    let new_anar = anar | ANAR_10_FDX | ANAR_100_FDX | ANAR_SELECTOR | ANAR_PAUSE | ANAR_ASM_DIR;
                    self.mdic_write(PHY_REG_ANAR, new_anar);
                }
                // Register 9: 1000BASE-T Control (1KTCTL)
                if let Some(msctl) = self.mdic_read(PHY_REG_1KTCTL) {
                    self.mdic_write(PHY_REG_1KTCTL, msctl | TCTL_1KT_FDX);
                }

                // Restart Auto-Negotiation
                if let Some(bmcr) = self.mdic_read(PHY_REG_BMCR) {
                    self.mdic_write(PHY_REG_BMCR, bmcr | (1 << 9) | (1 << 12)); // Restart Auto-Neg + Enable Auto-Neg
                }

                // I219-V Quirk: Disable Low Power Link Up (LPLU)
                if let Some(pscr) = self.mdic_read(PHY_REG_PSCR) {
                    if pscr & (PSCR_LPLU_D0 | PSCR_LPLU_NON_D0) != 0 {
                        serial::serial_print("[e1000e] Disabling PHY Low Power Link Up (LPLU)\n");
                        self.mdic_write(PHY_REG_PSCR, pscr & !(PSCR_LPLU_D0 | PSCR_LPLU_NON_D0));
                    }
                }
            } else {
                serial::serial_print("[e1000e] WARN: MDIC read of PHY BMCR failed\n");
            }
        } else {
            serial::serial_print("[e1000e] WARN: Could not detect PHY address! Link may stay offline.\n");
        }

        // 4. Read MAC address.
        //    BIOS/UEFI firmware typically programs RAR[0]; if it looks valid
        //    we use it directly.  Otherwise fall back to the NVM.
        let rar_mac = self.mac_from_rar();
        let rar_valid = rar_mac.iter().any(|&b| b != 0)
            && rar_mac != [0xFF; 6];

        if rar_valid {
            self.mac = rar_mac;
        } else {
            if let (Some(w0), Some(w1), Some(w2)) = (
                self.nvm_read(0),
                self.nvm_read(1),
                self.nvm_read(2),
            ) {
                self.mac = [
                    (w0 & 0xFF) as u8, ((w0 >> 8) & 0xFF) as u8,
                    (w1 & 0xFF) as u8, ((w1 >> 8) & 0xFF) as u8,
                    (w2 & 0xFF) as u8, ((w2 >> 8) & 0xFF) as u8,
                ];
            } else {
                serial::serial_print("[e1000e] WARN: Could not read MAC from NVM\n");
                // Leave as all-zeros; DHCP will still work as long as the
                // smoltcp stack is given a valid EthernetAddress.
            }
        }

        // 5. General device configuration: set SLU (Set Link Up) and ASDE
        //    (Auto-Speed Detection).  Use a read-modify-write so that
        //    NVM-auto-loaded fields (e.g. GIO-master-disable, reserved bits)
        //    are preserved.  CTRL.RST must already be 0 here (we polled
        //    above).  CTRL.FD is ignored when ASDE=1 (speed/duplex come
        //    from PHY auto-negotiation), but we set it anyway so the MAC
        //    defaults to full-duplex if ASDE is ever cleared.
        {
            let mut ctrl = self.read32(REG_CTRL);
            ctrl |= CTRL_SLU | CTRL_ASDE | CTRL_FD;
            if self.is_pch() {
                // Enable Flow Control bits in CTRL for PCH controllers
                ctrl |= CTRL_RFCE | CTRL_TFCE;
            }
            self.write32(REG_CTRL, ctrl);
        }

        // 5b. Disable Intel Management Engine (ME) filters.
        //     The MANC (Management Control) register controls filtering logic that
        //     can intercept DHCP (UDP 68), ARP, and Neighbor Discovery traffic
        //     even when DRV_LOAD is set. Disabling these filters ensures all
        //     traffic reaches the host OS.
        if self.is_pch() {
            let manc = self.read32(REG_MANC);
            // Bits 13, 14, 15: ARP, DHCP, and Neighbor Discovery filtering
            // Bits 20, 21: IPv4 and IPv6 filtering redirection
            self.write32(REG_MANC, manc & !((1 << 13) | (1 << 14) | (1 << 15) | (1 << 20) | (1 << 21)));
            serial::serial_print("[e1000e] MANC filters disabled (Intel ME bypass)\n");
        }

        // 5a. Disable Energy Efficient Ethernet (EEE) on PCH-family NICs.
        //     EEE can cause link drops and latency issues that break DHCP on
        //     certain I219-V hardware revisions.
        if self.is_pch() {
            let ipcnfg = self.read32(REG_IPCNFG);
            serial::serial_print("[e1000e] Applying IPCNFG DMA quirk (bit 3) and disabling EEE (bit 0)\n");
            // Set bit 3 for DMA timing stability, clear bit 0 for EEE disable
            self.write32(REG_IPCNFG, (ipcnfg | (1 << 3)) & !(1 << 0));

            let eeer = self.read32(REG_EEER);
            if eeer & (1 << 0 | 1 << 1) != 0 {
                serial::serial_print("[e1000e] Disabling EEE in EEER (bits 0, 1)\n");
                self.write32(REG_EEER, eeer & !(1 << 0 | 1 << 1));
            }
        }

        // 6. Zero the Multicast Table Array (MTA) — 128 × 32-bit entries
        for i in 0..128u32 {
            self.write32(REG_MTA + i * 4, 0);
        }

        // 7. Initialise the RX descriptor ring
        let rx_ring_bytes = RX_RING_SIZE * core::mem::size_of::<RxDesc>();
        // Intel RDBAL must be 128-byte aligned.
        let (rx_desc_ptr, rx_desc_phys) = match memory::alloc_dma_buffer(rx_ring_bytes, 128) {
            Some(p) => p,
            None => {
                serial::serial_print("[e1000e] ERROR: RX descriptor ring alloc failed\n");
                return false;
            }
        };
        core::ptr::write_bytes(rx_desc_ptr, 0, rx_ring_bytes);
        self.rx_descs_virt = rx_desc_ptr as u64;
        self.rx_descs_phys = rx_desc_phys;

        for i in 0..RX_RING_SIZE {
            let (buf_ptr, buf_phys) = match memory::alloc_dma_buffer(PACKET_BUF_SIZE, 16) {
                Some(p) => p,
                None => {
                    serial::serial_print("[e1000e] ERROR: RX buffer alloc failed\n");
                    return false;
                }
            };
            self.rx_bufs[i] = (buf_ptr as u64, buf_phys);

            let desc = (self.rx_descs_virt as *mut RxDesc).add(i);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
            // Flush initial descriptor state
            self.clflush(desc as u64);
        }

        // Programme the ring registers
        self.write32(REG_RDBAL, rx_desc_phys as u32);
        self.write32(REG_RDBAH, (rx_desc_phys >> 32) as u32);
        self.write32(REG_RDLEN, rx_ring_bytes as u32);
        self.write32(REG_RDH, 0);
        // Give all descriptors to hardware except the last slot (ring-full sentinel)
        self.write32(REG_RDT, (RX_RING_SIZE - 1) as u32);
        self.rx_tail = 0;

        // Disable IP/TCP/UDP checksum offload.  On I219-V the NVM may leave
        // RXCSUM non-zero, which causes the hardware to set TCPE (bit 5) and
        // IPE (bit 6) in the descriptor errors byte for certain frames.  Our
        // driver treats any non-zero errors byte as a discard signal, so
        // DHCP OFFER packets can be silently dropped.  Clearing RXCSUM
        // prevents the hardware from reporting those checksum results.
        self.write32(REG_RXCSUM, 0);

        // 7b. I219-V Quirk: Force Legacy descriptors and disable advanced offloads.
        //     UEFI/UEFI-PXE firmware often leaves the NIC in "Extended Descriptor"
        //     mode (32 bytes). Resetting the MAC (CTRL_RST) does not always clear this.
        //     Forcing legacy mode ensures the NIC expects our 16-byte RxDesc structs.
        //     We also clear IPv6 and NFS offload bits that can interfere with receive.
        if self.is_pch() {
            let rfctl = self.read32(REG_RFCTL);
            // Bit 15: Extended descriptors
            // Bit 14: IPv6 Xsum offload
            // Bits 13-12: NFS write/read filtering
            self.write32(REG_RFCTL, rfctl & !((1 << 15) | (1 << 14) | (1 << 13) | (1 << 12)));
            serial::serial_print("[e1000e] Forcing Legacy descriptors (RFCTL logic adjusted)\n");
        }

        // Enable RX: unicast+broadcast+multicast accept, strip CRC, 2 KiB buffers.
        // RCTL_UPE (Unicast Promiscuous) is included to ensure that unicast DHCP
        // OFFERs from DHCP servers that do not honor the BROADCAST flag are
        // accepted even if there is any transient issue with the RAR[0] filter.
        self.write32(REG_RCTL, RCTL_EN | RCTL_UPE | RCTL_BAM | RCTL_MPE | RCTL_SECRC);

        // 8. Initialise the TX descriptor ring
        let tx_ring_bytes = TX_RING_SIZE * core::mem::size_of::<TxDesc>();
        // Intel TDBAL must be 128-byte aligned.
        let (tx_desc_ptr, tx_desc_phys) = match memory::alloc_dma_buffer(tx_ring_bytes, 128) {
            Some(p) => p,
            None => {
                serial::serial_print("[e1000e] ERROR: TX descriptor ring alloc failed\n");
                return false;
            }
        };
        core::ptr::write_bytes(tx_desc_ptr, 0, tx_ring_bytes);
        self.tx_descs_virt = tx_desc_ptr as u64;
        self.tx_descs_phys = tx_desc_phys;

        for i in 0..TX_RING_SIZE {
            let (buf_ptr, buf_phys) = match memory::alloc_dma_buffer(PACKET_BUF_SIZE, 16) {
                Some(p) => p,
                None => {
                    serial::serial_print("[e1000e] ERROR: TX buffer alloc failed\n");
                    return false;
                }
            };
            self.tx_bufs[i] = (buf_ptr as u64, buf_phys);

            let desc = (self.tx_descs_virt as *mut TxDesc).add(i);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            // Mark slot as done so software can use it immediately
            write_volatile(core::ptr::addr_of_mut!((*desc).status), TXD_STA_DD);
            // Flush initial descriptor state
            self.clflush(desc as u64);
        }

        self.write32(REG_TDBAL, tx_desc_phys as u32);
        self.write32(REG_TDBAH, (tx_desc_phys >> 32) as u32);
        self.write32(REG_TDLEN, tx_ring_bytes as u32);
        self.write32(REG_TDH, 0);
        self.write32(REG_TDT, 0);
        self.tx_tail = 0;

        // Enable TX: pad short frames, standard collision settings
        self.write32(REG_TCTL, TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD);

        // Standard inter-packet gap for 802.3 GbE
        self.write32(REG_TIPG, TIPG_IPGT_GBE);

        // 9. Programme RAR[0] with our MAC and set the Address Valid bit
        let ral = (self.mac[0] as u32)
            | ((self.mac[1] as u32) << 8)
            | ((self.mac[2] as u32) << 16)
            | ((self.mac[3] as u32) << 24);
        let rah = (self.mac[4] as u32)
            | ((self.mac[5] as u32) << 8)
            | (1u32 << 31); // AV (Address Valid) bit
        self.write32(REG_RAL0, ral);
        self.write32(REG_RAH0, rah);

        // 10. Configure IEEE 802.3x flow control.
        //     Programme the standard PAUSE-frame multicast destination address
        //     and EtherType so the hardware can recognise incoming PAUSE frames
        //     and generate outgoing PAUSE frames when the RX FIFO is filling up.
        //     This is symmetric flow control (both TX and RX PAUSE enabled).
        self.write32(REG_FCAL,  FLOW_CTRL_ADDR_LO);
        self.write32(REG_FCAH,  FLOW_CTRL_ADDR_HI);
        self.write32(REG_FCT,   FLOW_CTRL_TYPE);
        self.write32(REG_FCTTV, FCTTV_DEFAULT);
        
        if self.is_pch() {
            // I219-V specific watermarks for 4 KB RX FIFO.
            // FCRTH (High Water Mark) = 0x1000 (4 KB) - 0x600 = 0xA00
            // FCRTL (Low Water Mark)  = 0x1000 (4 KB) - 0x800 = 0x800
            self.write32(REG_FCRTH, 0x0A00);
            self.write32(REG_FCRTL, 0x0800);
        } else {
            self.write32(REG_FCRTH, FCRTH_DEFAULT);
            self.write32(REG_FCRTL, FCRTL_DEFAULT);
        }

        // 11. Set up interrupt coalescing timers.
        //     Even though this driver operates in polling mode (no interrupt
        //     handler is registered), setting ITR/RDTR/RADV to reasonable
        //     values matches Linux e1000e behaviour and prevents the hardware
        //     from asserting the interrupt line unnecessarily on shared IRQs.
        //     ITR = 0 disables interrupt throttling; RDTR/RADV set a short
        //     receive delay to reduce burst latency without excess interrupt rate.
        self.write32(REG_ITR,  0);          // No interrupt throttling
        self.write32(REG_RDTR, 0);          // No RX interrupt delay timer
        self.write32(REG_RADV, 0);          // No RX absolute interrupt delay timer
        self.write32(REG_TIDV, 0);          // No TX interrupt delay
        self.write32(REG_TADV, 0);          // No TX absolute delay timer

        // 12. Wait for the PHY to finish auto-negotiation (link-up), up to ~2 s.
        //     On real hardware the I217/I218/I219 PHY can take 1–3 seconds to
        //     establish a GbE link after powering up.  Without this wait, DHCP
        //     DISCOVERs sent before link-up fill the 32-slot TX ring with frames
        //     the hardware cannot yet transmit; by the time the link does come up
        //     smoltcp's retry state may be confused.  A short active poll here
        //     ensures the network service can start DHCP on a live link.
        //
        //     NOTE: This is a spin-poll because the scheduler is not yet running
        //     at driver-init time (sleep_ms is unavailable).  The same pattern is
        //     used by the MDIC and PHY power-up waits earlier in this function.
        //     Actual elapsed time varies with CPU speed and SMT state; the inner
        //     loop provides "enough" delay between STATUS reads without burning
        //     through all 32 TX slots as link comes up.
        serial::serial_print("[e1000e] Waiting for link (up to ~5s)...\n");
        let mut link_up = false;
        // Poll STATUS_LU every ~50 000 PAUSE iterations (varies by CPU speed).
        // 10 000 outer iterations gives ample time for GbE auto-negotiation;
        // on real hardware (I217/I218/I219) auto-negotiation can take up to
        // 3–5 seconds, especially on X299/Z370 platforms.
        for _ in 0..10_000u32 {
            if self.read32(REG_STATUS) & STATUS_LU != 0 {
                link_up = true;
                break;
            }
            for _ in 0..50_000u32 { core::hint::spin_loop(); }
        }
        if link_up {
            serial::serial_print("[e1000e] Link UP\n");
            // PHY settling delay after STATUS_LU asserts: the I219-V PHY
            // can still be completing auto-negotiation state transitions.
            // A longer delay (≈ 5 ms @ 3 GHz) prevents transient receive
            // errors in the very first frames from causing early DHCP
            // OFFERs to be silently discarded, and also gives the Intel ME
            // additional time to complete its DHCP traffic hand-off after
            // the DRV_LOAD signal set above.
            // 500_000 × PAUSE ≈ 5 ms at 3 GHz (PAUSE ≈ 10 ns).
            for _ in 0..500_000u32 { core::hint::spin_loop(); }
        } else {
            serial::serial_print("[e1000e] Link not yet up after timeout (proceeding anyway)\n");
        }

        // 13. Detect and record the current link state (speed, duplex).
        self.detect_link_state();
        if self.link_up {
            serial::serial_print("[e1000e] Speed: ");
            serial::serial_print_dec(self.link_speed as u64);
            serial::serial_print(" Mb/s, ");
            serial::serial_print(if self.full_duplex { "full-duplex\n" } else { "half-duplex\n" });
        }

        // 14. Clear all hardware statistics registers by reading them once.
        //     Statistics registers are read-on-clear, so this establishes a
        //     clean zero baseline for subsequent update_hw_stats() calls.
        self.update_hw_stats();
        // Reset the software counters too (update_hw_stats accumulated
        // uninitialised or stale NVM values in the first read above).
        self.rx_packets = 0;
        self.tx_packets = 0;
        self.rx_bytes   = 0;
        self.tx_bytes   = 0;
        self.rx_errors  = 0;
        self.tx_errors  = 0;

        true
    }

    /// Transmit a single Ethernet frame.
    unsafe fn send_packet(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() > 1514 {
            self.tx_errors += 1;
            return Err("e1000e: packet exceeds MTU");
        }

        let slot = self.tx_tail;
        let desc = (self.tx_descs_virt as *mut TxDesc).add(slot);

        // Invalidate (flush) descriptor before checking status bit to ensure we see hardware updates
        self.clflush(desc as u64);

        // Descriptor must be free (DD bit set by hardware after transmission)
        let sta = read_volatile(core::ptr::addr_of!((*desc).status));
        if sta & TXD_STA_DD == 0 {
            self.tx_errors += 1;
            return Err("e1000e: TX ring full");
        }

        // Copy frame into the pre-allocated DMA buffer for this slot
        let buf_virt = self.tx_bufs[slot].0 as *mut u8;
        let buf_phys = self.tx_bufs[slot].1;
        core::ptr::copy_nonoverlapping(data.as_ptr(), buf_virt, data.len());

        // PARCHE: Manual padding to 60 bytes (Ethernet minimum without FCS)
        // Some I219-V revisions silenty fail to transmit DHCP discovers if they
        // are shorter than 60 bytes, even when TCTL.PSP is set.
        let mut frame_len = data.len();
        if frame_len < 60 {
            core::ptr::write_bytes(buf_virt.add(frame_len), 0, 60 - frame_len);
            frame_len = 60;
        }

        // Flush the packet data to physical RAM
        self.flush_cache_range(buf_virt as u64, frame_len);

        // Fill the descriptor
        write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
        write_volatile(core::ptr::addr_of_mut!((*desc).length), frame_len as u16);
        write_volatile(core::ptr::addr_of_mut!((*desc).cso), 0);
        write_volatile(
            core::ptr::addr_of_mut!((*desc).cmd),
            TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS,
        );
        write_volatile(core::ptr::addr_of_mut!((*desc).status), 0); // Clear DD
        write_volatile(core::ptr::addr_of_mut!((*desc).css), 0);
        write_volatile(core::ptr::addr_of_mut!((*desc).special), 0);

        // Flush the descriptor itself to ensure the hardware sees our updates
        self.clflush(desc as u64);

        // Hardware memory fence to ensure descriptor and buffer data are
        // physically committed to RAM before the hardware sees the doorbell (TDT).
        // On x86-64, Release fence ensures previous stores are visible.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);

        // Advance the tail and ring the doorbell
        self.tx_tail = (slot + 1) % TX_RING_SIZE;
        self.write32(REG_TDT, self.tx_tail as u32);

        // Update software statistics
        self.tx_packets += 1;
        self.tx_bytes   += data.len() as u64;

        Ok(())
    }

    /// Receive one Ethernet frame into `buffer`.  Returns the frame length or
    /// `None` if no frame is currently available.
    unsafe fn receive_packet(&mut self, buffer: &mut [u8]) -> Option<usize> {
        let slot = self.rx_tail;
        let desc = (self.rx_descs_virt as *mut RxDesc).add(slot);

        // Invalidate cache for this descriptor so we see hardware updates (DD bit)
        self.clflush(desc as u64);

        let status = read_volatile(core::ptr::addr_of!((*desc).status));
        if status & RXD_STA_DD == 0 {
            return None; // Hardware has not written a frame here yet
        }

        // Hardware memory fence to ensure that the CPU reads the packet data
        // from RAM after the hardware has finished its DMA write.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        let buf_phys = self.rx_bufs[slot].1;

        // EOP (End of Packet) check.  With PACKET_BUF_SIZE = 2 KiB and
        // standard MTU = 1514 bytes every frame must fit in exactly one
        // descriptor, so EOP should always be set by hardware.
        //
        // However, several I217/I218/I219-V hardware revisions have an errata
        // where EOP is not set on valid, single-descriptor frames.  To avoid
        // silently dropping legitimate DHCP OFFERs on affected silicon, we
        // treat a descriptor with EOP=0 as complete when the written length
        // is ≤ PACKET_BUF_SIZE (i.e. the frame genuinely fits in one buffer
        // and no continuation descriptor is needed).  If the length were
        // exactly PACKET_BUF_SIZE it *could* be a truncated multi-descriptor
        // frame, so those are still discarded.
        if status & RXD_STA_EOP == 0 {
            let frame_len = read_volatile(core::ptr::addr_of!((*desc).length)) as usize;
            if frame_len >= PACKET_BUF_SIZE {
                // Genuine first-segment of a multi-descriptor (jumbo) frame:
                // discard and return the slot to hardware.
                write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
                write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
                self.write32(REG_RDT, slot as u32);
                self.rx_tail = (slot + 1) % RX_RING_SIZE;
                return None;
            }
            // EOP=0 but frame fits in one buffer: hardware errata, fall through
            // and process the frame as if EOP were set.
        }

        // Only discard frames with fatal receive errors (CRC, symbol, or DMA
        // errors — bits CE, SE, RXE in the errors byte).  Sequence errors (SEQ),
        // carrier extension errors, and checksum offload results (TCPE/IPE, which
        // are suppressed by writing RXCSUM = 0 during init) are transient or
        // informational and must not cause valid DHCP frames to be dropped.
        let errors = read_volatile(core::ptr::addr_of!((*desc).errors));
        if errors & RXD_ERR_FATAL != 0 {
            self.rx_errors += 1;
            serial::serial_print("[e1000e] fatal RX error: 0x");
            serial::serial_print_hex(errors as u64);
            serial::serial_print("\n");
            write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
            write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
            self.write32(REG_RDT, slot as u32);
            self.rx_tail = (slot + 1) % RX_RING_SIZE;
            return None; // Discard frame with fatal hardware error
        }

        let frame_len = read_volatile(core::ptr::addr_of!((*desc).length)) as usize;
        let copy_len  = core::cmp::min(frame_len, buffer.len());

        let src = self.rx_bufs[slot].0 as *const u8;
        
        // Invalidate cache for receive buffer before reading packet data
        self.flush_cache_range(src as u64, frame_len);

        core::ptr::copy_nonoverlapping(src, buffer.as_mut_ptr(), copy_len);

        // Return the descriptor to hardware: clear status, restore buffer addr,
        // and advance RDT so hardware knows it can reuse this slot.
        write_volatile(core::ptr::addr_of_mut!((*desc).status), 0);
        write_volatile(core::ptr::addr_of_mut!((*desc).buffer_addr), buf_phys);
        self.write32(REG_RDT, slot as u32);

        self.rx_tail = (slot + 1) % RX_RING_SIZE;

        // Update software statistics
        self.rx_packets += 1;
        self.rx_bytes   += copy_len as u64;

        Some(copy_len)
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Public device methods (called via the NetworkDevice trait in eth.rs)
// ───────────────────────────────────────────────────────────────────────────
impl E1000EDevice {
    pub fn get_mac_address(&self) -> [u8; 6] {
        self.inner.lock().mac
    }

    pub fn send_packet(&self, data: &[u8]) -> Result<(), &'static str> {
        let mut inner = self.inner.lock();
        unsafe { inner.send_packet(data) }
    }

    pub fn receive_packet(&self, buffer: &mut [u8]) -> Option<usize> {
        let mut inner = self.inner.lock();
        unsafe { inner.receive_packet(buffer) }
    }

    /// Return the current link state: `(link_up, speed_mbps, full_duplex)`.
    ///
    /// The speed is one of 10, 100, or 1000 (Mb/s), or 0 when the link is
    /// down.  The values are re-read from the STATUS register every call so
    /// they reflect the current hardware state after auto-negotiation.
    pub fn get_link_state(&self) -> (bool, u32, bool) {
        let mut inner = self.inner.lock();
        inner.detect_link_state();
        (inner.link_up, inner.link_speed, inner.full_duplex)
    }

    /// Return cumulative packet and byte counters.
    ///
    /// The tuple is `(rx_packets, tx_packets, rx_bytes, tx_bytes,
    /// rx_errors, tx_errors)`.  Hardware statistics registers (which are
    /// read-on-clear) are accumulated into the software counters each call,
    /// so repeated calls return monotonically increasing totals.
    pub fn get_stats(&self) -> (u64, u64, u64, u64, u64, u64) {
        let mut inner = self.inner.lock();
        inner.update_hw_stats();
        (
            inner.rx_packets,
            inner.tx_packets,
            inner.rx_bytes,
            inner.tx_bytes,
            inner.rx_errors,
            inner.tx_errors,
        )
    }
}

// ───────────────────────────────────────────────────────────────────────────
// PCI initialisation — scans all devices and registers found NICs
// ───────────────────────────────────────────────────────────────────────────

/// Returns `true` if `device_id` belongs to the e1000e PCH Ethernet family.
fn is_e1000e_device(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == INTEL_VENDOR_ID && E1000E_DEVICE_IDS.contains(&device_id)
}

/// Initialise all Intel e1000e-family Ethernet controllers found on the PCI bus.
/// Each successfully initialised device is registered with the global
/// `eth::NET_DEVICE_REGISTRY` so it is visible as `eth:0`, `eth:1`, …
pub fn init() {
    serial::serial_print("[e1000e] Scanning PCI for Intel Ethernet controllers...\n");

    for dev in crate::pci::get_all_devices() {
        if !is_e1000e_device(dev.vendor_id, dev.device_id) {
            continue;
        }

        serial::serial_print("[e1000e] Found Intel Ethernet: device_id=0x");
        serial::serial_print_hex(dev.device_id as u64);
        serial::serial_print(" Bus=");
        serial::serial_print_dec(dev.bus as u64);
        serial::serial_print(" Dev=");
        serial::serial_print_dec(dev.device as u64);
        serial::serial_print("\n");

        unsafe {
            // Enable memory-space decoding and bus-mastering DMA
            crate::pci::enable_device(&dev, true);

            // BAR0 is the 128 KiB memory-mapped register space
            let bar0_phys = crate::pci::get_bar(&dev, 0);
            if bar0_phys == 0 {
                serial::serial_print("[e1000e] ERROR: BAR0 is zero, skipping device\n");
                continue;
            }

            // Map the MMIO region (128 KiB = 0x20000 bytes).
            // Per the memory module note: extend length by the page-offset of
            // the physical address to avoid under-mapping across a page boundary.
            let page_offset = (bar0_phys & 0xFFF) as usize;
            let mmio_virt = crate::memory::map_mmio_range(bar0_phys, 0x2_0000 + page_offset);
            if mmio_virt == 0 {
                serial::serial_print("[e1000e] ERROR: MMIO mapping failed, skipping device\n");
                continue;
            }

            // mmio_base must include the page offset if map_mmio_range returns
            // the start of the mapped virtual page range.
            let mmio_base = mmio_virt + page_offset as u64;

            let inner = E1000EInner {
                mmio_base,
                device_id: dev.device_id,
                bus: dev.bus,
                dev: dev.device,
                func: dev.function,
                mac: [0u8; 6],
                phy_addr: 1, // Default, will be updated during init()
                rx_descs_virt: 0,
                rx_descs_phys: 0,
                rx_bufs: [(0, 0); RX_RING_SIZE],
                rx_tail: 0,
                tx_descs_virt: 0,
                tx_descs_phys: 0,
                tx_bufs: [(0, 0); TX_RING_SIZE],
                tx_tail: 0,
                link_up:     false,
                link_speed:  0,
                full_duplex: false,
                rx_packets:  0,
                tx_packets:  0,
                rx_bytes:    0,
                tx_bytes:    0,
                rx_errors:   0,
                tx_errors:   0,
            };

            let device = E1000EDevice { inner: Mutex::new(inner) };

            // Run hardware initialisation
            if !device.inner.lock().init() {
                serial::serial_print("[e1000e] Hardware init failed, skipping device\n");
                continue;
            }

            let mac = device.get_mac_address();
            serial::serial_print("[e1000e] Initialized. MAC: ");
            for i in 0..6 {
                serial::serial_print_hex(mac[i] as u64);
                if i < 5 { serial::serial_print(":"); }
            }
            serial::serial_print("\n");

            let arc_dev: Arc<E1000EDevice> = Arc::new(device);
            E1000E_DEVICES.lock().push(arc_dev.clone());

            // Register with the eth scheme's unified device registry so the
            // network service can open it as eth:N
            crate::eth::eth_register_device(arc_dev);
        }
    }
}
