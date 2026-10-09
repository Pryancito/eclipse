//! `issetugid(2)`, the question a FreeBSD program asks before it decides
//! that the environment and the data segment it woke up with are its own:
//! "are my ids the ones I was started with?" It was answered with a
//! constant 0, which is the answer that makes a set-user-ID program trust
//! whatever its caller left in `MALLOC_OPTIONS` or `LD_*`.
//!
//! FreeBSD keeps it as `P_SUGID` on the process. It is deliberately
//! sticky -- `kern_prot.c` explains that a program that started as root
//! and *became* a user without an exec "cannot know everything that libc
//! might have put in their data segment" -- so these tests are mostly
//! about the two ways it must NOT be forgotten.

use super::dup_fd_tests::a_process;
use super::*;

#[test]
fn a_process_that_has_not_touched_its_ids_is_not_tainted() {
    // The case every process on this machine is in. Answering 1 here
    // would put the whole system in the hardened mode, which is the
    // mirror of the bug and just as wrong.
    assert!(!a_process().is_sugid());
}

#[test]
fn dropping_privilege_taints_the_process_even_though_the_ids_end_up_even() {
    // Root calling `setuid(1000)` lands on ruid == euid == suid == 1000,
    // which looks exactly like a process that was started as that user.
    // It is not one: everything in its memory was put there by root. This
    // is the case the flag exists for, and the one a rule derived from
    // the ids alone cannot see.
    let proc = a_process();
    proc.set_uid(1000).unwrap();
    let creds = proc.credentials();
    assert_eq!((creds.ruid, creds.euid, creds.suid), (1000, 1000, 1000));
    assert!(proc.is_sugid());
}

#[test]
fn a_call_that_moves_no_id_does_not_taint() {
    // `sys_setuid` latches inside `if (change)`. Root setting its own id
    // changes nothing about who arranged this process's memory.
    let proc = a_process();
    proc.set_uid(ROOT_UID).unwrap();
    assert!(!proc.is_sugid());
}

#[test]
fn a_refused_switch_does_not_taint() {
    // An unprivileged process asking to become root is told EPERM; it
    // must not come away marked as though it had succeeded.
    let proc = a_process();
    {
        let mut inner = proc.inner.lock();
        inner.credentials.ruid = 1000;
        inner.credentials.set_euid(1000);
        inner.credentials.suid = 1000;
    }
    assert!(proc.set_uid(ROOT_UID).is_err());
    assert!(!proc.is_sugid());
}

#[test]
fn setting_the_supplementary_groups_taints_whatever_the_list_says() {
    // `kern_setgroups()` calls `setsugid(p)` unconditionally -- it never
    // compares the new list with the old one. A process that has called
    // `setgroups` has been rearranged by whoever called it.
    let proc = a_process();
    proc.set_groups(proc.groups());
    assert!(proc.is_sugid());
}

#[test]
fn every_setter_that_moves_an_id_latches_it() {
    // Six setters, six chances to forget the latch, and forgetting is
    // silent: the program asks whether it is tainted and is told no. So
    // the table is the test -- a seventh setter added without it fails
    // here by name.
    type Setter = (&'static str, fn(&LinuxProcess) -> LxResult);
    let setters: [Setter; 6] = [
        ("setuid", |p| p.set_uid(1000)),
        ("setgid", |p| p.set_gid(1000)),
        ("setreuid", |p| p.set_reuid(1000, 1000)),
        ("setregid", |p| p.set_regid(1000, 1000)),
        ("setresuid", |p| p.set_resuid(1000, 1000, 1000)),
        ("setresgid", |p| p.set_resgid(1000, 1000, 1000)),
    ];
    for (name, call) in setters {
        let proc = a_process();
        call(&proc).unwrap_or_else(|e| panic!("{} was refused: {:?}", name, e));
        assert!(proc.is_sugid(), "{} moved an id without tainting", name);
    }
}

#[test]
fn moving_only_the_saved_id_taints_too() {
    // `setresuid(-1, -1, 1000)` moves nothing a `getuid`/`geteuid` pair
    // would show, and it is still a change of who this process may
    // become -- FreeBSD's `sys_setresuid` latches on the saved id like on
    // the other two. A watch list that only held the real and effective
    // ids would call this a no-op.
    let proc = a_process();
    proc.set_resuid(NO_ID, NO_ID, 1000).unwrap();
    assert_eq!(proc.credentials().suid, 1000);
    assert!(proc.is_sugid());
}

#[test]
fn an_exec_that_grants_nothing_clears_the_taint() {
    // `do_execve()`: `p->p_flag &= ~P_SUGID` when the image granted no id
    // AND the effective ids already match the real ones. The new image
    // did not inherit the old one's data segment -- `execve` threw the
    // address space away -- so the doubt goes with it.
    let mut inner = LinuxProcessInner::default();
    inner.sugid = true;
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    assert!(!inner.sugid);
}

#[test]
fn an_exec_cannot_clear_it_while_the_effective_ids_are_uneven() {
    // The other half of the same `else` branch. A process running
    // set-user-ID root keeps its effective id across the exec, so the
    // program it just became is privileged over whoever asked for it --
    // whether or not this particular image had a set-user-ID bit.
    let mut inner = LinuxProcessInner::default();
    inner.credentials.ruid = 1000;
    inner.sugid = false;
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    assert!(inner.sugid);
}

#[test]
fn a_setuid_exec_taints_a_process_that_was_clean() {
    let mut inner = LinuxProcessInner::default();
    inner.credentials.ruid = 1000;
    inner.credentials.set_euid(1000);
    inner.credentials.suid = 1000;
    inner.credentials.rgid = 1000;
    inner.credentials.set_egid(1000);
    inner.credentials.sgid = 1000;
    inner.credentials.groups = vec![1000];
    assert!(!inner.sugid);
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, 1000));
    assert!(inner.sugid);
}
