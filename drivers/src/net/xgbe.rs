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
}
