use super::*;
use alloc::sync::Arc;
use core::convert::TryFrom;
use zircon_object::{
    dev::pci::{
        constants::*,
        pci_init_args::{PciInitArgsAddrWindows, PciInitArgsHeader, PCI_INIT_ARG_MAX_SIZE},
        MmioPcieAddressProvider, PCIeBusDriver, PciAddrSpace, PciEcamRegion, PcieDeviceInfo,
        PcieDeviceKObject, PcieIrqMode, PmioPcieAddressProvider,
    },
    dev::{Resource, ResourceKind},
    vm::{pages, VmObject},
};

impl Syscall<'_> {
    pub fn sys_pci_add_subtract_io_range(
        &self,
        handle: HandleValue,
        mmio: bool,
        base: u64,
        len: u64,
        add: bool,
    ) -> ZxResult {
        info!(
            "pci.add_subtract_io_range: handle={:#x}, mmio={:#}, base={:#x}, len={:#x}, add={:#}",
            handle, mmio, base, len, add
        );
        let proc = self.thread.proc();
        proc.get_object::<Resource>(handle)?
            .validate(ResourceKind::ROOT)?;
        let addr_space = if mmio {
            PciAddrSpace::MMIO
        } else {
            PciAddrSpace::PIO
        };
        if add {
            PCIeBusDriver::add_bus_region(base, len, addr_space)
        } else {
            PCIeBusDriver::sub_bus_region(base, len, addr_space)
        }
    }

    #[allow(clippy::too_many_arguments, unused_variables, unused_mut)]
    pub fn sys_pci_cfg_pio_rw(
        &self,
        handle: HandleValue,
        bus: u8,
        dev: u8,
        func: u8,
        offset: u8,
        mut value_ptr: UserInOutPtr<u32>,
        width: usize,
        write: bool,
    ) -> ZxResult {
        info!(
            "pci.cfg_pio_rw: handle={:#x}, addr={:x}:{:x}:{:x}, offset={:#x}, width={:#x}, write={:#}",
            handle, bus, dev, func, offset, width, write
        );
        cfg_if::cfg_if! {
            if #[cfg(all(target_arch = "x86_64", target_os = "none"))] {
                use zircon_object::dev::pci::{pio_config_read, pio_config_write};
                let proc = self.thread.proc();
                proc.get_object::<Resource>(handle)?
                    .validate(ResourceKind::ROOT)?;
                if write {
                    let value = value_ptr.read()?;
                    pio_config_write(bus, dev, func, offset, value, width)?;
                } else {
                    let value = pio_config_read(bus, dev, func, offset, width)?;
                    value_ptr.write(value)?;
                }
                Ok(())
            } else {
                Err(ZxError::NOT_SUPPORTED)
            }
        }
    }

    // TODO: review
    pub fn sys_pci_init(&self, handle: HandleValue, init_buf: usize, len: u32) -> ZxResult {
        info!(
            "pci.init: handle={:#x}, init_buf={:#x}, len={:#x}",
            handle, init_buf, len
        );
        let proc = self.thread.proc();
        proc.get_object::<Resource>(handle)?
            .validate(ResourceKind::ROOT)?;
        if len > PCI_INIT_ARG_MAX_SIZE as u32 {
            return Err(ZxError::INVALID_ARGS);
        }
        const HEADER_SIZE: usize = core::mem::size_of::<PciInitArgsHeader>();
        const ADDR_WINDOWS_SIZE: usize = core::mem::size_of::<PciInitArgsAddrWindows>();
        let mut arg_header = UserInPtr::<PciInitArgsHeader>::from(init_buf).read()?;
        let expected_len = HEADER_SIZE + arg_header.addr_window_count as usize * ADDR_WINDOWS_SIZE;
        if len != expected_len as u32 {
            return Err(ZxError::INVALID_ARGS);
        }
        let mut addr_windows = UserInPtr::<PciInitArgsAddrWindows>::from(init_buf + HEADER_SIZE)
            .read_array(arg_header.addr_window_count as usize)?;
        // `num_irqs` is a user field and indexes a fixed `[PciInitArgsIrqs; 224]`
        // inside the header, so an out-of-range count is a kernel
        // index-out-of-bounds panic rather than an error. `len` above does not
        // bound it: the length check is about the address windows that follow the
        // header, and the irq array is inside the header whatever `num_irqs`
        // says. Zircon refuses the call.
        if arg_header.num_irqs as usize > PCI_MAX_IRQS {
            return Err(ZxError::INVALID_ARGS);
        }
        arg_header.configure_interrupt()?;
        if arg_header.addr_window_count != 1 {
            return Err(ZxError::INVALID_ARGS); // for non DesignWare Controller
        }
        let addr_win = &mut addr_windows[0];
        if addr_win.bus_start != 0 || addr_win.bus_start > addr_win.bus_end {
            return Err(ZxError::INVALID_ARGS);
        }
        // Some systems will report overly large PCIe config regions
        // that collide with architectural registers.
        #[cfg(target_arch = "x86_64")]
        if let Some((size, bus_end)) =
            clamp_ecam_window(addr_win.base, addr_win.bus_start, addr_win.bus_end)?
        {
            addr_win.size = size;
            addr_win.bus_end = bus_end;
        }
        if addr_win.cfg_space_type == PCI_CFG_SPACE_TYPE_MMIO {
            if addr_win.size < PCIE_ECAM_BYTES_PER_BUS
                || addr_win.size / PCIE_ECAM_BYTES_PER_BUS
                    > PCIE_MAX_BUSSES - addr_win.bus_start as usize
            {
                return Err(ZxError::INVALID_ARGS);
            }
            let addr_provider = Arc::new(MmioPcieAddressProvider::default());
            addr_provider.add_ecam(PciEcamRegion {
                phys_base: addr_win.base,
                size: addr_win.size,
                bus_start: addr_win.bus_start,
                bus_end: addr_win.bus_end,
            })?;
            PCIeBusDriver::set_address_translation_provider(addr_provider)?;
        } else if addr_win.cfg_space_type == PCI_CFG_SPACE_TYPE_PIO {
            let addr_provider = Arc::new(PmioPcieAddressProvider);
            PCIeBusDriver::set_address_translation_provider(addr_provider)?;
        } else {
            return Err(ZxError::INVALID_ARGS);
        }
        PCIeBusDriver::add_root(0, arg_header.dev_pin_to_global_irq)?;
        PCIeBusDriver::start_bus_driver()?;
        Ok(())
    }

    pub fn sys_pci_map_interrupt(
        &self,
        dev: HandleValue,
        irq: i32,
        mut out_handle: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!("pci.map_interrupt: handle={:#x}, irq={:#x}", dev, irq);
        let proc = self.thread.proc();
        let dev = proc.get_object_with_rights::<PcieDeviceKObject>(dev, Rights::READ)?;
        let interrupt = dev.map_interrupt(irq)?;
        install_handle(
            proc,
            Handle::new(interrupt, Rights::DEFAULT_PCI_INTERRUPT),
            &mut out_handle,
        )
    }

    pub fn sys_pci_get_nth_device(
        &self,
        handle: HandleValue,
        index: u32,
        mut out_info: UserOutPtr<PcieDeviceInfo>,
        mut out_handle: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!(
            "pci.get_nth_device: handle={:#x}, index={:#x}",
            handle, index,
        );
        let proc = self.thread.proc();
        proc.get_object::<Resource>(handle)?
            .validate(ResourceKind::ROOT)?;
        let (info, device) = PCIeBusDriver::get_nth_device(index as usize)?;
        out_info.write(info)?;
        install_handle(
            proc,
            Handle::new(device, Rights::DEFAULT_DEVICE),
            &mut out_handle,
        )
    }

    pub fn sys_pci_get_bar(
        &self,
        handle: HandleValue,
        bar_num: u32,
        mut out_bar: UserOutPtr<PciBar>,
        mut out_handle: UserOutPtr<HandleValue>,
    ) -> ZxResult {
        info!("pci.get_bar: handle={:#x}, bar_num={:#x}", handle, bar_num);
        let proc = self.thread.proc();
        let device =
            proc.get_object_with_rights::<PcieDeviceKObject>(handle, Rights::READ | Rights::WRITE)?;
        let info = device.get_bar(bar_num)?;
        let mut bar_ = PciBar {
            id: 0,
            size: info.size as usize,
            bar_type: if info.is_mmio { 1 } else { 2 },
            addr: 0,
        };
        if info.is_mmio {
            let vmo = VmObject::new_physical(info.bus_addr as usize, pages(info.size as usize));
            install_handle(proc, Handle::new(vmo, Rights::DEFAULT_VMO), &mut out_handle)?;
            device.enable_mmio()?;
        } else {
            bar_.addr = info.bus_addr;
            device.enable_pio()?;
        }
        out_bar.write(bar_)?;
        Ok(())
    }

    pub fn sys_pci_enable_bus_master(&self, handle: HandleValue, enable: bool) -> ZxResult {
        info!("pci.get_bar: handle={:#x}, enable={}", handle, enable);
        let proc = self.thread.proc();
        let device = proc.get_object_with_rights::<PcieDeviceKObject>(handle, Rights::WRITE)?;
        device.enable_master(enable)
    }

    pub fn sys_pci_query_irq_mode(
        &self,
        handle: HandleValue,
        mode: u32,
        mut out_max_irqs: UserOutPtr<u32>,
    ) -> ZxResult {
        let mode = PcieIrqMode::try_from(mode).map_err(|_| ZxError::INVALID_ARGS)?;
        info!("pci.query_irq_mode: handle={:#x}, mode={:?}", handle, mode);
        let proc = self.thread.proc();
        let device = proc.get_object_with_rights::<PcieDeviceKObject>(handle, Rights::READ)?;
        let caps = device.get_irq_mode_capabilities(mode)?;
        out_max_irqs.write(caps.max_irqs)?;
        Ok(())
    }

    pub fn sys_pci_set_irq_mode(
        &self,
        handle: HandleValue,
        mode: u32,
        requested_irq_count: u32,
    ) -> ZxResult {
        let mode = PcieIrqMode::try_from(mode).map_err(|_| ZxError::INVALID_ARGS)?;
        info!(
            "pci.set_irq_mode: handle={:#x}, mode={:?}, requested_irq_count={:#x}",
            handle, mode, requested_irq_count
        );
        let proc = self.thread.proc();
        let device = proc.get_object_with_rights::<PcieDeviceKObject>(handle, Rights::WRITE)?;
        device.set_irq_mode(mode, requested_irq_count as usize)
    }

    pub fn sys_pci_config_read(
        &self,
        handle: HandleValue,
        offset: usize,
        width: usize,
        mut out_val: UserOutPtr<u32>,
    ) -> ZxResult {
        info!(
            "pci.config_read: handle={:#x}, offset={:x}, width={:x}",
            handle, offset, width
        );
        let proc = self.thread.proc();
        let device =
            proc.get_object_with_rights::<PcieDeviceKObject>(handle, Rights::READ | Rights::WRITE)?;
        let value = device.config_read(offset, width)?;
        out_val.write(value)?;
        Ok(())
    }

    pub fn sys_pci_config_write(
        &self,
        handle: HandleValue,
        offset: usize,
        width: usize,
        value: u32,
    ) -> ZxResult {
        info!(
            "pci.config_write: handle={:#x}, offset={:x}, width={:x}, value={:x}",
            handle, offset, width, value
        );
        let proc = self.thread.proc();
        let device =
            proc.get_object_with_rights::<PcieDeviceKObject>(handle, Rights::READ | Rights::WRITE)?;
        device.config_write(offset, width, value)?;
        Ok(())
    }
}

#[repr(C)]
pub struct PciBar {
    id: u32,
    bar_type: u32,
    size: usize,
    addr: u64,
}

/// Trim an ECAM window that runs into the architectural registers below 4 GiB.
///
/// Some firmware reports a PCIe config region far larger than the machine has,
/// overlapping the HPET/IOAPIC block at `0xfec0_0000`. Zircon rounds the
/// surviving length DOWN to whole buses and recomputes the last bus number.
/// Returns `None` when the window ends below the limit and needs no trimming.
///
/// `HIGH_LIMIT` and the bus arithmetic live here rather than inline in
/// `sys_pci_init` so they can be tested: reaching them through the syscall needs
/// the root resource and a user buffer, and what went wrong in them is pure
/// arithmetic.
#[cfg(target_arch = "x86_64")]
fn clamp_ecam_window(base: u64, bus_start: u8, bus_end: u8) -> ZxResult<Option<(usize, u8)>> {
    const HIGH_LIMIT: u64 = 0xfec0_0000;
    // `sys_pci_init` refuses an inverted range before it gets here; the helper
    // says so itself so the subtraction below cannot wrap, which would turn a
    // 256 MiB window into a 4 GiB one.
    if bus_end < bus_start {
        return Err(ZxError::INVALID_ARGS);
    }
    let num_buses = (bus_end - bus_start) as u64 + 1;
    // `base` comes from userspace, so the window's end can overflow rather than
    // merely exceed the limit.
    let end = num_buses
        .checked_mul(PCIE_ECAM_BYTES_PER_BUS as u64)
        .and_then(|len| base.checked_add(len))
        .ok_or(ZxError::INVALID_ARGS)?;
    if end <= HIGH_LIMIT {
        return Ok(None);
    }
    if HIGH_LIMIT < base {
        return Err(ZxError::INVALID_ARGS);
    }
    // ROUNDDOWN to whole buses: mask off the low bits, `& !(N - 1)`. Masking
    // with `& (N - 1)` keeps the sub-megabyte REMAINDER instead, which is
    // always less than one bus -- so the window came out describing zero buses
    // and the bus count below went to `0 + 0 - 1`, an underflowing `usize`:
    // a panic in a debug kernel, `usize::MAX` in release.
    let size = ((HIGH_LIMIT - base) & !(PCIE_ECAM_BYTES_PER_BUS as u64 - 1)) as usize;
    let buses = size / PCIE_ECAM_BYTES_PER_BUS;
    // No whole bus survives the trim, so there is no window to describe. Saying
    // so is what keeps the subtraction below from wrapping, and it is the answer
    // the MMIO check further down would reach anyway.
    if buses == 0 {
        return Err(ZxError::INVALID_ARGS);
    }
    let new_bus_end = buses + bus_start as usize - 1;
    // Belt and braces: the trim only ever shortens the range, so this cannot
    // fire for a range that fits in `u8` to begin with. Zircon checks it, so we
    // keep it.
    if new_bus_end >= PCIE_MAX_BUSSES {
        return Err(ZxError::INVALID_ARGS);
    }
    Ok(Some((size, new_bus_end as u8)))
}

#[cfg(all(test, target_arch = "x86_64"))]
mod ecam_clamp_tests {
    use super::*;

    /// One bus is 1 MiB of config space.
    const BUS: u64 = PCIE_ECAM_BYTES_PER_BUS as u64;

    #[test]
    fn a_window_that_ends_below_the_limit_is_left_alone() {
        // 4 buses starting at 16 MiB ends far below 0xfec0_0000.
        assert_eq!(clamp_ecam_window(0x0100_0000, 0, 3), Ok(None));
        // And one that ends exactly on the limit is still untouched.
        assert_eq!(clamp_ecam_window(0xfec0_0000 - BUS, 0, 0), Ok(None));
    }

    /// The trimmed window is a whole number of buses, and the bus count follows
    /// it. This is the case that used to underflow: the mask kept the remainder
    /// instead of rounding down, so the window described zero buses and the last
    /// bus number was computed as `0 - 1`.
    #[test]
    fn a_window_past_the_limit_is_trimmed_to_whole_buses() {
        // Firmware claims 256 buses, but only four and a half megabytes of the
        // window sit below the limit.
        let base = 0xfec0_0000 - 4 * BUS - BUS / 2;
        let (size, bus_end) = clamp_ecam_window(base, 0, 255)
            .expect("a trimmable window")
            .expect("it needed trimming");
        assert_eq!(size as u64, 4 * BUS, "the half bus was not rounded away");
        assert_eq!(size as u64 % BUS, 0, "the window is not whole buses");
        assert_eq!(bus_end, 3, "the last bus does not match the length");
        // The length and the bus range agree, which is what the MMIO check
        // further down relies on.
        assert_eq!(size / PCIE_ECAM_BYTES_PER_BUS, bus_end as usize + 1);
    }

    /// The trim only ever shortens the range, so the last bus it names is inside
    /// the range firmware asked for. This is why the `PCIE_MAX_BUSSES` guard at
    /// the end of the function cannot fire: removing it leaves every test green,
    /// and that is the reason, not a hole in the tests.
    #[test]
    fn the_trim_never_names_a_bus_past_the_requested_range() {
        for requested_end in [1u8, 7, 64, 255] {
            let base = 0xfec0_0000 - BUS * requested_end as u64;
            if let Ok(Some((_, bus_end))) = clamp_ecam_window(base, 0, requested_end) {
                assert!(
                    bus_end <= requested_end,
                    "asked for {} buses, got bus {}",
                    requested_end,
                    bus_end
                );
            }
        }
    }

    /// An inverted range is refused instead of wrapping the bus count. The
    /// syscall checks it first, but the helper is the thing under test.
    #[test]
    fn an_inverted_bus_range_is_refused() {
        assert_eq!(
            clamp_ecam_window(0x0100_0000, 4, 3),
            Err(ZxError::INVALID_ARGS)
        );
    }

    /// A window whose surviving part is smaller than one bus is refused rather
    /// than described as an empty one. Returning a zero-length window is what
    /// made the bus arithmetic wrap.
    #[test]
    fn a_window_with_less_than_one_bus_left_is_refused() {
        // Half a megabyte below the limit.
        assert_eq!(
            clamp_ecam_window(0xfec0_0000 - BUS / 2, 0, 3),
            Err(ZxError::INVALID_ARGS)
        );
        // And one that starts exactly on the limit.
        assert_eq!(
            clamp_ecam_window(0xfec0_0000, 0, 0),
            Err(ZxError::INVALID_ARGS)
        );
    }

    #[test]
    fn a_window_that_starts_above_the_limit_is_refused() {
        assert_eq!(
            clamp_ecam_window(0xffff_0000, 0, 0),
            Err(ZxError::INVALID_ARGS)
        );
    }

    /// A base near the top of the address space makes the window's end
    /// overflow, not merely exceed the limit. Userspace picks the base.
    ///
    /// Only the addition can overflow. The bus count is at most 256 and a bus is
    /// 1 MiB, so the multiplication tops out at 256 MiB: turning its
    /// `checked_mul` into a wrapping one leaves every test green because it
    /// cannot wrap, not because nothing checks it.
    #[test]
    fn a_window_whose_end_overflows_is_refused() {
        assert_eq!(
            clamp_ecam_window(u64::MAX, 0, 255),
            Err(ZxError::INVALID_ARGS)
        );
        assert_eq!(
            clamp_ecam_window(u64::MAX - BUS, 0, 0),
            Err(ZxError::INVALID_ARGS)
        );
    }

    /// The trimmed window can never name a bus past the end of the bus space.
    #[test]
    fn the_trimmed_window_stays_inside_the_bus_space() {
        for base in [0u64, 0x1000_0000, 0xe000_0000, 0xfeb0_0000] {
            if let Ok(Some((size, bus_end))) = clamp_ecam_window(base, 0, 255) {
                assert!(
                    (bus_end as usize) < PCIE_MAX_BUSSES,
                    "base {:#x} named bus {}",
                    base,
                    bus_end
                );
                assert!(size >= PCIE_ECAM_BYTES_PER_BUS);
            }
        }
    }
}
