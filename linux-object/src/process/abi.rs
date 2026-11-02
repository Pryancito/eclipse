//! What operating system's ABI a process's binary speaks.
//!
//! One enum and two questions about it, in a file of their own because
//! `process.rs` is at its line ceiling (`xtask/src/tamano.rs`) and this is a
//! self-contained part of it: the personality is decided once, by the loader,
//! from the ELF header, and from then on it is only ever read.

/// System-call personality of a process: which operating system's ABI its
/// binary speaks. Detected from the ELF header at load time (see
/// `crate::loader`) and consulted by the trap handler to route each syscall to
/// the right translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Abi {
    /// Native Linux ABI (the default for every binary this kernel runs).
    #[default]
    Linux,
    /// FreeBSD/amd64 ABI (`ELFOSABI_FREEBSD` or a FreeBSD ABI note). Only
    /// meaningful on x86_64; other architectures always run as [`Abi::Linux`].
    Freebsd,
    /// 32-bit Linux/i386 ABI: an `ELFCLASS32` / `EM_386` image running in the
    /// CPU's compatibility mode on this 64-bit kernel -- Linux's
    /// `CONFIG_COMPAT`, its `TIF_ADDR32` process.
    ///
    /// It is the same kernel and the same system calls; what differs is the
    /// shape of the conversation. The process lives below 4 GiB, its initial
    /// stack is built out of 4-byte words, it enters through `int $0x80` with
    /// the i386 numbering in `eax` and its arguments in `ebx, ecx, edx, esi,
    /// edi, ebp`, and anything it passes by reference has the i386 layout.
    /// Only meaningful on x86_64.
    Linux32,
}

impl Abi {
    /// Whether this is a 32-bit personality: the process's pointers,
    /// `long`s and `off_t` are four bytes wide, not eight.
    ///
    /// Asked by everything that has to write a word into the process's memory
    /// or read one out of it, so that the width lives in one place instead of
    /// in each caller's `match`.
    pub fn is_32bit(self) -> bool {
        matches!(self, Abi::Linux32)
    }

    /// How wide one machine word is in this personality, in bytes.
    pub fn word_size(self) -> usize {
        if self.is_32bit() {
            4
        } else {
            8
        }
    }
}
