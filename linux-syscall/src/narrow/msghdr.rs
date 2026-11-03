//! The envelope of `sendmsg`/`recvmsg`, and the control messages inside it.
//!
//! ```text
//! native                                   i386
//! struct msghdr {                          struct msghdr {
//!     void   *msg_name;       //  0, 8         void     *msg_name;       //  0, 4
//!     socklen_t msg_namelen;  //  8, 4         socklen_t msg_namelen;    //  4, 4
//!     // 4 bytes of padding
//!     struct iovec *msg_iov;  // 16, 8         struct iovec *msg_iov;    //  8, 4
//!     size_t  msg_iovlen;     // 24, 8         size_t    msg_iovlen;     // 12, 4
//!     void   *msg_control;    // 32, 8         void     *msg_control;    // 16, 4
//!     size_t  msg_controllen; // 40, 8         size_t    msg_controllen;  // 20, 4
//!     int     msg_flags;      // 48, 4         int       msg_flags;      // 24, 4
//!     // 4 bytes of padding
//! };  // 56 bytes                          };  // 28 bytes
//! ```
//!
//! Three pointers and two `size_t` halve, `msg_namelen` and `msg_flags` are
//! `u32`/`i32` in both, and the native one carries two words of padding the
//! narrow one does not need. **Half the size, and read at the wrong width it
//! does not fail**: `msg_name` comes out with `msg_namelen` in its high half,
//! `msg_iov` out of the caller's `msg_iov` and `msg_iovlen` together --- an
//! address nobody named --- and `msg_iovlen` out of the control fields. The
//! same shape of defect as the `iovec` ([`super::iovec`]) one level up.
//!
//! The write-back direction is worse than a misread, and it is why this
//! module exists at all rather than a cast: `recvmsg` reports three things
//! through the caller's structure (`msg_namelen`, `msg_controllen` and
//! `msg_flags`), and writing them at the native offsets puts `msg_flags`
//! twenty bytes PAST the end of a 32-bit caller's 28-byte structure, over
//! whatever the caller keeps next to it.
//!
//! ## `struct cmsghdr`, which halves too
//!
//! ```text
//! native                                   i386
//! struct cmsghdr {                         struct cmsghdr {
//!     size_t cmsg_len;    //  0, 8             size_t cmsg_len;    // 0, 4
//!     int    cmsg_level;  //  8, 4             int    cmsg_level;  // 4, 4
//!     int    cmsg_type;   // 12, 4             int    cmsg_type;   // 8, 4
//! };  // 16 bytes, CMSG_ALIGN 8             };  // 12 bytes, CMSG_ALIGN 4
//! ```
//!
//! `CMSG_ALIGN` is `sizeof(long)` in Linux's own macro, so a 32-bit program
//! pads its control messages to 4 and not to 8. **Both numbers move at
//! once**, which is what makes this more than an offset table: a reader that
//! kept the 16-byte header would take the first four bytes of the payload as
//! the rest of `cmsg_len`, and one that kept the 8-byte step would step a
//! 16-byte `SCM_RIGHTS` of one descriptor to 16 instead of 16 --- the same by
//! luck --- and a two-descriptor one (20 bytes narrow) to 24 instead of 20,
//! landing inside the next message.
//!
//! So a `SCM_RIGHTS` of one descriptor is 16 bytes natively (16 header + 4,
//! padded to 8) and 16 narrow (12 + 4, already a multiple of 4) --- equal,
//! and it is the only count that is. Two descriptors are 24 and 20. That
//! coincidence at one descriptor is the reason this is not something a single
//! round trip would catch, and why the tests below use two.

use crate::*;
use core::convert::TryInto;
use core::mem::size_of;
use kernel_hal::user::{UserInPtr, UserOutPtr};
use linux_object::net::MsgHdr;
use linux_object::process::Abi;

/// Where each field of a `struct msghdr` sits, and how wide the two `size_t`
/// in it are, for one ABI.
///
/// The native row is `offset_of!`, not arithmetic: a field added to
/// [`MsgHdr`] would otherwise leave every offset here one word short and put
/// `recvmsg`'s answers inside the wrong fields. The narrow row is arithmetic
/// because there is no narrow structure to ask --- every field is four bytes,
/// in declaration order, with no padding, which is the whole of the i386
/// layout and is what [`THE_NARROW_SIZE`] pins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MsgHdrLayout {
    /// The whole structure's size, which is also the `mmsghdr` stride's
    /// first term.
    pub size: usize,
    pub name: usize,
    pub namelen: usize,
    pub iov: usize,
    pub iovlen: usize,
    pub control: usize,
    pub controllen: usize,
    pub flags: usize,
    /// How many bytes a `size_t` takes here: the width of `msg_iovlen` and
    /// `msg_controllen`, and the one thing about this structure that is not
    /// an offset.
    pub word: usize,
}

/// A 32-bit `struct msghdr`: seven four-byte fields and no padding.
///
/// Written out rather than derived from the field count, so that a field
/// added to the layout below shows up as a failure here rather than as a
/// structure read one word short.
const THE_NARROW_SIZE: usize = 28;

/// The narrow layout: seven words of four bytes, in declaration order.
pub(crate) const NARROW: MsgHdrLayout = MsgHdrLayout {
    size: THE_NARROW_SIZE,
    name: 0,
    namelen: 4,
    iov: 8,
    iovlen: 12,
    control: 16,
    controllen: 20,
    flags: 24,
    word: 4,
};

/// The layout of the ABI `abi` writes a `struct msghdr` in.
pub(crate) fn msghdr_layout(abi: Abi) -> MsgHdrLayout {
    if abi.is_32bit() {
        NARROW
    } else {
        MsgHdrLayout {
            size: size_of::<MsgHdr>(),
            name: core::mem::offset_of!(MsgHdr, msg_name),
            namelen: core::mem::offset_of!(MsgHdr, msg_namelen),
            iov: core::mem::offset_of!(MsgHdr, msg_iov),
            iovlen: core::mem::offset_of!(MsgHdr, msg_iovlen),
            control: core::mem::offset_of!(MsgHdr, msg_control),
            controllen: core::mem::offset_of!(MsgHdr, msg_controllen),
            flags: core::mem::offset_of!(MsgHdr, msg_flags),
            word: size_of::<usize>(),
        }
    }
}

/// How a `struct cmsghdr` is laid out, which is two numbers that move
/// together: the header's length and what `CMSG_ALIGN` rounds to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CmsgLayout {
    /// `sizeof(struct cmsghdr)`: 16 native, 12 narrow.
    pub hdr_len: usize,
    /// `CMSG_ALIGN`'s granularity, which is `sizeof(long)`: 8 native, 4
    /// narrow.
    pub align: usize,
    /// How wide `cmsg_len` itself is, which is the `size_t` of this ABI. The
    /// two `int` that follow it are four bytes in both, so this is the only
    /// part of the header that moves.
    pub len_width: usize,
}

impl CmsgLayout {
    /// `CMSG_ALIGN(len)`.
    ///
    /// The mask is derived from `align`, which is a power of two by
    /// construction (`sizeof(long)`), so this is the same arithmetic the
    /// macro does and not a second rule.
    pub fn align_up(&self, len: usize) -> usize {
        (len + self.align - 1) & !(self.align - 1)
    }

    /// `cmsg_len`, read out of the front of a message.
    ///
    /// Narrow it is a four-byte `size_t`, native an eight-byte one, which is
    /// the field whose width a reader of the wrong ABI gets wrong first: a
    /// native read of a narrow message takes `cmsg_level` as the top half of
    /// the length, so a `SOL_SOCKET` message (level 1) comes back as a length
    /// of about four gigabytes.
    pub fn read_len(&self, bytes: &[u8]) -> Option<usize> {
        let field = bytes.get(..self.len_width)?;
        Some(match self.len_width {
            4 => u32::from_ne_bytes(field.try_into().ok()?) as usize,
            _ => u64::from_ne_bytes(field.try_into().ok()?) as usize,
        })
    }

    /// `cmsg_len` written into the front of a message, at this ABI's width.
    pub fn push_len(&self, buf: &mut alloc::vec::Vec<u8>, len: usize) {
        if self.len_width == 4 {
            buf.extend_from_slice(&(len as u32).to_ne_bytes());
        } else {
            buf.extend_from_slice(&(len as u64).to_ne_bytes());
        }
    }
}

/// The native `struct cmsghdr`: a `size_t`, then two `int`.
pub(crate) const NATIVE_CMSG: CmsgLayout = CmsgLayout {
    hdr_len: size_of::<usize>() + 8,
    align: size_of::<usize>(),
    len_width: size_of::<usize>(),
};

/// The i386 one, where both numbers are four.
pub(crate) const NARROW_CMSG: CmsgLayout = CmsgLayout {
    hdr_len: 12,
    align: 4,
    len_width: 4,
};

/// How `abi` lays out a control message.
pub(crate) fn cmsg_layout(abi: Abi) -> CmsgLayout {
    if abi.is_32bit() {
        NARROW_CMSG
    } else {
        NATIVE_CMSG
    }
}

impl Syscall<'_> {
    /// The layout of a `struct msghdr` as the **calling process** writes one,
    /// which is Linux's `in_compat_syscall()` asked of the process and not of
    /// the call site.
    pub(crate) fn msghdr_layout(&self) -> MsgHdrLayout {
        msghdr_layout(self.linux_process().abi())
    }

    /// The layout of a `struct cmsghdr` as the calling process writes one.
    pub(crate) fn cmsg_layout(&self) -> CmsgLayout {
        cmsg_layout(self.linux_process().abi())
    }

    /// Read one `struct msghdr` out of the caller's memory, at the caller's
    /// width, and hand back the native one the rest of the kernel uses.
    ///
    /// The narrow path reads the seven words one at a time rather than
    /// transmuting a block, because widening is what it is for: a four-byte
    /// `msg_iovlen` of 2 has to become an eight-byte 2 and not two bytes of
    /// the pointer that follows it.
    pub(crate) fn read_msghdr(&self, addr: usize) -> LxResult<MsgHdr> {
        let l = self.msghdr_layout();
        if l.word == size_of::<usize>() {
            let p: UserInPtr<MsgHdr> = addr.into();
            return p.read().map_err(Into::into);
        }
        // Seven four-byte words, which is the whole narrow structure. One
        // `read_array` rather than seven reads: it is one page check, and a
        // structure that straddles an unmapped boundary is `EFAULT` for the
        // whole call either way.
        let p: UserInPtr<u32> = addr.into();
        let w = p.read_array(THE_NARROW_SIZE / 4)?;
        Ok(MsgHdr::from_parts(
            w[0] as usize,
            w[1],
            w[2] as usize,
            w[3] as usize,
            w[4] as usize,
            w[5] as usize,
            w[6] as i32,
        ))
    }

    /// Write `msg_namelen` back, at the caller's offset.
    ///
    /// `socklen_t` is a `u32` in both ABIs, so only the offset moves.
    pub(crate) fn write_msg_namelen(&self, addr: usize, len: u32) -> LxResult {
        let l = self.msghdr_layout();
        let mut p: UserOutPtr<u32> = (addr + l.namelen).into();
        p.write(len)?;
        Ok(())
    }

    /// Write `msg_controllen` back, at the caller's offset **and width**.
    ///
    /// This is the one field where both move: a narrow caller's is four bytes
    /// at offset 20, and writing eight there would carry four bytes of the
    /// answer into `msg_flags`.
    pub(crate) fn write_msg_controllen(&self, addr: usize, len: usize) -> LxResult {
        let l = self.msghdr_layout();
        let at = addr + l.controllen;
        if l.word == 4 {
            let mut p: UserOutPtr<u32> = at.into();
            p.write(len as u32)?;
        } else {
            let mut p: UserOutPtr<usize> = at.into();
            p.write(len)?;
        }
        Ok(())
    }

    /// Write `msg_flags` back, at the caller's offset.
    pub(crate) fn write_msg_flags(&self, addr: usize, flags: i32) -> LxResult {
        let l = self.msghdr_layout();
        let mut p: UserOutPtr<i32> = (addr + l.flags).into();
        p.write(flags)?;
        Ok(())
    }
}

/// `struct mmsghdr` from `<sys/socket.h>`: one batch entry for
/// `sendmmsg`/`recvmmsg` --- a plain `msghdr` plus the per-message transfer
/// count the kernel writes back.
///
/// Only its layout is used (the stride and the `msg_len` offset); the
/// per-entry `msg_hdr` is re-read by the wrapped single-message syscalls
/// straight from user memory. Both batch calls walk the caller's array by
/// that stride and write the count at that offset, which is why the two come
/// from this declaration and not from arithmetic: a `msghdr` that grows a
/// field would otherwise leave the walk one stride short of the real
/// `mmsghdr` and put each count inside the NEXT entry, over its `msg_name`
/// pointer.
#[repr(C)]
struct MMsgHdr {
    msg_hdr: MsgHdr,
    msg_len: u32,
    _pad: u32,
}

/// The stride of a `struct mmsghdr` array and where the count sits in one
/// entry, for one ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MMsgHdrLayout {
    /// The distance between two entries.
    pub stride: usize,
    /// Where `msg_len` sits inside one. It is an `unsigned int` in both
    /// ABIs, so only the offset moves and never the width.
    pub len: usize,
}

/// A 32-bit `struct mmsghdr`: the 28-byte envelope and then the count.
///
/// **Not twice the narrow envelope, and not half the native entry.** The
/// native one is 56 + 4 rounded up to the structure's own 8-byte alignment,
/// which is 64; the narrow one is 28 + 4, which is 32 and needs no rounding
/// because an i386 `msghdr` aligns to 4. So the two strides happen to be 32
/// and 64 --- a factor of two, like the envelope --- while the count's
/// offsets are 28 and 56, which are not in that ratio at all. A walk that
/// halved the native numbers wholesale would put the count four bytes past
/// the end of each entry, inside the next one's `msg_name`.
const NARROW_MMSG: MMsgHdrLayout = MMsgHdrLayout {
    stride: 32,
    len: 28,
};

/// The native entry, straight out of the declaration above.
pub(crate) const NATIVE_MMSG: MMsgHdrLayout = MMsgHdrLayout {
    stride: size_of::<MMsgHdr>(),
    len: core::mem::offset_of!(MMsgHdr, msg_len),
};

/// How `abi` lays out an array of them.
pub(crate) fn mmsghdr_layout(abi: Abi) -> MMsgHdrLayout {
    if abi.is_32bit() {
        NARROW_MMSG
    } else {
        NATIVE_MMSG
    }
}

impl Syscall<'_> {
    /// The layout of a `struct mmsghdr` array as the calling process writes
    /// one.
    pub(crate) fn mmsghdr_layout(&self) -> MMsgHdrLayout {
        mmsghdr_layout(self.linux_process().abi())
    }
}

#[cfg(test)]
mod tests;
