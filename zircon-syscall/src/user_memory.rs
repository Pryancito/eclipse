//! Shared user-address validation for syscall buffers.
use kernel_hal::user::UserOutPtr;
use kernel_hal::MMUFlags;
use zircon_object::{
    object::{Handle, HandleValue},
    task::Process,
    ZxError, ZxResult,
};

pub(crate) fn validate_user_range(
    proc: &Process,
    addr: usize,
    len: usize,
    access: MMUFlags,
) -> ZxResult {
    proc.vmar()
        .check_user_range(addr, len, access)
        .map_err(|_| ZxError::INVALID_ARGS)
}

pub(crate) fn validate_optional_user_range(
    proc: &Process,
    addr: usize,
    len: usize,
    access: MMUFlags,
) -> ZxResult {
    if addr == 0 {
        Ok(())
    } else {
        validate_user_range(proc, addr, len, access)
    }
}

/// Checks an out pointer the way the address space sees it: not null, aligned,
/// and mapped writable in `proc`. `UserPtr::check` alone asks the kernel
/// handler, which libos answers with an unconditional "yes".
pub(crate) fn check_out<T>(proc: &Process, out: &UserOutPtr<T>) -> ZxResult {
    out.check()?;
    validate_user_range(
        proc,
        out.as_addr(),
        core::mem::size_of::<T>(),
        MMUFlags::WRITE,
    )
}

/// Installs `handle` in `proc` and hands its value to userspace, or does
/// neither.
///
/// Every `*_create` syscall used to do `out.write(proc.add_handle(handle))?`:
/// the handle went into the table first, so an unmapped or misaligned `out`
/// answered `INVALID_ARGS` with the handle (and the object behind it) still
/// installed and unreachable to the caller, one leak per failed call.
pub(crate) fn install_handle(
    proc: &Process,
    handle: Handle,
    out: &mut UserOutPtr<HandleValue>,
) -> ZxResult {
    check_out(proc, out)?;
    install_handle_value(proc, proc.add_handle(handle), out)
}

/// [`install_handle`] for a handle `proc` already holds under `value` (a
/// duplicate): the value reaches userspace or the handle is closed again.
pub(crate) fn install_handle_value(
    proc: &Process,
    value: HandleValue,
    out: &mut UserOutPtr<HandleValue>,
) -> ZxResult {
    let written = check_out(proc, out).and_then(|_| out.write(value).map_err(ZxError::from));
    if let Err(e) = written {
        proc.remove_handle(value).ok();
        return Err(e);
    }
    Ok(())
}

/// [`install_handle`] for the syscalls that create two handles at once: both
/// pointers are checked before either handle exists, and if a write still
/// fails both handles come back out of the table.
pub(crate) fn install_handle_pair(
    proc: &Process,
    handles: (Handle, Handle),
    outs: (&mut UserOutPtr<HandleValue>, &mut UserOutPtr<HandleValue>),
) -> ZxResult {
    check_out(proc, outs.0)?;
    check_out(proc, outs.1)?;
    let values = [proc.add_handle(handles.0), proc.add_handle(handles.1)];
    if let Err(e) = outs
        .0
        .write(values[0])
        .and_then(|_| outs.1.write(values[1]))
    {
        proc.remove_handles(&values).ok();
        return Err(e.into());
    }
    Ok(())
}

/// Hand a message's freshly installed handles to the caller, taking them back
/// if the caller never gets their values.
///
/// `zx_channel_read` and `zx_channel_read_etc` install every handle the message
/// carried and only then copy the values (or the `HandleInfo`s) into user
/// memory. The message is already off the channel by that point, so a handle
/// left in the table when that copy fails can never be read again and nothing
/// in the process can name it to close it: the channel, VMO or process behind
/// it stays alive as long as the reader does.
///
/// [`install_handle_pair`] closes the same window for the two-handle `*_create`
/// syscalls, and [`install_handle_value`] for the one-handle ones. This is the
/// N-handle shape, and the one the sweep that wrote those two missed.
///
/// `take_back` is a parameter rather than a `remove_handles` call so the rule
/// can be checked without a process and a mapped user buffer.
pub(crate) fn hand_out_handles<V: Copy>(
    values: &[V],
    report: impl FnOnce(&[V]) -> ZxResult,
    take_back: impl FnOnce(&[V]),
) -> ZxResult {
    match report(values) {
        Ok(()) => Ok(()),
        Err(err) => {
            take_back(values);
            Err(err)
        }
    }
}

#[cfg(test)]
mod hand_out_tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// The bug: a failed copy left every handle of the message in the table.
    #[test]
    fn a_copy_that_faults_gives_every_handle_back() {
        let taken: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        let err = hand_out_handles(
            &[7u32, 8, 9],
            |_| Err(ZxError::INVALID_ARGS),
            |values| taken.borrow_mut().extend_from_slice(values),
        );
        assert_eq!(err, Err(ZxError::INVALID_ARGS));
        assert_eq!(
            *taken.borrow(),
            [7, 8, 9],
            "a failed copy left handles in the table"
        );
    }

    /// And a copy that lands keeps them: the caller now owns them, and taking
    /// them back would close objects it is about to use.
    #[test]
    fn a_copy_that_lands_keeps_them() {
        let taken: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        let got: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        let ok = hand_out_handles(
            &[7u32, 8, 9],
            |values| {
                got.borrow_mut().extend_from_slice(values);
                Ok(())
            },
            |values| taken.borrow_mut().extend_from_slice(values),
        );
        assert_eq!(ok, Ok(()));
        assert_eq!(
            *got.borrow(),
            [7, 8, 9],
            "the caller was handed something else"
        );
        assert!(
            taken.borrow().is_empty(),
            "handles the caller owns were taken back"
        );
    }

    /// A message with no handles at all is the common case, and it neither
    /// reports nothing nor takes anything back.
    #[test]
    fn a_message_with_no_handles_is_still_reported() {
        let reported = RefCell::new(false);
        let ok = hand_out_handles(
            &[] as &[u32],
            |values| {
                assert!(values.is_empty());
                *reported.borrow_mut() = true;
                Ok(())
            },
            |_| panic!("nothing was installed, so nothing can come back"),
        );
        assert_eq!(ok, Ok(()));
        assert!(*reported.borrow(), "the empty copy was skipped");
    }
}
