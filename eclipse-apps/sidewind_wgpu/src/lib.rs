//! `sidewind_wgpu` — wgpu-inspired GPU abstraction for Eclipse OS.
//!
//! Provides a wgpu-compatible API surface implemented over the OS-native GPU
//! backends:
//!
//! * **VirtIO/virgl** — used when running under QEMU/KVM with `virtio-gpu-gl`
//!   or `virtio-gpu` (via the `virgl_*` and `gpu_*` kernel syscalls).
//! * **NVIDIA** — used on bare-metal NVIDIA hardware (Turing/Ampere/Ada/Hopper)
//!   via BAR0 MMIO register access through `sidewind_nvidia`.
//!
//! ## Quick-start
//! ```no_run
//! use sidewind_wgpu::{Instance, Backend, TextureFormat, Color};
//!
//! // Pick the best available backend automatically.
//! let instance = Instance::new(Backend::Auto);
//!
//! // Allocate a display surface (1920×1080, BGRA8 format).
//! let mut surface = instance.create_surface(1920, 1080).unwrap();
//!
//! // Create a device/queue pair.
//! let (device, queue) = instance.create_device();
//!
//! // Record a frame.
//! let mut encoder = device.create_command_encoder();
//! {
//!     let mut pass = encoder.begin_render_pass(
//!         surface.current_texture(),
//!         Color { r: 0.05, g: 0.05, b: 0.10, a: 1.0 },
//!     );
//!     // issue draw calls through `pass` …
//! }
//! let cmds = encoder.finish();
//! queue.submit(cmds);
//! surface.present();
//! ```
//!
//! The API mirrors [wgpu](https://docs.rs/wgpu) closely enough that code can
//! eventually be migrated to real wgpu once the OS provides the required
//! Vulkan/DRI infrastructure.

#![no_std]
#![allow(dead_code)]

extern crate alloc;

pub mod backend;
pub mod buffer;
pub mod device;
pub mod encoder;
pub mod error;
pub mod instance;
pub mod pipeline;
pub mod surface;
pub mod texture;

// ── Top-level re-exports (mirrors wgpu's flat namespace) ─────────────────────
pub use backend::Backend;
pub use buffer::{Buffer, BufferDescriptor, BufferUsages, MapMode};
pub use device::{Device, Queue};
pub use encoder::{CommandBuffer, CommandEncoder, RenderPass};
pub use error::WgpuError;
pub use instance::Instance;
pub use pipeline::{
    BlendState, ColorTargetState, FragmentState, PrimitiveState, PrimitiveTopology,
    RenderPipeline, RenderPipelineDescriptor, ShaderModule, VertexState,
};
pub use surface::{PresentMode, Surface, SurfaceTexture};
pub use texture::{Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages};

/// RGBA color with `f64` components in `[0.0, 1.0]`.
///
/// Matches `wgpu::Color` exactly so calling code compiles unmodified when
/// ported to real wgpu later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

impl Color {
    pub const BLACK: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };
    pub const RED: Color   = Color { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const GREEN: Color = Color { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
    pub const BLUE: Color  = Color { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };

    /// Convert to a packed `0xAARRGGBB` u32 (BGRA byte order in framebuffer).
    #[inline]
    pub fn to_bgra_u32(self) -> u32 {
        let a = (self.a.clamp(0.0, 1.0) * 255.0) as u32;
        let r = (self.r.clamp(0.0, 1.0) * 255.0) as u32;
        let g = (self.g.clamp(0.0, 1.0) * 255.0) as u32;
        let b = (self.b.clamp(0.0, 1.0) * 255.0) as u32;
        (a << 24) | (r << 16) | (g << 8) | b
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::BLACK
    }
}
