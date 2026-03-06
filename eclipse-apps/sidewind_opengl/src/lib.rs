//! `sidewind_opengl` — Software OpenGL implementation for Eclipse OS / NVIDIA.
//!
//! A pure `no_std` CPU rasterizer that writes pixels directly to a mapped
//! framebuffer.  It provides:
//!
//! * **`GlContext`** — core entry point (clear, draw_arrays, draw_elements, fill_rect).
//! * **`Pipeline<VS, FS>`** — vertex + fragment shader glue.
//! * **Built-in shaders** — `PassthroughVS`, `FlatColorFS`, `TextureFS`.
//! * **`DepthBuffer`** — fixed-capacity z-buffer.
//! * **`Mat4 / Vec3 / Vec4`** — `no_std` linear-algebra primitives.
//! * **`Texture2D`** — RGBA texture with nearest/bilinear sampling.
//!
//! ## Quick-start
//! ```no_run
//! use sidewind_opengl::{GlContext, GL_COLOR_BUFFER_BIT, GL_DEPTH_BUFFER_BIT};
//!
//! // fb_ptr comes from the kernel BAR or shared-memory framebuffer.
//! let fb_ptr = core::ptr::null_mut::<u32>();
//! let mut gl = unsafe { GlContext::new(fb_ptr, 1920, 1080) };
//! gl.set_clear_color(0.1, 0.1, 0.15, 1.0);
//! gl.clear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
//! gl.fill_rect(100, 100, 200, 100, 0, 120, 220, 255);
//! ```

#![no_std]
#![allow(dead_code)]

extern crate core;

pub mod types;
pub mod math;
pub mod buffer;
pub mod texture;
pub mod pipeline;
pub mod rasterizer;
pub mod context;

// ── Flat re-exports ─────────────────────────────────────────────────────────
pub use context::{GlContext, GlState, GL_MAX_FB_WIDTH, GL_MAX_FB_HEIGHT, GL_MAX_FB_PIXELS};
pub use types::*;
pub use math::{Vec2, Vec3, Vec4, Mat4, fast_sqrt, fast_sin, fast_cos, sin_cos, clamp, floor_f32};
pub use pipeline::{
    VertexIn, Varying, VertexShader, FragmentShader, Pipeline,
    PassthroughVS, FlatColorFS, TextureFS,
};
pub use rasterizer::DepthBuffer;
pub use buffer::{VertexBuffer, IndexBuffer, VertexAttrib};
pub use texture::Texture2D;
