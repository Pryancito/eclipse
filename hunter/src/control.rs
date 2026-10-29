//! The control plane: one small directive language that both
//! `/etc/hunter/policy.conf` (read once at boot) and a write to `/proc/hunter`
//! (by root, at any time) speak.
//!
//! Before this module every knob in [`policy`] and [`heuristics`] was a Rust
//! function, and the kernel called exactly none of the ones that tighten:
//! nothing ever raised a domain to `Enforce` and nothing ever engaged the
//! tighten-only latch, so a booted system ran every domain but `syscall` in
//! audit mode for good, and an operator had no way to change that short of
//! rebuilding the kernel. This is that way.
//!
//! ## Language
//!
//! One directive per line; blank lines and `#` comments (whole-line, or
//! trailing after whitespace) are ignored.
//!
//! | Directive                               | Effect                                        |
//! |-----------------------------------------|-----------------------------------------------|
//! | `mode <domain> <off\|report\|enforce>`  | domain is `syscall`, `wx`, `exec`, `anomaly`, `integrity`, `av` |
//! | `learning <on\|off>`                    | exec trust-on-first-use                       |
//! | `anomaly-detection <on\|off>`           | per-process flood / fork-bomb heuristics      |
//! | `privileged-deny <on\|off>`             | block module/ptrace/bpf/... under `anomaly=enforce` |
//! | `trust <path>` / `trust <dir/>`         | add to the exec allowlist                     |
//! | `deny <path>` / `deny <dir/>`           | add to the exec blacklist                     |
//! | `untrusted <dir/>`                      | add a world-writable location                 |
//! | `hash <sha256> <path>`                  | pin a program's SHA-256 (see [`crate::integrity`]) |
//! | `require-hash <on\|off>`                | an unpinned program is an integrity violation |
//! | `signature <hash\|bytes>:<data>:<Name>` | add an antivirus signature (see [`crate::av`]) |
//! | `feed-key <name> <hex>`                 | trust an Ed25519 key for signed feeds (see [`crate::feed`]) |
//! | `require-signed-feeds <on\|off>`        | refuse signature feeds that are not signed    |
//! | `seal`                                  | engage the one-way tighten-only latch         |
//!
//! A trailing `/` makes `trust` / `deny` a directory prefix, exactly as in
//! `/etc/hunter/{whitelist,blacklist}`.
//!
//! ## All or nothing
//!
//! A script is parsed in full before anything is applied, and a single bad
//! line rejects the whole of it. Applying the lines that parse sounds kinder
//! and is not: the lines of a policy depend on each other (`mode exec enforce`
//! means something quite different without the `learning off` two lines
//! down), and a script half applied is a stance nobody wrote. The rejection is
//! recorded as a `Critical` event naming the line, so a typo in `policy.conf`
//! is loud at boot instead of silently leaving the defaults in place.
//!
//! ## The latch
//!
//! Directives run in order, so a `seal` takes effect for the lines after it.
//! Every relaxing directive goes through the same tighten-only checks the
//! [`policy`] setters already make, and this module adds the two the
//! heuristics setters do not: `anomaly-detection off` and `privileged-deny off`
//! are refused once sealed.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::av;
use crate::event_log::{record, Severity};
use crate::feed;
use crate::heuristics;
use crate::integrity::{self, Digest};
use crate::policy::{self, Mode};

/// An enforcement domain a `mode` directive can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Syscall,
    Wx,
    Exec,
    Anomaly,
    Integrity,
    Av,
}

/// One parsed line of the control language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    Mode(Domain, Mode),
    Learning(bool),
    AnomalyDetection(bool),
    PrivilegedDeny(bool),
    Trust(String),
    Deny(String),
    Untrusted(String),
    Hash(Digest, String),
    RequireHash(bool),
    Signature(av::Signature),
    FeedKey(String, feed::PublicKey),
    RequireSignedFeeds(bool),
    Seal,
}

/// Why a script was rejected: the 1-based line and what was wrong with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

/// Parses one line. `Ok(None)` for a blank line or a comment.
pub fn parse_line(line: &str) -> Result<Option<Directive>, String> {
    let line = strip_comment(line).trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let arity = |n: usize| -> Result<(), String> {
        if words.len() == n {
            Ok(())
        } else {
            Err(format!(
                "'{}' takes {} argument{}",
                words[0],
                n - 1,
                if n == 2 { "" } else { "s" }
            ))
        }
    };
    let directive = match words[0] {
        "mode" => {
            arity(3)?;
            Directive::Mode(parse_domain(words[1])?, parse_mode(words[2])?)
        }
        "learning" => {
            arity(2)?;
            Directive::Learning(parse_switch(words[1])?)
        }
        "anomaly-detection" => {
            arity(2)?;
            Directive::AnomalyDetection(parse_switch(words[1])?)
        }
        "privileged-deny" => {
            arity(2)?;
            Directive::PrivilegedDeny(parse_switch(words[1])?)
        }
        "trust" => {
            arity(2)?;
            Directive::Trust(parse_path(words[1])?)
        }
        "deny" => {
            arity(2)?;
            Directive::Deny(parse_path(words[1])?)
        }
        "untrusted" => {
            arity(2)?;
            let p = parse_path(words[1])?;
            if !p.ends_with('/') {
                return Err(format!(
                    "'untrusted' takes a directory ending in '/': {}",
                    p
                ));
            }
            Directive::Untrusted(p)
        }
        "hash" => {
            // The rest of the line is one `sha256sum` line, so a path with a
            // space in it pins the same here as in `/etc/hunter/hashes`.
            let rest = line["hash".len()..].trim_start();
            match integrity::parse_sums_line(rest)? {
                Some((digest, path)) => Directive::Hash(digest, path),
                None => return Err(String::from("'hash' takes '<sha256> <path>'")),
            }
        }
        "signature" => {
            // The rest of the line is one `/etc/hunter/signatures` line.
            let rest = line["signature".len()..].trim_start();
            match av::parse_signature(rest)? {
                Some(sig) => Directive::Signature(sig),
                None => {
                    return Err(String::from(
                        "'signature' takes '<hash|bytes>:<data>:<Name>'",
                    ))
                }
            }
        }
        "require-hash" => {
            arity(2)?;
            Directive::RequireHash(parse_switch(words[1])?)
        }
        "feed-key" => {
            arity(3)?;
            if !feed::valid_name(words[1]) {
                return Err(format!("bad key name '{}'", words[1]));
            }
            // An Ed25519 public key is 32 bytes: 64 hex digits, the same
            // spelling as a SHA-256.
            let key = integrity::parse_hex(words[2])
                .ok_or_else(|| format!("not a 64-hex-digit Ed25519 key: {}", words[2]))?;
            Directive::FeedKey(String::from(words[1]), key)
        }
        "require-signed-feeds" => {
            arity(2)?;
            Directive::RequireSignedFeeds(parse_switch(words[1])?)
        }
        "seal" => {
            arity(1)?;
            Directive::Seal
        }
        other => return Err(format!("unknown directive '{}'", other)),
    };
    Ok(Some(directive))
}

/// Cuts a trailing comment: a `#` at the start of the line or after
/// whitespace, so `mode wx report  # JITs` is `mode wx report` while a `#`
/// inside a word (a path) is kept. The shipped example annotates its lines
/// this way, and uncommenting one of them must give a line that parses.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

fn parse_domain(word: &str) -> Result<Domain, String> {
    match word {
        "syscall" => Ok(Domain::Syscall),
        "wx" => Ok(Domain::Wx),
        "exec" => Ok(Domain::Exec),
        "anomaly" => Ok(Domain::Anomaly),
        "integrity" => Ok(Domain::Integrity),
        "av" => Ok(Domain::Av),
        other => Err(format!(
            "unknown domain '{}' (syscall, wx, exec, anomaly, integrity, av)",
            other
        )),
    }
}

fn parse_mode(word: &str) -> Result<Mode, String> {
    match word {
        "off" => Ok(Mode::Off),
        "report" => Ok(Mode::Report),
        "enforce" => Ok(Mode::Enforce),
        other => Err(format!("unknown mode '{}' (off, report, enforce)", other)),
    }
}

fn parse_switch(word: &str) -> Result<bool, String> {
    match word {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(format!("expected 'on' or 'off', got '{}'", other)),
    }
}

/// Only absolute paths: a relative one would be resolved against nothing, and
/// `canonicalize` would quietly root it, which is not what the operator wrote.
fn parse_path(word: &str) -> Result<String, String> {
    if word.starts_with('/') {
        Ok(String::from(word))
    } else {
        Err(format!("path must be absolute: {}", word))
    }
}

/// Parses a whole script, or answers the first line that does not parse.
pub fn parse_script(text: &str) -> Result<Vec<Directive>, ParseError> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        match parse_line(line) {
            Ok(Some(d)) => out.push(d),
            Ok(None) => {}
            Err(message) => {
                return Err(ParseError {
                    line: i + 1,
                    message,
                })
            }
        }
    }
    Ok(out)
}

/// Applies one directive. The policy setters record their own `CONTROL` events
/// (and refuse relaxations under the latch); the two heuristics switches are
/// gated and recorded here because their setters do neither.
pub fn apply(d: &Directive) {
    match d {
        Directive::Mode(Domain::Syscall, m) => policy::set_syscall_mode(*m),
        Directive::Mode(Domain::Wx, m) => policy::set_wx_mode(*m),
        Directive::Mode(Domain::Exec, m) => policy::set_exec_mode(*m),
        Directive::Mode(Domain::Anomaly, m) => policy::set_anomaly_mode(*m),
        Directive::Mode(Domain::Integrity, m) => policy::set_integrity_mode(*m),
        Directive::Mode(Domain::Av, m) => policy::set_av_mode(*m),
        Directive::Learning(on) => policy::set_exec_learning(*on),
        Directive::AnomalyDetection(on) => {
            if gate_switch("anomaly-detection", *on) {
                heuristics::set_anomaly_detection(*on);
            }
        }
        Directive::PrivilegedDeny(on) => {
            if gate_switch("privileged-deny", *on) {
                heuristics::set_privileged_deny(*on);
            }
        }
        Directive::Trust(p) if p.ends_with('/') => policy::add_trusted_exec_prefix(p.clone()),
        Directive::Trust(p) => policy::add_trusted_exec_path(p.clone()),
        Directive::Deny(p) if p.ends_with('/') => policy::add_blacklisted_exec_prefix(p.clone()),
        Directive::Deny(p) => policy::add_blacklisted_exec_path(p.clone()),
        Directive::Untrusted(p) => policy::add_untrusted_exec_prefix(p.clone()),
        Directive::Hash(d, p) => integrity::add_exec_hash(p, *d),
        Directive::RequireHash(on) => integrity::set_require_hash(*on),
        Directive::Signature(sig) => {
            if feed::add_local(sig.clone()) {
                record(
                    0,
                    Severity::Notice,
                    "CONTROL",
                    "CONFIG",
                    format!("av signature added: {}", sig.name()),
                );
            }
        }
        Directive::FeedKey(name, key) => {
            feed::add_key(name, *key);
        }
        Directive::RequireSignedFeeds(on) => feed::set_require_signed(*on),
        Directive::Seal => policy::seal_tighten_only(),
    }
}

/// Turning a heuristic off is a relaxation: refused (and said) once sealed.
/// Turning one on always proceeds. Records the change either way.
fn gate_switch(name: &str, on: bool) -> bool {
    if !on && policy::is_tighten_only() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing control change: {} off (tighten-only latch)",
                name
            ),
        );
        return false;
    }
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("{} {}", name, if on { "on" } else { "off" }),
    );
    true
}

/// Parses `text` and, only if every line parses, applies it in order.
///
/// `origin` names where the script came from (`/etc/hunter/policy.conf`,
/// `/proc/hunter`) and `pid` who sent it (`0` for the kernel at boot), both for
/// the audit trail. Answers how many directives were applied.
pub fn apply_script(text: &str, origin: &str, pid: u64) -> Result<usize, ParseError> {
    let directives = match parse_script(text) {
        Ok(d) => d,
        Err(e) => {
            record(
                pid,
                Severity::Critical,
                "CONTROL",
                "REJECTED",
                format!(
                    "{}: line {}: {}; nothing applied",
                    origin, e.line, e.message
                ),
            );
            return Err(e);
        }
    };
    record(
        pid,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("{}: applying {} directive(s)", origin, directives.len()),
    );
    for d in &directives {
        apply(d);
    }
    Ok(directives.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn reset() {
        policy::reset_for_test();
        heuristics::set_anomaly_detection(false);
        heuristics::set_privileged_deny(false);
    }

    #[test]
    fn every_directive_parses() {
        let script = "\
            # comment\n\
            \n\
            mode syscall enforce\n\
            mode wx report\n\
            mode exec enforce\n\
            mode anomaly off\n\
            learning off\n\
            anomaly-detection on\n\
            privileged-deny on\n\
            trust /usr/bin/\n\
            trust /opt/app/agent\n\
            deny /srv/evil\n\
            untrusted /run/user/\n\
            mode integrity enforce\n\
            hash ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  /usr/bin/my tool\n\
            require-hash on\n\
            seal\n";
        assert_eq!(
            parse_script(script).unwrap(),
            vec![
                Directive::Mode(Domain::Syscall, Mode::Enforce),
                Directive::Mode(Domain::Wx, Mode::Report),
                Directive::Mode(Domain::Exec, Mode::Enforce),
                Directive::Mode(Domain::Anomaly, Mode::Off),
                Directive::Learning(false),
                Directive::AnomalyDetection(true),
                Directive::PrivilegedDeny(true),
                Directive::Trust(String::from("/usr/bin/")),
                Directive::Trust(String::from("/opt/app/agent")),
                Directive::Deny(String::from("/srv/evil")),
                Directive::Untrusted(String::from("/run/user/")),
                Directive::Mode(Domain::Integrity, Mode::Enforce),
                Directive::Hash(
                    integrity::parse_hex(
                        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                    )
                    .unwrap(),
                    String::from("/usr/bin/my tool")
                ),
                Directive::RequireHash(true),
                Directive::Seal,
            ]
        );
    }

    /// The example the image ships as `/etc/hunter/policy.conf`: inert as
    /// shipped, and every line of it valid once uncommented -- an example that
    /// fails to parse would reject the whole file of whoever copies it.
    #[test]
    fn the_shipped_example_is_inert_and_every_line_of_it_parses() {
        let example = include_str!("../../tools/hunterctl/policy.conf");
        assert_eq!(parse_script(example).unwrap(), vec![]);
        const VERBS: [&str; 12] = [
            "mode",
            "learning",
            "anomaly-detection",
            "privileged-deny",
            "trust",
            "deny",
            "untrusted",
            "hash",
            "require-hash",
            "signature",
            "require-signed-feeds",
            "seal",
        ];
        // `# mode wx report   # note` -> `mode wx report   # note`, which must
        // parse as it stands; prose lines skipped.
        let uncommented = example
            .lines()
            .filter_map(|l| l.strip_prefix("# "))
            .filter(|l| VERBS.contains(&l.split_whitespace().next().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_script(&uncommented).unwrap();
        assert!(parsed.len() >= 10, "{:?}", parsed);
        assert_eq!(parsed.last(), Some(&Directive::Seal));
    }

    #[test]
    fn feed_keys_and_the_signed_feeds_switch_parse_and_apply() {
        let _g = crate::test_globals::lock();
        reset();
        feed::reset_for_test();
        let pk = "e894b9801690dba2c365a9fa5f12e2b3a6fb788668ad34e48eb50265a8d41970";
        let parsed = parse_script(&format!(
            "feed-key test-pub {}\nrequire-signed-feeds on\n",
            pk
        ))
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                Directive::FeedKey(String::from("test-pub"), integrity::parse_hex(pk).unwrap()),
                Directive::RequireSignedFeeds(true),
            ]
        );
        for d in &parsed {
            apply(d);
        }
        assert!(feed::require_signed());
        assert!(feed::render().contains(&format!("key test-pub {}", pk)));
        for bad in [
            "feed-key test-pub",
            "feed-key ../x 00",
            "feed-key k e894b980",
            "require-signed-feeds maybe",
        ] {
            assert!(parse_line(bad).is_err(), "accepted: {}", bad);
        }
        feed::reset_for_test();
    }

    /// `/etc/hunter/keys` as shipped: comments only, so it trusts nobody.
    #[test]
    fn the_shipped_keys_file_is_inert() {
        let keys = include_str!("../../tools/hunterctl/keys");
        assert_eq!(parse_script(keys).unwrap(), vec![]);
    }

    #[test]
    fn trailing_comments_are_cut_but_a_hash_inside_a_path_is_kept() {
        assert_eq!(
            parse_line("mode wx report   # JITs").unwrap(),
            Some(Directive::Mode(Domain::Wx, Mode::Report))
        );
        assert_eq!(
            parse_line("deny /opt/c#/tool\t# why").unwrap(),
            Some(Directive::Deny(String::from("/opt/c#/tool")))
        );
        assert!(parse_line("mode wx report extra # comment").is_err());
    }

    #[test]
    fn a_bad_line_is_named_by_number() {
        let err = parse_script("mode exec enforce\n\nmode exec enforcing\n").unwrap_err();
        assert_eq!(err.line, 3);
        assert!(err.message.contains("enforcing"), "{}", err.message);

        for bad in [
            "mode exec",
            "mode disk enforce",
            "learning maybe",
            "trust relative/path",
            "untrusted /run/user",
            "seal now",
            "hash deadbeef /bin/x",
            "hash",
            "require-hash",
            "frobnicate",
        ] {
            assert!(parse_line(bad).is_err(), "accepted: {}", bad);
        }
    }

    #[test]
    fn a_rejected_script_applies_nothing() {
        // The first line is valid and would raise wx; the third is a typo. The
        // stance must come out exactly as it went in.
        let _g = crate::test_globals::lock();
        reset();
        let before = policy::wx_mode();
        assert!(apply_script(
            "mode wx enforce\nlearning off\nmode exec enforcd\n",
            "test",
            0
        )
        .is_err());
        assert_eq!(policy::wx_mode(), before);
    }

    #[test]
    fn a_script_tightens_and_seals() {
        let _g = crate::test_globals::lock();
        reset();
        let n = apply_script(
            "mode wx enforce\nmode exec enforce\nlearning off\ndeny /srv/evil\nseal\n",
            "test",
            0,
        )
        .unwrap();
        assert_eq!(n, 5);
        assert_eq!(policy::wx_mode(), Mode::Enforce);
        assert_eq!(policy::exec_mode(), Mode::Enforce);
        assert!(!policy::exec_learning_enabled());
        assert!(policy::is_exec_blacklisted("/srv/evil"));
        assert!(policy::is_tighten_only());

        // After the seal, a later write cannot relax anything, heuristics
        // switches included -- but it can still tighten.
        apply_script(
            "mode wx report\nlearning on\ntrust /\nanomaly-detection off\n",
            "test",
            0,
        )
        .unwrap();
        assert_eq!(policy::wx_mode(), Mode::Enforce);
        assert!(!policy::exec_learning_enabled());
        assert!(!policy::is_exec_listed("/anything"));

        apply_script("mode anomaly enforce\n", "test", 0).unwrap();
        assert_eq!(policy::anomaly_mode(), Mode::Enforce);
        reset();
    }

    #[test]
    fn the_seal_only_binds_the_lines_after_it() {
        let _g = crate::test_globals::lock();
        reset();
        apply_script("mode wx off\nseal\nmode exec off\n", "test", 0).unwrap();
        assert_eq!(policy::wx_mode(), Mode::Off, "before the seal: applied");
        assert_ne!(policy::exec_mode(), Mode::Off, "after the seal: refused");
        reset();
    }

    #[test]
    fn heuristics_switches_are_gated_by_the_latch() {
        let _g = crate::test_globals::lock();
        reset();
        apply_script(
            "anomaly-detection on\nprivileged-deny on\nseal\n",
            "test",
            0,
        )
        .unwrap();
        apply_script("anomaly-detection off\nprivileged-deny off\n", "test", 0).unwrap();
        assert!(heuristics::anomaly_detection_enabled());
        assert!(heuristics::privileged_deny_enabled());
        reset();
    }
}
