//! A `clone` that fails **after** the child exists: what the parent is left
//! with.
//!
//! `fork_from` files the new child in its parent's child table and stamps it
//! `Running` before `start_forked_thread` runs, and that function can still
//! fail --- `CLONE_SETTLS` reads a `struct user_desc` out of user memory
//! there, which is the one argument of a 32-bit `clone` whose contents the
//! kernel has to agree with. The error reached the caller and the child
//! stayed: no thread, so it never ran, never exited, never became a zombie
//! and never left the table.
//!
//! The parent then never heard `ECHILD` again. `wait_child_any_interest`
//! answers it only when neither the live table nor the zombies hold a
//! candidate, so the phantom turned every later `wait` into `EAGAIN`, which
//! `WNOHANG` reports as **0**: "you have a child, it is not ready". A loop
//! that reaps until `ECHILD` never ends.
//!
//! Found by the sonda in Eclipse and not by reading: its step 85 expects
//! `ECHILD` from a `waitid` and got 0, in the second image --- the one that
//! had just run step 88, whose `clone` is **meant** to be refused.
//!
//! Fixed pids live in 48_59x.

use super::clone_tests::{a_user_desc, FIRST_ENTRY};
use super::*;
use crate::on_a_thread::on_a_thread;
use crate::task::CloneFlags;
use alloc::boxed::Box;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::Signal;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};

/// i386's `clone`, which is the native number too.
const I386_CLONE: u32 = 120;
/// i386's `waitpid`, the number this layer answers itself (tanda 34). The
/// reaping call the sonda uses, and the one a 32-bit `wait(2)` makes.
const I386_WAITPID: u32 = 7;

/// `waitpid(-1, ...)` as it arrives from a 32-bit frame: zero-extended.
const ANY_CHILD: usize = 0xffff_ffff;
const WNOHANG: usize = 1;

const ECHILD: isize = -(LxError::ECHILD as isize);
const EINVAL: isize = -(LxError::EINVAL as isize);

/// A child **process** that carries a thread pointer: no `CLONE_THREAD`, so
/// this goes through `fork_impl` and files a child, and `CLONE_SETTLS`, so
/// the descriptor is read after it has. Plus the exit signal in the low byte,
/// as every `fork` names it.
fn a_fork_with_tls() -> usize {
    CloneFlags::SETTLS.bits() | Signal::SIGCHLD as usize
}

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "clone-unwind", lp).unwrap()
}

fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A 32-bit `clone` with `desc` as its thread pointer, and then a
/// `waitpid(-1, WNOHANG)`: what the clone answered, and what the parent is
/// told about its children afterwards.
fn clone_then_wait(pid: KoID, desc: [u8; 16]) -> (isize, isize) {
    let at = desc.as_ptr() as usize;
    let proc = a_process(pid, Abi::Linux32);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let cloned = as_mut(sys)
                .compat32_syscall(I386_CLONE, [a_fork_with_tls(), 0, 0, at, 0, 0])
                .await;
            let waited = as_mut(sys)
                .compat32_syscall(I386_WAITPID, [ANY_CHILD, 0, WNOHANG, 0, 0, 0])
                .await;
            (cloned, waited)
        })
    });
    proc.exit(0);
    got
}

/// **A `clone` that is refused leaves no child behind.**
///
/// The descriptor names slot `-1`, which `set_new_tls` refuses
/// (`can_allocate = 0`), so the failure lands in `set_child_thread_pointer`
/// --- past `fork_from`, which has already filed the child. The clone is
/// refused either way; what this test is about is the line after it.
///
/// Two mutations die here: dropping `abandon_unstarted_child` from the error
/// path of `start_forked_thread` (the child stays in the table and the wait
/// answers 0), and taking the child out of the table without exiting it (the
/// wait is right, but its address space and its slot in the job are kept for
/// as long as the machine runs --- which this test cannot see, and the one
/// below is why the first half is not enough).
#[test]
fn a_refused_clone_leaves_no_child_to_wait_for() {
    let (cloned, waited) = clone_then_wait(48_590, a_user_desc(u32::MAX));
    assert_eq!(
        cloned, EINVAL,
        "the clone was not refused where this test needs it refused"
    );
    assert_eq!(
        waited, ECHILD,
        "the parent was told it has a child: the refused clone left a phantom \
         in the child table, and every later wait answers 0 instead of ECHILD"
    );
}

/// **And a `clone` that succeeds still leaves one**, so the `ECHILD` above is
/// the undo and not a `wait` that has stopped seeing children at all.
///
/// `WNOHANG` over a live child is `0`: "you have one, it is not ready". That
/// is the exact answer the phantom produced, which is why the two tests have
/// to be read together --- a kernel that answered `ECHILD` to both would pass
/// the one above and lose every `wait` in the system.
#[test]
fn a_clone_that_works_does_leave_a_child() {
    let (cloned, waited) = clone_then_wait(48_591, a_user_desc(FIRST_ENTRY));
    assert!(
        cloned > 0,
        "the clone answered {}, so this test is not measuring a live child",
        cloned
    );
    assert_eq!(
        waited, 0,
        "a live child that has not exited is 0 under WNOHANG"
    );
}
