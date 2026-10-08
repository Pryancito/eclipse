//! `check_kill_permission()`: who may signal whom, and how the verdicts
//! of a whole group or a whole machine fold into one answer.
//!
//! `sys_kill` asked nothing at all, so any process could `SIGKILL` any
//! other. It is the same shape of hole as the scheduler's, with a
//! different set of ids -- and the difference between the two sets is
//! the interesting part.

use super::*;

const USER: u32 = 1000;
const OTHER: u32 = 1001;

fn creds(ruid: u32, euid: u32, suid: u32) -> Credentials {
    Credentials {
        ruid,
        euid,
        suid,
        rgid: ruid,
        egid: euid,
        sgid: suid,
        fsuid: euid,
        fsgid: euid,
        groups: Vec::new(),
        umask: 0o022,
    }
}

fn plain(uid: u32) -> Credentials {
    creds(uid, uid, uid)
}

// ---- which four ids ---------------------------------------------------

#[test]
fn a_process_running_as_you_is_yours_to_kill() {
    assert!(LinuxProcess::may_signal_cred(&plain(USER), &plain(USER)));
}

#[test]
fn a_setuid_program_stays_killable_by_the_user_who_started_it() {
    // Its real and saved uids are still yours while it runs as root.
    let started_by_user = creds(USER, ROOT_UID, USER);
    assert!(LinuxProcess::may_signal_cred(
        &plain(USER),
        &started_by_user
    ));
}

#[test]
fn the_user_a_program_turned_into_cannot_kill_it() {
    // The one id that does NOT count is the target's effective uid. A
    // program that dropped from root to OTHER keeps ruid/suid = root, so
    // OTHER -- whom it is now running as -- still cannot touch it.
    let dropped_to_other = creds(ROOT_UID, OTHER, ROOT_UID);
    assert!(!LinuxProcess::may_signal_cred(
        &plain(OTHER),
        &dropped_to_other
    ));
}

#[test]
fn the_targets_effective_uid_is_the_one_id_that_does_not_count() {
    // The same pair, read by the two rules: reniceing looks at the
    // target's effective uid and killing does not, so a process that has
    // turned into you is yours to renice and not yours to kill.
    let caller = plain(USER);
    let turned_into_you = creds(OTHER, USER, OTHER);
    assert!(LinuxProcess::may_set_priority_of(&caller, &turned_into_you));
    assert!(!LinuxProcess::may_signal_cred(&caller, &turned_into_you));
}

#[test]
fn both_of_the_callers_uids_count() {
    // A caller half-way through a set-user-ID dance reaches the processes
    // of BOTH users -- `cred->euid` and `cred->uid` each get a turn.
    let half_way = creds(USER, OTHER, OTHER);
    assert!(LinuxProcess::may_signal_cred(&half_way, &plain(USER)));
    assert!(LinuxProcess::may_signal_cred(&half_way, &plain(OTHER)));
}

#[test]
fn each_of_the_four_pairs_reaches_on_its_own() {
    // Four ids compared and no coincidences: each case matches exactly
    // one of the four clauses, so dropping any one of them shows up.
    // The caller is `ruid A, euid B`; none of these uids is root.
    const A: u32 = 10;
    const B: u32 = 11;
    const C: u32 = 12;
    let caller = creds(A, B, A);
    assert!(
        LinuxProcess::may_signal_cred(&caller, &creds(C, C, B)),
        "cred->euid against tcred->suid"
    );
    assert!(
        LinuxProcess::may_signal_cred(&caller, &creds(B, C, C)),
        "cred->euid against tcred->uid"
    );
    assert!(
        LinuxProcess::may_signal_cred(&caller, &creds(C, C, A)),
        "cred->uid against tcred->suid"
    );
    assert!(
        LinuxProcess::may_signal_cred(&caller, &creds(A, C, C)),
        "cred->uid against tcred->uid"
    );
    assert!(
        !LinuxProcess::may_signal_cred(&caller, &creds(C, C, C)),
        "and nothing in common reaches nothing"
    );
}

#[test]
fn a_strangers_process_needs_the_capability() {
    assert!(!LinuxProcess::may_signal_cred(&plain(USER), &plain(OTHER)));
    assert!(LinuxProcess::may_signal_cred(
        &plain(ROOT_UID),
        &plain(OTHER)
    ));
}

#[test]
fn the_capability_is_read_off_the_callers_effective_uid() {
    let dropped_root = creds(ROOT_UID, USER, ROOT_UID);
    assert!(!LinuxProcess::may_signal_cred(&dropped_root, &plain(OTHER)));
}

// ---- the two ways past ------------------------------------------------

fn verdict(
    caller: &Credentials,
    target: &Credentials,
    same_group: bool,
    same_session: bool,
    signal: Option<LinuxSignal>,
) -> LxResult<()> {
    LinuxProcess::may_signal(caller, target, same_group, same_session, signal)
}

#[test]
fn a_thread_always_reaches_its_own_thread_group() {
    // Whatever the ids say: a process signalling itself is never a
    // permission question.
    assert_eq!(
        verdict(
            &plain(USER),
            &plain(OTHER),
            true,
            false,
            Some(LinuxSignal::SIGKILL)
        ),
        Ok(())
    );
}

#[test]
fn sigcont_reaches_your_own_session_whoever_owns_it() {
    // The shell that resumes a job is not always the job's owner, and a
    // session nobody could continue would be a session that can be wedged
    // from the inside.
    assert_eq!(
        verdict(
            &plain(USER),
            &plain(OTHER),
            false,
            true,
            Some(LinuxSignal::SIGCONT)
        ),
        Ok(())
    );
}

#[test]
fn sigcont_to_another_session_is_eperm_like_anything_else() {
    assert_eq!(
        verdict(
            &plain(USER),
            &plain(OTHER),
            false,
            false,
            Some(LinuxSignal::SIGCONT)
        ),
        Err(LxError::EPERM)
    );
}

#[test]
fn no_signal_but_sigcont_gets_the_session_exception() {
    for sig in [
        LinuxSignal::SIGTERM,
        LinuxSignal::SIGKILL,
        LinuxSignal::SIGHUP,
        LinuxSignal::SIGSTOP,
    ] {
        assert_eq!(
            verdict(&plain(USER), &plain(OTHER), false, true, Some(sig)),
            Err(LxError::EPERM),
            "{:?} should not ride the SIGCONT exception",
            sig
        );
    }
}

#[test]
fn a_probe_is_refused_exactly_like_a_signal() {
    // kill(pid, 0) delivers nothing and asks the same question. If it
    // did not, it would be a way to ask whether a process you may not
    // signal exists.
    assert_eq!(
        verdict(&plain(USER), &plain(OTHER), false, false, None),
        Err(LxError::EPERM)
    );
    assert_eq!(
        verdict(&plain(USER), &plain(OTHER), false, true, None),
        Err(LxError::EPERM),
        "and it is not SIGCONT, so the session does not help it"
    );
}

#[test]
fn the_capability_reaches_any_owner_in_any_session() {
    assert_eq!(
        verdict(
            &plain(ROOT_UID),
            &plain(OTHER),
            false,
            false,
            Some(LinuxSignal::SIGKILL)
        ),
        Ok(())
    );
}

// ---- folding a group --------------------------------------------------

fn fold_group(members: &[LxResult<()>]) -> LxResult<()> {
    members
        .iter()
        .copied()
        .fold(Err(LxError::ESRCH), LinuxProcess::fold_group_signal)
}

#[test]
fn a_group_with_no_members_is_esrch() {
    assert_eq!(fold_group(&[]), Err(LxError::ESRCH));
}

#[test]
fn one_member_reached_makes_the_whole_group_send_a_success() {
    assert_eq!(fold_group(&[Err(LxError::EPERM), Ok(())]), Ok(()));
    assert_eq!(
        fold_group(&[Ok(()), Err(LxError::EPERM)]),
        Ok(()),
        "and a later refusal cannot take the success back"
    );
}

#[test]
fn a_group_that_refused_every_member_reports_the_last_refusal() {
    assert_eq!(
        fold_group(&[Err(LxError::ESRCH), Err(LxError::EPERM)]),
        Err(LxError::EPERM)
    );
    assert_eq!(
        fold_group(&[Err(LxError::EPERM), Err(LxError::EINVAL)]),
        Err(LxError::EINVAL)
    );
}

#[test]
fn a_group_signal_and_a_group_renice_fold_the_opposite_way() {
    // Same two members, one refused and one reached. The signal landed
    // somewhere, so the call worked; the renice did not go through
    // everywhere, so it did not.
    let members = [Err(LxError::EPERM), Ok(())];
    assert_eq!(fold_group(&members), Ok(()));
    assert_eq!(
        members
            .iter()
            .copied()
            .fold(Err(LxError::ESRCH), LinuxProcess::fold_priority_verdict),
        Err(LxError::EPERM)
    );
}

// ---- folding a broadcast ----------------------------------------------

fn fold_broadcast(seen: &[LxResult<()>]) -> LxResult<()> {
    seen.iter()
        .copied()
        .fold(Ok(()), LinuxProcess::fold_broadcast_signal)
}

#[test]
fn a_broadcast_that_every_process_refused_still_reports_success() {
    // Deliberate in Linux: kill(-1, SIGTERM) means "everything I am
    // allowed to", and being allowed nothing is not an error. `retval`
    // starts at 0 and EPERM is the one error that never reaches it.
    assert_eq!(
        fold_broadcast(&[Err(LxError::EPERM), Err(LxError::EPERM)]),
        Ok(())
    );
}

#[test]
fn a_broadcast_reports_an_error_that_is_not_eperm() {
    assert_eq!(
        fold_broadcast(&[Err(LxError::EPERM), Err(LxError::EINVAL)]),
        Err(LxError::EINVAL)
    );
}

#[test]
fn a_later_process_overwrites_an_earlier_error_in_a_broadcast() {
    // `retval = err` with no guard, so the last non-EPERM answer is the
    // one that comes back -- a success included.
    assert_eq!(fold_broadcast(&[Err(LxError::EINVAL), Ok(())]), Ok(()));
}
