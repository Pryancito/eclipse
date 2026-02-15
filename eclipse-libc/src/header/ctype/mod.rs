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
