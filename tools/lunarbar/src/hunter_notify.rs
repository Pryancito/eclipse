//! hunter's notifications: what the bar pops up, and the one door anything
//! else uses to pop one up too.
//!
//! Two things raise a notification:
//!
//! - **The kernel.** Every new warning or critical in hunter's event ring
//!   becomes one, with the actions that make sense for it ([`from_event`]):
//!   quarantine for a malware detection, allow for a program the allowlist
//!   refused, view and ignore for the rest. Nothing has to opt in: a new
//!   domain that records events (the firewall, the ransomware heuristics)
//!   gets its notifications for free, and its actions are one more arm in
//!   [`from_event`].
//! - **Any program, through a spool directory** ([`SPOOL`]). A producer drops
//!   one small `key=value` file per alert ([`Alert`], [`render`]); the bar
//!   shows it, runs the command behind the button the user pressed, and, when
//!   the producer asked for it, writes back which button that was. That is
//!   how a scheduled scan reports, and how the firewall asks "may X connect?"
//!   and waits for the answer. From a shell it is `hunterctl notify`; from
//!   Rust, [`post`] and [`take_answer`].
//!
//! Why a directory and not a socket or the kernel: the bar has no event loop
//! for anything but Wayland and a 1 Hz tick, a producer may run before the bar
//! does (or while it restarts), and a file per alert needs no daemon, no
//! protocol version and no client library — `printf` and `mv` will do.
//!
//! The spool's files can carry a command the bar will run, so the bar trusts
//! only what root (or its own user) wrote: the directory and every file must
//! belong to one of them and be writable by nobody else ([`pending`]).
//! Anything else is skipped, not deleted — it is not ours to remove.

use crate::hunter::Event;
use crate::hunter_firewall::{self as fw, Dir};
use crate::i18n::Lang;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Where producers drop their alerts. Under `/run` so a reboot forgets them.
pub const SPOOL: &str = "/run/hunter/notify";

/// An alert file's suffix; a reply's; and the prefix of a file still being
/// written (renamed into place when complete, so the bar never reads half).
pub const ALERT_EXT: &str = ".alert";
pub const ANSWER_EXT: &str = ".answer";

/// What the bar answers when the user closed the alert without pressing any of
/// its buttons, and when it expired.
pub const DISMISS: &str = "dismiss";
pub const TIMEOUT: &str = "timeout";

/// Buttons per alert: the card has room for three beside its close cross.
pub const MAX_ACTIONS: usize = 3;
/// No title, body, label or command is longer than this many characters; an
/// alert file longer than [`MAX_FILE`] bytes is not read at all.
const MAX_FIELD: usize = 512;
const MAX_FILE: u64 = 8192;
/// Alerts read from the spool per pass. A producer gone wrong can fill the
/// directory; the bar shows the oldest this many and leaves the rest queued.
pub const MAX_PENDING: usize = 32;

/// How urgent an alert is: its colour and how long it stays up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    #[default]
    Info,
    Warn,
    Crit,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Severity> {
        match s {
            "info" => Some(Severity::Info),
            "warn" | "warning" => Some(Severity::Warn),
            "crit" | "critical" => Some(Severity::Crit),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Crit => "crit",
        }
    }

    /// Seconds on screen when the alert does not say. A critical stays until
    /// someone deals with it: it is the one that needs a decision.
    pub fn default_timeout(self) -> Option<u32> {
        match self {
            Severity::Info => Some(6),
            Severity::Warn => Some(12),
            Severity::Crit => None,
        }
    }
}

/// One button.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Action {
    /// What the answer file says when it is pressed. `[A-Za-z0-9_.-]`.
    pub id: String,
    pub label: String,
    /// A `/bin/sh -c` command the bar runs when it is pressed, if any.
    pub run: Option<String>,
}

/// One notification.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Alert {
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub actions: Vec<Action>,
    /// Seconds on screen; `None` takes [`Severity::default_timeout`], and `0`
    /// means until the user closes it.
    pub timeout: Option<u32>,
    /// The producer waits for an answer file. Without it the bar writes none,
    /// so a producer that only informs leaves nothing behind in the spool.
    pub reply: bool,
}

impl Alert {
    /// The seconds this alert stays up, `None` for until it is closed.
    pub fn lifetime(&self) -> Option<u32> {
        match self.timeout {
            Some(0) => None,
            Some(s) => Some(s),
            None => self.severity.default_timeout(),
        }
    }
}

/// One line of a field: no control characters (a newline would start a new
/// key, a tab would split an action), capped at [`MAX_FIELD`].
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_FIELD)
        .collect::<String>()
        .trim()
        .to_string()
}

/// An id that is safe as a file name and as an answer: no slash, no dot in
/// front (those are files still being written), nothing a shell would read.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Read an alert file:
///
/// ```text
/// severity=crit
/// title=Malware detected
/// body=Eicar-Test-Signature in /root/eicar.com
/// timeout=30
/// reply=1
/// action=quarantine<TAB>Quarantine<TAB>/sbin/hunterctl quarantine '/root/eicar.com'
/// action=ignore<TAB>Ignore
/// ```
///
/// Unknown keys are skipped, so a newer producer still shows on an older bar.
/// `None` without a title, which is the one thing a card cannot do without.
pub fn parse(text: &str) -> Option<Alert> {
    let mut a = Alert::default();
    for line in text.lines() {
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "severity" => a.severity = Severity::parse(val.trim()).unwrap_or_default(),
            "title" => a.title = clean(val),
            "body" => a.body = clean(val),
            "timeout" => a.timeout = val.trim().parse().ok(),
            "reply" => a.reply = matches!(val.trim(), "1" | "yes" | "true"),
            "action" if a.actions.len() < MAX_ACTIONS => {
                let mut f = val.splitn(3, '\t');
                let id = f.next().unwrap_or("").trim();
                let label = clean(f.next().unwrap_or(""));
                let run = f.next().map(clean).filter(|r| !r.is_empty());
                if valid_id(id) && !label.is_empty() {
                    a.actions.push(Action {
                        id: id.to_string(),
                        label,
                        run,
                    });
                }
            }
            _ => {}
        }
    }
    (!a.title.is_empty()).then_some(a)
}

/// Write an alert in the format [`parse`] reads.
pub fn render(a: &Alert) -> String {
    let mut s = format!(
        "severity={}\ntitle={}\n",
        a.severity.as_str(),
        clean(&a.title)
    );
    if !a.body.is_empty() {
        s += &format!("body={}\n", clean(&a.body));
    }
    if let Some(t) = a.timeout {
        s += &format!("timeout={t}\n");
    }
    if a.reply {
        s += "reply=1\n";
    }
    for act in a.actions.iter().take(MAX_ACTIONS) {
        s += &format!("action={}\t{}", act.id, clean(&act.label));
        if let Some(run) = &act.run {
            s += &format!("\t{}", clean(run));
        }
        s.push('\n');
    }
    s
}

/// Drop `a` into the spool at `dir` and return its id. Written under a
/// dot-name and renamed, so the bar never reads a half-written alert.
pub fn post(dir: &Path, a: &Alert) -> std::io::Result<String> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    if a.actions.iter().any(|x| !valid_id(&x.id)) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let id = format!(
        "{secs}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = dir.join(format!(".{id}"));
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(render(a).as_bytes())?;
    }
    std::fs::rename(&tmp, dir.join(format!("{id}{ALERT_EXT}")))?;
    Ok(id)
}

/// The answer to alert `id`, once the bar wrote one; reading it removes it.
pub fn take_answer(dir: &Path, id: &str) -> Option<String> {
    if !valid_id(id) {
        return None;
    }
    let path = dir.join(format!("{id}{ANSWER_EXT}"));
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(text.trim().to_string())
}

/// Close alert `id`: remove its file and, when its producer waits for one,
/// write `answer` beside it (the answer first, so a producer polling for it
/// never sees the alert gone and no answer).
pub fn answer(dir: &Path, id: &str, reply: bool, answer: &str) {
    if !valid_id(id) {
        return;
    }
    if reply {
        let tmp = dir.join(format!(".{id}{ANSWER_EXT}"));
        if std::fs::write(&tmp, format!("{answer}\n")).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(format!("{id}{ANSWER_EXT}")));
        }
    }
    let _ = std::fs::remove_file(dir.join(format!("{id}{ALERT_EXT}")));
}

/// Whether a spool file or directory is one the bar may act on: owned by root
/// or by the bar's own user, and writable by no one else.
fn trusted(meta: &std::fs::Metadata) -> bool {
    let me = unsafe { libc::geteuid() };
    (meta.uid() == 0 || meta.uid() == me) && meta.mode() & 0o022 == 0
}

/// The alerts waiting in `dir`, oldest first (ids start with the time), at
/// most [`MAX_PENDING`]. Files the bar does not trust, or cannot read as an
/// alert, are left where they are.
pub fn pending(dir: &Path) -> Vec<(String, Alert)> {
    let Ok(meta) = std::fs::metadata(dir) else {
        return Vec::new();
    };
    if !meta.is_dir() || !trusted(&meta) {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = name.strip_suffix(ALERT_EXT)?;
            valid_id(id).then(|| id.to_string())
        })
        .collect();
    ids.sort();
    ids.into_iter()
        .filter_map(|id| {
            let path = dir.join(format!("{id}{ALERT_EXT}"));
            // symlink_metadata: a link is not a file someone trusted wrote.
            let m = std::fs::symlink_metadata(&path).ok()?;
            if !m.is_file() || !trusted(&m) || m.len() > MAX_FILE {
                return None;
            }
            Some((id, parse(&std::fs::read_to_string(&path).ok()?)?))
        })
        .take(MAX_PENDING)
        .collect()
}

// ---- Alerts from the kernel's event ring ---------------------------------

/// What a kernel alert's buttons do beyond running a command: the bar opens
/// the event log for [`VIEW`] and stops showing that exact event for the rest
/// of the session for [`IGNORE`].
pub const VIEW: &str = "view";
pub const IGNORE: &str = "ignore";
pub const QUARANTINE: &str = "quarantine";
pub const ALLOW: &str = "allow";
/// A firewall question's answers, and its view: the bar sends the rule and
/// saves the rule file itself ([`answer_net`]), so these carry no command.
pub const NET_ALLOW: &str = "net-allow";
pub const NET_DENY: &str = "net-deny";
pub const NET_VIEW: &str = "net-view";

/// The notification's words.
pub struct Text {
    pub quarantine: &'static str,
    pub allow: &'static str,
    pub ignore: &'static str,
    pub view: &'static str,
    /// `{n}` alerts not shown.
    pub more: &'static str,
    pub malware: &'static str,
    pub exec_blocked: &'static str,
    pub exec_untrusted: &'static str,
    pub integrity: &'static str,
    pub wx: &'static str,
    pub anomaly: &'static str,
    pub syscall: &'static str,
    pub other: &'static str,
    pub quarantined: &'static str,
    pub not_quarantined: &'static str,
    pub allowed: &'static str,
    pub not_allowed: &'static str,
    pub block: &'static str,
    pub net_ask: &'static str,
    pub net_allowed: &'static str,
    pub net_denied: &'static str,
    pub net_failed: &'static str,
    pub net_sealed: &'static str,
    pub net_unsaved: &'static str,
}

const ES: Text = Text {
    quarantine: "cuarentena",
    allow: "permitir",
    ignore: "ignorar",
    view: "ver",
    more: "más",
    malware: "Malware detectado",
    exec_blocked: "Programa bloqueado",
    exec_untrusted: "Programa no confiable",
    integrity: "Binario alterado",
    wx: "Memoria W+X",
    anomaly: "Comportamiento anómalo",
    syscall: "Llamada al sistema vetada",
    other: "Alerta de hunter",
    quarantined: "En cuarentena",
    not_quarantined: "No se pudo poner en cuarentena",
    allowed: "Permitido",
    not_allowed: "hunter no lo permitió",
    block: "bloquear",
    net_ask: "¿Permitir acceso a la red?",
    net_allowed: "Permitido y guardado",
    net_denied: "Bloqueado y guardado",
    net_failed: "hunter no aceptó la respuesta",
    net_sealed: "Sellado: no se puede permitir",
    net_unsaved: "Aplicado, pero no se guardó en /etc/hunter/firewall",
};

const EN: Text = Text {
    quarantine: "quarantine",
    allow: "allow",
    ignore: "ignore",
    view: "view",
    more: "more",
    malware: "Malware detected",
    exec_blocked: "Program blocked",
    exec_untrusted: "Untrusted program",
    integrity: "Tampered binary",
    wx: "W+X memory",
    anomaly: "Anomalous behaviour",
    syscall: "Syscall vetoed",
    other: "hunter alert",
    quarantined: "Quarantined",
    not_quarantined: "Could not quarantine",
    allowed: "Allowed",
    not_allowed: "hunter did not allow it",
    block: "block",
    net_ask: "Allow network access?",
    net_allowed: "Allowed and saved",
    net_denied: "Blocked and saved",
    net_failed: "hunter did not take the answer",
    net_sealed: "Sealed: cannot allow",
    net_unsaved: "Applied, but not saved to /etc/hunter/firewall",
};

pub fn text(lang: Lang) -> &'static Text {
    match lang {
        Lang::Es => &ES,
        Lang::En => &EN,
    }
}

/// `'s'` for `/bin/sh`, whatever is in it.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The first absolute path in an event's text, without the punctuation the
/// kernel puts after it. Paths with spaces come out cut at the space: the
/// ring does not quote them, so there is no telling where one ends.
fn first_path(desc: &str) -> Option<&str> {
    desc.split_whitespace()
        .find(|t| t.starts_with('/'))
        .map(|t| t.trim_end_matches([')', ',', ':', ';']))
        .filter(|p| p.len() > 1)
}

/// The path of the file the antivirus matched: `malware NAME in PATH`, with
/// ` (on-demand scan)` after it for a scan's detections.
fn malware_path(desc: &str) -> Option<&str> {
    let (_, rest) = desc.rsplit_once(" in ")?;
    let p = rest
        .strip_suffix(" (on-demand scan)")
        .unwrap_or(rest)
        .trim();
    (p.starts_with('/') && p.len() > 1).then_some(p)
}

/// A command that does `cmd` and reports how it went through the spool,
/// which is how the user learns whether the button worked.
fn and_report(cmd: &str, ok: &str, failed: &str, what: &str) -> String {
    let n = |sev: &str, title: &str| {
        format!(
            "/sbin/hunterctl notify --severity {sev} {} {}",
            sh_quote(title),
            sh_quote(what)
        )
    };
    format!("{cmd} && {} || {}", n("info", ok), n("warn", failed))
}

/// The program and direction a firewall question is about, out of its ring
/// text: `firewall: PATH wants to connect to PEER (...)`, or `wants to listen
/// on`. The path runs up to ` wants to `, so a space in it is kept.
pub fn net_question(desc: &str) -> Option<(&str, Dir)> {
    let rest = desc.strip_prefix("firewall: ")?;
    let (program, what) = rest.rsplit_once(" wants to ")?;
    let dir = if what.starts_with("connect to ") {
        Dir::Out
    } else if what.starts_with("listen on ") {
        Dir::In
    } else {
        return None;
    };
    program.starts_with('/').then_some((program, dir))
}

/// How answering a firewall question went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetAnswer {
    /// Sent, and `/etc/hunter/firewall` rewritten.
    Saved,
    /// Sent, but the rule file could not be written: it holds until reboot.
    Unsaved,
    /// hunter took the write and kept the question open: an `allow` under
    /// the seal, which is a relaxation.
    Sealed,
    /// The write failed (not root, no hunter), or the text is no question.
    Failed,
}

/// Answer the question a `NET ASK` event (its ring text, `NET ASK: ...`)
/// asked, the way lunarhunter's firewall tab does: the `net allow|deny`
/// directive to `/proc/hunter`, then the whole live rule set written back to
/// `/etc/hunter/firewall` so the answer survives a reboot. `when` dates the
/// file's header.
pub fn answer_net(event_text: &str, allow: bool, when: &str) -> NetAnswer {
    let desc = event_text.split_once(": ").map_or("", |(_, d)| d);
    let Some((program, dir)) = net_question(desc) else {
        return NetAnswer::Failed;
    };
    let q = fw::Question {
        dir,
        count: 0,
        pid: 0,
        first: 0,
        last: 0,
        peer: String::new(),
        program: program.to_string(),
    };
    if crate::hunter::send(&fw::answer_script(&q, allow)).is_err() {
        return NetAnswer::Failed;
    }
    let Some(f) = fw::read() else {
        return NetAnswer::Unsaved;
    };
    if allow
        && f.questions
            .iter()
            .any(|x| x.program == program && x.dir == dir)
    {
        return NetAnswer::Sealed;
    }
    match fw::save(&f, when, fw::RULES_FILE) {
        Ok(()) => NetAnswer::Saved,
        Err(_) => NetAnswer::Unsaved,
    }
}

/// The notification for one ring event, and its actions.
///
/// - Malware (the `AV` domain) with a file to point at: quarantine.
/// - A program the allowlist refused: allow — `hunterctl trust`, which is
///   exactly what that refusal asks for. Not for a program in an untrusted
///   location (`/tmp`, a world-writable directory): hunter checks those before
///   the allowlist, so trusting one would change nothing and the button would
///   lie. Not under the seal either: trusting is a relaxation and is refused.
/// - A firewall question (`NET ASK`): allow and block, which answer it and
///   save the rule ([`answer_net`]), and view, which opens the firewall tab.
///   It stays until answered: under `enforce` the program is waiting.
/// - Everything else gets view (the event log) and ignore.
pub fn from_event(e: &Event, sealed: bool, lang: Lang) -> Alert {
    let t = text(lang);
    // The ring's text is `CATEGORY ACTION: description`.
    let (cat, rest) = e.text.split_once(' ').unwrap_or((&e.text, ""));
    let (verb, desc) = rest.split_once(": ").unwrap_or(("", rest));
    if let ("NET", "ASK", Some(_)) = (cat, verb, net_question(desc)) {
        let act = |id: &str, label: &str| Action {
            id: id.into(),
            label: label.into(),
            run: None,
        };
        return Alert {
            severity: Severity::Warn,
            title: t.net_ask.into(),
            body: clean(desc.strip_prefix("firewall: ").unwrap_or(desc)),
            actions: vec![
                act(NET_ALLOW, t.allow),
                act(NET_DENY, t.block),
                act(NET_VIEW, t.view),
            ],
            timeout: Some(0),
            reply: false,
        };
    }
    let blocked = verb.contains("BLOCKED");
    let mut actions = Vec::new();
    let title = match cat {
        "AV" => {
            if let Some(p) = malware_path(desc) {
                actions.push(Action {
                    id: QUARANTINE.into(),
                    label: t.quarantine.into(),
                    run: Some(and_report(
                        &format!("/sbin/hunterctl quarantine {}", sh_quote(p)),
                        t.quarantined,
                        t.not_quarantined,
                        p,
                    )),
                });
            }
            t.malware
        }
        "EXEC" => {
            let allowlist = desc.contains("not on trusted-program allowlist");
            if let (true, false, Some(p)) = (allowlist, sealed, first_path(desc)) {
                actions.push(Action {
                    id: ALLOW.into(),
                    label: t.allow.into(),
                    run: Some(and_report(
                        &format!("/sbin/hunterctl trust {}", sh_quote(p)),
                        t.allowed,
                        t.not_allowed,
                        p,
                    )),
                });
            }
            if blocked {
                t.exec_blocked
            } else {
                t.exec_untrusted
            }
        }
        "INTEGRITY" => t.integrity,
        "WX" => t.wx,
        "ANOMALY" => t.anomaly,
        "SYSCALL" => t.syscall,
        _ => t.other,
    };
    actions.push(Action {
        id: VIEW.into(),
        label: t.view.into(),
        run: None,
    });
    actions.push(Action {
        id: IGNORE.into(),
        label: t.ignore.into(),
        run: None,
    });
    actions.truncate(MAX_ACTIONS);
    Alert {
        severity: if e.critical {
            Severity::Crit
        } else {
            Severity::Warn
        },
        title: title.to_string(),
        body: clean(if desc.is_empty() { &e.text } else { desc }),
        actions,
        timeout: None,
        reply: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(critical: bool, text: &str) -> Event {
        Event {
            seq: 1,
            critical,
            text: text.into(),
        }
    }

    fn ids(a: &Alert) -> Vec<&str> {
        a.actions.iter().map(|x| x.id.as_str()).collect()
    }

    #[test]
    fn an_alert_survives_a_round_trip_through_its_file() {
        let a = Alert {
            severity: Severity::Crit,
            title: "Firewall".into(),
            body: "¿permitir que /usr/bin/curl se conecte?".into(),
            actions: vec![
                Action {
                    id: "allow".into(),
                    label: "permitir".into(),
                    run: Some("/sbin/hunterctl net allow /usr/bin/curl".into()),
                },
                Action {
                    id: "deny".into(),
                    label: "denegar".into(),
                    run: None,
                },
            ],
            timeout: Some(30),
            reply: true,
        };
        assert_eq!(parse(&render(&a)), Some(a));
    }

    #[test]
    fn a_newline_in_a_field_cannot_inject_a_key() {
        let a = Alert {
            title: "x\naction=evil\tboom\trm -rf /".into(),
            ..Alert::default()
        };
        let back = parse(&render(&a)).unwrap();
        assert!(back.actions.is_empty());
        assert_eq!(back.title, "x action=evil boom rm -rf /");
    }

    #[test]
    fn what_a_card_cannot_show_is_dropped() {
        assert_eq!(parse("severity=crit\nbody=no title\n"), None);
        let a = parse(
            "title=t\naction=../x\tbad id\naction=ok\t\naction=a\tA\naction=b\tB\naction=c\tC\naction=d\tD\nfuture=1\n",
        )
        .unwrap();
        // A bad id and an empty label are skipped; at most three buttons.
        assert_eq!(ids(&a), ["a", "b", "c"]);
        assert_eq!(a.severity, Severity::Info);
    }

    #[test]
    fn hunterctl_notify_writes_what_this_reads() {
        let ctl = include_str!("../../hunterctl/hunterctl");
        assert!(ctl.contains(&format!("NOTIFY={SPOOL}\n")));
        for key in ["severity=", "title=", "body=", "timeout=", "reply=1"] {
            assert!(ctl.contains(&format!("echo \"{key}")), "{key}");
        }
        assert!(ctl.contains(r"printf 'action=%s\t%s\t%s\n'"));
        assert!(ctl.contains(&format!("$id{ALERT_EXT}")));
        assert!(ctl.contains(&format!("$id{ANSWER_EXT}")));
        // An action without a command, as that printf leaves it.
        let a = parse("title=t\naction=deny\tdenegar\t\n").unwrap();
        assert_eq!(a.actions[0].run, None);
    }

    #[test]
    fn lifetimes_follow_the_severity_unless_the_alert_says() {
        let mut a = Alert::default();
        assert_eq!(a.lifetime(), Some(6));
        a.severity = Severity::Crit;
        assert_eq!(a.lifetime(), None);
        a.timeout = Some(20);
        assert_eq!(a.lifetime(), Some(20));
        a.timeout = Some(0);
        assert_eq!(a.lifetime(), None);
    }

    #[test]
    fn ids_are_file_names_and_nothing_else() {
        assert!(valid_id("1730000000-42-0"));
        for bad in ["", ".hidden", "a/b", "a b", "x;rm", &"a".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn malware_offers_quarantine_of_the_file_it_names() {
        let a = from_event(
            &ev(
                true,
                "AV BLOCKED: malware Eicar-Test-Signature in /root/eicar.com",
            ),
            false,
            Lang::Es,
        );
        assert_eq!(a.severity, Severity::Crit);
        assert_eq!(a.title, "Malware detectado");
        assert_eq!(a.body, "malware Eicar-Test-Signature in /root/eicar.com");
        assert_eq!(ids(&a), [QUARANTINE, VIEW, IGNORE]);
        let run = a.actions[0].run.as_deref().unwrap();
        assert!(run.starts_with("/sbin/hunterctl quarantine '/root/eicar.com' && "));
        // And a scan's detection, which says so after the path.
        let a = from_event(
            &ev(
                false,
                "AV DETECT: malware X in /home/a b.bin (on-demand scan)",
            ),
            false,
            Lang::En,
        );
        assert!(a.actions[0]
            .run
            .as_deref()
            .unwrap()
            .contains("'/home/a b.bin'"));
    }

    #[test]
    fn a_quote_in_a_path_stays_inside_the_command() {
        let a = from_event(
            &ev(true, "AV BLOCKED: malware X in /tmp/it's"),
            false,
            Lang::En,
        );
        let run = a.actions[0].run.as_deref().unwrap();
        assert!(run.starts_with("/sbin/hunterctl quarantine '/tmp/it'\\''s' && "));
    }

    #[test]
    fn allow_is_offered_only_where_trusting_would_help() {
        let refused =
            "EXEC BLOCKED: blocked exec (not on trusted-program allowlist): /opt/tool/bin/run";
        let a = from_event(&ev(true, refused), false, Lang::Es);
        assert_eq!(ids(&a), [ALLOW, VIEW, IGNORE]);
        assert!(a.actions[0]
            .run
            .as_deref()
            .unwrap()
            .starts_with("/sbin/hunterctl trust '/opt/tool/bin/run' && "));
        assert_eq!(a.title, "Programa bloqueado");
        // Sealed: trusting is a relaxation hunter refuses.
        assert_eq!(
            ids(&from_event(&ev(true, refused), true, Lang::Es)),
            [VIEW, IGNORE]
        );
        // An untrusted location is checked before the allowlist.
        let tmp = "EXEC BLOCKED: blocked exec (untrusted path): /tmp/a.out";
        assert_eq!(
            ids(&from_event(&ev(true, tmp), false, Lang::Es)),
            [VIEW, IGNORE]
        );
        // Report mode: same offer, a warning, a different title.
        let rep = "EXEC WARNING: exec policy violation (not on trusted-program allowlist): /opt/x";
        let a = from_event(&ev(false, rep), false, Lang::Es);
        assert_eq!(
            (a.severity, a.title.as_str()),
            (Severity::Warn, "Programa no confiable")
        );
        assert_eq!(ids(&a), [ALLOW, VIEW, IGNORE]);
    }

    #[test]
    fn a_firewall_question_offers_allow_block_and_its_tab() {
        // As hunter/src/firewall.rs words it.
        let out = "NET ASK: firewall: /usr/bin/wget wants to connect to 1.2.3.4:443 \
                   (no rule; waiting for an answer)";
        let a = from_event(&ev(false, out), false, Lang::Es);
        assert_eq!(ids(&a), [NET_ALLOW, NET_DENY, NET_VIEW]);
        assert_eq!(a.title, "¿Permitir acceso a la red?");
        assert!(a
            .body
            .starts_with("/usr/bin/wget wants to connect to 1.2.3.4:443"));
        // It stays until answered.
        assert_eq!(a.lifetime(), None);
        assert!(a.actions.iter().all(|x| x.run.is_none()));
        assert_eq!(
            net_question(out.split_once(": ").unwrap().1),
            Some(("/usr/bin/wget", Dir::Out))
        );
        let listen = "firewall: /opt/my app/srv wants to listen on 0.0.0.0:8080 (no rule; allowed in report mode)";
        assert_eq!(net_question(listen), Some(("/opt/my app/srv", Dir::In)));
        // A NET event that is not a question is an ordinary alert.
        let other = from_event(
            &ev(false, "NET BLOCKED: firewall: /x denied"),
            false,
            Lang::En,
        );
        assert_eq!(ids(&other), [VIEW, IGNORE]);
        assert_eq!(
            net_question("firewall: relative wants to connect to x"),
            None
        );
    }

    #[test]
    fn answering_something_that_is_not_a_question_sends_nothing() {
        assert_eq!(answer_net("WX WARNING: x", true, "now"), NetAnswer::Failed);
    }

    #[test]
    fn every_other_event_can_be_viewed_or_ignored() {
        for text in [
            "WX WARNING: mprotect: W+X mapping",
            "ANOMALY WARNING: fork flood",
            "INTEGRITY BLOCKED: hash mismatch /bin/su",
            "NET BLOCKED: something new",
        ] {
            let a = from_event(&ev(false, text), false, Lang::En);
            assert_eq!(ids(&a), [VIEW, IGNORE], "{text}");
            assert!(!a.title.is_empty());
        }
    }

    #[test]
    fn the_first_path_drops_the_punctuation_after_it() {
        assert_eq!(
            first_path("blocked exec (untrusted path: /tmp/x (run as x))"),
            Some("/tmp/x")
        );
        assert_eq!(first_path("no path here /"), None);
    }

    /// A spool in a temporary directory, removed afterwards.
    struct Spool(std::path::PathBuf);
    impl Spool {
        fn new(tag: &str) -> Spool {
            let p =
                std::env::temp_dir().join(format!("lunarbar-notify-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            Spool(p)
        }
    }
    impl Drop for Spool {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn post_pending_answer_and_take_answer_close_the_loop() {
        use std::os::unix::fs::PermissionsExt;
        let s = Spool::new("loop");
        let a = Alert {
            title: "¿permitir?".into(),
            reply: true,
            actions: vec![Action {
                id: "yes".into(),
                label: "sí".into(),
                run: None,
            }],
            ..Alert::default()
        };
        let id = post(&s.0, &a).unwrap();
        let mode = std::fs::metadata(&s.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let got = pending(&s.0);
        assert_eq!(got, vec![(id.clone(), a)]);
        assert_eq!(take_answer(&s.0, &id), None);
        answer(&s.0, &id, true, "yes");
        assert!(pending(&s.0).is_empty());
        assert_eq!(take_answer(&s.0, &id).as_deref(), Some("yes"));
        // Taken once.
        assert_eq!(take_answer(&s.0, &id), None);
    }

    #[test]
    fn no_answer_file_is_left_for_a_producer_that_does_not_wait() {
        let s = Spool::new("noreply");
        let id = post(
            &s.0,
            &Alert {
                title: "scan done".into(),
                ..Alert::default()
            },
        )
        .unwrap();
        answer(&s.0, &id, false, DISMISS);
        assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
    }

    #[test]
    fn a_spool_anyone_can_write_to_is_not_read() {
        use std::os::unix::fs::PermissionsExt;
        let s = Spool::new("open");
        post(
            &s.0,
            &Alert {
                title: "x".into(),
                ..Alert::default()
            },
        )
        .unwrap();
        assert_eq!(pending(&s.0).len(), 1);
        std::fs::set_permissions(&s.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(pending(&s.0).is_empty());
        std::fs::set_permissions(&s.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        // And a single file anyone could have rewritten.
        let f = std::fs::read_dir(&s.0)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(pending(&s.0).is_empty());
    }

    #[test]
    fn half_written_files_and_strangers_are_skipped() {
        let s = Spool::new("skip");
        std::fs::create_dir_all(&s.0).unwrap();
        std::fs::write(s.0.join(".1-2-3"), "title=half\n").unwrap();
        std::fs::write(s.0.join("notes.txt"), "title=no\n").unwrap();
        std::fs::write(s.0.join("1-2-3.alert"), "body=no title\n").unwrap();
        assert!(pending(&s.0).is_empty());
    }
}
