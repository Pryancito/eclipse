//! termios.h - Terminal control
use crate::types::*;

pub type tcflag_t = c_uint;
pub type cc_t = c_uchar;
pub type speed_t = c_uint;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct termios {
    pub c_iflag: tcflag_t,
    pub c_oflag: tcflag_t,
    pub c_cflag: tcflag_t,
    pub c_lflag: tcflag_t,
    pub c_line: cc_t,
    pub c_cc: [cc_t; 32],
    pub c_ispeed: speed_t,
    pub c_ospeed: speed_t,
}

#[no_mangle]
pub unsafe extern "C" fn tcgetattr(_fd: c_int, _termios_p: *mut termios) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn tcsetattr(_fd: c_int, _optional_actions: c_int, _termios_p: *const termios) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn cfsetispeed(_termios_p: *mut termios, _speed: speed_t) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn cfsetospeed(_termios_p: *mut termios, _speed: speed_t) -> c_int {
    0
}
