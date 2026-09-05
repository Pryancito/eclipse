use kernel_hal::KernelConfig;
use rboot::BootInfo;

/// rboot hands `BootInfo.cmdline` as a *physical* pointer (identity-mapped
/// while the bootloader runs). After the kernel page tables come up that
/// low address is unmapped, so reading `cfg.cmdline` later — notably from
/// the panic banner's `FB_ROT180` flag check — is a kernel #PF on a
/// userspace-looking vaddr (e.g. `0x7e99_xxxx`) that isolation then kills
/// as the current process. Snapshot into kernel BSS via the physmap, which
/// is already live here (same conversion as `fb_addr` just below). No heap
/// yet, so a static buffer; 4 KiB is well above any rboot.conf cmdline.
fn snapshot_cmdline(boot_info: &BootInfo) -> &'static str {
    const MAX: usize = 4096;
    static mut STORAGE: [u8; MAX] = [0; MAX];
    let n = boot_info.cmdline.len().min(MAX);
    if n == 0 {
        return "";
    }
    let virt = boot_info
        .physical_memory_offset
        .wrapping_add(boot_info.cmdline.as_ptr() as u64) as *const u8;
    unsafe {
        let dst = core::ptr::addr_of_mut!(STORAGE) as *mut u8;
        core::ptr::copy_nonoverlapping(virt, dst, n);
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(dst as *const u8, n))
    }
}

#[unsafe(no_mangle)]
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
        cmdline: snapshot_cmdline(boot_info),
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
