//! Vertex / fragment shader traits and pipeline glue.
//!
//! By implementing `VertexShader` and `FragmentShader` you define the
//! programmable stages of the rasterizer. The pipeline connects them and
//! passes interpolated varyings to the fragment stage.

use crate::math::{Vec2, Vec3, Vec4};

// ─────────────────────────────────────────────────────────────────────────────
// Per-vertex input coming from the vertex buffer
// ─────────────────────────────────────────────────────────────────────────────

/// Raw vertex data extracted from a `VertexBuffer`.
///
/// At most 16 floats per vertex (position + normals + 2× UV + colour).
#[derive(Debug, Clone, Copy)]
pub struct VertexIn {
    /// Raw float components (up to 16).
    pub data: [f32; 16],
    /// Number of meaningful components.
    pub count: usize,
}

impl VertexIn {
    pub fn position(&self) -> Vec4 {
        Vec4::new(
            *self.data.get(0).unwrap_or(&0.0),
            *self.data.get(1).unwrap_or(&0.0),
            *self.data.get(2).unwrap_or(&0.0),
            *self.data.get(3).unwrap_or(&1.0),
        )
    }
    pub fn uv(&self) -> Vec2 {
        Vec2::new(
            *self.data.get(3).unwrap_or(&0.0),
            *self.data.get(4).unwrap_or(&0.0),
        )
    }
    pub fn normal(&self) -> Vec3 {
        Vec3::new(
            *self.data.get(5).unwrap_or(&0.0),
            *self.data.get(6).unwrap_or(&0.0),
            *self.data.get(7).unwrap_or(&1.0),
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Vertex shader output / fragment shader input (varyings)
// ─────────────────────────────────────────────────────────────────────────────

/// Output of the vertex shader.  Also serves as the interpolated input to the
/// fragment shader.  Keep to ≤ 8 varyings to stay cache-friendly.
#[derive(Debug, Clone, Copy)]
pub struct Varying {
    /// Clip-space position (before perspective divide).
    pub clip_pos: Vec4,
    /// Interpolated colour [r, g, b, a] in [0, 1].
    pub color: [f32; 4],
    /// Interpolated texture coordinates.
    pub uv: Vec2,
    /// Interpolated world-space normal.
    pub normal: Vec3,
}

impl Varying {
    pub const fn zero() -> Self {
        Self {
            clip_pos: Vec4::new(0.0, 0.0, 0.0, 1.0),
            color: [1.0, 1.0, 1.0, 1.0],
            uv: Vec2::new(0.0, 0.0),
            normal: Vec3::new(0.0, 0.0, 1.0),
        }
    }

    /// Linearly interpolate varyings (used during rasterisation).
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        Self {
            clip_pos: a.clip_pos.lerp(b.clip_pos, t),
            color: [
                a.color[0] + (b.color[0] - a.color[0]) * t,
                a.color[1] + (b.color[1] - a.color[1]) * t,
                a.color[2] + (b.color[2] - a.color[2]) * t,
                a.color[3] + (b.color[3] - a.color[3]) * t,
            ],
            uv: a.uv.lerp(b.uv, t),
            normal: a.normal.lerp(b.normal, t),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Shader traits
// ─────────────────────────────────────────────────────────────────────────────

/// Vertex shader — transforms one vertex into clip-space.
pub trait VertexShader {
    fn process(&self, v: VertexIn) -> Varying;
}

/// Fragment shader — shades one pixel given interpolated varyings.
///
/// Returns `[r, g, b, a]` in the range `[0, 1]`.
/// Return `None` to discard the fragment (alpha-test / clip).
pub trait FragmentShader {
    fn process(&self, v: Varying) -> Option<[f32; 4]>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Built-in shaders
// ─────────────────────────────────────────────────────────────────────────────

/// Passthrough vertex shader: uses the first 4 floats as clip-space XYZ+W,
/// floats 4–5 as UV, floats 6–9 as RGBA colour.
pub struct PassthroughVS {
    pub mvp: crate::math::Mat4,
}
impl PassthroughVS {
    pub fn new(mvp: crate::math::Mat4) -> Self { Self { mvp } }
}
impl VertexShader for PassthroughVS {
    fn process(&self, v: VertexIn) -> Varying {
        let pos = v.position();
        let clip = self.mvp.mul_vec4(pos);
        Varying {
            clip_pos: clip,
            color: [
                *v.data.get(6).unwrap_or(&1.0),
                *v.data.get(7).unwrap_or(&1.0),
                *v.data.get(8).unwrap_or(&1.0),
                *v.data.get(9).unwrap_or(&1.0),
            ],
            uv: v.uv(),
            normal: Vec3::new(0.0, 0.0, 1.0),
        }
    }
}

/// Flat colour fragment shader — uses the interpolated `varying.color`.
pub struct FlatColorFS;
impl FragmentShader for FlatColorFS {
    fn process(&self, v: Varying) -> Option<[f32; 4]> {
        Some(v.color)
    }
}

/// Fragment shader that samples a texture (must be supplied via a `&[u8]` map).
pub struct TextureFS<'a> {
    /// Flat RGBA byte array — width × height × 4 bytes, row-major.
    pub texels:  &'a [u8],
    pub tex_w:   u32,
    pub tex_h:   u32,
}
impl<'a> FragmentShader for TextureFS<'a> {
    fn process(&self, v: Varying) -> Option<[f32; 4]> {
        let u = v.uv.x - crate::math::floor_f32(v.uv.x);
        let vv = v.uv.y - crate::math::floor_f32(v.uv.y);
        let px = ((u * self.tex_w as f32) as u32).min(self.tex_w  - 1);
        let py = ((vv * self.tex_h as f32) as u32).min(self.tex_h - 1);
        let idx = ((py * self.tex_w + px) * 4) as usize;
        if idx + 3 >= self.texels.len() { return Some([1.0, 0.0, 1.0, 1.0]); }
        Some([
            self.texels[idx    ] as f32 / 255.0,
            self.texels[idx + 1] as f32 / 255.0,
            self.texels[idx + 2] as f32 / 255.0,
            self.texels[idx + 3] as f32 / 255.0,
        ])
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pipeline
// ─────────────────────────────────────────────────────────────────────────────

/// Ties a vertex shader and a fragment shader together for use with
/// `GlContext::draw_triangles`.
pub struct Pipeline<VS, FS>
where
    VS: VertexShader,
    FS: FragmentShader,
{
    pub vertex_shader:   VS,
    pub fragment_shader: FS,
    /// Floating-point stride (bytes per vertex in the buffer).
    pub stride: u32,
    /// Byte offset of the position attribute.
    pub pos_offset: u32,
    /// Number of components in the position attribute (3 or 4).
    pub pos_components: u8,
}

impl<VS: VertexShader, FS: FragmentShader> Pipeline<VS, FS> {
    /// Create a pipeline with explicit stride and offset.
    pub fn new(vs: VS, fs: FS, stride: u32, pos_offset: u32, pos_components: u8) -> Self {
        Self {
            vertex_shader: vs,
            fragment_shader: fs,
            stride,
            pos_offset,
            pos_components,
        }
    }

    /// Build a `VertexIn` for vertex `idx` from a flat float slice.
    pub fn build_vertex(&self, vertices: &[f32], idx: usize) -> VertexIn {
        let stride_f = (self.stride / 4) as usize;
        let base = idx * stride_f;
        let mut data = [0.0f32; 16];
        let count = (stride_f).min(16);
        for i in 0..count {
            data[i] = *vertices.get(base + i).unwrap_or(&0.0);
        }
        VertexIn { data, count }
    }

    pub fn shade_vertex(&self, v: VertexIn) -> Varying {
        self.vertex_shader.process(v)
    }

    pub fn shade_fragment(&self, v: Varying) -> Option<[f32; 4]> {
        self.fragment_shader.process(v)
    }
}
