use super::{
    bus::PCIeBusDriver,
    caps::{
        PciCapAdvFeatures, PciCapPcie, PciCapability, PciCapabilityMsi, PciCapabilityStd,
        PciMsiBlock,
    },
    config::{
        PciConfig, PciReg16, PciReg32, PciReg8, PCIE_BASE_CONFIG_SIZE, PCIE_EXTENDED_CONFIG_SIZE,
    },
    constants::*,
    pci_init_args::PciIrqSwizzleLut,
};
use crate::{vm::PAGE_SIZE, ZxError, ZxResult};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use kernel_hal::interrupt;
use kernel_hal::sync::{Mutex, MutexGuard};
use numeric_enum_macro::numeric_enum;
use region_alloc::RegionAllocator;

numeric_enum! {
    #[repr(u8)]
    #[derive(PartialEq, Eq, Copy, Clone, Debug)]
    pub enum PcieDeviceType {
        Unknown = 0xFF,
        PcieEndpoint = 0x0,
        LegacyPcieEndpoint = 0x1,
        RcIntegratedEndpoint = 0x9,
        RcEventCollector = 0xA,
        // Type 1 config header types
        RcRootPort = 0x4,
        SwitchUpstreamPort = 0x5,
        SwitchDownstreamPort = 0x6,
        PcieToPciBridge = 0x7,
        PciToPcieBridge = 0x8,
    }
}

pub struct PcieUpstream {
    managed_bus_id: usize,
    inner: Mutex<PcieUpstreamInner>,
}

struct PcieUpstreamInner {
    weak_super: Weak<dyn IPciNode>,
    downstream: Box<[Option<Arc<dyn IPciNode>>]>,
}

impl PcieUpstream {
    pub fn create(managed_bus_id: usize) -> Arc<Self> {
        Arc::new(PcieUpstream {
            managed_bus_id,
            inner: Mutex::new(PcieUpstreamInner {
                weak_super: Weak::<PciRoot>::new(),
                downstream: {
                    let mut vec =
                        Vec::<Option<Arc<dyn IPciNode>>>::with_capacity(PCI_MAX_FUNCTIONS_PER_BUS);
                    vec.resize(PCI_MAX_FUNCTIONS_PER_BUS, None);
                    vec.into_boxed_slice()
                },
            }),
        })
    }

    pub fn scan_downstream(&self, driver: &PCIeBusDriver) {
        for dev_id in 0..PCI_MAX_DEVICES_PER_BUS {
            for func_id in 0..PCI_MAX_FUNCTIONS_PER_DEVICE {
                let cfg = driver.get_config(self.managed_bus_id, dev_id, func_id);
                if cfg.is_none() {
                    warn!("bus being scanned is outside ecam region!");
                    return;
                }
                let (cfg, _paddr) = cfg.unwrap();
                let vendor_id = cfg.read16(PciReg16::VendorId);
                let mut good_device = vendor_id as usize != PCIE_INVALID_VENDOR_ID;
                if good_device {
                    let device_id = cfg.read16(PciReg16::DeviceId);
                    info!(
                        "Found device {:#x?}:{:#x?} at {:#x?}:{:#x?}.{:#x?}",
                        vendor_id, device_id, self.managed_bus_id, dev_id, func_id
                    );
                    let ndx = dev_id * PCI_MAX_FUNCTIONS_PER_DEVICE + func_id;
                    let downstream_device = self.get_downstream(ndx);
                    match downstream_device {
                        Some(dev) => {
                            if let PciNodeType::Bridge = dev.node_type() {
                                dev.as_upstream().unwrap().scan_downstream(driver);
                            }
                        }
                        None => {
                            if self
                                .scan_device(cfg.as_ref(), dev_id, func_id, Some(vendor_id), driver)
                                .is_none()
                            {
                                info!(
                                    "failed to initialize device {:#x?}:{:#x?}.{:#x?}",
                                    self.managed_bus_id, dev_id, func_id
                                );
                                good_device = false;
                            }
                        }
                    }
                    info!("a device is discovered");
                }
                // At the point of function #0, if either there is no device, or cfg's
                // header indicates that it is not a multi-function device, just move on to
                // next device
                if func_id == 0
                    && (!good_device
                        || (cfg.read8(PciReg8::HeaderType) & PCI_HEADER_TYPE_MULTI_FN) != 0)
                {
                    break;
                }
            }
        }
    }

    pub fn allocate_downstream_bars(&self) {
        for dev_id in 0..PCI_MAX_DEVICES_PER_BUS {
            let dev = self.get_downstream(dev_id);
            if dev.is_none() {
                continue;
            }
            let dev = dev.unwrap();
            if dev.allocate_bars().is_err() {
                error!("Allocate Bar Fail");
                dev.disable();
            }
        }
    }

    fn scan_device(
        &self,
        cfg: &PciConfig,
        dev_id: usize,
        func_id: usize,
        vendor_id: Option<u16>,
        driver: &PCIeBusDriver,
    ) -> Option<Arc<dyn IPciNode>> {
        let vendor_id = vendor_id.unwrap_or_else(|| cfg.read16(PciReg16::VendorId));
        if vendor_id == PCIE_INVALID_VENDOR_ID as u16 {
            return None;
        }
        let header_type = cfg.read8(PciReg8::HeaderType) & 0x7f;
        let weak_super = self.inner.lock().weak_super.clone();
        if header_type == PCI_HEADER_TYPE_PCI_BRIDGE {
            let secondary_id = cfg.read8(PciReg8::SecondaryBusId);
            PciBridge::new(weak_super, dev_id, func_id, secondary_id as usize, driver)
                .map(|x| x as _)
        } else {
            PciDeviceNode::new(weak_super, dev_id, func_id, driver).map(|x| x as _)
        }
    }

    pub fn get_downstream(&self, index: usize) -> Option<Arc<dyn IPciNode>> {
        if index >= PCI_MAX_FUNCTIONS_PER_BUS {
            return None;
        }
        self.inner.lock().downstream[index].clone()
    }
    pub fn set_downstream(&self, index: usize, down: Option<Arc<dyn IPciNode>>) {
        self.inner.lock().downstream[index] = down;
    }

    pub fn set_super(&self, weak_super: Weak<dyn IPciNode>) {
        self.inner.lock().weak_super = weak_super;
    }
}

/// Struct used to fetch information about a configured base address register.
#[allow(missing_docs)]
#[derive(Default, Debug, Copy, Clone)]
pub struct PcieBarInfo {
    pub is_mmio: bool,
    pub is_64bit: bool,
    pub is_prefetchable: bool,
    pub first_bar_reg: usize,
    pub size: u64,
    pub bus_addr: u64,
    allocation: Option<(usize, usize)>,
}

/// What a BAR's low register says it is, before any probing.
///
/// Bit 0 picks memory or I/O space and bits 2:1 the memory width (PCI Local
/// Bus 3.0 §6.2.5.1). `0b01` in bits 2:1 is reserved: a device answering
/// that is malformed, and guessing a width would mean probing the wrong
/// number of registers.
fn bar_kind(index: usize, low: u32) -> ZxResult<(bool, bool)> {
    if (low & PCI_BAR_IO_TYPE_MASK) != PCI_BAR_IO_TYPE_MMIO {
        return Ok((false, false));
    }
    match low & PCI_BAR_MMIO_TYPE_MASK {
        PCI_BAR_MMIO_TYPE_32BIT => Ok((true, false)),
        PCI_BAR_MMIO_TYPE_64BIT => Ok((true, true)),
        _ => {
            warn!(
                "Unrecognized MMIO BAR type (BAR[{}] == {:#x?}) while fetching BAR info",
                index, low
            );
            Err(ZxError::BAD_STATE)
        }
    }
}

/// What a BAR's registers mean once the size probe has run.
///
/// `low`/`high` are the two registers as the device left them; `low_probe`/
/// `high_probe` are what they read back after writing ones into the address
/// bits, which is how a BAR reports its size. `high` and `high_probe` are
/// only read for a 64-bit BAR and are ignored otherwise.
///
/// Taking the upper register as an argument is the point: the loop that did
/// this inline read it into a `bar_val` shadowed inside the `if`, so by the
/// time the bus address was assembled `bar_val` was the *low* register
/// again and the upper half of every 64-bit BAR was the low register's own
/// type and prefetch bits. A BAR below 4 GiB, which is all QEMU hands out,
/// has an upper register of zero and hides it.
fn bar_info_from_probe(
    first_bar_reg: usize,
    is_mmio: bool,
    is_64bit: bool,
    low: u32,
    low_probe: u32,
    high: u32,
    high_probe: u32,
) -> PcieBarInfo {
    let addr_mask = if is_mmio {
        PCI_BAR_MMIO_ADDR_MASK
    } else {
        PCI_BAR_PIO_ADDR_MASK
    };
    let mut size_mask = !(low_probe & addr_mask) as u64;
    if is_64bit {
        size_mask |= (!high_probe as u64) << 32;
    }
    // An unimplemented BAR probes back as zeros, so the mask is all ones and
    // the size wraps to nothing. `wrapping_add` says that on purpose: a
    // 64-bit BAR claiming the whole address space used to overflow here.
    let size = if is_64bit {
        size_mask.wrapping_add(1)
    } else {
        (size_mask + 1) as u32 as u64
    };
    let size = if is_mmio {
        size
    } else {
        size & PCIE_PIO_ADDR_SPACE_MASK
    };
    let addr_lo = (low & addr_mask) as u64;
    PcieBarInfo {
        is_mmio,
        is_64bit,
        is_prefetchable: is_mmio && (low & PCI_BAR_MMIO_PREFETCH_MASK) != 0,
        first_bar_reg,
        size,
        bus_addr: if is_64bit {
            addr_lo | ((high as u64) << 32)
        } else {
            addr_lo
        },
        allocation: None,
    }
}

/// How much address space to reserve for a BAR of `size`.
///
/// A BAR may be as small as 16 bytes of memory or 4 bytes of I/O. An MMIO
/// BAR is handed to userspace as a VMO by `zx_pci_get_bar`, and a VMO is
/// whole pages, so a sub-page MMIO BAR has to be rounded up or the mapping
/// reaches into whatever sits next to it in the window. I/O ports have no
/// page table behind them and the whole space is 64 KiB, so an I/O BAR is
/// reserved at its natural size.
fn bar_alignment(size: u64, is_mmio: bool) -> usize {
    let size = size as usize;
    if size >= PAGE_SIZE || (PCIE_HAS_IO_ADDR_SPACE && !is_mmio) {
        size
    } else {
        PAGE_SIZE
    }
}

/// The per-vector mask register after masking or unmasking one vector.
///
/// One function, so the two callers cannot disagree about the direction of
/// the shift: `enable_irq` unmasked with `!(1 >> irq_id)`, which is `!1` for
/// vector 0 and `!0` — a no-op — for every vector after it. A device using
/// one MSI vector works; the second vector onwards stays masked forever and
/// its interrupts never arrive.
fn msi_mask_after(current: u32, irq: usize, mask: bool) -> u32 {
    // `1 << 32` is a panic in debug and a silent wrap round to bit 0 in
    // release, which masks somebody else's vector. The count is bounded to 32
    // where it is chosen, so nothing reaches this today; a shift that could
    // quietly move the wrong bit is not worth leaving to that.
    let Some(bit) = (irq < u32::BITS as usize).then(|| 1u32 << irq) else {
        warn!("pci: msi vector {} has no mask bit", irq);
        return current;
    };
    if mask {
        current | bit
    } else {
        current & !bit
    }
}

/// How many MSI vectors a request may be given, encoded the way the Multiple
/// Message Enable field wants it.
///
/// A device can be given a power-of-two number of vectors, at most 32 and at
/// most what its capability advertises as Multiple Message Capable (PCI
/// Local Bus 3.0 §6.8.1.3). Deciding that here is what keeps it out of the
/// two `assert!`s it used to reach inside the MSI setup, where a
/// `zx_pci_set_irq_mode(handle, MSI, 33)` from userspace panicked the
/// kernel.
fn msi_multi_message_encoding(requested_irqs: usize, device_max: u32) -> ZxResult<u16> {
    if !(1..=PCIE_MAX_MSI_IRQS).contains(&requested_irqs) || requested_irqs > device_max as usize {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(requested_irqs.next_power_of_two().trailing_zeros() as u16)
}

/// Bounds and alignment for a `zx_pci_config_read`/`zx_pci_config_write`.
///
/// The offset comes from userspace. A function advertising a PCI Express
/// capability has the 4 KiB extended configuration space (PCI Express 4.0
/// §7.2.2), where the extended capabilities live; one without it has only
/// the 256-byte PCI header (PCI Local Bus 3.0 §6.1). These two were the
/// wrong way round, so extended capabilities were unreachable on the
/// devices that have them and readable past the end on the devices that do
/// not. Accesses must be naturally aligned as well: the bus cannot express
/// a dword read at an odd offset, and over PIO it would straddle
/// `CONFIG_DATA`.
fn check_config_access(offset: usize, width: usize, has_pcie_cap: bool) -> ZxResult {
    if !matches!(width, 1 | 2 | 4) {
        return Err(ZxError::INVALID_ARGS);
    }
    if !offset.is_multiple_of(width) {
        return Err(ZxError::INVALID_ARGS);
    }
    let cfg_size = if has_pcie_cap {
        PCIE_EXTENDED_CONFIG_SIZE
    } else {
        PCIE_BASE_CONFIG_SIZE
    };
    if offset.checked_add(width).ok_or(ZxError::INVALID_ARGS)? > cfg_size {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(())
}

/// Struct for managing shared legacy IRQ handlers.
#[derive(Default)]
pub struct SharedLegacyIrqHandler {
    /// The IRQ id.
    pub irq_id: usize,
    device_handler: Mutex<Vec<Arc<PcieDevice>>>,
}

impl SharedLegacyIrqHandler {
    /// Create a new SharedLegacyIrqHandler.
    pub fn create(irq_id: usize) -> Option<Arc<SharedLegacyIrqHandler>> {
        info!("SharedLegacyIrqHandler created for {:#x?}", irq_id);
        interrupt::mask_irq(irq_id).unwrap();
        let handler = Arc::new(SharedLegacyIrqHandler {
            irq_id,
            device_handler: Mutex::new(Vec::new()),
        });
        let handler_copy = handler.clone();
        interrupt::register_irq_handler(irq_id, Arc::new(move || handler_copy.handle())).ok()?;
        Some(handler)
    }

    /// Handle the IRQ.
    pub fn handle(&self) {
        let device_handler = self.device_handler.lock();
        if device_handler.is_empty() {
            interrupt::mask_irq(self.irq_id).unwrap();
            return;
        }
        for dev in device_handler.iter() {
            let cfg = dev.config().unwrap();
            let _command = cfg.read16(PciReg16::Command);
            // let status = cfg.read16(PciReg16::Status);
            // if (command & PCIE_CFG_COMMAND_INT_DISABLE) != 0 {
            //     continue;
            // }
            let inner = dev.inner.lock();
            let handler_lock = inner.irq.handlers[0].handler.lock();
            let handler = if inner.irq.handlers.is_empty() {
                None
            } else {
                let handler = &inner.irq.handlers[0];
                if handler.get_masked() {
                    handler_lock.as_ref()
                } else {
                    None
                }
            };
            let ret = if let Some(h) = handler {
                let code = h();
                if (code & PCIE_IRQRET_MASK) != 0 {
                    inner.irq.handlers[0].set_masked(true);
                }
                code
            } else {
                PCIE_IRQRET_MASK
            };
            if (ret & PCIE_IRQRET_MASK) != 0 {
                cfg.write16(
                    PciReg16::Command,
                    cfg.read16(PciReg16::Command) | PCIE_CFG_COMMAND_INT_DISABLE,
                );
            }
        }
    }
    pub fn add_device(&self, device: Arc<PcieDevice>) {
        let cfg = device.config().unwrap();
        cfg.write16(
            PciReg16::Command,
            cfg.read16(PciReg16::Command) | PCIE_CFG_COMMAND_INT_DISABLE,
        );
        let mut device_handler = self.device_handler.lock();
        let is_first = device_handler.is_empty();
        device_handler.push(device);
        if is_first {
            interrupt::unmask_irq(self.irq_id).unwrap();
        }
    }
    pub fn remove_device(&self, device: Arc<PcieDevice>) {
        let cfg = device.config().unwrap();
        cfg.write16(
            PciReg16::Command,
            cfg.read16(PciReg16::Command) | PCIE_CFG_COMMAND_INT_DISABLE,
        );
        let mut device_handler = self.device_handler.lock();
        device_handler.retain(|h| Arc::ptr_eq(h, &device));
        if device_handler.is_empty() {
            interrupt::mask_irq(self.irq_id).unwrap();
        }
    }
}

numeric_enum! {
    #[repr(u32)]
    #[derive(Debug, PartialEq, Eq, Copy, Clone, Default)]
      /// Enumeration which defines the IRQ modes a PCIe device may be operating in.
      pub enum PcieIrqMode {
        /// All IRQs are disabled.  0 total IRQs are supported in this mode.
        #[default]
        Disabled = 0,
        ///    Devices may support up to 1 legacy IRQ in total.  Exclusive IRQ access
        ///    cannot be guaranteed (the IRQ may be shared with other devices)
        Legacy = 1,
        /// Devices may support up to 32 MSI IRQs in total.  IRQs may be allocated
        ///    exclusively, resources permitting.
        Msi = 2,
        ///   Devices may support up to 2048 MSI-X IRQs in total.  IRQs may be allocated
        ///   exclusively, resources permitting.
        MsiX = 3,
        #[allow(missing_docs)]
        Count = 4,
    }
}

/// Struct for managing IRQ handlers.
pub struct PcieIrqHandle {
    _handle: Option<Box<dyn Fn() + Send + Sync>>,
    _enabled: bool,
}

#[derive(Default)]
pub struct PcieLegacyIrqState {
    pub pin: u8,
    pub id: usize,
    pub shared_handler: Arc<SharedLegacyIrqHandler>,
    pub handlers: Vec<PcieIrqHandle>, // WARNING
    pub mode: PcieIrqMode,            // WANRING
    pub msi: Option<PciCapabilityMsi>,
    pub pcie: Option<PciCapPcie>,
}

pub struct PcieIrqState {
    pub legacy: PcieLegacyIrqState,
    pub mode: PcieIrqMode,
    pub handlers: Vec<Arc<PcieIrqHandlerState>>,
    pub registered_handler_count: usize,
}

impl Default for PcieIrqState {
    fn default() -> Self {
        Self {
            legacy: Default::default(),
            mode: PcieIrqMode::Disabled,
            handlers: Vec::default(),
            registered_handler_count: 0,
        }
    }
}

/// Class for managing shared legacy IRQ handlers.
#[derive(Default)]
pub struct PcieIrqHandlerState {
    irq_id: usize,
    masked: Mutex<bool>,
    enabled: Mutex<bool>,
    handler: Mutex<Option<Box<dyn Fn() -> u32 + Send + Sync>>>,
}

impl PcieIrqHandlerState {
    pub fn set_masked(&self, masked: bool) {
        *self.masked.lock() = masked;
    }
    pub fn get_masked(&self) -> bool {
        *self.masked.lock()
    }
    pub fn set_handler(&self, h: Option<Box<dyn Fn() -> u32 + Send + Sync>>) {
        *self.handler.lock() = h;
    }
    pub fn has_handler(&self) -> bool {
        self.handler.lock().is_some()
    }
    pub fn enable(&self, e: bool) {
        *self.enabled.lock() = e;
    }
}

pub struct PcieDevice {
    pub bus_id: usize,
    pub dev_id: usize,
    pub func_id: usize,
    // pub is_bridge: bool,
    pub bar_count: usize,
    cfg: Option<Arc<PciConfig>>,
    _cfg_phys: usize,
    dev_lock: Mutex<()>,
    command_lock: Mutex<()>,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_id: u8,
    pub subclass_id: u8,
    pub prog_if: u8,
    pub rev_id: u8,
    pub inner: Mutex<PcieDeviceInner>,
}

pub struct PcieDeviceInner {
    pub irq: PcieIrqState,
    pub bars: [PcieBarInfo; 6],
    pub caps: Vec<PciCapability>,
    pub plugged_in: bool,
    pub upstream: Weak<dyn IPciNode>,
    pub weak_super: Weak<dyn IPciNode>,
    pub disabled: bool,
}

impl Default for PcieDeviceInner {
    fn default() -> Self {
        PcieDeviceInner {
            irq: Default::default(),
            bars: Default::default(),
            caps: Default::default(),
            plugged_in: false,
            upstream: Weak::<PciRoot>::new(),
            weak_super: Weak::<PciRoot>::new(),
            disabled: false,
        }
    }
}

impl PcieDeviceInner {
    pub fn arc_self(&self) -> Arc<PcieDevice> {
        self.weak_super.upgrade().unwrap().device()
    }
    pub fn msi(&self) -> Option<(&PciCapabilityStd, &PciCapabilityMsi)> {
        for c in self.caps.iter() {
            if let PciCapability::Msi(std, msi) = c {
                if std.is_valid() {
                    return Some((std, msi));
                }
            }
        }
        None
    }
    pub fn pcie(&self) -> Option<(&PciCapabilityStd, &PciCapPcie)> {
        for c in self.caps.iter() {
            if let PciCapability::Pcie(std, pcie) = c {
                if std.is_valid() {
                    return Some((std, pcie));
                }
            }
        }
        None
    }
}

impl PcieDevice {
    pub fn create(
        upstream: Weak<dyn IPciNode>,
        dev_id: usize,
        func_id: usize,
        driver: &PCIeBusDriver,
    ) -> Option<Arc<Self>> {
        let ups = upstream.upgrade().unwrap().as_upstream()?;
        let (cfg, paddr) = driver.get_config(ups.managed_bus_id, dev_id, func_id)?;
        let inst = Arc::new(PcieDevice {
            bus_id: ups.managed_bus_id,
            dev_id,
            func_id,
            // is_bridge: false,
            bar_count: 6, // PCIE BAR regs per device
            cfg: Some(cfg.clone()),
            _cfg_phys: paddr,
            dev_lock: Mutex::default(),
            command_lock: Mutex::default(),
            vendor_id: cfg.read16(PciReg16::VendorId),
            device_id: cfg.read16(PciReg16::DeviceId),
            class_id: cfg.read8(PciReg8::BaseClass),
            subclass_id: cfg.read8(PciReg8::SubClass),
            prog_if: cfg.read8(PciReg8::ProgramInterface),
            rev_id: cfg.read8(PciReg8::RevisionId),
            inner: Default::default(),
        });
        inst.init(upstream, driver).unwrap();
        Some(inst)
    }
    fn init(&self, upstream: Weak<dyn IPciNode>, driver: &PCIeBusDriver) -> ZxResult {
        info!("init PciDevice");
        self.init_probe_bars()?;
        self.init_capabilities()?;
        self.init_legacy_irq(&upstream, driver)?;
        let mut inner = self.inner.lock();
        inner.plugged_in = true;
        // let sup = inner.weak_super.upgrade().unwrap().clone();
        drop(inner);
        // driver.link_device_to_upstream(sup, upstream);
        Ok(())
    }

    fn init_probe_bars(&self) -> ZxResult {
        info!("init PciDevice probe bars");
        // probe bars
        let mut i = 0;
        let cfg = self.cfg.as_ref().unwrap();
        while i < self.bar_count {
            let low = cfg.read_bar(i);
            let (is_mmio, is_64bit) = bar_kind(i, low)?;
            if is_64bit && i + 1 >= self.bar_count {
                warn!(
                    "Illegal 64-bit MMIO BAR position {}/{} while fetching BAR info",
                    i, self.bar_count
                );
                return Err(ZxError::BAD_STATE);
            }
            // Disable either MMIO or PIO (depending on the BAR type) access while we perform the probe.
            // let _cmd_lock = self.command_lock.lock(); lock is useless during init
            let backup = cfg.read16(PciReg16::Command);
            cfg.write16(
                PciReg16::Command,
                backup
                    & !(if is_mmio {
                        PCI_COMMAND_MEM_EN
                    } else {
                        PCI_COMMAND_IO_EN
                    }),
            );
            // Figure out the size of this BAR region by writing 1's to the address bits
            let addr_mask = if is_mmio {
                PCI_BAR_MMIO_ADDR_MASK
            } else {
                PCI_BAR_PIO_ADDR_MASK
            };
            cfg.write_bar(i, low | addr_mask);
            let low_probe = cfg.read_bar(i);
            cfg.write_bar(i, low);
            let (high, high_probe) = if is_64bit {
                let high = cfg.read_bar(i + 1);
                cfg.write_bar(i + 1, 0xFFFF_FFFF);
                let high_probe = cfg.read_bar(i + 1);
                cfg.write_bar(i + 1, high);
                (high, high_probe)
            } else {
                (0, 0)
            };
            cfg.write16(PciReg16::Command, backup);
            let bar_info =
                bar_info_from_probe(i, is_mmio, is_64bit, low, low_probe, high, high_probe);
            let bar_info_size = bar_info.size;
            self.inner.lock().bars[i] = bar_info;
            i += 1;
            if is_64bit && bar_info_size > 0 {
                i += 1;
                if i > self.bar_count {
                    return Err(ZxError::BAD_STATE);
                }
            }
        }
        Ok(())
    }
    fn init_capabilities(&self) -> ZxResult {
        info!("init PciDevice caps");
        let cfg = self.cfg.as_ref().unwrap();
        let mut cap_offset = cfg.read8(PciReg8::CapabilitiesPtr);
        let mut found_num = 0;
        while cap_offset != 0 && found_num < (256 - 64) / 4 {
            if cap_offset == 0xff || !(64..=252).contains(&cap_offset) {
                return Err(ZxError::INVALID_ARGS);
            }
            let id = cfg.read8_(cap_offset as usize);
            let std = PciCapabilityStd::create(cap_offset as u16, id);
            let mut inner = self.inner.lock();
            let cap = match id {
                0x5 => PciCapability::Msi(
                    std,
                    PciCapabilityMsi::create(cfg.as_ref(), cap_offset as usize, id),
                ),
                0x10 => PciCapability::Pcie(
                    std,
                    PciCapPcie::create(cfg.as_ref(), cap_offset as u16, id),
                ),
                0x13 => PciCapability::AdvFeatures(
                    std,
                    PciCapAdvFeatures::create(cfg.as_ref(), cap_offset as u16, id),
                ),
                _ => PciCapability::Std(std),
            };
            inner.caps.push(cap);
            cap_offset = cfg.read8_(cap_offset as usize + 1) & 0xFC;
            found_num += 1;
        }
        Ok(())
    }
    fn init_legacy_irq(&self, upstream: &Weak<dyn IPciNode>, driver: &PCIeBusDriver) -> ZxResult {
        info!("init PciDevice legacy irq");
        self.modify_cmd(0, 1 << 10);
        let cfg = self.cfg.as_ref().unwrap();
        let pin = cfg.read8(PciReg8::InterruptPin);
        let mut inner = self.inner.lock();
        inner.irq.legacy.pin = pin;
        if pin != 0 {
            inner.irq.legacy.id = self.map_pin_to_irq_locked(upstream, pin)?;
            inner.irq.legacy.shared_handler =
                driver.find_legacy_irq_handler(inner.irq.legacy.id)?;
        }
        Ok(())
    }
    fn map_pin_to_irq_locked(
        &self,
        // _lock: &MutexGuard<()>, lock is useless during init
        upstream: &Weak<dyn IPciNode>,
        mut pin: u8,
    ) -> ZxResult<usize> {
        // Don't use self.inner.lock() in this function !!!
        if pin == 0 || pin > 4 {
            return Err(ZxError::BAD_STATE);
        }
        pin -= 1;
        let mut dev_id = self.dev_id;
        let mut func_id = self.func_id;
        let mut upstream = upstream.clone();
        while let Some(up) = upstream.upgrade() {
            if let PciNodeType::Bridge = up.node_type() {
                let bdev = up.device();
                match bdev.pcie_device_type() {
                    PcieDeviceType::Unknown
                    | PcieDeviceType::SwitchUpstreamPort
                    | PcieDeviceType::PcieToPciBridge
                    | PcieDeviceType::PciToPcieBridge => {
                        pin = (pin + dev_id as u8) % 4;
                    }
                    _ => (),
                }
                let dev = up.device();
                dev_id = dev.dev_id;
                func_id = dev.func_id;
                upstream = dev.upstream();
            } else {
                break;
            }
        }
        let upstream = upstream.upgrade();
        if let Some(up_ptr) = upstream {
            if let Some(up) = up_ptr.as_root() {
                return up.swizzle(dev_id, func_id, pin as usize);
            }
        }
        Err(ZxError::BAD_STATE)
    }

    pub fn allocate_bars(&self) -> ZxResult {
        let mut inner = self.inner.lock();
        assert!(inner.plugged_in);
        for i in 0..self.bar_count {
            let bar_info = &inner.bars[i];
            if bar_info.size == 0 || bar_info.allocation.is_some() {
                continue;
            }
            let upstream = inner.upstream.upgrade().ok_or(ZxError::UNAVAILABLE)?;
            let bar_info = &mut inner.bars[i];
            if bar_info.bus_addr != 0 {
                let allocator =
                    if upstream.node_type() == PciNodeType::Bridge && bar_info.is_prefetchable {
                        Some(upstream.pf_mmio_regions())
                    } else if bar_info.is_mmio {
                        let inclusive_end = bar_info.bus_addr + bar_info.size - 1;
                        if inclusive_end <= u32::MAX.into() {
                            Some(upstream.mmio_lo_regions())
                        } else if bar_info.bus_addr > u32::MAX.into() {
                            Some(upstream.mmio_hi_regions())
                        } else {
                            None
                        }
                    } else {
                        Some(upstream.pio_regions())
                    };
                if let Some(allocator) = allocator {
                    let base: usize = bar_info.bus_addr as _;
                    let size: usize = bar_info.size as _;
                    if allocator.lock().allocate_by_addr(base, size) {
                        bar_info.allocation = Some((base, size));
                        continue;
                    }
                }
                error!("Failed to preserve device window");
                bar_info.bus_addr = 0;
            }
            warn!("No bar addr for {}...", i);
            self.assign_cmd(PCIE_CFG_COMMAND_INT_DISABLE);
            let allocator = if bar_info.is_mmio {
                if bar_info.is_64bit {
                    upstream.mmio_hi_regions()
                } else {
                    upstream.mmio_lo_regions()
                }
            } else {
                upstream.pio_regions()
            };
            let addr_mask: u32 = if bar_info.is_mmio {
                PCI_BAR_MMIO_ADDR_MASK
            } else {
                PCI_BAR_PIO_ADDR_MASK
            };
            let align_size = bar_alignment(bar_info.size, bar_info.is_mmio);
            let alloc1 = allocator.lock().allocate_by_size(align_size, align_size);
            match alloc1 {
                Some(a) => bar_info.allocation = Some(a),
                None => {
                    if bar_info.is_mmio && bar_info.is_64bit {
                        bar_info.allocation = upstream
                            .mmio_lo_regions()
                            .lock()
                            .allocate_by_size(align_size, align_size);
                    }
                    if bar_info.allocation.is_none() {
                        return Err(ZxError::NOT_FOUND);
                    }
                }
            }
            let bar_reg = bar_info.first_bar_reg;
            bar_info.bus_addr = bar_info.allocation.as_ref().unwrap().0 as u64;
            let cfg = self.cfg.as_ref().unwrap();
            let bar_val = cfg.read_bar(bar_reg) & !addr_mask;
            cfg.write_bar(bar_reg, (bar_info.bus_addr & 0xFFFF_FFFF) as u32 | bar_val);
            if bar_info.is_64bit {
                cfg.write_bar(bar_reg + 1, (bar_info.bus_addr >> 32) as u32);
            }
        }
        Ok(())
    }

    fn assign_cmd(&self, value: u16) {
        self.modify_cmd(0xffff, value)
    }

    fn modify_cmd(&self, clr: u16, set: u16) {
        let _cmd_lock = self.command_lock.lock();
        let cfg = self.cfg.as_ref().unwrap();
        let oldval = cfg.read16(PciReg16::Command);
        cfg.write16(PciReg16::Command, oldval & !clr | set)
    }
    fn modify_cmd_adv(&self, clr: u16, set: u16) -> ZxResult {
        if !self.inner.lock().plugged_in {
            return Err(ZxError::UNAVAILABLE);
        }
        let _guard = self.dev_lock.lock();
        self.modify_cmd(clr & !(1 << 10), set & !(1 << 10));
        Ok(())
    }
    pub fn upstream(&self) -> Weak<dyn IPciNode> {
        self.inner.lock().upstream.clone()
    }
    pub fn dev_id(&self) -> usize {
        self.dev_id
    }
    pub fn func_id(&self) -> usize {
        self.func_id
    }
    pub fn set_upstream(&self, up: Weak<dyn IPciNode>) {
        self.inner.lock().upstream = up;
    }
    pub fn set_super(&self, sup: Weak<dyn IPciNode>) {
        self.inner.lock().weak_super = sup;
    }
    fn pcie_device_type(&self) -> PcieDeviceType {
        for cap in self.inner.lock().caps.iter() {
            if let PciCapability::Pcie(_std, pcie) = cap {
                return pcie.dev_type;
            }
        }
        PcieDeviceType::Unknown
    }
    pub fn config(&self) -> Option<Arc<PciConfig>> {
        self.cfg.clone()
    }

    /// Enable MMIO.
    pub fn enable_mmio(&self, enable: bool) -> ZxResult {
        self.modify_cmd_adv(
            if enable { 0 } else { PCI_COMMAND_MEM_EN },
            if enable { PCI_COMMAND_MEM_EN } else { 0 },
        )
    }

    /// Enable PIO.
    pub fn enable_pio(&self, enable: bool) -> ZxResult {
        self.modify_cmd_adv(
            if enable { 0 } else { PCI_COMMAND_IO_EN },
            if enable { PCI_COMMAND_IO_EN } else { 0 },
        )
    }

    /// Enable bus mastering.
    pub fn enable_master(&self, enable: bool) -> ZxResult {
        self.modify_cmd_adv(
            if enable { 0 } else { PCI_COMMAND_BUS_MASTER_EN },
            if enable { PCI_COMMAND_BUS_MASTER_EN } else { 0 },
        )?;
        if let Some(up) = self.upstream().upgrade() {
            up.enable_bus_master(enable)
        } else {
            Ok(())
        }
    }

    /// Enable or disable one of this device's IRQ vectors.
    ///
    /// Every one of the five checks below used to be an `assert!`, and the
    /// caller is `zx_interrupt_ack`/`zx_interrupt_destroy` by way of
    /// [`PciInterrupt`](crate::dev::interrupt::Interrupt)'s mask and unmask.
    /// A device whose IRQ mode went back to `Disabled` under a live interrupt
    /// object has an empty vector table, so acking that interrupt afterwards
    /// panicked the kernel from userspace, in three syscalls, all of them
    /// documented ones.
    pub fn enable_irq(&self, irq_id: usize, enable: bool) -> ZxResult {
        let _dev_lcok = self.dev_lock.lock();
        let inner = self.inner.lock();
        if !inner.plugged_in {
            return Err(ZxError::BAD_STATE);
        }
        // Cloned out of the table so the borrow ends here: `msi()` below
        // borrows `inner` again.
        let handler = inner
            .irq
            .handlers
            .get(irq_id)
            .ok_or(ZxError::INVALID_ARGS)?
            .clone();
        if enable && (inner.disabled || !handler.has_handler()) {
            return Err(ZxError::BAD_STATE);
        }
        match inner.irq.mode {
            PcieIrqMode::Legacy => {
                if enable {
                    self.modify_cmd(PCIE_CFG_COMMAND_INT_DISABLE, 0);
                } else {
                    self.modify_cmd(0, PCIE_CFG_COMMAND_INT_DISABLE);
                }
            }
            PcieIrqMode::Msi => {
                let (_std, msi) = inner.msi().ok_or(ZxError::BAD_STATE)?;
                if msi.has_pvm {
                    let cfg = self.cfg.as_ref().ok_or(ZxError::BAD_STATE)?;
                    let val = cfg.read32_(msi.mask_bits_offset);
                    cfg.write32_(msi.mask_bits_offset, msi_mask_after(val, irq_id, !enable));
                }
                // x86_64 does not support msi masking
                #[cfg(not(target_arch = "x86_64"))]
                error!("If the platform supports msi masking, do so");
            }
            // A mode with no way to mask a vector: `Disabled`, which every
            // device starts in. It was `unreachable!()`, on a value the table
            // above is supposed to agree with and nothing enforces.
            _ => return Err(ZxError::BAD_STATE),
        }
        handler.enable(enable);
        Ok(())
    }

    /// Install the handler for one of this device's IRQ vectors.
    ///
    /// `zx_pci_map_interrupt` lands here, and the object it goes through lets
    /// any vector number below **ten** past -- a constant this kernel made up,
    /// with no relation to the count `zx_pci_set_irq_mode` actually allocated.
    /// So both of these were a kernel panic reachable from userspace: asking
    /// for a vector the device was never given, and asking before setting an
    /// IRQ mode at all, which is a device whose vector table is still empty.
    /// The second needs one syscall, on a handle any driver process holds.
    pub fn register_irq_handle(
        &self,
        irq_id: usize,
        handle: Box<dyn Fn() -> u32 + Send + Sync>,
    ) -> ZxResult {
        let _dev_lcok = self.dev_lock.lock();
        let inner = self.inner.lock();
        if inner.disabled || !inner.plugged_in || inner.irq.mode == PcieIrqMode::Disabled {
            return Err(ZxError::BAD_STATE);
        }
        inner
            .irq
            .handlers
            .get(irq_id)
            .ok_or(ZxError::INVALID_ARGS)?
            .set_handler(Some(handle));
        Ok(())
    }

    /// Remove the handler for one of this device's IRQ vectors.
    ///
    /// Total where [`Self::register_irq_handle`] is strict, and for a reason:
    /// this runs from the interrupt object's own teardown, including its
    /// `Drop`, and by then the vector table may already have been emptied by
    /// a `zx_pci_set_irq_mode` or the device unplugged. "No handler on that
    /// vector" is what the caller is asking for, so a vector that is not
    /// there is the answer, not four `assert!`s.
    pub fn unregister_irq_handle(&self, irq_id: usize) {
        let _dev_lcok = self.dev_lock.lock();
        let inner = self.inner.lock();
        if let Some(handler) = inner.irq.handlers.get(irq_id) {
            handler.set_handler(None);
        }
    }

    /// Get PcieBarInfo.
    pub fn get_bar(&self, bar_num: usize) -> Option<PcieBarInfo> {
        if bar_num >= self.bar_count {
            None
        } else {
            Some(self.inner.lock().bars[bar_num])
        }
    }

    /// Gets info about the capabilities of a PCI device's IRQ modes.
    pub fn get_irq_mode_capabilities(&self, mode: PcieIrqMode) -> ZxResult<PcieIrqModeCaps> {
        let inner = self.inner.lock();
        if inner.plugged_in {
            match mode {
                PcieIrqMode::Disabled => Ok(PcieIrqModeCaps::default()),
                PcieIrqMode::Legacy => {
                    if inner.irq.legacy.pin != 0 {
                        Ok(PcieIrqModeCaps {
                            max_irqs: 1,
                            per_vector_masking_supported: true,
                        })
                    } else {
                        warn!("get_irq_mode_capabilities: Legacy pin == 0");
                        Err(ZxError::NOT_SUPPORTED)
                    }
                }
                PcieIrqMode::Msi => {
                    if let Some((_std, msi)) = inner.msi() {
                        return Ok(PcieIrqModeCaps {
                            max_irqs: msi.max_irq,
                            per_vector_masking_supported: msi.has_pvm,
                        });
                    }
                    warn!("get_irq_mode_capabilities: MSI not found");
                    Err(ZxError::NOT_SUPPORTED)
                }
                PcieIrqMode::MsiX => Err(ZxError::NOT_SUPPORTED),
                _ => Err(ZxError::INVALID_ARGS),
            }
        } else {
            Err(ZxError::BAD_STATE)
        }
    }
    fn mask_legacy_irq(&self, inner: &MutexGuard<PcieDeviceInner>, mask: bool) -> ZxResult {
        if inner.irq.handlers.is_empty() {
            return Err(ZxError::INVALID_ARGS);
        }
        if mask {
            self.modify_cmd(0, PCIE_CFG_COMMAND_INT_DISABLE);
        } else {
            self.modify_cmd(PCIE_CFG_COMMAND_INT_DISABLE, 0);
        }
        inner.irq.handlers[0].set_masked(mask);
        Ok(())
    }
    fn reset_irq_bookkeeping(&self, inner: &mut MutexGuard<PcieDeviceInner>) {
        inner.irq.handlers.clear();
        inner.irq.mode = PcieIrqMode::Disabled;
        inner.irq.registered_handler_count = 0;
    }
    fn allocate_irq_handler(
        &self,
        inner: &mut MutexGuard<PcieDeviceInner>,
        requested_irqs: usize,
        masked: bool,
    ) {
        assert!(inner.irq.handlers.is_empty());
        for i in 0..requested_irqs {
            inner.irq.handlers.push(Arc::new(PcieIrqHandlerState {
                irq_id: i,
                enabled: Mutex::new(false),
                masked: Mutex::new(masked),
                handler: Mutex::new(None),
            }))
        }
    }
    fn enter_msi_irq_mode(
        &self,
        inner: &mut MutexGuard<PcieDeviceInner>,
        requested_irqs: usize,
    ) -> ZxResult {
        let (_std, msi) = inner.msi().ok_or(ZxError::NOT_SUPPORTED)?;
        let initially_masked = if msi.has_pvm {
            self.cfg
                .as_ref()
                .unwrap()
                .write32_(msi.mask_bits_offset, u32::MAX);
            true
        } else {
            false
        };
        match PciMsiBlock::allocate(requested_irqs) {
            Ok(block) => *msi.irq_block.lock() = block,
            Err(ex) => {
                self.leave_msi_irq_mode(inner);
                return Err(ex);
            }
        };
        self.allocate_irq_handler(inner, requested_irqs, initially_masked);
        inner.irq.mode = PcieIrqMode::Msi;
        let (_std, msi) = inner.msi().ok_or(ZxError::NOT_SUPPORTED)?;
        let block = msi.irq_block.lock();
        let (target_addr, target_data) = (block.target_addr, block.target_data);
        self.set_msi_target(inner, target_addr, target_data);
        self.set_msi_multi_message_enb(inner, requested_irqs)?;
        for (i, e) in inner.irq.handlers.iter().enumerate() {
            let arc_self = inner.arc_self();
            let handler_copy = e.clone();
            block.register_handler(
                i,
                Box::new(move || Self::msi_irq_handler(arc_self.clone(), handler_copy.clone())),
            )?;
        }
        self.set_msi_enb(inner, true);
        Ok(())
    }
    fn leave_msi_irq_mode(&self, inner: &mut MutexGuard<PcieDeviceInner>) {
        self.set_msi_target(inner, 0x0, 0x0);
        // free msi blocks
        {
            let (_std, msi) = inner.msi().unwrap();
            let block = msi.irq_block.lock();
            if block.allocated {
                for i in 0..block.num_irq {
                    // Unhooking a vector on the way out: nothing to unwind to
                    // if the interrupt layer has already forgotten the block.
                    if let Err(err) = block.register_handler(i, Box::new(|| {})) {
                        warn!("could not unhook MSI vector {}: {:?}", i, err);
                    }
                }
                block.free();
            }
        }
        self.reset_irq_bookkeeping(inner);
    }
    fn set_msi_target(
        &self,
        inner: &MutexGuard<PcieDeviceInner>,
        target_addr: u64,
        target_data: u32,
    ) {
        let (std, msi) = inner.msi().unwrap();
        assert!(msi.is_64bit || (target_addr >> 32) == 0);
        assert!((target_data >> 16) == 0);
        self.set_msi_enb(inner, false);
        self.mask_all_msi_vectors(inner);
        let cfg = self.cfg.as_ref().unwrap();
        let addr_reg = std.base + 0x4;
        let addr_reg_upper = std.base + 0x8;
        let data_reg = msi.data_offset;
        cfg.write32_(addr_reg as usize, target_addr as u32);
        if msi.is_64bit {
            cfg.write32_(addr_reg_upper as usize, (target_addr >> 32) as u32);
        }
        cfg.write16_(data_reg, target_data as u16);
    }
    fn set_msi_multi_message_enb(
        &self,
        inner: &MutexGuard<PcieDeviceInner>,
        requested_irqs: usize,
    ) -> ZxResult {
        let cfg = self.cfg.as_ref().unwrap();
        let (std, msi) = inner.msi().ok_or(ZxError::NOT_SUPPORTED)?;
        let log2 = msi_multi_message_encoding(requested_irqs, msi.max_irq)?;
        let ctrl_addr = std.base as usize + PciCapabilityMsi::ctrl_offset();
        let mut val = cfg.read16_(ctrl_addr);
        val = (val & !0x70) | ((log2 & 0x7) << 4);
        cfg.write16_(ctrl_addr, val);
        Ok(())
    }
    fn set_msi_enb(&self, inner: &MutexGuard<PcieDeviceInner>, enable: bool) {
        let cfg = self.cfg.as_ref().unwrap();
        let (std, _msi) = inner.msi().unwrap();
        let ctrl_addr = std.base as usize + PciCapabilityMsi::ctrl_offset();
        let val = cfg.read16_(ctrl_addr);
        cfg.write16_(ctrl_addr, (val & !0x1) | (enable as u16));
    }
    fn mask_all_msi_vectors(&self, inner: &MutexGuard<PcieDeviceInner>) {
        for i in 0..inner.irq.handlers.len() {
            self.mask_msi_irq(inner, i, true);
        }
        // just to be careful
        let cfg = self.cfg.as_ref().unwrap();
        let (_std, msi) = inner.msi().unwrap();
        if msi.has_pvm {
            cfg.write32_(msi.mask_bits_offset, u32::MAX);
        }
    }
    /// Mask or unmask one MSI vector, answering whether it was already masked.
    ///
    /// Runs from the MSI interrupt handler, i.e. from IRQ context, holding a
    /// vector number captured when the handler was registered. Nothing keeps
    /// that vector alive: `zx_pci_set_irq_mode` empties the table while the
    /// block the device writes into is still live. So the three `assert!`s
    /// and `unwrap`s this used to open with were a kernel panic taken from an
    /// interrupt, which is the worst place to take one.
    fn mask_msi_irq(&self, inner: &MutexGuard<PcieDeviceInner>, irq: usize, mask: bool) -> bool {
        let Some(handler) = inner.irq.handlers.get(irq).cloned() else {
            return false;
        };
        let Some(cfg) = self.cfg.as_ref() else {
            return false;
        };
        let Some((_std, msi)) = inner.msi() else {
            return false;
        };
        if mask && !msi.has_pvm {
            return false;
        }
        if msi.has_pvm {
            let addr = msi.mask_bits_offset;
            let val = cfg.read32_(addr);
            cfg.write32_(addr, msi_mask_after(val, irq, mask));
        }
        // Per vector, not vector 0's: `msi_irq_handler` reads this back to
        // decide whether the interrupt it is holding is one it already
        // masked, so sharing one flag between vectors made every vector
        // after the first answer for vector 0.
        let ret = handler.get_masked();
        handler.set_masked(mask);
        ret
    }
    fn msi_irq_handler(dev: Arc<PcieDevice>, state: Arc<PcieIrqHandlerState>) {
        // Perhaps dead lock?
        let inner = dev.inner.lock();
        let (_std, msi) = inner.msi().unwrap();
        if msi.has_pvm && dev.mask_msi_irq(&inner, state.irq_id, true) {
            return;
        }
        if let Some(h) = &*state.handler.lock() {
            let ret = h();
            if (ret & PCIE_IRQRET_MASK) == 0 {
                dev.mask_msi_irq(&inner, state.irq_id, false);
            }
        }
    }

    /// Set IRQ mode.
    pub fn set_irq_mode(&self, mode: PcieIrqMode, requested_irqs: usize) -> ZxResult {
        let mut inner = self.inner.lock();
        let mut requested_irqs = requested_irqs;
        if let PcieIrqMode::Disabled = mode {
            requested_irqs = 0;
        } else if !inner.plugged_in {
            return Err(ZxError::BAD_STATE);
        } else if requested_irqs < 1 {
            return Err(ZxError::INVALID_ARGS);
        }
        // Settle the whole request before touching anything. Everything the
        // request can be refused for is known here, and the dismantling of
        // the device's current mode comes next and is not undone: a refusal
        // taken after it leaves the device with no vectors, no handlers and
        // every interrupt object over it pointing at nothing, which is not
        // what an error return means.
        //
        // Every one of these used to be found on the far side of it. A count
        // the hardware cannot express was an `assert!` halfway through the
        // MSI setup; `MsiX` is a mode this driver does not implement; and
        // `Count` is a variant of the enum the syscall converts into, so
        // `zx_pci_set_irq_mode(handle, 4, 1)` is a one-syscall way for a
        // driver process to disarm its own device and be told it did
        // nothing.
        match mode {
            PcieIrqMode::Disabled => {}
            PcieIrqMode::Legacy => {
                if inner.irq.legacy.pin == 0 || requested_irqs > 1 {
                    return Err(ZxError::NOT_SUPPORTED);
                }
            }
            PcieIrqMode::Msi => {
                let device_max = inner
                    .msi()
                    .map(|(_std, msi)| msi.max_irq)
                    .ok_or(ZxError::NOT_SUPPORTED)?;
                msi_multi_message_encoding(requested_irqs, device_max)?;
            }
            PcieIrqMode::MsiX => return Err(ZxError::NOT_SUPPORTED),
            _ => return Err(ZxError::INVALID_ARGS),
        }
        match inner.irq.mode {
            PcieIrqMode::Legacy => {
                self.mask_legacy_irq(&inner, true)?;
                inner
                    .irq
                    .legacy
                    .shared_handler
                    .remove_device(inner.arc_self());
                self.reset_irq_bookkeeping(&mut inner);
            }
            PcieIrqMode::Msi => {
                self.leave_msi_irq_mode(&mut inner);
            }
            PcieIrqMode::MsiX => {
                return Err(ZxError::NOT_SUPPORTED);
            }
            PcieIrqMode::Disabled => {}
            _ => {
                return Err(ZxError::INVALID_ARGS);
            }
        }
        match mode {
            PcieIrqMode::Disabled => Ok(()),
            PcieIrqMode::Legacy => {
                if inner.irq.legacy.pin == 0 || requested_irqs > 1 {
                    return Err(ZxError::NOT_SUPPORTED);
                }
                self.modify_cmd(0, PCIE_CFG_COMMAND_INT_DISABLE);
                self.allocate_irq_handler(&mut inner, requested_irqs, true);
                inner.irq.mode = PcieIrqMode::Legacy;
                inner.irq.legacy.shared_handler.add_device(inner.arc_self());
                Ok(())
            }
            PcieIrqMode::Msi => self.enter_msi_irq_mode(&mut inner, requested_irqs),
            PcieIrqMode::MsiX => Err(ZxError::NOT_SUPPORTED),
            _ => Err(ZxError::INVALID_ARGS),
        }
    }

    /// Read the device's config.
    pub fn config_read(&self, offset: usize, width: usize) -> ZxResult<u32> {
        let inner = self.inner.lock();
        check_config_access(offset, width, inner.pcie().is_some())?;
        let cfg = self.cfg.as_ref().unwrap();
        match width {
            1 => Ok(cfg.read8_(offset) as u32),
            2 => Ok(cfg.read16_(offset) as u32),
            4 => Ok(cfg.read32_(offset)),
            _ => Err(ZxError::INVALID_ARGS),
        }
    }

    /// Write the device's config.
    pub fn config_write(&self, offset: usize, width: usize, val: u32) -> ZxResult {
        let inner = self.inner.lock();
        check_config_access(offset, width, inner.pcie().is_some())?;
        let cfg = self.cfg.as_ref().unwrap();
        match width {
            1 => cfg.write8_(offset, val as u8),
            2 => cfg.write16_(offset, val as u16),
            4 => cfg.write32_(offset, val),
            _ => return Err(ZxError::INVALID_ARGS),
        };
        Ok(())
    }
}

#[derive(PartialEq, Eq)]
pub enum PciNodeType {
    Root,
    Bridge,
    Device,
}

pub trait IPciNode: Send + Sync {
    fn node_type(&self) -> PciNodeType;
    fn device(&self) -> Arc<PcieDevice>;
    fn as_upstream(&self) -> Option<Arc<PcieUpstream>>;
    fn as_root(&self) -> Option<&PciRoot> {
        None
    }
    fn allocate_bars(&self) -> ZxResult {
        unimplemented!("IPciNode.allocate_bars")
    }
    fn disable(&self) {
        unimplemented!("IPciNode.disable");
    }
    fn pf_mmio_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        unimplemented!("IPciNode.pf_mmio_regions");
    }
    fn mmio_lo_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        unimplemented!("IPciNode.mmio_lo_regions");
    }
    fn mmio_hi_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        unimplemented!("IPciNode.mmio_hi_regions");
    }
    fn pio_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        unimplemented!("IPciNode.pio_regions");
    }
    fn enable_bus_master(&self, _enable: bool) -> ZxResult {
        unimplemented!("IPciNode.enable_bus_master");
    }
    fn enable_irq(&self, irq_id: usize) -> ZxResult {
        self.device().enable_irq(irq_id, true)
    }
    fn disable_irq(&self, irq_id: usize) -> ZxResult {
        self.device().enable_irq(irq_id, false)
    }
    fn register_irq_handle(
        &self,
        irq_id: usize,
        handle: Box<dyn Fn() -> u32 + Send + Sync>,
    ) -> ZxResult {
        self.device().register_irq_handle(irq_id, handle)
    }
    fn unregister_irq_handle(&self, irq_id: usize) {
        self.device().unregister_irq_handle(irq_id);
    }
}

pub struct PciRoot {
    pub base_upstream: Arc<PcieUpstream>,
    lut: PciIrqSwizzleLut,
    mmio_hi: Arc<Mutex<RegionAllocator>>,
    mmio_lo: Arc<Mutex<RegionAllocator>>,
    pio_region: Arc<Mutex<RegionAllocator>>,
}

impl PciRoot {
    pub fn new(managed_bus_id: usize, lut: PciIrqSwizzleLut, bus: &PCIeBusDriver) -> Arc<Self> {
        let inner_ups = PcieUpstream::create(managed_bus_id);
        let node = Arc::new(PciRoot {
            base_upstream: inner_ups,
            lut,
            mmio_hi: bus.mmio_hi.clone(),
            mmio_lo: bus.mmio_lo.clone(),
            pio_region: bus.pio_region.clone(),
        });
        node.base_upstream
            .set_super(Arc::downgrade(&(node.clone() as _)));
        node
    }
    pub fn swizzle(&self, dev_id: usize, func_id: usize, pin: usize) -> ZxResult<usize> {
        self.lut.swizzle(dev_id, func_id, pin)
    }
    pub fn managed_bus_id(&self) -> usize {
        self.base_upstream.managed_bus_id
    }
}

impl IPciNode for PciRoot {
    fn node_type(&self) -> PciNodeType {
        PciNodeType::Root
    }
    fn device(&self) -> Arc<PcieDevice> {
        unimplemented!()
    }
    fn as_upstream(&self) -> Option<Arc<PcieUpstream>> {
        Some(self.base_upstream.clone())
    }
    fn as_root(&self) -> Option<&PciRoot> {
        Some(self)
    }
    fn allocate_bars(&self) -> ZxResult {
        unimplemented!();
    }
    fn mmio_lo_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.mmio_lo.clone()
    }
    fn mmio_hi_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.mmio_hi.clone()
    }
    fn pio_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.pio_region.clone()
    }
    fn enable_bus_master(&self, _enable: bool) -> ZxResult {
        Ok(())
    }
}

pub struct PciDeviceNode {
    base_device: Arc<PcieDevice>,
}

impl PciDeviceNode {
    pub fn new(
        upstream: Weak<dyn IPciNode>,
        dev_id: usize,
        func_id: usize,
        driver: &PCIeBusDriver,
    ) -> Option<Arc<Self>> {
        info!("Create PciDeviceNode");
        let up_to_move = upstream.clone();
        PcieDevice::create(upstream, dev_id, func_id, driver).map(move |x| {
            let node = Arc::new(PciDeviceNode { base_device: x });
            node.base_device
                .as_ref()
                .set_super(Arc::downgrade(&(node.clone() as _)));
            // test_interface(node.clone() as _);
            driver.link_device_to_upstream(node.clone(), up_to_move.clone());
            node
        })
    }
}

impl IPciNode for PciDeviceNode {
    fn node_type(&self) -> PciNodeType {
        PciNodeType::Device
    }
    fn device(&self) -> Arc<PcieDevice> {
        self.base_device.clone()
    }
    fn as_upstream(&self) -> Option<Arc<PcieUpstream>> {
        None
    }
    fn allocate_bars(&self) -> ZxResult {
        self.base_device.allocate_bars()
    }
    fn enable_bus_master(&self, enable: bool) -> ZxResult {
        self.base_device.enable_master(enable)
    }
}

pub struct PciBridge {
    base_device: Arc<PcieDevice>,
    base_upstream: Arc<PcieUpstream>,
    mmio_lo: Arc<Mutex<RegionAllocator>>,
    mmio_hi: Arc<Mutex<RegionAllocator>>,
    pio_region: Arc<Mutex<RegionAllocator>>,
    pf_mmio: Arc<Mutex<RegionAllocator>>,
    inner: Mutex<PciBridgeInner>,
    downstream_bus_mastering_cnt: Mutex<usize>,
}

#[derive(Default)]
struct PciBridgeInner {
    pf_mem_base: u64,
    pf_mem_limit: u64,
    mem_base: u32,
    mem_limit: u32,
    io_base: u32,
    io_limit: u32,
    supports_32bit_pio: bool,
}

impl PciBridge {
    pub fn new(
        upstream: Weak<dyn IPciNode>,
        dev_id: usize,
        func_id: usize,
        managed_bus_id: usize,
        driver: &PCIeBusDriver,
    ) -> Option<Arc<Self>> {
        warn!("Create Pci Bridge");
        let father = upstream.upgrade().and_then(|x| x.as_upstream());
        father.as_ref()?;
        let inner_ups = PcieUpstream::create(managed_bus_id);
        let inner_dev = PcieDevice::create(upstream, dev_id, func_id, driver);
        inner_dev.map(move |x| {
            let node = Arc::new(PciBridge {
                base_device: x,
                base_upstream: inner_ups,
                mmio_hi: Default::default(),
                mmio_lo: Default::default(),
                pf_mmio: Default::default(),
                pio_region: Default::default(),
                inner: Default::default(),
                downstream_bus_mastering_cnt: Mutex::new(0),
            });
            node.base_device
                .set_super(Arc::downgrade(&(node.clone() as _)));
            node.base_upstream
                .set_super(Arc::downgrade(&(node.clone() as _)));
            node.init(driver);
            node
        })
    }

    fn init(&self, driver: &PCIeBusDriver) {
        let device = self.base_device.clone();
        let as_upstream = self.base_upstream.clone();
        let cfg = device.cfg.as_ref().unwrap();
        let primary_id = cfg.read8(PciReg8::PrimaryBusId) as usize;
        let secondary_id = cfg.read8(PciReg8::SecondaryBusId) as usize;
        assert_ne!(primary_id, secondary_id);
        assert_eq!(primary_id, device.bus_id);
        assert_eq!(secondary_id, as_upstream.managed_bus_id);

        let base: u32 = cfg.read8(PciReg8::IoBase) as _;
        let limit: u32 = cfg.read8(PciReg8::IoLimit) as _;
        let mut inner = self.inner.lock();
        inner.supports_32bit_pio = ((base & 0xF) == 0x1) && ((base & 0xF) == (limit & 0xF));
        inner.io_base = (base & !0xF) << 8;
        inner.io_limit = limit << 8 | 0xFFF;
        if inner.supports_32bit_pio {
            inner.io_base |= (cfg.read16(PciReg16::IoBaseUpper) as u32) << 16;
            inner.io_limit |= (cfg.read16(PciReg16::IoLimitUpper) as u32) << 16;
        }
        inner.mem_base = (cfg.read16(PciReg16::MemoryBase) as u32) << 16 & !0xFFFFF;
        inner.mem_limit = (cfg.read16(PciReg16::MemoryLimit) as u32) << 16 | 0xFFFFF;

        let base: u64 = cfg.read16(PciReg16::PrefetchableMemoryBase) as _;
        let limit: u64 = cfg.read16(PciReg16::PrefetchableMemoryLimit) as _;
        let supports_64bit_pf_mem = ((base & 0xF) == 0x1) && ((base & 0xF) == (limit & 0xF));
        inner.pf_mem_base = (base & !0xF) << 16;
        inner.pf_mem_limit = (limit << 16) | 0xFFFFF;
        if supports_64bit_pf_mem {
            inner.pf_mem_base |= (cfg.read32(PciReg32::PrefetchableMemoryBaseUpper) as u64) << 32;
            inner.pf_mem_limit |= (cfg.read32(PciReg32::PrefetchableMemoryLimitUpper) as u64) << 32;
        }

        device.inner.lock().plugged_in = true;
        let sup = device.inner.lock().weak_super.upgrade().unwrap();
        let upstream = device.upstream();
        driver.link_device_to_upstream(sup, upstream);
        as_upstream.scan_downstream(driver);
    }
}

impl IPciNode for PciBridge {
    fn node_type(&self) -> PciNodeType {
        PciNodeType::Bridge
    }
    fn device(&self) -> Arc<PcieDevice> {
        self.base_device.clone()
    }
    fn as_upstream(&self) -> Option<Arc<PcieUpstream>> {
        Some(self.base_upstream.clone())
    }
    fn pf_mmio_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.pf_mmio.clone()
    }
    fn mmio_lo_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.mmio_lo.clone()
    }
    fn mmio_hi_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.mmio_hi.clone()
    }
    fn pio_regions(&self) -> Arc<Mutex<RegionAllocator>> {
        self.pio_region.clone()
    }
    fn allocate_bars(&self) -> ZxResult {
        warn!("Allocate bars for bridge");
        let inner = self.inner.lock();
        let upstream = self.base_device.upstream().upgrade().unwrap();
        if inner.io_base <= inner.io_limit {
            let size = (inner.io_limit - inner.io_base + 1) as usize;
            if !upstream
                .pio_regions()
                .lock()
                .allocate_by_addr(inner.io_base as usize, size)
            {
                return Err(ZxError::NO_MEMORY);
            }
            self.pio_regions().lock().add(inner.io_base as usize, size);
        }
        if inner.mem_base <= inner.mem_limit {
            let size = (inner.mem_limit - inner.mem_base + 1) as usize;
            if !upstream
                .mmio_lo_regions()
                .lock()
                .allocate_by_addr(inner.mem_base as usize, size)
            {
                return Err(ZxError::NO_MEMORY);
            }
            self.mmio_lo_regions()
                .lock()
                .add(inner.mem_base as usize, size);
        }
        if inner.pf_mem_base <= inner.pf_mem_limit {
            let size = (inner.pf_mem_limit - inner.pf_mem_base + 1) as usize;
            match upstream.node_type() {
                PciNodeType::Root => {
                    if !upstream
                        .mmio_lo_regions()
                        .lock()
                        .allocate_by_addr(inner.pf_mem_base as usize, size)
                        && !upstream
                            .mmio_hi_regions()
                            .lock()
                            .allocate_by_addr(inner.pf_mem_base as usize, size)
                    {
                        return Err(ZxError::NO_MEMORY);
                    }
                }
                PciNodeType::Bridge => {
                    if !upstream
                        .pf_mmio_regions()
                        .lock()
                        .allocate_by_addr(inner.pf_mem_base as usize, size)
                    {
                        return Err(ZxError::NO_MEMORY);
                    }
                }
                _ => {
                    unreachable!("Upstream node must be root or bridge");
                }
            }
            self.pf_mmio_regions()
                .lock()
                .add(inner.pf_mem_base as usize, size);
        }
        self.base_device.allocate_bars()?;
        warn!("Allocate finish");
        upstream.as_upstream().unwrap().allocate_downstream_bars();
        Ok(())
    }
    fn enable_bus_master(&self, enable: bool) -> ZxResult {
        let count = {
            let mut count = self.downstream_bus_mastering_cnt.lock();
            if enable {
                *count += 1;
            } else if *count == 0 {
                return Err(ZxError::BAD_STATE);
            } else {
                *count -= 1;
            }
            *count
        };
        if count > 0 {
            self.base_device.enable_master(false)?;
        }
        if count == 1 && enable {
            self.base_device.enable_master(true)?;
        }
        Ok(())
    }
}

const PCI_HEADER_TYPE_MULTI_FN: u8 = 0x80;
const _PCI_HEADER_TYPE_STANDARD: u8 = 0x00;
const PCI_HEADER_TYPE_PCI_BRIDGE: u8 = 0x01;

const PCI_BAR_IO_TYPE_MASK: u32 = 0x1;
const PCI_BAR_IO_TYPE_MMIO: u32 = 0x0;
const _PCI_BAR_IO_TYPE_PIO: u32 = 0x1;

const PCI_BAR_MMIO_TYPE_MASK: u32 = 0x6;
const PCI_BAR_MMIO_TYPE_32BIT: u32 = 0x0;
const PCI_BAR_MMIO_TYPE_64BIT: u32 = 0x4;
const PCI_BAR_MMIO_ADDR_MASK: u32 = 0xFFFF_FFF0;
const PCI_BAR_PIO_ADDR_MASK: u32 = 0xFFFF_FFFC;

const PCI_BAR_MMIO_PREFETCH_MASK: u32 = 0x8;

const PCI_COMMAND_IO_EN: u16 = 0x0001;
const PCI_COMMAND_MEM_EN: u16 = 0x0002;
const PCI_COMMAND_BUS_MASTER_EN: u16 = 0x0004;

const PCIE_CFG_COMMAND_INT_DISABLE: u16 = 1 << 10;
const _PCIE_CFG_STATUS_INT_SYS: u16 = 1 << 3;

#[cfg(target_arch = "x86_64")]
const PCIE_HAS_IO_ADDR_SPACE: bool = true;
#[cfg(not(target_arch = "x86_64"))]
const PCIE_HAS_IO_ADDR_SPACE: bool = false;

/// A structure used to hold output parameters when calling
/// `pcie_query_irq_mode_capabilities`.
#[derive(Default)]
pub struct PcieIrqModeCaps {
    /// The maximum number of IRQ supported by the selected mode
    pub max_irqs: u32,
    /// For MSI or MSI-X, indicates whether or not per-vector-masking has been
    /// implemented by the hardware.
    pub per_vector_masking_supported: bool,
}

#[cfg(test)]
mod pci_bar_and_config_tests {
    use super::*;
    use crate::dev::Interrupt;

    /// One PCI function's configuration space, in memory: the thing that makes
    /// the tests below possible at all, since the PCI bus driver only runs
    /// under Zircon userboot against a real bus and not one line of it is
    /// executed by CI.
    ///
    /// It lives in `harness.rs` now, because `caps.rs` and `config.rs` need the
    /// same one and three copies of a device are three devices that can
    /// disagree.
    use super::super::harness::ConfigSpace;

    /// A device with nothing but a configuration space behind it: the
    /// identifiers are a real RTX 2060 SUPER (TU106), the card this kernel is
    /// actually run on.
    fn device_with(cfg: Arc<PciConfig>) -> PcieDevice {
        PcieDevice {
            bus_id: 0,
            dev_id: 3,
            func_id: 0,
            bar_count: 6,
            cfg: Some(cfg),
            _cfg_phys: 0,
            dev_lock: Mutex::default(),
            command_lock: Mutex::default(),
            vendor_id: 0x10de,
            device_id: 0x1f06,
            class_id: 0x03,
            subclass_id: 0x00,
            prog_if: 0x00,
            rev_id: 0xa1,
            inner: Default::default(),
        }
    }

    /// The capability list of a card like the one above: power management at
    /// 0x40, MSI (64-bit, per-vector masking) at 0x50, PCI Express at 0x68.
    fn seed_capability_list(space: &mut ConfigSpace) {
        space.0[PciReg8::CapabilitiesPtr as usize] = 0x40;
        // id 0x01, next 0x50.
        space.poke32(0x40, 0x0000_5001);
        // id 0x05, next 0x68, control 0x0184 = 64-bit address, per-vector
        // masking, and four vectors supported.
        space.poke32(0x50, 0x0184_6805);
        // id 0x10, next 0x00, capability register 0x0002 = version 2, endpoint.
        space.poke32(0x68, 0x0002_0010);
    }

    // ---------- What a BAR register says it is ----------

    #[test]
    fn a_bar_declares_its_space_and_its_width() {
        // Memory, 32-bit: bit 0 clear, bits 2:1 == 0b00.
        assert_eq!(bar_kind(0, 0xF600_0000), Ok((true, false)));
        // Memory, 64-bit prefetchable: bits 2:1 == 0b10, bit 3 set.
        assert_eq!(bar_kind(1, 0x0000_000C), Ok((true, true)));
        // I/O: bit 0 set, and the width bits mean nothing there.
        assert_eq!(bar_kind(5, 0x0000_E001), Ok((false, false)));
        assert_eq!(bar_kind(5, 0x0000_E003), Ok((false, false)));
    }

    #[test]
    fn a_memory_bar_of_a_reserved_width_is_refused() {
        // 0b01 in bits 2:1 was "below 1 MiB" on the original PCI bus and is
        // reserved now. Guessing would mean probing the wrong register count.
        assert_eq!(bar_kind(0, 0xF600_0002), Err(ZxError::BAD_STATE));
        assert_eq!(bar_kind(0, 0xF600_0006), Err(ZxError::BAD_STATE));
    }

    // ---------- What the two registers mean after the size probe ----------

    #[test]
    fn a_32_bit_memory_bar_reports_its_size_and_its_address() {
        // BAR0 of the card: 16 MiB of registers at 0xF600_0000.
        let bar = bar_info_from_probe(0, true, false, 0xF600_0000, 0xFF00_0000, 0, 0);
        assert!(bar.is_mmio);
        assert!(!bar.is_64bit);
        assert!(!bar.is_prefetchable);
        assert_eq!(bar.size, 16 << 20);
        assert_eq!(bar.bus_addr, 0xF600_0000);
        assert_eq!(bar.first_bar_reg, 0);
    }

    #[test]
    fn a_64_bit_bar_takes_its_high_half_from_the_upper_register() {
        // BAR1 of the card: 256 MiB of framebuffer aperture, prefetchable,
        // which firmware puts at 0x10_0000_0000 — above 4 GiB, where QEMU
        // never puts anything, which is why this went unnoticed.
        let bar = bar_info_from_probe(
            1,
            true,
            true,
            0x0000_000C,
            0xF000_000C,
            0x0000_0010,
            0xFFFF_FFFF,
        );
        assert!(bar.is_64bit);
        assert!(bar.is_prefetchable);
        assert_eq!(bar.size, 256 << 20);
        // Not 0x0000_000C_0000_0000: the upper half is the upper register,
        // never the low one's type and prefetch bits.
        assert_eq!(bar.bus_addr, 0x0000_0010_0000_0000);
    }

    #[test]
    fn an_io_bar_stays_inside_the_32_bit_port_space() {
        // BAR5: 128 I/O ports at 0xE000.
        let bar = bar_info_from_probe(5, false, false, 0x0000_E001, 0xFFFF_FF81, 0, 0);
        assert!(!bar.is_mmio);
        assert!(!bar.is_prefetchable);
        assert_eq!(bar.size, 128);
        assert_eq!(bar.bus_addr, 0xE000);
    }

    #[test]
    fn prefetchable_is_a_memory_only_bit() {
        // Bit 3 of an I/O BAR is part of the address, not a prefetch hint.
        let bar = bar_info_from_probe(5, false, false, 0x0000_E009, 0xFFFF_FF81, 0, 0);
        assert!(!bar.is_prefetchable);
        assert_eq!(bar.bus_addr, 0xE008);
    }

    #[test]
    fn an_unimplemented_bar_has_no_size() {
        // Registers read back as zeros, so the size mask is all ones and the
        // size wraps to nothing. The 64-bit case used to overflow instead.
        assert_eq!(bar_info_from_probe(2, true, false, 0, 0, 0, 0).size, 0);
        assert_eq!(bar_info_from_probe(2, false, false, 0, 0, 0, 0).size, 0);
        assert_eq!(bar_info_from_probe(2, true, true, 0, 0, 0, 0).size, 0);
    }

    #[test]
    fn the_bars_of_the_card_come_out_as_the_card_declares_them() {
        // (index, is_mmio, is_64bit, low, low_probe, high, high_probe)
        // then the size and bus address the card is documented to have.
        let card = [
            (
                (
                    0usize,
                    true,
                    false,
                    0xF600_0000u32,
                    0xFF00_0000u32,
                    0u32,
                    0u32,
                ),
                16u64 << 20,
                0xF600_0000u64,
            ),
            (
                (
                    1,
                    true,
                    true,
                    0x0000_000C,
                    0xF000_000C,
                    0x0000_0010,
                    0xFFFF_FFFF,
                ),
                256 << 20,
                0x10_0000_0000,
            ),
            (
                (
                    3,
                    true,
                    true,
                    0x0000_000C,
                    0xFE00_000C,
                    0x0000_0011,
                    0xFFFF_FFFF,
                ),
                32 << 20,
                0x11_0000_0000,
            ),
            (
                (5, false, false, 0x0000_E001, 0xFFFF_FF81, 0, 0),
                128,
                0xE000,
            ),
        ];
        for ((i, is_mmio, is_64bit, low, low_probe, high, high_probe), size, addr) in card {
            let bar = bar_info_from_probe(i, is_mmio, is_64bit, low, low_probe, high, high_probe);
            assert_eq!(bar.size, size, "tamaño de BAR{}", i);
            assert_eq!(bar.bus_addr, addr, "dirección de BAR{}", i);
        }
    }

    #[test]
    fn probing_the_bars_leaves_every_register_as_it_found_it() {
        // A configuration space that is only memory cannot answer a size
        // probe — writing ones and reading them back gives ones — so what
        // this pins is the handling around the probe: that the command
        // register is put back, that both halves of a 64-bit BAR are put
        // back, and that a 64-bit BAR consumes the register after it. A BAR
        // left holding the probe pattern would have the device decoding a
        // window it does not own.
        let mut space = ConfigSpace::new();
        let bars = [
            0xF600_0000u32, // 0: memory, 32-bit
            0x0000_000C,    // 1: memory, 64-bit prefetchable, low half
            0x0000_0010,    //    and its upper half
            0xF500_0000,    // 3: memory, 32-bit
            0x0000_0000,    // 4: unimplemented
            0x0000_E001,    // 5: I/O
        ];
        for (i, val) in bars.iter().enumerate() {
            space.poke32(0x10 + i * 4, *val);
        }
        space.poke32(0x04, 0x0010_0007); // command: I/O, memory and bus master on
        let dev = device_with(space.config());
        dev.init_probe_bars().unwrap();

        for (i, val) in bars.iter().enumerate() {
            assert_eq!(space.peek32(0x10 + i * 4), *val, "BAR{} sin restaurar", i);
        }
        assert_eq!(
            space.peek16(0x04),
            0x0007,
            "registro de comando sin restaurar"
        );

        let inner = dev.inner.lock();
        // Sixteen bytes is what a memory BAR backed by plain memory reports;
        // four is what an I/O BAR reports, its address mask being two bits
        // wider.
        assert_eq!(inner.bars[0].size, 16);
        assert!(inner.bars[0].is_mmio && !inner.bars[0].is_64bit);
        assert!(inner.bars[1].is_64bit && inner.bars[1].is_prefetchable);
        assert_eq!(inner.bars[1].size, 16);
        // Register 2 is the upper half of BAR1 and is never a BAR of its own.
        assert_eq!(inner.bars[2].size, 0);
        assert_eq!(inner.bars[2].first_bar_reg, 0);
        assert_eq!(inner.bars[3].size, 16);
        assert_eq!(inner.bars[4].size, 16);
        assert_eq!(inner.bars[5].size, 4);
        assert!(!inner.bars[5].is_mmio);
    }

    // ---------- How much space a BAR is given ----------

    #[test]
    fn a_memory_bar_smaller_than_a_page_is_given_a_whole_page() {
        // It becomes a VMO handed to userspace, and a VMO is whole pages, so
        // 128 bytes of registers must not share their page with the next
        // device's.
        assert_eq!(bar_alignment(128, true), PAGE_SIZE);
        assert_eq!(bar_alignment(PAGE_SIZE as u64 - 1, true), PAGE_SIZE);
        assert_eq!(bar_alignment(PAGE_SIZE as u64, true), PAGE_SIZE);
        assert_eq!(bar_alignment(16 << 20, true), 16 << 20);
    }

    #[test]
    fn an_io_bar_is_given_its_natural_size_where_there_are_ports() {
        // The whole I/O space is 64 KiB and has no page table behind it, so
        // rounding 32 ports up to a page would spend a sixteenth of it.
        let expected = if PCIE_HAS_IO_ADDR_SPACE {
            32
        } else {
            PAGE_SIZE
        };
        assert_eq!(bar_alignment(32, false), expected);
        assert_eq!(bar_alignment(16 << 20, false), 16 << 20);
    }

    // ---------- The per-vector MSI mask ----------

    #[test]
    fn masking_a_vector_touches_only_its_own_bit() {
        assert_eq!(msi_mask_after(0, 0, true), 0b1);
        assert_eq!(msi_mask_after(0, 3, true), 0b1000);
        assert_eq!(msi_mask_after(0b1010, 0, true), 0b1011);
        assert_eq!(msi_mask_after(0b1111, 1, false), 0b1101);
        assert_eq!(msi_mask_after(u32::MAX, 31, false), 0x7FFF_FFFF);
    }

    #[test]
    fn every_vector_masks_and_unmasks_back_to_where_it_started() {
        // The unmask side used to shift the wrong way: `!(1 >> irq)` is `!1`
        // for vector 0 and `!0` — a no-op — for every vector after it, so
        // only the first vector of a device could ever be unmasked.
        for irq in 0..PCIE_MAX_MSI_IRQS {
            let masked = msi_mask_after(0, irq, true);
            assert_eq!(masked, 1u32 << irq, "vector {}", irq);
            assert_eq!(msi_mask_after(masked, irq, false), 0, "vector {}", irq);
            assert_eq!(
                msi_mask_after(u32::MAX, irq, false),
                !(1u32 << irq),
                "vector {}",
                irq
            );
        }
    }

    #[test]
    fn a_vector_count_the_hardware_cannot_express_is_refused() {
        // Four vectors is what the fake card advertises.
        assert_eq!(msi_multi_message_encoding(1, 4), Ok(0));
        assert_eq!(msi_multi_message_encoding(2, 4), Ok(1));
        assert_eq!(msi_multi_message_encoding(3, 4), Ok(2));
        assert_eq!(msi_multi_message_encoding(4, 4), Ok(2));
        assert_eq!(msi_multi_message_encoding(5, 4), Err(ZxError::INVALID_ARGS));
        // A device that supports the whole file of 32.
        assert_eq!(msi_multi_message_encoding(32, 32), Ok(5));
        assert_eq!(
            msi_multi_message_encoding(33, 32),
            Err(ZxError::INVALID_ARGS)
        );
        // The field a device states its vector count in is three bits wide,
        // so a malformed one can claim 64 or 128 — more than the Multiple
        // Message Enable field can encode, and more than there would be
        // handlers for. What a device claims is a ceiling, not a licence.
        assert_eq!(msi_multi_message_encoding(32, 128), Ok(5));
        assert_eq!(
            msi_multi_message_encoding(64, 128),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            msi_multi_message_encoding(33, 64),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            msi_multi_message_encoding(0, 32),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            msi_multi_message_encoding(usize::MAX, 32),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn asking_for_more_vectors_than_a_device_has_is_an_error_not_a_panic() {
        // `zx_pci_set_irq_mode(handle, MSI, 33)` reached an `assert!` inside
        // the MSI setup, which is a kernel panic from a syscall.
        let mut space = ConfigSpace::new();
        seed_capability_list(&mut space);
        let dev = device_with(space.config());
        dev.init_capabilities().unwrap();
        dev.inner.lock().plugged_in = true;
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Msi, 33),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Msi, 5),
            Err(ZxError::INVALID_ARGS)
        );
        // And the device is left as it was, not half torn down.
        assert_eq!(dev.inner.lock().irq.mode, PcieIrqMode::Disabled);
        assert!(dev.inner.lock().irq.handlers.is_empty());
    }

    #[test]
    fn msi_on_a_device_without_the_capability_is_refused_before_anything_moves() {
        let mut space = ConfigSpace::new();
        let dev = device_with(space.config());
        dev.inner.lock().plugged_in = true;
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Msi, 1),
            Err(ZxError::NOT_SUPPORTED)
        );
        assert_eq!(dev.inner.lock().irq.mode, PcieIrqMode::Disabled);
    }

    // ---------- Bounds and alignment of a configuration access ----------

    #[test]
    fn a_pci_express_device_reaches_its_extended_space() {
        // The extended capabilities — AER, ARI, resizable BAR — all live
        // above 0x100, and a driver that cannot read them cannot use them.
        assert_eq!(check_config_access(0x100, 4, true), Ok(()));
        assert_eq!(check_config_access(4092, 4, true), Ok(()));
        assert_eq!(
            check_config_access(4096, 1, true),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn a_plain_pci_device_stops_at_its_header() {
        // 256 bytes is all it has; past that is another function's registers.
        assert_eq!(check_config_access(252, 4, false), Ok(()));
        assert_eq!(
            check_config_access(256, 1, false),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            check_config_access(0x100, 4, false),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn a_configuration_access_must_be_naturally_aligned() {
        assert_eq!(check_config_access(0x10, 4, true), Ok(()));
        assert_eq!(
            check_config_access(0x11, 4, true),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            check_config_access(0x12, 4, true),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(check_config_access(0x12, 2, true), Ok(()));
        assert_eq!(
            check_config_access(0x13, 2, true),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(check_config_access(0x13, 1, true), Ok(()));
    }

    #[test]
    fn only_a_byte_a_word_or_a_dword_can_be_asked_for() {
        for width in [0usize, 3, 5, 8, 16] {
            assert_eq!(
                check_config_access(0, width, true),
                Err(ZxError::INVALID_ARGS),
                "ancho {}",
                width
            );
        }
        // An offset that would wrap instead of exceeding the space.
        assert_eq!(
            check_config_access(usize::MAX - 3, 4, true),
            Err(ZxError::INVALID_ARGS)
        );
    }

    // ---------- Against a configuration space ----------

    #[test]
    fn a_configuration_read_lands_on_the_devices_own_registers() {
        let mut space = ConfigSpace::new();
        // Vendor 0x10DE, device 0x1F06 — where a driver looks first.
        space.poke32(0x00, 0x1F06_10DE);
        space.poke32(0x10, 0xF600_0000);
        space.0[0x3C] = 0x0B;
        let dev = device_with(space.config());
        assert_eq!(dev.config_read(0x00, 4), Ok(0x1F06_10DE));
        assert_eq!(dev.config_read(0x00, 2), Ok(0x10DE));
        assert_eq!(dev.config_read(0x02, 2), Ok(0x1F06));
        assert_eq!(dev.config_read(0x10, 4), Ok(0xF600_0000));
        assert_eq!(dev.config_read(0x3C, 1), Ok(0x0B));
    }

    #[test]
    fn a_configuration_write_lands_on_the_devices_own_registers() {
        let mut space = ConfigSpace::new();
        // Neighbours with something in them, so a write that is wider than it
        // was asked to be shows up: the byte at 0x0D is the latency timer,
        // sitting between the cache line size and the header type.
        space.poke32(0x0C, 0xAABB_CCDD);
        space.poke32(0x04, 0x1111_2222);
        let dev = device_with(space.config());
        assert_eq!(dev.config_write(0x04, 2, 0x0007), Ok(()));
        assert_eq!(dev.config_write(0x14, 4, 0xDEAD_BEEF), Ok(()));
        assert_eq!(dev.config_write(0x0D, 1, 0x40), Ok(()));
        assert_eq!(space.peek16(0x04), 0x0007);
        assert_eq!(space.peek32(0x14), 0xDEAD_BEEF);
        assert_eq!(space.peek8(0x0D), 0x40);
        // Each write is exactly as wide as it was asked to be: the byte write
        // left the three registers around it alone, and the word write left
        // the upper half of its dword alone.
        assert_eq!(space.peek32(0x0C), 0xAABB_40DD);
        assert_eq!(space.peek32(0x04), 0x1111_0007);
        // And nothing outside the registers it was told to write.
        assert_eq!(space.peek32(0x00), 0);
        assert_eq!(space.peek32(0x10), 0);
    }

    #[test]
    fn the_space_a_device_may_be_asked_for_follows_its_capabilities() {
        let mut space = ConfigSpace::new();
        space.poke32(0x100, 0xCAFE_F00D);
        let dev = device_with(space.config());
        // With no capability list walked yet there is no PCI Express
        // capability, so the header is the whole space.
        assert_eq!(dev.config_read(0x100, 4), Err(ZxError::INVALID_ARGS));
        assert_eq!(dev.config_write(0x100, 4, 1), Err(ZxError::INVALID_ARGS));
        seed_capability_list(&mut space);
        let dev = device_with(space.config());
        dev.init_capabilities().unwrap();
        assert!(dev.inner.lock().pcie().is_some());
        assert_eq!(dev.config_read(0x100, 4), Ok(0xCAFE_F00D));
        assert_eq!(dev.config_write(0x100, 4, 0x0000_0002), Ok(()));
        assert_eq!(space.peek32(0x100), 0x0000_0002);
    }

    // ---------- The capability walk ----------

    #[test]
    fn the_capability_list_of_the_card_is_walked_in_order() {
        let mut space = ConfigSpace::new();
        seed_capability_list(&mut space);
        let dev = device_with(space.config());
        dev.init_capabilities().unwrap();
        let inner = dev.inner.lock();
        let ids: alloc::vec::Vec<u8> = inner
            .caps
            .iter()
            .map(|c| match c {
                PciCapability::Msi(std, _) => std.id,
                PciCapability::Pcie(std, _) => std.id,
                PciCapability::AdvFeatures(std, _) => std.id,
                PciCapability::Std(std) => std.id,
            })
            .collect();
        assert_eq!(ids, alloc::vec![0x01, 0x05, 0x10]);
        let (std, msi) = inner.msi().unwrap();
        assert_eq!(std.base, 0x50);
        assert!(msi.is_64bit);
        assert!(msi.has_pvm);
        assert_eq!(msi.mask_bits_offset, 0x60);
        let (_std, pcie) = inner.pcie().unwrap();
        assert_eq!(pcie.version, 2);
    }

    #[test]
    fn the_msi_mask_register_is_initialised_inside_the_devices_space() {
        // Bringing up a capability with per-vector masking starts with every
        // vector masked. That write is an offset inside the capability, so it
        // has to go through `base`; it used to be handed to the accessor that
        // takes a whole address, which put it at address 0x60.
        let mut space = ConfigSpace::new();
        seed_capability_list(&mut space);
        let dev = device_with(space.config());
        dev.init_capabilities().unwrap();
        assert_eq!(space.peek32(0x60), u32::MAX);
        // The control word keeps its 64-bit, masking and vector-count bits.
        assert_eq!(space.peek16(0x52), 0x0184);
    }

    #[test]
    fn a_capability_pointer_outside_the_list_area_is_refused() {
        let mut space = ConfigSpace::new();
        // 0x30 is inside the standard header, not the capability area.
        space.0[PciReg8::CapabilitiesPtr as usize] = 0x30;
        let dev = device_with(space.config());
        assert_eq!(dev.init_capabilities(), Err(ZxError::INVALID_ARGS));
    }

    // ---------- Masking a vector against a configuration space ----------

    #[test]
    fn unmasking_the_second_vector_of_a_device_actually_unmasks_it() {
        let mut space = ConfigSpace::new();
        seed_capability_list(&mut space);
        let dev = device_with(space.config());
        dev.init_capabilities().unwrap();
        {
            let mut inner = dev.inner.lock();
            dev.allocate_irq_handler(&mut inner, 4, true);
            inner.irq.mode = PcieIrqMode::Msi;
            inner.plugged_in = true;
            for h in inner.irq.handlers.iter() {
                h.set_handler(Some(alloc::boxed::Box::new(|| 0)));
            }
        }
        // Every vector starts masked.
        assert_eq!(space.peek32(0x60), u32::MAX);
        for irq in 0..4 {
            assert_eq!(dev.enable_irq(irq, true), Ok(()));
        }
        // The four vectors this device uses are now unmasked, and nothing
        // else was touched.
        assert_eq!(space.peek32(0x60), 0xFFFF_FFF0);
        assert_eq!(dev.enable_irq(2, false), Ok(()));
        assert_eq!(space.peek32(0x60), 0xFFFF_FFF4);
    }

    #[test]
    fn each_vector_remembers_whether_it_is_masked() {
        // `msi_irq_handler` asks `mask_msi_irq` whether the vector it is
        // holding was already masked, to tell a real interrupt from one it
        // masked itself. That bookkeeping was kept for vector 0 only, so
        // every vector after the first answered for vector 0.
        let mut space = ConfigSpace::new();
        seed_capability_list(&mut space);
        let dev = device_with(space.config());
        dev.init_capabilities().unwrap();
        let mut inner = dev.inner.lock();
        dev.allocate_irq_handler(&mut inner, 4, false);
        let inner = inner;
        // Bringing the capability up masked every vector; start from none
        // masked so each call's own bit is the one that moves.
        for irq in 0..4 {
            dev.mask_msi_irq(&inner, irq, false);
        }
        assert_eq!(space.peek32(0x60), 0xFFFF_FFF0);
        assert!(!dev.mask_msi_irq(&inner, 2, true));
        // Vector 2 is masked now, and it is vector 2 that says so.
        assert!(dev.mask_msi_irq(&inner, 2, true));
        assert!(!dev.mask_msi_irq(&inner, 0, true));
        assert!(!dev.mask_msi_irq(&inner, 3, false));
        assert_eq!(space.peek32(0x60), 0xFFFF_FFF5);
    }

    // ---------- The vector table, and who is allowed to name a vector ----------

    /// A device in MSI mode with `vectors` vectors, each of them masked,
    /// which is where a `zx_pci_set_irq_mode(handle, MSI, vectors)` leaves
    /// one. The real entry also allocates an interrupt block from the
    /// platform, of which a host has none; nothing under test here reads it.
    fn device_in_msi_mode(space: &mut ConfigSpace, vectors: usize) -> Arc<PcieDevice> {
        seed_capability_list(space);
        let dev = Arc::new(device_with(space.config()));
        dev.init_capabilities().unwrap();
        let mut inner = dev.inner.lock();
        inner.plugged_in = true;
        dev.allocate_irq_handler(&mut inner, vectors, true);
        inner.irq.mode = PcieIrqMode::Msi;
        drop(inner);
        dev
    }

    /// The node a driver process reaches the device through: this is what
    /// `zx_pci_map_interrupt` hands to `Interrupt::new_pci`.
    fn node_of(dev: Arc<PcieDevice>) -> Arc<dyn IPciNode> {
        Arc::new(PciDeviceNode { base_device: dev })
    }

    fn a_handler() -> alloc::boxed::Box<dyn Fn() -> u32 + Send + Sync> {
        alloc::boxed::Box::new(|| 0)
    }

    #[test]
    fn mapping_an_interrupt_before_any_irq_mode_is_a_refusal_not_a_panic() {
        // One syscall, on the handle every PCI driver process holds:
        // `zx_pci_map_interrupt` without a `zx_pci_set_irq_mode` before it.
        // The device's vector table is still empty and its mode is
        // `Disabled`, and two of the four `assert!`s here answered for that.
        let mut space = ConfigSpace::new();
        seed_capability_list(&mut space);
        let dev = Arc::new(device_with(space.config()));
        dev.init_capabilities().unwrap();
        dev.inner.lock().plugged_in = true;
        assert_eq!(
            dev.register_irq_handle(0, a_handler()),
            Err(ZxError::BAD_STATE)
        );
        // And the same thing through the object userspace actually gets.
        assert_eq!(
            Interrupt::new_pci(node_of(dev), 0, true).err(),
            Some(ZxError::BAD_STATE)
        );
    }

    #[test]
    fn a_vector_the_device_was_never_given_is_refused() {
        // The other half of that syscall. The number is checked against
        // `irqs_avail_cnt`, a constant this kernel sets to ten and never
        // revisits, while how many vectors the device has is whatever the
        // last `zx_pci_set_irq_mode` asked for. Everything in between was
        // `assert!(irq_id < inner.irq.handlers.len())`.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        assert_eq!(
            dev.register_irq_handle(4, a_handler()),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            dev.register_irq_handle(9, a_handler()),
            Err(ZxError::INVALID_ARGS)
        );
        // And the same number arriving at the other end, from an ack or a
        // destroy on an interrupt object that outlived its vector.
        assert_eq!(dev.enable_irq(4, true), Err(ZxError::INVALID_ARGS));
        assert_eq!(dev.enable_irq(4, false), Err(ZxError::INVALID_ARGS));
        assert_eq!(space.peek32(0x60), u32::MAX);
        assert_eq!(
            Interrupt::new_pci(node_of(dev), 5, true).err(),
            Some(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn an_interrupt_object_takes_the_vector_it_names_and_unmasks_it() {
        // The path that is meant to work, so that the refusals around it are
        // refusals and not the function having stopped working.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        assert_eq!(space.peek32(0x60), u32::MAX);
        let irq = Interrupt::new_pci(node_of(dev.clone()), 2, true).unwrap();
        {
            let inner = dev.inner.lock();
            assert!(inner.irq.handlers[2].has_handler());
            for &other in [0usize, 1, 3].iter() {
                assert!(
                    !inner.irq.handlers[other].has_handler(),
                    "vector {} took somebody else's handler",
                    other
                );
            }
        }
        // Creating the object unmasks what it just registered, and that is
        // one bit of one register of the device.
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFB);
        assert_eq!(irq.destroy(), Ok(()));
        // Teardown masks it again and gives the vector back.
        assert_eq!(space.peek32(0x60), u32::MAX);
        assert!(!dev.inner.lock().irq.handlers[2].has_handler());
    }

    #[test]
    fn a_vector_with_no_handler_cannot_be_unmasked_but_can_be_masked() {
        // The asymmetry is deliberate. Unmasking a vector nothing answers
        // for arms an interrupt line into an empty slot; masking one is what
        // teardown does, and teardown runs once the handler is already gone.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        assert_eq!(dev.enable_irq(1, true), Err(ZxError::BAD_STATE));
        assert_eq!(dev.enable_irq(1, false), Ok(()));
        dev.register_irq_handle(1, a_handler()).unwrap();
        assert_eq!(dev.enable_irq(1, true), Ok(()));
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFD);
    }

    #[test]
    fn a_device_whose_irq_mode_went_back_to_disabled_refuses_its_old_vectors() {
        // Three syscalls, every one of them documented: map an interrupt,
        // put the device's IRQ mode back to `Disabled` -- which empties the
        // vector table while the interrupt object is still live and still
        // holding vector 2 -- then destroy the interrupt. Its teardown masks
        // and unregisters that vector, and both of those asserted it was
        // still there.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        let irq = Interrupt::new_pci(node_of(dev.clone()), 2, true).unwrap();
        dev.set_irq_mode(PcieIrqMode::Disabled, 0).unwrap();
        assert!(dev.inner.lock().irq.handlers.is_empty());
        assert_eq!(irq.destroy(), Ok(()));
    }

    #[test]
    fn an_unplugged_device_refuses_its_vectors_instead_of_asserting() {
        // A hot-unplug under a live interrupt object. `plugged_in` was the
        // opening `assert!` of all three of these.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        dev.register_irq_handle(0, a_handler()).unwrap();
        dev.inner.lock().plugged_in = false;
        assert_eq!(dev.enable_irq(0, true), Err(ZxError::BAD_STATE));
        assert_eq!(dev.enable_irq(0, false), Err(ZxError::BAD_STATE));
        assert_eq!(
            dev.register_irq_handle(0, a_handler()),
            Err(ZxError::BAD_STATE)
        );
        // Handing a handler back is the exception, because that is teardown
        // and teardown has to work on a device that is already gone.
        dev.unregister_irq_handle(0);
        assert!(!dev.inner.lock().irq.handlers[0].has_handler());
    }

    #[test]
    fn a_disabled_device_refuses_to_arm_a_vector() {
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        dev.register_irq_handle(0, a_handler()).unwrap();
        dev.inner.lock().disabled = true;
        assert_eq!(dev.enable_irq(0, true), Err(ZxError::BAD_STATE));
        assert_eq!(
            dev.register_irq_handle(1, a_handler()),
            Err(ZxError::BAD_STATE)
        );
        // Disarming it still has to work: that is how it gets disabled.
        assert_eq!(dev.enable_irq(0, false), Ok(()));
    }

    #[test]
    fn a_vector_of_a_device_in_a_mode_with_no_masking_is_refused() {
        // The last of the five, and the only one with no syscall behind it:
        // `inner.irq.mode` is a field two other functions agree to keep in
        // step with the vector table, and this arm was an `unreachable!()`
        // resting on that agreement. Made total rather than traced.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        dev.register_irq_handle(0, a_handler()).unwrap();
        dev.inner.lock().irq.mode = PcieIrqMode::MsiX;
        assert_eq!(dev.enable_irq(0, true), Err(ZxError::BAD_STATE));
        assert_eq!(dev.enable_irq(0, false), Err(ZxError::BAD_STATE));
    }

    #[test]
    fn unregistering_a_vector_takes_only_that_ones_handler() {
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        for irq in 0..4 {
            dev.register_irq_handle(irq, a_handler()).unwrap();
        }
        dev.unregister_irq_handle(2);
        let inner = dev.inner.lock();
        assert!(!inner.irq.handlers[2].has_handler());
        for &other in [0usize, 1, 3].iter() {
            assert!(
                inner.irq.handlers[other].has_handler(),
                "vector {} lost its handler too",
                other
            );
        }
    }

    #[test]
    fn unregistering_a_vector_that_is_gone_is_not_a_panic() {
        // This runs from the interrupt object's teardown, which a process
        // exiting reaches through a `Drop`, and a panic taken while already
        // unwinding aborts. All four of its `assert!`s can be false by then.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        dev.set_irq_mode(PcieIrqMode::Disabled, 0).unwrap();
        dev.unregister_irq_handle(0);
        dev.unregister_irq_handle(9);
        dev.inner.lock().plugged_in = false;
        dev.unregister_irq_handle(0);
    }

    #[test]
    fn masking_a_vector_that_is_no_longer_there_reports_nothing() {
        // `msi_irq_handler` runs in interrupt context holding a vector
        // number captured when its handler was registered, and asks this
        // whether that vector was already masked, to tell a real interrupt
        // from one it masked itself. `zx_pci_set_irq_mode` empties the table
        // underneath it, so the number can be stale -- and an interrupt is
        // the worst place in the kernel to take a panic.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        {
            // Every vector starts masked, so this one reports it was.
            let inner = dev.inner.lock();
            assert!(dev.mask_msi_irq(&inner, 0, false));
            assert_eq!(space.peek32(0x60), 0xFFFF_FFFE);
        }
        dev.set_irq_mode(PcieIrqMode::Disabled, 0).unwrap();
        let inner = dev.inner.lock();
        assert!(!dev.mask_msi_irq(&inner, 0, true));
        assert!(!dev.mask_msi_irq(&inner, 0, false));
        // And the register it would have written is the one teardown left.
        assert_eq!(space.peek32(0x60), u32::MAX);
    }

    #[test]
    fn a_vector_with_no_bit_in_the_mask_register_is_left_alone() {
        // Thirty-two vectors is all the register holds, and the count is
        // bounded to that where it is chosen. `1 << 32` is a panic in debug
        // and, in release, a wrap round to bit 0 -- somebody else's vector.
        assert_eq!(msi_mask_after(0xFFFF_FFFF, 32, false), 0xFFFF_FFFF);
        assert_eq!(msi_mask_after(0, 33, true), 0);
        assert_eq!(msi_mask_after(0, usize::MAX, true), 0);
        // The last vector that does have one still moves.
        assert_eq!(msi_mask_after(0xFFFF_FFFF, 31, false), 0x7FFF_FFFF);
        assert_eq!(msi_mask_after(0, 31, true), 0x8000_0000);
    }

    #[test]
    fn a_refused_irq_mode_leaves_the_device_the_way_it_found_it() {
        // `Count` is a variant of the enum the syscall turns the user's
        // number into, so `zx_pci_set_irq_mode(handle, 4, 1)` gets all the
        // way in and is refused at the end -- on the far side of taking the
        // device's current mode apart, which nothing undoes. One syscall for
        // a driver process to disarm its own device and be told it did
        // nothing. `MsiX` and the two legacy refusals sat there too.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        dev.register_irq_handle(1, a_handler()).unwrap();
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Count, 1),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::MsiX, 1),
            Err(ZxError::NOT_SUPPORTED)
        );
        // A card with no legacy interrupt pin, which is what a PCI Express
        // card is, asking for the one mode it has not got.
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Legacy, 1),
            Err(ZxError::NOT_SUPPORTED)
        );
        // More legacy interrupts than any device can have.
        dev.inner.lock().irq.legacy.pin = 1;
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Legacy, 2),
            Err(ZxError::NOT_SUPPORTED)
        );
        // And a vector count the MSI capability cannot encode, which is the
        // same bug one layer down and is settled here for the same reason.
        assert_eq!(
            dev.set_irq_mode(PcieIrqMode::Msi, 33),
            Err(ZxError::INVALID_ARGS)
        );
        // Five refusals, and the device is still doing what it was.
        let inner = dev.inner.lock();
        assert_eq!(inner.irq.mode, PcieIrqMode::Msi);
        assert_eq!(inner.irq.handlers.len(), 4);
        assert!(inner.irq.handlers[1].has_handler());
    }

    #[test]
    fn an_interrupt_the_platform_cannot_mask_never_touches_the_mask_register() {
        // Whether a PCI interrupt is maskable at all comes from the bus, and
        // the interrupt object has to leave the device's per-vector mask
        // alone when it is not. Registering the handler is not optional.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        // Leave vector 2 unmasked before the object exists, so that a mask
        // it should not be doing has somewhere to show. Starting from every
        // vector masked, as the device does, hides it: masking what is
        // already masked writes the same register back.
        dev.register_irq_handle(2, a_handler()).unwrap();
        assert_eq!(dev.enable_irq(2, true), Ok(()));
        dev.unregister_irq_handle(2);
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFB);
        let irq = Interrupt::new_pci(node_of(dev.clone()), 2, false).unwrap();
        assert!(dev.inner.lock().irq.handlers[2].has_handler());
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFB);
        assert_eq!(irq.destroy(), Ok(()));
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFB);
        assert!(!dev.inner.lock().irq.handlers[2].has_handler());
    }

    #[test]
    fn destroying_an_interrupt_twice_does_not_take_the_vector_twice() {
        // `zx_interrupt_destroy` on a handle that has already been
        // destroyed. The second round finds its registration handed back
        // and has to leave the vector alone, rather than mask whatever is
        // on it by then.
        let mut space = ConfigSpace::new();
        let dev = device_in_msi_mode(&mut space, 4);
        let irq = Interrupt::new_pci(node_of(dev.clone()), 2, true).unwrap();
        assert_eq!(irq.destroy(), Ok(()));
        // Somebody else takes vector 2 and arms it.
        dev.register_irq_handle(2, a_handler()).unwrap();
        assert_eq!(dev.enable_irq(2, true), Ok(()));
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFB);
        assert_eq!(irq.destroy(), Ok(()));
        assert_eq!(space.peek32(0x60), 0xFFFF_FFFB);
        assert!(dev.inner.lock().irq.handlers[2].has_handler());
    }
}
