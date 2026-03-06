//! 2D texture with nearest-neighbour and bilinear sampling.
//!
//! Uses a flat fixed-size array `[u8; N]` where `N = W * H * 4`.
//! This avoids `generic_const_exprs` which is not yet stable.

use super::math::floor_f32;

/// A 2D RGBA texture backed by a flat array of `N` bytes (N = W * H * 4).
///
/// Construct with concrete sizes:
/// ```no_run
/// use sidewind::opengl::Texture2D;
/// let tex: Texture2D<1920, 1080, {1920 * 1080 * 4}> = Texture2D::new();
/// ```
pub struct Texture2D<const W: usize, const H: usize, const N: usize> {
    pixels: [u8; N], // RGBA, row-major (4 bytes per pixel)
}

impl<const W: usize, const H: usize, const N: usize> Texture2D<W, H, N> {
    /// Create a fully transparent black texture.
    pub const fn new() -> Self {
        assert!(N == W * H * 4, "N must equal W * H * 4");
        Self { pixels: [0u8; N] }
    }

    /// Fill the entire texture with a solid colour.
    pub fn fill(&mut self, rgba: [u8; 4]) {
        let mut i = 0;
        while i + 3 < N {
            self.pixels[i    ] = rgba[0];
            self.pixels[i + 1] = rgba[1];
            self.pixels[i + 2] = rgba[2];
            self.pixels[i + 3] = rgba[3];
            i += 4;
        }
    }

    /// Upload a contiguous RGBA slice, row-major.
    ///
    /// `data` must have length `>= src_w * src_h * 4` bytes.
    pub fn upload_region(&mut self, x: u32, y: u32, src_w: u32, src_h: u32, data: &[u8]) -> bool {
        if (x + src_w) > W as u32 || (y + src_h) > H as u32 { return false; }
        if data.len() < (src_w * src_h * 4) as usize { return false; }
        for row in 0..src_h {
            for col in 0..src_w {
                let src_idx = ((row * src_w + col) * 4) as usize;
                let dst_px  = (((y + row) * W as u32 + (x + col)) * 4) as usize;
                if dst_px + 3 < N {
                    self.pixels[dst_px    ] = data[src_idx    ];
                    self.pixels[dst_px + 1] = data[src_idx + 1];
                    self.pixels[dst_px + 2] = data[src_idx + 2];
                    self.pixels[dst_px + 3] = data[src_idx + 3];
                }
            }
        }
        true
    }

    #[inline]
    fn frac(x: f32) -> f32 { x - floor_f32(x) }

    /// Nearest-neighbour sample at normalised coordinates `(u, v)` ∈ [0, 1].
    /// Wraps (GL_REPEAT semantics).
    #[inline]
    pub fn sample(&self, u: f32, v: f32) -> [u8; 4] {
        let u = Self::frac(u);
        let v = Self::frac(v);
        let px = ((u * W as f32) as usize).min(W - 1);
        let py = ((v * H as f32) as usize).min(H - 1);
        let idx = (py * W + px) * 4;
        if idx + 3 < N {
            [self.pixels[idx], self.pixels[idx+1], self.pixels[idx+2], self.pixels[idx+3]]
        } else { [0, 0, 0, 255] }
    }

    /// Clamp-to-edge sample.
    #[inline]
    pub fn sample_clamp(&self, u: f32, v: f32) -> [u8; 4] {
        let u = u.clamp(0.0, 1.0);
        let v = v.clamp(0.0, 1.0);
        let px = ((u * W as f32) as usize).min(W - 1);
        let py = ((v * H as f32) as usize).min(H - 1);
        let idx = (py * W + px) * 4;
        if idx + 3 < N {
            [self.pixels[idx], self.pixels[idx+1], self.pixels[idx+2], self.pixels[idx+3]]
        } else { [0, 0, 0, 255] }
    }

    /// Bilinear sample.
    pub fn sample_bilinear(&self, u: f32, v: f32) -> [u8; 4] {
        let u = Self::frac(u);
        let v = Self::frac(v);
        let fx = u * (W - 1) as f32;
        let fy = v * (H - 1) as f32;
        let x0 = (fx as usize).min(W - 1);
        let y0 = (fy as usize).min(H - 1);
        let x1 = (x0 + 1).min(W - 1);
        let y1 = (y0 + 1).min(H - 1);
        let tx = fx - x0 as f32;
        let ty = fy - y0 as f32;

        let lerp_u8 = |a: u8, b: u8, t: f32| -> u8 {
            (a as f32 + (b as f32 - a as f32) * t) as u8
        };
        let lerp_px = |p0: [u8;4], p1: [u8;4], t: f32| [
            lerp_u8(p0[0],p1[0],t), lerp_u8(p0[1],p1[1],t),
            lerp_u8(p0[2],p1[2],t), lerp_u8(p0[3],p1[3],t),
        ];

        let g = |px: usize, py: usize| -> [u8; 4] {
            let i = (py * W + px) * 4;
            if i + 3 < N { [self.pixels[i],self.pixels[i+1],self.pixels[i+2],self.pixels[i+3]] }
            else { [0,0,0,255] }
        };
        lerp_px(lerp_px(g(x0,y0), g(x1,y0), tx), lerp_px(g(x0,y1), g(x1,y1), tx), ty)
    }

    #[inline] pub fn width(&self)  -> u32 { W as u32 }
    #[inline] pub fn height(&self) -> u32 { H as u32 }
}
