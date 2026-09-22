use super::nodes::{
    IPciNode, PciNodeType, PciRoot, PcieBarInfo, PcieIrqMode, PcieIrqModeCaps,
    SharedLegacyIrqHandler,
};
use super::{
    config::PciConfig, constants::*, pci_init_args::PciIrqSwizzleLut, pmio::pci_bdf_raw_addr,
    MappedEcamRegion, PciAddrSpace, PciEcamRegion,
};
use crate::dev::Interrupt;
use crate::object::*;
use crate::vm::{kernel_allocate_physical, CachePolicy, MMUFlags, PhysAddr, VirtAddr};
use crate::{ZxError, ZxResult};

use alloc::sync::{Arc, Weak};
use alloc::{collections::BTreeMap, vec::Vec};
use core::cmp::{max, min};
use core::marker::{Send, Sync};
use kernel_hal::sync::Mutex;
use lazy_static::*;
use region_alloc::RegionAllocator;

/// How an MMIO window is split between the allocator below 4 GiB and the one
/// above it, as `(base, size)` for each half.
///
/// A window that straddles the boundary belongs to both. The high half used
/// to be handed `end` where a size was expected, so a window reaching past
/// 4 GiB registered everything from the boundary up to its own end *plus*
/// four gigabytes — address space that is not the PCI window at all, and
/// that a BAR could then be allocated over. QEMU's window lies entirely
/// below 4 GiB, so the high half never ran.
#[allow(clippy::type_complexity)]
fn split_mmio_window(base: u64, size: u64) -> ZxResult<(Option<(u64, u64)>, Option<(u64, u64)>)> {
    const BOUNDARY: u64 = u32::MAX as u64 + 1;
    let end = base.checked_add(size).ok_or(ZxError::INVALID_ARGS)?;
    let lo = if base < BOUNDARY {
        Some((base, min(BOUNDARY - base, size)))
    } else {
        None
    };
    let hi = if end > BOUNDARY {
        let hi_base = max(base, BOUNDARY);
        Some((hi_base, end - hi_base))
    } else {
        None
    };
    Ok((lo, hi))
}

/// Whether two inclusive ranges of bus numbers share a bus.
fn bus_ranges_overlap(a: (u8, u8), b: (u8, u8)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

/// Whether an ECAM window may be added, given the windows already mapped.
///
/// Two windows claiming the same bus would give one function two
/// configuration spaces at different addresses. The test for that was
/// written the wrong way round — it asked whether the ranges were
/// *disjoint* — so a second host bridge's window was refused and an
/// overlapping one was taken. It also only looked at the neighbour below,
/// missing a window that starts lower and reaches over an existing one, and
/// its last clause computed `bus_end + 1` on a `u8`, which overflows on bus
/// 255.
fn check_ecam_region(
    ecam: &PciEcamRegion,
    mut existing: impl Iterator<Item = (u8, u8)>,
) -> ZxResult {
    if ecam.bus_start > ecam.bus_end {
        return Err(ZxError::INVALID_ARGS);
    }
    let bus_count = (ecam.bus_end - ecam.bus_start) as usize + 1;
    if ecam.size != bus_count * PCIE_ECAM_BYTES_PER_BUS {
        return Err(ZxError::INVALID_ARGS);
    }
    let want = (ecam.bus_start, ecam.bus_end);
    if existing.any(|have| bus_ranges_overlap(want, have)) {
        return Err(ZxError::BAD_STATE);
    }
    Ok(())
}

/// Where one function's configuration space sits inside an ECAM window.
///
/// PCI Express 4.0 §7.2.2 lays the window out with the bus in bits 27:20,
/// the device in 19:15 and the function in 14:12. A device or function
/// number past the end of its field does not wrap, it carries into the next
/// field — device 32 addresses the next bus, which is somebody else's
/// registers — so it is refused rather than encoded.
fn ecam_offset(bus_offset: u8, device_id: u8, function_id: u8) -> ZxResult<usize> {
    if device_id as usize >= PCI_MAX_DEVICES_PER_BUS
        || function_id as usize >= PCI_MAX_FUNCTIONS_PER_DEVICE
    {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok((bus_offset as usize) << 20 | (device_id as usize) << 15 | (function_id as usize) << 12)
}

/// PCIE Bus Driver.
pub struct PCIeBusDriver {
    pub(crate) mmio_lo: Arc<Mutex<RegionAllocator>>,
    pub(crate) mmio_hi: Arc<Mutex<RegionAllocator>>,
    pub(crate) pio_region: Arc<Mutex<RegionAllocator>>,
    address_provider: Option<Arc<dyn PCIeAddressProvider>>,
    roots: BTreeMap<usize, Arc<PciRoot>>,
    state: PCIeBusDriverState,
    bus_topology: Mutex<()>,
    configs: Mutex<Vec<Arc<PciConfig>>>,
    legacy_irq_list: Mutex<Vec<Arc<SharedLegacyIrqHandler>>>,
}

#[derive(PartialEq, Debug)]
enum PCIeBusDriverState {
    NotStarted,
    StartingScanning,
    StartingRunningQuirks,
    StartingResourceAllocation,
    Operational,
}

lazy_static! {
    static ref _INSTANCE: Mutex<PCIeBusDriver> = Mutex::new(PCIeBusDriver::new());
}

impl PCIeBusDriver {
    /// Add bus region.
    pub fn add_bus_region(base: u64, size: u64, aspace: PciAddrSpace) -> ZxResult {
        _INSTANCE.lock().add_bus_region_inner(base, size, aspace)
    }
    /// Subtract bus region.
    pub fn sub_bus_region(base: u64, size: u64, aspace: PciAddrSpace) -> ZxResult {
        _INSTANCE.lock().sub_bus_region_inner(base, size, aspace)
    }
    /// A PcieAddressProvider translates a BDF address to an address that the
    /// system can use to access ECAMs.
    pub fn set_address_translation_provider(provider: Arc<dyn PCIeAddressProvider>) -> ZxResult {
        _INSTANCE
            .lock()
            .set_address_translation_provider_inner(provider)
    }

    /// Add a root bus to the driver and attempt to scan it for devices.
    pub fn add_root(bus_id: usize, lut: PciIrqSwizzleLut) -> ZxResult {
        let mut bus = _INSTANCE.lock();
        let root = PciRoot::new(bus_id, lut, &bus);
        bus.add_root_inner(root)
    }

    /// Start the bus driver.
    pub fn start_bus_driver() -> ZxResult {
        _INSTANCE.lock().start_bus_driver_inner()
    }

    /// Get the "Nth" device.
    pub fn get_nth_device(n: usize) -> ZxResult<(PcieDeviceInfo, Arc<PcieDeviceKObject>)> {
        let device_node = _INSTANCE
            .lock()
            .get_nth_device_inner(n)
            .ok_or(ZxError::OUT_OF_RANGE)?;
        let device = device_node.device();
        let info = PcieDeviceInfo {
            vendor_id: device.vendor_id,
            device_id: device.device_id,
            base_class: device.class_id,
            sub_class: device.subclass_id,
            program_interface: device.prog_if,
            revision_id: device.rev_id,
            bus_id: device.bus_id as u8,
            dev_id: device.dev_id as u8,
            func_id: device.func_id as u8,
            _padding1: 0,
        };
        let object = PcieDeviceKObject::new(device_node.clone());
        Ok((info, object))
    }
}

impl PCIeBusDriver {
    fn new() -> Self {
        PCIeBusDriver {
            mmio_lo: Default::default(),
            mmio_hi: Default::default(),
            pio_region: Default::default(),
            address_provider: None,
            roots: BTreeMap::new(),
            state: PCIeBusDriverState::NotStarted,
            bus_topology: Mutex::default(),
            legacy_irq_list: Mutex::new(Vec::new()),
            configs: Mutex::new(Vec::new()),
        }
    }
    fn add_bus_region_inner(&mut self, base: u64, size: u64, aspace: PciAddrSpace) -> ZxResult {
        self.add_or_sub_bus_region(base, size, aspace, true)
    }
    fn sub_bus_region_inner(&mut self, base: u64, size: u64, aspace: PciAddrSpace) -> ZxResult {
        self.add_or_sub_bus_region(base, size, aspace, false)
    }
    fn set_address_translation_provider_inner(
        &mut self,
        provider: Arc<dyn PCIeAddressProvider>,
    ) -> ZxResult {
        if self.is_started(false) {
            return Err(ZxError::BAD_STATE);
        }
        self.address_provider = Some(provider);
        Ok(())
    }
    fn add_root_inner(&mut self, root: Arc<PciRoot>) -> ZxResult {
        if self.is_started(false) {
            return Err(ZxError::BAD_STATE);
        }
        if self.roots.contains_key(&root.managed_bus_id()) {
            return Err(ZxError::ALREADY_EXISTS);
        }
        self.bus_topology.lock();
        self.roots.insert(root.managed_bus_id(), root);
        Ok(())
    }
    fn add_or_sub_bus_region(
        &mut self,
        base: u64,
        size: u64,
        aspace: PciAddrSpace,
        is_add: bool,
    ) -> ZxResult {
        if self.is_started(true) {
            return Err(ZxError::BAD_STATE);
        }
        if size == 0 {
            return Err(ZxError::INVALID_ARGS);
        }
        if aspace == PciAddrSpace::MMIO {
            let (lo, hi) = split_mmio_window(base, size)?;
            if let Some((lo_base, lo_size)) = lo {
                self.mmio_lo
                    .lock()
                    .add_or_subtract(lo_base as usize, lo_size as usize, is_add);
            }
            if let Some((hi_base, hi_size)) = hi {
                self.mmio_hi
                    .lock()
                    .add_or_subtract(hi_base as usize, hi_size as usize, is_add);
            }
        } else if aspace == PciAddrSpace::PIO {
            let end = base + size - 1;
            if ((base | end) & !PCIE_PIO_ADDR_SPACE_MASK) != 0 {
                return Err(ZxError::INVALID_ARGS);
            }
            self.pio_region
                .lock()
                .add_or_subtract(base as usize, size as usize, is_add);
        }
        Ok(())
    }

    fn start_bus_driver_inner(&mut self) -> ZxResult {
        self.transfer_state(
            PCIeBusDriverState::NotStarted,
            PCIeBusDriverState::StartingScanning,
        )?;
        self.foreach_root(
            |root, _c| {
                root.base_upstream.scan_downstream(self);
                true
            },
            (),
        );
        self.transfer_state(
            PCIeBusDriverState::StartingScanning,
            PCIeBusDriverState::StartingRunningQuirks,
        )?;
        warn!("pci: skip quirks");
        self.transfer_state(
            PCIeBusDriverState::StartingRunningQuirks,
            PCIeBusDriverState::StartingResourceAllocation,
        )?;
        self.foreach_root(
            |root, _| {
                root.base_upstream.allocate_downstream_bars();
                true
            },
            (),
        );
        self.transfer_state(
            PCIeBusDriverState::StartingResourceAllocation,
            PCIeBusDriverState::Operational,
        )?;
        Ok(())
    }
    fn foreach_root<T, C>(&self, callback: T, context: C) -> C
    where
        T: Fn(Arc<PciRoot>, &mut C) -> bool,
    {
        let mut bus_top_guard = self.bus_topology.lock();
        let mut context = context;
        for root in self.roots.values() {
            drop(bus_top_guard);
            if !callback(root.clone(), &mut context) {
                return context;
            }
            bus_top_guard = self.bus_topology.lock();
        }
        drop(bus_top_guard);
        context
    }

    #[allow(dead_code)]
    fn foreach_device<T, C>(&self, callback: &T, context: C) -> C
    where
        T: Fn(Arc<dyn IPciNode>, &mut C, usize) -> bool,
    {
        self.foreach_root(
            |root, ctx| {
                self.foreach_downstream(root, 0 /*level*/, callback, &mut (ctx.0))
            },
            (context, &self),
        )
        .0
    }

    #[allow(dead_code)]
    fn foreach_downstream<T, C>(
        &self,
        upstream: Arc<dyn IPciNode>,
        level: usize,
        callback: &T,
        context: &mut C,
    ) -> bool
    where
        T: Fn(Arc<dyn IPciNode>, &mut C, usize) -> bool,
    {
        if level > 256 || upstream.as_upstream().is_none() {
            return true;
        }
        let upstream = upstream.as_upstream().unwrap();
        for i in 0..PCI_MAX_FUNCTIONS_PER_BUS {
            let device = upstream.get_downstream(i);
            if let Some(dev) = device {
                if !callback(dev.clone(), context, level) {
                    return false;
                }
                if let PciNodeType::Bridge = dev.node_type() {
                    if !self.foreach_downstream(dev, level + 1, callback, context) {
                        return false;
                    }
                }
            }
        }
        true
    }
    fn transfer_state(
        &mut self,
        expected: PCIeBusDriverState,
        target: PCIeBusDriverState,
    ) -> ZxResult {
        trace!("transfer state from {:#x?} to {:#x?}", expected, target);
        if self.state != expected {
            return Err(ZxError::BAD_STATE);
        }
        self.state = target;
        Ok(())
    }
    fn is_started(&self, _allow_quirks_phase: bool) -> bool {
        !matches!(self.state, PCIeBusDriverState::NotStarted)
    }

    /// Get a device's config.
    pub fn get_config(
        &self,
        bus_id: usize,
        dev_id: usize,
        func_id: usize,
    ) -> Option<(Arc<PciConfig>, PhysAddr)> {
        self.address_provider.as_ref()?;
        let (paddr, vaddr) = self
            .address_provider
            .clone()
            .unwrap()
            .translate(bus_id as u8, dev_id as u8, func_id as u8)
            .ok()?;
        let mut config = self.configs.lock();
        let cfg = config.iter().find(|x| x.base == vaddr);
        if let Some(x) = cfg {
            return Some((x.clone(), paddr));
        }
        let cfg = self
            .address_provider
            .clone()
            .unwrap()
            .create_config(vaddr as u64);
        config.push(cfg.clone());
        Some((cfg, paddr))
    }

    /// Link a device to an upstream node.
    pub fn link_device_to_upstream(&self, down: Arc<dyn IPciNode>, up: Weak<dyn IPciNode>) {
        let _guard = self.bus_topology.lock();
        let dev = down.device();
        dev.set_upstream(up.clone());
        let up = up.upgrade().unwrap().as_upstream().unwrap();
        up.set_downstream(
            dev.dev_id() * PCI_MAX_FUNCTIONS_PER_DEVICE + dev.func_id(),
            Some(down.clone()),
        );
    }

    /// Find the legacy IRQ handler.
    pub fn find_legacy_irq_handler(&self, irq_id: usize) -> ZxResult<Arc<SharedLegacyIrqHandler>> {
        let mut list = self.legacy_irq_list.lock();
        for i in list.iter() {
            if irq_id == i.irq_id {
                return Ok(i.clone());
            }
        }
        SharedLegacyIrqHandler::create(irq_id)
            .inspect(|x| {
                list.push(x.clone());
            })
            .ok_or(ZxError::NO_RESOURCES)
    }

    fn get_nth_device_inner(&self, n: usize) -> Option<Arc<dyn IPciNode>> {
        self.foreach_device(
            &|device, context: &mut (usize, Option<Arc<_>>), _level| {
                if context.0 == 0 {
                    context.1 = Some(device);
                    false
                } else {
                    context.0 -= 1;
                    true
                }
            },
            (n, None),
        )
        .1
    }
}

/// PcieAddressProvider is an interface that implements translation from a BDF to
/// a PCI ECAM address.
pub trait PCIeAddressProvider: Send + Sync {
    /// Creates a config that corresponds to the type of the PcieAddressProvider.
    fn create_config(&self, addr: u64) -> Arc<PciConfig>;

    /// Accepts a PCI BDF triple and returns ZX_OK if it is able to translate it
    /// into an ECAM address.
    fn translate(&self, bus_id: u8, dev_id: u8, func_id: u8) -> ZxResult<(PhysAddr, VirtAddr)>;
}

/// Systems that have memory mapped Config Spaces.
#[derive(Default)]
pub struct MmioPcieAddressProvider {
    ecam_regions: Mutex<BTreeMap<u8, MappedEcamRegion>>,
}

impl MmioPcieAddressProvider {
    /// Add a ECAM region.
    pub fn add_ecam(&self, ecam: PciEcamRegion) -> ZxResult {
        let mut inner = self.ecam_regions.lock();
        check_ecam_region(
            &ecam,
            inner.values().map(|v| (v.ecam.bus_start, v.ecam.bus_end)),
        )?;
        let vaddr = kernel_allocate_physical(
            ecam.size,
            ecam.phys_base as PhysAddr,
            MMUFlags::READ | MMUFlags::WRITE,
            CachePolicy::UncachedDevice,
        )?;
        inner.insert(
            ecam.bus_start,
            MappedEcamRegion {
                ecam,
                vaddr: vaddr as u64,
            },
        );
        Ok(())
    }
}

impl PCIeAddressProvider for MmioPcieAddressProvider {
    fn create_config(&self, addr: u64) -> Arc<PciConfig> {
        Arc::new(PciConfig {
            addr_space: PciAddrSpace::MMIO,
            base: addr as usize,
        })
    }
    fn translate(
        &self,
        bus_id: u8,
        device_id: u8,
        function_id: u8,
    ) -> ZxResult<(PhysAddr, VirtAddr)> {
        let regions = self.ecam_regions.lock();
        let target = regions.range(..=bus_id).last().ok_or(ZxError::NOT_FOUND)?;
        if bus_id < target.1.ecam.bus_start || bus_id > target.1.ecam.bus_end {
            return Err(ZxError::NOT_FOUND);
        }
        let bus_id = bus_id - target.1.ecam.bus_start;
        let offset = ecam_offset(bus_id, device_id, function_id)?;
        let phys = target.1.ecam.phys_base as usize + offset;
        let vaddr = target.1.vaddr as usize + offset;
        Ok((phys, vaddr))
    }
}

/// Systems that have PIO mapped Config Spaces.
#[derive(Default)]
pub struct PmioPcieAddressProvider;

impl PCIeAddressProvider for PmioPcieAddressProvider {
    fn create_config(&self, addr: u64) -> Arc<PciConfig> {
        Arc::new(PciConfig {
            addr_space: PciAddrSpace::PIO,
            base: addr as usize,
        })
    }
    fn translate(
        &self,
        bus_id: u8,
        device_id: u8,
        function_id: u8,
    ) -> ZxResult<(PhysAddr, VirtAddr)> {
        let virt = pci_bdf_raw_addr(bus_id, device_id, function_id, 0);
        Ok((0, virt as VirtAddr))
    }
}

/// Info returned to dev manager for PCIe devices when probing.
#[allow(missing_docs)]
#[repr(C)]
#[derive(Clone, Debug)]
pub struct PcieDeviceInfo {
    pub vendor_id: u16,
    pub device_id: u16,
    pub base_class: u8,
    pub sub_class: u8,
    pub program_interface: u8,
    pub revision_id: u8,
    pub bus_id: u8,
    pub dev_id: u8,
    pub func_id: u8,
    _padding1: u8,
}

/// PCIE Device Entity.
pub struct PcieDeviceKObject {
    base: KObjectBase,
    device: Arc<dyn IPciNode>,
    irqs_avail_cnt: u32, // WARNING
    irqs_maskable: bool, // WARNING
}

impl_kobject!(PcieDeviceKObject);

impl PcieDeviceKObject {
    /// Create a new PcieDeviceKObject.
    pub fn new(device: Arc<dyn IPciNode>) -> Arc<PcieDeviceKObject> {
        Arc::new(PcieDeviceKObject {
            base: KObjectBase::new(),
            device,
            irqs_avail_cnt: 10,  // WARNING
            irqs_maskable: true, // WARNING
        })
    }

    /// Get PcieBarInfo.
    pub fn get_bar(&self, bar_num: u32) -> ZxResult<PcieBarInfo> {
        let device = self.device.device();
        device.get_bar(bar_num as usize).ok_or(ZxError::NOT_FOUND)
    }

    /// Map the interrupt to the IRQ.
    pub fn map_interrupt(&self, irq: i32) -> ZxResult<Arc<Interrupt>> {
        if irq < 0 || irq as u32 >= self.irqs_avail_cnt {
            return Err(ZxError::INVALID_ARGS);
        }
        Interrupt::new_pci(self.device.clone(), irq as u32, self.irqs_maskable)
    }

    /// Enable MMIO.
    pub fn enable_mmio(&self) -> ZxResult {
        self.device.device().enable_mmio(true)
    }

    /// Enable PIO.
    pub fn enable_pio(&self) -> ZxResult {
        self.device.device().enable_pio(true)
    }

    /// Enable bus mastering.
    pub fn enable_master(&self, enable: bool) -> ZxResult {
        self.device.device().enable_master(enable)
    }

    /// Check whether `mode` is capable PCI device's IRQ modes.
    pub fn get_irq_mode_capabilities(&self, mode: PcieIrqMode) -> ZxResult<PcieIrqModeCaps> {
        self.device.device().get_irq_mode_capabilities(mode)
    }

    /// Set IRQ mode.
    pub fn set_irq_mode(&self, mode: PcieIrqMode, requested_irqs: usize) -> ZxResult {
        self.device.device().set_irq_mode(mode, requested_irqs)
    }

    /// Read the device's config.
    pub fn config_read(&self, offset: usize, width: usize) -> ZxResult<u32> {
        self.device.device().config_read(offset, width)
    }

    /// Write the device's config.
    pub fn config_write(&self, offset: usize, width: usize, val: u32) -> ZxResult {
        self.device.device().config_write(offset, width, val)
    }
}

#[cfg(test)]
mod pci_window_tests {
    use super::*;

    const BOUNDARY: u64 = u32::MAX as u64 + 1;

    fn ecam(bus_start: u8, bus_end: u8) -> PciEcamRegion {
        PciEcamRegion {
            phys_base: 0xE000_0000,
            size: ((bus_end - bus_start) as usize + 1) * PCIE_ECAM_BYTES_PER_BUS,
            bus_start,
            bus_end,
        }
    }

    // ---------- Splitting an MMIO window at 4 GiB ----------

    #[test]
    fn a_window_below_four_gigabytes_stays_below() {
        // What QEMU hands out, which is why nothing else here ever ran.
        assert_eq!(
            split_mmio_window(0xC000_0000, 0x3EC0_0000),
            Ok((Some((0xC000_0000, 0x3EC0_0000)), None))
        );
        // And one that ends exactly on the boundary is still all low.
        assert_eq!(
            split_mmio_window(0xC000_0000, 0x4000_0000),
            Ok((Some((0xC000_0000, 0x4000_0000)), None))
        );
    }

    #[test]
    fn a_window_above_four_gigabytes_stays_above() {
        assert_eq!(
            split_mmio_window(BOUNDARY, 0x4_0000_0000),
            Ok((None, Some((BOUNDARY, 0x4_0000_0000))))
        );
        assert_eq!(
            split_mmio_window(0x10_0000_0000, 1 << 30),
            Ok((None, Some((0x10_0000_0000, 1 << 30))))
        );
    }

    #[test]
    fn a_window_that_straddles_four_gigabytes_is_cut_in_two() {
        // 2 GiB of window starting at 2 GiB: half below the boundary, half
        // above. The high half is 2 GiB — not 6 GiB, which is what passing
        // the window's end where a size belongs used to register, handing
        // the allocator four gigabytes of address space that is not the PCI
        // window and that a BAR could then be placed over.
        assert_eq!(
            split_mmio_window(0x8000_0000, 0x1_0000_0000),
            Ok((
                Some((0x8000_0000, 0x8000_0000)),
                Some((BOUNDARY, 0x8000_0000))
            ))
        );
    }

    #[test]
    fn the_two_halves_are_exactly_the_window_and_nothing_more() {
        for (base, size) in [
            (0u64, 1u64),
            (0, BOUNDARY),
            (0, BOUNDARY + 1),
            (0x8000_0000, 0x1_0000_0000),
            (BOUNDARY - 1, 2),
            (BOUNDARY, 1),
            (0xC000_0000, 0x3EC0_0000),
            (0x10_0000_0000, 1 << 28),
        ] {
            let (lo, hi) = split_mmio_window(base, size).unwrap();
            let covered: u64 = lo.map_or(0, |(_, s)| s) + hi.map_or(0, |(_, s)| s);
            assert_eq!(covered, size, "ventana {:#x}+{:#x}", base, size);
            if let Some((lo_base, lo_size)) = lo {
                assert_eq!(lo_base, base);
                assert!(lo_base + lo_size <= BOUNDARY);
            }
            if let Some((hi_base, hi_size)) = hi {
                assert!(hi_base >= BOUNDARY);
                assert_eq!(hi_base + hi_size, base + size);
            }
            if let (Some((lo_base, lo_size)), Some((hi_base, _))) = (lo, hi) {
                assert_eq!(lo_base + lo_size, hi_base, "las mitades dejan un hueco");
            }
        }
    }

    #[test]
    fn a_window_that_wraps_the_address_space_is_refused() {
        assert_eq!(split_mmio_window(u64::MAX, 2), Err(ZxError::INVALID_ARGS));
        assert_eq!(split_mmio_window(1, u64::MAX), Err(ZxError::INVALID_ARGS));
    }

    // ---------- Adding an ECAM window ----------

    #[test]
    fn two_bus_ranges_overlap_only_when_they_share_a_bus() {
        assert!(bus_ranges_overlap((0, 7), (7, 15)));
        assert!(bus_ranges_overlap((0, 255), (128, 128)));
        assert!(bus_ranges_overlap((4, 4), (4, 4)));
        assert!(!bus_ranges_overlap((0, 7), (8, 15)));
        assert!(!bus_ranges_overlap((8, 15), (0, 7)));
    }

    #[test]
    fn a_second_host_bridges_window_is_accepted() {
        // The machine has one window for bus 0 and a second for the rest.
        // This is the case the old test refused: it asked whether the two
        // ranges were disjoint and called that an overlap.
        let existing = [(0u8, 0u8)];
        assert_eq!(
            check_ecam_region(&ecam(1, 255), existing.iter().copied()),
            Ok(())
        );
        assert_eq!(
            check_ecam_region(&ecam(128, 255), existing.iter().copied()),
            Ok(())
        );
    }

    #[test]
    fn a_window_that_claims_a_bus_twice_is_refused() {
        let existing = [(8u8, 15u8)];
        // Overlapping from above, from below, and swallowing it whole.
        assert_eq!(
            check_ecam_region(&ecam(12, 20), existing.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
        assert_eq!(
            check_ecam_region(&ecam(0, 8), existing.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
        assert_eq!(
            check_ecam_region(&ecam(0, 255), existing.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
        assert_eq!(
            check_ecam_region(&ecam(8, 15), existing.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
        // With more than one window already mapped the clash can be with any
        // of them; the old check only ever looked at the neighbour below.
        let two = [(0u8, 3u8), (8u8, 15u8)];
        assert_eq!(
            check_ecam_region(&ecam(12, 20), two.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
        assert_eq!(
            check_ecam_region(&ecam(2, 2), two.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
        // And the gap between them is still free.
        assert_eq!(check_ecam_region(&ecam(4, 7), two.iter().copied()), Ok(()));
    }

    #[test]
    fn a_window_reaching_the_last_bus_does_not_overflow() {
        // The old check computed `bus_end + 1` on a `u8`.
        let existing = [(255u8, 255u8)];
        assert_eq!(
            check_ecam_region(&ecam(0, 254), existing.iter().copied()),
            Ok(())
        );
        assert_eq!(
            check_ecam_region(&ecam(200, 255), existing.iter().copied()),
            Err(ZxError::BAD_STATE)
        );
    }

    #[test]
    fn a_window_has_to_describe_the_buses_it_claims() {
        assert_eq!(check_ecam_region(&ecam(0, 0), core::iter::empty()), Ok(()));
        let backwards = PciEcamRegion {
            phys_base: 0xE000_0000,
            size: PCIE_ECAM_BYTES_PER_BUS,
            bus_start: 8,
            bus_end: 4,
        };
        assert_eq!(
            check_ecam_region(&backwards, core::iter::empty()),
            Err(ZxError::INVALID_ARGS)
        );
        let wrong_size = PciEcamRegion {
            phys_base: 0xE000_0000,
            size: PCIE_ECAM_BYTES_PER_BUS * 3,
            bus_start: 0,
            bus_end: 7,
        };
        assert_eq!(
            check_ecam_region(&wrong_size, core::iter::empty()),
            Err(ZxError::INVALID_ARGS)
        );
    }

    // ---------- Finding a function inside a window ----------

    #[test]
    fn a_function_sits_where_the_specification_puts_it() {
        assert_eq!(ecam_offset(0, 0, 0), Ok(0));
        assert_eq!(ecam_offset(1, 0, 0), Ok(1 << 20));
        assert_eq!(ecam_offset(0, 1, 0), Ok(1 << 15));
        assert_eq!(ecam_offset(0, 0, 1), Ok(1 << 12));
        assert_eq!(ecam_offset(0, 31, 7), Ok(31 << 15 | 7 << 12));
        assert_eq!(ecam_offset(255, 31, 7), Ok(255 << 20 | 31 << 15 | 7 << 12));
    }

    #[test]
    fn a_device_or_function_past_the_end_of_its_field_is_refused() {
        // Device 32 does not wrap, it carries into the bus field: it would
        // address the next bus, which is another device's registers.
        assert_eq!(ecam_offset(0, 32, 0), Err(ZxError::INVALID_ARGS));
        assert_eq!(ecam_offset(0, 255, 0), Err(ZxError::INVALID_ARGS));
        assert_eq!(ecam_offset(0, 0, 8), Err(ZxError::INVALID_ARGS));
        assert_eq!(ecam_offset(0, 0, 255), Err(ZxError::INVALID_ARGS));
    }

    #[test]
    fn every_function_of_a_bus_gets_its_own_place_inside_that_bus() {
        for dev in 0..PCI_MAX_DEVICES_PER_BUS as u8 {
            for func in 0..PCI_MAX_FUNCTIONS_PER_DEVICE as u8 {
                let offset = ecam_offset(3, dev, func).unwrap();
                // Inside bus 3's own megabyte, and nowhere near bus 4's.
                assert!(offset >= 3 << 20, "dev {} func {}", dev, func);
                assert!(offset < 4 << 20, "dev {} func {}", dev, func);
                assert_eq!(
                    offset - (3 << 20) + 4096,
                    (dev as usize * PCI_MAX_FUNCTIONS_PER_DEVICE + func as usize + 1) * 4096
                );
            }
        }
    }
}
