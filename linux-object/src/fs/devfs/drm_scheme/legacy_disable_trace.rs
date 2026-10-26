//! The four ioctls of wlroots' legacy CRTC-disable, as they arrive and as
//! they end. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

/// How many outcomes are narrated before the budget closes. Every call this
/// file watches belongs to a modeset or a cursor change at startup and
/// teardown, not to a frame, so a handful covers a whole session of a
/// compositor coming and going.
const TRACE_BUDGET: u32 = 48;
static TRACED: AtomicU32 = AtomicU32::new(0);

/// A cursor BITMAP change gets a budget of its own, and a small one.
///
/// It is the only call here a running desktop repeats: the pointer shape
/// changes whenever it crosses a window edge or a text field, so on the
/// shared budget it would burn through the whole allowance long before the
/// teardown this file exists to narrate. A separate counter means the
/// `DPMS`/`SETCRTC`/`GETFB` lines of a disable can never be starved by
/// pointer-shape churn, and the first few shapes are still shown -- which is
/// what says whether a bitmap reaches us at all.
const CURSOR_BO_BUDGET: u32 = 16;
static CURSOR_BO_TRACED: AtomicU32 = AtomicU32::new(0);

/// Say, at `error!`, which check refused a call of the legacy disable path.
///
/// wlroots reports the whole operation and not the call:
///
/// ```text
/// [backend/drm/drm.c:1385] connector HDMI-A-1: Failed to disable CRTC 1
/// ```
///
/// That is `dealloc_crtc`, and its `drm_crtc_commit` runs
/// `legacy_crtc_commit`, which for `active = false, modeset = true` issues
/// FOUR ioctls in a row and gives up on the first one that fails: the
/// connector's `DPMS` property, `drmModeSetCrtc` with no mode and no
/// connectors, `drmModeGetFB` on the cursor's framebuffer, and
/// `drmModeSetCursor`. Only the first two report at `WLR_ERROR`; the two
/// cursor ones log at `WLR_DEBUG`, so on a rig at the default log level that
/// single line is ALL there is -- and the kernel said nothing either, because
/// each of these arms refuses by returning an errno and nothing else.
///
/// So name the check here. One `dmesg` then decides which of the four it is,
/// instead of the four being indistinguishable from outside.
pub(super) fn kms_refused(op: &str, id: u32, why: &str, err: FsError) -> FsError {
    let n = TRACED.fetch_add(1, Ordering::Relaxed);
    if n < TRACE_BUDGET {
        log::error!(
            "[drm] {} id={} refused: {} -> {:?}{}",
            op,
            id,
            why,
            err,
            tail(n)
        );
    }
    err
}

/// The counterpart, and half of the same answer: a mode-less `SETCRTC` -- the
/// call that turns the pipe off -- answered 0 after blanking the CRTC,
/// dropping its fb and taking the mode with it. A disable that got this far
/// moves the question to the cursor calls that follow it.
pub(super) fn kms_disabled(crtc_id: u32) {
    let n = TRACED.fetch_add(1, Ordering::Relaxed);
    if n < TRACE_BUDGET {
        log::error!(
            "[drm] SETCRTC crtc={} disable (no mode, no connectors) -> OK, pipe off{}",
            crtc_id,
            tail(n)
        );
    }
}

/// Call 1 of the disable: the connector's `DPMS` property. `off` is any
/// value but `DRM_MODE_DPMS_ON`.
pub(super) fn dpms_set(connector_id: u32, value: u64) {
    let n = TRACED.fetch_add(1, Ordering::Relaxed);
    if n < TRACE_BUDGET {
        log::error!(
            "[drm] SETPROPERTY connector={} DPMS={} -> OK{}",
            connector_id,
            value,
            tail(n)
        );
    }
}

/// Call 2, as it arrives. `mode_valid = 0` with `count_connectors = 0` is
/// the disable; anything else is a modeset and not this path.
pub(super) fn setcrtc_called(crtc_id: u32, fb_id: u32, mode_valid: u32, conns: u32) {
    let n = TRACED.fetch_add(1, Ordering::Relaxed);
    if n < TRACE_BUDGET {
        log::error!(
            "[drm] SETCRTC crtc={} fb={} mode_valid={} connectors={}{}",
            crtc_id,
            fb_id,
            mode_valid,
            conns,
            tail(n)
        );
    }
}

/// Call 3, once it has found the framebuffer: wlroots asks for the cursor
/// buffer's GEM handle here, and a `handle = 0` is the answer a client that
/// is not the master or root gets.
pub(super) fn getfb_served(fb_id: u32, handle: u32) {
    let n = TRACED.fetch_add(1, Ordering::Relaxed);
    if n < TRACE_BUDGET {
        log::error!("[drm] GETFB fb={} -> handle={}{}", fb_id, handle, tail(n));
    }
}

/// Call 4, as it arrives, for a BITMAP change only (a pure `MOVE` is a
/// pointer motion and is not part of a disable).
///
/// A `handle = 0` here is "hide the pointer", which is what the disable
/// sends; a non-zero one is a shape, and the line after it says whether its
/// pixels could be reached.
pub(super) fn cursor_bo_called(crtc_id: u32, handle: u32, w: u32, h: u32) {
    let n = CURSOR_BO_TRACED.fetch_add(1, Ordering::Relaxed);
    if n < CURSOR_BO_BUDGET {
        log::error!(
            "[drm] CURSOR crtc={} bitmap handle={} {}x{}{}",
            crtc_id,
            handle,
            w,
            h,
            if n + 1 == CURSOR_BO_BUDGET {
                " [further cursor bitmaps not traced]"
            } else {
                ""
            }
        );
    }
}

fn tail(n: u32) -> &'static str {
    if n + 1 == TRACE_BUDGET {
        " [further legacy-disable outcomes not traced]"
    } else {
        ""
    }
}
