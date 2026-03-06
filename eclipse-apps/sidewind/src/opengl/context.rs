//! `GlContext` — the main entry point of the software OpenGL implementation.
//!
//! Wraps a raw framebuffer pointer and exposes a simplified OpenGL-like API.
//! All rendering is done on the CPU; the result is written directly to the
//! framebuffer slice (BGRA u32, row-major).

use super::math::Mat4;
use super::pipeline::{Pipeline, VertexShader, FragmentShader};
use super::rasterizer::{DepthBuffer, draw_indexed_triangles, draw_arrays_triangles};
use super::types::{GLbitfield, GL_COLOR_BUFFER_BIT, GL_DEPTH_BUFFER_BIT};

// ─────────────────────────────────────────────────────────────────────────────
// Maximum framebuffer dimensions supported by the built-in depth buffer.
// Adjust if you need a larger surface.
// ─────────────────────────────────────────────────────────────────────────────
pub const GL_MAX_FB_WIDTH:  usize = 1920;
pub const GL_MAX_FB_HEIGHT: usize = 1080;
pub const GL_MAX_FB_PIXELS: usize = GL_MAX_FB_WIDTH * GL_MAX_FB_HEIGHT;

// ─────────────────────────────────────────────────────────────────────────────
// State flags
// ─────────────────────────────────────────────────────────────────────────────
#[derive(Clone, Copy)]
pub struct GlState {
    pub blend:      bool,
    pub depth_test: bool,
    pub cull_face:  bool,
}

impl GlState {
    pub const fn default() -> Self {
        Self { blend: false, depth_test: true, cull_face: false }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GlContext
// ─────────────────────────────────────────────────────────────────────────────

/// Software OpenGL context.
///
/// # Safety
/// `fb_ptr` must remain valid (non-null, aligned, in-bounds) for the entire
/// lifetime of this context.  Typically this is a VRAM BAR2 or shared-memory
/// framebuffer mapped by the kernel.
pub struct GlContext {
    /// Pointer to the first pixel of the framebuffer (BGRA u32, row-major).
    pub fb_ptr:    *mut u32,
    /// Width of the framebuffer in pixels.
    pub fb_width:  u32,
    /// Height of the framebuffer in pixels.
    pub fb_height: u32,
    /// Active viewport: (x, y, width, height).
    pub viewport:  (u32, u32, u32, u32),
    /// Clear colour [r, g, b, a] normalised to [0, 1].
    pub clear_color: [f32; 4],
    /// Render state flags.
    pub state: GlState,
    /// Software depth buffer (fixed capacity).
    pub depth: DepthBuffer<GL_MAX_FB_PIXELS>,
}

impl GlContext {
    /// Create a new context backed by `fb_ptr`.
    ///
    /// # Safety
    /// `fb_ptr` must point to at least `width * height` u32 pixels.
    pub unsafe fn new(fb_ptr: *mut u32, width: u32, height: u32) -> Self {
        Self {
            fb_ptr,
            fb_width: width,
            fb_height: height,
            viewport: (0, 0, width, height),
            clear_color: [0.0, 0.0, 0.0, 1.0],
            state: GlState::default(),
            depth: DepthBuffer::new(),
        }
    }

    // ── Framebuffer as mutable slice ─────────────────────────────────────────

    /// Return the framebuffer as a mutable slice of u32 pixels.
    ///
    /// # Safety
    /// Caller must ensure `fb_ptr` is valid.
    unsafe fn fb_slice(&mut self) -> &mut [u32] {
        let n = (self.fb_width * self.fb_height) as usize;
        core::slice::from_raw_parts_mut(self.fb_ptr, n)
    }

    // ── State setters ────────────────────────────────────────────────────────

    pub fn set_viewport(&mut self, x: u32, y: u32, w: u32, h: u32) {
        self.viewport = (x, y, w, h);
    }

    pub fn set_clear_color(&mut self, r: f32, g: f32, b: f32, a: f32) {
        self.clear_color = [r, g, b, a];
    }

    pub fn enable_blend(&mut self, on: bool)      { self.state.blend      = on; }
    pub fn enable_depth_test(&mut self, on: bool) { self.state.depth_test = on; }
    pub fn enable_cull_face(&mut self, on: bool)  { self.state.cull_face  = on; }

    // ── Clear ─────────────────────────────────────────────────────────────────

    /// Clear the buffers specified by `mask`.
    pub fn clear(&mut self, mask: GLbitfield) {
        if mask & GL_COLOR_BUFFER_BIT != 0 {
            let r = (self.clear_color[0].clamp(0.0, 1.0) * 255.0) as u32;
            let g = (self.clear_color[1].clamp(0.0, 1.0) * 255.0) as u32;
            let b = (self.clear_color[2].clamp(0.0, 1.0) * 255.0) as u32;
            let px = 0xFF00_0000 | (r << 16) | (g << 8) | b;
            unsafe {
                let fb = self.fb_slice();
                fb.fill(px);
            }
        }
        if mask & GL_DEPTH_BUFFER_BIT != 0 {
            self.depth.clear();
        }
    }

    // ── Draw calls ────────────────────────────────────────────────────────────

    /// Draw a non-indexed triangle list.
    ///
    /// `vertices` is a flat `f32` array interpreted by `pipeline`.
    /// `first` is the starting vertex index; `count` must be a multiple of 3.
    ///
    /// # Safety
    /// `fb_ptr` must remain valid.
    pub unsafe fn draw_arrays<VS, FS>(
        &mut self,
        pipeline: &Pipeline<VS, FS>,
        vertices: &[f32],
        first:    usize,
        count:    usize,
    )
    where
        VS: VertexShader,
        FS: FragmentShader,
    {
        let vp = self.viewport;
        let stride_px = self.fb_width;
        let blend = self.state.blend;
        let n = (self.fb_width * self.fb_height) as usize;
        let fb = core::slice::from_raw_parts_mut(self.fb_ptr, n);
        draw_arrays_triangles(pipeline, vertices, first, count, vp, fb, &mut self.depth, stride_px, blend);
    }

    /// Draw an indexed triangle list.
    ///
    /// `indices` is a `u32` array; every three entries form one triangle.
    ///
    /// # Safety
    /// `fb_ptr` must remain valid.
    pub unsafe fn draw_elements<VS, FS>(
        &mut self,
        pipeline: &Pipeline<VS, FS>,
        vertices: &[f32],
        indices:  &[u32],
    )
    where
        VS: VertexShader,
        FS: FragmentShader,
    {
        let vp = self.viewport;
        let stride_px = self.fb_width;
        let blend = self.state.blend;
        let n = (self.fb_width * self.fb_height) as usize;
        let fb = core::slice::from_raw_parts_mut(self.fb_ptr, n);
        draw_indexed_triangles(pipeline, vertices, indices, vp, fb, &mut self.depth, stride_px, blend);
    }

    // ── Convenience: draw a coloured quad (2 triangles) ──────────────────────

    /// Fill a screen-space rectangle with a flat colour.
    ///
    /// `x`, `y`, `w`, `h` are in pixels.  This bypasses the pipeline entirely
    /// for maximum speed.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: u8, g: u8, b: u8, a: u8) {
        let px = 0xFF00_0000u32 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
        let (vp_x, vp_y, vp_w, vp_h) = self.viewport;
        let clamp_range = |v: i32, lo: i32, hi: i32| v.max(lo).min(hi);
        let x0 = clamp_range(x, vp_x as i32, (vp_x + vp_w) as i32);
        let y0 = clamp_range(y, vp_y as i32, (vp_y + vp_h) as i32);
        let x1 = clamp_range(x + w, vp_x as i32, (vp_x + vp_w) as i32);
        let y1 = clamp_range(y + h, vp_y as i32, (vp_y + vp_h) as i32);
        if x0 >= x1 || y0 >= y1 { return; }

        let fb_width = self.fb_width;
        let fb_height = self.fb_height;
        unsafe {
            let n = (fb_width * fb_height) as usize;
            let fb = core::slice::from_raw_parts_mut(self.fb_ptr, n);
            let stride = fb_width as usize;
            for row in y0..y1 {
                let base = row as usize * stride + x0 as usize;
                let end  = base + (x1 - x0) as usize;
                if end <= fb.len() {
                    if a == 255 {
                        fb[base..end].fill(px);
                    } else {
                        let fa = a as f32 / 255.0;
                        let ia = 1.0 - fa;
                        for p in &mut fb[base..end] {
                            let dr = ((*p >> 16) & 0xFF) as f32;
                            let dg = ((*p >>  8) & 0xFF) as f32;
                            let db = ( *p        & 0xFF) as f32;
                            let nr = (r as f32 * fa + dr * ia) as u32;
                            let ng = (g as f32 * fa + dg * ia) as u32;
                            let nb = (b as f32 * fa + db * ia) as u32;
                            *p = 0xFF00_0000 | (nr << 16) | (ng << 8) | nb;
                        }
                    }
                }
            }
        }
    }

    /// Return the current MVP-identity (identity) matrix — convenience for
    /// simple 2D rendering without a camera.
    pub fn identity_mvp() -> Mat4 { Mat4::identity() }
}
