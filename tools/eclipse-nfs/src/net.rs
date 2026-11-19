//! The connection to the NFS server: a TCP socket of this process, from a
//! reserved port when it may have one.

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::Instant;

use nfs3::client::{RpcTransport, TransportError};
use nfs3::connect::{Connector, CONNECT_TIMEOUT_MS};
use nfs3::errno::Errno;

/// The reserved ports tried, from the top: the range Linux's `resvport`
/// takes (`sunrpc.min_resvport` to `max_resvport`).
const RESERVED: std::ops::RangeInclusive<u16> = 665..=1023;

/// The longest a [`RpcTransport::recv`] waits for bytes, in milliseconds:
/// it returns as soon as some arrive, so this only bounds how often the
/// client looks at its clock while it waits.
const POLL_MS: u64 = 10;

/// A socket to the server, as the client wants one: `recv` that waits a
/// little and no more, and `send` that sends everything.
pub struct Transport {
    sock: TcpStream,
    started: Instant,
    /// The connection went down, or the client gave it up: every call on
    /// it from now on fails, and only a new one helps.
    lost: bool,
}

impl Transport {
    /// Whether the connection is gone.
    pub fn is_lost(&self) -> bool {
        self.lost
    }
}

impl RpcTransport for Transport {
    fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let sent = self.sock.write_all(data).map_err(|e| match e.kind() {
            ErrorKind::ConnectionReset => TransportError::Reset,
            _ => TransportError::Closed,
        });
        self.lost |= sent.is_err();
        sent
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let got = match self.sock.read(buf) {
            Ok(0) => Err(TransportError::Closed),
            Ok(n) => Ok(n),
            // The wait of `SO_RCVTIMEO` ran out: nothing yet.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(0),
            Err(e) if e.kind() == ErrorKind::Interrupted => Ok(0),
            Err(e) if e.kind() == ErrorKind::ConnectionReset => Err(TransportError::Reset),
            Err(_) => Err(TransportError::Closed),
        };
        self.lost |= got.is_err();
        got
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn poll(&mut self) {
        // The wait is in `recv`, which blocks for up to `POLL_MS` with
        // `SO_RCVTIMEO`: one system call per look and not two, and every
        // call on a socket of the network process is a trip to it.
    }

    fn give_up(&mut self) {
        self.lost = true;
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

/// Connections to one address.
pub struct Dial {
    addr: IpAddr,
    started: Instant,
    /// Whether a reserved port may be had: root may bind one, and a server
    /// that exports `secure` (Linux's default) answers nobody else.
    reserved: bool,
    /// The reserved port taken last; the next is below it.
    next: u16,
}

impl Dial {
    /// Connections to `addr`.
    pub fn new(addr: IpAddr) -> Self {
        Self {
            addr,
            started: Instant::now(),
            // SAFETY: no pointers.
            reserved: unsafe { libc::geteuid() } == 0,
            // Each process starts somewhere else in the range, so the
            // server after one that died does not take its ports first.
            next: *RESERVED.end() + 1 - (std::process::id() % 64) as u16,
        }
    }
}

/// `errno` of the last call that failed.
fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// A TCP socket of `addr`'s family.
fn socket(addr: &SocketAddr) -> Result<std::os::fd::OwnedFd, Errno> {
    let family = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    // SAFETY: no pointers.
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(Errno(last_errno()));
    }
    // SAFETY: a descriptor just made and owned here alone.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Bind `fd` to the local `port` of `addr`'s family, with `SO_REUSEADDR`:
/// a port a connection of this machine closed a moment ago waits out
/// `TIME_WAIT`, and taking it again is what that option is for.
fn bind(fd: &std::os::fd::OwnedFd, addr: &SocketAddr, port: u16) -> Result<(), Errno> {
    let one: libc::c_int = 1;
    // SAFETY: an int option, of its size.
    unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    let local = match addr {
        SocketAddr::V4(_) => SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), port),
        SocketAddr::V6(_) => SocketAddr::new(std::net::Ipv6Addr::UNSPECIFIED.into(), port),
    };
    let (storage, len) = sockaddr(&local);
    // SAFETY: an address of its length.
    match unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &storage as *const _ as *const libc::sockaddr,
            len,
        )
    } {
        0 => Ok(()),
        _ => Err(Errno(last_errno())),
    }
}

/// `connect(2)` of `fd` to `addr`, waiting at most `timeout_ms`.
fn connect(fd: &std::os::fd::OwnedFd, addr: &SocketAddr, timeout_ms: u64) -> Result<(), Errno> {
    let raw = fd.as_raw_fd();
    // SAFETY: no pointers.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let (storage, len) = sockaddr(addr);
    // SAFETY: an address of its length.
    let r = unsafe { libc::connect(raw, &storage as *const _ as *const libc::sockaddr, len) };
    if r != 0 {
        let e = last_errno();
        if e != libc::EINPROGRESS {
            return Err(Errno(e));
        }
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ms = timeout_ms.min(i32::MAX as u64) as i32;
        // SAFETY: one `pollfd`.
        let n = unsafe { libc::poll(&mut pfd, 1, ms.max(1)) };
        if n == 0 {
            return Err(Errno::ETIMEDOUT);
        }
        let mut err: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: an int option, of its size.
        unsafe {
            libc::getsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if err != 0 {
            return Err(Errno(err));
        }
    }
    Ok(())
}

/// `addr` as the C library has it.
fn sockaddr(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero `sockaddr_storage` is a valid one.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            // SAFETY: `sockaddr_storage` holds any address.
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = a.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            // SAFETY: as above.
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = a.port().to_be();
            sin6.sin6_addr.s6_addr = a.ip().octets();
            sin6.sin6_scope_id = a.scope_id();
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, len as libc::socklen_t)
}

impl Connector for Dial {
    type Transport = Transport;

    fn connect(&mut self, port: u16, timeout_ms: u64) -> Result<Transport, Errno> {
        let addr = SocketAddr::new(self.addr, port);
        let timeout_ms = timeout_ms.min(CONNECT_TIMEOUT_MS);
        // A reserved port, from where the last one was taken. One in use
        // is `EADDRINUSE` at the bind, or `EADDRNOTAVAIL` at the connect
        // when the same pair of ends is still waiting out `TIME_WAIT`, and
        // the next is tried. Not root after all, or none left: any port, and
        // a server that wants a reserved one says so.
        let mut fd = None;
        if self.reserved {
            let span = (*RESERVED.end() - *RESERVED.start()) as usize + 1;
            for _ in 0..span {
                self.next = if self.next <= *RESERVED.start() {
                    *RESERVED.end()
                } else {
                    self.next - 1
                };
                let sock = socket(&addr)?;
                match bind(&sock, &addr, self.next) {
                    Ok(()) => {}
                    Err(Errno(libc::EADDRINUSE)) => continue,
                    Err(_) => break,
                }
                match connect(&sock, &addr, timeout_ms) {
                    Ok(()) => {
                        fd = Some(sock);
                        break;
                    }
                    Err(Errno(libc::EADDRNOTAVAIL | libc::EADDRINUSE)) => continue,
                    Err(e) => return Err(e),
                }
            }
        }
        let fd = match fd {
            Some(fd) => fd,
            None => {
                let sock = socket(&addr)?;
                connect(&sock, &addr, timeout_ms)?;
                sock
            }
        };
        let sock = TcpStream::from(fd);
        sock.set_nodelay(true).map_err(|_| Errno::EIO)?;
        sock.set_nonblocking(false).map_err(|_| Errno::EIO)?;
        sock.set_read_timeout(Some(std::time::Duration::from_millis(POLL_MS)))
            .map_err(|_| Errno::EIO)?;
        Ok(Transport {
            sock,
            started: Instant::now(),
            lost: false,
        })
    }

    fn now_ns(&self) -> u64 {
        self.started.elapsed().as_nanos() as u64
    }

    /// From the time of day and not [`now_ns`](Self::now_ns), which starts
    /// at zero in every process: the server after one that died may be
    /// given the same reserved port, and must not start at the ids its
    /// replies still carry.
    fn first_xid(&self) -> u32 {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        since.as_nanos() as u32 ^ std::process::id().rotate_left(16)
    }
}
