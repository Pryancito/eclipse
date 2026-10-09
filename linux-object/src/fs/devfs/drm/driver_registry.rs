//! Which card answers which question.
//!
//! `DRM_STATE.drivers` is one list, and three different questions get asked of
//! it: who is the primary, who has the monitor, and who does compute. They
//! have three different answers on a box with more than one card, and getting
//! them from `drivers.first()` -- which, because [`register_driver`] inserts
//! at 0, is the LAST card probed -- is what asked the headless card whether
//! the panel needed a software blit and whether there was a vblank to wait
//! on. Kept together here so the next question that needs an answer is asked
//! next to the other three instead of reaching for the first entry again.

use super::*;

/// Register a new DRM driver
pub fn register_driver(driver: Arc<dyn DrmScheme>) {
    let mut state = DRM_STATE.lock();
    if driver.name() == "simplefb" {
        state.drivers.push(driver);
    } else {
        state.drivers.insert(0, driver);
    }
}

/// Unregister a driver [`register_driver`] added, matched by identity
/// (`Arc::ptr_eq`) rather than by name. Returns whether one was removed.
///
/// Test-only, and `DRM_STATE.drivers` is append-only for a reason: nothing in
/// this kernel unplugs a GPU. But a unit-test binary runs every test of the
/// crate in ONE process, and a registered driver changes the answers the whole
/// DRM core gives -- `software_kms_active()` asks every driver whether it can
/// scan out, `get_primary_driver()` takes the first entry, and `get_resources`
/// filters the topology on whether any driver has hardware KMS. One left behind
/// would put every later test on the hardware path.
#[cfg(test)]
pub(crate) fn unregister_driver(driver: &Arc<dyn DrmScheme>) -> bool {
    let mut state = DRM_STATE.lock();
    match state.drivers.iter().position(|d| Arc::ptr_eq(d, driver)) {
        Some(pos) => {
            state.drivers.remove(pos);
            true
        }
        None => false,
    }
}

/// Get the primary DRM driver
pub fn get_primary_driver() -> Option<Arc<dyn DrmScheme>> {
    DRM_STATE.lock().drivers.first().cloned()
}

/// The DRM driver of the card that drives the display, for the questions that
/// are about the monitor rather than about whoever happens to be first in the
/// list: does the panel need the software blit, and is there a real vblank to
/// wait on.
///
/// That is the first registered driver which is not a compute GPU. A compute
/// GPU is one that drives no display by definition (the NVIDIA driver answers
/// `is_compute_gpu()` false exactly for the card whose BAR1 holds the boot
/// framebuffer), and a non-NVIDIA driver answers false too, which is right:
/// a plain KMS or framebuffer driver does drive the output. When every
/// registered driver says it is a compute GPU there is no better answer than
/// [`get_primary_driver`], so that is the fallback rather than `None` -- a
/// `None` here would silently turn the software-KMS answer around.
pub fn get_display_driver() -> Option<Arc<dyn DrmScheme>> {
    let state = DRM_STATE.lock();
    state
        .drivers
        .iter()
        .find(|d| !d.is_compute_gpu())
        .or_else(|| state.drivers.first())
        .cloned()
}

/// `nvidia.compute=BB.DD.F` on the kernel cmdline (hex, dots — the cmdline
/// already uses `:` as its token separator, so a PCI BDF with colons cannot
/// be a single token). Example: `nvidia.compute=65.00.0`.
fn parse_nvidia_compute_bdf() -> Option<(u8, u8, u8)> {
    parse_compute_bdf_in(kernel_hal::boot::cmdline().as_str())
}

/// [`parse_nvidia_compute_bdf`] against a given cmdline, which is the only way
/// to test it: the real one comes from the bootloader.
///
/// The function field is optional (a GPU is function 0), but **an unparsable
/// one is a rejection, not a default**. Falling back to 0 on anything that did
/// not parse meant `nvidia.compute=65.00.zz` pinned compute to `65.00.0`
/// without a word, which is a pin the user never asked for -- and a pin is
/// exactly the knob someone reaches for when the automatic choice already went
/// wrong, so it has to either mean what it says or say nothing. Extra
/// dot-separated segments are a rejection for the same reason.
fn parse_compute_bdf_in(cmdline: &str) -> Option<(u8, u8, u8)> {
    for tok in cmdline.split([':', ' ', '\t', '\n']) {
        let Some(rest) = tok.strip_prefix("nvidia.compute=") else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let mut parts = rest.split('.');
        let bus = u8::from_str_radix(parts.next()?, 16).ok()?;
        let dev = u8::from_str_radix(parts.next()?, 16).ok()?;
        let func = match parts.next() {
            Some(p) => u8::from_str_radix(p, 16).ok()?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        return Some((bus, dev, func));
    }
    None
}

#[cfg(test)]
mod compute_bdf_tests;

/// The NVIDIA GPU that owns compute (SAXPY, NVK EXEC, CE-present): either
/// the `nvidia.compute=BB.DD.F` pin, or the first driver with
/// [`DrmScheme::is_compute_gpu`]. Never the console GPU — its GSP resume
/// can wedge the bus.
pub fn get_compute_driver() -> Option<Arc<dyn DrmScheme>> {
    let drivers = kernel_hal::drivers::all_drm();
    let list = drivers.as_vec();
    if let Some((bus, dev, func)) = parse_nvidia_compute_bdf() {
        if let Some(d) = list.iter().find(|d| {
            matches!(
                d.pci_bdf(),
                Some((_, b, dv, f)) if b == bus && dv == dev && f == func
            )
        }) {
            if d.is_console_gpu() {
                kernel_hal::klog_info!(
                    "[drm] nvidia.compute={:02x}.{:02x}.{:x} is the console GPU — ignoring pin \
                     (GSP on the GOP card can wedge the bus)",
                    bus,
                    dev,
                    func
                );
            } else {
                return Some(d.clone());
            }
        } else {
            kernel_hal::klog_info!(
                "[drm] nvidia.compute={:02x}.{:02x}.{:x} does not match any DRM GPU",
                bus,
                dev,
                func
            );
        }
    }
    list.iter().find(|d| d.is_compute_gpu()).cloned()
}
