//! I/O Module - File and stream I/O using eclipse-libc
//!
//! Provides std-like I/O interfaces built on top of eclipse-libc's FILE streams.

use core::ptr;
use eclipse_libc::*;
use ::alloc::string::String;
use ::alloc::vec::Vec;

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
