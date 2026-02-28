//! `Device` and `Queue` types for `sidewind_wgpu`.

use crate::backend::ActiveBackend;
use crate::buffer::{Buffer, BufferDescriptor};
use crate::encoder::CommandEncoder;
use crate::encoder::CommandBuffer;
use crate::error::WgpuError;
use crate::pipeline::{RenderPipeline, RenderPipelineDescriptor};
use crate::texture::{Texture, TextureDescriptor};

use eclipse_libc::{virgl_alloc_backing, virgl_ctx_create};
use sidewind_nvidia::features::opengl::GlKernelContext;

/// A logical GPU device.
///
/// Created via `Instance::create_device`.  Owns GPU-side resources and
/// provides factory methods that match the wgpu API.
pub struct Device {
    pub(crate) active_backend: ActiveBackend,
    /// virgl context ID (VirtIO backend; 0 if not using virgl).
    pub(crate) virgl_ctx_id: u32,
    /// NVIDIA kernel context (NVIDIA backend; `None` otherwise).
    pub(crate) gl_ctx: Option<GlKernelContext>,
    /// BAR0 virtual address (NVIDIA backend; 0 otherwise).
    pub(crate) bar0_virt: u64,
}

impl Device {
    /// Create a new command encoder.
    pub fn create_command_encoder(&self) -> CommandEncoder {
        CommandEncoder::new(self.active_backend, self.virgl_ctx_id, self.bar0_virt)
    }

    /// Create a render pipeline from `descriptor`.
    pub fn create_render_pipeline(&self, descriptor: RenderPipelineDescriptor) -> RenderPipeline {
        RenderPipeline::new(descriptor)
    }

    /// Allocate a GPU buffer.
    ///
    /// On VirtIO, backing memory is obtained via `virgl_alloc_backing`.
    /// On NVIDIA, a VRAM slab is carved out of `GlKernelContext`.
    /// On the framebuffer/software path, a host allocation is used.
    pub fn create_buffer(&mut self, descriptor: &BufferDescriptor) -> Result<Buffer, WgpuError> {
        let mut buf = Buffer::new_empty(descriptor);

        match self.active_backend {
            ActiveBackend::VirtIo => {
                if let Some(vaddr) = virgl_alloc_backing(descriptor.size as usize) {
                    buf.host_vaddr = vaddr;
                } else {
                    return Err(WgpuError::OutOfMemory);
                }
            }
            ActiveBackend::Nvidia => {
                if let Some(ref mut ctx) = self.gl_ctx {
                    // Treat the buffer as a 1×N RGBA pixel strip for VRAM allocation.
                    let pixels = (descriptor.size + 3) / 4;
                    let offset = ctx.alloc_surface(pixels as u32, 1)
                        .ok_or(WgpuError::OutOfMemory)?;
                    buf.vram_offset = Some(offset);
                    buf.host_vaddr  = ctx.surface_virt(offset);
                } else {
                    return Err(WgpuError::DeviceLost);
                }
            }
            ActiveBackend::Framebuffer => {
                // No GPU allocator; host pointer must be supplied by caller.
                // Return an empty buffer — the caller is responsible for
                // pointing host_vaddr at a suitable allocation.
            }
        }

        Ok(buf)
    }

    /// Allocate a GPU texture.
    ///
    /// The backing memory strategy mirrors `create_buffer`.
    pub fn create_texture(&mut self, descriptor: &TextureDescriptor) -> Result<Texture, WgpuError> {
        let mut tex = Texture::new_empty(*descriptor);
        let byte_size = tex.byte_size();

        match self.active_backend {
            ActiveBackend::VirtIo => {
                if let Some(vaddr) = virgl_alloc_backing(byte_size) {
                    tex.backing_vaddr = vaddr;
                    tex.backing_size  = byte_size;
                } else {
                    return Err(WgpuError::OutOfMemory);
                }
            }
            ActiveBackend::Nvidia => {
                if let Some(ref mut ctx) = self.gl_ctx {
                    let offset = ctx
                        .alloc_surface(descriptor.size.width, descriptor.size.height)
                        .ok_or(WgpuError::OutOfMemory)?;
                    tex.vram_offset   = Some(offset);
                    tex.backing_vaddr = ctx.surface_virt(offset);
                    tex.backing_size  = byte_size;
                } else {
                    return Err(WgpuError::DeviceLost);
                }
            }
            ActiveBackend::Framebuffer => {
                // Caller supplies backing memory via `tex.backing_vaddr`.
            }
        }

        Ok(tex)
    }

    /// Return the active backend.
    pub fn backend(&self) -> ActiveBackend {
        self.active_backend
    }
}

// ── Queue ─────────────────────────────────────────────────────────────────────

/// A GPU command queue.
///
/// Created alongside the `Device` by `Instance::create_device`.
pub struct Queue {
    pub(crate) active_backend: ActiveBackend,
    pub(crate) virgl_ctx_id: u32,
}

impl Queue {
    /// Submit one or more `CommandBuffer`s for execution on the GPU.
    ///
    /// On Eclipse OS, command buffers are executed immediately (no async
    /// scheduling) to keep the implementation simple and `no_std`-compatible.
    pub fn submit<I: IntoIterator<Item = CommandBuffer>>(&self, command_buffers: I) {
        for buf in command_buffers {
            buf.execute();
        }
    }

    /// Write `data` into `buffer` starting at `offset`.
    ///
    /// This is the equivalent of `wgpu::Queue::write_buffer`.
    pub fn write_buffer(&self, buffer: &Buffer, offset: u64, data: &[u8]) {
        buffer.write(offset, data);
    }

    /// Write `data` into `texture` at mip level 0, row 0.
    pub fn write_texture(&self, texture: &Texture, data: &[u8]) {
        if texture.backing_vaddr < 0x1000 { return; }
        let max = texture.byte_size().min(data.len());
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                texture.backing_vaddr as *mut u8,
                max,
            );
        }
    }
}

// ── virgl context helper ──────────────────────────────────────────────────────

/// Create a virgl 3-D context named `name`.
/// Returns the context ID on success, 0 on failure.
pub(crate) fn create_virgl_ctx(name: &[u8]) -> u32 {
    virgl_ctx_create(name).unwrap_or(0)
}
