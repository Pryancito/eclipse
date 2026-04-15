// ── Modo sysroot (rustc-dep-of-std) ─────────────────────────────────────────
// Cuando Rust's std compila con features = ["rustc-dep-of-std"], necesitamos:
//   • no_core (el sysroot proporciona core vía rustc-std-workspace-core)
//   • sin alloc ni eclipse-syscall (no están disponibles aún en esa etapa)
//   • sólo tipos, constantes y declaraciones extern "C"
//
// Modo normal (builds de aplicaciones) ──────────────────────────────────────
//   • no_std  (alloc y eclipse-syscall sí están disponibles)
//   • implementaciones completas de funciones POSIX
// ────────────────────────────────────────────────────────────────────────────

// Modo no_core cuando somos la libc del sysroot
#![cfg_attr(feature = "rustc-dep-of-std", feature(no_core))]
#![cfg_attr(feature = "rustc-dep-of-std", no_core)]
// Modo no_std en builds normales
#![cfg_attr(not(feature = "rustc-dep-of-std"), no_std)]

#![feature(c_variadic)]
#![feature(linkage)]
#![cfg_attr(not(feature = "rustc-dep-of-std"), feature(alloc_error_handler))]
#![feature(thread_local)]
#![allow(non_camel_case_types, non_upper_case_globals, unused_macros)]

// ── Fuente de 'core' según modo ──────────────────────────────────────────────
#[cfg(feature = "rustc-dep-of-std")]
extern crate rustc_std_workspace_core as core;

// ── alloc y eclipse-syscall solo en modo normal ───────────────────────────────
#[cfg(not(feature = "rustc-dep-of-std"))]
extern crate alloc;

#[cfg(not(feature = "rustc-dep-of-std"))]
extern crate eclipse_syscall;

// ── Macros de depuración (solo disponibles en modo normal) ───────────────────
#[cfg(not(feature = "rustc-dep-of-std"))]
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::header::stdio::_print(format_args!($($arg)*)));
}
#[cfg(not(feature = "rustc-dep-of-std"))]
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($fmt:expr) => ($crate::print!(core::concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => ($crate::print!(core::concat!($fmt, "\n"), $($arg)*));
}

// ── asm con stubs de POSIX (solo en modo normal para el target Eclipse) ───────
#[cfg(all(
    not(feature = "rustc-dep-of-std"),
    not(any(test, feature = "host-testing")),
    eclipse_target
))]
#[cfg(feature = "crt0")]
core::arch::global_asm!(include_str!("posix_stubs.s"));

#[cfg(not(feature = "crt0"))]
core::arch::global_asm!(include_str!("posix_stubs_nostart.s"));

// ── Prelude mínima para no_core (rustc-dep-of-std) ───────────────────────────
// En modo no_core nada está en scope automáticamente. Importamos lo estrictamente
// necesario para que types.rs y los extern "C" compilen correctamente.
#[cfg(feature = "rustc-dep-of-std")]
mod sysroot_prelude {
    pub(crate) use core::clone::Clone;
    pub(crate) use core::default::Default;
    pub(crate) use core::marker::{Copy, Send, Sync};
    pub(crate) use core::option::Option;
    pub(crate) use core::prelude::v1::derive;
    pub(crate) use core::sync::atomic::AtomicI32;
    pub(crate) use core::{ptr, mem};
}

#[cfg(feature = "rustc-dep-of-std")]
#[allow(unused_imports)]
use sysroot_prelude::*;

// ── Módulos siempre presentes (sólo types, constantes y declaraciones) ────────
pub mod types;

// ── Módulo con todos los símbolos extra que Rust std necesita (solo sysroot) ──
#[cfg(feature = "rustc-dep-of-std")]
pub mod sysroot_symbols;
#[cfg(feature = "rustc-dep-of-std")]
pub use sysroot_symbols::*;

// ── Módulos con implementaciones (solo en modo normal) ────────────────────────
#[cfg(not(feature = "rustc-dep-of-std"))]
pub mod internal_alloc;

#[cfg(not(feature = "rustc-dep-of-std"))]
pub mod c_str;

#[cfg(not(feature = "rustc-dep-of-std"))]
pub mod stack_chk;

#[cfg(not(feature = "rustc-dep-of-std"))]
pub mod platform;

#[cfg(not(feature = "rustc-dep-of-std"))]
pub mod header {
    pub mod stdio;
    pub mod stdlib;
    pub mod string;
    pub mod pthread;
    pub mod unistd;
    pub mod time;
    pub mod errno;
    pub mod signal;
    pub mod poll;
    pub mod dlfcn;
    pub mod math;
    pub mod locale;
    pub mod sys_shm;
    pub mod sys_socket;
    pub mod sys_uio;
    pub mod sys_ioctl;
    pub mod net_inet;
    pub mod netdb;
    pub mod sys_utsname;
    pub mod sys_wait;
    pub mod sys_resource;
    pub mod ctype;
    pub mod fcntl;
    pub mod sys_stat;
    pub mod sys_select;
    pub mod termios;
    pub mod sys_mman;
    pub mod dirent;
    pub mod pwd;
    pub mod grp;
    pub mod ifaddrs;
    pub mod sys_eclipse;
    pub mod sys_timerfd;
    pub mod sys_eventfd;
}

// ── Re-exports en modo normal ─────────────────────────────────────────────────
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use types::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::stdio::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::stdlib::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use crate::internal_alloc::{malloc, free, calloc, realloc};
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::string::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::pthread::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::unistd::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::time::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::errno::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::signal::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::poll::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::dlfcn::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::math::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::locale::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_shm::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_socket::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_uio::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_ioctl::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::net_inet::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::netdb::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_utsname::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_wait::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_resource::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_stat::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_select::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::termios::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::ctype::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::fcntl::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_mman::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::dirent::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::pwd::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::grp::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::ifaddrs::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_timerfd::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_eventfd::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_eclipse::*;

// ── C runtime bootstrap for programs compiled against eclipse-relibc ─────────
// `__libc_start_main` is called by our assembly `_start`.  It initialises the
// environ table from the kernel-provided envp and then calls main().
#[cfg(all(
    not(feature = "rustc-dep-of-std"),
    not(any(test, feature = "host-testing")),
    eclipse_target
))]
#[no_mangle]
pub unsafe extern "C" fn __libc_start_main(
    argc: isize,
    argv: *const *const types::c_char,
    envp: *const *const types::c_char,
) -> types::c_int {
    // Initialise the heap before anything else (including environ_init which
    // calls malloc).
    internal_alloc::init_heap_if_needed();

    // Initialise the environ table from the kernel-supplied envp.
    header::stdlib::environ_init(envp);

    // Declare the application's main() symbol.
    extern "C" {
        fn main(argc: isize, argv: *const *const types::c_char, envp: *const *const types::c_char) -> types::c_int;
    }

    let ret = main(argc, argv, envp);
    // _exit skips atexit handlers; use exit() for proper cleanup.
    header::stdlib::exit(ret);
}

// ── Constantes de archivo (modo normal vía eclipse-syscall) ──────────────────
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use types::*;

// ── Minimal wide/multibyte stubs (ASCII) ─────────────────────────────────────
#[cfg(all(
    not(feature = "rustc-dep-of-std"),
    not(any(test, feature = "host-testing")),
    eclipse_target,
))]
mod wide_stubs {
    use crate::types::*;

    #[repr(C)]
    pub struct mbstate_t {
        _opaque: c_uint,
    }

    #[no_mangle]
    pub unsafe extern "C" fn mbrtowc(pwc: *mut u32, s: *const c_char, n: size_t, _ps: *mut mbstate_t) -> size_t {
        if s.is_null() {
            return 0;
        }
        if n == 0 {
            return usize::MAX;
        }
        let b = *(s as *const u8);
        if !pwc.is_null() {
            *pwc = b as u32;
        }
        if b == 0 { 0 } else { 1 }
    }

    #[no_mangle]
    pub unsafe extern "C" fn towlower(wc: c_uint) -> c_uint {
        if wc >= b'A' as c_uint && wc <= b'Z' as c_uint {
            wc + 32
        } else {
            wc
        }
    }
}

// ── Misc POSIX stubs needed by bash bring-up ─────────────────────────────────
#[cfg(all(
    not(feature = "rustc-dep-of-std"),
    not(any(test, feature = "host-testing")),
    eclipse_target,
))]
mod posix_stubs_for_bash {
    use crate::types::*;

    #[no_mangle]
    pub unsafe extern "C" fn mktemp(_tmpl: *mut c_char) -> *mut c_char {
        // Not secure; placeholder for bring-up. Indicate failure.
        core::ptr::null_mut()
    }

    #[no_mangle]
    pub unsafe extern "C" fn ttyname(_fd: c_int) -> *mut c_char {
        core::ptr::null_mut()
    }

    #[no_mangle]
    pub unsafe extern "C" fn mknod(_path: *const c_char, _mode: mode_t, _dev: dev_t) -> c_int {
        *crate::header::errno::__errno_location() = 38; // ENOSYS
        -1
    }

    // group database stubs
    #[repr(C)]
    pub struct group {
        pub gr_name: *mut c_char,
        pub gr_passwd: *mut c_char,
        pub gr_gid: gid_t,
        pub gr_mem: *mut *mut c_char,
    }

    #[no_mangle]
    pub unsafe extern "C" fn setgrent() {}

    #[no_mangle]
    pub unsafe extern "C" fn endgrent() {}

    #[no_mangle]
    pub unsafe extern "C" fn getgrent() -> *mut group {
        core::ptr::null_mut()
    }
}

#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_RDONLY:   c_int = eclipse_syscall::flag::O_RDONLY   as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_WRONLY:   c_int = eclipse_syscall::flag::O_WRONLY   as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_RDWR:     c_int = eclipse_syscall::flag::O_RDWR     as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_CREAT:    c_int = eclipse_syscall::flag::O_CREAT    as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_EXCL:     c_int = eclipse_syscall::flag::O_EXCL     as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_NOCTTY:   c_int = eclipse_syscall::flag::O_NOCTTY   as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_TRUNC:    c_int = eclipse_syscall::flag::O_TRUNC    as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_APPEND:   c_int = eclipse_syscall::flag::O_APPEND   as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_NONBLOCK: c_int = eclipse_syscall::flag::O_NONBLOCK as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_CLOEXEC:  c_int = eclipse_syscall::flag::O_CLOEXEC  as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_NOFOLLOW: c_int = eclipse_syscall::flag::O_NOFOLLOW as c_int;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub const O_DIRECTORY:c_int = eclipse_syscall::flag::O_DIRECTORY as c_int;

// ── Constantes de archivo (modo sysroot — valores hardcoded POSIX/Linux x86-64)
#[cfg(feature = "rustc-dep-of-std")]
pub use types::*;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_RDONLY:    c_int = 0x0000;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_WRONLY:    c_int = 0x0001;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_RDWR:      c_int = 0x0002;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_CREAT:     c_int = 0x0040;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_EXCL:      c_int = 0x0080;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_NOCTTY:    c_int = 0x0100;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_TRUNC:     c_int = 0x0200;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_APPEND:    c_int = 0x0400;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_NONBLOCK:  c_int = 0x0800;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_CLOEXEC:   c_int = 0x80000;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_NOFOLLOW:  c_int = 0x20000;
#[cfg(feature = "rustc-dep-of-std")]
pub const O_DIRECTORY: c_int = 0x10000;

// ── Constantes de syscall (siempre hardcoded para x86-64) ────────────────────
pub const SYS_getrandom: c_int = 318; // x86-64 Linux
pub const SYS_futex:     c_long = 202;
pub const FUTEX_WAIT:         c_int = 0;
pub const FUTEX_WAKE:         c_int = 1;
pub const FUTEX_PRIVATE_FLAG: c_int = 128;
pub const INT_MAX:            c_int = i32::MAX;

// ── Declaraciones extern "C" en modo sysroot ─────────────────────────────────
// Rust's std llama a estas funciones en tiempo de enlace; las implementaciones
// las provee eclipse-relibc cuando se enlaza el binario final.
#[cfg(feature = "rustc-dep-of-std")]
extern "C" {
    pub fn malloc(size: size_t) -> *mut c_void;
    pub fn free(ptr: *mut c_void);
    pub fn calloc(nmemb: size_t, size: size_t) -> *mut c_void;
    pub fn realloc(ptr: *mut c_void, new_size: size_t) -> *mut c_void;

    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> ssize_t;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> ssize_t;
    pub fn open(path: *const c_char, flags: c_int, _args: ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn lseek(fd: c_int, offset: off_t, whence: c_int) -> off_t;
    pub fn fstat(fd: c_int, buf: *mut stat) -> c_int;
    pub fn stat(path: *const c_char, buf: *mut stat) -> c_int;

    pub fn mmap(
        addr: *mut c_void, len: size_t, prot: c_int,
        flags: c_int, fd: c_int, offset: off_t,
    ) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;
    pub fn mprotect(addr: *mut c_void, len: size_t, prot: c_int) -> c_int;

    pub fn getpid() -> pid_t;
    pub fn getppid() -> pid_t;
    pub fn exit(status: c_int) -> !;
    pub fn _exit(status: c_int) -> !;
    pub fn abort() -> !;

    pub fn pthread_mutex_init(mutex: *mut pthread_mutex_t, attr: *const pthread_mutexattr_t) -> c_int;
    pub fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int;
    pub fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int;
    pub fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> c_int;
    pub fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int;

    pub fn pthread_condattr_init(attr: *mut pthread_condattr_t) -> c_int;
    pub fn pthread_condattr_setclock(attr: *mut pthread_condattr_t, clock: clockid_t) -> c_int;
    pub fn pthread_condattr_destroy(attr: *mut pthread_condattr_t) -> c_int;
    pub fn pthread_cond_init(cond: *mut pthread_cond_t, attr: *const pthread_condattr_t) -> c_int;
    pub fn pthread_cond_wait(cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t) -> c_int;
    pub fn pthread_cond_timedwait(
        cond: *mut pthread_cond_t, mutex: *mut pthread_mutex_t,
        abstime: *const timespec,
    ) -> c_int;
    pub fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int;
    pub fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int;
    pub fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t, attr: *const pthread_attr_t,
        f: extern "C" fn(*mut c_void) -> *mut c_void,
        value: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;
    pub fn pthread_detach(thread: pthread_t) -> c_int;
    pub fn pthread_self() -> pthread_t;
    pub fn pthread_attr_init(attr: *mut pthread_attr_t) -> c_int;
    pub fn pthread_attr_setstacksize(attr: *mut pthread_attr_t, size: size_t) -> c_int;
    pub fn pthread_attr_destroy(attr: *mut pthread_attr_t) -> c_int;

    pub fn pthread_key_create(key: *mut pthread_key_t, dtor: Option<unsafe extern "C" fn(*mut c_void)>) -> c_int;
    pub fn pthread_key_delete(key: pthread_key_t) -> c_int;
    pub fn pthread_getspecific(key: pthread_key_t) -> *mut c_void;
    pub fn pthread_setspecific(key: pthread_key_t, value: *const c_void) -> c_int;

    pub fn clock_gettime(clk_id: clockid_t, tp: *mut timespec) -> c_int;
    pub fn nanosleep(req: *const timespec, rem: *mut timespec) -> c_int;

    pub fn sigaction(signum: c_int, act: *const sigaction_t, oldact: *mut sigaction_t) -> c_int;

    pub fn getenv(name: *const c_char) -> *mut c_char;

    pub fn strlen(s: *const c_char) -> size_t;
    pub fn memcpy(dest: *mut c_void, src: *const c_void, n: size_t) -> *mut c_void;
    pub fn memmove(dest: *mut c_void, src: *const c_void, n: size_t) -> *mut c_void;
    pub fn memset(s: *mut c_void, c: c_int, n: size_t) -> *mut c_void;
    pub fn memcmp(s1: *const c_void, s2: *const c_void, n: size_t) -> c_int;

    pub fn __errno_location() -> *mut c_int;
    pub fn strerror(errnum: c_int) -> *mut c_char;

    pub fn getcwd(buf: *mut c_char, size: size_t) -> *mut c_char;
    pub fn chdir(path: *const c_char) -> c_int;

    pub fn isatty(fd: c_int) -> c_int;

    pub fn syscall(num: c_long, _args: ...) -> c_long;
}

// ── MMAP_FAILED (constante que std usa directamente) ─────────────────────────
pub const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

// ── Constantes de mmap/mprotect (sysroot y modo normal) ──────────────────────
pub const PROT_NONE:    c_int = 0;
pub const PROT_READ:    c_int = 1;
pub const PROT_WRITE:   c_int = 2;
pub const PROT_EXEC:    c_int = 4;
pub const MAP_SHARED:   c_int = 0x01;
pub const MAP_PRIVATE:  c_int = 0x02;
pub const MAP_FIXED:    c_int = 0x10;
pub const MAP_ANON:     c_int = 0x20;
pub const MAP_ANONYMOUS:c_int = 0x20;

// ── Constantes de error (sysroot y modo normal) ───────────────────────────────
pub const EPERM:    c_int = 1;
pub const ENOENT:   c_int = 2;
pub const EINTR:    c_int = 4;
pub const EBADF:    c_int = 9;
pub const EAGAIN:   c_int = 11;
pub const EWOULDBLOCK: c_int = 11;
pub const ENOMEM:   c_int = 12;
pub const EACCES:   c_int = 13;
pub const EEXIST:   c_int = 17;
pub const ENOTDIR:  c_int = 20;
pub const EINVAL:   c_int = 22;
pub const EPIPE:    c_int = 32;
pub const ERANGE:   c_int = 34;
pub const ENOSYS:   c_int = 38;
pub const EADDRINUSE: c_int = 98;
pub const EADDRNOTAVAIL: c_int = 99;
pub const ETIMEDOUT: c_int = 110;

// ── Constantes de señales ─────────────────────────────────────────────────────
pub const SIGABRT:  c_int = 6;
pub const SIGFPE:   c_int = 8;
pub const SIGILL:   c_int = 4;
pub const SIGPIPE:  c_int = 13;
pub const SIGSEGV:  c_int = 11;
pub const SIGTERM:  c_int = 15;
pub const SIGBUS:   c_int = 7;
pub const SIGALRM:  c_int = 14;
pub const SIG_DFL: sighandler_t = 0 as sighandler_t;
pub const SIG_IGN: sighandler_t = 1 as sighandler_t;

// ── Constantes de seek ────────────────────────────────────────────────────────
pub const SEEK_SET: c_int = 0;
pub const SEEK_CUR: c_int = 1;
pub const SEEK_END: c_int = 2;

// ── Constantes de clock ───────────────────────────────────────────────────────
pub const CLOCK_REALTIME:  clockid_t = 0;
pub const CLOCK_MONOTONIC: clockid_t = 1;

// ── Constantes de socket ──────────────────────────────────────────────────────
pub const AF_INET:  c_int = 2;
pub const AF_INET6: c_int = 10;
pub const AF_UNIX:  c_int = 1;
pub const SOCK_STREAM: c_int = 1;
pub const SOCK_DGRAM:  c_int = 2;

// ── syscall (modo normal — implementación de stub) ────────────────────────────
#[cfg(all(
    not(feature = "rustc-dep-of-std"),
    not(any(test, feature = "host-testing"))
))]
#[no_mangle]
pub unsafe extern "C" fn syscall(_num: c_long, _args: ...) -> c_long { 0 }

#[cfg(any(test, feature = "host-testing"))]
extern "C" { pub fn syscall(num: c_long, _args: ...) -> c_long; }

// ── Stubs de unwind (solo en builds de Eclipse, no en modo sysroot) ───────────
// En host `*-linux-musl` + `std`, el enlazador ya trae `libunwind`/`libc` reales;
// estos `#[no_mangle]` duplican símbolos (fallo de link al `cargo test` de lunas, etc.).
#[cfg(all(
    not(feature = "use_std"),
    not(any(test, feature = "host-testing")),
    eclipse_target
))]
mod unwind_stubs {
    #[no_mangle] pub unsafe extern "C" fn _Unwind_GetRegionStart() -> usize { 0 }
    #[no_mangle] pub unsafe extern "C" fn _Unwind_SetGR() {}
    #[no_mangle] pub unsafe extern "C" fn _Unwind_SetIP() {}
    #[no_mangle] pub unsafe extern "C" fn _Unwind_GetTextRelBase() -> usize { 0 }
    #[no_mangle] pub unsafe extern "C" fn _Unwind_GetDataRelBase() -> usize { 0 }
    #[no_mangle] pub unsafe extern "C" fn _Unwind_GetLanguageSpecificData() -> *const u8 { core::ptr::null() }
    #[no_mangle] pub unsafe extern "C" fn _Unwind_GetIPInfo() -> usize { 0 }
    #[no_mangle] pub unsafe extern "C" fn __gcc_personality_v0() {}
    #[no_mangle] pub unsafe extern "C" fn _Unwind_Resume() {}
}

// ── Panic handler propio de Eclipse OS (solo cuando NO hay std) ───────────────
#[cfg(all(
    feature = "panic-handler",
    not(feature = "use_std"),
    not(any(test, feature = "host-testing"))
))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
