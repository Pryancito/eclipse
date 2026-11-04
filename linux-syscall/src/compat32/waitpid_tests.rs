//! `waitpid` (7), the one call in this layer that has **no native number at
//! all**.
//!
//! x86_64 kept only `wait4` (61) and `waitid` (247). There is no
//! `waitpid` to translate number 7 INTO, so a row in either allowlist is not
//! merely wrong, it is impossible --- and that is why a table built by
//! matching names could never have found this one. Linux draws the same line
//! from the other side: `sys_waitpid` is compiled only where
//! `CONFIG_ARCH_WANT_SYS_WAITPID` is set, which 32-bit x86 sets and x86_64
//! does not, and its body is one line:
//!
//! ```c
//! SYSCALL_DEFINE3(waitpid, pid_t, pid, int __user *, stat_addr, int, options)
//! {
//!     return kernel_wait4(pid, stat_addr, options, NULL);
//! }
//! ```
//!
//! What it cost: a 32-bit process could **create** a child --- `clone` (120)
//! went in the tanda 32 --- and then had no way to reap it. Number 7 answered
//! `ENOSYS`, so `wait(2)`, `waitpid(2)` and `system(3)` were all out of
//! reach. Found by step 88 of the sonda in Eclipse, which waits for the child
//! it just made, and not by reading a table.
//!
//! And the number is not free either: **7 is `poll` natively**. The same
//! integer names two different calls, which is the whole argument for a
//! dispatch that answers i386's own numbers before any table is asked.
//!
//! Fixed pids live in 48_58x.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};

/// i386's number for it, which is `poll`'s natively.
const I386_WAITPID_NUM: u32 = 7;

/// `waitpid(-1, ...)`: any child. As it arrives from a 32-bit trap frame,
/// zero-extended, which is the form the handler has to read as a sign.
const ANY_CHILD: usize = 0xffff_ffff;

/// A bit that is in no `wait4` option mask, so `wait4_options` refuses it.
const A_BAD_OPTION: usize = 0x0000_0010;

const ECHILD: isize = -(LxError::ECHILD as isize);
const EINVAL: isize = -(LxError::EINVAL as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "waitpid32", lp).unwrap()
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// Run number 7 through the i386 dispatch with these three arguments.
fn waitpid32(pid: KoID, args: [usize; 3]) -> isize {
    let proc = a_process(pid, Abi::Linux32);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            as_mut(sys)
                .compat32_syscall(I386_WAITPID_NUM, [args[0], args[1], args[2], 0, 0, 0])
                .await
        })
    });
    proc.exit(0);
    got
}

/// **Number 7 is in no translation table, and it must not be**: there is no
/// native number for it to point at.
///
/// The second half is the one that bites: **7 is `poll` natively.** A
/// dispatch that reached the table first, or a table built by number rather
/// than by name, would hand `waitpid`'s three arguments to `poll` --- which
/// reads the first as a `struct pollfd *` and the second as a count.
#[test]
fn number_7_is_waitpid_here_and_poll_natively() {
    assert_eq!(crate::compat32::name(I386_WAITPID_NUM), Some("waitpid"));
    assert_eq!(
        crate::compat32::native_number(I386_WAITPID_NUM),
        None,
        "7 has a row, and the only call it could point at is the wrong one"
    );
    assert_eq!(
        Sys::POLL as u32,
        I386_WAITPID_NUM,
        "7 stopped being poll natively, which is what made this number \
         load-bearing"
    );
    // And i386's own `poll` is a different number again.
    assert_eq!(crate::compat32::name(168), Some("poll"));
}

/// **A 32-bit `waitpid` reaches `wait4` and is answered.**
///
/// `ENOSYS` is what it answered before this tanda, and what step 88 of the
/// sonda hit in Eclipse right after `clone` had given it a child. With no
/// children the right answer is `ECHILD`, and the difference between the two
/// is the whole fix.
#[test]
fn a_32_bit_waitpid_is_answered_and_not_enosys() {
    let got = waitpid32(48_580, [ANY_CHILD, 0, 0]);
    assert_ne!(
        got, ENOSYS,
        "7 is not in the i386 dispatch: a 32-bit process cannot reap a child"
    );
    assert_eq!(
        got, ECHILD,
        "waitpid for any child of a process with none answered {}",
        got
    );
}

/// **And it is `wait4`'s nucleus it reaches**, not something that merely
/// answers: a bit outside the option mask is `EINVAL`, and ahead of the
/// search for a child, which is the order `kernel_wait4` checks in.
///
/// This is the test that says WHICH call the arm is wired to. The process
/// below has no children, so `ECHILD` would come back from almost anything;
/// `EINVAL` for a bad option comes from `wait4_options` alone.
#[test]
fn a_bad_option_is_refused_before_any_child_is_looked_for() {
    let got = waitpid32(48_581, [ANY_CHILD, 0, A_BAD_OPTION]);
    assert_eq!(
        got, EINVAL,
        "a bit outside the mask answered {} rather than EINVAL, so this is \
         not wait4's option check",
        got
    );
}

/// **A native process's number 7 is still `poll`.**
///
/// The guard on the other side: the arm lives in the i386 dispatch, so it
/// cannot reach a 64-bit caller. `poll` with a null `fds` and a count of
/// zero is a sleep of `timeout` milliseconds, and zero milliseconds is
/// `Ok(0)` --- an answer `waitpid` cannot give for a process with no
/// children.
#[test]
fn the_same_number_from_a_native_process_is_still_poll() {
    let proc = a_process(48_582, Abi::Linux);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            as_mut(sys)
                .syscall(Sys::POLL as u32, [0, 0, 0, 0, 0, 0])
                .await
        })
    });
    assert_eq!(
        got, 0,
        "number 7 from a native process answered {}, which is not poll's \
         answer for an empty set",
        got
    );
    proc.exit(0);
}
