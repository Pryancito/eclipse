//! `prctl(2)` option by option, asked through the syscall.
//!
//! Twenty-one options and one default arm, and the default arm is the
//! reason this file exists: `sys_prctl` used to answer `Ok(0)` to EVERY
//! option it did not know. A program asking `PR_GET_SECCOMP` was told
//! "not in seccomp mode"; one asking `PR_SET_SECCOMP` was told its filter
//! had been installed. Both are lies a hardening library acts on.
//!
//! The argument gates are covered where they live -- `prctl_arg_tests`,
//! `keepcaps_tests`, `comm_from_path_tests` -- so what is left is the
//! wiring, and here it has a shape of its own: **eight setter/getter
//! pairs over eight different fields**, five of which are booleans. Any
//! two of those five crossed over is invisible to a test that sets them
//! both, so each is set and cleared on its own with the other four read
//! back at every step.
//!
//! And the pairs do not even answer the same way: four getters return the
//! value, two write it through a pointer, one writes sixteen bytes. A
//! getter that returned what it should have written answers 0, which for
//! a boolean is a plausible "no".
//!
//! Fixed pids live in 47_14x here.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::LinuxProcess;
use linux_object::thread::TASK_COMM_LEN;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

/// An option number `prctl` does not define. The real ones end well below
/// this.
const A_MADE_UP_OPTION: i32 = 4242;

/// `SIGUSR1`, a signal that is not the default of anything.
const A_SIGNAL: usize = 10;

/// What a buffer is seeded with, so that "wrote nothing" cannot pass for
/// "wrote zero".
const A_SEED: u8 = 0xaa;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
        .unwrap()
}

/// The address of a host variable, as the `a2` of a `prctl` that writes
/// through it.
fn at<T>(slot: &mut T) -> usize {
    slot as *mut T as usize
}

/// The five boolean flags at once, each read back the way its own getter
/// answers: `dumpable`, `keepcaps`, `thp_disable`, `no_new_privs` return
/// theirs, `child_subreaper` writes it.
fn flags(sys: &Syscall<'_>) -> (usize, usize, usize, usize, usize) {
    let mut subreaper: i32 = -1;
    assert_eq!(
        sys.sys_prctl(PR_GET_CHILD_SUBREAPER, at(&mut subreaper), 0, 0, 0),
        Ok(0)
    );
    (
        sys.sys_prctl(PR_GET_DUMPABLE, 0, 0, 0, 0).unwrap(),
        sys.sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0).unwrap(),
        sys.sys_prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0).unwrap(),
        sys.sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0).unwrap(),
        subreaper as usize,
    )
}

#[test]
fn each_boolean_flag_moves_on_its_own_and_leaves_the_other_four_where_they_were() {
    // Five flags, five bits, one field each. Setting them all and reading
    // them all back would pass with any two of them crossed over, so they
    // go up one at a time and the whole set is read at every step.
    // `no_new_privs` is last because it is the one that cannot be
    // cleared.
    let proc = a_process(47_140, "prctl-flags");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(sys.sys_prctl(PR_SET_DUMPABLE, 0, 0, 0, 0), Ok(0));
            assert_eq!(
                flags(sys),
                (0, 0, 0, 0, 0),
                "a process starts with one of these flags already up"
            );

            assert_eq!(sys.sys_prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0), Ok(0));
            assert_eq!(flags(sys), (0, 1, 0, 0, 0), "keepcaps went somewhere else");
            assert_eq!(sys.sys_prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0), Ok(0));
            assert_eq!(
                flags(sys),
                (0, 1, 1, 0, 0),
                "thp_disable went somewhere else"
            );
            assert_eq!(sys.sys_prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0), Ok(0));
            assert_eq!(
                flags(sys),
                (0, 1, 1, 0, 1),
                "child_subreaper went somewhere else"
            );
            assert_eq!(sys.sys_prctl(PR_SET_DUMPABLE, 1, 0, 0, 0), Ok(0));
            assert_eq!(flags(sys), (1, 1, 1, 0, 1), "dumpable went somewhere else");

            // And down again one at a time, which is a different store
            // from the one above: a setter that only ever wrote `true`
            // would have passed everything so far.
            assert_eq!(sys.sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
            assert_eq!(flags(sys), (1, 0, 1, 0, 1), "keepcaps would not go down");
            assert_eq!(sys.sys_prctl(PR_SET_THP_DISABLE, 0, 0, 0, 0), Ok(0));
            assert_eq!(flags(sys), (1, 0, 0, 0, 1), "thp_disable would not go down");
            assert_eq!(sys.sys_prctl(PR_SET_CHILD_SUBREAPER, 0, 0, 0, 0), Ok(0));
            assert_eq!(
                flags(sys),
                (1, 0, 0, 0, 0),
                "child_subreaper would not go down"
            );

            // Last, because from here there is no way back.
            assert_eq!(sys.sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), Ok(0));
            assert_eq!(
                flags(sys),
                (1, 0, 0, 1, 0),
                "no_new_privs went somewhere else"
            );
        })
    });
}

#[test]
fn an_option_this_kernel_does_not_know_is_einval_and_never_a_quiet_zero() {
    // The arm that matters most and the one a kernel is most tempted to
    // get wrong, because answering 0 makes every `prctl` "work".
    // `PR_SET_SECCOMP` answering 0 tells a hardening library its filter
    // is installed, and from then on it trusts the sandbox it does not
    // have.
    let proc = a_process(47_141, "prctl-unknown");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_prctl(A_MADE_UP_OPTION, 0, 0, 0, 0),
                Err(LxError::EINVAL),
                "an unknown prctl option was accepted"
            );
            assert_eq!(
                sys.sys_prctl(PR_SET_SECCOMP, 1, 0, 0, 0),
                Err(LxError::EINVAL),
                "this kernel said it had installed a seccomp filter"
            );
            assert_eq!(
                sys.sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0),
                Err(LxError::EINVAL),
                "this kernel answered a seccomp mode"
            );
        })
    });
}

#[test]
fn set_dumpable_takes_the_two_values_the_man_page_allows_and_no_others() {
    // "Since Linux 2.6.13 only SUID_DUMP_DISABLE (0) and SUID_DUMP_USER
    // (1) may be set this way": 2 is SUID_DUMP_ROOT, which the kernel
    // sets itself and userspace may not ask for, and anything above it is
    // nothing at all.
    let proc = a_process(47_142, "prctl-dumpable");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(sys.sys_prctl(PR_SET_DUMPABLE, 1, 0, 0, 0), Ok(0));
            for wrong in [2, 3, usize::MAX] {
                assert_eq!(
                    sys.sys_prctl(PR_SET_DUMPABLE, wrong, 0, 0, 0),
                    Err(LxError::EINVAL),
                    "a dumpable value the man page forbids was accepted"
                );
            }
            assert_eq!(
                sys.sys_prctl(PR_GET_DUMPABLE, 0, 0, 0, 0),
                Ok(1),
                "a refused set_dumpable moved the flag anyway"
            );
            assert_eq!(sys.sys_prctl(PR_SET_DUMPABLE, 0, 0, 0, 0), Ok(0));
            assert_eq!(sys.sys_prctl(PR_GET_DUMPABLE, 0, 0, 0, 0), Ok(0));
        })
    });
}

#[test]
fn no_new_privs_wants_a_one_and_three_zeros_and_so_does_its_getter() {
    // prctl(2): arg2 must be 1 -- the flag can never be cleared -- and
    // the remaining arguments must be zero. The getter checks its three
    // too, which is unusual enough to be worth pinning: a kernel that
    // ignored them would quietly accept the call a future option gives a
    // meaning to.
    let proc = a_process(47_143, "prctl-nnp");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            for (a2, a3, a4, a5) in [(0, 0, 0, 0), (2, 0, 0, 0), (1, 1, 0, 0), (1, 0, 1, 0)] {
                assert_eq!(
                    sys.sys_prctl(PR_SET_NO_NEW_PRIVS, a2, a3, a4, a5),
                    Err(LxError::EINVAL),
                    "set_no_new_privs took arguments it should refuse"
                );
            }
            assert_eq!(
                sys.sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 1),
                Err(LxError::EINVAL)
            );
            assert_eq!(
                sys.sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0),
                Ok(0),
                "a refused set_no_new_privs set the flag anyway"
            );

            assert_eq!(
                sys.sys_prctl(PR_GET_NO_NEW_PRIVS, 1, 0, 0, 0),
                Err(LxError::EINVAL)
            );
            assert_eq!(sys.sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), Ok(0));
            assert_eq!(sys.sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), Ok(1));
        })
    });
}

#[test]
fn the_name_is_fifteen_bytes_at_most_stops_at_the_first_nul_and_comes_back_padded() {
    // `strncpy_from_user(comm, arg2, sizeof(comm) - 1)`: fifteen bytes,
    // byte by byte, stopping at the first NUL, so a name of fifteen bytes
    // with no terminator at the end of a mapping is read without touching
    // what lies past it. And `PR_GET_NAME` writes exactly sixteen,
    // NUL-terminated and NUL-padded, which is what a caller that passed a
    // sixteen-byte buffer is counting on.
    let proc = a_process(47_144, "prctl-name");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            // Twenty bytes and a terminator: only the first fifteen are
            // the name.
            let long = b"abcdefghijklmnopqrst\0";
            assert_eq!(
                sys.sys_prctl(PR_SET_NAME, long.as_ptr() as usize, 0, 0, 0),
                Ok(0)
            );

            // Twenty-four bytes of buffer, so that writing more than
            // sixteen shows up too.
            let mut buf = [A_SEED; TASK_COMM_LEN + 8];
            assert_eq!(
                sys.sys_prctl(PR_GET_NAME, buf.as_mut_ptr() as usize, 0, 0, 0),
                Ok(0)
            );
            assert_eq!(
                &buf[..15],
                b"abcdefghijklmno",
                "the name was not cut at fifteen bytes"
            );
            assert_eq!(buf[15], 0, "the name came back without its terminator");
            assert_eq!(
                &buf[TASK_COMM_LEN..],
                &[A_SEED; 8],
                "get_name wrote past the sixteen bytes it promises"
            );

            // A NUL in the middle ends the name, and the rest of the
            // sixteen bytes comes back as padding and not as whatever was
            // in the buffer.
            let embedded = b"ab\0cdef\0";
            assert_eq!(
                sys.sys_prctl(PR_SET_NAME, embedded.as_ptr() as usize, 0, 0, 0),
                Ok(0)
            );
            let mut again = [A_SEED; TASK_COMM_LEN + 8];
            assert_eq!(
                sys.sys_prctl(PR_GET_NAME, again.as_mut_ptr() as usize, 0, 0, 0),
                Ok(0)
            );
            assert_eq!(&again[..2], b"ab", "the name did not stop at the NUL");
            assert_eq!(
                &again[2..TASK_COMM_LEN],
                &[0u8; TASK_COMM_LEN - 2],
                "get_name did not pad the rest of the name with NULs"
            );
        })
    });
}

#[test]
fn a_thread_that_was_never_named_is_called_after_its_executables_basename() {
    // What `/proc/<pid>/comm` reports for a thread nobody has named, and
    // it is the whole program's name rather than its path: a buffer of
    // sixteen bytes could not hold the path anyway, and `ps` prints this.
    let proc = a_process(47_145, "prctl-basename");
    let it = proc.clone();
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            it.linux().set_execute_path("/usr/local/bin/a-program");
            let mut buf = [A_SEED; TASK_COMM_LEN];
            assert_eq!(
                sys.sys_prctl(PR_GET_NAME, buf.as_mut_ptr() as usize, 0, 0, 0),
                Ok(0)
            );
            assert_eq!(
                &buf[..9],
                b"a-program",
                "an unnamed thread was not named after its executable"
            );
            assert_eq!(buf[9], 0, "the fallback name came back unterminated");

            // The fallback is the ONE name that can be longer than
            // fifteen bytes: `PR_SET_NAME` never stores more, but an
            // executable's basename has no such limit, so this is the
            // only path on which the terminator can be lost -- and a
            // caller that reads a sixteen-byte buffer with no NUL in it
            // runs off the end of its own array.
            it.linux()
                .set_execute_path("/opt/a-program-whose-name-is-far-too-long");
            let mut long = [A_SEED; TASK_COMM_LEN];
            assert_eq!(
                sys.sys_prctl(PR_GET_NAME, long.as_mut_ptr() as usize, 0, 0, 0),
                Ok(0)
            );
            assert_eq!(
                &long[..15],
                b"a-program-whose",
                "a long fallback name was not cut at fifteen bytes"
            );
            assert_eq!(
                long[15], 0,
                "a fallback name longer than the buffer came back unterminated"
            );
        })
    });
}

#[test]
fn get_tid_address_reports_the_word_set_tid_address_registered() {
    // The two calls are the only pair in the kernel that crosses this
    // boundary: one is a syscall of its own, the other an option of
    // `prctl`, and they have to agree about one word of user memory.
    let proc = a_process(47_146, "prctl-tid-address");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut word: i32 = 0;
            let registered = at(&mut word);

            let mut reported: usize = A_SEED as usize;
            assert_eq!(
                sys.sys_prctl(PR_GET_TID_ADDRESS, at(&mut reported), 0, 0, 0),
                Ok(0)
            );
            assert_eq!(reported, 0, "a thread that registered nothing has a word");

            assert!(sys
                .sys_set_tid_address(UserOutPtr::from(registered))
                .is_ok());
            assert_eq!(
                sys.sys_prctl(PR_GET_TID_ADDRESS, at(&mut reported), 0, 0, 0),
                Ok(0)
            );
            assert_eq!(
                reported, registered,
                "get_tid_address did not report what set_tid_address stored"
            );
        })
    });
}

#[test]
fn capbset_read_answers_for_every_capability_and_refuses_the_one_past_the_last() {
    // A root-run kernel, so every capability that exists is in the
    // bounding set; what the call has to get right is where the set ends,
    // because a caller walks it from 0 upwards until it is told the number
    // means nothing.
    let proc = a_process(47_147, "prctl-capbset");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            for cap in [0, 1, CAP_LAST_CAP as usize] {
                assert_eq!(
                    sys.sys_prctl(PR_CAPBSET_READ, cap, 0, 0, 0),
                    Ok(1),
                    "a capability that exists was not in the bounding set"
                );
            }
            assert_eq!(
                sys.sys_prctl(PR_CAPBSET_READ, CAP_LAST_CAP as usize + 1, 0, 0, 0),
                Err(LxError::EINVAL),
                "a capability number past the last one was answered"
            );
        })
    });
}

#[test]
fn the_timer_slack_reads_as_the_default_until_something_asks_for_another() {
    // A zero stored means "the default", so the getter cannot simply hand
    // back the field: a thread that asked for 0 is asking to go back to
    // the default, and one that never asked is already there. The number
    // is what a `poll` or `nanosleep` is allowed to round its deadline
    // by, so a thread reading 0 would believe it had none.
    let proc = a_process(47_148, "prctl-timerslack");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            assert_eq!(
                sys.sys_prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0),
                Ok(TIMERSLACK_DEFAULT_NS as usize),
                "a thread that never set a timer slack has none"
            );

            let asked = 1_234_567;
            assert_eq!(sys.sys_prctl(PR_SET_TIMERSLACK, asked, 0, 0, 0), Ok(0));
            assert_eq!(sys.sys_prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0), Ok(asked));

            assert_eq!(sys.sys_prctl(PR_SET_TIMERSLACK, 0, 0, 0, 0), Ok(0));
            assert_eq!(
                sys.sys_prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0),
                Ok(TIMERSLACK_DEFAULT_NS as usize),
                "asking for a zero slack did not go back to the default"
            );
        })
    });
}

#[test]
fn the_parent_death_signal_goes_through_its_own_gate_and_comes_back_by_pointer() {
    // The one option whose setter has a gate of its own (`pdeathsig_from_arg`
    // refuses a number that is no signal) and whose getter writes rather
    // than returns. A getter that returned it would answer 0, which is
    // "no signal" -- a plausible answer, and the wrong one.
    let proc = a_process(47_149, "prctl-pdeathsig");
    on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut got: i32 = -1;
            assert_eq!(
                sys.sys_prctl(PR_GET_PDEATHSIG, at(&mut got), 0, 0, 0),
                Ok(0)
            );
            assert_eq!(got, 0, "a process starts with a parent death signal");

            assert_eq!(sys.sys_prctl(PR_SET_PDEATHSIG, A_SIGNAL, 0, 0, 0), Ok(0));
            assert_eq!(
                sys.sys_prctl(PR_GET_PDEATHSIG, at(&mut got), 0, 0, 0),
                Ok(0)
            );
            assert_eq!(
                got, A_SIGNAL as i32,
                "the parent death signal did not come back through the pointer"
            );

            assert_eq!(
                sys.sys_prctl(PR_SET_PDEATHSIG, 1000, 0, 0, 0),
                Err(LxError::EINVAL),
                "a number that is no signal was taken for one"
            );
            assert_eq!(
                sys.sys_prctl(PR_GET_PDEATHSIG, at(&mut got), 0, 0, 0),
                Ok(0)
            );
            assert_eq!(
                got, A_SIGNAL as i32,
                "a refused set_pdeathsig changed the signal anyway"
            );

            // And zero is how a process says it wants none again.
            assert_eq!(sys.sys_prctl(PR_SET_PDEATHSIG, 0, 0, 0, 0), Ok(0));
            assert_eq!(
                sys.sys_prctl(PR_GET_PDEATHSIG, at(&mut got), 0, 0, 0),
                Ok(0)
            );
            assert_eq!(got, 0, "a parent death signal could not be cleared");
        })
    });
}
