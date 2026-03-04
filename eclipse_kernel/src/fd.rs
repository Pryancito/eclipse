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

/// Close a file descriptor for a process
pub fn fd_close(pid: ProcessId, fd: usize) -> bool {
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return false,
    };
    let mut tables = FD_TABLES.lock();
    tables[pid_idx].close(fd)
}

/// Initialize FD system
pub fn init() {
    crate::serial::serial_print("File descriptor system initialized\n");
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
    let mut tables = FD_TABLES.lock();
    tables[child_idx] = tables[parent_idx];
}

/// Initialize standard I/O for a process
pub fn fd_init_stdio(pid: ProcessId) {
    // Resolve the slot before touching FD_TABLES so high-PID processes get proper stdio.
    let pid_idx = match pid_to_fd_idx(pid) {
        Some(i) => i,
        None => return,
    };
    if let Ok((scheme_id, resource_id)) = crate::scheme::open("log:", 0, 0) {
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
