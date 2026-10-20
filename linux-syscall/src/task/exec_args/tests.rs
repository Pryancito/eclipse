//! What an exec takes for `argv`, and when it says `E2BIG`, as Linux 6.18
//! does (measured with `proc-survey2`).

use super::*;
use linux_object::process::{RLimit, USER_STACK_SIZE};
use rcore_fs_ramfs::RamFS;

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| String::from(*s)).collect()
}

fn argv_at(pointers: &[usize]) -> UserInPtr<UserInPtr<u8>> {
    UserInPtr::from(pointers.as_ptr() as usize)
}

#[test]
fn a_null_or_an_empty_argv_is_one_empty_argument() {
    assert_eq!(read_argv(UserInPtr::from(0)).unwrap(), strings(&[""]));
    assert_eq!(read_argv(argv_at(&[0])).unwrap(), strings(&[""]));
    let (a, b) = (b"uno\0", b"dos\0");
    assert_eq!(
        read_argv(argv_at(&[a.as_ptr() as usize, b.as_ptr() as usize, 0])).unwrap(),
        strings(&["uno", "dos"]),
        "an argv with arguments is what it says"
    );
}

#[test]
fn one_string_may_take_32_pages_with_its_nul() {
    let most = "a".repeat(MAX_ARG_STRLEN - 1);
    let over = "a".repeat(MAX_ARG_STRLEN);
    let limit = usize::MAX;
    assert!(check_arg_sizes("f", &[most.clone()], &[], limit).is_ok());
    assert_eq!(
        check_arg_sizes("f", &[String::new(), over.clone()], &[], limit),
        Err(LxError::E2BIG)
    );
    assert_eq!(
        check_arg_sizes("f", &[], &[over.clone()], limit),
        Err(LxError::E2BIG),
        "an environment string"
    );
    assert_eq!(
        check_arg_sizes(&over, &[], &[], limit),
        Err(LxError::E2BIG),
        "the file name"
    );
    assert!(check_arg_sizes(&most.clone(), &[most.clone()], &[most], limit).is_ok());
}

#[test]
fn the_total_is_the_strings_with_their_nuls_and_a_pointer_for_each() {
    let (args, envs) = (strings(&["ab"]), strings(&["c"]));
    // "f\0", "ab\0" and "c\0" are 7 bytes; two pointers 16.
    assert!(check_arg_sizes("f", &args, &envs, 23).is_ok());
    assert_eq!(check_arg_sizes("f", &args, &envs, 22), Err(LxError::E2BIG));
}

#[test]
fn the_total_may_be_a_quarter_of_rlimit_stack_within_linux_s_bounds() {
    assert_eq!(arg_limit(1 << 20), 256 << 10);
    assert_eq!(arg_limit(USER_STACK_SIZE), 2 << 20);
    assert_eq!(arg_limit(64 << 10), 128 << 10, "never under 32 pages");
    assert_eq!(arg_limit(0), 128 << 10);
    for high in [RLIM_INFINITY, 1 << 40, 64 << 20] {
        assert_eq!(
            arg_limit(high),
            ARG_MAX.min(6 << 20),
            "a quarter of {:#x}: no more than 6 MiB nor than the stack holds",
            high
        );
    }
}

#[test]
fn an_exec_answers_to_the_caller_s_rlimit_stack() {
    let proc = LinuxProcess::new(RamFS::new(), 0);
    let args = vec!["a".repeat(4095); 75]; // 300 KiB
    assert!(check_exec_args(&proc, "/bin/prog", &args, &[]).is_ok());
    let lowered = RLimit {
        cur: 1 << 20,
        max: RLIM_INFINITY,
    };
    proc.rlimit(RLIMIT_STACK, Some(lowered), false).unwrap();
    assert_eq!(
        check_exec_args(&proc, "/bin/prog", &args, &[]),
        Err(LxError::E2BIG),
        "300 KiB under a 1 MiB stack, whose quarter is 256"
    );
}
