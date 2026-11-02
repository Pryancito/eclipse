//! `struct stat64`, which is not `struct stat` with the fields made smaller.
//!
//! i386 has three `stat` structures, and this kernel answers one of them. The
//! oldest two (`stat`, 106, and `oldstat`, 18) carry a 32-bit inode, a 32-bit
//! size and a 16-bit device, which is why Linux grew a third: `stat64`, with
//! `st_ino`, `st_size`, `st_dev` and `st_blocks` widened to 64 bits. Every
//! libc built in this century calls that one, and the two it replaced are not
//! mapped here. See the dispatch in [`super`].
//!
//! ## What makes it a conversion and not a cast
//!
//! Three things, and none of them is the width of a field on its own:
//!
//! 1. **The order is different.** In the native `struct stat` the inode is the
//!    second field; in `stat64` the real `st_ino` is the **last**, at offset
//!    88, and offset 12 holds `__st_ino`, the same inode truncated to 32 bits,
//!    left there for the binaries compiled against the old structure. A
//!    program that reads one and a kernel that fills the other do not disagree
//!    loudly: they disagree about which field is which.
//! 2. **64-bit fields sit at 4-byte alignment.** The i386 System V ABI aligns
//!    a `long long` to 4, so `st_size` begins at offset 44. A Rust
//!    `#[repr(C)]` struct with a `u64` there would be padded to 48 and the
//!    whole tail would shift by four bytes, so every 64-bit field is spelled
//!    as its two halves. That also makes each half's offset something a test
//!    can assert.
//! 3. **The timestamps are 32-bit seconds**, so this structure cannot describe
//!    a time past 2038 --- which is the whole reason the `statx` and
//!    `*_time64` families exist. Linux truncates here and so does this; a
//!    32-bit program that wants the real answer has to ask `statx`.
//!
//! ## How the layout is checked
//!
//! By assertion against the numbers in Linux's
//! `arch/x86/include/uapi/asm/stat.h`, which is the authority, and then **by
//! running a 32-bit `fstat64` on the Linux of the build container**: see the
//! steps `tools/i386-hello` has for it. An offset table that is wrong by four
//! bytes passes every unit test that was written from the same wrong table.

use super::*;
use linux_object::error::LxError;
use linux_object::fs::vfs::Metadata;

/// `struct stat64` as i386 lays it out: 96 bytes, 4-byte aligned, every
/// 64-bit field split into the two halves that alignment forces.
///
/// The field names are Linux's, `__st_ino` included --- it is not padding and
/// it is not a mistake, it is the 32-bit inode the old structure had, kept at
/// the offset it had.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Stat64 {
    /// `st_dev`, low half (offset 0).
    st_dev_lo: u32,
    /// `st_dev`, high half (offset 4).
    st_dev_hi: u32,
    /// `__pad0` (offset 8).
    _pad0: u32,
    /// `__st_ino` (offset 12): the inode truncated to 32 bits, for the
    /// binaries that were compiled against `struct stat`.
    __st_ino: u32,
    /// `st_mode` (offset 16).
    st_mode: u32,
    /// `st_nlink` (offset 20).
    st_nlink: u32,
    /// `st_uid` (offset 24).
    st_uid: u32,
    /// `st_gid` (offset 28).
    st_gid: u32,
    /// `st_rdev`, low half (offset 32).
    st_rdev_lo: u32,
    /// `st_rdev`, high half (offset 36).
    st_rdev_hi: u32,
    /// `__pad3` (offset 40).
    _pad3: u32,
    /// `st_size`, low half (offset 44). Note the offset: this is the field
    /// that is not 8-aligned.
    st_size_lo: u32,
    /// `st_size`, high half (offset 48).
    st_size_hi: u32,
    /// `st_blksize` (offset 52).
    st_blksize: u32,
    /// `st_blocks`, low half (offset 56), in 512-byte units.
    st_blocks_lo: u32,
    /// `st_blocks`, high half (offset 60).
    st_blocks_hi: u32,
    /// `st_atime` (offset 64), seconds, 32 bits wide.
    st_atime: u32,
    /// `st_atime_nsec` (offset 68).
    st_atime_nsec: u32,
    /// `st_mtime` (offset 72).
    st_mtime: u32,
    /// `st_mtime_nsec` (offset 76).
    st_mtime_nsec: u32,
    /// `st_ctime` (offset 80).
    st_ctime: u32,
    /// `st_ctime_nsec` (offset 84).
    st_ctime_nsec: u32,
    /// `st_ino`, low half (offset 88): the real inode, all 64 bits of it, at
    /// the end of the structure rather than the beginning.
    st_ino_lo: u32,
    /// `st_ino`, high half (offset 92).
    st_ino_hi: u32,
}

/// The size `sizeof(struct stat64)` has on i386. A program passes a buffer of
/// exactly this, so a structure of any other size writes past it.
pub const STAT64_SIZE: usize = 96;

const _: () = {
    assert!(core::mem::size_of::<Stat64>() == STAT64_SIZE);
    assert!(core::mem::align_of::<Stat64>() == 4);
    // The offsets, from `arch/x86/include/uapi/asm/stat.h`. These are the
    // numbers a wrong structure gets wrong, and the two that matter most are
    // `st_size` at 44 (not 48, which is where 8-byte alignment would put it)
    // and `st_ino` at 88 (not 12, which is where `struct stat` keeps it).
    assert!(core::mem::offset_of!(Stat64, st_dev_lo) == 0);
    assert!(core::mem::offset_of!(Stat64, __st_ino) == 12);
    assert!(core::mem::offset_of!(Stat64, st_mode) == 16);
    assert!(core::mem::offset_of!(Stat64, st_nlink) == 20);
    assert!(core::mem::offset_of!(Stat64, st_uid) == 24);
    assert!(core::mem::offset_of!(Stat64, st_gid) == 28);
    assert!(core::mem::offset_of!(Stat64, st_rdev_lo) == 32);
    assert!(core::mem::offset_of!(Stat64, st_size_lo) == 44);
    assert!(core::mem::offset_of!(Stat64, st_blksize) == 52);
    assert!(core::mem::offset_of!(Stat64, st_blocks_lo) == 56);
    assert!(core::mem::offset_of!(Stat64, st_atime) == 64);
    assert!(core::mem::offset_of!(Stat64, st_mtime) == 72);
    assert!(core::mem::offset_of!(Stat64, st_ctime) == 80);
    assert!(core::mem::offset_of!(Stat64, st_ino_lo) == 88);
};

/// The low and high halves of a 64-bit value, in the order i386 stores them
/// (little-endian: low first).
fn split(value: u64) -> (u32, u32) {
    (value as u32, (value >> 32) as u32)
}

impl Stat64 {
    /// Read `st_size` back, for the tests: the halves joined again.
    #[cfg(test)]
    fn size(&self) -> u64 {
        u64::from(self.st_size_lo) | (u64::from(self.st_size_hi) << 32)
    }

    /// Read `st_ino` back, for the tests.
    #[cfg(test)]
    fn ino(&self) -> u64 {
        u64::from(self.st_ino_lo) | (u64::from(self.st_ino_hi) << 32)
    }
}

impl From<Metadata> for Stat64 {
    fn from(info: Metadata) -> Self {
        let (st_dev_lo, st_dev_hi) = split(info.dev as u64);
        let (st_rdev_lo, st_rdev_hi) = split(info.rdev as u64);
        let (st_size_lo, st_size_hi) = split(info.size as u64);
        let (st_blocks_lo, st_blocks_hi) = split(info.blocks as u64);
        let (st_ino_lo, st_ino_hi) = split(info.inode as u64);
        Stat64 {
            st_dev_lo,
            st_dev_hi,
            _pad0: 0,
            // Truncated on purpose, and Linux truncates it the same way
            // (`tmp.__st_ino = (u32)stat->ino`). It is the old field; the real
            // one is `st_ino` at the end.
            __st_ino: info.inode as u32,
            st_mode: crate::file::StatMode::from_type_mode(info.type_, info.mode).bits(),
            st_nlink: info.nlinks as u32,
            st_uid: info.uid as u32,
            st_gid: info.gid as u32,
            st_rdev_lo,
            st_rdev_hi,
            _pad3: 0,
            st_size_lo,
            st_size_hi,
            st_blksize: info.blk_size as u32,
            st_blocks_lo,
            st_blocks_hi,
            // Seconds truncated to 32 bits, as Linux does here: this structure
            // has no room for more, which is what `statx` is for. A `u32` is
            // right rather than an `i32` because the field is an
            // `unsigned long`; either way the bits are the same and a time
            // outside 1970..2038 is wrong in both kernels.
            st_atime: info.atime.sec as u32,
            st_atime_nsec: info.atime.nsec as u32,
            st_mtime: info.mtime.sec as u32,
            st_mtime_nsec: info.mtime.nsec as u32,
            st_ctime: info.ctime.sec as u32,
            st_ctime_nsec: info.ctime.nsec as u32,
            st_ino_lo,
            st_ino_hi,
        }
    }
}

impl Syscall<'_> {
    /// `stat64(2)`, i386 195.
    pub fn sys_stat64(&self, path: usize, stat_ptr: usize) -> SysResult {
        self.fstatat64_inner(FileDesc::CWD, path, stat_ptr, 0)
    }

    /// `lstat64(2)`, i386 196: the link itself, not what it points at.
    pub fn sys_lstat64(&self, path: usize, stat_ptr: usize) -> SysResult {
        self.fstatat64_inner(
            FileDesc::CWD,
            path,
            stat_ptr,
            crate::file::AtFlags::SYMLINK_NOFOLLOW.bits(),
        )
    }

    /// `fstat64(2)`, i386 197.
    ///
    /// Not `fstatat64(fd, "", AT_EMPTY_PATH)` written out: it goes through
    /// `get_file_like`, so it answers for every kind of descriptor and not
    /// only the ones with an inode behind them --- a socket, an eventfd, an
    /// epoll set. The native `sys_fstat` takes the same path for the same
    /// reason.
    pub fn sys_fstat64(&self, fd: usize, stat_ptr: usize) -> SysResult {
        let fd = FileDesc::from(fd as i32);
        let meta = self.linux_process().get_file_like(fd)?.metadata()?;
        self.write_stat64(stat_ptr, meta)
    }

    /// `fstatat64(2)`, i386 300.
    pub fn sys_fstatat64(
        &self,
        dirfd: usize,
        path: usize,
        stat_ptr: usize,
        flags: usize,
    ) -> SysResult {
        self.fstatat64_inner(FileDesc::from(dirfd as i32), path, stat_ptr, flags)
    }

    /// The whole family, once: the shared lookup, then the 32-bit layout.
    fn fstatat64_inner(
        &self,
        dirfd: FileDesc,
        path: usize,
        stat_ptr: usize,
        flags: usize,
    ) -> SysResult {
        let meta = self.stat_metadata(dirfd, path.into(), flags)?;
        self.write_stat64(stat_ptr, meta)
    }

    /// Lay the answer out and copy it to the 96 bytes the program provided.
    fn write_stat64(&self, stat_ptr: usize, meta: Metadata) -> SysResult {
        if stat_ptr == 0 {
            return Err(LxError::EFAULT);
        }
        let mut out: UserOutPtr<Stat64> = stat_ptr.into();
        out.write(meta.into())?;
        Ok(0)
    }
}

#[cfg(test)]
mod tests;
