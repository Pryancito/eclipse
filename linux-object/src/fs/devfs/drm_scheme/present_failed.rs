//! `present_failed`: how a refused present is reported, once per cause,
//! and the counter behind it. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

pub(super) static PRESENT_FAIL_TRACED: AtomicU32 = AtomicU32::new(0);

/// Decide what a failed present means for the ioctl that asked for it, and say
/// so on the console.
///
/// A modeset is a *configuration* operation: `drm_mode_setcrtc` binds a fb to a
/// CRTC and programs a mode, and Linux fails it for a bad argument, never
/// because a frame could not be copied. Answering `EIO` because the blit did
/// not happen conflated the two, and wlroots' legacy backend reads that as the
/// output being broken — it retries the whole modeset next frame, forever,
/// never advancing to page-flips. The compositor log fills with
///
/// ```text
/// [backend/drm/legacy.c:123] connector HDMI-A-1: Failed to set CRTC: I/O error
/// ```
///
/// at frame rate while the kernel says nothing, because every reason inside the
/// present path is a `warn!` and the rig boots at `LOG=error`. A single
/// unpresentable frame took down the whole desktop.
///
/// So: a bad fb id keeps failing the ioctl, with the `ENOENT` Linux uses for it
/// ("Unknown FB ID") rather than `EIO` — that one is a real client error, and
/// the errno points at fb lifetime instead of at the bus. Everything else is
/// reported and swallowed: the CRTC takes the binding it was asked for, the
/// compositor keeps running, and the next present gets another chance. That is
/// the same call `DIRTYFB` already makes a few arms down, for the same reason.
pub(super) fn present_failed(
    op: &str,
    fb_id: u32,
    crtc_id: u32,
    err: drm::PresentError,
) -> core::result::Result<(), FsError> {
    let n = PRESENT_FAIL_TRACED.fetch_add(1, Ordering::Relaxed);
    if n < PRESENT_FAIL_TRACE_BUDGET {
        // error!, not warn!: the rig boots at LOG=error, and this line is the
        // one that says why the screen is black. The reasons underneath are
        // warn!/klog and were invisible there.
        //
        // For a missing fb, say whether the id was one WE took away. A
        // nouveau-backed fb dies with its GEM handle (Linux's never does,
        // because there the fb holds its own reference), so "the client is
        // presenting an id it never had" and "the client is presenting the id
        // we pulled out from under it" both arrive here looking identical --
        // and they have opposite fixes.
        let taken_by = match err {
            drm::PresentError::NoSuchFb => drm::fb_retired_reason(fb_id),
            _ => None,
        };
        log::error!(
            "[drm] {} fb={} crtc={} did not present: {}{}{}{}",
            op,
            fb_id,
            crtc_id,
            err.as_str(),
            match taken_by {
                Some(why) => alloc::format!(" (this fb was retired by {})", why.as_str()),
                None => alloc::string::String::new(),
            },
            match err {
                drm::PresentError::NoSuchFb => " -> ENOENT to caller",
                _ => " -> reported OK to caller (the modeset stands; only this frame is lost)",
            },
            if n + 1 == PRESENT_FAIL_TRACE_BUDGET {
                " [further present failures not traced]"
            } else {
                ""
            },
        );
    }
    match err {
        drm::PresentError::NoSuchFb => Err(FsError::EntryNotFound),
        // EINVAL, as `drm_mode_setcrtc` answers for a framebuffer no plane
        // can scan out ("Invalid pixel format" / failed atomic check). The
        // CRTC is deliberately NOT bound to it: unlike the two below, this
        // framebuffer will never become presentable, so leaving `crtc_fb`
        // naming it would make every later repaint try it again.
        drm::PresentError::UnsupportedLayout => Err(FsError::InvalidParam),
        drm::PresentError::NoDisplay | drm::PresentError::NoBacking => {
            // We are about to answer 0, so the CRTC really is configured with
            // this fb and `GETCRTC` has to say so. `present_now_checked` binds
            // it on every path that succeeds and returns before binding on the
            // ones that do not, which would otherwise leave the readback
            // naming the previous frame's fb. Safe for both reasons that get
            // here: every consumer of `crtc_fb` -- `repaint_for_cursor` and
            // the next present -- re-checks the display and the fb's backing
            // before it touches a pixel.
            drm::set_crtc_fb(crtc_id, fb_id);
            Ok(())
        }
    }
}
