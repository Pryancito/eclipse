//! The arguments and environment `execve` hands the new program, and when
//! they are too big for it.
//!
//! An `argv` of NULL was `EFAULT` and an empty one `EINVAL`. Linux runs the
//! program with one empty argument in both cases (`do_execveat_common`,
//! since 5.18: "process ... launched with NULL argv: empty string added"),
//! so a program started that way sees `argc == 1` and `argv[0] == ""`
//! instead of not starting at all.
//!
//! One argument longer than `MAX_ARG_STRLEN` (32 pages, its NUL included)
//! was taken, where Linux refuses it with `E2BIG` (`valid_arg_len`); and the
//! total was a quarter of the stack the loader maps whatever `RLIMIT_STACK`
//! said, so a shell that lowered it with `ulimit -s` still got the full
//! amount.

use super::*;
use linux_object::process::{LinuxProcess, RLIMIT_STACK, RLIM_INFINITY};

/// `MAX_ARG_STRLEN`: the longest one argument or environment string may be,
/// its NUL included.
pub(super) const MAX_ARG_STRLEN: usize = 32 * PAGE_SIZE;

/// `ARG_MAX` in Linux's own sense: what the strings may take however low
/// `RLIMIT_STACK` is.
const ARG_FLOOR: usize = 32 * PAGE_SIZE;

/// `_STK_LIM / 4 * 3`: what the strings may take however high
/// `RLIMIT_STACK` is.
const ARG_CEILING: usize = 6 << 20;

/// `argv` as the new program gets it: what the caller passed, or one empty
/// string when that is no argument at all.
pub(super) fn read_argv(argv: UserInPtr<UserInPtr<u8>>) -> LxResult<Vec<String>> {
    let mut args = if argv.is_null() {
        Vec::new()
    } else {
        argv.read_cstring_array()?
    };
    if args.is_empty() {
        args.push(String::new());
    }
    Ok(args)
}

/// How many bytes of strings and pointers an exec may hand the new program
/// with `RLIMIT_STACK` at `stack_limit`: a quarter of it, within
/// `[ARG_FLOOR, ARG_CEILING]` (`bprm_stack_limits`), and never more than the
/// stack the loader maps can hold ([`ARG_MAX`]).
pub(super) fn arg_limit(stack_limit: u64) -> usize {
    let quarter = if stack_limit == RLIM_INFINITY {
        ARG_CEILING
    } else {
        usize::try_from(stack_limit / 4).unwrap_or(ARG_CEILING)
    };
    quarter.clamp(ARG_FLOOR, ARG_CEILING).min(ARG_MAX)
}

/// `E2BIG` when one string is longer than [`MAX_ARG_STRLEN`] or all of them
/// -- the file name, the arguments and the environment, each with its NUL,
/// and a pointer for each argument and variable -- take more than `limit`.
pub(super) fn check_arg_sizes(
    filename: &str,
    args: &[String],
    envs: &[String],
    limit: usize,
) -> LxResult<()> {
    let strings = || core::iter::once(filename).chain(args.iter().chain(envs).map(String::as_str));
    if strings().any(|s| s.len() + 1 > MAX_ARG_STRLEN) {
        return Err(LxError::E2BIG);
    }
    let pointers = (args.len() + envs.len()) * size_of::<usize>();
    let bytes: usize = strings().map(|s| s.len() + 1).sum::<usize>() + pointers;
    if bytes > limit {
        return Err(LxError::E2BIG);
    }
    Ok(())
}

/// [`check_arg_sizes`] against `proc`'s `RLIMIT_STACK`.
pub(super) fn check_exec_args(
    proc: &LinuxProcess,
    filename: &str,
    args: &[String],
    envs: &[String],
) -> LxResult<()> {
    let stack = proc.rlimit(RLIMIT_STACK, None, false)?.cur;
    check_arg_sizes(filename, args, envs, arg_limit(stack))
}

#[cfg(test)]
mod abi_tests;
#[cfg(test)]
mod tests;
