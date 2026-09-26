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
use crate::vm::{kernel_allocate_physical, CachePolicy, MMUFlags, PhysAddr, VirtAddr, PAGE_SIZE};
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
///
/// Everything in `ecam` comes from userspace: `zx_pci_init` copies in an array
/// of address windows and hands the chosen one straight here. It checks the
/// *size* against the bus range and nothing else, so the base used to arrive
/// unexamined, and the base is what every configuration access is measured
/// from:
///
/// * **A base that is not page aligned.** [`ecam_offset`] puts every function
///   on a 4 KiB boundary because that is where PCI Express 4.0 §7.2.2 puts it,
///   and `kernel_allocate_physical` maps whole frames. A base of
///   `0x4000_0800` therefore maps from `0x4000_0000` and hands back a virtual
///   address 0x800 into that first frame, so every function's configuration
///   space starts 2 KiB inside the previous function's: bus 0 device 0 reads
///   the middle of device 0's header as its vendor ID.
/// * **A base whose window runs off the end of the address space.**
///   `translate` adds the function's offset to the base, and the kernel is
///   built `release` with no `overflow-checks`, so a window based near
///   `usize::MAX` wraps that sum round to a low address and a configuration
///   read lands on whatever is mapped there.
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
    if !ecam.phys_base.is_multiple_of(PAGE_SIZE as u64) {
        return Err(ZxError::INVALID_ARGS);
    }
    // The last byte of the window, not one past it: a window is allowed to
    // end exactly at the top of the address space, and `translate` never
    // computes an address beyond that byte. `size` is a whole number of buses
    // by the check above, so it is never zero here.
    if ecam.phys_base.checked_add(ecam.size as u64 - 1).is_none() {
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
    /// A driver with nothing in it. `pub(super)` so the tests of the nodes
    /// that hang off it can build one: the only other way to reach a driver is
    /// the `lazy_static` singleton, and a test that started *that* would decide
    /// what state every other test in the binary sees.
    pub(super) fn new() -> Self {
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
    /// Install the thing that turns a BDF into an address.
    ///
    /// The cache of configuration spaces goes with it. `get_config` keys that
    /// cache on the *virtual address* the provider returned, so an entry made
    /// by one provider answers for any later provider that lands on the same
    /// address -- and the entry outlives the mapping it describes. Replacing
    /// an MMIO provider (whose addresses are wherever `add_ecam` mapped the
    /// window) would leave configuration spaces pointing into the old window,
    /// which nothing unmaps but nothing owns either. `zx_pci_init` may be
    /// called again after a failure further down: it is only refused once the
    /// bus driver has started, and `add_root` failing with `ALREADY_EXISTS` is
    /// exactly such a failure.
    fn set_address_translation_provider_inner(
        &mut self,
        provider: Arc<dyn PCIeAddressProvider>,
    ) -> ZxResult {
        if self.is_started(false) {
            return Err(ZxError::BAD_STATE);
        }
        self.address_provider = Some(provider);
        self.configs.lock().clear();
        Ok(())
    }
    fn add_root_inner(&mut self, root: Arc<PciRoot>) -> ZxResult {
        if self.is_started(false) {
            return Err(ZxError::BAD_STATE);
        }
        if self.roots.contains_key(&root.managed_bus_id()) {
            return Err(ZxError::ALREADY_EXISTS);
        }
        // Bound, not dropped on the spot. `self.bus_topology.lock();` as a
        // statement takes the lock and gives it back before the line that was
        // meant to be under it, which is the one shape of lock bug that reads
        // as correct code; `foreach_root` holds this same lock while it walks
        // the map this line inserts into.
        let _topology = self.bus_topology.lock();
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
        match aspace {
            PciAddrSpace::MMIO => {
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
            }
            PciAddrSpace::PIO => {
                // The window, not its two ends. This was `base + size - 1`
                // followed by a check that `base` and that inclusive end both
                // fit in the I/O space, and the sum is unchecked on a `u64`
                // that came from `zx_pci_add_subtract_io_range` -- so a size
                // large enough to wrap the address space put the end back
                // *below* the base and both ends passed. `base = 0x8000_0000`
                // with `size = 0xFFFF_FFFF_8000_1000` ends at 0xFFF, which is
                // inside the space, and the allocator was then told it owns
                // sixteen exabytes of I/O ports. Every port number it hands
                // out above 0xFFFF is truncated by `out` to sixteen bits, so a
                // BAR placed up there is programmed on top of another
                // device's ports.
                let end = base.checked_add(size).ok_or(ZxError::INVALID_ARGS)?;
                if end > PCIE_PIO_ADDR_SPACE_MASK + 1 {
                    return Err(ZxError::INVALID_ARGS);
                }
                self.pio_region
                    .lock()
                    .add_or_subtract(base as usize, size as usize, is_add);
            }
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
    /// Whether the bus driver is past the point where `allow_quirks_phase`
    /// work may still be done.
    ///
    /// The argument was ignored, so the two callers that pass different values
    /// got the same answer. It exists because the quirks phase runs *inside*
    /// `start_bus_driver`, between the scan and the resource allocation, and a
    /// quirk handler's whole job is to correct what the scan found: taking a
    /// window away from a chipset that lied about it is
    /// `sub_bus_region`, which is why that path asks with quirks allowed while
    /// `add_root` and `set_address_translation_provider` -- which describe the
    /// topology itself, and cannot change once it has been walked -- ask
    /// without. This kernel logs `pci: skip quirks` and runs none, so nothing
    /// is behind the distinction today; a handler added later would otherwise
    /// find `BAD_STATE` and no hint as to why.
    fn is_started(&self, allow_quirks_phase: bool) -> bool {
        match self.state {
            PCIeBusDriverState::NotStarted => false,
            PCIeBusDriverState::StartingRunningQuirks => !allow_quirks_phase,
            _ => true,
        }
    }

    /// Get a device's config.
    pub fn get_config(
        &self,
        bus_id: usize,
        dev_id: usize,
        func_id: usize,
    ) -> Option<(Arc<PciConfig>, PhysAddr)> {
        // Asked once. `as_ref()?` established that there is a provider and
        // then each use asked again with `.clone().unwrap()`, which is a panic
        // written down as the answer to a question already answered.
        let provider = self.address_provider.as_ref()?;
        let (paddr, vaddr) = provider
            .translate(bus_id as u8, dev_id as u8, func_id as u8)
            .ok()?;
        let mut config = self.configs.lock();
        if let Some(x) = config.iter().find(|x| x.base == vaddr) {
            return Some((x.clone(), paddr));
        }
        let cfg = provider.create_config(vaddr as u64);
        config.push(cfg.clone());
        Some((cfg, paddr))
    }

    /// Link a device to an upstream node.
    pub fn link_device_to_upstream(&self, down: Arc<dyn IPciNode>, up: Weak<dyn IPciNode>) {
        let _guard = self.bus_topology.lock();
        let dev = down.device();
        dev.set_upstream(up.clone());
        // Two `unwrap`s: a `Weak` that may have died, and a node that may not
        // be an upstream at all. The scan links every device it finds to the
        // bridge above it, and the link runs after the device object exists,
        // so a bridge dropped in between -- a `scan_downstream` that failed
        // and let its bridge go, a teardown racing the scan -- took the
        // kernel down. The device keeps the dead `Weak` it was given and
        // simply never appears below its parent.
        let Some(up) = up.upgrade().and_then(|node| node.as_upstream()) else {
            warn!(
                "pci: device {:02x}.{} has no upstream to be linked under",
                dev.dev_id(),
                dev.func_id()
            );
            return;
        };
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
    /// Whether the interrupt objects handed out for this device may mask and
    /// unmask their own vector.
    ///
    /// `true` for every device, and it has to stay that way until something
    /// asks the device: `PciInterrupt::unmask` is a no-op when this is false,
    /// and `Interrupt::new_pci` unmasks the vector it just registered, so a
    /// `false` here does not mean "cannot be masked", it means **the
    /// interrupt never arrives**. The device is the one that knows -- legacy
    /// is always maskable, MSI only with per-vector masking -- and its
    /// `enable_irq` already refuses what it cannot do.
    irqs_maskable: bool,
}

impl_kobject!(PcieDeviceKObject);

impl PcieDeviceKObject {
    /// Create a new PcieDeviceKObject.
    pub fn new(device: Arc<dyn IPciNode>) -> Arc<PcieDeviceKObject> {
        Arc::new(PcieDeviceKObject {
            base: KObjectBase::new(),
            device,
            irqs_maskable: true,
        })
    }

    /// Get PcieBarInfo.
    pub fn get_bar(&self, bar_num: u32) -> ZxResult<PcieBarInfo> {
        let device = self.device.device();
        device.get_bar(bar_num as usize).ok_or(ZxError::NOT_FOUND)
    }

    /// Map the interrupt to the IRQ.
    ///
    /// How many vectors the device has is whatever the last
    /// `zx_pci_set_irq_mode` asked for, and this was checked against
    /// `irqs_avail_cnt`: the number **ten**, written into every device object
    /// and never revisited. A device given sixteen or thirty-two MSI vectors
    /// -- which is what `msi_multi_message_encoding` exists to negotiate --
    /// could not have vectors ten and up mapped at all: `zx_pci_map_interrupt`
    /// answered `INVALID_ARGS` for a vector the device has, has a handler slot
    /// for, and will raise. The device answers instead, in
    /// `register_irq_handle`, where the number meets the table that stands for
    /// it; all that is left here is that `irq` arrives as a signed integer
    /// from userspace.
    pub fn map_interrupt(&self, irq: i32) -> ZxResult<Arc<Interrupt>> {
        if irq < 0 {
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

#[cfg(test)]
mod pci_bus_driver_tests {
    use super::*;

    /// A driver with nothing in it: four empty allocators, no address
    /// provider, no roots, `NotStarted`.
    ///
    /// The real one is a `lazy_static` behind a `Mutex`, so every test that
    /// went through `PCIeBusDriver::add_bus_region` and friends would share
    /// one driver, and the first one to call `start_bus_driver` would decide
    /// what state the rest of the binary sees. Each test gets its own.
    fn a_driver() -> PCIeBusDriver {
        PCIeBusDriver::new()
    }

    /// An address provider that answers from a base, so a test can say what
    /// address a function's configuration space is at.
    struct FixedProvider {
        vaddr: usize,
    }

    impl PCIeAddressProvider for FixedProvider {
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
            let offset = ecam_offset(bus_id, device_id, function_id)?;
            Ok((0x1_0000 + offset, self.vaddr + offset))
        }
    }

    fn ecam_at(phys_base: u64, buses: u8) -> PciEcamRegion {
        PciEcamRegion {
            phys_base,
            size: buses as usize * PCIE_ECAM_BYTES_PER_BUS,
            bus_start: 0,
            bus_end: buses - 1,
        }
    }

    // ---------- The window userspace describes the ECAM with ----------

    #[test]
    fn an_ecam_window_that_does_not_start_on_a_page_is_refused() {
        // `zx_pci_init` checks this window's size against the buses it claims
        // and never looks at its base. Every function's configuration space
        // is a 4 KiB region at a 4 KiB boundary inside the window, so a base
        // 0x800 into a page puts each of them half in the one before: device
        // 0's vendor ID would be read out of the middle of somebody's header.
        assert_eq!(
            check_ecam_region(&ecam_at(0xE000_0000, 1), core::iter::empty()),
            Ok(())
        );
        for base in [0x800u64, 0xE000_0800, 0xE000_0001, 0xE000_0FFF] {
            assert_eq!(
                check_ecam_region(&ecam_at(base, 1), core::iter::empty()),
                Err(ZxError::INVALID_ARGS),
                "base {:#x}",
                base
            );
        }
    }

    #[test]
    fn an_ecam_window_that_runs_off_the_end_of_the_address_space_is_refused() {
        // `translate` adds the function's offset to this base with a plain
        // `+`, and the kernel is built `release` with no `overflow-checks`:
        // the sum comes back low and small, and the configuration read lands
        // on whatever is mapped down there.
        let last_page = u64::MAX - PAGE_SIZE as u64 + 1;
        assert_eq!(
            check_ecam_region(&ecam_at(last_page, 1), core::iter::empty()),
            Err(ZxError::INVALID_ARGS)
        );
        // One bus is 1 MiB, so a window 1 MiB below the end is the last one
        // that fits exactly.
        let exact = u64::MAX - PCIE_ECAM_BYTES_PER_BUS as u64 + 1;
        assert_eq!(
            check_ecam_region(&ecam_at(exact, 1), core::iter::empty()),
            Ok(())
        );
        assert_eq!(
            check_ecam_region(&ecam_at(exact, 2), core::iter::empty()),
            Err(ZxError::INVALID_ARGS)
        );
    }

    // ---------- The I/O window userspace adds and subtracts ----------

    #[test]
    fn an_io_window_whose_size_wraps_the_address_space_is_refused() {
        // The one that mattered. `zx_pci_add_subtract_io_range` passes `base`
        // and `len` through with nothing but a root-resource check, and the
        // test here was `base + size - 1` -- unchecked -- followed by "do
        // `base` and that end both fit in the I/O space". A size big enough
        // to wrap puts the end back *below* the base, so both ends fit and
        // the window is taken.
        let mut bus = a_driver();
        let base = 0x8000_0000u64;
        let size = 0xFFFF_FFFF_8000_1000u64;
        // The shape of the old check, so this test is about the fix and not
        // about the numbers: the wrapped inclusive end is 0xFFF.
        assert_eq!(base.wrapping_add(size).wrapping_sub(1), 0xFFF);
        assert_eq!(
            bus.add_bus_region_inner(base, size, PciAddrSpace::PIO),
            Err(ZxError::INVALID_ARGS)
        );
        // Nothing reached the allocator: it was being told it owned sixteen
        // exabytes of I/O ports, and every port above 0xFFFF that it handed
        // out is truncated to sixteen bits by `out`, on top of another
        // device's registers.
        assert!(bus
            .pio_region
            .lock()
            .allocate_by_size(0x1000, 0x1000)
            .is_none());
    }

    #[test]
    fn an_io_window_that_reaches_past_the_io_space_is_refused() {
        let mut bus = a_driver();
        let space = PCIE_PIO_ADDR_SPACE_MASK + 1;
        for (base, size) in [
            (space, 0x1000),
            (space - 0x800, 0x1000),
            (0, space + 1),
            (u64::MAX, 1),
        ] {
            assert_eq!(
                bus.add_bus_region_inner(base, size, PciAddrSpace::PIO),
                Err(ZxError::INVALID_ARGS),
                "{:#x}+{:#x}",
                base,
                size
            );
        }
        // A window of no size is not a window either, whichever space it is in.
        assert_eq!(
            bus.add_bus_region_inner(0x1000, 0, PciAddrSpace::PIO),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            bus.add_bus_region_inner(0x1000, 0, PciAddrSpace::MMIO),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn an_io_window_inside_the_io_space_reaches_the_allocator() {
        // And the last byte of the space is still inside it: the check is on
        // the exclusive end, which is where an off-by-one would show.
        let mut bus = a_driver();
        assert_eq!(
            bus.add_bus_region_inner(0xC000, 0x1000, PciAddrSpace::PIO),
            Ok(())
        );
        assert_eq!(
            bus.pio_region.lock().allocate_by_size(0x100, 0x100),
            Some((0xC000, 0x100))
        );
        let space = PCIE_PIO_ADDR_SPACE_MASK + 1;
        assert_eq!(
            bus.add_bus_region_inner(space - 0x1000, 0x1000, PciAddrSpace::PIO),
            Ok(())
        );
        assert!(bus.pio_region.lock().check_point((space - 1) as usize));
    }

    #[test]
    fn an_mmio_window_lands_in_the_allocator_for_its_half_of_memory() {
        // `split_mmio_window` has its own tests; this is the wiring, which is
        // what decides whether a 64-bit BAR can be placed at all.
        let mut bus = a_driver();
        assert_eq!(
            bus.add_bus_region_inner(0xE000_0000, 0x1000_0000, PciAddrSpace::MMIO),
            Ok(())
        );
        assert!(bus.mmio_lo.lock().check_region(0xE000_0000, 0x1000_0000));
        assert!(!bus.mmio_hi.lock().check_point(0xE000_0000));
        assert_eq!(
            bus.add_bus_region_inner(0x10_0000_0000, 0x1_0000_0000, PciAddrSpace::MMIO),
            Ok(())
        );
        assert!(bus
            .mmio_hi
            .lock()
            .check_region(0x10_0000_0000, 0x1_0000_0000));
        // And taking it away again empties the same allocator.
        assert_eq!(
            bus.sub_bus_region_inner(0x10_0000_0000, 0x1_0000_0000, PciAddrSpace::MMIO),
            Ok(())
        );
        assert!(!bus.mmio_hi.lock().check_point(0x10_0000_0000));
    }

    // ---------- What may still change once the bus has been walked ----------

    #[test]
    fn a_quirk_may_still_take_a_window_away_while_the_quirks_phase_runs() {
        // `is_started` took an `allow_quirks_phase` argument and ignored it,
        // so the two callers that pass different values got the same answer.
        // The quirks phase runs inside `start_bus_driver`, between the scan
        // and the resource allocation, and correcting a window a chipset lied
        // about is exactly `sub_bus_region`.
        let mut bus = a_driver();
        bus.state = PCIeBusDriverState::StartingRunningQuirks;
        assert_eq!(
            bus.add_bus_region_inner(0xC000, 0x1000, PciAddrSpace::PIO),
            Ok(())
        );
        assert_eq!(
            bus.sub_bus_region_inner(0xC800, 0x800, PciAddrSpace::PIO),
            Ok(())
        );
        assert!(!bus.pio_region.lock().check_point(0xC800));
    }

    #[test]
    fn a_window_cannot_be_added_once_the_bus_is_being_scanned_or_is_running() {
        for state in [
            PCIeBusDriverState::StartingScanning,
            PCIeBusDriverState::StartingResourceAllocation,
            PCIeBusDriverState::Operational,
        ] {
            let mut bus = a_driver();
            bus.state = state;
            assert_eq!(
                bus.add_bus_region_inner(0xC000, 0x1000, PciAddrSpace::PIO),
                Err(ZxError::BAD_STATE)
            );
        }
    }

    #[test]
    fn the_topology_is_fixed_the_moment_the_bus_driver_starts() {
        // These two describe the bus itself and cannot change once it has
        // been walked, so unlike a window they are refused in the quirks
        // phase too. That is the distinction `is_started`'s argument is for.
        for state in [
            PCIeBusDriverState::StartingScanning,
            PCIeBusDriverState::StartingRunningQuirks,
            PCIeBusDriverState::Operational,
        ] {
            let mut bus = a_driver();
            bus.state = state;
            assert_eq!(
                bus.set_address_translation_provider_inner(Arc::new(FixedProvider {
                    vaddr: 0xFFFF_0000_0000
                })),
                Err(ZxError::BAD_STATE)
            );
            let root = PciRoot::new(0, PciIrqSwizzleLut::zeroed(), &bus);
            assert_eq!(bus.add_root_inner(root), Err(ZxError::BAD_STATE));
        }
    }

    #[test]
    fn a_second_root_claiming_the_same_bus_is_refused() {
        // Two roots on one bus would give every function of it two
        // configuration spaces and two sets of allocators.
        let mut bus = a_driver();
        let first = PciRoot::new(0, PciIrqSwizzleLut::zeroed(), &bus);
        assert_eq!(bus.add_root_inner(first), Ok(()));
        let again = PciRoot::new(0, PciIrqSwizzleLut::zeroed(), &bus);
        assert_eq!(bus.add_root_inner(again), Err(ZxError::ALREADY_EXISTS));
        let other = PciRoot::new(1, PciIrqSwizzleLut::zeroed(), &bus);
        assert_eq!(bus.add_root_inner(other), Ok(()));
    }

    // ---------- The cache of configuration spaces ----------

    #[test]
    fn the_same_function_asked_for_twice_gets_the_same_configuration_space() {
        // `PciConfig` is what every register access goes through, and a
        // device holds on to the one it was created with, so a second one for
        // the same function is a second view of the same registers.
        let mut bus = a_driver();
        assert_eq!(
            bus.set_address_translation_provider_inner(Arc::new(FixedProvider {
                vaddr: 0xFFFF_0000_0000
            })),
            Ok(())
        );
        let (first, phys) = bus.get_config(0, 3, 0).unwrap();
        let (again, phys_again) = bus.get_config(0, 3, 0).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(phys, phys_again);
        let (other, other_phys) = bus.get_config(0, 4, 0).unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        assert_ne!(phys, other_phys);
        assert_eq!(first.base, 0xFFFF_0000_0000 + (3 << 15));
        assert_eq!(other.base, 0xFFFF_0000_0000 + (4 << 15));
    }

    #[test]
    fn a_function_past_the_end_of_its_field_has_no_configuration_space() {
        let mut bus = a_driver();
        assert_eq!(
            bus.set_address_translation_provider_inner(Arc::new(FixedProvider {
                vaddr: 0xFFFF_0000_0000
            })),
            Ok(())
        );
        assert!(bus.get_config(0, 32, 0).is_none());
        assert!(bus.get_config(0, 0, 8).is_none());
        // And with no provider at all there is no configuration space either,
        // which used to be an `as_ref()?` followed by two `unwrap`s of the
        // same `Option`.
        let empty = a_driver();
        assert!(empty.get_config(0, 3, 0).is_none());
    }

    #[test]
    fn replacing_the_address_provider_forgets_the_old_ones_configuration_spaces() {
        // The cache is keyed on the virtual address the provider returned, so
        // an entry made by one provider answers for the next one that lands
        // on the same address -- and it describes a mapping that provider
        // owned. `zx_pci_init` may be called again after a failure further
        // down, and `add_root` failing with `ALREADY_EXISTS` is such a
        // failure.
        let mut bus = a_driver();
        assert_eq!(
            bus.set_address_translation_provider_inner(Arc::new(FixedProvider {
                vaddr: 0xFFFF_0000_0000
            })),
            Ok(())
        );
        let (stale, _) = bus.get_config(0, 3, 0).unwrap();
        assert_eq!(
            bus.set_address_translation_provider_inner(Arc::new(FixedProvider {
                vaddr: 0xFFFF_0000_0000
            })),
            Ok(())
        );
        let (fresh, _) = bus.get_config(0, 3, 0).unwrap();
        assert_eq!(stale.base, fresh.base);
        assert!(!Arc::ptr_eq(&stale, &fresh));
    }
}
