//! The resource limits `init` starts with.

use super::*;

/// `INIT_RLIMITS` (`include/asm-generic/resource.h`), one row per resource in
/// the order above.
///
/// `RLIMIT_NPROC` and `RLIMIT_SIGPENDING` are `{0, 0}` in Linux because
/// `init` overwrites them from `max_threads` before any userspace runs.
/// `RLIMIT_SIGPENDING` is taken from memory the same way (see
/// [`default_sigpending_limit`]); `RLIMIT_NPROC` starts unlimited, a
/// departure (it is held once somebody sets it).
///
/// What a process used to get was [`RLimit::default`] -- 1024/1024 -- for the
/// one limit that existed. The soft limit is unchanged; the hard one becomes
/// Linux's `INR_OPEN_MAX`, so a process can raise its own descriptor budget
/// to 4096 the way it can anywhere else.
pub(super) const INIT_RLIMITS: [RLimit; RLIM_NLIMITS] = {
    const INF: RLimit = RLimit {
        cur: RLIM_INFINITY,
        max: RLIM_INFINITY,
    };
    let mut table = [INF; RLIM_NLIMITS];
    table[RLIMIT_STACK] = RLimit {
        cur: USER_STACK_SIZE,
        max: RLIM_INFINITY,
    };
    // No core is ever written, so a soft limit of zero is not a policy
    // this kernel is choosing: it is the size of the dump.
    table[RLIMIT_CORE] = RLimit {
        cur: 0,
        max: RLIM_INFINITY,
    };
    // `INR_OPEN_CUR` / `INR_OPEN_MAX`.
    table[RLIMIT_NOFILE] = RLimit {
        cur: 1024,
        max: 4096,
    };
    // `MLOCK_LIMIT`.
    table[RLIMIT_MEMLOCK] = RLimit {
        cur: USER_STACK_SIZE,
        max: USER_STACK_SIZE,
    };
    // `MQ_BYTES_MAX`.
    table[RLIMIT_MSGQUEUE] = RLimit {
        cur: 819_200,
        max: 819_200,
    };
    // A process may not raise its own priority here at all, which is what
    // Linux's zero means.
    table[RLIMIT_NICE] = RLimit { cur: 0, max: 0 };
    table[RLIMIT_RTPRIO] = RLimit { cur: 0, max: 0 };
    table
};
