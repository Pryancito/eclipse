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

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn tcgetattr(fd: c_int, termios_p: *mut termios) -> c_int {
    crate::header::sys_ioctl::ioctl(fd, 0x5401, termios_p as *mut c_void)
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn tcsetattr(fd: c_int, _optional_actions: c_int, termios_p: *const termios) -> c_int {
    // We ignore _optional_actions for now and just set it.
    crate::header::sys_ioctl::ioctl(fd, 0x5402, termios_p as *mut c_void)
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn cfsetispeed(termios_p: *mut termios, speed: speed_t) -> c_int {
    if !termios_p.is_null() {
        (*termios_p).c_ispeed = speed;
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn cfsetospeed(termios_p: *mut termios, speed: speed_t) -> c_int {
    if !termios_p.is_null() {
        (*termios_p).c_ospeed = speed;
    }
    0
}
