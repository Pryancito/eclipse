//! Which signal the parent gets when a child exits, and when that is decided.
//!
//! `clone(2)` packs it into the low byte of the flags word (`CSIGNAL`) and
//! `clone3` carries the same number in a field of its own, with the same
//! meaning -- including `0`, which means send nothing at all. Nothing
//! separated the two halves of that word in a place a test could name, and
//! the separation is where an off-by-a-mask lives.
//!
//! The other half of this is an ORDER, and it is why these tests exist: the
//! value used to be stored on the way BACK from the fork, after the child's
//! thread had already been started. A child that exited in that window died
//! under the default instead of what the caller asked for. The default is
//! what [`a_forked_child_starts_out_sending_sigchld`] pins, because it is
//! what the window handed out; the fork path now takes the signal as an
//! argument and stores it before the child can run.
//!
//! Fixed pids live in 43_38x here; they are a global table, so every file
//! that pins one has its own range.

use super::*;
use linux_object::process::LinuxProcess;
use linux_object::signal::Signal as LinuxSignal;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

fn a_parent(pid: KoID) -> Arc<Process> {
    let parent = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "parent",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&parent).unwrap();
    parent
}

/// musl's `fork`: a bare `SIGCHLD` with no flags at all.
#[test]
fn a_plain_fork_asks_for_sigchld() {
    assert_eq!(exit_signal_of_clone(0x11), LinuxSignal::SIGCHLD as u8);
}

/// glibc's `arch_fork`, where the signal shares the word with two flags.
#[test]
fn the_flags_above_the_signal_are_not_part_of_it() {
    // CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID | SIGCHLD.
    assert_eq!(
        exit_signal_of_clone(0x0120_0011),
        LinuxSignal::SIGCHLD as u8
    );
    // CLONE_VM | CLONE_VFORK | SIGCHLD, which is what `posix_spawn` asks for.
    assert_eq!(exit_signal_of_clone(0x4111), LinuxSignal::SIGCHLD as u8);
}

/// A signal that is not `SIGCHLD` is the case the mask has to carry: a parent
/// waiting on `SIGUSR1` for its child's death gets nothing at all if the
/// number is dropped or widened.
#[test]
fn a_signal_other_than_sigchld_comes_through_as_itself() {
    assert_eq!(
        exit_signal_of_clone(0x0100_0000 | LinuxSignal::SIGUSR1 as usize),
        LinuxSignal::SIGUSR1 as u8
    );
}

/// `0` is not "unset": it is `clone3`'s way of asking for no signal, and it
/// has to survive as a zero. It is also what the whole fix is about, because
/// the default is NOT zero.
#[test]
fn an_exit_signal_of_zero_stays_zero() {
    assert_eq!(exit_signal_of_clone(0), 0);
    // And the flags above it do not put anything back.
    assert_eq!(exit_signal_of_clone(0x0012_3400), 0);
}

/// What a child is born with, which is what the old order handed out to any
/// child that exited before the caller's choice was stored: `SIGCHLD`, not
/// the caller's signal and not "none".
#[test]
fn a_forked_child_starts_out_sending_sigchld() {
    let parent = a_parent(43_381);
    let child = Process::fork_from(&parent).unwrap();
    assert_eq!(
        child.linux().exit_signal(),
        LinuxSignal::SIGCHLD as u8,
        "so a child that died in the old window died under SIGCHLD"
    );
    // Which is a different death from either of the two a caller can ask for:
    // `0` leaves the zombie and sends nothing, and another signal does not go
    // through the `SIGCHLD` disposition at all (`child_death_policy`).
    assert_ne!(child.linux().exit_signal(), 0);
}

/// `CLONE_PARENT`: the child's death goes to the caller's parent, with the
/// signal the caller's own goes with, and not with the one in the word.
#[test]
fn a_clone_parent_child_takes_no_signal_from_the_flags() {
    let sibling = 0x8000 | LinuxSignal::SIGUSR1 as usize;
    assert_eq!(
        child_of_clone(CloneFlags::from_bits_truncate(sibling), sibling),
        ChildOf::CallersParent
    );
    let fork = LinuxSignal::SIGUSR1 as usize;
    assert_eq!(
        child_of_clone(CloneFlags::from_bits_truncate(fork), fork),
        ChildOf::Caller(LinuxSignal::SIGUSR1 as u8)
    );
    assert_eq!(child_of_clone(CloneFlags::empty(), 0), ChildOf::Caller(0));
}
