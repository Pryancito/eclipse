//! Translate the error this kernel produces into the `errno` a FreeBSD
//! program will read.
//!
//! The two systems agree on 1..=34 (`EPERM`..`ERANGE`) and then diverge. The
//! trap that makes a blind pass-through wrong is that `EAGAIN` and `EDEADLK`
//! are *swapped*: Linux has `EAGAIN = 11`, `EDEADLK = 35`; FreeBSD has
//! `EDEADLK = 11`, `EAGAIN = 35`. Handing a Linux `EAGAIN` (11) straight to a
//! FreeBSD program would read as `EDEADLK`, so the mapping is done value by
//! value against `sys/sys/errno.h`.
//!
//! **The match below is exhaustive on purpose.** What it replaced was a match
//! on the raw integer with a `_ => EINVAL` arm, and an arm like that answers
//! every question, including the ones nobody has thought about: six of the
//! errors this kernel can actually produce were falling through it. The worst
//! was `ECONNRESET`, which every network program tests for by name after a
//! read; it was arriving as `EINVAL`, so the branch that reconnects was the
//! branch that never ran. Without a `_` arm, a new [`LxError`] variant does
//! not compile until somebody has decided what a FreeBSD program should see.

use super::consts::errno as bsd;
use linux_object::error::LxError;

/// Map an [`LxError`] to the FreeBSD `errno` a FreeBSD binary expects.
///
/// Three errors have no peer on FreeBSD and say so here rather than landing
/// on a number that means something else: `ETIME` (a timer that expired)
/// answers `ETIMEDOUT`, and `EUNDEF` and `EBADFD` answer `EINVAL`.
pub fn lx_to_freebsd(err: LxError) -> i32 {
    match err {
        // Not an error number on either system. It reaches here only from a
        // `SysResult` that was built wrong, and on the Linux path it would
        // be returned as `-0`, which is indistinguishable from success; the
        // FreeBSD path at least raises the carry flag.
        LxError::EUNDEF => bsd::EINVAL,

        // ---- identical, EPERM(1) .. ECHILD(10) --------------------------
        LxError::EPERM => bsd::EPERM,
        LxError::ENOENT => bsd::ENOENT,
        LxError::ESRCH => bsd::ESRCH,
        LxError::EINTR => bsd::EINTR,
        LxError::EIO => bsd::EIO,
        LxError::ENXIO => bsd::ENXIO,
        LxError::E2BIG => bsd::E2BIG,
        LxError::ENOEXEC => bsd::ENOEXEC,
        LxError::EBADF => bsd::EBADF,
        LxError::ECHILD => bsd::ECHILD,

        // ---- the swap ---------------------------------------------------
        LxError::EAGAIN => bsd::EAGAIN,   // Linux 11 -> FreeBSD 35
        LxError::EDEADLK => bsd::EDEADLK, // Linux 35 -> FreeBSD 11

        // ---- identical again, ENOMEM(12) .. ERANGE(34) ------------------
        LxError::ENOMEM => bsd::ENOMEM,
        LxError::EACCES => bsd::EACCES,
        LxError::EFAULT => bsd::EFAULT,
        LxError::ENOTBLK => bsd::ENOTBLK,
        LxError::EBUSY => bsd::EBUSY,
        LxError::EEXIST => bsd::EEXIST,
        LxError::EXDEV => bsd::EXDEV,
        LxError::ENODEV => bsd::ENODEV,
        LxError::ENOTDIR => bsd::ENOTDIR,
        LxError::EISDIR => bsd::EISDIR,
        LxError::EINVAL => bsd::EINVAL,
        LxError::ENFILE => bsd::ENFILE,
        LxError::EMFILE => bsd::EMFILE,
        LxError::ENOTTY => bsd::ENOTTY,
        LxError::ETXTBSY => bsd::ETXTBSY,
        LxError::EFBIG => bsd::EFBIG,
        LxError::ENOSPC => bsd::ENOSPC,
        LxError::ESPIPE => bsd::ESPIPE,
        LxError::EROFS => bsd::EROFS,
        LxError::EMLINK => bsd::EMLINK,
        LxError::EPIPE => bsd::EPIPE,
        LxError::EDOM => bsd::EDOM,
        LxError::ERANGE => bsd::ERANGE,

        // ---- past 35 the numbers differ ---------------------------------
        LxError::ENAMETOOLONG => bsd::ENAMETOOLONG,
        LxError::ENOLCK => bsd::ENOLCK,
        LxError::ENOSYS => bsd::ENOSYS,
        LxError::ENOTEMPTY => bsd::ENOTEMPTY,
        LxError::ELOOP => bsd::ELOOP,
        LxError::ENOMSG => bsd::ENOMSG,
        LxError::EIDRM => bsd::EIDRM,
        // Linux's `ENODATA` covers both "no message" and "no such extended
        // attribute"; FreeBSD splits them and only the second has a number.
        LxError::ENODATA => bsd::ENOATTR,
        // FreeBSD has no `ETIME`. Its number over there is `ELOOP`, so a
        // pass-through would report a symlink loop for an expired timer.
        // `ETIMEDOUT` is the one a FreeBSD program tests for.
        LxError::ETIME => bsd::ETIMEDOUT,
        LxError::EOVERFLOW => bsd::EOVERFLOW,
        // FreeBSD has no `EBADFD` ("the descriptor is fine, the object behind
        // it is not"). Its number over there is `ENOLCK`. `EBADF` would be
        // worse than `EINVAL`: it would claim the descriptor itself is wrong,
        // and a program would close a descriptor that is still good.
        LxError::EBADFD => bsd::EINVAL,
        LxError::ENOTSOCK => bsd::ENOTSOCK,
        LxError::EMSGSIZE => bsd::EMSGSIZE,
        LxError::ENOPROTOOPT => bsd::ENOPROTOOPT,
        LxError::EOPNOTSUPP => bsd::EOPNOTSUPP,
        LxError::EPFNOSUPPORT => bsd::EPFNOSUPPORT,
        LxError::EAFNOSUPPORT => bsd::EAFNOSUPPORT,
        LxError::EADDRINUSE => bsd::EADDRINUSE,
        LxError::EADDRNOTAVAIL => bsd::EADDRNOTAVAIL,
        LxError::ECONNRESET => bsd::ECONNRESET,
        LxError::ENOBUFS => bsd::ENOBUFS,
        LxError::EISCONN => bsd::EISCONN,
        LxError::ENOTCONN => bsd::ENOTCONN,
        LxError::ETIMEDOUT => bsd::ETIMEDOUT,
        LxError::ECONNREFUSED => bsd::ECONNREFUSED,
        LxError::EALREADY => bsd::EALREADY,
        LxError::EINPROGRESS => bsd::EINPROGRESS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Every variant of [`LxError`]. The table in `lx_to_freebsd` is what the
    /// compiler keeps complete; this list only has to stay in step with it so
    /// that the properties below cover the whole enum.
    const ALL: &[LxError] = &[
        LxError::EUNDEF,
        LxError::EPERM,
        LxError::ENOENT,
        LxError::ESRCH,
        LxError::EINTR,
        LxError::EIO,
        LxError::ENXIO,
        LxError::E2BIG,
        LxError::ENOEXEC,
        LxError::EBADF,
        LxError::ECHILD,
        LxError::EAGAIN,
        LxError::ENOMEM,
        LxError::EACCES,
        LxError::EFAULT,
        LxError::ENOTBLK,
        LxError::EBUSY,
        LxError::EEXIST,
        LxError::EXDEV,
        LxError::ENODEV,
        LxError::ENOTDIR,
        LxError::EISDIR,
        LxError::EINVAL,
        LxError::ENFILE,
        LxError::EMFILE,
        LxError::ENOTTY,
        LxError::ETXTBSY,
        LxError::EFBIG,
        LxError::ENOSPC,
        LxError::ESPIPE,
        LxError::EROFS,
        LxError::EMLINK,
        LxError::EPIPE,
        LxError::EDOM,
        LxError::ERANGE,
        LxError::EDEADLK,
        LxError::ENAMETOOLONG,
        LxError::ENOLCK,
        LxError::ENOSYS,
        LxError::ENOTEMPTY,
        LxError::ELOOP,
        LxError::ENOMSG,
        LxError::EIDRM,
        LxError::ENODATA,
        LxError::ETIME,
        LxError::EBADFD,
        LxError::EOVERFLOW,
        LxError::ENOTSOCK,
        LxError::ENOPROTOOPT,
        LxError::EOPNOTSUPP,
        LxError::EPFNOSUPPORT,
        LxError::EAFNOSUPPORT,
        LxError::ECONNRESET,
        LxError::ENOBUFS,
        LxError::EISCONN,
        LxError::ENOTCONN,
        LxError::ETIMEDOUT,
        LxError::ECONNREFUSED,
        LxError::EINPROGRESS,
        LxError::EADDRINUSE,
        LxError::EADDRNOTAVAIL,
        LxError::EMSGSIZE,
        LxError::EALREADY,
    ];

    #[test]
    fn eagain_and_edeadlk_are_swapped() {
        // The whole reason this table exists.
        assert_eq!(lx_to_freebsd(LxError::EAGAIN), 35);
        assert_eq!(lx_to_freebsd(LxError::EDEADLK), 11);
    }

    #[test]
    fn the_numbers_both_systems_share_are_left_alone() {
        for &e in ALL {
            let lin = e as i32;
            if (1..=10).contains(&lin) || (12..=34).contains(&lin) {
                assert_eq!(lx_to_freebsd(e), lin, "{:?}", e);
            }
        }
    }

    #[test]
    fn diverging_values_map_to_freebsd_numbers() {
        assert_eq!(lx_to_freebsd(LxError::ENOSYS), 78);
        assert_eq!(lx_to_freebsd(LxError::ELOOP), 62);
        assert_eq!(lx_to_freebsd(LxError::ENAMETOOLONG), 63);
        assert_eq!(lx_to_freebsd(LxError::ETIMEDOUT), 60);
        assert_eq!(lx_to_freebsd(LxError::EINPROGRESS), 36);
    }

    /// The six that used to fall through `_ => EINVAL`.
    #[test]
    fn the_errors_that_fell_through_have_their_own_number_now() {
        assert_eq!(lx_to_freebsd(LxError::ENOMSG), 83);
        assert_eq!(lx_to_freebsd(LxError::EOVERFLOW), 84);
        assert_eq!(lx_to_freebsd(LxError::EADDRNOTAVAIL), 49);
        // The one that matters most: a FreeBSD program that reads a reset
        // connection tests `errno == ECONNRESET` and reconnects.
        assert_eq!(lx_to_freebsd(LxError::ECONNRESET), 54);
        // These two have no peer, and answer the nearest thing rather than a
        // number that means something else over there.
        assert_eq!(lx_to_freebsd(LxError::ETIME), 60); // ETIMEDOUT
        assert_eq!(lx_to_freebsd(LxError::EBADFD), 22); // EINVAL
    }

    #[test]
    fn no_error_becomes_a_number_freebsd_does_not_have() {
        for &e in ALL {
            let n = lx_to_freebsd(e);
            assert!(
                (1..=bsd::ELAST).contains(&n),
                "{:?} -> {}, outside 1..={}",
                e,
                n,
                bsd::ELAST
            );
        }
    }

    /// A hand-written table of sixty-odd rows earns exactly one kind of
    /// mistake: two unrelated errors typed onto one number. Only the three
    /// that have no FreeBSD peer are allowed to share.
    #[test]
    fn only_the_errors_without_a_peer_share_a_number() {
        let mut seen: Vec<(i32, LxError)> = Vec::new();
        for &e in ALL {
            let n = lx_to_freebsd(e);
            if let Some(&(_, other)) = seen.iter().find(|(m, _)| *m == n) {
                let excused = matches!(
                    (other, e),
                    (LxError::EUNDEF, LxError::EINVAL)
                        | (LxError::EUNDEF, LxError::EBADFD)
                        | (LxError::EINVAL, LxError::EBADFD)
                        | (LxError::ETIMEDOUT, LxError::ETIME)
                        | (LxError::ETIME, LxError::ETIMEDOUT)
                );
                assert!(excused, "{:?} and {:?} both answer {}", other, e, n);
            }
            seen.push((n, e));
        }
    }

    /// `ALL` is kept by hand, so it gets checked against the one thing that
    /// cannot go stale: the discriminants themselves.
    #[test]
    fn the_list_names_each_error_once() {
        let mut nums: Vec<i32> = ALL.iter().map(|&e| e as i32).collect();
        let before = nums.len();
        nums.sort_unstable();
        nums.dedup();
        assert_eq!(nums.len(), before, "a repeated entry in ALL");
    }
}
