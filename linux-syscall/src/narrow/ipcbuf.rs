//! `semid64_ds`, `msqid64_ds` and `shmid64_ds`: the three structures the
//! `*ctl` of System V hands back, and the only ones in this crate where a
//! 32-bit layout is **not** the native one with its words halved.
//!
//! The multiplexer that reaches them is [`crate::compat32::ipc`]; this module
//! is the layout, and it lives here rather than there because `semctl`,
//! `msgctl` and `shmctl` are the same handlers at both widths --- there is no
//! separate i386 number for them, the way there is for `fstat64` --- and this
//! crate builds for architectures that have no compatibility mode at all.
//!
//! ## Why halving the words is wrong here
//!
//! Every one of these three begins with a `struct ipc64_perm`, which is 36
//! bytes on i386 and 48 here. The difference is not a field: it is that its
//! two trailing `__kernel_ulong_t` reserved words are four bytes there and
//! eight here, and that the native one is padded from 28 to 32 to 8-align
//! them. So the whole tail of every structure moves, and it moves by a
//! different amount than the fields' own widths would suggest.
//!
//! Then the times. On a 32-bit architecture Linux does not store a
//! `__kernel_long_t` seconds and a reserved word beside it: it splits each
//! time into a **pair** of `unsigned long`, a low half and a `_high` half
//! (`arch/x86/include/uapi/asm/sembuf.h` and its two neighbours). So what
//! reads as padding in the native layout is live data there, and a `semctl`
//! that halved its words would report the seconds and then write the
//! seconds' own high half into the field that holds `nsems`.
//!
//! Because i386 is little-endian and the `_high` half always follows its low
//! half, such a pair **is** a 64-bit little-endian value at a 4-aligned
//! offset. That is what makes this module short: the split is a layout fact,
//! not an arithmetic one, and each structure below spells it out as the two
//! `u32` halves that the 4-byte alignment forces --- the same way
//! [`crate::compat32::stat::Stat64`] spells out `st_size`.
//!
//! ## What a 32-bit caller got before
//!
//! Nothing: the multiplexer refused every command that carries one of these
//! with `ENOSYS`, on purpose, rather than write 104 bytes into the 64-byte
//! buffer a 32-bit `semctl(IPC_STAT)` passes. That refusal is what this
//! module replaces, and the per-family refusal lists in
//! [`crate::compat32::ipc`] shrink to what is still missing.
//!
//! ## What `abi32` means here
//!
//! The switch below is the `abi32` of the rest of [`crate::narrow`], which
//! is `Abi::is_32bit()` of the calling process, while everything in this
//! module is specifically **i386**: little-endian, and the `_high` half
//! after its low half. Those agree because `Abi` has exactly one 32-bit
//! variant and [`crate::compat32`] is x86_64-only --- compatibility mode is
//! an x86 thing. A second 32-bit personality with another layout would need
//! the choice made on the ABI and not on its width, and the place for that
//! is `Abi` itself, not here: the whole `narrow` family halves
//! little-endian words and would need the same.
//!
//! ## How the layout is checked
//!
//! The sizes and the offset of every field are asserted against Linux's own
//! headers, below. They are not derived from the native structures, because
//! a table derived from the thing it is meant to check cannot disagree with
//! it.

use super::{wide_get_unaligned, wide_put_unaligned};
use crate::ipc::{ShmInfo, ShmInfo64};
use kernel_hal::user::{UserInPtr, UserOutPtr};
use linux_object::error::LxResult;
use linux_object::ipc::{IpcPerm, MsqidDs, SemidDs, ShmidDs};

/// `struct ipc64_perm` as i386 lays it out: 36 bytes, 4-aligned.
///
/// `seq` and `__pad2` are an `unsigned short` each in Linux's header and are
/// one `u32` here, which is what the native [`IpcPerm`] does with them too
/// (`__seq`). Keeping them as one field is the only place this module does
/// not follow the header field for field, and it costs nothing: the two
/// halves are never read apart.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct IpcPerm32 {
    /// `key` (offset 0).
    key: u32,
    /// `uid` (offset 4).
    uid: u32,
    /// `gid` (offset 8).
    gid: u32,
    /// `cuid` (offset 12).
    cuid: u32,
    /// `cgid` (offset 16).
    cgid: u32,
    /// `mode` (offset 20). `__pad1` after it is zero bytes wide, because
    /// `__kernel_mode_t` is already four.
    mode: u32,
    /// `seq` and `__pad2` (offset 24).
    seq: u32,
    /// `__unused1` (offset 28): four bytes here, eight natively.
    __unused1: u32,
    /// `__unused2` (offset 32).
    __unused2: u32,
}

impl From<IpcPerm> for IpcPerm32 {
    fn from(p: IpcPerm) -> Self {
        Self {
            key: p.key,
            uid: p.uid,
            gid: p.gid,
            cuid: p.cuid,
            cgid: p.cgid,
            mode: p.mode,
            seq: p.__seq,
            __unused1: 0,
            __unused2: 0,
        }
    }
}

impl From<IpcPerm32> for IpcPerm {
    fn from(p: IpcPerm32) -> Self {
        Self {
            key: p.key,
            uid: p.uid,
            gid: p.gid,
            cuid: p.cuid,
            cgid: p.cgid,
            mode: p.mode,
            __seq: p.seq,
            __pad1: 0,
            __pad2: 0,
        }
    }
}

/// A time as the 32-bit layouts carry it: the low half, then the `_high`
/// half. Little-endian, so the pair is the 64-bit value.
///
/// The arithmetic is in `u64` and not in `usize` deliberately: a `>> 32` of
/// a `usize` is undefined on a 32-bit target, and while nothing builds this
/// crate for one today (the three architectures are all 64-bit), the shift
/// is the kind of thing that is wrong long before anyone finds out. Pointed
/// out by Copilot on the #2409.
fn split(t: usize) -> (u32, u32) {
    let t = t as u64;
    (t as u32, (t >> 32) as u32)
}

/// [`split`] the other way. On a 32-bit host the join would truncate, which
/// is the honest answer there: the native structures carry a `usize`.
fn join(lo: u32, hi: u32) -> usize {
    (lo as u64 | ((hi as u64) << 32)) as usize
}

/// `struct semid64_ds` as i386 lays it out: 64 bytes, against 104 here.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SemidDs32 {
    /// `sem_perm` (offset 0).
    perm: IpcPerm32,
    /// `sem_otime` (offset 36).
    otime_lo: u32,
    /// `sem_otime_high` (offset 40).
    otime_hi: u32,
    /// `sem_ctime` (offset 44).
    ctime_lo: u32,
    /// `sem_ctime_high` (offset 48).
    ctime_hi: u32,
    /// `sem_nsems` (offset 52).
    nsems: u32,
    /// `__unused3` (offset 56).
    __unused3: u32,
    /// `__unused4` (offset 60).
    __unused4: u32,
}

impl From<SemidDs> for SemidDs32 {
    fn from(ds: SemidDs) -> Self {
        let (otime_lo, otime_hi) = split(ds.otime);
        let (ctime_lo, ctime_hi) = split(ds.ctime);
        Self {
            perm: ds.perm.into(),
            otime_lo,
            otime_hi,
            ctime_lo,
            ctime_hi,
            // `unsigned long` there, so the count truncates, as it does in
            // Linux's own `copy_compat_semid_to_user`.
            nsems: ds.nsems as u32,
            __unused3: 0,
            __unused4: 0,
        }
    }
}

impl From<SemidDs32> for SemidDs {
    fn from(ds: SemidDs32) -> Self {
        // Field by field and not a struct literal: the reserved tail of
        // each native structure is private, which is right --- nothing
        // outside `linux-object` has any business setting it.
        let mut out = SemidDs::default();
        out.perm = ds.perm.into();
        out.otime = join(ds.otime_lo, ds.otime_hi);
        out.ctime = join(ds.ctime_lo, ds.ctime_hi);
        out.nsems = ds.nsems as usize;
        out
    }
}

/// `struct msqid64_ds` as i386 lays it out: 88 bytes, against 120 here.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MsqidDs32 {
    /// `msg_perm` (offset 0).
    perm: IpcPerm32,
    /// `msg_stime` (offset 36).
    stime_lo: u32,
    /// `msg_stime_high` (offset 40).
    stime_hi: u32,
    /// `msg_rtime` (offset 44).
    rtime_lo: u32,
    /// `msg_rtime_high` (offset 48).
    rtime_hi: u32,
    /// `msg_ctime` (offset 52).
    ctime_lo: u32,
    /// `msg_ctime_high` (offset 56).
    ctime_hi: u32,
    /// `msg_cbytes` (offset 60).
    cbytes: u32,
    /// `msg_qnum` (offset 64).
    qnum: u32,
    /// `msg_qbytes` (offset 68). The one field of these three structures
    /// that `IPC_SET` reads as well as writes, so its offset is load-bearing
    /// in both directions.
    qbytes: u32,
    /// `msg_lspid` (offset 72): a `pid_t`, four bytes at both widths.
    lspid: u32,
    /// `msg_lrpid` (offset 76).
    lrpid: u32,
    /// `__unused4` (offset 80).
    __unused4: u32,
    /// `__unused5` (offset 84).
    __unused5: u32,
}

impl From<MsqidDs> for MsqidDs32 {
    fn from(ds: MsqidDs) -> Self {
        let (stime_lo, stime_hi) = split(ds.stime);
        let (rtime_lo, rtime_hi) = split(ds.rtime);
        let (ctime_lo, ctime_hi) = split(ds.ctime);
        Self {
            perm: ds.perm.into(),
            stime_lo,
            stime_hi,
            rtime_lo,
            rtime_hi,
            ctime_lo,
            ctime_hi,
            cbytes: ds.cbytes as u32,
            qnum: ds.qnum as u32,
            qbytes: ds.qbytes as u32,
            lspid: ds.lspid,
            lrpid: ds.lrpid,
            __unused4: 0,
            __unused5: 0,
        }
    }
}

impl From<MsqidDs32> for MsqidDs {
    fn from(ds: MsqidDs32) -> Self {
        let mut out = MsqidDs::default();
        out.perm = ds.perm.into();
        out.stime = join(ds.stime_lo, ds.stime_hi);
        out.rtime = join(ds.rtime_lo, ds.rtime_hi);
        out.ctime = join(ds.ctime_lo, ds.ctime_hi);
        out.cbytes = ds.cbytes as usize;
        out.qnum = ds.qnum as usize;
        out.qbytes = ds.qbytes as usize;
        out.lspid = ds.lspid;
        out.lrpid = ds.lrpid;
        out
    }
}

/// `struct shmid64_ds` as i386 lays it out: 84 bytes, against 112 here.
///
/// The odd one of the three: `shm_segsz` comes **before** the times, so this
/// is the only one whose first time does not begin at offset 36.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShmidDs32 {
    /// `shm_perm` (offset 0).
    perm: IpcPerm32,
    /// `shm_segsz` (offset 36): a `size_t`, so four bytes there.
    segsz: u32,
    /// `shm_atime` (offset 40).
    atime_lo: u32,
    /// `shm_atime_high` (offset 44).
    atime_hi: u32,
    /// `shm_dtime` (offset 48).
    dtime_lo: u32,
    /// `shm_dtime_high` (offset 52).
    dtime_hi: u32,
    /// `shm_ctime` (offset 56).
    ctime_lo: u32,
    /// `shm_ctime_high` (offset 60).
    ctime_hi: u32,
    /// `shm_cpid` (offset 64).
    cpid: u32,
    /// `shm_lpid` (offset 68).
    lpid: u32,
    /// `shm_nattch` (offset 72).
    nattch: u32,
    /// `__unused4` (offset 76).
    __unused4: u32,
    /// `__unused5` (offset 80).
    __unused5: u32,
}

impl From<ShmidDs> for ShmidDs32 {
    fn from(ds: ShmidDs) -> Self {
        let (atime_lo, atime_hi) = split(ds.atime);
        let (dtime_lo, dtime_hi) = split(ds.dtime);
        let (ctime_lo, ctime_hi) = split(ds.ctime);
        Self {
            perm: ds.perm.into(),
            segsz: ds.segsz as u32,
            atime_lo,
            atime_hi,
            dtime_lo,
            dtime_hi,
            ctime_lo,
            ctime_hi,
            cpid: ds.cpid,
            lpid: ds.lpid,
            nattch: ds.nattch as u32,
            __unused4: 0,
            __unused5: 0,
        }
    }
}

impl From<ShmidDs32> for ShmidDs {
    fn from(ds: ShmidDs32) -> Self {
        let mut out = ShmidDs::default();
        out.perm = ds.perm.into();
        out.segsz = ds.segsz as usize;
        out.atime = join(ds.atime_lo, ds.atime_hi);
        out.dtime = join(ds.dtime_lo, ds.dtime_hi);
        out.ctime = join(ds.ctime_lo, ds.ctime_hi);
        out.cpid = ds.cpid;
        out.lpid = ds.lpid;
        out.nattch = ds.nattch as usize;
        out
    }
}

/// The four write-outs and three read-ins, one pair per family.
///
/// Each is the whole of what a handler needs to say about the width: the
/// `abi32` comes from the process ([`crate::Syscall::narrow_abi`]), and both
/// widths go through the unaligned accessors, because an i386 buffer is
/// 4-aligned and `copy_to_user` demands no alignment at either width. See
/// [`super::narrow_put`].
macro_rules! ds_pair {
    ($put:ident, $get:ident, $native:ty, $narrow:ty, $what:literal) => {
        #[doc = concat!("Write a `", $what, "` at the caller's width.")]
        pub(crate) fn $put(abi32: bool, addr: usize, ds: $native) -> LxResult<()> {
            if abi32 {
                let mut ptr: UserOutPtr<$narrow> = addr.into();
                return wide_put_unaligned(true, &mut ptr, ds.into());
            }
            let mut ptr: UserOutPtr<$native> = addr.into();
            wide_put_unaligned(false, &mut ptr, ds)
        }

        #[doc = concat!("Read a `", $what, "` at the caller's width.")]
        pub(crate) fn $get(abi32: bool, addr: usize) -> LxResult<$native> {
            if abi32 {
                let ptr: UserInPtr<$narrow> = addr.into();
                return Ok(wide_get_unaligned(true, &ptr)?.into());
            }
            let ptr: UserInPtr<$native> = addr.into();
            wide_get_unaligned(false, &ptr)
        }
    };
}

/// `struct shminfo64`, which IS a pure run of words and so needs no layout
/// of its own: nine `__kernel_ulong_t` and nothing else, four bytes each
/// there and eight here. 36 bytes against 72.
///
/// This is the one structure of the family that the word-halving of
/// [`super::Narrow`] serves as it stands, and saying so is the point: the
/// trait's contract is that the structure has no padding and no field that
/// is not a word in EITHER ABI, and `shminfo64` is the only one here that
/// can make that claim.
impl super::Narrow for ShmInfo64 {
    const WORDS: usize = 9;

    fn to_words(self, out: &mut [u64]) {
        for (slot, field) in out[0..9].iter_mut().zip([
            self.shmmax,
            self.shmmin,
            self.shmmni,
            self.shmseg,
            self.shmall,
            self.unused[0],
            self.unused[1],
            self.unused[2],
            self.unused[3],
        ]) {
            *slot = field as u64;
        }
    }

    fn from_words(words: &[u64]) -> Self {
        let at = |i: usize| words[i] as usize;
        Self {
            shmmax: at(0),
            shmmin: at(1),
            shmmni: at(2),
            shmseg: at(3),
            shmall: at(4),
            unused: [at(5), at(6), at(7), at(8)],
        }
    }
}

/// `struct shm_info` as i386 lays it out: 24 bytes against 48, and **not** a
/// pure run of words.
///
/// It leads with an `int used_ids` and then five `__kernel_ulong_t`. At this
/// width the `int` and a word are both four bytes, so the whole thing is six
/// four-byte fields; natively the `int` is followed by **four bytes of
/// padding** to 8-align `shm_tot`. So the native image is not nine words and
/// [`super::Narrow`] cannot honestly be claimed for it --- the contract is
/// about both ABIs --- which is why this one gets a layout and `shminfo64`
/// above does not.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ShmInfo32 {
    /// `used_ids` (offset 0): an `int` at both widths.
    used_ids: i32,
    /// `shm_tot` (offset 4). Natively at 8, because of the padding.
    shm_tot: u32,
    /// `shm_rss` (offset 8).
    shm_rss: u32,
    /// `shm_swp` (offset 12).
    shm_swp: u32,
    /// `swap_attempts` (offset 16): unused since Linux 2.4.
    swap_attempts: u32,
    /// `swap_successes` (offset 20).
    swap_successes: u32,
}

impl From<ShmInfo> for ShmInfo32 {
    fn from(i: ShmInfo) -> Self {
        Self {
            used_ids: i.used_ids,
            shm_tot: i.shm_tot as u32,
            shm_rss: i.shm_rss as u32,
            shm_swp: i.shm_swp as u32,
            swap_attempts: i.swap_attempts as u32,
            swap_successes: i.swap_successes as u32,
        }
    }
}

/// Write a `struct shm_info` at the caller's width. Out only: nothing reads
/// one in.
pub(crate) fn put_shm_info(abi32: bool, addr: usize, info: ShmInfo) -> LxResult<()> {
    if abi32 {
        let mut ptr: UserOutPtr<ShmInfo32> = addr.into();
        return wide_put_unaligned(true, &mut ptr, info.into());
    }
    let mut ptr: UserOutPtr<ShmInfo> = addr.into();
    wide_put_unaligned(false, &mut ptr, info)
}

ds_pair!(put_semid_ds, get_semid_ds, SemidDs, SemidDs32, "semid64_ds");
ds_pair!(put_msqid_ds, get_msqid_ds, MsqidDs, MsqidDs32, "msqid64_ds");
ds_pair!(put_shmid_ds, get_shmid_ds, ShmidDs, ShmidDs32, "shmid64_ds");

#[cfg(test)]
mod tests;
