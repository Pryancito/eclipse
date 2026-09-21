//! What a syscall hands back to its caller, and what it takes back when it
//! cannot finish.
//!
//! One rule, in two shapes: **a call that fails must leave the caller exactly
//! as it found them.** Linux keeps it by copying the result out last, and by
//! undoing what it had already installed when that copy faults. Both shapes
//! were written out by hand at each call site here, and at most sites the
//! hand-written version was missing a piece.

use kernel_hal::user::UserOutPtr;
use linux_object::error::LxError;

/// Apply a change, and copy the previous value out only once it succeeded.
///
/// `rt_sigaction`, `rt_sigprocmask`, `sigaltstack`, `setitimer`,
/// `timer_settime` and `timerfd_settime` all take a new value and a pointer to
/// receive the old one, and all of them end in Linux with the `copy_to_user`
/// of the old one: a call that fails leaves the caller's buffer as it found
/// it. Writing it up front instead hands back a value the call never promised,
/// and the reason a program asks for the old state is to restore it later.
///
/// Same shape as the `fd_set` of `select`, which was emptied before the wait
/// could fail. One helper, so the rule cannot be half-applied again.
pub(crate) fn commit_and_report_old<T>(
    old: T,
    out: &mut UserOutPtr<T>,
    change: impl FnOnce() -> Result<(), LxError>,
) -> Result<(), LxError> {
    change()?;
    out.write_if_not_null(old)?;
    Ok(())
}

/// Hand a pair of freshly installed descriptors to the caller, taking them
/// back if anything between here and the caller's copy fails.
///
/// `pipe2` and `socketpair` install two descriptors in the process's table and
/// only then write the numbers into user memory. If that write faults, Linux
/// takes both back (`put_unused_fd` and `fput` in `__do_pipe_flags`); here they
/// stayed installed and the caller never learned their numbers, so nothing
/// left in the process could ever close them. `pipe2((int *)1, 0)` in a loop
/// leaks two descriptors a turn until the fd table is full, and then every
/// `open` in that process fails.
///
/// The same hole opens without a bad pointer at all: when installing the
/// *second* descriptor is what fails — which is exactly what happens at the
/// fd limit — the first one is already in the table.
pub(crate) fn hand_out_pair<D: Copy>(
    first: D,
    make_second: impl FnOnce() -> Result<D, LxError>,
    report: impl FnOnce(D, D) -> Result<(), LxError>,
    take_back: impl Fn(D),
) -> Result<(), LxError> {
    let second = match make_second() {
        Ok(second) => second,
        Err(err) => {
            take_back(first);
            return Err(err);
        }
    };
    hand_out_one(
        second,
        |second| report(first, second),
        |second| {
            take_back(first);
            take_back(second);
        },
    )
}

/// Hand one freshly installed descriptor to the caller, taking it back if the
/// caller never gets the number.
///
/// `clone(CLONE_PIDFD)` installs a pidfd and writes it to `parent_tid`. Linux
/// undoes it when that write faults (`bad_fork_put_pidfd`); here it stayed in
/// the table with nobody able to name it.
pub(crate) fn hand_out_one<D: Copy>(
    handed: D,
    report: impl FnOnce(D) -> Result<(), LxError>,
    take_back: impl FnOnce(D),
) -> Result<(), LxError> {
    match report(handed) {
        Ok(()) => Ok(()),
        Err(err) => {
            take_back(handed);
            Err(err)
        }
    }
}

#[cfg(test)]
mod outparam_tests {
    use super::*;
    use core::cell::RefCell;

    // `libos` addresses are ordinary host addresses, so a local variable is a
    // valid stand-in for the caller's buffer and the copy below runs for real.
    fn user_out<T>(slot: &mut T) -> UserOutPtr<T> {
        UserOutPtr::from(slot as *mut T as usize)
    }

    #[test]
    fn a_call_that_fails_leaves_the_callers_old_value_alone() {
        let mut slot = 0xdead_beefu64;
        let mut out = user_out(&mut slot);
        let err = commit_and_report_old(1234u64, &mut out, || Err(LxError::EFAULT));
        assert_eq!(err, Err(LxError::EFAULT));
        assert_eq!(slot, 0xdead_beef, "a failed call wrote the caller's buffer");
    }

    #[test]
    fn a_call_that_succeeds_reports_the_value_from_before_it() {
        let mut slot = 0xdead_beefu64;
        let mut out = user_out(&mut slot);
        assert_eq!(commit_and_report_old(1234u64, &mut out, || Ok(())), Ok(()));
        assert_eq!(slot, 1234);
    }

    #[test]
    fn the_change_happens_before_the_old_value_is_reported() {
        // Order, not just outcome: the closure must have run by the time the
        // copy out does, or a caller could observe a value nobody set.
        let mut ran = false;
        let mut slot = 0u64;
        let mut out = user_out(&mut slot);
        commit_and_report_old(7u64, &mut out, || {
            ran = true;
            Ok(())
        })
        .expect("nothing failed");
        assert!(ran, "the change never ran");
        assert_eq!(slot, 7);
    }

    #[test]
    fn a_null_out_pointer_is_not_a_fault() {
        // Every one of these syscalls takes NULL for "do not report the old
        // value"; it is the common case for a program that only sets.
        let mut ran = false;
        let mut out: UserOutPtr<u64> = UserOutPtr::from(0usize);
        assert_eq!(
            commit_and_report_old(7u64, &mut out, || {
                ran = true;
                Ok(())
            }),
            Ok(())
        );
        assert!(ran);
    }

    // ---- the descriptor pair ----------------------------------------------

    /// Records every descriptor handed back, in order.
    fn taken_back(
        second: Result<i32, LxError>,
        report: Result<(), LxError>,
    ) -> (Result<(), LxError>, alloc::vec::Vec<i32>) {
        let log = RefCell::new(alloc::vec::Vec::new());
        let result = hand_out_pair(3, || second, |_, _| report, |fd| log.borrow_mut().push(fd));
        let taken = log.into_inner();
        (result, taken)
    }

    #[test]
    fn a_pair_that_reaches_the_caller_is_not_taken_back() {
        assert_eq!(taken_back(Ok(4), Ok(())), (Ok(()), alloc::vec![]));
    }

    #[test]
    fn a_copy_that_faults_takes_both_descriptors_back() {
        // Otherwise they stay in the fd table with nothing left that knows
        // their numbers: a leak the process cannot undo.
        assert_eq!(
            taken_back(Ok(4), Err(LxError::EFAULT)),
            (Err(LxError::EFAULT), alloc::vec![3, 4])
        );
    }

    #[test]
    fn a_second_descriptor_that_never_arrives_takes_the_first_back() {
        // No bad pointer needed: this is what the fd limit looks like.
        assert_eq!(
            taken_back(Err(LxError::EMFILE), Err(LxError::EFAULT)),
            (Err(LxError::EMFILE), alloc::vec![3])
        );
    }

    // ---- one descriptor ---------------------------------------------------

    fn taken_back_one(report: Result<(), LxError>) -> (Result<(), LxError>, alloc::vec::Vec<i32>) {
        let log = RefCell::new(alloc::vec::Vec::new());
        let result = hand_out_one(9, |_| report, |fd| log.borrow_mut().push(fd));
        let taken = log.into_inner();
        (result, taken)
    }

    #[test]
    fn one_descriptor_that_reaches_the_caller_stays() {
        assert_eq!(taken_back_one(Ok(())), (Ok(()), alloc::vec![]));
    }

    #[test]
    fn one_descriptor_whose_number_never_arrives_is_taken_back() {
        // `clone(CLONE_PIDFD)` with a faulting `parent_tid`: the pidfd was
        // installed and its number lost, so nothing left could close it.
        assert_eq!(
            taken_back_one(Err(LxError::EFAULT)),
            (Err(LxError::EFAULT), alloc::vec![9])
        );
    }

    #[test]
    fn a_descriptor_is_handed_out_exactly_once() {
        // Not twice: closing the same fd twice would close whatever the
        // process opened in the meantime under that number.
        let (_, taken) = taken_back_one(Err(LxError::EFAULT));
        assert_eq!(taken.len(), 1);
        let (_, taken) = taken_back(Ok(4), Err(LxError::EFAULT));
        assert_eq!(taken.len(), 2);
        assert_ne!(taken[0], taken[1]);
    }

    #[test]
    fn the_first_error_is_the_one_the_caller_sees() {
        // The take-back must not overwrite the reason. EMFILE is what a
        // program checks for to know it should close something.
        let (result, _) = taken_back(Err(LxError::EMFILE), Ok(()));
        assert_eq!(result, Err(LxError::EMFILE));
    }
}
