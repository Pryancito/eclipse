//! Backend layer — DRM/KMS (`drm.rs`) o winit (`winit.rs`).
//!
//! La elección se hace en runtime: si existe `/dev/dri/card0` y estamos fuera
//! de una sesión X/Wayland, vamos por DRM; en caso contrario, winit.

pub mod drm;
pub mod session;
pub mod winit;

use crate::state::LabwcState;

pub enum BackendKind { Drm, Winit }

pub fn detect() -> BackendKind {
    #[cfg(target_os = "linux")]
    {
        if std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok() {
            return BackendKind::Winit;
        }
        if std::path::Path::new("/dev/dri/card0").exists() { return BackendKind::Drm; }
        return BackendKind::Winit;
    }
    #[cfg(not(target_os = "linux"))]
    {
        BackendKind::Drm   // Eclipse OS siempre va por DRM.
    }
}

pub fn run(state: LabwcState) -> anyhow::Result<()> {
    match detect() {
        BackendKind::Drm   => drm::run(state),
        BackendKind::Winit => winit::run(state),
    }
}
