//! The one thread a forked child starts with --- and giving the child back
//! when it cannot be given one.
//!
//! Out of `task.rs` as its own module because of what it now holds: the undo.
//! A `clone` that answers an error made NO child, and this is the only place
//! in this kernel where a child exists before the call can still fail.
//!
//! The thread path says so in its own words, beside its
//! `terminate_abandoned`: *"the fork path above asks about the pidfd's word
//! before it forks, so it has no child to take back"*. That was true of the
//! pidfd and false of `CLONE_SETTLS`, whose `struct user_desc` is read in
//! `set_child_thread_pointer`, **after** `fork_from` has filed the child in
//! its parent's table and stamped it `Running`. The child then had no thread
//! and never would, so it never exited, never became a zombie and never left
//! the table: a live child, forever.
//!
//! What that costs the parent is not a leak but a wrong answer. A `wait`
//! asks `wait_child_any_interest`, which answers `ECHILD` only when NEITHER
//! the live table nor the zombies hold a candidate; with the phantom in the
//! live table it answers `EAGAIN` instead, which `wait4`/`waitid` turn into
//! **0**. So a 32-bit process that had reaped its only child was told, for
//! the rest of its life, "you have children, none of them ready" --- and a
//! loop written the way every shell writes it never ends. Found by the i386
//! probe in Eclipse: its step 85 expects `ECHILD` and got 0, in the image
//! that had just run step 88's deliberately-refused `clone`.

use super::*;

impl Syscall<'_> {
    /// The one thread a forked (or vforked) child starts with: this thread's
    /// context with the stack, TLS and return value the caller asked for,
    /// and the TID bookkeeping of `ctid` done BEFORE it runs a single user
    /// instruction. Returns the child's thread id.
    ///
    /// The child's address space already exists (a copy from `fork_from`,
    /// or this one from `vfork_from`), so `CLONE_CHILD_SETTID` cannot go
    /// through this thread's `UserOutPtr`: after a `fork` that writes the
    /// parent's memory, which the child does not see. It is written into the
    /// child's VMAR instead; see [`publish_child_tid`].
    pub(super) fn start_forked_thread(
        &self,
        new_proc: &Arc<Process>,
        newsp: usize,
        newtls: usize,
        ctid: ChildTidRequest,
        child_of: ChildOf,
        words: ParentWords,
    ) -> LxResult<KoID> {
        // What the child thread takes from this one -- the signal mask above
        // all, which a `fork` inherits (sigprocmask(2)). `false`: a fork does
        // not share the address space, so the alternate signal stack comes
        // across too. (For a vfork, Linux's rule is `(clone_flags &
        // (CLONE_VM|CLONE_VFORK)) == CLONE_VM`, and a vfork sets BOTH bits,
        // so the stack comes across there just the same.)
        // Outside the block below, because the answer is asked for after it:
        // the fd is handed out before `start` and its failure reported after.
        let mut pidfd: LxResult<Option<linux_object::fs::FileDesc>> = Ok(None);
        let inherited = self.thread.lock_linux().forked_child();
        let new_thread = Thread::create_linux_with(new_proc, inherited)
            .inspect_err(|_| abandon_unstarted_child(new_proc))?;
        // Everything from here to `start` can fail, and the child process is
        // ALREADY filed in its parent's child table: `fork_from` put it there
        // before this function was called. So the whole fallible tail is one
        // block with ONE error path, which takes back the thread that will
        // never run and the child that was never born -- the same shape as
        // the thread path's `clone_start`, and for the same reason.
        let up_to_start = (|| -> LxResult<()> {
            new_proc.linux().set_forking_thread(self.thread.id());
            let mut new_ctx = self.thread.context_cloned()?;
            if newsp != 0 {
                new_ctx.set_field(UserContextField::StackPointer, newsp);
            }
            if newtls != 0 {
                self.set_child_thread_pointer(&mut new_ctx, newtls)?;
            }
            new_ctx.set_field(UserContextField::ReturnValue, 0);
            // A FreeBSD child returns 0 in %rax, 1 in %rdx and a clear carry flag
            // (cpu_fork, sys/amd64/amd64/vm_machdep.c). libc's fork() stub branches
            // on carry, so a stale CF inherited from the parent's context would make
            // the child believe fork() failed. Linux needs only %rax = 0.
            #[cfg(target_arch = "x86_64")]
            if self.linux_process().abi() == linux_object::process::Abi::Freebsd {
                let g = new_ctx.general_mut();
                g.rdx = 1;
                g.rflags &= !1;
            }
            new_thread.with_context(|ctx| *ctx = new_ctx)?;
            // A child forked inside a signal handler has the handler's frame in
            // its copy of the stack, and returns from it.
            new_thread.inherit_backup_contexts(self.thread);
            new_thread.inherit_sched(self.thread);
            publish_child_tid(&new_thread, ctid);
            // Everything below this line and above `start` is "what the child has
            // to be holding before it can execute an instruction", and it is in
            // the same function as the `start` on purpose: each of these used to
            // be done somewhere the child was already running by the time it
            // happened.
            match child_of {
                ChildOf::Caller(signal) => new_proc.linux().set_exit_signal(signal),
                ChildOf::CallersParent => {
                    linux_object::process::give_to_callers_parent(self.zircon_process(), new_proc)
                }
            }
            // hunter: inherit the parent's syscall whitelist into the child so a
            // process cannot shed its policy merely by forking, and seed a fresh
            // anomaly window for the new pid.
            //
            // BEFORE `start`, and that is the whole point: a pid with no
            // whitelist registered is permissive (`is_syscall_allowed` answers
            // `Ok` on a map miss), so every instant the child is runnable without
            // its inherited policy is an instant in which it may make a syscall
            // the policy forbids. Started first, that window is real on any
            // machine with a second CPU to run the child on -- and on one CPU
            // too, if the parent is preempted here. Linux installs the child's
            // credentials and seccomp filters in `copy_process`, before
            // `wake_up_new_task`, for the same reason.
            hunter::task_fork(self.zircon_process().id(), new_proc.id());
            // The caller's words, last before `start` (see `ParentWords`). The
            // pidfd's can still fail if another thread took the page's write
            // permission away since `check`; the child then runs anyway, as
            // every child did when its pidfd was handed out after it started.
            pidfd = words
                .pidfd
                .map(|slot| {
                    self.hand_out_pidfd(PidFd::new(new_proc.clone(), PIDFD_OPEN_FLAGS), slot)
                })
                .transpose();
            if let Some(mut slot) = words.parent_tid {
                let _ = slot.write_if_not_null(new_thread.id() as i32);
            }
            new_thread.start(self.thread_fn).inspect_err(|_| {
                if let Ok(Some(fd)) = pidfd {
                    let _ = self.linux_process().close_file(fd);
                }
            })?;
            Ok(())
        })();
        if let Err(err) = up_to_start {
            // The thread first: `Thread::kill` on a thread that never ran
            // finds no coroutine, so it never leaves the process's thread
            // list and `Process::exit` -- which only tears down when the list
            // is empty -- would never release the child's address space. See
            // the long comment on the thread path's own `terminate_abandoned`.
            new_thread.terminate_abandoned();
            abandon_unstarted_child(new_proc);
            return Err(err);
        }
        // PAST `start`: the child is running. A pidfd word that cannot be
        // written is reported, as it was before, and the child is NOT taken
        // back -- Linux never reaches here (it writes the word before it
        // wakes the child), and killing something that is already executing
        // user instructions is a different change from this one.
        pidfd?;
        Ok(new_thread.id())
    }
}

/// Take back a child that was filed in its parent's table and then could not
/// be finished, before it ran one instruction.
///
/// Linux's `bad_fork_*` unwind, in the order this kernel needs it: the child
/// leaves its parent's table FIRST (with the mark that stops its termination
/// filing a zombie, see [`LinuxProcess::abandon_child`]), and only then is it
/// exited --- which is what releases its address space and takes it out of
/// its job.
///
/// The parent is looked up on the child and not passed in, because it is not
/// always the caller: `CLONE_PARENT` hands the child to the caller's own
/// parent (`give_to_callers_parent`), and it is that table the child has to
/// leave.
///
/// The exit code is nobody's to read: no zombie is kept, so no `wait` will
/// ever see it.
fn abandon_unstarted_child(child: &Arc<Process>) {
    if let Some(parent) = child.linux().parent() {
        parent.linux().abandon_child(child.id());
    }
    child.exit(0);
}
