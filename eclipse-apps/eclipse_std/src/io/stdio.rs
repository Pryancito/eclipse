//! Standard I/O - stdin, stdout, stderr

use super::{Read, Write, Result, Error, ErrorKind};
use libc::*;

/// Standard input
pub struct Stdin {
    // Uses eclipse-libc stdin
}

/// Standard output
pub struct Stdout {
    // Uses eclipse-libc stdout
}

/// Standard error
pub struct Stderr {
    // Uses eclipse-libc stderr
}

/// Get standard input
pub fn stdin() -> Stdin {
    Stdin {}
}

/// Get standard output
pub fn stdout() -> Stdout {
    Stdout {}
}

/// Get standard error
pub fn stderr() -> Stderr {
    Stderr {}
}

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        unsafe {
            let n = libc::fread(
                buf.as_mut_ptr() as *mut c_void,
                1,
                buf.len(),
                libc::stdin
            );
            Ok(n)
        }
    }
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        unsafe {
            let n = libc::fwrite(
                buf.as_ptr() as *const c_void,
                1,
                buf.len(),
                libc::stdout
            );
            Ok(n)
        }
    }
    
    fn flush(&mut self) -> Result<()> {
        unsafe {
            libc::fflush(libc::stdout);
        }
        Ok(())
    }
}

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        unsafe {
            let n = libc::fwrite(
                buf.as_ptr() as *const c_void,
                1,
                buf.len(),
                libc::stderr
            );
            Ok(n)
        }
    }
    
    fn flush(&mut self) -> Result<()> {
        unsafe {
            libc::fflush(libc::stderr);
        }
        Ok(())
    }
}
