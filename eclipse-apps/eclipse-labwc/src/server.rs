//! Server boot — crea Display, EventLoop, LabwcState y arranca el backend.

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

use crate::backend;
use crate::state::LabwcState;

pub fn run() -> anyhow::Result<()> {
    let event_loop: EventLoop<'static, LabwcState> = EventLoop::try_new()?;
    let mut display: Display<LabwcState> = Display::new()?;

    let state = LabwcState::new(
        &mut display,
        event_loop.handle(),
        event_loop.get_signal(),
    )?;

    backend::run(state)
}
