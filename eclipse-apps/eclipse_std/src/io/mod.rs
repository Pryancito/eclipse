//! I/O Module - File and stream I/O using eclipse-libc
//!
//! Provides std-like I/O interfaces built on top of eclipse-libc's FILE streams.

use core::ptr;
use eclipse_libc::*;
use alloc::string::String;
use alloc::vec::Vec;

pub use self::stdio::{stdin, stdout, stderr, Stdin, Stdout, Stderr};

mod stdio;

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
    InvalidInput,
    Other,
}

impl Error {
    pub fn new(kind: ErrorKind) -> Self {
        Error { kind }
    }
    
    pub fn kind(&self) -> ErrorKind {
        self.kind
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
            Err(_) => Err(Error::new(ErrorKind::InvalidInput)),
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
                Ok(0) => return Err(Error::new(ErrorKind::Other)),
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

/// File handle wrapping eclipse-libc FILE*
pub struct File {
    ptr: *mut FILE,
    path: String,
}

impl File {
    /// Open a file for reading
    pub fn open(path: &str) -> Result<Self> {
        let mut c_path = Vec::from(path.as_bytes());
        c_path.push(0);
        
        let mode = b"r\0";
        
        unsafe {
            let ptr = fopen(c_path.as_ptr() as *const c_char, mode.as_ptr() as *const c_char);
            if ptr.is_null() {
                return Err(Error::new(ErrorKind::NotFound));
            }
            
            Ok(File {
                ptr,
                path: String::from(path),
            })
        }
    }
    
    /// Create a file for writing
    pub fn create(path: &str) -> Result<Self> {
        let mut c_path = Vec::from(path.as_bytes());
        c_path.push(0);
        
        let mode = b"w\0";
        
        unsafe {
            let ptr = fopen(c_path.as_ptr() as *const c_char, mode.as_ptr() as *const c_char);
            if ptr.is_null() {
                return Err(Error::new(ErrorKind::PermissionDenied));
            }
            
            Ok(File {
                ptr,
                path: String::from(path),
            })
        }
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        unsafe {
            let n = fread(
                buf.as_mut_ptr() as *mut c_void,
                1,
                buf.len(),
                self.ptr
            );
            Ok(n)
        }
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        unsafe {
            let n = fwrite(
                buf.as_ptr() as *const c_void,
                1,
                buf.len(),
                self.ptr
            );
            Ok(n)
        }
    }
    
    fn flush(&mut self) -> Result<()> {
        unsafe {
            fflush(self.ptr);
        }
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe {
            fclose(self.ptr);
        }
    }
}
