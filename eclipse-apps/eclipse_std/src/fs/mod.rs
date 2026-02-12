//! File System Module - File operations using eclipse-libc
//!
//! Provides std-like File interface built on top of eclipse-libc's fopen/fread/fwrite.

use eclipse_libc::*;
use ::alloc::string::String;
use ::alloc::vec::Vec;
use crate::io::{Read, Write, Result, Error, ErrorKind};

/// File handle wrapping eclipse-libc FILE*
pub struct File {
    ptr: *mut FILE,
    #[allow(dead_code)]
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

/// Metadata about a file (Stub)
pub struct Metadata {
    // TODO: Implement
}

impl Metadata {
    pub fn is_file(&self) -> bool { true }
    pub fn is_dir(&self) -> bool { false }
}

/// Read the entire contents of a file into a bytes vector
pub fn read(path: &str) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read the entire contents of a file into a string
pub fn read_to_string(path: &str) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

/// Write a slice as the entire contents of a file
pub fn write(path: &str, contents: &[u8]) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(contents)?;
    Ok(())
}
