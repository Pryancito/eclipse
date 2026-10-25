//! Silencing the bring-up narration on the path the desktop waits for.
//!
//! Own file for the size ceiling of `nvidia.rs`; no logic lives anywhere else.

/// Silence the bring-up narration for as long as this lives.
///
/// The `bringup_stepN` methods serve two callers that want opposite things.
/// A person reading `cat /proc/gpustepN` wants every RM line, and pays for it
/// knowingly. The AUTOMATIC bring-up at boot -- `auto_bringup_compute` on the
/// secondary cards, `ensure_console_gpu_brought_up` and
/// `build_console_gr_ladder` on the console one -- calls the very same
/// methods, and then drops the `String` they return on the floor. Everything
/// spent producing it was spent for nobody: a mutex and a push per RM line, a
/// 256 KiB buffer, a second pass reformatting every line of it, and
/// `live_echo` lifting each line to ERROR so a framebuffer console that lives
/// in the BAR1 of the GPU under bring-up renders it.
///
/// That cost sits squarely on the path the desktop is waiting for, so the
/// automatic callers arm this. `nvidia.gputrace` disarms it for a debugging
/// boot, and a manual `/proc/gpustep*` read never arms it at all.
pub(super) struct QuietBringup(bool);

impl QuietBringup {
    /// Arm, unless `nvidia.gputrace` asked for the narration back.
    pub(super) fn arm() -> Self {
        if crate::display::nouveau_uapi::gpu_trace_enabled() {
            // Report the CURRENT setting as the one to restore, so `Drop`
            // leaves things exactly as it found them.
            return QuietBringup(nvidia_rm_sys::os_interface::narration_is_off());
        }
        QuietBringup(nvidia_rm_sys::os_interface::narration_off_set(true))
    }
}

impl Drop for QuietBringup {
    fn drop(&mut self) {
        nvidia_rm_sys::os_interface::narration_off_set(self.0);
    }
}
