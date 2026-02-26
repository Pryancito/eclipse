//! Funciones de utilidad estándar.
//! Símbolos C (no_mangle + extern "C") para que el linker resuelva las referencias
//! que emite el compilador al optimizar copias/zeroing de memoria.

/// Símbolo C requerido por el linker en target no-GNU (x86_64-unknown-eclipse).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = *src.add(i);
    }
}

#[cfg(target_env = "gnu")]
pub unsafe fn memcpy(dest: *mut u8, src: *const u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = *src.add(i);
    }
}

/// Símbolo C requerido por el linker en target no-GNU (x86_64-unknown-eclipse).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn memset(dest: *mut u8, val: u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = val;
    }
}

#[cfg(target_env = "gnu")]
pub unsafe fn memset(dest: *mut u8, val: u8, n: usize) {
    for i in 0..n {
        *dest.add(i) = val;
    }
}

/// Símbolo C requerido por el linker (comparación de slices, etc.).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn bcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    for i in 0..n {
        if *s1.add(i) != *s2.add(i) {
            return 1;
        }
    }
    0
}

/// Símbolo C requerido por el linker (swap/copias con solapamiento).
#[cfg(not(target_env = "gnu"))]
#[no_mangle]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, n: usize) {
    if n == 0 {
        return;
    }
    if dest < src as *mut u8 {
        for i in 0..n {
            *dest.add(i) = *src.add(i);
        }
    } else if dest > src as *mut u8 {
        for i in (0..n).rev() {
            *dest.add(i) = *src.add(i);
        }
    }
}

pub unsafe fn memcmp(s1: *const u8, s2: *const u8, n: usize) -> i32 {
    for i in 0..n {
        let a = *s1.add(i);
        let b = *s2.add(i);
        if a != b {
            return (a as i32) - (b as i32);
        }
    }
    0
}

pub unsafe fn strlen(s: *const u8) -> usize {
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    len
}
