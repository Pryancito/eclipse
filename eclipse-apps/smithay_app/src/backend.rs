use crate::render::FramebufferState;
use crate::ipc::IpcHandler;
use crate::input::CompositorEvent;

/// Backend represents the hardware/OS interface.
/// It encapsulates the Framebuffer and IPC capabilities.
pub struct Backend {
    pub fb: FramebufferState,
    pub ipc: IpcHandler,
    /// wgpu-compatible GPU instance, available when the `wgpu` feature is enabled.
    /// Provides accelerated rendering through VirtIO virgl or NVIDIA BAR0 MMIO.
    #[cfg(feature = "wgpu")]
    pub wgpu: Option<sidewind_wgpu::Instance>,
}

impl Backend {
    pub fn new() -> Option<Self> {
        #[cfg(not(test))]
        let fb = crate::render::FramebufferState::init()?;
        #[cfg(test)]
        let fb = crate::render::FramebufferState::mock();
        
        let ipc = IpcHandler::new();

        #[cfg(feature = "wgpu")]
        let wgpu = {
            use sidewind_wgpu::{Instance, Backend as WgpuBackend};
            // Probe the best available GPU backend automatically.
            // On VirtIO GPU (QEMU/KVM) this creates a virgl 3D context;
            // on bare-metal NVIDIA the caller should pass Backend::Nvidia.
            let inst = Instance::new(WgpuBackend::Auto);
            Some(inst)
        };

        Some(Self {
            fb,
            ipc,
            #[cfg(feature = "wgpu")]
            wgpu,
        })
    }

    pub fn poll_event(&mut self) -> Option<CompositorEvent> {
        self.ipc.process_messages()
    }

    pub fn swap_buffers(&mut self) {
        let _ = self.fb.present();
    }
}
