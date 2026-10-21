//! What `unshare(2)` answers, flag by flag, as Linux 6.18 does
//! (`clone-survey`), and the one place it cannot: a thread of several.

use super::*;

const ALONE: Sharing = Sharing {
    threads: false,
    memory: false,
};
const THREADED: Sharing = Sharing {
    threads: true,
    memory: false,
};
const VFORKED: Sharing = Sharing {
    threads: false,
    memory: true,
};

#[test]
fn nothing_at_all_is_0_whoever_asks() {
    assert_eq!(unshare_plan(0, ALONE), Ok(false));
    assert_eq!(unshare_plan(0, THREADED), Ok(false));
    assert_eq!(unshare_plan(0, VFORKED), Ok(false));
}

/// `check_unshare_flags` looks at the bits before anything else: a namespace
/// asked for with a bit it does not know is `EINVAL`, not the `ENOSYS` of
/// the namespace.
#[test]
fn a_flag_it_does_not_know_is_einval_before_anything_else() {
    for bits in [
        0x8000_0000,
        0x8000_0000 | CloneFlags::NEWUSER.bits(),
        0x11, // SIGCHLD, which is a clone's and not an unshare's
        CloneFlags::VFORK.bits(),
        CloneFlags::PARENT.bits(),
        CloneFlags::SETTLS.bits(),
        CloneFlags::PIDFD.bits(),
    ] {
        assert_eq!(
            unshare_plan(bits, ALONE),
            Err(LxError::EINVAL),
            "{:#x}",
            bits
        );
    }
}

/// The answer bubblewrap and glycin are built on (see the `Sys::UNSHARE`
/// arm): every namespace, `CLONE_NEWTIME` included, is still `ENOSYS`,
/// alone or with other flags.
#[test]
fn a_namespace_is_still_enosys() {
    for bits in [
        CloneFlags::NEWNS.bits(),
        CloneFlags::NEWCGROUP.bits(),
        CloneFlags::NEWUTS.bits(),
        CloneFlags::NEWIPC.bits(),
        CloneFlags::NEWUSER.bits(),
        CloneFlags::NEWPID.bits(),
        CloneFlags::NEWNET.bits(),
        0x80,
        CloneFlags::NEWUSER.bits() | CloneFlags::FS.bits(),
    ] {
        assert_eq!(
            unshare_plan(bits, ALONE),
            Err(LxError::ENOSYS),
            "{:#x}",
            bits
        );
        assert_eq!(
            unshare_plan(bits, THREADED),
            Err(LxError::ENOSYS),
            "{:#x}",
            bits
        );
    }
}

/// One process with one thread shares its working directory and its
/// descriptors with nobody: `clone` refuses `CLONE_FS` and `CLONE_FILES`
/// without `CLONE_THREAD`.
#[test]
fn the_cwd_and_the_descriptors_of_one_thread_are_0_and_nothing_to_do() {
    let fs_files = CloneFlags::FS.bits() | CloneFlags::FILES.bits();
    assert_eq!(unshare_plan(CloneFlags::FS.bits(), ALONE), Ok(false));
    assert_eq!(unshare_plan(CloneFlags::FILES.bits(), ALONE), Ok(false));
    assert_eq!(unshare_plan(fs_files, ALONE), Ok(false));
    // A `vfork` child has its own of both.
    assert_eq!(unshare_plan(fs_files, VFORKED), Ok(false));
}

/// Linux gives the one thread its own; here they are the process's, so a 0
/// would leave that thread's `chdir` and `close` moving every other thread.
#[test]
fn a_thread_of_several_cannot_have_its_own_cwd_descriptors_or_undo_list() {
    for flag in [CloneFlags::FS, CloneFlags::FILES, CloneFlags::SYSVSEM] {
        assert_eq!(
            unshare_plan(flag.bits(), THREADED),
            Err(LxError::EINVAL),
            "{:?}",
            flag
        );
    }
}

/// `exit_sem`, for the process's one thread.
#[test]
fn clone_sysvsem_alone_gives_back_the_undo_list() {
    assert_eq!(unshare_plan(CloneFlags::SYSVSEM.bits(), ALONE), Ok(true));
    assert_eq!(
        unshare_plan(CloneFlags::SYSVSEM.bits() | CloneFlags::FS.bits(), ALONE),
        Ok(true)
    );
    assert_eq!(unshare_plan(CloneFlags::FS.bits(), ALONE), Ok(false));
}

/// `CLONE_VM` takes `CLONE_SIGHAND`, which takes `CLONE_THREAD`: none of the
/// three is anything to a process with one thread, and each is `EINVAL` to
/// one with more.
#[test]
fn the_thread_group_the_handlers_and_the_memory_need_one_thread() {
    for flag in [CloneFlags::THREAD, CloneFlags::SIGHAND, CloneFlags::VM] {
        assert_eq!(unshare_plan(flag.bits(), ALONE), Ok(false), "{:?}", flag);
        assert_eq!(
            unshare_plan(flag.bits(), THREADED),
            Err(LxError::EINVAL),
            "{:?} with another thread",
            flag
        );
    }
}

/// `current_is_single_threaded`: an address space another process runs in
/// is shared, whatever the threads.
#[test]
fn the_memory_of_a_vfork_child_is_shared() {
    assert_eq!(
        unshare_plan(CloneFlags::VM.bits(), VFORKED),
        Err(LxError::EINVAL)
    );
    // The handlers and the thread group are its own.
    assert_eq!(unshare_plan(CloneFlags::SIGHAND.bits(), VFORKED), Ok(false));
    assert_eq!(unshare_plan(CloneFlags::THREAD.bits(), VFORKED), Ok(false));
}

// Through the syscall. Fixed pids live in 44_42x.

mod through_the_syscall {
    use super::*;
    use crate::ipc::{SemBuf, SemFlags, SemctlCmds};
    use crate::on_a_thread::on_a_thread;
    use alloc::boxed::Box;
    use linux_object::process::LinuxProcess;
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::ROOT_JOB;

    fn a_process(pid: KoID) -> Arc<Process> {
        Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "unshare",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap()
    }

    /// Made the way a program makes it: by number, through the table, where
    /// the arm answered `ENOSYS` to all of it.
    async fn unshare(sys: &Syscall<'_>, flags: usize) -> isize {
        let mut own = Syscall {
            thread: sys.thread,
            thread_fn: sys.thread_fn,
            syscall_entry: sys.syscall_entry,
        };
        own.syscall(crate::Sys::UNSHARE as u32, [flags, 0, 0, 0, 0, 0])
            .await
    }

    #[test]
    fn the_table_reaches_it_and_a_namespace_is_still_enosys() {
        let proc = a_process(44_421);
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                assert_eq!(unshare(sys, 0).await, 0);
                assert_eq!(
                    unshare(sys, CloneFlags::FS.bits() | CloneFlags::FILES.bits()).await,
                    0
                );
                assert_eq!(
                    unshare(sys, CloneFlags::NEWUSER.bits()).await,
                    -(LxError::ENOSYS as isize)
                );
                assert_eq!(unshare(sys, 0x8000_0000).await, -(LxError::EINVAL as isize));
            })
        });
    }

    fn getval(sys: &Syscall<'_>, id: usize) -> usize {
        sys.sys_semctl(id, 0, SemctlCmds::GETVAL as usize, 0)
            .unwrap()
    }

    /// `exit_sem` without the exit: what the process owes goes back, and the
    /// next `SEM_UNDO` starts a list of its own, which goes back the same way.
    #[test]
    fn clone_sysvsem_gives_back_what_sem_undo_took() {
        let proc = a_process(44_422);
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                let id = sys.sys_semget(0, 1, 0o600).unwrap();
                let up = [SemBuf::new(0, 2, SemFlags::SEM_UNDO.bits())];
                sys.sys_semop(id, (up.as_ptr() as usize).into(), 1)
                    .await
                    .unwrap();
                assert_eq!(getval(sys, id), 2);
                assert!(sys.linux_process().semaphores_undo_list());
                assert_eq!(sys.sys_unshare(CloneFlags::SYSVSEM.bits()), Ok(0));
                assert_eq!(getval(sys, id), 0, "the SEM_UNDO was not given back");
                assert!(
                    !sys.linux_process().semaphores_undo_list(),
                    "the list outlived its exit_sem"
                );
                let one = [SemBuf::new(0, 1, SemFlags::SEM_UNDO.bits())];
                sys.sys_semop(id, (one.as_ptr() as usize).into(), 1)
                    .await
                    .unwrap();
                assert_eq!(sys.sys_unshare(CloneFlags::SYSVSEM.bits()), Ok(0));
                assert_eq!(getval(sys, id), 0, "the new list was not given back");
                // Without the flag nothing goes.
                sys.sys_semop(id, (one.as_ptr() as usize).into(), 1)
                    .await
                    .unwrap();
                assert_eq!(sys.sys_unshare(CloneFlags::FS.bits()), Ok(0));
                assert_eq!(getval(sys, id), 1, "CLONE_FS gave back the SEM_UNDO");
            })
        });
    }

    /// A `vfork` child runs in its parent's memory until its `execve`.
    #[test]
    fn a_vfork_child_shares_its_memory() {
        let parent = a_process(44_424);
        let child = Process::vfork_from(&parent).unwrap();
        on_a_thread(&child, move |sys| {
            Box::pin(async move {
                assert_eq!(sys.sys_unshare(CloneFlags::VM.bits()), Err(LxError::EINVAL));
                assert_eq!(sys.sys_unshare(CloneFlags::FS.bits()), Ok(0));
            })
        });
    }

    /// The facts the plan is given are the process's own: a second thread
    /// makes `CLONE_FS` an `EINVAL`.
    #[test]
    fn a_second_thread_is_seen() {
        let proc = a_process(44_423);
        let other = Thread::create_linux(&proc).unwrap();
        on_a_thread(&proc, move |sys| {
            Box::pin(async move {
                assert_eq!(sys.sys_unshare(CloneFlags::FS.bits()), Err(LxError::EINVAL));
                assert_eq!(sys.sys_unshare(0), Ok(0));
            })
        });
        drop(other);
    }
}
