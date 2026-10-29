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

mod datapath;
pub use datapath::*;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_datapath;
