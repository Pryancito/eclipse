//! Bring-up smoke test for the C-compile + FFI-link pipeline that will host
//! vendored NVIDIA open-gpu-kernel-modules source in Eclipse. See build.rs
//! and vendor/smoketest.c -- none of this is NVIDIA code yet.
#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

extern "C" {
    fn nvrm_smoketest_add(a: u32, b: u32) -> u32;
}

/// Set by the C side via `nvrm_smoketest_log` so callers can confirm the
/// C-to-Rust callback direction actually ran (not just C-to-C linkage).
static LAST_LOGGED: AtomicU32 = AtomicU32::new(0);

/// Called from vendor/smoketest.c -- proves C code can call back into a
/// Rust-implemented function, the same shape `os-interface.h` will need.
#[no_mangle]
pub extern "C" fn nvrm_smoketest_log(value: u32) {
    LAST_LOGGED.store(value, Ordering::SeqCst);
}

/// Runs the round trip (Rust -> C -> Rust) and returns `(result, logged)`.
/// Both should equal `a + b` if the pipeline works end to end.
pub fn smoke_test(a: u32, b: u32) -> (u32, u32) {
    let result = unsafe { nvrm_smoketest_add(a, b) };
    (result, LAST_LOGGED.load(Ordering::SeqCst))
}
