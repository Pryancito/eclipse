//! Eclipse STD v2.0 - Standard Library for Eclipse OS
//!
//! Provides a std-like interface compatible con la API de Rust std, usando
//! eclipse-libc para syscalls de Eclipse OS.
//!
//! ## Modo std_compat
//!
//! Con la feature `std_compat`, eclipse_std omite `#[panic_handler]` y
//! `#[alloc_error_handler]`, permitiendo coexistir con libstd (p.ej. al usar
//! Smithay en builds Linux). En ese caso, std aporta los lang items.

#![no_std]
#![cfg_attr(not(feature = "std_compat"), feature(alloc_error_handler))]
#![feature(prelude_import)]
#![feature(lang_items)]
#![feature(core_intrinsics)]

pub extern crate core;
pub extern crate alloc;

// Core functionality re-exports
pub use core::option::Option::{self, Some, None};
pub use core::result::Result::{self, Ok, Err};
pub use core::marker::{Send, Sync, Sized, Unpin};
pub use core::ops::{Fn, FnMut, FnOnce, Drop};
pub use core::clone::Clone;
pub use core::default::Default;
pub use core::convert::{From, Into, AsRef, AsMut, TryFrom, TryInto};
pub use core::iter::{Iterator, IntoIterator, Extend};
pub use core::fmt::{self, Debug, Display};
pub use core::u16;
pub use core::u32;
pub use core::u64;
pub use core::i32;
pub use core::f32;
pub use core::f64;

pub extern crate libc; // This is eclipse-libc-posix from Cargo.toml
pub use alloc::vec as vec;

use core::panic::PanicInfo;

pub mod ffi {
    pub use core::ffi::*;
    pub use crate::libc::{c_char, c_int, c_long, c_void, size_t, pid_t, FILE};
    pub use crate::env::{OsStr, OsString};
}

// Standard module re-exports (bridge to core)
pub mod result { pub use core::result::*; }
pub mod option { pub use core::option::*; }
pub mod iter { pub use core::iter::*; }
pub mod marker { pub use core::marker::*; }
pub mod ops { pub use core::ops::*; }
pub mod mem { pub use core::mem::*; }
pub mod convert { pub use core::convert::*; }
pub mod any { pub use core::any::*; }
pub mod cell { pub use core::cell::*; }
pub mod ptr { pub use core::ptr::*; }
pub mod slice { pub use core::slice::*; }
pub mod str { pub use core::str::*; }
pub mod char { pub use core::char::*; }
pub mod hash { pub use core::hash::*; }
pub mod hint { pub use core::hint::*; }
pub mod task { pub use core::task::*; }
pub mod future { pub use core::future::*; }
pub mod pin { pub use core::pin::*; }
pub mod cmp { pub use core::cmp::*; }
pub mod ascii { pub use core::ascii::*; }
pub mod intrinsics { pub use core::intrinsics::*; }

pub mod heap;
#[macro_use]
pub mod macros;
pub mod rt;

pub mod io;
pub mod fs;
pub mod path;
pub mod process;
pub mod net;
pub mod sync;
pub mod env;
pub mod time;
pub mod thread;
pub mod error;
pub mod os;

pub mod collections {
    //! Collections module - re-exports from alloc
    pub use alloc::vec::Vec;
    pub use alloc::string::String;
    pub use alloc::boxed::Box;
    pub use alloc::collections::*;
}

/// Compatibilidad con std::string (p. ej. bitflags con feature std)
pub mod string {
    pub use alloc::string::*;
}

pub mod prelude {
    //! Prelude - compatible con std::prelude::v1 para que dependencias (bitflags, wayland-*, etc.) compilen.
    pub mod v1 {
        pub use core::prelude::v1::*;
        pub use core::cmp::{PartialEq, PartialOrd, Eq, Ord};
        pub use core::option::Option::{self, Some, None};
        pub use core::result::Result::{self, Ok, Err};
        pub use core::matches;

        // Mismos re-exports que std::prelude::v1 (marker, ops, iter, mem, convert)
        pub use crate::marker::{Send, Sized, Sync, Unpin};
        pub use crate::ops::{Drop, Fn, FnMut, FnOnce};
        pub use crate::mem::{drop, align_of, align_of_val, size_of, size_of_val};
        pub use crate::convert::{AsMut, AsRef, From, Into, TryInto, TryFrom};
        pub use crate::iter::{
            DoubleEndedIterator, ExactSizeIterator, Extend, IntoIterator, Iterator,
        };
        pub use core::clone::Clone;
        pub use core::default::Default;
        pub use core::borrow::{Borrow, BorrowMut};

        pub use crate::heap::init_heap;
        pub use crate::{print, println, eprint, eprintln};
        pub use crate::rt::argc;
        pub use alloc::string::{String, ToString};
        pub use alloc::vec::Vec;
        pub use alloc::format;
        pub use alloc::boxed::Box;

        pub use crate::io::{Read, Write, stdin, stdout, stderr};
        pub use crate::fs::{self, File};
        pub use crate::path::{self, Path, PathBuf};
        pub use crate::process::{self, Command, Child};
        pub use crate::net;
        pub use crate::time;
        pub use crate::thread;
        pub use crate::sync::{Mutex, Condvar};
        pub use alloc::borrow::ToOwned;
    }
    #[prelude_import]
    pub use self::v1::*;
    pub use self::v1 as rust_2015;
    pub use self::v1 as rust_2018;
    pub use self::v1 as rust_2021;
}

// Re-export core macros to be available via std::...
pub use core::{panic, assert, assert_eq, assert_ne, debug_assert, debug_assert_eq, debug_assert_ne, unreachable, write, writeln, todo, unimplemented, compile_error};

/// Initialize the Eclipse OS application runtime
pub fn init_runtime() {
    heap::init_heap();
}

/// Main wrapper that calls user's main function
pub fn main_wrapper<F, R>(user_main: F) -> !
where
    F: FnOnce() -> R,
    R: Termination,
{
    // Initialize runtime (heap, etc)
    init_runtime();
    
    // Notify init (PID 1) that we are READY and ALIVE
    // Note: We use our re-exported libc here
    unsafe {
        // En Eclipse OS, SYS_SEND (3) requiere un msg_type. Usamos 0 para READY/HEART.
        let _ = crate::libc::eclipse_send(1, 0, b"READY\0".as_ptr() as *const crate::ffi::c_void, 6, 0);
        let _ = crate::libc::eclipse_send(1, 0, b"HEART\0".as_ptr() as *const crate::ffi::c_void, 6, 0);
    }
    
    // Call user's main function
    let res = user_main();
    let exit_code = res.report();
    
    unsafe {
        crate::libc::exit(exit_code as i32);
    }
}

pub trait Termination {
    fn report(self) -> i32;
}

impl Termination for () {
    fn report(self) -> i32 { 0 }
}

impl Termination for i32 {
    fn report(self) -> i32 { self }
}

impl<T, E> Termination for Result<T, E> {
    fn report(self) -> i32 {
        if self.is_ok() { 0 } else { 1 }
    }
}

#[lang = "start"]
fn lang_start<T: Termination + 'static>(
    main: fn() -> T,
    _argc: isize,
    _argv: *const *const u8,
    _sigpipe: u8,
) -> isize {
    // This is called by the compiler-generated main function.
    // For now we just call main and ignore argc/argv/sigpipe (they are handled in rt.rs)
    main().report() as isize
}

/// El punto de entrada real (crt0) está en `rt::_start`: lee argc del stack,
/// inicializa heap, notifica a init y llama a la `main()` del usuario, luego exit(code).
/// La aplicación debe definir `#[no_mangle] pub extern "Rust" fn main() -> i32`.

/// Macro to create a main entry point for Eclipse OS applications
/// This hides the need for #![no_main] and pub extern "C" fn _start()
#[deprecated(note = "Standard fn main() is now supported without #![no_main]. This macro is obsolete.")]
#[macro_export]
macro_rules! main {
    ($main_fn:ident) => {
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            // Stack alignment for x86_64
            unsafe { core::arch::asm!("and rsp, -16", options(nomem, nostack, preserves_flags)); }
            $crate::main_wrapper($main_fn)
        }
    };
}

/// Panic handler for Eclipse OS applications.
/// Omitido en std_compat: std real proporciona el handler.
#[cfg(all(
    feature = "panic-handler",
    not(feature = "no-panic-handler"),
    not(feature = "std_compat"),
    not(test)
))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    crate::eprintln!("\n!!! ECLIPSE APP PANIC !!!");
    if let Some(location) = info.location() {
        crate::eprintln!("Location: {}:{}:{}", 
            location.file(), location.line(), location.column());
    }
    
    // Exit with error code
    unsafe {
        crate::libc::exit(1);
    }
}

/// Alloc error handler.
/// Omitido en std_compat: std real proporciona el handler.
#[cfg(all(
    feature = "alloc-error-handler",
    not(feature = "no-panic-handler"),
    not(feature = "std_compat"),
    not(test)
))]
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    crate::eprintln!("\n!!! ALLOCATION ERROR !!!");
    crate::eprintln!("Failed to allocate {} bytes with alignment {}", 
        layout.size(), layout.align());
    
    unsafe {
        crate::libc::exit(2);
    }
}

