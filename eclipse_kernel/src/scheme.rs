//! Scheme trait and registry implementation
//!
//! Inspired by Redox OS, this module provides a unified interface for 
//! all system resources (files, devices, pipes, etc.) via URL-like paths.

use alloc::vec::Vec;
use alloc::string::String;
use spin::Mutex;

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
    pub const EPERM:  usize = 1;   // Operation not permitted
    pub const EBUSY: usize = 16;   // Device or resource busy
    pub const EPIPE: usize = 32;   // Broken pipe
    pub const EAFNOSUPPORT: usize = 97; // Address family not supported
    pub const ENOTDIR: usize = 20; // Not a directory
    pub const EROFS: usize = 30;   // Read-only file system
    pub const ENODEV: usize = 19;  // No such device
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

/// Tamaño máximo de región SHM (creación por defecto y `ftruncate`).
/// Debe ser ≤ [`crate::memory::MAX_KERNEL_DMA_HEAP_ALLOC`]; ver `invariants`.
pub const SHM_REGION_MAX_BYTES: usize = 16 * 1024 * 1024;

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
    pub rdev: u64,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}


/// Dispatch routing functions for the global scheme registry
fn get_scheme(idx: usize) -> Result<Arc<dyn Scheme>, usize> {
    let reg = REGISTRY.lock();
    reg.schemes.get(idx).map(|(_, s)| Arc::clone(s)).ok_or(error::EBADF)
}

pub fn read(scheme_idx: usize, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.read(id, buffer, offset)
}

pub fn write(scheme_idx: usize, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.write(id, buffer, offset)
}

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

    scheme.open(relative_path, flags, mode).map(|id| (i, id))
}

pub fn close(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.close(id)
}

pub fn lseek(scheme_idx: usize, id: usize, offset: isize, whence: usize, current_offset: u64) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.lseek(id, offset, whence, current_offset)
}

pub fn fstat(scheme_idx: usize, id: usize, stat: &mut Stat) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.fstat(id, stat)
}

pub fn stat(path: &str, stat: &mut Stat) -> Result<usize, usize> {
    let (sid, rid) = open(path, 0, 0)?;
    let res = fstat(sid, rid, stat);
    let _ = close(sid, rid);
    res
}

pub fn pread(scheme_idx: usize, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize> {
    read(scheme_idx, id, buffer, offset)
}

pub fn pwrite(scheme_idx: usize, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize> {
    write(scheme_idx, id, buffer, offset)
}

pub fn getdents(scheme_idx: usize, id: usize) -> Result<Vec<String>, usize> {
    get_scheme(scheme_idx)?.getdents(id)
}

pub fn poll(scheme_idx: usize, id: usize, events: usize) -> Result<usize, usize> {
    let scheme = get_scheme(scheme_idx)?;
    match scheme.poll(id, events) {
        Ok(r) => Ok(r),
        Err(e) => {
            if e != error::EAGAIN {
                crate::serial::serial_printf(format_args!("[SCHEME] poll error {} for scheme_idx={}, resource_id={}\n", e, scheme_idx, id));
            }
            Err(e)
        }
    }
}

pub fn fmap(scheme_idx: usize, id: usize, offset: usize, len: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.fmap(id, offset, len)
}

pub fn ftruncate(scheme_idx: usize, id: usize, len: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.ftruncate(id, len)
}

pub fn fsync(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.fsync(id)
}

pub fn fdatasync(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.fdatasync(id)
}

pub fn flock(scheme_idx: usize, id: usize, operation: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.flock(id, operation)
}

pub fn ioctl(scheme_idx: usize, id: usize, request: usize, arg: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.ioctl(id, request, arg)
}

pub fn check_access(scheme_idx: usize, id: usize, mask: u8) -> Result<(), usize> {
    get_scheme(scheme_idx)?.check_access(id, mask)
}

pub fn dup(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.dup(id)
}

pub fn dup_independent(scheme_idx: usize, id: usize) -> Result<usize, usize> {
    get_scheme(scheme_idx)?.dup_independent(id)
}

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
        let (_, scheme) = reg.schemes.iter()
            .find(|(name, _)| name == old_scheme)
            .ok_or(error::ENOENT)?;
        Arc::clone(scheme)
    };

    scheme.rename(old_rel, new_rel)
}

pub fn readlink(path: &str, bufsiz: usize) -> Result<String, usize> {
    let mut parts = path.splitn(2, ':');
    let scheme_name = parts.next().ok_or(error::EINVAL)?;
    let rel_path = parts.next().unwrap_or("");

    let scheme = {
        let reg = REGISTRY.lock();
        let (_, scheme) = reg.schemes.iter()
            .find(|(name, _)| name == scheme_name)
            .ok_or(error::ENOENT)?;
        Arc::clone(scheme)
    };

    scheme.readlink(rel_path, bufsiz)
}

pub fn rmdir(path: &str) -> Result<usize, usize> {
    let mut parts = path.splitn(2, ':');
    let scheme_name = parts.next().ok_or(error::EINVAL)?;
    let rel_path = parts.next().unwrap_or("");

    let scheme = {
        let reg = REGISTRY.lock();
        let (_, scheme) = reg.schemes.iter()
            .find(|(name, _)| name == scheme_name)
            .ok_or(error::ENOENT)?;
        Arc::clone(scheme)
    };

    scheme.rmdir(rel_path)
}

/// The Scheme trait defines the interface for all resource providers.
pub trait Scheme: Send + Sync {
    /// Open a resource at the given path
    fn open(&self, path: &str, flags: usize, mode: u32) -> Result<usize, usize>;

    /// Read data from a resource at a given offset
    fn read(&self, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize>;

    /// Write data to a resource at a given offset
    fn write(&self, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize>;

    /// Seek within a resource
    fn lseek(&self, id: usize, offset: isize, whence: usize, current_offset: u64) -> Result<usize, usize>;

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

    /// Synchronize resource state to storage
    fn fsync(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    /// Synchronize data to storage (metadata may not be synced)
    fn fdatasync(&self, _id: usize) -> Result<usize, usize> {
        Ok(0)
    }

    /// Apply or remove an advisory lock on the open file
    fn flock(&self, _id: usize, _operation: usize) -> Result<usize, usize> {
        Ok(0)
    }

    /// Create a directory
    fn mkdir(&self, _path: &str, _mode: u32) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Remove a file
    fn unlink(&self, _path: &str) -> Result<usize, usize> {
        Err(error::ENOSYS)
    }

    /// Remove a directory
    fn rmdir(&self, _path: &str) -> Result<usize, usize> {
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

    /// Check if the current process has access to the resource.
    /// `mask` is 4=R, 2=W, 1=X.
    fn check_access(&self, _id: usize, _mask: u8) -> Result<(), usize> {
        Ok(())
    }

    /// List directory entries for a directory resource.
    fn getdents(&self, _id: usize) -> Result<Vec<String>, usize> {
        Err(error::ENOSYS)
    }

    /// Read the target of a symbolic link.
    fn readlink(&self, _path: &str, _bufsiz: usize) -> Result<String, usize> {
        Err(error::EINVAL)
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
pub use crate::random_scheme::RandomScheme;

pub struct LogScheme;

impl Scheme for LogScheme {
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        Ok(0) // Single resource for logging
    }

    fn read(&self, _id: usize, _buffer: &mut [u8], _offset: u64) -> Result<usize, usize> {
        // Log is write-only; read returns 0 (EOF) so stdin read(0) doesn't fail with EIO
        Ok(0)
    }

    fn write(&self, _id: usize, buf: &[u8], _offset: u64) -> Result<usize, usize> {
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

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
        Err(error::EIO) // Not seekable
    }

    fn close(&self, _id: usize) -> Result<usize, usize> {
        Ok(0) // Nothing to close
    }

    fn fstat(&self, _id: usize, _stat: &mut Stat) -> Result<usize, usize> {
        Err(error::EIO) // No stat info
    }
}

// ---------------------------------------------------------------------------
// MemfdScheme — anonymous in-memory files created by memfd_create(2)
// ---------------------------------------------------------------------------

struct MemfdEntry {
    phys_addr: u64,
    allocated_bytes: usize,
    logical_size: usize,
    seals: u32,
}

/// Scheme backing `memfd_create(2)`.  Each `open` call creates a new anonymous
/// file backed by contiguous physical pages.  The file starts with size 0 and
/// grows via `ftruncate`.  It supports `read`, `write`, `fmap` (for `mmap`),
/// and `fstat`.  Entries are private to the opener (not accessible by name).
pub struct MemfdScheme {
    entries: Mutex<Vec<Option<MemfdEntry>>>,
}

unsafe impl Send for MemfdScheme {}
unsafe impl Sync for MemfdScheme {}

impl MemfdScheme {
    pub fn new() -> Self {
        Self { entries: Mutex::new(Vec::new()) }
    }

    pub fn get_seals(&self, id: usize) -> Result<u32, usize> {
        let entries = self.entries.lock();
        let e = entries.get(id).and_then(|s| s.as_ref()).ok_or(error::EBADF)?;
        Ok(e.seals)
    }

    pub fn add_seals(&self, id: usize, seals: u32) -> Result<(), usize> {
        let mut entries = self.entries.lock();
        let e = entries.get_mut(id).and_then(|s| s.as_mut()).ok_or(error::EBADF)?;
        
        if (e.seals & 0x0001) != 0 { // F_SEAL_SEAL
            return Err(error::EPERM);
        }
        
        e.seals |= seals;
        Ok(())
    }
}

impl Scheme for MemfdScheme {
    /// Create a new anonymous file.  `path` and `flags` are ignored; the caller
    /// (sys_memfd_create) is responsible for O_CLOEXEC handling.
    fn open(&self, _path: &str, _flags: usize, _mode: u32) -> Result<usize, usize> {
        let mut entries = self.entries.lock();
        for (i, slot) in entries.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(MemfdEntry { phys_addr: 0, allocated_bytes: 0, logical_size: 0, seals: 0 });
                return Ok(i);
            }
        }
        let id = entries.len();
        entries.push(Some(MemfdEntry { phys_addr: 0, allocated_bytes: 0, logical_size: 0, seals: 0 }));
        Ok(id)
    }

    fn read(&self, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize> {
        let entries = self.entries.lock();
        let e = entries.get(id).and_then(|s| s.as_ref()).ok_or(error::EBADF)?;
        let off = offset as usize;
        if off >= e.logical_size || e.allocated_bytes == 0 {
            return Ok(0);
        }
        let avail = e.logical_size - off;
        let to_copy = buffer.len().min(avail);
        let virt = crate::memory::PHYS_MEM_OFFSET + e.phys_addr + off as u64;
        unsafe {
            core::ptr::copy_nonoverlapping(virt as *const u8, buffer.as_mut_ptr(), to_copy);
        }
        Ok(to_copy)
    }

    fn write(&self, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize> {
        let entries = self.entries.lock();
        let e = entries.get(id).and_then(|s| s.as_ref()).ok_or(error::EBADF)?;
        
        if (e.seals & 0x0008) != 0 { // F_SEAL_WRITE
            return Err(error::EPERM);
        }

        let off = offset as usize;
        if e.allocated_bytes == 0 || off >= e.logical_size {
            return Err(error::EINVAL);
        }
        let avail = e.logical_size - off;
        let to_copy = buffer.len().min(avail);
        let virt = crate::memory::PHYS_MEM_OFFSET + e.phys_addr + off as u64;
        unsafe {
            core::ptr::copy_nonoverlapping(buffer.as_ptr(), virt as *mut u8, to_copy);
        }
        Ok(to_copy)
    }

    fn lseek(&self, _id: usize, offset: isize, whence: usize, current_offset: u64) -> Result<usize, usize> {
        // SEEK_SET=0, SEEK_CUR=1, SEEK_END=2
        let new_off: i64 = match whence {
            0 => offset as i64,
            1 => current_offset as i64 + offset as i64,
            _ => return Err(error::EINVAL),
        };
        if new_off < 0 { return Err(error::EINVAL); }
        Ok(new_off as usize)
    }

    /// `ftruncate` sets the logical file size, allocating physical pages on
    /// the first call.  Shrinking (logical_size only) is supported; growing
    /// beyond the initially allocated region is not (returns EINVAL).
    fn ftruncate(&self, id: usize, len: usize) -> Result<usize, usize> {
        let mut entries = self.entries.lock();
        let e = entries.get_mut(id).and_then(|s| s.as_mut()).ok_or(error::EBADF)?;
        
        if len < e.logical_size && (e.seals & 0x0002) != 0 { // F_SEAL_SHRINK
            return Err(error::EPERM);
        }
        if len > e.logical_size && (e.seals & 0x0004) != 0 { // F_SEAL_GROW
            return Err(error::EPERM);
        }

        if len > SHM_REGION_MAX_BYTES {
            return Err(error::EINVAL);
        }
        let needed_pages = (len + 0xFFF) / 0x1000;
        if needed_pages > e.allocated_bytes / 0x1000 {
            if e.allocated_bytes != 0 {
                // Growing beyond initial allocation is not supported.
                return Err(error::EINVAL);
            }
            // First allocation.
            match crate::memory::alloc_phys_frames_contig(needed_pages as u64) {
                Some(phys) => {
                    let virt = crate::memory::PHYS_MEM_OFFSET + phys;
                    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, needed_pages * 0x1000); }
                    e.phys_addr = phys;
                    e.allocated_bytes = needed_pages * 0x1000;
                }
                None => return Err(error::ENOMEM),
            }
        }
        e.logical_size = len;
        Ok(0)
    }

    fn fmap(&self, id: usize, offset: usize, len: usize) -> Result<usize, usize> {
        let entries = self.entries.lock();
        let e = entries.get(id).and_then(|s| s.as_ref()).ok_or(error::EBADF)?;
        if e.allocated_bytes == 0 {
            return Err(error::EINVAL);
        }
        if offset + len > e.allocated_bytes {
            return Err(error::EINVAL);
        }
        Ok(e.phys_addr as usize + offset)
    }

    fn fstat(&self, id: usize, stat: &mut Stat) -> Result<usize, usize> {
        let entries = self.entries.lock();
        let e = entries.get(id).and_then(|s| s.as_ref()).ok_or(error::EBADF)?;
        stat.size = e.logical_size as u64;
        stat.mode = 0o600 | 0x8000; // S_IFREG | rw-------
        Ok(0)
    }

    fn close(&self, id: usize) -> Result<usize, usize> {
        let mut entries = self.entries.lock();
        if id < entries.len() {
            if entries[id].is_some() {
                // Physical pages are not freed (no free_phys_frames_contig yet);
                // same limitation as ShmScheme.
                entries[id] = None;
                return Ok(0);
            }
        }
        Err(error::EBADF)
    }
}


pub fn init() {
    register_scheme("log", Arc::new(LogScheme));
    register_scheme("random", Arc::new(RandomScheme::new()));
    register_scheme("tty", Arc::new(crate::tty::TtyScheme::new()));
    register_scheme("shm", Arc::new(ShmScheme::new()));
    register_scheme("memfd", get_memfd_scheme().clone());
    register_scheme("drm", Arc::new(crate::drm_scheme::DrmScheme));
    register_scheme("eth", Arc::new(crate::eth::EthScheme));
    register_scheme("pty", Arc::new(crate::pty::PtyScheme::new()));
    // El PipeScheme usa el singleton global PIPE_SCHEME; el proxy delega en él.
    register_scheme("pipe", Arc::new(PipeSchemeProxy));
    register_scheme("epoll", crate::epoll::get_epoll_scheme().clone());
    register_scheme("eventfd", crate::eventfd::get_eventfd_scheme().clone());
    register_scheme("signalfd", Arc::new(crate::signalfd::SignalfdScheme::new()));
    register_scheme("timerfd", crate::timerfd::get_timerfd_scheme().clone());
    register_scheme("kqueue", crate::kqueue::get_kqueue_scheme().clone());

    // Schemes from servers module
    register_scheme("display", Arc::new(crate::servers::DisplayScheme));
    register_scheme("input", Arc::new(crate::servers::InputScheme::new()));
    register_scheme("snd", Arc::new(crate::servers::AudioScheme));
    register_scheme("net", Arc::new(crate::servers::NetworkScheme));
    // "socket" scheme is registered by servers::init(), called after this function
}

static MEMFD_SCHEME: spin::Once<Arc<MemfdScheme>> = spin::Once::new();

pub fn get_memfd_scheme() -> &'static Arc<MemfdScheme> {
    MEMFD_SCHEME.call_once(|| Arc::new(MemfdScheme::new()))
}

/// Proxy sin estado que delega todas las operaciones en el singleton PIPE_SCHEME.
struct PipeSchemeProxy;

impl Scheme for PipeSchemeProxy {
    fn open(&self, path: &str, flags: usize, mode: u32) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.open(path, flags, mode)
    }
    fn read(&self, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.read(id, buffer, offset)
    }
    fn write(&self, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.write(id, buffer, offset)
    }
    fn lseek(&self, id: usize, offset: isize, whence: usize, current_offset: u64) -> Result<usize, usize> {
        crate::pipe::PIPE_SCHEME.lseek(id, offset, whence, current_offset)
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
                let size: usize = SHM_REGION_MAX_BYTES;
                
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

    fn read(&self, id: usize, buffer: &mut [u8], offset: u64) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let regions = self.regions.lock();
        let region = regions.get(name).ok_or(error::EIO)?;

        if offset as usize >= region.size {
            return Ok(0);
        }

        // For SHM, read/write at the given offset.
        let to_copy = core::cmp::min(buffer.len(), region.size - offset as usize);
        let virt = crate::memory::PHYS_MEM_OFFSET + region.phys_addr + offset;
        unsafe {
            core::ptr::copy_nonoverlapping(virt as *const u8, buffer.as_mut_ptr(), to_copy);
        }
        Ok(to_copy)
    }

    fn write(&self, id: usize, buffer: &[u8], offset: u64) -> Result<usize, usize> {
        let handles = self.handles.lock();
        let name = handles.get(id).and_then(|h| h.as_ref()).ok_or(error::EBADF)?;
        let regions = self.regions.lock();
        let region = regions.get(name).ok_or(error::EIO)?;

        if offset as usize >= region.size {
            return Err(error::EINVAL);
        }

        let to_copy = core::cmp::min(buffer.len(), region.size - offset as usize);
        let virt = crate::memory::PHYS_MEM_OFFSET + region.phys_addr + offset;
        unsafe {
            core::ptr::copy_nonoverlapping(buffer.as_ptr(), virt as *mut u8, to_copy);
        }
        Ok(to_copy)
    }

    fn lseek(&self, _id: usize, _offset: isize, _whence: usize, _current_offset: u64) -> Result<usize, usize> {
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
        if len > SHM_REGION_MAX_BYTES {
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
