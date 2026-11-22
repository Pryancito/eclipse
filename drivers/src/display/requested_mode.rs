//! The `resolution=WxH` of `rboot.conf`, for the driver that can really set
//! it.
//!
//! rboot can only ask the firmware's GOP for a mode, and a GOP lists a handful
//! (an NVIDIA one often has no 1920x1080 at all on a 1440p or 4K panel). So a
//! fixed resolution used to be lost twice: the GOP had no such mode, and once
//! the compositor started it set the mode nvkms marks as the monitor's
//! preferred one -- the panel's native -- whatever `rboot.conf` said.
//!
//! The loader hands the value over in `BootInfo.gop` whether or not the GOP
//! could set it, the kernel stores it here, and the DRM connector lists the
//! driver's mode of that size as the preferred one ([`prefer_requested`]).
//! labwc, Abyss and every wlroots/Smithay compositor enable an output at its
//! preferred mode, so the desktop comes up at `WxH` through nvkms's own
//! modeset, the way `video=WxH` steers a Linux DRM console.

use crate::scheme::drm::DisplayMode;
use alloc::vec::Vec;
use lock::Mutex;

/// `(width, height)` from `resolution=WxH`; `None` for `auto` or no key.
static REQUESTED: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Record the `resolution=WxH` the bootloader was asked for. A zero size is
/// "nothing asked" (`auto`, or an rboot that predates the record).
pub fn set_requested_mode(width: u32, height: u32) {
    *REQUESTED.lock() = (width != 0 && height != 0).then_some((width, height));
}

/// The `resolution=WxH` the bootloader was asked for, if any.
pub fn requested_mode() -> Option<(u32, u32)> {
    *REQUESTED.lock()
}

/// `modes` as the connector should list them when `requested` is the size
/// `rboot.conf` asked for: the mode of that size closest to 60 Hz (the one
/// nvkms already prefers, if it is of that size) becomes the only preferred
/// mode and goes first; the rest keep their order. A size the driver does
/// not offer leaves the list untouched -- the monitor's own preferred mode is
/// better than a mode nvkms would refuse to set.
pub fn prefer_requested(
    mut modes: Vec<DisplayMode>,
    requested: Option<(u32, u32)>,
) -> Vec<DisplayMode> {
    let Some((w, h)) = requested else {
        return modes;
    };
    let pick = modes
        .iter()
        .enumerate()
        .filter(|(_, m)| m.width == w && m.height == h)
        .min_by_key(|(_, m)| (!m.preferred, m.refresh_hz.abs_diff(60)))
        .map(|(i, _)| i);
    let Some(i) = pick else {
        return modes;
    };
    let mut chosen = modes.remove(i);
    chosen.preferred = true;
    for m in &mut modes {
        m.preferred = false;
    }
    modes.insert(0, chosen);
    modes
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn mode(width: u32, height: u32, refresh_hz: u32, preferred: bool) -> DisplayMode {
        DisplayMode {
            width,
            height,
            refresh_hz,
            pixel_clock_khz: 0,
            preferred,
        }
    }

    #[test]
    fn nothing_asked_keeps_the_monitors_preference() {
        let modes = vec![mode(2560, 1440, 60, true), mode(1920, 1080, 60, false)];
        assert_eq!(prefer_requested(modes.clone(), None), modes);
    }

    #[test]
    fn the_asked_size_becomes_the_only_preferred_mode_and_goes_first() {
        let modes = vec![
            mode(2560, 1440, 144, true),
            mode(2560, 1440, 60, false),
            mode(1920, 1080, 144, false),
            mode(1920, 1080, 60, false),
            mode(1280, 720, 60, false),
        ];
        let out = prefer_requested(modes, Some((1920, 1080)));
        assert_eq!(out[0], mode(1920, 1080, 60, true));
        assert_eq!(out.iter().filter(|m| m.preferred).count(), 1);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn a_size_the_driver_does_not_offer_changes_nothing() {
        let modes = vec![mode(2560, 1440, 60, true), mode(1920, 1080, 60, false)];
        assert_eq!(prefer_requested(modes.clone(), Some((1600, 900))), modes);
    }

    #[test]
    fn the_native_mode_asked_for_keeps_its_own_refresh_rate() {
        let modes = vec![mode(2560, 1440, 144, true), mode(2560, 1440, 60, false)];
        let out = prefer_requested(modes, Some((2560, 1440)));
        assert_eq!(out[0], mode(2560, 1440, 144, true));
        assert!(!out[1].preferred);
    }

    #[test]
    fn a_zero_size_is_nothing_asked() {
        set_requested_mode(0, 1080);
        assert_eq!(requested_mode(), None);
        set_requested_mode(1920, 1080);
        assert_eq!(requested_mode(), Some((1920, 1080)));
        set_requested_mode(0, 0);
        assert_eq!(requested_mode(), None);
    }
}
