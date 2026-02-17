//! string.h - String operations
use crate::types::*;
use core::ptr;

// Memory operations

#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, n: size_t) -> *mut c_void {
    if n > 0 {
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rdi") dest => _,
            inout("rsi") src => _,
        );
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, n: size_t) -> *mut c_void {
    if n > 0 {
        if (dest as usize) < (src as usize) {
            // Adelante hacia atrás
            core::arch::asm!(
                "rep movsb",
                inout("rcx") n => _,
                inout("rdi") dest => _,
                inout("rsi") src => _,
            );
        } else {
            // Atrás hacia adelante
            core::arch::asm!(
                "std",
                "rep movsb",
                "cld",
                inout("rcx") n => _,
                inout("rdi") (dest as *mut u8).add(n - 1) => _,
                inout("rsi") (src as *const u8).add(n - 1) => _,
            );
        }
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut c_void, c: c_int, n: size_t) -> *mut c_void {
    if n > 0 {
        core::arch::asm!(
            "rep stosb",
            inout("rcx") n => _,
            inout("rdi") s => _,
            in("al") c as u8,
        );
    }
    s
}

#[no_mangle]
pub unsafe extern "C" fn __memcpy_chk(dest: *mut c_void, src: *const c_void, n: size_t, _destlen: size_t) -> *mut c_void {
    memcpy(dest, src, n)
}

#[no_mangle]
pub unsafe extern "C" fn __memset_chk(s: *mut c_void, c: c_int, n: size_t, _destlen: size_t) -> *mut c_void {
    memset(s, c, n)
}

#[no_mangle]
pub unsafe extern "C" fn __memmove_chk(dest: *mut c_void, src: *const c_void, n: size_t, _destlen: size_t) -> *mut c_void {
    memmove(dest, src, n)
}

#[no_mangle]
pub unsafe extern "C" fn __strcpy_chk(dest: *mut c_char, src: *const c_char, _destlen: size_t) -> *mut c_char {
    strcpy(dest, src)
}

#[no_mangle]
pub unsafe extern "C" fn __strcat_chk(dest: *mut c_char, src: *const c_char, _destlen: size_t) -> *mut c_char {
    strcat(dest, src)
}

#[no_mangle]
pub unsafe extern "C" fn __strncpy_chk(dest: *mut c_char, src: *const c_char, n: size_t, _destlen: size_t) -> *mut c_char {
    strncpy(dest, src, n)
}

#[no_mangle]
pub unsafe extern "C" fn __stpcpy_chk(dest: *mut c_char, src: *const c_char, _destlen: size_t) -> *mut c_char {
    stpcpy(dest, src)
}

#[no_mangle]
pub unsafe extern "C" fn bcmp(s1: *const c_void, s2: *const c_void, n: size_t) -> c_int {
    let s1 = s1 as *const u8;
    let s2 = s2 as *const u8;
    for i in 0..n {
        if *s1.add(i) != *s2.add(i) {
            return 1;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn ffs(i: c_int) -> c_int {
    if i == 0 {
        0
    } else {
        i.trailing_zeros() as c_int + 1
    }
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(s1: *const c_void, s2: *const c_void, n: size_t) -> c_int {
    let p1 = s1 as *const u8;
    let p2 = s2 as *const u8;
    
    for i in 0..n {
        let c1 = *p1.add(i);
        let c2 = *p2.add(i);
        if c1 != c2 {
            return c1 as c_int - c2 as c_int;
        }
    }
    0
}

// String operations

#[no_mangle]
pub unsafe extern "C" fn strlen(s: *const c_char) -> size_t {
    crate::c_str::strlen(s)
}

#[no_mangle]
pub unsafe extern "C" fn strcmp(s1: *const c_char, s2: *const c_char) -> c_int {
    let mut i = 0;
    loop {
        let c1 = *s1.add(i);
        let c2 = *s2.add(i);
        
        if c1 == 0 && c2 == 0 {
            return 0;
        }
        if c1 != c2 {
            return c1 as c_int - c2 as c_int;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncmp(s1: *const c_char, s2: *const c_char, n: size_t) -> c_int {
    for i in 0..n {
        let c1 = *s1.add(i);
        let c2 = *s2.add(i);
        
        if c1 == 0 && c2 == 0 {
            return 0;
        }
        if c1 != c2 {
            return c1 as c_int - c2 as c_int;
        }
        if c1 == 0 {
            return 0;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn strcpy(dest: *mut c_char, src: *const c_char) -> *mut c_char {
    let mut i = 0;
    loop {
        let c = *src.add(i);
        *dest.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn stpcpy(dest: *mut c_char, src: *const c_char) -> *mut c_char {
    let mut i = 0;
    loop {
        let c = *src.add(i);
        *dest.add(i) = c;
        if c == 0 {
            return dest.add(i);
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncpy(dest: *mut c_char, src: *const c_char, n: size_t) -> *mut c_char {
    let mut i = 0;
    while i < n {
        let c = *src.add(i);
        *dest.add(i) = c;
        if c == 0 {
            break;
        }
        i += 1;
    }
    // Pad with zeros if needed
    while i < n {
        *dest.add(i) = 0;
        i += 1;
    }
    dest
}

#[no_mangle]
pub unsafe extern "C" fn strcat(dest: *mut c_char, src: *const c_char) -> *mut c_char {
    let dest_len = strlen(dest);
    strcpy(dest.add(dest_len), src);
    dest
}

#[no_mangle]
pub unsafe extern "C" fn strncat(dest: *mut c_char, src: *const c_char, n: size_t) -> *mut c_char {
    let dest_len = strlen(dest);
    let mut i = 0;
    while i < n {
        let c = *src.add(i);
        if c == 0 {
            break;
        }
        *dest.add(dest_len + i) = c;
        i += 1;
    }
    *dest.add(dest_len + i) = 0;
    dest
}

#[no_mangle]
pub unsafe extern "C" fn strchr(s: *const c_char, c: c_int) -> *mut c_char {
    let target = c as c_char;
    let mut i = 0;
    loop {
        let ch = *s.add(i);
        if ch == target {
            return s.add(i) as *mut c_char;
        }
        if ch == 0 {
            return ptr::null_mut();
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strrchr(s: *const c_char, c: c_int) -> *mut c_char {
    let target = c as c_char;
    let len = strlen(s);
    let mut i = len;
    
    loop {
        if *s.add(i) == target {
            return s.add(i) as *mut c_char;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn strstr(haystack: *const c_char, needle: *const c_char) -> *mut c_char {
    let needle_len = strlen(needle);
    if needle_len == 0 {
        return haystack as *mut c_char;
    }
    
    let haystack_len = strlen(haystack);
    if needle_len > haystack_len {
        return ptr::null_mut();
    }
    
    for i in 0..=(haystack_len - needle_len) {
        if strncmp(haystack.add(i), needle, needle_len) == 0 {
            return haystack.add(i) as *mut c_char;
        }
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn strdup(s: *const c_char) -> *mut c_char {
    use crate::internal_alloc::malloc;
    
    let len = strlen(s);
    let new_str = malloc(len + 1) as *mut c_char;
    if new_str.is_null() {
        return ptr::null_mut();
    }
    memcpy(new_str as *mut c_void, s as *const c_void, len + 1);
    new_str
}

#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const c_void, c: c_int, n: size_t) -> *mut c_void {
    let p = s as *const u8;
    let target = c as u8;
    for i in 0..n {
        if *p.add(i) == target {
            return p.add(i) as *mut c_void;
        }
    }
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn strnlen(s: *const c_char, maxlen: size_t) -> size_t {
    let mut i = 0;
    while i < maxlen && *s.add(i) != 0 {
        i += 1;
    }
    i
}

#[no_mangle]
pub unsafe extern "C" fn strcasecmp(s1: *const c_char, s2: *const c_char) -> c_int {
    let mut i = 0;
    loop {
        let mut c1 = *s1.add(i) as u8;
        let mut c2 = *s2.add(i) as u8;
        
        if c1 >= b'A' && c1 <= b'Z' { c1 += 32; }
        if c2 >= b'A' && c2 <= b'Z' { c2 += 32; }
        
        if c1 == 0 && c2 == 0 {
            return 0;
        }
        if c1 != c2 {
            return c1 as c_int - c2 as c_int;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn strncasecmp(s1: *const c_char, s2: *const c_char, n: size_t) -> c_int {
    if n == 0 { return 0; }
    for i in 0..n {
        let mut c1 = *s1.add(i) as u8;
        let mut c2 = *s2.add(i) as u8;
        
        if c1 >= b'A' && c1 <= b'Z' { c1 += 32; }
        if c2 >= b'A' && c2 <= b'Z' { c2 += 32; }
        
        if c1 == 0 && c2 == 0 {
            return 0;
        }
        if c1 != c2 {
            return c1 as c_int - c2 as c_int;
        }
        if c1 == 0 {
            return 0;
        }
    }
    0
}
