//! sys/ioctl.h - I/O Control
use crate::types::*;

#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, request: c_ulong, arg: *mut c_void) -> c_int {
    let ret = crate::eclipse_syscall::syscall3(
        crate::eclipse_syscall::number::SYS_IOCTL,
        fd as usize,
        request as usize,
        arg as usize,
    );
    if ret >= 4096 { // Error range in our unsigned return
        // In our ABI, errors are negative values cast to usize (e.g. -EFAULT = 0xFF...F2)
        // Convert back to positive errno
        *crate::__errno_location() = -(ret as isize) as c_int;
        -1
    } else {
        ret as c_int
    }
}
