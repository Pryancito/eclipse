//! time.h - Time functions
use crate::types::*;

#[allow(non_camel_case_types)]
pub type clock_t = c_long;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct tm {
    pub tm_sec: c_int,
    pub tm_min: c_int,
    pub tm_hour: c_int,
    pub tm_mday: c_int,
    pub tm_mon: c_int,
    pub tm_year: c_int,
    pub tm_wday: c_int,
    pub tm_yday: c_int,
    pub tm_isdst: c_int,
    pub tm_gmtoff: c_long,
    pub tm_zone: *const c_char,
}

pub const CLOCK_REALTIME: clockid_t = 0;
pub const CLOCK_MONOTONIC: clockid_t = 1;

static mut TIME_COUNTER: time_t = 0;

#[no_mangle]
pub unsafe extern "C" fn clock_gettime(_clk_id: clockid_t, tp: *mut timespec) -> c_int {
    if tp.is_null() {
        return -1;
    }
    // Simple implementation
    (*tp).tv_sec = TIME_COUNTER;
    (*tp).tv_nsec = 0;
    0
}

#[no_mangle]
pub unsafe extern "C" fn gmtime_r(timer: *const time_t, result: *mut tm) -> *mut tm {
    if timer.is_null() || result.is_null() {
        return core::ptr::null_mut();
    }
    core::ptr::write_bytes(result, 0, 1);
    result
}

#[no_mangle]
pub unsafe extern "C" fn localtime_r(timer: *const time_t, result: *mut tm) -> *mut tm {
    gmtime_r(timer, result)
}

#[no_mangle]
pub unsafe extern "C" fn gettimeofday(_tv: *mut timeval, _tz: *mut c_void) -> c_int {
    if !_tv.is_null() {
        (*_tv).tv_sec = TIME_COUNTER;
        (*_tv).tv_usec = 0;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn setitimer(_which: c_int, _new_value: *const itimerval, _old_value: *mut itimerval) -> c_int {
    0
}

#[no_mangle]
pub unsafe extern "C" fn getitimer(_which: c_int, curr_value: *mut itimerval) -> c_int {
    if !curr_value.is_null() {
        (*curr_value).it_interval.tv_sec = 0;
        (*curr_value).it_interval.tv_usec = 0;
        (*curr_value).it_value.tv_sec = 0;
        (*curr_value).it_value.tv_usec = 0;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn time(tloc: *mut time_t) -> time_t {
    TIME_COUNTER += 1;
    if !tloc.is_null() {
        *tloc = TIME_COUNTER;
    }
    TIME_COUNTER
}

#[no_mangle]
pub unsafe extern "C" fn clock() -> clock_t {
    TIME_COUNTER * 1000
}

#[no_mangle]
pub unsafe extern "C" fn difftime(time1: time_t, time0: time_t) -> c_double {
    (time1 - time0) as c_double
}

#[no_mangle]
pub unsafe extern "C" fn mktime(timeptr: *mut tm) -> time_t {
    if timeptr.is_null() {
        return -1;
    }
    let t = (*timeptr).tm_sec as time_t +
            (*timeptr).tm_min as time_t * 60 +
            (*timeptr).tm_hour as time_t * 3600;
    t
}

#[no_mangle]
pub unsafe extern "C" fn timegm(timeptr: *mut tm) -> time_t {
    mktime(timeptr)
}

#[no_mangle]
pub unsafe extern "C" fn gmtime(timer: *const time_t) -> *mut tm {
    if timer.is_null() {
        return core::ptr::null_mut();
    }
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn localtime(timer: *const time_t) -> *mut tm {
    gmtime(timer)
}

#[no_mangle]
pub unsafe extern "C" fn nanosleep(req: *const timespec, rem: *mut timespec) -> c_int {
    if req.is_null() {
        return -1;
    }
    use eclipse_syscall::number::SYS_NANOSLEEP;
    let result = eclipse_syscall::syscall2(
        SYS_NANOSLEEP,
        req as usize,
        rem as usize
    );
    if result == usize::MAX {
        -1
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn ctime(_timep: *const time_t) -> *mut c_char {
    static mut BUF: [c_char; 26] = [0; 26];
    let s = b"Mon Jan 01 00:00:00 1970\n\0";
    for i in 0..s.len() {
        BUF[i] = s[i] as c_char;
    }
    BUF.as_mut_ptr()
}
