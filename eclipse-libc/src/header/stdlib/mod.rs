//! stdlib.h - Standard library
use crate::types::*;
use eclipse_syscall::call::exit as sys_exit;
use crate::header::string::strlen;

#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    sys_exit(1);
}

#[no_mangle]
pub unsafe extern "C" fn exit(status: c_int) -> ! {
    sys_exit(status as i32);
}

// Re-export malloc/free/calloc/realloc from alloc module
pub use crate::internal_alloc::{malloc, free, calloc, realloc};

// String to number conversions

#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const c_char) -> c_int {
    strtol(s, core::ptr::null_mut(), 10) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn atol(s: *const c_char) -> c_long {
    strtol(s, core::ptr::null_mut(), 10)
}

#[no_mangle]
pub unsafe extern "C" fn atoll(s: *const c_char) -> c_longlong {
    strtoll(s, core::ptr::null_mut(), 10)
}

#[no_mangle]
pub unsafe extern "C" fn strtol(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long {
    let mut result: c_long = 0;
    let mut sign: c_long = 1;
    let mut ptr = s;
    
    // Skip whitespace
    while *ptr == b' ' as c_char || *ptr == b'\t' as c_char || *ptr == b'\n' as c_char {
        ptr = ptr.add(1);
    }
    
    // Handle sign
    if *ptr == b'-' as c_char {
        sign = -1;
        ptr = ptr.add(1);
    } else if *ptr == b'+' as c_char {
        ptr = ptr.add(1);
    }
    
    // Detect base if base == 0
    let actual_base = if base == 0 {
        if *ptr == b'0' as c_char {
            ptr = ptr.add(1);
            if *ptr == b'x' as c_char || *ptr == b'X' as c_char {
                ptr = ptr.add(1);
                16
            } else {
                8
            }
        } else {
            10
        }
    } else {
        base
    };
    
    // Handle 0x prefix for base 16
    if actual_base == 16 && *ptr == b'0' as c_char {
        ptr = ptr.add(1);
        if *ptr == b'x' as c_char || *ptr == b'X' as c_char {
            ptr = ptr.add(1);
        } else {
            ptr = ptr.sub(1);
        }
    }
    
    // Parse digits
    loop {
        let c = *ptr;
        let digit = if c >= b'0' as c_char && c <= b'9' as c_char {
            (c as u8 - b'0') as c_int
        } else if c >= b'a' as c_char && c <= b'z' as c_char {
            (c as u8 - b'a' + 10) as c_int
        } else if c >= b'A' as c_char && c <= b'Z' as c_char {
            (c as u8 - b'A' + 10) as c_int
        } else {
            break;
        };
        
        if digit >= actual_base {
            break;
        }
        
        result = result * actual_base as c_long + digit as c_long;
        ptr = ptr.add(1);
    }
    
    if !endptr.is_null() {
        *endptr = ptr as *mut c_char;
    }
    
    result * sign
}

#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtol(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long {
    strtol(s, endptr, base)
}

#[no_mangle]
pub unsafe extern "C" fn strtoll(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_longlong {
    strtol(s, endptr, base) as c_longlong
}

#[no_mangle]
pub unsafe extern "C" fn strtoul(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulong {
    let mut result: c_ulong = 0;
    let mut ptr = s;
    
    // Skip whitespace
    while *ptr == b' ' as c_char || *ptr == b'\t' as c_char || *ptr == b'\n' as c_char {
        ptr = ptr.add(1);
    }
    
    // Handle + sign (unsigned doesn't use -)
    if *ptr == b'+' as c_char {
        ptr = ptr.add(1);
    }
    
    // Detect base if base == 0
    let actual_base = if base == 0 {
        if *ptr == b'0' as c_char {
            ptr = ptr.add(1);
            if *ptr == b'x' as c_char || *ptr == b'X' as c_char {
                ptr = ptr.add(1);
                16
            } else {
                8
            }
        } else {
            10
        }
    } else {
        base
    };
    
    // Handle 0x prefix for base 16
    if actual_base == 16 && *ptr == b'0' as c_char {
        ptr = ptr.add(1);
        if *ptr == b'x' as c_char || *ptr == b'X' as c_char {
            ptr = ptr.add(1);
        } else {
            ptr = ptr.sub(1);
        }
    }
    
    // Parse digits
    loop {
        let c = *ptr;
        let digit = if c >= b'0' as c_char && c <= b'9' as c_char {
            (c as u8 - b'0') as c_int
        } else if c >= b'a' as c_char && c <= b'z' as c_char {
            (c as u8 - b'a' + 10) as c_int
        } else if c >= b'A' as c_char && c <= b'Z' as c_char {
            (c as u8 - b'A' + 10) as c_int
        } else {
            break;
        };
        
        if digit >= actual_base {
            break;
        }
        
        result = result * actual_base as c_ulong + digit as c_ulong;
        ptr = ptr.add(1);
    }
    
    if !endptr.is_null() {
        *endptr = ptr as *mut c_char;
    }
    
    result
}

#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtoul(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulong {
    strtoul(s, endptr, base)
}

#[no_mangle]
pub unsafe extern "C" fn strtoull(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulonglong {
    strtoul(s, endptr, base) as c_ulonglong
}

#[no_mangle]
pub unsafe extern "C" fn strtod(nptr: *const c_char, endptr: *mut *mut c_char) -> c_double {
    let mut result: f64 = 0.0;
    let mut ptr = nptr;
    
    // Skip whitespace
    while *ptr == b' ' as c_char || *ptr == b'\t' as c_char || *ptr == b'\n' as c_char {
        ptr = ptr.add(1);
    }
    
    let mut sign = 1.0;
    if *ptr == b'-' as c_char {
        sign = -1.0;
        ptr = ptr.add(1);
    } else if *ptr == b'+' as c_char {
        ptr = ptr.add(1);
    }
    
    while *ptr >= b'0' as c_char && *ptr <= b'9' as c_char {
        result = result * 10.0 + (*ptr as u8 - b'0') as f64;
        ptr = ptr.add(1);
    }
    
    if *ptr == b'.' as c_char {
        ptr = ptr.add(1);
        let mut factor = 0.1;
        while *ptr >= b'0' as c_char && *ptr <= b'9' as c_char {
            result += (*ptr as u8 - b'0') as f64 * factor;
            factor /= 10.0;
            ptr = ptr.add(1);
        }
    }
    
    if !endptr.is_null() {
        *endptr = ptr as *mut c_char;
    }
    
    result * sign
}

#[no_mangle]
pub unsafe extern "C" fn strtof(nptr: *const c_char, endptr: *mut *mut c_char) -> c_float {
    strtod(nptr, endptr) as c_float
}

#[no_mangle]
pub unsafe extern "C" fn atof(nptr: *const c_char) -> c_double {
    strtod(nptr, core::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn qsort(
    base: *mut c_void,
    nmemb: size_t,
    size: size_t,
    compar: extern "C" fn(*const c_void, *const c_void) -> c_int
) {
    if nmemb < 2 || size == 0 {
        return;
    }
    // Simple Bubble Sort for now (O(n^2))
    let mut i = 0;
    while i < nmemb - 1 {
        let mut j = 0;
        while j < nmemb - i - 1 {
            let p1 = (base as *mut u8).add(j * size);
            let p2 = (base as *mut u8).add((j + 1) * size);
            if compar(p1 as *const c_void, p2 as *const c_void) > 0 {
                // Swap
                let mut k = 0;
                while k < size {
                    let tmp = *p1.add(k);
                    *p1.add(k) = *p2.add(k);
                    *p2.add(k) = tmp;
                    k += 1;
                }
            }
            j += 1;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn abs(n: c_int) -> c_int {
    if n < 0 { -n } else { n }
}

#[no_mangle]
pub unsafe extern "C" fn labs(n: c_long) -> c_long {
    if n < 0 { -n } else { n }
}

#[no_mangle]
pub unsafe extern "C" fn llabs(n: c_longlong) -> c_longlong {
    if n < 0 { -n } else { n }
}

static mut RAND_SEED: u32 = 1;

#[no_mangle]
pub unsafe extern "C" fn rand() -> c_int {
    RAND_SEED = RAND_SEED.wrapping_mul(1103515245).wrapping_add(12345);
    ((RAND_SEED / 65536) % 32768) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn srand(seed: c_uint) {
    RAND_SEED = seed;
}

#[no_mangle]
pub unsafe extern "C" fn getenv(_name: *const c_char) -> *mut c_char {
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn setenv(_name: *const c_char, _value: *const c_char, _overwrite: c_int) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn unsetenv(_name: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn system(_command: *const c_char) -> c_int {
    -1
}

#[no_mangle]
pub unsafe extern "C" fn realpath(path: *const c_char, resolved_path: *mut c_char) -> *mut c_char {
    use crate::header::string::{strcpy, strdup};
    if !resolved_path.is_null() {
        strcpy(resolved_path, path);
        resolved_path
    } else {
        strdup(path)
    }
}
