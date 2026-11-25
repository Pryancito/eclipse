//! Where a device's BARs end up, and the command-register switches a
//! driver turns: what `zx_pci_enable_bus_master` and the bus driver's BAR
//! pass do to the chip, read back from a configuration space in memory.
//!
//! The BAR pass is the one place this kernel decides where a device's
//! registers live: a window firmware chose and the bridge above covers is
//! kept where it is, anything else is moved into free space of the right
//! kind and the BAR register is rewritten to say so. A wrong answer there
//! is two devices decoding the same addresses.

use super::super::bus::PCIeBusDriver;
use super::super::harness::ConfigSpace;
use super::*;

/// A function with nothing but a configuration space behind it, plugged
/// in.
fn a_device(cfg: Arc<PciConfig>) -> Arc<PcieDevice> {
    let dev = Arc::new(PcieDevice {
        bus_id: 0,
        dev_id: 2,
        func_id: 0,
        bar_count: 6,
        cfg: Some(cfg),
        _cfg_phys: 0,
        dev_lock: Mutex::default(),
        command_lock: Mutex::default(),
        vendor_id: 0x8086,
        device_id: 0x10d3,
        class_id: 0x02,
        subclass_id: 0x00,
        prog_if: 0x00,
        rev_id: 0x00,
        inner: Default::default(),
    });
    dev.inner.lock().plugged_in = true;
    dev
}

/// A root with `lo` (base, size) of 32-bit MMIO, `hi` of 64-bit MMIO and
/// `pio` of ports free for its devices, and `dev` below it.
fn under_a_root(
    bus: &PCIeBusDriver,
    dev: &Arc<PcieDevice>,
    lo: Option<(usize, usize)>,
    hi: Option<(usize, usize)>,
    pio: Option<(usize, usize)>,
) -> Arc<PciRoot> {
    for (regions, free) in [
        (&bus.mmio_lo, lo),
        (&bus.mmio_hi, hi),
        (&bus.pio_region, pio),
    ] {
        if let Some((base, size)) = free {
            regions.lock().add(base, size);
        }
    }
    let root = PciRoot::new(0, PciIrqSwizzleLut::zeroed(), bus);
    let up: Arc<dyn IPciNode> = root.clone();
    dev.set_upstream(Arc::downgrade(&up));
    root
}

/// BAR `index` as the probe left it: its size, where firmware put it, and
/// the type bits the register keeps.
fn bar(dev: &PcieDevice, index: usize, mmio: bool, wide: bool, size: u64, at: u64) {
    dev.inner.lock().bars[index] = PcieBarInfo {
        is_mmio: mmio,
        is_64bit: wide,
        is_prefetchable: false,
        first_bar_reg: index,
        size,
        bus_addr: at,
        allocation: None,
    };
}

const BAR0: usize = 0x10;

#[test]
fn a_window_firmware_placed_inside_the_bridges_space_stays_where_it_is() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    space.poke32(BAR0, 0xe000_0000);
    space.poke16(PciReg16::Command as usize, PCI_COMMAND_MEM_EN);
    let dev = a_device(space.config());
    let _root = under_a_root(&bus, &dev, Some((0xe000_0000, 0x1000_0000)), None, None);
    bar(&dev, 0, true, false, 0x2_0000, 0xe000_0000);
    assert_eq!(dev.allocate_bars(), Ok(()));
    assert_eq!(space.peek32(BAR0), 0xe000_0000, "the BAR was rewritten");
    assert_eq!(
        space.peek16(PciReg16::Command as usize),
        PCI_COMMAND_MEM_EN,
        "a device left where it was keeps decoding"
    );
    assert_eq!(dev.get_bar(0).unwrap().bus_addr, 0xe000_0000);
    // And the window is taken: nobody else is given it.
    assert!(!bus.mmio_lo.lock().allocate_by_addr(0xe000_0000, 0x2_0000));
}

#[test]
fn a_window_firmware_never_placed_is_given_aligned_space_and_the_bar_says_where() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    // A 32-bit, prefetchable-off memory BAR with no address: type bits 0.
    space.poke32(BAR0, 0);
    space.poke16(
        PciReg16::Command as usize,
        PCI_COMMAND_MEM_EN | PCI_COMMAND_IO_EN,
    );
    let dev = a_device(space.config());
    let _root = under_a_root(&bus, &dev, Some((0xe000_1000, 0x10_0000)), None, None);
    bar(&dev, 0, true, false, 0x1_0000, 0);
    assert_eq!(dev.allocate_bars(), Ok(()));
    let at = dev.get_bar(0).unwrap().bus_addr;
    assert_ne!(at, 0);
    assert_eq!(at % 0x1_0000, 0, "a BAR is aligned to its size");
    assert!((0xe000_1000..0xe010_1000).contains(&at));
    assert_eq!(space.peek32(BAR0) as u64, at, "the chip was not told");
    // Decoding is off while its registers move, until a driver turns it on.
    assert_eq!(
        space.peek16(PciReg16::Command as usize),
        PCIE_CFG_COMMAND_INT_DISABLE
    );
}

#[test]
fn a_window_outside_the_bridges_space_is_moved_into_it() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    space.poke32(BAR0, 0xd000_0000);
    let dev = a_device(space.config());
    let _root = under_a_root(&bus, &dev, Some((0xe000_0000, 0x10_0000)), None, None);
    bar(&dev, 0, true, false, 0x1000, 0xd000_0000);
    assert_eq!(dev.allocate_bars(), Ok(()));
    let at = dev.get_bar(0).unwrap().bus_addr;
    assert!(
        (0xe000_0000..0xe010_0000).contains(&at),
        "left at {:#x}",
        at
    );
    assert_eq!(space.peek32(BAR0) as u64, at);
}

#[test]
fn a_64_bit_window_with_no_high_space_falls_back_to_low_space() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    // Type bits 0b100: 64-bit memory. Both registers start at all ones.
    space.poke32(BAR0, 0xffff_fff4);
    space.poke32(BAR0 + 4, 0xffff_ffff);
    let dev = a_device(space.config());
    let _root = under_a_root(&bus, &dev, Some((0xe000_0000, 0x10_0000)), None, None);
    bar(&dev, 0, true, true, 0x4000, 0);
    assert_eq!(dev.allocate_bars(), Ok(()));
    let at = dev.get_bar(0).unwrap().bus_addr;
    assert!((0xe000_0000..0xe010_0000).contains(&at));
    assert_eq!(
        space.peek32(BAR0),
        at as u32 | 0b100,
        "the type bits are kept"
    );
    assert_eq!(space.peek32(BAR0 + 4), 0, "the high half says below 4 GiB");
}

#[test]
fn a_64_bit_window_goes_high_when_there_is_high_space() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    space.poke32(BAR0, 0b100);
    let dev = a_device(space.config());
    let high = 0x40_0000_0000usize;
    let _root = under_a_root(
        &bus,
        &dev,
        Some((0xe000_0000, 0x10_0000)),
        Some((high, 0x1000_0000)),
        None,
    );
    bar(&dev, 0, true, true, 0x10_0000, 0);
    assert_eq!(dev.allocate_bars(), Ok(()));
    let at = dev.get_bar(0).unwrap().bus_addr;
    assert!(at >= high as u64, "a 64-bit BAR was put low: {:#x}", at);
    assert_eq!(space.peek32(BAR0 + 4), (at >> 32) as u32);
    assert_eq!(space.peek32(BAR0) & !0xf, at as u32);
}

#[test]
fn an_io_window_comes_from_the_ports() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    // Bit 0: I/O space.
    space.poke32(BAR0, 1);
    let dev = a_device(space.config());
    let _root = under_a_root(
        &bus,
        &dev,
        Some((0xe000_0000, 0x10_0000)),
        None,
        Some((0xc000, 0x1000)),
    );
    bar(&dev, 0, false, false, 0x20, 0);
    assert_eq!(dev.allocate_bars(), Ok(()));
    let at = dev.get_bar(0).unwrap().bus_addr;
    assert!((0xc000..0xd000).contains(&at), "ports at {:#x}", at);
    assert_eq!(space.peek32(BAR0), at as u32 | 1);
}

#[test]
fn a_window_with_no_room_anywhere_fails_the_device_and_the_rest_are_left() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    // 64 KiB of space, a 1 MiB BAR.
    let _root = under_a_root(&bus, &dev, Some((0xe000_0000, 0x1_0000)), None, None);
    bar(&dev, 0, true, false, 0x10_0000, 0);
    assert_eq!(dev.allocate_bars(), Err(ZxError::NOT_FOUND));
    assert_eq!(
        space.peek32(BAR0),
        0,
        "a BAR with no space was given an address"
    );
    // The space is all still there for another device.
    assert!(bus.mmio_lo.lock().allocate_by_addr(0xe000_0000, 0x1_0000));
}

#[test]
fn a_device_with_nothing_above_it_cannot_be_given_space() {
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    bar(&dev, 0, true, false, 0x1000, 0);
    assert_eq!(dev.allocate_bars(), Err(ZxError::UNAVAILABLE));
    // A device with no BARs at all needs nothing from anyone.
    let mut other = ConfigSpace::new();
    let empty = a_device(other.config());
    assert_eq!(empty.allocate_bars(), Ok(()));
}

#[test]
fn a_second_pass_leaves_windows_already_given_alone() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    let _root = under_a_root(&bus, &dev, Some((0xe000_0000, 0x10_0000)), None, None);
    bar(&dev, 0, true, false, 0x1000, 0);
    bar(&dev, 2, true, false, 0x1000, 0);
    assert_eq!(dev.allocate_bars(), Ok(()));
    let first = (
        dev.get_bar(0).unwrap().bus_addr,
        dev.get_bar(2).unwrap().bus_addr,
    );
    assert_ne!(first.0, first.1, "two BARs were given the same window");
    assert_eq!(dev.allocate_bars(), Ok(()));
    let second = (
        dev.get_bar(0).unwrap().bus_addr,
        dev.get_bar(2).unwrap().bus_addr,
    );
    assert_eq!(first, second);
}

#[test]
fn get_bar_answers_only_for_the_bars_the_device_has() {
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    bar(&dev, 5, true, false, 0x1000, 0xe000_0000);
    assert_eq!(dev.get_bar(5).map(|b| b.size), Some(0x1000));
    assert!(dev.get_bar(6).is_none());
    assert!(dev.get_bar(usize::MAX).is_none());
}

// ---- the command register -------------------------------------------------

#[test]
fn mmio_and_pio_decoding_turn_on_and_off_and_leave_the_interrupt_bit_be() {
    let mut space = ConfigSpace::new();
    space.poke16(PciReg16::Command as usize, PCIE_CFG_COMMAND_INT_DISABLE);
    let dev = a_device(space.config());
    let cmd = |s: &ConfigSpace| s.peek16(PciReg16::Command as usize);
    assert_eq!(dev.enable_mmio(true), Ok(()));
    assert_eq!(
        cmd(&space),
        PCIE_CFG_COMMAND_INT_DISABLE | PCI_COMMAND_MEM_EN
    );
    assert_eq!(dev.enable_pio(true), Ok(()));
    assert_eq!(
        cmd(&space),
        PCIE_CFG_COMMAND_INT_DISABLE | PCI_COMMAND_MEM_EN | PCI_COMMAND_IO_EN
    );
    assert_eq!(dev.enable_mmio(false), Ok(()));
    assert_eq!(dev.enable_pio(false), Ok(()));
    assert_eq!(
        cmd(&space),
        PCIE_CFG_COMMAND_INT_DISABLE,
        "turning decoding off also let the device interrupt"
    );
}

#[test]
fn an_unplugged_device_is_not_switched_on() {
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    dev.inner.lock().plugged_in = false;
    assert_eq!(dev.enable_mmio(true), Err(ZxError::UNAVAILABLE));
    assert_eq!(dev.enable_pio(true), Err(ZxError::UNAVAILABLE));
    assert_eq!(dev.enable_master(true), Err(ZxError::UNAVAILABLE));
    assert_eq!(space.peek16(PciReg16::Command as usize), 0);
}

#[test]
fn bus_mastering_of_a_device_under_a_root_needs_nothing_from_the_root() {
    let bus = PCIeBusDriver::new();
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    let _root = under_a_root(&bus, &dev, None, None, None);
    assert_eq!(dev.enable_master(true), Ok(()));
    assert_eq!(
        space.peek16(PciReg16::Command as usize),
        PCI_COMMAND_BUS_MASTER_EN
    );
    assert_eq!(dev.enable_master(false), Ok(()));
    assert_eq!(space.peek16(PciReg16::Command as usize), 0);
}

// ---- what a mode can offer --------------------------------------------------

#[test]
fn the_irq_modes_a_device_offers_follow_its_pin_and_its_capabilities() {
    let mut space = ConfigSpace::new();
    let dev = a_device(space.config());
    assert_eq!(
        dev.get_irq_mode_capabilities(PcieIrqMode::Disabled)
            .map(|c| c.max_irqs),
        Ok(0)
    );
    // No pin, no MSI capability: neither mode.
    assert_eq!(
        dev.get_irq_mode_capabilities(PcieIrqMode::Legacy).err(),
        Some(ZxError::NOT_SUPPORTED)
    );
    assert_eq!(
        dev.get_irq_mode_capabilities(PcieIrqMode::Msi).err(),
        Some(ZxError::NOT_SUPPORTED)
    );
    assert_eq!(
        dev.get_irq_mode_capabilities(PcieIrqMode::MsiX).err(),
        Some(ZxError::NOT_SUPPORTED)
    );
    dev.inner.lock().irq.legacy.pin = 1;
    let legacy = dev.get_irq_mode_capabilities(PcieIrqMode::Legacy).unwrap();
    assert_eq!(
        (legacy.max_irqs, legacy.per_vector_masking_supported),
        (1, true)
    );
    dev.inner.lock().plugged_in = false;
    assert_eq!(
        dev.get_irq_mode_capabilities(PcieIrqMode::Legacy).err(),
        Some(ZxError::BAD_STATE)
    );
}
