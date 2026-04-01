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
// We use raw syscall wrappers since the `libc` crate may not expose all
// symbols for the Eclipse OS target; these match the POSIX prototypes.

extern "C" {
    fn socket(domain: i32, type_: i32, protocol: i32) -> i32;
    fn bind(fd: i32, addr: *const SockaddrUn, addrlen: u32) -> i32;
    fn listen(fd: i32, backlog: i32) -> i32;
    fn accept(fd: i32, addr: *mut SockaddrUn, addrlen: *mut u32) -> i32;
    fn connect(fd: i32, addr: *const SockaddrUn, addrlen: u32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn unlink(path: *const u8) -> i32;
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
}

const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;
/// Linux/Eclipse `O_NONBLOCK` flag for `fcntl(F_SETFL)`.
const O_NONBLOCK: i32 = 0o4000;
const F_SETFL: i32 = 4;

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

        let mut written = 0;
        while written < len {
            let n = unsafe { write(self.fd, buf[written..len].as_ptr(), len - written) };
            if n <= 0 { return Err(SendError::IoError); }
            written += n as usize;
        }
        Ok(())
    }

    fn recv(&self) -> Result<(Vec<u8>, Vec<Handle>), RecvError> {
        let mut tmp = [0u8; 4096];
        let n = unsafe { read(self.fd, tmp.as_mut_ptr(), tmp.len()) };
        if n < 0 {
            // EAGAIN / EWOULDBLOCK on non-blocking socket → no data yet
            return Err(RecvError::IoError);
        }
        if n == 0 {
            // EOF — peer disconnected
            return Err(RecvError::IoError);
        }
        let mut buf = self.recv_buf.borrow_mut();
        buf.extend_from_slice(&tmp[..n as usize]);

        // Return whatever we have; the caller (WaylandServer / handshake loop)
        // will call recv again if it needs more data.
        let data = buf.clone();
        buf.clear();
        Ok((data, Vec::new()))
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
