//! C type definitions
#![allow(non_camel_case_types)]

// En modo rustc-dep-of-std (no_core), los tipos básicos y #[derive] no están
// en scope automáticamente — deben importarse explícitamente desde core.
#[cfg(feature = "rustc-dep-of-std")]
use core::{
    clone::Clone,
    cmp::{PartialEq, Eq},
    default::Default,
    marker::Copy,
    option::Option,
    prelude::v1::derive,
    sync::atomic::AtomicI32,
};
#[cfg(not(feature = "rustc-dep-of-std"))]
use core::sync::atomic::AtomicI32;

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

#[repr(C)]
#[derive(Copy, Clone)]
pub struct itimerspec {
    pub it_interval: timespec,
    pub it_value: timespec,
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

// ── Tipos necesarios para las declaraciones extern "C" del modo sysroot ───────
// Disponibles en AMBOS modos (normal y rustc-dep-of-std) para que los módulos
// de header y el bloque extern "C" de lib.rs compartan las mismas definiciones.

/// Tipo de puntero a función manejadora de señal.
pub type sighandler_t = *mut c_void;

/// Estructura sigaction — compatible con Linux x86-64.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sigaction_t {
    pub sa_sigaction: usize,  // sa_handler / sa_sigaction (union)
    pub sa_mask:      sigset_t,
    pub sa_flags:     c_int,
    pub sa_restorer:  Option<unsafe extern "C" fn()>,
}

/// pthread_t — identificador de hilo.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct pthread_t {
    pub thread_id: u64,
    /// Slot en heap: puntero al valor devuelto por `start_routine` (para `pthread_join`).
    pub join_cell: *mut *mut c_void,
}
impl Default for pthread_t {
    fn default() -> Self {
        pthread_t {
            thread_id: 0,
            join_cell: core::ptr::null_mut(),
        }
    }
}

/// pthread_mutex_t — mutex de bajo nivel usando AtomicI32.
#[repr(C)]
pub struct pthread_mutex_t {
    pub lock: core::sync::atomic::AtomicI32,
}
impl Default for pthread_mutex_t {
    fn default() -> Self { pthread_mutex_t { lock: core::sync::atomic::AtomicI32::new(0) } }
}

/// pthread_cond_t — variable de condición usando AtomicI32 (futex).
#[repr(C)]
pub struct pthread_cond_t {
    pub value: core::sync::atomic::AtomicI32,
}
impl Default for pthread_cond_t {
    fn default() -> Self { pthread_cond_t { value: core::sync::atomic::AtomicI32::new(0) } }
}

pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t =
    pthread_mutex_t { lock: core::sync::atomic::AtomicI32::new(0) };
pub const PTHREAD_COND_INITIALIZER: pthread_cond_t =
    pthread_cond_t { value: core::sync::atomic::AtomicI32::new(0) };

/// pthread_attr_t — opaque (tamaño suficiente para stack_size + detach_state).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct pthread_attr_t {
    _data: [u8; 56],
}
impl Default for pthread_attr_t {
    fn default() -> Self { pthread_attr_t { _data: [0; 56] } }
}

/// pthread_mutexattr_t — opaque.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct pthread_mutexattr_t { pub _kind: c_int }

/// pthread_condattr_t — opaque.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct pthread_condattr_t { pub _clock: c_int }

/// pthread_key_t — clave TLS.
pub type pthread_key_t = usize;

/// FILE — opaque (para el usuario), pero con estructura interna para stdio.
#[repr(C)]
pub struct FILE {
    pub fd: c_int,
    pub flags: c_int,
    pub buffer: *mut u8,
    pub buf_pos: usize,
    pub buf_size: usize,
    pub buf_capacity: usize,
    pub lock: core::sync::atomic::AtomicI32,
}

/// Constantes pthread
pub const PTHREAD_STACK_MIN: size_t = 16384;
