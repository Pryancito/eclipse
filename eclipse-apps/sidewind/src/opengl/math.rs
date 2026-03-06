//! Minimal `no_std` linear-algebra library for the software rasterizer.
//!
//! Covers Vec2/Vec3/Vec4 and Mat4 with the operations needed for a
//! perspective rendering pipeline. All arithmetic uses `f32` — no `libm`
//! required because we only use the primitives available through `core`.

use core::ops::{Add, Sub, Mul, Neg, Index, IndexMut};

// ─────────────────────────────────────────────────────────────────────────────
// Vec2
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    #[inline] pub const fn new(x: f32, y: f32) -> Self { Self { x, y } }
    #[inline] pub const fn zero() -> Self { Self { x: 0.0, y: 0.0 } }

    #[inline] pub fn dot(self, o: Self) -> f32 { self.x * o.x + self.y * o.y }
    #[inline] pub fn len_sq(self) -> f32 { self.dot(self) }

    // Integer truncation avoids `sqrtf` from libm
    #[inline] pub fn length(self) -> f32 { fast_sqrt(self.len_sq()) }

    #[inline] pub fn normalize(self) -> Self {
        let l = self.length();
        if l < 1e-10 { return Self::zero(); }
        Self::new(self.x / l, self.y / l)
    }

    #[inline] pub fn lerp(self, o: Self, t: f32) -> Self {
        Self::new(
            self.x + (o.x - self.x) * t,
            self.y + (o.y - self.y) * t,
        )
    }
}

impl Add for Vec2 { type Output = Self; fn add(self, o: Self) -> Self { Self::new(self.x + o.x, self.y + o.y) } }
impl Sub for Vec2 { type Output = Self; fn sub(self, o: Self) -> Self { Self::new(self.x - o.x, self.y - o.y) } }
impl Mul<f32> for Vec2 { type Output = Self; fn mul(self, s: f32) -> Self { Self::new(self.x * s, self.y * s) } }
impl Neg for Vec2 { type Output = Self; fn neg(self) -> Self { Self::new(-self.x, -self.y) } }

// ─────────────────────────────────────────────────────────────────────────────
// Vec3
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    #[inline] pub const fn new(x: f32, y: f32, z: f32) -> Self { Self { x, y, z } }
    #[inline] pub const fn zero() -> Self { Self { x: 0.0, y: 0.0, z: 0.0 } }
    #[inline] pub fn splat(v: f32) -> Self { Self::new(v, v, v) }

    #[inline] pub fn dot(self, o: Self) -> f32 { self.x * o.x + self.y * o.y + self.z * o.z }
    #[inline] pub fn len_sq(self) -> f32 { self.dot(self) }
    #[inline] pub fn length(self) -> f32 { fast_sqrt(self.len_sq()) }

    #[inline] pub fn normalize(self) -> Self {
        let l = self.length();
        if l < 1e-10 { return Self::zero(); }
        Self::new(self.x / l, self.y / l, self.z / l)
    }

    #[inline] pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    #[inline] pub fn lerp(self, o: Self, t: f32) -> Self {
        Self::new(
            self.x + (o.x - self.x) * t,
            self.y + (o.y - self.y) * t,
            self.z + (o.z - self.z) * t,
        )
    }

    #[inline] pub fn to_vec4(self, w: f32) -> Vec4 { Vec4::new(self.x, self.y, self.z, w) }

    /// Reflect `self` around normal `n` (both should be normalized).
    #[inline] pub fn reflect(self, n: Self) -> Self {
        self - n * (2.0 * self.dot(n))
    }
}

impl Add for Vec3 { type Output = Self; fn add(self, o: Self) -> Self { Self::new(self.x + o.x, self.y + o.y, self.z + o.z) } }
impl Sub for Vec3 { type Output = Self; fn sub(self, o: Self) -> Self { Self::new(self.x - o.x, self.y - o.y, self.z - o.z) } }
impl Mul<f32> for Vec3 { type Output = Self; fn mul(self, s: f32) -> Self { Self::new(self.x * s, self.y * s, self.z * s) } }
impl Mul<Vec3> for Vec3 { type Output = Self; fn mul(self, o: Self) -> Self { Self::new(self.x * o.x, self.y * o.y, self.z * o.z) } }
impl Neg for Vec3 { type Output = Self; fn neg(self) -> Self { Self::new(-self.x, -self.y, -self.z) } }

// ─────────────────────────────────────────────────────────────────────────────
// Vec4
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Vec4 {
    #[inline] pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self { Self { x, y, z, w } }
    #[inline] pub const fn zero() -> Self { Self { x: 0.0, y: 0.0, z: 0.0, w: 0.0 } }

    #[inline] pub fn xyz(self) -> Vec3 { Vec3::new(self.x, self.y, self.z) }
    #[inline] pub fn xy(self) -> Vec2 { Vec2::new(self.x, self.y) }

    /// Perspective divide (NDC).
    #[inline] pub fn perspective_divide(self) -> Vec3 {
        let inv_w = if self.w.abs() < 1e-10 { 0.0 } else { 1.0 / self.w };
        Vec3::new(self.x * inv_w, self.y * inv_w, self.z * inv_w)
    }

    #[inline] pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    #[inline] pub fn lerp(self, o: Self, t: f32) -> Self {
        Self::new(
            self.x + (o.x - self.x) * t,
            self.y + (o.y - self.y) * t,
            self.z + (o.z - self.z) * t,
            self.w + (o.w - self.w) * t,
        )
    }
}

impl Add for Vec4 { type Output = Self; fn add(self, o: Self) -> Self { Self::new(self.x + o.x, self.y + o.y, self.z + o.z, self.w + o.w) } }
impl Sub for Vec4 { type Output = Self; fn sub(self, o: Self) -> Self { Self::new(self.x - o.x, self.y - o.y, self.z - o.z, self.w - o.w) } }
impl Mul<f32> for Vec4 { type Output = Self; fn mul(self, s: f32) -> Self { Self::new(self.x * s, self.y * s, self.z * s, self.w * s) } }

// ─────────────────────────────────────────────────────────────────────────────
// Mat4  (column-major, matching OpenGL convention)
// ─────────────────────────────────────────────────────────────────────────────

/// Column-major 4×4 matrix. `m[col][row]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat4 {
    pub cols: [[f32; 4]; 4],
}

impl Mat4 {
    /// Construct from columns (c0..c3 each [f32; 4] = [row0, row1, row2, row3]).
    #[inline]
    pub const fn from_cols(c0: [f32; 4], c1: [f32; 4], c2: [f32; 4], c3: [f32; 4]) -> Self {
        Self { cols: [c0, c1, c2, c3] }
    }

    pub fn zero() -> Self { Self { cols: [[0.0; 4]; 4] } }

    pub fn identity() -> Self {
        Self::from_cols(
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        )
    }

    /// Translation matrix.
    pub fn translate(tx: f32, ty: f32, tz: f32) -> Self {
        Self::from_cols(
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [tx,  ty,  tz,  1.0],
        )
    }

    /// Uniform scale matrix.
    pub fn scale(sx: f32, sy: f32, sz: f32) -> Self {
        Self::from_cols(
            [sx,  0.0, 0.0, 0.0],
            [0.0, sy,  0.0, 0.0],
            [0.0, 0.0, sz,  0.0],
            [0.0, 0.0, 0.0, 1.0],
        )
    }

    /// Rotation around the X axis (radians).
    pub fn rotate_x(angle: f32) -> Self {
        let (s, c) = sin_cos(angle);
        Self::from_cols(
            [1.0, 0.0, 0.0, 0.0],
            [0.0,   c,   s, 0.0],
            [0.0,  -s,   c, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        )
    }

    /// Rotation around the Y axis (radians).
    pub fn rotate_y(angle: f32) -> Self {
        let (s, c) = sin_cos(angle);
        Self::from_cols(
            [  c, 0.0,  -s, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [  s, 0.0,   c, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        )
    }

    /// Rotation around the Z axis (radians).
    pub fn rotate_z(angle: f32) -> Self {
        let (s, c) = sin_cos(angle);
        Self::from_cols(
            [  c,   s, 0.0, 0.0],
            [ -s,   c, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        )
    }

    /// Right-handed perspective projection (OpenGL clip-space convention).
    ///
    /// - `fov_y` : vertical field of view in **radians**
    /// - `aspect`: width / height
    /// - `near`  : near clip distance (> 0)
    /// - `far`   : far  clip distance (> near)
    pub fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Self {
        let half_tan = fast_tan(fov_y * 0.5);
        let f = 1.0 / half_tan;
        let range = 1.0 / (near - far);
        Self::from_cols(
            [f / aspect, 0.0, 0.0,                        0.0],
            [0.0,          f, 0.0,                        0.0],
            [0.0,        0.0, (near + far) * range,      -1.0],
            [0.0,        0.0, 2.0 * near * far * range,   0.0],
        )
    }

    /// Right-handed orthographic projection.
    #[allow(clippy::too_many_arguments)]
    pub fn ortho(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> Self {
        let tx = -(right + left)  / (right - left);
        let ty = -(top   + bottom) / (top   - bottom);
        let tz = -(far   + near)  / (far   - near);
        Self::from_cols(
            [2.0 / (right - left), 0.0,                  0.0,               0.0],
            [0.0,                  2.0 / (top - bottom),  0.0,               0.0],
            [0.0,                  0.0,                  -2.0 / (far - near), 0.0],
            [tx,                   ty,                    tz,                1.0],
        )
    }

    /// Look-at view matrix (right-handed).
    pub fn look_at(eye: Vec3, center: Vec3, up: Vec3) -> Self {
        let f = (center - eye).normalize();
        let s = f.cross(up).normalize();
        let u = s.cross(f);
        Self::from_cols(
            [ s.x,  u.x, -f.x, 0.0],
            [ s.y,  u.y, -f.y, 0.0],
            [ s.z,  u.z, -f.z, 0.0],
            [-s.dot(eye), -u.dot(eye), f.dot(eye), 1.0],
        )
    }

    /// Transpose.
    pub fn transpose(self) -> Self {
        let c = self.cols;
        Self::from_cols(
            [c[0][0], c[1][0], c[2][0], c[3][0]],
            [c[0][1], c[1][1], c[2][1], c[3][1]],
            [c[0][2], c[1][2], c[2][2], c[3][2]],
            [c[0][3], c[1][3], c[2][3], c[3][3]],
        )
    }

    /// Transform a Vec4 by this matrix.
    pub fn mul_vec4(self, v: Vec4) -> Vec4 {
        let c = &self.cols;
        Vec4::new(
            c[0][0]*v.x + c[1][0]*v.y + c[2][0]*v.z + c[3][0]*v.w,
            c[0][1]*v.x + c[1][1]*v.y + c[2][1]*v.z + c[3][1]*v.w,
            c[0][2]*v.x + c[1][2]*v.y + c[2][2]*v.z + c[3][2]*v.w,
            c[0][3]*v.x + c[1][3]*v.y + c[2][3]*v.z + c[3][3]*v.w,
        )
    }
}

impl Mul for Mat4 {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        let mut out = Self::zero();
        for col in 0..4 {
            for row in 0..4 {
                let mut sum = 0.0f32;
                for k in 0..4 {
                    sum += self.cols[k][row] * rhs.cols[col][k];
                }
                out.cols[col][row] = sum;
            }
        }
        out
    }
}

impl Index<usize> for Mat4 {
    type Output = [f32; 4];
    fn index(&self, col: usize) -> &[f32; 4] { &self.cols[col] }
}
impl IndexMut<usize> for Mat4 {
    fn index_mut(&mut self, col: usize) -> &mut [f32; 4] { &mut self.cols[col] }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fast scalar helpers (no_std – not using libm)
// ─────────────────────────────────────────────────────────────────────────────

/// Babylonian / Newton–Raphson square root — good to ~6 ULP for positive f32.
#[inline]
pub fn fast_sqrt(x: f32) -> f32 {
    if x <= 0.0 { return 0.0; }
    // Initial guess via bit manipulation
    let i = x.to_bits();
    let j = (i >> 1).wrapping_add(0x1FBD_1DF5);
    let mut v = f32::from_bits(j);
    // Two Newton–Raphson iterations
    v = 0.5 * (v + x / v);
    v = 0.5 * (v + x / v);
    v
}

/// Taylor-series sin (works well for |angle| < 2π).
#[inline]
pub fn fast_sin(x: f32) -> f32 {
    // Reduce to [-π, π]
    const PI: f32 = core::f32::consts::PI;
    const TAU: f32 = 2.0 * PI;
    let a = x - TAU * libm_roundf(x / TAU);
    // Horner's method: sin(x) ≈ x - x³/6 + x⁵/120 - x⁷/5040
    let a2 = a * a;
    a * (1.0 - a2 * (1.0/6.0 - a2 * (1.0/120.0 - a2 * (1.0/5040.0))))
}

/// Taylor-series cos.
#[inline]
pub fn fast_cos(x: f32) -> f32 {
    use core::f32::consts::FRAC_PI_2;
    fast_sin(x + FRAC_PI_2)
}

#[inline]
pub fn sin_cos(x: f32) -> (f32, f32) { (fast_sin(x), fast_cos(x)) }

/// Approximation of tan via sin/cos.
#[inline]
pub fn fast_tan(x: f32) -> f32 {
    let c = fast_cos(x);
    if c.abs() < 1e-10 { return 1e10; }
    fast_sin(x) / c
}

/// Floor (no_std — does NOT require libm).
///
/// Returns the largest integer ≤ `x` as an `f32`.
#[inline]
pub fn floor_f32(x: f32) -> f32 {
    if x >= 2.0e9 || x <= -2.0e9 || x != x { return x; }
    let i = x as i32;
    let fi = i as f32;
    if x < fi { fi - 1.0 } else { fi }
}

/// Round to nearest float (replaces libm::roundf).
#[inline]
fn libm_roundf(x: f32) -> f32 {
    // Safe integer rounding via casting (UB-free for values in i32 range)
    if x >= 2.0e9 || x <= -2.0e9 { return x; }
    if x >= 0.0 { (x + 0.5) as i32 as f32 }
    else        { (x - 0.5) as i32 as f32 }
}

/// Clamp a float to [lo, hi].
#[inline]
pub fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo { lo } else if v > hi { hi } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fast_sqrt() {
        assert_eq!(fast_sqrt(0.0), 0.0);
        assert_eq!(fast_sqrt(1.0), 1.0);
        assert!((fast_sqrt(4.0) - 2.0).abs() < 1e-5);
        assert!((fast_sqrt(2.0) * fast_sqrt(2.0) - 2.0).abs() < 1e-4);
    }

    #[test]
    fn test_clamp() {
        assert_eq!(clamp(1.0, 0.0, 2.0), 1.0);
        assert_eq!(clamp(-1.0, 0.0, 2.0), 0.0);
        assert_eq!(clamp(3.0, 0.0, 2.0), 2.0);
    }

    #[test]
    fn test_floor_f32() {
        assert_eq!(floor_f32(1.5), 1.0);
        assert_eq!(floor_f32(-1.5), -2.0);
        assert_eq!(floor_f32(2.0), 2.0);
    }

    #[test]
    fn test_vec2() {
        let a = Vec2::new(3.0, 4.0);
        assert_eq!(a.len_sq(), 25.0);
        assert!((a.length() - 5.0).abs() < 1e-5);
        let u = a.normalize();
        assert!((u.length() - 1.0).abs() < 1e-5);
        assert!((u.x - 0.6).abs() < 1e-4);
        assert_eq!(a.dot(Vec2::new(1.0, 0.0)), 3.0);
    }

    #[test]
    fn test_vec3_cross() {
        let x = Vec3::new(1.0, 0.0, 0.0);
        let y = Vec3::new(0.0, 1.0, 0.0);
        let z = x.cross(y);
        assert!((z.x - 0.0).abs() < 1e-6);
        assert!((z.y - 0.0).abs() < 1e-6);
        assert!((z.z - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_vec4_perspective_divide() {
        let v = Vec4::new(2.0, 4.0, 6.0, 2.0);
        let p = v.perspective_divide();
        assert_eq!(p.x, 1.0);
        assert_eq!(p.y, 2.0);
        assert_eq!(p.z, 3.0);
    }

    #[test]
    fn test_mat4_identity() {
        let i = Mat4::identity();
        let v = Vec4::new(1.0, 2.0, 3.0, 1.0);
        let w = i.mul_vec4(v);
        assert_eq!(w.x, 1.0);
        assert_eq!(w.y, 2.0);
        assert_eq!(w.z, 3.0);
        assert_eq!(w.w, 1.0);
    }

    #[test]
    fn test_mat4_translate() {
        let t = Mat4::translate(10.0, 20.0, 30.0);
        let v = Vec4::new(1.0, 1.0, 1.0, 1.0);
        let w = t.mul_vec4(v);
        assert_eq!(w.x, 11.0);
        assert_eq!(w.y, 21.0);
        assert_eq!(w.z, 31.0);
        assert_eq!(w.w, 1.0);
    }

    #[test]
    fn test_mat4_scale() {
        let s = Mat4::scale(2.0, 3.0, 4.0);
        let v = Vec4::new(1.0, 1.0, 1.0, 1.0);
        let w = s.mul_vec4(v);
        assert_eq!(w.x, 2.0);
        assert_eq!(w.y, 3.0);
        assert_eq!(w.z, 4.0);
    }

    // ── Stress tests ────────────────────────────────────────────────────────

    #[test]
    fn test_stress_mat4_mul_chain() {
        const ITERS: u32 = 20_000;
        for i in 0..ITERS {
            let t = Mat4::translate((i % 100) as f32, 0.0, 0.0);
            let s = Mat4::scale(1.0, 1.0, 1.0);
            let m = t * s;
            let v = Vec4::new(1.0, 1.0, 1.0, 1.0);
            let out = m.mul_vec4(v);
            assert!(out.x >= 0.0 && out.x <= 100.0);
            assert_eq!(out.w, 1.0);
        }
    }

    #[test]
    fn test_stress_vec_normalize_and_dot() {
        const ITERS: u32 = 50_000;
        for i in 0..ITERS {
            let x = ((i % 1000) as f32) * 0.01;
            let v = Vec3::new(x, 1.0, 0.5);
            let u = v.normalize();
            let len_sq = u.dot(u);
            assert!(len_sq >= 0.99 && len_sq <= 1.01, "iter {} len_sq {}", i, len_sq);
        }
    }

    #[test]
    fn test_stress_fast_sqrt_clamp() {
        const ITERS: u32 = 100_000;
        for i in 0..ITERS {
            let x = (i as f32) * 0.001;
            let s = fast_sqrt(x);
            assert!(s >= 0.0);
            if x > 0.0 {
                let sq = s * s;
                let rel_err = ((sq - x) / x).abs();
                assert!(rel_err < 0.01, "iter {} x={} s={} rel_err={}", i, x, s, rel_err);
            }
            let c = clamp(x, 0.0, 1.0);
            assert!(c >= 0.0 && c <= 1.0);
        }
    }
}
