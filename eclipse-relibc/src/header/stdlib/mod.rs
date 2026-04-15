//! stdlib.h - Standard library
use crate::types::*;
use crate::eclipse_syscall::call::exit as sys_exit;

#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, feature = "eclipse-syscall")))]
#[no_mangle]
pub unsafe extern "C" fn abort() -> ! {
    use crate::header::unistd::_exit;
    _exit(1);
    loop {}
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn exit(status: c_int) -> !;
}

#[cfg(all(not(any(test, feature = "host-testing")), any(eclipse_target, feature = "eclipse-syscall")))]
#[no_mangle]
pub unsafe extern "C" fn exit(status: c_int) -> ! {
    use crate::header::unistd::_exit;
    _exit(status);
    loop {}
}

// Re-export malloc/free/calloc/realloc from alloc module
pub use crate::internal_alloc::{malloc, free, calloc, realloc};

// String to number conversions

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn atoi(s: *const c_char) -> c_int {
    strtol(s, core::ptr::null_mut(), 10) as c_int
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn atol(s: *const c_char) -> c_long {
    strtol(s, core::ptr::null_mut(), 10)
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn atoll(s: *const c_char) -> c_longlong {
    strtoll(s, core::ptr::null_mut(), 10)
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
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

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtol(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_long {
    strtol(s, endptr, base)
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn strtoll(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_longlong {
    strtol(s, endptr, base) as c_longlong
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
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

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn __isoc23_strtoul(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulong {
    strtoul(s, endptr, base)
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn strtoull(s: *const c_char, endptr: *mut *mut c_char, base: c_int) -> c_ulonglong {
    strtoul(s, endptr, base) as c_ulonglong
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
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

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn strtof(nptr: *const c_char, endptr: *mut *mut c_char) -> c_float {
    strtod(nptr, endptr) as c_float
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn atof(nptr: *const c_char) -> c_double {
    strtod(nptr, core::ptr::null_mut())
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
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

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn abs(n: c_int) -> c_int {
    if n < 0 { -n } else { n }
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn labs(n: c_long) -> c_long {
    if n < 0 { -n } else { n }
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn llabs(n: c_longlong) -> c_longlong {
    if n < 0 { -n } else { n }
}

#[thread_local]
static mut RAND_SEED: u32 = 1;

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn rand() -> c_int {
    RAND_SEED = RAND_SEED.wrapping_mul(1103515245).wrapping_add(12345);
    ((RAND_SEED / 65536) % 32768) as c_int
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn srand(seed: c_uint) {
    RAND_SEED = seed;
}

// ─── Environment variable storage ───────────────────────────────────────────
//
// We maintain a flat heap-allocated array of NUL-terminated "KEY=VALUE\0"
// strings and expose the standard `environ` pointer (char **environ) so that
// C programs (including bash) can iterate over the environment directly.
//
// Layout of ENVIRON_ARRAY: a null-terminated array of *mut c_char pointers.
// ENVIRON_STORAGE holds the heap allocation; ENVIRON_CAP its capacity.

use crate::internal_alloc::{malloc as libc_malloc, free as libc_free, realloc as libc_realloc};

/// Number of environment entries currently stored.
static mut ENVIRON_COUNT: usize = 0;
/// Capacity of the ENVIRON_ARRAY (in pointer slots, including the NULL terminator).
static mut ENVIRON_CAP: usize = 0;
/// Heap-allocated array of *mut c_char pointers, NULL-terminated.
static mut ENVIRON_ARRAY: *mut *mut c_char = core::ptr::null_mut();

/// The `environ` symbol required by POSIX (char **environ).
/// Points to ENVIRON_ARRAY.
#[no_mangle]
pub static mut environ: *mut *mut c_char = core::ptr::null_mut();

/// Initialize the environ table from the envp array passed by the kernel on
/// the process stack.  Called from `__libc_start_main` / `_start`.
/// If `envp` is NULL the function is a no-op.
pub unsafe fn environ_init(envp: *const *const c_char) {
    if envp.is_null() {
        // Ensure environ points to a valid NULL-terminated array even if empty.
        if ENVIRON_ARRAY.is_null() {
            let ptr = libc_malloc(core::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
            if !ptr.is_null() {
                *ptr = core::ptr::null_mut();
                ENVIRON_ARRAY = ptr;
                ENVIRON_CAP = 1;
                environ = ENVIRON_ARRAY;
            }
        }
        return;
    }
    // Count entries.
    let mut count = 0usize;
    let mut p = envp;
    while !(*p).is_null() { count += 1; p = p.add(1); }

    // Allocate array (count entries + NULL terminator).
    let cap = count + 1;
    let ptr = libc_malloc(cap * core::mem::size_of::<*mut c_char>()) as *mut *mut c_char;
    if ptr.is_null() { return; }

    // Copy each string into a fresh heap allocation.
    for i in 0..count {
        let src = *(envp.add(i)) as *const u8;
        let len = {
            let mut l = 0usize;
            while *src.add(l) != 0 { l += 1; }
            l
        };
        let dst = libc_malloc(len + 1) as *mut c_char;
        if dst.is_null() {
            // Leak everything so far; can't do much else without panic.
            *ptr.add(i) = core::ptr::null_mut();
            continue;
        }
        core::ptr::copy_nonoverlapping(src, dst as *mut u8, len + 1);
        *ptr.add(i) = dst;
    }
    *ptr.add(count) = core::ptr::null_mut();

    ENVIRON_ARRAY = ptr;
    ENVIRON_COUNT = count;
    ENVIRON_CAP = cap;
    environ = ENVIRON_ARRAY;
}

/// Ensure ENVIRON_ARRAY has room for at least one more entry + NULL.
unsafe fn environ_grow() -> bool {
    let needed = ENVIRON_COUNT + 2; // +1 new entry, +1 NULL
    if needed <= ENVIRON_CAP { return true; }
    let new_cap = needed + 8;
    let new_ptr = libc_realloc(
        ENVIRON_ARRAY as *mut c_void,
        new_cap * core::mem::size_of::<*mut c_char>(),
    ) as *mut *mut c_char;
    if new_ptr.is_null() { return false; }
    ENVIRON_ARRAY = new_ptr;
    ENVIRON_CAP = new_cap;
    environ = ENVIRON_ARRAY;
    true
}

/// Return a pointer to the `environ` array for use by execv/execvp.
pub fn environ_ptr() -> *const *const c_char {
    unsafe { environ as *const *const c_char }
}

/// Internal helper: look up an env variable name (as a Rust str), return its
/// index in ENVIRON_ARRAY or None.
unsafe fn environ_find(name: &str) -> Option<usize> {
    if ENVIRON_ARRAY.is_null() { return None; }
    let nlen = name.len();
    for i in 0..ENVIRON_COUNT {
        let entry = *ENVIRON_ARRAY.add(i);
        if entry.is_null() { break; }
        let entry_bytes = entry as *const u8;
        // Check that the first `nlen` bytes match `name` and the next byte is '='.
        let matches = name.bytes().enumerate().all(|(j, b)| *entry_bytes.add(j) == b);
        if matches && *entry_bytes.add(nlen) == b'=' {
            return Some(i);
        }
    }
    None
}

/// Internal: get env value as a Rust `&str` (zero-copy, borrows from heap).
pub fn getenv_str(name: &str) -> Option<&'static str> {
    unsafe {
        let idx = environ_find(name)?;
        let entry = *ENVIRON_ARRAY.add(idx) as *const u8;
        let nlen = name.len();
        // Value starts after "NAME="
        let val_ptr = entry.add(nlen + 1);
        let mut val_len = 0usize;
        while *val_ptr.add(val_len) != 0 { val_len += 1; }
        let slice = core::slice::from_raw_parts(val_ptr, val_len);
        core::str::from_utf8(slice).ok()
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn getenv(name: *const c_char) -> *mut c_char {
    if name.is_null() { return core::ptr::null_mut(); }
    let name_str = match core::ffi::CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => return core::ptr::null_mut(),
    };
    match environ_find(name_str) {
        Some(idx) => {
            let entry = *ENVIRON_ARRAY.add(idx);
            // Return pointer to the value part (after "NAME=").
            entry.add(name_str.len() + 1)
        }
        None => core::ptr::null_mut(),
    }
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int {
    if name.is_null() || value.is_null() {
        *crate::header::errno::__errno_location() = 22; // EINVAL
        return -1;
    }
    let name_str = match core::ffi::CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => { *crate::header::errno::__errno_location() = 22; return -1; }
    };
    // Reject empty name or name containing '='.
    if name_str.is_empty() || name_str.contains('=') {
        *crate::header::errno::__errno_location() = 22; // EINVAL
        return -1;
    }
    let val_str = core::ffi::CStr::from_ptr(value).to_bytes();
    let new_entry_len = name_str.len() + 1 + val_str.len() + 1; // "NAME=VALUE\0"
    let new_entry = libc_malloc(new_entry_len) as *mut c_char;
    if new_entry.is_null() {
        *crate::header::errno::__errno_location() = 12; // ENOMEM
        return -1;
    }
    {
        let p = new_entry as *mut u8;
        core::ptr::copy_nonoverlapping(name_str.as_ptr(), p, name_str.len());
        *p.add(name_str.len()) = b'=';
        core::ptr::copy_nonoverlapping(val_str.as_ptr(), p.add(name_str.len() + 1), val_str.len());
        *p.add(new_entry_len - 1) = 0;
    }
    match environ_find(name_str) {
        Some(idx) if overwrite != 0 => {
            let old = *ENVIRON_ARRAY.add(idx);
            if !old.is_null() { libc_free(old as *mut c_void); }
            *ENVIRON_ARRAY.add(idx) = new_entry;
        }
        Some(_) => {
            // Variable exists and overwrite==0: discard new entry.
            libc_free(new_entry as *mut c_void);
        }
        None => {
            if !environ_grow() {
                libc_free(new_entry as *mut c_void);
                *crate::header::errno::__errno_location() = 12;
                return -1;
            }
            *ENVIRON_ARRAY.add(ENVIRON_COUNT) = new_entry;
            ENVIRON_COUNT += 1;
            *ENVIRON_ARRAY.add(ENVIRON_COUNT) = core::ptr::null_mut();
        }
    }
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn unsetenv(name: *const c_char) -> c_int {
    if name.is_null() {
        *crate::header::errno::__errno_location() = 22;
        return -1;
    }
    let name_str = match core::ffi::CStr::from_ptr(name).to_str() {
        Ok(s) => s,
        Err(_) => { *crate::header::errno::__errno_location() = 22; return -1; }
    };
    if name_str.is_empty() || name_str.contains('=') {
        *crate::header::errno::__errno_location() = 22;
        return -1;
    }
    if let Some(idx) = environ_find(name_str) {
        let old = *ENVIRON_ARRAY.add(idx);
        if !old.is_null() { libc_free(old as *mut c_void); }
        // Shift remaining entries left.
        for i in idx..ENVIRON_COUNT - 1 {
            *ENVIRON_ARRAY.add(i) = *ENVIRON_ARRAY.add(i + 1);
        }
        ENVIRON_COUNT -= 1;
        *ENVIRON_ARRAY.add(ENVIRON_COUNT) = core::ptr::null_mut();
    }
    0
}

/// putenv("NAME=VALUE") — add or change an environment variable.
#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn putenv(string: *mut c_char) -> c_int {
    if string.is_null() {
        *crate::header::errno::__errno_location() = 22;
        return -1;
    }
    // Find '=' separator.
    let mut eq_off = 0usize;
    loop {
        let b = *(string as *const u8).add(eq_off);
        if b == 0 {
            // No '=' — treat as unsetenv(string).
            return unsetenv(string as *const c_char);
        }
        if b == b'=' { break; }
        eq_off += 1;
    }
    let name_slice = core::slice::from_raw_parts(string as *const u8, eq_off);
    let name_str = match core::str::from_utf8(name_slice) {
        Ok(s) => s,
        Err(_) => { *crate::header::errno::__errno_location() = 22; return -1; }
    };
    // Build a new heap copy of the string so caller can later modify/free theirs.
    let total_len = {
        let mut l = 0usize;
        while *(string as *const u8).add(l) != 0 { l += 1; }
        l + 1
    };
    let copy = libc_malloc(total_len) as *mut c_char;
    if copy.is_null() {
        *crate::header::errno::__errno_location() = 12;
        return -1;
    }
    core::ptr::copy_nonoverlapping(string as *const u8, copy as *mut u8, total_len);
    match environ_find(name_str) {
        Some(idx) => {
            let old = *ENVIRON_ARRAY.add(idx);
            if !old.is_null() { libc_free(old as *mut c_void); }
            *ENVIRON_ARRAY.add(idx) = copy;
        }
        None => {
            if !environ_grow() {
                libc_free(copy as *mut c_void);
                *crate::header::errno::__errno_location() = 12;
                return -1;
            }
            *ENVIRON_ARRAY.add(ENVIRON_COUNT) = copy;
            ENVIRON_COUNT += 1;
            *ENVIRON_ARRAY.add(ENVIRON_COUNT) = core::ptr::null_mut();
        }
    }
    0
}

/// clearenv — clear the environment.
#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn clearenv() -> c_int {
    if !ENVIRON_ARRAY.is_null() {
        for i in 0..ENVIRON_COUNT {
            let p = *ENVIRON_ARRAY.add(i);
            if !p.is_null() { libc_free(p as *mut c_void); }
        }
        *ENVIRON_ARRAY = core::ptr::null_mut();
    }
    ENVIRON_COUNT = 0;
    0
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn system(_command: *const c_char) -> c_int {
    -1
}

#[cfg(all(not(any(test, feature = "host-testing")), eclipse_target))]
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
