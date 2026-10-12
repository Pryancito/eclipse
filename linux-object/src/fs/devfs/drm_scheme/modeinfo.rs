//! The `drm_mode_modeinfo` this scanout advertises, from the panel timing
//! or the DMT fallback. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

pub(super) fn make_modeinfo(w: u32, h: u32) -> [u8; 68] {
    make_modeinfo_with(w, h, panel_timing().as_ref())
}

/// [`make_modeinfo`] with the panel's timing handed in, so a test can drive
/// both sources without touching the process-wide boot EDID.
pub(super) fn make_modeinfo_with(w: u32, h: u32, panel: Option<&edid::DetailedTiming>) -> [u8; 68] {
    let mut m = [0u8; 68];
    // The resolution has to match, and that is the whole safety argument: the
    // preferred timing describes the panel's native mode, and firmware is free
    // to have programmed a different one (1080p on a 4K panel is the common
    // case). A timing for a mode that is not scanning out is a refresh rate
    // for a different mode, which is worse than admitting we do not know.
    let ml = panel
        .filter(|t| t.hdisplay == w && t.vdisplay == h)
        .and_then(Modeline::from_panel)
        .unwrap_or_else(|| Modeline::synthetic(w, h));
    m[0..4].copy_from_slice(&ml.clock_khz.to_ne_bytes());
    m[4..6].copy_from_slice(&ml.hdisplay.to_ne_bytes());
    m[6..8].copy_from_slice(&ml.hsync_start.to_ne_bytes());
    m[8..10].copy_from_slice(&ml.hsync_end.to_ne_bytes());
    m[10..12].copy_from_slice(&ml.htotal.to_ne_bytes());
    // hskew @12..14 = 0
    m[14..16].copy_from_slice(&ml.vdisplay.to_ne_bytes());
    m[16..18].copy_from_slice(&ml.vsync_start.to_ne_bytes());
    m[18..20].copy_from_slice(&ml.vsync_end.to_ne_bytes());
    m[20..22].copy_from_slice(&ml.vtotal.to_ne_bytes());
    // vscan @22..24 = 0
    m[24..28].copy_from_slice(&ml.vrefresh.to_ne_bytes()); // vrefresh (Hz)
    m[28..32].copy_from_slice(&ml.flags.to_ne_bytes());
    // type @32..36: DRM_MODE_TYPE_DRIVER(0x40) | DRM_MODE_TYPE_PREFERRED(0x08)
    m[32..36].copy_from_slice(&0x48u32.to_ne_bytes());
    // name @36..68 ("WxH")
    let mut name = [0u8; 32];
    let mut i = 0;
    let put = |buf: &mut [u8; 32], i: &mut usize, val: u32| {
        if val == 0 {
            if *i < buf.len() {
                buf[*i] = b'0';
                *i += 1;
            }
            return;
        }
        let mut digits = [0u8; 10];
        let mut n = 0;
        let mut v = val;
        while v > 0 {
            digits[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
        }
        while n > 0 && *i < buf.len() {
            n -= 1;
            buf[*i] = digits[n];
            *i += 1;
        }
    };
    put(&mut name, &mut i, w);
    if i < name.len() {
        name[i] = b'x';
        i += 1;
    }
    put(&mut name, &mut i, h);
    m[36..68].copy_from_slice(&name);
    m
}
