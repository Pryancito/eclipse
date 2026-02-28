//! Display surface management for `sidewind_wgpu`.
//!
//! A `Surface` represents the OS display output.  On Eclipse OS it wraps
//! either a VirtIO GPU display buffer or the EFI GOP framebuffer.

use crate::error::WgpuError;
use crate::texture::{Extent3d, Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};

/// How the surface presents frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    /// No vertical sync; present as fast as possible.
    Immediate,
    /// Wait for the next vertical blank before presenting.
    Fifo,
    /// Mailbox triple-buffering.
    Mailbox,
}

/// A frame from `Surface::acquire_texture` that can be used as a render target.
pub struct SurfaceTexture {
    pub texture: Texture,
    pub(crate) resource_id: u32,
    pub(crate) front_addr: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
}

impl SurfaceTexture {
    /// Present this frame to the display.
    ///
    /// For VirtIO GPU, this invokes the `gpu_present` kernel syscall.
    /// For the software framebuffer path it copies the back-buffer to the
    /// front-buffer with an `sfence`.
    pub fn present(self) {
        use eclipse_libc::gpu_present;
        if self.resource_id != 0 {
            let _ = gpu_present(self.resource_id, 0, 0, self.width, self.height);
        } else if self.front_addr >= 0x1000 {
            let size_bytes = (self.pitch as usize).saturating_mul(self.height as usize);
            let back_vaddr = self.texture.backing_vaddr;
            if back_vaddr >= 0x1000 {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        back_vaddr as *const u8,
                        self.front_addr as *mut u8,
                        size_bytes,
                    );
                    // Flush Write-Combining buffer so the GOP display controller
                    // sees the update on real NVIDIA hardware (memory-mapped GOP fb).
                    core::arch::asm!("sfence", options(nostack, preserves_flags));
                }
            }
        }
    }
}

/// The OS display surface.
///
/// Created via `Instance::create_surface`.
pub struct Surface {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) format: TextureFormat,
    pub(crate) present_mode: PresentMode,
    /// Front-buffer virtual address (read by the display controller).
    pub(crate) front_addr: usize,
    /// VirtIO GPU resource ID (0 if using software framebuffer path).
    pub(crate) gpu_resource_id: u32,
    /// Pre-built back-buffer texture.  Returned by `current_texture()`.
    pub(crate) back_texture: Texture,
}

impl Surface {
    /// Build the back-buffer `Texture` from raw surface parameters.
    pub(crate) fn make_back_texture(
        width: u32,
        height: u32,
        pitch: u32,
        format: TextureFormat,
        back_vaddr: u64,
    ) -> Texture {
        let desc = TextureDescriptor {
            size: Extent3d::new(width, height),
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST,
        };
        let mut tex = Texture::new_empty(desc);
        tex.backing_vaddr = back_vaddr;
        tex.backing_size  = (pitch as usize).saturating_mul(height as usize);
        tex
    }

    /// Configure the surface presentation mode.
    pub fn configure(&mut self, mode: PresentMode) {
        self.present_mode = mode;
    }

    /// The current width of the surface in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The current height of the surface in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Return a reference to the current back-buffer texture.
    ///
    /// This is the render target for the next frame.  Pass it to
    /// `CommandEncoder::begin_render_pass` to draw into it, then call
    /// `Surface::present_texture` to flip to the display.
    pub fn current_texture(&self) -> &Texture {
        &self.back_texture
    }

    /// Acquire the next frame, returning a `SurfaceTexture` that can be used
    /// as a render target and later presented.
    pub fn acquire_texture(&self) -> Result<SurfaceTexture, WgpuError> {
        let desc = TextureDescriptor {
            size: Extent3d::new(self.width, self.height),
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: self.format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_DST,
        };
        let mut tex = Texture::new_empty(desc);
        tex.backing_vaddr = self.back_texture.backing_vaddr;
        tex.backing_size  = self.back_texture.backing_size;

        Ok(SurfaceTexture {
            texture: tex,
            resource_id: self.gpu_resource_id,
            front_addr: self.front_addr,
            width: self.width,
            height: self.height,
            pitch: self.pitch,
        })
    }

    /// Present the current back-buffer to the display.
    ///
    /// Equivalent to `acquire_texture()?.present()` but avoids allocating a
    /// redundant `SurfaceTexture`.
    pub fn present(&self) {
        use eclipse_libc::gpu_present;
        if self.gpu_resource_id != 0 {
            let _ = gpu_present(self.gpu_resource_id, 0, 0, self.width, self.height);
        } else if self.front_addr >= 0x1000 {
            let back_vaddr = self.back_texture.backing_vaddr;
            let size_bytes = self.back_texture.backing_size;
            if back_vaddr >= 0x1000 && size_bytes > 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        back_vaddr as *const u8,
                        self.front_addr as *mut u8,
                        size_bytes,
                    );
                    core::arch::asm!("sfence", options(nostack, preserves_flags));
                }
            }
        }
    }

    /// Width × height in pixels.
    pub fn extent(&self) -> Extent3d {
        Extent3d::new(self.width, self.height)
    }
}

