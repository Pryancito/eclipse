//! Why a `SETCRTC` was refused, and what a disable actually did.
//! Out of `drm_scheme.rs` for its size ceiling.

use super::*;

/// How many `SETCRTC` outcomes are narrated before the budget closes. A
/// modeset is a startup/mode-change operation, not a per-frame one, so a
/// handful covers a whole session of a compositor coming and going.
const SETCRTC_TRACE_BUDGET: u32 = 16;
static SETCRTC_TRACED: AtomicU32 = AtomicU32::new(0);

/// Say, at `error!`, which check refused a `SETCRTC` and with what errno.
///
/// wlroots reports the whole operation and not the call:
///
/// ```text
/// [backend/drm/drm.c:1385] connector HDMI-A-1: Failed to disable CRTC 1
/// ```
///
/// A disable is `drmModeSetCrtc(fd, crtc, 0, 0, 0, NULL, 0, NULL)` plus the
/// DPMS property, so that one line covers two ioctls and every refusal inside
/// them, and the kernel said nothing at all: on this arm the only way out is
/// the CRTC lookup, whose `ENOENT` left no trace. Naming the check here means
/// one `dmesg` decides whether the refusal is even ours -- and the OK line
/// below is half of the same answer, because a disable that went through
/// moves the question to the DPMS half.
pub(super) fn setcrtc_refused(why: &str, crtc_id: u32, err: FsError) -> FsError {
    let n = SETCRTC_TRACED.fetch_add(1, Ordering::Relaxed);
    if n < SETCRTC_TRACE_BUDGET {
        log::error!(
            "[drm] SETCRTC crtc={} refused: {} -> {:?}{}",
            crtc_id,
            why,
            err,
            tail(n)
        );
    }
    err
}

/// The counterpart: a mode-less `SETCRTC` -- the call that turns the pipe off
/// -- answered 0 after blanking the CRTC, dropping its fb and taking the mode
/// with it.
pub(super) fn setcrtc_disabled(crtc_id: u32) {
    let n = SETCRTC_TRACED.fetch_add(1, Ordering::Relaxed);
    if n < SETCRTC_TRACE_BUDGET {
        log::error!(
            "[drm] SETCRTC crtc={} disable (no mode, no connectors) -> OK, pipe off{}",
            crtc_id,
            tail(n)
        );
    }
}

fn tail(n: u32) -> &'static str {
    if n + 1 == SETCRTC_TRACE_BUDGET {
        " [further SETCRTC outcomes not traced]"
    } else {
        ""
    }
}
