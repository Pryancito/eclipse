//! The mount path, against the scripted connector the greeting is measured
//! against.
//!
//! The greeting has its own suite and this does not repeat it. What is new
//! here is the step *after* it — the session's own connection, and the
//! `FSINFO` down it — and the two things that can only go wrong once the
//! steps are put together: a fourth connection handed a fresh timeout, and a
//! session built on a server that has not agreed to anything.

use super::*;
use crate::error::LxError;
use crate::fs::nfs::dialer::*;
use crate::fs::nfs::fake;
use crate::fs::nfs::nfs3;
use crate::fs::nfs::portmap;

const NFSD_PORT: u16 = 2049;

/// A dialer that answers the greeting and then `FSINFO` on `nfsd`'s port.
///
/// `fsinfo_ok`'s numbers are the server's limits: 8 KiB read, 16 KiB write.
/// Deliberately below the client's 32 KiB default, so what the negotiation
/// does is visible — equal numbers would agree whether or not it happened.
fn a_mountable_server() -> Dialer {
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, getport(NFSD_PORT));
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    d.answer(
        NFSD_PORT,
        fake::fsinfo_ok(8 * 1024, 8 * 1024, 512, 16 * 1024, 4096),
    );
    d
}

#[test]
fn the_session_connects_to_the_port_the_greeting_found() {
    let mut d = a_mountable_server();
    let log = d.log();
    connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    assert_eq!(
        log.lock().dialled,
        vec![
            portmap::PORTMAP_PORT,
            portmap::PORTMAP_PORT,
            MOUNTD_PORT,
            NFSD_PORT
        ],
        "the session's connection is the fourth dial and goes to the port \
         portmap named for the NFS program"
    );
}

#[test]
fn the_sessions_connection_shares_the_greetings_deadline() {
    // The budget is absolute and read from the real clock, so a shared
    // deadline and a restarted one are the same number until time actually
    // passes -- a whole scripted greeting takes under a millisecond. So one
    // step is made to cost real time, and what is measured is that the
    // session's dial was handed **less** than the first dial was: with a
    // fresh budget it would be handed the same or more, however slow the
    // steps before it were.
    const BURN_MS: u64 = 40;
    let mut d = a_mountable_server();
    d.slow(MOUNTD_PORT, BURN_MS);
    let log = d.log();
    connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    let log = log.lock();
    let first = log.budgets[0];
    let last = *log.budgets.last().expect("a dial");
    assert!(
        first >= last + BURN_MS / 2,
        "the session's dial got {} ms of a budget that started at {} ms, \
         after a step that took {} ms: a fourth full wait on a mount(2) \
         that asked for one",
        last,
        first,
        BURN_MS
    );
}

#[test]
fn fsinfo_is_asked_on_the_sessions_connection_before_the_mount_succeeds() {
    let mut d = a_mountable_server();
    let log = d.log();
    connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    let log = log.lock();
    let last = log.calls.last().expect("a call");
    assert_eq!(last.port, NFSD_PORT);
    assert_eq!(last.procedure, nfs3::PROC_FSINFO);
    assert_eq!(last.cred_flavor, 1, "AUTH_UNIX, as every session call is");
}

#[test]
fn a_server_that_refuses_fsinfo_does_not_get_mounted() {
    let mut d = a_mountable_server();
    d.clear(NFSD_PORT);
    // NFS3ERR_STALE: the handle `mountd` handed over is one `nfsd` will not
    // take. Invisible to the greeting, which never spoke to `nfsd`.
    d.answer(NFSD_PORT, fake::failure(70));
    let err = connect_export(&mut d, &a_source(), &quick_options())
        .err()
        .expect("a mount that fails");
    assert_eq!(err, LxError::ESTALE);
}

#[test]
fn the_transfer_sizes_come_down_to_what_the_server_will_carry() {
    let mut d = a_mountable_server();
    let mounted = connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    let session = mounted.fs.session().lock();
    assert_eq!(session.read_size(), 8 * 1024);
    assert_eq!(session.write_size(), 16 * 1024);
}

#[test]
fn the_root_the_session_calls_on_is_the_handle_mountd_sent() {
    let mut d = a_mountable_server();
    let mounted = connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    assert_eq!(mounted.fs.session().lock().root().as_bytes(), ROOT);
}

#[test]
fn the_mount_daemons_port_is_kept_for_the_unmount() {
    let mut d = a_mountable_server();
    let mounted = connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    assert_eq!(mounted.mount_port, MOUNTD_PORT);
    assert_eq!(mounted.nfs_port, NFSD_PORT);
}

#[test]
fn only_the_sessions_connection_is_still_open_when_the_mount_returns() {
    let mut d = a_mountable_server();
    let log = d.log();
    let mounted = connect_export(&mut d, &a_source(), &quick_options()).expect("mount");
    assert_eq!(
        log.lock().open,
        1,
        "the greeting's three connections are let go of and the session's \
         is the one that is held"
    );
    drop(mounted);
    assert_eq!(log.lock().open, 0);
}

#[test]
fn a_greeting_that_fails_opens_no_session() {
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, getport(NFSD_PORT));
    // MNT3ERR_ACCES.
    d.answer(MOUNTD_PORT, mnt_err(13));
    let log = d.log();
    let err = connect_export(&mut d, &a_source(), &quick_options())
        .err()
        .expect("a mount that fails");
    assert_eq!(err, LxError::EACCES);
    assert!(
        !log.lock().dialled.contains(&NFSD_PORT),
        "nfsd was dialled for an export mountd refused: {:?}",
        log.lock().dialled
    );
}

#[test]
fn a_mount_with_no_budget_left_never_reaches_the_session() {
    let mut d = a_mountable_server();
    let log = d.log();
    let opts = NfsOptions {
        timeout_ms: 0,
        ..quick_options()
    };
    // `greeting_budget` floors the budget at `GREETING_TIMEOUT_MS`, so the
    // way to measure an exhausted one is to ask the greeting directly with
    // nothing in it. What this holds is the arithmetic the mount depends on:
    // out of time means no dial, not a dial with a zero timeout that waits
    // for ever.
    let err = greeting::greet_with_budget(&mut d, &a_source(), &opts, 0)
        .expect_err("a greeting that fails");
    assert_eq!(err, LxError::ETIMEDOUT);
    assert!(log.lock().dialled.is_empty());
}

/// Two mounts under one deadline: the second gets what the first left, not
/// a minute of its own.
///
/// This is what `/etc/fstab` does with its network lines, and the reason it
/// does: a deadline per line means a boot that waits a minute *per*
/// unreachable server before the shell appears, so the bound has to be on
/// the run. Measured the same way the three-step version is — a step made
/// to cost real time, and the later dial checked for having **less** —
/// because an absolute deadline read from the real clock is indis-
/// tinguishable from a fresh one until time passes.
#[test]
fn a_second_mount_under_the_same_deadline_gets_what_the_first_left_of_it() {
    const BURN_MS: u64 = 40;
    let budget = Budget::new(greeting::greeting_budget(&quick_options()));

    let mut first = a_mountable_server();
    first.slow(MOUNTD_PORT, BURN_MS);
    let first_log = first.log();
    connect_export_with_deadline(&mut first, &a_source(), &quick_options(), &budget)
        .expect("the first line mounts");

    let mut second = a_mountable_server();
    let second_log = second.log();
    connect_export_with_deadline(&mut second, &a_source(), &quick_options(), &budget)
        .expect("the second line mounts");

    let opened = first_log.lock().budgets[0];
    let inherited = second_log.lock().budgets[0];
    assert!(
        opened >= inherited + BURN_MS / 2,
        "the second line's first dial got {} ms of a deadline that started \
         at {} ms, after a line that spent {} ms: a fresh minute per fstab \
         line, which is the arithmetic the shared deadline exists to stop",
        inherited,
        opened,
        BURN_MS
    );
}

/// A deadline already spent mounts nothing and, above all, dials nothing.
///
/// The state an fstab run reaches after its budget has gone on servers that
/// never answered: every line after that point has to be cheap, because
/// the point of the bound is that boot stops waiting — not that it waits
/// the same amount in smaller pieces.
#[test]
fn a_line_whose_deadline_is_already_spent_dials_nobody() {
    let budget = Budget::new(0);
    let mut d = a_mountable_server();
    let log = d.log();
    let err = match connect_export_with_deadline(&mut d, &a_source(), &quick_options(), &budget) {
        Ok(_) => panic!("a spent deadline cannot mount"),
        Err(e) => e,
    };
    assert_eq!(err, LxError::ETIMEDOUT);
    assert!(
        log.lock().dialled.is_empty(),
        "dialled {:?} with no time left to hear an answer in",
        log.lock().dialled
    );
}
