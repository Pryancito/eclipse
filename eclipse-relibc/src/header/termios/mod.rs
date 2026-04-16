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

// c_iflag
pub const IGNBRK:  tcflag_t = 0o000001;
pub const BRKINT:  tcflag_t = 0o000002;
pub const IGNPAR:  tcflag_t = 0o000004;
pub const PARMRK:  tcflag_t = 0o000010;
pub const INPCK:   tcflag_t = 0o000020;
pub const ISTRIP:  tcflag_t = 0o000040;
pub const INLCR:   tcflag_t = 0o000100;
pub const IGNCR:   tcflag_t = 0o000200;
pub const ICRNL:   tcflag_t = 0o000400;
pub const IUCLC:   tcflag_t = 0o001000;
pub const IXON:    tcflag_t = 0o002000;
pub const IXANY:   tcflag_t = 0o004000;
pub const IXOFF:   tcflag_t = 0o010000;
pub const IMAXBEL: tcflag_t = 0o020000;
pub const IUTF8:   tcflag_t = 0o040000;

// c_oflag
pub const OPOST:  tcflag_t = 0o000001;
pub const OLCUC:  tcflag_t = 0o000002;
pub const ONLCR:  tcflag_t = 0o000004;
pub const OCRNL:  tcflag_t = 0o000010;
pub const ONOCR:  tcflag_t = 0o000020;
pub const ONLRET: tcflag_t = 0o000040;
pub const OFILL:  tcflag_t = 0o000100;
pub const OFDEL:  tcflag_t = 0o000200;

// c_lflag
pub const ISIG:    tcflag_t = 0o000001;
pub const ICANON:  tcflag_t = 0o000002;
pub const ECHO:    tcflag_t = 0o000010;
pub const ECHOE:   tcflag_t = 0o000020;
pub const ECHOK:   tcflag_t = 0o000040;
pub const ECHONL:  tcflag_t = 0o000100;
pub const NOFLSH:  tcflag_t = 0o000200;
pub const TOSTOP:  tcflag_t = 0o000400;
pub const IEXTEN:  tcflag_t = 0o100000;

// Indices for c_cc
pub const VINTR:    usize = 0;
pub const VQUIT:    usize = 1;
pub const VERASE:   usize = 2;
pub const VKILL:    usize = 3;
pub const VEOF:     usize = 4;
pub const VTIME:    usize = 5;
pub const VMIN:     usize = 6;
pub const VSWTC:    usize = 7;
pub const VSTART:   usize = 8;
pub const VSTOP:    usize = 9;
pub const VSUSP:    usize = 10;
pub const VEOL:     usize = 11;
pub const VREPRINT: usize = 12;
pub const VDISCARD: usize = 13;
pub const VWERASE:  usize = 14;
pub const VLNEXT:   usize = 15;
pub const VEOL2:    usize = 16;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn tcgetattr(fd: c_int, termios_p: *mut termios) -> c_int {
    crate::header::sys_ioctl::ioctl(fd, 0x5401, termios_p as *mut c_void)
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn tcsetattr(fd: c_int, _optional_actions: c_int, termios_p: *const termios) -> c_int {
    // We ignore _optional_actions for now and just set it.
    crate::header::sys_ioctl::ioctl(fd, 0x5402, termios_p as *mut c_void)
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn cfsetispeed(termios_p: *mut termios, speed: speed_t) -> c_int {
    if !termios_p.is_null() {
        (*termios_p).c_ispeed = speed;
    }
    0
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn cfsetospeed(termios_p: *mut termios, speed: speed_t) -> c_int {
    if !termios_p.is_null() {
        (*termios_p).c_ospeed = speed;
    }
    0
}

pub const TCSANOW:   c_int = 0;
pub const TCSADRAIN: c_int = 1;
pub const TCSAFLUSH: c_int = 2;

pub const B0:     speed_t = 0;
pub const B50:    speed_t = 1;
pub const B75:    speed_t = 2;
pub const B110:   speed_t = 3;
pub const B134:   speed_t = 4;
pub const B150:   speed_t = 5;
pub const B200:   speed_t = 6;
pub const B300:   speed_t = 7;
pub const B600:   speed_t = 8;
pub const B1200:  speed_t = 9;
pub const B1800:  speed_t = 10;
pub const B2400:  speed_t = 11;
pub const B4800:  speed_t = 12;
pub const B9600:  speed_t = 13;
pub const B19200: speed_t = 14;
pub const B38400: speed_t = 15;

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn tcdrain(fd: c_int) -> c_int {
    crate::header::sys_ioctl::ioctl(fd, 0x5409, core::ptr::null_mut())
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn tcflush(fd: c_int, queue_selector: c_int) -> c_int {
    crate::header::sys_ioctl::ioctl(fd, 0x540B, queue_selector as *mut c_void)
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn tcflow(fd: c_int, action: c_int) -> c_int {
    crate::header::sys_ioctl::ioctl(fd, 0x540C, action as *mut c_void)
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn cfgetispeed(termios_p: *const termios) -> speed_t {
    if termios_p.is_null() { return B9600; }
    (*termios_p).c_ispeed
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn cfgetospeed(termios_p: *const termios) -> speed_t {
    if termios_p.is_null() { return B9600; }
    (*termios_p).c_ospeed
}
