//! Everything `lunarhunter` reads about hunter, parsed: the whole report, the
//! quarantine index, the signature feeds and the signature files on disk.
//!
//! `lunarbar::hunter` reads the report down to what fits in the bar's menu
//! (seven modes, three counters, the four newest alerts). The full window needs
//! the rest of it — every counter, the heuristics switches, the learned
//! programs, every event in the ring with its time and pid — and the files
//! `hunterctl` keeps beside the kernel. This is that half, still pure (text
//! in, values out) so the tests read the formats without a kernel.
//!
//! The formats are the kernel's and `hunterctl`'s, not ours: `render_report`
//! in `hunter/src/lib.rs`, the ring line in `hunter/src/event_log.rs`, the
//! index `hunterctl quarantine` appends to and the `feeds` file it reads.
//! Nothing here writes to hunter except [`policy_script`], which spells the
//! same directives `/etc/hunter/policy.conf` takes.

use crate::hunter::{self, Mode, Status, DOMAINS};

/// The quarantine `hunterctl quarantine` moves files into.
pub const QUARANTINE_DIR: &str = "/var/lib/hunter/quarantine";
/// The boot policy the kernel reads once (see `tools/hunterctl/policy.conf`).
pub const POLICY: &str = "/etc/hunter/policy.conf";
/// Where `hunterctl update-signatures` downloads from.
pub const FEEDS: &str = "/etc/hunter/feeds";
/// The local signature database, and the directory of downloaded feeds.
pub const SIGNATURES: &str = "/etc/hunter/signatures";
pub const SIGNATURES_D: &str = "/etc/hunter/signatures.d";
/// The SHA-256 pins `hunterctl hash` keeps.
pub const HASHES: &str = "/etc/hunter/hashes";

/// An event's severity, as the ring spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Notice,
    Warn,
    Crit,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Severity> {
        match s {
            "INFO" => Some(Severity::Info),
            "NOTICE" => Some(Severity::Notice),
            "WARN" => Some(Severity::Warn),
            "CRIT" => Some(Severity::Crit),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Notice => "NOTICE",
            Severity::Warn => "WARN",
            Severity::Crit => "CRIT",
        }
    }
}

/// One line of the event ring, every field kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RingEvent {
    pub seq: u64,
    /// Seconds since boot, as printed (`12.345`).
    pub ts: String,
    pub pid: u64,
    pub sev: Severity,
    pub category: String,
    /// `action: description`.
    pub text: String,
}

/// The whole report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// What the bar reads, so the window and the menu can never disagree about
    /// a mode.
    pub status: Status,
    pub version: String,
    pub total: u64,
    /// Warnings that were let through (`report` mode).
    pub reported: u64,
    pub dropped: u64,
    pub critical_dropped: u64,
    pub syscall_policies: u64,
    pub allowlist_active: bool,
    pub trusted: u64,
    pub learned: u64,
    pub blacklisted: u64,
    pub anomaly_detection: bool,
    pub privileged_deny: bool,
    pub require_hash: bool,
    pub sig_hash: u64,
    pub sig_bytes: u64,
    pub learned_programs: Vec<String>,
    /// Every event the ring holds, oldest first.
    pub events: Vec<RingEvent>,
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

fn num(line: &str, key: &str) -> u64 {
    field(line, key)
        .map(|v| v.trim_matches(|c| c == '(' || c == ')' || c == ',' || c == ';'))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn on(line: &str, key: &str) -> bool {
    field(line, key).is_some_and(|v| v.trim_end_matches([';', ',']) == "on")
}

/// Parse the report. `None` exactly when [`hunter::parse`] says it is not one.
pub fn parse_report(text: &str) -> Option<Report> {
    let mut r = Report {
        status: hunter::parse(text)?,
        ..Report::default()
    };
    let mut lines = text.lines();
    let mut in_learned = false;
    for line in lines.by_ref() {
        if in_learned {
            if let Some(p) = line.strip_prefix("  ") {
                r.learned_programs.push(p.to_string());
                continue;
            }
            in_learned = false;
        }
        if let Some(v) = line.strip_prefix("hunter security subsystem v") {
            r.version = v.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("events:") {
            r.total = num(rest, "total");
            r.reported = num(rest, "reported");
            r.dropped = num(rest, "dropped");
            r.critical_dropped = num(rest, "critical_dropped");
        } else if let Some(rest) = line.strip_prefix("active syscall policies:") {
            r.syscall_policies = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("exec allowlist:") {
            // "active (configured=4, learned=2); blacklist=1; learning=on"
            r.allowlist_active = rest.trim_start().starts_with("active");
            r.trusted = num(rest, "(configured");
            r.learned = num(rest, "learned");
            r.blacklisted = num(rest, "blacklist");
        } else if let Some(rest) = line.strip_prefix("heuristics:") {
            r.anomaly_detection = on(rest, "anomaly-detection");
            r.privileged_deny = on(rest, "privileged-deny");
        } else if let Some(rest) = line.strip_prefix("integrity:") {
            r.require_hash = on(rest, "require-hash");
        } else if let Some(rest) = line.strip_prefix("av:") {
            r.sig_hash = num(rest, "(hash");
            r.sig_bytes = num(rest, "bytes");
        } else if line.starts_with("learned-programs") {
            in_learned = true;
        } else if line.starts_with("recent events") {
            break;
        }
    }
    r.events = lines.filter_map(parse_ring_line).collect();
    Some(r)
}

/// One ring line: `[   seq] +   ts s pid=N   SEV    CATEGORY  action: desc`.
pub fn parse_ring_line(line: &str) -> Option<RingEvent> {
    let rest = line.trim_start().strip_prefix('[')?;
    let (seq, rest) = rest.split_once(']')?;
    let seq = seq.trim().parse().ok()?;
    let rest = rest.trim_start().strip_prefix('+')?.trim_start();
    let (ts, rest) = rest.split_once(char::is_whitespace)?;
    let ts = ts.strip_suffix('s')?.to_string();
    let rest = rest.trim_start().strip_prefix("pid=")?;
    let (pid, rest) = rest.split_once(char::is_whitespace)?;
    let pid = pid.parse().ok()?;
    let rest = rest.trim_start();
    let (sev, rest) = rest.split_once(char::is_whitespace)?;
    let sev = Severity::parse(sev)?;
    let rest = rest.trim_start();
    let (category, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(RingEvent {
        seq,
        ts,
        pid,
        sev,
        category: category.to_string(),
        text: text.trim().to_string(),
    })
}

/// Read and parse `/proc/hunter`.
pub fn read() -> Option<Report> {
    parse_report(&std::fs::read_to_string(hunter::PROC).ok()?)
}

/// A file in the quarantine, as `hunterctl quarantine` indexed it:
/// `date<TAB>sha256<TAB>mode<TAB>original path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quarantined {
    pub date: String,
    pub sha256: String,
    pub mode: String,
    pub path: String,
}

/// The index, newest last as it was appended. Lines that are not four fields
/// with a 64-digit hash are skipped: `restore` takes the hash as an argument,
/// and a row that could not be restored is a row the window should not offer.
pub fn parse_quarantine_index(text: &str) -> Vec<Quarantined> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t');
            let (date, sha, mode, path) = (f.next()?, f.next()?, f.next()?, f.next()?);
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) || path.is_empty() {
                return None;
            }
            Some(Quarantined {
                date: date.to_string(),
                sha256: sha.to_string(),
                mode: mode.to_string(),
                path: path.to_string(),
            })
        })
        .collect()
}

pub fn read_quarantine() -> Vec<Quarantined> {
    std::fs::read_to_string(format!("{QUARANTINE_DIR}/index"))
        .map(|t| parse_quarantine_index(&t))
        .unwrap_or_default()
}

/// One feed `hunterctl update-signatures` downloads: `<format> <name> <url>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    pub format: String,
    pub name: String,
    pub url: String,
}

pub fn parse_feeds(text: &str) -> Vec<Feed> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some(Feed {
                format: w.next()?.to_string(),
                name: w.next()?.to_string(),
                url: w.next()?.to_string(),
            })
        })
        .collect()
}

/// A signature file on disk and what is in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigFile {
    pub path: String,
    pub hashes: usize,
    pub patterns: usize,
    /// Seconds since the epoch of its last change, when the disk says.
    pub mtime: Option<u64>,
}

/// Count the signature lines of one file: what the kernel loads at boot.
pub fn count_signatures(text: &str) -> (usize, usize) {
    text.lines().fold((0, 0), |(h, b), l| {
        let l = l.trim();
        if l.starts_with("hash:") {
            (h + 1, b)
        } else if l.starts_with("bytes:") {
            (h, b + 1)
        } else {
            (h, b)
        }
    })
}

/// `/etc/hunter/signatures` then every feed in `signatures.d/`, skipping the
/// dot-files `update-signatures` writes before renaming and the `.sig` beside
/// a signed feed (the kernel skips both too).
pub fn read_signature_files() -> Vec<SigFile> {
    let mut paths = vec![SIGNATURES.to_string()];
    if let Ok(rd) = std::fs::read_dir(SIGNATURES_D) {
        let mut names: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| !n.starts_with('.') && !n.ends_with(".sig"))
            .collect();
        names.sort();
        paths.extend(names.into_iter().map(|n| format!("{SIGNATURES_D}/{n}")));
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let (hashes, patterns) = count_signatures(&text);
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            Some(SigFile {
                path,
                hashes,
                patterns,
                mtime,
            })
        })
        .collect()
}

/// How many programs `/etc/hunter/hashes` pins (one `sha256sum` line each).
pub fn count_pins(text: &str) -> usize {
    text.lines()
        .filter(|l| l.len() > 66 && l.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit))
        .count()
}

/// The script that reproduces the stance the report shows: the seven modes,
/// learning, the two heuristics, require-hash, the two media switches, and
/// the seal when it is on.
///
/// This is what "save as the boot policy" writes, so a stance tuned in the
/// window survives the reboot that would otherwise drop it back to the
/// defaults. Only switches, never lists: the allowlist, the pins and the
/// signatures have their own files (`whitelist`, `hashes`, `signatures.d`),
/// and copying them here would make two places to edit one thing.
pub fn policy_script(r: &Report) -> String {
    let st = &r.status;
    let onoff = |b: bool| if b { "on" } else { "off" };
    let mut s = String::new();
    for (d, m) in DOMAINS.iter().zip(st.modes.iter()) {
        s.push_str(&format!("mode {d} {}\n", m.as_str()));
    }
    s.push_str(&format!("learning {}\n", onoff(st.learning)));
    s.push_str(&format!(
        "anomaly-detection {}\n",
        onoff(r.anomaly_detection)
    ));
    s.push_str(&format!("privileged-deny {}\n", onoff(r.privileged_deny)));
    s.push_str(&format!("require-hash {}\n", onoff(r.require_hash)));
    if st.media_known {
        s.push_str(&format!("media-scan {}\n", onoff(st.media_scan)));
        s.push_str(&format!("media-lock {}\n", onoff(st.media_lock)));
    }
    if st.sealed {
        s.push_str("seal\n");
    }
    s
}

/// The boot policy file: a header saying where it came from, then the script.
pub fn policy_file(r: &Report, when: &str) -> String {
    format!(
        "# /etc/hunter/policy.conf — written by lunarhunter on {when}.\n\
         # The previous file was kept as policy.conf.bak. Same language as\n\
         # `hunterctl apply` (see docs/hunter-security.md); applied at boot all\n\
         # or nothing.\n\n{}",
        policy_script(r)
    )
}

/// A switch `lunarhunter` can flip that is not a domain mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    Learning,
    AnomalyDetection,
    PrivilegedDeny,
    RequireHash,
    MediaScan,
    MediaLock,
}

impl Switch {
    pub const ALL: [Switch; 6] = [
        Switch::Learning,
        Switch::AnomalyDetection,
        Switch::PrivilegedDeny,
        Switch::RequireHash,
        Switch::MediaScan,
        Switch::MediaLock,
    ];

    /// The directive, which is also `hunterctl`'s subcommand.
    pub fn directive(self) -> &'static str {
        match self {
            Switch::Learning => "learning",
            Switch::AnomalyDetection => "anomaly-detection",
            Switch::PrivilegedDeny => "privileged-deny",
            Switch::RequireHash => "require-hash",
            Switch::MediaScan => "media-scan",
            Switch::MediaLock => "media-lock",
        }
    }

    pub fn get(self, r: &Report) -> bool {
        match self {
            Switch::Learning => r.status.learning,
            Switch::AnomalyDetection => r.anomaly_detection,
            Switch::PrivilegedDeny => r.privileged_deny,
            Switch::RequireHash => r.require_hash,
            Switch::MediaScan => r.status.media_scan,
            Switch::MediaLock => r.status.media_lock,
        }
    }

    /// Which way relaxes. Learning ON trusts what runs, so on is the loose
    /// side; the others are protections, so off is.
    pub fn relaxes(self, to: bool) -> bool {
        match self {
            Switch::Learning => to,
            _ => !to,
        }
    }

    pub fn script(self, to: bool) -> String {
        format!("{} {}\n", self.directive(), if to { "on" } else { "off" })
    }
}

/// Would `mode` for `domain` be refused under the seal?
pub fn sealed_refuses_mode(r: &Report, domain: usize, mode: Mode) -> bool {
    r.status.sealed && r.status.relaxes(domain, mode)
}

pub fn sealed_refuses_switch(r: &Report, sw: Switch, to: bool) -> bool {
    r.status.sealed && sw.relaxes(to) && sw.get(r) != to
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = "hunter security subsystem v0.1.0\n\
enforcement: syscall=enforce wx=report exec=report anomaly=off integrity=report av=enforce behavior=enforce\n\
events: total=12 blocked=3 warnings=5 reported=4 critical=2 dropped=1 critical_dropped=0\n\
active syscall policies: 2\n\
exec allowlist: active (configured=4, learned=2); blacklist=1; learning=on\n\
heuristics: anomaly-detection=on privileged-deny=off\n\
integrity: pinned=7 require-hash=on\n\
av: signatures=5002 (hash=5000 bytes=2) detections=1\n\
media: auto-scan=on lock=off devices=0\n\
learned-programs (2):\n  /bin/ls\n  /bin/sh\n\
\n\
recent events (oldest first):\n\
[     1] +     0.100s pid=1     NOTICE CONTROL   CONFIG: mode av enforce\n\
[     2] +  1234.000s pid=40    WARN   EXEC      REPORTED: untrusted exec /tmp/a\n\
[     3] +     2.000s pid=41    CRIT   AV        AV BLOCKED: /tmp/eicar.com (Eicar-Test-Signature)\n\
[     5] +     4.000s pid=43    INFO   EXEC      ALLOWED: /bin/ls\n";

    #[test]
    fn every_counter_and_switch_of_the_report_is_read() {
        let r = parse_report(REPORT).unwrap();
        assert_eq!(r.version, "0.1.0");
        assert_eq!(
            (r.total, r.reported, r.dropped, r.critical_dropped),
            (12, 4, 1, 0)
        );
        assert_eq!(r.syscall_policies, 2);
        assert!(r.allowlist_active);
        assert_eq!((r.trusted, r.learned, r.blacklisted), (4, 2, 1));
        assert!(r.anomaly_detection && !r.privileged_deny && r.require_hash);
        assert_eq!((r.sig_hash, r.sig_bytes), (5000, 2));
        assert_eq!(r.learned_programs, ["/bin/ls", "/bin/sh"]);
        // The bar's half agrees, because it IS the bar's half.
        assert_eq!(r.status, hunter::parse(REPORT).unwrap());
    }

    #[test]
    fn the_ring_keeps_every_severity_and_field() {
        let r = parse_report(REPORT).unwrap();
        assert_eq!(r.events.len(), 4);
        let e = &r.events[1];
        assert_eq!((e.seq, e.ts.as_str(), e.pid), (2, "1234.000", 40));
        assert_eq!((e.sev, e.category.as_str()), (Severity::Warn, "EXEC"));
        assert_eq!(e.text, "REPORTED: untrusted exec /tmp/a");
        assert_eq!(r.events[0].sev, Severity::Notice);
        assert_eq!(r.events[3].sev, Severity::Info);
    }

    #[test]
    fn an_empty_ring_and_no_learned_programs_read_as_empty() {
        let quiet = REPORT
            .replace("learned-programs (2):\n  /bin/ls\n  /bin/sh\n", "")
            .split("recent events")
            .next()
            .unwrap()
            .to_string()
            + "recent events (oldest first):\n(no security events recorded)\n";
        let r = parse_report(&quiet).unwrap();
        assert!(r.events.is_empty() && r.learned_programs.is_empty());
    }

    #[test]
    fn not_a_report_is_none() {
        assert_eq!(parse_report("cpu 1 2 3\n"), None);
    }

    #[test]
    fn the_quarantine_index_is_read_and_bad_rows_skipped() {
        let sha = "a".repeat(64);
        let text = format!(
            "2026-10-28T10:00:00\t{sha}\t755\t/tmp/evil bin\nbroken line\n2026\tnothex\t644\t/x\n"
        );
        let q = parse_quarantine_index(&text);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].path, "/tmp/evil bin");
        assert_eq!(q[0].mode, "755");
    }

    #[test]
    fn the_shipped_feeds_and_signatures_parse() {
        let feeds = parse_feeds(include_str!("../../hunterctl/feeds"));
        assert_eq!(feeds.len(), 1);
        assert_eq!(feeds[0].format, "sha256");
        assert!(feeds[0].url.starts_with("https://"));
        // No sample signature ships: EICAR's pattern flagged every file that
        // quoted it.
        assert_eq!(
            count_signatures(include_str!("../../hunterctl/signatures")),
            (0, 0)
        );
    }

    #[test]
    fn pins_are_counted_by_sha256sum_line() {
        let sha = "0".repeat(64);
        assert_eq!(
            count_pins(&format!("{sha}  /bin/ls\n# x\n\n{sha}  /bin/sh\n")),
            2
        );
    }

    #[test]
    fn the_policy_reproduces_the_stance_and_parses_as_hunter_language() {
        let mut r = parse_report(REPORT).unwrap();
        let s = policy_script(&r);
        assert!(s.starts_with("mode syscall enforce\nmode wx report\n"));
        // The behavior domain too: without its line, a saved policy reset
        // ransomware/injection protection to the default at the next boot.
        assert!(s.contains("mode av enforce\nmode behavior enforce\nlearning on\nanomaly-detection on\n"));
        assert!(s.contains("privileged-deny off\nrequire-hash on\nmedia-scan on\nmedia-lock off\n"));
        // A kernel without the media table would reject the whole file.
        let mut old = r.clone();
        old.status.media_known = false;
        assert!(!policy_script(&old).contains("media"));
        assert!(!s.contains("seal"));
        r.status.sealed = true;
        assert!(policy_script(&r).ends_with("seal\n"));
        // Every directive is one hunterctl's policy.conf documents.
        let doc = include_str!("../../hunterctl/policy.conf");
        for line in policy_script(&r).lines() {
            let word = line.split_whitespace().next().unwrap();
            assert!(doc.contains(&format!("# {word}")), "{word}");
        }
    }

    #[test]
    fn the_seal_refuses_only_relaxations() {
        let mut r = parse_report(REPORT).unwrap();
        assert!(!sealed_refuses_switch(&r, Switch::Learning, false));
        r.status.sealed = true;
        // learning is on: turning it off tightens.
        assert!(!sealed_refuses_switch(&r, Switch::Learning, false));
        // require-hash is on: off relaxes.
        assert!(sealed_refuses_switch(&r, Switch::RequireHash, false));
        // privileged-deny is off: on tightens.
        assert!(!sealed_refuses_switch(&r, Switch::PrivilegedDeny, true));
        assert!(sealed_refuses_mode(&r, 0, Mode::Report));
        assert!(!sealed_refuses_mode(&r, 3, Mode::Enforce));
        assert_eq!(
            Switch::AnomalyDetection.script(true),
            "anomaly-detection on\n"
        );
    }
}
