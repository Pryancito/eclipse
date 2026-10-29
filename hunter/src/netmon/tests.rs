//! Each test names a listener the monitor must (or must not) shout about.

use super::*;
use crate::event_log;
use alloc::vec;

fn reset() -> std::sync::MutexGuard<'static, ()> {
    let g = crate::test_globals::lock();
    policy::reset_for_test();
    event_log::reset_for_test();
    reset_for_test();
    g
}

extern crate std;

fn sock(pid: u64, program: &str, proto: Proto, state: State, addr: &str, port: u16) -> Sock {
    Sock {
        pid,
        program: String::from(program),
        proto,
        v6: addr.contains(':'),
        state,
        local_addr: String::from(addr),
        local_port: port,
        remote: None,
    }
}

fn net_alerts() -> usize {
    event_log::render().matches("unexpected listener").count()
}

#[test]
fn a_backdoor_listener_is_reported_once_by_name() {
    let _g = reset();
    let nc = sock(
        42,
        "/usr/bin/nc",
        Proto::Tcp,
        State::Listen,
        "0.0.0.0",
        4444,
    );
    observe(vec![nc.clone()]);
    let log = event_log::render();
    assert!(log.contains("unexpected listener: tcp 0.0.0.0:4444 by /usr/bin/nc"));
    assert!(log.contains("WARN"));
    // Reading the table again is not a second event...
    observe(vec![nc.clone()]);
    on_listen(&nc);
    assert_eq!(net_alerts(), 1);
    // ...but the same program listening again after it closed is.
    observe(vec![]);
    observe(vec![nc]);
    assert_eq!(net_alerts(), 2);
}

#[test]
fn clients_and_loopback_are_not_listeners_to_report() {
    let _g = reset();
    let mut estab = sock(
        7,
        "/bin/curl",
        Proto::Tcp,
        State::Established,
        "10.0.2.15",
        50000,
    );
    estab.remote = Some((String::from("93.184.216.34"), 443));
    observe(vec![
        estab,
        // A DNS query's ephemeral port.
        sock(
            7,
            "/bin/curl",
            Proto::Udp,
            State::Unconnected,
            "0.0.0.0",
            51234,
        ),
        // A local-only resolver.
        sock(
            9,
            "/sbin/resolv",
            Proto::Udp,
            State::Unconnected,
            "127.0.0.1",
            53,
        ),
        sock(9, "/sbin/resolv", Proto::Tcp, State::Listen, "::1", 53),
        // The DHCP client, expected out of the box.
        sock(
            3,
            "/sbin/edhcpc",
            Proto::Udp,
            State::Unconnected,
            "0.0.0.0",
            68,
        ),
    ]);
    assert_eq!(net_alerts(), 0);
    // Loopback stops being exempt when the operator says so.
    apply(&Directive::IgnoreLoopback(false), 0);
    observe(vec![sock(
        9,
        "/sbin/resolv",
        Proto::Tcp,
        State::Listen,
        "::1",
        53,
    )]);
    assert_eq!(net_alerts(), 1);
}

#[test]
fn a_udp_service_port_is_a_listener() {
    let _g = reset();
    observe(vec![sock(
        5,
        "/tmp/x",
        Proto::Udp,
        State::Unconnected,
        "0.0.0.0",
        53,
    )]);
    assert!(event_log::render().contains("unexpected listener: udp 0.0.0.0:53 by /tmp/x"));
}

#[test]
fn a_rule_naming_a_program_does_not_cover_another() {
    let _g = reset();
    apply_script(
        "expect tcp 22 /usr/sbin/sshd\nexpect any 8000-8099\n",
        "test",
        0,
    )
    .unwrap();
    observe(vec![
        sock(
            10,
            "/usr/sbin/sshd",
            Proto::Tcp,
            State::Listen,
            "0.0.0.0",
            22,
        ),
        sock(
            11,
            "/usr/bin/python3",
            Proto::Tcp,
            State::Listen,
            "::",
            8080,
        ),
    ]);
    assert_eq!(net_alerts(), 0);
    observe(vec![sock(
        12,
        "/bin/sh",
        Proto::Tcp,
        State::Listen,
        "0.0.0.0",
        22,
    )]);
    assert!(event_log::render().contains("tcp 0.0.0.0:22 by /bin/sh"));
}

#[test]
fn forget_takes_a_rule_back() {
    let _g = reset();
    apply_script("expect tcp 22", "test", 0).unwrap();
    apply_script("forget tcp 22", "test", 0).unwrap();
    assert_eq!(rules(), default_rules());
    observe(vec![sock(
        1,
        "/usr/sbin/sshd",
        Proto::Tcp,
        State::Listen,
        "0.0.0.0",
        22,
    )]);
    assert_eq!(net_alerts(), 1);
}

#[test]
fn alerts_off_is_quiet_and_the_table_still_says_alert() {
    let _g = reset();
    apply_script("alerts off", "test", 0).unwrap();
    observe(vec![sock(
        42,
        "/usr/bin/nc",
        Proto::Tcp,
        State::Listen,
        "0.0.0.0",
        4444,
    )]);
    assert_eq!(net_alerts(), 0);
    let table = render();
    assert!(table.contains("alerts=off"));
    assert!(table.contains("sock 42 tcp LISTEN 0.0.0.0:4444 - ALERT /usr/bin/nc"));
}

#[test]
fn a_bad_script_applies_nothing() {
    let _g = reset();
    for bad in [
        "expect tcp 22\nexpect tcp 0",
        "expect tcp 22\nexpect sctp 22",
        "expect tcp 90-80",
        "expect tcp 22 sshd",
        "alerts maybe",
        "listen tcp 22",
        "expect tcp",
    ] {
        assert!(apply_script(bad, "test", 0).is_err(), "{}", bad);
    }
    assert_eq!(rules(), default_rules());
    assert!(event_log::render().contains("REJECTED"));
}

#[test]
fn sealed_hunter_refuses_to_silence_alerts() {
    let _g = reset();
    policy::seal_tighten_only();
    apply_script("expect tcp 4444\nalerts off\nignore-loopback on", "test", 0).unwrap();
    assert_eq!(rules(), default_rules());
    assert!(alerts_enabled());
    // Tightening still goes through.
    apply_script("ignore-loopback off\nforget udp 68", "test", 0).unwrap();
    assert!(!ignore_loopback());
    assert_eq!(rules().len(), 1);
    assert!(event_log::render().contains("refused relaxing netmon change"));
    policy::reset_for_test();
}

#[test]
fn the_rules_block_round_trips_through_the_language() {
    let _g = reset();
    let script = "expect tcp 22 /usr/sbin/sshd\nexpect any 6000-6010\n";
    apply_script(script, "test", 0).unwrap();
    let table = render();
    let rules_block: String = table
        .lines()
        .filter(|l| l.starts_with("expect "))
        .map(|l| format!("{}\n", l))
        .collect();
    reset_for_test();
    RULES.lock().clear();
    apply_script(&rules_block, "test", 0).unwrap();
    let mut want = default_rules();
    want.extend(parse_script(script).unwrap().into_iter().map(|d| match d {
        Directive::Expect(r) => r,
        _ => unreachable!(),
    }));
    assert_eq!(rules(), want);
}

#[test]
fn v6_addresses_keep_their_port_apart() {
    assert_eq!(join_addr("::", 22), "[::]:22");
    assert_eq!(join_addr("10.0.2.15", 22), "10.0.2.15:22");
    let mut s = sock(1, "/x", Proto::Tcp, State::Established, "fe80::1", 50000);
    s.remote = Some((String::from("2001:db8::1"), 443));
    let _g = reset();
    observe(vec![s]);
    assert!(render().contains("sock 1 tcp6 ESTAB [fe80::1]:50000 [2001:db8::1]:443 ok /x"));
}
