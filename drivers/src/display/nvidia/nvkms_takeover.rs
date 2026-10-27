//! `nvidia.nvkms`: give this GPU's display to nvidia-modeset, the way
//! `nvidia-drm.modeset=1` does in Linux. PR 3 of `docs/README-nvkms-port.md`.
//!
//! This step allocates the nvkms device -- nvkms's own core channel and
//! display state for the card -- and logs what nvkms sees on it. Like
//! nvidia-drm before its first atomic commit, it changes nothing on screen:
//! nvkms keeps the firmware's scanout running until a modeset is applied,
//! which is the next PR.
//!
//! nvkms and this driver's own display ladder (the NVC57E surface flip and
//! the NVC57A cursor) cannot share a card, because both allocate its core
//! channel. Whichever comes first keeps it: nvkms claims both ladder states
//! with [`NVKMS_OWNS`], and refuses a card whose ladder is already up.
//!
//! Own file for the size ceiling of `nvidia.rs`.

use super::NvidiaGpu;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use nvidia_rm_sys::nvkms::{self, KmsDevice};
use nvidia_rm_sys::nvkms_os;

/// The value of `surfaceflip_state` and `hwcur_state` while nvkms owns the
/// card. Not 0, so the ladder never initialises; not 1, so it never runs.
pub(super) const NVKMS_OWNS: u8 = 3;

/// Claim one ladder state for nvkms: `Ok` if it was untried (or failed, which
/// left no channel behind), `Err` with the state if the ladder is up.
fn claim(state: &AtomicU8) -> Result<u8, u8> {
    loop {
        let cur = state.load(Ordering::Acquire);
        if cur == 1 {
            return Err(cur);
        }
        if state
            .compare_exchange(cur, NVKMS_OWNS, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(cur);
        }
    }
}

/// The bits per pixel of a firmware framebuffer, from its geometry, for
/// `screen_info.lfb_depth`: the boot handover gives a pitch and a width, and a
/// UEFI GOP framebuffer is 32-bit in practice.
pub(super) fn console_depth(width: u32, pitch: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    match pitch.saturating_mul(8) / width {
        32.. => 32,
        24..=31 => 24,
        16..=23 => 16,
        bpp => bpp,
    }
}

impl NvidiaGpu {
    fn bdf_tag(&self) -> String {
        format!(
            "{:02x}:{:02x}.0 {}",
            self.pci_bus, self.pci_device, self.gpu_model
        )
    }

    /// Whether nvkms holds this card's display.
    pub(super) fn nvkms_owns_display(&self) -> bool {
        self.nvkms_device.lock().is_some()
    }

    /// Load nvkms (once per boot) and give it this GPU. One line per fact,
    /// for the boot log.
    pub(super) fn nvkms_load(&self) -> Vec<String> {
        let tag = self.bdf_tag();
        if self.nvkms_owns_display() {
            return Vec::new();
        }
        if self.rm_device_instance.lock().is_none() {
            return alloc::vec![format!("{}: sin RM, nvkms no puede tomarla", tag)];
        }

        let sf = claim(&self.surfaceflip_state);
        let hc = claim(&self.hwcur_state);
        if sf.is_err() || hc.is_err() {
            // Give back what was claimed: the ladder keeps the card.
            if let Ok(prev) = sf {
                self.surfaceflip_state.store(prev, Ordering::Release);
            }
            if let Ok(prev) = hc {
                self.hwcur_state.store(prev, Ordering::Release);
            }
            return alloc::vec![format!(
                "{}: la escalera propia del display ya tiene el canal core \
                 (surfaceflip/hwcursor); nvkms no se carga en esta GPU",
                tag
            )];
        }
        let release = |gpu: &Self| {
            gpu.surfaceflip_state.store(0, Ordering::Release);
            gpu.hwcur_state.store(0, Ordering::Release);
        };

        if !nvkms_os::module_load() {
            release(self);
            return alloc::vec![format!("{}: nvKmsModuleLoad fallo", tag)];
        }
        let Some(gpu_id) = nvkms_os::gpu_id_at(self.pci_domain, self.pci_bus, self.pci_device)
        else {
            release(self);
            return alloc::vec![format!("{}: el RM no lista esta GPU", tag)];
        };
        let Some(device) = KmsDevice::alloc(gpu_id) else {
            release(self);
            return alloc::vec![format!(
                "{}: allocateDevice fallo para gpu id {:#x} (el motivo, en las lineas NVKMS de arriba)",
                tag, gpu_id
            )];
        };

        let mut lines = alloc::vec![format!(
            "{}: nvkms tiene el display (gpu id {:#x}); la pantalla no cambia hasta el modeset",
            tag, gpu_id
        )];
        match device.probe() {
            Some(info) => {
                for line in nvkms::describe(&info) {
                    lines.push(format!("{}: {}", tag, line));
                }
            }
            None => lines.push(format!("{}: no se pudo consultar el dispositivo", tag)),
        }
        *self.nvkms_device.lock() = Some(device);
        lines
    }

    /// Run nvkms's due timers, and say how many microseconds until the next
    /// one (`u64::MAX` for none queued). `None` when this card is not nvkms's.
    pub(super) fn nvkms_service(&self) -> Option<u64> {
        if !self.nvkms_owns_display() {
            return None;
        }
        Some(match nvkms_os::service_now() {
            Some(deadline) => deadline.saturating_sub(nvkms_os::nvkms_get_usec()),
            None => u64::MAX,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvkms_claims_an_untried_or_failed_ladder() {
        let s = AtomicU8::new(0);
        assert_eq!(claim(&s), Ok(0));
        assert_eq!(s.load(Ordering::Relaxed), NVKMS_OWNS);
        let s = AtomicU8::new(2);
        assert_eq!(claim(&s), Ok(2));
    }

    #[test]
    fn nvkms_never_takes_a_core_channel_the_ladder_holds() {
        let s = AtomicU8::new(1);
        assert_eq!(claim(&s), Err(1));
        assert_eq!(s.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_console_depth_comes_from_pitch_and_width() {
        assert_eq!(console_depth(1920, 7680), 32);
        assert_eq!(console_depth(2048, 8192), 32);
        // A padded pitch is still 32-bit.
        assert_eq!(console_depth(1366, 5504), 32);
        assert_eq!(console_depth(1024, 2048), 16);
        assert_eq!(console_depth(0, 7680), 0);
    }
}
