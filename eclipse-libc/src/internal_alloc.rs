//! Memory allocator
use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use eclipse_syscall::call::mmap;
use eclipse_syscall::flag::*;
use crate::types::*;

const ALIGNMENT: usize = 16;

pub struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if size == 0 {
            return ptr::null_mut();
        }
        
        // For now, use mmap for all allocations
        match mmap(0, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) {
            Ok(addr) => addr as *mut u8,
            Err(_) => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // TODO: implement munmap
    }
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn malloc(size: size_t) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }
    let layout = Layout::from_size_align_unchecked(size, ALIGNMENT);
    Allocator.alloc(layout) as *mut c_void
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" {
    pub fn malloc(size: size_t) -> *mut c_void;
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn free(_ptr: *mut c_void) {
    // TODO: implement
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" {
    pub fn free(ptr: *mut c_void);
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut c_void {
    let total = nmemb.saturating_mul(size);
    if total == 0 {
        return ptr::null_mut();
    }
    let ptr = malloc(total);
    if !ptr.is_null() {
        ptr::write_bytes(ptr as *mut u8, 0, total);
    }
    ptr
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" {
    pub fn calloc(nmemb: size_t, size: size_t) -> *mut c_void;
}

#[cfg(any(not(any(target_os = "linux", unix)), eclipse_target))]
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, new_size: size_t) -> *mut c_void {
    if ptr.is_null() {
        return malloc(new_size);
    }
    if new_size == 0 {
        free(ptr);
        return ptr::null_mut();
    }
    // TODO: optimize
    let new_ptr = malloc(new_size);
    if !new_ptr.is_null() {
        // This is a simplified version - we don't know old size
        ptr::copy_nonoverlapping(ptr as *const u8, new_ptr as *mut u8, new_size);
        free(ptr);
    }
    new_ptr
}

#[cfg(all(any(target_os = "linux", unix), not(eclipse_target)))]
extern "C" {
    pub fn realloc(ptr: *mut c_void, new_size: size_t) -> *mut c_void;
}
