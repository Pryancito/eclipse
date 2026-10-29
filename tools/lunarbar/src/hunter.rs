//! The panel's client for hunter, Eclipse OS's in-kernel security subsystem.
//!
//! hunter has exactly one door, and this module uses no other: `/proc/hunter`.
//! Reading it gives the report `hunterctl status` prints (modes, counters,
//! the event ring); writing it sends a script in hunter's control language,
//! which the kernel applies all or nothing and only from root. So the bar holds
//! no policy of its own either: it parses what the kernel says and spells the
//! same directives `hunterctl` does, which keeps the two front ends from ever
//! disagreeing about what a button means.
//!
//! Everything here is the pure half (parse the report, build a script, decide
//! whether a change would relax a sealed system) plus the two syscalls, so the
//! tests cover what the menu shows without a kernel to talk to.

use crate::i18n::Lang;
use std::io::Write;

/// hunter's report and control file.
pub const PROC: &str = "/proc/hunter";

/// The domains `mode <domain> <mode>` takes, in the order the report prints
/// them (and the menu shows them).
pub const DOMAINS: [&str; 6] = ["syscall", "wx", "exec", "anomaly", "integrity", "av"];

/// How many of the newest warnings/criticals the menu lists.
pub const RECENT: usize = 4;

/// What `hunterctl lockdown` sends, word for word: exec, integrity and the
/// antivirus enforced, learning off, then the seal. W^X and the anomaly
/// heuristics stay where they are (enforcing W^X breaks every JIT); see the
/// comment on `lockdown` in `tools/hunterctl/hunterctl`.
pub const LOCKDOWN: &str =
    "mode exec enforce\nmode integrity enforce\nmode av enforce\nlearning off\nseal\n";

/// A domain's enforcement mode, as hunter spells it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    #[default]
    Off,
    Report,
    Enforce,
}

impl Mode {
    /// Left to right, the order the menu draws the three segments in. Being
    /// also the derive order, `a < b` reads "a is looser than b".
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Report, Mode::Enforce];

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "report" => Some(Mode::Report),
            "enforce" => Some(Mode::Enforce),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

/// One warning or critical from the event ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    pub critical: bool,
    /// `CATEGORY action: description`, as the ring prints it.
    pub text: String,
}

/// What the report says, reduced to what the bar draws.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Indexed like [`DOMAINS`].
    pub modes: [Mode; 6],
    /// The tighten-only latch is engaged: relaxations are refused until reboot.
    pub sealed: bool,
    pub learning: bool,
    pub blocked: u64,
    pub warnings: u64,
    pub criticals: u64,
    pub signatures: u64,
    pub detections: u64,
    pub pinned: u64,
    /// The newest [`RECENT`] warnings and criticals, newest first.
    pub recent: Vec<Event>,
}

/// The bar's shield, one glance's worth of the status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shield {
    /// A critical arrived that nobody has opened the menu to look at yet.
    Alert,
    /// Some domain is off.
    Open,
    /// Every domain at least reports.
    Guarded,
}

impl Status {
    /// The shield for this status, given the critical count the user last
    /// saw (the menu opening is what "saw" means).
    pub fn shield(&self, seen_criticals: u64) -> Shield {
        if self.criticals > seen_criticals {
            Shield::Alert
        } else if self.modes.contains(&Mode::Off) {
            Shield::Open
        } else {
            Shield::Guarded
        }
    }

    /// Would setting `domain` to `mode` relax it? Under the seal hunter
    /// refuses that (and says so only in its event ring), so the menu does
    /// not offer it.
    pub fn relaxes(&self, domain: usize, mode: Mode) -> bool {
        self.modes.get(domain).is_some_and(|&cur| mode < cur)
    }
}

/// `key=value` out of a run of whitespace-separated tokens.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|t| t.strip_prefix(key)?.strip_prefix('='))
}

fn num(line: &str, key: &str) -> u64 {
    field(line, key).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Parse hunter's report. `None` when the text is not one (no `enforcement:`
/// line): a kernel without hunter, or a file that is something else.
pub fn parse(report: &str) -> Option<Status> {
    let mut st = Status::default();
    let mut have_modes = false;
    let mut in_events = false;
    let mut events = Vec::new();
    for line in report.lines() {
        if in_events {
            if let Some(e) = parse_event(line) {
                events.push(e);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("enforcement:") {
            for (i, d) in DOMAINS.iter().enumerate() {
                st.modes[i] = field(rest, d).and_then(Mode::parse)?;
            }
            st.sealed = rest.contains("[tighten-only]");
            have_modes = true;
        } else if let Some(rest) = line.strip_prefix("events:") {
            st.blocked = num(rest, "blocked");
            st.warnings = num(rest, "warnings");
            st.criticals = num(rest, "critical");
        } else if let Some(rest) = line.strip_prefix("exec allowlist:") {
            // "...; learning=on"
            st.learning = rest
                .replace(';', " ")
                .split_whitespace()
                .any(|t| t == "learning=on");
        } else if let Some(rest) = line.strip_prefix("integrity:") {
            st.pinned = num(rest, "pinned");
        } else if let Some(rest) = line.strip_prefix("av:") {
            st.signatures = num(rest, "signatures");
            st.detections = num(rest, "detections");
        } else if line.starts_with("recent events") {
            in_events = true;
        }
    }
    if !have_modes {
        return None;
    }
    let start = events.len().saturating_sub(RECENT);
    st.recent = events.split_off(start);
    st.recent.reverse();
    Some(st)
}

/// One ring line, `[   seq] +   ts s pid=N   SEV    CATEGORY  action: desc`,
/// kept only when it is a warning or a critical.
fn parse_event(line: &str) -> Option<Event> {
    let rest = line.trim_start().strip_prefix('[')?;
    let (seq, rest) = rest.split_once(']')?;
    let seq = seq.trim().parse().ok()?;
    // Skip the timestamp (`+` then the right-aligned seconds, so two tokens
    // or one depending on its width) and the pid, then read the severity.
    // What follows is the category and the free text, whose spacing is kept.
    let mut rest = rest.trim_start().strip_prefix('+')?.trim_start();
    let mut sev = "";
    for _ in 0..3 {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        sev = &rest[..end];
        rest = rest[end..].trim_start();
    }
    let critical = match sev {
        "CRIT" => true,
        "WARN" => false,
        _ => return None,
    };
    let (cat, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    Some(Event {
        seq,
        critical,
        text: format!("{cat} {}", text.trim_start()),
    })
}

/// Read and parse `/proc/hunter`. `None` when this kernel has no hunter.
pub fn read() -> Option<Status> {
    parse(&std::fs::read_to_string(PROC).ok()?)
}

/// Why a script did not go in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Not root.
    Denied,
    /// hunter rejected the script (it is applied all or nothing, so nothing
    /// changed).
    Rejected,
    /// No `/proc/hunter`, or the write failed some other way.
    Unavailable,
}

/// Send one script to hunter in ONE write, which is what makes it all or
/// nothing: hunter parses each write on its own.
pub fn send(script: &str) -> Result<(), SendError> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(PROC)
        .map_err(|e| io_error(&e))?;
    match f.write(script.as_bytes()) {
        Ok(n) if n == script.len() => Ok(()),
        Ok(_) => Err(SendError::Unavailable),
        Err(e) => Err(io_error(&e)),
    }
}

fn io_error(e: &std::io::Error) -> SendError {
    match e.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => SendError::Denied,
        Some(libc::EINVAL) => SendError::Rejected,
        _ => SendError::Unavailable,
    }
}

/// The directive that sets one domain's mode.
pub fn mode_script(domain: usize, mode: Mode) -> Option<String> {
    Some(format!("mode {} {}\n", DOMAINS.get(domain)?, mode.as_str()))
}

/// The menu's words.
pub struct Text {
    pub title: &'static str,
    pub sealed: &'static str,
    pub blocked: &'static str,
    pub alerts: &'static str,
    pub signatures: &'static str,
    pub learning: &'static str,
    pub modes: [&'static str; 3],
    pub scan: &'static str,
    pub update: &'static str,
    pub log: &'static str,
    pub lockdown: &'static str,
    /// The header button that opens `lunarhunter`.
    pub open: &'static str,
    pub confirm: &'static str,
    pub no_events: &'static str,
    pub denied: &'static str,
    pub rejected: &'static str,
    pub refused: &'static str,
    pub unavailable: &'static str,
}

const ES: Text = Text {
    title: "hunter",
    sealed: "sellado",
    blocked: "bloqueos",
    alerts: "alertas",
    signatures: "firmas",
    learning: "aprendizaje",
    modes: ["off", "aviso", "bloqueo"],
    scan: "escanear",
    update: "firmas",
    log: "registro",
    lockdown: "blindar",
    open: "abrir",
    confirm: "¿seguro?",
    no_events: "sin alertas",
    denied: "hace falta root",
    rejected: "hunter rechazó el cambio",
    refused: "sellado: no se puede relajar",
    unavailable: "/proc/hunter no responde",
};

const EN: Text = Text {
    title: "hunter",
    sealed: "sealed",
    blocked: "blocked",
    alerts: "alerts",
    signatures: "signatures",
    learning: "learning",
    modes: ["off", "report", "enforce"],
    scan: "scan",
    update: "update",
    log: "log",
    lockdown: "lockdown",
    open: "open",
    confirm: "sure?",
    no_events: "no alerts",
    denied: "root required",
    rejected: "hunter rejected the change",
    refused: "sealed: cannot relax",
    unavailable: "/proc/hunter not responding",
};

pub fn text(lang: Lang) -> &'static Text {
    match lang {
        Lang::Es => &ES,
        Lang::En => &EN,
    }
}

/// The message for a failed send.
pub fn error_text(lang: Lang, e: SendError) -> &'static str {
    let t = text(lang);
    match e {
        SendError::Denied => t.denied,
        SendError::Rejected => t.rejected,
        SendError::Unavailable => t.unavailable,
    }
}

/// The terminal commands behind the menu's buttons. `hunterctl` is at
/// `/sbin/hunterctl` (see `install_hunter` in xtask); the trailing `read`
/// keeps the window open on the result instead of closing it the moment
/// the command ends.
pub fn scan_command(home: &str) -> String {
    hold(&format!(
        "/sbin/hunterctl scan --quarantine '{}'",
        home.replace('\'', "")
    ))
}

pub fn update_command() -> String {
    hold("/sbin/hunterctl update-signatures")
}

/// The whole report, ring included; the terminal's scrollback holds it, which
/// spares depending on a pager.
pub fn log_command() -> String {
    hold("/sbin/hunterctl events")
}

/// `lunarhunter`, the window with the rest of hunter (see
/// `src/bin/lunarhunter.rs`). When it is installed the menu's buttons open it
/// on the right tab instead of a terminal: a scan with a count that moves and
/// a stop button, an update whose feeds are listed, the ring with a filter.
pub const APP: &str = "lunarhunter";

/// The whole window, on its first tab.
pub fn app_command() -> String {
    APP.to_string()
}

/// Scan `home` at once, quarantining as it goes: what `scan_command` does in
/// a terminal. `--quarantine` before `--scan`, which takes every word after it.
pub fn app_scan_command(home: &str) -> String {
    format!("{APP} --quarantine --scan '{}'", home.replace('\'', ""))
}

pub fn app_update_command() -> String {
    format!("{APP} --update")
}

pub fn app_events_command() -> String {
    format!("{APP} --tab eventos")
}

fn hold(cmd: &str) -> String {
    format!("{cmd}; echo; echo '[enter]'; read _")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report as the kernel renders it (`render_report` in hunter/src/lib.rs).
    const REPORT: &str = "hunter security subsystem v0.1.0\n\
enforcement: syscall=enforce wx=report exec=report anomaly=off integrity=report av=enforce\n\
events: total=12 blocked=3 warnings=5 reported=4 critical=2 dropped=0 critical_dropped=0\n\
active syscall policies: 0\n\
exec allowlist: active (configured=4, learned=2); blacklist=1; learning=on\n\
heuristics: anomaly-detection=on privileged-deny=off\n\
integrity: pinned=7 require-hash=off\n\
av: signatures=5002 (hash=5000 bytes=2) detections=1\n\
learned-programs (2):\n  /bin/ls\n  /bin/sh\n\
\n\
recent events (oldest first):\n\
[     1] +     0.100s pid=1     NOTICE CONTROL   CONFIG: mode av enforce\n\
[     2] +     1.000s pid=40    WARN   EXEC      REPORTED: untrusted exec /tmp/a\n\
[     3] +     2.000s pid=41    CRIT   AV        AV BLOCKED: /tmp/eicar.com (Eicar-Test-Signature)\n\
[     4] +     3.000s pid=42    WARN   WX        REPORTED: W+X mapping\n\
[     5] +     4.000s pid=43    INFO   EXEC      ALLOWED: /bin/ls\n\
[     6] +     5.000s pid=44    CRIT   INTEGRITY BLOCKED: hash mismatch /bin/su\n\
[     7] +     6.000s pid=45    WARN   ANOMALY   REPORTED: fork flood\n";

    #[test]
    fn the_report_reads_into_modes_counters_and_flags() {
        let st = parse(REPORT).unwrap();
        assert_eq!(
            st.modes,
            [
                Mode::Enforce,
                Mode::Report,
                Mode::Report,
                Mode::Off,
                Mode::Report,
                Mode::Enforce
            ]
        );
        assert!(!st.sealed);
        assert!(st.learning);
        assert_eq!((st.blocked, st.warnings, st.criticals), (3, 5, 2));
        assert_eq!((st.signatures, st.detections, st.pinned), (5002, 1, 7));
    }

    #[test]
    fn the_seal_is_read_off_the_enforcement_line() {
        let sealed = REPORT.replace("av=enforce\n", "av=enforce [tighten-only]\n");
        assert!(parse(&sealed).unwrap().sealed);
    }

    #[test]
    fn only_the_newest_warnings_and_criticals_are_kept_newest_first() {
        let st = parse(REPORT).unwrap();
        let seqs: Vec<u64> = st.recent.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, [7, 6, 4, 3]);
        assert!(st.recent[1].critical);
        assert_eq!(
            st.recent[1].text,
            "INTEGRITY BLOCKED: hash mismatch /bin/su"
        );
        assert_eq!(
            st.recent[3].text,
            "AV AV BLOCKED: /tmp/eicar.com (Eicar-Test-Signature)"
        );
    }

    #[test]
    fn an_empty_ring_lists_nothing() {
        let quiet = REPORT.split("recent events").next().unwrap().to_string()
            + "recent events (oldest first):\n(no security events recorded)\n";
        assert!(parse(&quiet).unwrap().recent.is_empty());
    }

    #[test]
    fn something_that_is_not_a_report_is_no_status() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("cpu  1 2 3 4\n"), None);
        // A mode hunter does not have is not silently read as off.
        assert_eq!(
            parse("enforcement: syscall=maybe wx=off exec=off anomaly=off integrity=off av=off\n"),
            None
        );
    }

    #[test]
    fn the_shield_says_alert_until_the_criticals_are_seen() {
        let st = parse(REPORT).unwrap();
        assert_eq!(st.shield(0), Shield::Alert);
        // Seen, but anomaly is off.
        assert_eq!(st.shield(2), Shield::Open);
        let mut all_on = st.clone();
        all_on.modes[3] = Mode::Report;
        assert_eq!(all_on.shield(2), Shield::Guarded);
    }

    #[test]
    fn relaxing_is_any_step_down_from_the_current_mode() {
        let st = parse(REPORT).unwrap();
        // syscall is at enforce.
        assert!(st.relaxes(0, Mode::Report));
        assert!(st.relaxes(0, Mode::Off));
        assert!(!st.relaxes(0, Mode::Enforce));
        // anomaly is off: nothing below it.
        assert!(!st.relaxes(3, Mode::Off));
        assert!(!st.relaxes(3, Mode::Enforce));
        assert!(!st.relaxes(99, Mode::Off));
    }

    #[test]
    fn the_scripts_are_the_ones_hunterctl_sends() {
        assert_eq!(
            mode_script(5, Mode::Enforce).as_deref(),
            Some("mode av enforce\n")
        );
        assert_eq!(mode_script(6, Mode::Off), None);
        // Same directives as `hunterctl lockdown`, same order, seal last.
        let ctl = include_str!("../../hunterctl/hunterctl");
        let block = ctl.split("lockdown)").nth(1).unwrap();
        let sent = block
            .split("send \"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(format!("{sent}\n"), LOCKDOWN);
    }

    #[test]
    fn every_domain_the_bar_offers_is_one_the_kernel_reports() {
        for d in DOMAINS {
            assert!(REPORT.contains(&format!(" {d}=")), "{d}");
        }
    }

    #[test]
    fn a_quote_in_the_home_path_cannot_break_out_of_the_scan_command() {
        let cmd = scan_command("/root/it's");
        assert!(cmd.starts_with("/sbin/hunterctl scan --quarantine '/root/its';"));
    }

    #[test]
    fn the_window_commands_keep_a_quote_in_home_out() {
        assert_eq!(
            app_scan_command("/root/it's"),
            "lunarhunter --quarantine --scan '/root/its'"
        );
        assert!(app_events_command().starts_with("lunarhunter "));
    }

    #[test]
    fn both_languages_name_the_three_modes() {
        for l in [Lang::Es, Lang::En] {
            assert!(text(l).modes.iter().all(|m| !m.is_empty()));
        }
    }
}
