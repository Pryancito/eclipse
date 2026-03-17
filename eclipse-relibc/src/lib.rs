#![no_std]
#![feature(c_variadic)]
#![feature(linkage)]
#![feature(alloc_error_handler)]
#![feature(thread_local)]

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::header::stdio::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($fmt:expr) => ($crate::print!(core::concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => ($crate::print!(core::concat!($fmt, "\n"), $($arg)*));
}

#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
core::arch::global_asm!(include_str!("posix_stubs.s"));

extern crate alloc;
extern crate eclipse_syscall;

pub mod types;
pub mod internal_alloc;
pub mod c_str;
pub mod stack_chk;
pub mod platform;
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
}

pub use types::*;
pub use header::stdio::*;
pub use header::stdlib::*;
pub use crate::internal_alloc::{malloc, free, calloc, realloc};
pub use header::string::*;
pub use header::pthread::*;
pub use header::unistd::*;
pub use header::time::*;
pub use header::errno::*;
pub use header::signal::*;
pub use header::poll::*;
pub use header::dlfcn::*;
pub use header::math::*;
pub use header::locale::*;
pub use header::sys_shm::*;
pub use header::sys_socket::*;
pub use header::sys_uio::*;
pub use header::sys_ioctl::*;
pub use header::net_inet::*;
pub use header::netdb::*;
pub use header::sys_utsname::*;
pub use header::sys_wait::*;
pub use header::sys_resource::*;
pub use header::sys_stat::*;
pub use header::sys_select::*;
pub use header::termios::*;
pub use header::ctype::*;
pub use header::fcntl::*;
pub use header::sys_mman::*;
pub use header::dirent::*;
pub use header::pwd::*;
pub use header::grp::*;
pub use header::ifaddrs::*;
pub use header::sys_eclipse::*;
pub const O_RDONLY: c_int = eclipse_syscall::flag::O_RDONLY as c_int;
pub const O_WRONLY: c_int = eclipse_syscall::flag::O_WRONLY as c_int;
pub const O_RDWR: c_int = eclipse_syscall::flag::O_RDWR as c_int;
pub const O_CREAT: c_int = eclipse_syscall::flag::O_CREAT as c_int;
pub const O_EXCL: c_int = eclipse_syscall::flag::O_EXCL as c_int;
pub const O_NOCTTY: c_int = eclipse_syscall::flag::O_NOCTTY as c_int;
pub const O_TRUNC: c_int = eclipse_syscall::flag::O_TRUNC as c_int;
pub const O_APPEND: c_int = eclipse_syscall::flag::O_APPEND as c_int;
pub const O_NONBLOCK: c_int = eclipse_syscall::flag::O_NONBLOCK as c_int;
pub const O_CLOEXEC: c_int = eclipse_syscall::flag::O_CLOEXEC as c_int;
pub const O_NOFOLLOW: c_int = eclipse_syscall::flag::O_NOFOLLOW as c_int;
pub const O_DIRECTORY: c_int = eclipse_syscall::flag::O_DIRECTORY as c_int;

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
pub const SYS_getrandom: c_int = eclipse_syscall::number::SYS_GETRANDOM as c_int;
#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
pub const SYS_getrandom: c_int = 318;

/// Linux x86-64 syscall number for futex(2).
/// Provided so that crates compiled against this libc replacement can reference
/// it without triggering "cannot find value" errors on Linux host builds.
pub const SYS_futex: c_long = 202;

/// Futex operation: wait for value change.
pub const FUTEX_WAIT: c_int = 0;
/// Futex operation: wake up waiters.
pub const FUTEX_WAKE: c_int = 1;
/// Futex flag: process-private (faster, no cross-process sharing needed).
pub const FUTEX_PRIVATE_FLAG: c_int = 128;

/// Maximum value of a signed 32-bit integer.
pub const INT_MAX: c_int = i32::MAX;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "none", target_os = "linux", eclipse_target)))]
#[no_mangle]
pub unsafe extern "C" fn syscall(num: c_long, ...) -> c_long {
    // Accept any number/type of arguments after `num` to remain ABI-compatible
    // with callers like getrandom that pass heterogeneous argument types.
    // On Eclipse OS the only syscall routed through this function is
    // SYS_getrandom; for all others we fall through and return 0.
    if num == SYS_getrandom as c_long {
        // Not reachable via this path (getrandom uses the dedicated shim).
    }
    0
}

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
extern "C" {
    pub fn syscall(num: c_long, ...) -> c_long;
}


#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetRegionStart() -> usize { 0 }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_SetGR() { }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_SetIP() { }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetTextRelBase() -> usize { 0 }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetDataRelBase() -> usize { 0 }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetLanguageSpecificData() -> *const u8 { core::ptr::null() }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetIPInfo() -> usize { 0 }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn __gcc_personality_v0() { }
#[cfg(all(not(any(test, feature = "host-testing")), not(target_os = "linux")))]
#[no_mangle]
pub unsafe extern "C" fn _Unwind_Resume() { }

// Handler de pánico propio solo para binarios "puros" de Eclipse OS
// (sin `std`): kernel/userspace con `target_os = "none"` o `--cfg eclipse_target`.
// En binarios host con `std`, el runtime de Rust ya define `panic_impl`,
// así que aquí lo desactivamos para evitar el duplicado.
#[cfg(all(
    not(any(test, feature = "host-testing")),
    feature = "panic-handler",
    eclipse_target,
))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    use crate::header::unistd::_exit;
    unsafe { _exit(1); }
    loop {}
}
