use super::*;
use linux_object::signal::Signal as LinuxSignal;

#[test]
fn zero_disarms() {
    assert_eq!(pdeathsig_from_arg(0), Ok(0));
}

#[test]
fn every_signal_is_latched_as_itself() {
    for n in 1..=LinuxSignal::RTMAX {
        assert_eq!(pdeathsig_from_arg(n), Ok(n as u8), "PR_SET_PDEATHSIG {}", n);
    }
}

#[test]
fn a_number_above_nsig_is_rejected_not_truncated() {
    // 265 & 0xff == 9: the child asked for a signal that does not exist and
    // got SIGKILL on its parent's death.
    assert_eq!(pdeathsig_from_arg(265), Err(LxError::EINVAL));
    assert_eq!(pdeathsig_from_arg(65), Err(LxError::EINVAL));
    assert_eq!(pdeathsig_from_arg(256), Err(LxError::EINVAL));
}

#[test]
fn prctl_does_not_truncate_to_int_the_way_kill_does() {
    // prctl(2) declares arg2 `unsigned long`, so unlike kill(2) there is no
    // 32-bit narrowing before `valid_signal()`: the high half makes the
    // number out of range instead of disappearing.
    assert_eq!(
        pdeathsig_from_arg(0xdead_beef_0000_0009),
        Err(LxError::EINVAL)
    );
    assert_eq!(pdeathsig_from_arg(0x1_0000_0000), Err(LxError::EINVAL));
    // The same two words through kill(2)'s rule, for contrast.
    assert_eq!(
        LinuxSignal::from_syscall_arg(0xdead_beef_0000_0009),
        Ok(Some(LinuxSignal::SIGKILL))
    );
    assert_eq!(LinuxSignal::from_syscall_arg(0x1_0000_0000), Ok(None));
}

#[test]
fn a_negative_argument_is_out_of_range() {
    // prctl never sees it as negative; it sees a very large unsigned.
    assert_eq!(pdeathsig_from_arg(usize::MAX), Err(LxError::EINVAL));
    assert_eq!(pdeathsig_from_arg(-9i64 as usize), Err(LxError::EINVAL));
}
