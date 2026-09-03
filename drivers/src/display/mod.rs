//! Only UEFI Display currently.

// The NVIDIA GPU driver is built on the x86_64-only `nvidia-rm-sys` shim
// (which compiles C objects and links the RM blob for that target), so the
// whole stack is gated to x86_64. Other arches get UEFI display only.
#[cfg(target_arch = "x86_64")]
mod nouveau_uapi;
#[cfg(target_arch = "x86_64")]
mod nvidia;
#[cfg(target_arch = "x86_64")]
mod nvidia_hooks;
mod uefi;

#[cfg(target_arch = "x86_64")]
pub use nouveau_uapi::{enabled as nouveau_uapi_enabled, set_enabled as set_nouveau_uapi_enabled};
#[cfg(target_arch = "x86_64")]
pub use nvidia::{
    boot_edid, hdmi_audio_status, nvidia_hda_is_monitor_gpu, set_boot_edid, set_boot_fb_info,
    NvidiaGpu, NvidiaGpuDriverPci,
};

/// Re-export of `nvidia_rm_sys::os_interface::set_thread_id_provider`, so
/// `zCore` (which depends on this crate via kernel-hal, not on nvidia-rm-sys
/// directly) can hand the RM a REAL per-thread identity at boot -- see the
/// provider's doc in os_interface.rs for the lock-reentrancy bug a constant
/// id causes.
#[cfg(target_arch = "x86_64")]
pub fn set_rm_thread_id_provider(f: fn() -> u64) {
    nvidia_rm_sys::os_interface::set_thread_id_provider(f);
}
/// No RM off x86_64 (`nvidia-rm-sys` is an x86_64-only dependency of this
/// crate, see Cargo.toml), so there is nobody to hand the provider to.
#[cfg(not(target_arch = "x86_64"))]
pub fn set_rm_thread_id_provider(_f: fn() -> u64) {}
pub use uefi::UefiDisplay;

/// Re-push ELD and unmute HDMI/DP audio on the GPU driving the monitor.
/// The HDA driver calls this at stream start: firmware GOP never enables
/// audio packets, so without it `wavplay` succeeds and the monitor stays
/// silent. Matches Linux: one HDMI sink (the live display), not every GPU.
pub fn kick_hdmi_audio() {
    #[cfg(target_arch = "x86_64")]
    nvidia::NvidiaGpu::kick_hdmi_audio_all();
}

/// Whether this NVIDIA HDA function (`bus:dev.1`) belongs to the GPU that
/// scans out the GOP framebuffer. Extra GPUs are left unbound, like Linux
/// leaving their HDMI pins without ELD. Always `true` off x86_64 (no NVIDIA
/// driver).
#[cfg(not(target_arch = "x86_64"))]
pub fn nvidia_hda_is_monitor_gpu(_bus: u8, _device: u8) -> bool {
    true
}

/// The UEFI-captured EDID is only wired up on x86_64 (via the NVIDIA/UEFI
/// boot path). On other arches there is no boot EDID; readers (procfs
/// `gpuedid`, the DRM synthetic connector) get `None` and fall back to their
/// mode-derived estimates.
#[cfg(not(target_arch = "x86_64"))]
pub fn boot_edid() -> Option<([u8; 128], u32)> {
    None
}

/// The nouveau-compatible uAPI (and the syncobj capability bits gated on it)
/// only exists on x86_64, where the NVIDIA driver itself exists. Other
/// arches never enable it.
#[cfg(not(target_arch = "x86_64"))]
pub fn nouveau_uapi_enabled() -> bool {
    false
}
