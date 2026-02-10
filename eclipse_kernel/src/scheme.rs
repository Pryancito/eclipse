//! Scheme trait and registry implementation
//!
//! Inspired by Redox OS, this module provides a unified interface for 
//! all system resources (files, devices, pipes, etc.) via URL-like paths.

use alloc::vec::Vec;
use alloc::string::String;
use alloc::boxed::Box;
use spin::Mutex;
use crate::process::ProcessId;

use alloc::sync::Arc;

/// Error codes matching POSIX/Redox
pub mod error {
    pub const ENOENT: usize = 2;   // No such file or directory
    pub const EIO: usize = 5;      // I/O error
    pub const EBADF: usize = 9;    // Bad file descriptor
    pub const EINVAL: usize = 22;  // Invalid argument
    pub const ENOSYS: usize = 38;  // Function not implemented
}

/// Stat information for a resource
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
}

/// The Scheme trait defines the interface for all resource providers.
pub trait Scheme: Send + Sync {
    /// Open a resource at the given path
    fn open(&self, path: &str, flags: usize, mode: u32) -> Result<usize, usize>;

    /// Read data from a resource
    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize>;

    /// Write data to a resource
    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize>;

    /// Seek within a resource
    fn lseek(&self, id: usize, offset: isize, whence: usize) -> Result<usize, usize>;

    /// Close a resource
    fn close(&self, id: usize) -> Result<usize, usize>;

    /// Get information about a resource
    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize>;

    /// Map a resource into memory
    fn fmap(&self, _id: usize, _offset: usize, _len: usize) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }
}

/// Registry for all system schemes
struct Registry {
    schemes: Vec<(String, Arc<dyn Scheme>)>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    schemes: Vec::new(),
});

// --- Log Scheme ---

pub struct LogScheme;

impl Scheme for LogScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0) // Single resource for logging
    }

    fn read(&self, _id: usize, _buffer: &mut [u8]) -> Result<usize, usize> {
        Err(error::EIO) // Log is write-only
    }

    fn write(&self, _id: usize, buf: &[u8]) -> Result<usize, usize> {
        if let Ok(s) = core::str::from_utf8(buf) {
            crate::serial::serial_print(s);
            Ok(buf.len())
        } else {
            // Fallback for non-UTF8 logs
            for &b in buf {
                crate::serial::serial_print_char(b as char);
            }
            Ok(buf.len())
        }
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        Err(error::EIO) // Not seekable
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0) // Nothing to close
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Err(error::EIO) // No stat info
    }
}

pub fn init() {
    register_scheme("log", Arc::new(LogScheme));
}

/// Register a new scheme
pub fn register_scheme(name: &str, scheme: Arc<dyn Scheme>) {
    let mut reg = REGISTRY.lock();
    reg.schemes.push((String::from(name), scheme));
}

/// Open a path by routing to the appropriate scheme
pub fn open(path: &str, flags: usize, mode: u32) -> Result<(usize, usize), usize> {
    let mut parts = path.splitn(2, ':');
    let scheme_name = parts.next().ok_or(error::EINVAL)?;
    let relative_path = parts.next().unwrap_or("");

    let (i, scheme) = {
        let reg = REGISTRY.lock();
        let (i, (_, scheme)) = reg.schemes.iter().enumerate()
            .find(|(_, (name, _))| name == scheme_name)
            .ok_or(error::ENOENT)?;
        (i, Arc::clone(scheme))
    };

    match scheme.open(relative_path, flags, mode) {
        Ok(id) => Ok((i, id)),
        Err(e) => Err(e),
    }
}

/// Read from a resource in a specific scheme
pub fn read(scheme_idx: usize, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.read(id, buffer)
}

/// Write to a resource in a specific scheme
pub fn write(scheme_idx: usize, id: usize, buffer: &[u8]) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.write(id, buffer)
}

/// Seek in a resource in a specific scheme
pub fn lseek(scheme_idx: usize, id: usize, offset: isize, whence: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.lseek(id, offset, whence)
}

/// Close a resource in a specific scheme
pub fn close(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.close(id)
}

/// Map a resource in a specific scheme
pub fn fmap(scheme_idx: usize, id: usize, offset: usize, len: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.fmap(id, offset, len)
}
