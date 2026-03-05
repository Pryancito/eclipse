use crate::render::FramebufferState;
use crate::ipc::IpcHandler;
use crate::input::CompositorEvent;
use core::option::Option::{self, Some, None};

/// Backend represents the hardware/OS interface.
/// It encapsulates the Framebuffer and IPC capabilities.
pub struct Backend {
    pub fb: FramebufferState,
    pub ipc: IpcHandler,
}

impl Backend {
    pub fn new() -> Option<Self> {
        #[cfg(not(test))]
        let fb = crate::render::FramebufferState::init()?;
        #[cfg(test)]
        let fb = crate::render::FramebufferState::mock();
        
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
