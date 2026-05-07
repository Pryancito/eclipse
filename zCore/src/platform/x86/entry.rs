use kernel_hal::KernelConfig;
use rboot::BootInfo;

#[no_mangle]
pub extern "C" fn _start(boot_info: &'static BootInfo) -> ! {
    let info = boot_info.graphic_info;

    // Ultra-early marker: prove we entered the kernel on real hardware.
    // Draw a tiny progress bar update directly via GOP framebuffer, without KCONFIG.
    {
        let (sw, sh) = info.mode.resolution();
        let sw = sw as usize;
        let sh = sh as usize;
        let stride = info.mode.stride() as usize;
        let fb = info.fb_addr as *mut u32;
        let rot180 = boot_info.cmdline.contains("FB_ROT180=1")
            || boot_info.cmdline.contains("FB_ROT180=true")
            || boot_info.cmdline.contains("FB_ROT180=on")
            || boot_info.cmdline.contains("FB_ROT180");

        let bar_w: usize = 400;
        let bar_h: usize = 20;
        let x = sw.saturating_sub(bar_w) / 2;
        let y = sh.saturating_sub(bar_h) / 2;
        let progress = 52usize; // first kernel-visible tick after rboot's 50%
        let fill_w = (bar_w * progress) / 100;

        // White in 0x00RRGGBB (we only need it to be visible as a marker).
        let white: u32 = 0x00FF_FFFF;
        let black: u32 = 0x0000_0000;

        unsafe {
            // Fill a 1px border area to make it obvious.
            for yy in (y.saturating_sub(2))..(y + bar_h + 2).min(sh) {
                for xx in (x.saturating_sub(2))..(x + bar_w + 2).min(sw) {
                    let is_border = yy == y.saturating_sub(2)
                        || yy + 1 == (y + bar_h + 2).min(sh)
                        || xx == x.saturating_sub(2)
                        || xx + 1 == (x + bar_w + 2).min(sw);
                    if is_border {
                        let (mut px, mut py) = (xx, yy);
                        if rot180 {
                            px = sw.saturating_sub(1).saturating_sub(px);
                            py = sh.saturating_sub(1).saturating_sub(py);
                        }
                        core::ptr::write_volatile(fb.add(py * stride + px), white);
                    }
                }
            }
            // Fill progress portion.
            for yy in y..(y + bar_h).min(sh) {
                for xx in x..(x + fill_w).min(sw) {
                    let (mut px, mut py) = (xx, yy);
                    if rot180 {
                        px = sw.saturating_sub(1).saturating_sub(px);
                        py = sh.saturating_sub(1).saturating_sub(py);
                    }
                    core::ptr::write_volatile(fb.add(py * stride + px), white);
                }
                for xx in (x + fill_w).min(sw)..(x + bar_w).min(sw) {
                    let (mut px, mut py) = (xx, yy);
                    if rot180 {
                        px = sw.saturating_sub(1).saturating_sub(px);
                        py = sh.saturating_sub(1).saturating_sub(py);
                    }
                    core::ptr::write_volatile(fb.add(py * stride + px), black);
                }
            }
        }
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

        acpi_rsdp: boot_info.acpi2_rsdp_addr,
        smbios: boot_info.smbios_addr,
        ap_fn: crate::secondary_main,
    };
    crate::primary_main(config);
    unreachable!()
}
