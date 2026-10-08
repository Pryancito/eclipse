//! The `int` arguments of the uAPI, read out of a 64-bit register.
//!
//! A syscall argument arrives as a whole register, but `pid_t`, `id_t` and
//! the `who` of `getrusage` are 32-bit `int`s in the ABI: the kernel's
//! `SYSCALL_DEFINE` truncates the register to the declared type before the
//! handler ever sees it. So `-1` is the same argument whether the caller's
//! libc sign-extended it (`0xffff_ffff_ffff_ffff`, glibc and musl) or its
//! compiler zero-extended a 32-bit slot (`0x0000_0000_ffff_ffff`, which is
//! what `syscall(SYS_getrusage, RUSAGE_CHILDREN, &ru)` puts in the register:
//! a varargs `int` is not widened). Reading all 64 bits turns the second form
//! into a pid of four billion and answers `ESRCH` or `EINVAL` for an
//! argument Linux understands. `kill(2)`'s `pid_arg` already does this; the
//! helpers here are the same `(int)` for the rest.

use linux_object::error::{LxError, LxResult};

/// The `(int)` of a register: the low 32 bits, sign and all.
pub fn int_arg(raw: usize) -> i32 {
    // Truncating cast, on purpose: this is the `(int)` of the uAPI.
    raw as u32 as i32
}

/// Whose usage `getrusage(2)` reports.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum RusageWho {
    /// `RUSAGE_SELF` (0): the calling process.
    Process,
    /// `RUSAGE_CHILDREN` (-1): the children it has waited for.
    Children,
    /// `RUSAGE_THREAD` (1): the calling thread.
    Thread,
}

/// `getrusage(2)`'s `who`, an `int` with two of its three values negative or
/// small enough to be told apart from their zero-extended twins only by
/// reading it as one.
pub fn rusage_who(raw: usize) -> LxResult<RusageWho> {
    match int_arg(raw) {
        0 => Ok(RusageWho::Process),
        -1 => Ok(RusageWho::Children),
        1 => Ok(RusageWho::Thread),
        _ => Err(LxError::EINVAL),
    }
}

/// The `pid` of the `sched_*` calls that look a task up by it:
/// `sched_setscheduler`, `sched_getscheduler`, `sched_setparam`,
/// `sched_getparam`, `sched_rr_get_interval`, `sched_setattr` and
/// `sched_getattr`. Each of them opens with `if (pid < 0) return -EINVAL;`
/// before any lookup, so a negative one is `EINVAL`, not `ESRCH`.
pub fn sched_pid(raw: usize) -> LxResult<usize> {
    let pid = int_arg(raw);
    if pid < 0 {
        return Err(LxError::EINVAL);
    }
    Ok(pid as usize)
}

/// The `(pid, param)` pair of `sched_setscheduler`, `sched_setparam` and
/// `sched_getparam`, which open with `if (!param || pid < 0) return
/// -EINVAL;`: a NULL `struct sched_param` is `EINVAL`, judged before the
/// pointer is read (`EFAULT`) and before the pid is looked up (`ESRCH`).
/// A NULL was `EFAULT` here, and an unreadable pointer was `EFAULT` before
/// a negative pid was `EINVAL`.
pub fn sched_param_pid(raw: usize, param_is_null: bool) -> LxResult<usize> {
    if param_is_null {
        return Err(LxError::EINVAL);
    }
    sched_pid(raw)
}

/// The `int bufsiz` of `readlink(2)` and `readlinkat(2)`: `do_readlinkat`
/// opens with `if (bufsiz <= 0) return -EINVAL;`, before the path is even
/// copied in. Read as a `usize`, zero returned zero bytes of a link that
/// exists and a negative one was a huge buffer clamped to a page, so
/// `readlink(path, buf, -1)` filled `buf` past its end.
pub fn readlink_bufsiz(raw: usize) -> LxResult<usize> {
    let bufsiz = int_arg(raw);
    if bufsiz <= 0 {
        return Err(LxError::EINVAL);
    }
    Ok(bufsiz as usize)
}

/// A `pid_t` that names a task with no rule of its own for negatives:
/// `sched_setaffinity`, `sched_getaffinity` and `prlimit64` hand it to
/// `find_task_by_vpid`, which finds nothing, so a negative one is `ESRCH`.
pub fn task_pid(raw: usize) -> LxResult<usize> {
    let pid = int_arg(raw);
    if pid < 0 {
        return Err(LxError::ESRCH);
    }
    Ok(pid as usize)
}

/// `waitid(2)`'s `id` for `P_PID` and `P_PGID`: an `id_t` that
/// `kernel_waitid` reads as a `pid_t`. For `P_PID` it must be positive
/// (`upid <= 0` is `EINVAL`); for `P_PGID` zero means the caller's own
/// group and a negative one is `EINVAL`.
pub fn waitid_id(raw: usize, zero_allowed: bool) -> LxResult<usize> {
    let id = int_arg(raw);
    if id < 0 || (id == 0 && !zero_allowed) {
        return Err(LxError::EINVAL);
    }
    Ok(id as usize)
}

/// A `(loff_t offset, loff_t len)` pair, as `ksys_sync_file_range` judges
/// it before it looks the descriptor up: `loff_t` is signed, so a register
/// with the top bit set is a negative offset or length (`EINVAL`), and an
/// end past `LLONG_MAX` (`(s64)(offset + nbytes) < 0`) is `EINVAL` too.
/// Read as `u64` these were huge, and accepted.
pub fn loff_range(offset: usize, len: usize) -> LxResult<(u64, u64)> {
    let (offset, len) = (offset as i64, len as i64);
    if offset < 0 || len < 0 || offset.checked_add(len).is_none() {
        return Err(LxError::EINVAL);
    }
    Ok((offset as u64, len as u64))
}

/// A `loff_t len` on its own, as `generic_fadvise` judges it (`len < 0` is
/// `EINVAL`; the offset is not looked at). `readahead(2)` goes through the
/// same test with its `size_t count` widened to `loff_t`.
pub fn loff_len(len: usize) -> LxResult<u64> {
    if (len as i64) < 0 {
        return Err(LxError::EINVAL);
    }
    Ok(len as u64)
}

/// A `uid_t`/`gid_t` argument of `setuid(2)`, `setgid(2)` and `setgroups(2)`:
/// `make_kuid` + `uid_valid` reject `(uid_t)-1` with `EINVAL` before any
/// privilege is looked at. It used to go through: as root, `setuid(-1)`
/// returned 0 and left every id at 4294967295; unprivileged it was `EPERM`.
pub fn set_id(raw: usize) -> LxResult<u32> {
    let id = raw as u32;
    if id == u32::MAX {
        return Err(LxError::EINVAL);
    }
    Ok(id)
}

/// `NGROUPS_MAX`: the most supplementary groups a process may hold.
pub const NGROUPS_MAX: usize = 65536;

/// The `int gidsetsize` of `getgroups(2)` and `setgroups(2)`. `getgroups`
/// refuses a negative one (`EINVAL`); `setgroups` reads it as unsigned and
/// refuses anything over `NGROUPS_MAX` (`EINVAL`), which is also what keeps
/// the kernel from allocating whatever size the caller names.
pub fn groups_size(raw: usize, for_set: bool) -> LxResult<usize> {
    let size = int_arg(raw);
    if for_set {
        if size as u32 as usize > NGROUPS_MAX {
            return Err(LxError::EINVAL);
        }
        Ok(size as u32 as usize)
    } else {
        if size < 0 {
            return Err(LxError::EINVAL);
        }
        Ok(size as usize)
    }
}

/// The list `setgroups(2)` was handed, once read: `groups_from_user` puts
/// every entry through `gid_valid`, so a `(gid_t)-1` anywhere in it is
/// `EINVAL` and nothing is changed.
pub fn groups_list(groups: alloc::vec::Vec<u32>) -> LxResult<alloc::vec::Vec<u32>> {
    if groups.contains(&u32::MAX) {
        return Err(LxError::EINVAL);
    }
    Ok(groups)
}

#[cfg(test)]
mod id_arg_tests {
    //! The id and count arguments Linux refuses before looking at privilege.

    use super::*;

    /// `-1` is not an id, however the register carries it; anything else is.
    #[test]
    fn minus_one_is_not_an_id() {
        assert_eq!(set_id(0xffff_ffff), Err(LxError::EINVAL));
        assert_eq!(set_id(usize::MAX), Err(LxError::EINVAL));
        assert_eq!(set_id(0), Ok(0));
        assert_eq!(set_id(1000), Ok(1000));
        assert_eq!(set_id(0xffff_fffe), Ok(0xffff_fffe));
        // Only the low 32 bits are the argument.
        assert_eq!(set_id(0x1_0000_03e8), Ok(1000));
    }

    /// `getgroups` refuses a negative size; `setgroups` reads it unsigned
    /// and refuses more than `NGROUPS_MAX`.
    #[test]
    fn a_negative_or_oversized_group_count_is_refused() {
        assert_eq!(groups_size(0, false), Ok(0));
        assert_eq!(groups_size(0xffff_ffff, false), Err(LxError::EINVAL), "-1");
        assert_eq!(groups_size(usize::MAX, false), Err(LxError::EINVAL));
        assert_eq!(
            groups_size(100_000, false),
            Ok(100_000),
            "getgroups has no ceiling"
        );
        assert_eq!(groups_size(0, true), Ok(0));
        assert_eq!(groups_size(NGROUPS_MAX, true), Ok(NGROUPS_MAX));
        assert_eq!(groups_size(NGROUPS_MAX + 1, true), Err(LxError::EINVAL));
        assert_eq!(
            groups_size(0xffff_ffff, true),
            Err(LxError::EINVAL),
            "-1 is huge unsigned"
        );
        assert_eq!(groups_size(usize::MAX, true), Err(LxError::EINVAL));
    }

    /// A `-1` anywhere in the list refuses the whole list.
    #[test]
    fn a_minus_one_in_the_group_list_refuses_it_whole() {
        assert_eq!(groups_list(alloc::vec![]), Ok(alloc::vec![]));
        assert_eq!(
            groups_list(alloc::vec![0, 1000, 4]),
            Ok(alloc::vec![0, 1000, 4])
        );
        assert_eq!(
            groups_list(alloc::vec![1000, u32::MAX]),
            Err(LxError::EINVAL)
        );
        assert_eq!(
            groups_list(alloc::vec![u32::MAX, 1000]),
            Err(LxError::EINVAL)
        );
    }
}

#[cfg(test)]
mod loff_tests {
    //! The signed 64-bit lengths, which were read unsigned.

    use super::*;

    /// `sync_file_range(fd, -1, 0, 0)` is EINVAL in Linux; it was a sync of
    /// everything from byte 2^64-1 on, which is to say a success.
    #[test]
    fn a_negative_offset_or_length_is_einval() {
        assert_eq!(loff_range(0, 0), Ok((0, 0)));
        assert_eq!(loff_range(4096, 1 << 20), Ok((4096, 1 << 20)));
        assert_eq!(loff_range(i64::MAX as usize, 0), Ok((i64::MAX as u64, 0)));
        for (offset, len) in [(usize::MAX, 0), (0, usize::MAX), (1 << 63, 1), (1, 1 << 63)] {
            assert_eq!(
                loff_range(offset, len),
                Err(LxError::EINVAL),
                "{:#x} {:#x}",
                offset,
                len
            );
        }
    }

    /// An end past `LLONG_MAX` is EINVAL, and exactly at it is not.
    #[test]
    fn an_end_past_llong_max_is_einval() {
        let max = i64::MAX as usize;
        assert_eq!(loff_range(max - 1, 1), Ok(((max - 1) as u64, 1)));
        assert_eq!(loff_range(max, 1), Err(LxError::EINVAL));
        assert_eq!(loff_range(1, max), Err(LxError::EINVAL));
        assert_eq!(loff_range(max / 2 + 1, max / 2 + 1), Err(LxError::EINVAL));
    }

    /// `fadvise64(fd, off, -1, advice)` is EINVAL; `readahead(fd, off,
    /// (size_t)-1)` too, once the count is a `loff_t`.
    #[test]
    fn a_negative_length_alone_is_einval() {
        assert_eq!(loff_len(0), Ok(0));
        assert_eq!(loff_len(i64::MAX as usize), Ok(i64::MAX as u64));
        assert_eq!(loff_len(1 << 63), Err(LxError::EINVAL));
        assert_eq!(loff_len(usize::MAX), Err(LxError::EINVAL));
    }
}

#[cfg(test)]
mod tests {
    //! The `(int)` of each argument, with `-1` spelled both ways a register
    //! can carry it.

    use super::*;

    /// A 32-bit `-1` zero-extended, and sign-extended: the same argument.
    const MINUS_ONE_ZX: usize = 0xffff_ffff;
    const MINUS_ONE_SX: usize = usize::MAX;

    #[test]
    fn a_register_is_read_as_its_low_32_bits_with_sign() {
        assert_eq!(int_arg(0), 0);
        assert_eq!(int_arg(7), 7);
        assert_eq!(int_arg(MINUS_ONE_ZX), -1);
        assert_eq!(int_arg(MINUS_ONE_SX), -1);
        assert_eq!(
            int_arg(0x1_0000_0000),
            0,
            "the high half is not the argument"
        );
        assert_eq!(int_arg(0x8000_0000), i32::MIN);
    }

    /// `syscall(SYS_getrusage, RUSAGE_CHILDREN, &ru)` puts `0xffff_ffff` in
    /// the register (a varargs `int` is not widened): `RUSAGE_CHILDREN`,
    /// not `EINVAL`.
    #[test]
    fn rusage_children_is_minus_one_however_it_arrives() {
        assert_eq!(rusage_who(0), Ok(RusageWho::Process));
        assert_eq!(rusage_who(1), Ok(RusageWho::Thread));
        assert_eq!(rusage_who(MINUS_ONE_ZX), Ok(RusageWho::Children));
        assert_eq!(rusage_who(MINUS_ONE_SX), Ok(RusageWho::Children));
        // Junk in the high half is not the argument either.
        assert_eq!(rusage_who(0x1_0000_0001), Ok(RusageWho::Thread));
        for raw in [2, MINUS_ONE_ZX - 1, 0x1_0000_0002] {
            assert_eq!(rusage_who(raw), Err(LxError::EINVAL), "{:#x}", raw);
        }
    }

    /// `sched_getscheduler(-1)` is `EINVAL` in Linux, `ESRCH` was the
    /// answer here (a pid of four billion, looked up and not found).
    #[test]
    fn a_negative_sched_pid_is_einval_and_a_negative_task_pid_is_esrch() {
        assert_eq!(sched_pid(0), Ok(0));
        assert_eq!(sched_pid(1234), Ok(1234));
        assert_eq!(sched_pid(MINUS_ONE_ZX), Err(LxError::EINVAL));
        assert_eq!(sched_pid(MINUS_ONE_SX), Err(LxError::EINVAL));
        assert_eq!(task_pid(0), Ok(0));
        assert_eq!(task_pid(1234), Ok(1234));
        assert_eq!(task_pid(MINUS_ONE_ZX), Err(LxError::ESRCH));
        assert_eq!(task_pid(MINUS_ONE_SX), Err(LxError::ESRCH));
        // A pid with junk in the high half is the pid in the low half.
        assert_eq!(sched_pid(0x1_0000_0000 | 42), Ok(42));
        assert_eq!(task_pid(0x1_0000_0000 | 42), Ok(42));
    }

    /// `sched_getparam(pid, NULL)` is `EINVAL` before anything else, and
    /// a negative pid with a good pointer is `EINVAL` too; a good pair is
    /// the pid.
    #[test]
    fn a_null_sched_param_is_einval_before_the_pid() {
        assert_eq!(sched_param_pid(0, true), Err(LxError::EINVAL));
        assert_eq!(sched_param_pid(1234, true), Err(LxError::EINVAL));
        assert_eq!(sched_param_pid(MINUS_ONE_SX, true), Err(LxError::EINVAL));
        assert_eq!(sched_param_pid(0, false), Ok(0));
        assert_eq!(sched_param_pid(1234, false), Ok(1234));
        assert_eq!(sched_param_pid(MINUS_ONE_ZX, false), Err(LxError::EINVAL));
        assert_eq!(sched_param_pid(MINUS_ONE_SX, false), Err(LxError::EINVAL));
    }

    /// `readlink(p, buf, 0)` and `readlink(p, buf, -1)` are `EINVAL`; the
    /// size is an `int`, so junk in the high half is not part of it.
    #[test]
    fn a_readlink_buffer_size_must_be_positive() {
        assert_eq!(readlink_bufsiz(0), Err(LxError::EINVAL));
        assert_eq!(readlink_bufsiz(MINUS_ONE_ZX), Err(LxError::EINVAL));
        assert_eq!(readlink_bufsiz(MINUS_ONE_SX), Err(LxError::EINVAL));
        assert_eq!(
            readlink_bufsiz(0x8000_0000),
            Err(LxError::EINVAL),
            "INT_MIN"
        );
        assert_eq!(readlink_bufsiz(1), Ok(1));
        assert_eq!(readlink_bufsiz(4096), Ok(4096));
        assert_eq!(readlink_bufsiz(0x7fff_ffff), Ok(0x7fff_ffff));
        assert_eq!(readlink_bufsiz(0x1_0000_0000 | 64), Ok(64));
    }

    /// `P_PID` wants a positive pid; `P_PGID` takes zero for the caller's
    /// own group; neither takes a negative one.
    #[test]
    fn waitid_takes_zero_only_for_a_process_group() {
        assert_eq!(waitid_id(5, false), Ok(5));
        assert_eq!(waitid_id(0, false), Err(LxError::EINVAL));
        assert_eq!(waitid_id(0, true), Ok(0));
        assert_eq!(waitid_id(5, true), Ok(5));
        for raw in [MINUS_ONE_ZX, MINUS_ONE_SX, 0x8000_0000] {
            assert_eq!(waitid_id(raw, false), Err(LxError::EINVAL), "{:#x}", raw);
            assert_eq!(waitid_id(raw, true), Err(LxError::EINVAL), "{:#x}", raw);
        }
    }
}

#[cfg(test)]
mod benches {
    //! What the uAPI's `(int)` costs. These helpers sit at the very front of
    //! a syscall -- before any lookup, any copy, any lock -- so the question
    //! they answer is whether reading an argument correctly is something a
    //! kernel can afford to do on every call. It is: every row but one is a
    //! truncating cast and a compare.
    //!
    //! The exception is `groups_list`, which scans what `setgroups(2)` was
    //! handed looking for `(gid_t)-1`, and whose length the caller chooses up
    //! to `NGROUPS_MAX`. That one comes as a family so the slope can be read.
    //!
    //! Every row black-boxes its inputs. Two cycles (~0.6 ns) is `b.iter`'s
    //! own loop; a row there with an opaque input is a real compare and
    //! branch, a row there with a constant input is a call that was folded
    //! away and never ran.

    use super::*;
    use test::{black_box, Bencher};

    /// A 32-bit `-1` sign-extended into the register, which is what glibc and
    /// musl put there, and the value every helper here has a rule about.
    const MINUS_ONE: usize = usize::MAX;

    #[bench]
    fn int_arg_of_a_register(b: &mut Bencher) {
        b.iter(|| black_box(int_arg(black_box(MINUS_ONE))));
    }

    #[bench]
    fn rusage_who_accepted(b: &mut Bencher) {
        b.iter(|| black_box(rusage_who(black_box(MINUS_ONE))));
    }

    /// The refused `who`, because the match falls through all three arms
    /// before it answers: a different path, not a different cost.
    #[bench]
    fn rusage_who_refused(b: &mut Bencher) {
        b.iter(|| black_box(rusage_who(black_box(42))));
    }

    #[bench]
    fn sched_pid_accepted(b: &mut Bencher) {
        b.iter(|| black_box(sched_pid(black_box(1234))));
    }

    #[bench]
    fn sched_pid_negative(b: &mut Bencher) {
        b.iter(|| black_box(sched_pid(black_box(MINUS_ONE))));
    }

    /// The NULL `struct sched_param`, which is `EINVAL` before the pid is
    /// even read: the short-circuit this helper exists for.
    #[bench]
    fn sched_param_pid_null_param(b: &mut Bencher) {
        b.iter(|| black_box(sched_param_pid(black_box(1234), black_box(true))));
    }

    #[bench]
    fn readlink_bufsiz_accepted(b: &mut Bencher) {
        b.iter(|| black_box(readlink_bufsiz(black_box(4096))));
    }

    /// `readlink(path, buf, -1)`, which used to be a huge buffer clamped to a
    /// page and wrote past the caller's end.
    #[bench]
    fn readlink_bufsiz_negative(b: &mut Bencher) {
        b.iter(|| black_box(readlink_bufsiz(black_box(MINUS_ONE))));
    }

    #[bench]
    fn task_pid_accepted(b: &mut Bencher) {
        b.iter(|| black_box(task_pid(black_box(1234))));
    }

    #[bench]
    fn waitid_id_accepted(b: &mut Bencher) {
        b.iter(|| black_box(waitid_id(black_box(1234), black_box(false))));
    }

    /// `P_PGID` with an `id` of zero: the caller's own group, allowed only
    /// because the flag says so.
    #[bench]
    fn waitid_id_zero_for_pgid(b: &mut Bencher) {
        b.iter(|| black_box(waitid_id(black_box(0), black_box(true))));
    }

    /// The `(offset, len)` of `sync_file_range`, including the checked add
    /// that catches an end past `LLONG_MAX`.
    #[bench]
    fn loff_range_accepted(b: &mut Bencher) {
        b.iter(|| black_box(loff_range(black_box(4096), black_box(65536))));
    }

    /// The overflowing pair, which is the one the `checked_add` is for.
    #[bench]
    fn loff_range_overflowing(b: &mut Bencher) {
        b.iter(|| black_box(loff_range(black_box(i64::MAX as usize), black_box(4096))));
    }

    #[bench]
    fn loff_len_accepted(b: &mut Bencher) {
        b.iter(|| black_box(loff_len(black_box(65536))));
    }

    /// `setuid(-1)`, which returned 0 and left every id at 4294967295.
    #[bench]
    fn set_id_refused(b: &mut Bencher) {
        b.iter(|| black_box(set_id(black_box(MINUS_ONE))));
    }

    #[bench]
    fn set_id_accepted(b: &mut Bencher) {
        b.iter(|| black_box(set_id(black_box(1000))));
    }

    /// `getgroups`'s `gidsetsize` and `setgroups`'s, which are the same
    /// register read two different ways -- signed for one, unsigned and
    /// capped for the other.
    #[bench]
    fn groups_size_for_get(b: &mut Bencher) {
        b.iter(|| black_box(groups_size(black_box(32), black_box(false))));
    }

    #[bench]
    fn groups_size_for_set_over_the_cap(b: &mut Bencher) {
        b.iter(|| black_box(groups_size(black_box(NGROUPS_MAX + 1), black_box(true))));
    }

    /// `groups_list` over `n` groups, none of them `(gid_t)-1`, so the scan
    /// runs to the end -- which is the case `setgroups(2)` takes when it
    /// succeeds, and the only row here with a length in it.
    fn bench_groups_list(b: &mut Bencher, n: usize) {
        let groups: alloc::vec::Vec<u32> = (0..n as u32).collect();
        b.iter(|| {
            // `groups_list` consumes the vector and hands it back, so the
            // clone is in the figure. `groups_list_clone_only` is what to
            // subtract for the scan on its own.
            black_box(groups_list(black_box(groups.clone())))
        });
    }

    /// The clone the rows below pay for, on its own.
    fn bench_groups_clone(b: &mut Bencher, n: usize) {
        let groups: alloc::vec::Vec<u32> = (0..n as u32).collect();
        b.iter(|| black_box(black_box(&groups).clone()));
    }

    #[bench]
    fn groups_list_of_1(b: &mut Bencher) {
        bench_groups_list(b, 1);
    }

    /// 32 groups: more than a desktop login has, and the shape `id` prints.
    #[bench]
    fn groups_list_of_32(b: &mut Bencher) {
        bench_groups_list(b, 32);
    }

    /// `NGROUPS_MAX`, the widest list the kernel will take, which is what
    /// bounds this scan at all: without the cap in `groups_size` the length
    /// came from the caller.
    #[bench]
    fn groups_list_of_ngroups_max(b: &mut Bencher) {
        bench_groups_list(b, NGROUPS_MAX);
    }

    #[bench]
    fn groups_list_clone_only_of_32(b: &mut Bencher) {
        bench_groups_clone(b, 32);
    }

    #[bench]
    fn groups_list_clone_only_of_ngroups_max(b: &mut Bencher) {
        bench_groups_clone(b, NGROUPS_MAX);
    }

    /// A `(gid_t)-1` at the front, which the scan finds at once: against the
    /// row above, the difference is what a refusal saves.
    #[bench]
    fn groups_list_refused_at_the_front(b: &mut Bencher) {
        let mut groups: alloc::vec::Vec<u32> = (0..NGROUPS_MAX as u32).collect();
        groups[0] = u32::MAX;
        b.iter(|| black_box(groups_list(black_box(groups.clone()))));
    }
}
