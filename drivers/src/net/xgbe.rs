//! AMD XGMAC 10GbE (`amd-xgbe`): PCI identity, resource layout and the
//! read-only part of bring-up.
//!
//! This is the other AMD Ethernet controller on x86_64, the 10 Gb one found
//! on AMD APUs and embedded parts. Linux drives it as `amd-xgbe`
//! (`drivers/net/ethernet/amd/xgbe/`).
//!
//! # What this module does, and what it deliberately does not
//!
//! **No part of this has run on the hardware.** QEMU does not emulate an
//! XGMAC, so unlike `pcnet32` next door there is no way to boot it here
//! either. What that leaves is the part that can be got right from the
//! specification and checked without a chip: the PCI identity, which BAR is
//! which, where the sub-blocks sit inside BAR0, how the PCS indirect window
//! is decoded, and how the station address and port properties are read.
//!
//! So the probe **identifies the device, reads and logs everything it can
//! read, and then declines**. It does not register a `NetScheme`. A driver
//! that claims an interface it cannot service is worse than no driver: the
//! kernel would hand it packets it silently drops, and `ifconfig` would show
//! a link that is not there. Declining leaves the device exactly as unclaimed
//! as it is today, and the log is the input the datapath batch needs --- it
//! prints the real channel, queue and FIFO geometry of whatever card is in
//! the machine, which is what a blind implementation would otherwise have to
//! guess.
//!
//! # Where these numbers come from
//!
//! Every offset, field position and identifier below was taken from the
//! `amd-xgbe` source itself (`xgbe-common.h`, `xgbe.h`, `xgbe-pci.c`), not
//! from memory, and each constant carries the Linux name it corresponds to.
//! The tests pin every one of them, so a later edit cannot quietly move a
//! register. Two kernels were read: v6.12 and master --- `0x1641` and the V3
//! PCS window only exist in the newer one.

use alloc::sync::Arc;

use pci::{PCIDevice, BAR};

use crate::builder::IoMapper;
use crate::bus::pci_drivers::PciDriver;
use crate::{Device, DeviceError, DeviceResult};

// ——— PCI identity ———
//
// `xgbe_pci_table` in `xgbe-pci.c`.

/// AMD's PCI vendor id.
pub const PCI_VENDOR_AMD: u16 = 0x1022;

/// Which generation of the controller a device id names.
///
/// Linux calls these `xgbe_v2a`, `xgbe_v2b` and `xgbe_v3`. They differ in the
/// FIFO ceiling and in how the PCS is reached, and the FIFO ceiling is a
/// *clamp* on what the port property register reports, so it cannot be left
/// out even here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Variant {
    /// `xgbe_v2a`: 224 KiB of FIFO each way.
    V2a,
    /// `xgbe_v2b`: 64 KiB each way.
    V2b,
    /// `xgbe_v3`: 64 KiB each way, and the PCS is reached through the SMN
    /// rather than the BAR1 window.
    V3,
}

impl Variant {
    /// `tx_max_fifo_size` / `rx_max_fifo_size` in the version data. The same
    /// value for both directions on every variant so far.
    #[inline]
    pub const fn max_fifo_size(self) -> u32 {
        match self {
            Variant::V2a => 229_376,
            Variant::V2b | Variant::V3 => 65_536,
        }
    }

    /// Is the PCS reached through the System Management Network rather than
    /// the window in BAR1? (`XGBE_XPCS_ACCESS_V3`.)
    #[inline]
    pub const fn pcs_through_smn(self) -> bool {
        matches!(self, Variant::V3)
    }
}

/// The controller generation for `device_id`, or `None` if AMD's vendor id is
/// on something else entirely --- which it usually is, since the host
/// bridges, the IOMMU and the GPU all share it.
#[inline]
pub const fn variant_for(device_id: u16) -> Option<Variant> {
    match device_id {
        0x1458 => Some(Variant::V2a),
        0x1459 => Some(Variant::V2b),
        // Only in kernels newer than v6.12.
        0x1641 => Some(Variant::V3),
        _ => None,
    }
}

/// Does this driver handle `(vendor, device)`?
#[inline]
pub const fn xgbe_matched(vendor_id: u16, device_id: u16) -> bool {
    vendor_id == PCI_VENDOR_AMD && variant_for(device_id).is_some()
}

// ——— Resource layout ———
//
// `XGBE_XGMAC_BAR` and friends in `xgbe.h`.

/// BAR holding the XGMAC register block.
pub const XGMAC_BAR: usize = 0;
/// BAR holding the XPCS register block.
pub const XPCS_BAR: usize = 1;

/// Offset of the port property block inside the XGMAC BAR
/// (`XGBE_MAC_PROP_OFFSET`). These registers are not a separate BAR; reading
/// them from the start of BAR0 lands in the MAC's own registers instead.
pub const MAC_PROP_OFFSET: usize = 0x1d000;
/// Offset of the I2C controller inside the XGMAC BAR
/// (`XGBE_I2C_CTRL_OFFSET`). The SFP+ cage hangs off it.
pub const I2C_CTRL_OFFSET: usize = 0x1e000;

/// The smallest BAR0 this driver will accept: the port property block must be
/// inside it, or every read below is off the end of the mapping.
pub const MIN_XGMAC_BAR_LEN: usize = I2C_CTRL_OFFSET + 0x1000;

// ——— Port property registers (the `XP_` block) ———

/// `XP_PROP_0`: port id, mode and supported speeds.
pub const XP_PROP_0: usize = 0x0000;
/// `XP_PROP_1`: the maximum DMA channel and hardware queue counts.
pub const XP_PROP_1: usize = 0x0004;
/// `XP_PROP_2`: the FIFO sizes, in units of 16 KiB.
pub const XP_PROP_2: usize = 0x0008;
/// `XP_PROP_3`: the SFP+ GPIO map and the MDIO reset method.
pub const XP_PROP_3: usize = 0x000c;
/// `XP_PROP_4`: further board properties.
pub const XP_PROP_4: usize = 0x0010;
/// `XP_MAC_ADDR_LO`: the low four bytes of the station address.
pub const XP_MAC_ADDR_LO: usize = 0x0020;
/// `XP_MAC_ADDR_HI`: the high two bytes, plus the VALID bit.
pub const XP_MAC_ADDR_HI: usize = 0x0024;
/// `XP_INT_EN`: the interrupt enable mask for the whole block.
pub const XP_INT_EN: usize = 0x0078;

// ——— The PCS indirect window ———
//
// The XPCS registers are not all mapped at once: a window into them is
// selected through one register and read through another, and WHICH pair of
// registers that is depends on the HOST BRIDGE, not on the NIC. Getting this
// from the NIC's own device id --- the obvious guess --- lands on the wrong
// registers on every Raven, Yellow Carp, Renoir and P100a part.

/// `PCS_V2_WINDOW_DEF` / `PCS_V2_WINDOW_SELECT`: the default pair.
pub const PCS_V2_WINDOW: (usize, usize) = (0x9060, 0x9064);
/// `PCS_V2_RV_WINDOW_*`: Raven.
pub const PCS_V2_RV_WINDOW: (usize, usize) = (0x1060, 0x1064);
/// `PCS_V2_YC_WINDOW_*`: Yellow Carp.
pub const PCS_V2_YC_WINDOW: (usize, usize) = (0x18060, 0x18064);
/// `PCS_V3_RN_WINDOW_*`: Renoir.
pub const PCS_V3_RN_WINDOW: (usize, usize) = (0xf8078, 0xf807c);
/// `PCS_P100A_WINDOW_*`. Note the select is NOT four bytes past the def here,
/// which is why these are carried as pairs rather than a base and a stride.
pub const PCS_P100A_WINDOW: (usize, usize) = (0x8060, 0x8080);

/// Host bridge device ids that change the window pair (`xgbe.h`).
pub const BRIDGE_RV: u16 = 0x15d0;
pub const BRIDGE_YC: u16 = 0x14b5;
pub const BRIDGE_RN: u16 = 0x1630;
pub const BRIDGE_P100A: u16 = 0x1122;

/// `PCS_RN_SMN_BASE_ADDR` and `PCS_RN_PORT_ADDR_SIZE`: on V3 the PCS is
/// reached over the System Management Network, at a base that moves with the
/// port id.
pub const PCS_RN_SMN_BASE_ADDR: u32 = 0x11e0_0000;
pub const PCS_RN_PORT_ADDR_SIZE: u32 = 0x0010_0000;

/// The `(window_def, window_select)` register pair to use, given the device
/// at 0:0.0.
///
/// A host bridge that is not AMD's, or an AMD one not in the table, takes the
/// default pair --- the same fallback Linux uses.
#[inline]
pub const fn pcs_window_regs(bridge_vendor: u16, bridge_device: u16) -> (usize, usize) {
    if bridge_vendor != PCI_VENDOR_AMD {
        return PCS_V2_WINDOW;
    }
    match bridge_device {
        BRIDGE_P100A => PCS_P100A_WINDOW,
        BRIDGE_RV => PCS_V2_RV_WINDOW,
        BRIDGE_YC => PCS_V2_YC_WINDOW,
        BRIDGE_RN => PCS_V3_RN_WINDOW,
        _ => PCS_V2_WINDOW,
    }
}

/// The SMN address of a V3 part's window definition register.
#[inline]
pub const fn smn_window_address(port_id: u32, window_def_reg: usize) -> u32 {
    PCS_RN_SMN_BASE_ADDR
        .wrapping_add(PCS_RN_PORT_ADDR_SIZE.wrapping_mul(port_id))
        .wrapping_add(window_def_reg as u32)
}

/// The decoded PCS window: where it starts, how big it is, and the mask that
/// turns a PCS register address into an offset inside it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PcsWindow {
    pub base: u32,
    pub size: u32,
    pub mask: u32,
}

/// Decode `PCS_V2_WINDOW_DEF`.
///
/// Both fields are stored scaled, and the scaling is the part that is easy to
/// get wrong: the offset is in 64-byte units (bits 19..6, shifted back up by
/// 6) and the size is a log2 exponent biased by 7, so a stored 0 means 128
/// bytes and not 1. A window decoded one bit out does not fail --- it reads
/// the wrong PCS registers and reports a link state belonging to nothing.
#[inline]
pub const fn decode_pcs_window(reg: u32) -> PcsWindow {
    // `PCS_V2_WINDOW_DEF_OFFSET` is at bit 6, 14 bits wide.
    let base = ((reg >> 6) & 0x3fff) << 6;
    // `PCS_V2_WINDOW_DEF_SIZE` is at bit 2, 4 bits wide.
    let size = 1u32 << (((reg >> 2) & 0xf) + 7);
    PcsWindow {
        base,
        size,
        mask: size - 1,
    }
}

// ——— Station address ———

/// The station address from `XP_MAC_ADDR_LO` / `XP_MAC_ADDR_HI`, or `None`
/// when the board has none to give.
///
/// Three ways it can be absent, and all three have to be caught here because
/// none of them fails loudly later: the VALID bit (bit 31 of the high word)
/// is clear, the address is all zeroes, or the group bit is set. A multicast
/// address used as a station address makes every switch on the segment flood
/// this port's traffic and nothing ever answers it.
#[inline]
pub fn mac_from_prop(lo: u32, hi: u32) -> Option<[u8; 6]> {
    // `XP_MAC_ADDR_HI_VALID` is bit 31.
    if hi & (1 << 31) == 0 {
        return None;
    }
    let mac = [
        (lo & 0xff) as u8,
        ((lo >> 8) & 0xff) as u8,
        ((lo >> 16) & 0xff) as u8,
        ((lo >> 24) & 0xff) as u8,
        (hi & 0xff) as u8,
        ((hi >> 8) & 0xff) as u8,
    ];
    if mac == [0; 6] || mac[0] & 1 == 1 {
        return None;
    }
    Some(mac)
}

// ——— Port geometry ———

/// How many DMA channels and hardware queues the port has, from `XP_PROP_1`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Counts {
    pub tx_channels: u32,
    pub rx_channels: u32,
    pub tx_queues: u32,
    pub rx_queues: u32,
}

/// Decode `XP_PROP_1`. Each field is five bits, and they are NOT in the order
/// the name suggests: transmit queues sit at the bottom, receive queues at 8,
/// transmit channels at 16 and receive channels at 24.
#[inline]
pub const fn decode_counts(pp1: u32) -> Counts {
    Counts {
        tx_queues: pp1 & 0x1f,
        rx_queues: (pp1 >> 8) & 0x1f,
        tx_channels: (pp1 >> 16) & 0x1f,
        rx_channels: (pp1 >> 24) & 0x1f,
    }
}

/// The usable FIFO sizes in bytes, from `XP_PROP_2` and the variant.
///
/// The register counts 16 KiB units, and the variant's ceiling is a clamp on
/// the result, not an alternative to it: a board may report more FIFO than
/// the controller generation can actually drive.
#[inline]
pub fn decode_fifo_sizes(pp2: u32, variant: Variant) -> (u32, u32) {
    let ceiling = variant.max_fifo_size();
    let tx = (pp2 & 0xffff).saturating_mul(16_384).min(ceiling);
    let rx = ((pp2 >> 16) & 0xffff).saturating_mul(16_384).min(ceiling);
    (tx, rx)
}

/// The port id from `XP_PROP_0`: which of the controller's ports this is.
#[inline]
pub const fn port_id(pp0: u32) -> u32 {
    pp0 & 0xff
}

/// The port mode from `XP_PROP_0`: what is on the other side of the MAC
/// (a backplane, an SFP+ cage, a BASE-T PHY...).
#[inline]
pub const fn port_mode(pp0: u32) -> u32 {
    (pp0 >> 8) & 0xf
}

/// The speeds the port supports, from `XP_PROP_0`, as a bitmap.
#[inline]
pub const fn port_speeds(pp0: u32) -> u32 {
    (pp0 >> 22) & 0x1f
}

// ——— What the MAC says about itself ———
//
// The port property block read above is the *board's* description of the
// port, written by firmware. These four registers are the *controller's*
// description of itself, hardwired at synthesis time: which version of the
// Synopsys XGMAC core this is, and which of its optional blocks were built
// in. Linux reads them in `xgbe_get_all_hw_features` (`xgbe-drv.c`) before it
// configures anything, and then branches on them for the rest of bring-up ---
// `xgbe-dev.c` alone gates on `SNPSVER` in six places, against `0x20`, `0x21`
// and `0x30`.
//
// They matter here for the same reason, one batch early: a datapath written
// without them has to assume a geometry (how many channels, how wide a DMA
// address, how big a FIFO to split between the queues) and assume a feature
// set (whether there is a checksum engine, whether TSO exists, whether the
// hash filter has a table at all). Each of those assumptions is a silent
// wrong answer on a chip that reports otherwise. Read and logged now, they
// are a fact about the card in Moebius's machine instead.
//
// All four are read-only, and reading them changes nothing: this batch adds
// no write to the device.

/// `MAC_VR`: the core's version triple.
pub const MAC_VR: usize = 0x0110;
/// `MAC_HWF0R`: optional-feature register 0 --- the offload and filter blocks.
pub const MAC_HWF0R: usize = 0x011c;
/// `MAC_HWF1R`: optional-feature register 1 --- FIFO sizes, DMA width, TSO/RSS.
pub const MAC_HWF1R: usize = 0x0120;
/// `MAC_HWF2R`: optional-feature register 2 --- the queue and channel counts.
pub const MAC_HWF2R: usize = 0x0124;

/// Linux's `XGMAC_GET_BITS`: `width` bits of `reg` starting at `index`.
///
/// Spelled out rather than hidden in a macro, because every call below quotes
/// the `_INDEX`/`_WIDTH` pair from `xgbe-common.h` as its two arguments, and
/// that is exactly what the tests check.
#[inline]
pub const fn get_bits(reg: u32, index: u32, width: u32) -> u32 {
    (reg >> index) & ((1u32 << width) - 1)
}

/// The three bytes of `MAC_VR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacVersion {
    /// `MAC_VR_SNPSVER`: the Synopsys core revision. This is the one Linux
    /// branches on; `0x30` and above is the generation that needs the newer
    /// sequences, below `0x21` the older ones.
    pub snps: u8,
    /// `MAC_VR_DEVID`: the device identity byte.
    pub dev_id: u8,
    /// `MAC_VR_USERVER`: the integrator's own revision, free for AMD to set.
    pub user: u8,
}

/// Decode `MAC_VR`.
#[inline]
pub const fn decode_mac_version(mac_vr: u32) -> MacVersion {
    MacVersion {
        snps: get_bits(mac_vr, 0, 8) as u8,
        dev_id: get_bits(mac_vr, 8, 8) as u8,
        user: get_bits(mac_vr, 16, 8) as u8,
    }
}

/// Linux's `XGBE_MAX_PPS_OUT` (`xgbe.h`). A chip reporting more than this is
/// clamped rather than believed: the driver has storage for four.
pub const MAX_PPS_OUT: u32 = 4;
/// Linux's `XGBE_MAX_AUX_SNAP` (`xgbe.h`).
pub const MAX_AUX_SNAP: u32 = 4;

/// What the controller reports it was built with: Linux's
/// `struct xgbe_hw_features`, field for field and in its order, with the
/// same post-processing applied.
///
/// "Post-processing" is not cosmetic. Four of these fields are *not* the raw
/// register value: the counts are zero-based and get incremented, the hash
/// table size is an index into {0, 64, 128, 256}, the DMA width is an index
/// into {32, 40, 48} bits, and the FIFO sizes are a log2 biased by 7. A
/// datapath that read the raw bits would size its rings for one channel when
/// the chip has eight, and its FIFO split for 128 bytes when the chip has
/// 128 KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HwFeatures {
    // HWF0R: what blocks exist.
    /// `GMIISEL`: 1000 Mbps support.
    pub gmii: bool,
    /// `VLHASH`: VLAN hash filter.
    pub vlan_hash: bool,
    /// `SMASEL`: the SMA (MDIO) interface.
    pub sma: bool,
    /// `RWKSEL`: PMT remote wake-up packet.
    pub remote_wake: bool,
    /// `MGKSEL`: PMT magic packet.
    pub magic_packet: bool,
    /// `MMCSEL`: the RMON/MMC counter block.
    pub mmc: bool,
    /// `ARPOFFSEL`: ARP offload.
    pub arp_offload: bool,
    /// `TSSEL`: IEEE 1588-2008 advanced timestamping.
    pub timestamp: bool,
    /// `EEESEL`: Energy Efficient Ethernet.
    pub eee: bool,
    /// `TXCOESEL`: transmit checksum offload.
    pub tx_coe: bool,
    /// `RXCOESEL`: receive checksum offload.
    pub rx_coe: bool,
    /// `ADDMACADRSEL`: how many additional MAC addresses the filter holds.
    pub addn_mac: u32,
    /// `TSSTSSEL`: the timestamp source.
    pub ts_src: u32,
    /// `SAVLANINS`: source-address or VLAN insertion.
    pub sa_vlan_ins: bool,
    /// `VXN`: VXLAN/NVGRE awareness.
    pub vxn: bool,

    // HWF1R: sizes and the heavier offloads.
    /// `RXFIFOSIZE`, translated to bytes.
    pub rx_fifo_size: u32,
    /// `TXFIFOSIZE`, translated to bytes.
    pub tx_fifo_size: u32,
    /// `ADVTHWORD`: the advanced-timestamp high word.
    pub adv_ts_hi: bool,
    /// `ADDR64`, translated to a number of address bits (32, 40 or 48).
    pub dma_width: u32,
    /// `DCBEN`: the DCB block.
    pub dcb: bool,
    /// `SPHEN`: split-header receive.
    pub split_header: bool,
    /// `TSOEN`: TCP segmentation offload.
    pub tso: bool,
    /// `DBGMEMA`: the DMA debug registers.
    pub dma_debug: bool,
    /// `RSSEN`: receive-side scaling.
    pub rss: bool,
    /// `NUMTC`, incremented: the number of traffic classes.
    pub tc_count: u32,
    /// `HASHTBLSZ`, translated: 0, 64, 128 or 256 entries.
    pub hash_table_size: u32,
    /// `L3L4FNUM`: how many L3/L4 filters there are.
    pub l3l4_filters: u32,

    // HWF2R: the geometry.
    /// `RXQCNT`, incremented.
    pub rx_queues: u32,
    /// `TXQCNT`, incremented.
    pub tx_queues: u32,
    /// `RXCHCNT`, incremented.
    pub rx_channels: u32,
    /// `TXCHCNT`, incremented.
    pub tx_channels: u32,
    /// `PPSOUTNUM`, clamped to [`MAX_PPS_OUT`].
    pub pps_outputs: u32,
    /// `AUXSNAPNUM`, clamped to [`MAX_AUX_SNAP`].
    pub aux_snapshots: u32,
}

/// `1 << (field + 7)`: the FIFO-size translation of
/// `xgbe_get_all_hw_features`, saturating instead of overflowing.
///
/// A deliberate divergence from Linux, and the only one in this function. The
/// field is five bits wide, so a register can claim 2^38 bytes of FIFO; Linux
/// shifts into an `unsigned int` and wraps, which in Rust is a panic in a
/// debug build. No real part reports above 11 (256 KiB), and the caller caps
/// the result by the variant's ceiling in any case, so saturating is the
/// honest reading of a value that cannot be true.
#[inline]
const fn fifo_bytes(field: u32) -> u32 {
    match 1u32.checked_shl(field + 7) {
        Some(bytes) => bytes,
        None => u32::MAX,
    }
}

/// Decode the three feature registers, in the order and with the translations
/// of `xgbe_get_all_hw_features` (`xgbe-drv.c`).
pub fn decode_hw_features(hfr0: u32, hfr1: u32, hfr2: u32) -> HwFeatures {
    // Each `get_bits` argument pair below is the `_INDEX`/`_WIDTH` pair of
    // the same name in `xgbe-common.h`.
    let mut f = HwFeatures {
        gmii: get_bits(hfr0, 1, 1) != 0,
        vlan_hash: get_bits(hfr0, 4, 1) != 0,
        sma: get_bits(hfr0, 5, 1) != 0,
        remote_wake: get_bits(hfr0, 6, 1) != 0,
        magic_packet: get_bits(hfr0, 7, 1) != 0,
        mmc: get_bits(hfr0, 8, 1) != 0,
        arp_offload: get_bits(hfr0, 9, 1) != 0,
        timestamp: get_bits(hfr0, 12, 1) != 0,
        eee: get_bits(hfr0, 13, 1) != 0,
        tx_coe: get_bits(hfr0, 14, 1) != 0,
        rx_coe: get_bits(hfr0, 16, 1) != 0,
        addn_mac: get_bits(hfr0, 18, 5),
        ts_src: get_bits(hfr0, 25, 2),
        sa_vlan_ins: get_bits(hfr0, 27, 1) != 0,
        vxn: get_bits(hfr0, 29, 1) != 0,

        rx_fifo_size: get_bits(hfr1, 0, 5),
        tx_fifo_size: get_bits(hfr1, 6, 5),
        adv_ts_hi: get_bits(hfr1, 13, 1) != 0,
        dma_width: get_bits(hfr1, 14, 2),
        dcb: get_bits(hfr1, 16, 1) != 0,
        split_header: get_bits(hfr1, 17, 1) != 0,
        tso: get_bits(hfr1, 18, 1) != 0,
        dma_debug: get_bits(hfr1, 19, 1) != 0,
        rss: get_bits(hfr1, 20, 1) != 0,
        tc_count: get_bits(hfr1, 21, 3),
        hash_table_size: get_bits(hfr1, 24, 3),
        l3l4_filters: get_bits(hfr1, 27, 4),

        rx_queues: get_bits(hfr2, 0, 4),
        tx_queues: get_bits(hfr2, 6, 4),
        rx_channels: get_bits(hfr2, 12, 4),
        tx_channels: get_bits(hfr2, 18, 4),
        pps_outputs: get_bits(hfr2, 24, 3),
        aux_snapshots: get_bits(hfr2, 28, 3),
    };

    // Linux clamps these two instead of trusting them, because it has room
    // for four of each and a larger number would index off the end.
    f.pps_outputs = f.pps_outputs.min(MAX_PPS_OUT);
    f.aux_snapshots = f.aux_snapshots.min(MAX_AUX_SNAP);

    // An index, not a count. 0 means there is no hash filter table at all,
    // which `xgbe-dev.c` checks for before it programs one.
    f.hash_table_size = match f.hash_table_size {
        1 => 64,
        2 => 128,
        3 => 256,
        _ => 0,
    };

    // An index into address widths. Linux's `default` arm folds every
    // undefined encoding (3) back to 32, which is the safe direction: a
    // descriptor address that fits in 32 bits fits in 48 as well.
    f.dma_width = match f.dma_width {
        1 => 40,
        2 => 48,
        _ => 32,
    };

    // Zero-based counts.
    f.rx_queues += 1;
    f.tx_queues += 1;
    f.rx_channels += 1;
    f.tx_channels += 1;
    f.tc_count += 1;

    // log2 of the size, biased by 7, so 0 is 128 bytes and 11 is 256 KiB.
    // Note this is the FIFO the *controller* has; what the driver may use is
    // this capped by the variant's ceiling (`xgbe_get_tx_fifo_size` in
    // `xgbe-dev.c` takes the `min` of the two), which is why
    // [`Variant::max_fifo_size`] exists next door.
    f.rx_fifo_size = fifo_bytes(f.rx_fifo_size);
    f.tx_fifo_size = fifo_bytes(f.tx_fifo_size);

    f
}

// ——— The per-channel DMA register map ———
//
// The XGMAC's DMA engine is a bank of identical channels. Each one owns a
// transmit ring and a receive ring, and each one has its own copy of the same
// register set at a fixed stride inside BAR0. `MAC_HWF2R` above says how many
// there are; this says where they live.
//
// Linux never adds these offsets to a base by hand: it works out
// `channel->dma_regs` once per channel in `xgbe_alloc_channels`
// (`xgbe-drv.c`) ---
//
//     channel->dma_regs = pdata->xgmac_regs + DMA_CH_BASE +
//                         (DMA_CH_INC * i);
//
// --- and every later access goes through `XGMAC_DMA_IOREAD(channel, reg)`.
// Same shape here, for the same reason: the channel index is then applied in
// exactly one place, and a register offset can never accidentally be read
// against the wrong channel's base.
//
// Still read-only in this batch. Nothing below is written to a device yet;
// the probe continues to decline.

/// `DMA_CH_BASE`: where channel 0's registers start inside BAR0.
pub const DMA_CH_BASE: usize = 0x3100;
/// `DMA_CH_INC`: the stride from one channel's register block to the next.
///
/// Note this is larger than the highest register below (`DMA_CH_SR` at
/// `0x60`): there is a hole at the end of each channel's block. A loop that
/// assumed the blocks were packed end to end would land one channel's
/// registers on another's.
pub const DMA_CH_INC: usize = 0x80;

/// `DMA_CH_CR`: the channel's control register.
pub const DMA_CH_CR: usize = 0x00;
/// `DMA_CH_TCR`: transmit control.
pub const DMA_CH_TCR: usize = 0x04;
/// `DMA_CH_RCR`: receive control.
pub const DMA_CH_RCR: usize = 0x08;
/// `DMA_CH_TDLR_HI`: the high half of the transmit descriptor list address.
pub const DMA_CH_TDLR_HI: usize = 0x10;
/// `DMA_CH_TDLR_LO`: the low half.
pub const DMA_CH_TDLR_LO: usize = 0x14;
/// `DMA_CH_RDLR_HI`: the high half of the receive descriptor list address.
pub const DMA_CH_RDLR_HI: usize = 0x18;
/// `DMA_CH_RDLR_LO`: the low half.
pub const DMA_CH_RDLR_LO: usize = 0x1c;
/// `DMA_CH_TDTR_LO`: the transmit tail pointer. There is no `_HI`: the tail
/// is written as a low word only, and the engine takes the high half from the
/// list address.
pub const DMA_CH_TDTR_LO: usize = 0x24;
/// `DMA_CH_RDTR_LO`: the receive tail pointer, likewise.
pub const DMA_CH_RDTR_LO: usize = 0x2c;
/// `DMA_CH_TDRLR`: transmit ring length, as a last-index.
pub const DMA_CH_TDRLR: usize = 0x30;
/// `DMA_CH_RDRLR`: receive ring length, as a last-index.
pub const DMA_CH_RDRLR: usize = 0x34;
/// `DMA_CH_IER`: this channel's interrupt enables.
pub const DMA_CH_IER: usize = 0x38;
/// `DMA_CH_RIWT`: the receive interrupt watchdog timer, for coalescing.
pub const DMA_CH_RIWT: usize = 0x3c;
/// `DMA_CH_SR`: this channel's status, and where an interrupt is acked.
pub const DMA_CH_SR: usize = 0x60;

/// Where channel `index`'s register block sits, given the mapped base of
/// BAR0. Linux's `xgbe_alloc_channels`.
#[inline]
pub const fn channel_regs(xgmac_base: usize, index: u32) -> usize {
    xgmac_base + DMA_CH_BASE + DMA_CH_INC * index as usize
}

// The two register pairs that carry a 64-bit address are stored HIGH HALF
// FIRST: `TDLR_HI` is at 0x10 and `TDLR_LO` at 0x14. That is the opposite of
// what writing a `u64` as two consecutive words would produce, and getting it
// wrong points the engine's descriptor fetch at an address with its halves
// swapped --- which on a 32-bit-addressed ring means a fetch from zero, and
// on a wider one a fetch from somewhere else entirely. Hence a named pair
// instead of arithmetic at each call site.

/// The two register offsets that together hold the transmit descriptor list
/// address, in `(high, low)` order --- which is also their order in memory.
pub const DMA_CH_TDLR: (usize, usize) = (DMA_CH_TDLR_HI, DMA_CH_TDLR_LO);
/// The same for the receive descriptor list.
pub const DMA_CH_RDLR: (usize, usize) = (DMA_CH_RDLR_HI, DMA_CH_RDLR_LO);

/// Split a descriptor-ring address into the halves those two registers want.
#[inline]
pub const fn split_ring_addr(paddr: u64) -> (u32, u32) {
    ((paddr >> 32) as u32, paddr as u32)
}

// ——— The descriptor ———
//
// Linux's `struct xgbe_ring_desc` (`xgbe.h`): four little-endian words, and
// nothing else. The same four words mean different things depending on
// direction, on whether the descriptor is a normal or a context one, and ---
// for two fields --- on who wrote it last. That last part is the trap, and
// the reason the accessors below are named for their direction rather than
// for the word they read.

/// A ring descriptor: `struct xgbe_ring_desc`, four 32-bit words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Desc {
    /// `desc0`
    pub desc0: u32,
    /// `desc1`
    pub desc1: u32,
    /// `desc2`
    pub desc2: u32,
    /// `desc3` --- the word that carries `OWN`, and the one both sides race
    /// over.
    pub desc3: u32,
}

/// `DESC3`'s `OWN` bit, at index 31 in both the transmit and the receive
/// normal descriptor. Set means the engine owns the descriptor; cleared means
/// the driver does.
pub const DESC3_OWN: u32 = 1 << 31;
/// `CTXT`, index 30 --- but only on a descriptor the *chip* wrote back. On a
/// receive descriptor the *driver* is handing over, bit 30 is `INTE`
/// (interrupt on completion). One bit, two meanings, chosen by who wrote it
/// last; see [`rx_desc3_inte`].
pub const DESC3_CTXT: u32 = 1 << 30;
/// `FD`, index 29: this descriptor holds the first fragment of a frame.
pub const DESC3_FD: u32 = 1 << 29;
/// `LD`, index 28: ... and the last.
pub const DESC3_LD: u32 = 1 << 28;

/// Whether the DMA engine still owns this descriptor, so the driver must not
/// touch it. `RX_NORMAL_DESC3_OWN` / `TX_NORMAL_DESC3_OWN`, index 31.
#[inline]
pub const fn desc_owned_by_dma(desc3: u32) -> bool {
    desc3 & DESC3_OWN != 0
}

/// Whether a written-back descriptor is a context descriptor rather than a
/// normal one. `RX_NORMAL_DESC3_CTXT`, index 30.
#[inline]
pub const fn desc_is_context(desc3: u32) -> bool {
    desc3 & DESC3_CTXT != 0
}

/// `RX_NORMAL_DESC3_INTE`, index 30: the same bit as [`DESC3_CTXT`], as the
/// driver sets it when handing a receive descriptor to the engine.
///
/// Two functions over one bit on purpose. The bit is only `INTE` on the way
/// in and only `CTXT` on the way back, so a single accessor would read
/// "interrupt wanted" off a descriptor the chip filled in, which is
/// meaningless, or test for a context descriptor on one the driver is still
/// building, which is always false.
#[inline]
pub const fn rx_desc3_inte(desc3: u32) -> bool {
    desc3 & DESC3_CTXT != 0
}

/// The received frame's length in bytes. `RX_NORMAL_DESC3_PL`, index 0,
/// width 14.
#[inline]
pub const fn rx_packet_len(desc3: u32) -> u32 {
    get_bits(desc3, 0, 14)
}

/// Whether the engine flagged an error on the frame.
/// `RX_NORMAL_DESC3_ES`, index 15, width 1.
#[inline]
pub const fn rx_error_summary(desc3: u32) -> bool {
    get_bits(desc3, 15, 1) != 0
}

/// The header length a split-header receive put in `desc2`.
/// `RX_NORMAL_DESC2_HL`, index 0, width 10. Note this is TEN bits, not the
/// fourteen that the transmit side's length field in the same word uses.
#[inline]
pub const fn rx_header_len(desc2: u32) -> u32 {
    get_bits(desc2, 0, 10)
}

/// The buffer length the driver puts in a transmit descriptor's `desc2`.
/// `TX_NORMAL_DESC2_HL_B1L`, index 0, width 14.
#[inline]
pub const fn tx_desc2_buffer_len(len: u32) -> u32 {
    len & ((1 << 14) - 1)
}

/// The total frame length the driver puts in the first transmit descriptor.
/// `TX_NORMAL_DESC3_FL`, index 0, width 15.
///
/// Fifteen bits, and they overlap `TCPPL` (index 0, width **18**), which is
/// what the same bits mean when `TSE` (index 18) is set for segmentation
/// offload. So this field cannot be written without knowing whether the
/// descriptor is a TSO one: with `TSE` set, bit 18 is the bottom of the
/// payload length, and a frame length masked to fifteen bits would be read as
/// part of it.
#[inline]
pub const fn tx_desc3_frame_len(len: u32) -> u32 {
    len & ((1 << 15) - 1)
}

/// `TX_NORMAL_DESC3_TSE`, index 18: the bit that re-reads the bottom of
/// `desc3` as a TCP payload length.
pub const TX_DESC3_TSE: u32 = 1 << 18;

// ——— Reaching the registers ———
//
// The batches so far were pure arithmetic over values read in one place. From
// here on there are *sequences*: an order of writes, a bit polled until it
// clears, a step that must not happen on an older core. Those are exactly the
// parts that cannot be checked by reading the code back, and exactly the
// parts that leave no trace when they are wrong --- a reset that was never
// waited for does not report anything, it just hands over a chip that is
// still resetting.
//
// So the same shape as `pcnet32` next door: the register access goes through
// a trait, the real implementation is volatile MMIO, and the tests drive the
// genuine sequence against an in-memory XGMAC. That is the only way anything
// here can be exercised at all, since QEMU does not emulate this controller.

/// Access to the XGMAC register block. Offsets are relative to the base of
/// BAR0, so the constants above can be passed straight in.
pub trait MmioAccess: Send + Sync {
    /// Read one 32-bit register.
    fn read32(&self, off: usize) -> u32;
    /// Write one 32-bit register.
    fn write32(&self, off: usize, value: u32);
}

/// Volatile MMIO against a mapped BAR0.
pub struct Bar0 {
    base: usize,
}

impl Bar0 {
    /// Wrap a mapped BAR0 base address.
    ///
    /// # Safety
    /// `base` must be a mapping of the device's BAR0 that stays valid for the
    /// lifetime of this value, and at least [`MIN_XGMAC_BAR_LEN`] long.
    pub const unsafe fn new(base: usize) -> Self {
        Self { base }
    }
}

impl MmioAccess for Bar0 {
    fn read32(&self, off: usize) -> u32 {
        // Safety: the constructor's contract covers the whole BAR.
        unsafe { core::ptr::read_volatile((self.base + off) as *const u32) }
    }

    fn write32(&self, off: usize, value: u32) {
        unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, value) }
    }
}

/// Linux's `SET_BITS`: replace `width` bits of `reg` at `index` with `value`.
#[inline]
pub const fn set_bits(reg: u32, index: u32, width: u32, value: u32) -> u32 {
    let mask = ((1u32 << width) - 1) << index;
    (reg & !mask) | ((value << index) & mask)
}

/// Linux's `XGMAC_IOWRITE_BITS`: read the register, replace one field, write
/// it back.
///
/// Read-modify-write and not a bare write, which matters: every register
/// touched below shares its word with fields that must survive the change.
#[inline]
pub fn write_field(acc: &dyn MmioAccess, off: usize, index: u32, width: u32, value: u32) {
    let reg = acc.read32(off);
    acc.write32(off, set_bits(reg, index, width, value));
}

/// Linux's `XGMAC_IOREAD_BITS`.
#[inline]
pub fn read_field(acc: &dyn MmioAccess, off: usize, index: u32, width: u32) -> u32 {
    get_bits(acc.read32(off), index, width)
}

// ——— The DMA mode register, and the software reset ———

/// `DMA_MR`: the DMA mode register.
pub const DMA_MR: usize = 0x3000;
/// `DMA_SBMR`: the system bus mode register.
pub const DMA_SBMR: usize = 0x3004;

/// `DMA_MR_SWR`, index 0, width 1: write one to start a software reset. The
/// chip clears it when the reset is done, so this is both the command and the
/// completion flag.
pub const DMA_MR_SWR: (u32, u32) = (0, 1);

/// `MTL_Q_BASE`: where queue 0's MTL registers start inside BAR0.
pub const MTL_Q_BASE: usize = 0x1100;
/// `MTL_Q_INC`: the stride between queues --- the same `0x80` as the DMA
/// channels, and just as much of a trap, since the registers in a queue's
/// block do not fill it.
pub const MTL_Q_INC: usize = 0x80;
/// `MTL_Q_TQOMR`: a transmit queue's operation mode register.
pub const MTL_Q_TQOMR: usize = 0x00;
/// `MTL_Q_RQOMR`: a receive queue's operation mode register.
pub const MTL_Q_RQOMR: usize = 0x40;
/// `MTL_Q_TQOMR_FTQ`, index 0, width 1: flush this transmit queue. Like
/// `SWR`, the chip clears it when it is done.
pub const MTL_Q_TQOMR_FTQ: (u32, u32) = (0, 1);

/// The offset of per-queue register `reg` for queue `n`. Linux's
/// `XGMAC_MTL_IOREAD`.
#[inline]
pub const fn mtl_queue_reg(n: u32, reg: usize) -> usize {
    MTL_Q_BASE + MTL_Q_INC * n as usize + reg
}

/// How many times Linux polls a self-clearing bit before giving up
/// (`count = 2000` in `__xgbe_exit` and `xgbe_flush_tx_queues`).
pub const POLL_LIMIT: u32 = 2000;

/// The core revision below which the transmit queues must not be flushed.
/// `xgbe_flush_tx_queues` returns immediately when `SNPSVER < 0x21`.
pub const FLUSH_MIN_SNPSVER: u8 = 0x21;

/// Issue one software reset and wait for the chip to clear `SWR`.
///
/// `wait` is called between polls; the caller decides what a delay means
/// here, which is also what lets a test run the sequence with no delay at
/// all. Linux sleeps 10--15 us after the write and 500--600 us per poll.
///
/// Returns [`DeviceError::NotReady`] if the bit is still set after
/// [`POLL_LIMIT`] polls --- Linux's `-EBUSY`.
pub fn software_reset_once(acc: &dyn MmioAccess, wait: &mut dyn FnMut()) -> DeviceResult<()> {
    let (index, width) = DMA_MR_SWR;
    write_field(acc, DMA_MR, index, width, 1);
    wait();

    let mut count = POLL_LIMIT;
    while count > 0 && read_field(acc, DMA_MR, index, width) != 0 {
        count -= 1;
        wait();
    }
    if count == 0 {
        warn!("[xgbe] the controller did not come out of software reset");
        return Err(DeviceError::NotReady);
    }
    Ok(())
}

/// Reset the controller, the way `xgbe_exit` does: **twice**.
///
/// Not a belt-and-braces flourish. Linux's own comment says it: "To guard
/// against possible incorrectly generated interrupts, issue the software
/// reset twice." A single reset can leave an interrupt pending that was
/// raised while the engine was coming down, and the second one clears it.
/// Dropping the second reset is the sort of change that looks like a
/// simplification and shows up a week later as an interrupt storm at boot.
pub fn software_reset(acc: &dyn MmioAccess, wait: &mut dyn FnMut()) -> DeviceResult<()> {
    software_reset_once(acc, wait)?;
    software_reset_once(acc, wait)
}

/// Flush every transmit queue, as `xgbe_flush_tx_queues` does.
///
/// Two things that are easy to get wrong and leave no trace:
///
/// * The whole step is **skipped** on a core older than
///   [`FLUSH_MIN_SNPSVER`]. `FTQ` does not exist there, so writing it sets a
///   reserved bit in a register that also holds the queue size and the
///   store-and-forward mode.
/// * All the queues are kicked **first**, and only then are they polled.
///   Flushing one at a time and waiting for each serialises what the hardware
///   does in parallel, and on eight queues that is eight timeouts' worth of
///   worst case instead of one.
pub fn flush_tx_queues(
    acc: &dyn MmioAccess,
    snps_ver: u8,
    tx_queues: u32,
    wait: &mut dyn FnMut(),
) -> DeviceResult<()> {
    if snps_ver < FLUSH_MIN_SNPSVER {
        return Ok(());
    }
    let (index, width) = MTL_Q_TQOMR_FTQ;

    for q in 0..tx_queues {
        write_field(acc, mtl_queue_reg(q, MTL_Q_TQOMR), index, width, 1);
    }

    for q in 0..tx_queues {
        let off = mtl_queue_reg(q, MTL_Q_TQOMR);
        let mut count = POLL_LIMIT;
        while count > 0 && read_field(acc, off, index, width) != 0 {
            count -= 1;
            wait();
        }
        if count == 0 {
            warn!("[xgbe] transmit queue {} did not finish flushing", q);
            return Err(DeviceError::NotReady);
        }
    }
    Ok(())
}

// ——— The system bus mode ———
//
// `DMA_SBMR` is the register that says how the engine may behave on the bus:
// how long a burst it may issue, whether it must keep its transfers aligned,
// how many reads and writes it may have outstanding, and whether descriptor
// addresses are 32 or 64 bits wide.
//
// `xgbe_config_dma_bus` (`xgbe-dev.c`) builds it in a way worth copying
// exactly: ONE read, five fields set in the local copy, ONE write. Not five
// read-modify-writes. The difference is not style --- a field-by-field
// version publishes four intermediate states to a live bus master, each a
// combination the driver never intended, and bit 0 of this register is a
// "set this" bit whose meaning the datasheet does not spell out, which is
// exactly the kind of thing that must not be observed half-applied.

/// `DMA_SBMR_UNDEF`, index 0, width 1.
///
/// Linux sets it to one, under the comment "Set the System Bus mode", and the
/// field's own name in `xgbe-common.h` is `UNDEF`. So: a bit that must be
/// one, that nobody has documented. It is copied here because the alternative
/// is diverging from the one driver known to work on this controller on a bit
/// whose effect is unknown.
pub const DMA_SBMR_UNDEF: (u32, u32) = (0, 1);
/// `DMA_SBMR_BLEN`, index 1, width 7: the permitted burst lengths.
pub const DMA_SBMR_BLEN: (u32, u32) = (1, 7);
/// `DMA_SBMR_EAME`, index 11, width 1: enhanced addressing mode, i.e. the
/// engine may use descriptor addresses wider than 32 bits. Linux sets it
/// unconditionally.
pub const DMA_SBMR_EAME: (u32, u32) = (11, 1);
/// `DMA_SBMR_AAL`, index 12, width 1: address-aligned beats.
pub const DMA_SBMR_AAL: (u32, u32) = (12, 1);
/// `DMA_SBMR_RD_OSR_LMT`, index 16, width 6: outstanding read requests.
pub const DMA_SBMR_RD_OSR_LMT: (u32, u32) = (16, 6);
/// `DMA_SBMR_WR_OSR_LMT`, index 24, width 6: outstanding write requests.
pub const DMA_SBMR_WR_OSR_LMT: (u32, u32) = (24, 6);

/// Turn a burst length in beats into the `BLEN` field value, the way Linux's
/// `blen >> 2` does.
///
/// That shift looks like a unit conversion and is not. `BLEN` is a **bitmap**:
/// its seven bits are one each for bursts of 4, 8, 16, 32, 64, 128 and 256
/// beats, so dividing the beat count by four lands exactly on that length's
/// own bit. Which means the field can legitimately carry several lengths at
/// once --- Linux happens to set one.
///
/// Returns `None` for anything that is not one of those seven lengths, rather
/// than shifting it into a neighbouring length's bit.
#[inline]
pub const fn blen_field(beats: u32) -> Option<u32> {
    match beats {
        4 | 8 | 16 | 32 | 64 | 128 | 256 => Some(beats >> 2),
        _ => None,
    }
}

/// How the bus should be configured. Every member is a decision, which is
/// why there is no `Default`: see [`system_bus_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusConfig {
    /// Burst length in beats: 4, 8, 16, 32, 64, 128 or 256.
    pub burst_beats: u32,
    /// Whether to require address-aligned beats (`AAL`).
    pub aligned_beats: bool,
    /// How many reads may be outstanding, counted from one.
    pub read_outstanding: u32,
    /// How many writes may be outstanding, counted from one.
    pub write_outstanding: u32,
}

/// Build the value for `DMA_SBMR` from `sbmr` as the chip currently has it.
///
/// Three of these fields are **not** written as the number they represent,
/// and all three are the sort of mistake that produces a working-but-wrong
/// bus master:
///
/// * `BLEN` takes the burst length's own bit, not the length (see
///   [`blen_field`]).
/// * `RD_OSR_LMT` and `WR_OSR_LMT` are written as **limit minus one**: the
///   field counts from zero, so writing the limit itself permits one more
///   request than intended, and writing zero as a limit underflows to 63.
///   Hence [`DeviceError::InvalidParam`] on a zero limit instead of the
///   wraparound Linux would produce from an unset platform property.
/// * `EAME` must be on before a descriptor address above 4 GiB is programmed,
///   and the high half of the list address register is written regardless ---
///   so without this bit the engine takes the low half and the ring is
///   fetched from the wrong place with nothing reported.
pub fn system_bus_mode(sbmr: u32, cfg: BusConfig) -> DeviceResult<u32> {
    let blen = blen_field(cfg.burst_beats).ok_or(DeviceError::InvalidParam)?;
    if cfg.read_outstanding == 0 || cfg.write_outstanding == 0 {
        return Err(DeviceError::InvalidParam);
    }
    let max = 1 << DMA_SBMR_RD_OSR_LMT.1;
    if cfg.read_outstanding > max || cfg.write_outstanding > max {
        return Err(DeviceError::InvalidParam);
    }

    let mut v = sbmr;
    v = set_bits(v, DMA_SBMR_EAME.0, DMA_SBMR_EAME.1, 1);
    v = set_bits(v, DMA_SBMR_UNDEF.0, DMA_SBMR_UNDEF.1, 1);
    v = set_bits(v, DMA_SBMR_BLEN.0, DMA_SBMR_BLEN.1, blen);
    v = set_bits(v, DMA_SBMR_AAL.0, DMA_SBMR_AAL.1, cfg.aligned_beats as u32);
    v = set_bits(
        v,
        DMA_SBMR_RD_OSR_LMT.0,
        DMA_SBMR_RD_OSR_LMT.1,
        cfg.read_outstanding - 1,
    );
    v = set_bits(
        v,
        DMA_SBMR_WR_OSR_LMT.0,
        DMA_SBMR_WR_OSR_LMT.1,
        cfg.write_outstanding - 1,
    );
    Ok(v)
}

/// Apply [`system_bus_mode`] to the device: one read, one write.
///
/// Note what is *not* here: a default `BusConfig`. The burst length and the
/// outstanding-request limits are platform properties in Linux --- read from
/// the device tree or ACPI --- and the PCI probe in the sources read here
/// does not set them at all, which leaves them zero in a freshly allocated
/// `pdata`. Inheriting that would mean a burst field of zero and an
/// outstanding limit that underflowed to 63. So the caller must choose, and
/// nothing is chosen here on a controller nobody has run: picking numbers for
/// a bus whose behaviour cannot be measured would be a guess dressed up as a
/// default.
pub fn config_dma_bus(acc: &dyn MmioAccess, cfg: BusConfig) -> DeviceResult<()> {
    let value = system_bus_mode(acc.read32(DMA_SBMR), cfg)?;
    acc.write32(DMA_SBMR, value);
    Ok(())
}

// ——— The station address ———

/// `MAC_MACA0HR`: the high half of MAC address 0 --- the primary address.
pub const MAC_MACA0HR: usize = 0x0300;
/// `MAC_MACA0LR`: its low half.
pub const MAC_MACA0LR: usize = 0x0304;
/// `MAC_MACA1HR`: the high half of MAC address 1, the first of the additional
/// addresses whose count `ADDMACADRSEL` reports.
pub const MAC_MACA1HR: usize = 0x0308;

/// `MAC_MACA1HR_AE`, index 31, width 1: address enable.
///
/// Defined in `xgbe-common.h` for `MACA1HR` and **not** for `MACA0HR`.
/// Address 0 is the station address and is always enabled; the additional
/// addresses each have this bit. Setting it on `MACA0HR` would put a one into
/// a bit the primary address register does not define, in the same write that
/// carries two bytes of the address.
pub const MAC_MACA1HR_AE: (u32, u32) = (31, 1);

/// Split a station address into the `(high, low)` register values, as
/// `xgbe_set_mac_address` does.
///
/// The halves are not symmetric: the low word takes **four** bytes and the
/// high word **two**, both little-endian within their word. So the high
/// register is a half-filled word whose top sixteen bits stay zero on address
/// 0 --- which is also why the `AE` bit of the additional addresses has
/// somewhere to live.
#[inline]
pub const fn mac_to_regs(mac: [u8; 6]) -> (u32, u32) {
    let hi = ((mac[5] as u32) << 8) | (mac[4] as u32);
    let lo = ((mac[3] as u32) << 24)
        | ((mac[2] as u32) << 16)
        | ((mac[1] as u32) << 8)
        | (mac[0] as u32);
    (hi, lo)
}

/// Program the station address into address slot 0.
///
/// High half first, then low, which is the order Linux writes them. Keep it:
/// on this family of MACs the low word is the one that latches the pair, so
/// writing low first leaves the chip briefly filtering on an address whose
/// top two bytes are whatever was there before.
pub fn set_mac_address(acc: &dyn MmioAccess, mac: [u8; 6]) {
    let (hi, lo) = mac_to_regs(mac);
    acc.write32(MAC_MACA0HR, hi);
    acc.write32(MAC_MACA0LR, lo);
}

// ——— Turning the thing on ———
//
// Three layers sit between a descriptor and the wire: the DMA channel that
// fetches it, the MTL queue that buffers the frame, and the MAC that puts it
// on the line. Bringing them up means enabling all three, and the order is
// **inside out** --- channel, then queue, then MAC --- in both directions.
//
// That order is the whole content of these two functions, and it is not
// arbitrary. Enabling the MAC first would open the line into a path whose
// queues and channels are not running yet: on receive that is frames arriving
// at a queue that cannot drain, and the chip's answer to that is to drop them
// and raise an overflow. Enabling inside out means every stage downstream of
// the one being switched on is already able to move data.
//
// What is NOT here is the matching disable, and that is deliberate rather
// than unfinished: **the disable is not this sequence reversed**, and the two
// directions do not even disable the same way as each other.
//
//   * `xgbe_disable_tx` goes inside out *again* --- channels, then queues,
//     then the MAC last --- with a wait for the DMA to finish its pending
//     descriptors before it starts and a wait for each MTL queue to drain in
//     the middle. The MAC stays up until the end precisely so that what is
//     already queued can still get out.
//   * `xgbe_disable_rx` goes the other way, outside in: the MAC first, then
//     the queues, then the channels. Nothing new must be accepted before the
//     path behind it is torn down.
//
// So "disable is enable backwards" is wrong for transmit, and "transmit and
// receive are symmetric" is wrong for disable. Both are exactly the kind of
// tidying-up a later reader would apply, which is why the asymmetry is
// written down here before the code that depends on it exists. The two
// disable paths need the drain loops, so they come in their own batch.

/// `MAC_TCR`: the MAC's transmit configuration.
pub const MAC_TCR: usize = 0x0000;
/// `MAC_RCR`: the MAC's receive configuration.
pub const MAC_RCR: usize = 0x0004;
/// `MAC_RQC0R`: receive queue control 0 --- which receive queues are enabled.
pub const MAC_RQC0R: usize = 0x00a0;

/// `MAC_TCR_TE`, index 0, width 1: transmitter enable.
pub const MAC_TCR_TE: (u32, u32) = (0, 1);
/// `MAC_RCR_RE`, index 0, width 1: receiver enable.
pub const MAC_RCR_RE: (u32, u32) = (0, 1);
/// `MAC_RCR_ACS`, index 1, width 1: strip the pad and FCS from a received
/// frame.
pub const MAC_RCR_ACS: (u32, u32) = (1, 1);
/// `MAC_RCR_CST`, index 2, width 1: CRC stripping for type frames.
pub const MAC_RCR_CST: (u32, u32) = (2, 1);
/// `MAC_RCR_DCRCC`, index 3, width 1: do **not** check the received CRC.
/// Linux clears it, so the MAC does check.
pub const MAC_RCR_DCRCC: (u32, u32) = (3, 1);

/// `MTL_Q_TQOMR_TXQEN`, index 2, width **2**.
pub const MTL_Q_TQOMR_TXQEN: (u32, u32) = (2, 2);
/// `MTL_Q_ENABLED`: the value that enables a queue, which is **2 and not 1**.
/// `TXQEN` is two bits wide and 1 means "enabled for average-bit-rate
/// scheduling", a different mode. Writing a boolean into it is the mistake
/// the field's width invites.
pub const MTL_Q_ENABLED: u32 = 0x02;
/// `MTL_Q_DISABLED`.
pub const MTL_Q_DISABLED: u32 = 0x00;

/// The offset of per-channel register `reg` for channel `n`, relative to the
/// base of BAR0 --- i.e. what [`MmioAccess`] wants, where [`channel_regs`]
/// takes a mapped address. Same arithmetic, different starting point, and
/// mixing them up is an access at a wild address.
#[inline]
pub const fn dma_channel_reg(n: u32, reg: usize) -> usize {
    channel_regs(0, n) + reg
}

/// `DMA_CH_TCR_ST`, index 0, width 1: start this channel's transmit engine.
pub const DMA_CH_TCR_ST: (u32, u32) = (0, 1);
/// `DMA_CH_RCR_SR`, index 0, width 1: start this channel's receive engine.
pub const DMA_CH_RCR_SR: (u32, u32) = (0, 1);

/// The value for `MAC_RQC0R` that enables the first `queues` receive queues.
///
/// Linux builds it as `reg_val |= (0x02 << (i << 1))`: two bits per queue,
/// and the enabled value in those two bits is **2**, the same `MTL_Q_ENABLED`
/// as the transmit side. So eight queues is `0xaaaa`, not `0xff`.
///
/// The register is written **whole**, not read-modify-written, which is also
/// how a queue gets disabled: `xgbe_disable_rx` writes a bare zero. That
/// means any queue not named here is switched off by the same write.
#[inline]
pub const fn rx_queue_enable_map(queues: u32) -> u32 {
    let mut map = 0u32;
    let mut i = 0;
    while i < queues && i < 16 {
        map |= MTL_Q_ENABLED << (i << 1);
        i += 1;
    }
    map
}

/// Enable transmit: every channel, then every queue, then the MAC.
///
/// `channels` and `queues` come from [`HwFeatures`], not from a guess.
pub fn enable_tx(acc: &dyn MmioAccess, channels: u32, queues: u32) {
    for ch in 0..channels {
        let off = dma_channel_reg(ch, DMA_CH_TCR);
        write_field(acc, off, DMA_CH_TCR_ST.0, DMA_CH_TCR_ST.1, 1);
    }
    for q in 0..queues {
        write_field(
            acc,
            mtl_queue_reg(q, MTL_Q_TQOMR),
            MTL_Q_TQOMR_TXQEN.0,
            MTL_Q_TQOMR_TXQEN.1,
            MTL_Q_ENABLED,
        );
    }
    write_field(acc, MAC_TCR, MAC_TCR_TE.0, MAC_TCR_TE.1, 1);
}

/// Enable receive: every channel, then the queue map, then the MAC.
///
/// The MAC's receive configuration is set in the order Linux uses, and the
/// enable is **last of the four** writes to `MAC_RCR`. Setting `RE` before
/// `ACS` and `CST` would open the line for the window in between, during
/// which frames are delivered with their padding and FCS still attached ---
/// which is not an error anywhere, just a handful of frames that arrive four
/// bytes too long at the very start of the link.
pub fn enable_rx(acc: &dyn MmioAccess, channels: u32, queues: u32) {
    for ch in 0..channels {
        let off = dma_channel_reg(ch, DMA_CH_RCR);
        write_field(acc, off, DMA_CH_RCR_SR.0, DMA_CH_RCR_SR.1, 1);
    }

    acc.write32(MAC_RQC0R, rx_queue_enable_map(queues));

    write_field(acc, MAC_RCR, MAC_RCR_DCRCC.0, MAC_RCR_DCRCC.1, 0);
    write_field(acc, MAC_RCR, MAC_RCR_CST.0, MAC_RCR_CST.1, 1);
    write_field(acc, MAC_RCR, MAC_RCR_ACS.0, MAC_RCR_ACS.1, 1);
    write_field(acc, MAC_RCR, MAC_RCR_RE.0, MAC_RCR_RE.1, 1);
}

// ——— Turning it off ———
//
// The counterpart to [`enable_tx`] and [`enable_rx`], and the batch that
// shows why they could not share a function with a boolean. Transmit comes
// down inside out, like it went up, with two waits; receive comes down
// outside in, with one. The reasoning is in the comment above `enable_tx`.
//
// Both waits are for the hardware to go quiet, and both are bounded: Linux
// gives each one `XGBE_DMA_STOP_TIMEOUT` seconds and then logs and carries on
// rather than returning an error. Carrying on is the right call --- the
// caller is tearing the interface down, and refusing to finish because the
// chip is wedged leaves it half-up.

/// `XGBE_DMA_STOP_TIMEOUT`, in seconds: how long Linux waits for each stage
/// to go quiet before giving up on it and continuing anyway.
pub const DMA_STOP_TIMEOUT_SECS: u32 = 1;

/// `DMA_DSR0`: DMA debug status 0, which holds the engine state of the first
/// three queues.
pub const DMA_DSR0: usize = 0x3020;
/// `DMA_DSR1`: and of the ones after that.
pub const DMA_DSR1: usize = 0x3024;
/// `DMA_DSR_RPS_WIDTH` / `DMA_DSR_TPS_WIDTH`: four bits each.
pub const DMA_DSR_TPS_WIDTH: u32 = 4;
/// `DMA_DSR_Q_WIDTH`: receive state plus transmit state, so eight bits per
/// queue.
pub const DMA_DSR_Q_WIDTH: u32 = 8;
/// `DMA_DSR0_TPS_START`: where queue 0's transmit state begins in `DSR0`.
pub const DMA_DSR0_TPS_START: u32 = 12;
/// `DMA_DSRX_FIRST_QUEUE`: the first queue that lives outside `DSR0`.
pub const DMA_DSRX_FIRST_QUEUE: u32 = 3;
/// `DMA_DSRX_INC`: the stride from `DSR1` to the next status register.
pub const DMA_DSRX_INC: usize = 4;
/// `DMA_DSRX_QPR`: queues per status register, from `DSR1` on.
pub const DMA_DSRX_QPR: u32 = 4;
/// `DMA_DSRX_TPS_START`: where the first transmit state begins in `DSR1` and
/// later --- **4**, not the 12 of `DSR0`.
pub const DMA_DSRX_TPS_START: u32 = 4;

/// `DMA_TPS_STOPPED`.
pub const DMA_TPS_STOPPED: u32 = 0x00;
/// `DMA_TPS_SUSPENDED`. Note it is **6**, not 1: the transmit-process state
/// is a four-bit enumeration, and "stopped enough to proceed" is two
/// particular values out of sixteen, not a flag.
pub const DMA_TPS_SUSPENDED: u32 = 0x06;

/// Where queue `queue`'s transmit engine state lives: `(register offset, bit
/// index)`.
///
/// This mapping is the trap of the batch, because it looks like it should be
/// `DSR0 + queue * something` and is not. Queues 0 to 2 live in `DSR0`
/// starting at bit 12; from queue 3 on they live in `DSR1` and the registers
/// after it, **four to a register**, starting at bit 4 instead. So queue 3 is
/// not "queue 2 plus eight bits": it is a different register at a different
/// starting offset. A linear reading puts queue 3's state where queue 0's
/// receive state is and waits for a stop that will never be reported.
#[inline]
pub const fn tx_dma_status_pos(queue: u32) -> (usize, u32) {
    if queue < DMA_DSRX_FIRST_QUEUE {
        (DMA_DSR0, queue * DMA_DSR_Q_WIDTH + DMA_DSR0_TPS_START)
    } else {
        let qidx = queue - DMA_DSRX_FIRST_QUEUE;
        let reg = DMA_DSR1 + (qidx / DMA_DSRX_QPR) as usize * DMA_DSRX_INC;
        let pos = (qidx % DMA_DSRX_QPR) * DMA_DSR_Q_WIDTH + DMA_DSRX_TPS_START;
        (reg, pos)
    }
}

/// Whether a transmit-process state means the engine is quiet enough to be
/// switched off.
#[inline]
pub const fn tx_dma_is_quiet(state: u32) -> bool {
    state == DMA_TPS_STOPPED || state == DMA_TPS_SUSPENDED
}

/// `MTL_Q_TQDR`: a transmit queue's debug register.
pub const MTL_Q_TQDR: usize = 0x08;
/// `MTL_Q_RQDR`: a receive queue's debug register.
pub const MTL_Q_RQDR: usize = 0x48;
/// `MTL_Q_TQDR_TRCSTS`, index 1, width 2: the read-controller state.
pub const MTL_Q_TQDR_TRCSTS: (u32, u32) = (1, 2);
/// `MTL_Q_TQDR_TXQSTS`, index 4, width 1: whether the queue holds anything.
pub const MTL_Q_TQDR_TXQSTS: (u32, u32) = (4, 1);
/// `MTL_Q_RQDR_RXQSTS`, index 4, width 2: the receive queue's fill state.
pub const MTL_Q_RQDR_RXQSTS: (u32, u32) = (4, 2);
/// `MTL_Q_RQDR_PRXQ`, index 16, width 14: how many bytes are still in it.
pub const MTL_Q_RQDR_PRXQ: (u32, u32) = (16, 14);

/// Whether a transmit queue has drained, from its debug register.
///
/// The read-controller test is `!= 1`, not `== 0`: `TRCSTS` is a two-bit
/// state and 1 is the one that means "reading a frame out right now". The
/// other three, zero among them, are all fine to stop on.
#[inline]
pub const fn tx_queue_drained(tqdr: u32) -> bool {
    get_bits(tqdr, MTL_Q_TQDR_TRCSTS.0, MTL_Q_TQDR_TRCSTS.1) != 1
        && get_bits(tqdr, MTL_Q_TQDR_TXQSTS.0, MTL_Q_TQDR_TXQSTS.1) == 0
}

/// Whether a receive queue has drained: nothing in its FIFO and no bytes
/// left to hand over.
#[inline]
pub const fn rx_queue_drained(rqdr: u32) -> bool {
    get_bits(rqdr, MTL_Q_RQDR_PRXQ.0, MTL_Q_RQDR_PRXQ.1) == 0
        && get_bits(rqdr, MTL_Q_RQDR_RXQSTS.0, MTL_Q_RQDR_RXQSTS.1) == 0
}

/// How a bounded wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// The hardware went quiet.
    Yes,
    /// It did not, within the budget. The caller carries on regardless: see
    /// the module comment.
    TimedOut,
    /// Nothing was waited for, because there is no link and so nothing can
    /// complete.
    NoLink,
}

/// Wait for one transmit queue to go quiet before its channel is stopped.
///
/// Two paths, chosen by the core revision, and they read entirely different
/// registers:
///
/// * Above `SNPSVER 0x20`, the queue's own `MTL_Q_TQDR`.
/// * At `0x20` and below, the DMA debug status register for that queue, found
///   through [`tx_dma_status_pos`].
///
/// `has_link` is not a convenience. With no link the transmit path never
/// completes, so waiting is a guaranteed timeout on every queue in turn ---
/// one second each, on a path whose whole job is to bring the interface down.
/// Linux skips it for exactly that reason.
pub fn prepare_tx_stop(
    acc: &dyn MmioAccess,
    snps_ver: u8,
    queue: u32,
    has_link: bool,
    poll: &mut dyn FnMut() -> bool,
) -> Drained {
    if !has_link {
        return Drained::NoLink;
    }
    if snps_ver > 0x20 {
        while poll() {
            if tx_queue_drained(acc.read32(mtl_queue_reg(queue, MTL_Q_TQDR))) {
                return Drained::Yes;
            }
        }
        return Drained::TimedOut;
    }

    let (reg, pos) = tx_dma_status_pos(queue);
    while poll() {
        let state = get_bits(acc.read32(reg), pos, DMA_DSR_TPS_WIDTH);
        if tx_dma_is_quiet(state) {
            return Drained::Yes;
        }
    }
    Drained::TimedOut
}

/// Wait for one receive queue's FIFO to empty before its channel is stopped.
///
/// No link check here, and that asymmetry is Linux's: a receive queue drains
/// on its own whether or not the link is up, because what is in it came from
/// the wire already.
pub fn prepare_rx_stop(
    acc: &dyn MmioAccess,
    queue: u32,
    poll: &mut dyn FnMut() -> bool,
) -> Drained {
    while poll() {
        if rx_queue_drained(acc.read32(mtl_queue_reg(queue, MTL_Q_RQDR))) {
            return Drained::Yes;
        }
    }
    Drained::TimedOut
}

/// Bring transmit down: channels, then queues, then the MAC **last**.
///
/// The MAC stays up to the end on purpose, so that frames already in the
/// queues can still reach the wire. `tx_complete` is the caller's check that
/// the rings have been consumed --- the driver knows its own producer and
/// consumer indices, the chip does not report them --- and it is skipped
/// without a link for the same reason as above.
pub fn disable_tx(
    acc: &dyn MmioAccess,
    snps_ver: u8,
    channels: u32,
    queues: u32,
    has_link: bool,
    tx_complete: &mut dyn FnMut() -> bool,
    poll: &mut dyn FnMut() -> bool,
) {
    if has_link {
        while poll() && !tx_complete() {}
    }

    for ch in 0..channels {
        let off = dma_channel_reg(ch, DMA_CH_TCR);
        write_field(acc, off, DMA_CH_TCR_ST.0, DMA_CH_TCR_ST.1, 0);
    }

    for q in 0..queues {
        if prepare_tx_stop(acc, snps_ver, q, has_link, poll) == Drained::TimedOut {
            warn!("[xgbe] timed out waiting for transmit queue {} to empty", q);
        }
    }

    for q in 0..queues {
        write_field(
            acc,
            mtl_queue_reg(q, MTL_Q_TQOMR),
            MTL_Q_TQOMR_TXQEN.0,
            MTL_Q_TQOMR_TXQEN.1,
            MTL_Q_DISABLED,
        );
    }

    write_field(acc, MAC_TCR, MAC_TCR_TE.0, MAC_TCR_TE.1, 0);
}

/// Bring receive down: the MAC **first**, then the queues, then the channels.
///
/// The opposite direction to [`disable_tx`], and to [`enable_rx`]. Nothing
/// new may be accepted while the path behind it is being torn down, so the
/// receiver is shut before anything else moves.
pub fn disable_rx(
    acc: &dyn MmioAccess,
    channels: u32,
    queues: u32,
    poll: &mut dyn FnMut() -> bool,
) {
    write_field(acc, MAC_RCR, MAC_RCR_CST.0, MAC_RCR_CST.1, 0);
    write_field(acc, MAC_RCR, MAC_RCR_ACS.0, MAC_RCR_ACS.1, 0);
    write_field(acc, MAC_RCR, MAC_RCR_RE.0, MAC_RCR_RE.1, 0);

    for q in 0..queues {
        if prepare_rx_stop(acc, q, poll) == Drained::TimedOut {
            warn!("[xgbe] timed out waiting for receive queue {} to empty", q);
        }
    }

    acc.write32(MAC_RQC0R, 0);

    for ch in 0..channels {
        let off = dma_channel_reg(ch, DMA_CH_RCR);
        write_field(acc, off, DMA_CH_RCR_SR.0, DMA_CH_RCR_SR.1, 0);
    }
}

// ——— Handing the chip its rings ———
//
// The last configuration step before anything moves: telling each channel
// where its descriptor ring is, how long it is, and --- for receive only ---
// how far into it the driver has filled.

/// `DMA_CH_RCR_RBSZ`, index **1**, width 14: the receive buffer size.
///
/// Index 1, not 0, so the value sits shifted by one bit in the register. The
/// bottom bit belongs to `SR` (start receive), which is why a buffer size
/// written at index 0 would start the engine as a side effect of being an odd
/// number of bytes.
pub const DMA_CH_RCR_RBSZ: (u32, u32) = (1, 14);

/// Program a transmit ring into its channel.
///
/// `ring_paddr` is the physical address of descriptor 0, `count` the number
/// of descriptors.
///
/// Two things to get right, and neither reports anything when wrong:
///
/// * The length register takes **`count - 1`**: it is a last index, not a
///   count. Writing the count gives the engine one descriptor more than
///   exists, which it will happily fetch from whatever follows the ring.
/// * The address halves go in **high first** (see [`DMA_CH_TDLR`]).
///
/// There is deliberately **no tail pointer** here. A transmit ring starts
/// empty: the driver has nothing for the engine to fetch, and the tail is
/// written later, when a frame is queued. Writing one now would point the
/// engine at descriptors the driver has not filled in.
pub fn program_tx_ring(
    acc: &dyn MmioAccess,
    channel: u32,
    ring_paddr: u64,
    count: u32,
) -> DeviceResult<()> {
    if count == 0 {
        return Err(DeviceError::InvalidParam);
    }
    let (hi, lo) = split_ring_addr(ring_paddr);

    acc.write32(dma_channel_reg(channel, DMA_CH_TDRLR), count - 1);
    acc.write32(dma_channel_reg(channel, DMA_CH_TDLR_HI), hi);
    acc.write32(dma_channel_reg(channel, DMA_CH_TDLR_LO), lo);
    Ok(())
}

/// Program a receive ring into its channel, and set the buffer size.
///
/// Same two rules as [`program_tx_ring`], plus the one that makes receive
/// different: it **does** get a tail pointer, and the tail points at the
/// address of the **last descriptor**, not one past the end.
///
/// That is the opposite of the usual convention and the reason it is spelled
/// out here. The tail is "the last descriptor the driver owns"; a receive
/// ring starts with every descriptor handed to the engine, so the tail is the
/// last one. One past the end would be `ring_paddr + count * 16`, an address
/// outside the ring, and the engine would fetch a descriptor from whatever
/// the allocator put there next.
pub fn program_rx_ring(
    acc: &dyn MmioAccess,
    channel: u32,
    ring_paddr: u64,
    count: u32,
    buffer_size: u32,
) -> DeviceResult<()> {
    if count == 0 || buffer_size == 0 || buffer_size >= (1 << DMA_CH_RCR_RBSZ.1) {
        return Err(DeviceError::InvalidParam);
    }
    let (hi, lo) = split_ring_addr(ring_paddr);

    write_field(
        acc,
        dma_channel_reg(channel, DMA_CH_RCR),
        DMA_CH_RCR_RBSZ.0,
        DMA_CH_RCR_RBSZ.1,
        buffer_size,
    );

    acc.write32(dma_channel_reg(channel, DMA_CH_RDRLR), count - 1);
    acc.write32(dma_channel_reg(channel, DMA_CH_RDLR_HI), hi);
    acc.write32(dma_channel_reg(channel, DMA_CH_RDLR_LO), lo);

    let last = ring_paddr + ((count - 1) as u64) * core::mem::size_of::<Desc>() as u64;
    acc.write32(dma_channel_reg(channel, DMA_CH_RDTR_LO), last as u32);
    Ok(())
}

/// Whether descriptor `index` should ask for an interrupt, given the receive
/// coalescing settings. `xgbe_rx_desc_reset`.
///
/// With no coalescing at all, every descriptor interrupts. With a frame count
/// set, every `rx_frames`-th one does --- counted on `index + 1`, so the
/// *last* descriptor of each group carries the flag and not the first.
#[inline]
pub const fn rx_desc_wants_interrupt(index: u32, rx_usecs: u32, rx_frames: u32) -> bool {
    if rx_usecs == 0 && rx_frames == 0 {
        return true;
    }
    rx_frames != 0 && (index + 1).is_multiple_of(rx_frames)
}

/// Build a receive descriptor, ready except for its `OWN` bit.
///
/// A receive descriptor carries **two** buffers: the header buffer in
/// `desc0`/`desc1` and the payload buffer in `desc2`/`desc3`, each as a full
/// 64-bit address in low-then-high order. Which means all four words are
/// address --- and `desc3`, the high half of the payload address, is **also**
/// where `OWN` and `INTE` live.
///
/// So the control bits are OR'd into an address word. It works because a
/// physical address has nothing in its top bits, but it has two consequences
/// worth stating: writing `desc3` as "just the address" clears `OWN`, and
/// writing it as "just the flags" destroys the payload address.
///
/// The `OWN` bit is left off here on purpose. Linux sets it in a separate
/// write with a barrier either side, because the receive engine may already
/// be running over this ring: everything else must be visible before the
/// descriptor is handed over, and the handover must be visible before
/// anything else happens. Returning the descriptor without `OWN` makes that
/// ordering the caller's explicit step instead of a detail buried in here.
#[inline]
pub fn rx_desc_init(hdr_dma: u64, buf_dma: u64, wants_interrupt: bool) -> Desc {
    let (hdr_hi, hdr_lo) = split_ring_addr(hdr_dma);
    let (buf_hi, buf_lo) = split_ring_addr(buf_dma);
    let mut desc3 = buf_hi;
    if wants_interrupt {
        desc3 |= DESC3_CTXT; // RX_NORMAL_DESC3_INTE, the same bit 30.
    }
    Desc {
        desc0: hdr_lo,
        desc1: hdr_hi,
        desc2: buf_lo,
        desc3,
    }
}

// ——— Putting a frame into the transmit ring ———

/// `TX_NORMAL_DESC2_HL_B1L`, index 0, width 14: the length of *this*
/// fragment. See [`TX_DESC3_FL`] for the other length in the same
/// descriptor.
pub const TX_DESC2_HL_B1L: (u32, u32) = (0, 14);
/// `TX_NORMAL_DESC2_VTIR`, index 14, width 2: what to do with a VLAN tag.
pub const TX_DESC2_VTIR: (u32, u32) = (14, 2);
/// `TX_NORMAL_DESC2_VLAN_INSERT`, the `VTIR` value that inserts the tag the
/// context descriptor carried.
pub const TX_DESC2_VLAN_INSERT: u32 = 0x2;
/// `TX_NORMAL_DESC2_TTSE`, index 30, width 1: take a transmit timestamp.
pub const TX_DESC2_TTSE: (u32, u32) = (30, 1);
/// `TX_NORMAL_DESC2_IC`, index 31, width 1: raise an interrupt once this
/// descriptor completes.
///
/// Bit 31 of `desc2`, which is not bit 31 of `desc3`: that one is `OWN`. The
/// two words put entirely unrelated meanings in the same position, so a field
/// written into the wrong word still lands on a real bit and the chip obeys
/// it.
pub const TX_DESC2_IC: (u32, u32) = (31, 1);

/// `TX_NORMAL_DESC3_FL`, index 0, width 15: the length of the WHOLE frame,
/// written on the first descriptor only. Already masked by
/// [`tx_desc3_frame_len`].
pub const TX_DESC3_FL: (u32, u32) = (0, 15);
/// `TX_NORMAL_DESC3_CIC`, index 16, width 2: checksum insertion control.
pub const TX_DESC3_CIC: (u32, u32) = (16, 2);
/// `TX_NORMAL_DESC3_CIC` fully on: header and payload checksums both.
pub const TX_DESC3_CIC_FULL: u32 = 0x3;
/// `TX_NORMAL_DESC3_CPC`, index 26, width 2: CRC and pad control. Zero means
/// insert both, which is what an ordinary frame wants, so this driver never
/// writes anything else --- but the field is named here because a zero is
/// only obviously right once you know the field exists.
pub const TX_DESC3_CPC: (u32, u32) = (26, 2);

/// One fragment of an outgoing frame: a bus address and how many bytes live
/// at it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxFragment {
    /// The address the engine will read from.
    pub dma: u64,
    /// How many bytes to read. Must fit [`TX_DESC2_HL_B1L`] and not be zero.
    pub len: u32,
}

/// A frame about to be handed to the transmit engine.
#[derive(Debug, Clone, Copy)]
pub struct TxFrame<'a> {
    /// The fragments, in wire order. The first becomes the first descriptor.
    pub fragments: &'a [TxFragment],
    /// The total length of the frame, for [`TX_DESC3_FL`]. Kept separate from
    /// the sum of the fragment lengths because the engine reads it from one
    /// place and the fragments from another, and
    /// [`build_tx_frame`] checks that the two agree rather than deriving one
    /// from the other.
    pub total_len: u32,
    /// Ask the chip to compute the checksums.
    pub checksum_offload: bool,
    /// Raise an interrupt when the frame has gone out.
    pub wants_interrupt: bool,
}

/// Write a frame's descriptors into `ring`, starting at `start` and wrapping,
/// and return the index of the next free descriptor. `xgbe_dev_xmit` for an
/// ordinary frame: no segmentation offload, no VLAN insertion, no timestamp,
/// so no context descriptor and no second pass over `desc3`'s bottom bits.
///
/// The first descriptor comes back WITHOUT its `OWN` bit, on purpose. Handing
/// it over is [`hand_tx_frame_to_dma`]'s job, after the caller's barrier: an
/// engine that is already running would otherwise be free to start on a frame
/// whose later fragments have not been written yet. Every other descriptor is
/// handed over as it is built, because none of them means anything until the
/// first one says so.
pub fn build_tx_frame(ring: &mut [Desc], start: usize, frame: &TxFrame) -> DeviceResult<usize> {
    let count = frame.fragments.len();
    if count == 0 || count > ring.len() || start >= ring.len() {
        return Err(DeviceError::InvalidParam);
    }
    if frame.total_len == 0 || frame.total_len >= (1 << TX_DESC3_FL.1) {
        return Err(DeviceError::InvalidParam);
    }
    let mut summed: u32 = 0;
    for fragment in frame.fragments {
        if fragment.len == 0 || fragment.len >= (1 << TX_DESC2_HL_B1L.1) {
            return Err(DeviceError::InvalidParam);
        }
        summed = summed
            .checked_add(fragment.len)
            .ok_or(DeviceError::InvalidParam)?;
    }
    if summed != frame.total_len {
        return Err(DeviceError::InvalidParam);
    }

    let len = ring.len();
    let csum = if frame.checksum_offload {
        set_bits(0, TX_DESC3_CIC.0, TX_DESC3_CIC.1, TX_DESC3_CIC_FULL)
    } else {
        0
    };

    for (i, fragment) in frame.fragments.iter().enumerate() {
        let (hi, lo) = split_ring_addr(fragment.dma);
        let mut desc2 = set_bits(0, TX_DESC2_HL_B1L.0, TX_DESC2_HL_B1L.1, fragment.len);
        let mut desc3 = csum;
        if i == 0 {
            // FL and FD go on the first descriptor only: FL is the whole
            // frame, while desc2's length is just this fragment.
            desc3 |= DESC3_FD | tx_desc3_frame_len(frame.total_len);
        } else {
            // ... and OWN goes on every descriptor BUT the first.
            desc3 |= DESC3_OWN;
        }
        if i == count - 1 {
            desc3 |= DESC3_LD;
            if frame.wants_interrupt {
                // IC rides the LAST descriptor, and it lives in desc2.
                desc2 = set_bits(desc2, TX_DESC2_IC.0, TX_DESC2_IC.1, 1);
            }
        }
        ring[(start + i) % len] = Desc {
            desc0: lo,
            desc1: hi,
            desc2,
            desc3,
        };
    }

    Ok((start + count) % len)
}

/// Set `OWN` on the frame's first descriptor, which is what actually hands the
/// frame to the engine. The caller issues its write barrier between
/// [`build_tx_frame`] and this call; nothing here can do it for them, because
/// the ring is ordinary memory and the ordering that matters is the one the
/// caller's CPU publishes.
#[inline]
pub fn hand_tx_frame_to_dma(ring: &mut [Desc], start: usize) {
    ring[start].desc3 |= DESC3_OWN;
}

/// Poke the transmit engine by writing the address of the next FREE
/// descriptor to the tail pointer. `xgbe_tx_start_xmit`.
///
/// Note which descriptor that is: the one AFTER the frame, not the frame's
/// last. The transmit tail is a stop sign --- the engine runs up to it and
/// halts --- while the receive tail of [`program_rx_ring`] points at the last
/// descriptor the driver owns. Same register name on two rings, opposite
/// convention.
#[inline]
pub fn kick_tx(acc: &dyn MmioAccess, channel: u32, next_free_paddr: u64) {
    acc.write32(
        dma_channel_reg(channel, DMA_CH_TDTR_LO),
        next_free_paddr as u32,
    );
}

/// The bus address of descriptor `index` in a ring that starts at
/// `ring_paddr`. Both tail pointers want an address, not an index.
#[inline]
pub const fn desc_paddr(ring_paddr: u64, index: usize) -> u64 {
    ring_paddr + (index as u64) * core::mem::size_of::<Desc>() as u64
}

// ——— Reading back what the chip received ———

/// `RX_NORMAL_DESC0_OVT`, index 0, width 16: the outer VLAN tag.
///
/// On the way IN, `desc0` was the low half of the header buffer's address.
/// The chip overwrites the address words with results, so every field below
/// is only meaningful once `OWN` has gone back to the driver.
pub const RX_DESC0_OVT: (u32, u32) = (0, 16);
/// `RX_NORMAL_DESC2_TNP`, index 11, width 1: the frame arrived tunnelled.
pub const RX_DESC2_TNP: (u32, u32) = (11, 1);
/// `RX_NORMAL_DESC3_ETLT`, index 16, width 4: "error type / length type".
/// One field, two jobs, told apart by [`RX_DESC3_ES`] --- and not the way
/// round the name suggests. See [`rx_frame_verdict`].
pub const RX_DESC3_ETLT: (u32, u32) = (16, 4);
/// `RX_NORMAL_DESC3_ES`, index 15, width 1: the error summary.
pub const RX_DESC3_ES: (u32, u32) = (15, 1);
/// `RX_NORMAL_DESC3_L34T`, index 20, width 4: which layer-3 and layer-4
/// protocols the chip recognised.
pub const RX_DESC3_L34T: (u32, u32) = (20, 4);
/// `RX_NORMAL_DESC3_RSV`, index 26, width 1: `desc1` holds an RSS hash.
pub const RX_DESC3_RSV: (u32, u32) = (26, 1);
/// `RX_NORMAL_DESC3_CDA`, index 27, width 1: a context descriptor follows
/// this one.
pub const RX_DESC3_CDA: (u32, u32) = (27, 1);

/// `ETLT` 0x09 with no error: the frame carried a VLAN tag, and `desc0`'s
/// [`RX_DESC0_OVT`] holds it. A value in the field named for error types
/// that is not an error at all.
pub const ETLT_VLAN_TAG: u32 = 0x09;
/// `ETLT` 0x05 with an error: the IP header checksum was wrong.
pub const ETLT_IP_HEADER_CSUM: u32 = 0x05;
/// `ETLT` 0x06 with an error: the payload checksum was wrong.
pub const ETLT_PAYLOAD_CSUM: u32 = 0x06;

/// `RX_DESC3_L34T_IPV4_TCP`.
pub const L34T_IPV4_TCP: u32 = 1;
/// `RX_DESC3_L34T_IPV4_UDP`.
pub const L34T_IPV4_UDP: u32 = 2;
/// `RX_DESC3_L34T_IPV6_TCP`.
pub const L34T_IPV6_TCP: u32 = 9;
/// `RX_DESC3_L34T_IPV6_UDP`.
pub const L34T_IPV6_UDP: u32 = 10;

/// Whether the RSS hash covers layer 4 as well as layer 3, from `L34T`. The
/// four values that name a transport protocol hash over the ports too;
/// everything else, ICMP and the two "unknown"s included, hashes over the
/// addresses alone.
#[inline]
pub const fn rss_hash_covers_ports(l34t: u32) -> bool {
    matches!(
        l34t,
        L34T_IPV4_TCP | L34T_IPV4_UDP | L34T_IPV6_TCP | L34T_IPV6_UDP
    )
}

/// What the chip thought of a received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxVerdict {
    /// Nothing wrong with it.
    Clean,
    /// A checksum was wrong, but the frame itself arrived intact. Linux hands
    /// this one up the stack with the hardware-checksum flag cleared, rather
    /// than dropping it: the frame is real and something above may still want
    /// it.
    ChecksumBad,
    /// The frame is damaged. Drop it.
    FrameBad,
}

/// Read `ES` and `ETLT` together and say what happened.
///
/// The trap is the condition Linux writes as `if (!err || !etlt)`: a frame is
/// fine when the error summary is clear **OR** when the error type is zero.
/// An error summary on its own is NOT an error --- `ETLT` has to name one
/// too. Reading `ES` alone, which is what its name invites, drops good
/// frames.
///
/// This driver has no tunnelling support, so the two `ETLT` values that only
/// mean a tunnel checksum error on a tunnelled frame (0x09 and 0x0a) land in
/// [`RxVerdict::FrameBad`] here, which is exactly where Linux puts them when
/// the frame is not tunnelled.
#[inline]
pub const fn rx_frame_verdict(desc3: u32) -> RxVerdict {
    let err = get_bits(desc3, RX_DESC3_ES.0, RX_DESC3_ES.1);
    let etlt = get_bits(desc3, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1);
    if err == 0 || etlt == 0 {
        return RxVerdict::Clean;
    }
    match etlt {
        ETLT_IP_HEADER_CSUM | ETLT_PAYLOAD_CSUM => RxVerdict::ChecksumBad,
        _ => RxVerdict::FrameBad,
    }
}

/// Everything a written-back normal receive descriptor says.
///
/// The `Option`s are not politeness: each of those fields is only meaningful
/// on one end of a multi-descriptor frame, and the header length and the
/// packet length sit in the *same descriptor* under opposite conditions. A
/// struct of plain integers would invite reading a length off the middle of a
/// frame, where the field holds whatever the chip last left there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxDescriptor {
    /// `FD`: the frame starts here.
    pub first: bool,
    /// `LD`: the frame ends here.
    pub last: bool,
    /// `HL`, valid on the first descriptor only: how many bytes went into the
    /// header buffer when split-header receive is on. Zero means the whole
    /// frame went into the payload buffer.
    pub header_len: Option<u32>,
    /// `PL`, valid on the last descriptor only: the length of the whole
    /// frame.
    pub packet_len: Option<u32>,
    /// Valid on the last descriptor only, for the same reason.
    pub verdict: Option<RxVerdict>,
    /// The outer VLAN tag, when the frame carried one.
    pub vlan_tag: Option<u16>,
    /// `desc1`, when `RSV` says it is a hash rather than the address half it
    /// used to be.
    pub rss_hash: Option<u32>,
    /// Whether that hash covers the layer-4 ports.
    pub rss_covers_ports: bool,
    /// `CDA`: the next descriptor is a context descriptor, not another
    /// fragment.
    pub context_next: bool,
}

/// What came back in a receive descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxRead {
    /// `OWN` is still set: the chip has not finished with this descriptor and
    /// nothing in it may be read. `xgbe_dev_read` returning 1.
    NotYours,
    /// A context descriptor, which carries a timestamp rather than a frame.
    Context,
    /// A normal descriptor.
    Normal(RxDescriptor),
}

/// Parse a written-back receive descriptor. `xgbe_dev_read`.
///
/// The caller issues its read barrier between seeing `OWN` clear and using
/// anything else, which is why the `OWN` test comes first and alone: every
/// other word of the descriptor was written by the chip and is only settled
/// once ownership has come back.
pub fn read_rx_desc(desc: &Desc) -> RxRead {
    if desc_owned_by_dma(desc.desc3) {
        return RxRead::NotYours;
    }
    if desc_is_context(desc.desc3) {
        return RxRead::Context;
    }

    let first = desc.desc3 & DESC3_FD != 0;
    let last = desc.desc3 & DESC3_LD != 0;
    let verdict = if last {
        Some(rx_frame_verdict(desc.desc3))
    } else {
        None
    };
    let vlan_tag = match verdict {
        Some(RxVerdict::Clean)
            if get_bits(desc.desc3, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1) == ETLT_VLAN_TAG =>
        {
            Some(get_bits(desc.desc0, RX_DESC0_OVT.0, RX_DESC0_OVT.1) as u16)
        }
        _ => None,
    };
    let rss_hash = if get_bits(desc.desc3, RX_DESC3_RSV.0, RX_DESC3_RSV.1) != 0 {
        Some(desc.desc1)
    } else {
        None
    };

    RxRead::Normal(RxDescriptor {
        first,
        last,
        header_len: if first {
            Some(rx_header_len(desc.desc2))
        } else {
            None
        },
        packet_len: if last {
            Some(rx_packet_len(desc.desc3))
        } else {
            None
        },
        verdict,
        vlan_tag,
        rss_hash,
        rss_covers_ports: rss_hash.is_some()
            && rss_hash_covers_ports(get_bits(desc.desc3, RX_DESC3_L34T.0, RX_DESC3_L34T.1)),
        context_next: get_bits(desc.desc3, RX_DESC3_CDA.0, RX_DESC3_CDA.1) != 0,
    })
}

/// Whether the frame arrived through a tunnel. `RX_NORMAL_DESC2_TNP`.
///
/// Nothing in this driver acts on it yet --- tunnel offload needs the two
/// `ETLT` values that only mean a checksum error on a tunnelled frame, and
/// those are deliberately left as [`RxVerdict::FrameBad`] above. It is named
/// so that the bit does not look free when somebody comes to use `desc2` for
/// something else.
#[inline]
pub const fn rx_is_tunnelled(desc2: u32) -> bool {
    get_bits(desc2, RX_DESC2_TNP.0, RX_DESC2_TNP.1) != 0
}

// ——— The probe ———

/// Read a 32-bit register. `base` is a mapped virtual address.
///
/// # Safety
/// `base + off` must be inside a mapping of the device's registers.
#[inline]
unsafe fn read32(base: usize, off: usize) -> u32 {
    core::ptr::read_volatile((base + off) as *const u32)
}

pub struct XgbeDriverPci;

impl PciDriver for XgbeDriverPci {
    fn name(&self) -> &str {
        "xgbe"
    }

    fn matched(&self, vendor_id: u16, device_id: u16) -> bool {
        xgbe_matched(vendor_id, device_id)
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        _irq: Option<usize>,
    ) -> DeviceResult<Device> {
        let variant = variant_for(dev.id.device_id).ok_or(DeviceError::NotSupported)?;

        let Some(BAR::Memory(xgmac_addr, xgmac_len, _, _)) = dev.bars[XGMAC_BAR] else {
            warn!("[xgbe] BAR{} is not memory space", XGMAC_BAR);
            return Err(DeviceError::NotSupported);
        };
        if (xgmac_len as usize) < MIN_XGMAC_BAR_LEN {
            // Reading the port properties out of a BAR that does not reach
            // them returns whatever is mapped after it.
            warn!(
                "[xgbe] BAR{} is {:#x} bytes; the port property block alone sits at {:#x}",
                XGMAC_BAR, xgmac_len, MAC_PROP_OFFSET
            );
            return Err(DeviceError::NotSupported);
        }
        if !matches!(dev.bars[XPCS_BAR], Some(BAR::Memory(..))) {
            warn!(
                "[xgbe] BAR{} (the XPCS block) is not memory space",
                XPCS_BAR
            );
            return Err(DeviceError::NotSupported);
        }

        let xgmac = crate::bus::resolve_window(mapper, xgmac_addr as usize, xgmac_len as usize, 0);
        let xprop = xgmac + MAC_PROP_OFFSET;

        let (pp0, pp1, pp2, lo, hi) = unsafe {
            (
                read32(xprop, XP_PROP_0),
                read32(xprop, XP_PROP_1),
                read32(xprop, XP_PROP_2),
                read32(xprop, XP_MAC_ADDR_LO),
                read32(xprop, XP_MAC_ADDR_HI),
            )
        };

        // What the core says about itself. Read-only, and read before
        // anything is written, exactly as `xgbe_get_all_hw_features` does.
        let (mac_vr, hfr0, hfr1, hfr2) = unsafe {
            (
                read32(xgmac, MAC_VR),
                read32(xgmac, MAC_HWF0R),
                read32(xgmac, MAC_HWF1R),
                read32(xgmac, MAC_HWF2R),
            )
        };
        let version = decode_mac_version(mac_vr);
        let feat = decode_hw_features(hfr0, hfr1, hfr2);

        let counts = decode_counts(pp1);
        let (tx_fifo, rx_fifo) = decode_fifo_sizes(pp2, variant);

        info!(
            "[xgbe] {:?} at {:02x}:{:02x}.{} --- port {}, mode {}, speeds {:#07b}",
            variant,
            dev.loc.bus,
            dev.loc.device,
            dev.loc.function,
            port_id(pp0),
            port_mode(pp0),
            port_speeds(pp0)
        );
        info!(
            "[xgbe] {} tx / {} rx DMA channels, {} tx / {} rx hardware queues",
            counts.tx_channels, counts.rx_channels, counts.tx_queues, counts.rx_queues
        );
        info!(
            "[xgbe] {} bytes of tx FIFO, {} of rx, and the PCS is reached {}",
            tx_fifo,
            rx_fifo,
            if variant.pcs_through_smn() {
                "over the SMN"
            } else {
                "through the BAR1 window"
            }
        );
        // The core's own account of itself. Two lines, because this is the
        // input the datapath batches need and a log line that scrolled past
        // without the numbers is no use to anyone.
        info!(
            "[xgbe] XGMAC core {:#04x} (dev id {:#04x}, integrator rev {:#04x}), \
             {}-bit DMA addresses",
            version.snps, version.dev_id, version.user, feat.dma_width
        );
        info!(
            "[xgbe] the core reports {} tx / {} rx channels, {} tx / {} rx queues, \
             {} traffic classes, {} B tx FIFO / {} B rx FIFO",
            feat.tx_channels,
            feat.rx_channels,
            feat.tx_queues,
            feat.rx_queues,
            feat.tc_count,
            feat.tx_fifo_size,
            feat.rx_fifo_size
        );
        info!(
            "[xgbe] built with: tx csum {}, rx csum {}, TSO {}, RSS {}, DCB {}, \
             split header {}, VLAN hash {}, MDIO {}, 1GbE {}, timestamping {}",
            feat.tx_coe,
            feat.rx_coe,
            feat.tso,
            feat.rss,
            feat.dcb,
            feat.split_header,
            feat.vlan_hash,
            feat.sma,
            feat.gmii,
            feat.timestamp
        );
        info!(
            "[xgbe] filters: {} extra MAC addresses, {} L3/L4 filters, \
             hash table of {} entries ({})",
            feat.addn_mac,
            feat.l3l4_filters,
            feat.hash_table_size,
            if feat.hash_table_size == 0 {
                "no hash filter"
            } else {
                "hash filtering available"
            }
        );

        match mac_from_prop(lo, hi) {
            Some(mac) => info!(
                "[xgbe] station address {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            ),
            None => warn!(
                "[xgbe] no usable station address in the port properties \
                 (lo={:#010x} hi={:#010x})",
                lo, hi
            ),
        }

        // And stop here, on purpose. There is no datapath yet, and an
        // interface registered without one would be handed packets to drop in
        // silence while `ifconfig` showed a link that does not exist. The
        // device stays unclaimed, exactly as it is without this driver; what
        // the lines above print is the geometry the datapath has to be built
        // against, which is otherwise a guess.
        warn!(
            "[xgbe] identified, but there is no datapath for this controller yet: \
             leaving the device unclaimed"
        );
        Err(DeviceError::NotSupported)
    }
}

#[cfg(test)]
mod tests {
    //! Each constant below is pinned against the `amd-xgbe` source it was
    //! taken from, named in the assertion. None of this has run on the
    //! hardware, so the source IS the oracle: a test that merely repeated the
    //! constant back would prove nothing, and the point of writing the Linux
    //! name next to each value is that the next person can check it in one
    //! grep. Read from torvalds/linux at v6.12 and at master on 28-10-2026.

    use super::*;

    // ——— Identity ———

    #[test]
    fn the_three_controller_ids_map_to_their_version_data() {
        // `xgbe_pci_table` in xgbe-pci.c: 0x1458 -> xgbe_v2a,
        // 0x1459 -> xgbe_v2b, 0x1641 -> xgbe_v3 (master only).
        assert_eq!(variant_for(0x1458), Some(Variant::V2a));
        assert_eq!(variant_for(0x1459), Some(Variant::V2b));
        assert_eq!(variant_for(0x1641), Some(Variant::V3));
    }

    #[test]
    fn amds_other_devices_are_not_claimed_as_network_controllers() {
        // AMD's vendor id is on the host bridges, the IOMMU and the GPU too.
        // Claiming one of those takes a device away from the driver that can
        // actually drive it.
        assert!(xgbe_matched(0x1022, 0x1458));
        assert!(xgbe_matched(0x1022, 0x1459));
        assert!(xgbe_matched(0x1022, 0x1641));
        assert!(!xgbe_matched(0x1022, 0x2000), "that is the PCnet");
        assert!(!xgbe_matched(0x1022, 0x15d0), "that is the Raven bridge");
        assert!(!xgbe_matched(0x1022, 0x14b5), "the Yellow Carp bridge");
        assert!(!xgbe_matched(0x1022, 0x1630), "the Renoir bridge");
        assert!(!xgbe_matched(0x1022, 0x1122), "the P100a bridge");
        assert!(!xgbe_matched(0x1022, 0x1457), "one below the first id");
        assert!(!xgbe_matched(0x1022, 0x145a), "one above the second");
        // And another vendor's 0x1458 is somebody else's device.
        assert!(!xgbe_matched(0x8086, 0x1458));
        assert!(!xgbe_matched(0x10de, 0x1458));
    }

    #[test]
    fn the_fifo_ceiling_belongs_to_the_version_not_the_board() {
        // `tx_max_fifo_size` / `rx_max_fifo_size` in the three
        // `xgbe_version_data` structs.
        assert_eq!(Variant::V2a.max_fifo_size(), 229_376);
        assert_eq!(Variant::V2b.max_fifo_size(), 65_536);
        assert_eq!(Variant::V3.max_fifo_size(), 65_536);
    }

    #[test]
    fn only_the_v3_reaches_its_pcs_over_the_smn() {
        // `.xpcs_access = XGBE_XPCS_ACCESS_V3` on xgbe_v3 alone.
        assert!(!Variant::V2a.pcs_through_smn());
        assert!(!Variant::V2b.pcs_through_smn());
        assert!(Variant::V3.pcs_through_smn());
    }

    // ——— Resource layout ———

    #[test]
    fn the_two_bars_and_the_sub_blocks_sit_where_xgbe_h_says() {
        assert_eq!(XGMAC_BAR, 0, "XGBE_XGMAC_BAR");
        assert_eq!(XPCS_BAR, 1, "XGBE_XPCS_BAR");
        assert_ne!(XGMAC_BAR, XPCS_BAR, "swapping them maps each block twice");
        assert_eq!(MAC_PROP_OFFSET, 0x1d000, "XGBE_MAC_PROP_OFFSET");
        assert_eq!(I2C_CTRL_OFFSET, 0x1e000, "XGBE_I2C_CTRL_OFFSET");
        assert!(
            MIN_XGMAC_BAR_LEN > I2C_CTRL_OFFSET,
            "the minimum BAR must cover the last sub-block, not end at it"
        );
    }

    #[test]
    fn the_port_property_registers_sit_where_xgbe_common_h_says() {
        assert_eq!(XP_PROP_0, 0x0000);
        assert_eq!(XP_PROP_1, 0x0004);
        assert_eq!(XP_PROP_2, 0x0008);
        assert_eq!(XP_PROP_3, 0x000c);
        assert_eq!(XP_PROP_4, 0x0010);
        assert_eq!(XP_MAC_ADDR_LO, 0x0020);
        assert_eq!(XP_MAC_ADDR_HI, 0x0024);
        assert_eq!(XP_INT_EN, 0x0078);
        // The address pair is contiguous and in that order; reading them the
        // other way round gives a plausible-looking but wrong MAC.
        assert_eq!(XP_MAC_ADDR_HI, XP_MAC_ADDR_LO + 4);
    }

    // ——— The PCS window ———

    #[test]
    fn the_window_register_pair_comes_from_the_host_bridge() {
        // This is the one that looks like it should depend on the NIC. It
        // does not: `xgbe_pci_probe` looks up 0:0.0 and switches on THAT
        // device id. Keying it off the NIC lands on the wrong registers on
        // every Raven, Yellow Carp, Renoir and P100a part.
        assert_eq!(pcs_window_regs(0x1022, BRIDGE_RV), (0x1060, 0x1064));
        assert_eq!(pcs_window_regs(0x1022, BRIDGE_YC), (0x18060, 0x18064));
        assert_eq!(pcs_window_regs(0x1022, BRIDGE_RN), (0xf8078, 0xf807c));
        assert_eq!(pcs_window_regs(0x1022, BRIDGE_P100A), (0x8060, 0x8080));
        // An AMD bridge not in the table, and a non-AMD bridge, both fall
        // back to the default pair.
        assert_eq!(pcs_window_regs(0x1022, 0x1234), (0x9060, 0x9064));
        assert_eq!(pcs_window_regs(0x8086, BRIDGE_RV), (0x9060, 0x9064));
        assert_eq!(pcs_window_regs(0x8086, 0x0c00), (0x9060, 0x9064));
    }

    #[test]
    fn the_p100a_select_register_is_not_four_past_its_def() {
        // Every other pair is `def` and `def + 4`, so a driver that stores a
        // base and adds four works on three parts out of four and then reads
        // 0x8064 --- a different register entirely --- on a P100a.
        for (def, sel) in [
            PCS_V2_WINDOW,
            PCS_V2_RV_WINDOW,
            PCS_V2_YC_WINDOW,
            PCS_V3_RN_WINDOW,
        ] {
            assert_eq!(sel, def + 4, "pair at {:#x}", def);
        }
        let (def, sel) = PCS_P100A_WINDOW;
        assert_eq!(def, 0x8060);
        assert_eq!(sel, 0x8080);
        assert_ne!(sel, def + 4, "the P100a is the exception");
    }

    #[test]
    fn the_window_offset_is_in_sixty_four_byte_units() {
        // `xpcs_window = XPCS_GET_BITS(reg, PCS_V2_WINDOW_DEF, OFFSET) << 6`,
        // with OFFSET at bit 6, 14 bits wide. So the field is both extracted
        // from bit 6 AND shifted back up by 6; doing only one of the two is
        // off by a factor of 64 in either direction.
        assert_eq!(decode_pcs_window(0).base, 0);
        // A stored offset of 1 means 64 bytes in.
        assert_eq!(decode_pcs_window(1 << 6).base, 64);
        assert_eq!(decode_pcs_window(2 << 6).base, 128);
        // The widest the field goes.
        assert_eq!(decode_pcs_window(0x3fff << 6).base, 0x3fff << 6);
        // Bits below the field must not leak into it.
        assert_eq!(decode_pcs_window(0x3f).base, 0, "bits 5..0 are not OFFSET");
        // Nor bits above it.
        assert_eq!(
            decode_pcs_window(0xffff_ffff).base,
            0x3fff << 6,
            "OFFSET is 14 bits, not everything above bit 6"
        );
    }

    #[test]
    fn the_window_size_is_a_log2_exponent_biased_by_seven() {
        // `size = 1 << (size + 7)`, with SIZE at bit 2, 4 bits wide. A stored
        // 0 means 128 bytes, NOT 1 and not 0.
        assert_eq!(decode_pcs_window(0).size, 128);
        assert_eq!(decode_pcs_window(1 << 2).size, 256);
        assert_eq!(decode_pcs_window(2 << 2).size, 512);
        assert_eq!(decode_pcs_window(0xf << 2).size, 1 << 22);
        // The mask is the size minus one, so it is all-ones and can be ANDed
        // with an address.
        for reg in [0u32, 1 << 2, 7 << 2, 0xf << 2] {
            let w = decode_pcs_window(reg);
            assert_eq!(w.mask, w.size - 1);
            assert_eq!(w.mask & w.size, 0, "size must be a power of two");
        }
        // SIZE is four bits: bit 6 belongs to OFFSET, not to SIZE.
        assert_eq!(
            decode_pcs_window(0x3f).size,
            decode_pcs_window(0xf << 2).size
        );
    }

    #[test]
    fn the_two_window_fields_do_not_overlap() {
        // OFFSET starts at 6 and SIZE occupies 5..2, so a register with only
        // one of them set must decode the other as its zero value.
        let only_offset = decode_pcs_window(0x2a << 6);
        assert_eq!(only_offset.base, 0x2a << 6);
        assert_eq!(only_offset.size, 128, "SIZE reads as zero -> 128");
        let only_size = decode_pcs_window(0x5 << 2);
        assert_eq!(only_size.base, 0, "OFFSET reads as zero");
        assert_eq!(only_size.size, 1 << 12);
    }

    #[test]
    fn the_smn_address_of_a_v3_window_moves_with_the_port() {
        // `smn_base = PCS_RN_SMN_BASE_ADDR + PCS_RN_PORT_ADDR_SIZE * port_id`
        // and then the window register is added on top.
        assert_eq!(smn_window_address(0, 0), 0x11e0_0000);
        assert_eq!(smn_window_address(1, 0), 0x11f0_0000);
        assert_eq!(smn_window_address(2, 0), 0x1200_0000);
        let (def, _) = PCS_V3_RN_WINDOW;
        assert_eq!(smn_window_address(0, def), 0x11e0_0000 + 0xf8078);
        assert_eq!(smn_window_address(1, def), 0x11f0_0000 + 0xf8078);
    }

    // ——— The station address ———

    #[test]
    fn the_station_address_is_assembled_little_endian_across_both_words() {
        // lo carries bytes 0..3 and hi bytes 4..5, each from the BOTTOM of
        // its word up. Assembling it the other way round gives a MAC that
        // looks fine in a log and matches nothing on the wire.
        // The first byte is even on purpose: an odd one is a multicast
        // address and is refused by the check below, which is how this very
        // test caught its own first set of numbers.
        let mac = mac_from_prop(0x4433_2212, 0x8000_6655).expect("valid");
        assert_eq!(mac, [0x12, 0x22, 0x33, 0x44, 0x55, 0x66]);
    }

    #[test]
    fn an_address_without_the_valid_bit_is_refused() {
        // `XP_MAC_ADDR_HI_VALID` is bit 31.
        assert_eq!(mac_from_prop(0x4433_2212, 0x0000_6655), None);
        assert!(mac_from_prop(0x4433_2212, 0x8000_6655).is_some());
        // And the bit alone is not enough to make an empty address usable.
        assert_eq!(mac_from_prop(0, 0x8000_0000), None);
    }

    #[test]
    fn a_multicast_station_address_is_refused() {
        // The group bit is the low bit of the FIRST byte, which lives in the
        // low byte of `lo`. A board that reports one would have every switch
        // on the segment flood its traffic, and nothing would answer it.
        assert_eq!(mac_from_prop(0x4433_2201, 0x8000_6655), None);
        assert_eq!(mac_from_prop(0x4433_22ff, 0x8000_6655), None);
        assert!(
            mac_from_prop(0x4433_2202, 0x8000_6655).is_some(),
            "an even first byte is unicast and fine"
        );
        // The broadcast address is multicast too, so it is caught by the same
        // check rather than needing one of its own.
        assert_eq!(mac_from_prop(0xffff_ffff, 0x8000_ffff), None);
    }

    // ——— Port geometry ———

    #[test]
    fn the_counts_are_five_bit_fields_in_an_order_the_names_do_not_suggest() {
        // XP_PROP_1: MAX_TX_QUEUES at 0, MAX_RX_QUEUES at 8, MAX_TX_DMA at
        // 16, MAX_RX_DMA at 24. Reading channels and queues the other way
        // round gives a plausible number for each and the wrong ring count.
        let counts = decode_counts(0x0403_0201);
        assert_eq!(counts.tx_queues, 1, "bits 4..0");
        assert_eq!(counts.rx_queues, 2, "bits 12..8");
        assert_eq!(counts.tx_channels, 3, "bits 20..16");
        assert_eq!(counts.rx_channels, 4, "bits 28..24");

        // Each field is five bits, so the three bits above each one belong to
        // nothing and must not leak in.
        let all_ones = decode_counts(0xffff_ffff);
        assert_eq!(all_ones.tx_queues, 0x1f);
        assert_eq!(all_ones.rx_queues, 0x1f);
        assert_eq!(all_ones.tx_channels, 0x1f);
        assert_eq!(all_ones.rx_channels, 0x1f);
    }

    #[test]
    fn the_fifo_size_is_in_sixteen_kibibyte_units_and_the_version_clamps_it() {
        // The clamp is not an alternative to the register: a board can report
        // more FIFO than the controller generation can drive, and Linux takes
        // the min of the two.
        assert_eq!(decode_fifo_sizes(0, Variant::V2b), (0, 0));
        assert_eq!(decode_fifo_sizes(1, Variant::V2b), (16_384, 0));
        assert_eq!(
            decode_fifo_sizes(1 | (2 << 16), Variant::V2b),
            (16_384, 32_768)
        );
        // Four units is 64 KiB, exactly the V2b ceiling: not clamped.
        assert_eq!(
            decode_fifo_sizes(4 | (4 << 16), Variant::V2b),
            (65_536, 65_536)
        );
        // Five is over it, and comes back as the ceiling.
        assert_eq!(
            decode_fifo_sizes(5 | (5 << 16), Variant::V2b),
            (65_536, 65_536)
        );
        // The same board on a V2a is not clamped there.
        assert_eq!(
            decode_fifo_sizes(5 | (5 << 16), Variant::V2a),
            (81_920, 81_920)
        );
        // And a board reporting the whole 16-bit field does not overflow.
        let (tx, rx) = decode_fifo_sizes(0xffff_ffff, Variant::V2a);
        assert_eq!((tx, rx), (229_376, 229_376));
    }

    #[test]
    fn the_port_fields_are_at_their_own_offsets_in_prop_zero() {
        // PORT_ID at 0 (8 bits), PORT_MODE at 8 (4), PORT_SPEEDS at 22 (5).
        // Note the gap: bits 21..12 are something else and must not leak.
        let pp0 = 0x07 | (0x0a << 8) | (0x15 << 22);
        assert_eq!(port_id(pp0), 0x07);
        assert_eq!(port_mode(pp0), 0x0a);
        assert_eq!(port_speeds(pp0), 0x15);

        let all_ones = 0xffff_ffffu32;
        assert_eq!(port_id(all_ones), 0xff, "eight bits");
        assert_eq!(port_mode(all_ones), 0xf, "four bits");
        assert_eq!(port_speeds(all_ones), 0x1f, "five bits");

        // Bits between the mode and the speeds belong to neither.
        let between = 0x003f_f000;
        assert_eq!(port_mode(between), 0);
        assert_eq!(port_speeds(between), 0);
    }

    // ——— What the MAC says about itself ———
    //
    // These registers cannot be read here, so the only thing worth testing is
    // the decode, and the only way a decode test has any value is if it
    // pins the field's POSITION rather than repeating the arithmetic. So each
    // case below sets exactly one field in an otherwise zero register and
    // compares the whole structure against the all-zero decode with that one
    // field changed. A field written one bit off, or two fields overlapping,
    // fails --- which a per-field `assert_eq!` on its own value would not
    // catch.

    #[test]
    fn the_four_self_description_registers_are_where_xgbe_common_h_says() {
        assert_eq!(MAC_VR, 0x0110);
        assert_eq!(MAC_HWF0R, 0x011c);
        assert_eq!(MAC_HWF1R, 0x0120);
        assert_eq!(MAC_HWF2R, 0x0124);
        // And they are inside BAR0, well below the port property block, so
        // the BAR length the probe already insists on covers them.
        assert!(MAC_HWF2R < MAC_PROP_OFFSET);
    }

    #[test]
    fn get_bits_is_xgmac_get_bits() {
        // Linux: `(((var) >> (prefix##_##field##_INDEX)) & ...)`.
        assert_eq!(get_bits(0xffff_ffff, 0, 1), 1);
        assert_eq!(get_bits(0xffff_ffff, 18, 5), 0x1f);
        assert_eq!(get_bits(0x1234_5678, 16, 8), 0x34);
        assert_eq!(get_bits(0b1010_0000, 5, 1), 1);
        assert_eq!(get_bits(0b1010_0000, 6, 1), 0);
    }

    #[test]
    fn mac_vr_is_three_bytes_in_order() {
        // MAC_VR_SNPSVER index 0, DEVID index 8, USERVER index 16, all 8 wide.
        let v = decode_mac_version(0x00ab_cd30);
        assert_eq!(v.snps, 0x30);
        assert_eq!(v.dev_id, 0xcd);
        assert_eq!(v.user, 0xab);
        // The top byte of the register is not part of any of the three.
        assert_eq!(decode_mac_version(0xff00_0000), decode_mac_version(0));
    }

    #[test]
    fn the_snps_version_the_rest_of_the_driver_will_branch_on() {
        // `xgbe-dev.c` compares SNPSVER against exactly these three values,
        // so the byte has to come out of bit 0 and not bit 8.
        assert_eq!(decode_mac_version(0x0000_0030).snps, 0x30);
        assert_eq!(decode_mac_version(0x0000_0021).snps, 0x21);
        assert_eq!(decode_mac_version(0x0000_0020).snps, 0x20);
    }

    /// The all-zero decode: no optional block present, every zero-based count
    /// at one, the smallest FIFO, the narrowest DMA, no hash table.
    fn zeroed() -> HwFeatures {
        decode_hw_features(0, 0, 0)
    }

    #[test]
    fn an_all_zero_chip_still_has_one_of_each_zero_based_thing() {
        let f = zeroed();
        assert_eq!(f.rx_queues, 1);
        assert_eq!(f.tx_queues, 1);
        assert_eq!(f.rx_channels, 1);
        assert_eq!(f.tx_channels, 1);
        assert_eq!(f.tc_count, 1);
        // `1 << (0 + 7)`.
        assert_eq!(f.rx_fifo_size, 128);
        assert_eq!(f.tx_fifo_size, 128);
        // ADDR64 == 0 is 32 bits, HASHTBLSZ == 0 is no table.
        assert_eq!(f.dma_width, 32);
        assert_eq!(f.hash_table_size, 0);
        assert!(!f.tso && !f.rss && !f.tx_coe && !f.rx_coe);
    }

    #[test]
    fn every_field_of_hwf0r_sits_at_its_linux_index() {
        let z = zeroed();
        // MAC_HWF0R_GMIISEL_INDEX 1, _WIDTH 1
        assert_eq!(
            decode_hw_features(1 << 1, 0, 0),
            HwFeatures { gmii: true, ..z }
        );
        // VLHASH 4/1
        assert_eq!(
            decode_hw_features(1 << 4, 0, 0),
            HwFeatures {
                vlan_hash: true,
                ..z
            }
        );
        // SMASEL 5/1
        assert_eq!(
            decode_hw_features(1 << 5, 0, 0),
            HwFeatures { sma: true, ..z }
        );
        // RWKSEL 6/1
        assert_eq!(
            decode_hw_features(1 << 6, 0, 0),
            HwFeatures {
                remote_wake: true,
                ..z
            }
        );
        // MGKSEL 7/1
        assert_eq!(
            decode_hw_features(1 << 7, 0, 0),
            HwFeatures {
                magic_packet: true,
                ..z
            }
        );
        // MMCSEL 8/1
        assert_eq!(
            decode_hw_features(1 << 8, 0, 0),
            HwFeatures { mmc: true, ..z }
        );
        // ARPOFFSEL 9/1
        assert_eq!(
            decode_hw_features(1 << 9, 0, 0),
            HwFeatures {
                arp_offload: true,
                ..z
            }
        );
        // TSSEL 12/1
        assert_eq!(
            decode_hw_features(1 << 12, 0, 0),
            HwFeatures {
                timestamp: true,
                ..z
            }
        );
        // EEESEL 13/1
        assert_eq!(
            decode_hw_features(1 << 13, 0, 0),
            HwFeatures { eee: true, ..z }
        );
        // TXCOESEL 14/1
        assert_eq!(
            decode_hw_features(1 << 14, 0, 0),
            HwFeatures { tx_coe: true, ..z }
        );
        // RXCOESEL 16/1 --- note 15 is NOT a feature bit, and a decode that
        // read "the bit after TXCOESEL" would be wrong here.
        assert_eq!(
            decode_hw_features(1 << 16, 0, 0),
            HwFeatures { rx_coe: true, ..z }
        );
        assert_eq!(decode_hw_features(1 << 15, 0, 0), z);
        // ADDMACADRSEL 18/5
        assert_eq!(
            decode_hw_features(0x1f << 18, 0, 0),
            HwFeatures { addn_mac: 31, ..z }
        );
        // TSSTSSEL 25/2 --- two bits, and 25 is inside the 18/5 field's
        // neighbourhood, so an off-by-one on either shows up here.
        assert_eq!(
            decode_hw_features(0b11 << 25, 0, 0),
            HwFeatures { ts_src: 3, ..z }
        );
        // SAVLANINS 27/1
        assert_eq!(
            decode_hw_features(1 << 27, 0, 0),
            HwFeatures {
                sa_vlan_ins: true,
                ..z
            }
        );
        // VXN 29/1
        assert_eq!(
            decode_hw_features(1 << 29, 0, 0),
            HwFeatures { vxn: true, ..z }
        );
    }

    #[test]
    fn every_field_of_hwf1r_sits_at_its_linux_index() {
        let z = zeroed();
        // RXFIFOSIZE 0/5, translated: `1 << (11 + 7)` is 256 KiB.
        assert_eq!(
            decode_hw_features(0, 11, 0),
            HwFeatures {
                rx_fifo_size: 256 * 1024,
                ..z
            }
        );
        // TXFIFOSIZE 6/5
        assert_eq!(
            decode_hw_features(0, 11 << 6, 0),
            HwFeatures {
                tx_fifo_size: 256 * 1024,
                ..z
            }
        );
        // ADVTHWORD 13/1
        assert_eq!(
            decode_hw_features(0, 1 << 13, 0),
            HwFeatures {
                adv_ts_hi: true,
                ..z
            }
        );
        // ADDR64 14/2, translated to a width in bits.
        assert_eq!(
            decode_hw_features(0, 1 << 14, 0),
            HwFeatures { dma_width: 40, ..z }
        );
        // DCBEN 16/1
        assert_eq!(
            decode_hw_features(0, 1 << 16, 0),
            HwFeatures { dcb: true, ..z }
        );
        // SPHEN 17/1
        assert_eq!(
            decode_hw_features(0, 1 << 17, 0),
            HwFeatures {
                split_header: true,
                ..z
            }
        );
        // TSOEN 18/1
        assert_eq!(
            decode_hw_features(0, 1 << 18, 0),
            HwFeatures { tso: true, ..z }
        );
        // DBGMEMA 19/1
        assert_eq!(
            decode_hw_features(0, 1 << 19, 0),
            HwFeatures {
                dma_debug: true,
                ..z
            }
        );
        // RSSEN 20/1
        assert_eq!(
            decode_hw_features(0, 1 << 20, 0),
            HwFeatures { rss: true, ..z }
        );
        // NUMTC 21/3, incremented: 7 encodes eight traffic classes.
        assert_eq!(
            decode_hw_features(0, 0b111 << 21, 0),
            HwFeatures { tc_count: 8, ..z }
        );
        // HASHTBLSZ 24/3, translated.
        assert_eq!(
            decode_hw_features(0, 3 << 24, 0),
            HwFeatures {
                hash_table_size: 256,
                ..z
            }
        );
        // L3L4FNUM 27/4
        assert_eq!(
            decode_hw_features(0, 0xf << 27, 0),
            HwFeatures {
                l3l4_filters: 15,
                ..z
            }
        );
    }

    #[test]
    fn every_field_of_hwf2r_sits_at_its_linux_index() {
        let z = zeroed();
        // RXQCNT 0/4, incremented.
        assert_eq!(
            decode_hw_features(0, 0, 0xf),
            HwFeatures { rx_queues: 16, ..z }
        );
        // TXQCNT 6/4 --- bits 4 and 5 belong to nobody, so a packed-fields
        // reading of this register would put it at 4 and be wrong.
        assert_eq!(
            decode_hw_features(0, 0, 0xf << 6),
            HwFeatures { tx_queues: 16, ..z }
        );
        assert_eq!(decode_hw_features(0, 0, 0b11 << 4), z);
        // RXCHCNT 12/4
        assert_eq!(
            decode_hw_features(0, 0, 0xf << 12),
            HwFeatures {
                rx_channels: 16,
                ..z
            }
        );
        // TXCHCNT 18/4
        assert_eq!(
            decode_hw_features(0, 0, 0xf << 18),
            HwFeatures {
                tx_channels: 16,
                ..z
            }
        );
        // PPSOUTNUM 24/3, clamped to XGBE_MAX_PPS_OUT.
        assert_eq!(
            decode_hw_features(0, 0, 1 << 24),
            HwFeatures {
                pps_outputs: 1,
                ..z
            }
        );
        // AUXSNAPNUM 28/3, clamped to XGBE_MAX_AUX_SNAP.
        assert_eq!(
            decode_hw_features(0, 0, 1 << 28),
            HwFeatures {
                aux_snapshots: 1,
                ..z
            }
        );
    }

    #[test]
    fn the_hash_table_size_is_an_index_not_a_count() {
        // `switch (hw_feat->hash_table_size)` in xgbe_get_all_hw_features.
        for (encoded, entries) in [(0u32, 0u32), (1, 64), (2, 128), (3, 256)] {
            assert_eq!(
                decode_hw_features(0, encoded << 24, 0).hash_table_size,
                entries,
                "HASHTBLSZ {} should mean {} entries",
                encoded,
                entries
            );
        }
        // Five, six and seven have no meaning; Linux's switch leaves them as
        // the raw value, which would be read as a table of five entries. We
        // read them as no table, which is the arm that makes `xgbe-dev.c`
        // skip hash filtering altogether instead of programming nonsense.
        for encoded in [4u32, 5, 6, 7] {
            assert_eq!(decode_hw_features(0, encoded << 24, 0).hash_table_size, 0);
        }
    }

    #[test]
    fn the_dma_width_is_an_index_into_three_widths() {
        // `switch (hw_feat->dma_width)`: 0 -> 32, 1 -> 40, 2 -> 48,
        // default -> 32.
        for (encoded, bits) in [(0u32, 32u32), (1, 40), (2, 48), (3, 32)] {
            assert_eq!(decode_hw_features(0, encoded << 14, 0).dma_width, bits);
        }
    }

    #[test]
    fn more_pps_outputs_than_the_driver_has_room_for_are_clamped() {
        // Linux warns and clamps to XGBE_MAX_PPS_OUT / XGBE_MAX_AUX_SNAP = 4.
        assert_eq!(MAX_PPS_OUT, 4);
        assert_eq!(MAX_AUX_SNAP, 4);
        let f = decode_hw_features(0, 0, (0b111 << 24) | (0b111 << 28));
        assert_eq!(f.pps_outputs, 4);
        assert_eq!(f.aux_snapshots, 4);
        // Four itself is not clamped away.
        let f = decode_hw_features(0, 0, (4 << 24) | (4 << 28));
        assert_eq!(f.pps_outputs, 4);
        assert_eq!(f.aux_snapshots, 4);
    }

    #[test]
    fn an_impossible_fifo_size_saturates_instead_of_wrapping() {
        // The field is 5 bits, so a chip can claim `1 << 38` bytes. Linux
        // wraps it in an `unsigned int`; in a debug build the same shift
        // panics, which would turn a bad register read into a dead boot.
        assert_eq!(fifo_bytes(0), 128);
        assert_eq!(fifo_bytes(11), 256 * 1024);
        assert_eq!(fifo_bytes(24), 1 << 31);
        assert_eq!(fifo_bytes(25), u32::MAX);
        assert_eq!(fifo_bytes(31), u32::MAX);
        // Which is what the register path must do too, in both directions.
        let f = decode_hw_features(0, 0x1f | (0x1f << 6), 0);
        assert_eq!(f.rx_fifo_size, u32::MAX);
        assert_eq!(f.tx_fifo_size, u32::MAX);
    }

    #[test]
    fn a_plausible_v2a_report_decodes_whole() {
        // Not a reading from a chip --- nobody here has one. It is the three
        // registers a part with eight channels, eight queues, eight traffic
        // classes, 128 KiB of FIFO each way, 40-bit DMA and the usual
        // offloads would present, assembled field by field from the indices
        // above. Its value is that it exercises every field at once: a decode
        // where two fields overlap can pass each single-field case and still
        // fail here.
        let hfr0 = (1 << 1)      // GMIISEL
            | (1 << 4)           // VLHASH
            | (1 << 5)           // SMASEL
            | (1 << 8)           // MMCSEL
            | (1 << 12)          // TSSEL
            | (1 << 14)          // TXCOESEL
            | (1 << 16)          // RXCOESEL
            | (31 << 18)         // ADDMACADRSEL
            | (1 << 27); // SAVLANINS
        let hfr1 = 10            // RXFIFOSIZE -> 128 KiB
            | (10 << 6)          // TXFIFOSIZE -> 128 KiB
            | (1 << 14)          // ADDR64 -> 40 bits
            | (1 << 16)          // DCBEN
            | (1 << 17)          // SPHEN
            | (1 << 18)          // TSOEN
            | (1 << 20)          // RSSEN
            | (7 << 21)          // NUMTC -> 8
            | (3 << 24)          // HASHTBLSZ -> 256
            | (8 << 27); // L3L4FNUM
        let hfr2 = 7             // RXQCNT -> 8
            | (7 << 6)           // TXQCNT -> 8
            | (7 << 12)          // RXCHCNT -> 8
            | (7 << 18); // TXCHCNT -> 8

        let f = decode_hw_features(hfr0, hfr1, hfr2);
        assert_eq!(f.tx_channels, 8);
        assert_eq!(f.rx_channels, 8);
        assert_eq!(f.tx_queues, 8);
        assert_eq!(f.rx_queues, 8);
        assert_eq!(f.tc_count, 8);
        assert_eq!(f.tx_fifo_size, 128 * 1024);
        assert_eq!(f.rx_fifo_size, 128 * 1024);
        assert_eq!(f.dma_width, 40);
        assert_eq!(f.hash_table_size, 256);
        assert_eq!(f.addn_mac, 31);
        assert_eq!(f.l3l4_filters, 8);
        assert!(f.gmii && f.vlan_hash && f.sma && f.mmc && f.timestamp);
        assert!(f.tx_coe && f.rx_coe && f.tso && f.rss && f.dcb && f.split_header);
        assert!(f.sa_vlan_ins);
        // And what such a part does NOT have, so the all-ones mistake fails.
        assert!(!f.eee && !f.arp_offload && !f.remote_wake && !f.magic_packet);
        assert!(!f.vxn && !f.adv_ts_hi && !f.dma_debug);
        assert_eq!(f.ts_src, 0);
        assert_eq!(f.pps_outputs, 0);
        assert_eq!(f.aux_snapshots, 0);

        // The FIFO the core has is not necessarily the FIFO the driver may
        // use: a V2a's ceiling is larger than this, a V2b's is smaller.
        assert!(f.tx_fifo_size <= Variant::V2a.max_fifo_size());
        assert!(f.tx_fifo_size > Variant::V2b.max_fifo_size());
    }

    // ——— The per-channel DMA register map ———

    #[test]
    fn the_channel_block_is_where_xgbe_common_h_says() {
        assert_eq!(DMA_CH_BASE, 0x3100);
        assert_eq!(DMA_CH_INC, 0x80);
        // Inside BAR0 and well below the port property block, like the MAC
        // registers.
        assert!(DMA_CH_BASE < MAC_PROP_OFFSET);
    }

    #[test]
    fn every_per_channel_register_is_at_its_linux_offset() {
        for (name, off, want) in [
            ("DMA_CH_CR", DMA_CH_CR, 0x00),
            ("DMA_CH_TCR", DMA_CH_TCR, 0x04),
            ("DMA_CH_RCR", DMA_CH_RCR, 0x08),
            ("DMA_CH_TDLR_HI", DMA_CH_TDLR_HI, 0x10),
            ("DMA_CH_TDLR_LO", DMA_CH_TDLR_LO, 0x14),
            ("DMA_CH_RDLR_HI", DMA_CH_RDLR_HI, 0x18),
            ("DMA_CH_RDLR_LO", DMA_CH_RDLR_LO, 0x1c),
            ("DMA_CH_TDTR_LO", DMA_CH_TDTR_LO, 0x24),
            ("DMA_CH_RDTR_LO", DMA_CH_RDTR_LO, 0x2c),
            ("DMA_CH_TDRLR", DMA_CH_TDRLR, 0x30),
            ("DMA_CH_RDRLR", DMA_CH_RDRLR, 0x34),
            ("DMA_CH_IER", DMA_CH_IER, 0x38),
            ("DMA_CH_RIWT", DMA_CH_RIWT, 0x3c),
            ("DMA_CH_SR", DMA_CH_SR, 0x60),
        ] {
            assert_eq!(off, want, "{} moved", name);
        }
    }

    #[test]
    fn the_channel_stride_leaves_a_hole_at_the_end_of_each_block() {
        // The highest register in a channel's block is DMA_CH_SR at 0x60, but
        // the stride is 0x80. Packing the blocks end to end --- the obvious
        // mistake --- would put channel 1's registers inside channel 0's
        // reach and read the wrong engine's status.
        assert!(DMA_CH_SR < DMA_CH_INC);
        assert_ne!(DMA_CH_SR + 4, DMA_CH_INC);
    }

    #[test]
    fn a_channels_block_is_its_index_times_the_stride() {
        // `channel->dma_regs = xgmac_regs + DMA_CH_BASE + DMA_CH_INC * i`.
        let base = 0xffff_8000_0000_0000usize;
        assert_eq!(channel_regs(base, 0), base + 0x3100);
        assert_eq!(channel_regs(base, 1), base + 0x3180);
        assert_eq!(channel_regs(base, 7), base + 0x3100 + 7 * 0x80);
        // And the blocks do not overlap, for the largest geometry HWF2R can
        // report (sixteen channels).
        for i in 0..16u32 {
            assert_eq!(
                channel_regs(base, i + 1) - channel_regs(base, i),
                DMA_CH_INC
            );
        }
    }

    #[test]
    fn the_list_address_registers_hold_the_high_half_first() {
        // This is the one that bites: TDLR_HI is at the LOWER address. A
        // `u64` written as two consecutive words would land the halves
        // swapped and point the descriptor fetch somewhere else.
        assert_eq!(DMA_CH_TDLR, (0x10, 0x14));
        assert_eq!(DMA_CH_RDLR, (0x18, 0x1c));
        assert!(DMA_CH_TDLR.0 < DMA_CH_TDLR.1);
        assert!(DMA_CH_RDLR.0 < DMA_CH_RDLR.1);
    }

    #[test]
    fn splitting_a_ring_address_puts_the_halves_in_register_order() {
        assert_eq!(split_ring_addr(0x0000_0001_dead_b000), (1, 0xdead_b000));
        assert_eq!(split_ring_addr(0x8000_0000), (0, 0x8000_0000));
        // A 32-bit-addressed ring has nothing in the high word, which is the
        // case where a swap would write the ring address into _HI and zero
        // into _LO --- a fetch from near zero, with no error reported.
        let (hi, lo) = split_ring_addr(0x1000_0000);
        assert_eq!(hi, 0);
        assert_eq!(lo, 0x1000_0000);
    }

    #[test]
    fn the_tail_pointers_have_no_high_half() {
        // Linux defines only `_LO` for both tail registers, and the gaps at
        // 0x20 and 0x28 where a `_HI` would sit stay undefined. So a tail
        // write is one word, not two.
        assert_eq!(DMA_CH_TDTR_LO, 0x24);
        assert_eq!(DMA_CH_RDTR_LO, 0x2c);
        assert_eq!(DMA_CH_TDTR_LO - DMA_CH_TDLR_LO, 0x10);
    }

    // ——— The descriptor ———

    #[test]
    fn a_descriptor_is_four_words_and_sixteen_bytes() {
        // `struct xgbe_ring_desc`: four `__le32`, nothing else. The engine
        // indexes the ring by this size, so a padded or reordered struct
        // would desynchronise driver and chip after the first entry.
        assert_eq!(core::mem::size_of::<Desc>(), 16);
        assert_eq!(core::mem::align_of::<Desc>(), 4);
    }

    #[test]
    fn the_desc3_flags_are_at_their_linux_indices() {
        // OWN 31, CTXT 30, FD 29, LD 28, in both directions.
        assert_eq!(DESC3_OWN, 1 << 31);
        assert_eq!(DESC3_CTXT, 1 << 30);
        assert_eq!(DESC3_FD, 1 << 29);
        assert_eq!(DESC3_LD, 1 << 28);
        assert!(desc_owned_by_dma(DESC3_OWN));
        assert!(!desc_owned_by_dma(0));
        assert!(!desc_owned_by_dma(DESC3_CTXT | DESC3_FD | DESC3_LD));
        assert!(desc_is_context(DESC3_CTXT));
        assert!(!desc_is_context(DESC3_OWN));
    }

    #[test]
    fn bit_thirty_is_inte_going_out_and_ctxt_coming_back() {
        // One bit, two meanings, chosen by who wrote the descriptor last:
        // RX_NORMAL_DESC3_INTE_INDEX and RX_NORMAL_DESC3_CTXT_INDEX are both
        // 30. The two accessors read the same bit; the point is that the
        // caller has to say which direction it is asking about.
        let d = DESC3_CTXT;
        assert!(rx_desc3_inte(d));
        assert!(desc_is_context(d));
        assert!(!rx_desc3_inte(0));
    }

    #[test]
    fn the_received_length_and_error_come_out_of_desc3() {
        // RX_NORMAL_DESC3_PL index 0 width 14, ES index 15 width 1.
        let d = DESC3_OWN | DESC3_FD | DESC3_LD | 1514;
        assert_eq!(rx_packet_len(d), 1514);
        assert!(!rx_error_summary(d));
        assert!(rx_error_summary(d | (1 << 15)));
        // Fourteen bits, so the field tops out at 16383 and the bit above it
        // is the error summary --- a length read fifteen bits wide would
        // report an errored 1514-byte frame as a 34282-byte one.
        assert_eq!(rx_packet_len(0x3fff), 0x3fff);
        assert_eq!(rx_packet_len(1514 | (1 << 15)), 1514);
    }

    #[test]
    fn the_split_header_length_is_narrower_than_the_transmit_length() {
        // RX_NORMAL_DESC2_HL is 10 bits; TX_NORMAL_DESC2_HL_B1L in the same
        // word on the other side is 14. Same word, same index, different
        // width.
        assert_eq!(rx_header_len(0x3ff), 0x3ff);
        assert_eq!(rx_header_len(0xffff_ffff), 0x3ff);
        assert_eq!(tx_desc2_buffer_len(0x3fff), 0x3fff);
        assert_eq!(tx_desc2_buffer_len(1514), 1514);
    }

    #[test]
    fn the_transmit_frame_length_shares_its_bits_with_the_tso_payload() {
        // TX_NORMAL_DESC3_FL is index 0 width 15; TCPPL is index 0 width 18;
        // TSE is index 18. So bit 18 is simultaneously "this is a TSO
        // descriptor" and the bottom of the payload length it selects.
        assert_eq!(tx_desc3_frame_len(1514), 1514);
        assert_eq!(tx_desc3_frame_len(0x7fff), 0x7fff);
        assert_eq!(TX_DESC3_TSE, 1 << 18);
        // The frame length can never set TSE by itself, which is what makes
        // the non-TSO path safe.
        assert_eq!(tx_desc3_frame_len(0xffff_ffff) & TX_DESC3_TSE, 0);
    }

    // ——— The sequences, against a fake XGMAC ———
    //
    // These are the first tests here that run real driver code rather than
    // checking a number. The fake is deliberately not a stub that returns
    // zero: a self-clearing bit that clears immediately would pass a reset
    // routine that never polls at all, so the fake clears `SWR` and `FTQ`
    // only after a configurable number of reads, and records every access in
    // order.

    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// Every register touch, in order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Access {
        Read(usize),
        Write(usize, u32),
    }

    struct FakeXgmac {
        /// Sparse register file.
        regs: RefCell<alloc::collections::BTreeMap<usize, u32>>,
        log: RefCell<Vec<Access>>,
        /// How many reads a self-clearing bit survives before the fake
        /// clears it. 0 means it clears on the first read.
        clear_after: RefCell<alloc::collections::BTreeMap<usize, u32>>,
        /// Never clear these, to exercise the timeout.
        stuck: RefCell<alloc::collections::BTreeSet<usize>>,
    }

    // The fake is only ever touched from the test thread; `MmioAccess`
    // requires Send + Sync because a real driver holds one across CPUs.
    unsafe impl Send for FakeXgmac {}
    unsafe impl Sync for FakeXgmac {}

    impl FakeXgmac {
        fn new() -> Self {
            Self {
                regs: RefCell::new(alloc::collections::BTreeMap::new()),
                log: RefCell::new(Vec::new()),
                clear_after: RefCell::new(alloc::collections::BTreeMap::new()),
                stuck: RefCell::new(alloc::collections::BTreeSet::new()),
            }
        }

        /// Preload a register, as a chip with other fields already set would
        /// present it. Not logged: this is the chip's state, not an access.
        fn preload(&self, off: usize, value: u32) {
            self.regs.borrow_mut().insert(off, value);
        }

        /// Make `off`'s bit 0 survive `n` reads before clearing.
        fn clear_bit0_after(&self, off: usize, n: u32) {
            self.clear_after.borrow_mut().insert(off, n);
        }

        /// Make `off`'s bit 0 never clear.
        fn stick_bit0(&self, off: usize) {
            self.stuck.borrow_mut().insert(off);
        }

        fn log(&self) -> Vec<Access> {
            self.log.borrow().clone()
        }

        fn writes_to(&self, off: usize) -> usize {
            self.log()
                .iter()
                .filter(|a| matches!(a, Access::Write(o, _) if *o == off))
                .count()
        }

        fn reads_of(&self, off: usize) -> usize {
            self.log()
                .iter()
                .filter(|a| matches!(a, Access::Read(o) if *o == off))
                .count()
        }
    }

    impl MmioAccess for FakeXgmac {
        fn read32(&self, off: usize) -> u32 {
            self.log.borrow_mut().push(Access::Read(off));
            let mut regs = self.regs.borrow_mut();
            let value = *regs.get(&off).unwrap_or(&0);
            if value & 1 != 0 && !self.stuck.borrow().contains(&off) {
                let mut after = self.clear_after.borrow_mut();
                match after.get_mut(&off) {
                    Some(0) | None => {
                        regs.insert(off, value & !1);
                    }
                    Some(left) => *left -= 1,
                }
            }
            value
        }

        fn write32(&self, off: usize, value: u32) {
            self.log.borrow_mut().push(Access::Write(off, value));
            self.regs.borrow_mut().insert(off, value);
        }
    }

    /// A delay that counts how often it was called.
    fn counting_wait(n: &core::cell::Cell<u32>) -> impl FnMut() + '_ {
        move || n.set(n.get() + 1)
    }

    #[test]
    fn set_bits_replaces_only_its_own_field() {
        // Linux's `SET_BITS`.
        assert_eq!(set_bits(0, 0, 1, 1), 1);
        assert_eq!(set_bits(0xffff_ffff, 0, 1, 0), 0xffff_fffe);
        assert_eq!(set_bits(0, 16, 10, 0x3ff), 0x3ff << 16);
        // A value wider than the field cannot spill into the next one.
        assert_eq!(set_bits(0, 4, 3, 0xff), 0x7 << 4);
        // And the rest of the word survives.
        assert_eq!(set_bits(0xdead_0000, 0, 1, 1), 0xdead_0001);
    }

    #[test]
    fn writing_a_field_is_a_read_modify_write() {
        // This is why `write_field` exists rather than a bare store: every
        // register the sequences touch shares its word with fields that must
        // survive. `MTL_Q_TQOMR` holds the queue size and the
        // store-and-forward mode alongside `FTQ`.
        let fake = FakeXgmac::new();
        fake.preload(MTL_Q_TQOMR, 0x0123_0000);
        write_field(&fake, MTL_Q_TQOMR, 0, 1, 1);
        assert_eq!(fake.log()[0], Access::Read(MTL_Q_TQOMR));
        assert_eq!(fake.log()[1], Access::Write(MTL_Q_TQOMR, 0x0123_0001));
    }

    #[test]
    fn the_dma_mode_and_mtl_queue_registers_are_where_linux_says() {
        assert_eq!(DMA_MR, 0x3000);
        assert_eq!(DMA_SBMR, 0x3004);
        assert_eq!(MTL_Q_BASE, 0x1100);
        assert_eq!(MTL_Q_INC, 0x80);
        assert_eq!(MTL_Q_TQOMR, 0x00);
        assert_eq!(MTL_Q_RQOMR, 0x40);
        assert_eq!(DMA_MR_SWR, (0, 1));
        assert_eq!(MTL_Q_TQOMR_FTQ, (0, 1));
        assert_eq!(POLL_LIMIT, 2000);
        // The MTL block sits below the DMA block, and neither reaches the
        // other: MTL queue 15's last register is still under DMA_MR.
        assert!(mtl_queue_reg(15, MTL_Q_RQOMR) < DMA_MR);
    }

    #[test]
    fn a_queues_registers_are_its_index_times_the_stride() {
        assert_eq!(mtl_queue_reg(0, MTL_Q_TQOMR), 0x1100);
        assert_eq!(mtl_queue_reg(1, MTL_Q_TQOMR), 0x1180);
        assert_eq!(mtl_queue_reg(1, MTL_Q_RQOMR), 0x11c0);
        assert_eq!(mtl_queue_reg(7, MTL_Q_TQOMR), 0x1100 + 7 * 0x80);
    }

    #[test]
    fn the_reset_is_issued_twice_on_purpose() {
        // Linux's comment in `xgbe_exit`: "To guard against possible
        // incorrectly generated interrupts, issue the software reset twice."
        // Collapsing it to one looks like a simplification and shows up as
        // an interrupt storm at boot, so the count is the test.
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(0);
        assert!(software_reset(&fake, &mut counting_wait(&n)).is_ok());
        assert_eq!(fake.writes_to(DMA_MR), 2);
        for a in fake.log() {
            if let Access::Write(off, value) = a {
                assert_eq!(off, DMA_MR);
                assert_eq!(value & 1, 1, "the write must set SWR");
            }
        }
    }

    #[test]
    fn the_reset_waits_for_the_chip_instead_of_assuming() {
        // A chip that holds SWR for a while must be polled until it clears.
        // A routine that wrote the bit and returned would pass a "reset"
        // chip that is still resetting --- and nothing would report it.
        let fake = FakeXgmac::new();
        fake.clear_bit0_after(DMA_MR, 5);
        let n = core::cell::Cell::new(0);
        assert!(software_reset_once(&fake, &mut counting_wait(&n)).is_ok());
        // One read per poll until it cleared, so more than one read.
        assert!(
            fake.reads_of(DMA_MR) > 2,
            "polled {} times",
            fake.reads_of(DMA_MR)
        );
        // And the delay was actually used: once after the write, once per
        // poll that found the bit still set.
        assert!(n.get() >= 2, "waited {} times", n.get());
    }

    #[test]
    fn a_chip_stuck_in_reset_is_reported_not_waited_on_forever() {
        // Linux's `-EBUSY` after 2000 polls.
        let fake = FakeXgmac::new();
        fake.stick_bit0(DMA_MR);
        let n = core::cell::Cell::new(0);
        assert_eq!(
            software_reset_once(&fake, &mut counting_wait(&n)),
            Err(DeviceError::NotReady)
        );
        // POLL_LIMIT polls, plus the one read of the read-modify-write that
        // issued the reset in the first place.
        assert_eq!(fake.reads_of(DMA_MR), POLL_LIMIT as usize + 1);
    }

    #[test]
    fn a_failed_first_reset_does_not_issue_the_second() {
        let fake = FakeXgmac::new();
        fake.stick_bit0(DMA_MR);
        let n = core::cell::Cell::new(0);
        assert!(software_reset(&fake, &mut counting_wait(&n)).is_err());
        assert_eq!(fake.writes_to(DMA_MR), 1);
    }

    #[test]
    fn an_older_core_is_not_asked_to_flush_its_queues() {
        // `xgbe_flush_tx_queues` returns immediately below SNPSVER 0x21,
        // because FTQ does not exist there --- writing it would set a
        // reserved bit in a register that also holds the queue size and the
        // store-and-forward mode.
        assert_eq!(FLUSH_MIN_SNPSVER, 0x21);
        for ver in [0x00u8, 0x10, 0x20] {
            let fake = FakeXgmac::new();
            let n = core::cell::Cell::new(0);
            assert!(flush_tx_queues(&fake, ver, 8, &mut counting_wait(&n)).is_ok());
            assert!(fake.log().is_empty(), "core {:#04x} was touched", ver);
        }
    }

    #[test]
    fn every_queue_is_kicked_before_any_is_polled() {
        // The order is the point. Flushing one queue at a time and waiting
        // for each serialises what the hardware does in parallel: on eight
        // queues that is eight timeouts' worth of worst case instead of one.
        let fake = FakeXgmac::new();
        for q in 0..4 {
            fake.clear_bit0_after(mtl_queue_reg(q, MTL_Q_TQOMR), 3);
        }
        let n = core::cell::Cell::new(0);
        assert!(flush_tx_queues(&fake, 0x30, 4, &mut counting_wait(&n)).is_ok());

        let log = fake.log();
        let last_write = log
            .iter()
            .rposition(|a| matches!(a, Access::Write(..)))
            .expect("there must be writes");
        let first_poll_read = log
            .iter()
            .enumerate()
            .filter(|(_, a)| matches!(a, Access::Read(..)))
            // The read-modify-write of each kick reads too, so the polls are
            // the reads that come after the last write.
            .map(|(i, _)| i)
            .find(|i| *i > last_write)
            .expect("there must be polls");
        assert!(first_poll_read > last_write);
        // All four queues were kicked, each exactly once.
        for q in 0..4 {
            assert_eq!(fake.writes_to(mtl_queue_reg(q, MTL_Q_TQOMR)), 1);
        }
    }

    #[test]
    fn a_queue_that_will_not_flush_is_reported_with_its_number() {
        let fake = FakeXgmac::new();
        fake.stick_bit0(mtl_queue_reg(2, MTL_Q_TQOMR));
        let n = core::cell::Cell::new(0);
        assert_eq!(
            flush_tx_queues(&fake, 0x30, 4, &mut counting_wait(&n)),
            Err(DeviceError::NotReady)
        );
        // Queue 3 is never polled, because queue 2 failed first --- and all
        // four were still kicked before any poll.
        assert_eq!(fake.writes_to(mtl_queue_reg(3, MTL_Q_TQOMR)), 1);
    }

    // ——— The system bus mode ———

    #[test]
    fn every_sbmr_field_is_at_its_linux_index() {
        assert_eq!(DMA_SBMR_UNDEF, (0, 1));
        assert_eq!(DMA_SBMR_BLEN, (1, 7));
        assert_eq!(DMA_SBMR_EAME, (11, 1));
        assert_eq!(DMA_SBMR_AAL, (12, 1));
        assert_eq!(DMA_SBMR_RD_OSR_LMT, (16, 6));
        assert_eq!(DMA_SBMR_WR_OSR_LMT, (24, 6));
        // BLEN runs from 1 to 7 inclusive, so it stops short of EAME at 11
        // with a gap, and does NOT start at 0 where UNDEF is.
        assert_eq!(DMA_SBMR_BLEN.0 + DMA_SBMR_BLEN.1, 8);
        assert!(DMA_SBMR_BLEN.0 > DMA_SBMR_UNDEF.0);
    }

    #[test]
    fn the_burst_length_field_is_a_bitmap_of_lengths_not_a_number() {
        // `blen >> 2` in xgbe_config_dma_bus looks like a unit conversion and
        // is not: each of BLEN's seven bits is one burst length, so dividing
        // the beat count by four lands on that length's own bit.
        for (beats, bit) in [
            (4u32, 0u32),
            (8, 1),
            (16, 2),
            (32, 3),
            (64, 4),
            (128, 5),
            (256, 6),
        ] {
            assert_eq!(
                blen_field(beats),
                Some(1 << bit),
                "{} beats should be bit {} of BLEN",
                beats,
                bit
            );
        }
        // Each one is a single bit, which is what proves it is a bitmap.
        for beats in [4u32, 8, 16, 32, 64, 128, 256] {
            assert_eq!(blen_field(beats).unwrap().count_ones(), 1);
        }
        // And every length fits in the seven-bit field.
        assert!(blen_field(256).unwrap() < (1 << DMA_SBMR_BLEN.1));
    }

    #[test]
    fn a_burst_length_that_is_not_one_of_the_seven_is_refused() {
        // Shifting, say, 100 beats would land on bit 25 of the field, which
        // is outside it --- or worse, on a different length's bit.
        for beats in [0u32, 1, 2, 3, 5, 100, 512, 1024] {
            assert_eq!(blen_field(beats), None, "{} beats was accepted", beats);
        }
    }

    #[test]
    fn the_outstanding_limits_are_written_one_less_than_they_are() {
        // `RD_OSR_LMT, pdata->rd_osr_limit - 1`. The field counts from zero.
        let cfg = BusConfig {
            burst_beats: 256,
            aligned_beats: false,
            read_outstanding: 8,
            write_outstanding: 4,
        };
        let v = system_bus_mode(0, cfg).unwrap();
        assert_eq!(get_bits(v, DMA_SBMR_RD_OSR_LMT.0, DMA_SBMR_RD_OSR_LMT.1), 7);
        assert_eq!(get_bits(v, DMA_SBMR_WR_OSR_LMT.0, DMA_SBMR_WR_OSR_LMT.1), 3);
        // One request outstanding is a zero in the field, not a one.
        let one = BusConfig {
            read_outstanding: 1,
            write_outstanding: 1,
            ..cfg
        };
        let v = system_bus_mode(0, one).unwrap();
        assert_eq!(get_bits(v, DMA_SBMR_RD_OSR_LMT.0, DMA_SBMR_RD_OSR_LMT.1), 0);
    }

    #[test]
    fn a_zero_outstanding_limit_is_refused_instead_of_underflowing_to_sixty_three() {
        // This is the one that a missing platform property produces in Linux:
        // `0 - 1` in an `unsigned int`, masked to six bits, is 63 --- the
        // most permissive setting there is, from a value nobody set.
        let cfg = BusConfig {
            burst_beats: 256,
            aligned_beats: false,
            read_outstanding: 0,
            write_outstanding: 8,
        };
        assert_eq!(system_bus_mode(0, cfg), Err(DeviceError::InvalidParam));
        let cfg = BusConfig {
            write_outstanding: 0,
            read_outstanding: 8,
            ..cfg
        };
        assert_eq!(system_bus_mode(0, cfg), Err(DeviceError::InvalidParam));
        // And a limit above what six bits can hold is refused too, rather
        // than silently truncated to something smaller than asked for.
        let cfg = BusConfig {
            read_outstanding: 65,
            write_outstanding: 8,
            ..cfg
        };
        assert_eq!(system_bus_mode(0, cfg), Err(DeviceError::InvalidParam));
    }

    #[test]
    fn enhanced_addressing_and_the_undocumented_bit_are_always_set() {
        // EAME must be on before any descriptor address above 4 GiB is
        // programmed: the high half of the list address register gets written
        // regardless, so without it the engine fetches the ring from the
        // wrong place and reports nothing. UNDEF is set because Linux sets
        // it and nobody has documented what it does.
        let cfg = BusConfig {
            burst_beats: 32,
            aligned_beats: true,
            read_outstanding: 2,
            write_outstanding: 2,
        };
        let v = system_bus_mode(0, cfg).unwrap();
        assert_eq!(get_bits(v, DMA_SBMR_EAME.0, DMA_SBMR_EAME.1), 1);
        assert_eq!(get_bits(v, DMA_SBMR_UNDEF.0, DMA_SBMR_UNDEF.1), 1);
        assert_eq!(get_bits(v, DMA_SBMR_AAL.0, DMA_SBMR_AAL.1), 1);
        let v = system_bus_mode(
            0,
            BusConfig {
                aligned_beats: false,
                ..cfg
            },
        )
        .unwrap();
        assert_eq!(get_bits(v, DMA_SBMR_AAL.0, DMA_SBMR_AAL.1), 0);
    }

    #[test]
    fn the_bus_mode_is_one_read_and_one_write_not_five() {
        // The point of building the word locally. Five read-modify-writes
        // would publish four intermediate states to a live bus master, each
        // a combination the driver never intended --- including, briefly,
        // a burst length with EAME already on and the limits still at
        // whatever the chip had.
        let fake = FakeXgmac::new();
        fake.preload(DMA_SBMR, 0x0000_0e00);
        let cfg = BusConfig {
            burst_beats: 256,
            aligned_beats: false,
            read_outstanding: 8,
            write_outstanding: 8,
        };
        assert!(config_dma_bus(&fake, cfg).is_ok());
        let log = fake.log();
        assert_eq!(log.len(), 2, "{:?}", log);
        assert_eq!(log[0], Access::Read(DMA_SBMR));
        assert!(matches!(log[1], Access::Write(DMA_SBMR, _)));
    }

    #[test]
    fn the_bits_the_driver_does_not_claim_survive_the_bus_mode_write() {
        // Bits 13..15 and 22..23 belong to no field above, so whatever the
        // chip reports there has to come back out untouched.
        let preserved = (1 << 13) | (1 << 15) | (1 << 22) | (1 << 23);
        let cfg = BusConfig {
            burst_beats: 4,
            aligned_beats: false,
            read_outstanding: 1,
            write_outstanding: 1,
        };
        let v = system_bus_mode(preserved, cfg).unwrap();
        assert_eq!(v & preserved, preserved);
    }

    #[test]
    fn a_refused_bus_config_writes_nothing() {
        let fake = FakeXgmac::new();
        let cfg = BusConfig {
            burst_beats: 100,
            aligned_beats: false,
            read_outstanding: 8,
            write_outstanding: 8,
        };
        assert_eq!(config_dma_bus(&fake, cfg), Err(DeviceError::InvalidParam));
        assert_eq!(fake.writes_to(DMA_SBMR), 0);
    }

    // ——— The station address ———

    #[test]
    fn the_address_registers_are_where_linux_says() {
        assert_eq!(MAC_MACA0HR, 0x0300);
        assert_eq!(MAC_MACA0LR, 0x0304);
        assert_eq!(MAC_MACA1HR, 0x0308);
        // Each address slot is a pair of words, so slot 1 starts one pair on.
        assert_eq!(MAC_MACA1HR - MAC_MACA0HR, 8);
        assert_eq!(MAC_MACA1HR_AE, (31, 1));
    }

    #[test]
    fn the_address_halves_are_four_bytes_and_two_not_three_and_three() {
        // `xgbe_set_mac_address`: the low word takes bytes 0..3 and the high
        // word bytes 4..5, each little-endian within its word. Splitting it
        // evenly --- the obvious guess --- puts byte 3 in the wrong register
        // and the card answers to an address nobody assigned it.
        let (hi, lo) = mac_to_regs([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(lo, 0x3322_1100);
        assert_eq!(hi, 0x0000_5544);
    }

    #[test]
    fn the_high_word_of_address_zero_leaves_room_for_the_enable_bit() {
        // The high register is a half-filled word: its top sixteen bits stay
        // zero, which is where the additional addresses' AE bit lives. So a
        // station address can never set AE by accident, and address 0 --- the
        // one slot whose HR has no AE defined --- must not have it set.
        let (hi, _) = mac_to_regs([0xff; 6]);
        assert_eq!(hi, 0x0000_ffff);
        assert_eq!(hi >> 16, 0);
        assert_eq!(hi & (1 << MAC_MACA1HR_AE.0), 0);
    }

    #[test]
    fn the_station_address_is_written_high_half_first() {
        // The order Linux uses, kept because the low word is the one that
        // latches the pair: writing low first leaves the chip briefly
        // filtering on an address whose top two bytes are stale.
        let fake = FakeXgmac::new();
        set_mac_address(&fake, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        assert_eq!(
            fake.log(),
            alloc::vec![
                Access::Write(MAC_MACA0HR, 0x0000_5634),
                Access::Write(MAC_MACA0LR, 0x1200_5452),
            ]
        );
    }

    #[test]
    fn an_address_read_from_the_port_properties_survives_the_round_trip() {
        // The two halves this batch writes and the two the probe reads are
        // different registers with different layouts, so the only way to know
        // they agree is to put a MAC through both.
        let mac = [0x02, 0xde, 0xad, 0xbe, 0xef, 0x10];
        let (hi, lo) = mac_to_regs(mac);
        // `mac_from_prop` wants the port-property layout, which carries the
        // valid bit in the high word --- the MAC registers have no such bit,
        // which is the difference worth noticing.
        assert_eq!(mac_from_prop(lo, hi | (1 << 31)), Some(mac));
    }

    // ——— Turning the thing on ———

    /// The offsets a write landed on, in order, ignoring the reads of each
    /// read-modify-write.
    fn written_offsets(fake: &FakeXgmac) -> alloc::vec::Vec<usize> {
        fake.log()
            .iter()
            .filter_map(|a| match a {
                Access::Write(off, _) => Some(*off),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_mac_control_registers_are_where_linux_says() {
        assert_eq!(MAC_TCR, 0x0000);
        assert_eq!(MAC_RCR, 0x0004);
        assert_eq!(MAC_RQC0R, 0x00a0);
        assert_eq!(MAC_TCR_TE, (0, 1));
        assert_eq!(MAC_RCR_RE, (0, 1));
        assert_eq!(MAC_RCR_ACS, (1, 1));
        assert_eq!(MAC_RCR_CST, (2, 1));
        assert_eq!(MAC_RCR_DCRCC, (3, 1));
        assert_eq!(DMA_CH_TCR_ST, (0, 1));
        assert_eq!(DMA_CH_RCR_SR, (0, 1));
    }

    #[test]
    fn the_queue_enable_value_is_two_not_one() {
        // TXQEN is TWO bits wide, and 1 in those two bits is
        // "enabled for average-bit-rate scheduling" --- a different mode, not
        // "on". Writing a boolean into it is what the field's width invites.
        assert_eq!(MTL_Q_TQOMR_TXQEN, (2, 2));
        assert_eq!(MTL_Q_ENABLED, 0x02);
        assert_eq!(MTL_Q_DISABLED, 0x00);
        assert_ne!(MTL_Q_ENABLED, 1);
    }

    #[test]
    fn the_receive_queue_map_is_two_bits_per_queue() {
        // `reg_val |= (0x02 << (i << 1))` in xgbe_enable_rx.
        assert_eq!(rx_queue_enable_map(0), 0x0000_0000);
        assert_eq!(rx_queue_enable_map(1), 0x0000_0002);
        assert_eq!(rx_queue_enable_map(2), 0x0000_000a);
        assert_eq!(rx_queue_enable_map(4), 0x0000_00aa);
        // Eight queues is 0xaaaa, NOT 0xff: a one-bit-per-queue reading of
        // this register enables half the queues it meant to and puts the
        // other half into the wrong mode.
        assert_eq!(rx_queue_enable_map(8), 0x0000_aaaa);
        assert_ne!(rx_queue_enable_map(8), 0xff);
        assert_eq!(rx_queue_enable_map(16), 0xaaaa_aaaa);
    }

    #[test]
    fn transmit_comes_up_from_the_inside_out() {
        // Channels, then queues, then the MAC. Enabling the MAC first opens
        // the line into a path that cannot move anything yet.
        let fake = FakeXgmac::new();
        enable_tx(&fake, 2, 3);
        assert_eq!(
            written_offsets(&fake),
            alloc::vec![
                dma_channel_reg(0, DMA_CH_TCR),
                dma_channel_reg(1, DMA_CH_TCR),
                mtl_queue_reg(0, MTL_Q_TQOMR),
                mtl_queue_reg(1, MTL_Q_TQOMR),
                mtl_queue_reg(2, MTL_Q_TQOMR),
                MAC_TCR,
            ]
        );
    }

    #[test]
    fn the_transmit_enable_writes_the_right_values() {
        let fake = FakeXgmac::new();
        // A queue register that already carries its size and mode, so a
        // bare write instead of a read-modify-write would show up.
        fake.preload(mtl_queue_reg(0, MTL_Q_TQOMR), 0x01f0_0002);
        enable_tx(&fake, 1, 1);

        let tqomr = fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR));
        assert_eq!(
            get_bits(tqomr, MTL_Q_TQOMR_TXQEN.0, MTL_Q_TQOMR_TXQEN.1),
            MTL_Q_ENABLED
        );
        assert_eq!(tqomr & 0x01f0_0000, 0x01f0_0000, "the queue size survived");

        assert_eq!(
            get_bits(
                fake.read32(dma_channel_reg(0, DMA_CH_TCR)),
                DMA_CH_TCR_ST.0,
                DMA_CH_TCR_ST.1
            ),
            1
        );
        assert_eq!(
            get_bits(fake.read32(MAC_TCR), MAC_TCR_TE.0, MAC_TCR_TE.1),
            1
        );
    }

    #[test]
    fn receive_comes_up_from_the_inside_out_too() {
        let fake = FakeXgmac::new();
        enable_rx(&fake, 2, 2);
        assert_eq!(
            written_offsets(&fake),
            alloc::vec![
                dma_channel_reg(0, DMA_CH_RCR),
                dma_channel_reg(1, DMA_CH_RCR),
                MAC_RQC0R,
                MAC_RCR, // DCRCC
                MAC_RCR, // CST
                MAC_RCR, // ACS
                MAC_RCR, // RE
            ]
        );
    }

    #[test]
    fn the_receiver_is_enabled_after_the_frame_trimming_not_before() {
        // RE is the LAST of the four writes to MAC_RCR. Enabling it before
        // ACS and CST opens the line for the window in between, during which
        // frames arrive with their padding and FCS still attached --- not an
        // error anywhere, just a handful of over-long frames at the very
        // start of the link.
        let fake = FakeXgmac::new();
        enable_rx(&fake, 1, 1);
        let rcr_writes: alloc::vec::Vec<u32> = fake
            .log()
            .iter()
            .filter_map(|a| match a {
                Access::Write(off, v) if *off == MAC_RCR => Some(*v),
                _ => None,
            })
            .collect();
        assert_eq!(rcr_writes.len(), 4);
        // RE is off for the first three writes and on only in the last.
        for v in &rcr_writes[..3] {
            assert_eq!(get_bits(*v, MAC_RCR_RE.0, MAC_RCR_RE.1), 0);
        }
        let last = rcr_writes[3];
        assert_eq!(get_bits(last, MAC_RCR_RE.0, MAC_RCR_RE.1), 1);
        // And by then the trimming is already on and CRC checking is not
        // disabled.
        assert_eq!(get_bits(last, MAC_RCR_ACS.0, MAC_RCR_ACS.1), 1);
        assert_eq!(get_bits(last, MAC_RCR_CST.0, MAC_RCR_CST.1), 1);
        assert_eq!(get_bits(last, MAC_RCR_DCRCC.0, MAC_RCR_DCRCC.1), 0);
    }

    #[test]
    fn a_chip_that_came_up_with_crc_checking_disabled_has_it_turned_back_on() {
        // DCRCC is "do NOT check the CRC", so Linux clearing it is the
        // meaningful direction. A chip, or a previous driver, that left it
        // set would otherwise keep delivering frames with bad CRCs.
        let fake = FakeXgmac::new();
        fake.preload(MAC_RCR, 1 << MAC_RCR_DCRCC.0);
        enable_rx(&fake, 1, 1);
        assert_eq!(
            get_bits(fake.read32(MAC_RCR), MAC_RCR_DCRCC.0, MAC_RCR_DCRCC.1),
            0
        );
    }

    #[test]
    fn the_queue_map_is_written_whole_not_merged() {
        // `XGMAC_IOWRITE(pdata, MAC_RQC0R, reg_val)`: no read first. That is
        // also how a queue gets disabled --- xgbe_disable_rx writes a bare
        // zero --- so any queue not in the map is switched off by this same
        // write, whatever it was doing before.
        let fake = FakeXgmac::new();
        fake.preload(MAC_RQC0R, 0xaaaa_aaaa);
        enable_rx(&fake, 1, 2);
        assert_eq!(fake.read32(MAC_RQC0R), rx_queue_enable_map(2));
        // Exactly one write, and no read of it at all.
        assert_eq!(fake.writes_to(MAC_RQC0R), 1);
    }

    #[test]
    fn a_channel_register_offset_is_not_a_channel_address() {
        // `dma_channel_reg` is relative to BAR0 for MmioAccess;
        // `channel_regs` takes a mapped address. Same arithmetic, different
        // starting point, and mixing them up is an access at a wild address.
        assert_eq!(dma_channel_reg(0, DMA_CH_TCR), DMA_CH_BASE + DMA_CH_TCR);
        assert_eq!(dma_channel_reg(3, DMA_CH_SR), DMA_CH_BASE + 3 * 0x80 + 0x60);
        let base = 0xffff_8000_0000_0000usize;
        assert_eq!(channel_regs(base, 3), base + dma_channel_reg(3, 0));
    }

    #[test]
    fn no_channels_and_no_queues_still_leaves_the_mac_consistent() {
        // The geometry comes from MAC_HWF2R, which always reports at least
        // one of each, so zero is not a real chip --- but a caller that
        // passed zero must not end up with a MAC enabled over nothing.
        let fake = FakeXgmac::new();
        enable_tx(&fake, 0, 0);
        assert_eq!(written_offsets(&fake), alloc::vec![MAC_TCR]);
        let fake = FakeXgmac::new();
        enable_rx(&fake, 0, 0);
        // The queue map written is empty, so no receive queue is enabled
        // even though RE ends up set.
        assert_eq!(fake.read32(MAC_RQC0R), 0);
    }

    // ——— Turning it off ———

    /// A poll budget: returns true while there is time left.
    fn budget(n: &core::cell::Cell<u32>) -> impl FnMut() -> bool + '_ {
        move || {
            if n.get() == 0 {
                false
            } else {
                n.set(n.get() - 1);
                true
            }
        }
    }

    #[test]
    fn the_debug_status_constants_are_where_linux_says() {
        assert_eq!(DMA_DSR0, 0x3020);
        assert_eq!(DMA_DSR1, 0x3024);
        assert_eq!(DMA_DSR_TPS_WIDTH, 4);
        // DMA_DSR_Q_WIDTH is RPS_WIDTH + TPS_WIDTH, both 4.
        assert_eq!(DMA_DSR_Q_WIDTH, 8);
        assert_eq!(DMA_DSR0_TPS_START, 12);
        assert_eq!(DMA_DSRX_FIRST_QUEUE, 3);
        assert_eq!(DMA_DSRX_INC, 4);
        assert_eq!(DMA_DSRX_QPR, 4);
        assert_eq!(DMA_DSRX_TPS_START, 4);
        assert_eq!(MTL_Q_TQDR, 0x08);
        assert_eq!(MTL_Q_RQDR, 0x48);
        assert_eq!(DMA_STOP_TIMEOUT_SECS, 1);
    }

    #[test]
    fn the_queue_to_status_mapping_is_not_linear() {
        // Queues 0..2 are in DSR0 from bit 12, eight bits apart.
        assert_eq!(tx_dma_status_pos(0), (DMA_DSR0, 12));
        assert_eq!(tx_dma_status_pos(1), (DMA_DSR0, 20));
        assert_eq!(tx_dma_status_pos(2), (DMA_DSR0, 28));
        // And then it JUMPS: queue 3 is a different register at a different
        // starting offset, not "queue 2 plus eight bits". A linear reading
        // would say (DMA_DSR0, 36), which is not even inside the register.
        assert_eq!(tx_dma_status_pos(3), (DMA_DSR1, 4));
        assert_ne!(tx_dma_status_pos(3), (DMA_DSR0, 36));
        assert_eq!(tx_dma_status_pos(4), (DMA_DSR1, 12));
        assert_eq!(tx_dma_status_pos(6), (DMA_DSR1, 28));
        // Four queues to a register from DSR1 on, then the next register.
        assert_eq!(tx_dma_status_pos(7), (DMA_DSR1 + 4, 4));
        assert_eq!(tx_dma_status_pos(10), (DMA_DSR1 + 4, 28));
        assert_eq!(tx_dma_status_pos(11), (DMA_DSR1 + 8, 4));
    }

    #[test]
    fn every_status_field_stays_inside_its_register() {
        for q in 0..16u32 {
            let (_, pos) = tx_dma_status_pos(q);
            assert!(
                pos + DMA_DSR_TPS_WIDTH <= 32,
                "queue {} runs past the end of its register",
                q
            );
        }
    }

    #[test]
    fn stopped_and_suspended_are_two_values_of_sixteen_not_a_flag() {
        // TPS is a four-bit enumeration. "Quiet enough to switch off" is
        // exactly 0 and 6; everything else, 1 among them, is not.
        assert_eq!(DMA_TPS_STOPPED, 0x00);
        assert_eq!(DMA_TPS_SUSPENDED, 0x06);
        assert!(tx_dma_is_quiet(0));
        assert!(tx_dma_is_quiet(6));
        for state in [1u32, 2, 3, 4, 5, 7, 8, 15] {
            assert!(!tx_dma_is_quiet(state), "{} was treated as quiet", state);
        }
    }

    #[test]
    fn a_transmit_queue_is_drained_when_it_is_not_reading_a_frame_out() {
        // `TRCSTS != 1 && TXQSTS == 0`. The read-controller test is NOT
        // `== 0`: TRCSTS is two bits and only the value 1 means "reading a
        // frame out right now".
        assert!(tx_queue_drained(0));
        // TRCSTS == 1: busy.
        assert!(!tx_queue_drained(1 << 1));
        // TRCSTS == 2 or 3: not busy, so drained as far as this test goes.
        assert!(tx_queue_drained(2 << 1));
        assert!(tx_queue_drained(3 << 1));
        // TXQSTS set: the queue still holds something.
        assert!(!tx_queue_drained(1 << 4));
        assert!(!tx_queue_drained((2 << 1) | (1 << 4)));
    }

    #[test]
    fn a_receive_queue_is_drained_when_it_is_empty_in_both_senses() {
        assert!(rx_queue_drained(0));
        // PRXQ: bytes still in the FIFO.
        assert!(!rx_queue_drained(1 << 16));
        // RXQSTS: the fill state, two bits.
        assert!(!rx_queue_drained(1 << 4));
        assert!(!rx_queue_drained(3 << 4));
        // A bit outside both fields does not make a drained queue look busy.
        assert!(rx_queue_drained(1 << 7));
    }

    #[test]
    fn a_newer_core_waits_on_the_queue_and_an_older_one_on_the_dma_status() {
        // The two paths read entirely different registers, so which one ran
        // is visible in the access log.
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(10);
        assert_eq!(
            prepare_tx_stop(&fake, 0x30, 1, true, &mut budget(&n)),
            Drained::Yes
        );
        assert!(fake.reads_of(mtl_queue_reg(1, MTL_Q_TQDR)) > 0);
        assert_eq!(fake.reads_of(DMA_DSR0), 0);

        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(10);
        assert_eq!(
            prepare_tx_stop(&fake, 0x20, 1, true, &mut budget(&n)),
            Drained::Yes
        );
        assert!(fake.reads_of(DMA_DSR0) > 0);
        assert_eq!(fake.reads_of(mtl_queue_reg(1, MTL_Q_TQDR)), 0);
    }

    #[test]
    fn with_no_link_nothing_is_waited_for() {
        // With no link the transmit path never completes, so waiting is a
        // guaranteed timeout on every queue in turn --- one second each, on
        // the path whose whole job is to bring the interface down.
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(10);
        assert_eq!(
            prepare_tx_stop(&fake, 0x30, 0, false, &mut budget(&n)),
            Drained::NoLink
        );
        assert!(fake.log().is_empty());
        // And the budget was not spent either.
        assert_eq!(n.get(), 10);
    }

    #[test]
    fn a_queue_that_never_drains_times_out_instead_of_spinning() {
        let fake = FakeXgmac::new();
        // TXQSTS stuck set: the queue always holds something.
        fake.preload(mtl_queue_reg(0, MTL_Q_TQDR), 1 << 4);
        let n = core::cell::Cell::new(5);
        assert_eq!(
            prepare_tx_stop(&fake, 0x30, 0, true, &mut budget(&n)),
            Drained::TimedOut
        );
        assert_eq!(n.get(), 0, "the whole budget was used");

        let fake = FakeXgmac::new();
        fake.preload(mtl_queue_reg(0, MTL_Q_RQDR), 1 << 16);
        let n = core::cell::Cell::new(5);
        assert_eq!(
            prepare_rx_stop(&fake, 0, &mut budget(&n)),
            Drained::TimedOut
        );
    }

    #[test]
    fn a_receive_queue_is_drained_even_with_no_link() {
        // Linux's asymmetry, kept: what is in a receive queue came off the
        // wire already, so it drains whether or not the link is still up.
        // `prepare_rx_stop` has no link parameter at all, which is the
        // clearest way to say it.
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(5);
        assert_eq!(prepare_rx_stop(&fake, 0, &mut budget(&n)), Drained::Yes);
        assert!(fake.reads_of(mtl_queue_reg(0, MTL_Q_RQDR)) > 0);
    }

    #[test]
    fn transmit_goes_down_inside_out_with_the_mac_last() {
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(100);
        let mut complete = || true;
        disable_tx(&fake, 0x30, 2, 2, true, &mut complete, &mut budget(&n));
        assert_eq!(
            written_offsets(&fake),
            alloc::vec![
                dma_channel_reg(0, DMA_CH_TCR),
                dma_channel_reg(1, DMA_CH_TCR),
                mtl_queue_reg(0, MTL_Q_TQOMR),
                mtl_queue_reg(1, MTL_Q_TQOMR),
                MAC_TCR,
            ]
        );
        // The MAC was disabled last of all, after both queues.
        assert_eq!(*written_offsets(&fake).last().unwrap(), MAC_TCR);
        assert_eq!(
            get_bits(fake.read32(MAC_TCR), MAC_TCR_TE.0, MAC_TCR_TE.1),
            0
        );
        assert_eq!(
            get_bits(
                fake.read32(mtl_queue_reg(0, MTL_Q_TQOMR)),
                MTL_Q_TQOMR_TXQEN.0,
                MTL_Q_TQOMR_TXQEN.1
            ),
            MTL_Q_DISABLED
        );
    }

    #[test]
    fn transmit_waits_for_its_rings_before_it_stops_the_channels() {
        // The ring check is the driver's own: the chip does not report the
        // producer and consumer indices. It has to happen before the first
        // channel is switched off, or descriptors already handed over are
        // abandoned.
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(100);
        let calls = core::cell::Cell::new(0u32);
        let mut complete = || {
            calls.set(calls.get() + 1);
            calls.get() >= 3
        };
        disable_tx(&fake, 0x30, 1, 1, true, &mut complete, &mut budget(&n));
        assert_eq!(calls.get(), 3);
        // And it was called before anything was written.
        assert!(!fake.log().is_empty());
    }

    #[test]
    fn with_no_link_transmit_does_not_wait_for_rings_that_cannot_drain() {
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(100);
        let calls = core::cell::Cell::new(0u32);
        let mut complete = || {
            calls.set(calls.get() + 1);
            false
        };
        disable_tx(&fake, 0x30, 1, 1, false, &mut complete, &mut budget(&n));
        assert_eq!(calls.get(), 0, "the ring check ran without a link");
        assert_eq!(n.get(), 100, "the budget was spent without a link");
        // The interface still came all the way down.
        assert_eq!(*written_offsets(&fake).last().unwrap(), MAC_TCR);
    }

    #[test]
    fn receive_goes_down_outside_in_with_the_mac_first() {
        // The opposite direction to disable_tx AND to enable_rx. Nothing new
        // may be accepted while the path behind it is torn down.
        let fake = FakeXgmac::new();
        let n = core::cell::Cell::new(100);
        disable_rx(&fake, 2, 2, &mut budget(&n));
        assert_eq!(
            written_offsets(&fake),
            alloc::vec![
                MAC_RCR, // CST
                MAC_RCR, // ACS
                MAC_RCR, // RE
                MAC_RQC0R,
                dma_channel_reg(0, DMA_CH_RCR),
                dma_channel_reg(1, DMA_CH_RCR),
            ]
        );
        assert_eq!(fake.read32(MAC_RQC0R), 0);
        assert_eq!(
            get_bits(fake.read32(MAC_RCR), MAC_RCR_RE.0, MAC_RCR_RE.1),
            0
        );
    }

    #[test]
    fn disabling_is_not_enabling_reversed_for_transmit() {
        // The claim the module comment makes, as a test: both orders start
        // at the channels and end at the MAC.
        let up = FakeXgmac::new();
        enable_tx(&up, 2, 2);
        let down = FakeXgmac::new();
        let n = core::cell::Cell::new(100);
        let mut complete = || true;
        disable_tx(&down, 0x30, 2, 2, true, &mut complete, &mut budget(&n));
        assert_eq!(written_offsets(&up), written_offsets(&down));

        // Receive, by contrast, really is reversed.
        let up = FakeXgmac::new();
        enable_rx(&up, 2, 2);
        let down = FakeXgmac::new();
        let n = core::cell::Cell::new(100);
        disable_rx(&down, 2, 2, &mut budget(&n));
        // What reverses is the order of the LAYERS, not the iteration
        // within one: both still walk their channels 0, 1, .... So the
        // claim to check is which layer each sequence starts and ends on.
        let up_offsets = written_offsets(&up);
        let down_offsets = written_offsets(&down);
        assert_eq!(*up_offsets.first().unwrap(), dma_channel_reg(0, DMA_CH_RCR));
        assert_eq!(*up_offsets.last().unwrap(), MAC_RCR);
        assert_eq!(*down_offsets.first().unwrap(), MAC_RCR);
        assert_eq!(
            *down_offsets.last().unwrap(),
            dma_channel_reg(1, DMA_CH_RCR)
        );
    }

    // ——— Handing the chip its rings ———

    #[test]
    fn the_receive_buffer_size_field_does_not_start_at_bit_zero() {
        // RBSZ is at index 1. Bit 0 is SR, start-receive --- so a buffer
        // size written at index 0 would start the engine as a side effect of
        // being an odd number of bytes.
        assert_eq!(DMA_CH_RCR_RBSZ, (1, 14));
        assert_eq!(DMA_CH_RCR_RBSZ.0, DMA_CH_RCR_SR.0 + DMA_CH_RCR_SR.1);
        let fake = FakeXgmac::new();
        program_rx_ring(&fake, 0, 0x1000, 4, 2048).unwrap();
        let rcr = fake.read32(dma_channel_reg(0, DMA_CH_RCR));
        assert_eq!(get_bits(rcr, DMA_CH_RCR_RBSZ.0, DMA_CH_RCR_RBSZ.1), 2048);
        // And the engine was NOT started by writing the size.
        assert_eq!(get_bits(rcr, DMA_CH_RCR_SR.0, DMA_CH_RCR_SR.1), 0);
    }

    #[test]
    fn the_ring_length_register_takes_a_last_index_not_a_count() {
        // `ring->rdesc_count - 1`. Writing the count gives the engine one
        // descriptor more than exists, which it fetches from whatever the
        // allocator put after the ring.
        let fake = FakeXgmac::new();
        program_tx_ring(&fake, 0, 0x2000, 32).unwrap();
        assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_TDRLR)), 31);

        let fake = FakeXgmac::new();
        program_rx_ring(&fake, 1, 0x3000, 64, 2048).unwrap();
        assert_eq!(fake.read32(dma_channel_reg(1, DMA_CH_RDRLR)), 63);
    }

    #[test]
    fn an_empty_ring_is_refused_rather_than_written_as_minus_one() {
        // `count - 1` on zero wraps to 0xffffffff: a ring of four billion
        // descriptors, which is the worst possible thing to hand a bus
        // master.
        let fake = FakeXgmac::new();
        assert_eq!(
            program_tx_ring(&fake, 0, 0x2000, 0),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            program_rx_ring(&fake, 0, 0x2000, 0, 2048),
            Err(DeviceError::InvalidParam)
        );
        // A buffer size that does not fit the field is refused too, rather
        // than truncated to something smaller than the buffers actually are
        // --- which would overrun nothing, but would silently split every
        // frame.
        assert_eq!(
            program_rx_ring(&fake, 0, 0x2000, 4, 1 << 14),
            Err(DeviceError::InvalidParam)
        );
        assert_eq!(
            program_rx_ring(&fake, 0, 0x2000, 4, 0),
            Err(DeviceError::InvalidParam)
        );
        assert!(fake.log().is_empty());
    }

    #[test]
    fn the_ring_address_goes_in_high_half_first() {
        let fake = FakeXgmac::new();
        program_tx_ring(&fake, 0, 0x0000_0007_dead_0000, 16).unwrap();
        assert_eq!(
            written_offsets(&fake),
            alloc::vec![
                dma_channel_reg(0, DMA_CH_TDRLR),
                dma_channel_reg(0, DMA_CH_TDLR_HI),
                dma_channel_reg(0, DMA_CH_TDLR_LO),
            ]
        );
        assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_TDLR_HI)), 7);
        assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_TDLR_LO)), 0xdead_0000);
    }

    #[test]
    fn a_transmit_ring_gets_no_tail_pointer_and_a_receive_ring_does() {
        // A transmit ring starts EMPTY: the driver has nothing for the
        // engine to fetch, and the tail is written later when a frame is
        // queued. Writing one now would point the engine at descriptors
        // nobody has filled in.
        let fake = FakeXgmac::new();
        program_tx_ring(&fake, 0, 0x2000, 16).unwrap();
        assert_eq!(fake.writes_to(dma_channel_reg(0, DMA_CH_TDTR_LO)), 0);

        let fake = FakeXgmac::new();
        program_rx_ring(&fake, 0, 0x2000, 16, 2048).unwrap();
        assert_eq!(fake.writes_to(dma_channel_reg(0, DMA_CH_RDTR_LO)), 1);
    }

    #[test]
    fn the_receive_tail_points_at_the_last_descriptor_not_past_the_end() {
        // The opposite of the usual convention: the tail is "the last
        // descriptor the driver owns", and a receive ring starts with every
        // descriptor handed over. One past the end would be an address
        // OUTSIDE the ring, and the engine would fetch a descriptor from
        // whatever the allocator put there next.
        let base = 0x0000_0001_0000_0000u64;
        let count = 8u32;
        let fake = FakeXgmac::new();
        program_rx_ring(&fake, 0, base, count, 2048).unwrap();

        let tail = fake.read32(dma_channel_reg(0, DMA_CH_RDTR_LO));
        let last = base + (count as u64 - 1) * 16;
        assert_eq!(tail, last as u32);
        // Explicitly NOT one past the end.
        assert_ne!(tail, (base + count as u64 * 16) as u32);
        // And it is inside the ring.
        assert!((tail as u64) < base as u32 as u64 + count as u64 * 16);
    }

    #[test]
    fn the_tail_is_the_low_half_only() {
        // There is no RDTR_HI: the engine takes the high half from the list
        // address register. So the tail write is one word.
        let base = 0x0000_0009_8000_0000u64;
        let fake = FakeXgmac::new();
        program_rx_ring(&fake, 0, base, 2, 2048).unwrap();
        let expect = (base + 16) as u32;
        assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_RDTR_LO)), expect);
        // The high half appears only in RDLR_HI.
        assert_eq!(fake.read32(dma_channel_reg(0, DMA_CH_RDLR_HI)), 9);
    }

    #[test]
    fn interrupt_coalescing_marks_the_last_descriptor_of_each_group() {
        // With nothing set, every descriptor interrupts.
        for i in 0..4u32 {
            assert!(rx_desc_wants_interrupt(i, 0, 0));
        }
        // With a frame count, `(index + 1) % rx_frames` --- so the LAST
        // descriptor of each group carries the flag, not the first. Marking
        // index 0 instead would interrupt one descriptor early, every time.
        assert!(!rx_desc_wants_interrupt(0, 0, 4));
        assert!(!rx_desc_wants_interrupt(1, 0, 4));
        assert!(!rx_desc_wants_interrupt(2, 0, 4));
        assert!(rx_desc_wants_interrupt(3, 0, 4));
        assert!(rx_desc_wants_interrupt(7, 0, 4));
        // With only a time budget set, no descriptor is marked: the timer
        // does the work.
        for i in 0..4u32 {
            assert!(!rx_desc_wants_interrupt(i, 50, 0));
        }
    }

    #[test]
    fn a_receive_descriptor_carries_two_buffers_in_four_words() {
        // Header buffer in desc0/desc1, payload buffer in desc2/desc3, each
        // low-then-high. All four words are address.
        let hdr = 0x0000_0002_1111_0000u64;
        let buf = 0x0000_0003_2222_0000u64;
        let d = rx_desc_init(hdr, buf, false);
        assert_eq!(d.desc0, 0x1111_0000);
        assert_eq!(d.desc1, 2);
        assert_eq!(d.desc2, 0x2222_0000);
        assert_eq!(d.desc3, 3);
    }

    #[test]
    fn the_control_bits_live_inside_an_address_word() {
        // desc3 is the high half of the payload address AND where OWN and
        // INTE live. It works because a physical address has nothing in its
        // top bits, but writing desc3 as "just the address" clears OWN, and
        // writing it as "just the flags" destroys the address.
        let buf = 0x0000_0003_2222_0000u64;
        let plain = rx_desc_init(0, buf, false);
        let with_inte = rx_desc_init(0, buf, true);
        assert_eq!(with_inte.desc3, plain.desc3 | DESC3_CTXT);
        // The address half survives the flag.
        assert_eq!(with_inte.desc3 & 0x0000_ffff, 3);
        assert!(rx_desc3_inte(with_inte.desc3));
        assert!(!rx_desc3_inte(plain.desc3));
    }

    #[test]
    fn the_descriptor_is_not_handed_over_by_this_function() {
        // OWN is left off on purpose: Linux sets it in a separate write with
        // a barrier either side, because the receive engine may already be
        // running over this ring. Returning the descriptor without OWN makes
        // that ordering the caller's explicit step.
        let d = rx_desc_init(0x1000, 0x2000, true);
        assert!(!desc_owned_by_dma(d.desc3));
        // And handing it over is then one OR, which is what the caller does
        // after its barrier.
        assert!(desc_owned_by_dma(d.desc3 | DESC3_OWN));
    }

    // ——— Tanda 11: a frame in the transmit ring ———

    fn fragment(dma: u64, len: u32) -> TxFragment {
        TxFragment { dma, len }
    }

    fn one_frame(fragments: &[TxFragment]) -> TxFrame<'_> {
        TxFrame {
            fragments,
            total_len: fragments.iter().map(|f| f.len).sum(),
            checksum_offload: false,
            wants_interrupt: false,
        }
    }

    #[test]
    fn the_first_descriptor_is_the_only_one_left_unowned() {
        let frags = [
            fragment(0x1000, 100),
            fragment(0x2000, 200),
            fragment(0x3000, 300),
        ];
        let mut ring = [Desc::default(); 8];
        let next = build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        assert_eq!(next, 3);
        // Every descriptor but the first is handed over as it is built; the
        // first waits for the caller's barrier.
        assert!(!desc_owned_by_dma(ring[0].desc3));
        assert!(desc_owned_by_dma(ring[1].desc3));
        assert!(desc_owned_by_dma(ring[2].desc3));
    }

    #[test]
    fn handing_the_frame_over_touches_only_the_first_descriptor() {
        let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        let before = ring;
        hand_tx_frame_to_dma(&mut ring, 0);
        assert_eq!(ring[0].desc3, before[0].desc3 | DESC3_OWN);
        assert_eq!(ring[1], before[1]);
        // And the rest of the first descriptor is untouched: OWN is an OR,
        // not a rewrite of the word.
        assert_eq!(ring[0].desc0, before[0].desc0);
        assert_eq!(ring[0].desc2, before[0].desc2);
    }

    #[test]
    fn first_and_last_are_different_descriptors_and_different_bits() {
        let frags = [
            fragment(0x1000, 100),
            fragment(0x2000, 200),
            fragment(0x3000, 300),
        ];
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        assert_eq!(ring[0].desc3 & DESC3_FD, DESC3_FD);
        assert_eq!(ring[0].desc3 & DESC3_LD, 0);
        assert_eq!(ring[1].desc3 & (DESC3_FD | DESC3_LD), 0);
        assert_eq!(ring[2].desc3 & DESC3_LD, DESC3_LD);
        assert_eq!(ring[2].desc3 & DESC3_FD, 0);
    }

    #[test]
    fn a_frame_of_one_fragment_is_both_first_and_last() {
        let frags = [fragment(0x1000, 60)];
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        assert_eq!(ring[0].desc3 & (DESC3_FD | DESC3_LD), DESC3_FD | DESC3_LD);
        // Still unowned: a one-fragment frame is handed over the same way.
        assert!(!desc_owned_by_dma(ring[0].desc3));
    }

    #[test]
    fn the_two_lengths_have_different_scopes() {
        let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        // desc2 holds THIS fragment's length, on every descriptor.
        assert_eq!(
            get_bits(ring[0].desc2, TX_DESC2_HL_B1L.0, TX_DESC2_HL_B1L.1),
            100
        );
        assert_eq!(
            get_bits(ring[1].desc2, TX_DESC2_HL_B1L.0, TX_DESC2_HL_B1L.1),
            200
        );
        // desc3 holds the WHOLE frame's length, on the first only.
        assert_eq!(get_bits(ring[0].desc3, TX_DESC3_FL.0, TX_DESC3_FL.1), 300);
        assert_eq!(get_bits(ring[1].desc3, TX_DESC3_FL.0, TX_DESC3_FL.1), 0);
    }

    #[test]
    fn the_interrupt_bit_is_in_desc2_not_desc3() {
        let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
        let mut frame = one_frame(&frags);
        frame.wants_interrupt = true;
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &frame).unwrap();
        // IC is bit 31 of desc2. Bit 31 of desc3 is OWN, and the last
        // descriptor has that too, so the test pins the WORD, not just the
        // position.
        assert_eq!(ring[1].desc2 & (1 << 31), 1 << 31);
        assert_eq!(ring[0].desc2 & (1 << 31), 0);
        // And without it asked for, nothing sets bit 31 of desc2 anywhere.
        let mut quiet = [Desc::default(); 8];
        build_tx_frame(&mut quiet, 0, &one_frame(&frags)).unwrap();
        assert_eq!(quiet[0].desc2 & (1 << 31), 0);
        assert_eq!(quiet[1].desc2 & (1 << 31), 0);
    }

    #[test]
    fn the_checksum_bits_ride_every_descriptor() {
        let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
        let mut frame = one_frame(&frags);
        frame.checksum_offload = true;
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &frame).unwrap();
        // Linux sets CIC on the first descriptor and again in the loop over
        // the rest: it is not a per-frame flag that the first carries.
        for d in &ring[..2] {
            assert_eq!(
                get_bits(d.desc3, TX_DESC3_CIC.0, TX_DESC3_CIC.1),
                TX_DESC3_CIC_FULL
            );
        }
    }

    #[test]
    fn the_crc_and_pad_field_is_left_at_zero() {
        let frags = [fragment(0x1000, 100)];
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        // Zero is "insert CRC and pad", which is what an ordinary frame
        // wants. A stray one here would send frames without a CRC.
        assert_eq!(get_bits(ring[0].desc3, TX_DESC3_CPC.0, TX_DESC3_CPC.1), 0);
    }

    #[test]
    fn the_address_is_split_low_half_first() {
        let frags = [fragment(0x0000_0001_dead_beef, 64)];
        let mut ring = [Desc::default(); 8];
        build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        assert_eq!(ring[0].desc0, 0xdead_beef);
        assert_eq!(ring[0].desc1, 1);
    }

    #[test]
    fn the_ring_wraps_and_the_next_index_wraps_with_it() {
        let frags = [
            fragment(0x1000, 100),
            fragment(0x2000, 200),
            fragment(0x3000, 300),
        ];
        let mut ring = [Desc::default(); 4];
        let next = build_tx_frame(&mut ring, 3, &one_frame(&frags)).unwrap();
        assert_eq!(next, 2);
        // Index 3 is the first, then 0 and 1.
        assert_eq!(ring[3].desc3 & DESC3_FD, DESC3_FD);
        assert!(!desc_owned_by_dma(ring[3].desc3));
        assert_eq!(ring[1].desc3 & DESC3_LD, DESC3_LD);
        // Descriptor 2 was never touched.
        assert_eq!(ring[2], Desc::default());
    }

    #[test]
    fn a_frame_that_does_not_fit_the_ring_is_refused() {
        let frags = [
            fragment(0x1000, 10),
            fragment(0x2000, 10),
            fragment(0x3000, 10),
        ];
        let mut ring = [Desc::default(); 2];
        assert!(build_tx_frame(&mut ring, 0, &one_frame(&frags)).is_err());
        // Nothing was written before the refusal.
        assert_eq!(ring[0], Desc::default());
    }

    #[test]
    fn the_lengths_must_agree_with_each_other() {
        let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
        let mut ring = [Desc::default(); 8];
        let mut frame = one_frame(&frags);
        frame.total_len = 299;
        // A frame length that disagrees with the fragments would make the
        // engine send 299 bytes out of 300 handed to it, silently.
        assert!(build_tx_frame(&mut ring, 0, &frame).is_err());
        frame.total_len = 300;
        assert!(build_tx_frame(&mut ring, 0, &frame).is_ok());
    }

    #[test]
    fn an_empty_frame_and_an_empty_fragment_are_both_refused() {
        let mut ring = [Desc::default(); 8];
        assert!(build_tx_frame(&mut ring, 0, &one_frame(&[])).is_err());
        let frags = [fragment(0x1000, 0)];
        assert!(build_tx_frame(&mut ring, 0, &one_frame(&frags)).is_err());
    }

    #[test]
    fn a_length_too_big_for_its_field_is_refused_not_truncated() {
        let mut ring = [Desc::default(); 8];
        // A fragment of exactly 1 << 14 would be written as zero.
        let frags = [fragment(0x1000, 1 << TX_DESC2_HL_B1L.1)];
        assert!(build_tx_frame(&mut ring, 0, &one_frame(&frags)).is_err());
        // And a frame length of 1 << 15 likewise, even split across
        // fragments that each fit.
        let big = [fragment(0x1000, 16383), fragment(0x2000, 16385)];
        assert!(build_tx_frame(&mut ring, 0, &one_frame(&big)).is_err());
    }

    #[test]
    fn the_transmit_tail_points_past_the_frame_not_at_it() {
        let fake = FakeXgmac::new();
        let ring_paddr = 0x10_0000u64;
        let frags = [fragment(0x1000, 100), fragment(0x2000, 200)];
        let mut ring = [Desc::default(); 8];
        let next = build_tx_frame(&mut ring, 0, &one_frame(&frags)).unwrap();
        kick_tx(&fake, 0, desc_paddr(ring_paddr, next));
        // Two descriptors used, so the tail is descriptor 2: one PAST the
        // last one written. The receive tail of program_rx_ring points AT
        // its last descriptor instead.
        let expected = ring_paddr + 2 * core::mem::size_of::<Desc>() as u64;
        let log = fake.log();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0],
            Access::Write(dma_channel_reg(0, DMA_CH_TDTR_LO), expected as u32)
        );
    }

    #[test]
    fn the_transmit_tail_is_the_low_half_only() {
        let fake = FakeXgmac::new();
        // There is no TDTR_HI: the ring lives wholly inside the window the
        // TDLR pair established, so only the low half moves.
        kick_tx(&fake, 1, 0x0000_0007_1234_5678);
        let log = fake.log();
        assert_eq!(log.len(), 1);
        assert_eq!(
            log[0],
            Access::Write(dma_channel_reg(1, DMA_CH_TDTR_LO), 0x1234_5678)
        );
    }

    // ——— Tanda 12: reading a received frame back ———

    /// A written-back descriptor: desc3 assembled from flags, the rest zero
    /// unless a test puts something there.
    fn written_back(desc3: u32) -> Desc {
        Desc {
            desc0: 0,
            desc1: 0,
            desc2: 0,
            desc3,
        }
    }

    fn whole_frame(extra: u32) -> Desc {
        written_back(DESC3_FD | DESC3_LD | extra)
    }

    #[test]
    fn a_descriptor_the_chip_still_owns_yields_nothing() {
        // Every other word was written by the chip and is not settled until
        // OWN comes back, so the OWN test comes first and alone.
        let d = written_back(DESC3_OWN | DESC3_FD | DESC3_LD | 1500);
        assert_eq!(read_rx_desc(&d), RxRead::NotYours);
    }

    #[test]
    fn a_context_descriptor_is_not_a_frame() {
        let d = written_back(DESC3_CTXT);
        assert_eq!(read_rx_desc(&d), RxRead::Context);
        // And CTXT is only read once OWN is clear: with OWN set it is still
        // the chip's.
        let busy = written_back(DESC3_OWN | DESC3_CTXT);
        assert_eq!(read_rx_desc(&busy), RxRead::NotYours);
    }

    #[test]
    fn an_error_summary_on_its_own_is_not_an_error() {
        // This is the one that costs good frames. Linux: "no error if err is
        // 0 OR etlt is 0". ES set with ETLT zero is a CLEAN frame.
        let d = set_bits(0, RX_DESC3_ES.0, RX_DESC3_ES.1, 1);
        assert_eq!(rx_frame_verdict(d), RxVerdict::Clean);
        // And an ETLT on its own, with no error summary, likewise.
        let e = set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, 0x0c);
        assert_eq!(rx_frame_verdict(e), RxVerdict::Clean);
        // It takes both.
        assert_eq!(rx_frame_verdict(d | e), RxVerdict::FrameBad);
    }

    #[test]
    fn a_bad_checksum_is_not_a_bad_frame() {
        let err = set_bits(0, RX_DESC3_ES.0, RX_DESC3_ES.1, 1);
        for etlt in [ETLT_IP_HEADER_CSUM, ETLT_PAYLOAD_CSUM] {
            let d = err | set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, etlt);
            assert_eq!(
                rx_frame_verdict(d),
                RxVerdict::ChecksumBad,
                "etlt {:#04x}",
                etlt
            );
        }
        // 0x09 and 0x0a only mean a TUNNEL checksum error on a tunnelled
        // frame; with no tunnel support they are damage, which is where
        // Linux puts them too.
        for etlt in [0x09u32, 0x0a] {
            let d = err | set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, etlt);
            assert_eq!(
                rx_frame_verdict(d),
                RxVerdict::FrameBad,
                "etlt {:#04x}",
                etlt
            );
        }
    }

    #[test]
    fn the_same_etlt_value_is_a_vlan_tag_or_damage_depending_on_the_error_bit() {
        let etlt = set_bits(0, RX_DESC3_ETLT.0, RX_DESC3_ETLT.1, ETLT_VLAN_TAG);
        // Without the error summary: a VLAN tag, and desc0 carries it.
        let mut tagged = whole_frame(etlt);
        tagged.desc0 = 0xffff_0123;
        match read_rx_desc(&tagged) {
            RxRead::Normal(d) => {
                assert_eq!(d.verdict, Some(RxVerdict::Clean));
                // OVT is the low SIXTEEN bits of desc0, not all of it.
                assert_eq!(d.vlan_tag, Some(0x0123));
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
        // With it: damage, and no tag is reported.
        let mut bad = whole_frame(etlt | set_bits(0, RX_DESC3_ES.0, RX_DESC3_ES.1, 1));
        bad.desc0 = 0xffff_0123;
        match read_rx_desc(&bad) {
            RxRead::Normal(d) => {
                assert_eq!(d.verdict, Some(RxVerdict::FrameBad));
                assert_eq!(d.vlan_tag, None);
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn the_two_lengths_are_valid_at_opposite_ends_of_the_frame() {
        // Same descriptor word layout, opposite conditions: HL only on the
        // first, PL only on the last. A middle descriptor has neither, and
        // whatever sits in those bits is leftover.
        let mut middle = written_back(0x3fff);
        middle.desc2 = 0x03ff;
        match read_rx_desc(&middle) {
            RxRead::Normal(d) => {
                assert!(!d.first);
                assert!(!d.last);
                assert_eq!(d.header_len, None);
                assert_eq!(d.packet_len, None);
                assert_eq!(d.verdict, None);
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn the_first_descriptor_carries_the_header_length_and_the_last_the_frame_length() {
        let mut first = written_back(DESC3_FD | 1234);
        first.desc2 = 54;
        match read_rx_desc(&first) {
            RxRead::Normal(d) => {
                assert_eq!(d.header_len, Some(54));
                // 1234 is sitting in PL's bits, but this is not the last
                // descriptor, so it is not a frame length.
                assert_eq!(d.packet_len, None);
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
        let mut last = written_back(DESC3_LD | 1234);
        last.desc2 = 54;
        match read_rx_desc(&last) {
            RxRead::Normal(d) => {
                assert_eq!(d.packet_len, Some(1234));
                assert_eq!(d.header_len, None);
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn the_header_length_field_is_ten_bits_not_fourteen() {
        // desc2's length field on the way back is TEN bits wide, while the
        // transmit side's length in the same word is fourteen.
        let mut d = written_back(DESC3_FD);
        d.desc2 = 0xffff_ffff;
        match read_rx_desc(&d) {
            RxRead::Normal(r) => assert_eq!(r.header_len, Some(0x3ff)),
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn desc1_is_a_hash_only_when_the_chip_says_so() {
        // On the way in, desc1 was the high half of the header address. The
        // chip overwrites it, but only RSV says the new value means
        // anything.
        let mut quiet = whole_frame(0);
        quiet.desc1 = 0xdead_beef;
        match read_rx_desc(&quiet) {
            RxRead::Normal(d) => {
                assert_eq!(d.rss_hash, None);
                assert!(!d.rss_covers_ports);
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
        let mut hashed = whole_frame(set_bits(0, RX_DESC3_RSV.0, RX_DESC3_RSV.1, 1));
        hashed.desc1 = 0xdead_beef;
        match read_rx_desc(&hashed) {
            RxRead::Normal(d) => assert_eq!(d.rss_hash, Some(0xdead_beef)),
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn only_the_four_transport_types_hash_over_the_ports() {
        for l34t in [L34T_IPV4_TCP, L34T_IPV4_UDP, L34T_IPV6_TCP, L34T_IPV6_UDP] {
            assert!(rss_hash_covers_ports(l34t), "l34t {}", l34t);
        }
        // ICMP (3, 11) and the two "unknown"s (7, 15) hash over the
        // addresses alone, as does anything else the field can hold.
        for l34t in [0u32, 3, 7, 11, 15, 4, 8, 12] {
            assert!(!rss_hash_covers_ports(l34t), "l34t {}", l34t);
        }
    }

    #[test]
    fn the_l34_type_is_only_read_when_there_is_a_hash() {
        // L34T holds a protocol whether or not RSV is set, so reading it
        // unconditionally would report a port hash on a frame that has no
        // hash at all.
        let l34t = set_bits(0, RX_DESC3_L34T.0, RX_DESC3_L34T.1, L34T_IPV4_TCP);
        match read_rx_desc(&whole_frame(l34t)) {
            RxRead::Normal(d) => {
                assert_eq!(d.rss_hash, None);
                assert!(!d.rss_covers_ports);
            }
            other => panic!("not a normal descriptor: {:?}", other),
        }
        let rsv = set_bits(0, RX_DESC3_RSV.0, RX_DESC3_RSV.1, 1);
        match read_rx_desc(&whole_frame(l34t | rsv)) {
            RxRead::Normal(d) => assert!(d.rss_covers_ports),
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn a_context_descriptor_is_announced_a_descriptor_ahead() {
        let cda = set_bits(0, RX_DESC3_CDA.0, RX_DESC3_CDA.1, 1);
        match read_rx_desc(&whole_frame(cda)) {
            RxRead::Normal(d) => assert!(d.context_next),
            other => panic!("not a normal descriptor: {:?}", other),
        }
        match read_rx_desc(&whole_frame(0)) {
            RxRead::Normal(d) => assert!(!d.context_next),
            other => panic!("not a normal descriptor: {:?}", other),
        }
    }

    #[test]
    fn the_flag_bits_do_not_collide_with_each_other() {
        // FD, LD, CDA, RSV, CTXT and OWN all live in desc3's top byte, and
        // ETLT, ES and L34T below them. Setting each alone must leave the
        // rest of the reading unchanged.
        let fields = [
            RX_DESC3_ES,
            RX_DESC3_ETLT,
            RX_DESC3_L34T,
            RX_DESC3_RSV,
            RX_DESC3_CDA,
        ];
        for (i, a) in fields.iter().enumerate() {
            let word = set_bits(0, a.0, a.1, (1 << a.1) - 1);
            for (j, b) in fields.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert_eq!(get_bits(word, b.0, b.1), 0, "{:?} bleeds into {:?}", a, b);
            }
            // ... and none of them reaches OWN, CTXT, FD or LD.
            assert_eq!(word & (DESC3_OWN | DESC3_CTXT | DESC3_FD | DESC3_LD), 0);
        }
    }

    #[test]
    fn the_tunnel_bit_is_named_but_not_acted_on() {
        assert!(rx_is_tunnelled(1 << 11));
        assert!(!rx_is_tunnelled(0));
        // It is bit 11 of desc2, above the ten-bit header length, so a full
        // header length does not look like a tunnel.
        assert!(!rx_is_tunnelled(0x3ff));
    }
}
