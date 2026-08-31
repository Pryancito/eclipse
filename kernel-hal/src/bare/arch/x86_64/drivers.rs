use alloc::sync::Arc;
use core::ptr::NonNull;

#[cfg(feature = "graphic")]
use alloc::format;

use acpi::platform::address::AddressSpace;
use acpi::sdt::Signature;
use acpi::{AcpiHandler, AcpiTables, PhysicalMapping};
use spin::Mutex;
use x86_64::instructions::port::Port;
use zcore_drivers::irq::x86::Apic;
use zcore_drivers::prelude::{IrqPolarity, IrqTriggerMode};
use zcore_drivers::scheme::{IrqScheme, SchemeUpcast};
use zcore_drivers::uart::{BufferedUart, Uart16550Pmio};
use zcore_drivers::{Device, DeviceResult};

use super::trap;
use crate::drivers;

const PAGE_SIZE: usize = 4096;
const ACPI_PM1_PWRBTN: u16 = 1 << 8;

#[derive(Clone)]
struct AcpiMapHandler {
    phys_to_virt: fn(usize) -> usize,
}

impl AcpiHandler for AcpiMapHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let aligned_start = physical_address & !(PAGE_SIZE - 1);
        let aligned_end = (physical_address + size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        PhysicalMapping::new(
            physical_address,
            unsafe { NonNull::new_unchecked((self.phys_to_virt)(physical_address) as *mut T) },
            size,
            aligned_end - aligned_start,
            self.clone(),
        )
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}
}

#[derive(Clone, Copy)]
struct AcpiPowerButton {
    pm1a_status: u16,
    pm1a_enable: u16,
    pm1b_status: Option<u16>,
    pm1b_enable: Option<u16>,
}

impl AcpiPowerButton {
    fn from_fadt(fadt: &acpi::fadt::Fadt) -> Option<(usize, Self)> {
        let sci = fadt.sci_interrupt as usize;
        let pm1a = fadt.pm1a_event_block().ok()?;
        if sci == 0
            || pm1a.address_space != AddressSpace::SystemIo
            || pm1a.address == 0
            || pm1a.bit_width < 32
        {
            return None;
        }
        let pm1a = pm1a.address as u16;
        let pm1b = fadt
            .pm1b_event_block()
            .ok()
            .flatten()
            .filter(|gas| {
                gas.address_space == AddressSpace::SystemIo
                    && gas.address != 0
                    && gas.bit_width >= 32
            })
            .map(|gas| gas.address as u16);
        Some((
            sci,
            Self {
                pm1a_status: pm1a,
                pm1a_enable: pm1a + 2,
                pm1b_status: pm1b,
                pm1b_enable: pm1b.map(|base| base + 2),
            },
        ))
    }

    fn read(port: u16) -> u16 {
        unsafe { Port::<u16>::new(port).read() }
    }

    fn clear(status_port: u16) {
        unsafe { Port::<u16>::new(status_port).write(ACPI_PM1_PWRBTN) }
    }

    fn asserted_pair(status_port: u16, enable_port: u16) -> bool {
        let status = Self::read(status_port);
        let enabled = Self::read(enable_port);
        (status & enabled & ACPI_PM1_PWRBTN) != 0
    }

    fn handle_irq(&self) -> bool {
        let mut pressed = false;
        if Self::asserted_pair(self.pm1a_status, self.pm1a_enable) {
            Self::clear(self.pm1a_status);
            pressed = true;
        }
        if let (Some(status), Some(enable)) = (self.pm1b_status, self.pm1b_enable) {
            if Self::asserted_pair(status, enable) {
                Self::clear(status);
                pressed = true;
            }
        }
        pressed
    }
}

static ACPI_POWER_BUTTON: Mutex<Option<AcpiPowerButton>> = Mutex::new(None);

fn acpi_power_button_irq() {
    if ACPI_POWER_BUTTON
        .lock()
        .as_ref()
        .is_some_and(|power| power.handle_irq())
    {
        crate::klog_warn!("[acpi] power button pressed; powering off");
        crate::cpu::reset();
    }
}

fn init_acpi_power_button(irq: &Arc<Apic>) {
    let rsdp = super::special::pc_firmware_tables().0 as usize;
    if rsdp == 0 {
        return;
    }
    let tables = match unsafe {
        AcpiTables::from_rsdp(
            AcpiMapHandler {
                phys_to_virt: crate::mem::phys_to_virt,
            },
            rsdp,
        )
    } {
        Ok(t) => t,
        Err(e) => {
            crate::klog_warn!("[acpi] power button disabled: ACPI parse failed: {:?}", e);
            return;
        }
    };
    let fadt = match unsafe { tables.get_sdt::<acpi::fadt::Fadt>(Signature::FADT) } {
        Ok(Some(t)) => t,
        Ok(None) => {
            crate::klog_warn!("[acpi] power button disabled: no FADT");
            return;
        }
        Err(e) => {
            crate::klog_warn!("[acpi] power button disabled: no FADT: {:?}", e);
            return;
        }
    };
    let Some((sci, power)) = AcpiPowerButton::from_fadt(&fadt) else {
        crate::klog_warn!("[acpi] power button disabled: SCI/PM1 event block unavailable");
        return;
    };
    if irq
        .configure(sci, IrqTriggerMode::Level, IrqPolarity::ActiveLow)
        .and_then(|_| irq.register_handler(sci, Arc::new(acpi_power_button_irq)))
        .and_then(|_| irq.unmask(sci))
        .is_err()
    {
        crate::klog_warn!("[acpi] power button disabled: failed to route SCI {}", sci);
        return;
    }
    *ACPI_POWER_BUTTON.lock() = Some(power);
}

pub(super) fn init_early() -> DeviceResult {
    let uart = Arc::new(Uart16550Pmio::new(0x3F8));
    drivers::add_device(Device::Uart(BufferedUart::new(uart)));
    let uart = Arc::new(Uart16550Pmio::new(0x2F8));
    drivers::add_device(Device::Uart(BufferedUart::new(uart)));
    Ok(())
}

fn boot_progress(p: u32) {
    #[cfg(feature = "graphic")]
    crate::console::early_progress_bar(p);
    #[cfg(not(feature = "graphic"))]
    let _ = p;
}

pub(super) fn init() -> DeviceResult {
    boot_progress(81);
    zcore_drivers::init();
    boot_progress(82);
    Apic::init_local_apic_bsp(crate::mem::phys_to_virt);
    let irq = Arc::new(Apic::new(
        super::special::pc_firmware_tables().0 as usize,
        crate::mem::phys_to_virt,
    ));
    let uarts = drivers::all_uart();
    if let Some(u) = uarts.try_get(0) {
        irq.register_device(trap::X86_ISA_IRQ_COM1, u.clone().upcast())?;
        irq.unmask(trap::X86_ISA_IRQ_COM1)?;

        if let Some(u) = uarts.try_get(1) {
            irq.register_device(trap::X86_ISA_IRQ_COM2, u.clone().upcast())?;
            irq.unmask(trap::X86_ISA_IRQ_COM2)?;
        }
    }

    // PS/2 Keyboard and Mouse initialization and registration
    let ps2_input = Arc::new(zcore_drivers::input::Ps2Input::new());
    irq.register_device(trap::X86_ISA_IRQ_KEYBOARD, ps2_input.clone().upcast())?;
    irq.unmask(trap::X86_ISA_IRQ_KEYBOARD)?;
    irq.register_device(trap::X86_ISA_IRQ_MOUSE, ps2_input.clone().upcast())?;
    irq.unmask(trap::X86_ISA_IRQ_MOUSE)?;
    drivers::add_device(Device::Input(ps2_input));
    // ACPI power button (SCI) — OPT-IN via `acpi.powerbtn`, default OFF.
    //
    // The SCI is a LEVEL-triggered, SHARED ACPI interrupt: on real hardware the
    // firmware routes General Purpose Events (embedded controller, thermal
    // zones, lid, ...) to it, not just the power button. This handler only
    // reads/clears the PM1 power-button status bit; it has no AML interpreter
    // or GPE dispatcher, so it cannot service or CLEAR any other source. On a
    // level-triggered line an unhandled GPE keeps the line asserted, so once
    // unmasked the SCI re-fires forever -> an interrupt storm that starves the
    // PS/2 mouse (IRQ 12, unmasked just above) and keyboard (IRQ 1): the
    // pointer froze / stopped drawing on real hardware. QEMU rarely raises
    // those GPEs, so it only bit bare metal. Until the handler drains the GPE
    // status blocks (GPE0/GPE1 STS write-1-to-clear) the safe default is to
    // leave the SCI MASKED — input is never starved. Opt in with `acpi.powerbtn`
    // once GPE draining lands and can be validated on hardware.
    if crate::KCONFIG.cmdline.contains("acpi.powerbtn") {
        init_acpi_power_button(&irq);
    }

    use x2apic::lapic::{TimerDivide, TimerMode};

    irq.register_local_apic_handler(trap::X86_INT_APIC_TIMER, Arc::new(super::trap::super_timer))?;
    // IPI vector: 0xf3 = X86_INT_LOCAL_APIC_BASE + 3
    irq.register_local_apic_handler(
        0xf3,
        Arc::new(|| {
            // Either a TLB shootdown (queue entries: invlpg/flush + publish the
            // satisfied generation) or a pure reschedule wake (empty queue: the
            // interrupt already broke `hlt`, nothing else to do).
            crate::common::ipi::tlb_shootdown_ack();
        }),
    )?;

    if Apic::local_apic_ready() {
        // SAFETY: called once on BSP during primary_init
        let lapic = Apic::local_apic();
        lapic.set_timer_mode(TimerMode::Periodic);
        lapic.set_timer_divide(TimerDivide::Div256); // indeed it is Div1, the name is confusing.
        let cycles =
            super::cpu::cpu_frequency() as u64 * 1_000_000 / super::super::timer::TICKS_PER_SEC;
        lapic.set_timer_initial(cycles as u32);
        lapic.disable_timer();
    } else {
        crate::klog_warn!("[drivers] LAPIC unavailable — APIC timer left disabled");
    }

    #[cfg(all(not(feature = "no-pci"), feature = "xhci-usb-hid"))]
    {
        use zcore_drivers::usb::xhci_hid;
        let irq_apic: Arc<dyn zcore_drivers::scheme::IrqScheme> = irq.clone();
        xhci_hid::pci_set_irq_host(irq_apic);
    }

    drivers::add_device(Device::Irq(irq.clone()));
    boot_progress(83);

    // Register the UEFI GOP framebuffer as a Display device EARLY and
    // UNCONDITIONALLY, before the fallible PCI / nvidia bring-up below.
    //
    // linux-object's software-KMS path (drm.rs `software_kms_active`) is what
    // actually drives the compositor: it blits wlroots' dumb buffer into the
    // GOP framebuffer via `scanout`. That path is gated purely on
    // `drivers::all_display().first().is_some()`. If we only registered the
    // display in the graphics-console block further down, it would run AFTER
    // `pci::init(...)?` (which can return Err and bail out of `init`) and
    // behind an else-arm, so a fallible/reordered PCI path could leave
    // `all_display()` empty. In that state GETRESOURCES hands wlroots the
    // nvidia stub CRTC (whose `page_flip` is a no-op) and the screen stays
    // black. Registering here guarantees a real scanout target regardless of
    // how PCI / nvidia bring-up goes.
    #[cfg(feature = "graphic")]
    {
        use crate::KCONFIG;
        use zcore_drivers::display::UefiDisplay;
        use zcore_drivers::prelude::{ColorFormat, DisplayInfo};

        let (width, height) = KCONFIG.fb_mode.resolution();
        let stride = KCONFIG.fb_mode.stride();

        // Publish the boot framebuffer geometry + real panel EDID to the
        // display layer up front so the DRM connector / blit code sees them
        // regardless of the PCI path (also covers the `no-pci` build).
        zcore_drivers::display::set_boot_fb_info(
            KCONFIG.fb_addr,
            width as u32,
            height as u32,
            (stride * 4) as u32,
        );
        zcore_drivers::display::set_boot_edid(&KCONFIG.fb_edid, KCONFIG.fb_edid_size);

        if KCONFIG.fb_addr != 0
            && width != 0
            && height != 0
            && crate::drivers::all_display().first().is_none()
        {
            let display = Arc::new(UefiDisplay::new(DisplayInfo {
                width: width as _,
                height: height as _,
                pitch: (stride * 4) as u32,
                format: ColorFormat::ARGB8888,
                fb_base_vaddr: crate::mem::phys_to_virt(KCONFIG.fb_addr as usize),
                fb_size: KCONFIG.fb_size as usize,
            }));
            crate::drivers::add_device(Device::Display(display));
        }
    }

    #[cfg(not(feature = "no-pci"))]
    {
        // PCI scan
        use crate::vm::{GenericPageTable, PageTable};
        use crate::{CachePolicy, MMUFlags, PhysAddr, VirtAddr};
        use zcore_drivers::builder::IoMapper;
        use zcore_drivers::bus::pci;

        struct IoMapperImpl;
        impl IoMapper for IoMapperImpl {
            fn query_or_map(&self, paddr: PhysAddr, size: usize) -> Option<VirtAddr> {
                let vaddr = crate::mem::phys_to_virt(paddr);
                let mut pt = PageTable::from_current();

                if let Ok((paddr_mapped, _, _)) = pt.query(vaddr) {
                    if paddr_mapped == paddr {
                        return Some(vaddr);
                    }
                }

                let size = (size + 0xfff) & !0xfff;
                let flags = MMUFlags::READ
                    | MMUFlags::WRITE
                    | MMUFlags::DEVICE
                    | MMUFlags::from_bits_truncate(CachePolicy::UncachedDevice as usize);

                // debug!, not warn!: fires for every PCI BAR of every device at
                // boot; at LOG=warn each line pays the per-byte UART spin.
                debug!(
                    "[xhci] Mapeando BAR PCI en PT kernel: {:#x} -> {:#x} (size: {:#x})",
                    paddr, vaddr, size
                );
                if let Err(e) = pt.map_cont(vaddr, size, paddr, flags) {
                    crate::klog_err!("[xhci] failed to map PCI BAR: {:?}", e);
                    return None;
                }

                core::mem::forget(pt);
                Some(vaddr)
            }
        }

        // Boot framebuffer geometry + panel EDID were already published to the
        // display layer in the early graphic block above (before this fallible
        // PCI init), so display drivers already have native-resolution info.

        boot_progress(84);
        let pci_devs = pci::init(Some(Arc::new(IoMapperImpl)))?;
        // Do NOT drain deferred jobs here — e1000e PHY/MAC init runs in the idle
        // loop / NIC poll path so boot progress is not blocked at 84% or 87%.
        boot_progress(87);
        for d in pci_devs.into_iter() {
            drivers::add_device(d);
        }

        // Finish deferred NIC work without starving HID at login.
        for _ in 0..2 {
            zcore_drivers::utils::deferred_job::drain_deferred_jobs_max(1);
        }

        // Finish MSI registrations for USB
        #[cfg(feature = "xhci-usb-hid")]
        {
            use zcore_drivers::usb::xhci_hid;
            xhci_hid::pci_set_irq_host(irq.clone());
            let _ = xhci_hid::pci_finish_msi_registrations();
        }

        // Finish MSI registrations for Net
        {
            use zcore_drivers::net;
            net::pci_set_irq_host(irq.clone());
            let _ = net::pci_finish_msi_registrations();
        }
    }

    boot_progress(88);

    // Re-assert write-combining on the boot framebuffer AFTER the PCI scan.
    // The early retype in `primary_init` runs before PCI bring-up: on real
    // hardware the GOP surface lives inside the console GPU's BAR1, whose
    // physmap alias is not premapped by rboot, so that pass finds NotMapped
    // and converts nothing -- and the NVIDIA bring-up then `query_or_map`s
    // the whole BAR1 as UncachedDevice, leaving the scanout pages UC. A UC
    // blit moves ~42 MB/s (one bus transaction per store), which showed up
    // as a 99 ms present (7-11 FPS desktop). This second pass retypes those
    // now-existing 4 KiB PTEs to WC. Idempotent everywhere else (QEMU/VBox
    // fb pages are already WC from the early pass).
    super::pat::enable_framebuffer_wc();

    #[cfg(feature = "graphic")]
    let graphics_console_note = {
        use alloc::string::String;
        if let Some(display) = crate::drivers::all_display().first() {
            crate::console::init_graphic_console(display.clone());
            let _ = display.need_flush();
            let info = display.info();
            Some(format!("{} {}x{}", display.name(), info.width, info.height))
        } else {
            use crate::KCONFIG;
            use zcore_drivers::display::UefiDisplay;
            use zcore_drivers::prelude::{ColorFormat, DisplayInfo};

            let (width, height) = KCONFIG.fb_mode.resolution();
            let stride = KCONFIG.fb_mode.stride();
            if KCONFIG.fb_addr == 0 || width == 0 || height == 0 {
                crate::klog_warn!(
                    "[drivers] no framebuffer from bootloader (fb_addr={:#x}, {}x{}) — skipping graphic console",
                    KCONFIG.fb_addr, width, height
                );
                Some(String::from("unavailable (no bootloader framebuffer)"))
            } else {
                let display = Arc::new(UefiDisplay::new(DisplayInfo {
                    width: width as _,
                    height: height as _,
                    pitch: (stride * 4) as u32,
                    format: ColorFormat::ARGB8888,
                    fb_base_vaddr: crate::mem::phys_to_virt(KCONFIG.fb_addr as usize),
                    fb_size: KCONFIG.fb_size as usize,
                }));
                crate::drivers::add_device(Device::Display(display.clone()));
                crate::console::init_graphic_console(display.clone());
                Some(format!("uefi-gop {}x{}", width, height))
            }
        }
    };

    #[cfg(not(feature = "graphic"))]
    let graphics_console_note: Option<alloc::string::String> = None;

    drivers::klog_graphics_device_summary(graphics_console_note.as_deref());

    use crate::net;
    net::init();

    crate::klog_info!("Eclipse: drivers init complete");
    Ok(())
}
