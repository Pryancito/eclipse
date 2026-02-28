//! Render pipeline types for `sidewind_wgpu`.
//!
//! The pipeline configuration mirrors wgpu's API for source compatibility.
//! On Eclipse OS the "shader" is always a built-in software rasteriser
//! (from `sidewind_opengl`) — no SPIR-V compilation is performed.

/// A compiled shader module.
///
/// On Eclipse OS all shaders are built-in; this struct is a named tag so
/// that descriptor code can be written in the wgpu style.
#[derive(Debug, Clone, Copy)]
pub struct ShaderModule {
    /// Index into a small table of built-in shaders.
    pub(crate) id: u32,
}

// ── Built-in shader IDs ───────────────────────────────────────────────────────

/// Passthrough vertex shader: copies position/UV from vertex buffer as-is.
pub const SHADER_PASSTHROUGH_VS: ShaderModule = ShaderModule { id: 0 };
/// Flat colour fragment shader: outputs `blend_constant` colour for every fragment.
pub const SHADER_FLAT_COLOR_FS:  ShaderModule = ShaderModule { id: 1 };
/// Texture-sampling fragment shader: samples from the bound texture.
pub const SHADER_TEXTURE_FS:     ShaderModule = ShaderModule { id: 2 };

// ── Vertex input ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct VertexState {
    pub shader: ShaderModule,
    /// Bytes between the start of consecutive vertices.
    pub stride: u64,
}

// ── Primitive assembly ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveTopology {
    PointList,
    LineList,
    LineStrip,
    TriangleList,
    TriangleStrip,
}

impl Default for PrimitiveTopology {
    fn default() -> Self {
        Self::TriangleList
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PrimitiveState {
    pub topology: PrimitiveTopology,
}

// ── Fragment output ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendFactor {
    Zero, One, SrcAlpha, OneMinusSrcAlpha,
}

#[derive(Debug, Clone, Copy)]
pub struct BlendComponent {
    pub src_factor: BlendFactor,
    pub dst_factor: BlendFactor,
}

impl Default for BlendComponent {
    fn default() -> Self {
        Self { src_factor: BlendFactor::SrcAlpha, dst_factor: BlendFactor::OneMinusSrcAlpha }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BlendState {
    pub color: BlendComponent,
    pub alpha: BlendComponent,
}

impl BlendState {
    pub const ALPHA_BLENDING: Self = Self {
        color: BlendComponent { src_factor: BlendFactor::SrcAlpha, dst_factor: BlendFactor::OneMinusSrcAlpha },
        alpha: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::OneMinusSrcAlpha },
    };
    pub const REPLACE: Self = Self {
        color: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::Zero },
        alpha: BlendComponent { src_factor: BlendFactor::One, dst_factor: BlendFactor::Zero },
    };
}

#[derive(Debug, Clone, Copy)]
pub struct ColorTargetState {
    pub format: crate::texture::TextureFormat,
    pub blend: Option<BlendState>,
    pub write_mask: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct FragmentState {
    pub shader: ShaderModule,
    pub target: ColorTargetState,
}

// ── Pipeline ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct RenderPipelineDescriptor {
    pub vertex: VertexState,
    pub primitive: PrimitiveState,
    pub fragment: FragmentState,
}

/// A compiled render pipeline.
///
/// On Eclipse OS this stores the descriptor so that the software rasteriser
/// in `sidewind_opengl` can be configured correctly when draw calls are issued.
pub struct RenderPipeline {
    pub(crate) descriptor: RenderPipelineDescriptor,
}

impl RenderPipeline {
    pub(crate) fn new(descriptor: RenderPipelineDescriptor) -> Self {
        Self { descriptor }
    }

    /// The vertex stride used by this pipeline.
    pub fn vertex_stride(&self) -> u64 {
        self.descriptor.vertex.stride
    }

    /// The primitive topology used by this pipeline.
    pub fn topology(&self) -> PrimitiveTopology {
        self.descriptor.primitive.topology
    }
}
