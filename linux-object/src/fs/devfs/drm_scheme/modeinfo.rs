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

/// `struct drm_mode_modeinfo` (`drm_mode.h`): the 68 bytes `SETCRTC`,
/// `GETCRTC` and `GETCONNECTOR` carry, read and written by name rather than
/// by offset. The layout is the uapi one, `repr(C)` with no padding (every
/// `u16` run is even), which the `const` below pins to 68 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DrmModeModeinfo {
    pub(super) clock: u32,
    pub(super) hdisplay: u16,
    pub(super) hsync_start: u16,
    pub(super) hsync_end: u16,
    pub(super) htotal: u16,
    /// Carried through unread: nothing here skews a scanline.
    #[allow(dead_code)]
    pub(super) hskew: u16,
    pub(super) vdisplay: u16,
    pub(super) vsync_start: u16,
    pub(super) vsync_end: u16,
    pub(super) vtotal: u16,
    pub(super) vscan: u16,
    pub(super) vrefresh: u32,
    pub(super) flags: u32,
    pub(super) type_: u32,
    pub(super) name: [u8; 32],
}

const _: () = assert!(core::mem::size_of::<DrmModeModeinfo>() == 68);

impl DrmModeModeinfo {
    /// The mode in the ioctl's 68 bytes, at whatever alignment they sit.
    pub(super) fn from_bytes(m: &[u8; 68]) -> Self {
        // SAFETY: the source is 68 readable bytes, the size of `Self`
        // (pinned above), every field is plain data for which any bit
        // pattern is a value, and `read_unaligned` asks no alignment of it.
        unsafe { core::ptr::read_unaligned(m.as_ptr() as *const Self) }
    }

    /// The 68 bytes the ioctl carries for this mode.
    pub(super) fn to_bytes(self) -> [u8; 68] {
        let mut m = [0u8; 68];
        // SAFETY: the destination is 68 writable bytes, the size of `Self`,
        // and `write_unaligned` asks no alignment of it.
        unsafe { core::ptr::write_unaligned(m.as_mut_ptr() as *mut Self, self) };
        m
    }
}
