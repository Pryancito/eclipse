//! stdlib.h - Standard library
use eclipse_syscall::call::exit;
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    exit(1);
}
