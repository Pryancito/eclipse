//! The test-only syscalls, `zx_syscall_test_*`.
//!
//! Zircon publishes these for one purpose: to exercise the generated syscall
//! glue itself, which is why their bodies are arithmetic and their whole
//! point is in the edges. `core-tests`' `SyscallGenerationTest` is the only
//! caller, and until these existed its thirteen cases all failed with
//! `ZX_ERR_NOT_SUPPORTED` -- and `HandleCopyoutFailure` took the whole
//! process down with it, stopping the suite a third of the way through.
//!
//! What each one owes its caller is not a convention this kernel gets to
//! choose: it is whatever `SyscallGenerationTest` asserts, and every value
//! below was read out of the test binary rather than out of documentation.

use super::*;
use crate::user_memory::validate_user_range;
use zircon_object::signal::Event;
use zircon_object::task::ExceptionType;

/// `ZX_EXCP_POLICY_CODE_HANDLE_LEAK`, the `synth_code` a policy exception
/// carries when a syscall made a handle it then could not hand over.
const POLICY_CODE_HANDLE_LEAK: u32 = 20;

/// The sum of `n` arguments, which is what `zx_syscall_test_0` through
/// `zx_syscall_test_8` (and their `_rust_` twins) answer.
///
/// The arguments are `int32_t` on the wire even though they arrive in
/// register-wide slots, so each one is narrowed before it is added: the test
/// passes 1 through 8 and expects 0, 1, 3, 6, 10, 15, 21, 28, 36.
pub(crate) fn syscall_test_sum(args: &[usize]) -> isize {
    args.iter().map(|&a| a as u32 as i32 as isize).sum()
}

/// `zx_syscall_test_wrapper` and `zx_syscall_test_rust_wrapper`: the same sum
/// with the two guards the test exercises.
///
/// The guards are the reason this one is not just `syscall_test_sum`: the test
/// asserts `INVALID_ARGS` for `(-1, 2, 3)` and `OUT_OF_RANGE` for
/// `(10, 20, 30)`, so a negative argument and a total over fifty are errors
/// rather than answers.
pub(crate) fn syscall_test_wrapper(a: usize, b: usize, c: usize) -> isize {
    let args = [a as u32 as i32, b as u32 as i32, c as u32 as i32];
    if args.iter().any(|&a| a < 1) {
        return ZxError::INVALID_ARGS as isize;
    }
    let sum: i32 = args.iter().sum();
    if sum > 50 {
        return ZxError::OUT_OF_RANGE as isize;
    }
    sum as isize
}

/// The four `zx_syscall_test_widening_*` syscalls: the sum of four arguments
/// declared 64, 32, 16 and 8 bits wide.
///
/// The whole point of these is the narrowing, so it happens here and not in
/// the caller: the test passes values that do not fit the declared widths and
/// expects the sum of what fits, `static_cast<uint64_t>(k64) +
/// static_cast<uint32_t>(k32) + static_cast<uint16_t>(k16) +
/// static_cast<uint8_t>(k8)`. The vDSO stubs pass the register through
/// untouched, so a kernel that adds the registers as they arrive gets a
/// different number.
pub(crate) fn syscall_test_widening_unsigned(a: usize, b: usize, c: usize, d: usize) -> isize {
    let sum = (a as u64)
        .wrapping_add(b as u32 as u64)
        .wrapping_add(c as u16 as u64)
        .wrapping_add(d as u8 as u64);
    sum as isize
}

/// [`syscall_test_widening_unsigned`] with the signed declarations: each
/// argument is sign-extended from its own width before it is added.
pub(crate) fn syscall_test_widening_signed(a: usize, b: usize, c: usize, d: usize) -> isize {
    let sum = (a as i64)
        .wrapping_add(b as u32 as i32 as i64)
        .wrapping_add(c as u16 as i16 as i64)
        .wrapping_add(d as u8 as i8 as i64);
    sum as isize
}

impl Syscall<'_> {
    /// The test syscalls whose answer is a number rather than a status.
    ///
    /// `None` means this is not one of them, so the ordinary dispatch runs.
    /// It is one function and not twenty-two match arms in `syscall` because
    /// every one of them answers the same shape, and because the arms that
    /// differ only in how many arguments they take are better read as a
    /// slice length than as twenty near-identical lines.
    pub(crate) fn syscall_test_value(&self, sys_type: &Sys, args: &[usize; 8]) -> Option<isize> {
        let sum = |n: usize| Some(syscall_test_sum(&args[..n]));
        match *sys_type {
            Sys::SYSCALL_TEST_0 | Sys::SYSCALL_TEST_RUST_0 => sum(0),
            Sys::SYSCALL_TEST_1 | Sys::SYSCALL_TEST_RUST_1 => sum(1),
            Sys::SYSCALL_TEST_2 | Sys::SYSCALL_TEST_RUST_2 => sum(2),
            Sys::SYSCALL_TEST_3 | Sys::SYSCALL_TEST_RUST_3 => sum(3),
            Sys::SYSCALL_TEST_4 | Sys::SYSCALL_TEST_RUST_4 => sum(4),
            Sys::SYSCALL_TEST_5 | Sys::SYSCALL_TEST_RUST_5 => sum(5),
            Sys::SYSCALL_TEST_6 | Sys::SYSCALL_TEST_RUST_6 => sum(6),
            Sys::SYSCALL_TEST_7 | Sys::SYSCALL_TEST_RUST_7 => sum(7),
            Sys::SYSCALL_TEST_8 | Sys::SYSCALL_TEST_RUST_8 => sum(8),
            Sys::SYSCALL_TEST_WRAPPER | Sys::SYSCALL_TEST_RUST_WRAPPER => {
                Some(syscall_test_wrapper(args[0], args[1], args[2]))
            }
            Sys::SYSCALL_TEST_WIDENING_UNSIGNED_NARROW
            | Sys::SYSCALL_TEST_WIDENING_UNSIGNED_WIDE => Some(syscall_test_widening_unsigned(
                args[0], args[1], args[2], args[3],
            )),
            Sys::SYSCALL_TEST_WIDENING_SIGNED_NARROW | Sys::SYSCALL_TEST_WIDENING_SIGNED_WIDE => {
                Some(syscall_test_widening_signed(
                    args[0], args[1], args[2], args[3],
                ))
            }
            _ => None,
        }
    }

    /// `zx_syscall_test_rust_inptr`: read one word through the in pointer and
    /// hand it back through the out pointer.
    pub fn sys_syscall_test_rust_inptr(
        &self,
        input: UserInPtr<u32>,
        mut out: UserOutPtr<u32>,
    ) -> ZxResult {
        info!("syscall_test.rust_inptr: in={:#x?}, out={:#x?}", input, out);
        let proc = self.thread.proc();
        validate_user_range(
            proc,
            input.as_addr(),
            core::mem::size_of::<u32>(),
            kernel_hal::MMUFlags::READ,
        )?;
        let value = input.read()?;
        check_out(proc, &out)?;
        out.write(value).map_err(ZxError::from)
    }

    /// `zx_syscall_test_rust_outptr`: hand the argument back through the out
    /// pointer.
    pub fn sys_syscall_test_rust_outptr(&self, value: u32, mut out: UserOutPtr<u32>) -> ZxResult {
        info!(
            "syscall_test.rust_outptr: value={:#x}, out={:#x?}",
            value, out
        );
        check_out(self.thread.proc(), &out)?;
        out.write(value).map_err(ZxError::from)
    }

    /// `zx_syscall_test_rust_inoutptr`: double the word in place.
    pub fn sys_syscall_test_rust_inoutptr(&self, mut inout: UserInOutPtr<u32>) -> ZxResult {
        info!("syscall_test.rust_inoutptr: inout={:#x?}", inout);
        let proc = self.thread.proc();
        validate_user_range(
            proc,
            inout.as_addr(),
            core::mem::size_of::<u32>(),
            kernel_hal::MMUFlags::READ | kernel_hal::MMUFlags::WRITE,
        )?;
        let value = inout.read()?;
        inout.write(value.wrapping_mul(2)).map_err(ZxError::from)
    }

    /// `zx_syscall_test_rust_handle`: hand the handle argument back through
    /// the out pointer.
    ///
    /// The handle is **not** resolved, and that is not an omission. Measured:
    /// the test passes the literal `0x1234567b`, which is nobody's handle, and
    /// asserts both that the call succeeds and that the word comes back. What
    /// the syscall is for is the marshalling of a handle-shaped argument, so
    /// looking the value up in the table would fail every call.
    pub fn sys_syscall_test_rust_handle(
        &self,
        handle: HandleValue,
        mut out: UserOutPtr<u32>,
    ) -> ZxResult {
        info!("syscall_test.rust_handle: handle={:#x}", handle);
        check_out(self.thread.proc(), &out)?;
        out.write(handle).map_err(ZxError::from)
    }

    /// `zx_syscall_test_handle_create`: make a handle, hand it over, and
    /// answer with the status the caller asked for.
    ///
    /// The caller picks the return value, which is the point: the test calls
    /// it once with `ZX_OK` and once with `ZX_ERR_UNAVAILABLE` and expects
    /// each back.
    ///
    /// The order here is deliberately the one every other `*_create` syscall
    /// in this kernel avoids. [`install_handle`] checks the out pointer before
    /// the handle exists, precisely so that a bad pointer cannot leak one.
    /// Here the leak is the subject: `HandleCopyoutFailure` hands in a pointer
    /// it has made unwritable and asserts that the kernel notices it has made
    /// a handle it cannot deliver, and says so as a policy exception carrying
    /// `ZX_EXCP_POLICY_CODE_HANDLE_LEAK`. So the handle is made first, and a
    /// failed copyout is reported rather than quietly avoided.
    ///
    /// The answer is the raw status word and not a `ZxResult`, because the
    /// status is the caller's to choose and nothing here interprets it: there
    /// is no `ZxError` for an arbitrary `int32_t`, and inventing one would mean
    /// this syscall could only hand back the codes this kernel happens to
    /// name.
    pub async fn sys_syscall_test_handle_create(
        &self,
        status: i32,
        mut out: UserOutPtr<HandleValue>,
    ) -> isize {
        info!(
            "syscall_test.handle_create: status={}, out={:#x?}",
            status, out
        );
        let proc = self.thread.proc();
        if status != 0 {
            // Measured: the test asserts the out handle is
            // `ZX_HANDLE_INVALID` after a failed call, so a caller asking for
            // a failure gets no handle made at all. Writing the invalid value
            // rather than leaving the word alone is what says so even to a
            // caller who did not clear it first.
            out.write(INVALID_HANDLE).ok();
            return status as isize;
        }
        let value = proc.add_handle(Handle::new(Event::new(), Rights::DEFAULT_EVENT));
        let delivered = check_out(proc, &out).and_then(|_| out.write(value).map_err(ZxError::from));
        if let Err(err) = delivered {
            // The handle is closed again, so nothing is really leaked -- but
            // the kernel did make one it could not hand over, and that is what
            // the exception reports. The wait is what the test is waiting for:
            // its handler reads the exception and lets go, and only then does
            // this syscall answer.
            proc.remove_handle(value).ok();
            self.thread
                .raise_synth_exception(ExceptionType::PolicyError, POLICY_CODE_HANDLE_LEAK, 0)
                .await;
            return err as isize;
        }
        status as isize
    }
}

/// The arithmetic of the test syscalls, which is the whole of what they are.
#[cfg(test)]
mod tests {
    use super::*;

    /// The sums the test asserts, name by name: `zx_syscall_test_N(1..N)`.
    #[test]
    fn the_sums_are_the_ones_the_test_names() {
        let args: [usize; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let expected = [0, 1, 3, 6, 10, 15, 21, 28, 36];
        for (n, &want) in expected.iter().enumerate() {
            assert_eq!(syscall_test_sum(&args[..n]), want, "test_{}", n);
        }
    }

    /// The arguments are 32 bits wide on the wire, so a register carrying more
    /// than that is narrowed rather than added whole. Without the narrowing a
    /// caller passing `-1` in a 64-bit register would add `u32::MAX`.
    #[test]
    fn an_argument_is_narrowed_to_the_thirty_two_bits_it_is_declared_with() {
        assert_eq!(syscall_test_sum(&[usize::MAX, 1]), 0);
        assert_eq!(syscall_test_sum(&[0xffff_ffff_0000_0002]), 2);
    }

    /// The wrapper's two guards, which are the only thing separating it from
    /// `syscall_test_sum`.
    #[test]
    fn the_wrapper_refuses_a_negative_argument_and_a_total_over_fifty() {
        assert_eq!(syscall_test_wrapper(1, 2, 3), 6);
        assert_eq!(
            syscall_test_wrapper(usize::MAX, 2, 3),
            ZxError::INVALID_ARGS as isize
        );
        assert_eq!(
            syscall_test_wrapper(0, 2, 3),
            ZxError::INVALID_ARGS as isize
        );
        assert_eq!(
            syscall_test_wrapper(10, 20, 30),
            ZxError::OUT_OF_RANGE as isize
        );
        // Fifty is the edge and it is inside: only *more* than fifty is out of
        // range.
        assert_eq!(syscall_test_wrapper(20, 20, 10), 50);
    }

    /// The widening syscalls exist for exactly this: each argument counts only
    /// as wide as it is declared. Adding the registers as they arrive gives a
    /// different number for every one of these cases.
    #[test]
    fn each_widening_argument_counts_only_as_wide_as_it_is_declared() {
        assert_eq!(syscall_test_widening_unsigned(1, 2, 3, 4), 10);
        assert_eq!(
            syscall_test_widening_unsigned(0, 0x1_0000_0001, 0x1_0001, 0x101),
            1 + 1 + 1
        );
        assert_eq!(syscall_test_widening_signed(1, 2, 3, 4), 10);
        // Each one sign-extends from its own width: -1 as a 32-bit, a 16-bit
        // and an 8-bit argument is -1 three times, not three huge numbers.
        assert_eq!(
            syscall_test_widening_signed(0, 0xffff_ffff, 0xffff, 0xff),
            -3
        );
    }
}
