//! ctype.h - Character classification
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn isspace(c: c_int) -> c_int {
    let c = c as u8 as char;
    if c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\x0b' || c == '\x0c' {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn isdigit(c: c_int) -> c_int {
    if c >= b'0' as c_int && c <= b'9' as c_int {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn isprint(c: c_int) -> c_int {
    if c >= 32 && c <= 126 {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn isupper(c: c_int) -> c_int {
    if c >= b'A' as c_int && c <= b'Z' as c_int {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn islower(c: c_int) -> c_int {
    if c >= b'a' as c_int && c <= b'z' as c_int {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn isalpha(c: c_int) -> c_int {
    if isupper(c) != 0 || islower(c) != 0 {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn isalnum(c: c_int) -> c_int {
    if isalpha(c) != 0 || isdigit(c) != 0 {
        1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn toupper(c: c_int) -> c_int {
    if islower(c) != 0 {
        c - 32
    } else {
        c
    }
}

#[no_mangle]
pub unsafe extern "C" fn tolower(c: c_int) -> c_int {
    if isupper(c) != 0 {
        c + 32
    } else {
        c
    }
}

// glibc character trait table (minimal version)
// bits 0-7: iscntrl, isblank, isspace, isupper, islower, isdigit, isxdigit, ispunct
// bits 8-15: isalpha, isalnum, isgraph, isprint, ...
static CTYPE_B_TABLE: [u16; 384] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // -128 to -113
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // -16 to -1
    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, // 0 to 7 (cntrl)
    0x0001, 0x0003, 0x0003, 0x0003, 0x0003, 0x0003, 0x0001, 0x0001, // 8 to 15 (cntrl, 9-13 space)
    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
    0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001, 0x0001,
    0x0003, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080, // 32 to 39 (32 space)
    0x0080, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080,
    0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, // 48 to 55 (digit)
    0x0020, 0x0020, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080,
    0x0080, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, // 64 to 71 (A-G upper)
    0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108,
    0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108, 0x0108,
    0x0108, 0x0108, 0x0108, 0x0080, 0x0080, 0x0080, 0x0080, 0x0080,
    0x0080, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, // 96 to 103 (a-g lower)
    0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110,
    0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110, 0x0110,
    0x0110, 0x0110, 0x0110, 0x0080, 0x0080, 0x0080, 0x0080, 0x0001,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // 128 to 255
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

static mut CTYPE_B_PTR: *const u16 = unsafe { &CTYPE_B_TABLE[128] as *const u16 };

#[no_mangle]
pub unsafe extern "C" fn __ctype_b_loc() -> *mut *const u16 {
    &raw mut CTYPE_B_PTR as *mut *const u16
}

static mut CTYPE_TOLOWER_TABLE: [i32; 384] = [0; 384];
static mut CTYPE_TOLOWER_PTR: *const i32 = core::ptr::null();

#[no_mangle]
pub unsafe extern "C" fn __ctype_tolower_loc() -> *mut *const i32 {
    if CTYPE_TOLOWER_PTR.is_null() {
        for i in 0..384 {
            let c = (i as i32) - 128;
            if c >= b'A' as i32 && c <= b'Z' as i32 {
                CTYPE_TOLOWER_TABLE[i] = c + 32;
            } else {
                CTYPE_TOLOWER_TABLE[i] = c;
            }
        }
        CTYPE_TOLOWER_PTR = &CTYPE_TOLOWER_TABLE[128] as *const i32;
    }
    &raw mut CTYPE_TOLOWER_PTR as *mut *const i32
}
