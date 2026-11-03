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

// ---------------------------------------------------------------------------
// What the firewall TELLS the person, and the bookkeeping behind the list of
// questions. The tests above decide verdicts; a wide mutation lot over
// `firewall.rs` showed that none of them looks at the event a decision raises,
// at which question gets evicted when the list fills, or at the edges of the
// control language -- 24 of 54 mutations passed the suite untouched. These are
// those 24.
// ---------------------------------------------------------------------------

/// A clock a test can move. `clock::set_time_source` takes a plain `fn`
/// pointer, which cannot capture, so the reading lives in a static the
/// function reads.
static FAKE_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn fake_clock() -> u64 {
    FAKE_NS.load(Ordering::SeqCst)
}

/// Moves the test clock to `s` seconds since boot.
fn at_second(s: u64) {
    FAKE_NS.store(s * 1_000_000_000, Ordering::SeqCst);
}

/// Puts the clock back when the test ends, before releasing the crate lock.
struct Logged<G>(G);

impl<G> Drop for Logged<G> {
    fn drop(&mut self) {
        clock::reset_for_test();
    }
}

/// [`reset`], plus an empty event ring and a clock the test can move with
/// [`at_second`].
///
/// hunter's clock is sealed on its first registration -- an attacker who
/// reaches the mutator must not be able to stop the clock and silence
/// detection -- so a test that wants a clock of its own has to break the seal,
/// which is the whole reason `clock::reset_for_test` exists. Dropping this
/// hands the seal back empty, the way `event_log`'s own clock tests leave it:
/// the tests above read `now 0` and would stop doing so otherwise.
fn reset_logged() -> impl Drop {
    let g = reset();
    crate::event_log::reset_for_test();
    at_second(0);
    clock::reset_for_test();
    clock::set_time_source(fake_clock);
    Logged(g)
}

#[test]
fn a_question_reaches_the_desktop_as_a_warning_and_only_once() {
    let _g = reset_logged();
    // `Warning` and not `Notice` because the desktop shows warnings --
    // lunarbar's alerts, the notifications -- and a question nobody is shown
    // is a firewall that silently stops asking. This exact regression has
    // happened once already.
    for _ in 0..5 {
        assert_eq!(
            check(7, "/bin/q", Direction::Out, "1.2.3.4:443", false),
            Verdict::Allow
        );
    }
    let log = crate::event_log::render();
    assert!(
        log.contains(
            "firewall: /bin/q wants to connect to 1.2.3.4:443 (no rule; allowed in report mode)"
        ),
        "the question was not recorded:\n{}",
        log
    );
    // Once per program and direction, however many attempts are behind it:
    // five connections in a loop must not be five lines in the desktop's
    // notifications.
    assert_eq!(
        log.matches("firewall: /bin/q wants").count(),
        1,
        "the question was raised more than once:\n{}",
        log
    );
    assert_eq!(
        crate::event_log::stats().warnings,
        1,
        "the question did not reach the desktop as a warning:\n{}",
        log
    );
    assert_eq!(pending().len(), 1);
    assert_eq!(pending()[0].count, 5);
}

#[test]
fn the_question_says_which_way_the_program_is_going() {
    let _g = reset_logged();
    check(7, "/bin/out", Direction::Out, "1.2.3.4:443", false);
    check(7, "/bin/in", Direction::In, "0.0.0.0:8080", false);
    let log = crate::event_log::render();
    // "wants to connect to" and "wants to listen on" are the two halves of
    // what the person is being asked to decide; told the wrong way round, the
    // dialog asks about the opposite of what the program is doing.
    assert!(
        log.contains("firewall: /bin/out wants to connect to 1.2.3.4:443"),
        "{}",
        log
    );
    assert!(
        log.contains("firewall: /bin/in wants to listen on 0.0.0.0:8080"),
        "{}",
        log
    );
}

#[test]
fn blocks_under_a_rule_are_logged_at_one_two_four_and_eight() {
    let _g = reset_logged();
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/b"),
        Some(Direction::Out),
    ));
    for _ in 0..8 {
        assert_eq!(
            check(3, "/bin/b", Direction::Out, "9.9.9.9:53", false),
            Verdict::Deny
        );
    }
    let log = crate::event_log::render();
    // A program retrying in a loop must not fill the ring: an event at 1, 2,
    // 4 and 8 blocks and at nothing in between. Without the brake, the 512
    // entries of the general log are gone in half a second and every other
    // domain's events go with them.
    assert_eq!(
        log.matches("firewall rule: /bin/b out").count(),
        4,
        "eight blocks did not produce four events:\n{}",
        log
    );
    assert!(log.contains("9.9.9.9:53 (1 time)"), "{}", log);
    assert!(log.contains("9.9.9.9:53 (2 times)"), "{}", log);
    assert!(log.contains("9.9.9.9:53 (4 times)"), "{}", log);
    assert!(log.contains("9.9.9.9:53 (8 times)"), "{}", log);
}

#[test]
fn changing_a_rule_starts_the_block_count_again() {
    let _g = reset_logged();
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/c"),
        Some(Direction::Out),
    ));
    for _ in 0..2 {
        check(3, "/bin/c", Direction::Out, "9.9.9.9:53", false);
    }
    // The person changes their mind twice. The count behind the brake belongs
    // to the rule, so the next block after a change is the first one again --
    // otherwise a program denied, allowed and denied again is already past
    // the first powers of two and its blocks go unrecorded.
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/bin/c"),
        Some(Direction::Out),
    ));
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/c"),
        Some(Direction::Out),
    ));
    check(3, "/bin/c", Direction::Out, "9.9.9.9:53", false);
    let log = crate::event_log::render();
    assert_eq!(
        log.matches("firewall rule: /bin/c out 9.9.9.9:53 (1 time)")
            .count(),
        2,
        "the block count was not reset with the rule:\n{}",
        log
    );
}

#[test]
fn the_question_list_evicts_the_one_nobody_has_asked_about_lately() {
    let _g = reset_logged();
    // One question per second, so first-asked order and last-asked order
    // start out the same.
    for i in 0..MAX_PENDING {
        at_second(1 + i as u64);
        check(
            9,
            &format!("/bin/p{:02}", i),
            Direction::Out,
            "1.2.3.4:443",
            false,
        );
    }
    assert_eq!(pending().len(), MAX_PENDING);
    // Now the two orders disagree: the OLDEST question is the one being
    // retried, and the one nobody has repeated since is `/bin/p01`.
    at_second(100);
    check(9, "/bin/p00", Direction::Out, "1.2.3.4:443", false);
    at_second(101);
    check(9, "/bin/zz", Direction::Out, "5.6.7.8:80", false);

    let list = pending();
    let names: Vec<&str> = list.iter().map(|p| p.program.as_str()).collect();
    assert_eq!(list.len(), MAX_PENDING);
    assert!(
        names.contains(&"/bin/zz"),
        "the new question was not added: {:?}",
        names
    );
    // Evicting the most recently asked throws away the question the person is
    // most likely to be looking at, and under `Enforce` the one a call is
    // waiting on.
    assert!(
        names.contains(&"/bin/p00"),
        "the question being retried was evicted: {:?}",
        names
    );
    assert!(
        !names.contains(&"/bin/p01"),
        "the question nobody has repeated survived: {:?}",
        names
    );
    // Refreshing a question moves its LAST attempt, not its first: the pair
    // is what tells the person how long a program has been trying.
    let p00 = list.iter().find(|p| p.program == "/bin/p00").unwrap();
    assert_eq!((p00.first, p00.last, p00.count), (1, 100, 2));
    // And the same three numbers in `/proc/hunter_firewall`, where `now` is
    // seconds since boot -- not milliseconds, which would make every age in
    // the file a thousand times too large.
    let proc = render();
    assert!(proc.contains("\nnow 101\n"), "{}", proc);
    assert!(
        proc.contains("pending out 2 9 1 100 1.2.3.4:443 /bin/p00\n"),
        "{}",
        proc
    );
}

#[test]
fn an_answer_in_one_direction_leaves_the_other_question_open() {
    let _g = reset_logged();
    set_net_mode(Mode::Enforce);
    assert_eq!(
        check(5, "/bin/d", Direction::Out, "1.1.1.1:80", false),
        Verdict::Ask
    );
    assert_eq!(
        check(5, "/bin/d", Direction::In, "0.0.0.0:80", false),
        Verdict::Ask
    );
    assert_eq!(pending().len(), 2);
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/bin/d"),
        Some(Direction::Out),
    ));
    // Answering one direction answers one question. Clearing both would make
    // the person's "yes, it may connect" also dismiss "and may it listen?",
    // which they were never shown.
    let list = pending();
    assert_eq!(list.len(), 1, "both questions were closed by one answer");
    assert_eq!(list[0].dir, Direction::In);
    // And the call still waiting on the `in` question must not be released by
    // the `out` answer.
    assert_eq!(decided("/bin/d", Direction::Out), Some(true));
    assert_eq!(
        decided("/bin/d", Direction::In),
        None,
        "the waiting bind was answered by the connect rule"
    );
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/d"),
        Some(Direction::In),
    ));
    assert_eq!(decided("/bin/d", Direction::In), Some(false));
    assert!(pending().is_empty());
}

#[test]
fn the_rule_list_is_capped_and_still_lets_a_rule_be_tightened() {
    let _g = reset_logged();
    for i in 0..MAX_RULES {
        apply(&NetDirective::Rule(
            Action::Allow,
            format!("/bin/r{}", i),
            Some(Direction::Out),
        ));
    }
    assert_eq!(rule_count(), MAX_RULES);
    // The cap bounds kernel memory against a runaway rules file, so the
    // number itself is the contract: 4096 rules at a path each.
    assert_eq!(MAX_RULES, 4096);
    assert_eq!(MAX_PENDING, 32);
    crate::event_log::reset_for_test();
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/bin/over"),
        Some(Direction::Out),
    ));
    assert_eq!(rule("/bin/over", Direction::Out), None);
    assert_eq!(rule_count(), MAX_RULES);
    let log = crate::event_log::render();
    assert!(
        log.contains("firewall rule list full (4096); not added: /bin/over"),
        "a refused rule said nothing:\n{}",
        log
    );
    // A full list must not freeze the rules already in it: the person can
    // still turn an `allow` into a `deny`, which is the change that matters.
    apply(&NetDirective::Rule(
        Action::Deny,
        String::from("/bin/r0"),
        Some(Direction::Out),
    ));
    assert_eq!(
        rule("/bin/r0", Direction::Out),
        Some(Action::Deny),
        "a full rule list stopped an existing rule from being tightened"
    );
}

#[test]
fn the_control_language_refuses_what_is_not_a_program_path() {
    let _g = reset();
    // Absolute, not merely containing a slash: a rule for `usr/bin/curl` is a
    // rule for nothing, and it would sit in the file looking like one.
    assert!(parse_directive("allow usr/bin/curl out").is_err());
    assert!(parse_directive("deny ../bin/sh").is_err());
    assert!(parse_directive("forget bin/sh").is_err());
    // `both` is the two directions, which is what no direction means.
    assert_eq!(
        parse_directive("allow /bin/x both"),
        Ok(NetDirective::Rule(
            Action::Allow,
            String::from("/bin/x"),
            None
        ))
    );
    // The longest wait an operator may set is 300 seconds, inclusive.
    assert_eq!(
        parse_directive("timeout 300"),
        Ok(NetDirective::Timeout(300))
    );
    assert!(parse_directive("timeout 301").is_err());
    // A comment is a line that STARTS with a hash. A program whose path
    // contains one is a program, not a comment.
    assert_eq!(parse_rules_line("# allow /bin/x out"), Ok(None));
    assert_eq!(
        parse_rules_line("allow /opt/c#/run out"),
        Ok(Some(NetDirective::Rule(
            Action::Allow,
            String::from("/opt/c#/run"),
            Some(Direction::Out)
        )))
    );
}

#[test]
fn the_timeout_is_announced_when_it_changes_and_not_otherwise() {
    let _g = reset_logged();
    apply(&NetDirective::Timeout(10));
    assert_eq!(ask_timeout_secs(), 10);
    assert_eq!(
        crate::event_log::render().matches("net timeout 10").count(),
        1,
        "the new timeout was not announced"
    );
    apply(&NetDirective::Timeout(10));
    assert_eq!(
        crate::event_log::render().matches("net timeout 10").count(),
        1,
        "the same timeout was announced twice"
    );
}

#[test]
fn forgetting_a_rule_that_only_allows_is_a_tightening_and_outlives_the_latch() {
    let _g = reset();
    apply(&NetDirective::Rule(
        Action::Allow,
        String::from("/bin/f"),
        None,
    ));
    policy::seal_tighten_only();
    // Dropping an `allow` puts the program back on the list of questions,
    // which is the tighter state: the latch has nothing to refuse. Only
    // forgetting a `deny` relaxes, and that one is gated above.
    apply(&NetDirective::Forget(String::from("/bin/f")));
    assert_eq!(rule("/bin/f", Direction::Out), None);
    assert_eq!(rule("/bin/f", Direction::In), None);
    assert_eq!(rule_count(), 0);
}
