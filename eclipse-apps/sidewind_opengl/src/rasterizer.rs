//! Software triangle rasterizer — barycentric coverage + linear z-buffer.
//!
//! The rasterizer operates entirely on the CPU, writing BGRA u32 pixels
//! directly to a raw framebuffer pointer.  This eliminates any GPU dependency
//! while providing a complete, correct OpenGL-like raster pipeline.
//!
//! ## Algorithm
//! For each triangle (after vertex shading + perspective divide + viewport
//! transform), the rasterizer:
//! 1. Computes the bounding box clipped to the viewport.
//! 2. Iterates every covered pixel, testing barycentric coverage.
//! 3. Perspective-correct interpolation of varyings.
//! 4. Z-buffer test (less-than, depth in NDC [-1, +1]).
//! 5. Calls the fragment shader; blends if alpha < 1.
//! 6. Writes the resulting colour as BGRA u32.

use crate::math::{Vec2, Vec4, clamp};
use crate::pipeline::{Pipeline, VertexShader, FragmentShader, Varying};

// ─────────────────────────────────────────────────────────────────────────────
// Screen-space vertex
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct ScreenVert {
    sx: f32,      // screen x (pixels)
    sy: f32,      // screen y (pixels)
    sz: f32,      // NDC z (for depth buffer)
    inv_w: f32,   // 1/clip_w  (for perspective correction)
    vary: Varying,
}

// ─────────────────────────────────────────────────────────────────────────────
// Rasterizer state
// ─────────────────────────────────────────────────────────────────────────────

/// Fixed-size software depth buffer.
///
/// `N = MAX_WIDTH * MAX_HEIGHT`.  Initialise it with `f32::INFINITY` each
/// frame via `clear_depth()`.
pub struct DepthBuffer<const N: usize> {
    data: [f32; N],
}

impl<const N: usize> DepthBuffer<N> {
    pub const fn new() -> Self { Self { data: [f32::INFINITY; N] } }
    pub fn clear(&mut self) { self.data.fill(f32::INFINITY); }
    #[inline] fn get(&self, idx: usize) -> f32 { self.data.get(idx).copied().unwrap_or(f32::INFINITY) }
    #[inline] fn set(&mut self, idx: usize, v: f32) { if let Some(d) = self.data.get_mut(idx) { *d = v; } }
}

// ─────────────────────────────────────────────────────────────────────────────
// Main rasterize_triangle function
// ─────────────────────────────────────────────────────────────────────────────

/// Rasterize one triangle into `fb` (BGRA u32 layout, row-major).
///
/// - `varyings`   : three post-vertex-shader outputs.
/// - `viewport`   : (x, y, width, height) in pixels.
/// - `fb`         : mutable slice of the framebuffer (`stride` u32 per row).
/// - `depth`      : mutable depth buffer (must be `stride * height` entries).
/// - `stride`     : pixels per row in `fb` (may differ from viewport width).
#[allow(clippy::too_many_arguments)]
pub fn rasterize_triangle<const DN: usize, FS: FragmentShader>(
    varyings: [Varying; 3],
    viewport: (u32, u32, u32, u32),  // x, y, w, h
    fb:       &mut [u32],
    depth:    &mut DepthBuffer<DN>,
    stride:   u32,
    fs:       &FS,
    blend:    bool,
) {
    let (vp_x, vp_y, vp_w, vp_h) = viewport;

    // ── Clip-space → NDC → Screen-space ──────────────────────────────────────
    let sverts: [ScreenVert; 3] = {
        let mut out = [ScreenVert { sx: 0.0, sy: 0.0, sz: 0.0, inv_w: 0.0, vary: Varying::zero() }; 3];
        for (i, v) in varyings.iter().enumerate() {
            let clip = v.clip_pos;
            // Perspective divide
            let inv_w = if clip.w.abs() < 1e-10 { 0.0 } else { 1.0 / clip.w };
            let ndcx = clip.x * inv_w;
            let ndcy = clip.y * inv_w;
            let ndcz = clip.z * inv_w;
            // Viewport transform
            let sx = (ndcx + 1.0) * 0.5 * vp_w as f32 + vp_x as f32;
            let sy = (1.0 - ndcy) * 0.5 * vp_h as f32 + vp_y as f32; // flip Y
            out[i] = ScreenVert { sx, sy, sz: ndcz, inv_w, vary: *v };
        }
        out
    };

    let [s0, s1, s2] = sverts;

    // ── Bounding box (clipped to viewport) ───────────────────────────────────
    let min_x = s0.sx.min(s1.sx).min(s2.sx).max(vp_x as f32) as i32;
    let max_x = (s0.sx.max(s1.sx).max(s2.sx) + 1.0).min((vp_x + vp_w) as f32) as i32;
    let min_y = s0.sy.min(s1.sy).min(s2.sy).max(vp_y as f32) as i32;
    let max_y = (s0.sy.max(s1.sy).max(s2.sy) + 1.0).min((vp_y + vp_h) as f32) as i32;

    if min_x >= max_x || min_y >= max_y { return; }

    // ── Edge function helpers (sign of the 2D cross product) ─────────────────
    let edge = |ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32| -> f32 {
        (bx - ax) * (py - ay) - (by - ay) * (px - ax)
    };

    let area = edge(s0.sx, s0.sy, s1.sx, s1.sy, s2.sx, s2.sy);
    if area.abs() < 1.0 { return; } // degenerate
    let inv_area = 1.0 / area;

    // ── Pixel loop ─────────────────────────────────────────────────────────
    for py in min_y..max_y {
        for px in min_x..max_x {
            let pfx = px as f32 + 0.5;
            let pfy = py as f32 + 0.5;

            // Barycentric weights
            let w0 = edge(s1.sx, s1.sy, s2.sx, s2.sy, pfx, pfy) * inv_area;
            let w1 = edge(s2.sx, s2.sy, s0.sx, s0.sy, pfx, pfy) * inv_area;
            let w2 = 1.0 - w0 - w1;

            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 { continue; }

            // Perspective-correct interpolation
            let inv_w = s0.inv_w * w0 + s1.inv_w * w1 + s2.inv_w * w2;
            let corr  = if inv_w.abs() < 1e-10 { 0.0 } else { 1.0 / inv_w };

            let interp_vec = |a: f32, b: f32, c: f32| -> f32 {
                (a * s0.inv_w * w0 + b * s1.inv_w * w1 + c * s2.inv_w * w2) * corr
            };

            let z = s0.sz * w0 + s1.sz * w1 + s2.sz * w2;

            // Depth test
            let fb_idx = (py as u32 * stride + px as u32) as usize;
            if z >= depth.get(fb_idx) { continue; }

            // Interpolate varyings
            let color = [
                interp_vec(s0.vary.color[0], s1.vary.color[0], s2.vary.color[0]),
                interp_vec(s0.vary.color[1], s1.vary.color[1], s2.vary.color[1]),
                interp_vec(s0.vary.color[2], s1.vary.color[2], s2.vary.color[2]),
                interp_vec(s0.vary.color[3], s1.vary.color[3], s2.vary.color[3]),
            ];
            let uv = Vec2::new(
                interp_vec(s0.vary.uv.x, s1.vary.uv.x, s2.vary.uv.x),
                interp_vec(s0.vary.uv.y, s1.vary.uv.y, s2.vary.uv.y),
            );
            let normal = crate::math::Vec3::new(
                interp_vec(s0.vary.normal.x, s1.vary.normal.x, s2.vary.normal.x),
                interp_vec(s0.vary.normal.y, s1.vary.normal.y, s2.vary.normal.y),
                interp_vec(s0.vary.normal.z, s1.vary.normal.z, s2.vary.normal.z),
            );

            let frag_in = Varying {
                clip_pos: Vec4::new(0.0, 0.0, z, 1.0), // only z is meaningful
                color,
                uv,
                normal,
            };

            // Fragment shader
            let Some(fc) = fs.process(frag_in) else { continue };

            let fr = clamp(fc[0], 0.0, 1.0);
            let fg = clamp(fc[1], 0.0, 1.0);
            let fb_c = clamp(fc[2], 0.0, 1.0);
            let fa = clamp(fc[3], 0.0, 1.0);

            let rb = (fr * 255.0) as u32;
            let gb = (fg * 255.0) as u32;
            let bb = (fb_c * 255.0) as u32;

            let new_px = if blend && fa < 1.0 {
                // Alpha blend over existing pixel (src-alpha, 1-src-alpha)
                let dst = fb.get(fb_idx).copied().unwrap_or(0);
                let dr = (dst >> 16) & 0xFF;
                let dg = (dst >>  8) & 0xFF;
                let db =  dst        & 0xFF;
                let ia = (1.0 - fa) * 255.0;
                let ar = fa;
                let nr = ((rb as f32 * fa * 255.0 + dr as f32 * ia) / 255.0) as u32;
                let ng = ((gb as f32 * fa * 255.0 + dg as f32 * ia) / 255.0) as u32;
                let nb = ((bb as f32 * fa * 255.0 + db as f32 * ia) / 255.0) as u32;
                0xFF00_0000 | (nr.min(255) << 16) | (ng.min(255) << 8) | nb.min(255)
            } else {
                0xFF00_0000 | (rb << 16) | (gb << 8) | bb  // BGRA / ARGB (XRGB with A=0xFF)
            };

            if let Some(p) = fb.get_mut(fb_idx) { *p = new_px; }
            depth.set(fb_idx, z);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Rasterizer entry-point: draw indexed triangles
// ─────────────────────────────────────────────────────────────────────────────

/// Draw an indexed triangle list.
///
/// - `vertices` : flat float array (interpreted by `pipeline`).
/// - `indices`  : every 3 indices form one triangle.
#[allow(clippy::too_many_arguments)]
pub fn draw_indexed_triangles<const DN: usize, VS: VertexShader, FS: FragmentShader>(
    pipeline:  &Pipeline<VS, FS>,
    vertices:  &[f32],
    indices:   &[u32],
    viewport:  (u32, u32, u32, u32),
    fb:        &mut [u32],
    depth:     &mut DepthBuffer<DN>,
    stride_px: u32,
    blend:     bool,
) {
    let tri_count = indices.len() / 3;
    for t in 0..tri_count {
        let i0 = indices[t * 3    ] as usize;
        let i1 = indices[t * 3 + 1] as usize;
        let i2 = indices[t * 3 + 2] as usize;

        let v0 = pipeline.shade_vertex(pipeline.build_vertex(vertices, i0));
        let v1 = pipeline.shade_vertex(pipeline.build_vertex(vertices, i1));
        let v2 = pipeline.shade_vertex(pipeline.build_vertex(vertices, i2));

        rasterize_triangle([v0, v1, v2], viewport, fb, depth, stride_px, &pipeline.fragment_shader, blend);
    }
}

/// Draw a non-indexed triangle list (every 3 vertices = 1 triangle).
pub fn draw_arrays_triangles<const DN: usize, VS: VertexShader, FS: FragmentShader>(
    pipeline:  &Pipeline<VS, FS>,
    vertices:  &[f32],
    first:     usize,
    count:     usize,
    viewport:  (u32, u32, u32, u32),
    fb:        &mut [u32],
    depth:     &mut DepthBuffer<DN>,
    stride_px: u32,
    blend:     bool,
) {
    let tri_count = count / 3;
    for t in 0..tri_count {
        let i0 = first + t * 3;
        let i1 = i0 + 1;
        let i2 = i0 + 2;

        let v0 = pipeline.shade_vertex(pipeline.build_vertex(vertices, i0));
        let v1 = pipeline.shade_vertex(pipeline.build_vertex(vertices, i1));
        let v2 = pipeline.shade_vertex(pipeline.build_vertex(vertices, i2));

        rasterize_triangle([v0, v1, v2], viewport, fb, depth, stride_px, &pipeline.fragment_shader, blend);
    }
}
