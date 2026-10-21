//! `drm_mode_getfb`/`drm_mode_getfb2_ioctl`: who gets the backing GEM
//! handle. A child of `drm_scheme` for its size ceiling (`xtask tamano`).

use super::{drm, master_check, DrmDev};

impl DrmDev {
    /// The handle `GETFB`/`GETFB2` report for `fb`: "GET_FB() is an
    /// unprivileged ioctl so we must not return a buffer-handle to
    /// non-master processes!" -- the caller is the node's current master
    /// (`drm_is_current_master`) or has `CAP_SYS_ADMIN`, and then gets a
    /// handle of its own for the object ([`drm::fb_handle::fb_gem_handle_for`],
    /// `fb->funcs->create_handle`); everyone else reads 0, with the geometry.
    /// This went to the process that created the framebuffer, master or
    /// not, and to nobody else: a compositor that took over from another
    /// could not reach the framebuffers it inherited, and a client that
    /// created one kept a route to it after losing the master.
    pub(super) fn fb_handle_for_caller(&self, fb: &drm::DrmFramebuffer) -> u32 {
        if !drm::is_master(self.minor, self.file_owner()) && !drm::current_is_root() {
            return 0;
        }
        drm::fb_handle::fb_gem_handle_for(fb, master_check::caller_pid())
    }
}
