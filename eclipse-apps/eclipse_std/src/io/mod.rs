//! I/O Module - File and stream I/O using eclipse-libc
//!
//! Provides std-like I/O interfaces built on top of eclipse-libc's FILE streams.
use core::prelude::v1::*;
use ::alloc::string::String;
use ::alloc::vec::Vec;

pub use self::stdio::{stdin, stdout, stderr, Stdin, Stdout, Stderr};

mod stdio;

/// A buffer type used with `writev` and `write_vectored`.
///
/// This type is ABI-compatible with `libc::iovec`.
#[repr(transparent)]
pub struct IoSlice<'a> {
    vec: crate::libc::iovec,
    _p: core::marker::PhantomData<&'a [u8]>,
}

impl<'a> IoSlice<'a> {
    /// Create a new `IoSlice` wrapping a byte slice.
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        IoSlice {
            vec: crate::libc::iovec {
                iov_base: buf.as_ptr() as *mut _,
                iov_len: buf.len(),
            },
            _p: core::marker::PhantomData,
        }
    }

    /// Return the number of bytes this slice contains.
    #[inline]
    pub fn len(&self) -> usize {
        self.vec.iov_len
    }

    /// Return `true` if this slice is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.vec.iov_len == 0
    }
}

impl<'a> core::ops::Deref for IoSlice<'a> {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vec.iov_base as *const u8, self.vec.iov_len) }
    }
}

/// A mutable buffer type used with `readv` and `read_vectored`.
///
/// This type is ABI-compatible with `libc::iovec`.
#[repr(transparent)]
pub struct IoSliceMut<'a> {
    vec: crate::libc::iovec,
    _p: core::marker::PhantomData<&'a mut [u8]>,
}

impl<'a> IoSliceMut<'a> {
    /// Create a new `IoSliceMut` wrapping a mutable byte slice.
    #[inline]
    pub fn new(buf: &'a mut [u8]) -> Self {
        IoSliceMut {
            vec: crate::libc::iovec {
                iov_base: buf.as_mut_ptr() as *mut _,
                iov_len: buf.len(),
            },
            _p: core::marker::PhantomData,
        }
    }

    /// Return the number of bytes this slice contains.
    #[inline]
    pub fn len(&self) -> usize {
        self.vec.iov_len
    }

    /// Return `true` if this slice is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.vec.iov_len == 0
    }
}

impl<'a> core::ops::Deref for IoSliceMut<'a> {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.vec.iov_base as *const u8, self.vec.iov_len) }
    }
}

impl<'a> core::ops::DerefMut for IoSliceMut<'a> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.vec.iov_base as *mut u8, self.vec.iov_len) }
    }
}

/// Result type for I/O operations
pub type Result<T> = core::result::Result<T, Error>;

/// I/O Error type
#[derive(Debug, Clone, Copy)]
pub struct Error {
    pub kind: ErrorKind,
}

/// Error kind for I/O operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    InvalidInput,
    InvalidData,
    TimedOut,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
}

impl Error {
    pub fn new<E>(kind: ErrorKind, _error: E) -> Self {
        Error { kind }
    }
    
    pub fn from_raw_os_error(_code: i32) -> Self {
        // Simple mapping for now
        Error { kind: ErrorKind::Other }
    }
    
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.kind)
    }
}

impl crate::error::Error for Error {}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Error {
        Error { kind }
    }
}

/// Read trait for reading bytes
pub trait Read {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
    
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        let mut total = 0;
        let mut chunk = [0u8; 4096];
        
        loop {
            match self.read(&mut chunk) {
                Ok(0) => return Ok(total),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    total += n;
                }
                Err(e) => return Err(e),
            }
        }
    }
    
    fn read_to_string(&mut self, buf: &mut String) -> Result<usize> {
        let mut bytes = Vec::new();
        let n = self.read_to_end(&mut bytes)?;
        
        match core::str::from_utf8(&bytes) {
            Ok(s) => {
                buf.push_str(s);
                Ok(n)
            }
            Err(_) => Err(Error::new(ErrorKind::InvalidInput, "invalid utf-8")),
        }
    }
}

/// Write trait for writing bytes
pub trait Write {
    fn write(&mut self, buf: &[u8]) -> Result<usize>;
    
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        let mut total = 0;
        
        while total < buf.len() {
            match self.write(&buf[total..]) {
                Ok(0) => return Err(Error::new(ErrorKind::Other, "zero-length write")),
                Ok(n) => total += n,
                Err(e) => return Err(e),
            }
        }
        
        Ok(())
    }
    
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
