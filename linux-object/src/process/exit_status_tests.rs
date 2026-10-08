//! The status word a `wait(2)` hands back, which is the ONLY thing a
//! parent learns about how its child finished.
//!
//! `sys/wait.h` packs two different endings into one int and tells them
//! apart by the low seven bits. This kernel only ever built one of the
//! two shapes: the default-action kill path stored `128 + signo` -- the
//! number a SHELL prints, which the shell computes ITSELF from
//! `WIFSIGNALED` -- and `wait` shifted it up eight bits like any exit
//! code. So every process the kernel killed was reported as one that had
//! called `exit(128 + n)`.

use super::*;

/// The macros from `sys/wait.h`, spelled as glibc spells them, so the
/// tests below ask the questions userspace asks.
fn wifexited(status: i32) -> bool {
    status & 0x7f == 0
}
fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}
fn wifsignaled(status: i32) -> bool {
    // `((signed char) (((status) & 0x7f) + 1) >> 1) > 0`
    ((((status & 0x7f) + 1) as i8) >> 1) > 0
}
fn wtermsig(status: i32) -> i32 {
    status & 0x7f
}

#[test]
fn an_ordinary_exit_is_an_exit_with_its_code() {
    for code in [0i64, 1, 2, 42, 127, 255] {
        let status = wait_status_exited(code);
        assert!(wifexited(status), "exit({})", code);
        assert!(!wifsignaled(status), "exit({})", code);
        assert_eq!(wexitstatus(status), code as i32);
    }
}

/// `exit(2)` takes an `int` and the parent sees only its low byte -- which
/// is why every shell script that ends in `exit(256)` reports success.
#[test]
fn only_the_low_byte_of_an_exit_code_reaches_the_parent() {
    assert_eq!(wexitstatus(wait_status_exited(256)), 0);
    assert_eq!(wexitstatus(wait_status_exited(257)), 1);
    // And the whole word, not just what WEXITSTATUS masks back out: a
    // status carrying bits above the second byte is not the status Linux
    // hands over, and userspace is free to compare the int itself
    // (`status == 0` is the idiom for "the command worked").
    assert_eq!(wait_status_exited(256), 0);
    assert_eq!(wait_status_exited(0x1234_5601), 1 << 8);
}

/// The shape that did not exist. `system()` and every supervisor use
/// `WIFSIGNALED` to tell a command that failed from one that was
/// interrupted, and a shell prints "Killed" off it.
#[test]
fn a_death_by_signal_says_so_and_names_the_signal() {
    for sig in [
        LinuxSignal::SIGHUP,
        LinuxSignal::SIGINT,
        LinuxSignal::SIGKILL,
        LinuxSignal::SIGSEGV,
        LinuxSignal::SIGPIPE,
        LinuxSignal::SIGTERM,
    ] {
        let status = wait_status_exited(exit_code_killed_by(sig as u8));
        assert!(wifsignaled(status), "killed by {:?}", sig);
        assert!(!wifexited(status), "killed by {:?}", sig);
        assert_eq!(wtermsig(status), sig as i32);
    }
}

/// And the two are distinguishable, which is the whole point: storing
/// `128 + signo` made a SIGKILL indistinguishable from a program that
/// really does `exit(137)` -- and both then read as an ordinary exit.
#[test]
fn a_program_that_exits_with_137_is_not_a_process_killed_by_sigkill() {
    let exited = wait_status_exited(128 + LinuxSignal::SIGKILL as i64);
    let killed = wait_status_exited(exit_code_killed_by(LinuxSignal::SIGKILL as u8));
    assert_ne!(exited, killed);
    assert!(wifexited(exited) && !wifsignaled(exited));
    assert!(wifsignaled(killed) && !wifexited(killed));
    assert_eq!(wexitstatus(exited), 137);
    assert_eq!(wtermsig(killed), 9);
}

/// A stopped child is the third shape, and it must not collide with the
/// other two: `0x7f` in the low byte is what `WIFSTOPPED` looks for.
#[test]
fn a_stop_is_neither_of_the_two() {
    let status = wait_status_stopped(LinuxSignal::SIGTSTP as u8);
    assert!(!wifexited(status));
    assert!(
        !wifsignaled(status),
        "0x7f is the stop marker, not a signal"
    );
    assert_eq!(status & 0xff, 0x7f);
}
