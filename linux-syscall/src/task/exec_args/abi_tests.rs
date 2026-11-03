//! The length of one argument, written out instead of derived from the
//! constant it pins.
//!
//! `one_string_may_take_32_pages_with_its_nul` builds its argument out of
//! [`MAX_ARG_STRLEN`], so it holds for whatever that constant says: with the
//! limit at 31 pages that test still passes while `execve` refuses an
//! argument Linux takes, and a program that builds a long one -- a shell
//! expanding a glob, `xargs` filling a command line -- meets an `E2BIG` it
//! has no reason to expect. The value here is Linux's own, so a typo in the
//! constant shows up as a failure rather than as a program that will not
//! start.

use super::*;

/// Linux's `MAX_ARG_STRLEN`, `32 * PAGE_SIZE` with a 4 KiB page
/// (`include/uapi/linux/binfmts.h`), the NUL included. It is what
/// `xargs --show-limits` reports as the longest argument.
const LINUX_MAX_ARG_STRLEN: usize = 131_072;

#[test]
fn one_argument_is_as_long_as_linux_takes_and_not_a_byte_longer() {
    assert_eq!(PAGE_SIZE, 4096, "Linux's number assumes a 4 KiB page");
    assert_eq!(MAX_ARG_STRLEN, LINUX_MAX_ARG_STRLEN);

    // And through the check, which is where a program meets it: the longest
    // argument is one byte short of the limit, because its NUL counts.
    let limit = arg_limit(RLIM_INFINITY);
    let longest = String::from_utf8(vec![b'a'; LINUX_MAX_ARG_STRLEN - 1]).unwrap();
    let one_too_long = String::from_utf8(vec![b'a'; LINUX_MAX_ARG_STRLEN]).unwrap();
    assert_eq!(
        check_arg_sizes("/bin/sh", &[longest], &[], limit, 8),
        Ok(()),
        "{} bytes and its NUL are {} pages exactly",
        LINUX_MAX_ARG_STRLEN - 1,
        LINUX_MAX_ARG_STRLEN / PAGE_SIZE
    );
    assert_eq!(
        check_arg_sizes("/bin/sh", &[one_too_long], &[], limit, 8),
        Err(LxError::E2BIG)
    );
}

/// What one pointer of the new program's table costs the limit.
///
/// Linux counts `sizeof(void *)` of the **new** program
/// (`bprm_stack_limits` through `bprm->p`), which for a 32-bit image is
/// four. Counting eight refuses an argument list that fits, by the size of
/// the table itself --- and the refusal is `E2BIG` from a call that would
/// have worked, which a program has no reason to expect and no way to
/// shorten its way out of except by passing fewer arguments than Linux
/// allows.
#[test]
fn a_pointer_of_the_table_costs_what_the_new_programs_pointer_costs() {
    use linux_object::process::Abi;
    assert_eq!(pointer_size(Abi::Linux32), 4);
    assert_eq!(pointer_size(Abi::Linux), size_of::<usize>());

    // Through the check, with a list that fits at four bytes a pointer and
    // not at eight: four one-byte arguments are 2 + 4 * 2 = 10 bytes of
    // strings with their NULs, and the table is 16 bytes against 32.
    let args: Vec<String> = (0..4).map(|_| String::from("a")).collect();
    assert_eq!(check_arg_sizes("f", &args, &[], 30, 4), Ok(()));
    assert_eq!(
        check_arg_sizes("f", &args, &[], 30, 8),
        Err(LxError::E2BIG),
        "the same list, with the table counted at the native width"
    );
}
