//! Eclipse STD - Standard Library Compatibility Layer for Eclipse OS
//! 
//! This library provides a std-like interface for Eclipse OS applications
//! while maintaining compatibility with the no_std microkernel architecture.

#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::panic::PanicInfo;
use eclipse_libc::{exit, yield_cpu};

pub mod heap;
pub mod io;
pub mod macros;
pub mod sync;

pub mod prelude {
    pub use crate::heap::init_heap;
    pub use crate::{print, println, eprint, eprintln};
    pub use crate::eclipse_main;
    pub use alloc::string::{String, ToString};
    pub use alloc::vec::Vec;
    pub use alloc::format;
    pub use alloc::boxed::Box;
}

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
    exit(exit_code);
}

/// Macro to create a main entry point for Eclipse OS applications
#[macro_export]
macro_rules! eclipse_main {
    ($main_fn:ident) => {
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            $crate::main_wrapper($main_fn)
        }
    };
}

/// Panic handler for Eclipse OS applications
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    use crate::io::{eprintln};
    
    eprintln!("\n!!! PANIC !!!");
    if let Some(location) = info.location() {
        eprintln!("Location: {}:{}:{}", 
            location.file(), location.line(), location.column());
    }
    
    // Exit with error code
    exit(1);
}

/// Alloc error handler
#[alloc_error_handler]
fn alloc_error_handler(layout: alloc::alloc::Layout) -> ! {
    use crate::io::{eprintln};
    
    eprintln!("\n!!! ALLOCATION ERROR !!!");
    eprintln!("Failed to allocate {} bytes with alignment {}", 
        layout.size(), layout.align());
    
    exit(2);
}

// Re-export macros
pub use crate::io::{print, println, eprint, eprintln};
