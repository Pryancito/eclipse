use eclipse_libc::gpu_command;

/// SideWind GPU Backend Types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuBackend {
    VirtioGpu = 0,
    Nvidia    = 1,
    Software  = 2,
}

/// Unified SideWind GPU API (no_std)
/// Inspired by wgpu, but simplified for Eclipse OS.
pub struct GpuDevice {
    backend: GpuBackend,
}

#[derive(Debug, Clone, Copy)]
pub enum GpuError {
    BackendNotSupported,
    CommandFailed,
    InvalidPayload,
}

pub type GpuResult<T> = Result<T, GpuError>;

impl GpuDevice {
    /// Create a new GPU device by detecting available hardware.
    pub fn new() -> Self {
        // Autodetection logic could go here (asking kernel or checking PCI)
        // For now, default to VirtIO-GPU (backend 0)
        Self {
            backend: GpuBackend::VirtioGpu,
        }
    }

    /// Create a GPU device for a specific backend (e.g. for NVIDIA testing)
    pub fn for_backend(backend: GpuBackend) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> GpuBackend {
        self.backend
    }

    /// Submit a command buffer to the GPU.
    pub fn submit(&self, command_id: usize, payload: &[u8]) -> GpuResult<()> {
        match gpu_command(self.backend as usize, command_id, payload) {
            Ok(0) => Ok(()),
            _ => Err(GpuError::CommandFailed),
        }
    }
}

impl Default for GpuDevice {
    fn default() -> Self {
        Self::new()
    }
}

/// A simplified CommandEncoder to build GPU command buffers.
pub struct GpuCommandEncoder<'a> {
    device: &'a GpuDevice,
}

impl<'a> GpuCommandEncoder<'a> {
    pub fn new(device: &'a GpuDevice) -> Self {
        Self { device }
    }

    // High-level 2D operations (can be expanded)

    /// Fill a rectangle with a solid color (NVIDIA/VirtIO accelerated)
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) -> GpuResult<()> {
        if w == 0 || h == 0 {
            return Ok(());
        }
        match self.device.backend {
            GpuBackend::Nvidia => {
                // Command 0 = 2D Execute (FillRect)
                // Payload: [X:u32, Y:u32, W:u32, H:u32, Color:u32]
                let mut p = [0u8; 20];
                p[0..4].copy_from_slice(&x.to_le_bytes());
                p[4..8].copy_from_slice(&y.to_le_bytes());
                p[8..12].copy_from_slice(&w.to_le_bytes());
                p[12..16].copy_from_slice(&h.to_le_bytes());
                p[16..20].copy_from_slice(&color.to_le_bytes());
                self.device.submit(0, &p)
            },
            GpuBackend::VirtioGpu => {
                // VirGL command stream building is not yet implemented.
                // Return BackendNotSupported until proper VirGL streaming is added.
                Err(GpuError::BackendNotSupported)
            },
            _ => Err(GpuError::BackendNotSupported),
        }
    }

    /// Blit a region from one buffer to another
    pub fn blit(&mut self, _src_x: u32, _src_y: u32, _dst_x: u32, _dst_y: u32, _w: u32, _h: u32) -> GpuResult<()> {
        // Similar to fill_rect but uses Blit command
        Ok(())
    }
}

/// Integration with SideWind Surfaces
pub trait SurfaceGpuExt {
    fn gpu_device(&self) -> GpuDevice;
}

impl SurfaceGpuExt for crate::SideWindSurface {
    fn gpu_device(&self) -> GpuDevice {
        GpuDevice::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_device_default_backend() {
        let dev = GpuDevice::new();
        assert_eq!(dev.backend(), GpuBackend::VirtioGpu);
    }

    #[test]
    fn gpu_device_for_backend() {
        let dev = GpuDevice::for_backend(GpuBackend::Nvidia);
        assert_eq!(dev.backend(), GpuBackend::Nvidia);
    }

    #[test]
    fn fill_rect_rejects_zero_size() {
        let dev = GpuDevice::new();
        let mut enc = GpuCommandEncoder::new(&dev);
        // Zero-size rect is a no-op – should succeed without calling the kernel.
        assert!(enc.fill_rect(0, 0, 0, 10, 0xFF_FF0000).is_ok());
        assert!(enc.fill_rect(0, 0, 10, 0, 0xFF_FF0000).is_ok());
    }
}

