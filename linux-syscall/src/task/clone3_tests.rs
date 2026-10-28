use super::*;

/// `struct clone_args` version 0, as eight `u64`, in field order.
fn args(
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
) -> [u64; 8] {
    [
        flags,
        pidfd,
        child_tid,
        parent_tid,
        exit_signal,
        stack,
        stack_size,
        tls,
    ]
}

fn ok(words: [u64; 8]) -> Clone3Args {
    clone3_to_clone(words).expect("should decode")
}

#[test]
fn a_stack_that_overflows_is_refused_instead_of_panicking_the_kernel() {
    // `stack + stack_size` are both read straight out of userspace, and a
    // plain `+` panics on a debug build. Any process could have taken the
    // kernel down with this the moment clone3 became reachable.
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, 0, u64::MAX, 1, 0)),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, 0, 1, u64::MAX, 0)),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, 0, u64::MAX, u64::MAX, 0)),
        Err(LxError::EINVAL)
    );
    // The largest pair that does not overflow is still a legal request.
    let a = ok(args(0, 0, 0, 0, 0, u64::MAX - 1, 1, 0));
    assert_eq!(a.newsp, u64::MAX as usize);
}

#[test]
fn the_child_stack_pointer_is_the_top_of_the_range() {
    // Legacy clone is handed the top of the stack; clone3 gives the low
    // address and a size, and the kernel adds them.
    let a = ok(args(0, 0, 0, 0, 0, 0x7000_0000, 0x10_0000, 0));
    assert_eq!(a.newsp, 0x7010_0000);
    // No stack at all means "share the caller's", not "top of nothing".
    let a = ok(args(0, 0, 0, 0, 0, 0, 0, 0));
    assert_eq!(a.newsp, 0);
}

#[test]
fn a_stack_without_a_size_or_a_size_without_a_stack_is_refused() {
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, 0, 0x7000_0000, 0, 0)),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, 0, 0, 0x10_0000, 0)),
        Err(LxError::EINVAL)
    );
}

#[test]
fn the_exit_signal_is_folded_back_into_the_legacy_flags_word() {
    // clone3 gives the exit signal its own field; legacy clone packs it
    // into the low byte of flags, which is what sys_clone reads.
    let a = ok(args(0x0011_0f00, 0, 0, 0, 17, 0, 0, 0));
    assert_eq!(a.flags & 0xff, 17, "SIGCHLD must survive the conversion");
    assert_eq!(a.flags & !0xff, 0x0011_0f00);
}

#[test]
fn flags_carrying_a_signal_in_their_low_byte_are_refused() {
    // In clone3 the low byte of flags is reserved; a caller putting the
    // signal there as well would have it counted twice.
    assert_eq!(
        clone3_to_clone(args(0x0011_0f11, 0, 0, 0, 0, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
}

/// `CLONE_CLEAR_SIGHAND` (1<<32) and friends are not in `CloneFlags`.
/// Truncating them used to answer success while leaving handlers alone;
/// glibc/`posix_spawn` need `EINVAL` so they can fall back.
#[test]
fn an_unknown_clone3_flag_is_einval_not_a_silent_success() {
    const CLONE_CLEAR_SIGHAND: u64 = 1 << 32;
    const CLONE_INTO_CGROUP: u64 = 1 << 33;
    assert_eq!(
        clone3_to_clone(args(CLONE_CLEAR_SIGHAND, 0, 0, 0, 0, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        clone3_to_clone(args(CLONE_INTO_CGROUP, 0, 0, 0, 0, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
    // A known bit beside an unknown one is still EINVAL.
    let with_vfork = CloneFlags::VFORK.bits() as u64 | CLONE_CLEAR_SIGHAND;
    assert_eq!(
        clone3_to_clone(args(with_vfork, 0, 0, 0, 0, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
    // Known flags still decode.
    assert!(clone3_to_clone(args(CloneFlags::VFORK.bits() as u64, 0, 0, 0, 0, 0, 0, 0)).is_ok());
}

/// Legacy `clone` truncates the same unknown bits; it must refuse them
/// for the same reason `clone3` does.
#[test]
fn legacy_clone_also_rejects_unknown_flags() {
    const CLONE_CLEAR_SIGHAND: usize = 1 << 32;
    assert_eq!(
        clone_flags_from_word(CLONE_CLEAR_SIGHAND),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        clone_flags_from_word(CloneFlags::VFORK.bits() | CLONE_CLEAR_SIGHAND),
        Err(LxError::EINVAL)
    );
    let known = CloneFlags::VFORK.bits() | 17; // SIGCHLD in CSIGNAL
    let f = clone_flags_from_word(known).unwrap();
    assert!(f.contains(CloneFlags::VFORK));
    assert_eq!(f.bits() & 0xff, 17);
}

#[test]
fn a_signal_number_that_does_not_exist_is_refused() {
    assert!(clone3_to_clone(args(0, 0, 0, 0, 64, 0, 0, 0)).is_ok());
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, 65, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
    assert_eq!(
        clone3_to_clone(args(0, 0, 0, 0, u64::MAX, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
}

#[test]
fn the_pidfd_and_the_parent_tid_each_keep_their_own_field() {
    let pidfd_flag = CloneFlags::PIDFD.bits() as u64;
    for flags in [pidfd_flag, 0] {
        let a = ok(args(flags, 0xfd00, 0, 0xba5e, 0, 0, 0, 0));
        assert_eq!((a.pidfd, a.parent_tid), (0xfd00, 0xba5e), "{flags:#x}");
    }
}

#[test]
fn a_pidfd_and_a_parent_tid_can_both_be_asked_for_but_not_at_one_word() {
    // Linux 6.18 takes the two together from `clone3`, where each has a
    // field, and refuses them at one address (`kernel_clone`), where one
    // would land on the other.
    let pidfd = CloneFlags::PIDFD.bits() as u64;
    let settid = CloneFlags::PARENT_SETTID.bits() as u64;
    let a = ok(args(pidfd | settid, 0x1000, 0, 0x2000, 0, 0, 0, 0));
    assert_eq!((a.pidfd, a.parent_tid), (0x1000, 0x2000));
    assert_eq!(
        clone3_to_clone(args(pidfd | settid, 0x1000, 0, 0x1000, 0, 0, 0, 0)),
        Err(LxError::EINVAL)
    );
    // One word is only a clash when both are asked for.
    for flags in [pidfd, settid, 0] {
        assert!(clone3_to_clone(args(flags, 0x1000, 0, 0x1000, 0, 0, 0, 0)).is_ok());
    }
}

#[test]
fn the_remaining_fields_are_passed_through_untouched() {
    let a = ok(args(0x0001_0000, 0, 0xc71d, 0xba5e, 0, 0, 0, 0x715));
    assert_eq!(a.child_tid, 0xc71d);
    assert_eq!(a.tls, 0x715);
    assert_eq!(a.parent_tid, 0xba5e);
}

#[test]
fn version_zero_of_the_struct_is_sixty_four_bytes() {
    assert_eq!(CLONE_ARGS_SIZE_VER0, 64);
    assert_eq!(CLONE_ARGS_SIZE_VER0 / 8, 8, "eight u64 fields");
}
