//! The calls that carry a 64-bit value through 32-bit registers.
//!
//! A register on i386 holds 32 bits, and a file offset is 64. So every call
//! that takes an offset got a second number on i386 with the offset split
//! across two arguments, and `mmap` got one that counts the offset in pages
//! instead of bytes. There is nothing clever in any of them --- each is two
//! halves put back together --- and nothing forgiving either: **the two halves
//! are not in the same order in every call**, and a pair swapped silently
//! reads four gigabytes into a file instead of four bytes.
//!
//! So the order of each one is stated where it is used, taken from Linux's own
//! `arch/x86/ia32/sys_ia32.c` and `fs/read_write.c`:
//!
//! | call | i386 arguments |
//! |---|---|
//! | `_llseek` (140) | fd, **high**, **low**, result, whence |
//! | `pread64` (180) | fd, buf, count, **low**, **high** |
//! | `pwrite64` (181) | fd, buf, count, **low**, **high** |
//! | `truncate64` (193) | path, **low**, **high** |
//! | `ftruncate64` (194) | fd, **low**, **high** |
//! | `mmap2` (192) | addr, len, prot, flags, fd, offset **in 4 KiB units** |
//!
//! `_llseek` is the odd one, and it is the one that is odd in Linux too: it is
//! not an i386 call at all but a generic one, from the days when every 32-bit
//! architecture needed it, and it puts the high half first because that is
//! how it was declared.
//!
//! Each half arrives zero-extended: the dispatch in `loader/src/linux.rs`
//! masks every argument register to 32 bits for a compat thread, because the
//! upper half of `rbx` belongs to whatever ran before and is not the
//! program's. So the halves are joined without sign extension, which is right
//! for an offset that the native handler rejects when negative anyway.

use super::*;
use linux_object::error::LxError;

/// The two halves of a 64-bit value, low first.
///
/// Shared with [`super::iovec`], whose `preadv`/`pwritev` split their offset
/// the same way `pread64` does. One function, so the order can be argued
/// about in one place.
///
/// Four bytes wide each, because that is what a 32-bit register holds, and
/// the mask says so rather than trusting that the dispatch already did it:
/// this is the one function that would turn a stray upper half into an offset
/// of several terabytes.
pub(super) fn join(low: usize, high: usize) -> u64 {
    ((high as u64 & 0xffff_ffff) << 32) | (low as u64 & 0xffff_ffff)
}

impl Syscall<'_> {
    /// `mmap2(2)`, i386 192: `mmap` with the offset in 4 KiB units.
    ///
    /// The unit is 4096 whatever the page size is --- it is part of the ABI,
    /// not of the machine --- which is what lets a 32-bit `unsigned long`
    /// reach an offset of 16 TiB. `mmap`(90) is a different call again, with
    /// its arguments in a block of memory, and it is not this one.
    pub async fn sys_mmap2(
        &mut self,
        addr: usize,
        len: usize,
        prot: usize,
        flags: usize,
        fd: usize,
        pages: usize,
    ) -> SysResult {
        /// The unit of `mmap2`'s offset, fixed by the ABI at 4 KiB.
        const MMAP2_UNIT: u64 = 4096;
        let offset = (pages as u64 & 0xffff_ffff) * MMAP2_UNIT;
        self.sys_mmap(addr, len, prot, flags, (fd as i32).into(), offset)
            .await
    }

    /// `_llseek(2)`, i386 140: `lseek` whose offset arrives in two registers,
    /// **high first**, and whose result goes to a pointer rather than into
    /// the return value --- because a 32-bit return value could not hold it.
    pub fn sys_llseek(
        &self,
        fd: usize,
        offset_high: usize,
        offset_low: usize,
        result: usize,
        whence: usize,
    ) -> SysResult {
        let offset = join(offset_low, offset_high) as i64;
        let at = self.sys_lseek((fd as i32).into(), offset, whence)?;
        let mut out: UserOutPtr<u64> = result.into();
        out.write(at as u64)?;
        // Not the offset: `_llseek` returns 0 on success and leaves the
        // offset in `*result`. Returning the offset here would be a value a
        // 32-bit program reads as an error for any file over 2 GiB.
        Ok(0)
    }

    /// `pread64(2)`, i386 180: the offset in two registers, **low first**.
    pub async fn sys_pread64(
        &self,
        fd: usize,
        base: usize,
        len: usize,
        offset_low: usize,
        offset_high: usize,
    ) -> SysResult {
        self.sys_pread(
            (fd as i32).into(),
            base.into(),
            len,
            join(offset_low, offset_high),
        )
        .await
    }

    /// `pwrite64(2)`, i386 181: the offset in two registers, **low first**.
    pub fn sys_pwrite64(
        &self,
        fd: usize,
        base: usize,
        len: usize,
        offset_low: usize,
        offset_high: usize,
    ) -> SysResult {
        self.sys_pwrite(
            (fd as i32).into(),
            base.into(),
            len,
            join(offset_low, offset_high),
        )
    }

    /// `truncate64(2)`, i386 193: the length in two registers, **low first**.
    pub fn sys_truncate64(&self, path: usize, len_low: usize, len_high: usize) -> SysResult {
        let len = join(len_low, len_high);
        self.sys_truncate(
            path.into(),
            usize::try_from(len).map_err(|_| LxError::EINVAL)?,
        )
    }

    /// `ftruncate64(2)`, i386 194: the length in two registers, **low first**.
    pub fn sys_ftruncate64(&self, fd: usize, len_low: usize, len_high: usize) -> SysResult {
        let len = join(len_low, len_high);
        self.sys_ftruncate(
            (fd as i32).into(),
            usize::try_from(len).map_err(|_| LxError::EINVAL)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::join;

    /// Joining two halves, and the three ways it has gone wrong elsewhere:
    /// the halves swapped, the high half sign-extended, and an upper half
    /// left over from whatever ran before leaking into the offset.
    #[test]
    fn two_halves_make_one_offset() {
        assert_eq!(join(0, 0), 0);
        assert_eq!(join(1, 0), 1);
        assert_eq!(join(0, 1), 1 << 32);
        assert_eq!(join(0xffff_ffff, 0), 0xffff_ffff);
        assert_eq!(join(0x8000_0000, 0), 0x8000_0000, "the low half is signed");
        assert_eq!(join(0, 0x8000_0000), 0x8000_0000_0000_0000);
        // And the mask: a register whose upper half is not the program's.
        assert_eq!(join(0xdead_0000_0000_0001, 0), 1);
        assert_eq!(join(0, 0xbeef_0000_0000_0002), 2 << 32);
    }
}
