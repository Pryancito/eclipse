//! Bringing the console GPU up late, and giving it the GR ladder its clients
//! need.
//!
//! The console GPU is the one card `auto_bringup_compute` skips on purpose:
//! its GSP wedges on the SEC2 `STARTCPU` store while scanout is live, so it
//! comes up later, with scanout parked -- from the deferred boot task, from a
//! GPU client's first ioctl, or from an operator's `/proc/gpustep14`. Both
//! halves of "later" live here: running the ladder once, and making sure the
//! GR allocation ladder its clients need exists afterwards.

use super::NvidiaGpu;
use crate::display::nouveau_uapi as nv;
use crate::scheme::DrmScheme;
use core::sync::atomic::Ordering;

impl NvidiaGpu {
    /// Builds this card's GR allocation ladder (step16 + step17) and says
    /// whether it is there afterwards. Idempotent: both steps return their
    /// cached allocation once built.
    ///
    /// `eclipse_rm_ctx_alloc` refuses with `NV_ERR_INVALID_STATE` while this
    /// card's `g_grAllocDone` is false, so until the ladder exists EVERY
    /// client on this GPU falls back to context 0 -- where a process cannot
    /// appear in `nvidia-smi`'s table at all. The secondary cards never had
    /// that problem: `auto_bringup_compute` runs step16/17 itself right after
    /// the state-load. The CONSOLE GPU is the card that does not go through
    /// that function (skipped there on purpose: its GSP wedges on the SEC2
    /// STARTCPU store with scanout live), so its ladder was left to whichever
    /// client's `CHANNEL_ALLOC` happened to rebuild it later -- and a
    /// compositor that allocated its channel at startup, long before this
    /// card's GSP came up, never asks again.
    pub(super) fn ensure_gr_ladder(&self, device_instance: u32) -> &'static str {
        let _ = self.bringup_step16();
        let _ = self.bringup_step17();
        if nvidia_rm_sys::rm_init::gr_ready(device_instance) {
            "ladder GR construido (sus clientes pueden tener contexto propio y salir en nvidia-smi)"
        } else {
            "ladder GR NO construido: sus clientes caeran al contexto 0 y no saldran en nvidia-smi"
        }
    }

    pub(super) fn ensure_console_gpu_brought_up(&self) {
        // Secondary GPUs are already state-loaded at boot by
        // `auto_bringup_compute`; only the console GPU is skipped there, so it is
        // the one that still needs this on-demand path.
        if !self.drives_boot_display() {
            return;
        }
        // Default OFF: safer boot. Opt in with `nvidia.console_gsp`.
        if !nv::console_gsp_enabled() {
            return;
        }
        // Already up — the steady-state fast path (brief lock, no boot).
        if self.rm_device_instance.lock().is_some() {
            return;
        }
        // One attempt, ever.
        if self.auto_bringup_done.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::klog_warn!(
            "[auto-bringup] console GPU not attached; running the full bring-up \
             automatically on first GPU client (attach -> GSP boot -> state-load \
             -> CE) so EXEC works without `cat /proc/gpustep14`"
        );
        // step14 self-guards to the console GPU and chains step5/boot/8/9/10; its
        // internal live-echo streams each stage to the console, so a wedge leaves
        // its last stage visible even though this call would then never return.
        let report = self.bringup_step14();
        for line in report.lines() {
            crate::klog_warn!("[auto-bringup] {}", line);
        }
        if self.rm_device_instance.lock().is_some() {
            crate::klog_warn!(
                "[auto-bringup] console GPU RM-attached and state-loaded — EXEC enabled"
            );
        } else {
            crate::klog_warn!(
                "[auto-bringup] console GPU bring-up did NOT attach the RM; \
                 GEM/VM_BIND/EXEC stay ENODEV until reboot (see the stage trail above)"
            );
        }
    }
}
