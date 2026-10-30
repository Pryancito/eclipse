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
//! | `mode <domain> <off\|report\|enforce>`  | domain is `syscall`, `wx`, `exec`, `anomaly`, `integrity`, `av`, `net`, `behavior` |
//! | `learning <on\|off>`                    | exec trust-on-first-use                       |
//! | `anomaly-detection <on\|off>`           | per-process flood / fork-bomb heuristics      |
//! | `privileged-deny <on\|off>`             | block module/ptrace/bpf/... under `anomaly=enforce` |
//! | `trust <path>` / `trust <dir/>`         | add to the exec allowlist                     |
//! | `deny <path>` / `deny <dir/>`           | add to the exec blacklist                     |
//! | `untrusted <dir/>`                      | add a world-writable location                 |
//! | `hash <sha256> <path>`                  | pin a program's SHA-256 (see [`crate::integrity`]) |
//! | `require-hash <on\|off>`                | an unpinned program is an integrity violation |
//! | `signature <hash\|bytes>:<data>:<Name>` | add an antivirus signature (see [`crate::av`]) |
//! | `media-scan <on\|off>`                  | queue new removable media for a scan (see [`crate::media`]) |
//! | `media-lock <on\|off>`                  | nothing runs from media until it scans clean  |
//! | `media <id> scanning\|failed`           | a scan agent took / gave up on a device       |
//! | `media <id> done <files> <infected>`    | a scan agent's verdict on a device            |
//! | `feed-key <name> <hex>`                 | trust an Ed25519 key for signed feeds (see [`crate::feed`]) |
//! | `require-signed-feeds <on\|off>`        | refuse signature feeds that are not signed    |
//! | `net <allow\|deny> <path> [in\|out]`   | a firewall rule (see [`crate::firewall`]); also `net forget\|dismiss <path>`, `net timeout <secs>` |
//! | `canary <path>`                         | a decoy file: touching it is ransomware (see [`crate::ransomware`]) |
//! | `exempt <path>`                         | a program the behaviour detectors leave alone |
//! | `release <pid>`                         | lift a process's ransomware containment       |
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
use crate::behavior;
use crate::event_log::{record, Severity};
use crate::feed;
use crate::firewall;
use crate::heuristics;
use crate::integrity::{self, Digest};
use crate::media;
use crate::policy::{self, Mode};
use crate::ransomware;

/// An enforcement domain a `mode` directive can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Syscall,
    Wx,
    Exec,
    Anomaly,
    Integrity,
    Av,
    Net,
    Behavior,
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
    MediaScan(bool),
    MediaLock(bool),
    Media(u64, media::Report),
    FeedKey(String, feed::PublicKey),
    RequireSignedFeeds(bool),
    Net(firewall::NetDirective),
    Canary(String),
    Exempt(String),
    Release(u64),
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
        "trust" => Directive::Trust(rest_path(line, "trust")?),
        "deny" => Directive::Deny(rest_path(line, "deny")?),
        "untrusted" => {
            let p = rest_path(line, "untrusted")?;
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
        "net" => {
            // The rest of the line, so a rule's path may hold a space, as in
            // `/etc/hunter/firewall`, whose lines are these without the `net`.
            Directive::Net(firewall::parse_directive(&line["net".len()..])?)
        }
        "require-hash" => {
            arity(2)?;
            Directive::RequireHash(parse_switch(words[1])?)
        }
        "media-scan" => {
            arity(2)?;
            Directive::MediaScan(parse_switch(words[1])?)
        }
        "media-lock" => {
            arity(2)?;
            Directive::MediaLock(parse_switch(words[1])?)
        }
        "media" => {
            let id = words.get(1).and_then(|w| w.parse().ok());
            let count = |i: usize| words.get(i).and_then(|w| w.parse().ok());
            let report = match words.get(2).copied() {
                Some("scanning") if words.len() == 3 => Some(media::Report::Scanning),
                Some("failed") if words.len() == 3 => Some(media::Report::Failed),
                Some("done") if words.len() == 5 => match (count(3), count(4)) {
                    (Some(files), Some(infected)) => Some(media::Report::Done { files, infected }),
                    _ => None,
                },
                _ => None,
            };
            match (id, report) {
                (Some(id), Some(r)) => Directive::Media(id, r),
                _ => {
                    return Err(String::from(
                        "'media' takes '<id> scanning', '<id> failed' or '<id> done <files> <infected>'",
                    ))
                }
            }
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
        "canary" => Directive::Canary(rest_path(line, "canary")?),
        "exempt" => Directive::Exempt(rest_path(line, "exempt")?),
        "release" => {
            arity(2)?;
            match words[1].parse::<u64>() {
                Ok(pid) if pid > 0 => Directive::Release(pid),
                _ => return Err(format!("'release' takes a pid, got '{}'", words[1])),
            }
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
pub(crate) fn strip_comment(line: &str) -> &str {
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
        "net" => Ok(Domain::Net),
        "behavior" => Ok(Domain::Behavior),
        other => Err(format!(
            "unknown domain '{}' (syscall, wx, exec, anomaly, integrity, av, net, behavior)",
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

/// The rest of the line after the directive's word, as one absolute path.
/// The rest and not the next word, so a path with a space in it can be
/// trusted, denied or armed: `hunterctl quarantine` denies whatever path the
/// malware had, and a word-sized `deny` refused `/home/u/Mis documentos/x`
/// -- after the file was already moved away, leaving a fresh copy dropped
/// there free to run. Like `hash`, whose path is also the rest of the line.
fn rest_path(line: &str, word: &str) -> Result<String, String> {
    let rest = line[word.len()..].trim();
    if rest.is_empty() {
        return Err(format!("'{}' takes a path", word));
    }
    parse_path(rest)
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
        Directive::Mode(Domain::Net, m) => firewall::set_net_mode(*m),
        Directive::Mode(Domain::Behavior, m) => behavior::set_mode(*m),
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
        Directive::MediaScan(on) => media::set_auto_scan(*on),
        Directive::MediaLock(on) => media::set_lock(*on),
        Directive::Media(id, r) => {
            if !media::report(*id, *r) {
                record(
                    0,
                    Severity::Notice,
                    "MEDIA",
                    "STALE",
                    format!("report for unknown media #{} ignored (unmounted?)", id),
                );
            }
        }
        Directive::FeedKey(name, key) => {
            feed::add_key(name, *key);
        }
        Directive::RequireSignedFeeds(on) => feed::set_require_signed(*on),
        Directive::Net(n) => firewall::apply(n),
        Directive::Canary(p) => ransomware::add_canary(p.clone()),
        Directive::Exempt(p) => behavior::add_exempt(p.clone()),
        Directive::Release(pid) => {
            behavior::release(*pid);
        }
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
        behavior::reset_for_test();
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
            media-scan on\n\
            media-lock on\n\
            media 7 scanning\n\
            media 7 done 120 0\n\
            media 8 failed\n\
            mode behavior enforce\n\
            canary /home/u/.~budget.xlsx\n\
            exempt /usr/bin/gdb\n\
            release 42\n\
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
                Directive::MediaScan(true),
                Directive::MediaLock(true),
                Directive::Media(7, media::Report::Scanning),
                Directive::Media(
                    7,
                    media::Report::Done {
                        files: 120,
                        infected: 0
                    }
                ),
                Directive::Media(8, media::Report::Failed),
                Directive::Mode(Domain::Behavior, Mode::Enforce),
                Directive::Canary(String::from("/home/u/.~budget.xlsx")),
                Directive::Exempt(String::from("/usr/bin/gdb")),
                Directive::Release(42),
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
        const VERBS: [&str; 17] = [
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
            "media-scan",
            "media-lock",
            "require-signed-feeds",
            "canary",
            "exempt",
            "release",
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
    fn a_path_with_spaces_is_one_path() {
        // What `hunterctl quarantine` sends for malware that was in a
        // folder with a space in its name: the deny must take, or a fresh
        // copy dropped at the same path runs.
        assert_eq!(
            parse_line("deny /home/u/Mis documentos/factura.pdf.exe").unwrap(),
            Some(Directive::Deny(String::from(
                "/home/u/Mis documentos/factura.pdf.exe"
            )))
        );
        assert_eq!(
            parse_line("trust  /opt/My App/bin/app  # vendor").unwrap(),
            Some(Directive::Trust(String::from("/opt/My App/bin/app")))
        );
        assert_eq!(
            parse_line("canary /home/u/Mis documentos/.~nominas.ods").unwrap(),
            Some(Directive::Canary(String::from(
                "/home/u/Mis documentos/.~nominas.ods"
            )))
        );
        assert_eq!(
            parse_line("exempt /opt/Backup Tool/bin/backup").unwrap(),
            Some(Directive::Exempt(String::from(
                "/opt/Backup Tool/bin/backup"
            )))
        );
        assert_eq!(
            parse_line("untrusted /media/USB DISK/").unwrap(),
            Some(Directive::Untrusted(String::from("/media/USB DISK/")))
        );
        for bad in [
            "deny",
            "deny   ",
            "trust  # nothing",
            "deny relative/with space",
        ] {
            assert!(parse_line(bad).is_err(), "accepted: {}", bad);
        }
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
            "media-lock",
            "media 3",
            "media x done 1 0",
            "media 3 done 1",
            "media 3 done -1 0",
            "media 3 scanning now",
            "canary relative",
            "exempt",
            "release self",
            "release 0",
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
