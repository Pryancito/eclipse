//! math.h — Math functions
//!
//! Software implementations of IEEE 754 double- and single-precision
//! transcendental functions. These replace the system libm for Eclipse OS
//! targets, which have no libm in the sysroot.
use crate::types::*;

// ─── cfg shorthand ────────────────────────────────────────────────────────────
// All public C symbols are only emitted for Eclipse-OS targets (or non-Linux
// hosts) to avoid symbol collisions with the system libm during host builds.
#[cfg(all(
    not(any(test, feature = "host-testing")),
    any(
        target_os = "eclipse",
        eclipse_target,
        not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))))
    )
))]
mod imp {
    use crate::types::*;

    // ── compile-time constants ─────────────────────────────────────────────────
    const PI: f64 = core::f64::consts::PI;
    const TAU: f64 = 2.0 * PI;
    const FRAC_PI_2: f64 = PI / 2.0;
    const FRAC_PI_4: f64 = PI / 4.0;
    // ln(2) and its reciprocal (log₂(e))
    const LN2: f64 = core::f64::consts::LN_2;
    const LOG2_E: f64 = core::f64::consts::LOG2_E;
    // log₁₀(e)
    const LOG10_E: f64 = 0.434_294_481_903_251_82_f64;
    // 2^52 — largest f64 with no fractional part
    const NO_FRAC: f64 = 4.503_599_627_370_496e15_f64;

    // ── helpers ───────────────────────────────────────────────────────────────

    #[inline(always)]
    pub(super) fn abs64(x: f64) -> f64 {
        f64::from_bits(x.to_bits() & 0x7FFF_FFFF_FFFF_FFFF)
    }

    #[inline(always)]
    pub(super) fn abs32(x: f32) -> f32 {
        f32::from_bits(x.to_bits() & 0x7FFF_FFFF)
    }

    /// floor for f64 via integer truncation.
    #[inline(always)]
    pub(super) fn ifloor64(x: f64) -> f64 {
        if !x.is_finite() || abs64(x) >= NO_FRAC {
            return x;
        }
        let t = x as i64 as f64;
        if t > x { t - 1.0 } else { t }
    }

    // ── sin / cos core polynomials ────────────────────────────────────────────
    //   Valid on [0, π/4].  With 7 terms the truncation error is < 6e-15
    //   for |x| ≤ π/4, which satisfies f64 precision requirements.

    #[inline(always)]
    fn sin_core(x: f64) -> f64 {
        let x2 = x * x;
        x * (1.0
            + x2 * (-1.666_666_666_666_666_8e-1
            + x2 * (8.333_333_333_333_167e-3
            + x2 * (-1.984_126_984_120_503_5e-4
            + x2 * (2.755_731_922_378_143e-6
            + x2 * (-2.505_210_757_490_748e-8
            + x2 * 1.605_904_138_901e-10))))))
    }

    #[inline(always)]
    fn cos_core(x: f64) -> f64 {
        let x2 = x * x;
        1.0 + x2
            * (-0.5
            + x2 * (4.166_666_666_666_668e-2
            + x2 * (-1.388_888_888_889_136e-3
            + x2 * (2.480_158_730_156_3e-5
            + x2 * (-2.755_731_921_777_5e-7
            + x2 * 2.087_536_706e-9)))))
    }

    /// sin for x ∈ [0, π/2] — half-quadrant reduction to keep |x| ≤ π/4.
    #[inline(always)]
    fn sin_halfpi(x: f64) -> f64 {
        if x <= FRAC_PI_4 { sin_core(x) } else { cos_core(FRAC_PI_2 - x) }
    }

    /// cos for x ∈ [0, π/2] — half-quadrant reduction to keep |x| ≤ π/4.
    #[inline(always)]
    fn cos_halfpi(x: f64) -> f64 {
        if x <= FRAC_PI_4 { cos_core(x) } else { sin_core(FRAC_PI_2 - x) }
    }

    // ── sin64 / cos64 (full range) ─────────────────────────────────────────────

    pub(super) fn sin64(x: f64) -> f64 {
        if !x.is_finite() {
            return f64::NAN;
        }
        let sign = if x < 0.0 { -1.0_f64 } else { 1.0 };
        let x = abs64(x);
        // Reduce to [0, 2π).
        let n = (x / TAU) as u64;
        let r = x - n as f64 * TAU;
        let v = if r <= FRAC_PI_2 {
            sin_halfpi(r)
        } else if r <= PI {
            sin_halfpi(PI - r)
        } else if r <= 3.0 * FRAC_PI_2 {
            -sin_halfpi(r - PI)
        } else {
            -sin_halfpi(TAU - r)
        };
        sign * v
    }

    pub(super) fn cos64(x: f64) -> f64 {
        if !x.is_finite() {
            return f64::NAN;
        }
        let x = abs64(x);
        let n = (x / TAU) as u64;
        let r = x - n as f64 * TAU;
        if r <= FRAC_PI_2 {
            cos_halfpi(r)
        } else if r <= PI {
            -cos_halfpi(PI - r)
        } else if r <= 3.0 * FRAC_PI_2 {
            -cos_halfpi(r - PI)
        } else {
            cos_halfpi(TAU - r)
        }
    }

    // ── sqrt64 ────────────────────────────────────────────────────────────────
    // Newton-Raphson, 4 iterations → < 1 ULP for positive normal numbers.

    pub(super) fn sqrt64(x: f64) -> f64 {
        if x < 0.0 {
            return f64::NAN;
        }
        if x == 0.0 || !x.is_finite() {
            return x;
        }
        let bits = x.to_bits();
        let mut v = f64::from_bits((bits.wrapping_add(0x3FF0_0000_0000_0000)) >> 1);
        v = 0.5 * (v + x / v);
        v = 0.5 * (v + x / v);
        v = 0.5 * (v + x / v);
        v = 0.5 * (v + x / v);
        v
    }

    // ── atan core / atan64 / atan2_64 ─────────────────────────────────────────
    // tan(π/8) = √2 - 1 ≈ 0.41421 — used for argument reduction.
    const TAN_PI_8: f64 = 0.414_213_562_373_095_05_f64;

    /// Polynomial for atan(x)/x on [0, tan(π/8)] ≈ [0, 0.414].
    /// 8 terms → truncation error < 8e-9 at x = tan(π/8).  Good for graphics.
    #[inline(always)]
    fn atan_core(x: f64) -> f64 {
        let x2 = x * x;
        x * (1.0
            + x2 * (-3.333_333_333_333_3e-1
            + x2 * (2.000_000_000_000_0e-1
            + x2 * (-1.428_571_428_571_4e-1
            + x2 * (1.111_111_111_111_1e-1
            + x2 * (-9.090_909_090_909_1e-2
            + x2 * (7.692_307_692_307_7e-2
            + x2 * (-6.666_666_666_666_7e-2))))))))
    }

    pub(super) fn atan64(x: f64) -> f64 {
        if !x.is_finite() {
            if x.is_nan() {
                return x;
            }
            return if x > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 };
        }
        let sign = if x < 0.0 { -1.0_f64 } else { 1.0 };
        let ax = abs64(x);
        // Step 1: reduce |x| > 1  →  x' = 1/x, result = π/2 − atan(x')
        let (ax, offset) = if ax > 1.0 {
            (1.0 / ax, FRAC_PI_2)
        } else {
            (ax, 0.0)
        };
        // Step 2: reduce x' > tan(π/8)  →  x'' = (x'−1)/(x'+1), result = π/4 + atan(x'')
        let (ax, offset) = if ax > TAN_PI_8 {
            ((ax - 1.0) / (ax + 1.0), offset + FRAC_PI_4)
        } else {
            (ax, offset)
        };
        let result = offset + atan_core(ax);
        sign * result
    }

    pub(super) fn atan2_64(y: f64, x: f64) -> f64 {
        if y.is_nan() || x.is_nan() {
            return f64::NAN;
        }
        if x == 0.0 {
            if y > 0.0 {
                return FRAC_PI_2;
            }
            if y < 0.0 {
                return -FRAC_PI_2;
            }
            return 0.0;
        }
        if y == 0.0 {
            return if x > 0.0 { 0.0 } else { PI };
        }
        if x.is_infinite() {
            if y.is_infinite() {
                return if x > 0.0 {
                    if y > 0.0 { FRAC_PI_4 } else { -FRAC_PI_4 }
                } else {
                    if y > 0.0 { 3.0 * FRAC_PI_4 } else { -3.0 * FRAC_PI_4 }
                };
            }
            return if x > 0.0 { 0.0 } else if y >= 0.0 { PI } else { -PI };
        }
        if y.is_infinite() {
            return if y > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 };
        }
        let t = atan64(y / x);
        if x > 0.0 {
            t
        } else if y >= 0.0 {
            t + PI
        } else {
            t - PI
        }
    }

    // ── asin64 / acos64 ───────────────────────────────────────────────────────

    pub(super) fn asin64(x: f64) -> f64 {
        let ax = abs64(x);
        if ax > 1.0 {
            return f64::NAN;
        }
        if ax == 1.0 {
            return if x > 0.0 { FRAC_PI_2 } else { -FRAC_PI_2 };
        }
        // asin(x) = atan(x / sqrt(1 - x²))
        let result = atan64(x / sqrt64(1.0 - x * x));
        result
    }

    pub(super) fn acos64(x: f64) -> f64 {
        FRAC_PI_2 - asin64(x)
    }

    // ── exp64 ─────────────────────────────────────────────────────────────────

    pub(super) fn exp64(x: f64) -> f64 {
        if x > 709.782_711_149_557_4 {
            return f64::INFINITY;
        }
        if x < -745.133_219_101_941_6 {
            return 0.0;
        }
        if x.is_nan() {
            return x;
        }
        // e^x = 2^n * e^r, where n = round(x / ln2), r = x - n*ln2.
        let n = (x * LOG2_E + 0.5) as i64;
        let r = x - n as f64 * LN2;
        // Minimax polynomial for e^r on [−ln2/2, ln2/2].
        let r2 = r * r;
        let p = 1.0
            + r
            + r2 * (5.0e-1
            + r * (1.666_666_666_7e-1
            + r * (4.166_666_667e-2
            + r * (8.333_333e-3
            + r * (1.388_89e-3
            + r * 1.984e-4)))));
        // Scale mantissa by 2^n via biased-exponent arithmetic.
        let bits = p.to_bits();
        let e = (bits >> 52) as i64 + n;
        if e >= 2047 {
            return f64::INFINITY;
        }
        if e <= 0 {
            return 0.0;
        }
        f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | ((e as u64) << 52))
    }

    // ── log64 ─────────────────────────────────────────────────────────────────

    pub(super) fn log64(x: f64) -> f64 {
        if x.is_nan() {
            return x;
        }
        if x < 0.0 {
            return f64::NAN;
        }
        if x == 0.0 {
            return f64::NEG_INFINITY;
        }
        if x.is_infinite() {
            return f64::INFINITY;
        }
        let bits = x.to_bits();
        let exp = ((bits >> 52) & 0x7FF) as i64 - 1023;
        let m = f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
        // log(m) for m ∈ [1, 2) via atanh((m-1)/(m+1)) * 2.
        let u = (m - 1.0) / (m + 1.0);
        let u2 = u * u;
        let log_m = 2.0
            * u
            * (1.0
            + u2 * (3.333_333_333e-1
            + u2 * (2.0e-1
            + u2 * (1.428_571_4e-1
            + u2 * (1.111_111e-1
            + u2 * (9.09e-2
            + u2 * 7.69e-2))))));
        log_m + exp as f64 * LN2
    }

    // ── pow64 ─────────────────────────────────────────────────────────────────

    pub(super) fn pow64(base: f64, exp_: f64) -> f64 {
        if exp_ == 0.0 {
            return 1.0;
        }
        if base == 1.0 {
            return 1.0;
        }
        if exp_.is_nan() || base.is_nan() {
            return f64::NAN;
        }
        if base == 0.0 {
            return if exp_ > 0.0 { 0.0 } else { f64::INFINITY };
        }
        if base < 0.0 {
            let int_exp = exp_ as i64;
            if int_exp as f64 == exp_ {
                let r = pow64(-base, exp_);
                return if int_exp % 2 != 0 { -r } else { r };
            }
            return f64::NAN;
        }
        exp64(exp_ * log64(base))
    }

    // ── fmod64 ────────────────────────────────────────────────────────────────
    // C standard: result has the same sign as x (truncation, not floor).

    pub(super) fn fmod64(x: f64, y: f64) -> f64 {
        if y == 0.0 || x.is_infinite() || x.is_nan() || y.is_nan() {
            return f64::NAN;
        }
        if y.is_infinite() {
            return x;
        }
        // Truncate toward zero (same sign as x).
        let q = x / y;
        let q_trunc = if abs64(q) >= NO_FRAC { q } else { q as i64 as f64 };
        x - q_trunc * y
    }

    // ── Public C functions ────────────────────────────────────────────────────

    #[no_mangle]
    pub unsafe extern "C" fn fabs(x: c_double) -> c_double {
        abs64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn fabsf(x: c_float) -> c_float {
        abs32(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn floor(x: c_double) -> c_double {
        ifloor64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn floorf(x: c_float) -> c_float {
        ifloor64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn ceil(x: c_double) -> c_double {
        if !x.is_finite() || abs64(x) >= NO_FRAC {
            return x;
        }
        let t = x as i64 as f64;
        if t < x { t + 1.0 } else { t }
    }

    #[no_mangle]
    pub unsafe extern "C" fn ceilf(x: c_float) -> c_float {
        ceil(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn round(x: c_double) -> c_double {
        ifloor64(x + 0.5)
    }

    #[no_mangle]
    pub unsafe extern "C" fn roundf(x: c_float) -> c_float {
        round(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn trunc(x: c_double) -> c_double {
        if !x.is_finite() || abs64(x) >= NO_FRAC {
            return x;
        }
        x as i64 as f64
    }

    #[no_mangle]
    pub unsafe extern "C" fn truncf(x: c_float) -> c_float {
        trunc(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn sqrt(x: c_double) -> c_double {
        sqrt64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn sqrtf(x: c_float) -> c_float {
        sqrt64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn hypot(x: c_double, y: c_double) -> c_double {
        let ax = abs64(x);
        let ay = abs64(y);
        let (a, b) = if ax >= ay { (ax, ay) } else { (ay, ax) };
        if a == 0.0 {
            return 0.0;
        }
        let r = b / a;
        a * sqrt64(1.0 + r * r)
    }

    #[no_mangle]
    pub unsafe extern "C" fn hypotf(x: c_float, y: c_float) -> c_float {
        hypot(x as f64, y as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn sin(x: c_double) -> c_double {
        sin64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn sinf(x: c_float) -> c_float {
        sin64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn cos(x: c_double) -> c_double {
        cos64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn cosf(x: c_float) -> c_float {
        cos64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn tan(x: c_double) -> c_double {
        let c = cos64(x);
        if c == 0.0 {
            return if sin64(x) > 0.0 { f64::INFINITY } else { f64::NEG_INFINITY };
        }
        sin64(x) / c
    }

    #[no_mangle]
    pub unsafe extern "C" fn tanf(x: c_float) -> c_float {
        tan(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn atan(x: c_double) -> c_double {
        atan64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn atanf(x: c_float) -> c_float {
        atan64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn atan2(y: c_double, x: c_double) -> c_double {
        atan2_64(y, x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn atan2f(y: c_float, x: c_float) -> c_float {
        atan2_64(y as f64, x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn asin(x: c_double) -> c_double {
        asin64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn asinf(x: c_float) -> c_float {
        asin64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn acos(x: c_double) -> c_double {
        acos64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn acosf(x: c_float) -> c_float {
        acos64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn exp(x: c_double) -> c_double {
        exp64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn expf(x: c_float) -> c_float {
        exp64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn log(x: c_double) -> c_double {
        log64(x)
    }

    #[no_mangle]
    pub unsafe extern "C" fn logf(x: c_float) -> c_float {
        log64(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn log10(x: c_double) -> c_double {
        log64(x) * LOG10_E
    }

    #[no_mangle]
    pub unsafe extern "C" fn log10f(x: c_float) -> c_float {
        log10(x as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn pow(base: c_double, exp_: c_double) -> c_double {
        pow64(base, exp_)
    }

    #[no_mangle]
    pub unsafe extern "C" fn powf(base: c_float, exp_: c_float) -> c_float {
        pow64(base as f64, exp_ as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn fmod(x: c_double, y: c_double) -> c_double {
        fmod64(x, y)
    }

    #[no_mangle]
    pub unsafe extern "C" fn fmodf(x: c_float, y: c_float) -> c_float {
        fmod64(x as f64, y as f64) as f32
    }

    #[no_mangle]
    pub unsafe extern "C" fn sincos(x: c_double, sinp: *mut c_double, cosp: *mut c_double) {
        *sinp = sin64(x);
        *cosp = cos64(x);
    }

    #[no_mangle]
    pub unsafe extern "C" fn sincosf(x: c_float, sinp: *mut c_float, cosp: *mut c_float) {
        *sinp = sin64(x as f64) as f32;
        *cosp = cos64(x as f64) as f32;
    }

    #[no_mangle]
    pub unsafe extern "C" fn frexp(x: c_double, exp: *mut c_int) -> c_double {
        if x == 0.0 {
            *exp = 0;
            return 0.0;
        }
        let bits = core::mem::transmute::<f64, u64>(x);
        let mut e = (((bits >> 52) & 0x7FF) as i32) - 1022;
        let mut m = (bits & 0x000F_FFFF_FFFF_FFFF) | 0x0010_0000_0000_0000;
        if e == -1022 {
            if m == 0x0010_0000_0000_0000 {
                *exp = 0;
                return 0.0;
            }
            while m & 0x0010_0000_0000_0000 == 0 {
                m <<= 1;
                e -= 1;
            }
        }
        *exp = e;
        let res_bits =
            (bits & 0x8000_0000_0000_0000) | (1022 << 52) | (m & 0x000F_FFFF_FFFF_FFFF);
        core::mem::transmute::<u64, f64>(res_bits)
    }

    #[no_mangle]
    pub unsafe extern "C" fn modf(x: c_double, iptr: *mut c_double) -> c_double {
        let i = x as i64;
        *iptr = i as f64;
        x - *iptr
    }
}

// ─── Re-export as public Rust items ──────────────────────────────────────────
// `#[no_mangle] pub extern "C"` functions defined inside `mod imp` are already
// emitted as global C symbols, but callers that use this crate as a Rust
// dependency (e.g. `use libc::sin`) also need them in the public namespace.
#[cfg(all(
    not(any(test, feature = "host-testing")),
    any(
        target_os = "eclipse",
        eclipse_target,
        not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target))))
    )
))]
pub use imp::{
    fabs, fabsf, floor, floorf, ceil, ceilf, round, roundf, trunc, truncf,
    sqrt, sqrtf, hypot, hypotf,
    sin, sinf, cos, cosf, tan, tanf,
    atan, atanf, atan2, atan2f,
    asin, asinf, acos, acosf,
    exp, expf, log, logf, log10, log10f,
    pow, powf, fmod, fmodf,
    sincos, sincosf, frexp, modf,
};
