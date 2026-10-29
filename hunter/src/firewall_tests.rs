//! Tests for the per-application firewall. hunter's state is global, so each
//! test takes the crate's test lock and starts from the boot defaults.

use super::*;
use crate::control;

fn reset() -> impl Drop {
    let g = crate::test_globals::lock();
    policy::reset_for_test();
    reset_for_test();
    g
}

#[test]
fn directives_parse() {
    assert_eq!(
        parse_directive("allow /usr/bin/curl out"),
        Ok(NetDirective::Rule(
            Action::Allow,
            String::from("/usr/bin/curl"),
            Some(Direction::Out)
        ))
    );
    assert_eq!(
        parse_directive("deny /usr/bin/nc"),
        Ok(NetDirective::Rule(
            Action::Deny,
            String::from("/usr/bin/nc"),
            None
        ))
    );
    assert_eq!(
        parse_directive("allow /opt/My App/run in"),
        Ok(NetDirective::Rule(
            Action::Allow,
            String::from("/opt/My App/run"),
            Some(Direction::In)
        ))
    );
    assert_eq!(
        parse_directive("allow /opt/My App/run"),
        Ok(NetDirective::Rule(
            Action::Allow,
            String::from("/opt/My App/run"),
            None
        ))
    );
    assert_eq!(parse_directive("timeout 10"), Ok(NetDirective::Timeout(10)));
    assert_eq!(
        parse_directive("mode enforce"),
        Ok(NetDirective::Mode(Mode::Enforce))
    );
    assert!(parse_directive("mode strict").is_err());
    assert!(parse_directive("timeout 0").is_err());
    assert!(parse_directive("timeout 9999").is_err());
    assert!(parse_directive("allow curl").is_err());
    assert!(parse_directive("allow").is_err());
    assert!(parse_directive("permit /bin/x").is_err());
    assert_eq!(parse_rules_line("  # comment"), Ok(None));
    assert_eq!(parse_rules_line(""), Ok(None));
}

#[test]
fn the_control_language_speaks_net() {
    let _g = reset();
    control::apply_script(
        "mode net enforce\nnet allow /usr/bin/curl out  # web\nnet timeout 5\n",
        "test",
        0,
    )
    .unwrap();
    assert_eq!(net_mode(), Mode::Enforce);
    assert_eq!(ask_timeout_secs(), 5);
    assert_eq!(rule("/usr/bin/curl", Direction::Out), Some(Action::Allow));
    assert_eq!(rule("/usr/bin/curl", Direction::In), None);
    // One bad `net` line rejects the whole script, as any other directive.
    assert!(control::apply_script("net deny /bin/nc\nnet frobnicate /x\n", "test", 0).is_err());
    assert_eq!(rule("/bin/nc", Direction::Out), None);
}

#[test]
fn report_mode_allows_and_asks_once() {
    let _g = reset();
    assert_eq!(
        check(7, "/usr/bin/wget", Direction::Out, "1.2.3.4:443", false),
        Verdict::Allow
    );
    assert_eq!(
        check(8, "/usr/bin/./wget", Direction::Out, "5.6.7.8:80", false),
        Verdict::Allow
    );
    let p = pending();
    assert_eq!(p.len(), 1, "the same program and direction is one question");
    assert_eq!(p[0].program, "/usr/bin/wget");
    assert_eq!(p[0].count, 2);
    assert_eq!(p[0].pid, 8);
    assert_eq!(p[0].peer, "5.6.7.8:80");
    // The other direction is another question.
    check(8, "/usr/bin/wget", Direction::In, "0.0.0.0:8080", false);
    assert_eq!(pending().len(), 2);
}

#[test]
fn local_traffic_and_off_are_never_asked() {
    let _g = reset();
    set_net_mode(Mode::Enforce);
    assert_eq!(
        check(1, "/bin/x", Direction::Out, "127.0.0.1:6000", true),
        Verdict::Allow
    );
    assert_eq!(
        check(1, "", Direction::Out, "1.1.1.1:53", false),
        Verdict::Allow
    );
    set_net_mode(Mode::Off);
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/x"),
        None,
    ));
    assert_eq!(
        check(1, "/bin/x", Direction::Out, "1.1.1.1:53", false),
        Verdict::Allow
    );
    assert!(pending().is_empty());
}

#[test]
fn rules_decide_and_answer_the_question() {
    let _g = reset();
    set_net_mode(Mode::Enforce);
    assert_eq!(
        check(3, "/usr/bin/curl", Direction::Out, "9.9.9.9:443", false),
        Verdict::Ask
    );
    assert_eq!(decided("/usr/bin/curl", Direction::Out), None);
    control::apply_script("net allow /usr/bin/curl", "lunarhunter", 0).unwrap();
    assert_eq!(decided("/usr/bin/curl", Direction::Out), Some(true));
    assert!(pending().is_empty(), "the answer closes the question");
    assert_eq!(
        check(3, "/usr/bin/curl", Direction::In, "0.0.0.0:80", false),
        Verdict::Allow,
        "a rule without a direction covers both"
    );

    check(4, "/usr/bin/nc", Direction::Out, "6.6.6.6:4444", false);
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/usr/bin/nc"),
        Some(Direction::Out),
    ));
    assert_eq!(decided("/usr/bin/nc", Direction::Out), Some(false));
    assert_eq!(
        check(4, "/usr/bin/nc", Direction::Out, "6.6.6.6:4444", false),
        Verdict::Deny
    );
}

#[test]
fn a_deny_rule_blocks_in_report_mode_too() {
    let _g = reset();
    assert_eq!(net_mode(), Mode::Report);
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/usr/bin/nc"),
        None,
    ));
    assert_eq!(
        check(4, "/usr/bin/nc", Direction::Out, "6.6.6.6:4444", false),
        Verdict::Deny
    );
}

#[test]
fn turning_enforce_off_releases_a_waiting_call() {
    let _g = reset();
    set_net_mode(Mode::Enforce);
    assert_eq!(
        check(3, "/bin/a", Direction::Out, "9.9.9.9:443", false),
        Verdict::Ask
    );
    set_net_mode(Mode::Report);
    assert_eq!(decided("/bin/a", Direction::Out), Some(true));
}

#[test]
fn dismiss_and_forget() {
    let _g = reset();
    check(3, "/bin/a", Direction::Out, "9.9.9.9:443", false);
    apply(&NetDirective::Dismiss(String::from("/bin/a")));
    assert!(pending().is_empty());
    assert_eq!(
        rule("/bin/a", Direction::Out),
        None,
        "dismiss decides nothing"
    );

    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/bin/a"),
        None,
    ));
    apply(&NetDirective::Forget(String::from("/bin/a")));
    assert_eq!(rule("/bin/a", Direction::Out), None);
    assert_eq!(rule_count(), 0);
}

#[test]
fn the_latch_refuses_allow_and_forgetting_a_deny() {
    let _g = reset();
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/d"),
        None,
    ));
    policy::seal_tighten_only();
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/bin/a"),
        None,
    ));
    assert_eq!(rule("/bin/a", Direction::Out), None);
    apply(&NetDirective::Forget(String::from("/bin/d")));
    assert_eq!(rule("/bin/d", Direction::Out), Some(Action::Deny));
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/e"),
        None,
    ));
    assert_eq!(rule("/bin/e", Direction::In), Some(Action::Deny));
    set_net_mode(Mode::Enforce);
    set_net_mode(Mode::Off);
    assert_eq!(net_mode(), Mode::Enforce);
}

#[test]
fn the_pending_list_is_bounded() {
    let _g = reset();
    for i in 0..(MAX_PENDING + 5) {
        check(
            1,
            &format!("/bin/p{}", i),
            Direction::Out,
            "1.1.1.1:1",
            false,
        );
    }
    assert_eq!(pending().len(), MAX_PENDING);
}

#[test]
fn the_proc_file_lists_rules_and_questions() {
    let _g = reset();
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/usr/bin/curl"),
        None,
    ));
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/opt/My App/srv"),
        Some(Direction::In),
    ));
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/opt/My App/srv"),
        Some(Direction::Out),
    ));
    check(
        42,
        "/usr/bin/wget",
        Direction::Out,
        "93.184.216.34:443",
        false,
    );
    let text = render();
    assert!(text.starts_with("mode report\ntimeout 30\nnow "));
    assert!(text.contains("rule allow both /usr/bin/curl\n"));
    assert!(text.contains("rule allow out /opt/My App/srv\n"));
    assert!(text.contains("rule deny in /opt/My App/srv\n"));
    assert!(text.contains("pending out 1 42 0 0 93.184.216.34:443 /usr/bin/wget\n"));
}
