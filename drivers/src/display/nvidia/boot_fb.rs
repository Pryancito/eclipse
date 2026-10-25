//! The boot (UEFI GOP) framebuffer and the one line the boot prints about
//! the GPU that scans it out. Split from `nvidia.rs` for size only.

use super::{String, BOOT_FB_INFO};
use alloc::format;

#[derive(Debug, Clone, Copy)]
pub(super) struct BootFbInfo {
    pub(super) phys: u64,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pitch: u32,
}

pub fn set_boot_fb_info(phys: u64, width: u32, height: u32, pitch: u32) {
    *BOOT_FB_INFO.lock() = Some(BootFbInfo {
        phys,
        width,
        height,
        pitch,
    });
}

/// Physical address of the boot (UEFI GOP) framebuffer, if known. The GPU whose
/// BAR1 aperture contains this address is the one driving the console.
pub(super) fn boot_fb_phys() -> Option<u64> {
    BOOT_FB_INFO.lock().map(|b| b.phys)
}

/// Byte size of the boot (UEFI GOP) framebuffer (`pitch * height`), if known.
/// Used by the P2P CE path/tests, which run on the compute GPU and therefore
/// cannot read the console FB geometry from their own `self.info`.
pub(super) fn boot_fb_size() -> Option<u64> {
    BOOT_FB_INFO
        .lock()
        .map(|b| b.pitch as u64 * b.height as u64)
}

/// The one line the boot prints for an NVIDIA GPU left cold because it is
/// scanning out the console.
///
/// It used to print nothing at all, which is how a dual-card box came to
/// report `2 GPU node(s), 1 with the RM up` with no way to tell from the log
/// WHICH card stayed down or why: the attach never failed, it never ran. The
/// line carries the three facts that decide it -- the boot framebuffer
/// address and the BAR1 window it fell inside, which is the whole of
/// `drives_boot_display` -- plus the hatch that brings the card up anyway
/// (the deferred bring-up, which is the default on a multi-GPU box and
/// `nvidia.console_gpu` on a single-GPU one; see
/// `schedule_deferred_console_bringup`). Pure so it can be tested on a
/// machine with no GPU, which is every machine here.
pub(super) fn console_gpu_cold_line(
    bus: u8,
    device: u8,
    model: &str,
    boot_fb_phys: u64,
    bar1_phys: u64,
    bar1_len: u64,
) -> String {
    format!(
        "GPU {:02x}:{:02x}.0 {} es la GPU de consola (fb de arranque {:#x} dentro de BAR1 \
         {:#x}+{:#x}) — fuera del bring-up de arranque: su GSP se cuelga en el store \
         STARTCPU de SEC2 con el scanout vivo. Se sube despues, con el escritorio ya \
         arriba y el scanout pausado (nvidia.console_gpu; nvidia.noconsole_gpu lo apaga)",
        bus, device, model, boot_fb_phys, bar1_phys, bar1_len,
    )
}
