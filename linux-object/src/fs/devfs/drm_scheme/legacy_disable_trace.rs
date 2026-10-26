//! Why an ioctl of wlroots' legacy CRTC-disable was refused, and what the
//! disable itself did. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

/// How many outcomes are narrated before the budget closes. Every call this
/// file watches belongs to a modeset or a cursor change at startup and
/// teardown, not to a frame, so a handful covers a whole session of a
/// compositor coming and going.
const TRACE_BUDGET: u32 = 24;
static TRACED: AtomicU32 = AtomicU32::new(0);

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

fn tail(n: u32) -> &'static str {
    if n + 1 == TRACE_BUDGET {
        " [further legacy-disable outcomes not traced]"
    } else {
        ""
    }
}
