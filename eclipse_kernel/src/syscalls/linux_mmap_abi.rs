//! Linux x86-64 mmap ABI constants and arena definitions

pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;
pub const PROT_MASK: u64 = 7;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_POPULATE: u64 = 0x08000;
pub const MAP_HUGETLB: u64 = 0x40000;
pub const MAP_HUGE_2MB: u64 = 21 << 26;

/// Donde `mmap_find_free` coloca `mmap(NULL, …)` anónimo.
pub const USER_ARENA_LO: u64 = 0x6000_0000;
/// Arena superior expandida para buffers de alta dirección (GPU/DRM).
pub const USER_ARENA_HI: u64 = 0x0000_7000_0000_0000;

/// Pila fija tras `exec`/`execve` / `spawn`.
pub const USER_EXEC_STACK_LO: u64 = 0x2000_0000;
pub const USER_EXEC_STACK_HI: u64 = USER_EXEC_STACK_LO + 0x10_0000;

/// Páginas extra más allá del tamaño redondeado.
pub const ANON_SLACK_BYTES: u64 = 0x8000;
