//! The numbers that go into a `drm_mode_modeinfo`: the synthetic 60 Hz
//! mode and the one read from the panel's EDID. Out of `drm_scheme.rs`
//! for its size ceiling; the struct stays with its users.

use super::*;

impl Modeline {
    /// The nominal mode: `w`x`h` at 60 Hz with fixed porches.
    ///
    /// A software framebuffer never programs CRT timings, so these are made up
    /// — but they must be *valid*: `hdisplay < hsync_start < hsync_end <
    /// htotal` (and the vertical analogue). The previous +10%/+5% blanking put
    /// `hsync_end > htotal` at 1366×768, which is MODE_H_ILLEGAL; compositors
    /// that recompute refresh from the porches then advertised ~55–59 Hz
    /// instead of 60.
    pub(super) fn synthetic(w: u32, h: u32) -> Self {
        let hdisplay = w as u16;
        let vdisplay = h as u16;
        // Fixed porches, always strictly increasing for any GOP-sized mode.
        let hsync_start = hdisplay.saturating_add(48);
        let hsync_end = hsync_start.saturating_add(32);
        let htotal = hsync_end.saturating_add(80);
        let vsync_start = vdisplay.saturating_add(3);
        let vsync_end = vsync_start.saturating_add(6);
        let vtotal = vsync_end.saturating_add(32);
        Self {
            clock_khz: clock_khz_for_refresh_mhz(htotal as u32, vtotal as u32, 60_000),
            hdisplay,
            hsync_start,
            hsync_end,
            htotal,
            vdisplay,
            vsync_start,
            vsync_end,
            vtotal,
            vrefresh: 60,
            // -hsync/-vsync. Made up like the rest, and nothing programs it:
            // the sync generator is already running, set by firmware.
            flags: DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC,
        }
    }

    /// The panel's own timing, from the preferred detailed timing of its EDID.
    ///
    /// `None` unless every number survives the trip into a `u16` and the mode
    /// states a refresh, because the fallback is the nominal mode above and
    /// that is strictly better than an illegal one: wlroots handed a mode
    /// whose sync runs past its total drops the output rather than picking
    /// another, and the desktop lands on the text console.
    pub(super) fn from_panel(t: &edid::DetailedTiming) -> Option<Self> {
        let vrefresh = t.refresh_hz();
        if !t.is_valid() || vrefresh == 0 {
            return None;
        }
        // The uAPI field is 16 bits; a monitor that states more is not a
        // monitor this can describe, so fall back rather than truncate.
        let fit = |v: u32| (v <= u16::MAX as u32).then_some(v as u16);
        let mut flags = 0;
        if t.separate_sync {
            flags |= if t.hsync_positive {
                DRM_MODE_FLAG_PHSYNC
            } else {
                DRM_MODE_FLAG_NHSYNC
            };
            flags |= if t.vsync_positive {
                DRM_MODE_FLAG_PVSYNC
            } else {
                DRM_MODE_FLAG_NVSYNC
            };
        }
        if t.interlaced {
            flags |= DRM_MODE_FLAG_INTERLACE;
        }
        Some(Self {
            clock_khz: t.clock_khz,
            hdisplay: fit(t.hdisplay)?,
            hsync_start: fit(t.hsync_start)?,
            hsync_end: fit(t.hsync_end)?,
            htotal: fit(t.htotal)?,
            vdisplay: fit(t.vdisplay)?,
            vsync_start: fit(t.vsync_start)?,
            vsync_end: fit(t.vsync_end)?,
            vtotal: fit(t.vtotal)?,
            vrefresh,
            flags,
        })
    }
}
