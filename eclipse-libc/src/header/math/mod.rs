//! math.h - Math functions
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn hypot(_x: c_double, _y: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn sqrt(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn sin(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn cos(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn tan(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn pow(_x: c_double, _y: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn fabs(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn floor(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn ceil(_x: c_double) -> c_double {
    0.0
}

#[no_mangle]
pub unsafe extern "C" fn atan2(_y: c_double, _x: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn exp(_x: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn log(_x: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn log10(_x: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn fmod(_x: c_double, _y: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn acos(_x: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn asin(_x: c_double) -> c_double { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn sincos(_x: c_double, sinp: *mut c_double, cosp: *mut c_double) {
    *sinp = 0.0;
    *cosp = 0.0;
}

#[no_mangle]
pub unsafe extern "C" fn acosf(_x: c_float) -> c_float { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn asinf(_x: c_float) -> c_float { 0.0 }

#[no_mangle]
pub unsafe extern "C" fn sincosf(_x: c_float, sinp: *mut c_float, cosp: *mut c_float) {
    *sinp = 0.0;
    *cosp = 0.0;
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
    
    // Normalize if subnormal
    if e == -1022 {
        if m == 0x0010_0000_0000_0000 { // Actually zero handled above
             *exp = 0;
             return 0.0;
        }
        while m & 0x0010_0000_0000_0000 == 0 {
            m <<= 1;
            e -= 1;
        }
    }

    *exp = e;
    let res_bits = (bits & 0x8000_0000_0000_0000) | (1022 << 52) | (m & 0x000F_FFFF_FFFF_FFFF);
    core::mem::transmute::<u64, f64>(res_bits)
}

#[no_mangle]
pub unsafe extern "C" fn modf(x: c_double, iptr: *mut c_double) -> c_double {
    let i = x as i64;
    *iptr = i as f64;
    x - *iptr
}
