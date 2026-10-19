//! The code `exit(2)` and `exit_group(2)` leave for the parent.
//!
//! Both took the caller's whole `int` and stored it as the process's exit
//! code. This kernel spells a death by signal as a NEGATIVE exit code
//! (`exit_code_killed_by`), so a program that called `exit(-1)` -- the
//! commonest error exit there is -- reached its parent as killed by SIGHUP:
//! `WIFSIGNALED`, `WTERMSIG == 1`, a shell printing "Hangup". Linux keeps
//! the low byte, and `exit(-1)` is `WEXITSTATUS == 255`.

use super::exit_code_of;
use linux_object::process::wait_status_exited;

fn wifexited(status: i32) -> bool {
    status & 0x7f == 0
}

fn wexitstatus(status: i32) -> i32 {
    (status >> 8) & 0xff
}

#[test]
fn a_negative_exit_code_is_an_exit_and_not_a_signal() {
    for (code, byte) in [(-1, 255), (-2, 254), (-128, 128), (-255, 1), (-256, 0)] {
        let status = wait_status_exited(exit_code_of(code));
        assert!(
            wifexited(status),
            "exit({}) gave status {:#x}",
            code,
            status
        );
        assert_eq!(wexitstatus(status), byte, "exit({})", code);
    }
}

#[test]
fn only_the_low_byte_of_any_code_is_kept() {
    for code in [
        0,
        1,
        42,
        255,
        256,
        257,
        0x1234_5601,
        i32::MAX,
        i32::MIN,
        i32::MIN + 1,
    ] {
        let kept = exit_code_of(code);
        assert_eq!(kept, i64::from(code & 0xff), "exit({:#x})", code);
        assert!(wifexited(wait_status_exited(kept)), "exit({:#x})", code);
    }
}
