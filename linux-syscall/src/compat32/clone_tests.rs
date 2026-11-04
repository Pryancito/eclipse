//! `clone` (120) from a 32-bit process: the argument that moved, and the one
//! whose MEANING changed.
//!
//! Both halves were measured on this container's Linux rather than read off a
//! header, because the two x86 ABIs disagree and the disagreement is exactly
//! the kind a name match cannot see:
//!
//! | | fourth argument | fifth |
//! |---|---|---|
//! | **i386** | **tls** | `child_tid` |
//! | **x86_64** | `child_tid` | **tls** |
//!
//! That is Linux's `CONFIG_CLONE_BACKWARDS`, which x86 selects **only in 32
//! bits**. How it was measured: on x86_64 the `tls` is an `FS` base, so a
//! non-canonical value answers `EPERM` before any child exists --- put it in
//! the fifth and the call fails, in the fourth and a child is born. On i386
//! the `tls` is a `struct user_desc *`, so the lever is a valid descriptor in
//! one slot and the address `1` in the other: bad in the fourth is `EFAULT`,
//! bad in the fifth is ignored.
//!
//! And that is the second difference. A native `CLONE_SETTLS` word **is** the
//! thread pointer; a 32-bit one points at the sixteen bytes
//! `set_thread_area` takes, because i386 keeps its thread pointer in a
//! segment base. A kernel that stored the word itself would give the new
//! thread a `%gs` base of whatever address its descriptor happened to live
//! at.
//!
//! Fixed pids live in 48_57x.

use super::*;
use crate::on_a_thread::on_a_thread;
use crate::task::CloneFlags;
use alloc::boxed::Box;
use linux_object::process::{Abi, LinuxProcess};
use linux_object::signal::Signal;
use rcore_fs_ramfs::RamFS;
use zircon_object::object::KoID;
use zircon_object::task::{Process, ROOT_JOB};

/// i386's number, which is the native one too --- what differs is the order
/// of the arguments behind it.
const I386_CLONE: u32 = 120;

const EINVAL: isize = -(LxError::EINVAL as isize);
const EPERM: isize = -(LxError::EPERM as isize);
const ENOSYS: isize = -(LxError::ENOSYS as isize);

/// `CLONE_VM | CLONE_THREAD | CLONE_SIGHAND | CLONE_SETTLS`: a thread, which
/// is the only kind of clone that carries a thread pointer.
fn a_thread_with_tls() -> usize {
    (CloneFlags::THREAD | CloneFlags::SIGHAND | CloneFlags::VM | CloneFlags::SETTLS).bits()
}

/// `GDT_ENTRY_TLS_MIN`, the first entry number a 32-bit thread may name.
pub(super) const FIRST_ENTRY: u32 = 12;

fn a_process(pid: KoID, abi: Abi) -> alloc::sync::Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    lp.set_abi(abi);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "clone32", lp).unwrap()
}

/// The sixteen bytes of a `struct user_desc` that names `entry`, with a base
/// a 32-bit thread could really have.
pub(super) fn a_user_desc(entry: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&entry.to_ne_bytes());
    b[4..8].copy_from_slice(&0x0804_f000u32.to_ne_bytes()); // base_addr
    b[8..12].copy_from_slice(&0x000f_ffffu32.to_ne_bytes()); // limit
                                                             // seg_32bit | limit_in_pages | useable, as `set_thread_area` reads them.
    b[12] = 0x01;
    b[13] = 0x05;
    b
}

/// A `Syscall` as a `&mut` one.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// **The thread pointer of a 32-bit `clone` is read from the FOURTH argument,
/// as the descriptor it is.**
///
/// Two mutations die here: the dispatch passing `(a0, a1, a2, a4, a3)` like
/// the x86_64 arm beside it, which reads the descriptor out of the
/// `child_tid` word; and `set_child_thread_pointer` storing the word itself
/// as the thread pointer, which never reads the descriptor at all and so
/// accepts the garbage in the fifth.
#[test]
fn the_tls_of_a_32_bit_clone_is_the_fourth_argument_and_a_user_desc() {
    let desc = a_user_desc(FIRST_ENTRY);
    let at = desc.as_ptr() as usize;
    let proc = a_process(48_570, Abi::Linux32);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            as_mut(sys)
                .compat32_syscall(I386_CLONE, [a_thread_with_tls(), 0, 0, at, 0xdead_beef, 0])
                .await
        })
    });
    assert_ne!(got, ENOSYS, "120 is not in the i386 dispatch");
    assert!(
        got > 0,
        "clone answered {}: the descriptor was read from somewhere other than \
         the fourth argument, or was not read at all",
        got
    );
    proc.exit(0);
}

/// **And it really is the fourth argument that is looked at**: a bad one is
/// refused even with a perfectly good descriptor in the fifth.
///
/// The mirror of the test above. The refusal is `EPERM` and not `EFAULT`,
/// because the word is checked against the user address space
/// (`clone_tls_checked`) before anything is read through it --- a check that
/// is right for both readings of the word, a base and a pointer. What matters
/// here is WHICH argument it falls on: a kernel using the x86_64 order would
/// check the fifth, find the descriptor's own address, and succeed.
#[test]
fn a_32_bit_clone_with_a_bad_fourth_argument_is_refused() {
    let desc = a_user_desc(FIRST_ENTRY);
    let at = desc.as_ptr() as usize;
    /// Not a user address, with the bound asked for below: on the host every
    /// address is a user one unless a test says otherwise.
    const NOT_USER: usize = 0xffff_ffff_8000_0000;
    let proc = a_process(48_571, Abi::Linux32);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            kernel_hal::user::set_strict_user_half(true);
            let got = as_mut(sys)
                .compat32_syscall(I386_CLONE, [a_thread_with_tls(), 0, 0, NOT_USER, at, 0])
                .await;
            kernel_hal::user::set_strict_user_half(false);
            got
        })
    });
    assert!(
        got < 0,
        "clone answered {}, so the fourth argument was ignored",
        got
    );
    assert_ne!(got, ENOSYS);
    assert_eq!(
        got, EPERM,
        "refused, but not by the user-address-space bound"
    );
    proc.exit(0);
}

/// **A descriptor that names no slot is refused**, which is where this path
/// and `set_thread_area` differ.
///
/// `-1` means "pick one and tell me" to `set_thread_area` and is `EINVAL`
/// here, because Linux's `set_new_tls` calls `do_set_thread_area` with
/// `can_allocate = 0`. Measured: a 32-bit `clone(CLONE_SETTLS)` whose
/// descriptor says `-1` answers `EINVAL` on this container's Linux. Nothing
/// real asks for it --- a library passes the entry its `set_thread_area`
/// already handed it.
#[test]
fn a_clone_descriptor_has_to_name_its_slot() {
    let desc = a_user_desc(u32::MAX);
    let at = desc.as_ptr() as usize;
    let proc = a_process(48_572, Abi::Linux32);
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            as_mut(sys)
                .compat32_syscall(I386_CLONE, [a_thread_with_tls(), 0, 0, at, 0, 0])
                .await
        })
    });
    assert_eq!(got, EINVAL, "clone answered {}", got);
    proc.exit(0);
}

/// **A native process's `CLONE_SETTLS` word is still the thread pointer
/// itself.**
///
/// The guard on the other side: the conversion is the 32-bit process's, not
/// the call's, so a native `clone` must not try to read sixteen bytes through
/// a word that is a plain `FS` base --- `0x0804_f000` is not a descriptor,
/// and reading it as one would answer `EINVAL` on its flags.
#[test]
fn a_native_clone_still_takes_its_tls_as_a_base() {
    const A_BASE: usize = 0x0804_f000;
    let proc = a_process(48_573, Abi::Linux);
    let got = on_a_thread(&proc, |sys| {
        Box::pin(async move {
            as_mut(sys)
                .syscall(Sys::CLONE as u32, [a_thread_with_tls(), 0, 0, 0, A_BASE, 0])
                .await
        })
    });
    assert!(got > 0, "the native clone answered {}", got);
    proc.exit(0);
}

/// 120 must not have a row in either allowlist: the dispatch answers it
/// before the table is asked, and a row would send i386's argument order to
/// the native handler.
#[test]
fn number_120_is_not_in_the_translation_table() {
    assert_eq!(crate::compat32::native_number(I386_CLONE), None);
    assert_eq!(crate::compat32::name(I386_CLONE), Some("clone"));
    // And the signal a clone without CLONE_THREAD reports with is i386's,
    // which happens to be the same number as the native one.
    assert_eq!(Signal::SIGCHLD as usize, 17);
}
