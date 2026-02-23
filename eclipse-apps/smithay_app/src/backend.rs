use crate::render::FramebufferState;
use crate::ipc::IpcHandler;
use crate::input::CompositorEvent;

/// Backend represents the hardware/OS interface.
/// It encapsulates the Framebuffer and IPC capabilities.
pub struct Backend {
    pub fb: FramebufferState,
    pub ipc: IpcHandler,
}

impl Backend {
    pub fn new() -> Option<Self> {
        let fb = FramebufferState::init()?;
        let ipc = IpcHandler::new();
        Some(Self { fb, ipc })
    }

    pub fn poll_event(&mut self) -> Option<CompositorEvent> {
        self.ipc.process_messages()
    }

    pub fn swap_buffers(&mut self) {
        let _ = self.fb.present();
    }
}
