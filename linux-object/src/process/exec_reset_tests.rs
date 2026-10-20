//! What the PROCESS must forget when it calls `execve`: its attachments
//! to an address space that no longer exists, and the parent's choice of
//! death signal once the exec has made the program privileged.

use super::*;

/// A process holding a shared segment at a known address, a parent-death
/// signal and an ordinary user's ids, so the tests have something real to
/// lose. Nothing here is left at its default: a fixture that is already
/// empty where the test asserts emptiness asserts nothing.
fn a_process_about_to_exec() -> LinuxProcessInner {
    use crate::ipc::ShmGuard;
    use zircon_object::vm::VmObject;
    let mut inner = LinuxProcessInner::default();
    let guard = Arc::new(kernel_hal::sync::Mutex::new(ShmGuard {
        shared_guard: VmObject::new_paged(1),
        shmid_ds: kernel_hal::sync::Mutex::new(Default::default()),
    }));
    inner.shm_identifiers.add(9, guard);
    let mut ident = inner.shm_identifiers.get(9).unwrap();
    ident.addr = 0x7f00_0000;
    inner.shm_identifiers.set(9, ident);
    inner.pdeathsig = crate::signal::Signal::SIGTERM as u8;
    inner.credentials.ruid = 1000;
    inner.credentials.set_euid(1000);
    inner.credentials.suid = 1000;
    inner.credentials.rgid = 1000;
    inner.credentials.set_egid(1000);
    inner.credentials.sgid = 1000;
    // Its own group, not root's: `secureexec` asks whether the effective
    // group is one this process already held, and a fixture left in group
    // 0 answers that question for a user who is not in group 0.
    inner.credentials.groups = vec![1000];
    inner
}

/// `SECBIT_KEEP_CAPS` is cleared by every exec, the unprivileged one
/// included, while `pdeathsig` (checked elsewhere) goes only with the
/// privileged one.
#[test]
fn an_exec_clears_keep_caps_whether_or_not_it_is_privileged() {
    for privileged in [false, true] {
        let mut inner = a_process_about_to_exec();
        inner.keep_caps = true;
        inner.reset_for_exec(privileged);
        assert!(!inner.keep_caps, "privileged = {}", privileged);
    }
}

#[test]
fn an_exec_forgets_the_segments_the_old_address_space_had_attached() {
    // Those mappings died with `vmar.clear()`. The record of where they
    // were is what `shmdt` trusts: it looks an address up in this very
    // map and unmaps that many bytes there, so kept across an exec it
    // punches a hole in the program that is running now.
    let mut inner = a_process_about_to_exec();
    assert_eq!(
        inner.shm_identifiers.get_id(0x7f00_0000),
        Some(9),
        "the fixture must be holding a segment to lose"
    );
    inner.reset_for_exec(false);
    assert_eq!(inner.shm_identifiers.get_id(0x7f00_0000), None);
    assert!(inner.shm_identifiers.get(9).is_none());
}

#[test]
fn an_exec_detaches_the_segments_on_the_segment_side_too() {
    // Linux unmaps the old mm's segments with `shm_close` on each, so
    // `shm_nattch` drops. Here the record was cleared and the count
    // stayed: a program that exec'd with a segment attached counted as
    // its user for ever.
    let mut inner = a_process_about_to_exec();
    let guard = inner.shm_identifiers.get(9).unwrap().guard;
    guard.lock().attach(1);
    assert_eq!(guard.lock().shmid_ds.lock().nattch, 1);
    let old = inner.reset_for_exec(false);
    assert_eq!(
        guard.lock().shmid_ds.lock().nattch,
        1,
        "the detach happens when the old table is dropped, outside the lock"
    );
    drop(old);
    assert_eq!(guard.lock().shmid_ds.lock().nattch, 0);
}

/// `begin_new_exec`: `me->flags &= ~PF_FORKNOEXEC`, and BEFORE
/// `do_close_on_exec`. A parent learns that its child exec'd from the EOF of
/// a close-on-exec pipe (`posix_spawn`, Rust's `Command`), and from that
/// moment its `setpgid` on the child is EACCES. Marked at the end of the
/// exec, as it was, the parent could still regroup a child that had closed
/// the pipe on its way into another program.
#[test]
fn the_window_its_parent_had_to_group_it_shuts_as_the_descriptors_go() {
    use super::dup_fd_tests::{a_process, an_open_log, Log};
    let proc = a_process();
    let fd = proc
        .add_file_cloexec(an_open_log(Log::new(), OpenFlags::RDWR), true)
        .unwrap();
    assert!(!proc.has_execed(), "a new process has not exec'd yet");

    proc.remove_cloexec_files();
    assert!(proc.get_file_like(fd).is_err(), "the descriptor went");
    assert!(proc.has_execed());
}

#[test]
fn an_ordinary_exec_keeps_the_parent_death_signal() {
    // prctl(2) puts PR_SET_PDEATHSIG among the settings an `execve`
    // preserves; Linux clears it only for a privilege-raising one. A
    // supervisor that sets it and then execs its real payload is relying
    // on exactly this.
    let mut inner = a_process_about_to_exec();
    inner.reset_for_exec(false);
    assert_eq!(inner.pdeathsig, crate::signal::Signal::SIGTERM as u8);
}

#[test]
fn an_exec_that_raises_privileges_drops_the_parent_death_signal() {
    // `begin_new_exec()`: `if (bprm->secureexec) me->pdeath_signal = 0;`
    // The parent chose both the signal and, by exiting, the moment --
    // while the child was still running code the parent controlled. It
    // must not keep that lever over a program that is now privileged.
    let mut inner = a_process_about_to_exec();
    inner.reset_for_exec(true);
    assert_eq!(inner.pdeathsig, 0);
}

#[test]
fn a_setuid_image_owned_by_another_user_raises_privileges() {
    let mut inner = a_process_about_to_exec();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, 1000));
    assert_eq!(inner.credentials.euid, ROOT_UID);
    // The saved id follows, which is what lets the program drop and
    // regain the privilege later.
    assert_eq!(inner.credentials.suid, ROOT_UID);
    // And the real id does not move: that is the whole point of setuid.
    assert_eq!(inner.credentials.ruid, 1000);
}

#[test]
fn a_setgid_image_alone_raises_privileges_too() {
    // Its own test because the group half is a second, separate decision
    // -- and a `raised` that only ever looked at the user half would let
    // a set-group-ID program keep the lever.
    let mut inner = a_process_about_to_exec();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o2755, 1000, 0));
    assert_eq!(inner.credentials.egid, 0);
    assert_eq!(inner.credentials.sgid, 0);
    assert_eq!(inner.credentials.rgid, 1000);
}

#[test]
fn a_setuid_bit_naming_the_id_we_already_run_as_raises_nothing() {
    // Linux's `secureexec` asks whether the ids CHANGED, not whether the
    // bits were set. A user's own set-user-ID binary grants that user
    // nothing, so nothing about the process needs hardening.
    let mut inner = a_process_about_to_exec();
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o6755, 1000, 1000));
    assert_eq!(inner.credentials.euid, 1000);
    assert_eq!(inner.credentials.egid, 1000);
}

#[test]
fn an_ordinary_image_raises_nothing() {
    let mut inner = a_process_about_to_exec();
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    assert_eq!(inner.credentials.euid, 1000);
    assert_eq!(inner.credentials.egid, 1000);
}

#[test]
fn no_new_privs_means_the_setuid_bits_are_not_honoured_at_all() {
    // Documentation/userspace-api/no_new_privs.rst. And since nothing was
    // granted, nothing was raised: reporting a raise here would have the
    // process hardened against a privilege it never got.
    let mut inner = a_process_about_to_exec();
    inner.no_new_privs = true;
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o6755, ROOT_UID, ROOT_UID));
    assert_eq!(inner.credentials.euid, 1000);
    assert_eq!(inner.credentials.egid, 1000);
    assert_eq!(inner.credentials.suid, 1000);
    assert_eq!(inner.credentials.sgid, 1000);
}

#[test]
fn a_setuid_image_on_a_nosuid_mount_grants_nothing() {
    // `mnt_may_suid(bprm->file->f_path.mnt)`: this is the whole of what
    // mounting `/tmp` with `nosuid` buys, and it was bought and never
    // delivered -- the option reached `/proc/mounts` and stopped there.
    let mut inner = a_process_about_to_exec();
    assert!(!inner.apply_exec_ids(0o6755, ROOT_UID, ROOT_UID, false));
    assert_eq!(inner.credentials.euid, 1000);
    assert_eq!(inner.credentials.egid, 1000);
    assert_eq!(inner.credentials.suid, 1000);
    assert_eq!(inner.credentials.sgid, 1000);
}

#[test]
fn nosuid_does_not_hide_a_process_that_was_already_privileged() {
    // The mount decides what this exec may GRANT. It says nothing about
    // what the process is already carrying, and a program running as root
    // has to distrust its environment wherever its image happens to live.
    let mut inner = a_process_already_setuid_root();
    assert!(inner.apply_exec_ids(0o0755, ROOT_UID, ROOT_UID, false));
    assert_eq!(inner.credentials.euid, ROOT_UID);
}

#[test]
fn the_mount_and_no_new_privs_are_two_separate_gates() {
    // Either one refusing is enough, and neither is the other: a table,
    // so that a rewrite collapsing them into one condition fails here by
    // name rather than in whichever of the two cases it got wrong.
    for (may_suid, no_new_privs, honoured) in [
        (true, false, true),
        (false, false, false),
        (true, true, false),
        (false, true, false),
    ] {
        let mut inner = a_process_about_to_exec();
        inner.no_new_privs = no_new_privs;
        inner.apply_exec_ids(0o4755, ROOT_UID, ROOT_UID, may_suid);
        let got = inner.credentials.euid == ROOT_UID;
        assert_eq!(
            got, honoured,
            "may_suid={} no_new_privs={}: euid ended {}",
            may_suid, no_new_privs, inner.credentials.euid
        );
    }
}

#[test]
fn the_bits_are_the_ones_linux_uses() {
    // Pinned against the octal literals, not against each other: a test
    // that checks a constant with the same constant moves with it.
    assert_eq!(MODE_SET_UID, 0o4000);
    assert_eq!(MODE_SET_GID, 0o2000);
    assert_eq!(MODE_EXEC_GRP, 0o0010);
}

// --- `bprm->secureexec` -------------------------------------------------
//
// `cap_bprm_creds_from_file()` asks three questions and takes any one of
// them as a yes. The first is about the image being loaded; the other two
// are about the process, and they are the ones a rule written only around
// the set-user-ID bits cannot see.

/// A process already running set-user-ID root: its real id is an ordinary
/// user's and its effective id is not. Whoever started it chose its
/// environment; whoever owns the id it runs as did not.
fn a_process_already_setuid_root() -> LinuxProcessInner {
    let mut inner = a_process_about_to_exec();
    inner.credentials.set_euid(ROOT_UID);
    inner.credentials.suid = ROOT_UID;
    inner
}

#[test]
fn an_already_privileged_process_is_secure_even_exec_ing_an_ordinary_image() {
    // `!uid_eq(new->euid, old->uid)`. There is no set-user-ID bit in
    // sight here: the effective id came from the PREVIOUS exec and
    // survives this one, so the ordinary shell being loaded runs with
    // exactly the same privilege -- and can trust the caller's
    // LD_PRELOAD exactly as little.
    let mut inner = a_process_already_setuid_root();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    assert_eq!(inner.credentials.euid, ROOT_UID);
    assert_eq!(inner.credentials.ruid, 1000);
}

#[test]
fn the_group_half_is_asked_on_its_own() {
    // `!gid_eq(new->egid, old->gid)`. A set-group-ID program that reads
    // the mail spool is privileged over its caller just as surely as a
    // set-user-ID one, and a rule that only ever looked at the user half
    // would hand it the caller's environment.
    let mut inner = a_process_about_to_exec();
    inner.credentials.set_egid(0);
    inner.credentials.sgid = 0;
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    assert_eq!(inner.credentials.egid, 0);
    assert_eq!(inner.credentials.rgid, 1000);
}

#[test]
fn no_new_privs_does_not_make_an_already_privileged_process_look_safe() {
    // Linux returns early from `bprm_fill_uid()` and computes
    // `secureexec` afterwards regardless. Refusing to RAISE a process
    // says nothing about how privileged it already was -- and answering
    // "not secure" here would hand the caller's LD_PRELOAD to a process
    // running as root, in the one mode whose whole purpose is that a
    // sandbox can exec without granting anything.
    let mut inner = a_process_already_setuid_root();
    inner.no_new_privs = true;
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o6755, 1000, 1000));
    // The bits were still not honoured: the image asked for uid 1000 and
    // got nothing, which is the whole of what no_new_privs promises.
    assert_eq!(inner.credentials.euid, ROOT_UID);
    assert_eq!(inner.credentials.egid, 1000);
}

#[test]
fn an_exec_that_moves_an_id_is_a_change_even_when_the_ids_end_up_even() {
    // `id_changed = !uid_eq(new->euid, old->euid)`, and it is a separate
    // question from the two that compare against the real ids -- this is
    // the case where only it can answer. A process running set-user-ID
    // root (ruid 1000, euid 0) execs an image whose set-user-ID bit names
    // 1000: the ids come out even, so both "effective != real" questions
    // say no, and the exec still performed an id transition that the new
    // image must be hardened against.
    let mut inner = a_process_about_to_exec();
    inner.credentials.set_euid(ROOT_UID);
    inner.credentials.suid = ROOT_UID;
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, 1000, ROOT_UID));
    assert_eq!(inner.credentials.euid, 1000);
    assert_eq!(inner.credentials.ruid, 1000);
    assert_eq!(inner.credentials.egid, inner.credentials.rgid);
}

#[test]
fn a_setgid_bit_on_a_file_the_group_cannot_execute_is_not_a_setgid_program() {
    // `(mode & (S_ISGID | S_IXGRP)) == (S_ISGID | S_IXGRP)`. Without the
    // group-execute bit, S_ISGID is the mandatory-locking convention --
    // an old, unrelated use of the same bit -- and honouring it would
    // move a process's effective group for a file that never claimed to
    // be a set-group-ID program at all.
    let mut inner = a_process_about_to_exec();
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o2744, ROOT_UID, ROOT_UID));
    assert_eq!(inner.credentials.egid, 1000);
    assert_eq!(inner.credentials.sgid, 1000);
}

#[test]
fn the_same_bit_with_group_execute_is_one() {
    // The other side of the line above, so that the test pair pins the
    // rule and not just one of its answers.
    let mut inner = a_process_about_to_exec();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o2754, ROOT_UID, ROOT_UID));
    assert_eq!(inner.credentials.egid, ROOT_UID);
    assert_eq!(inner.credentials.sgid, ROOT_UID);
}

#[test]
fn falling_back_to_a_group_the_caller_already_held_grants_nothing() {
    // `in_group_p(new->egid)`: the group half of `id_changed` is a
    // membership test, not a comparison. A process running set-group-ID
    // root that execs a set-group-ID image naming a group it is already
    // in has gained nothing to be hardened against -- it has given
    // something up.
    let mut inner = a_process_about_to_exec();
    inner.credentials.set_egid(ROOT_UID);
    inner.credentials.sgid = ROOT_UID;
    inner.credentials.groups = vec![1000];
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o2754, ROOT_UID, 1000));
    assert_eq!(inner.credentials.egid, 1000);
}

#[test]
fn a_group_the_caller_did_not_hold_is_a_raise_even_back_to_its_own() {
    // The same shape, minus the membership: Linux asks `in_group_p`,
    // which knows nothing about the real group id, so a process whose
    // supplementary list does not carry its own rgid is hardened when it
    // returns to it. Written down because it looks like a mistake and is
    // the rule as Linux states it.
    let mut inner = a_process_about_to_exec();
    inner.credentials.set_egid(ROOT_UID);
    inner.credentials.sgid = ROOT_UID;
    inner.credentials.groups = vec![ROOT_UID];
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o2754, ROOT_UID, 1000));
    assert_eq!(inner.credentials.egid, 1000);
}

#[test]
fn an_ordinary_process_exec_ing_an_ordinary_image_is_not_secure() {
    // The case every process on this machine is in, and the one that
    // matters most to get right in THIS direction: answering "secure"
    // here puts the whole system in secure mode, which drops LD_PRELOAD
    // everywhere and makes GLib refuse to autolaunch a session bus.
    let mut inner = LinuxProcessInner::default();
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
}

#[test]
fn an_ordinary_user_in_no_supplementary_group_is_not_secure_either() {
    // The same case with the supplementary list EMPTY, which is what a
    // system account looks like -- `nobody`, a daemon user, anyone whose
    // `/etc/group` lines name no extra group. `in_group_p(new->egid)` is
    // `egid == current_egid() || groups_search(...)`, and for these the
    // second half answers no, so the first half is the only thing between
    // an ordinary `execve` and secure mode. Every other fixture in this
    // file carries its own gid in the list and so never asks it.
    let mut inner = a_process_about_to_exec();
    inner.credentials.groups = Vec::new();
    assert!(
        !inner.apply_exec_ids_from_a_normal_mount(0o0755, 1000, 1000),
        "an exec that moves nothing is not a raise, list or no list"
    );
    assert_eq!(inner.credentials.egid, 1000);
}

// --- Dumpability is decided again at every exec -------------------------

#[test]
fn a_privilege_raising_exec_leaves_the_image_non_dumpable() {
    // `begin_new_exec()`: "Figure out dumpability". Left dumpable, the
    // unprivileged user who started a set-user-ID root program can read
    // the memory of a process running as root -- its `/proc/<pid>/maps`,
    // and through `pidfd_getfd` its open files.
    let mut inner = a_process_about_to_exec();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o4755, ROOT_UID, 1000));
    inner.reset_for_exec(true);
    assert_eq!(inner.dumpable, Some(SUID_DUMP_DISABLE));
}

#[test]
fn an_ordinary_exec_makes_the_image_dumpable_again() {
    // The other direction of the same `else`, and the one a program
    // notices: a shell that had hardened itself with
    // `prctl(PR_SET_DUMPABLE, 0)` must not hand that down to every
    // command it runs, or the user cannot attach a debugger to anything
    // they started from it.
    let mut inner = a_process_about_to_exec();
    inner.dumpable = Some(SUID_DUMP_DISABLE);
    assert!(!inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    inner.reset_for_exec(false);
    assert_eq!(inner.dumpable, Some(SUID_DUMP_USER));
}

#[test]
fn a_setgid_image_is_as_unreadable_as_a_setuid_one() {
    // `!(uid_eq(...) && gid_eq(...))`: the condition asks about the group
    // pair as well, and a version that only compared the user ids left
    // every set-group-ID program readable by whoever started it -- a
    // `mail` or `tty` program whose memory holds somebody else's data.
    let mut inner = a_process_about_to_exec();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o2755, 1000, 0));
    assert_eq!(inner.credentials.euid, 1000, "the user half did not move");
    inner.reset_for_exec(true);
    assert_eq!(inner.dumpable, Some(SUID_DUMP_DISABLE));
}

#[test]
fn the_ids_decide_it_and_not_the_secureexec_argument() {
    // `secureexec` is the wider question: an exec is secure when the ids
    // MOVED, while dumpability asks only whether the image now runs as
    // somebody other than who asked for it. A process that was already
    // set-user-ID root and execs an ordinary image is secure and ALSO
    // non-dumpable; one whose effective ids match its real ones is
    // neither, whatever the argument says.
    let mut inner = a_process_already_setuid_root();
    assert!(inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID));
    inner.reset_for_exec(true);
    assert_eq!(
        inner.dumpable,
        Some(SUID_DUMP_DISABLE),
        "euid is still root"
    );

    let mut inner = a_process_about_to_exec();
    inner.reset_for_exec(true);
    assert_eq!(
        inner.dumpable,
        Some(SUID_DUMP_USER),
        "nothing to hide from the user who owns every one of its ids"
    );
}

// --- The saved ids do not survive the exec ------------------------------
//
// execve(2), POSIX: "the saved set-user-ID of the new process image shall
// be set to the effective user ID of the new process image", and
// `cap_bprm_creds_from_file()` ends with the line that does it. It is not
// conditional on any bit of the image, which is exactly what makes it easy
// to leave out: with no set-user-ID bit in sight there seems to be nothing
// to do.

#[test]
fn an_exec_drops_the_saved_user_id_the_old_image_was_keeping() {
    // The process every set-user-ID root program becomes while it does its
    // unprivileged work: `setresuid(1000, 1000, 0)`, which keeps uid 0
    // SAVED on purpose so it can come back. Then it execs an ordinary
    // image. Carried over, the saved id is a door left open in a program
    // that was never privileged, and `setid_allowed` opens it by name.
    let mut inner = a_process_about_to_exec();
    inner.credentials.suid = ROOT_UID;
    assert!(
        !inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID),
        "the fixture must not be raising anything, or this is a test about setuid bits"
    );
    assert_eq!(
        inner.credentials.suid, 1000,
        "the image it just loaded can call setuid(0) and be root"
    );
    assert!(
        !LinuxProcess::setid_allowed(inner.credentials.ruid, inner.credentials.suid, ROOT_UID),
        "setuid(0) from the new image is still accepted"
    );
}

#[test]
fn an_exec_drops_the_saved_group_id_the_old_image_was_keeping() {
    // The same door, one field over. A set-group-ID `mail` program that
    // drops its group to serve a user keeps the group saved, and the
    // image it execs must not be able to ask for it back.
    let mut inner = a_process_about_to_exec();
    inner.credentials.sgid = 0;
    inner.apply_exec_ids_from_a_normal_mount(0o0755, ROOT_UID, ROOT_UID);
    assert_eq!(
        inner.credentials.sgid, 1000,
        "the image it just loaded can call setgid(0) and be in group 0"
    );
}

#[test]
fn even_an_exec_that_grants_nothing_because_of_nosuid_drops_them() {
    // The two gates are `bprm_fill_uid()`'s, and this line is not in
    // `bprm_fill_uid()`: `cap_bprm_creds_from_file()` runs it whatever the
    // mount or `no_new_privs` said. A `nosuid` mount is there to grant
    // LESS, so it cannot be the one case that leaves the hatch open.
    for (may_suid, no_new_privs) in [(false, false), (true, true)] {
        let mut inner = a_process_about_to_exec();
        inner.no_new_privs = no_new_privs;
        inner.credentials.suid = ROOT_UID;
        inner.apply_exec_ids(0o4755, ROOT_UID, ROOT_UID, may_suid);
        assert_eq!(
            inner.credentials.euid, 1000,
            "may_suid={} no_new_privs={}: the bit was honoured after all",
            may_suid, no_new_privs
        );
        assert_eq!(
            inner.credentials.suid, 1000,
            "may_suid={} no_new_privs={}",
            may_suid, no_new_privs
        );
    }
}

// --- The aux-vector identity block --------------------------------------

#[test]
fn the_identity_block_carries_the_ids_the_new_image_will_run_with() {
    // Built after `apply_exec_ids`, because that is when the ids are
    // final -- and mapped here, once, so no caller has to remember that
    // `AT_UID` is the REAL id while `AT_EUID` is the effective one.
    let mut inner = a_process_already_setuid_root();
    inner.credentials.rgid = 1001;
    let id = inner.aux_identity(true);
    assert_eq!(id.uid, 1000, "AT_UID is the REAL user id");
    assert_eq!(id.euid, ROOT_UID, "AT_EUID is the EFFECTIVE user id");
    assert_eq!(id.gid, 1001, "AT_GID is the REAL group id");
    assert_eq!(id.egid, 1000, "AT_EGID is the EFFECTIVE group id");
    assert!(id.secure);
}

#[test]
fn the_identity_block_reports_an_unprivileged_exec_as_such() {
    let id = LinuxProcessInner::default().aux_identity(false);
    assert!(!id.secure);
    assert_eq!((id.uid, id.euid, id.gid, id.egid), (0, 0, 0, 0));
}
