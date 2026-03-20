//! C type definitions
#![allow(non_camel_case_types)]

pub type c_char = i8;
pub type c_int = i32;
pub type c_uint = u32;
pub type c_uchar = u8;
pub type c_long = i64;
pub type c_ulong = u64;
pub type c_longlong = i64;
pub type c_ulonglong = u64;
pub type c_float = f32;
pub type c_double = f64;
pub type c_void = core::ffi::c_void;
pub type size_t = usize;
pub type ssize_t = isize;
pub type off_t = i64;
pub type pid_t = i32;
pub type mode_t = u32;
pub type c_short = i16;
pub type c_ushort = u16;
pub type clockid_t = c_int;
pub type nfds_t = usize;
pub type uid_t = u32;
pub type gid_t = u32;
pub type key_t = c_int;
pub type socklen_t = c_uint;
pub const NULL: *mut c_void = core::ptr::null_mut();
pub const SEEK_SET: c_int = 0;
pub const SEEK_CUR: c_int = 1;
pub const SEEK_END: c_int = 2;

pub type time_t = c_long;
pub type suseconds_t = c_long;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct timeval {
    pub tv_sec: time_t,
    pub tv_usec: suseconds_t,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

pub const FD_SETSIZE: usize = 1024;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct fd_set {
    pub fds_bits: [c_ulong; FD_SETSIZE / (8 * core::mem::size_of::<c_ulong>())],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct itimerval {
    pub it_interval: timeval,
    pub it_value: timeval,
}

pub const ITIMER_REAL: c_int = 0;
pub const ITIMER_VIRTUAL: c_int = 1;
pub const ITIMER_PROF: c_int = 2;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigset_t {
    pub sig: [c_ulong; 1],
}


#[repr(C)]
#[derive(Copy, Clone)]
pub struct stat {
    pub st_dev: dev_t,
    pub st_ino: ino_t,
    pub st_mode: mode_t,
    pub st_nlink: c_uint,
    pub st_uid: uid_t,
    pub st_gid: gid_t,
    pub st_rdev: dev_t,
    pub st_size: off_t,
    pub st_atime: time_t,
    pub st_mtime: time_t,
    pub st_ctime: time_t,
    pub st_blksize: c_long,
    pub st_blocks: c_long,
}

pub type dev_t = c_ulong;
pub type ino_t = c_ulong;
pub type useconds_t = c_uint;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct in6_addr {
    pub s6_addr: [u8; 16],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_in6 {
    pub sin6_family: c_ushort,
    pub sin6_port: c_ushort,
    pub sin6_flowinfo: c_uint,
    pub sin6_addr: in6_addr,
    pub sin6_scope_id: c_uint,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct passwd {
    pub pw_name: *mut c_char,
    pub pw_passwd: *mut c_char,
    pub pw_uid: uid_t,
    pub pw_gid: gid_t,
    pub pw_gecos: *mut c_char,
    pub pw_dir: *mut c_char,
    pub pw_shell: *mut c_char,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct group {
    pub gr_name: *mut c_char,
    pub gr_passwd: *mut c_char,
    pub gr_gid: gid_t,
    pub gr_mem: *mut *mut c_char,
}
