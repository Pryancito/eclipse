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
