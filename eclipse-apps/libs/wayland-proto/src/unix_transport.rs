//! Unix socket transport for standard Wayland compatibility.
//!
//! Provides `UnixSocketConnection` (implements `Connection`) and
//! `UnixSocketServer` (server-side accept loop) so that any standard
//! Wayland client (libwayland-client, SDL, GTK…) can connect to the
//! Eclipse OS compositor via `/tmp/wayland-0`.

use crate::wl::connection::{Connection, SendError, RecvError};
use crate::wl::wire::{ObjectId, Opcode, Payload, Handle, RawMessage};
use alloc::vec::Vec;
use core::cell::RefCell;
use smallvec::SmallVec;

/// Maximum single-message send buffer (4 KiB is enough for all Wayland messages).
const SEND_BUF_LEN: usize = 4096;

// ── libc bindings ────────────────────────────────────────────────────────────

extern "C" {
    fn socket(domain: i32, type_: i32, protocol: i32) -> i32;
    fn bind(fd: i32, addr: *const SockaddrUn, addrlen: u32) -> i32;
    fn listen(fd: i32, backlog: i32) -> i32;
    fn accept(fd: i32, addr: *mut SockaddrUn, addrlen: *mut u32) -> i32;
    fn connect(fd: i32, addr: *const SockaddrUn, addrlen: u32) -> i32;
    fn close(fd: i32) -> i32;
    fn unlink(path: *const u8) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
    fn recvmsg(fd: i32, msg: *mut MsgHdr, flags: i32) -> isize;
    /// POSIX `errno` — only valid immediately after a syscall returns -1.
    fn __errno_location() -> *mut i32;
}

/// Read the current `errno` value.
#[inline(always)]
fn errno() -> i32 {
    unsafe { *__errno_location() }
}

const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
const O_NONBLOCK: i32 = 0o4000;
const F_SETFL: i32 = 4;
/// `SOL_SOCKET` level for `cmsghdr`.
const SOL_SOCKET: i32 = 1;
/// `SCM_RIGHTS` — pass file descriptors as ancillary data.
const SCM_RIGHTS: i32 = 1;
/// EAGAIN / EWOULDBLOCK — non-blocking socket has no data.
const EAGAIN: i32 = 11;
const EWOULDBLOCK: i32 = 11; // same value on Linux

// ── POSIX structs for sendmsg/recvmsg ─────────────────────────────────────────

#[repr(C)]
struct IoVec {
    iov_base: *mut u8,
    iov_len:  usize,
}

#[repr(C)]
struct MsgHdr {
    msg_name:       *mut u8,
    msg_namelen:    u32,
    msg_iov:        *mut IoVec,
    msg_iovlen:     usize,
    msg_control:    *mut u8,
    msg_controllen: usize,
    msg_flags:      i32,
}

/// Control-message header followed by `cmsg_len - sizeof(CmsgHdr)` bytes of data.
#[repr(C)]
struct CmsgHdr {
    cmsg_len:   usize,
    cmsg_level: i32,
    cmsg_type:  i32,
    // fd data follows immediately after this header
}

/// Align `n` up to pointer size (required for `CMSG_NXTHDR` arithmetic).
#[inline(always)]
const fn cmsg_align(n: usize) -> usize {
    (n + core::mem::size_of::<usize>() - 1) & !(core::mem::size_of::<usize>() - 1)
}

/// Size of control buffer needed to pass `n` file descriptors.
#[inline(always)]
const fn cmsg_space(n: usize) -> usize {
    cmsg_align(core::mem::size_of::<CmsgHdr>()) + cmsg_align(n * core::mem::size_of::<i32>())
}

/// Write `fds` into a cmsg control buffer; returns the number of bytes written.
fn encode_fds(fds: &[i32], buf: &mut [u8]) -> usize {
    if fds.is_empty() { return 0; }
    let hdr_size = cmsg_align(core::mem::size_of::<CmsgHdr>());
    let data_size = fds.len() * core::mem::size_of::<i32>();
    let total = hdr_size + data_size;
    if buf.len() < total { return 0; }

    let hdr = CmsgHdr {
        cmsg_len:   total,
        cmsg_level: SOL_SOCKET,
        cmsg_type:  SCM_RIGHTS,
    };
    unsafe {
        core::ptr::write_unaligned(buf.as_mut_ptr() as *mut CmsgHdr, hdr);
        let data_ptr = buf.as_mut_ptr().add(hdr_size) as *mut i32;
        for (i, &fd) in fds.iter().enumerate() {
            core::ptr::write_unaligned(data_ptr.add(i), fd);
        }
    }
    total
}

/// Read file descriptors from a received control buffer.
fn decode_fds(buf: &[u8], cmsg_len: usize) -> alloc::vec::Vec<i32> {
    let mut fds = alloc::vec::Vec::new();
    let hdr_size = cmsg_align(core::mem::size_of::<CmsgHdr>());
    if cmsg_len < hdr_size { return fds; }
    let hdr = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const CmsgHdr) };
    if hdr.cmsg_level != SOL_SOCKET || hdr.cmsg_type != SCM_RIGHTS { return fds; }
    let data_len = hdr.cmsg_len.saturating_sub(hdr_size);
    let n = data_len / core::mem::size_of::<i32>();
    let data_ptr = unsafe { buf.as_ptr().add(hdr_size) as *const i32 };
    for i in 0..n {
        fds.push(unsafe { core::ptr::read_unaligned(data_ptr.add(i)) });
    }
    fds
}

// ── sockaddr_un ───────────────────────────────────────────────────────────────

#[repr(C)]
struct SockaddrUn {
    sun_family: u16,
    sun_path: [u8; 108],
}

impl SockaddrUn {
    fn new(path: &str) -> (Self, u32) {
        let mut addr = SockaddrUn { sun_family: AF_UNIX as u16, sun_path: [0; 108] };
        let bytes = path.as_bytes();
        let len = bytes.len().min(107);
        addr.sun_path[..len].copy_from_slice(&bytes[..len]);
        // addrlen = sizeof(sun_family) + actual path bytes + null terminator
        let addrlen = (2 + len + 1) as u32;
        (addr, addrlen)
    }
}

// ── UnixSocketConnection ─────────────────────────────────────────────────────

/// A `Connection` backed by a Unix domain socket file descriptor.
///
/// Used both by clients (created via [`connect`]) and by the server for each
/// accepted connection (created via [`from_fd`]).
pub struct UnixSocketConnection {
    fd: i32,
    /// Accumulates partial Wayland messages across `recv` calls.
    recv_buf: RefCell<alloc::vec::Vec<u8>>,
}

impl UnixSocketConnection {
    /// Connect to a Wayland compositor socket at `path` (e.g. `/tmp/wayland-0`).
    pub fn connect(path: &str) -> Option<Self> {
        let fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
        if fd < 0 { return None; }

        let (addr, addrlen) = SockaddrUn::new(path);
        if unsafe { connect(fd, &addr, addrlen) } < 0 {
            unsafe { close(fd) };
            return None;
        }
        Some(Self { fd, recv_buf: RefCell::new(alloc::vec::Vec::new()) })
    }

    /// Wrap an already-accepted file descriptor (server-side use).
    pub fn from_fd(fd: i32) -> Self {
        Self { fd, recv_buf: RefCell::new(alloc::vec::Vec::new()) }
    }

    /// Raw file descriptor (needed by the server accept loop).
    pub fn fd(&self) -> i32 { self.fd }

    /// Set this socket non-blocking.
    pub fn set_nonblocking(&self) {
        // Ignore errors: a blocking socket is sub-optimal but not fatal.
        unsafe { fcntl(self.fd, F_SETFL, O_NONBLOCK) };
    }
}

impl Connection for UnixSocketConnection {
    fn send(&self, sender: ObjectId, opcode: Opcode, args: &[Payload], handles: &[Handle]) -> Result<(), SendError> {
        let mut h_vec: Vec<Handle> = handles.to_vec();
        let raw = RawMessage {
            sender,
            opcode,
            args: SmallVec::from_iter(args.iter().cloned()),
        };
        let mut buf = [0u8; SEND_BUF_LEN];
        let len = raw.serialize(&mut buf, &mut h_vec).map_err(|_| SendError::IoError)?;

        // Build list of raw fds from Handle list.
        let fds: alloc::vec::Vec<i32> = h_vec.iter().map(|h| h.0).collect();

        if fds.is_empty() {
            // No ancillary data — simple sendmsg with no control message.
            let mut iov = IoVec { iov_base: buf.as_ptr() as *mut u8, iov_len: len };
            let msg = MsgHdr {
                msg_name: core::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: core::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            };
            let sent = unsafe { sendmsg(self.fd, &msg, 0) };
            if sent < 0 { return Err(SendError::IoError); }
        } else {
            // Ancillary SCM_RIGHTS data carrying the file descriptors.
            const MAX_CTRL: usize = cmsg_space(8); // up to 8 fds per message
            let mut ctrl_buf = [0u8; MAX_CTRL];
            let ctrl_len = encode_fds(&fds, &mut ctrl_buf);

            let mut iov = IoVec { iov_base: buf.as_ptr() as *mut u8, iov_len: len };
            let msg = MsgHdr {
                msg_name: core::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: ctrl_buf.as_mut_ptr(),
                msg_controllen: ctrl_len,
                msg_flags: 0,
            };
            let sent = unsafe { sendmsg(self.fd, &msg, 0) };
            if sent < 0 { return Err(SendError::IoError); }
        }
        Ok(())
    }

    fn recv(&self) -> Result<(Vec<u8>, Vec<Handle>), RecvError> {
        let mut data_buf = [0u8; 4096];
        const MAX_CTRL: usize = cmsg_space(8);
        let mut ctrl_buf = [0u8; MAX_CTRL];

        let mut iov = IoVec { iov_base: data_buf.as_mut_ptr(), iov_len: data_buf.len() };
        let mut msg = MsgHdr {
            msg_name: core::ptr::null_mut(),
            msg_namelen: 0,
            msg_iov: &mut iov,
            msg_iovlen: 1,
            msg_control: ctrl_buf.as_mut_ptr(),
            msg_controllen: MAX_CTRL,
            msg_flags: 0,
        };

        let n = unsafe { recvmsg(self.fd, &mut msg, 0) };
        if n <= 0 {
            // Eclipse OS non-blocking sockets may return 0 or negative values
            // (instead of -1+EAGAIN) when no data is available.  Treat all
            // such cases as WouldBlock to prevent false client disconnects.
            // Real disconnects are detected via process-level checks.
            let err = errno();
            if n < 0 && err != EAGAIN && err != EWOULDBLOCK && err != 0 {
                // Genuine I/O error (e.g., ECONNRESET, EPIPE) — real disconnect.
                return Err(RecvError::IoError);
            }
            return Err(RecvError::WouldBlock);
        }

        let mut buf = self.recv_buf.borrow_mut();
        buf.extend_from_slice(&data_buf[..n as usize]);

        // Decode any received file descriptors from ancillary data.
        let handles: Vec<Handle> = if msg.msg_controllen > 0 {
            decode_fds(&ctrl_buf[..msg.msg_controllen], msg.msg_controllen)
                .into_iter().map(Handle).collect()
        } else {
            Vec::new()
        };

        let data = core::mem::take(&mut *buf);
        Ok((data, handles))
    }
}

impl Drop for UnixSocketConnection {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { close(self.fd) };
        }
    }
}

// ── UnixSocketServer ─────────────────────────────────────────────────────────

/// Listens for incoming standard Wayland client connections on a Unix socket.
///
/// The server is non-blocking: `accept_nonblocking` returns immediately with
/// `None` if no client is waiting.
pub struct UnixSocketServer {
    fd: i32,
    path: alloc::string::String,
}

impl UnixSocketServer {
    /// Create, bind and listen on `path`.  Removes any stale socket file first.
    pub fn new(path: &str) -> Option<Self> {
        // Remove stale socket file (ignore errors)
        let mut pb = [0u8; 120];
        let bytes = path.as_bytes();
        pb[..bytes.len().min(119)].copy_from_slice(&bytes[..bytes.len().min(119)]);
        unsafe { unlink(pb.as_ptr()) };

        let fd = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
        if fd < 0 { return None; }

        let (addr, addrlen) = SockaddrUn::new(path);
        if unsafe { bind(fd, &addr, addrlen) } < 0 {
            unsafe { close(fd) };
            return None;
        }
        if unsafe { listen(fd, 32) } < 0 {
            unsafe { close(fd) };
            return None;
        }
        // Non-blocking so the main loop doesn't stall on accept
        unsafe { fcntl(fd, F_SETFL, O_NONBLOCK) };

        Some(Self { fd, path: alloc::string::String::from(path) })
    }

    /// Accept one pending client connection without blocking.
    ///
    /// Returns `None` when no client is queued.
    pub fn accept_nonblocking(&self) -> Option<UnixSocketConnection> {
        let mut addr = SockaddrUn { sun_family: 0, sun_path: [0; 108] };
        let mut addrlen = core::mem::size_of::<SockaddrUn>() as u32;
        let client_fd = unsafe { accept(self.fd, &mut addr, &mut addrlen) };
        if client_fd < 0 { return None; }
        // Also set the client socket non-blocking
        let conn = UnixSocketConnection::from_fd(client_fd);
        conn.set_nonblocking();
        Some(conn)
    }

    /// File descriptor of the listening socket (useful for poll/select).
    pub fn fd(&self) -> i32 { self.fd }
}

impl Drop for UnixSocketServer {
    fn drop(&mut self) {
        unsafe {
            let mut pb = [0u8; 120];
            pb[..self.path.len().min(119)].copy_from_slice(self.path.as_bytes());
            unlink(pb.as_ptr());
            close(self.fd);
        }
    }
}
