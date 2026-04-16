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
#![allow(ambiguous_glob_reexports)]

// ── Fuente de 'core' según modo ──────────────────────────────────────────────
#[cfg(feature = "rustc-dep-of-std")]
extern crate rustc_std_workspace_core as core;

#[cfg(not(feature = "rustc-dep-of-std"))]
extern crate alloc;

#[cfg(all(not(feature = "rustc-dep-of-std"), feature = "eclipse-syscall"))]
pub extern crate eclipse_syscall;

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
    not(any(test, feature = "host-testing"))
))]
mod asm_stubs {
    #[cfg(feature = "crt0")]
    core::arch::global_asm!(include_str!("posix_stubs.s"));

    #[cfg(not(feature = "crt0"))]
    core::arch::global_asm!(include_str!("posix_stubs_nostart.s"));
}

// ── Prelude mínima para no_core (rustc-dep-of-std) ───────────────────────────
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
    pub mod unistd;
    pub mod time;
    pub mod errno;
    pub mod signal;
    pub mod fcntl;
    pub mod sys_mman;
    pub mod sys_ioctl;
    pub mod sys_stat;
    pub mod sys_wait;
    pub mod sys_select;
    pub mod sys_socket;
    pub mod sys_uio;
    pub mod sys_utsname;
    pub mod sys_resource;
    pub mod sys_eventfd;
    pub mod sys_eclipse;
    pub mod poll;
    pub mod dirent;
    pub mod netdb;
    pub mod net_inet;
    pub mod pthread;
    pub mod math;
    pub mod ctype;
}

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
pub use header::fcntl::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_mman::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_ioctl::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_stat::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_wait::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_select::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_socket::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_uio::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_utsname::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_resource::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_eventfd::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::sys_eclipse::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::poll::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::dirent::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::netdb::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::net_inet::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::math::*;
#[cfg(not(feature = "rustc-dep-of-std"))]
pub use header::ctype::*;

#[cfg(not(feature = "rustc-dep-of-std"))]
pub use types::*;

// ── Stubs de unwind (solo en builds de Eclipse, no en modo sysroot) ───────────
#[cfg(all(
    not(feature = "use_std"),
    not(any(test, feature = "host-testing"))
))]
mod unwind_stubs {
    #[no_mangle]
    pub unsafe extern "C" fn _Unwind_Resume() { loop {} }
}

#[cfg(all(
    feature = "panic-handler",
    not(feature = "use_std"),
    not(any(test, feature = "host-testing")),
    not(feature = "rustc-dep-of-std")
))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[cfg(all(
    not(feature = "use_std"),
    not(any(test, feature = "host-testing")),
    not(feature = "rustc-dep-of-std")
))]
#[alloc_error_handler]
fn alloc_error(_layout: core::alloc::Layout) -> ! {
    loop {}
}

// ── Syscall wrapper ────────────────────────────────────────────────────────
#[cfg(not(feature = "rustc-dep-of-std"))]
pub mod syscall_wrappers {
    use crate::types::*;
    #[no_mangle]
    pub unsafe extern "C" fn syscall(_num: c_long, mut _args: ...) -> c_long {
        // Implementación básica de syscall variable args
        0
    }
}
