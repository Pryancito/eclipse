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

/// A process's [`Abi`], readable without the process's lock.
///
/// The trap handler asks for it on EVERY syscall, twice on x86_64 (is this a
/// FreeBSD process? a 32-bit one?), and it lived in `LinuxProcessInner`, the
/// process's big IRQ-off spin lock. So every syscall of every thread took
/// that lock twice just to read a byte that is written once per `execve`:
/// twenty threads of one process calling `getpid` queued on it, and
/// eclipse-bench's `getpid/s x20` came out at 0.95 Mops/s against 2.52 for
/// one thread on a 20-CPU machine (Linux 7.0 on the same machine: 123).
///
/// Written by the loader at load and at `execve`, when no other thread of the
/// process is running user code, and copied at `fork`; a relaxed byte is all
/// it needs.
#[derive(Default)]
pub(super) struct AbiCell(core::sync::atomic::AtomicU8);

impl AbiCell {
    /// A cell holding `abi`.
    pub(super) fn new(abi: Abi) -> Self {
        let cell = Self::default();
        cell.set(abi);
        cell
    }

    /// The personality now.
    #[inline]
    pub(super) fn get(&self) -> Abi {
        match self.0.load(core::sync::atomic::Ordering::Relaxed) {
            1 => Abi::Freebsd,
            2 => Abi::Linux32,
            _ => Abi::Linux,
        }
    }

    /// Change the personality.
    pub(super) fn set(&self, abi: Abi) {
        let byte = match abi {
            Abi::Linux => 0,
            Abi::Freebsd => 1,
            Abi::Linux32 => 2,
        };
        self.0.store(byte, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod abi_cell_tests {
    use super::*;

    #[test]
    fn every_personality_reads_back_as_it_was_set() {
        let cell = AbiCell::default();
        assert_eq!(cell.get(), Abi::Linux, "a new process speaks Linux");
        for abi in [Abi::Freebsd, Abi::Linux32, Abi::Linux] {
            cell.set(abi);
            assert_eq!(cell.get(), abi);
            assert_eq!(AbiCell::new(abi).get(), abi);
        }
    }
}
