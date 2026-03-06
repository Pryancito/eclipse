//! Software OpenGL implementation for Eclipse OS / NVIDIA.
//!
//! A pure `no_std` CPU rasterizer that writes pixels directly to a mapped
//! framebuffer.

#![allow(dead_code)]

pub mod types;
pub mod math;
pub mod buffer;
pub mod texture;
pub mod pipeline;
pub mod rasterizer;
pub mod context;

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
