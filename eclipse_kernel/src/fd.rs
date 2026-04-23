//! File Descriptor Management
//! 
//! Implements per-process file descriptor tables for syscall operations.

use alloc::vec::Vec;
use alloc::string::String;
use spin::Mutex;
use crate::process::ProcessId;

/// Maximum number of open files per process
pub const MAX_FDS_PER_PROCESS: usize = 64;

/// Maximum number of processes (matching scheduler limit)
pub const MAX_FD_PROCESSES: usize = 256;

/// File descriptor entry
#[derive(Clone, Debug, Copy)]
pub struct FileDescriptor {
    pub in_use: bool,
    pub scheme_id: usize,
    pub resource_id: usize,
    pub offset: u64,
    pub flags: u32,
}

impl FileDescriptor {
    pub const fn new() -> Self {
        Self {
            in_use: false,
            scheme_id: 0,
            resource_id: 0,
            offset: 0,
            flags: 0,
        }
    }
}

/// Per-process file descriptor table  
#[derive(Copy, Clone)]
pub struct FdTable {
    pub fds: [FileDescriptor; MAX_FDS_PER_PROCESS],
}

impl FdTable {
    pub const fn new() -> Self {
        const EMPTY_FD: FileDescriptor = FileDescriptor::new();
        Self {
            fds: [EMPTY_FD; MAX_FDS_PER_PROCESS],
        }
    }
    
    /// Allocate a new file descriptor
    /// Returns the FD number (3+) or None if table is full
    /// FDs 0-2 are reserved for stdio
    pub fn allocate(&mut self, scheme_id: usize, resource_id: usize, flags: u32) -> Option<usize> {
        // Start from FD 3 (0=stdin, 1=stdout, 2=stderr)
        for fd in 3..MAX_FDS_PER_PROCESS {
            if !self.fds[fd].in_use {
                self.fds[fd] = FileDescriptor {
                    in_use: true,
                    scheme_id,
                    resource_id,
                    offset: 0,
                    flags,
                };
                return Some(fd);
            }
        }
        None
    }
    
    /// Get a file descriptor
    pub fn get(&self, fd: usize) -> Option<&FileDescriptor> {
        if fd < MAX_FDS_PER_PROCESS && self.fds[fd].in_use {
            Some(&self.fds[fd])
        } else {
            None
        }
    }
    
    /// Get a mutable file descriptor
    pub fn get_mut(&mut self, fd: usize) -> Option<&mut FileDescriptor> {
        if fd < MAX_FDS_PER_PROCESS && self.fds[fd].in_use {
            Some(&mut self.fds[fd])
        } else {
            None
        }
    }
    
    /// Close a file descriptor
    pub fn close(&mut self, fd: usize) -> bool {
        if fd < MAX_FDS_PER_PROCESS && self.fds[fd].in_use {
            self.fds[fd].in_use = false;
            true
        } else {
            false
        }
    }
}

/// Global file descriptor tables (one per process slot)
///
/// **Important:** This array is indexed by PROCESS_TABLE *slot index* (0..MAX_FD_PROCESSES),
/// NOT by the raw PID value.  PIDs are monotonically increasing (they never wrap) so using
/// `pid as usize` as the index would silently fail for any process with PID >= 64.
/// Use `pid_to_fd_idx(pid)` (below) to obtain the correct slot index.
pub static FD_TABLES: Mutex<[FdTable; MAX_FD_PROCESSES]> = Mutex::new([FdTable::new(); MAX_FD_PROCESSES]);

/// Translate a PID to the corresponding FD_TABLES slot index.
///
/// FD_TABLES shares the same slot numbering as PROCESS_TABLE (0..MAX_FD_PROCESSES).
/// Since PIDs are monotonically increasing, we must not use the raw PID value as an
/// array index — we must resolve it to the reusable slot index via the IPC PID→slot map.
pub fn pid_to_fd_idx(pid: ProcessId) -> Option<usize> {
    if let Some(p) = crate::process::get_process(pid) {
        return Some(p.resources.lock().fd_table_idx);
    }
    None
}

/// Get the FD table for a process
pub fn get_fd_table(pid: ProcessId) -> Option<spin::MutexGuard<'static, [FdTable; MAX_FD_PROCESSES]>> {
    if pid_to_fd_idx(pid).is_some() {
        Some(FD_TABLES.lock())
    } else {
        None
    }
}

/// Open a file for a process using a scheme and resource
pub fn fd_open(pid: ProcessId, scheme_id: usize, resource_id: usize, flags: u32) -> Option<usize> {
    let pid_idx = pid_to_fd_idx(pid)?;
    let mut tables = FD_TABLES.lock();
    tables[pid_idx].allocate(scheme_id, resource_id, flags)
}

/// Get file descriptor for a process
pub fn fd_get(pid: ProcessId, fd: usize) -> Option<FileDescriptor> {
    let pid_idx = pid_to_fd_idx(pid)?;
    let tables = FD_TABLES.lock();
    tables[pid_idx].get(fd).cloned()
}

/// Update file descriptor flags (e.g. O_NONBLOCK via fcntl F_SETFL)
pub fn fd_set_flags(pid: ProcessId, fd: usize, flags: u32) -> bool {
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return false,
    };
    let mut tables = FD_TABLES.lock();
    if let Some(fd_entry) = tables[pid_idx].get_mut(fd) {
        fd_entry.flags = flags;
        return true;
    }
    false
}

/// Update file descriptor offset
pub fn fd_update_offset(pid: ProcessId, fd: usize, new_offset: u64) -> bool {
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return false,
    };
    let mut tables = FD_TABLES.lock();
    if let Some(fd_entry) = tables[pid_idx].get_mut(fd) {
        fd_entry.offset = new_offset;
        return true;
    }
    false
}

// Close a file descriptor for a process
pub fn fd_close(pid: ProcessId, fd: usize) -> bool {
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return false,
    };
    // Save scheme/resource IDs and mark the slot as free *before* releasing the
    // lock, then notify the scheme *after* the lock is dropped.  Calling
    // scheme::close() while FD_TABLES is held risks a deadlock if the scheme
    // implementation ever needs to re-acquire that lock.
    let ids = {
        let mut tables = FD_TABLES.lock();
        if fd < MAX_FDS_PER_PROCESS && tables[pid_idx].fds[fd].in_use {
            let scheme_id = tables[pid_idx].fds[fd].scheme_id;
            let resource_id = tables[pid_idx].fds[fd].resource_id;
            tables[pid_idx].fds[fd].in_use = false;
            Some((scheme_id, resource_id))
        } else {
            None
        }
    };
    if let Some((scheme_id, resource_id)) = ids {
        let _ = crate::scheme::close(scheme_id, resource_id);
        true
    } else {
        false
    }
}

/// Push an existing file descriptor entry into a new slot
pub fn fd_push(pid: ProcessId, fd_entry: FileDescriptor) -> Option<usize> {
    let pid_idx = pid_to_fd_idx(pid)?;
    let mut tables = FD_TABLES.lock();
    for fd in 3..MAX_FDS_PER_PROCESS {
        if !tables[pid_idx].fds[fd].in_use {
            tables[pid_idx].fds[fd] = fd_entry;
            tables[pid_idx].fds[fd].in_use = true;
            // Increment ref count
            let _ = crate::scheme::dup(fd_entry.scheme_id, fd_entry.resource_id);
            return Some(fd);
        }
    }
    None
}

/// Push an existing file descriptor entry into a specific slot
pub fn fd_push_at(pid: ProcessId, target_fd: usize, fd_entry: FileDescriptor) -> bool {
    if target_fd >= MAX_FDS_PER_PROCESS { return false; }
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return false,
    };
    let mut tables = FD_TABLES.lock();
    tables[pid_idx].fds[target_fd] = fd_entry;
    tables[pid_idx].fds[target_fd].in_use = true;
    // Increment ref count
    let _ = crate::scheme::dup(fd_entry.scheme_id, fd_entry.resource_id);
    true
}

/// Initialize FD system
pub fn init() {
    crate::serial::serial_print("File descriptor system initialized\n");
}

/// Copia la tabla de FDs de `src_slot` a `dst_slot` en `FD_TABLES` y hace `dup` en cada recurso.
/// Usado al separar un hijo vfork (`CLONE_VM`) en `exec` cuando antes compartía `fd_table_idx`
/// con el padre.
pub fn fd_duplicate_table_slots(src_slot: usize, dst_slot: usize) {
    if src_slot >= MAX_FD_PROCESSES || dst_slot >= MAX_FD_PROCESSES || src_slot == dst_slot {
        return;
    }

    let mut to_dup: [(usize, usize); MAX_FDS_PER_PROCESS] = [(0, 0); MAX_FDS_PER_PROCESS];
    let mut dup_count = 0;
    {
        let mut tables = FD_TABLES.lock();
        tables[dst_slot] = tables[src_slot];
        for fd in 0..MAX_FDS_PER_PROCESS {
            if tables[dst_slot].fds[fd].in_use {
                to_dup[dup_count] = (
                    tables[dst_slot].fds[fd].scheme_id,
                    tables[dst_slot].fds[fd].resource_id,
                );
                dup_count += 1;
            }
        }
    }

    for i in 0..dup_count {
        let _ = crate::scheme::dup(to_dup[i].0, to_dup[i].1);
    }
}

/// Clone parent's fd table to child (call from fork). Child gets same open fds as parent.
pub fn fd_clone_for_fork(parent_pid: ProcessId, child_pid: ProcessId) {
    // Resolve slots before acquiring FD_TABLES to avoid lock-order issues.
    let parent_idx = match pid_to_fd_idx(parent_pid) {
        Some(i) => i,
        None => return,
    };
    let child_idx = match pid_to_fd_idx(child_pid) {
        Some(i) => i,
        None => return,
    };

    // Collect FDs to dup before releasing the lock.
    let mut to_dup: [(usize, usize); MAX_FDS_PER_PROCESS] = [(0, 0); MAX_FDS_PER_PROCESS];
    let mut dup_count = 0;
    {
        let mut tables = FD_TABLES.lock();
        tables[child_idx] = tables[parent_idx];
        for fd in 0..MAX_FDS_PER_PROCESS {
            if tables[child_idx].fds[fd].in_use {
                to_dup[dup_count] = (
                    tables[child_idx].fds[fd].scheme_id,
                    tables[child_idx].fds[fd].resource_id,
                );
                dup_count += 1;
            }
        }
    } // FD_TABLES lock released here

    // Notify each scheme that a handle has been inherited by the child (ref counting).
    for i in 0..dup_count {
        let _ = crate::scheme::dup(to_dup[i].0, to_dup[i].1);
    }
}

/// Initialize standard I/O for a process
pub fn fd_init_stdio(pid: ProcessId) {
    // Resolve the slot before touching FD_TABLES so high-PID processes get proper stdio.
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return,
    };
    if let Ok((scheme_id, resource_id)) = crate::scheme::open("tty:", 0, 0) {
        let mut tables = FD_TABLES.lock();
        // FD 0: stdin (same as log for now; read returns EIO so apps get error, not "FD not found")
        tables[pid_idx].fds[0] = FileDescriptor {
            in_use: true,
            scheme_id,
            resource_id,
            offset: 0,
            flags: 0,
        };
        // FD 1: stdout
        tables[pid_idx].fds[1] = FileDescriptor {
            in_use: true,
            scheme_id,
            resource_id,
            offset: 0,
            flags: 0,
        };
        // FD 2: stderr
        tables[pid_idx].fds[2] = FileDescriptor {
            in_use: true,
            scheme_id,
            resource_id,
            offset: 0,
            flags: 0,
        };
    }
}

/// Si alguno de stdin/stdout/stderr no está en uso, inicializar los tres con `tty:`.
///
/// Tras `execve`/`exec` los FD se heredan; si el padre nunca abrió `tty:` (p. ej. fallo al
/// spawn), `write(1)` / `write(2)` devuelven EBADF y las trazas de arranque no llegan al serial.
pub fn fd_ensure_stdio(pid: ProcessId) {
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return,
    };
    let need = {
        let tables = FD_TABLES.lock();
        !tables[pid_idx].fds[0].in_use
            || !tables[pid_idx].fds[1].in_use
            || !tables[pid_idx].fds[2].in_use
    };
    if need {
        fd_init_stdio(pid);
    }
}

/// Get the current offset of an open file descriptor.
pub fn fd_get_offset(pid: ProcessId, fd: usize) -> Option<u64> {
    let pid_idx = pid_to_fd_idx(pid)?;
    let tables = FD_TABLES.lock();
    if fd < MAX_FDS_PER_PROCESS && tables[pid_idx].fds[fd].in_use {
        return Some(tables[pid_idx].fds[fd].offset);
    }
    None
}

/// Set the offset of an open file descriptor.
pub fn fd_set_offset(pid: ProcessId, fd: usize, offset: u64) {
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return,
    };
    let mut tables = FD_TABLES.lock();
    if fd < MAX_FDS_PER_PROCESS && tables[pid_idx].fds[fd].in_use {
        tables[pid_idx].fds[fd].offset = offset;
    }
}
