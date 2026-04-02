//! File System Module - File operations using eclipse-libc
//!
//! Provides std-like File interface built on top of eclipse-libc's fopen/fread/fwrite.
use crate::libc::*;
use core::prelude::v1::*;

use ::alloc::string::String;
use ::alloc::vec::Vec;
use crate::io::{Read, Write, Result, Error, ErrorKind};

/// Options for opening files
#[derive(Debug, Clone, Copy)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
        }
    }

    pub fn read(&mut self, read: bool) -> &mut Self { self.read = read; self }
    pub fn write(&mut self, write: bool) -> &mut Self { self.write = write; self }
    pub fn append(&mut self, append: bool) -> &mut Self { self.append = append; self }
    pub fn truncate(&mut self, truncate: bool) -> &mut Self { self.truncate = truncate; self }
    pub fn create(&mut self, create: bool) -> &mut Self { self.create = create; self }

    pub fn open(&self, path: &str) -> Result<File> {
        let mut c_path = Vec::from(path.as_bytes());
        c_path.push(0);

        // Determine mode string
        let mode: &[u8] = if self.append {
            if self.read { b"a+\0" } else { b"a\0" }
        } else if self.write {
            if self.truncate {
                if self.read { b"w+\0" } else { b"w\0" }
            } else {
                if self.read { b"r+\0" } else { b"r\0" }
            }
        } else {
            b"r\0"
        };

        unsafe {
            let ptr = crate::libc::fopen(c_path.as_ptr() as *const c_char, mode.as_ptr() as *const c_char);
            if ptr.is_null() {
                return Err(Error::new(ErrorKind::Other, "could not open file"));
            }
            Ok(File {
                ptr,
                path: String::from(path),
            })
        }
    }
}

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
            let ptr = crate::libc::fopen(c_path.as_ptr() as *const c_char, mode.as_ptr() as *const c_char);
            if ptr.is_null() {
                return Err(Error::new(ErrorKind::NotFound, "file not found"));
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
            let ptr = crate::libc::fopen(c_path.as_ptr() as *const c_char, mode.as_ptr() as *const c_char);
            if ptr.is_null() {
                return Err(Error::new(ErrorKind::PermissionDenied, "permission denied"));
            }
            
            Ok(File {
                ptr,
                path: String::from(path),
            })
        }
    }

    pub fn as_raw_fd(&self) -> crate::os::unix::io::RawFd {
        unsafe { crate::libc::fileno(self.ptr) }
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }
}

impl crate::os::unix::io::AsRawFd for File {
    fn as_raw_fd(&self) -> crate::os::unix::io::RawFd {
        self.as_raw_fd()
    }
}

impl crate::os::unix::io::AsFd for File {
    fn as_fd(&self) -> crate::os::unix::io::BorrowedFd<'_> {
        unsafe { crate::os::unix::io::BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        unsafe {
            let n = crate::libc::fread(
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
            let n = crate::libc::fwrite(
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
            crate::libc::fflush(self.ptr);
        }
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe {
            crate::libc::fclose(self.ptr);
        }
    }
}

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Metadata from `stat(2)` vía eclipse-relibc.
pub struct Metadata {
    mode: u32,
    len: u64,
}

impl Metadata {
    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn len(&self) -> u64 {
        self.len
    }
}

/// `stat` para una ruta.
pub fn metadata(path: &str) -> Result<Metadata> {
    let mut st = eclipse_syscall::call::Stat::default();
    eclipse_syscall::call::fstat_at(0, path, &mut st, 0)
        .map_err(|_| Error::new(ErrorKind::Other, "stat failed"))?;
    Ok(Metadata {
        mode: st.mode,
        len: st.size,
    })
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
