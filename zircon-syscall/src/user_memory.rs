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
