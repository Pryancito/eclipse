//! Unix domain socket support for Eclipse OS
//!
//! Provides `UnixStream` compatible with `std::os::unix::net::UnixStream`,
//! used by x11rb's `DefaultStream` to connect to the X11 server.

use crate::libc;
use crate::os::unix::io::{AsRawFd, IntoRawFd, OwnedFd, AsFd, BorrowedFd, RawFd, FromRawFd};
use crate::io::{self, Read, Write};
use crate::path::Path;

/// A Unix domain stream socket.
pub struct UnixStream {
    fd: OwnedFd,
}

impl UnixStream {
    /// Connect to the socket at `path`.
    pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path_str = path.as_ref().to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8")
        })?;

        // Build a null-terminated path for the sockaddr_un
        let path_bytes = path_str.as_bytes();
        if path_bytes.len() >= 108 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
        }

        // Create the socket
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX as _,
                libc::SOCK_STREAM as _,
                0,
            )
        };
        if fd < 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "socket() failed"));
        }

        // Build sockaddr_un
        let mut addr: libc::sockaddr_un = unsafe { core::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        for (i, &b) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = b as libc::c_char;
        }
        // Null-terminate (already zeroed, just be explicit)
        addr.sun_path[path_bytes.len()] = 0;

        let addr_len = (core::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len() + 1)
            as libc::socklen_t;

        let ret = unsafe {
            libc::connect(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                addr_len,
            )
        };

        if ret != 0 {
            unsafe { libc::close(fd) };
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "connect() failed"));
        }

        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Set the socket to non-blocking (or blocking) mode.
    /// On Eclipse OS, fcntl is a stub, so this is a best-effort operation.
    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        // fcntl is a stub on Eclipse OS; non-blocking mode is not yet enforced.
        // Return Ok to allow x11rb to proceed.
        Ok(())
    }

    /// Returns the raw file descriptor.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl AsFd for UnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl IntoRawFd for UnixStream {
    fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

impl From<UnixStream> for OwnedFd {
    fn from(s: UnixStream) -> OwnedFd {
        s.fd
    }
}

impl Read for UnixStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if ret < 0 {
            Err(io::Error::new(io::ErrorKind::Other, "recv() failed"))
        } else {
            Ok(ret as usize)
        }
    }
}

impl Write for UnixStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let ret = unsafe {
            libc::send(
                self.fd.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
                0,
            )
        };
        if ret < 0 {
            Err(io::Error::new(io::ErrorKind::Other, "send() failed"))
        } else {
            Ok(ret as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
