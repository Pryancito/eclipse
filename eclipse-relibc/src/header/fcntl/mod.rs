//! fcntl.h - File control
use crate::types::*;

// --- fcntl commands (compatible with Linux for rustix/x11rb) ---
pub const F_DUPFD:   c_int = 0;
pub const F_GETFD:   c_int = 1;
pub const F_SETFD:   c_int = 2;
pub const F_GETFL:   c_int = 3;
pub const F_SETFL:   c_int = 4;

// --- File descriptor flags ---
pub const FD_CLOEXEC: c_int = 1;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, mut arg: ...) -> c_int {
    let extra: usize = arg.arg::<usize>();
    match crate::eclipse_syscall::call::fcntl(fd as usize, cmd as usize, extra) {
        Ok(v) => v as c_int,
        Err(e) => {
            *crate::header::errno::__errno_location() = e.errno as c_int;
            -1
        }
    }
}
