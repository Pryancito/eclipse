//! `ioctl(2)`, i386 54: one number per device question, and three ways the
//! two ABIs can disagree about one.
//!
//! `ioctl` is not a system call so much as a thousand of them behind one
//! number, so it cannot go in either of the two allowlists
//! (`i386_abi_same.in` / `i386_abi_narrow.in`): those vouch for a *call*, and
//! here the thing to vouch for is the **request**. Until this module existed
//! the call was in neither list and a 32-bit program got `ENOSYS` for every
//! `ioctl` --- including `TCGETS`, which busybox's shell makes before it
//! prints a prompt.
//!
//! ## What can differ, measured
//!
//! Measured with i386 assembly probes on this container's Linux, not read off
//! a header. Three shapes, and only the first is the one the earlier batches
//! already know about:
//!
//! 1. **The structure behind the pointer.** `struct ifreq` is **32 bytes on
//!    i386 and 40 native** (`char ifr_name[16]` plus a union whose largest
//!    member is `struct ifmap`: 16 bytes against 24), and `struct ifconf` is
//!    **8 bytes against 16**, because it holds a pointer. Measured:
//!    `SIOCGIFADDR` from i386 filled bytes 16..31 of its `ifreq` and left
//!    32..39 untouched, and `SIOCGIFCONF` with an 8-byte block answered 0 and
//!    reported `ifc_len = 64` --- two `ifreq` of 32, `lo` and `eth0`.
//! 2. **The request number itself**, when `_IOC`'s size field holds a
//!    `size_t`, a `long` or a pointer, because that field is the
//!    `sizeof` the *caller's* ABI measures. On `/dev/loop0`:
//!
//!    | request as spelled | value | answer from i386 |
//!    |---|---|---|
//!    | `BLKGETSIZE64`, x86_64 | `0x80081272` | **ENOTTY** |
//!    | `BLKGETSIZE64`, i386 | `0x80041272` | 0 |
//!    | `BLKGETSIZE64`, i386, destination 4 mod 8 | `0x80041272` | 0 |
//!
//!    So the number a 32-bit program sends is one no native handler in this
//!    tree compares against. This is Linux's `compat_ioctl` table.
//! 3. **The width of the payload, with the number unchanged**, when the
//!    request is a bare `_IO` that carries no size at all. `BLKGETSIZE` is
//!    `_IO(0x12, 96)` --- the same number in both ABIs --- and writes **one
//!    `long`**: from i386 it wrote four bytes (8192 sectors of 512 = the 4 MiB
//!    of the loop file) and left the next word alone. A native handler would
//!    write eight.
//!
//! ## What this module vouches for
//!
//! The terminal family, which is the part a shell needs and the part where
//! there is provably nothing to convert. Every structure in it is made of
//! `u32`, `u16` and bytes:
//!
//! ```text
//! struct termios   36 bytes, align 4   (four u32, c_line, c_cc[19])
//! struct termios2  44 bytes, align 4   (plus c_ispeed, c_ospeed)
//! struct winsize    8 bytes, align 2   (four u16)
//! struct vt_mode    8 bytes, align 2
//! struct vt_stat    6 bytes, align 2
//! struct kbentry    4 bytes, align 2
//! int / unsigned int / char             the rest
//! ```
//!
//! None of them has a `long`, a pointer or a 64-bit field, so none of them
//! changes size, order **or alignment** between the two ABIs --- which means
//! the terminal family does not even have the third form of the trap, the one
//! [`crate::narrow`] exists for. Measured anyway rather than assumed:
//! `TCGETS` from i386 with its destination at an address 4 mod 8 answers 0,
//! and so do `TCSETS`, `TIOCGWINSZ`, `FIONREAD`, `TIOCOUTQ`, `TIOCGPTN`,
//! `TIOCSPTLCK`, `FIOCLEX`, `FIONCLEX` and `FIONBIO`. (`TIOCGSID` answers
//! `ENOTTY` on a `/dev/ptmx` master, which is what Linux answers too: that is
//! the device talking, not the ABI.)
//!
//! Their numbers are all legacy `0x54xx`/`0x4Bxx`/`0x56xx` ones that encode no
//! size, so the second form cannot reach them either, and their payload is
//! never a `long`, so neither can the third.
//!
//! ## What it does not, and why
//!
//! Every other request answers `ENOSYS` with its number in the log. **Not
//! `ENOTTY`**: that is the answer a device gives when it does not implement a
//! request a program is entitled to ask, and a program reading it concludes
//! "this device cannot do that" and carries on. `ENOSYS` says the thing that
//! is actually true --- this kernel has not converted that request for this
//! ABI --- and is the same answer the rest of this layer gives for a call it
//! has not vouched for.
//!
//! - **The block pair.** `BLKGETSIZE64` needs its number translated *and* its
//!   eight bytes written to a destination that may be 4-aligned, and the
//!   write happens in `linux_object`'s `io_control`, which does not know the
//!   calling process's ABI --- deliberately, since `io_control` is reached
//!   from the inode and not from the syscall layer. `BLKGETSIZE` needs the
//!   opposite: the same number, four bytes instead of eight. Both are small
//!   and both want a place to put a per-request conversion that this batch
//!   does not build.
//! - **The network ones** (`SIOCGIFCONF`, `SIOCGIFADDR`, `SIOCGIFINDEX`,
//!   `SIOCGIFFLAGS` and the rest of the `0x89xx` family) all pass a `struct
//!   ifreq` or `struct ifconf`, which is form 1: the first needs the union
//!   repacked, the second holds a pointer. That is the `iovec`'s problem again
//!   and belongs with it.
//! - **The device families** --- DRM, `snd`, `fbdev`, `input`, `dsp`, the
//!   NVIDIA nodes --- are all `_IOC` requests whose size field is the size of
//!   a structure full of pointers and `__u64`s, so for them forms 1 and 2
//!   apply at once: the number differs *and* the structure differs, per
//!   request, for hundreds of requests. A 32-bit program that wants a GPU is
//!   not the program this stage is for.

use super::*;

/// The terminal and VFS requests whose number, structure and alignment are
/// the same in both ABIs, each with its name for the log.
///
/// A table and not a `match` so that the tests can walk it: what is worth
/// pinning here is a property of the whole list (every entry's structure is
/// `long`-free, no entry's number encodes a size), and a property of a list
/// can only be tested by reading the list.
const SAME: &[(u32, &str)] = &[
    // `struct termios`, 36 bytes of u32 and bytes.
    (0x5401, "TCGETS"),
    (0x5402, "TCSETS"),
    (0x5403, "TCSETSW"),
    (0x5404, "TCSETSF"),
    // `struct termios2`, 44 bytes of the same. These four DO encode a size
    // (0x2c = 44), and it is the same 44 in both ABIs, which is the point:
    // the size field only differs when what it measures does.
    (0x802c542a, "TCGETS2"),
    (0x402c542b, "TCSETS2"),
    (0x402c542c, "TCSETSW2"),
    (0x402c542d, "TCSETSF2"),
    // A scalar in the argument, nothing read or written through it.
    (0x540b, "TCFLSH"),
    (0x540e, "TIOCSCTTY"),
    (0x5422, "TIOCNOTTY"),
    (0x5441, "TIOCGPTPEER"),
    // `int *`.
    (0x540f, "TIOCGPGRP"),
    (0x5410, "TIOCSPGRP"),
    (0x5411, "TIOCOUTQ"),
    (0x5415, "TIOCMGET"),
    (0x5416, "TIOCMBIS"),
    (0x5417, "TIOCMBIC"),
    (0x5418, "TIOCMSET"),
    (0x541b, "FIONREAD"),
    (0x5421, "FIONBIO"),
    (0x5429, "TIOCGSID"),
    (0x5452, "FIOASYNC"),
    (0x80045430, "TIOCGPTN"),
    (0x40045431, "TIOCSPTLCK"),
    // No argument at all: the fd's close-on-exec bit.
    (0x5450, "FIONCLEX"),
    (0x5451, "FIOCLEX"),
    // `struct winsize`, four u16.
    (0x5413, "TIOCGWINSZ"),
    (0x5414, "TIOCSWINSZ"),
    // `struct serial_icounter_struct`, all int.
    (0x545d, "TIOCGICOUNT"),
    // The console and keyboard: a char, an int, or `struct kbentry`.
    (0x4b30, "KDMKTONE"),
    (0x4b31, "KDGETLED"),
    (0x4b32, "KDSETLED"),
    (0x4b33, "KDGKBTYPE"),
    (0x4b3a, "KDSETMODE"),
    (0x4b3b, "KDGETMODE"),
    (0x4b44, "KDGKBMODE"),
    (0x4b45, "KDSKBMODE"),
    (0x4b46, "KDGKBENT"),
    // The VT switch: `struct vt_mode`, `struct vt_stat`, and scalars.
    (0x5600, "VT_OPENQRY"),
    (0x5601, "VT_GETMODE"),
    (0x5602, "VT_SETMODE"),
    (0x5603, "VT_GETSTATE"),
    (0x5605, "VT_RELDISP"),
    (0x5606, "VT_ACTIVATE"),
    (0x5607, "VT_WAITACTIVE"),
    (0x5608, "VT_DISALLOCATE"),
];

/// The name of a request this module vouches for, or `None`.
///
/// The lookup is linear over about forty entries, which is nothing next to
/// the device access the call is about to do, and keeps the table readable in
/// the order the families come rather than sorted by number.
fn vouched(request: u32) -> Option<&'static str> {
    SAME.iter()
        .find(|&&(number, _)| number == request)
        .map(|&(_, name)| name)
}

impl Syscall<'_> {
    /// `ioctl(fd, request, arg)` from a 32-bit process, i386 54.
    ///
    /// A vouched request goes to the native handler unchanged, because for
    /// these there is nothing to change: see the module documentation for the
    /// measurements that say so. Anything else answers `ENOSYS`.
    ///
    /// The request arrives from the trap frame as the low 32 bits
    /// zero-extended, which is the one thing that has to be got right here
    /// even for a pass-through. A native caller's `ioctl` request is an `int`
    /// widened by the variadic prototype, so musl sends `TIOCGPTN` as
    /// `0xffff_ffff_8004_5430` and this layer sends `0x0000_0000_8004_5430`;
    /// the handlers that take a `u32` see the same number either way, and
    /// `sys_ioctl`'s own small-value arms are all numbers with bit 31 clear,
    /// so neither form collides. Taking `request` as a `u32` here is what
    /// keeps that true rather than hoping the caller masked.
    pub async fn sys_ioctl32(
        &mut self,
        fd: usize,
        request: u32,
        arg1: usize,
        arg2: usize,
        arg3: usize,
    ) -> SysResult {
        match vouched(request) {
            Some(name) => {
                trace!(
                    "ioctl32: {:#x} ({}) on fd {} is the same in both ABIs",
                    request,
                    name,
                    fd
                );
                self.sys_ioctl(fd.into(), request as usize, arg1, arg2, arg3)
                    .await
            }
            None => {
                warn!(
                    "ioctl32: {:#x} on fd {} is not a request this layer has converted yet",
                    request, fd
                );
                Err(LxError::ENOSYS)
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wiring_tests;
