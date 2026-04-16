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
use alloc::collections::BTreeMap;

/// Error codes matching POSIX/Redox
pub mod error {
    pub const ENOENT: usize = 2;   // No such file or directory
    pub const ESRCH: usize = 3;    // No such process
    pub const EIO: usize = 5;      // I/O error
    pub const EEXIST: usize = 17;  // File exists (e.g. O_CREAT | O_EXCL)
    pub const EBADF: usize = 9;   // Bad file descriptor
    pub const EAGAIN: usize = 11;  // Try again
    pub const EINVAL: usize = 22;  // Invalid argument
    pub const ESPIPE: usize = 29;  // Illegal seek
    pub const ENOSYS: usize = 38;  // Function not implemented
    pub const EFAULT: usize = 14;  // Bad address
    pub const EISCONN: usize = 106; // Transport endpoint is already connected
    pub const ENOTCONN: usize = 107; // Transport endpoint is not connected
    pub const ENOMEM: usize = 12;  // Out of memory
    pub const EACCES: usize = 13;  // Permission denied
    pub const EBUSY: usize = 16;   // Device or resource busy
    pub const EPIPE: usize = 32;   // Broken pipe
    pub const EAFNOSUPPORT: usize = 97; // Address family not supported
}

/// Polling event flags (Linux-compatible)
pub mod event {
    pub const POLLIN: usize = 0x001;
    pub const POLLPRI: usize = 0x002;
    pub const POLLOUT: usize = 0x004;
    pub const POLLERR: usize = 0x008;
    pub const POLLHUP: usize = 0x010;
    pub const POLLNVAL: usize = 0x020;
}

/// Stat information for a resource
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}


/// Get file status in a specific scheme
pub fn fstat(scheme_idx: usize, id: usize, stat: &mut Stat) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        if let Some((_, s)) = reg.schemes.get(scheme_idx) {
             Arc::clone(s)
        } else {
             return Err(error::EBADF);
        }
    };
    scheme.fstat(id, stat)
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

    /// Perform a device-specific control operation
    fn ioctl(&self, _id: usize, _request: usize, _arg: usize) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Create a directory
    fn mkdir(&self, _path: &str, _mode: u32) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Remove a file
    fn unlink(&self, _path: &str) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Rename within the same scheme (`old_rel` / `new_rel` como en `open`).
    fn rename(&self, _old_rel: &str, _new_rel: &str) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Change the size of a resource
    fn ftruncate(&self, _id: usize, _len: usize) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Notify the scheme that an existing handle has been inherited by a new process.
    /// Called when a file descriptor is duplicated via spawn_with_stdio or fork.
    /// Schemes that use reference counting (e.g. PtyScheme) override this to increment
    /// their internal ref count so that close() only tears down the resource when the
    /// last reference is gone.
    fn dup(&self, _id: usize) -> Result<usize, usize> {
        Ok(0) // default: no-op for schemes without ref counting
    }

    /// Create a fully independent copy of resource `id` for delivery via SCM_RIGHTS.
    /// Returns a new resource id that is independent of the original; closing either
    /// one does not affect the other.  Returns `Err(ENOSYS)` if not supported.
    fn dup_independent(&self, _id: usize) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Poll for events on a resource.
    /// `events` is a bitmask of events to check for (POLLIN, POLLOUT, etc.).
    /// Returns a bitmask of events that are currently active.
    fn poll(&self, _id: usize, events: usize) -> Result<usize, usize> {
        Ok(events) // Default: return requested events (always ready)
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
        // Log is write-only; read returns 0 (EOF) so stdin read(0) doesn't fail with EIO
        Ok(0)
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
    register_scheme("tty", Arc::new(crate::tty::TtyScheme::new()));
    register_scheme("shm", Arc::new(ShmScheme::new()));
    register_scheme("drm", Arc::new(crate::drm_scheme::DrmScheme));
    register_scheme("eth", Arc::new(crate::eth::EthScheme));
    register_scheme("pty", Arc::new(crate::pty::PtyScheme::new()));
    // El PipeScheme usa el singleton global PIPE_SCHEME; el proxy delega en él.
    register_scheme("pipe", Arc::new(PipeSchemeProxy));
    register_scheme("epoll", crate::epoll::get_epoll_scheme().clone());
    register_scheme("eventfd", crate::eventfd::get_eventfd_scheme().clone());
    register_scheme("signalfd", Arc::new(crate::signalfd::SignalfdScheme::new()));
}

/// Proxy sin estado que delega todas las operaciones en el singleton PIPE_SCHEME.
struct PipeSchemeProxy;

impl Scheme for PipeSchemeProxy {
    fn open(&self, path: &str, flags: usize, mode: u32) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.open(path, flags, mode)
    }
    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.read(id, buffer)
    }
    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.write(id, buffer)
    }
    fn lseek(&self, id: usize, offset: isize, whence: usize) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.lseek(id, offset, whence)
    }
    fn close(&self, id: usize) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.close(id)
    }
    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.fstat(id, stat)
    }
    fn poll(&self, id: usize, events: usize) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.poll_pipe(id, events)
    }
}

/// Devuelve el scheme_id (índice en el REGISTRY) para un scheme dado por nombre.
pub fn get_scheme_id(name: &str) -> Option<usize> {
    let reg = REGISTRY.lock();
    reg.schemes.iter().enumerate()
        .find(|(_, (n, _))| n.as_str() == name)
        .map(|(i, _)| i)
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

/// Poll a resource in a specific scheme for events.
pub fn poll(scheme_idx: usize, id: usize, events: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.poll(id, events)
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

/// Truncate or extend a resource in a specific scheme
pub fn ftruncate(scheme_idx: usize, id: usize, len: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.ftruncate(id, len)
}

/// Perform an ioctl on a resource in a specific scheme
pub fn ioctl(scheme_idx: usize, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.ioctl(id, request, arg)
}

/// Notify the scheme that a handle has been inherited by a new process (fd dup/fork).
/// Schemes with reference counting (e.g. PtyScheme) use this to track open references
/// so that close() only destroys the resource when the last holder releases it.
pub fn dup(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.dup(id)
}

/// Create an independent copy of a resource for SCM_RIGHTS delivery.
/// Returns a new resource id that is independent of the original (closing one
/// does not affect the other).  Falls back to the original id for schemes that
/// do not support independent duplication.
pub fn dup_independent(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    let scheme = {
        let reg = REGISTRY.lock();
        Arc::clone(&reg.schemes.get(scheme_idx).ok_or(error::EBADF)?.1)
    };
    scheme.dup_independent(id)
}

/// Create a directory by routing to the appropriate scheme
pub fn mkdir(path: &str, mode: u32) -> Result<usize, usize> {
    let mut parts = path.splitn(2, ':');
    let scheme_name = parts.next().ok_or(error::EINVAL)?;
    let relative_path = parts.next().unwrap_or("");

    let scheme = {
        let reg = REGISTRY.lock();
        let (_, scheme) = reg.schemes.iter()
            .find(|(name, _)| name == scheme_name)
            .ok_or(error::ENOENT)?;
        Arc::clone(scheme)
    };

    scheme.mkdir(relative_path, mode)
}

/// Remove a file by routing to the appropriate scheme
pub fn unlink(path: &str) -> Result<usize, usize> {
    let mut parts = path.splitn(2, ':');
    let scheme_name = parts.next().ok_or(error::EINVAL)?;
    let relative_path = parts.next().unwrap_or("");

    let scheme = {
        let reg = REGISTRY.lock();
        let (_, scheme) = reg.schemes.iter()
            .find(|(name, _)| name == scheme_name)
            .ok_or(error::ENOENT)?;
        Arc::clone(scheme)
    };

    scheme.unlink(relative_path)
}

/// Renombra dentro del mismo esquema (`file:/a` → `file:` + `/a`).
pub fn rename(old_path: &str, new_path: &str) -> Result<usize, usize> {
    let mut old_parts = old_path.splitn(2, ':');
    let old_scheme = old_parts.next().ok_or(error::EINVAL)?;
    let old_rel = old_parts.next().unwrap_or("");

    let mut new_parts = new_path.splitn(2, ':');
    let new_scheme = new_parts.next().ok_or(error::EINVAL)?;
    let new_rel = new_parts.next().unwrap_or("");

    if old_scheme != new_scheme {
        return Err(error::EINVAL);
    }

    let scheme = {
        let reg = REGISTRY.lock();
        let (_, scheme) = reg
            .schemes
            .iter()
            .find(|(name, _)| name == old_scheme)
            .ok_or(error::ENOENT)?;
        Arc::clone(scheme)
    };

    scheme.rename(old_rel, new_rel)
}

/// Get the path/name of a resource for directory listing purposes.
/// Returns the path if available (filesystem scheme stores inode info).
pub fn get_resource_path(scheme_idx: usize, resource_id: usize) -> Option<alloc::string::String> {
    // Only the filesystem scheme tracks path information via inodes.
    let scheme_name = {
        let reg = REGISTRY.lock();
        reg.schemes.get(scheme_idx).map(|(name, _)| name.clone())
    };
    if scheme_name.as_deref() == Some("file") {
        // Use the inode to reconstruct the path is complex; instead provide directory listing
        // via get_dir_children_by_resource which uses the resource_id directly.
        // Return a sentinel value that the caller can use to identify this as a FS resource.
        Some(alloc::format!("__fs_resource:{}", resource_id))
    } else {
        None
    }
}

// --- SHM Scheme ---

pub struct ShmRegion {
    pub phys_addr: u64,
    pub size: usize,
    pub ref_count: usize,
    pub unlinked: bool,
}

pub struct ShmScheme {
    regions: Mutex<BTreeMap<String, ShmRegion>>,
    handles: Mutex<Vec<Option<String>>>,
}

impl ShmScheme {
    pub fn new() -> Self {
        Self {
            regions: Mutex::new(BTreeMap::new()),
            handles: Mutex::new(Vec::new()),
        }
    }
}

impl Scheme for ShmScheme {
    fn open(&self, path: &str, flags: usize, _mode: u32) -> Result<usize, usize> {
        // Flags from POSIX / eclipse-syscall (O_RDWR is 0x02, etc.)
        const O_CREAT: usize = 0x0040;
        const O_EXCL: usize = 0x0080;
        const O_TRUNC: usize = 0x0200;

        let mut regions = self.regions.lock();
        let mut handles = self.handles.lock();

        let name = path.trim_start_matches('/');
        if name.is_empty() {
            return Err(error::EINVAL);
        }

        if (flags & O_CREAT) != 0 {
            if regions.contains_key(name) {
                if (flags & O_EXCL) != 0 {
                    return Err(error::EEXIST);
                }
                // If not O_EXCL, we just open it (TRUNC handled if specified, but usually not for SHM)
            } else {
                // Create new region. Size is usually determined by ftruncate, but for now 
                // we might need a way to specify initial size or just allocate on first write/mmap.
                // Wayland usually does: open(SHM_NAME, O_CREAT|O_RDWR) -> ftruncate(size) -> mmap.
                // Since our Scheme doesn't have ftruncate yet, we'll use a hack or just allocate 4MB by default
                // and allow resizing if we add a resize method.
                // Actually, let's just create an empty region and use write or a special ioctl to set size.
                // Or better: for Wayland SHM, the client creates a pool with a certain size.
                
                // For now, let's default to a reasonably large size (e.g. 16MB) to keep it simple
                // until we have ftruncate.
                let size: usize = 16 * 1024 * 1024; 
                
                #[cfg(not(test))]
                let phys_addr_opt = crate::memory::alloc_phys_frames_contig((size / 4096) as u64);
                #[cfg(test)]
                let phys_addr_opt = Some(0x1234000); // Dummy addr for tests

                if let Some(phys_addr) = phys_addr_opt {
                    // Zero the memory
                    #[cfg(not(test))]
                    {
                        let virt = crate::memory::PHYS_MEM_OFFSET + phys_addr;
                        unsafe { core::ptr::write_bytes(virt as *mut u8, 0, size); }
                    }
                    
                    regions.insert(String::from(name), ShmRegion {
                        phys_addr,
                        size,
                        ref_count: 0,
                        unlinked: false,
                    });
                } else {
                    return Err(error::EIO); // Out of memory
                }
            }
        }

        if let Some(region) = regions.get_mut(name) {
            region.ref_count += 1;
        } else {
            return Err(error::ENOENT);
        }

        // Find or create a handle
        for (i, handle) in handles.iter_mut().enumerate() {
            if handle.is_none() {
                *handle = Some(String::from(name));
                return Ok(i);
            }
        }

        let id = handles.len();
        handles.push(Some(String::from(name)));
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8]) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let regions = self.regions.lock();
        let region = regions.get(name).ok_or(error::EIO)?;

        // For SHM, read/write might be used, but mmap is preferred.
        // We'll implement a simple read at offset 0 (since we don't track offset per handle yet)
        let to_copy = core::cmp::min(buffer.len(), region.size);
        let virt = crate::memory::PHYS_MEM_OFFSET + region.phys_addr;
        unsafe {
            core::ptr::copy_nonoverlapping(virt as *const u8, buffer.as_mut_ptr(), to_copy);
        }
        Ok(to_copy)
    }

    fn write(&self, id: usize, buffer: &[u8]) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let regions = self.regions.lock();
        let region = regions.get(name).ok_or(error::EIO)?;

        let to_copy = core::cmp::min(buffer.len(), region.size);
        let virt = crate::memory::PHYS_MEM_OFFSET + region.phys_addr;
        unsafe {
            core::ptr::copy_nonoverlapping(buffer.as_ptr(), virt as *mut u8, to_copy);
        }
        Ok(to_copy)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize) -> Result<usize, usize> {
        // Shared memory objects are treated as non-seekable via this interface for now.
        // Callers should use mmap and explicit offsets instead of lseek on shm handles.
        Err(error::ESPIPE)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut handles = self.handles.lock();
        if id < handles.len() {
            if let Some(name) = handles[id].take() {
                let mut regions = self.regions.lock();
                if let Some(region) = regions.get_mut(&name) {
                    region.ref_count = region.ref_count.saturating_sub(1);
                    if region.ref_count == 0 && region.unlinked {
                        regions.remove(&name);
                    }
                }
                return Ok(0);
            }
        }
        Err(error::EBADF)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let regions = self.regions.lock();
        let region = regions.get(name).ok_or(error::EIO)?;

        stat.size = region.size as u64;
        stat.mode = 0o666 | 0x8000; // Regular file, readable/writable
        Ok(0)
    }

    fn fmap(&self, id: usize, offset: usize, len: usize) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let regions = self.regions.lock();
        let region = regions.get(name).ok_or(error::EIO)?;

        if offset + len > region.size {
            return Err(error::EINVAL);
        }

        Ok((region.phys_addr as usize) + offset)
    }

    fn unlink(&self, path: &str) -> Result<usize, usize> {
        let name = path.trim_start_matches('/');
        let mut regions = self.regions.lock();
        if let Some(region) = regions.get_mut(name) {
            region.unlinked = true;
            if region.ref_count == 0 {
                regions.remove(name);
            }
            Ok(0)
        } else {
            Err(error::ENOENT)
        }
    }

    fn ftruncate(&self, id: usize, len: usize) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let mut regions = self.regions.lock();
        let region = regions.get_mut(name).ok_or(error::EIO)?;

        // For now, our prototype allocator fixes the region to 16MB. 
        // We only allow logical truncation within this physical allocation.
        if len > 16 * 1024 * 1024 {
             return Err(error::EINVAL);
        }
        region.size = len;
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[test]
    fn test_shm_refcount_unlink() {
        let shm = ShmScheme::new();
        // 1. Open region "test"
        let fd1 = shm.open("test", 0x40, 0).expect("failed to open shm"); // O_CREAT
        {
            let regions = shm.regions.lock();
            let reg = regions.get("test").expect("region not found");
            assert_eq!(reg.ref_count, 1);
            assert_eq!(reg.unlinked, false);
        }

        // 2. Open region "test" again
        let fd2 = shm.open("test", 0, 0).expect("failed to open shm again");
        {
            let regions = shm.regions.lock();
            let reg = regions.get("test").expect("region not found");
            assert_eq!(reg.ref_count, 2);
        }

        // 3. Unlink "test"
        shm.unlink("test").expect("failed to unlink");
        {
            let regions = shm.regions.lock();
            let reg = regions.get("test").expect("region should still exist");
            assert_eq!(reg.unlinked, true);
            assert_eq!(reg.ref_count, 2);
        }

        // 4. Close first handle
        shm.close(fd1).expect("failed to close fd1");
        {
            let regions = shm.regions.lock();
            let reg = regions.get("test").expect("region should still exist");
            assert_eq!(reg.ref_count, 1);
        }

        // 5. Close second handle
        shm.close(fd2).expect("failed to close fd2");
        {
            let regions = shm.regions.lock();
            assert!(regions.get("test").is_none(), "region should be removed after last close");
        }
    }
}
