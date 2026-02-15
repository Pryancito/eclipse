//! File Descriptor Management
//! 
//! Implements per-process file descriptor tables for syscall operations.

use alloc::vec::Vec;
use alloc::string::String;
use spin::Mutex;
use crate::process::ProcessId;

/// Maximum number of open files per process
const MAX_FDS_PER_PROCESS: usize = 64;

/// Maximum number of processes (matching scheduler limit)
const MAX_PROCESSES: usize = 64;

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
    fds: [FileDescriptor; MAX_FDS_PER_PROCESS],
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

/// Global file descriptor tables (one per process)
static FD_TABLES: Mutex<[FdTable; MAX_PROCESSES]> = Mutex::new([FdTable::new(); MAX_PROCESSES]);

/// Get the FD table for a process
pub fn get_fd_table(pid: ProcessId) -> Option<spin::MutexGuard<'static, [FdTable; MAX_PROCESSES]>> {
    if (pid as usize) < MAX_PROCESSES {
        Some(FD_TABLES.lock())
    } else {
        None
    }
}

/// Open a file for a process using a scheme and resource
pub fn fd_open(pid: ProcessId, scheme_id: usize, resource_id: usize, flags: u32) -> Option<usize> {
    let mut tables = FD_TABLES.lock();
    let pid_idx = pid as usize;
    if pid_idx < MAX_PROCESSES {
        tables[pid_idx].allocate(scheme_id, resource_id, flags)
    } else {
        None
    }
}

/// Get file descriptor for a process
pub fn fd_get(pid: ProcessId, fd: usize) -> Option<FileDescriptor> {
    let tables = FD_TABLES.lock();
    let pid_idx = pid as usize;
    if pid_idx < MAX_PROCESSES {
        tables[pid_idx].get(fd).cloned()
    } else {
        None
    }
}

/// Update file descriptor offset
pub fn fd_update_offset(pid: ProcessId, fd: usize, new_offset: u64) -> bool {
    let mut tables = FD_TABLES.lock();
    let pid_idx = pid as usize;
    if pid_idx < MAX_PROCESSES {
        if let Some(fd_entry) = tables[pid_idx].get_mut(fd) {
            fd_entry.offset = new_offset;
            return true;
        }
    }
    false
}

/// Close a file descriptor for a process
pub fn fd_close(pid: ProcessId, fd: usize) -> bool {
    let mut tables = FD_TABLES.lock();
    let pid_idx = pid as usize;
    if pid_idx < MAX_PROCESSES {
        tables[pid_idx].close(fd)
    } else {
        false
    }
}

/// Initialize FD system
pub fn init() {
    crate::serial::serial_print("File descriptor system initialized\n");
}

/// Clone parent's fd table to child (call from fork). Child gets same open fds as parent.
pub fn fd_clone_for_fork(parent_pid: ProcessId, child_pid: ProcessId) {
    let mut tables = FD_TABLES.lock();
    let parent_idx = parent_pid as usize;
    let child_idx = child_pid as usize;
    if parent_idx < MAX_PROCESSES && child_idx < MAX_PROCESSES {
        tables[child_idx] = tables[parent_idx];
    }
}

/// Initialize standard I/O for a process
pub fn fd_init_stdio(pid: ProcessId) {
    if let Ok((scheme_id, resource_id)) = crate::scheme::open("log:", 0, 0) {
        let mut tables = FD_TABLES.lock();
        let pid_idx = pid as usize;
        if pid_idx < MAX_PROCESSES {
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
}
