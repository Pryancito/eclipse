//! The one RM call behind [`super::comptags`]: reading this GPU's comptag
//! mode.
//!
//! Separate from `comptags` so that module stays free of FFI and runs its
//! tests on the host. Everything here is the plumbing -- the query, the
//! conversion and one log line; the decision this feeds lives next door, under
//! test.

use super::comptags::{self, ComptagCaps};

/// Reads the GPU's comptag configuration the first time a bind needs it, and
/// not again.
///
/// Once, because the configuration is static after state load and a game binds
/// dozens of surfaces a second -- asking per bind would take the RM lock each
/// time for an answer that cannot change. A FAILED read counts as having
/// asked, which is the case that matters most: with no GPU the query always
/// fails, and re-entering the RM on every bind to be told so again would be
/// the expensive way to stay switched off.
///
/// Does nothing at all unless `nvidia.gem_comptags` is on, so a boot without
/// the hatch never calls into the RM for this.
pub(super) fn ensure_read_once(device_instance: u32) {
    if !comptags::enabled() || !comptags::claim_caps_query() {
        return;
    }
    let caps = nvidia_rm_sys::rm_init::comptag_caps(device_instance)
        .ok()
        .map(|c| ComptagCaps {
            one_to_one: c.one_to_one != 0,
            one_to_four: c.one_to_four != 0,
            raw_mode: c.raw_mode != 0,
            compbit_backing_off: c.compbit_backing_off != 0,
            post_l2_off: c.post_l2_off != 0,
            scrub_on_free: c.scrub_on_free != 0,
            vas_for_ce: c.vas_for_ce != 0,
            compr_page_shift: c.compr_page_shift,
        });
    // Logged whichever way it went, and at info rather than debug: with the
    // hatch on, which mode the board is in is the first thing anyone will want
    // to know, and "1:4" or a disabled backing store explains a boot where
    // compression stays off despite the flag.
    match caps {
        Some(c) => crate::klog_info!(
            "[nouveau-uapi] comptag caps: 1:1={} 1:4={} raw={} compbit_off={} \
             postL2_off={} scrub={} vas_ce={} shift={}",
            c.one_to_one,
            c.one_to_four,
            c.raw_mode,
            c.compbit_backing_off,
            c.post_l2_off,
            c.scrub_on_free,
            c.vas_for_ce,
            c.compr_page_shift
        ),
        None => crate::klog_warn!(
            "[nouveau-uapi] comptag caps unreadable -- compressed kinds stay downgraded \
             despite nvidia.gem_comptags"
        ),
    }
    comptags::set_caps(caps);
}
