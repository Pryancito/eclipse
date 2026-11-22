use super::*;

#[test]
fn valid_combinations_are_accepted() {
    for flags in [
        MCL_CURRENT,
        MCL_FUTURE,
        MCL_CURRENT | MCL_FUTURE,
        MCL_CURRENT | MCL_ONFAULT,
        MCL_FUTURE | MCL_ONFAULT,
        MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT,
    ] {
        assert_eq!(check_mlockall_flags(flags), Ok(0), "{flags:#x}");
    }
}

#[test]
fn empty_lone_onfault_and_unknown_bits_are_einval() {
    for flags in [0, MCL_ONFAULT, 8, MCL_CURRENT | 16, usize::MAX] {
        assert_eq!(
            check_mlockall_flags(flags),
            Err(LxError::EINVAL),
            "{flags:#x}"
        );
    }
}

/// `can_do_mlock()`: a limit of 0 is `EPERM` without `CAP_IPC_LOCK`,
/// and any other limit, or the capability, lets the call go on.
#[test]
fn a_zero_memlock_limit_without_cap_ipc_lock_is_eperm() {
    assert_eq!(mlock_permitted(0, false), Err(LxError::EPERM));
    assert_eq!(mlock_permitted(0, true), Ok(()));
    assert_eq!(mlock_permitted(64 * 1024, false), Ok(()));
    assert_eq!(mlock_permitted(RLIM_INFINITY, false), Ok(()));
}
