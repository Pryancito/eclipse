//! What a `fork(2)` child starts life with. Every one of these is a
//! one-line decision that is invisible at the call site and wrong in only
//! one direction: a field that quietly falls back to its default gives
//! the child a fresh value where Linux gives it the parent's, and nothing
//! fails -- the child just behaves as if the parent had never configured
//! anything. Four were doing exactly that.

use super::*;

/// A parent with every field set to something that is *not* its default,
/// so a field the fork forgets shows up as the default and a field it
/// copies shows up as this.
fn a_configured_parent() -> LinuxProcessInner {
    let mut p = LinuxProcessInner {
        execute_path: String::from("/usr/bin/labwc"),
        cmdline: alloc::vec![String::from("labwc"), String::from("-s")],
        environ: alloc::vec![String::from("WAYLAND_DISPLAY=wayland-0")],
        current_working_directory: String::from("/home/moebius"),
        limits: {
            let mut limits = RLimits::default();
            limits
                .set(
                    RLIMIT_NOFILE,
                    RLimit {
                        cur: 65536,
                        max: 65536,
                    },
                )
                .unwrap();
            limits
        },
        brk: 0x5555_0010_0000,
        mapped_brk: 0x5555_0020_0000,
        pgid: 41,
        sid: 42,
        no_new_privs: true,
        dumpable: Some(0),
        personality: 0x0004_0000,
        thp_disable: true,
        keep_caps: true,
        children_utime_ns: 111,
        children_stime_ns: 222,
        pdeathsig: 15,
        child_subreaper: true,
        job_stopped: true,
        job_stop_sig: 19,
        job_stop_pending: true,
        job_continued_pending: true,
        has_execed: true,
        sugid: true,
        ..Default::default()
    };
    p.cloexec_fds.insert(7.into());
    p.files.insert(
        7.into(),
        crate::fs::Inotify::new(crate::fs::OpenFlags::empty()) as Arc<dyn FileLike>,
    );
    p
}

fn fork_of(parent: &LinuxProcessInner) -> LinuxProcessInner {
    parent.forked_child(41, 42, 999)
}

#[test]
fn a_child_is_born_now_not_when_its_parent_was() {
    // `copy_process`: `p->start_time = ktime_get_ns()`, never the
    // parent's. `/proc/<pid>/stat` field 22 comes from this.
    let mut parent = a_configured_parent();
    parent.start_ns = 5;
    assert_eq!(parent.forked_child(41, 42, 999).start_ns, 999);
}

#[test]
fn every_fork_counts_once_in_the_processes_created_since_boot() {
    // `total_forks` in `copy_process`: a counter, never the live count.
    // Other tests create processes too, so the count can grow by more
    // than ours, never by less and never shrink.
    let parent = a_configured_parent();
    let before = processes_created();
    let _first = parent.forked_child(41, 42, 1);
    let _second = parent.forked_child(41, 42, 2);
    assert!(processes_created() >= before + 2);
    // A dropped child stays counted.
    drop(_first);
    assert!(processes_created() >= before + 2);
}

#[test]
fn the_credential_taint_survives_the_fork() {
    // `p2->p_flag |= p1->p_flag & P_SUGID` (kern_fork.c). The child is a
    // copy of an address space that a more privileged program filled in,
    // so it inherits the doubt along with the memory -- and a child that
    // forgot it would answer `issetugid()` with 0 while holding exactly
    // the data segment the flag exists to warn about.
    let child = fork_of(&a_configured_parent());
    assert!(child.sugid);
}

#[test]
fn the_file_descriptor_limit_survives_the_fork() {
    // `ulimit -n 65536` only ever reaches a program through a fork: the
    // shell raises its own limit and then forks. Resetting it here undid
    // every raise in the system, silently, and the program hit EMFILE at
    // the default -- the failure a raised limit exists to prevent.
    let child = fork_of(&a_configured_parent());
    assert_eq!(child.limits.get(RLIMIT_NOFILE).unwrap().cur, 65536);
    assert_eq!(child.limits.get(RLIMIT_NOFILE).unwrap().max, 65536);
}

#[test]
fn the_heap_bookkeeping_survives_the_fork() {
    // `fork` copies the address space, so the heap is in the child. The
    // numbers that say where it ends were starting from zero, and
    // `sys_brk` returns the old break unchanged for anything below the
    // heap base -- so in a forked child `brk` could not move at all and
    // `sbrk(0)` answered 0. A child that never execs (a subshell, a
    // zygote) had its allocator pushed onto mmap for good.
    let child = fork_of(&a_configured_parent());
    assert_eq!(child.brk, 0x5555_0010_0000);
    assert_eq!(child.mapped_brk, 0x5555_0020_0000);
}

#[test]
fn the_environment_survives_the_fork() {
    // `/proc/<pid>/environ` reads the process's own memory in Linux, and
    // a fork copies that memory. A child that has not exec'd reported an
    // empty environment.
    let child = fork_of(&a_configured_parent());
    assert_eq!(
        child.environ,
        alloc::vec![String::from("WAYLAND_DISPLAY=wayland-0")]
    );
}

#[test]
fn the_working_directory_and_command_line_survive_the_fork() {
    let child = fork_of(&a_configured_parent());
    assert_eq!(child.current_working_directory, "/home/moebius");
    assert_eq!(child.execute_path, "/usr/bin/labwc");
    assert_eq!(child.cmdline.len(), 2);
}

#[test]
fn the_close_on_exec_set_is_copied_and_not_shared() {
    // POSIX: the child gets its own copy of each fd's FD_CLOEXEC flag, so
    // a later `fcntl(F_SETFD)` in either process must not reach the other.
    let parent = a_configured_parent();
    let mut child = fork_of(&parent);
    assert!(child.cloexec_fds.contains(&7.into()));
    child.cloexec_fds.remove(&7.into());
    assert!(
        parent.cloexec_fds.contains(&7.into()),
        "the child's copy must be its own"
    );
}

#[test]
fn the_prctl_settings_that_linux_inherits_do() {
    // A `no_new_privs` that did not survive fork would hand a child back
    // the setuid behaviour its parent gave up -- the one thing the flag
    // exists to make irreversible.
    let child = fork_of(&a_configured_parent());
    assert!(child.no_new_privs);
    assert_eq!(child.dumpable, Some(0));
    assert_eq!(child.personality, 0x0004_0000);
    assert!(child.thp_disable);
    assert!(child.keep_caps, "SECBIT_KEEP_CAPS is inherited");
}

#[test]
fn the_process_group_and_session_are_the_resolved_ones_passed_in() {
    // Not the parent's raw fields: an unset (0) pgid means "the parent's
    // own pid", and the child needs the concrete value or a Ctrl-C never
    // reaches it.
    let mut parent = a_configured_parent();
    parent.pgid = 0;
    parent.sid = 0;
    let child = parent.forked_child(1234, 5678, 0);
    assert_eq!(child.pgid, 1234);
    assert_eq!(child.sid, 5678);
}

#[test]
fn the_open_files_survive_the_fork() {
    // The one thing everybody knows a fork does. It is here so the
    // exhaustive list above cannot lose it while nobody is looking.
    let child = fork_of(&a_configured_parent());
    assert!(child.files.contains_key(&7.into()));
}

#[test]
fn a_child_starts_with_no_children_of_its_own() {
    let mut parent = a_configured_parent();
    parent.reaped_children.insert(99, (0, Default::default()));
    let child = fork_of(&parent);
    assert!(child.children.is_empty());
    assert!(
        child.reaped_children.is_empty(),
        "a newborn child has reaped nobody"
    );
    // `copy_process` zeroes `cutime`/`cstime`: a child must not be born
    // already credited with the CPU time of its parent's other children,
    // or `times(2)` double-counts it up the whole tree.
    assert_eq!(child.children_utime_ns, 0);
    assert_eq!(child.children_stime_ns, 0);
}

/// `copy_process`: `p->flags |= PF_FORKNOEXEC`. The parent has exec'd --
/// every process has -- but the child has not, and that flag is what
/// gives the shell its window to put the child in a job's process group
/// (see [`setpgid_verdict`]). Inherited, the window would never open and
/// every `setpgid(child, ...)` would answer EACCES.
#[test]
fn a_child_has_not_execed_however_long_its_parent_has_been_running() {
    let parent = a_configured_parent();
    assert!(parent.has_execed, "the fixture must have exec'd");
    assert!(!fork_of(&parent).has_execed);
}

#[test]
fn a_child_is_not_born_stopped_or_owing_a_notification() {
    // `job_stopped` carried over would leave the child parked before its
    // first instruction, waiting for a SIGCONT nobody will send it; the
    // pending flags carried over would make its first `waitpid` report a
    // stop that happened to its parent.
    let child = fork_of(&a_configured_parent());
    assert!(!child.job_stopped);
    assert_eq!(child.job_stop_sig, 0);
    assert!(!child.job_stop_pending);
    assert!(!child.job_continued_pending);
}

#[test]
fn the_parents_own_roles_are_not_handed_down() {
    // `p->pdeath_signal = 0` in `copy_process`: the signal is "tell me
    // when MY parent dies", so inheriting it would have the child killed
    // when its grandparent exits. The subreaper attribute is likewise the
    // parent's role, not something a child is born holding.
    let child = fork_of(&a_configured_parent());
    assert_eq!(child.pdeathsig, 0);
    assert!(!child.child_subreaper);
}

#[test]
fn the_semaphore_undo_state_is_not_inherited() {
    // A plain `fork` does NOT share SEM_UNDO state -- only
    // `CLONE_SYSVSEM` does. Copying it would have the child undo, on its
    // own exit, semaphore operations that its parent performed and that
    // the parent will undo again.
    let mut parent = a_configured_parent();
    let id = 7;
    parent.semaphores.add(
        id,
        crate::ipc::SemArray::get_or_create(0, 1, 0o666, 0, 0, &[]).unwrap(),
    );
    parent.semaphores.add_undo(id, 0, -1);

    let child = fork_of(&parent);
    assert!(
        child.semaphores.owes_no_undo(),
        "a forked child owes no semaphore undo"
    );
    // ...but it keeps the sets the parent had open. The ids are
    // per-process indices, so dropping the table left an id the parent
    // passed down naming nothing in the child.
    assert!(
        child.semaphores.get(id).is_some(),
        "the child must still find the set its parent had open"
    );
}

#[test]
fn the_shared_memory_attachments_survive_the_fork() {
    // `fork` copies the address space, so the segments the parent had
    // attached are mapped in the child too -- it is holding them whether
    // the kernel remembers or not. Without the record the child cannot
    // `shmdt` them, so the mapping stays for its whole life, and the
    // segment's use count is wrong. This is the same bookkeeping whose
    // loss on `IPC_RMID` leaked an address range per X11 frame.
    use crate::ipc::ShmGuard;
    use zircon_object::vm::VmObject;
    let mut parent = a_configured_parent();
    let guard = Arc::new(kernel_hal::sync::Mutex::new(ShmGuard {
        shared_guard: VmObject::new_paged(1),
        shmid_ds: kernel_hal::sync::Mutex::new(Default::default()),
    }));
    parent.shm_identifiers.add(9, guard);
    let mut ident = parent.shm_identifiers.get(9).unwrap();
    ident.addr = 0x7f00_0000;
    parent.shm_identifiers.set(9, ident);

    let child = fork_of(&parent);
    assert_eq!(
        child.shm_identifiers.get_id(0x7f00_0000),
        Some(9),
        "the child must be able to find the segment it inherited"
    );
}

#[test]
fn the_fork_counts_the_child_as_one_more_attachment() {
    // `shm_open` on the copied mapping: the segment's `shm_nattch` goes
    // up by one for the child, as `ipcs -m` shows on Linux, and comes
    // back down when the child dies. It used to move only on an explicit
    // `shmat`/`shmdt`, so a forked child was invisible to the count and
    // a parent that died with the segment attached left it one too high
    // for ever.
    use crate::ipc::ShmGuard;
    use zircon_object::vm::VmObject;
    let mut parent = a_configured_parent();
    let guard = Arc::new(kernel_hal::sync::Mutex::new(ShmGuard {
        shared_guard: VmObject::new_paged(1),
        shmid_ds: kernel_hal::sync::Mutex::new(Default::default()),
    }));
    guard.lock().attach(1);
    parent.shm_identifiers.attach(9, guard.clone(), 0x7f00_0000);
    let nattch = || guard.lock().shmid_ds.lock().nattch;
    assert_eq!(nattch(), 1);

    let child = fork_of(&parent);
    assert_eq!(nattch(), 2, "the child holds the mapping too");
    drop(child);
    assert_eq!(nattch(), 1, "the child's death is a detach");
    drop(parent);
    assert_eq!(nattch(), 0, "and so is the parent's");
}

#[test]
fn the_kernel_side_futex_objects_are_not_inherited() {
    // They are keyed by address in the parent's address space and hold
    // its waiters. The child's memory is a copy: same addresses,
    // different pages, and nobody waiting. Handing the child the
    // parent's objects would have a `futex_wake` in the child reach
    // threads of the parent that are waiting on their own memory.
    //
    // The table hangs off the PROCESS now rather than the inner state a
    // fork clones (see `LinuxProcess::futexes`), so there is no
    // inheritance step left to get wrong. What is left to pin is the
    // consequence: `fork` builds a fresh `LinuxProcess`, and a fresh one
    // starts with an empty table however full the process it came from is.
    static WORD: AtomicI32 = AtomicI32::new(0);
    let parent = super::dup_fd_tests::a_process();
    parent
        .futexes
        .lock()
        .get_or_create_dropping_swept(0x1000, || Futex::new(&WORD));
    assert_eq!(parent.futexes.lock().len(), 1);

    let child = super::dup_fd_tests::a_process();
    assert!(child.futexes.lock().is_empty());
}

#[test]
fn a_futex_lookup_and_the_big_process_lock_do_not_wait_for_each_other() {
    // The point of giving the table its own lock. On the machine both are
    // IRQ-off spin locks, so a holder of one really does burn the other
    // CPU's cycles: a thread handing off a condition variable used to
    // queue behind an unrelated descriptor lookup, or behind a `fork`
    // cloning the whole file table under that same lock.
    static WORD: AtomicI32 = AtomicI32::new(0);
    let proc = super::dup_fd_tests::a_process();

    let futexes = proc.futexes.lock();
    assert!(
        proc.inner.try_lock().is_some(),
        "a futex lookup must not hold the big process lock"
    );
    drop(futexes);

    let inner = proc.inner.lock();
    assert!(
        proc.futexes.try_lock().is_some(),
        "a futex lookup must not wait for the big process lock"
    );
    // And it really does complete with that lock held, rather than merely
    // finding the table's own lock free.
    proc.futexes
        .lock()
        .get_or_create_dropping_swept(0x2000, || Futex::new(&WORD));
    assert_eq!(proc.futexes.lock().len(), 1);
    drop(inner);
}
