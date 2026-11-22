//! Monitors on every card, and their hotplug, as in Linux.
//!
//! In Linux every card that scans out is its own `drm_device`: `card1`'s
//! framebuffers are `card1`'s driver's objects, a `SETCRTC` there modesets
//! `card1`'s monitor, its page flips land on `card1`'s heads, and when a
//! monitor comes or goes on it the kernel sends a `change` uevent with
//! `HOTPLUG=1` for that card (`drm_kms_helper_hotplug_event`), which makes
//! the compositor read its connectors again. Here only the panel's card did
//! any of that: another card's framebuffers were the panel driver's, its
//! frames were only recorded, and nothing ever told userspace a connector
//! changed.
//!
//! A card whose driver [`DrmScheme::drives_own_display`] (nvkms on that
//! card) now gets the Linux behaviour, whatever the number of cards: its
//! framebuffers are made by its own driver ([`fb_driver`]), its modesets
//! go to it ([`set_mode_on`]), and its frames are flipped by it
//! ([`present_on_checked`]). [`spawn_hotplug_poll`] runs Linux's output
//! poll over every card ([`DrmScheme::poll_connectors`]) and sends the
//! uevent for the ones that changed.

use super::*;
use crate::net::uevent;

/// The cards whose framebuffers their own driver made, by fb id. The panel's
/// card is never here: its framebuffers are the panel driver's, as always.
static FB_CARDS: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// The driver that makes `card`'s framebuffers: the panel's for the panel's
/// card, the card's own when it lights its own monitors, and the panel's
/// otherwise, as before (that card only records its frames). Takes
/// `GPU_NODES`: never with `DRM_STATE` held.
pub(super) fn fb_driver(card: u32) -> Option<Arc<dyn DrmScheme>> {
    if card != PANEL_CARD {
        let own = gpu_nodes()
            .into_iter()
            .find(|n| n.index == card)
            .map(|n| n.driver)
            .filter(|d| d.drives_own_display());
        if own.is_some() {
            return own;
        }
    }
    get_display_driver()
}

/// Framebuffer `fb_id` was made by `card`'s own driver.
pub(super) fn note_fb_card(fb_id: u32, card: u32) {
    if card == PANEL_CARD {
        return;
    }
    let mut cards = FB_CARDS.lock();
    cards.retain(|(fb, _)| *fb != fb_id);
    cards.push((fb_id, card));
}

/// The card whose own driver made `fb_id`, `None` for the panel driver's.
fn fb_card(fb_id: u32) -> Option<u32> {
    FB_CARDS
        .lock()
        .iter()
        .find(|(fb, _)| *fb == fb_id)
        .map(|(_, c)| *c)
}

/// The driver's own id of `fb_id`, when `card`'s driver made it.
fn driver_fb_on(card: u32, fb_id: u32) -> Option<u32> {
    if fb_card(fb_id) != Some(card) {
        return None;
    }
    get_fb(fb_id).and_then(|fb| fb.driver_fb_id)
}

/// [`present_now_checked`] for a request that arrived on node `minor`.
///
/// The panel's card presents exactly as before. Any other card's frame goes
/// to that card and nowhere else, as it would on its own `drm_device` in
/// Linux: its CRTC records it, so `GETCRTC` there reads it back, and when
/// the card lights its own monitors its driver flips to it. It never touches
/// the panel, the VT ownership or the software cursor, which are all the
/// panel's.
///
/// A flip the card's driver refuses loses that frame and nothing else, as a
/// frame the panel could not copy does: the compositor goes on.
pub fn present_on_checked(
    minor: u32,
    fb_id: u32,
    crtc_id: u32,
    rect: Option<(u32, u32, u32, u32)>,
) -> Result<(), PresentError> {
    let card = card_of_minor(minor);
    if card == PANEL_CARD {
        return present_now_checked(fb_id, crtc_id, rect);
    }
    if !DRM_STATE.lock().framebuffers.iter().any(|f| f.id == fb_id) {
        return Err(PresentError::NoSuchFb);
    }
    set_crtc_fb_on(card, fb_id);
    if let (Some(driver), Some(own)) = (driver_for_minor(minor), driver_fb_on(card, fb_id)) {
        if !driver.page_flip(own) {
            static REFUSED: AtomicBool = AtomicBool::new(false);
            if !REFUSED.swap(true, Ordering::Relaxed) {
                kernel_hal::klog_warn!(
                    "[drm] card{}: el driver no voltea el fb {} (el suyo {}); ese frame se pierde",
                    card,
                    fb_id,
                    own
                );
            }
        }
    }
    Ok(())
}

/// [`present_now_region`] for a request that arrived on node `minor`; see
/// [`present_on_checked`].
pub fn present_on_region(
    minor: u32,
    fb_id: u32,
    crtc_id: u32,
    rect: Option<(u32, u32, u32, u32)>,
) -> bool {
    present_on_checked(minor, fb_id, crtc_id, rect).is_ok()
}

/// The modes node `minor`'s connectors can be set to: the panel's as
/// [`driver_modes`] lists them, or the card's own driver's. Empty for a
/// card with no modesets of its own.
pub fn driver_modes_on(minor: u32) -> Vec<zcore_drivers::scheme::drm::DisplayMode> {
    if is_panel_node(minor) {
        return driver_modes();
    }
    driver_for_minor(minor)
        .filter(|d| d.drives_own_display())
        .map(|d| d.display_modes())
        .unwrap_or_default()
}

/// A modeset on another card than the panel's: `width`x`height` at about
/// `refresh_hz` on its monitors, scanning out `fb_id`, by its own driver.
/// [`ModeRefused::Fixed`] for a card that does no modesets of its own, so
/// the caller keeps what it did before (only the framebuffer is recorded).
/// Call it with no DRM lock held.
pub fn set_mode_on(
    minor: u32,
    width: u32,
    height: u32,
    refresh_hz: u32,
    fb_id: u32,
) -> Result<(), ModeRefused> {
    let card = card_of_minor(minor);
    let driver = driver_for_minor(minor)
        .filter(|d| d.drives_own_display())
        .ok_or(ModeRefused::Fixed)?;
    let own = driver_fb_on(card, fb_id).unwrap_or(0);
    match driver.set_display_mode(width, height, refresh_hz, own, false) {
        None => Err(ModeRefused::Fixed),
        Some(Err(why)) => Err(ModeRefused::Driver(why)),
        Some(Ok(())) => Ok(()),
    }
}

/// How often the connectors are detected again. Linux's poll helper waits
/// `DRM_OUTPUT_POLL_PERIOD` (10 s) between passes for connectors that have
/// no hotplug interrupt; none has one here, so the wait is shorter, for a
/// monitor plugged in to light up about as soon as on Linux, where most do.
pub const HOTPLUG_POLL_MS: u64 = 2_000;

/// One pass of the output poll: every card asked to detect its connectors
/// again, and a hotplug uevent sent for each card where one changed.
/// Returns how many were sent.
pub fn poll_hotplug() -> usize {
    let mut sent = 0;
    for node in gpu_nodes() {
        if !node.driver.poll_connectors() {
            continue;
        }
        let bdf = node.driver.pci_bdf().map(|(_, b, d, f)| (b, d, f));
        let minor = node.card_minor();
        let seqnum = uevent::drm_hotplug(minor, bdf);
        kernel_hal::klog_info!(
            "[drm] card{}: un monitor cambio de estado; uevent HOTPLUG=1 (SEQNUM={})",
            minor,
            seqnum
        );
        sent += 1;
    }
    sent
}

/// Run [`poll_hotplug`] every [`HOTPLUG_POLL_MS`] for the rest of the boot,
/// from a kernel thread (detection talks to the GPU and may sleep).
pub fn spawn_hotplug_poll() {
    kernel_hal::thread::spawn(async move {
        loop {
            let next =
                kernel_hal::timer::timer_now() + core::time::Duration::from_millis(HOTPLUG_POLL_MS);
            kernel_hal::thread::sleep_until(next).await;
            poll_hotplug();
        }
    });
}

#[cfg(test)]
mod tests;
