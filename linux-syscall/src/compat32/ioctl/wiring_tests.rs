//! The dispatch itself, end to end: a 32-bit process makes an `ioctl` and it
//! arrives.
//!
//! [`super::tests`] pins the table; this file pins that the table is
//! *consulted*. Without it, deleting the `I386_IOCTL` arm from
//! [`crate::compat32`] would leave every test in this directory green and
//! every 32-bit `ioctl` answering `ENOSYS` again, because `ioctl` is in
//! neither allowlist and so the translation table has nothing to say about
//! it either.
//!
//! Everything here goes through `compat32_syscall` by number rather than
//! calling `sys_ioctl32` directly, for the same reason: what is being tested
//! is the wiring, and calling the handler by name would skip it.
//!
//! `FIOCLEX`/`FIONCLEX` are the request to do it with. They take no argument
//! at all, so nothing in this test depends on a pointer, an alignment or a
//! structure --- and they have a *visible effect* that a second system call
//! can read back, which an `ioctl` that answered 0 and did nothing would
//! fail. That is the same pair, and the same read-back, as step 58 of the
//! i386 probe.
//!
//! Fixed pids live in 47_46x; 47_45x is `unaligned_tests` and everything
//! below 47_450 was already taken (see that file's note on why a collision
//! unregisters a running process).

extern crate std;

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use alloc::string::String;
use linux_object::process::{Abi, LinuxProcess};
use rcore_fs::vfs::FileType;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::{Process, ROOT_JOB};

/// i386 numbers, spelled here rather than imported: the whole point of the
/// test is to enter the layer the way a 32-bit program does, by number.
const IOCTL: u32 = 54;
const FCNTL64: u32 = 221;

/// `F_GETFD`, whose answer is the fd-table flags --- bit 0 is `FD_CLOEXEC`.
const F_GETFD: usize = 1;
const FD_CLOEXEC: isize = 1;

const FIOCLEX: usize = 0x5451;
const FIONCLEX: usize = 0x5450;

/// `BLKGETSIZE`, the request whose payload is a `long` and so is four bytes
/// for the caller and eight for the native handler. The refusal has to be
/// `ENOSYS` and not `ENOTTY`: see the module documentation of [`super`].
const BLKGETSIZE: usize = 0x1260;

/// The `Syscall` the harness lends, as a `&mut` one. `compat32_syscall`
/// takes `&mut self` (some i386-only calls it dispatches to do), and every
/// field of a `Syscall` is `Copy`; this is the same helper
/// `task::exit_wiring_tests` uses for the same reason.
fn as_mut<'a>(sys: &Syscall<'a>) -> Syscall<'a> {
    Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    }
}

/// A 32-bit process with `/<top>/f` in it.
fn a_process(pid: KoID, top: &str) -> Arc<Process> {
    let lp = LinuxProcess::new(RamFS::new(), 0);
    let top_dir = lp.root_inode().create(top, FileType::Dir, 0o755).unwrap();
    top_dir.create("f", FileType::File, 0o644).unwrap();
    lp.set_abi(Abi::Linux32);
    Process::create_with_fixed_id_ext(&ROOT_JOB, pid, "ioctl32", lp).unwrap()
}

/// A vouched request reaches the native handler and does what it says; an
/// unvouched one is refused without reaching it.
///
/// The close-on-exec bit is read back through `fcntl64(F_GETFD)` --- itself
/// made through the 32-bit dispatcher --- so an `ioctl` arm that returned 0
/// without touching the fd table would fail here rather than pass.
#[test]
fn a_32_bit_ioctl_reaches_the_native_handler_only_for_a_vouched_request() {
    let path = String::from("/ioc32/f\0");
    let at = path.as_ptr() as usize;

    let proc = a_process(47_460, "ioc32");
    let (set, after_set, cleared, after_clear, unvouched) = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = sys
                .sys_openat(FileDesc::CWD, UserInPtr::from(at), 0, 0)
                .expect("the file this process was created with") as usize;

            let set = sys.compat32_syscall(IOCTL, [fd, FIOCLEX, 0, 0, 0, 0]).await;
            let after_set = sys.sys_fcntl64(fd, F_GETFD, 0).await;
            let cleared = sys
                .compat32_syscall(IOCTL, [fd, FIONCLEX, 0, 0, 0, 0])
                .await;
            let after_clear = sys.sys_fcntl64(fd, F_GETFD, 0).await;
            let unvouched = sys
                .compat32_syscall(IOCTL, [fd, BLKGETSIZE, 0, 0, 0, 0])
                .await;
            (set, after_set, cleared, after_clear, unvouched)
        })
    });

    assert_eq!(
        set, 0,
        "ioctl(FIOCLEX) from a 32-bit process was not answered"
    );
    assert_eq!(
        after_set.map(|f| f as isize & FD_CLOEXEC),
        Ok(FD_CLOEXEC),
        "FIOCLEX answered but the close-on-exec bit did not move"
    );
    assert_eq!(cleared, 0, "ioctl(FIONCLEX) was not answered");
    assert_eq!(
        after_clear.map(|f| f as isize & FD_CLOEXEC),
        Ok(0),
        "FIONCLEX answered but the bit stayed set"
    );
    assert_eq!(
        unvouched,
        -(LxError::ENOSYS as isize),
        "an unconverted request must be ENOSYS, not ENOTTY and not a success"
    );
}

/// And `fcntl64` is the second call in that test rather than a bystander: it
/// has to be reachable through the same dispatcher, or the read-back above
/// would be measuring nothing.
#[test]
fn the_read_back_call_is_itself_reachable_from_the_32_bit_dispatcher() {
    let path = String::from("/ioc32b/f\0");
    let at = path.as_ptr() as usize;

    let proc = a_process(47_461, "ioc32b");
    let got = on_a_thread(&proc, move |sys| {
        Box::pin(async move {
            let mut sys = as_mut(sys);
            let fd = sys
                .sys_openat(FileDesc::CWD, UserInPtr::from(at), 0, 0)
                .expect("the file this process was created with") as usize;
            sys.compat32_syscall(FCNTL64, [fd, F_GETFD, 0, 0, 0, 0])
                .await
        })
    });
    assert!(
        got >= 0,
        "fcntl64(F_GETFD) through the 32-bit dispatcher answered {}",
        got
    );
}
