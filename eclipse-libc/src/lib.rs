#![no_std]
#![feature(c_variadic)]
#![feature(linkage)]

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

#[cfg(any(target_os = "none", target_os = "linux", eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn syscall(num: c_long, arg1: c_long, arg2: c_long, arg3: c_long) -> c_long {
    eclipse_syscall::syscall3(num as usize, arg1 as usize, arg2 as usize, arg3 as usize) as c_long
}

#[cfg(not(any(target_os = "none", target_os = "linux", eclipse_target)))]
extern "C" {
    pub fn syscall(num: c_long, ...) -> c_long;
}

#[cfg(all(not(test), feature = "allocator", any(target_os = "none", target_os = "linux", eclipse_target)))]
#[global_allocator]
static ALLOCATOR: internal_alloc::Allocator = internal_alloc::Allocator;

#[cfg(all(not(test), feature = "panic_handler", any(target_os = "none", target_os = "linux", eclipse_target)))]
#[panic_handler]
#[linkage = "weak"]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    eclipse_syscall::call::exit(1)
}

#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetRegionStart() -> usize { 0 }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_SetGR() { }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_SetIP() { }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetTextRelBase() -> usize { 0 }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetDataRelBase() -> usize { 0 }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetLanguageSpecificData() -> *const u8 { core::ptr::null() }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_GetIPInfo() -> usize { 0 }
#[no_mangle]
pub unsafe extern "C" fn __gcc_personality_v0() { }
#[no_mangle]
pub unsafe extern "C" fn _Unwind_Resume() { }
