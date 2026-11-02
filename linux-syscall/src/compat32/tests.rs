//! The i386 numbering, and the mapping onto this kernel's own.
//!
//! The mapping is generated, so what is worth checking is not each row but the
//! properties the generator is supposed to guarantee -- and the handful of
//! numbers that are load-bearing ABI, which a program would notice instantly
//! if they moved. Every number asserted here was read out of Linux's own
//! `asm/unistd_32.h`, the same file `i386_syscall.h.in` was transcribed from.

use super::*;
use crate::Sys;
use core::convert::TryFrom;

/// The table is sorted and has no repeated number: both are what
/// [`native_number`] and [`name`] rely on to use a binary search at all.
#[test]
fn the_tables_are_sorted_and_unique() {
    for pair in I386_TO_NATIVE.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "I386_TO_NATIVE is not sorted: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
    for pair in I386_NAMES.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "I386_NAMES is not sorted: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}

/// `syscall_32.tbl` has 451 entries in the kernel this table was taken from,
/// and its numbers run from 0 to 461 with gaps (the numbers withdrawn over the
/// years: 222, 223, 251, 285, and the `*_time64` hole at 415).
#[test]
fn the_i386_table_is_the_whole_table() {
    assert_eq!(I386_NAMES.len(), 451);
    assert_eq!(I386_NAMES.first().map(|r| r.0), Some(0));
    assert_eq!(I386_NAMES.last().map(|r| r.0), Some(461));
    for gap in [222, 223, 251, 285, 387, 415] {
        assert_eq!(name(gap), None, "{} should not be in syscall_32.tbl", gap);
    }
}

/// Every number the mapping points at is a number this kernel's own table
/// knows, and every number it maps FROM is a real i386 call. Both hold by
/// construction -- `build.rs` refuses to emit a row it cannot find in both
/// tables -- which is exactly why they are cheap to assert and worth
/// asserting: a generator that started matching some other way would be caught
/// here and not by a program crashing.
#[test]
fn every_mapped_number_is_a_syscall_on_both_sides() {
    for &(i386, native) in I386_TO_NATIVE {
        assert!(
            name(i386).is_some(),
            "i386 {} is mapped but is not in syscall_32.tbl",
            i386
        );
        assert!(
            Sys::try_from(native).is_ok(),
            "i386 {} ({}) maps to {}, which is not in this kernel's table",
            i386,
            name(i386).unwrap_or("?"),
            native
        );
    }
}

/// The mapping is an allowlist, so it is a small fraction of the table. If it
/// ever covered everything, the allowlist would have stopped being one --
/// which is the failure this guards against, not the count itself.
#[test]
fn the_mapping_is_a_small_allowlist() {
    assert!(
        I386_TO_NATIVE.len() < I386_NAMES.len() / 2,
        "{} of {} numbers are mapped: that is no longer an allowlist",
        I386_TO_NATIVE.len(),
        I386_NAMES.len()
    );
    assert!(!I386_TO_NATIVE.is_empty());
}

/// The numbers a static i386 program uses before it has done anything, and
/// where each one has to land. `write` is the whole point of the first
/// milestone: i386 4 is `write`, where native 4 is `stat`, so a mapping that
/// did nothing would have the first `write` of the first program stat-ing its
/// own buffer.
#[test]
fn the_numbers_a_first_program_uses() {
    assert_eq!(name(4), Some("write"));
    assert_eq!(native_number(4), Some(Sys::WRITE as u32));
    assert_eq!(name(3), Some("read"));
    assert_eq!(native_number(3), Some(Sys::READ as u32));
    assert_eq!(name(1), Some("exit"));
    assert_eq!(native_number(1), Some(Sys::EXIT as u32));
    assert_eq!(name(252), Some("exit_group"));
    assert_eq!(native_number(252), Some(Sys::EXIT_GROUP as u32));
    assert_eq!(name(45), Some("brk"));
    assert_eq!(native_number(45), Some(Sys::BRK as u32));
    assert_eq!(name(20), Some("getpid"));
    assert_eq!(native_number(20), Some(Sys::GETPID as u32));
}

/// `write` is 4 on i386 and 1 natively; `stat` is 106 on i386 and 4 natively.
/// The pair is the clearest statement of why the translation exists, and it
/// also says the translation is not symmetric nonsense: the numbers really do
/// cross over.
#[test]
fn the_numbers_cross_over() {
    assert_eq!(native_number(4), Some(1));
    assert_eq!(Sys::try_from(4u32), Ok(Sys::STAT));
}

/// The calls i386 has to itself must NOT be mapped: each passes something
/// whose 32-bit layout differs, so sending it to the native handler would
/// quietly read the wrong bytes out of the process. They answer `ENOSYS` --
/// with a name -- until each one has its conversion written.
///
/// The ones that now have theirs have moved to
/// [`the_calls_this_kernel_answers_itself_are_not_in_the_table`], which makes
/// the same claim about the table and a stronger one about the dispatch. This
/// list is what is left to write.
#[test]
fn the_i386_only_calls_are_left_unmapped() {
    for (num, called) in [
        (102, "socketcall"),
        (117, "ipc"),
        (119, "sigreturn"),
        (173, "rt_sigreturn"),
        (403, "clock_gettime64"),
        (422, "futex_time64"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) must not be mapped by name: its ABI differs",
            num,
            called
        );
    }
}

/// The credential calls i386 renamed. `getuid32` is mapped -- to native
/// `getuid`, which is a different name and the same ABI -- and `getuid`, the
/// i386 number that predates it, must NOT be, because that one returns an
/// `old_uid_t` of sixteen bits. The pair is the clearest case of why the
/// allowlist had to grow an `i386 = native` form rather than only matching
/// names: matching names would have mapped exactly the wrong one of the two.
#[test]
fn the_renamed_credential_calls_are_mapped_and_their_16_bit_ancestors_are_not() {
    for (num32, called, native) in [
        (199, "getuid32", Sys::GETUID),
        (200, "getgid32", Sys::GETGID),
        (201, "geteuid32", Sys::GETEUID),
        (202, "getegid32", Sys::GETEGID),
        (213, "setuid32", Sys::SETUID),
        (214, "setgid32", Sys::SETGID),
        (205, "getgroups32", Sys::GETGROUPS),
        (206, "setgroups32", Sys::SETGROUPS),
        (212, "chown32", Sys::CHOWN),
    ] {
        assert_eq!(name(num32), Some(called), "i386 {}", num32);
        assert_eq!(
            native_number(num32),
            Some(native as u32),
            "i386 {} ({}) does not reach its native twin",
            num32,
            called
        );
    }
    // And the sixteen-bit ancestors, each of which has its own i386 number
    // and must stay unmapped however much its name matches.
    for (num, called) in [
        (24, "getuid"),
        (47, "getgid"),
        (49, "geteuid"),
        (50, "getegid"),
        (23, "setuid"),
        (46, "setgid"),
        (80, "getgroups"),
        (81, "setgroups"),
        (182, "chown"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) is the 16-bit one: mapping it hands back the overflow uid",
            num,
            called
        );
    }
}

/// The calls the kernel answers ITSELF are absent from the translation table
/// on purpose, and that absence is load-bearing: the dispatch checks them
/// first, so a row here would be dead and a row that pointed at a native
/// number would be wrong. They are not `ENOSYS` either, which is what every
/// other unmapped number is -- the difference is the dispatch, and this is the
/// half of it that can be asserted here.
#[test]
fn the_calls_this_kernel_answers_itself_are_not_in_the_table() {
    for (num, called) in [
        (243, "set_thread_area"),
        (244, "get_thread_area"),
        // And the ones whose native twin exists but reads its arguments
        // differently: a 64-bit offset in two registers, or in 4 KiB units.
        // Mapping any of these by number would hand the native handler one
        // half of an offset as the whole of it.
        (140, "_llseek"),
        (180, "pread64"),
        (181, "pwrite64"),
        (192, "mmap2"),
        (193, "truncate64"),
        (194, "ftruncate64"),
        // And the ones whose answer is a structure of a shape this ABI does
        // not share. `fcntl` is the one that would look safest to map by
        // name: the number exists on both sides and most of its commands
        // take a scalar --- but six of them point at a `struct flock` that
        // is 16 or 24 bytes here and 32 there.
        (195, "stat64"),
        (196, "lstat64"),
        (197, "fstat64"),
        (300, "fstatat64"),
        (221, "fcntl64"),
        (55, "fcntl"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) has no native twin: it must not be mapped",
            num,
            called
        );
    }
}

/// A number in no table at all is `None` from both, not a panic and not a
/// neighbour's answer.
#[test]
fn a_number_in_no_table_maps_to_nothing() {
    for num in [462, 1000, u32::MAX] {
        assert_eq!(name(num), None, "{}", num);
        assert_eq!(native_number(num), None, "{}", num);
    }
}

/// The calls that share a NAME with a native one but not a shape. A mapping
/// built by matching names would have let every one of these through, and each
/// would have misread the caller's memory rather than failed.
///
/// `mmap` is the sharpest: i386 90 is `old_mmap`, which takes its six
/// arguments in a **block of memory** rather than in registers, so the native
/// handler would have read a pointer as a length. `clone` is the quietest:
/// same name, same arity, same types -- and the last two arguments swapped.
#[test]
fn the_calls_that_share_a_name_but_not_a_shape() {
    for (num, called, why) in [
        (90, "mmap", "old_mmap passes its arguments in memory"),
        (82, "select", "old_select passes its arguments in memory"),
        (
            19,
            "lseek",
            "a signed offset, and these registers read zero-extended",
        ),
        (
            120,
            "clone",
            "i386 swaps the last two arguments (tls, ctid)",
        ),
        (11, "execve", "argv and envp are arrays of 4-byte pointers"),
        (
            174,
            "rt_sigaction",
            "struct sigaction holds a handler pointer",
        ),
        (
            54,
            "ioctl",
            "the third argument is whatever the request says",
        ),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert_eq!(
            native_number(num),
            None,
            "i386 {} ({}) must not be mapped: {}",
            num,
            called,
            why
        );
    }
}

/// The calls whose registers are already the native ones but whose
/// **structures** the handler converts, because it asks the calling process
/// how wide a word is: `i386_abi_narrow.in`, Linux's `in_compat_syscall()`.
///
/// They are mapped, and they have to be: the number is all there is left to
/// translate once the conversion lives in the handler. Each one passed one of
/// the six structures whose i386 image is the native one halved --- and each
/// one answered `ENOSYS` until the conversion existed, which is the honest
/// half of this file's history.
#[test]
fn the_calls_whose_structures_the_handler_narrows_are_mapped() {
    for (num, called, what) in [
        (43, "times", "struct tms"),
        (77, "getrusage", "struct rusage"),
        (78, "gettimeofday", "struct timeval"),
        (79, "settimeofday", "struct timeval"),
        (104, "setitimer", "struct itimerval"),
        (105, "getitimer", "struct itimerval"),
        (114, "wait4", "struct rusage"),
        (142, "_newselect", "struct timeval"),
        (145, "readv", "struct iovec"),
        (146, "writev", "struct iovec"),
        (161, "sched_rr_get_interval", "struct timespec"),
        (162, "nanosleep", "struct timespec"),
        (260, "timer_settime", "struct itimerspec"),
        (261, "timer_gettime", "struct itimerspec"),
        (264, "clock_settime", "struct timespec"),
        (265, "clock_gettime", "struct timespec"),
        (266, "clock_getres", "struct timespec"),
        (267, "clock_nanosleep", "struct timespec"),
        (309, "ppoll", "struct timespec"),
        (325, "timerfd_settime", "struct itimerspec"),
        (326, "timerfd_gettime", "struct itimerspec"),
    ] {
        assert_eq!(name(num), Some(called), "i386 {}", num);
        assert!(
            native_number(num).is_some(),
            "i386 {} ({}) passes a {} and must be mapped",
            num,
            called,
            what
        );
    }
}

/// i386's `_newselect` (142) is x86_64's `select` (23), and i386's own
/// `select` (82) is the ancestor that takes its arguments in a block of
/// memory. Mapping the wrong one of the two would hand the native handler a
/// pointer where it expects `nfds`.
#[test]
fn the_select_that_is_mapped_is_the_one_that_uses_registers() {
    assert_eq!(name(142), Some("_newselect"));
    assert_eq!(name(82), Some("select"));
    assert_eq!(
        native_number(142),
        Some(23),
        "_newselect is the native select"
    );
    assert_eq!(native_number(82), None, "old_select takes a memory block");
}

/// `pselect6` is the one to keep out, and the reason is not its timeout ---
/// that is a `timespec`, which is converted. It is the sixth argument, a
/// pointer to `{ const sigset_t *ss; size_t ss_len; }`: two words, so 8
/// bytes to a 32-bit caller against 16 natively. Reading it at the native
/// width would take the length out of the next structure along.
#[test]
fn pselect6_stays_unmapped_for_its_sigset_argument_and_not_its_timeout() {
    assert_eq!(name(308), Some("pselect6"));
    assert_eq!(native_number(308), None);
}
