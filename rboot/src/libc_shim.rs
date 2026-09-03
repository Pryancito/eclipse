//! C runtime symbols the compiler expects the platform to provide.
//!
//! Newer LLVM turns any "walk a NUL-terminated `u16` string" loop -- uefi-rs'
//! `CStr16` handling in `File::get_info`, `system::firmware_vendor`, ... --
//! into a call to the libc `wcslen` (UEFI targets have a 16-bit `wchar_t`).
//! There is no libc here, so link fails with "undefined symbol: wcslen" unless
//! we define it. The loads are volatile so LLVM cannot recognise this loop as
//! the very idiom it replaced and turn the definition into a call to itself.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(s: *const u16) -> usize {
    let mut len = 0usize;
    // SAFETY: the caller guarantees `s` points at a NUL-terminated string.
    unsafe {
        while core::ptr::read_volatile(s.add(len)) != 0 {
            len += 1;
        }
    }
    len
}
