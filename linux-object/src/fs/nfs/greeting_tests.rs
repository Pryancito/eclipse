//! The greeting, against a scripted server reached through a scripted
//! connector.
//!
//! What has to be measured here is not a decoding — the codecs have their own
//! suites — but **which port was dialled, what program was asked for down it,
//! with whose credential, and whether the connection was let go of**. So the
//! connector writes every dial into a shared log, and the server behind it
//! decodes each call's program, version, procedure and credential flavour out
//! of the envelope and writes those down too. A mount that asks `mountd` for
//! the NFS program, or holds the mount daemon's connection open, is a
//! perfectly decodable conversation and wrong, and only the log shows it.

use super::*;
use crate::fs::nfs::dialer::*;
use crate::fs::nfs::nfs3;
use crate::fs::nfs::xdr::XdrDecoder;
use alloc::vec;

// --------------------------------------------------------------- portmap

#[test]
fn portmap_is_asked_on_port_111_for_the_two_programs_by_number() {
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let calls = &log.lock().calls;
    let asks: Vec<(u16, u32, u32, u32)> = calls
        .iter()
        .filter(|c| c.program == portmap::PROGRAM_PORTMAP)
        .map(|c| {
            let mut dec = XdrDecoder::new(&c.args);
            let program = dec.u32().unwrap();
            let version = dec.u32().unwrap();
            let protocol = dec.u32().unwrap();
            (c.port, program, version, protocol)
        })
        .collect();
    assert_eq!(
        asks,
        vec![
            (
                portmap::PORTMAP_PORT,
                portmap::PROGRAM_MOUNT,
                mount3::MOUNT_VERSION,
                portmap::IPPROTO_TCP
            ),
            (
                portmap::PORTMAP_PORT,
                portmap::PROGRAM_NFS,
                nfs3::NFS_VERSION,
                portmap::IPPROTO_TCP
            ),
        ],
        "portmap was asked the wrong thing, or on the wrong port"
    );
}

#[test]
fn an_answer_of_zero_is_not_a_port_and_is_never_dialled() {
    // The trap. `GETPORT` answers 0 for a program it has no registration
    // for, and 0 is not a port: dialling it fails somewhere else as
    // something else, which is a mount error that points at the wrong thing.
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(0));
    let log = d.log();

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::EOPNOTSUPP)
    );
    assert!(
        !log.lock().dialled.contains(&0),
        "port zero was dialled: {:?}",
        log.lock().dialled
    );
}

#[test]
fn portmap_is_asked_with_auth_null_and_mountd_with_auth_unix() {
    // Two different programs with two different ideas about who is asking.
    // `rpcbind` does not care -- a port number is not privileged -- and a
    // credential it does not recognise is a refused call for nothing, while
    // `mountd` decides what the export allows for *this* client and some
    // refuse `AUTH_NULL` outright.
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let calls = log.lock();
    for c in calls.calls.iter() {
        let want = if c.program == portmap::PROGRAM_PORTMAP {
            0
        } else {
            1
        };
        assert_eq!(
            c.cred_flavor, want,
            "program {} was asked with flavour {}",
            c.program, c.cred_flavor
        );
    }
}

// ------------------------------------------------------- the ports chosen

#[test]
fn a_port_in_the_options_skips_portmap_entirely() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).unwrap();

    assert_eq!(g.nfs_port, 2049);
    assert_eq!(g.mount_port, MOUNTD_PORT);
    assert_eq!(
        log.lock().dialled,
        vec![MOUNTD_PORT],
        "portmap was asked for a port the mount already knew"
    );
}

#[test]
fn a_portmap_that_cannot_be_reached_falls_back_to_2049_for_nfs() {
    // Deliberate divergence, and the asymmetry is the point: `nfsd` on 2049
    // is near-universal and a growing number of servers run it with no
    // `rpcbind` at all, so a refused portmap must not fail a mount that
    // would have worked.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let opts = NfsOptions {
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).unwrap();

    assert_eq!(g.nfs_port, portmap::NFS_PORT);
}

#[test]
fn a_portmap_that_cannot_be_reached_does_not_guess_a_port_for_mountd() {
    // The other half of the asymmetry. `mountd`'s port is genuinely
    // arbitrary, so a guess at it dials some other service -- and on a
    // machine that has one, sends it a `MNT`.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::ECONNREFUSED)
    );
    // Nothing but portmap: `does not contain mountd's port` would pass a
    // client that guessed some *other* port, which is the mistake.
    assert_eq!(
        log.lock().dialled,
        vec![portmap::PORTMAP_PORT],
        "a port was guessed for mountd"
    );
}

#[test]
fn a_portmap_that_says_nfs_is_not_registered_is_believed_rather_than_guessed() {
    // The difference between "cannot be reached" and "answered no". A
    // registration portmap denies is an answer: it knows, and falling back to
    // 2049 against its word would be dialling a port the server has just
    // said nothing is on.
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, getport(0));
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::EOPNOTSUPP)
    );
    assert!(
        !log.lock().dialled.contains(&portmap::NFS_PORT),
        "2049 was dialled against portmap's word"
    );
}

// ----------------------------------------------------------------- MOUNT

#[test]
fn the_export_path_is_what_goes_to_mountd_and_not_the_whole_source() {
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let calls = log.lock();
    let mnt = calls
        .calls
        .iter()
        .find(|c| c.program == portmap::PROGRAM_MOUNT)
        .expect("mountd was never asked");
    assert_eq!(mnt.procedure, mount3::PROC_MNT);
    assert_eq!(mnt.port, MOUNTD_PORT);
    let mut dec = XdrDecoder::new(&mnt.args);
    assert_eq!(
        core::str::from_utf8(dec.string_bytes().unwrap()).unwrap(),
        "/srv/media"
    );
}

#[test]
fn the_root_handle_is_the_one_the_server_sent() {
    let mut d = a_working_server();
    let g = greet(&mut d, &a_source(), &quick_options()).unwrap();
    assert_eq!(g.root.as_bytes(), ROOT);
}

#[test]
fn the_mount_daemons_connection_does_not_outlive_the_mnt() {
    // `mountd` commonly has very few slots, and a leak here shows up as the
    // *next* mount hanging -- a symptom with nothing to do with the mount
    // that caused it. The greeting ends holding no connection at all: the
    // one the session will use is opened by the session.
    let mut d = a_working_server();
    let log = d.log();
    greet(&mut d, &a_source(), &quick_options()).unwrap();

    let log = log.lock();
    assert_eq!(log.open, 0, "the greeting ended holding a connection");
    assert_eq!(
        log.most_open, 1,
        "two connections were held at once, so one was kept across a step"
    );
}

#[test]
fn a_server_that_lists_only_flavours_this_client_cannot_speak_is_refused_at_mount_time() {
    // The trap that looks like a mount that worked. The export lists
    // `RPCSEC_GSS` and nothing else, so every NFS call afterwards is refused
    // with an auth error -- arriving as `EACCES` on the first `stat`, long
    // after the mount said it was fine.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[6, 390_003]));
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert_eq!(greet(&mut d, &a_source(), &opts), Err(LxError::EACCES));
}

#[test]
fn an_empty_flavour_list_is_not_a_refusal() {
    // It means the server did not say, and plenty do not. Reading it as "no
    // flavours are acceptable" would refuse those servers outright.
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[]));
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).unwrap();
    assert_eq!(g.root.as_bytes(), ROOT);
    assert!(g.auth_flavors.is_empty());
}

#[test]
fn a_list_that_holds_auth_unix_among_others_is_accepted() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[390_003, 1]));
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert!(greet(&mut d, &a_source(), &opts).is_ok());
}

#[test]
fn each_refusal_from_mountd_gets_its_own_errno() {
    // The export path's verdict is the one error a person reading a failed
    // mount can act on: "no such export" and "your address is not allowed"
    // ask for different things, and both as `EIO` would ask for nothing.
    for (status, want) in [
        (2u32, LxError::ENOENT),
        (13, LxError::EACCES),
        (20, LxError::ENOTDIR),
        (63, LxError::ENAMETOOLONG),
        (10_004, LxError::EOPNOTSUPP),
        (1, LxError::EPERM),
    ] {
        let mut d = Dialer::new();
        d.answer(MOUNTD_PORT, mnt_err(status));
        let opts = NfsOptions {
            port: Some(2049),
            mountport: Some(MOUNTD_PORT),
            ..quick_options()
        };
        assert_eq!(
            greet(&mut d, &a_source(), &opts),
            Err(want),
            "MNT3ERR {} came out as the wrong errno",
            status
        );
    }
}

#[test]
fn an_export_path_longer_than_the_protocol_allows_never_reaches_the_wire() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();
    let long = alloc::string::String::from_utf8(vec![b'a'; mount3::MNTPATHLEN + 1]).unwrap();
    let source = NfsSource {
        host: alloc::string::String::from("nas"),
        export: alloc::format!("/{}", long),
    };
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert!(greet(&mut d, &source, &opts).is_err());
    assert!(
        log.lock().calls.is_empty(),
        "a path the protocol refuses was sent anyway"
    );
}

// ----------------------------------------------------------------- UMNT

#[test]
fn umnt_is_the_same_path_on_the_same_port_with_a_different_procedure() {
    let mut d = Dialer::new();
    d.answer(MOUNTD_PORT, umnt_ok());
    let log = d.log();

    unmount_export(&mut d, MOUNTD_PORT, "/srv/media", &quick_options()).unwrap();

    let log = log.lock();
    let call = &log.calls[0];
    assert_eq!(call.port, MOUNTD_PORT);
    assert_eq!(call.program, portmap::PROGRAM_MOUNT);
    assert_eq!(call.version, mount3::MOUNT_VERSION);
    assert_eq!(call.procedure, mount3::PROC_UMNT);
    assert_ne!(mount3::PROC_UMNT, mount3::PROC_MNT);
    let mut dec = XdrDecoder::new(&call.args);
    assert_eq!(
        core::str::from_utf8(dec.string_bytes().unwrap()).unwrap(),
        "/srv/media"
    );
    assert_eq!(log.open, 0, "the UMNT's connection was kept");
}

// ------------------------------------------------------------ the timing

#[test]
fn the_greeting_is_soft_even_on_a_hard_mount() {
    // A `hard` mount means "wait for the server rather than fail the call",
    // and a mount that has not happened yet has no calls to protect: all a
    // hard greeting could do is hold `mount(2)` for ever, on the thread that
    // drives the network stack.
    let mut d = Dialer::new();
    d.silent(MOUNTD_PORT, 8);
    let opts = NfsOptions {
        port: Some(2049),
        mountport: Some(MOUNTD_PORT),
        soft: false,
        ..quick_options()
    };

    let log = d.log();
    assert!(
        greet(&mut d, &a_source(), &opts).is_err(),
        "a hard mount against a silent mountd was supposed to fail, not hang"
    );
    // The outcome alone measures nothing: a `hard` call still ends, at the
    // client's own ten-minute ceiling, so both versions fail and only the
    // *number of calls* says which policy was in force. `retrans: 0` means
    // one call and no retransmission.
    assert_eq!(
        log.lock().calls.len(),
        1,
        "the greeting retransmitted, so it was not soft"
    );
}

// ------------------------------------- what portmap's answer is worth

#[test]
fn a_portmap_that_refuses_the_credential_is_not_a_portmap_that_is_absent() {
    // The fallback to 2049 is for *silence* only. `rpcbind` rejecting the
    // call is `rpcbind` speaking, and dialling 2049 on the strength of it is
    // dialling a port the server never offered -- and then sending NFS calls
    // to whatever answers.
    //
    // This cannot be told from a timeout by errno: both are `EIO`. That is
    // why the failure carries where it came from.
    // `mountport` in the options so the refused call is the one for **NFS**.
    // Without it the fake's denial lands on the `mountd` lookup instead,
    // which has no fallback at all -- so the test would fail for a reason
    // that has nothing to do with the fallback it is about, and pass however
    // the NFS failure were classified.
    let mut d = Dialer::new();
    d.deny_next(portmap::PORTMAP_PORT);
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();
    let opts = NfsOptions {
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let err = greet(&mut d, &a_source(), &opts).expect_err("a refused GETPORT");

    assert_eq!(err, LxError::EACCES);
    assert!(
        !log.lock().dialled.contains(&portmap::NFS_PORT),
        "2049 was dialled although portmap had answered"
    );
}

#[test]
fn a_portmap_whose_reply_does_not_decode_is_not_a_portmap_that_is_absent() {
    // The other way the same mistake arrives: bytes came back and they are
    // nonsense. Still an answer, and still `EIO`, which is why the errno
    // alone cannot gate the fallback.
    let mut d = Dialer::new();
    d.answer(portmap::PORTMAP_PORT, getport(MOUNTD_PORT));
    d.answer(portmap::PORTMAP_PORT, vec![0x00, 0x01]); // a truncated u32
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();

    greet(&mut d, &a_source(), &quick_options()).expect_err("a GETPORT that does not decode");

    assert!(
        !log.lock().dialled.contains(&portmap::NFS_PORT),
        "2049 was dialled on a reply that did not decode"
    );
}

#[test]
fn a_portmap_that_times_out_still_falls_back_because_nothing_answered() {
    // The case the fallback is for, kept beside the two above so narrowing
    // it to nothing would be visible: a reachable port that never replies is
    // silence, and silence says nothing about `nfsd`.
    // `mountport` in the options so the only portmap call is the one for
    // NFS: the fake's silence is per connection, so a silent port 111 would
    // otherwise swallow the `mountd` lookup first and fail the mount there.
    let mut d = Dialer::new();
    d.silent(portmap::PORTMAP_PORT, 1);
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let opts = NfsOptions {
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    let g = greet(&mut d, &a_source(), &opts).expect("a silent portmap failed the mount");

    assert_eq!(g.nfs_port, portmap::NFS_PORT);
}

#[test]
fn a_local_failure_does_not_fall_back_because_nothing_was_asked() {
    // No free local port is this machine's problem, not the server's. It
    // reads like "could not connect" and it is not: the server was never
    // asked, so there is nothing to conclude about `nfsd` -- and 2049 would
    // be no more reachable than 111 was, with the same missing port.
    let mut d = Dialer::new();
    d.refuse_locally(portmap::PORTMAP_PORT, LxError::EADDRNOTAVAIL);
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let log = d.log();
    let opts = NfsOptions {
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert_eq!(
        greet(&mut d, &a_source(), &opts),
        Err(LxError::EADDRNOTAVAIL)
    );
    assert!(
        !log.lock().dialled.contains(&portmap::NFS_PORT),
        "2049 was dialled with no local port to dial it from"
    );
}

#[test]
fn running_out_of_memory_is_a_local_failure_too() {
    // The same, and listed separately because it is the other errno
    // `register_smoltcp_socket` answers: a socket set that is full is not a
    // server that is absent.
    let mut d = Dialer::new();
    d.refuse_locally(portmap::PORTMAP_PORT, LxError::ENOMEM);
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[1]));
    let opts = NfsOptions {
        mountport: Some(MOUNTD_PORT),
        ..quick_options()
    };

    assert_eq!(greet(&mut d, &a_source(), &opts), Err(LxError::ENOMEM));
}

// --------------------------------------------- the flavours, and AUTH_NULL

#[test]
fn an_export_that_lists_only_auth_null_is_refused_at_mount_time() {
    // The check exists to turn a delayed `EACCES` into a mount that fails
    // saying why, and `AUTH_NULL` alone is exactly that case: every NFS call
    // a session makes carries `AUTH_UNIX` (`NfsSession::new` installs it and
    // `set_identity` installs it again; no API picks `AUTH_NULL`), so this
    // export's every call would be refused. Accepting it because this client
    // *can* encode `AUTH_NULL` confuses "can send" with "can mount with".
    let mut d = a_working_server();
    d.clear(MOUNTD_PORT);
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[0]));

    assert_eq!(
        greet(&mut d, &a_source(), &quick_options()),
        Err(LxError::EACCES)
    );
}

#[test]
fn an_export_that_lists_auth_null_and_auth_unix_is_accepted() {
    // The other side: refusing a list *containing* `AUTH_UNIX` because it
    // also offers `AUTH_NULL` would turn away exports that work.
    let mut d = a_working_server();
    d.clear(MOUNTD_PORT);
    d.answer(MOUNTD_PORT, mnt_ok(ROOT, &[0, 1]));

    greet(&mut d, &a_source(), &quick_options()).expect("an export offering AUTH_UNIX was refused");
}

// ------------------------------------------- one deadline for the greeting

#[test]
fn no_step_of_the_greeting_gets_a_fresh_timeout() {
    // The failure: three steps each finishing just inside their own sixty
    // seconds is a `mount(2)` that fails after three minutes, and nothing in
    // the code is wrong by its own measure. What makes it visible is the
    // number each step is *handed*: with one deadline every step gets what
    // is left of it, so no step's allowance is bigger than the one before.
    let mut d = a_working_server();
    let log = d.log();
    let budget = 30_000;

    greet_with_budget(&mut d, &a_source(), &quick_options(), budget).unwrap();

    let budgets = log.lock().budgets.clone();
    assert_eq!(budgets.len(), 3, "the three steps of the greeting");
    for (n, &b) in budgets.iter().enumerate() {
        assert!(
            b <= budget,
            "step {} was handed {} ms of a {} ms budget",
            n,
            b,
            budget
        );
    }
    for pair in budgets.windows(2) {
        assert!(
            pair[1] <= pair[0],
            "a step was handed more time than the one before it: {:?}",
            budgets
        );
    }
}

#[test]
fn a_greeting_with_no_budget_left_dials_nothing() {
    // The deadline is read before each step and not merely passed along, so
    // a greeting that is already out of time stops rather than making a call
    // whose answer it cannot wait for.
    let mut d = a_working_server();
    let log = d.log();

    assert_eq!(
        greet_with_budget(&mut d, &a_source(), &quick_options(), 0),
        Err(LxError::ETIMEDOUT)
    );
    assert!(log.lock().dialled.is_empty(), "a call was made out of time");
}

#[test]
fn the_budget_is_the_greetings_and_not_one_calls_timeout() {
    // `timeo` describes one call; the budget describes the mount. A caller
    // who raised `timeo` above the default asked for more patience per call,
    // which is not a request for a shorter mount -- so their number becomes
    // the floor.
    let mut d = a_working_server();
    let log = d.log();
    let opts = NfsOptions {
        timeout_ms: GREETING_TIMEOUT_MS * 2,
        ..quick_options()
    };

    greet(&mut d, &a_source(), &opts).unwrap();

    let first = log.lock().budgets[0];
    assert!(
        first > GREETING_TIMEOUT_MS,
        "a longer timeo shortened the greeting to {} ms",
        first
    );
}

#[test]
fn the_retries_of_a_step_cannot_outlast_the_budget() {
    // A policy handed the whole remaining budget as its per-call timeout
    // *and* the mount's retransmissions still takes one budget per try. The
    // retransmissions are capped by how many whole per-call timeouts the
    // budget holds.
    let opts = NfsOptions {
        timeout_ms: 1_000,
        retrans: 5,
        ..quick_options()
    };

    // Room for three tries, so two retries.
    assert_eq!(policy(&opts, 3_000).retrans, 2);
    // Room for one try only.
    assert_eq!(policy(&opts, 1_000).retrans, 0);
    // And what the mount asked for, when the budget is wide enough.
    assert_eq!(policy(&opts, 60_000).retrans, 5);
}

#[test]
fn a_step_never_waits_longer_than_the_budget_has_left() {
    // The per-call timeout is the *smaller* of the two. A step handed 200 ms
    // of budget but given the mount's own second would overrun the
    // greeting's deadline by itself, and the deadline would then only be
    // noticed after the step that blew it.
    let opts = NfsOptions {
        timeout_ms: 1_000,
        retrans: 0,
        ..quick_options()
    };

    assert_eq!(policy(&opts, 200).timeout_ms, 200);
    // And not longer than the mount asked for, either.
    assert_eq!(policy(&opts, 60_000).timeout_ms, 1_000);
}
