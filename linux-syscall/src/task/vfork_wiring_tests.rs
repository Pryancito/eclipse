//! `vfork(2)` and the fork-like `clone(2)`, called by their own numbers: the
//! wait that is the whole point of a `vfork`, and the one bit that decides
//! whether the child runs in its parent's address space or in a copy of it.
//!
//! `vfork_impl` was 13 of its lines at zero and `fork_impl` one: nothing had
//! ever asked this kernel for a `vfork`. `clone_refusal_tests` covers which
//! flag words are refused, and `clone_shares_without_thread` is a pure
//! helper with its own tests, but no test had ever let a fork-like clone
//! through to the end, so nothing said what the two accepted shapes --
//! `CLONE_VFORK|CLONE_VM|SIGCHLD`, which is `vfork(2)` and every libc's
//! `posix_spawn`, and `CLONE_VFORK|SIGCHLD`, which is not -- actually do
//! differently.
//!
//! What makes this testable at all is that the child's one thread has no
//! call registered with `on_a_thread`, so it ends as soon as it is started.
//! That is exactly the state a `vfork` waits for, so the parent's wait is
//! satisfied by the bench rather than hanging in it; if the wait were ever
//! wired to something that does not fire, these tests would time out and
//! say so instead of blocking the suite.
//!
//! It is also the limit of what they can see. The child is gone by the time
//! the parent gets its pid, so there is no child object left to ask which
//! address space it was given, and a child that never runs a user
//! instruction writes nothing into the one it shares. So `CLONE_VM` is
//! exercised on both sides here but not DISCRIMINATED: what the two shapes
//! do differently needs a child that runs, which is QEMU's job.
//!
//! Fixed pids live in 48_5xx.

use super::*;
use crate::on_a_thread::on_a_thread;
use alloc::boxed::Box;
use linux_object::process::ProcessExt;
use rcore_fs_ramfs::RamFS;
use zircon_object::task::ROOT_JOB;

const EINVAL: isize = -(LxError::EINVAL as isize);

/// `<linux/sched.h>`, written out rather than read back from this kernel's
/// own `CloneFlags`: a bit that drifted would still match itself, and the
/// only thing that would break is the libc that sends the number.
const CLONE_VM: usize = 1 << 8;
/// See [`CLONE_VM`].
const CLONE_VFORK: usize = 1 << 14;
/// See [`CLONE_VM`].
const CLONE_THREAD: usize = 1 << 16;
/// `SIGCHLD`, which a fork-like clone names as the child's exit signal.
const SIGCHLD: usize = 17;

fn a_process(pid: KoID, name: &str) -> Arc<Process> {
    let proc =
        Process::create_with_fixed_id_ext(&ROOT_JOB, pid, name, LinuxProcess::new(RamFS::new(), 0))
            .unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

async fn by_number(sys: &Syscall<'_>, num: Sys, args: [usize; 6]) -> isize {
    let mut own = Syscall {
        thread: sys.thread,
        thread_fn: sys.thread_fn,
        syscall_entry: sys.syscall_entry,
    };
    own.syscall(num as u32, args).await
}

async fn vfork(sys: &Syscall<'_>) -> isize {
    by_number(sys, Sys::VFORK, [0; 6]).await
}

async fn fork(sys: &Syscall<'_>) -> isize {
    by_number(sys, Sys::FORK, [0; 6]).await
}

async fn clone(sys: &Syscall<'_>, flags: usize) -> isize {
    by_number(sys, Sys::CLONE, [flags, 0, 0, 0, 0, 0]).await
}

/// `waitpid(pid, NULL, WNOHANG)`: the pid when that child is already
/// waitable, 0 while it is still running.
async fn reap_now(sys: &Syscall<'_>, pid: isize) -> isize {
    by_number(sys, Sys::WAIT4, [pid as usize, 0, 1, 0, 0, 0]).await
}

/// The name of child `pid`, asked while the call that forked it is still
/// running: the child is in its parent's job until its own thread finishes
/// dying, which is where this finds it.
fn name_of(proc: &Arc<Process>, pid: isize) -> alloc::string::String {
    proc.job()
        .find_process(pid as KoID)
        .unwrap_or_else(|| panic!("no process {} in the job", pid))
        .name()
}

/// `vfork(2)` comes back with the child's pid and only once the child is
/// gone: that wait is the whole contract of the call, and the reason a
/// `posix_spawn` can read the child's errno out of the shared page
/// afterwards.
///
/// What says the wait happened is that the child is ALREADY waitable when
/// the pid arrives: a `waitpid(pid, WNOHANG)` made right afterwards answers
/// with that same pid rather than 0. Nothing else in this bench can see it,
/// and the same question asked of a plain `fork` would be a race rather
/// than a fact, since `fork` promises nothing about when its child runs.
///
/// The pids are not compared with each other: collecting one hands it
/// straight back to the allocator, so the next fork here is given the very
/// same number, and that is correct -- a pid is free the moment its zombie
/// is reaped.
#[test]
fn a_vfork_comes_back_only_once_the_child_is_gone() {
    let parent = a_process(48_500, "vforker");
    let (vforked, reapable, forked) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let vforked = vfork(sys).await;
            let reapable = reap_now(sys, vforked).await;
            let forked = fork(sys).await;
            (vforked, reapable, forked)
        })
    });
    assert!(vforked > 0, "vfork answered {}", vforked);
    assert_eq!(
        reapable, vforked,
        "the child was already waitable, so vfork had waited for it"
    );
    assert!(forked > 0, "fork answered {}", forked);
    parent.exit(0);
}

/// The three shapes of a fork-like `clone` that this kernel accepts, and
/// what each of them promises: `SIGCHLD` alone is a plain `fork` and waits
/// for nothing; `CLONE_VFORK|SIGCHLD` waits, with a copied address space;
/// and `CLONE_VFORK|CLONE_VM|SIGCHLD` waits too, which is `vfork(2)` itself
/// and every libc's `posix_spawn`.
///
/// The two that carry `CLONE_VFORK` both come back with their child already
/// waitable, which is the half of `CLONE_VM`'s branch this bench can see;
/// which address space the child got it cannot, as the module comment says.
#[test]
fn the_two_vfork_shapes_both_wait_and_the_plain_fork_does_not() {
    let parent = a_process(48_501, "shapes");
    let (copied, copied_reapable, shared, shared_reapable, plain) =
        on_a_thread(&parent, move |sys| {
            Box::pin(async move {
                let copied = clone(sys, CLONE_VFORK | SIGCHLD).await;
                let copied_reapable = reap_now(sys, copied).await;
                let shared = clone(sys, CLONE_VFORK | CLONE_VM | SIGCHLD).await;
                let shared_reapable = reap_now(sys, shared).await;
                let plain = clone(sys, SIGCHLD).await;
                (copied, copied_reapable, shared, shared_reapable, plain)
            })
        });
    assert!(copied > 0, "CLONE_VFORK|SIGCHLD answered {}", copied);
    assert_eq!(
        copied_reapable, copied,
        "a vfork without CLONE_VM waits for its child just the same"
    );
    assert!(
        shared > 0,
        "CLONE_VFORK|CLONE_VM|SIGCHLD answered {}",
        shared
    );
    assert_eq!(
        shared_reapable, shared,
        "and so does the one that shares the address space"
    );
    assert!(plain > 0, "SIGCHLD alone answered {}", plain);
    parent.exit(0);
}

/// `CLONE_VM` on its own, with no `CLONE_THREAD` to make it a thread and no
/// `CLONE_VFORK` to make it a `vfork`, is the shape this kernel does not
/// implement, and it is refused rather than half-done.
///
/// The pair matters: the same bit is accepted the moment `CLONE_VFORK` joins
/// it, so the refusal is about the combination and not about `CLONE_VM`.
#[test]
fn clone_vm_without_a_thread_or_a_vfork_is_refused() {
    let parent = a_process(48_503, "lonevm");
    let (alone, with_vfork) = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            let alone = clone(sys, CLONE_VM | SIGCHLD).await;
            let with_vfork = clone(sys, CLONE_VM | CLONE_VFORK | SIGCHLD).await;
            (alone, with_vfork)
        })
    });
    assert_eq!(
        alone, EINVAL,
        "a shared address space with nobody to share it"
    );
    assert!(
        with_vfork > 0,
        "the same bit with CLONE_VFORK is a vfork: {}",
        with_vfork
    );
    parent.exit(0);
}

/// A forked child is named after the image its parent is running, as
/// `/proc/<pid>/comm` reports it, and after only its last component: that
/// name is what `ps` prints for a child that has not exec'd yet.
///
/// A parent with no image to name leaves the child with NO name: the name
/// the parent was created with is not carried over, so `ps` would print an
/// empty `comm` for it. In a running system every process has an image, so
/// the case does not arise there; it is written down here because it is what
/// the code does and nothing said so.
#[test]
fn a_forked_child_is_named_after_the_image_its_parent_runs() {
    let named = a_process(48_504, "placeholder");
    named.linux().set_execute_path("/usr/bin/busybox");
    let nameless = a_process(48_505, "placeholder");

    let from_the_path = on_a_thread(&named, move |sys| {
        Box::pin(async move {
            let pid = fork(sys).await;
            assert!(pid > 0, "fork answered {}", pid);
            name_of(sys.zircon_process(), pid)
        })
    });
    let with_no_path = on_a_thread(&nameless, move |sys| {
        Box::pin(async move {
            let pid = fork(sys).await;
            assert!(pid > 0, "fork answered {}", pid);
            name_of(sys.zircon_process(), pid)
        })
    });

    assert_eq!(
        from_the_path, "busybox",
        "the last component of the image, not the whole path"
    );
    assert_eq!(
        with_no_path, "",
        "a fork names the child after the image, and there is none"
    );
    named.exit(0);
    nameless.exit(0);
}

/// A fork-like `clone` whose flag word also asks for a thread's shape is
/// refused: `CLONE_THREAD` needs `CLONE_SIGHAND`, which needs `CLONE_VM`,
/// and a word that names the first without the others is `EINVAL` rather
/// than a process pretending to be a thread.
#[test]
fn a_thread_without_the_flags_a_thread_needs_is_refused() {
    let parent = a_process(48_506, "halfthread");
    let got = on_a_thread(&parent, move |sys| {
        Box::pin(async move {
            [
                clone(sys, CLONE_THREAD).await,
                clone(sys, CLONE_THREAD | CLONE_VM).await,
            ]
        })
    });
    assert_eq!(
        got, [EINVAL; 2],
        "a thread needs CLONE_SIGHAND and CLONE_VM, both"
    );
    parent.exit(0);
}
