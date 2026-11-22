//! The panel's mode, when a driver can change it.
//!
//! For most of this DRM core the panel has exactly one mode: the one the
//! firmware set, which [`super::display_mode`] reads off the boot framebuffer.
//! A driver that does real modesets -- nvidia-modeset on the card it scans
//! out, as `nvidia-drm.modeset=1` in Linux -- lists the monitor's own modes
//! ([`driver_modes`]), and a client's `SETCRTC` or atomic `MODE_ID` to another
//! one goes to that driver ([`set_mode`]). From then on [`panel_mode`] is the
//! mode the client set, so the connector, the CRTC readback and the checks
//! against the active area agree with what the monitor shows.
//!
//! The boot framebuffer keeps its size: [`super::display_mode`] still answers
//! the console's geometry, which is what the CPU and copy-engine presents
//! write into.

use super::get_display_driver;
use lock::Mutex;
use zcore_drivers::scheme::drm::DisplayMode;

/// The mode a client set through the driver, `None` while it is the
/// firmware's.
static SET_MODE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// What the panel scans out now: the mode a client set, else the
/// firmware's (`display_mode`'s width and height).
pub fn panel_mode() -> Option<(u32, u32)> {
    if let Some(m) = *SET_MODE.lock() {
        return Some(m);
    }
    super::display_mode().map(|(w, h, _)| (w, h))
}

/// The modes the panel's driver can set, preferred first. Empty when it
/// cannot set any: the connector then lists the firmware's one mode.
pub fn driver_modes() -> alloc::vec::Vec<DisplayMode> {
    get_display_driver()
        .map(|d| d.display_modes())
        .unwrap_or_default()
}

/// Why [`set_mode`] did not set a mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeRefused {
    /// The driver does no modesets: the panel has only the firmware's mode.
    Fixed,
    /// The driver tried, and says why not.
    Driver(alloc::string::String),
}

/// Ask the panel's driver for `width`x`height` at about `refresh_hz`,
/// scanning out `fb_id`. `test_only` only validates it (atomic `TEST_ONLY`
/// and `atomic_check`). Call it with no DRM lock held.
pub fn set_mode(
    width: u32,
    height: u32,
    refresh_hz: u32,
    fb_id: u32,
    test_only: bool,
) -> Result<(), ModeRefused> {
    let driver = get_display_driver().ok_or(ModeRefused::Fixed)?;
    match driver.set_display_mode(width, height, refresh_hz, fb_id, test_only) {
        None => Err(ModeRefused::Fixed),
        Some(Err(why)) => Err(ModeRefused::Driver(why)),
        Some(Ok(())) => {
            if !test_only {
                record(width, height);
            }
            Ok(())
        }
    }
}

/// The mode the driver now scans out. The firmware's own size clears the
/// override, so [`panel_mode`] answers as before.
fn record(width: u32, height: u32) {
    let firmware = super::display_mode().map(|(w, h, _)| (w, h));
    *SET_MODE.lock() = (firmware != Some((width, height))).then_some((width, height));
}

/// Whether a client's mode needs a modeset: it is not what the panel shows.
pub fn needs_modeset(width: u32, height: u32) -> bool {
    panel_mode() != Some((width, height))
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    *SET_MODE.lock() = None;
}
