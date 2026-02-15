//! sys/utsname.h - System identification
use crate::types::*;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct utsname {
    pub sysname: [c_char; 65],
    pub nodename: [c_char; 65],
    pub release: [c_char; 65],
    pub version: [c_char; 65],
    pub machine: [c_char; 65],
}

#[no_mangle]
pub unsafe extern "C" fn uname(buf: *mut utsname) -> c_int {
    if buf.is_null() {
        return -1;
    }
    
    // Stub: "EclipseOS", "eclipse", "0.1.0", "0.1.0", "x86_64"
    let sysname = b"EclipseOS\0";
    let nodename = b"eclipse\0";
    let release = b"0.1.0\0";
    let version = b"0.1.0\0";
    let machine = b"x86_64\0";
    
    ptr_copy(sysname.as_ptr(), (*buf).sysname.as_mut_ptr() as *mut u8, sysname.len());
    ptr_copy(nodename.as_ptr(), (*buf).nodename.as_mut_ptr() as *mut u8, nodename.len());
    ptr_copy(release.as_ptr(), (*buf).release.as_mut_ptr() as *mut u8, release.len());
    ptr_copy(version.as_ptr(), (*buf).version.as_mut_ptr() as *mut u8, version.len());
    ptr_copy(machine.as_ptr(), (*buf).machine.as_mut_ptr() as *mut u8, machine.len());
    
    0
}

unsafe fn ptr_copy(src: *const u8, dst: *mut u8, len: usize) {
    core::ptr::copy_nonoverlapping(src, dst, len);
}
