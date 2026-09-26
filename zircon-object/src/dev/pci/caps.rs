use super::{
    config::{PciConfig, PCIE_BASE_CONFIG_SIZE},
    nodes::PcieDeviceType,
};
use crate::{ZxError, ZxResult};

use alloc::boxed::Box;
use core::convert::TryFrom;
use kernel_hal::interrupt;
use kernel_hal::sync::Mutex;

/// Enumeration for PCI capabilities.
#[derive(Debug)]
pub enum PciCapability {
    /// MSI Interrupts.
    Msi(PciCapabilityStd, PciCapabilityMsi),
    /// PCI Express Capability.
    Pcie(PciCapabilityStd, PciCapPcie),
    AdvFeatures(PciCapabilityStd, PciCapAdvFeatures),
    Std(PciCapabilityStd),
}

#[derive(Debug)]
pub struct PciCapabilityStd {
    pub id: u8,
    pub base: u16,
}

impl PciCapabilityStd {
    pub fn create(base: u16, id: u8) -> PciCapabilityStd {
        PciCapabilityStd { id, base }
    }
    /// Whether this capability's header sits where a capability can sit.
    ///
    /// It used to be `true`, unconditionally, and `PcieDeviceInner::msi` and
    /// `pcie` use it as the filter that decides whether a device has a usable
    /// capability of that kind -- so it filtered nothing. A pointer below 0x40
    /// is inside the standard header, which the capability list may not overlap
    /// (PCI Local Bus 3.0 section 6.7), and one above 0xFC has no room for even
    /// the two bytes every capability starts with. `init_capabilities` checks
    /// the same thing about the pointer it reads; this is about the capability
    /// that was built, wherever it was built from.
    pub fn is_valid(&self) -> bool {
        (0x40..=0xFC).contains(&self.base)
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct PciMsiBlock {
    pub target_addr: u64,
    pub allocated: bool,
    pub base_irq: usize,
    pub num_irq: usize,
    pub target_data: u32,
}

impl PciMsiBlock {
    pub fn allocate(irq_num: usize) -> ZxResult<Self> {
        if irq_num == 0 || irq_num > 32 {
            return Err(ZxError::INVALID_ARGS);
        }
        let range = interrupt::msi_alloc_block(irq_num).map_err(|_| ZxError::NO_RESOURCES)?;
        Ok(PciMsiBlock {
            target_addr: (0xFEE0_0000 | 0x08) & !0x4,
            target_data: range.start as u32,
            base_irq: range.start,
            num_irq: range.len(),
            allocated: true,
        })
    }
    pub fn free(&self) {
        interrupt::msi_free_block(self.base_irq..self.base_irq + self.num_irq).ok();
    }
    /// Point one vector of the block at `handle`.
    ///
    /// The `assert!` and the `unwrap` this used to be are both reachable from
    /// `zx_pci_set_irq_mode`: the block is a `Mutex<PciMsiBlock>` that
    /// `leave_msi_irq_mode` empties, and the interrupt layer refuses a vector
    /// outside the block. Neither is a reason to take the machine down.
    pub fn register_handler(&self, msi_id: usize, handle: Box<dyn Fn() + Send + Sync>) -> ZxResult {
        if !self.allocated {
            return Err(ZxError::BAD_STATE);
        }
        interrupt::msi_register_handler(
            self.base_irq..self.base_irq + self.num_irq,
            msi_id,
            handle.into(),
        )
        .map_err(|_| ZxError::INVALID_ARGS)
    }
}

// @see PCI Local Bus Specification 3.0 Section 6.8.1
#[derive(Debug)]
pub struct PciCapabilityMsi {
    pub msi_size: u16,
    pub has_pvm: bool,
    pub is_64bit: bool,
    pub max_irq: u32,
    pub irq_block: Mutex<PciMsiBlock>,
    pub addr_upper_offset: usize,   // reg32
    pub data_offset: usize,         // reg16
    pub mask_bits_offset: usize,    // reg32
    pub pending_bits_offset: usize, // reg32
}

impl PciCapabilityMsi {
    pub fn create(cfg: &PciConfig, base: usize, id: u8) -> PciCapabilityMsi {
        assert_eq!(id, 0x5); // PCIE_CAP_ID_MSI
        let ctrl = cfg.read16_(base + 0x2);
        let has_pvm = (ctrl & 0x100) != 0;
        let is_64bit = (ctrl & 0x80) != 0;
        cfg.write16_(base + 0x2, ctrl & !0x71);
        let mask_bits = base + if is_64bit { 0x10 } else { 0xC };
        if has_pvm {
            // `mask_bits` is an offset inside the capability, like the two
            // accesses just above it; it went to the whole-address accessor.
            //
            // And it has to fit: `init_capabilities` bounds where a capability
            // *starts*, not how far it reaches. A 64-bit MSI capability with
            // per-vector masking is 20 bytes, so one starting at 0xFC has its
            // mask register at 0x10C -- outside the 256-byte header these
            // offsets are relative to. Over port I/O that is worse than reading
            // rubbish: `CONFIG_ADDRESS` carries the offset in its low bits, so
            // bit 8 of 0x10C lands in the **function number** and the write
            // goes to a different function's configuration space.
            if mask_bits + 4 <= PCIE_BASE_CONFIG_SIZE {
                cfg.write32_(mask_bits, 0xffff_ffff);
            } else {
                warn!(
                    "MSI capability at {:#x} keeps its mask register at {:#x}, outside the configuration header: not masking",
                    base, mask_bits
                );
            }
        }
        PciCapabilityMsi {
            msi_size: match (has_pvm, is_64bit) {
                (true, true) => 20,
                (true, false) => 16,
                (false, true) => 14,
                (false, false) => 10,
            },
            has_pvm,
            is_64bit,
            // Multiple Message Capable is three bits, and PCI Local Bus 3.0
            // section 6.8.1.3 defines six encodings -- 0 to 5, for 1 to 32
            // vectors. 6 and 7 are reserved, and shifting by the raw field
            // claimed **64 or 128** vectors for a device that used a reserved
            // encoding, or for a configuration window reading all ones. That
            // number leaves the kernel: `zx_pci_get_nth_device` reports it as
            // `max_irqs` for userspace to size an MSI request with, and
            // `msi_multi_message_encoding` measures the request against it.
            max_irq: 0x1 << ((ctrl >> 1) & 0x7).min(5),
            irq_block: Mutex::new(PciMsiBlock::default()),
            addr_upper_offset: if is_64bit {
                base + 0x8
            } else {
                0 /*shouldn't use it*/
            },
            data_offset: base + if is_64bit { 0xC } else { 0x8 },
            mask_bits_offset: base + if is_64bit { 0x10 } else { 0xC },
            pending_bits_offset: base + if is_64bit { 0x14 } else { 0x10 },
        }
    }
    pub fn ctrl_offset() -> usize {
        0x2
    }
    // `mask_bits_offset(bool)` and `addr_offset(bool)` used to live here,
    // returning the same two numbers the `mask_bits_offset` and `data_offset`
    // fields already carry. Nothing called the first one. The second was worse
    // than redundant: it was **misnamed** -- 0xC/0x8 is where the Message Data
    // register is, the Message Address one is at 0x4 whatever the width -- and
    // its one caller had spelled the variable `data_reg`, so it worked. Both
    // are gone and the field is the only answer.
}

/// PCI Express Capability.
#[derive(Debug, Clone, Copy)]
pub struct PciCapPcie {
    pub version: u8,
    pub dev_type: PcieDeviceType,
    pub has_flr: bool,
}

impl PciCapPcie {
    pub fn create(cfg: &PciConfig, base: u16, id: u8) -> PciCapPcie {
        assert_eq!(id, 0x10); // PCIE_CAP_ID_PCI_EXPRESS
        let caps = cfg.read8_(base as usize + 0x2);
        let device_caps = cfg.read32_(base as usize + 0x4);
        PciCapPcie {
            version: caps & 0xF,
            // Four bits hold sixteen values and PCI Express 4.0 table 7-14
            // defines ten: 0x2, 0x3 and 0xB upwards are reserved, and an
            // unmapped configuration window answers all ones, i.e. 0xF. This
            // `unwrap` turned every one of them into a dead machine in the
            // middle of enumerating the bus -- while `PcieDeviceType::Unknown`
            // exists for exactly this case and `PcieDevice::swizzle` already
            // has a branch for it.
            dev_type: PcieDeviceType::try_from((caps >> 4) & 0xF)
                .unwrap_or(PcieDeviceType::Unknown),
            has_flr: ((device_caps >> 28) & 0x1) != 0,
        }
    }
}

#[derive(Debug)]
pub struct PciCapAdvFeatures {
    pub has_flr: bool,
    pub has_tp: bool,
}

impl PciCapAdvFeatures {
    pub fn create(cfg: &PciConfig, base: u16, id: u8) -> PciCapAdvFeatures {
        assert_eq!(id, 0x13); // PCIE_CAP_ID_ADVANCED_FEATURES
        let caps = cfg.read8_(base as usize + 0x3);
        PciCapAdvFeatures {
            has_flr: ((caps >> 1) & 0x1) != 0,
            has_tp: (caps & 0x1) != 0,
        }
    }
}

#[cfg(test)]
mod caps_tests {
    use super::super::harness::ConfigSpace;
    use super::*;

    const MSI_ID: u8 = 0x5;
    const PCIE_ID: u8 = 0x10;
    const ADV_ID: u8 = 0x13;

    /// Seed a PCI Express capability at `base` whose capability register is
    /// `caps` and whose device-capability register is `dev_caps`.
    fn pcie_cap(base: u16, caps: u16, dev_caps: u32) -> PciCapPcie {
        let mut space = ConfigSpace::new();
        space.poke16(base as usize, u16::from(PCIE_ID));
        space.poke16(base as usize + 0x2, caps);
        space.poke32(base as usize + 0x4, dev_caps);
        let cfg = space.config();
        PciCapPcie::create(&cfg, base, PCIE_ID)
    }

    #[test]
    fn every_port_type_the_specification_defines_is_recognised() {
        use PcieDeviceType::*;
        for (field, want) in [
            (0x0, PcieEndpoint),
            (0x1, LegacyPcieEndpoint),
            (0x4, RcRootPort),
            (0x5, SwitchUpstreamPort),
            (0x6, SwitchDownstreamPort),
            (0x7, PcieToPciBridge),
            (0x8, PciToPcieBridge),
            (0x9, RcIntegratedEndpoint),
            (0xA, RcEventCollector),
        ] {
            let cap = pcie_cap(0x40, (field << 4) | 0x2, 0);
            assert_eq!(cap.dev_type, want, "port type {:#x}", field);
            assert_eq!(cap.version, 2);
        }
    }

    #[test]
    fn a_reserved_port_type_is_unknown_instead_of_a_dead_kernel() {
        // 0x2, 0x3 and 0xB upwards are reserved in PCI Express 4.0 table 7-14,
        // and `try_from` says `Err` for every one of them. This used to be an
        // `unwrap` in the middle of enumerating the bus.
        for field in [0x2u16, 0x3, 0xB, 0xC, 0xD, 0xE, 0xF] {
            let cap = pcie_cap(0x40, (field << 4) | 0x1, 0);
            assert_eq!(
                cap.dev_type,
                PcieDeviceType::Unknown,
                "reserved port type {:#x}",
                field
            );
        }
    }

    #[test]
    fn a_configuration_window_that_reads_all_ones_is_an_unknown_port() {
        // The shape a window that is not mapped answers with, which is how the
        // IOAPIC nearly killed the boot as well.
        let cap = pcie_cap(0x40, 0xFFFF, 0xFFFF_FFFF);
        assert_eq!(cap.dev_type, PcieDeviceType::Unknown);
        assert_eq!(cap.version, 0xF);
        assert!(cap.has_flr);
    }

    #[test]
    fn function_level_reset_is_read_from_bit_twenty_eight() {
        assert!(!pcie_cap(0x40, 0x2, 0).has_flr);
        assert!(pcie_cap(0x40, 0x2, 1 << 28).has_flr);
        assert!(!pcie_cap(0x40, 0x2, 1 << 27).has_flr);
    }

    /// Seed an MSI capability at `base` with control register `ctrl`.
    fn msi_at(base: usize, ctrl: u16) -> (alloc::boxed::Box<ConfigSpace>, PciCapabilityMsi) {
        let mut space = ConfigSpace::new();
        space.poke8(base, MSI_ID);
        space.poke16(base + 0x2, ctrl);
        let cap = {
            let cfg = space.config();
            PciCapabilityMsi::create(&cfg, base, MSI_ID)
        };
        (space, cap)
    }

    #[test]
    fn the_number_of_vectors_a_device_can_ask_for_stops_where_the_specification_does() {
        // Multiple Message Capable is three bits with six defined encodings.
        // 6 and 7 are reserved, and the raw shift claimed 64 and 128 vectors --
        // a number that leaves the kernel as `max_irqs`.
        for (field, want) in [(0u16, 1u32), (1, 2), (2, 4), (3, 8), (4, 16), (5, 32)] {
            let (_space, cap) = msi_at(0x50, field << 1);
            assert_eq!(cap.max_irq, want, "encoding {}", field);
        }
        for field in [6u16, 7] {
            let (_space, cap) = msi_at(0x50, field << 1);
            assert_eq!(cap.max_irq, 32, "reserved encoding {}", field);
        }
    }

    #[test]
    fn finding_an_msi_capability_disables_it_and_leaves_its_vectors_masked() {
        // 0x01B5: MSI enabled (bit 0), four vectors supported (bits 3:1),
        // **eight enabled** (bits 6:4), 64-bit (bit 7), per-vector masking
        // (bit 8). What has to come back is MSI off *and* Multiple Message
        // Enable back to zero, with the rest of the register untouched --
        // enabling one vector later while the device still thinks eight are
        // enabled is eight vectors of which seven have no handler.
        let (space, cap) = msi_at(0x50, 0x01B5);
        assert_eq!(space.peek16(0x52), 0x0184);
        assert_eq!(cap.max_irq, 4);
        assert!(cap.has_pvm);
        assert!(cap.is_64bit);
        assert_eq!(space.peek32(cap.mask_bits_offset), u32::MAX);
    }

    #[test]
    fn a_capability_without_per_vector_masking_has_no_mask_register_to_write() {
        // 0x0084: 64-bit, no per-vector masking. 0x60 is where the mask
        // register would be if it existed, and writing there would land on
        // whatever comes after this capability.
        let (space, cap) = msi_at(0x50, 0x0084);
        assert!(!cap.has_pvm);
        assert_eq!(space.peek32(0x60), 0);
        assert_eq!(cap.msi_size, 14);
    }

    #[test]
    fn the_mask_register_of_a_capability_that_runs_past_the_header_is_left_alone() {
        // A 64-bit capability with per-vector masking starting at 0xFC keeps
        // its mask register at 0x10C, outside the 256-byte header these offsets
        // are relative to. Over port I/O bit 8 of that offset lands in the
        // function number, so the write went to a different function.
        let (space, cap) = msi_at(0xFC, 0x0184);
        assert!(cap.has_pvm);
        assert_eq!(cap.mask_bits_offset, 0x10C);
        assert_eq!(space.peek32(0x10C), 0);
        // The control register write is inside the header and still happened.
        assert_eq!(space.peek16(0xFE), 0x0184);
    }

    #[test]
    fn a_mask_register_that_ends_exactly_at_the_end_of_the_header_is_still_written() {
        // The last capability that fits: base 0xEC puts the mask register at
        // 0xFC, whose four bytes end exactly at 0x100. Four bytes further along
        // and there is no room, so the boundary is where a fence-post mistake
        // either loses the masking or writes past the header.
        let (space, cap) = msi_at(0xEC, 0x0184);
        assert_eq!(cap.mask_bits_offset, 0xFC);
        assert_eq!(space.peek32(0xFC), u32::MAX);

        let (space, cap) = msi_at(0xF0, 0x0184);
        assert_eq!(cap.mask_bits_offset, 0x100);
        assert_eq!(space.peek32(0x100), 0);
    }

    #[test]
    fn the_registers_of_a_64_bit_capability_with_masking_are_where_the_specification_puts_them() {
        let (_space, cap) = msi_at(0x50, 0x0184);
        assert_eq!(cap.addr_upper_offset, 0x58);
        assert_eq!(cap.data_offset, 0x5C);
        assert_eq!(cap.mask_bits_offset, 0x60);
        assert_eq!(cap.pending_bits_offset, 0x64);
        assert_eq!(cap.msi_size, 20);
    }

    #[test]
    fn a_32_bit_capability_has_no_upper_address_register_and_packs_the_rest_down() {
        let (_space, cap) = msi_at(0x50, 0x0104);
        assert!(!cap.is_64bit);
        assert_eq!(cap.addr_upper_offset, 0);
        assert_eq!(cap.data_offset, 0x58);
        assert_eq!(cap.mask_bits_offset, 0x5C);
        assert_eq!(cap.pending_bits_offset, 0x60);
        assert_eq!(cap.msi_size, 16);
    }

    #[test]
    fn a_capability_pointer_the_list_could_not_have_given_is_not_valid() {
        // `is_valid` used to be `true`, and it is the filter
        // `PcieDeviceInner::msi` and `pcie` use to decide whether a device has
        // a usable capability of that kind.
        for base in [0u16, 0x1, 0x34, 0x3F] {
            assert!(
                !PciCapabilityStd::create(base, MSI_ID).is_valid(),
                "{:#x} is inside the standard header",
                base
            );
        }
        for base in [0xFD_u16, 0xFE, 0xFF, 0x100, 0xFFFF] {
            assert!(
                !PciCapabilityStd::create(base, MSI_ID).is_valid(),
                "{:#x} has no room for a capability header",
                base
            );
        }
        for base in [0x40_u16, 0x50, 0xFC] {
            assert!(
                PciCapabilityStd::create(base, MSI_ID).is_valid(),
                "{:#x}",
                base
            );
        }
    }

    #[test]
    fn a_block_that_was_never_allocated_refuses_a_handler_instead_of_panicking() {
        // Reachable from `zx_pci_set_irq_mode`: `leave_msi_irq_mode` empties
        // the block, and this used to be an `assert!`.
        let block = PciMsiBlock::default();
        assert!(!block.allocated);
        assert_eq!(
            block.register_handler(0, Box::new(|| {})),
            Err(ZxError::BAD_STATE)
        );
    }

    #[test]
    fn a_block_of_no_vectors_or_more_than_the_specification_allows_is_refused() {
        // `PciMsiBlock` is not `PartialEq`, so the error is what gets compared.
        for irq_num in [0, 33, 64, usize::MAX] {
            assert_eq!(
                PciMsiBlock::allocate(irq_num).err(),
                Some(ZxError::INVALID_ARGS),
                "{} vectors",
                irq_num
            );
        }
    }

    #[test]
    fn the_advanced_features_capability_reads_its_two_flags() {
        for (byte, flr, tp) in [
            (0u8, false, false),
            (1, false, true),
            (2, true, false),
            (3, true, true),
        ] {
            let mut space = ConfigSpace::new();
            space.poke8(0x40, ADV_ID);
            space.poke8(0x43, byte);
            let cfg = space.config();
            let cap = PciCapAdvFeatures::create(&cfg, 0x40, ADV_ID);
            assert_eq!((cap.has_flr, cap.has_tp), (flr, tp), "byte {:#x}", byte);
        }
    }
}
