//! `Instance` — the entry point for `sidewind_wgpu`.
//!
//! `Instance` selects a GPU backend, initialises it, and provides factory
//! methods for surfaces, devices, and queues.

use crate::backend::{ActiveBackend, Backend};
use crate::device::{create_virgl_ctx, Device, Queue};
use crate::error::WgpuError;
use crate::surface::Surface;
use crate::texture::TextureFormat;

use eclipse_libc::{
    get_framebuffer_info, get_gpu_display_info, gpu_alloc_display_buffer,
    map_framebuffer, mmap, PROT_READ, PROT_WRITE, MAP_PRIVATE, MAP_ANONYMOUS,
};
use sidewind_nvidia::features::opengl::GlKernelContext;

/// The kernel maps the physical GOP framebuffer at this virtual offset.
/// Matches `PHYS_MEM_OFFSET` in `smithay_app/src/render.rs`.
const PHYS_MEM_OFFSET: u64 = 0xFFFF_9000_0000_0000;

/// The entry point for `sidewind_wgpu`.
///
/// Analogous to `wgpu::Instance`.  Create one per process; it is cheap to
/// clone (it only carries a backend tag and a few addresses).
pub struct Instance {
    pub(crate) active: ActiveBackend,
    /// VirtIO virgl context ID (0 if not using VirtIO).
    pub(crate) virgl_ctx_id: u32,
    /// BAR0 virtual address for NVIDIA backend (0 otherwise).
    pub(crate) bar0_virt: u64,
    /// VRAM size in mebibytes (NVIDIA backend).
    pub(crate) vram_size_mb: u32,
}

impl Instance {
    /// Create an `Instance` using the specified backend.
    ///
    /// * `Backend::Auto` — tries VirtIO first, then NVIDIA, then Framebuffer.
    /// * `Backend::VirtIo` — always uses the VirtIO virgl path.
    /// * `Backend::Nvidia { … }` — always uses NVIDIA BAR0 MMIO.
    /// * `Backend::Framebuffer` — always uses the software framebuffer path.
    pub fn new(backend: Backend) -> Self {
        match backend {
            Backend::VirtIo => Self::init_virtio(),
            Backend::Nvidia { bar0_virt, vram_size_mb } => {
                Self::init_nvidia(bar0_virt, vram_size_mb)
            }
            Backend::Framebuffer => Self::init_framebuffer(),
            Backend::Auto => {
                // 1. Try VirtIO GPU
                let mut dims = [0u32; 2];
                if get_gpu_display_info(&mut dims) && dims[0] > 0 && dims[1] > 0 {
                    return Self::init_virtio();
                }
                // 2. Try NVIDIA (caller must supply BAR0 via Backend::Nvidia
                //    once they have the address; Auto falls back to framebuffer)
                Self::init_framebuffer()
            }
        }
    }

    // ── Backend initialisers ──────────────────────────────────────────────────

    fn init_virtio() -> Self {
        let ctx_id = create_virgl_ctx(b"sidewind_wgpu");
        Self {
            active: ActiveBackend::VirtIo,
            virgl_ctx_id: ctx_id,
            bar0_virt: 0,
            vram_size_mb: 0,
        }
    }

    fn init_nvidia(bar0_virt: u64, vram_size_mb: u32) -> Self {
        Self {
            active: ActiveBackend::Nvidia,
            virgl_ctx_id: 0,
            bar0_virt,
            vram_size_mb,
        }
    }

    fn init_framebuffer() -> Self {
        Self {
            active: ActiveBackend::Framebuffer,
            virgl_ctx_id: 0,
            bar0_virt: 0,
            vram_size_mb: 0,
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Return the backend that was selected.
    pub fn backend(&self) -> ActiveBackend {
        self.active
    }

    /// Allocate and return a display `Surface` of `width × height` pixels.
    ///
    /// On VirtIO, this calls `gpu_alloc_display_buffer` to allocate a
    /// scanout resource and obtain the mapped virtual address.
    /// On the framebuffer path, this maps the raw EFI GOP framebuffer and
    /// allocates a back-buffer via `mmap`.
    pub fn create_surface(&self, width: u32, height: u32) -> Result<Surface, WgpuError> {
        match self.active {
            ActiveBackend::VirtIo | ActiveBackend::Nvidia => {
                // Use VirtIO GPU display buffer (works for both VirtIO and
                // mixed NVIDIA+VirtIO setups where display is still VirtIO).
                let info = gpu_alloc_display_buffer(width, height)
                    .ok_or(WgpuError::NoAdapterFound)?;
                if info.vaddr < 0x1000 {
                    return Err(WgpuError::SurfaceLost);
                }
                let pitch = if info.pitch > 0 { info.pitch } else { width * 4 };
                let format = TextureFormat::Bgra8Unorm;
                let back_texture = Surface::make_back_texture(width, height, pitch, format, info.vaddr);
                Ok(Surface {
                    width,
                    height,
                    pitch,
                    format,
                    present_mode: crate::surface::PresentMode::Fifo,
                    front_addr: 0,
                    gpu_resource_id: info.resource_id,
                    back_texture,
                })
            }
            ActiveBackend::Framebuffer => {
                let fb_info = get_framebuffer_info().ok_or(WgpuError::NoAdapterFound)?;
                let fb_base = map_framebuffer().ok_or(WgpuError::SurfaceLost)?;
                let pitch = if fb_info.pitch > 0 { fb_info.pitch } else { fb_info.width * 4 };
                let fb_size = (pitch as u64) * (fb_info.height as u64);
                let back_buffer = mmap(0, fb_size, PROT_READ | PROT_WRITE,
                                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if back_buffer == 0 || back_buffer == u64::MAX {
                    return Err(WgpuError::OutOfMemory);
                }
                // Normalise the physical framebuffer address (strip the kernel
                // PHYS_MEM_OFFSET if present, matching the existing render.rs logic).
                let front_addr = if fb_base as u64 >= PHYS_MEM_OFFSET {
                    (fb_base as u64 - PHYS_MEM_OFFSET) as usize
                } else {
                    fb_base
                };
                let format = TextureFormat::Bgra8Unorm;
                let back_texture = Surface::make_back_texture(
                    fb_info.width, fb_info.height, pitch, format, back_buffer,
                );
                Ok(Surface {
                    width: fb_info.width,
                    height: fb_info.height,
                    pitch,
                    format,
                    present_mode: crate::surface::PresentMode::Fifo,
                    front_addr,
                    gpu_resource_id: 0,
                    back_texture,
                })
            }
        }
    }

    /// Create a `Device` / `Queue` pair.
    ///
    /// On VirtIO, a virgl 3-D context is created (if not already done).
    /// On NVIDIA, a `GlKernelContext` is initialised for VRAM allocation.
    pub fn create_device(&self) -> (Device, Queue) {
        let gl_ctx = if self.active == ActiveBackend::Nvidia && self.bar0_virt != 0 {
            GlKernelContext::init(self.bar0_virt, self.vram_size_mb)
        } else {
            None
        };

        let device = Device {
            active_backend: self.active,
            virgl_ctx_id: self.virgl_ctx_id,
            gl_ctx,
            bar0_virt: self.bar0_virt,
        };
        let queue = Queue {
            active_backend: self.active,
            virgl_ctx_id: self.virgl_ctx_id,
        };
        (device, queue)
    }
}
