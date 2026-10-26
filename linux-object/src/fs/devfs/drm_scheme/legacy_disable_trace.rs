//! The four ioctls of wlroots' legacy CRTC-disable, as they arrive and as
//! they end. Out of `drm_scheme.rs` for its size ceiling.

use super::*;

/// How many outcomes are narrated before the budget closes. Every call this
/// file watches belongs to a modeset or a cursor change at startup and
/// teardown, not to a frame, so a handful covers a whole session of a
/// compositor coming and going.
const TRACE_BUDGET: u32 = 48;
static TRACED: AtomicU32 = AtomicU32::new(0);

/// A cursor BITMAP line is spent on a CHANGE of state, never on a repeat.
///
/// Measured, in a QEMU boot of 26-oct: wlroots sends
/// `drmModeSetCursor(crtc, 0, 0, 0)` -- "hide the pointer" -- about eight
/// times a second, forever, because `drm_connector_is_cursor_visible` is
/// false and the call sits at the end of every `legacy_crtc_commit`. A
/// budget per CALL, even a separate one, was therefore spent in 1.6 s of
/// boot, and the box this file was written for loses its desktop at 15 s:
/// the one line the whole trace exists for would have been the first one
/// dropped.
///
/// So the state is remembered and a repeat costs nothing -- no line, no
/// budget. The count of collapsed repeats rides the NEXT change, which is
/// what says the churn is there at all; the two timestamps bound its rate.
const CURSOR_BO_BUDGET: u32 = 16;
static CURSOR_BO_TRACED: AtomicU32 = AtomicU32::new(0);

/// The last `(handle, width, height)` packed into one word, or [`NO_BO_YET`].
static LAST_CURSOR_BO: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(NO_BO_YET);

/// "No cursor bitmap has arrived yet". Not a state a caller can send: it
/// would need a handle of `u32::MAX` with a 65535x65535 bitmap, which
/// `MAX_CURSOR_DIM` refuses several arms earlier.
const NO_BO_YET: u64 = u64::MAX;

/// How many identical calls have arrived since the last line.
static CURSOR_BO_REPEATS: AtomicU32 = AtomicU32::new(0);

fn pack_bo(handle: u32, w: u32, h: u32) -> u64 {
    u64::from(handle) | (u64::from(w & 0xffff) << 32) | (u64::from(h & 0xffff) << 48)
}

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
    let now = pack_bo(handle, w, h);
    if LAST_CURSOR_BO.swap(now, Ordering::Relaxed) == now {
        CURSOR_BO_REPEATS.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let repeats = CURSOR_BO_REPEATS.swap(0, Ordering::Relaxed);
    let n = CURSOR_BO_TRACED.fetch_add(1, Ordering::Relaxed);
    if n < CURSOR_BO_BUDGET {
        log::error!(
            "[drm] CURSOR crtc={} bitmap handle={} {}x{}{}{}",
            crtc_id,
            handle,
            w,
            h,
            if repeats > 0 {
                alloc::format!(" (the previous state arrived {} more times)", repeats)
            } else {
                alloc::string::String::new()
            },
            if n + 1 == CURSOR_BO_BUDGET {
                " [further cursor bitmap CHANGES not traced]"
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

/// The one thing in this file that is not a klog line: whether two cursor
/// states are the same state.
///
/// A klog line has no assertion, but this does, and getting it wrong is how
/// the bug this collapse exists to fix comes back -- a pack that loses a
/// field swallows a real change, silently, and the trace stops saying
/// anything.
#[cfg(test)]
mod tests {
    use super::{pack_bo, NO_BO_YET};

    #[test]
    fn el_mismo_estado_se_empaqueta_igual() {
        assert_eq!(pack_bo(7, 64, 64), pack_bo(7, 64, 64));
    }

    #[test]
    fn un_cambio_en_cualquier_campo_cambia_el_paquete() {
        let base = pack_bo(7, 64, 64);
        // One per field, because a pack that drops a field only shows it on
        // that field: the hide/show pair differs in the handle alone, and a
        // theme change can keep the handle and move the size.
        assert_ne!(base, pack_bo(8, 64, 64));
        assert_ne!(base, pack_bo(7, 32, 64));
        assert_ne!(base, pack_bo(7, 64, 32));
        // And "hide the pointer", which is the state wlroots repeats.
        assert_ne!(base, pack_bo(0, 0, 0));
    }

    #[test]
    fn ningun_estado_real_choca_con_el_centinela() {
        // `NO_BO_YET` means "nothing has arrived yet". A real state reaching
        // it would make the first bitmap of the boot go untraced -- the one
        // line that says a bitmap gets here at all.
        for (handle, w, h) in [
            (0u32, 0u32, 0u32),
            (u32::MAX, 256, 256),
            (
                1,
                super::super::drm::MAX_CURSOR_DIM,
                super::super::drm::MAX_CURSOR_DIM,
            ),
        ] {
            assert_ne!(pack_bo(handle, w, h), NO_BO_YET);
        }
    }
}
