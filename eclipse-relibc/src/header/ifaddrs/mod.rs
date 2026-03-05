//! ifaddrs.h - Interface addresses
use crate::types::*;
use crate::internal_alloc::{malloc, free};
use core::ffi::c_void;

#[repr(C)]
pub struct ifaddrs {
    pub ifa_next: *mut ifaddrs,
    pub ifa_name: *mut c_char,
    pub ifa_flags: c_uint,
    pub ifa_addr: *mut crate::header::sys_socket::sockaddr,
    pub ifa_netmask: *mut crate::header::sys_socket::sockaddr,
    pub ifa_dstaddr: *mut crate::header::sys_socket::sockaddr,
    pub ifa_data: *mut c_void,
}

#[no_mangle]
pub unsafe extern "C" fn getifaddrs(ifap: *mut *mut ifaddrs) -> c_int {
    if ifap.is_null() { return -1; }
    
    // Provide a dummy loopback interface so X11 thinks network is available
    let name = b"lo\0";
    let entry = malloc(core::mem::size_of::<ifaddrs>()) as *mut ifaddrs;
    if entry.is_null() { return -1; }
    
    let name_buf = malloc(name.len());
    if name_buf.is_null() { 
        free(entry as *mut c_void);
        return -1; 
    }
    core::ptr::copy_nonoverlapping(name.as_ptr(), name_buf as *mut u8, name.len());
    
    (*entry).ifa_next = core::ptr::null_mut();
    (*entry).ifa_name = name_buf as *mut c_char;
    (*entry).ifa_flags = 0x1 | 0x8; // IFF_UP | IFF_LOOPBACK
    (*entry).ifa_addr = core::ptr::null_mut();
    (*entry).ifa_netmask = core::ptr::null_mut();
    (*entry).ifa_dstaddr = core::ptr::null_mut();
    (*entry).ifa_data = core::ptr::null_mut();
    
    *ifap = entry;
    0
}

#[no_mangle]
pub unsafe extern "C" fn freeifaddrs(_ifa: *mut ifaddrs) {
    // Stub
}
