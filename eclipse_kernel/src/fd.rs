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
    pub inode: u32,
    pub offset: u64,
    pub flags: u32,
    // Path removed to make it Copy-compatible
}

impl FileDescriptor {
    pub const fn new() -> Self {
        Self {
            in_use: false,
            inode: 0,
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
    pub fn allocate(&mut self, inode: u32, flags: u32) -> Option<usize> {
        // Start from FD 3 (0=stdin, 1=stdout, 2=stderr)
        for fd in 3..MAX_FDS_PER_PROCESS {
            if !self.fds[fd].in_use {
                self.fds[fd] = FileDescriptor {
                    in_use: true,
                    inode,
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
        if fd >= 3 && fd < MAX_FDS_PER_PROCESS && self.fds[fd].in_use {
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

/// Open a file for a process
pub fn fd_open(pid: ProcessId, inode: u32, flags: u32) -> Option<usize> {
    let mut tables = FD_TABLES.lock();
    let pid_idx = pid as usize;
    if pid_idx < MAX_PROCESSES {
        tables[pid_idx].allocate(inode, flags)
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
