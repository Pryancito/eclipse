//! The layout of the three `*id64_ds` at 32 bits, against Linux's headers.
//!
//! Every number below is from `arch/x86/include/uapi/asm/sembuf.h`,
//! `msgbuf.h` and `shmbuf.h` and from
//! `include/uapi/asm-generic/ipcbuf.h`, read under `#ifdef __i386__`. They
//! are written here as constants rather than computed from the native
//! structures on purpose: a table derived from the thing it checks cannot
//! disagree with it, which is how the `stat64` offsets were wrong by four
//! bytes and passed every test written from the same wrong table
//! ([`crate::compat32::stat`]).

use super::*;
use core::mem::{align_of, offset_of, size_of};

/// `struct ipc64_perm` under `__i386__`: six `__u32`, `seq` and `__pad2` as
/// two `unsigned short`, then two `__kernel_ulong_t` of four bytes each.
#[test]
fn the_ipc_perm_is_thirty_six_bytes_and_not_forty_eight() {
    assert_eq!(size_of::<IpcPerm32>(), 36);
    // Four, not eight: nothing in it is wider than a word, so nothing pads
    // it, and that is exactly why the native one is 48 and not 44.
    assert_eq!(align_of::<IpcPerm32>(), 4);
    assert_eq!(size_of::<IpcPerm>(), 48);

    assert_eq!(offset_of!(IpcPerm32, key), 0);
    assert_eq!(offset_of!(IpcPerm32, uid), 4);
    assert_eq!(offset_of!(IpcPerm32, gid), 8);
    assert_eq!(offset_of!(IpcPerm32, cuid), 12);
    assert_eq!(offset_of!(IpcPerm32, cgid), 16);
    assert_eq!(offset_of!(IpcPerm32, mode), 20);
    assert_eq!(offset_of!(IpcPerm32, seq), 24);
    assert_eq!(offset_of!(IpcPerm32, __unused1), 28);
    assert_eq!(offset_of!(IpcPerm32, __unused2), 32);
}

/// `struct semid64_ds` under `__i386__`: the perm, then `sem_otime` and
/// `sem_otime_high`, `sem_ctime` and `sem_ctime_high`, `sem_nsems` and two
/// reserved words. 64 bytes against the 104 of this architecture.
#[test]
fn the_semid_ds_is_sixty_four_bytes_and_its_times_are_pairs() {
    assert_eq!(size_of::<SemidDs32>(), 64);
    assert_eq!(align_of::<SemidDs32>(), 4);
    assert_eq!(size_of::<SemidDs>(), 104);

    assert_eq!(offset_of!(SemidDs32, perm), 0);
    assert_eq!(offset_of!(SemidDs32, otime_lo), 36);
    assert_eq!(offset_of!(SemidDs32, otime_hi), 40);
    assert_eq!(offset_of!(SemidDs32, ctime_lo), 44);
    assert_eq!(offset_of!(SemidDs32, ctime_hi), 48);
    assert_eq!(offset_of!(SemidDs32, nsems), 52);
    assert_eq!(offset_of!(SemidDs32, __unused3), 56);
    assert_eq!(offset_of!(SemidDs32, __unused4), 60);

    // The point of the pairs, stated as a number: in the NATIVE layout the
    // word after `sem_otime` is reserved and the one after `sem_ctime` is
    // reserved, and `sem_nsems` is at 80. Halving the native words would put
    // `nsems` at 40 -- which is where `sem_otime_high` lives.
    assert_eq!(offset_of!(SemidDs, nsems), 80);
    assert_eq!(offset_of!(SemidDs32, otime_hi), 40);
}

/// `struct msqid64_ds` under `__i386__`: three time pairs, three counts, two
/// pids and two reserved words. 88 bytes against 120.
#[test]
fn the_msqid_ds_is_eighty_eight_bytes_and_qbytes_is_at_sixty_eight() {
    assert_eq!(size_of::<MsqidDs32>(), 88);
    assert_eq!(align_of::<MsqidDs32>(), 4);
    assert_eq!(size_of::<MsqidDs>(), 120);

    assert_eq!(offset_of!(MsqidDs32, perm), 0);
    assert_eq!(offset_of!(MsqidDs32, stime_lo), 36);
    assert_eq!(offset_of!(MsqidDs32, stime_hi), 40);
    assert_eq!(offset_of!(MsqidDs32, rtime_lo), 44);
    assert_eq!(offset_of!(MsqidDs32, rtime_hi), 48);
    assert_eq!(offset_of!(MsqidDs32, ctime_lo), 52);
    assert_eq!(offset_of!(MsqidDs32, ctime_hi), 56);
    assert_eq!(offset_of!(MsqidDs32, cbytes), 60);
    assert_eq!(offset_of!(MsqidDs32, qnum), 64);
    assert_eq!(offset_of!(MsqidDs32, qbytes), 68);
    assert_eq!(offset_of!(MsqidDs32, lspid), 72);
    assert_eq!(offset_of!(MsqidDs32, lrpid), 76);
    assert_eq!(offset_of!(MsqidDs32, __unused4), 80);
    assert_eq!(offset_of!(MsqidDs32, __unused5), 84);

    // `msg_qbytes` is the only field of these three structures that
    // `IPC_SET` reads, so this offset is the one that decides whether a
    // 32-bit `msgctl(IPC_SET)` sets the queue capacity or some other field's
    // bytes. Natively it is at 88 -- which is the whole LENGTH of the narrow
    // structure, so reading at the native offset reads past the end of the
    // caller's buffer, and the native read of 120 bytes runs 32 bytes past
    // it before getting there.
    assert_eq!(offset_of!(MsqidDs, qbytes), 88);
    assert_eq!(size_of::<MsqidDs32>(), 88);
}

/// `struct shmid64_ds` under `__i386__`. The odd one: `shm_segsz` comes
/// before the times, so the first time pair starts at 40 and not 36.
#[test]
fn the_shmid_ds_puts_segsz_before_the_times() {
    assert_eq!(size_of::<ShmidDs32>(), 84);
    assert_eq!(align_of::<ShmidDs32>(), 4);
    assert_eq!(size_of::<ShmidDs>(), 112);

    assert_eq!(offset_of!(ShmidDs32, perm), 0);
    assert_eq!(offset_of!(ShmidDs32, segsz), 36);
    assert_eq!(offset_of!(ShmidDs32, atime_lo), 40);
    assert_eq!(offset_of!(ShmidDs32, atime_hi), 44);
    assert_eq!(offset_of!(ShmidDs32, dtime_lo), 48);
    assert_eq!(offset_of!(ShmidDs32, dtime_hi), 52);
    assert_eq!(offset_of!(ShmidDs32, ctime_lo), 56);
    assert_eq!(offset_of!(ShmidDs32, ctime_hi), 60);
    assert_eq!(offset_of!(ShmidDs32, cpid), 64);
    assert_eq!(offset_of!(ShmidDs32, lpid), 68);
    assert_eq!(offset_of!(ShmidDs32, nattch), 72);
    assert_eq!(offset_of!(ShmidDs32, __unused4), 76);
    assert_eq!(offset_of!(ShmidDs32, __unused5), 80);

    // Not 36: the size is the second field here and the times follow it, so
    // a table copied from `semid64_ds` would put every time four bytes
    // early and report the segment size as a timestamp.
    assert_ne!(offset_of!(ShmidDs32, atime_lo), 36);
}

/// A time whose seconds do not fit in 32 bits survives the pair, in both
/// directions. This is the whole reason the pair exists.
///
/// Only on a 64-bit host, and said so rather than worked around: the
/// constant below does not fit in a 32-bit `usize`, and neither does the
/// `otime` of the native structure, so there would be nothing to measure.
#[cfg(target_pointer_width = "64")]
#[test]
fn a_time_past_2106_survives_the_low_high_pair() {
    // 2106-02-07 is where an unsigned 32-bit count of seconds ends. One
    // second past it needs both halves.
    const LATE: usize = 0x1_0000_0000;

    let mut native = SemidDs::default();
    native.otime = LATE;
    native.ctime = LATE + 7;
    native.nsems = 3;

    let narrow: SemidDs32 = native.into();
    assert_eq!((narrow.otime_lo, narrow.otime_hi), (0, 1));
    assert_eq!((narrow.ctime_lo, narrow.ctime_hi), (7, 1));

    let back: SemidDs = narrow.into();
    assert_eq!(back.otime, LATE, "the low half alone would be zero");
    assert_eq!(back.ctime, LATE + 7);
    assert_eq!(back.nsems, 3);
}

/// What the round trip keeps and what it cannot: the permissions and the
/// times survive, and a count that does not fit in an `unsigned long` of
/// four bytes truncates, which is what Linux does there too.
#[test]
fn the_round_trip_keeps_the_perm_and_truncates_what_it_must() {
    let mut native = MsqidDs::default();
    native.perm = IpcPerm {
        key: 0x1234_5678,
        uid: 1000,
        gid: 1001,
        cuid: 1002,
        cgid: 1003,
        mode: 0o640,
        __seq: 9,
        __pad1: 0,
        __pad2: 0,
    };
    native.stime = 0x1234_5678_9abc;
    native.qbytes = 16384;
    native.qnum = 2;
    native.lspid = 4321;
    native.lrpid = 1234;
    // Past four billion: there is nowhere to put the top bits.
    native.cbytes = 0x1_0000_0002;

    let back: MsqidDs = MsqidDs32::from(native).into();
    assert_eq!(back.perm.key, 0x1234_5678);
    assert_eq!(back.perm.uid, 1000);
    assert_eq!(back.perm.gid, 1001);
    assert_eq!(back.perm.cuid, 1002);
    assert_eq!(back.perm.cgid, 1003);
    assert_eq!(back.perm.mode, 0o640);
    assert_eq!(back.perm.__seq, 9);
    assert_eq!(back.stime, 0x1234_5678_9abc, "a time is a pair, so it fits");
    assert_eq!(back.qbytes, 16384);
    assert_eq!(back.qnum, 2);
    assert_eq!(back.lspid, 4321);
    assert_eq!(back.lrpid, 1234);
    assert_eq!(
        back.cbytes, 2,
        "a count is one word there, so it truncates, as it does in Linux"
    );
}

/// The reserved tails are written as zeroes and not left as the caller had
/// them. Linux zeroes its `out` on the stack before filling it, so a
/// 32-bit program reading `__unused3` gets a zero and not kernel memory.
#[test]
fn the_reserved_words_go_out_as_zeroes() {
    let mut native = ShmidDs::default();
    native.segsz = 4096;
    let narrow: ShmidDs32 = native.into();
    assert_eq!(narrow.__unused4, 0);
    assert_eq!(narrow.__unused5, 0);
    assert_eq!(narrow.perm.__unused1, 0);
    assert_eq!(narrow.perm.__unused2, 0);
    assert_eq!(narrow.segsz, 4096);
}

/// `struct shminfo64` under `__i386__`: nine `__kernel_ulong_t` and nothing
/// else, which is why it needs no layout of its own. 36 bytes against 72.
#[test]
fn the_shminfo_is_nine_words_and_so_needs_no_layout() {
    use super::super::Narrow;
    assert_eq!(<ShmInfo64 as Narrow>::WORDS, 9);
    assert_eq!(size_of::<ShmInfo64>(), 9 * 8);
    // The claim the trait makes, stated as a number: nine words of eight
    // bytes IS the native structure, with no padding anywhere in it.
    assert_eq!(size_of::<ShmInfo64>(), <ShmInfo64 as Narrow>::WORDS * 8);

    // And the round trip through the words keeps every field, including the
    // four reserved ones.
    let mut info = ShmInfo64::linux_defaults();
    info.unused = [1, 2, 3, 4];
    let mut words = [0u64; 18];
    info.to_words(&mut words[..9]);
    assert_eq!(words[0], info.shmmax as u64);
    assert_eq!(words[4], info.shmall as u64);
    assert_eq!(&words[5..9], &[1, 2, 3, 4]);
    let back = <ShmInfo64 as Narrow>::from_words(&words[..9]);
    assert_eq!(back.shmmax, info.shmmax);
    assert_eq!(back.shmmni, info.shmmni);
    assert_eq!(back.unused, [1, 2, 3, 4]);
}

/// `struct shm_info` under `__i386__`: 24 bytes against 48, and the reason
/// it is not nine words is the **padding** the native layout puts after its
/// leading `int`.
#[test]
fn the_shm_info_is_not_a_pure_run_because_of_the_int() {
    assert_eq!(size_of::<ShmInfo32>(), 24);
    assert_eq!(align_of::<ShmInfo32>(), 4);
    assert_eq!(size_of::<ShmInfo>(), 48);

    assert_eq!(offset_of!(ShmInfo32, used_ids), 0);
    assert_eq!(offset_of!(ShmInfo32, shm_tot), 4);
    assert_eq!(offset_of!(ShmInfo32, shm_rss), 8);
    assert_eq!(offset_of!(ShmInfo32, shm_swp), 12);
    assert_eq!(offset_of!(ShmInfo32, swap_attempts), 16);
    assert_eq!(offset_of!(ShmInfo32, swap_successes), 20);

    // The padding, which is the whole reason this one is not six words: the
    // `int` is at 0 in both, and natively `shm_tot` is at 8 and not 4.
    assert_eq!(offset_of!(ShmInfo, used_ids), 0);
    assert_eq!(offset_of!(ShmInfo, shm_tot), 8);
    // Six words of eight bytes would be 48, which this structure is --- so
    // the size alone does NOT catch the padding. The offset does.
    assert_eq!(6 * 8, size_of::<ShmInfo>());
}

/// What the conversion keeps: the counts, and the `int` as an `int`.
#[test]
fn the_shm_info_conversion_keeps_the_counts() {
    let info = ShmInfo {
        used_ids: 3,
        shm_tot: 17,
        shm_rss: 17,
        shm_swp: 0,
        swap_attempts: 0,
        swap_successes: 0,
    };
    let narrow: ShmInfo32 = info.into();
    assert_eq!(narrow.used_ids, 3);
    assert_eq!(narrow.shm_tot, 17);
    assert_eq!(narrow.shm_rss, 17);
    assert_eq!(narrow.swap_successes, 0);
}
