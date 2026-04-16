//! sys/socket.h - Socket interface
use crate::types::*;
use crate::header::sys_uio::iovec;
use crate::header::net_inet::in_addr;
use core::ffi::c_int;
use core::mem::size_of;
use crate::eclipse_syscall::call::{socket as sys_socket, bind as sys_bind, listen as sys_listen, accept as sys_accept, connect as sys_connect};

/// Type alias for address family (compatible with Linux/POSIX for rustix).
pub type sa_family_t = c_ushort;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr {
    pub sa_family: c_ushort,
    pub sa_data: [c_char; 14],
}

/// Unix domain socket address (compatible with Linux/POSIX for rustix and x11rb).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct sockaddr_un {
    pub sun_family: sa_family_t,
    pub sun_path: [c_char; 108],
}

// Note: in6_addr and sockaddr_in6 are defined in crate::types.

/// Linger option for SO_LINGER (compatible with Linux/POSIX for rustix).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct linger {
    pub l_onoff: c_int,
    pub l_linger: c_int,
}

/// IPv4 multicast request (compatible with Linux/POSIX for rustix).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ip_mreq {
    pub imr_multiaddr: in_addr,
    pub imr_interface: in_addr,
}

/// IPv6 multicast request (compatible with Linux/POSIX for rustix).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ipv6_mreq {
    pub ipv6mr_multiaddr: in6_addr,
    pub ipv6mr_interface: c_uint,
}

// --- Socket type flags (compatible with Linux for rustix/x11rb) ---
pub const SOCK_CLOEXEC:  c_int = 0o2000000;  // 524288
pub const SOCK_NONBLOCK: c_int = 0o4000;     // 2048

// --- Socket option levels ---
pub const SOL_SOCKET: c_int = 1;

// --- Ancillary data types ---
pub const SCM_RIGHTS: c_int = 1;

// --- SO_* socket options ---
pub const SO_DEBUG:       c_int = 1;
pub const SO_REUSEADDR:   c_int = 2;
pub const SO_TYPE:        c_int = 3;
pub const SO_ERROR:       c_int = 4;
pub const SO_DONTROUTE:   c_int = 5;
pub const SO_BROADCAST:   c_int = 6;
pub const SO_SNDBUF:      c_int = 7;
pub const SO_RCVBUF:      c_int = 8;
pub const SO_KEEPALIVE:   c_int = 9;
pub const SO_OOBINLINE:   c_int = 10;
pub const SO_NO_CHECK:    c_int = 11;
pub const SO_PRIORITY:    c_int = 12;
pub const SO_LINGER:      c_int = 13;
pub const SO_BSDCOMPAT:   c_int = 14;
pub const SO_REUSEPORT:   c_int = 15;
pub const SO_PASSCRED:    c_int = 16;
pub const SO_PEERCRED:    c_int = 17;
pub const SO_RCVLOWAT:    c_int = 18;
pub const SO_SNDLOWAT:    c_int = 19;
pub const SO_RCVTIMEO:    c_int = 20;
pub const SO_SNDTIMEO:    c_int = 21;
pub const SO_ACCEPTCONN:  c_int = 30;
pub const SO_SNDBUFFORCE: c_int = 32;
pub const SO_RCVBUFFORCE: c_int = 33;
pub const SO_DOMAIN:      c_int = 39;
pub const SO_NOSIGPIPE:   c_int = 67;

// --- Protocol numbers ---
pub const IPPROTO_IP:   c_int = 0;
pub const IPPROTO_TCP:  c_int = 6;
pub const IPPROTO_UDP:  c_int = 17;
pub const IPPROTO_IPV6: c_int = 41;

// --- TCP socket options ---
pub const TCP_NODELAY:   c_int = 1;
pub const TCP_KEEPIDLE:  c_int = 4;
pub const TCP_KEEPINTVL: c_int = 5;
pub const TCP_KEEPCNT:   c_int = 6;

// --- IP socket options ---
pub const IP_TTL:           c_int = 2;
pub const IP_MULTICAST_IF:  c_int = 32;
pub const IP_MULTICAST_TTL: c_int = 33;
pub const IP_MULTICAST_LOOP:c_int = 34;
pub const IP_ADD_MEMBERSHIP:c_int = 35;
pub const IP_DROP_MEMBERSHIP:c_int = 36;

// --- IPv6 socket options ---
pub const IPV6_V6ONLY:         c_int = 26;
pub const IPV6_MULTICAST_IF:   c_int = 17;
pub const IPV6_MULTICAST_HOPS: c_int = 18;
pub const IPV6_MULTICAST_LOOP: c_int = 19;
pub const IPV6_ADD_MEMBERSHIP: c_int = 20;
pub const IPV6_DROP_MEMBERSHIP:c_int = 21;
pub const IPV6_TCLASS:         c_int = 67;

/// Message header para sendmsg/recvmsg (compatible con Linux/POSIX para rustix).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct msghdr {
    pub msg_name: *mut c_void,
    pub msg_namelen: socklen_t,
    pub msg_iov: *mut iovec,
    pub msg_iovlen: size_t,
    pub msg_control: *mut c_void,
    pub msg_controllen: size_t,
    pub msg_flags: c_int,
}

/// Control message header (datos auxiliares en recvmsg/sendmsg).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct cmsghdr {
    pub cmsg_len: size_t,
    pub cmsg_level: c_int,
    pub cmsg_type: c_int,
}

// Constantes MSG_* (valores Linux, compatibles con rustix)
pub const MSG_OOB: c_int = 1;
pub const MSG_PEEK: c_int = 2;
pub const MSG_DONTROUTE: c_int = 4;
pub const MSG_CTRUNC: c_int = 8;
pub const MSG_TRUNC: c_int = 0x20;
pub const MSG_DONTWAIT: c_int = 0x40;
pub const MSG_EOR: c_int = 0x80;
pub const MSG_WAITALL: c_int = 0x100;
pub const MSG_CONFIRM: c_int = 0x800;
pub const MSG_ERRQUEUE: c_int = 0x2000;
pub const MSG_NOSIGNAL: c_int = 0x4000;
pub const MSG_MORE: c_int = 0x8000;
pub const MSG_CMSG_CLOEXEC: c_int = 0x40000000;

/// Alineación para datos de control (CMSG), como en libc Linux.
#[inline]
const fn cmsg_align(len: usize) -> usize {
    (len + size_of::<usize>() - 1) & !(size_of::<usize>() - 1)
}

/// Primer header de control en un msghdr (nombre C para compatibilidad con libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub unsafe fn CMSG_FIRSTHDR(mhdr: *const msghdr) -> *mut cmsghdr {
    if !mhdr.is_null() && (*mhdr).msg_controllen as usize >= size_of::<cmsghdr>() {
        (*mhdr).msg_control.cast::<cmsghdr>()
    } else {
        core::ptr::null_mut::<cmsghdr>()
    }
}

/// Puntero a los datos de un cmsghdr (nombre C para compatibilidad con libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub unsafe fn CMSG_DATA(cmsg: *const cmsghdr) -> *mut c_uchar {
    cmsg.offset(1) as *mut c_uchar
}

/// Espacio necesario para un mensaje de control de longitud `length` (nombre C para libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub const fn CMSG_SPACE(length: c_uint) -> c_uint {
    (cmsg_align(length as usize) + cmsg_align(size_of::<cmsghdr>())) as c_uint
}

/// Longitud de un cmsghdr con payload de `length` bytes (nombre C para libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub const fn CMSG_LEN(length: c_uint) -> c_uint {
    cmsg_align(size_of::<cmsghdr>()) as c_uint + length
}

/// Siguiente header de control (nombre C para libc/rustix).
#[allow(non_snake_case)]
#[inline]
pub unsafe fn CMSG_NXTHDR(mhdr: *const msghdr, cmsg: *const cmsghdr) -> *mut cmsghdr {
    if mhdr.is_null() || cmsg.is_null() {
        return core::ptr::null_mut::<cmsghdr>();
    }
    let len = (*cmsg).cmsg_len as usize;
    if len < size_of::<cmsghdr>() {
        return core::ptr::null_mut::<cmsghdr>();
    }
    let next = (cmsg as *const u8).add(cmsg_align(len)) as *mut cmsghdr;
    let max = (*mhdr).msg_control as *const u8;
    let max = max.add((*mhdr).msg_controllen as usize);
    if next.add(1) as *const u8 > max {
        core::ptr::null_mut::<cmsghdr>()
    } else {
        next
    }
}


#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn socket(domain: c_int, type_: c_int, protocol: c_int) -> c_int;
    pub fn bind(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
    pub fn listen(sockfd: c_int, backlog: c_int) -> c_int;
    pub fn accept(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int;
    pub fn connect(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int;
    pub fn send(sockfd: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t;
    pub fn recv(sockfd: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t;
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn socket(domain: c_int, type_: c_int, protocol: c_int) -> c_int {
    match sys_socket(domain as usize, type_ as usize, protocol as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn bind(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int {
    match sys_bind(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn listen(sockfd: c_int, backlog: c_int) -> c_int {
    match sys_listen(sockfd as usize, backlog as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn accept(sockfd: c_int, addr: *mut sockaddr, addrlen: *mut socklen_t) -> c_int {
    // Note: addr and addrlen can be null. Our sys_accept wrapper handles it if we pass them.
    match sys_accept(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(fd) => fd as c_int,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn connect(sockfd: c_int, addr: *const sockaddr, addrlen: socklen_t) -> c_int {
    match sys_connect(sockfd as usize, addr as usize, addrlen as usize) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn shutdown(_sockfd: c_int, _how: c_int) -> c_int {
    -1
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn send(sockfd: c_int, buf: *const c_void, len: size_t, _flags: c_int) -> ssize_t {
    use crate::eclipse_syscall::call::write as sys_write;
    let slice = core::slice::from_raw_parts(buf as *const u8, len);
    match sys_write(sockfd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn recv(sockfd: c_int, buf: *mut c_void, len: size_t, _flags: c_int) -> ssize_t {
    use crate::eclipse_syscall::call::read as sys_read;
    let slice = core::slice::from_raw_parts_mut(buf as *mut u8, len);
    match sys_read(sockfd as usize, slice) {
        Ok(n) => n as ssize_t,
        Err(_) => -1,
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn getsockname(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn getpeername(_sockfd: c_int, _addr: *mut sockaddr, _addrlen: *mut socklen_t) -> c_int {
    -1
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn setsockopt(_sockfd: c_int, _level: c_int, _optname: c_int, _optval: *const c_void, _optlen: socklen_t) -> c_int {
    0
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn getsockopt(_sockfd: c_int, _level: c_int, _optname: c_int, _optval: *mut c_void, _optlen: *mut socklen_t) -> c_int {
    0
}

#[cfg(any(test, feature = "host-testing"))]
extern "C" {
    pub fn sendmsg(sockfd: c_int, msg: *const msghdr, flags: c_int) -> ssize_t;
    pub fn recvmsg(sockfd: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t;
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn sendmsg(sockfd: c_int, msg: *const msghdr, flags: c_int) -> ssize_t {
    // Use sys_sendmsg (syscall 46) so the kernel's SCM_RIGHTS control-message
    // path (fd passing) is exercised.  The previous implementation used
    // sys_write which silently dropped the ancillary data.
    let result = eclipse_syscall::syscall3(46, sockfd as usize, msg as usize, flags as usize);
    let signed = result as isize;
    if signed < 0 && signed >= -4096 {
        *crate::header::errno::__errno_location() = (-signed) as c_int;
        -1
    } else {
        result as ssize_t
    }
}

#[cfg(not(any(test, feature = "host-testing")))]
#[no_mangle]
pub unsafe extern "C" fn recvmsg(sockfd: c_int, msg: *mut msghdr, flags: c_int) -> ssize_t {
    // Use sys_recvmsg (syscall 47) so that:
    //   1. errno is set correctly (previously -1 was returned without setting
    //      errno, so stale errno caused EAGAIN to be misidentified as a fatal
    //      error, triggering a spurious Wayland client disconnect).
    //   2. SCM_RIGHTS ancillary data (fd passing) is delivered via the kernel's
    //      control-buffer path instead of being silently discarded.
    let result = eclipse_syscall::syscall3(47, sockfd as usize, msg as usize, flags as usize);
    let signed = result as isize;
    if signed < 0 && signed >= -4096 {
        *crate::header::errno::__errno_location() = (-signed) as c_int;
        -1
    } else {
        result as ssize_t
    }
}
