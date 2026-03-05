//! dirent.h - Directory operations
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct dirent {
    pub d_ino: ino_t,
    pub d_off: off_t,
    pub d_reclen: c_ushort,
    pub d_type: c_uchar,
    pub d_name: [c_char; 256],
}

pub struct DIR {
    pub fd: c_int,
}

#[no_mangle]
pub unsafe extern "C" fn opendir(_name: *const c_char) -> *mut DIR {
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn readdir(_dirp: *mut DIR) -> *mut dirent {
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn closedir(_dirp: *mut DIR) -> c_int {
    -1
}
