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
pub unsafe extern "C" fn fcntl(_fd: c_int, _cmd: c_int, _arg: ...) -> c_int {
    // Stub — Eclipse OS does not have a fcntl syscall yet.
    // F_SETFL/F_GETFL succeed silently; non-blocking mode is not yet enforced.
    0
}
