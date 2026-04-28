//! Winit backend — sólo desarrollo en host Linux.

use smithay::reexports::calloop::LoopHandle;

use crate::state::LabwcState;

pub fn run(_state: LabwcState) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // smithay::backend::winit::init devuelve un Backend + EventLoop.
        // El cuerpo es el mismo que en anvil/winit.rs upstream.
        anyhow::bail!("winit backend stub — see smithay/anvil/src/winit.rs for full impl")
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("winit not available on Eclipse OS target")
    }
}

/// Inserta libinput en el event loop. Reusable por DRM y winit.
pub fn insert_libinput(_handle: &LoopHandle<'static, LabwcState>) -> anyhow::Result<()> {
    // smithay::backend::libinput::LibinputInputBackend::new_with_session
    // En Eclipse, "session" es nuestra implementación que abre /dev/input/event*
    // directamente vía eclipse-relibc (sin seatd).
    Ok(())
}
