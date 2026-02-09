//! stdlib.h - Standard library
use crate::types::*;
use eclipse_syscall::call::exit as sys_exit;

#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    sys_exit(1);
}

// Re-export malloc/free/calloc/realloc from alloc module
pub use crate::alloc::{malloc, free, calloc, realloc};
