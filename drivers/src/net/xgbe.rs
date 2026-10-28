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
}
