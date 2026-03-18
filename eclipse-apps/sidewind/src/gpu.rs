#[cfg(not(target_os = "linux"))]
use eclipse_syscall::call::{gpu_command, gpu_get_backend};

#[cfg(target_os = "linux")]
pub unsafe fn gpu_command(_kind: usize, _command: usize, _payload: &[u8]) -> Result<usize, ()> { Err(()) }
#[cfg(target_os = "linux")]
pub fn gpu_get_backend() -> Result<usize, ()> { Ok(2) }

use core::fmt::Debug;

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
        let backend = match gpu_get_backend() {
            Ok(0) => GpuBackend::VirtioGpu,
            Ok(1) => GpuBackend::Nvidia,
            _ => GpuBackend::Software,
        };
        Self { backend }
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
        if unsafe { gpu_command(self.backend as usize, command_id, payload) }.is_ok() {
            Ok(())
        } else {
            Err(GpuError::CommandFailed)
        }
    }
}

/// A simplified CommandEncoder to build GPU command buffers.
pub struct GpuCommandEncoder<'a> {
    device: &'a GpuDevice,
    buffer: [u8; 32], // Small static buffer to save stack space
    offset: usize,
}

impl<'a> GpuCommandEncoder<'a> {
    pub fn new(device: &'a GpuDevice) -> Self {
        Self {
            device,
            buffer: [0u8; 32],
            offset: 0,
        }
    }

    // High-level 2D operations (can be expanded)
    
    /// Fill a rectangle with a solid color (NVIDIA/VirtIO accelerated)
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: u32) -> GpuResult<()> {
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
                // VirtIO uses Submit3D (Command 0)
                // For a fill_rect, we would build a VirGL command stream here.
                // (Placeholder for VirGL stream building)
                self.device.submit(0, b"VIRGL_FILL_RECT_PLACEHOLDER")
            },
            _ => Err(GpuError::BackendNotSupported),
        }
    }

    /// Blit (copy) a rectangle within the framebuffer. NVIDIA: command 1, payload 24 bytes.
    pub fn blit(&mut self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, w: u32, h: u32) -> GpuResult<()> {
        match self.device.backend {
            GpuBackend::Nvidia => {
                let mut p = [0u8; 24];
                p[0..4].copy_from_slice(&src_x.to_le_bytes());
                p[4..8].copy_from_slice(&src_y.to_le_bytes());
                p[8..12].copy_from_slice(&dst_x.to_le_bytes());
                p[12..16].copy_from_slice(&dst_y.to_le_bytes());
                p[16..20].copy_from_slice(&w.to_le_bytes());
                p[20..24].copy_from_slice(&h.to_le_bytes());
                self.device.submit(1, &p)
            }
            GpuBackend::VirtioGpu => {
                // Return error to fallback to software blit until VirtIO 2D resources or VirGL are used
                Err(GpuError::BackendNotSupported)
            }
            _ => Err(GpuError::BackendNotSupported),
        }
    }

    /// Blit from a specific GEM handle (DMABUF) to the primary framebuffer.
    /// NVIDIA: command 2, payload 28 bytes.
    pub fn blit_from_handle(&mut self, src_handle: u32, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, w: u32, h: u32) -> GpuResult<()> {
        match self.device.backend {
            GpuBackend::Nvidia => {
                let mut p = [0u8; 28];
                p[0..4].copy_from_slice(&src_handle.to_le_bytes());
                p[4..8].copy_from_slice(&src_x.to_le_bytes());
                p[8..12].copy_from_slice(&src_y.to_le_bytes());
                p[12..16].copy_from_slice(&dst_x.to_le_bytes());
                p[16..20].copy_from_slice(&dst_y.to_le_bytes());
                p[20..24].copy_from_slice(&w.to_le_bytes());
                p[24..28].copy_from_slice(&h.to_le_bytes());
                self.device.submit(2, &p)
            }
            _ => Err(GpuError::BackendNotSupported),
        }
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
