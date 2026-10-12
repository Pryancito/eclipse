//! What a `wait*` is allowed to collect, and what a dying child leaves
//! behind for it to collect.
//!
//! These are the two halves of one decision and they are easy to get subtly
//! wrong in ways nothing notices for a while. A parent that leaves a zombie
//! it will never wait for leaks a pid for the life of the process; a parent
//! that auto-reaps when it should not loses the exit status of a child it was
//! about to wait for, and the `wait` comes back `ECHILD`. Both show up as
//! "the build hangs sometimes" rather than as anything that points here.
//!
//! The rules are `do_notify_parent` (kernel/signal.c) and `wait_task_zombie`
//! (kernel/exit.c). Four functions implement them here and none had a test:
//! [`child_death_policy`], [`parent_wants_sigchld_for_stops`],
//! [`LinuxProcess::take_wait_notification`] and
//! [`scan_waitable_children`] with [`claim_exited_child`].
//!
//! Fixed pids live in 93_5xx here; they are a global table, so every file
//! that pins one has its own range.

use super::*;
use crate::signal::{SignalAction, SignalActionFlags, SIG_DFL, SIG_IGN};
use crate::thread::ThreadExt;
use rcore_fs_ramfs::RamFS;

fn a_parent(pid: KoID) -> Arc<Process> {
    let proc = Process::create_with_fixed_id_ext(
        &ROOT_JOB,
        pid,
        "parent",
        LinuxProcess::new(RamFS::new(), 0),
    )
    .unwrap();
    Thread::create_linux(&proc).unwrap();
    proc
}

/// Set the parent's `SIGCHLD` disposition, which is the whole input to the
/// death policy.
fn sigchld_action(proc: &Arc<Process>, handler: usize, flags: SignalActionFlags) {
    proc.linux().set_signal_action(
        LinuxSignal::SIGCHLD,
        SignalAction {
            handler,
            flags,
            restorer: 0,
            mask: Default::default(),
        },
    );
}

const SIGCHLD: u8 = LinuxSignal::SIGCHLD as u8;

/// What a dying child leaves behind: the autoreap decision.
mod the_death_policy {
    use super::*;

    /// The ordinary program: `SIGCHLD` left alone. The zombie stays for
    /// `wait*` to collect and the signal is sent.
    #[test]
    fn a_default_sigchld_leaves_the_zombie_and_sends_the_signal() {
        let parent = a_parent(93_501);
        assert_eq!(
            child_death_policy(parent.linux(), SIGCHLD),
            ChildDeathPolicy {
                autoreap: false,
                notify: true,
            }
        );
    }

    /// A handler installed is still "I will wait for them": sigaction(2) only
    /// changes the zombie when `SA_NOCLDWAIT` says so.
    #[test]
    fn a_handler_on_its_own_changes_nothing_about_the_zombie() {
        let parent = a_parent(93_502);
        sigchld_action(&parent, 0x4000, SignalActionFlags::empty());
        assert_eq!(
            child_death_policy(parent.linux(), SIGCHLD),
            ChildDeathPolicy {
                autoreap: false,
                notify: true,
            }
        );
    }

    /// `signal(SIGCHLD, SIG_IGN)` is the explicit "I will never wait for
    /// them": the child is released at once and NO signal is sent. A daemon
    /// that forks workers and never waits does exactly this, and leaving the
    /// zombie would leak a pid per worker for the life of the daemon.
    #[test]
    fn sig_ign_releases_the_child_and_says_nothing() {
        let parent = a_parent(93_503);
        sigchld_action(&parent, SIG_IGN, SignalActionFlags::empty());
        assert_eq!(
            child_death_policy(parent.linux(), SIGCHLD),
            ChildDeathPolicy {
                autoreap: true,
                notify: false,
            }
        );
    }

    /// `SA_NOCLDWAIT` with a handler is the half-way house, and the one worth
    /// a test of its own because the two halves point different ways: no
    /// zombie, but the handler still runs. A program that wants to log each
    /// child's death without ever calling `wait` asks for this.
    #[test]
    fn nocldwait_releases_the_child_but_still_runs_the_handler() {
        let parent = a_parent(93_504);
        sigchld_action(&parent, 0x4000, SignalActionFlags::NOCLDWAIT);
        assert_eq!(
            child_death_policy(parent.linux(), SIGCHLD),
            ChildDeathPolicy {
                autoreap: true,
                notify: true,
            }
        );
    }

    /// And the rule that bounds all three: they apply only when the child's
    /// exit signal IS `SIGCHLD`. A `clone` with a custom exit signal must not
    /// have its zombie swallowed by the parent's `SIGCHLD` disposition --
    /// which is what a thread library's pid-per-thread bookkeeping depends
    /// on, and what a `SIG_IGN` on `SIGCHLD` would otherwise destroy.
    #[test]
    fn a_custom_exit_signal_is_not_governed_by_the_sigchld_disposition() {
        let parent = a_parent(93_505);
        sigchld_action(&parent, SIG_IGN, SignalActionFlags::NOCLDWAIT);
        assert_eq!(
            child_death_policy(parent.linux(), LinuxSignal::SIGUSR1 as u8),
            ChildDeathPolicy {
                autoreap: false,
                notify: true,
            },
            "SIG_IGN and SA_NOCLDWAIT on SIGCHLD say nothing about a SIGUSR1 exit signal"
        );
    }

    /// `clone3` with `exit_signal = 0` wants no signal at all, and still a
    /// zombie: the pid is the parent's to collect.
    #[test]
    fn an_exit_signal_of_zero_leaves_the_zombie_and_sends_nothing() {
        let parent = a_parent(93_506);
        assert_eq!(
            child_death_policy(parent.linux(), 0),
            ChildDeathPolicy {
                autoreap: false,
                notify: false,
            }
        );
    }

    /// `SIG_DFL` is not `SIG_IGN`, however much the default action of
    /// `SIGCHLD` is to ignore it. The distinction is the whole of
    /// `do_notify_parent`'s test, and collapsing the two would auto-reap
    /// every child of every ordinary program.
    #[test]
    fn sig_dfl_is_not_sig_ign() {
        let parent = a_parent(93_507);
        sigchld_action(&parent, SIG_DFL, SignalActionFlags::empty());
        assert!(!child_death_policy(parent.linux(), SIGCHLD).autoreap);
    }
}

/// Whether a child STOPPING is worth a signal, which is a different flag.
mod stops {
    use super::*;

    /// By default a stop is reported.
    #[test]
    fn a_parent_hears_about_a_stop_by_default() {
        let parent = a_parent(93_511);
        assert!(parent_wants_sigchld_for_stops(&parent));
    }

    /// `SA_NOCLDSTOP` is the shell's flag: it drives job control through
    /// `wait(WUNTRACED)` and does not want a signal for every `^Z`.
    #[test]
    fn nocldstop_silences_the_stops_and_nothing_else() {
        let parent = a_parent(93_512);
        sigchld_action(&parent, 0x4000, SignalActionFlags::NOCLDSTOP);
        assert!(!parent_wants_sigchld_for_stops(&parent));
        assert!(
            child_death_policy(parent.linux(), SIGCHLD).notify,
            "NOCLDSTOP is about stops; a DEATH is still reported"
        );
    }
}

/// What `wait*` collects, and what it leaves pending.
mod what_a_wait_collects {
    use super::*;

    /// `WUNTRACED` and nothing else: a stop is collected and the pending
    /// flag is cleared, so a second `wait` does not report the same stop.
    #[test]
    fn a_stop_is_collected_once() {
        let proc = a_parent(93_521);
        let interest = WaitInterest {
            exited: true,
            stopped: true,
            continued: false,
        };
        proc.linux().job_stop(&proc, LinuxSignal::SIGTSTP as u8);
        assert_eq!(
            proc.linux().take_wait_notification(interest, true),
            Some(wait_status_stopped(LinuxSignal::SIGTSTP as u8))
        );
        assert_eq!(
            proc.linux().take_wait_notification(interest, true),
            None,
            "the stop was already collected"
        );
    }

    /// `WNOWAIT` (`reap = false`) peeks: the same stop is still there for the
    /// next caller. `waitid(WNOWAIT)` exists so a program can look at a
    /// child's state and leave it collectable.
    #[test]
    fn a_peek_leaves_the_stop_for_the_next_wait() {
        let proc = a_parent(93_522);
        let interest = WaitInterest {
            exited: true,
            stopped: true,
            continued: false,
        };
        proc.linux().job_stop(&proc, LinuxSignal::SIGSTOP as u8);
        let peeked = proc.linux().take_wait_notification(interest, false);
        assert_eq!(
            peeked,
            Some(wait_status_stopped(LinuxSignal::SIGSTOP as u8))
        );
        assert_eq!(
            proc.linux().take_wait_notification(interest, true),
            peeked,
            "WNOWAIT must not consume what it reported"
        );
    }

    /// A classic `wait4` has no `WUNTRACED`, so a stopped child is not
    /// something it may report -- and the stop stays pending for whoever
    /// does ask. A `wait` that returned a stop to a program that did not ask
    /// for one reports a status word the program has no branch for.
    #[test]
    fn a_classic_wait_does_not_see_a_stop_and_does_not_eat_it() {
        let proc = a_parent(93_523);
        proc.linux().job_stop(&proc, LinuxSignal::SIGTSTP as u8);
        assert_eq!(
            proc.linux()
                .take_wait_notification(WaitInterest::EXITED_ONLY, true),
            None
        );
        let with_untraced = WaitInterest {
            exited: true,
            stopped: true,
            continued: false,
        };
        assert!(
            proc.linux()
                .take_wait_notification(with_untraced, true)
                .is_some(),
            "the stop was still there for the wait that asked for it"
        );
    }

    /// `WCONTINUED`, the same way round.
    #[test]
    fn a_continue_is_collected_only_by_the_wait_that_asked_for_it() {
        let proc = a_parent(93_524);
        proc.linux().job_stop(&proc, LinuxSignal::SIGSTOP as u8);
        proc.linux().job_continue(&proc);
        let continued = WaitInterest {
            exited: true,
            stopped: false,
            continued: true,
        };
        assert_eq!(
            proc.linux()
                .take_wait_notification(WaitInterest::EXITED_ONLY, true),
            None
        );
        assert_eq!(
            proc.linux().take_wait_notification(continued, true),
            Some(WAIT_STATUS_CONTINUED)
        );
        assert_eq!(proc.linux().take_wait_notification(continued, true), None);
    }

    /// A child stopped and then resumed before anybody waited: the resume
    /// SUPERSEDES the uncollected stop, and a `wait` that asked for both
    /// reports the continue and nothing else.
    ///
    /// Which is Linux, and not obviously so -- the stop happened first, and a
    /// queue would report it first. But `wait` reports a child's STATE, not a
    /// log of its transitions: `task_stopped_code` looks at whether the task
    /// is stopped NOW, and a resumed one is not. A shell that printed
    /// "Stopped" for a job already running again would be reporting
    /// something that stopped being true before it was asked.
    #[test]
    fn a_resume_supersedes_a_stop_that_nobody_collected() {
        let proc = a_parent(93_525);
        proc.linux().job_stop(&proc, LinuxSignal::SIGTSTP as u8);
        proc.linux().job_continue(&proc);
        let both = WaitInterest {
            exited: true,
            stopped: true,
            continued: true,
        };
        assert_eq!(
            proc.linux().take_wait_notification(both, true),
            Some(WAIT_STATUS_CONTINUED),
            "the child is running, so the stop is not its state any more"
        );
        assert_eq!(proc.linux().take_wait_notification(both, true), None);
    }
}

/// Which child a `wait*` picks, over the whole set of them.
mod which_child {
    use super::*;

    /// A child that reports `Status::Exited` and is STILL in the parent's
    /// live table: the window [`claim_exited_child`] exists for.
    ///
    /// It has to be built by hand, because `exit` runs the death notify
    /// straight away here and that takes the child out of `children` -- which
    /// is exactly why the window is narrow, and why two waiters racing inside
    /// it is the kind of thing no ordinary test reaches. Putting the child
    /// back is modelling the moment before `PROCESS_TERMINATED` ran, not
    /// working around the code.
    fn exited_but_not_yet_recorded(parent: &Arc<Process>, code: i64) -> Arc<Process> {
        let child = Process::fork_from(parent).unwrap();
        child.exit(code);
        // `forget_child` clears the zombie record the notify just wrote, and
        // the insert puts the live child back where it was a moment before.
        parent.linux().forget_child(child.id());
        parent
            .linux()
            .inner
            .lock()
            .children
            .insert(child.id(), child.clone());
        child
    }

    /// Nothing to collect: no child has done anything.
    #[test]
    fn a_parent_whose_children_are_all_running_collects_nothing() {
        let parent = a_parent(93_531);
        let _a = Process::fork_from(&parent).unwrap();
        let _b = Process::fork_from(&parent).unwrap();
        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, None).is_none(),
            "neither child has exited or stopped"
        );
    }

    /// A child that exited is found, and reaping it means the second scan
    /// does not find it again.
    #[test]
    fn an_exited_child_is_collected_once() {
        let parent = a_parent(93_532);
        let child = Process::fork_from(&parent).unwrap();
        let pid = child.id();
        child.exit(7);

        let (got, status, _) =
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, None)
                .expect("the child exited")
                .expect("and it is collectable");
        assert_eq!(got, pid);
        assert_eq!(status, wait_status_exited(7));
        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, None).is_none(),
            "it was already collected"
        );
    }

    /// `WNOWAIT` over the whole set: the exited child is reported and left
    /// where it was.
    #[test]
    fn a_peeking_scan_leaves_the_child_collectable() {
        let parent = a_parent(93_533);
        let child = Process::fork_from(&parent).unwrap();
        let pid = child.id();
        child.exit(3);

        for _ in 0..2 {
            let (got, _, _) =
                scan_waitable_children(&parent, false, WaitInterest::EXITED_ONLY, None)
                    .expect("the child exited")
                    .expect("and it is collectable");
            assert_eq!(got, pid, "a peek must not consume the child");
        }
    }

    /// `waitpid(-pgid)` for a child that had ALREADY EXITED. This is how a
    /// job-control shell waits for a whole pipeline, and it could not work:
    /// recording the exit takes the child out of `children`, and the group
    /// filter only ever looked at live children, so the status was lost and
    /// the wait answered `ECHILD`.
    ///
    /// The zombie now remembers the group it died in, the way a zombie keeps
    /// its pgid in Linux.
    #[test]
    fn a_group_wait_collects_a_child_that_already_exited() {
        let parent = a_parent(93_534);
        let child = Process::fork_from(&parent).unwrap();
        let pid = child.id();
        child.linux().set_pgid_raw(pid); // its own group, as a pipeline leader
        child.exit(1);

        let (got, status, _) =
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(pid))
                .expect("the child of that group exited")
                .expect("and its status is collectable");
        assert_eq!((got, status), (pid, wait_status_exited(1)));
    }

    /// And the filter is still a filter: a dead child of ANOTHER group is not
    /// collected by a wait that asked for this one.
    #[test]
    fn a_group_wait_ignores_a_dead_child_of_another_group() {
        let parent = a_parent(93_540);
        let child = Process::fork_from(&parent).unwrap();
        let pid = child.id();
        child.linux().set_pgid_raw(pid);
        child.exit(1);

        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(93_540 + 7))
                .is_none(),
            "the dead child is in group {} and the wait asked for another",
            pid
        );
        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(pid)).is_some(),
            "and it is still there for the group that owns it"
        );
    }

    /// The group a zombie is matched against is the one it DIED in. `setpgid`
    /// moves a live child; once it is dead the number is fixed, because there
    /// is no longer a process to ask.
    #[test]
    fn a_zombie_is_matched_against_the_group_it_died_in() {
        let parent = a_parent(93_541);
        let child = Process::fork_from(&parent).unwrap();
        let pid = child.id();
        child.linux().set_pgid_raw(pid);
        // Moved again while still alive: the LAST group is the one that counts.
        child.linux().set_pgid_raw(93_541);
        child.exit(0);

        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(pid)).is_none(),
            "it left that group before it died"
        );
        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(93_541))
                .is_some(),
            "and it died in this one"
        );
    }

    /// A stop is found by the scan too, and only by a scan that asked for
    /// one -- the same rule as `take_wait_notification`, now over the set.
    #[test]
    fn a_stopped_child_is_found_only_by_the_scan_that_asked_for_stops() {
        let parent = a_parent(93_535);
        let child = Process::fork_from(&parent).unwrap();
        child.linux().job_stop(&child, LinuxSignal::SIGTSTP as u8);

        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, None).is_none(),
            "a classic wait4 does not report a stop"
        );
        let with_untraced = WaitInterest {
            exited: true,
            stopped: true,
            continued: false,
        };
        let (got, status, _) = scan_waitable_children(&parent, true, with_untraced, None)
            .expect("the child stopped")
            .expect("and the scan asked for stops");
        assert_eq!(got, child.id());
        assert_eq!(status, wait_status_stopped(LinuxSignal::SIGTSTP as u8));
    }

    /// The claim: two waiters can both see `Status::Exited` before either
    /// holds the process lock, and only one may report it. The second
    /// `claim_exited_child` for the same pid has to fail, or the same child
    /// is collected twice and the second `wait` returns a pid that no longer
    /// exists.
    #[test]
    fn only_one_claim_of_a_child_can_succeed() {
        let parent = a_parent(93_536);
        let child = exited_but_not_yet_recorded(&parent, 5);
        let pid = child.id();
        let cpu = child_cpu(&child);

        assert!(
            claim_exited_child(&parent, pid, cpu),
            "the first claim wins"
        );
        assert!(
            !claim_exited_child(&parent, pid, cpu),
            "the second claim must lose: the child is already somebody's"
        );
    }

    /// And the claim's CPU time is folded into `RUSAGE_CHILDREN` once, by
    /// whoever won it. Twice would make a `times(2)` loop report time the
    /// children never spent.
    #[test]
    fn a_lost_claim_does_not_charge_the_childrens_cpu_twice() {
        let parent = a_parent(93_537);
        let child = exited_but_not_yet_recorded(&parent, 0);
        let pid = child.id();
        let cpu = ChildCpu {
            utime_ns: 1_000,
            stime_ns: 2_000,
        };

        let before = parent.linux().children_cpu_ns();
        assert!(claim_exited_child(&parent, pid, cpu));
        let after_one = parent.linux().children_cpu_ns();
        assert_eq!(
            (after_one.0 - before.0, after_one.1 - before.1),
            (1_000, 2_000)
        );
        assert!(!claim_exited_child(&parent, pid, cpu));
        assert_eq!(
            parent.linux().children_cpu_ns(),
            after_one,
            "the losing claim charged the time a second time"
        );
    }

    /// A child already reaped out of the live table (the `SA_NOCLDWAIT` and
    /// death-notify path records it here) is still collectable, and exactly
    /// once. This is the shortcut at the top of the scan, which exists
    /// because the `Arc<Process>` is gone by then and there is nothing left
    /// to ask for a status.
    #[test]
    fn a_recorded_exit_is_collectable_without_the_child() {
        let parent = a_parent(93_538);
        parent.linux().record_child_exit(
            424_242,
            9,
            ChildCpu {
                utime_ns: 7,
                stime_ns: 11,
            },
            93_538,
        );
        let (pid, status, cpu) =
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, None)
                .expect("there is a recorded exit")
                .expect("and it is collectable");
        assert_eq!((pid, status), (424_242, wait_status_exited(9)));
        assert_eq!((cpu.utime_ns, cpu.stime_ns), (7, 11));
        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, None).is_none(),
            "collected once"
        );
    }

    /// The whole syscall, which is the claim that matters: `waitpid(-pgid)`
    /// for a pipeline whose process has already finished. It used to answer
    /// `ECHILD` -- "you have no such children" -- with the child's exit status
    /// sitting in the parent's own table, uncollectable by anybody, and a
    /// shell waiting on that job would be told the job never existed.
    #[test]
    fn a_nonblocking_group_wait_reports_the_dead_child_and_not_echild() {
        extern crate std;
        let parent = a_parent(93_542);
        let child = Process::fork_from(&parent).unwrap();
        let pid = child.id();
        child.linux().set_pgid_raw(pid);
        child.exit(4);

        let got = async_std::task::block_on(wait_child_any_interest(
            &parent,
            true,
            true,
            WaitInterest::EXITED_ONLY,
            Some(pid),
        ));
        assert_eq!(
            got.map(|(p, status, _)| (p, status)),
            Ok((pid, wait_status_exited(4)))
        );
    }

    /// And `ECHILD` is still the answer when there really is no such child:
    /// the group nobody is in.
    #[test]
    fn a_group_wait_for_a_group_with_no_children_is_still_echild() {
        extern crate std;
        let parent = a_parent(93_543);
        let _live = Process::fork_from(&parent).unwrap();
        let got = async_std::task::block_on(wait_child_any_interest(
            &parent,
            true,
            true,
            WaitInterest::EXITED_ONLY,
            Some(93_543 + 99),
        ));
        assert_eq!(got.map(|(p, _, _)| p), Err(LxError::ECHILD));
    }

    /// A recorded exit reaches a group wait too, by the pgid stored with it:
    /// the shortcut used to be skipped altogether for a group wait, because
    /// there was no group to match against and no process left to ask.
    #[test]
    fn a_group_wait_takes_a_recorded_exit_of_its_own_group() {
        let parent = a_parent(93_539);
        parent
            .linux()
            .record_child_exit(424_243, 1, ChildCpu::default(), 93_539);
        assert!(
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(93_539 + 7))
                .is_none(),
            "it is not in the group that was asked for"
        );
        let (pid, _, _) =
            scan_waitable_children(&parent, true, WaitInterest::EXITED_ONLY, Some(93_539))
                .expect("it is in this one")
                .expect("and it is collectable");
        assert_eq!(pid, 424_243);
    }

    /// `waitid(P_PGID, WSTOPPED)` on a group whose only member is a zombie:
    /// there is nothing to collect, but there IS still a child of that group
    /// to wait for, so Linux blocks instead of answering `ECHILD`. The
    /// zombie's pgid is the only thing that can say so, because the process
    /// object is gone -- this is the one call that reaches the recorded
    /// exits WITHOUT the scan's shortcut, which only runs when the waiter
    /// asked about exits at all.
    #[test]
    fn a_group_wait_for_stops_blocks_on_a_group_that_only_has_a_zombie() {
        extern crate std;
        let parent = a_parent(93_544);
        parent
            .linux()
            .record_child_exit(424_244, 0, ChildCpu::default(), 93_544);
        let stops_only = WaitInterest {
            exited: false,
            stopped: true,
            continued: false,
        };
        let got = async_std::task::block_on(wait_child_any_interest(
            &parent,
            true,
            true,
            stops_only,
            Some(93_544),
        ));
        assert_eq!(
            got.map(|(p, _, _)| p),
            Err(LxError::EAGAIN),
            "the group still has a child, it just has nothing to report yet"
        );
    }
}
