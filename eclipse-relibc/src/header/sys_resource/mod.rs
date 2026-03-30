//! sys/resource.h - Resource management
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct rlimit {
    pub rlim_cur: c_int,
    pub rlim_max: c_int,
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getrlimit(_resource: c_int, rlp: *mut rlimit) -> c_int {
    if !rlp.is_null() {
        (*rlp).rlim_cur = 256;
        (*rlp).rlim_max = 256;
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn setrlimit(_resource: c_int, _rlp: *const rlimit) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getpriority(_which: c_int, _who: c_int) -> c_int {
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn setpriority(_which: c_int, _who: c_int, _prio: c_int) -> c_int {
    0
}
