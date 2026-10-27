//! One bring-up for every card, the way Linux does it.
//!
//! Eclipse used to have two: `auto_bringup_compute` ran the state-load chain
//! on the cards that do NOT scan out, and the card that does was skipped
//! outright (`bringup_step6` refused it by name) and picked up much later by a
//! deferred task running a chain of its own. That split is what Moebius asked
//! to remove -- «nada de compute y console, va a ser como en linux» -- and his
//! reason is the one that matters: the userspace on top is Linux's, so what
//! the kernel below shows it should be what Linux shows it.
//!
//! Linux has no such split. Every card runs the same init; the one that is
//! primary *says so* to the RM (`RmDeterminePrimaryDevice` /
//! `RmSetConsolePreservationParams`, right before `kgspInitRm`) and is
//! prepared for its own firmware boot. Those are two facts about a card, asked
//! once inside one path -- not a second path.
//!
//! So what survives here is exactly those facts, and what they imply:
//!
//! * [`mark_primary_for_rm`] -- tell the RM this is the primary device with a
//!   live console in its BAR1, which is what Linux does at the same point.
//! * the `BootWindow` around the GSP boot, which only means anything on a card
//!   with a live raster.
//! * [`preserve_bar1_console`] -- keep that console mapped at the base of BAR1
//!   when the state load makes BAR1 virtual, as Linux does for efifb.
//! * [`recover_bar1`] -- say where BAR1 stands, and rebind it when it took an
//!   MMU fault, as the RM's interrupt handler does on Linux.
//!
//! What does not survive is any question of the form "is this the console
//! card, so do something else instead".

use super::{boot_fb::boot_fb_phys, BootFbInfo, NvidiaGpu, BOOT_FB_INFO};
use alloc::string::String;

/// Declare this GPU's real identity to the RM before its GSP boot, where
/// Linux declares it: it is the PRIMARY device, with a live UEFI GOP console
/// in its BAR1. Without it the `SET_GUEST_SYSTEM_INFO` RPC says
/// `bIsPrimary=false` and GSP-RM reserves no console region.
///
/// Idempotent -- plain property and field writes -- so a repeat costs
/// nothing. A failure is narrated and the boot continues: the mark is how the
/// firmware is *told*, not how it finds out, and a card that boots without it
/// is no worse off than one that never had the call.
pub(super) fn mark_primary_for_rm(gpu: &NvidiaGpu, tag: &str) -> String {
    let Some(device_instance) = *gpu.rm_device_instance.lock() else {
        return String::new();
    };
    let (console_size, at_bar1_base) = console_params(*BOOT_FB_INFO.lock(), gpu.bar1_phys);
    match nvidia_rm_sys::rm_init::mark_console_gpu(device_instance, console_size, at_bar1_base) {
        Ok(()) => alloc::format!(
            "[{}] identidad declarada al RM (PRIMARY_DEVICE, consola {} KiB, en la base de BAR1: {})\n",
            tag,
            console_size / 1024,
            at_bar1_base
        ),
        Err(status) => alloc::format!(
            "[{}] mark_console_gpu FALLO, NV_STATUS={:#x} (se arranca igual)\n",
            tag,
            status
        ),
    }
}

/// Before the console card's state load: ask the RM to keep the boot
/// console mapped at the base of BAR1 when it puts BAR1 into virtual
/// mode, as Linux does for efifb (`bPreserveBar1ConsoleEnabled`).
/// Without it nothing is mapped there, the kernel console's next write
/// faults BAR1, and with no interrupt servicing to rebind it every BAR1
/// access on the card reads `0xBAD0ACxx` from then on: the USERD of
/// every channel (so NVK's channels never run) and nvkms's mappings.
///
/// `None` on a card that scans nothing out, or whose console does not
/// start at the base of its BAR1; otherwise the line for the log.
pub(super) fn preserve_bar1_console(gpu: &NvidiaGpu, device_instance: u32) -> Option<String> {
    if !gpu.drives_boot_display() {
        return None;
    }
    let (size, at_bar1_base) = console_params(*BOOT_FB_INFO.lock(), gpu.bar1_phys);
    if size == 0 || !at_bar1_base {
        return None;
    }
    Some(
        match nvidia_rm_sys::rm_init::preserve_bar1_console(device_instance, size) {
            Ok(()) => alloc::format!(
                "[gpustep9]  consola de arranque ({} KiB de VRAM 0) mapeada en BAR1 0 para cuando BAR1 pase a virtual\n",
                size / 1024
            ),
            Err(status) => alloc::format!(
                "[gpustep9]  no se pudo mapear la consola en BAR1 0, NV_STATUS={:#x}: la primera escritura de la consola dejara BAR1 en fallo\n",
                status
            ),
        },
    )
}

/// Check BAR1 and rebind it when it took an MMU fault, as the RM's interrupt
/// handler does on Linux (Eclipse services no GPU interrupts, so nothing else
/// ever will). A fault counts when the MMU latched one, or when VRAM 0 read
/// through BAR1 comes back as the GPU's error answer: a GSP-RM that serviced
/// the fault itself has already cleared what the MMU latched. `Some((healthy,
/// line))` says everything the check saw -- the bind state, whether the RM
/// mapped the console, BAR1 against VRAM, the rebind -- whether or not it
/// found anything, so every boot's log answers where BAR1 stands; `None` when
/// the RM could not be asked.
pub(super) fn recover_bar1(device_instance: u32, tag: &str) -> Option<(bool, String)> {
    nvidia_rm_sys::rm_init::bar1_recover(device_instance)
        .ok()
        .map(|check| (check.healthy(), check.describe(tag)))
}

/// The two numbers `mark_primary_for_rm` sends: how many bytes of console the
/// RM must leave alone, and whether that console sits at the base of *this*
/// card's BAR1 aperture.
///
/// Pulled out of the caller because the caller needs a live RM handle and this
/// does not, so this is the half that can be checked on a machine with no GPU
/// -- which is every machine but Moebius's. A mutation lot found both halves
/// unguarded: `pitch * width` and an inverted `==` both compiled, both printed
/// a plausible line, and nothing failed.
///
/// `pitch * height`, never `pitch * width`: `pitch` is already bytes per
/// scanline, so multiplying by the width again overstates the console by the
/// pixel size -- four times over on the 32-bit GOP surface this boots on, and
/// GSP-RM would be asked to reserve all of it.
///
/// No boot framebuffer at all means no console to preserve: `(0, false)`, the
/// same thing the RM is told about a card that scans nothing out.
fn console_params(fb: Option<BootFbInfo>, bar1_phys: u64) -> (u64, bool) {
    match fb {
        Some(fb) => (fb.pitch as u64 * fb.height as u64, fb.phys == bar1_phys),
        None => (0, false),
    }
}

/// The line a card gets when the hatch leaves it cold, so the boot log names
/// the card that stayed down and why. Silence here is what once made a
/// two-card box report one GPU up and give no way to tell which or why.
pub(super) fn cold_line(gpu: &NvidiaGpu) -> String {
    super::boot_fb::console_gpu_cold_line(
        gpu.pci_bus,
        gpu.pci_device,
        gpu.gpu_model,
        boot_fb_phys().unwrap_or(0),
        gpu.bar1_phys,
        gpu.info.fb_size as u64,
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fb(phys: u64, width: u32, height: u32, pitch: u32) -> BootFbInfo {
        BootFbInfo {
            phys,
            width,
            height,
            pitch,
        }
    }

    /// The console size is `pitch * height`. The failure it guards is
    /// `pitch * width`, which on the 32-bit GOP surface this boots on asks
    /// GSP-RM to preserve four times the console that exists.
    #[test]
    fn the_console_size_is_the_pitch_times_the_height_not_the_width() {
        // 1920x1080, 32 bits per pixel: pitch is 7680 bytes.
        let (size, _) = console_params(Some(fb(0xd100_0000, 1920, 1080, 7680)), 0xd000_0000);
        assert_eq!(size, 7680 * 1080, "pitch * height, in bytes");
        assert_ne!(
            size,
            7680 * 1920,
            "pitch * width is the mistake, not the answer"
        );
    }

    /// Whether the console is at the base of THIS card's BAR1 is the one fact
    /// that distinguishes the primary from any other card, so an inverted
    /// comparison would hand the role to every card but the right one.
    #[test]
    fn the_console_is_at_the_bar1_base_only_when_the_addresses_match() {
        let (_, at_base) = console_params(Some(fb(0xd000_0000, 1920, 1080, 7680)), 0xd000_0000);
        assert!(
            at_base,
            "the boot fb starts exactly at this card's BAR1 base"
        );
        let (_, at_base) = console_params(Some(fb(0xd100_0000, 1920, 1080, 7680)), 0xd000_0000);
        assert!(
            !at_base,
            "inside the aperture but not at its base is not the same fact"
        );
        let (_, at_base) = console_params(Some(fb(0xd000_0000, 1920, 1080, 7680)), 0xe000_0000);
        assert!(!at_base, "another card's aperture entirely");
    }

    /// A card with no boot framebuffer is told the same thing as a card that
    /// scans nothing out: there is no console to preserve.
    #[test]
    fn no_boot_framebuffer_means_no_console_to_preserve() {
        assert_eq!(console_params(None, 0xd000_0000), (0, false));
    }
}
