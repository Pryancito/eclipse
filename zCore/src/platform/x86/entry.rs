use kernel_hal::KernelConfig;
use rboot::BootInfo;

#[no_mangle]
pub extern "C" fn _start(boot_info: &'static BootInfo) -> ! {
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
