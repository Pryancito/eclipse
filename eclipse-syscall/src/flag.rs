//! Flags and constants for syscalls

// File open flags
pub const O_RDONLY: usize = 0x0000;
pub const O_WRONLY: usize = 0x0001;
pub const O_RDWR: usize = 0x0002;
pub const O_CREAT: usize = 0x0040;
pub const O_EXCL: usize = 0x0080;
pub const O_NOCTTY: usize = 0x0100;
pub const O_TRUNC: usize = 0x0200;
pub const O_APPEND: usize = 0x0400;
pub const O_NONBLOCK: usize = 0x0800;
pub const O_CLOEXEC: usize = 0x80000;
pub const O_NOFOLLOW: usize = 0x20000;
pub const O_DIRECTORY: usize = 0x10000;

// mmap prot flags
pub const PROT_NONE: usize = 0x0;
pub const PROT_READ: usize = 0x1;
pub const PROT_WRITE: usize = 0x2;
pub const PROT_EXEC: usize = 0x4;

// mmap flags
pub const MAP_SHARED: usize = 0x01;
pub const MAP_PRIVATE: usize = 0x02;
pub const MAP_FIXED: usize = 0x10;
pub const MAP_ANONYMOUS: usize = 0x20;
pub const MAP_ANON: usize = MAP_ANONYMOUS;

// clone flags
pub const CLONE_VM: usize = 0x00000100;
pub const CLONE_FS: usize = 0x00000200;
pub const CLONE_FILES: usize = 0x00000400;
pub const CLONE_SIGHAND: usize = 0x00000800;
pub const CLONE_THREAD: usize = 0x00010000;

// lseek whence
pub const SEEK_SET: usize = 0;
pub const SEEK_CUR: usize = 1;
pub const SEEK_END: usize = 2;
