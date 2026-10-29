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

// ——— Interrupts: enabling them, and reading what fired ———

/// `DMA_CH_IER_TIE`, index 0: a frame finished transmitting.
pub const DMA_CH_IER_TIE: (u32, u32) = (0, 1);
/// `DMA_CH_IER_TXSE`, index 1: the transmit process stopped.
pub const DMA_CH_IER_TXSE: (u32, u32) = (1, 1);
/// `DMA_CH_IER_TBUE`, index 2: the transmit ring ran out of descriptors.
pub const DMA_CH_IER_TBUE: (u32, u32) = (2, 1);
/// `DMA_CH_IER_RIE`, index 6: a frame arrived.
pub const DMA_CH_IER_RIE: (u32, u32) = (6, 1);
/// `DMA_CH_IER_RBUE`, index 7: the receive ring ran out of descriptors.
pub const DMA_CH_IER_RBUE: (u32, u32) = (7, 1);
/// `DMA_CH_IER_RSE`, index 8: the receive process stopped.
pub const DMA_CH_IER_RSE: (u32, u32) = (8, 1);
/// `DMA_CH_IER_FBEE`, index 12: a fatal bus error.
pub const DMA_CH_IER_FBEE: (u32, u32) = (12, 1);

/// `DMA_CH_IER_AIE`, index 14: the abnormal interrupt summary, on a core at
/// [`FLUSH_MIN_SNPSVER`] or above.
pub const DMA_CH_IER_AIE: (u32, u32) = (14, 1);
/// `DMA_CH_IER_NIE`, index 15: the normal interrupt summary, on a core at
/// [`FLUSH_MIN_SNPSVER`] or above.
pub const DMA_CH_IER_NIE: (u32, u32) = (15, 1);
/// `DMA_CH_IER_AIE20`, index 15: the abnormal summary on an older core.
///
/// The same bit 15 that is `NIE` above. The two summary bits shifted by one
/// when the core moved to 0x21, so a mask built for the new layout and
/// written to an old core enables the ABNORMAL summary where it meant the
/// normal one, and leaves the normal one off --- an interface that reports
/// bus errors and never reports a received frame.
pub const DMA_CH_IER_AIE20: (u32, u32) = (15, 1);
/// `DMA_CH_IER_NIE20`, index 16: the normal summary on an older core.
pub const DMA_CH_IER_NIE20: (u32, u32) = (16, 1);

/// `DMA_CH_SR_TI`, index 0.
pub const DMA_CH_SR_TI: (u32, u32) = (0, 1);
/// `DMA_CH_SR_TPS`, index 1.
pub const DMA_CH_SR_TPS: (u32, u32) = (1, 1);
/// `DMA_CH_SR_TBU`, index 2.
pub const DMA_CH_SR_TBU: (u32, u32) = (2, 1);
/// `DMA_CH_SR_RI`, index 6.
pub const DMA_CH_SR_RI: (u32, u32) = (6, 1);
/// `DMA_CH_SR_RBU`, index 7.
pub const DMA_CH_SR_RBU: (u32, u32) = (7, 1);
/// `DMA_CH_SR_RPS`, index 8.
pub const DMA_CH_SR_RPS: (u32, u32) = (8, 1);
/// `DMA_CH_SR_FBE`, index 12.
pub const DMA_CH_SR_FBE: (u32, u32) = (12, 1);

/// The two summary enables for a core of this version: `(normal, abnormal)`.
/// Everything below bit 14 kept its place; only these two moved.
#[inline]
pub const fn summary_enables(snps_ver: u8) -> ((u32, u32), (u32, u32)) {
    if snps_ver < FLUSH_MIN_SNPSVER {
        (DMA_CH_IER_NIE20, DMA_CH_IER_AIE20)
    } else {
        (DMA_CH_IER_NIE, DMA_CH_IER_AIE)
    }
}

/// Build a channel's interrupt enable mask. `xgbe_enable_dma_interrupts`'s
/// `curr_ier`, for the case this driver has: one interrupt for the whole
/// device, so both the transmit-complete and receive-complete enables are
/// wanted.
///
/// `RBUE` goes on for a receive ring but there is no `TBUE` for a transmit
/// one. A transmit ring running dry is the ordinary state of an idle
/// interface and the driver refills it when it next sends; a receive ring
/// running dry means frames are being dropped on the floor and somebody has
/// to be told.
#[inline]
pub const fn channel_interrupt_mask(snps_ver: u8, has_tx: bool, has_rx: bool) -> u32 {
    let (nie, aie) = summary_enables(snps_ver);
    let mut ier = (1 << nie.0) | (1 << aie.0) | (1 << DMA_CH_IER_FBEE.0);
    if has_tx {
        ier |= 1 << DMA_CH_IER_TIE.0;
    }
    if has_rx {
        ier |= (1 << DMA_CH_IER_RBUE.0) | (1 << DMA_CH_IER_RIE.0);
    }
    ier
}

/// Turn a channel's interrupts on: acknowledge whatever is already pending,
/// then write the mask whole.
///
/// The order is not a style choice. A bit left standing in the status
/// register from before the mask lands fires the instant it does, and the
/// handler then runs for an event that happened while the interface was
/// down. And the mask is written WHOLE rather than OR'd in, because the
/// summary bits moved between core versions: a read-modify-write would
/// preserve the other version's summary bit from whatever configuration ran
/// before.
pub fn enable_channel_interrupts(
    acc: &dyn MmioAccess,
    channel: u32,
    snps_ver: u8,
    has_tx: bool,
    has_rx: bool,
) {
    ack_channel_interrupts(acc, channel);
    acc.write32(
        dma_channel_reg(channel, DMA_CH_IER),
        channel_interrupt_mask(snps_ver, has_tx, has_rx),
    );
}

/// Turn a channel's interrupts off. Zero is zero in either layout, so this
/// one does not need the version.
#[inline]
pub fn disable_channel_interrupts(acc: &dyn MmioAccess, channel: u32) {
    acc.write32(dma_channel_reg(channel, DMA_CH_IER), 0);
}

/// Acknowledge a channel's pending interrupts, and return what was pending.
///
/// The status register is write-one-to-clear, so it is cleared by writing
/// back exactly what was read: writing a zero clears NOTHING, and writing all
/// ones would acknowledge events that had not happened. Reading first is also
/// what makes this the handler's one call --- the bits are gone after the
/// write, so a second read would come back empty.
pub fn ack_channel_interrupts(acc: &dyn MmioAccess, channel: u32) -> u32 {
    let reg = dma_channel_reg(channel, DMA_CH_SR);
    let status = acc.read32(reg);
    acc.write32(reg, status);
    status
}

/// What a channel's status register was reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DmaStatus {
    /// `TI`: a frame went out.
    pub tx_complete: bool,
    /// `RI`: a frame came in.
    pub rx_complete: bool,
    /// `TBU`: the transmit ring is empty. Expected on an idle interface.
    pub tx_buffer_unavailable: bool,
    /// `RBU`: the receive ring is empty, and frames are being dropped.
    pub rx_buffer_unavailable: bool,
    /// `TPS`: the transmit process stopped.
    pub tx_stopped: bool,
    /// `RPS`: the receive process stopped.
    pub rx_stopped: bool,
    /// `FBE`: a fatal bus error. Nothing short of a reset recovers from this.
    pub fatal_bus_error: bool,
}

/// Decode a channel status word.
#[inline]
pub const fn decode_dma_status(sr: u32) -> DmaStatus {
    DmaStatus {
        tx_complete: get_bits(sr, DMA_CH_SR_TI.0, DMA_CH_SR_TI.1) != 0,
        rx_complete: get_bits(sr, DMA_CH_SR_RI.0, DMA_CH_SR_RI.1) != 0,
        tx_buffer_unavailable: get_bits(sr, DMA_CH_SR_TBU.0, DMA_CH_SR_TBU.1) != 0,
        rx_buffer_unavailable: get_bits(sr, DMA_CH_SR_RBU.0, DMA_CH_SR_RBU.1) != 0,
        tx_stopped: get_bits(sr, DMA_CH_SR_TPS.0, DMA_CH_SR_TPS.1) != 0,
        rx_stopped: get_bits(sr, DMA_CH_SR_RPS.0, DMA_CH_SR_RPS.1) != 0,
        fatal_bus_error: get_bits(sr, DMA_CH_SR_FBE.0, DMA_CH_SR_FBE.1) != 0,
    }
}

/// Whether anything in this status needs the driver to act. A status of all
/// zeroes happens: on a shared line another device's interrupt brings the
/// handler here too.
#[inline]
pub const fn dma_status_is_quiet(s: &DmaStatus) -> bool {
    !(s.tx_complete
        || s.rx_complete
        || s.tx_buffer_unavailable
        || s.rx_buffer_unavailable
        || s.tx_stopped
        || s.rx_stopped
        || s.fatal_bus_error)
}

// ——— Sharing the FIFO out between the queues ———

/// `XGMAC_FIFO_UNIT`: the granularity the FIFO is handed out in. Everything
/// below is counted in these, never in bytes.
pub const FIFO_UNIT: u32 = 256;
/// `XGMAC_FIFO_MIN_ALLOC`: what a non-priority receive queue gets.
pub const FIFO_MIN_ALLOC: u32 = 2048;
/// `IEEE_8021QAZ_MAX_TCS`: how many receive queues carry ordinary traffic.
/// Receive queues 9 and up (index 8 and up) carry PTP and DCB control frames
/// and do not need a large FIFO.
pub const MAX_PRIO_QUEUES: u32 = 8;

/// `MTL_Q_TQOMR_TQS`, index 16, width **10**.
pub const MTL_Q_TQOMR_TQS: (u32, u32) = (16, 10);
/// `MTL_Q_RQOMR_RQS`, index 16, width **9**.
///
/// One bit narrower than the transmit side's field at the same index of the
/// matching register. A helper that took the width as read from either side
/// would silently halve, or overflow, the other's largest queue.
pub const MTL_Q_RQOMR_RQS: (u32, u32) = (16, 9);

/// Turn a per-queue size in bytes into the value the register takes:
/// units of [`FIFO_UNIT`], minus one, because zero there means ONE unit.
///
/// Note where the asymmetry is. A queue of 256 bytes and a queue of 511
/// bytes both come out as zero, which is right --- the hardware cannot
/// express the difference. But a queue of *less* than 256 bytes also comes
/// out as zero, which grants it 256 bytes it was never given. Linux does the
/// same (`if (p_fifo) p_fifo--`), because the alternative is a queue with no
/// FIFO at all, and that does not work either.
#[inline]
pub const fn fifo_units(bytes: u32) -> u32 {
    let units = bytes / FIFO_UNIT;
    if units > 0 {
        units - 1
    } else {
        0
    }
}

/// The register value every queue gets when the FIFO is split equally.
/// `xgbe_calculate_equal_fifo`.
///
/// The remainder of the division is dropped rather than handed to queue
/// zero. A few hundred spare bytes are worth less than every queue behaving
/// the same way under load.
pub fn equal_fifo_share(total_bytes: u32, queues: u32) -> DeviceResult<u32> {
    if queues == 0 {
        return Err(DeviceError::InvalidParam);
    }
    Ok(fifo_units(total_bytes / queues))
}

/// How many of `queues` are priority queues, i.e. get a share of the FIFO
/// rather than the fixed minimum. `XGMAC_PRIO_QUEUES`.
#[inline]
pub const fn prio_queues(queues: u32) -> u32 {
    if queues < MAX_PRIO_QUEUES {
        queues
    } else {
        MAX_PRIO_QUEUES
    }
}

/// Program the transmit FIFO split. `xgbe_config_tx_fifo_size`.
///
/// Every transmit queue is a priority queue: the non-priority rule below is
/// a receive-side thing only.
pub fn config_tx_fifo(acc: &dyn MmioAccess, total_bytes: u32, queues: u32) -> DeviceResult<()> {
    let share = equal_fifo_share(total_bytes, queues)?;
    for q in 0..queues {
        write_field(
            acc,
            mtl_queue_reg(q, MTL_Q_TQOMR),
            MTL_Q_TQOMR_TQS.0,
            MTL_Q_TQOMR_TQS.1,
            share,
        );
    }
    Ok(())
}

/// Program the receive FIFO split. `xgbe_config_rx_fifo_size` without DCB:
/// the queues above [`MAX_PRIO_QUEUES`] take [`FIFO_MIN_ALLOC`] each off the
/// top, and what is left is split equally among the rest.
///
/// The subtraction comes FIRST. Splitting the whole FIFO and then carving
/// the minimum out of it would promise the priority queues more than exists,
/// and the overrun does not show up as an error --- it shows up as frames
/// dropped under load on whichever queue the hardware runs out on.
pub fn config_rx_fifo(acc: &dyn MmioAccess, total_bytes: u32, queues: u32) -> DeviceResult<()> {
    let prio = prio_queues(queues);
    if prio == 0 {
        return Err(DeviceError::InvalidParam);
    }
    let nonprio = queues - prio;
    let reserved = nonprio
        .checked_mul(FIFO_MIN_ALLOC)
        .ok_or(DeviceError::InvalidParam)?;
    if reserved >= total_bytes {
        return Err(DeviceError::InvalidParam);
    }
    let share = equal_fifo_share(total_bytes - reserved, prio)?;
    let minimum = fifo_units(FIFO_MIN_ALLOC);
    for q in 0..queues {
        let value = if q < prio { share } else { minimum };
        write_field(
            acc,
            mtl_queue_reg(q, MTL_Q_RQOMR),
            MTL_Q_RQOMR_RQS.0,
            MTL_Q_RQOMR_RQS.1,
            value,
        );
    }
    Ok(())
}

// ——— When to ask the other end to stop ———

/// `MTL_Q_RQFCR`: a receive queue's flow-control thresholds.
pub const MTL_Q_RQFCR: usize = 0x50;
/// `MTL_Q_RQFCR_RFA`, index **1**, width 6. Not index 0: bit 0 of this
/// register is not part of the field, so a threshold written from the bottom
/// of the word comes out halved.
pub const MTL_Q_RQFCR_RFA: (u32, u32) = (1, 6);
/// `MTL_Q_RQFCR_RFD`, index 17, width 6.
pub const MTL_Q_RQFCR_RFD: (u32, u32) = (17, 6);

/// `XGMAC_FLOW_CONTROL_UNIT`: the granularity of a threshold.
///
/// Note it is NOT [`FIFO_UNIT`], and the bias below is not the FIFO's
/// either. Two neighbouring quantities in the same driver, both "units minus
/// a bias", with different units and different biases.
pub const FLOW_CONTROL_UNIT: u32 = 512;

/// `XGMAC_FLOW_CONTROL_ALIGN`: round up to a whole [`FLOW_CONTROL_UNIT`].
#[inline]
pub const fn flow_control_align(bytes: u32) -> u32 {
    bytes.div_ceil(FLOW_CONTROL_UNIT) * FLOW_CONTROL_UNIT
}

/// `XGMAC_FLOW_CONTROL_VALUE`: bytes to the register's units, which are
/// [`FLOW_CONTROL_UNIT`] minus **two**.
#[inline]
pub const fn flow_control_value(bytes: u32) -> u32 {
    if bytes < 2 * FLOW_CONTROL_UNIT {
        0
    } else {
        bytes / FLOW_CONTROL_UNIT - 2
    }
}

/// A receive queue's two water marks, as the register takes them.
///
/// Both are measured from FULL, backwards: a threshold of zero means "when
/// 1024 bytes of room are left", and a BIGGER number means the chip asks the
/// other end to stop EARLIER, not later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlowControl {
    /// `RFA`: start asking the other end to pause.
    pub rfa: u32,
    /// `RFD`: stop asking.
    ///
    /// Always above [`FlowControl::rfa`], and because both count backwards
    /// from full that means the queue has to drain FURTHER before the pause
    /// is lifted. Swapping the two gives a pause that is lifted the instant
    /// it is raised, which on a loaded link is a storm of pause frames
    /// rather than no flow control at all.
    pub rfd: u32,
}

/// Whether flow control is off for this queue.
///
/// It is `rfd` that says so, not `rfa`: a zero in `rfa` is a perfectly real
/// threshold --- it is what a queue between 2 and 4 KiB gets --- and reading
/// "off" off it would silently disable flow control on exactly the queues
/// that were configured for it.
#[inline]
pub const fn flow_control_is_off(fc: &FlowControl) -> bool {
    fc.rfd == 0
}

/// The water marks for a receive queue of `q_fifo_bytes`, given the largest
/// frame the interface accepts. `xgbe_queue_flow_control_threshold` without
/// priority-based flow control.
///
/// Four bands, and the first three are constants out of Linux rather than
/// anything derived: under 2 KiB there is no room to pause in at all, and
/// between 2 KiB and a frame the fixed answers are what fits.
pub fn queue_flow_control(q_fifo_bytes: u32, max_frame: u32) -> FlowControl {
    let frame_fifo = flow_control_align(max_frame);

    if q_fifo_bytes <= 2048 {
        // No room to pause in: a queue this small cannot hold what is still
        // in flight by the time the other end sees the pause.
        return FlowControl { rfa: 0, rfd: 0 };
    }
    if q_fifo_bytes <= 4096 {
        // Full - 1024 bytes, and full - 1536.
        return FlowControl { rfa: 0, rfd: 1 };
    }
    if q_fifo_bytes <= frame_fifo {
        // Full - 2048 bytes, and full - 3584.
        return FlowControl { rfa: 2, rfd: 5 };
    }
    if q_fifo_bytes <= frame_fifo * 3 {
        // Pause once just over a frame has arrived; resume with just under
        // half a frame of room left.
        let rfa = q_fifo_bytes - frame_fifo;
        let rfd = rfa + frame_fifo / 2;
        return FlowControl {
            rfa: flow_control_value(rfa),
            rfd: flow_control_value(rfd),
        };
    }
    // Room for more than three frames: pause with just over two frames of
    // space still free.
    let rfa = frame_fifo * 2 + FLOW_CONTROL_UNIT;
    let rfd = rfa + frame_fifo;
    FlowControl {
        rfa: flow_control_value(rfa),
        rfd: flow_control_value(rfd),
    }
}

/// Write a queue's water marks. `xgbe_config_flow_control_threshold`.
pub fn config_flow_control(acc: &dyn MmioAccess, queue: u32, fc: &FlowControl) {
    let reg = mtl_queue_reg(queue, MTL_Q_RQFCR);
    write_field(acc, reg, MTL_Q_RQFCR_RFA.0, MTL_Q_RQFCR_RFA.1, fc.rfa);
    write_field(acc, reg, MTL_Q_RQFCR_RFD.0, MTL_Q_RQFCR_RFD.1, fc.rfd);
}

// ——— Who feeds whom: the three mappings ———

/// `MTL_Q_TQOMR_Q2TCMAP`, index 8, width 3: which traffic class a transmit
/// queue belongs to. Same register as the FIFO size of the batch before,
/// well clear of it.
pub const MTL_Q_TQOMR_Q2TCMAP: (u32, u32) = (8, 3);

/// `MAC_RQC2R`: the first of the registers mapping VLAN priorities to
/// receive queues.
pub const MAC_RQC2R: usize = 0x00a8;
/// `MAC_RQC2_INC` / `MAC_RQC2_Q_PER_REG`: four queues per register, one byte
/// each.
pub const MAC_RQC2_INC: usize = 4;
/// How many queues share one `MAC_RQC2R` register.
pub const MAC_RQC2_Q_PER_REG: u32 = 4;

/// `MTL_RQDCM0R`: the first of the registers mapping receive queues to DMA
/// channels.
pub const MTL_RQDCM0R: usize = 0x1030;
/// `MTL_RQDCM_INC`.
pub const MTL_RQDCM_INC: usize = 4;
/// `MTL_RQDCM_Q_PER_REG`.
pub const MTL_RQDCM_Q_PER_REG: u32 = 4;

/// The value Linux writes in a queue's `MTL_RQDCM0R` byte.
///
/// It is NOT a channel number, which is what a byte in a queue-to-channel
/// map looks like it should hold. It is the bit that says "map this queue to
/// a channel dynamically". A channel index written here --- 0 for the first
/// channel, say --- would read as that bit clear and nail every queue to
/// channel zero, which works right up until the second channel is used.
pub const RQDCM_DYNAMIC: u32 = 0x80;

/// Where index `n`'s byte lives when a map is packed four to a register:
/// the register's offset and the shift within it.
#[inline]
pub const fn packed_byte(base: usize, inc: usize, per_reg: u32, n: u32) -> (usize, u32) {
    let reg = base + (n / per_reg) as usize * inc;
    let shift = (n % per_reg) * 8;
    (reg, shift)
}

/// Which of `buckets` buckets gets item range `(start, len)` for bucket
/// `index`, spreading `total` items as evenly as they go.
///
/// The remainder is NOT all given to the last bucket: it goes one item each
/// to the first few. Eleven queues over four classes is 3, 3, 3, 2 --- not
/// 2, 2, 2, 5. That matters because the classes are served in order, so
/// piling the leftovers on one of them makes that one behave differently
/// under load.
#[inline]
pub const fn bucket_span(index: u32, total: u32, buckets: u32) -> (u32, u32) {
    let base = total / buckets;
    let extra = total % buckets;
    let len = if index < extra { base + 1 } else { base };
    let start = index * base + if index < extra { index } else { extra };
    (start, len)
}

/// Which traffic class a transmit queue belongs to.
#[inline]
pub fn tc_of_tx_queue(queue: u32, tx_queues: u32, tc_count: u32) -> Option<u32> {
    if tc_count == 0 || queue >= tx_queues {
        return None;
    }
    for tc in 0..tc_count {
        let (start, len) = bucket_span(tc, tx_queues, tc_count);
        if queue >= start && queue < start + len {
            return Some(tc);
        }
    }
    None
}

/// Map the transmit queues onto the traffic classes. The first part of
/// `xgbe_config_queue_mapping`.
pub fn map_tx_queues_to_tc(
    acc: &dyn MmioAccess,
    tx_queues: u32,
    tc_count: u32,
) -> DeviceResult<()> {
    if tc_count == 0 || tx_queues < tc_count {
        // A class with no queue has nowhere to put a frame.
        return Err(DeviceError::InvalidParam);
    }
    for queue in 0..tx_queues {
        let tc = tc_of_tx_queue(queue, tx_queues, tc_count).ok_or(DeviceError::InvalidParam)?;
        write_field(
            acc,
            mtl_queue_reg(queue, MTL_Q_TQOMR),
            MTL_Q_TQOMR_Q2TCMAP.0,
            MTL_Q_TQOMR_Q2TCMAP.1,
            tc,
        );
    }
    Ok(())
}

/// The bitmask of VLAN priorities a receive queue is to accept.
///
/// A byte per queue holding a mask of priorities --- not, as the name
/// "priority to queue map" suggests, a byte per priority holding a queue
/// number. The direction is the other way round from the way it is
/// described, and a map written the described way would put queue numbers
/// where the hardware reads priority bits.
#[inline]
pub fn prio_mask_for_queue(queue: u32, prio_queue_count: u32) -> u32 {
    if prio_queue_count == 0 || queue >= prio_queue_count {
        return 0;
    }
    let (start, len) = bucket_span(queue, MAX_PRIO_QUEUES, prio_queue_count);
    let mut mask = 0;
    for prio in start..start + len {
        mask |= 1 << prio;
    }
    mask
}

/// Spread the eight VLAN priorities over the receive queues. The second part
/// of `xgbe_config_queue_mapping`.
pub fn map_prios_to_rx_queues(acc: &dyn MmioAccess, rx_queues: u32) -> DeviceResult<()> {
    let prio = prio_queues(rx_queues);
    if prio == 0 {
        return Err(DeviceError::InvalidParam);
    }
    write_packed_bytes(
        acc,
        MAC_RQC2R,
        MAC_RQC2_INC,
        MAC_RQC2_Q_PER_REG,
        prio,
        |q| prio_mask_for_queue(q, prio),
    );
    Ok(())
}

/// Hand every receive queue to the dynamic channel mapping. The third part
/// of `xgbe_config_queue_mapping`.
pub fn map_rx_queues_to_channels(acc: &dyn MmioAccess, rx_queues: u32) -> DeviceResult<()> {
    if rx_queues == 0 {
        return Err(DeviceError::InvalidParam);
    }
    write_packed_bytes(
        acc,
        MTL_RQDCM0R,
        MTL_RQDCM_INC,
        MTL_RQDCM_Q_PER_REG,
        rx_queues,
        |_| RQDCM_DYNAMIC,
    );
    Ok(())
}

/// Write a byte-packed map, one whole register at a time.
///
/// Whole registers, not fields: these maps have no other tenants, and a
/// read-modify-write per byte would keep a mapping left behind by whatever
/// configuration ran before. The last register is written even when it is
/// only partly filled, with its unused bytes zero.
fn write_packed_bytes(
    acc: &dyn MmioAccess,
    base: usize,
    inc: usize,
    per_reg: u32,
    count: u32,
    mut byte_for: impl FnMut(u32) -> u32,
) {
    let mut value = 0u32;
    for n in 0..count {
        let (reg, shift) = packed_byte(base, inc, per_reg, n);
        value |= (byte_for(n) & 0xff) << shift;
        if (n + 1) % per_reg == 0 || n + 1 == count {
            acc.write32(reg, value);
            value = 0;
        }
    }
}

// ——— Store and forward, and who goes first ———

/// `MTL_Q_TQOMR_TSF`, index **1**, width 1: store the whole frame before
/// starting to transmit it.
///
/// Index 1, not 0. Bit 0 of this register is `FTQ`, the flush of batch six:
/// a store-and-forward bit written from the bottom of the word empties the
/// queue instead.
pub const MTL_Q_TQOMR_TSF: (u32, u32) = (1, 1);
/// `MTL_Q_TQOMR_TTC`, index 4, width **3**: how much of a frame to collect
/// before starting, when store-and-forward is off.
pub const MTL_Q_TQOMR_TTC: (u32, u32) = (4, 3);
/// `MTL_Q_RQOMR_RSF`, index **5**, width 1: the receive side's
/// store-and-forward.
///
/// Not index 1. The two store-and-forward bits carry the same name and sit
/// four bits apart in the two matching registers.
pub const MTL_Q_RQOMR_RSF: (u32, u32) = (5, 1);
/// `MTL_Q_RQOMR_RTC`, index 0, width **2**: the receive threshold. Different
/// index and different width from the transmit one.
pub const MTL_Q_RQOMR_RTC: (u32, u32) = (0, 2);

/// `MTL_OMR`: the MTL's own operating mode, one register for the block.
pub const MTL_OMR: usize = 0x1000;
/// `MTL_OMR_RAA`, index 2, width 1: the receive arbitration algorithm.
pub const MTL_OMR_RAA: (u32, u32) = (2, 1);
/// `MTL_OMR_ETSALG`, index 5, width 2: the transmit scheduling algorithm.
pub const MTL_OMR_ETSALG: (u32, u32) = (5, 2);
/// `MTL_ETSALG_WRR`: weighted round robin. **Zero.**
pub const MTL_ETSALG_WRR: u32 = 0x00;
/// `MTL_RAA_SP`: strict priority. **Also zero.**
///
/// Both scheduler selections are zero, so the whole of
/// [`config_mtl_scheduling`]'s first half writes nothing but zeroes. That
/// reads like dead code and is not: the register is not guaranteed to come
/// up this way, and the traffic-class half below is not zero at all.
pub const MTL_RAA_SP: u32 = 0x00;

/// `MTL_TC_ETSCR`, inside the same per-index block as the queue registers.
pub const MTL_TC_ETSCR: usize = 0x10;
/// `MTL_TC_QWR`, likewise.
pub const MTL_TC_QWR: usize = 0x18;
/// `MTL_TC_ETSCR_TSA`, index 0, width 2.
pub const MTL_TC_ETSCR_TSA: (u32, u32) = (0, 2);
/// `MTL_TC_QWR_QW`, index 0, width 21.
pub const MTL_TC_QWR_QW: (u32, u32) = (0, 21);
/// `MTL_TSA_ETS`: enhanced transmission selection.
pub const MTL_TSA_ETS: u32 = 0x02;

/// A traffic class's register block. `MTL_TC_BASE` **is** `MTL_Q_BASE` and
/// `MTL_TC_INC` **is** `MTL_Q_INC`: traffic class N and queue N share one
/// 0x80 block, with the queue registers at 0x00 and 0x40 and the class
/// registers at 0x10 and 0x18 inside it.
///
/// So there is no separate base to get wrong --- but a class index above the
/// queue count lands squarely in some other queue's block, which is why
/// [`config_tc_weights`] refuses one.
#[inline]
pub const fn mtl_tc_reg(tc: u32, reg: usize) -> usize {
    mtl_queue_reg(tc, reg)
}

/// Whether a queue holds a whole frame before moving it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarding {
    /// Wait for the whole frame. What this driver wants: a frame that
    /// under-runs halfway out of the port is a frame on the wire with a bad
    /// CRC.
    StoreAndForward,
    /// Start once `threshold` units have arrived. The threshold is the raw
    /// `TTC`/`RTC` value, which means different byte counts on the two
    /// sides.
    CutThrough { threshold: u32 },
}

/// Set the transmit queues' forwarding mode and threshold.
///
/// The threshold is written even in store-and-forward, where the hardware
/// ignores it, which is what Linux does. It is not dead: it is what the
/// queue falls back to if store-and-forward is ever turned off, and leaving
/// it at whatever the last configuration set is worse than setting it.
pub fn config_tx_forwarding(acc: &dyn MmioAccess, queues: u32, mode: Forwarding) {
    let (sf, ttc) = match mode {
        Forwarding::StoreAndForward => (1, 0),
        Forwarding::CutThrough { threshold } => (0, threshold),
    };
    for q in 0..queues {
        let reg = mtl_queue_reg(q, MTL_Q_TQOMR);
        write_field(acc, reg, MTL_Q_TQOMR_TSF.0, MTL_Q_TQOMR_TSF.1, sf);
        write_field(acc, reg, MTL_Q_TQOMR_TTC.0, MTL_Q_TQOMR_TTC.1, ttc);
    }
}

/// The same for the receive queues, with the other register's other
/// positions.
pub fn config_rx_forwarding(acc: &dyn MmioAccess, queues: u32, mode: Forwarding) {
    let (sf, rtc) = match mode {
        Forwarding::StoreAndForward => (1, 0),
        Forwarding::CutThrough { threshold } => (0, threshold),
    };
    for q in 0..queues {
        let reg = mtl_queue_reg(q, MTL_Q_RQOMR);
        write_field(acc, reg, MTL_Q_RQOMR_RSF.0, MTL_Q_RQOMR_RSF.1, sf);
        write_field(acc, reg, MTL_Q_RQOMR_RTC.0, MTL_Q_RQOMR_RTC.1, rtc);
    }
}

/// Pick the two scheduling algorithms. The first half of
/// `xgbe_config_mtl_mode`.
pub fn config_mtl_scheduling(acc: &dyn MmioAccess) {
    write_field(
        acc,
        MTL_OMR,
        MTL_OMR_ETSALG.0,
        MTL_OMR_ETSALG.1,
        MTL_ETSALG_WRR,
    );
    write_field(acc, MTL_OMR, MTL_OMR_RAA.0, MTL_OMR_RAA.1, MTL_RAA_SP);
}

/// Give every traffic class the same weight. The second half.
///
/// Equal weights, so the classes share the link evenly, which is the only
/// honest default for a driver that has no idea what the classes are for.
pub fn config_tc_weights(acc: &dyn MmioAccess, tc_count: u32, queues: u32) -> DeviceResult<()> {
    if tc_count == 0 || tc_count > queues {
        // A class above the queue count would write into another queue's
        // block, since the two share one.
        return Err(DeviceError::InvalidParam);
    }
    for tc in 0..tc_count {
        write_field(
            acc,
            mtl_tc_reg(tc, MTL_TC_ETSCR),
            MTL_TC_ETSCR_TSA.0,
            MTL_TC_ETSCR_TSA.1,
            MTL_TSA_ETS,
        );
        write_field(
            acc,
            mtl_tc_reg(tc, MTL_TC_QWR),
            MTL_TC_QWR_QW.0,
            MTL_TC_QWR_QW.1,
            1,
        );
    }
    Ok(())
}

// ——— Deciding which frames to keep ———

/// `MAC_PFR`: the packet filter register.
pub const MAC_PFR: usize = 0x0008;
/// `MAC_PFR_PR`, index 0: promiscuous --- keep everything.
pub const MAC_PFR_PR: (u32, u32) = (0, 1);
/// `MAC_PFR_HUC`, index 1: match unicast addresses through the hash table
/// rather than only the exact-match slots.
pub const MAC_PFR_HUC: (u32, u32) = (1, 1);
/// `MAC_PFR_HMC`, index 2: the same for multicast.
pub const MAC_PFR_HMC: (u32, u32) = (2, 1);
/// `MAC_PFR_PM`, index 4: keep every multicast frame.
pub const MAC_PFR_PM: (u32, u32) = (4, 1);
/// `MAC_PFR_HPF`, index 10: let a frame in on either the hash OR an
/// exact-match slot, instead of the hash alone.
pub const MAC_PFR_HPF: (u32, u32) = (10, 1);

/// `MAC_HTR0`: the first word of the hash filter table.
pub const MAC_HTR0: usize = 0x0010;
/// `MAC_HTR_INC`: and the stride between its words.
pub const MAC_HTR_INC: usize = 4;

/// How far to shift a reversed CRC to get a hash index, for a table of
/// `hash_table_size` bits.
///
/// Linux writes this as `26 - (hash_table_size >> 7)`, which is a log2 in
/// disguise: 64 bits gives 26 (six index bits), 128 gives 25, 256 gives 24.
/// Hard-coding the 24 that a 256-bit table wants works perfectly on the chip
/// it was written against and silently folds three quarters of the table
/// away on a smaller one.
#[inline]
pub const fn hash_table_shift(hash_table_size: u32) -> u32 {
    26 - (hash_table_size >> 7)
}

/// How many 32-bit words the table occupies.
#[inline]
pub const fn hash_table_words(hash_table_size: u32) -> u32 {
    hash_table_size / 32
}

/// The hash index of an address, from the Ethernet CRC of its six bytes.
///
/// `crc_le` is `crc32_le(~0, addr, 6)` --- the plain Ethernet CRC, nothing
/// done to it yet. The three steps here are each the kind that looks
/// optional and is not:
///
/// 1. **Complement it.** `crc32_le` leaves the residue uncomplemented.
/// 2. **Reverse the bits**, all thirty-two of them. The hardware indexes
///    from the other end of the word.
/// 3. **Shift right** by [`hash_table_shift`], keeping the top bits of the
///    reversed value --- which are the BOTTOM bits of the one that came in.
///
/// Skip the reversal and the index is still a number in range, still
/// deterministic, and still points at the wrong bucket, so the symptom is an
/// interface that drops exactly the multicast groups it was asked to join.
///
/// A CRC-32 crate hands back a value with step 1 ALREADY DONE, so code that
/// reaches for one and keeps Linux's `~` complements twice and lands
/// somewhere else entirely. [`crate::net::ether_hash::hash_bucket_of`] takes
/// an address and does the right thing; this takes Linux's intermediate
/// value and exists so the two can be checked against each other.
#[inline]
pub const fn hash_index(crc_le: u32, hash_table_size: u32) -> u32 {
    (!crc_le).reverse_bits() >> hash_table_shift(hash_table_size)
}

/// Which word of the table an index falls in, and which bit of it.
#[inline]
pub const fn hash_bucket(index: u32) -> (u32, u32) {
    (index >> 5, index & 0x1f)
}

/// Set an address's bit in a hash table being built.
///
/// The work is [`crate::net::ether_hash`]'s, because the GENET and the macb
/// compute the same bucket from the same CRC and there is no version of this
/// that is the XGMAC's own. What stays here is [`hash_index`], which says
/// the same thing in the terms Linux's `xgbe-dev.c` uses, and a test that
/// the two agree.
#[inline]
pub fn hash_table_add(table: &mut [u32], addr: &[u8], hash_table_size: u32) {
    crate::net::ether_hash::hash_table_set(table, addr, hash_table_size);
}

/// Write a built hash table out. `xgbe_set_mac_hash_table`'s last loop.
pub fn config_hash_table(acc: &dyn MmioAccess, table: &[u32], hash_table_size: u32) {
    let words = hash_table_words(hash_table_size) as usize;
    for i in 0..words {
        acc.write32(
            MAC_HTR0 + i * MAC_HTR_INC,
            table.get(i).copied().unwrap_or(0),
        );
    }
}

/// An exact-match address slot, one of the `addn_mac` the chip reports.
/// Slot zero is the station address of batch seven and is not one of these.
#[inline]
pub const fn mac_addr_slot(slot: u32) -> (usize, usize) {
    let base = MAC_MACA1HR + (slot as usize) * 8;
    (base, base + 4)
}

/// Put an address in an exact-match slot, or clear the slot with `None`.
///
/// The enable bit lives in the HIGH half, so the high half is written LAST:
/// writing it first arms a slot whose low four bytes are still the previous
/// address's.
pub fn config_mac_addr_slot(acc: &dyn MmioAccess, slot: u32, mac: Option<[u8; 6]>) {
    let (hi_reg, lo_reg) = mac_addr_slot(slot);
    match mac {
        Some(mac) => {
            let (hi, lo) = mac_to_regs(mac);
            acc.write32(lo_reg, lo);
            acc.write32(hi_reg, hi | (1 << MAC_MACA1HR_AE.0));
        }
        None => {
            // Clearing is the other way round: disarm first, then wipe.
            acc.write32(hi_reg, 0);
            acc.write32(lo_reg, 0);
        }
    }
}

/// What the interface should accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxFilter {
    /// Keep every frame, whoever it is for.
    pub promiscuous: bool,
    /// Keep every multicast frame.
    pub all_multicast: bool,
    /// Consult the hash table for multicast, as well as the exact slots.
    pub hash_multicast: bool,
    /// ... and for unicast.
    pub hash_unicast: bool,
}

/// Program the filter.
///
/// Note what promiscuous does NOT do: the hardware goes on filtering VLANs
/// in promiscuous mode, so "accept everything" does not accept everything
/// and Linux turns VLAN filtering off separately. This driver has no VLAN
/// filtering to turn off yet, which is the only reason that is not a bug
/// here --- and the reason to write it down before it is.
///
/// `HPF` is set whenever a hash is in use, so a frame gets in on the hash OR
/// an exact slot. Without it the exact slots stop mattering the moment the
/// hash is on, and the station address itself would be filtered out by a
/// hash it does not happen to land in.
pub fn config_rx_filter(acc: &dyn MmioAccess, filter: &RxFilter) {
    let hash = filter.hash_multicast || filter.hash_unicast;
    let mut pfr = 0u32;
    pfr = set_bits(pfr, MAC_PFR_PR.0, MAC_PFR_PR.1, filter.promiscuous as u32);
    pfr = set_bits(pfr, MAC_PFR_PM.0, MAC_PFR_PM.1, filter.all_multicast as u32);
    pfr = set_bits(
        pfr,
        MAC_PFR_HMC.0,
        MAC_PFR_HMC.1,
        filter.hash_multicast as u32,
    );
    pfr = set_bits(
        pfr,
        MAC_PFR_HUC.0,
        MAC_PFR_HUC.1,
        filter.hash_unicast as u32,
    );
    pfr = set_bits(pfr, MAC_PFR_HPF.0, MAC_PFR_HPF.1, hash as u32);
    acc.write32(MAC_PFR, pfr);
}

// ——— Not interrupting on every frame ———

/// `XGBE_V2_DMA_CLOCK_FREQ`: the DMA block's clock on the PCI parts, 500 MHz.
/// The watchdog below counts THIS clock, not the link's and not a wall
/// clock.
pub const DMA_CLOCK_HZ: u32 = 500_000_000;
/// `XGBE_V2_PTP_CLOCK_FREQ`: 125 MHz. Named only so that nobody reaches for
/// it here: it times timestamps, not the receive watchdog, and the two are
/// a factor of four apart.
pub const PTP_CLOCK_HZ: u32 = 125_000_000;

/// How many clock cycles one unit of the receive watchdog is worth.
pub const RIWT_CYCLES_PER_UNIT: u32 = 256;
/// `XGMAC_MAX_DMA_RIWT`: the widest value the eight-bit field holds.
pub const MAX_DMA_RIWT: u32 = 0xff;
/// `DMA_CH_RIWT_RWT`, index 0, width 8.
pub const DMA_CH_RIWT_RWT: (u32, u32) = (0, 8);

/// Microseconds to watchdog units. `xgbe_usec_to_riwt`.
///
/// Three things about this arithmetic:
///
/// - It counts the DMA clock, so the SAME microsecond figure becomes a
///   different register value on a part with a different clock. A number
///   copied out of another driver's defaults means a different delay here.
/// - Linux divides the rate down to MHz FIRST, so a clock that is not a
///   whole number of megahertz loses its fraction before anything else
///   happens. Kept as it is: matching the kernel matters more here than the
///   last few cycles, and the result is a coalescing delay, not a deadline.
/// - It is clamped. Linux clamps where the value arrives from ethtool
///   instead, so the conversion itself can hand back something that does not
///   fit the eight-bit field; a value that wrapped there would ask for a
///   *shorter* delay than requested, which is the wrong direction to fail.
#[inline]
pub const fn usec_to_riwt(usec: u32, clock_hz: u32) -> u32 {
    let mhz = clock_hz / 1_000_000;
    let riwt = usec.saturating_mul(mhz) / RIWT_CYCLES_PER_UNIT;
    if riwt > MAX_DMA_RIWT {
        MAX_DMA_RIWT
    } else {
        riwt
    }
}

/// And back. `xgbe_riwt_to_usec`.
///
/// Not the inverse of [`usec_to_riwt`]: both truncate, so a round trip
/// lands at or below where it started. Anything that compares a configured
/// delay against a read-back one has to expect that, rather than treating a
/// difference as the hardware having refused the setting.
#[inline]
pub const fn riwt_to_usec(riwt: u32, clock_hz: u32) -> u32 {
    let mhz = clock_hz / 1_000_000;
    if mhz == 0 {
        return 0;
    }
    riwt * RIWT_CYCLES_PER_UNIT / mhz
}

/// Set the receive watchdog on the channels that have a receive ring.
///
/// Linux's loop `break`s at the first channel without one rather than
/// skipping it, which only works because the receive channels come first.
/// Taking a count instead says that out loud.
pub fn config_rx_coalesce(acc: &dyn MmioAccess, rx_channels: u32, riwt: u32) {
    for ch in 0..rx_channels {
        write_field(
            acc,
            dma_channel_reg(ch, DMA_CH_RIWT),
            DMA_CH_RIWT_RWT.0,
            DMA_CH_RIWT_RWT.1,
            riwt.min(MAX_DMA_RIWT),
        );
    }
}

/// The transmit side's coalescing, which is no register at all.
///
/// `xgbe_config_tx_coalesce` in Linux is a function that returns success
/// without writing anything, and the symmetry with the receive side is a
/// trap: there is no transmit watchdog. What limits transmit interrupts is
/// the `IC` bit the driver decides to set on a descriptor, back in
/// [`build_tx_frame`] --- a per-frame choice, not a timer the hardware
/// keeps. This function exists so the pair reads honestly at the call site
/// rather than looking like an omission.
#[inline]
pub fn config_tx_coalesce(_acc: &dyn MmioAccess) {}

// ——— How long a frame may be, and how big a buffer it lands in ———

/// `ETH_HLEN`: the Ethernet header Linux counts into a receive buffer.
pub const ETH_HLEN: u32 = 14;
/// `ETH_FCS_LEN`: the trailing CRC, which the chip delivers.
pub const ETH_FCS_LEN: u32 = 4;
/// `VLAN_HLEN`: room for the one VLAN tag Linux always leaves space for,
/// whether or not the interface is on a VLAN.
pub const VLAN_HLEN: u32 = 4;
/// `ETH_FRAME_LEN`: header plus a standard payload, **without** the FCS.
pub const ETH_FRAME_LEN: u32 = 1514;

/// `XGBE_RX_MIN_BUF_SIZE`, `xgbe.h`: `ETH_FRAME_LEN + ETH_FCS_LEN + VLAN_HLEN`.
pub const RX_MIN_BUF_SIZE: u32 = ETH_FRAME_LEN + ETH_FCS_LEN + VLAN_HLEN;
/// `XGBE_RX_BUF_ALIGN`, `xgbe.h`.
pub const RX_BUF_ALIGN: u32 = 64;
/// Linux clamps the receive buffer at `PAGE_SIZE`, because it allocates one
/// page per buffer. 4 KiB here, which is the page size on every architecture
/// this kernel builds for.
///
/// This clamp is the whole reason a jumbo frame arrives **split**: a 9000-byte
/// MTU does not get a 9022-byte buffer, it gets 4096, and the frame comes back
/// over three descriptors with `FD` on the first and `LD` on the last. See
/// [`read_rx_desc`].
pub const RX_MAX_BUF_SIZE: u32 = 4096;

/// `XGMAC_STD_PACKET_MTU`.
pub const STD_PACKET_MTU: u32 = 1500;
/// `XGMAC_JUMBO_PACKET_MTU`: above this the giant-packet path is used.
pub const JUMBO_PACKET_MTU: u32 = 9000;
/// `XGMAC_MAX_JUMBO_PACKET`: 9000 plus header, FCS and a VLAN tag.
pub const MAX_JUMBO_PACKET: u32 = 9018;
/// `XGMAC_GIANT_PACKET_MTU`, `xgbe.h` at master: the length the giant path
/// programs into `GPSL`. Not 16384: `GPSL` is 14 bits, so 16383 is the
/// ceiling, and Linux stops 15 short of it.
pub const GIANT_PACKET_MTU: u32 = 16368;

/// `MAC_RCR_GPSLCE`, index 6, width 1: check the received length against
/// `GPSL` instead of against the fixed 1518/9018 limit.
pub const MAC_RCR_GPSLCE: (u32, u32) = (6, 1);
/// `MAC_RCR_WD`, index 7, width 1: **watchdog disable** on receive.
///
/// The name reads backwards. The receive watchdog is the timer that cuts a
/// frame off at about 2 KiB; a 1 here turns that policeman off, which is what
/// *allows* a giant frame, not what forbids one.
pub const MAC_RCR_WD: (u32, u32) = (7, 1);
/// `MAC_RCR_JE`, index 8, width 1: jumbo enable, i.e. raise the fixed length
/// limit from 1518 to 9018.
pub const MAC_RCR_JE: (u32, u32) = (8, 1);
/// `MAC_RCR_GPSL`, index 16, width 14: the giant packet size limit.
pub const MAC_RCR_GPSL: (u32, u32) = (16, 14);
/// `MAC_TCR_JD`, index 16, width 1: **jabber disable** on transmit. The
/// transmit-side twin of [`MAC_RCR_WD`], and it reads backwards for the same
/// reason.
pub const MAC_TCR_JD: (u32, u32) = (16, 1);

/// How big a receive buffer an MTU needs. `xgbe_calc_rx_buf_size`.
///
/// Three steps, in Linux's order, and each one changes the answer:
///
/// 1. the frame is the MTU plus header, FCS and a VLAN tag — 22 bytes, not 18,
///    because the tag is counted whether or not it is used;
/// 2. the result is clamped into `[RX_MIN_BUF_SIZE, RX_MAX_BUF_SIZE]`, so a
///    tiny MTU still gets 1522 and a jumbo MTU is cut to a page;
/// 3. and only then is it rounded **up** to [`RX_BUF_ALIGN`].
///
/// Rounding before clamping would be wrong at the top: 4096 is already
/// 64-aligned, so in this order the round-up can never push the result back
/// over the clamp. Doing it the other way round for a hypothetical unaligned
/// ceiling would.
#[inline]
pub const fn calc_rx_buf_size(mtu: u32) -> u32 {
    let wanted = mtu.saturating_add(ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN);
    let clamped = if wanted < RX_MIN_BUF_SIZE {
        RX_MIN_BUF_SIZE
    } else if wanted > RX_MAX_BUF_SIZE {
        RX_MAX_BUF_SIZE
    } else {
        wanted
    };
    (clamped + RX_BUF_ALIGN - 1) & !(RX_BUF_ALIGN - 1)
}

/// How many descriptors one frame of `mtu` can take up, given the buffer size
/// [`calc_rx_buf_size`] picked.
///
/// Not a Linux function: Linux discovers this by walking `FD`..`LD` as the
/// frames come back. It is here so the number is written down once, because
/// it is what decides whether a receive ring of `N` descriptors can hold
/// anything at all at a jumbo MTU.
#[inline]
pub const fn rx_buffers_for_mtu(mtu: u32) -> u32 {
    let buf = calc_rx_buf_size(mtu);
    let frame = mtu.saturating_add(ETH_HLEN + ETH_FCS_LEN + VLAN_HLEN);
    frame.div_ceil(buf)
}

/// Write the receive buffer size into every receive channel.
/// `xgbe_config_rx_buffer_size`.
///
/// [`program_rx_ring`] already sets `RBSZ` when it hands a ring over, and
/// Linux does the same thing twice for the same reason: `xgbe_change_mtu`
/// recomputes the buffer size and restarts the device, and the restart path
/// goes through this function rather than through ring programming.
pub fn config_rx_buffer_size(
    acc: &dyn MmioAccess,
    rx_channels: u32,
    buffer_size: u32,
) -> DeviceResult<()> {
    if buffer_size == 0 || buffer_size >= (1 << DMA_CH_RCR_RBSZ.1) {
        return Err(DeviceError::InvalidParam);
    }
    for channel in 0..rx_channels {
        write_field(
            acc,
            dma_channel_reg(channel, DMA_CH_RCR),
            DMA_CH_RCR_RBSZ.0,
            DMA_CH_RCR_RBSZ.1,
            buffer_size,
        );
    }
    Ok(())
}

/// Set the length limits for an MTU. `xgbe_config_jumbo_enable`.
///
/// Three regimes, and the middle one is the surprise:
///
/// * **MTU above 9000**: the two length policemen are switched off
///   (`WD` and `JD` to 1), `GPSL` is loaded with [`GIANT_PACKET_MTU`] and
///   `GPSLCE` turns the comparison against it on. `JE` is **not written at
///   all** on this path — the fixed 9018 limit is irrelevant once `GPSLCE`
///   is checking against `GPSL` instead.
/// * **MTU between 1501 and 9000**: `GPSLCE`, `WD` and `JD` go back to 0 and
///   `JE` goes to 1, raising the fixed limit to 9018.
/// * **MTU of 1500 or less**: the same four writes with `JE` at 0 as well.
///
/// So the normal path always writes all four fields, and the giant path
/// writes four different ones. Reading it as "set JE for jumbo" and stopping
/// there gets frames above 9018 dropped by a watchdog that was never turned
/// off, with no counter to say so.
pub fn config_jumbo_enable(acc: &dyn MmioAccess, mtu: u32) {
    if mtu > JUMBO_PACKET_MTU {
        write_field(
            acc,
            MAC_RCR,
            MAC_RCR_GPSL.0,
            MAC_RCR_GPSL.1,
            GIANT_PACKET_MTU,
        );
        write_field(acc, MAC_RCR, MAC_RCR_WD.0, MAC_RCR_WD.1, 1);
        write_field(acc, MAC_TCR, MAC_TCR_JD.0, MAC_TCR_JD.1, 1);
        write_field(acc, MAC_RCR, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1, 1);
    } else {
        let jumbo = if mtu > STD_PACKET_MTU { 1 } else { 0 };
        write_field(acc, MAC_RCR, MAC_RCR_GPSLCE.0, MAC_RCR_GPSLCE.1, 0);
        write_field(acc, MAC_RCR, MAC_RCR_WD.0, MAC_RCR_WD.1, 0);
        write_field(acc, MAC_TCR, MAC_TCR_JD.0, MAC_TCR_JD.1, 0);
        write_field(acc, MAC_RCR, MAC_RCR_JE.0, MAC_RCR_JE.1, jumbo);
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

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_datapath;
