//! stdio.h - Standard I/O
use crate::types::*;
use eclipse_syscall::call::write;

#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    let ch = [c as u8];
    match write(1, &ch) {
        Ok(_) => c,
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
    use crate::c_str::strlen;
    let len = strlen(s);
    let slice = core::slice::from_raw_parts(s as *const u8, len);
    match write(1, slice) {
        Ok(_) => {
            putchar(b'\n' as c_int);
            0
        }
        Err(_) => -1,
    }
}
