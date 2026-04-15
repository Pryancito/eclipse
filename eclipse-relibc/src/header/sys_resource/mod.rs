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

#[repr(C)]
#[derive(Copy, Clone)]
pub struct rusage {
    pub ru_utime: crate::types::timeval,
    pub ru_stime: crate::types::timeval,
    pub ru_maxrss: c_long,
    pub ru_ixrss: c_long,
    pub ru_idrss: c_long,
    pub ru_isrss: c_long,
    pub ru_minflt: c_long,
    pub ru_majflt: c_long,
    pub ru_nswap: c_long,
    pub ru_inblock: c_long,
    pub ru_oublock: c_long,
    pub ru_msgsnd: c_long,
    pub ru_msgrcv: c_long,
    pub ru_nsignals: c_long,
    pub ru_nvcsw: c_long,
    pub ru_nivcsw: c_long,
}

pub const RUSAGE_SELF:     c_int = 0;
pub const RUSAGE_CHILDREN: c_int = -1i32;

#[cfg(all(not(any(test, feature = "host-testing")), any(target_os = "eclipse", eclipse_target, not(all(target_os = "linux", not(any(target_os = "eclipse", eclipse_target)))))))]
#[no_mangle]
pub unsafe extern "C" fn getrusage(_who: c_int, usage: *mut rusage) -> c_int {
    if !usage.is_null() {
        core::ptr::write_bytes(usage as *mut u8, 0, core::mem::size_of::<rusage>());
    }
    0
}
