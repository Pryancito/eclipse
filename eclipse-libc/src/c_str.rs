//! C string utilities
use crate::types::*;

pub unsafe fn strlen(s: *const c_char) -> size_t {
    let mut len = 0;
    let mut p = s;
    while *p != 0 {
        len += 1;
        p = p.add(1);
    }
    len
}
