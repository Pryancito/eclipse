use super::pmio::{pmio_config_read_addr, pmio_config_write_addr};
use super::PciAddrSpace;
use numeric_enum_macro::numeric_enum;

#[derive(Debug)]
pub struct PciConfig {
    pub addr_space: PciAddrSpace,
    pub base: usize,
}

/// Access to one function's PCI configuration space.
///
/// `base` is where that function's configuration space starts: the ECAM
/// window for [`PciAddrSpace::MMIO`], or the `CONFIG_ADDRESS` value for
/// [`PciAddrSpace::PIO`]. Every public accessor here takes an offset
/// **within** the configuration space and adds `base` itself — the
/// `*_at_addr` helpers that take a whole address are private for that
/// reason. Handing a configuration-space offset to one of those was a
/// read or a write of kernel address `offset`, nowhere near the device.
#[allow(unsafe_code)]
impl PciConfig {
    /// What a read answers when the access cannot be made at all.
    ///
    /// Every accessor below used to `unwrap` the port-I/O result, and there are
    /// two ways in. On any build that is not x86_64 bare metal there **is** no
    /// port-I/O configuration space, so every [`PciAddrSpace::PIO`] access
    /// answers `NOT_SUPPORTED` -- and which address space a window uses comes
    /// out of the `zx_pci_init` arguments, i.e. from userspace. On x86_64, an
    /// access that would straddle `CONFIG_DATA` is refused, and `base` plus the
    /// offset can come from a capability pointer the **device** chose. Either
    /// way the kernel went down in the middle of enumerating the bus.
    ///
    /// All ones is what the bus answers for a function that is not there, and
    /// every reader here already copes with that: a vendor ID of 0xFFFF is how
    /// `PCIeBusDriver` decides a slot is empty.
    fn no_answer(addr: usize, width: usize) -> u32 {
        warn!(
            "PCI config read of {} bits at {:#x} could not be made",
            width, addr
        );
        u32::MAX
    }

    /// Same, for a write: there is nowhere to put the value, so say so and
    /// carry on. See [`PciConfig::no_answer`].
    fn dropped_write(addr: usize, width: usize) {
        warn!(
            "PCI config write of {} bits at {:#x} could not be made",
            width, addr
        );
    }

    fn read8_at_addr(&self, offset: usize) -> u8 {
        trace!("read8 @ {:#x?}", offset);
        match self.addr_space {
            PciAddrSpace::MMIO => unsafe { u8::from_le(*(offset as *const u8)) },
            PciAddrSpace::PIO => pmio_config_read_addr(offset as u32, 8)
                .unwrap_or_else(|_| Self::no_answer(offset, 8))
                as u8,
        }
    }
    fn read16_at_addr(&self, addr: usize) -> u16 {
        trace!("read16 @ {:#x?}", addr);
        match self.addr_space {
            PciAddrSpace::MMIO => unsafe { u16::from_le(*(addr as *const u16)) },
            PciAddrSpace::PIO => pmio_config_read_addr(addr as u32, 16)
                .unwrap_or_else(|_| Self::no_answer(addr, 16))
                as u16,
        }
    }
    fn read32_at_addr(&self, addr: usize) -> u32 {
        trace!("read32 @ {:#x?}", addr);
        match self.addr_space {
            PciAddrSpace::MMIO => unsafe { u32::from_le(*(addr as *const u32)) },
            PciAddrSpace::PIO => {
                pmio_config_read_addr(addr as u32, 32).unwrap_or_else(|_| Self::no_answer(addr, 32))
            }
        }
    }
    pub fn read8(&self, addr: PciReg8) -> u8 {
        self.read8_at_addr(self.base + addr as usize)
    }
    pub fn read8_(&self, addr: usize) -> u8 {
        self.read8_at_addr(self.base + addr)
    }
    pub fn read16(&self, addr: PciReg16) -> u16 {
        self.read16_at_addr(self.base + addr as usize)
    }
    pub fn read16_(&self, addr: usize) -> u16 {
        self.read16_at_addr(self.base + addr)
    }
    pub fn read32(&self, addr: PciReg32) -> u32 {
        self.read32_at_addr(self.base + addr as usize)
    }
    pub fn read32_(&self, addr: usize) -> u32 {
        self.read32_at_addr(self.base + addr)
    }
    pub fn read_bar(&self, bar_: usize) -> u32 {
        self.read32_at_addr(self.base + PciReg32::BARBase as usize + bar_ * 4)
    }

    fn write8_at_addr(&self, addr: usize, val: u8) {
        match self.addr_space {
            PciAddrSpace::MMIO => unsafe { *(addr as *mut u8) = val },
            PciAddrSpace::PIO => pmio_config_write_addr(addr as u32, val as u32, 8)
                .unwrap_or_else(|_| Self::dropped_write(addr, 8)),
        }
    }
    fn write16_at_addr(&self, addr: usize, val: u16) {
        trace!(
            "write16 @ {:#x?}, addr_space = {:#x?}",
            addr,
            self.addr_space
        );
        match self.addr_space {
            PciAddrSpace::MMIO => unsafe { *(addr as *mut u16) = val },
            PciAddrSpace::PIO => pmio_config_write_addr(addr as u32, val as u32, 16)
                .unwrap_or_else(|_| Self::dropped_write(addr, 16)),
        }
    }
    fn write32_at_addr(&self, addr: usize, val: u32) {
        match self.addr_space {
            PciAddrSpace::MMIO => unsafe { *(addr as *mut u32) = val },
            PciAddrSpace::PIO => pmio_config_write_addr(addr as u32, val, 32)
                .unwrap_or_else(|_| Self::dropped_write(addr, 32)),
        }
    }
    pub fn write8(&self, addr: PciReg8, val: u8) {
        self.write8_at_addr(self.base + addr as usize, val)
    }
    pub fn write8_(&self, addr: usize, val: u8) {
        self.write8_at_addr(self.base + addr, val)
    }
    pub fn write16(&self, addr: PciReg16, val: u16) {
        self.write16_at_addr(self.base + addr as usize, val)
    }
    pub fn write16_(&self, addr: usize, val: u16) {
        self.write16_at_addr(self.base + addr, val)
    }
    pub fn write32(&self, addr: PciReg32, val: u32) {
        self.write32_at_addr(self.base + addr as usize, val)
    }
    pub fn write32_(&self, addr: usize, val: u32) {
        self.write32_at_addr(self.base + addr, val)
    }
    pub fn write_bar(&self, bar_: usize, val: u32) {
        self.write32_at_addr(self.base + PciReg32::BARBase as usize + bar_ * 4, val)
    }
}

numeric_enum! {
    #[repr(usize)]
    pub enum PciReg8 {
        // standard
        RevisionId = 0x8,
        ProgramInterface = 0x9,
        SubClass = 0xA,
        BaseClass = 0xB,
        CacheLineSize = 0xC,
        LatencyTimer = 0xD,
        HeaderType = 0xE,
        Bist = 0xF,

        // bridge
        PrimaryBusId = 0x18,
        SecondaryBusId = 0x19,
        SubordinateBusId = 0x1A,
        SecondaryLatencyTimer = 0x1B,
        IoBase = 0x1C,
        IoLimit = 0x1D,
        CapabilitiesPtr = 0x34,
        InterruptLine = 0x3C,
        InterruptPin = 0x3D,
        MinGrant = 0x3E,
        MaxLatency = 0x3F,
    }
}
numeric_enum! {
    #[repr(usize)]
    pub enum PciReg16 {
        // standard
        VendorId = 0x0,
        DeviceId = 0x2,
        Command = 0x4,
        Status = 0x6,

        // bridge
        SecondaryStatus = 0x1E,
        MemoryBase = 0x20,
        MemoryLimit = 0x22,
        PrefetchableMemoryBase = 0x24,
        PrefetchableMemoryLimit = 0x26,
        IoBaseUpper = 0x30,
        IoLimitUpper = 0x32,
        BridgeControl = 0x3E,
    }
}
numeric_enum! {
    #[repr(usize)]
    pub enum PciReg32 {
        // standard
        BARBase = 0x10,

        // bridge
        PrefetchableMemoryBaseUpper = 0x28,
        PrefetchableMemoryLimitUpper = 0x2C,
        BridgeExpansionRomAddress = 0x38,
    }
}

pub const PCIE_BASE_CONFIG_SIZE: usize = 256;
pub const PCIE_EXTENDED_CONFIG_SIZE: usize = 4096;

#[cfg(test)]
mod config_tests {
    use super::super::harness::ConfigSpace;
    use super::*;

    #[test]
    fn every_width_reads_back_what_the_device_put_there() {
        let mut space = ConfigSpace::new();
        space.poke32(0x00, 0x1F06_10DE); // RTX 2060 SUPER: vendor 10DE, device 1F06
        space.poke32(0x08, 0x0300_00A1);
        let cfg = space.config();
        assert_eq!(cfg.read16(PciReg16::VendorId), 0x10DE);
        assert_eq!(cfg.read16(PciReg16::DeviceId), 0x1F06);
        assert_eq!(cfg.read8(PciReg8::RevisionId), 0xA1);
        assert_eq!(cfg.read8(PciReg8::BaseClass), 0x03);
        assert_eq!(cfg.read32_(0x0), 0x1F06_10DE);
        assert_eq!(cfg.read16_(0x2), 0x1F06);
        assert_eq!(cfg.read8_(0x1), 0x10);
    }

    #[test]
    fn an_offset_is_inside_the_configuration_space_and_not_an_address() {
        // The `*_at_addr` helpers take a whole address and the public accessors
        // take an offset. Handing an offset to one of the private ones was a
        // read or a write of kernel address `offset`, nowhere near the device.
        let mut space = ConfigSpace::new();
        {
            let cfg = space.config();
            cfg.write32_(0x40, 0xDEAD_BEEF);
            cfg.write16_(0x50, 0x1234);
            cfg.write8_(0x60, 0x5A);
            cfg.write16(PciReg16::Command, 0x0007);
        }
        assert_eq!(space.peek32(0x40), 0xDEAD_BEEF);
        assert_eq!(space.peek16(0x50), 0x1234);
        assert_eq!(space.peek8(0x60), 0x5A);
        assert_eq!(space.peek16(0x4), 0x0007);
    }

    #[test]
    fn each_bar_is_read_and_written_at_its_own_register() {
        let mut space = ConfigSpace::new();
        for bar in 0..6 {
            space.poke32(0x10 + bar * 4, 0x1000_0000 | bar as u32);
        }
        {
            let cfg = space.config();
            for bar in 0..6 {
                assert_eq!(cfg.read_bar(bar), 0x1000_0000 | bar as u32, "BAR {}", bar);
            }
            cfg.write_bar(3, 0xF000_0000);
        }
        assert_eq!(space.peek32(0x10 + 3 * 4), 0xF000_0000);
        // And nothing else moved.
        assert_eq!(space.peek32(0x10 + 2 * 4), 0x1000_0002);
        assert_eq!(space.peek32(0x10 + 4 * 4), 0x1000_0004);
    }

    #[test]
    fn a_port_io_space_that_cannot_be_reached_answers_like_an_empty_slot() {
        // Six `unwrap`s used to live here. On any build that is not x86_64 bare
        // metal there is no port-I/O configuration space at all, and which
        // address space a window uses comes out of the `zx_pci_init` arguments,
        // so userspace could pick the one that takes the kernel down. (This
        // test always takes that branch: a test binary is never
        // `target_os = "none"`.)
        let cfg = PciConfig {
            addr_space: PciAddrSpace::PIO,
            base: 0,
        };
        assert_eq!(cfg.read16(PciReg16::VendorId), 0xFFFF);
        assert_eq!(cfg.read8(PciReg8::HeaderType), 0xFF);
        assert_eq!(cfg.read32_(0x40), u32::MAX);
        assert_eq!(cfg.read_bar(0), u32::MAX);
        // And a write that cannot be made is dropped, not fatal.
        cfg.write8_(0x40, 0x11);
        cfg.write16_(0x40, 0x2222);
        cfg.write32_(0x40, 0x3333_3333);
        cfg.write_bar(0, 0);
    }

    #[test]
    fn the_registers_are_named_at_the_offsets_the_specification_gives_them() {
        // Every one of these is an offset a driver will never see written down
        // anywhere else, and a wrong one is a read of the neighbouring field.
        assert_eq!(PciReg16::VendorId as usize, 0x0);
        assert_eq!(PciReg16::DeviceId as usize, 0x2);
        assert_eq!(PciReg16::Command as usize, 0x4);
        assert_eq!(PciReg16::Status as usize, 0x6);
        assert_eq!(PciReg8::RevisionId as usize, 0x8);
        assert_eq!(PciReg8::BaseClass as usize, 0xB);
        assert_eq!(PciReg8::HeaderType as usize, 0xE);
        assert_eq!(PciReg32::BARBase as usize, 0x10);
        assert_eq!(PciReg8::PrimaryBusId as usize, 0x18);
        assert_eq!(PciReg8::SecondaryBusId as usize, 0x19);
        assert_eq!(PciReg8::SubordinateBusId as usize, 0x1A);
        assert_eq!(PciReg8::CapabilitiesPtr as usize, 0x34);
        assert_eq!(PciReg8::InterruptLine as usize, 0x3C);
        assert_eq!(PciReg8::InterruptPin as usize, 0x3D);
        assert_eq!(PCIE_BASE_CONFIG_SIZE, 256);
        assert_eq!(PCIE_EXTENDED_CONFIG_SIZE, 4096);
    }

    #[test]
    fn a_read_of_the_extended_space_reaches_past_the_first_header() {
        // A function advertising PCI Express has 4 KiB, and the extended
        // capabilities start at 0x100.
        let mut space = ConfigSpace::new();
        space.poke32(0x100, 0x0001_0001);
        space.poke32(PCIE_EXTENDED_CONFIG_SIZE - 4, 0xABCD_1234);
        let cfg = space.config();
        assert_eq!(cfg.read32_(0x100), 0x0001_0001);
        assert_eq!(cfg.read32_(PCIE_EXTENDED_CONFIG_SIZE - 4), 0xABCD_1234);
    }
}
