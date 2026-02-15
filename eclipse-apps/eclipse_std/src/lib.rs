//! Eclipse STD v2.0 - Standard Library for Eclipse OS
//!
//! This library provides a comprehensive std-like interface for Eclipse OS applications
//! using eclipse-libc (76 POSIX functions) for full functionality.
//!
//! # Features
//!
//! - File I/O using FILE streams
//! - Threading using pthread
//! - Synchronization (Mutex, Condvar)
//! - Collections (Vec, String, HashMap via alloc)
//! - println!/eprintln! macros
//! - main() function support

#![no_std]
#![feature(alloc_error_handler)]

pub extern crate alloc;
pub extern crate libc;

use core::panic::PanicInfo;

pub use core::fmt;
pub mod ffi {
    pub use crate::env::{OsStr, OsString};
}

pub use core::ptr;
pub mod heap;
#[macro_use]
pub mod macros;

pub mod io;
pub mod fs;
pub mod path;
pub mod process;
pub mod net;
pub mod env;
pub mod time;
pub mod thread;
pub mod sync;
pub mod error;
pub mod os;

pub mod collections {
    //! Collections module - re-exports from alloc
    pub use alloc::vec::Vec;
    pub use alloc::string::String;
    pub use alloc::boxed::Box;
    pub use alloc::collections::*;
}

pub mod prelude {
    //! Prelude - common imports for Eclipse OS applications
    pub use core::prelude::v1::*;
    pub use crate::heap::init_heap;
    pub use crate::{print, println, eprint, eprintln};
    pub use crate::eclipse_main;
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
    pub use alloc::borrow::{ToOwned, Borrow};
}

// Re-export core macros to be available via std::...
pub use core::{panic, assert, assert_eq, assert_ne, debug_assert, debug_assert_eq, debug_assert_ne, unreachable, write, writeln, todo, unimplemented, compile_error};

/// Initialize the Eclipse OS application runtime
pub fn init_runtime() {
    heap::init_heap();
}

/// Main wrapper that calls user's main function
pub fn main_wrapper<F>(user_main: F) -> !
where
    F: FnOnce() -> i32 + core::panic::UnwindSafe,
{
    // Initialize runtime
    init_runtime();
    
    // Call user's main function
    let exit_code = user_main();
    
    // Exit the application
    eclipse_syscall::call::exit(exit_code);
}

/// Macro to create a main entry point for Eclipse OS applications
#[macro_export]
macro_rules! eclipse_main {
    ($main_fn:ident) => {
        #[cfg(not(any(target_os = "linux", unix)))]
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            $crate::main_wrapper($main_fn)
        }

        #[cfg(any(target_os = "linux", unix))]
        #[no_mangle]
        pub extern "C" fn main(_argc: isize, _argv: *const *const u8) -> ! {
            $crate::main_wrapper($main_fn)
        }
    };
}

/// Panic handler for Eclipse OS applications
#[cfg(feature = "panic-handler")]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    eprintln!("\n!!! PANIC !!!");
    if let Some(location) = info.location() {
        eprintln!("Location: {}:{}:{}", 
            location.file(), location.line(), location.column());
    }
    let message = info.message();
    eprintln!("Message: {}", message);
    
    // Exit with error code
    unsafe {
        eclipse_syscall::call::exit(1);
    }
}

/// Alloc error handler
#[cfg(feature = "alloc-error-handler")]
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    eprintln!("\n!!! ALLOCATION ERROR !!!");
    eprintln!("Failed to allocate {} bytes with alignment {}", 
        layout.size(), layout.align());
    
    unsafe {
        eclipse_syscall::call::exit(2);
    }
}

// Re-export macros - they are already exported via #[macro_export]
// but we want them to be available via std::print! etc.
// pub use crate::{print, println, eprint, eprintln};
