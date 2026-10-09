use kernel_hal::KernelConfig;
use rboot::BootInfo;

/// The two halves of the progress bar have to agree on where the loader's range
/// ends, and this is the one place that can see both constants. A mismatch would
/// silently drop the loader's last marks -- the `ExitBootServices` pair, which is
/// exactly where a real machine hangs.
const _: () = assert!(kernel_hal::boot_marks::LOADER_SLOTS == rboot::LOADER_MARKS);

#[unsafe(no_mangle)]
pub extern "C" fn _start(boot_info: &'static BootInfo) -> ! {
    // The loader half of the boot timeline, first thing: the array lives in
    // `BootInfo`, on a heap the firmware still owns at this point, and rboot
    // measured things no kernel mark can see -- reading the kernel ELF and the
    // whole initramfs through the firmware's FAT driver. Raw TSC readings;
    // `primary_main` names the frequency to divide them by once it has one worth
    // trusting. See `kernel_hal::boot_marks`.
    kernel_hal::boot_marks::set_loader_marks(&boot_info.loader_marks);
    let info = boot_info.graphic_info;
    // Paint 52% *before* heap / klog / PIT calibration. rboot leaves the bar
    // at 51%; a stall in `memory::init` used to look like a failed jump.
    {
        let (w, h) = info.mode.resolution();
        let fb_vaddr = (boot_info.physical_memory_offset.wrapping_add(info.fb_addr)) as usize;
        kernel_hal::console::early_fb_prime(fb_vaddr, w, h, info.mode.stride());
        kernel_hal::console::early_progress_bar(52);
    }

    let config = KernelConfig {
        cmdline: boot_info.cmdline,
        initrd_start: boot_info.initramfs_addr,
        initrd_size: boot_info.initramfs_size,

        memory_map: boot_info.memory_map.as_slice(),
        phys_to_virt_offset: boot_info.physical_memory_offset as _,

        fb_mode: info.mode,
        fb_addr: info.fb_addr,
        fb_size: info.fb_size,
        fb_edid: boot_info.edid,
        fb_edid_size: boot_info.edid_size,

        acpi_rsdp: boot_info.acpi2_rsdp_addr,
        smbios: boot_info.smbios_addr,
        ap_fn: crate::secondary_main,
    };
    crate::primary_main(config);
    unreachable!()
}
