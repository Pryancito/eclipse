//! `CLONE_SETTLS` hands the new thread an FS base, and the CPU loads it with
//! `wrmsr` on the child's first trip to user mode -- in ring 0, where a
//! non-canonical value is a #GP the kernel does not survive. `arch_prctl`
//! already held the same base to Linux's rule; `clone` did not.

use super::*;
use zircon_object::vm::{USER_ASPACE_BASE, USER_ASPACE_SIZE};

const SETTLS_THREAD: CloneFlags = CloneFlags::from_bits_truncate(
    CloneFlags::VM.bits()
        | CloneFlags::SIGHAND.bits()
        | CloneFlags::THREAD.bits()
        | CloneFlags::SETTLS.bits(),
);

const TOP: usize = (USER_ASPACE_BASE + USER_ASPACE_SIZE) as usize;

#[test]
fn a_tls_pointer_in_user_space_is_taken() {
    assert_eq!(clone_tls_checked(SETTLS_THREAD, 0x7_0000_1000), Ok(()));
    assert_eq!(clone_tls_checked(SETTLS_THREAD, TOP - 1), Ok(()));
}

/// `EPERM`, what `copy_thread` gets back from `do_arch_prctl_64`: the clone
/// fails as a whole, before there is a child to clean up.
#[test]
fn a_tls_pointer_outside_user_space_fails_the_clone() {
    for bad in [TOP, 0x0000_8000_0000_0000, 1 << 63, usize::MAX] {
        assert_eq!(
            clone_tls_checked(SETTLS_THREAD, bad),
            Err(LxError::EPERM),
            "{bad:#x}"
        );
    }
}

/// Without `CLONE_SETTLS` the argument is whatever was in the register:
/// bubblewrap's raw `clone` leaves it undefined, and once handed over
/// `0xffffff9c` there. Refusing that would refuse bwrap.
#[test]
fn without_settls_the_argument_is_not_looked_at() {
    let fork = CloneFlags::from_bits_truncate(17); // SIGCHLD
    assert_eq!(clone_tls_checked(fork, usize::MAX), Ok(()));
    assert_eq!(
        clone_tls_checked(SETTLS_THREAD - CloneFlags::SETTLS, 1 << 63),
        Ok(())
    );
}

/// The same rule as `arch_prctl(ARCH_SET_FS)`, not a second copy of it.
#[test]
fn it_is_the_arch_prctl_rule() {
    for addr in [0, TOP - 1, TOP, 1 << 63] {
        assert_eq!(
            clone_tls_checked(SETTLS_THREAD, addr),
            crate::misc::user_base_settable(addr),
            "{addr:#x}"
        );
    }
}
