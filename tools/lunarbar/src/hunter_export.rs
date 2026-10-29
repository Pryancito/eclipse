//! hunter's security report, exported: the stance, the alerts of the period,
//! the quarantine and the signatures, as a file someone can keep, mail or
//! print — HTML, PDF or plain text.
//!
//! `lunarhunter --export` and `hunterctl report` write it, and the "Exportar
//! informe" button in the window's Resumen tab does the same into `$HOME`.
//! Everything here is pure (values in, bytes out) so the tests read the three
//! formats without a kernel: [`gather`] is the only function that touches the
//! disk, and it only reads what `lunarhunter` already reads.
//!
//! The period is a week by default, but hunter keeps its event ring in kernel
//! memory: a reboot empties it, and a busy week overflows it (the report's
//! `dropped=` counter says by how much). `hunterd` copies the ring to
//! `/var/log/hunter/events.log*` every few seconds, so the events come from
//! there when it can be read, plus whatever the ring has that the last pass
//! has not written yet. Without it (no hunterd yet, or a reader that is not
//! root: the log is `0640`) the ring is all there is. Either way the events
//! section covers "the period, or from where the history starts if that is
//! later", and says which, instead of pretending to a week it does not have.
//! The quarantine index and the signature files are on disk and do cover the
//! whole period.
//!
//! PDF without a PDF library: the text rendering, paginated in Courier (one of
//! the fourteen fonts every PDF reader carries, so nothing is embedded) with
//! WinAnsi encoding, which covers Spanish. Plain, but it opens anywhere and
//! needs nothing on Eclipse OS. The HTML is the rich one, and prints well
//! from a browser too.

use crate::hunter::{Mode, DOMAINS};
use crate::hunter_report::{Feed, Quarantined, Report, RingEvent, Severity, SigFile};
use crate::i18n::Lang;

/// The period a report covers by default.
pub const DEFAULT_DAYS: u32 = 7;

/// Signatures older than this are called stale.
const STALE_SIGNATURES: u64 = 7 * 86_400;

/// Rows in each "top" table.
const TOP: usize = 10;

/// What a report is made from.
#[derive(Clone, Debug, Default)]
pub struct Input {
    pub report: Report,
    pub quarantine: Vec<Quarantined>,
    pub sig_files: Vec<SigFile>,
    pub feeds: Vec<Feed>,
    pub pins: usize,
    pub host: String,
    /// Seconds since the epoch, when the report was made.
    pub now: u64,
    /// Seconds since boot, which turns the ring's timestamps into dates.
    pub uptime: Option<f64>,
    pub days: u32,
    /// hunterd's history, oldest first; empty when there is none to read.
    pub history: Vec<Logged>,
    /// The first 8 characters of this boot's `boot_id`, as hunterd names it.
    pub boot_id: Option<String>,
}

/// Where hunterd keeps the history, and its rotations (`.4` the oldest).
pub const HISTORY: &str = "/var/log/hunter/events.log";
const HISTORY_ROTATIONS: u32 = 4;

/// One line of hunterd's `events.log`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Logged {
    /// Seconds since the epoch.
    pub time: u64,
    pub boot: String,
    /// `None` on hunterd's own lines (`HUNTERD BOOT`, `HUNTERD LOST`).
    pub seq: Option<u64>,
    /// The event as the ring would have shown it (`text` is `action: description`).
    pub ev: RingEvent,
}

/// Seconds since the epoch from `YYYY-MM-DDTHH:MM:SSZ`, the inverse of
/// [`civil_time`] (Hinnant's `days_from_civil`).
pub fn parse_iso(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, m, d) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// One `events.log` line: `time boot seq uptime pid severity category action
/// description`, tab-separated. The format is hunterd's (see "hunterd: events
/// and learning on disk" in docs/hunter-security.md).
pub fn parse_history_line(line: &str) -> Option<Logged> {
    let f: Vec<&str> = line.splitn(9, '\t').collect();
    if f.len() < 8 {
        return None;
    }
    let seq = match f[2] {
        "-" => None,
        n => Some(n.parse().ok()?),
    };
    let desc = f.get(8).copied().unwrap_or("");
    Some(Logged {
        time: parse_iso(f[0])?,
        boot: f[1].to_string(),
        seq,
        ev: RingEvent {
            seq: seq.unwrap_or(0),
            ts: f[3].to_string(),
            pid: f[4].parse().ok()?,
            sev: Severity::parse(f[5])?,
            category: f[6].to_string(),
            text: if desc.is_empty() {
                f[7].to_string()
            } else {
                format!("{}: {desc}", f[7])
            },
        },
    })
}

pub fn parse_history(text: &str) -> Vec<Logged> {
    text.lines().filter_map(parse_history_line).collect()
}

/// The three files a report can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Html,
    Pdf,
    Text,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s.to_ascii_lowercase().as_str() {
            "html" | "htm" => Some(Format::Html),
            "pdf" => Some(Format::Pdf),
            "txt" | "text" | "texto" => Some(Format::Text),
            _ => None,
        }
    }

    /// By the file's extension; HTML when it has none we know.
    pub fn from_path(path: &str) -> Format {
        path.rsplit_once('.')
            .filter(|(_, ext)| !ext.contains('/'))
            .and_then(|(_, ext)| Format::parse(ext))
            .unwrap_or(Format::Html)
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Html => "html",
            Format::Pdf => "pdf",
            Format::Text => "txt",
        }
    }
}

fn tr(lang: Lang, es: &'static str, en: &'static str) -> &'static str {
    match lang {
        Lang::Es => es,
        Lang::En => en,
    }
}

/// `YYYY-MM-DD HH:MM` (UTC) from seconds since the epoch, with no libc locale
/// or timezone database: Howard Hinnant's `civil_from_days`.
pub fn civil_time(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// The default file name for a report made at `now`: one per day, so the
/// weekly one does not overwrite last week's.
pub fn file_name(now: u64, fmt: Format) -> String {
    format!("hunter-informe-{}.{}", &civil_time(now)[..10], fmt.ext())
}

// ── what the report says ─────────────────────────────────────────────────────

/// Everything the three renderings show, worked out once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Seconds since the epoch the events section starts at.
    pub from: u64,
    /// The events do not reach back to the start of the period: `from` is
    /// the boot (ring only) or the start of hunterd's history, not
    /// `now - days`.
    pub since_boot: bool,
    /// The events come from hunterd's history (plus the ring's newest).
    pub from_disk: bool,
    /// The ring's events in the period, oldest first, each with its date when
    /// the uptime is known.
    pub events: Vec<(Option<u64>, RingEvent)>,
    /// Per severity, indexed Info, Notice, Warn, Crit.
    pub by_sev: [usize; 4],
    /// Warnings and criticals per category, most first.
    pub categories: Vec<(String, usize)>,
    /// Warnings and criticals per subject (what follows `action:`), most first.
    pub subjects: Vec<(String, usize)>,
    /// Quarantined in the period.
    pub quarantined: Vec<Quarantined>,
    /// Age of the newest signature file, in seconds.
    pub sig_age: Option<u64>,
    pub recommendations: Vec<String>,
}

/// What an alert is about: the text after `ACTION: `, without a trailing
/// parenthesis (`/tmp/x (EICAR-Test-File)` is about `/tmp/x`).
fn subject(text: &str) -> String {
    let s = text.split_once(": ").map_or(text, |(_, s)| s).trim();
    let s = match s.rsplit_once(" (") {
        Some((head, tail)) if tail.ends_with(')') => head,
        _ => s,
    };
    s.to_string()
}

fn ranked(counts: std::collections::BTreeMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<_> = counts.into_iter().collect();
    // Most first; a tie by name, which the map already sorted.
    v.sort_by_key(|e| std::cmp::Reverse(e.1));
    v.truncate(TOP);
    v
}

pub fn summarize(inp: &Input, lang: Lang) -> Summary {
    let r = &inp.report;
    let start = inp.now.saturating_sub(u64::from(inp.days) * 86_400);
    let boot = inp.uptime.map(|u| inp.now.saturating_sub(u as u64));
    let date_of = |e: &RingEvent| -> Option<u64> {
        let ts: f64 = e.ts.parse().ok()?;
        Some(boot? + ts as u64)
    };
    let from_disk = !inp.history.is_empty();
    // Where the events begin: the oldest line on disk, or the boot.
    let begins = if from_disk {
        inp.history.first().map(|l| l.time)
    } else {
        boot
    };
    let since_boot = begins.is_none_or(|b| b > start);
    let from = begins.map_or(start, |b| b.max(start));

    // The ring's events the history does not have yet: by sequence number
    // when this boot's id is known (seq restarts every boot), otherwise by
    // date, after the last line written.
    let written = |e: &RingEvent, d: Option<u64>| -> bool {
        if !from_disk {
            return false;
        }
        if let Some(id) = &inp.boot_id {
            return inp
                .history
                .iter()
                .any(|l| &l.boot == id && l.seq.is_some_and(|q| q >= e.seq));
        }
        let last = inp.history.last().map_or(0, |l| l.time);
        d.is_none_or(|d| d <= last)
    };
    let ring = r
        .events
        .iter()
        .map(|e| (date_of(e), e.clone()))
        .filter(|(d, e)| !written(e, *d));
    // hunterd's BOOT lines are bookkeeping; its LOST lines are kept, since
    // events nobody saw are worth a line in a security report.
    let disk = inp
        .history
        .iter()
        .filter(|l| !(l.ev.category == "HUNTERD" && l.ev.text.starts_with("BOOT")))
        .map(|l| (Some(l.time), l.ev.clone()));
    let events: Vec<(Option<u64>, RingEvent)> = disk
        .chain(ring)
        .filter(|(d, _)| d.is_none_or(|d| d >= start))
        .collect();

    let mut by_sev = [0usize; 4];
    let mut cats = std::collections::BTreeMap::new();
    let mut subs = std::collections::BTreeMap::new();
    for (_, e) in &events {
        by_sev[e.sev as usize] += 1;
        if e.sev >= Severity::Warn {
            *cats.entry(e.category.clone()).or_insert(0) += 1;
            *subs.entry(subject(&e.text)).or_insert(0) += 1;
        }
    }

    // `hunterctl quarantine` dates the index with the local `date`, a
    // sortable `YYYY-MM-DDTHH:MM:SS`, so the day compares as text.
    let first_day = civil_time(start)[..10].to_string();
    let quarantined: Vec<Quarantined> = inp
        .quarantine
        .iter()
        .filter(|q| q.date.get(..10).is_some_and(|d| d >= first_day.as_str()))
        .cloned()
        .collect();

    let sig_age = inp
        .sig_files
        .iter()
        .filter_map(|f| f.mtime)
        .max()
        .map(|m| inp.now.saturating_sub(m));

    let mut s = Summary {
        from,
        since_boot,
        from_disk,
        events,
        by_sev,
        categories: ranked(cats),
        subjects: ranked(subs),
        quarantined,
        sig_age,
        recommendations: Vec::new(),
    };
    s.recommendations = recommendations(inp, &s, lang);
    s
}

/// What the operator should do about what the report shows, most urgent
/// first. Every line names the command that does it, so the report is
/// something to act on and not only to read.
fn recommendations(inp: &Input, s: &Summary, lang: Lang) -> Vec<String> {
    let r = &inp.report;
    let st = &r.status;
    let t = |es, en| tr(lang, es, en);
    let mut out = Vec::new();
    if s.by_sev[Severity::Crit as usize] > 0 || st.detections > 0 {
        out.push(format!(
            "{} {} {}",
            t("Revisa las", "Review the"),
            s.by_sev[Severity::Crit as usize],
            t(
                "alertas críticas del periodo (más abajo, o `hunterctl events`).",
                "critical alerts of the period (below, or `hunterctl events`)."
            )
        ));
    }
    for (d, m) in DOMAINS.iter().zip(st.modes.iter()) {
        if *m == Mode::Off {
            out.push(format!(
                "{d}: {} `hunterctl mode {d} report`.",
                t(
                    "apagado, no ve nada. Al menos que avise:",
                    "off, it sees nothing. At least let it report:"
                )
            ));
        }
    }
    let soft: Vec<&str> = [2usize, 4, 5]
        .into_iter()
        .filter(|&d| st.modes[d] == Mode::Report)
        .map(|d| DOMAINS[d])
        .collect();
    if !soft.is_empty() {
        out.push(format!(
            "{}: {}. {}",
            t("Solo avisan", "Only reporting"),
            soft.join(", "),
            t(
                "Si no hubo falsos positivos, `hunterctl mode <dominio> enforce`.",
                "If there were no false positives, `hunterctl mode <domain> enforce`."
            )
        ));
    }
    match s.sig_age {
        None => out.push(
            t(
                "No hay ficheros de firmas: `hunterctl update-signatures`.",
                "There are no signature files: `hunterctl update-signatures`.",
            )
            .into(),
        ),
        Some(age) if age > STALE_SIGNATURES => out.push(format!(
            "{} {} {}",
            t("Las firmas tienen", "The signatures are"),
            age / 86_400,
            t(
                "días: `hunterctl update-signatures`.",
                "days old: `hunterctl update-signatures`."
            )
        )),
        _ => {}
    }
    if st.learning {
        out.push(
            t(
                "El aprendizaje sigue activo: guarda lo aprendido (`hunterctl save-learned`) y apágalo (`hunterctl learning off`).",
                "Learning is still on: save what it learned (`hunterctl save-learned`) and turn it off (`hunterctl learning off`).",
            )
            .into(),
        );
    }
    if r.dropped > 0 || r.critical_dropped > 0 {
        out.push(format!(
            "{} {} ({} {}).",
            t(
                "El registro de eventos se desbordó y perdió",
                "The event ring overflowed and lost"
            ),
            r.dropped,
            r.critical_dropped,
            t("críticos", "critical")
        ));
    }
    if inp.pins == 0 {
        out.push(
            t(
                "Ningún programa tiene el hash fijado: `hunterctl hash /bin/* /sbin/*`.",
                "No program has its hash pinned: `hunterctl hash /bin/* /sbin/*`.",
            )
            .into(),
        );
    } else if !r.require_hash {
        out.push(
            t(
                "Un programa sin hash fijado se ejecuta igual: valora `hunterctl require-hash on`.",
                "An unpinned program still runs: consider `hunterctl require-hash on`.",
            )
            .into(),
        );
    }
    if !st.sealed {
        out.push(
            t(
                "La política no está sellada: cuando esté afinada, `hunterctl lockdown`.",
                "The policy is not sealed: once it is tuned, `hunterctl lockdown`.",
            )
            .into(),
        );
    }
    out
}

/// The stance in one line, as the window's headline says it.
fn stance(r: &Report, lang: Lang) -> (String, &'static str) {
    let s = &r.status;
    let off = s.modes.iter().filter(|&&m| m == Mode::Off).count();
    let enforcing = s.modes.iter().filter(|&&m| m == Mode::Enforce).count();
    if off > 0 {
        (
            format!(
                "{} {off} {}",
                tr(lang, "Expuesto:", "Exposed:"),
                tr(lang, "dominio(s) apagado(s)", "domain(s) off")
            ),
            "crit",
        )
    } else if enforcing == DOMAINS.len() {
        (
            tr(
                lang,
                "Protegido: todo bloquea",
                "Protected: everything enforces",
            )
            .into(),
            "ok",
        )
    } else {
        (
            format!(
                "{} {} {}",
                tr(lang, "Vigilando:", "Watching:"),
                DOMAINS.len() - enforcing,
                tr(lang, "dominio(s) solo avisan", "domain(s) only report")
            ),
            "warn",
        )
    }
}

/// Where the events come from and how far back they reach, when it is not
/// simply "the whole period, from the history on disk".
fn coverage(s: &Summary, lang: Lang) -> Option<String> {
    let t = |es, en| tr(lang, es, en);
    let rest = t(
        "La cuarentena y las firmas sí cubren el periodo entero.",
        "The quarantine and the signatures do cover the whole period.",
    );
    match (s.from_disk, s.since_boot) {
        (true, false) => None,
        (true, true) => Some(format!(
            "{} {} UTC. {rest}",
            t(
                "El historial de hunterd (/var/log/hunter) empieza el",
                "hunterd's history (/var/log/hunter) starts on"
            ),
            civil_time(s.from)
        )),
        (false, false) => Some(
            t(
                "Eventos tomados del anillo de hunter en memoria (no se pudo leer /var/log/hunter).",
                "Events taken from hunter's in-memory ring (/var/log/hunter could not be read).",
            )
            .into(),
        ),
        (false, true) => Some(format!(
            "{} {} UTC. {rest}",
            t(
                "Sin historial en disco, los eventos son los del anillo en memoria: desde el arranque, el",
                "With no history on disk, the events are the in-memory ring's: since boot, on"
            ),
            civil_time(s.from)
        )),
    }
}

fn when(d: Option<u64>, e: &RingEvent) -> String {
    match d {
        Some(d) => civil_time(d),
        None => format!("+{}s", e.ts),
    }
}

fn onoff(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

// ── HTML ─────────────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// `code` spans for the backquoted commands in a recommendation.
fn esc_code(s: &str) -> String {
    esc(s)
        .split('`')
        .enumerate()
        .map(|(i, part)| {
            if i % 2 == 1 {
                format!("<code>{part}</code>")
            } else {
                part.to_string()
            }
        })
        .collect()
}

fn sev_class(s: Severity) -> &'static str {
    match s {
        Severity::Info => "info",
        Severity::Notice => "notice",
        Severity::Warn => "warn",
        Severity::Crit => "crit",
    }
}

/// Light only, on purpose: it is a document to print and archive, and a dark
/// page prints as a block of toner.
const CSS: &str = "
:root{--fg:#1d2330;--dim:#5b6475;--line:#d9dee7;--card:#f4f6fa;--accent:#2563c9;
--ok:#1f8a4c;--warn:#b26a00;--crit:#c42b2b}
*{box-sizing:border-box}
body{margin:0;background:#fff;color:var(--fg);font:15px/1.5 system-ui,-apple-system,'Segoe UI',sans-serif}
main{max-width:980px;margin:0 auto;padding:32px 16px 48px}
header{border-bottom:3px solid var(--accent);padding-bottom:12px;margin-bottom:20px}
h1{margin:0;font-size:26px}h2{font-size:18px;margin:32px 0 10px;border-bottom:1px solid var(--line);padding-bottom:4px}
.meta{color:var(--dim);font-size:13px}
.stance{font-size:18px;font-weight:600;margin:14px 0 4px}
.ok{color:var(--ok)}.warn{color:var(--warn)}.crit{color:var(--crit)}.info,.notice{color:var(--dim)}
.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:10px}
.card{background:var(--card);border-radius:8px;padding:10px 12px}
.card b{display:block;font-size:24px}.card span{color:var(--dim);font-size:13px}
.bar{display:flex;height:12px;border-radius:6px;overflow:hidden;background:var(--card);margin:12px 0 4px}
.bar i{display:block}.bar .i{background:#9aa5b8}.bar .n{background:#6b8fd6}.bar .w{background:#e0a030}.bar .c{background:#d94848}
table{width:100%;border-collapse:collapse;font-size:13.5px}
th,td{text-align:left;padding:5px 8px;border-bottom:1px solid var(--line);vertical-align:top}
th{color:var(--dim);font-weight:600}td.n{text-align:right;font-variant-numeric:tabular-nums}
td.p{word-break:break-all}
code{font:12.5px ui-monospace,Menlo,Consolas,monospace;background:var(--card);padding:1px 4px;border-radius:4px}
ul.rec li{margin:4px 0}
.note{color:var(--dim);font-size:13px}
details summary{cursor:pointer;color:var(--accent)}
.two{display:grid;grid-template-columns:1fr 1fr;gap:24px}
@media (max-width:640px){.two{grid-template-columns:1fr}}
@media print{main{padding:0}h2{break-after:avoid}tr{break-inside:avoid}details{display:block}details>*{display:block}}
";

pub fn html(inp: &Input, lang: Lang) -> String {
    let r = &inp.report;
    let st = &r.status;
    let s = summarize(inp, lang);
    let t = |es, en| tr(lang, es, en);
    let mut h = String::new();
    let title = t("Informe de seguridad de hunter", "hunter security report");
    h.push_str(&format!(
        "<!doctype html>\n<html lang=\"{}\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title} — {}</title><style>{CSS}</style></head><body><main>\n",
        if lang == Lang::Es { "es" } else { "en" },
        esc(&inp.host),
    ));

    // Header and the stance.
    let (line, class) = stance(r, lang);
    h.push_str(&format!(
        "<header><h1>{title}</h1><div class=\"meta\">{} <b>{}</b> · hunter v{} · {} {} UTC · {} {} {} {} UTC</div>\
         <div class=\"stance {class}\">{}</div><div class=\"meta\">{} {} · {} {} · {} {}</div></header>\n",
        t("Equipo", "Host"),
        esc(&inp.host),
        esc(&r.version),
        t("generado el", "generated on"),
        civil_time(inp.now),
        t("periodo:", "period:"),
        inp.days,
        t("días, desde el", "days, since"),
        civil_time(inp.now.saturating_sub(u64::from(inp.days) * 86_400)),
        esc(&line),
        t("aprendizaje", "learning"),
        onoff(st.learning),
        t("lista de ejecución", "exec allowlist"),
        if r.allowlist_active { t("activa", "active") } else { t("inactiva", "inactive") },
        t("sello", "seal"),
        onoff(st.sealed),
    ));

    // The cards.
    let crit = s.by_sev[Severity::Crit as usize];
    let cards: [(&str, String, &str); 8] = [
        (
            t("eventos en el periodo", "events in the period"),
            s.events.len().to_string(),
            "",
        ),
        (
            t("avisos", "warnings"),
            s.by_sev[Severity::Warn as usize].to_string(),
            if s.by_sev[2] > 0 { "warn" } else { "" },
        ),
        (
            t("críticos", "critical"),
            crit.to_string(),
            if crit > 0 { "crit" } else { "" },
        ),
        (
            t("bloqueos (desde el arranque)", "blocked (since boot)"),
            st.blocked.to_string(),
            "",
        ),
        (
            t("detecciones del antivirus", "antivirus detections"),
            st.detections.to_string(),
            if st.detections > 0 { "crit" } else { "" },
        ),
        (
            t("en cuarentena en el periodo", "quarantined in the period"),
            s.quarantined.len().to_string(),
            "",
        ),
        (
            t("firmas cargadas", "signatures loaded"),
            st.signatures.to_string(),
            "",
        ),
        (
            t("programas fijados", "pinned programs"),
            inp.pins.to_string(),
            "",
        ),
    ];
    h.push_str("<div class=\"cards\">");
    for (cap, val, class) in cards {
        h.push_str(&format!(
            "<div class=\"card\"><b class=\"{class}\">{val}</b><span>{cap}</span></div>"
        ));
    }
    h.push_str("</div>\n");
    let total = s.events.len().max(1) as f64;
    h.push_str("<div class=\"bar\">");
    for (i, c) in ["i", "n", "w", "c"].iter().enumerate() {
        if s.by_sev[i] > 0 {
            h.push_str(&format!(
                "<i class=\"{c}\" style=\"width:{:.2}%\"></i>",
                s.by_sev[i] as f64 * 100.0 / total
            ));
        }
    }
    h.push_str(&format!(
        "</div><div class=\"note\">INFO {} · NOTICE {} · WARN {} · CRIT {}</div>\n",
        s.by_sev[0], s.by_sev[1], s.by_sev[2], s.by_sev[3]
    ));
    if let Some(note) = coverage(&s, lang) {
        h.push_str(&format!("<p class=\"note\">{}</p>\n", esc(&note)));
    }

    // What to do.
    h.push_str(&format!(
        "<h2>{}</h2>",
        t("Recomendaciones", "Recommendations")
    ));
    if s.recommendations.is_empty() {
        h.push_str(&format!(
            "<p class=\"ok\">{}</p>\n",
            t("Nada pendiente.", "Nothing pending.")
        ));
    } else {
        h.push_str("<ul class=\"rec\">");
        for rec in &s.recommendations {
            h.push_str(&format!("<li>{}</li>", esc_code(rec)));
        }
        h.push_str("</ul>\n");
    }

    // The stance in detail.
    h.push_str(&format!(
        "<h2>{}</h2><div class=\"two\"><table><tr><th>{}</th><th>{}</th></tr>",
        t("Protección", "Protection"),
        t("dominio", "domain"),
        t("modo", "mode")
    ));
    for (d, m) in DOMAINS.iter().zip(st.modes.iter()) {
        let class = match m {
            Mode::Off => "crit",
            Mode::Report => "warn",
            Mode::Enforce => "ok",
        };
        h.push_str(&format!(
            "<tr><td>{d}</td><td class=\"{class}\">{}</td></tr>",
            m.as_str()
        ));
    }
    h.push_str(&format!(
        "</table><table><tr><th>{}</th><th></th></tr>",
        t("interruptor", "switch")
    ));
    let switches = [
        (t("aprendizaje", "learning"), st.learning),
        (
            t("detección de anomalías", "anomaly detection"),
            r.anomaly_detection,
        ),
        (
            t("denegar privilegiadas", "privileged deny"),
            r.privileged_deny,
        ),
        (t("exigir hash", "require hash"), r.require_hash),
        (t("sello", "seal"), st.sealed),
    ];
    for (name, on) in switches {
        h.push_str(&format!("<tr><td>{name}</td><td>{}</td></tr>", onoff(on)));
    }
    h.push_str(&format!(
        "<tr><td>{}</td><td>{} + {} {}</td></tr></table></div>\n",
        t("lista de ejecución", "exec allowlist"),
        r.trusted,
        r.learned,
        t("aprendidos", "learned"),
    ));

    // Who raised the alerts.
    if !s.categories.is_empty() {
        h.push_str(&format!(
            "<h2>{}</h2><div class=\"two\"><table><tr><th>{}</th><th class=\"n\">{}</th></tr>",
            t("De dónde vienen las alertas", "Where the alerts come from"),
            t("categoría", "category"),
            t("alertas", "alerts")
        ));
        for (c, n) in &s.categories {
            h.push_str(&format!(
                "<tr><td>{}</td><td class=\"n\">{n}</td></tr>",
                esc(c)
            ));
        }
        h.push_str(&format!(
            "</table><table><tr><th>{}</th><th class=\"n\">{}</th></tr>",
            t("sobre qué", "about what"),
            t("alertas", "alerts")
        ));
        for (c, n) in &s.subjects {
            h.push_str(&format!(
                "<tr><td class=\"p\">{}</td><td class=\"n\">{n}</td></tr>",
                esc(c)
            ));
        }
        h.push_str("</table></div>\n");
    }

    // The alerts themselves, newest first.
    let alerts: Vec<_> = s
        .events
        .iter()
        .rev()
        .filter(|(_, e)| e.sev >= Severity::Warn)
        .collect();
    h.push_str(&format!(
        "<h2>{} ({})</h2>",
        t("Alertas", "Alerts"),
        alerts.len()
    ));
    if alerts.is_empty() {
        h.push_str(&format!(
            "<p class=\"ok\">{}</p>\n",
            t("Ninguna alerta en el periodo.", "No alerts in the period.")
        ));
    } else {
        h.push_str(&event_table(&alerts, lang));
    }

    // The quarantine.
    h.push_str(&format!(
        "<h2>{} ({})</h2>",
        t("Cuarentena del periodo", "Quarantined in the period"),
        s.quarantined.len()
    ));
    if s.quarantined.is_empty() {
        h.push_str(&format!(
            "<p class=\"note\">{} {}.</p>\n",
            t(
                "Nada puesto en cuarentena. En total hay",
                "Nothing put away. In all there are"
            ),
            inp.quarantine.len()
        ));
    } else {
        h.push_str(&format!(
            "<table><tr><th>{}</th><th>{}</th><th>SHA-256</th></tr>",
            t("fecha", "date"),
            t("ruta original", "original path")
        ));
        for q in s.quarantined.iter().rev() {
            h.push_str(&format!(
                "<tr><td>{}</td><td class=\"p\">{}</td><td><code>{}…</code></td></tr>",
                esc(&q.date.replace('T', " ")),
                esc(&q.path),
                esc(q.sha256.get(..16).unwrap_or(&q.sha256))
            ));
        }
        h.push_str("</table>\n");
    }

    // The signatures.
    h.push_str(&format!(
        "<h2>{}</h2><p class=\"note\">{} {} ({} hash, {} {})</p>",
        t("Firmas", "Signatures"),
        t("El núcleo tiene cargadas", "The kernel has loaded"),
        st.signatures,
        r.sig_hash,
        r.sig_bytes,
        t("patrones", "patterns")
    ));
    if !inp.sig_files.is_empty() {
        h.push_str(&format!(
            "<table><tr><th>{}</th><th class=\"n\">hash</th><th class=\"n\">{}</th><th>{}</th></tr>",
            t("fichero", "file"),
            t("patrones", "patterns"),
            t("actualizado", "updated")
        ));
        for f in &inp.sig_files {
            h.push_str(&format!(
                "<tr><td class=\"p\">{}</td><td class=\"n\">{}</td><td class=\"n\">{}</td><td>{}</td></tr>",
                esc(&f.path),
                f.hashes,
                f.patterns,
                f.mtime.map(civil_time).unwrap_or_default()
            ));
        }
        h.push_str("</table>\n");
    }
    if !inp.feeds.is_empty() {
        h.push_str(&format!("<p class=\"note\">{} ", t("Fuentes:", "Feeds:")));
        let names: Vec<String> = inp.feeds.iter().map(|f| esc(&f.name)).collect();
        h.push_str(&names.join(", "));
        h.push_str("</p>\n");
    }

    // Learned programs: what learning let in without anyone deciding.
    if !r.learned_programs.is_empty() {
        h.push_str(&format!(
            "<h2>{} ({})</h2><p class=\"note\">{}</p><ul>",
            t("Programas aprendidos", "Learned programs"),
            r.learned_programs.len(),
            t(
                "Confiados al primer uso, sin que nadie los aprobara: revísalos antes de `hunterctl save-learned`.",
                "Trusted on first use with nobody approving them: review them before `hunterctl save-learned`."
            )
            .replace('`', "")
        ));
        for p in &r.learned_programs {
            h.push_str(&format!("<li><code>{}</code></li>", esc(p)));
        }
        h.push_str("</ul>\n");
    }

    // Every event, folded away.
    h.push_str(&format!(
        "<h2>{}</h2><details><summary>{} ({})</summary>",
        t("Registro completo", "Full log"),
        t("mostrar todos los eventos", "show every event"),
        s.events.len()
    ));
    let all: Vec<_> = s.events.iter().rev().collect();
    h.push_str(&event_table(&all, lang));
    h.push_str(&format!(
        "</details>\n<p class=\"note\">{}</p>\n</main></body></html>\n",
        t(
            "Generado por lunarhunter a partir de /proc/hunter. Para un PDF: `hunterctl report --pdf`, o imprime esta página.",
            "Made by lunarhunter from /proc/hunter. For a PDF: `hunterctl report --pdf`, or print this page."
        )
        .replace('`', "")
    ));
    h
}

fn event_table(rows: &[&(Option<u64>, RingEvent)], lang: Lang) -> String {
    let t = |es, en| tr(lang, es, en);
    let mut h = format!(
        "<table><tr><th>{}</th><th>{}</th><th>{}</th><th>pid</th><th>{}</th></tr>",
        t("cuándo", "when"),
        t("gravedad", "severity"),
        t("categoría", "category"),
        t("qué", "what")
    );
    for (d, e) in rows {
        h.push_str(&format!(
            "<tr><td>{}</td><td class=\"{}\">{}</td><td>{}</td><td class=\"n\">{}</td><td class=\"p\">{}</td></tr>",
            when(*d, e),
            sev_class(e.sev),
            e.sev.as_str(),
            esc(&e.category),
            e.pid,
            esc(&e.text)
        ));
    }
    h.push_str("</table>\n");
    h
}

// ── plain text, and the PDF made of it ───────────────────────────────────────

/// One line of the text rendering; headings are bold in the PDF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub heading: bool,
    pub text: String,
}

fn line(text: impl Into<String>) -> Line {
    Line {
        heading: false,
        text: text.into(),
    }
}

fn heading(text: impl Into<String>) -> Line {
    Line {
        heading: true,
        text: text.into(),
    }
}

pub fn lines(inp: &Input, lang: Lang) -> Vec<Line> {
    let r = &inp.report;
    let st = &r.status;
    let s = summarize(inp, lang);
    let t = |es, en| tr(lang, es, en);
    let mut o = vec![
        heading(t(
            "INFORME DE SEGURIDAD DE HUNTER",
            "HUNTER SECURITY REPORT",
        )),
        line(format!(
            "{} {} · hunter v{} · {} {} UTC",
            t("Equipo", "Host"),
            inp.host,
            r.version,
            t("generado el", "generated on"),
            civil_time(inp.now)
        )),
        line(format!(
            "{} {} {} {} UTC",
            t("Periodo:", "Period:"),
            inp.days,
            t("días, desde el", "days, since"),
            civil_time(inp.now.saturating_sub(u64::from(inp.days) * 86_400))
        )),
        line(""),
        heading(stance(r, lang).0),
        line(format!(
            "{} {} · {} {} · {} {}",
            s.events.len(),
            t("eventos", "events"),
            s.by_sev[2],
            t("avisos", "warnings"),
            s.by_sev[3],
            t("críticos", "critical"),
        )),
        line(format!(
            "{} {} · {} {} · {} {} · {} {}",
            t("bloqueos desde el arranque:", "blocked since boot:"),
            st.blocked,
            t("detecciones:", "detections:"),
            st.detections,
            t("en cuarentena:", "quarantined:"),
            s.quarantined.len(),
            t("firmas:", "signatures:"),
            st.signatures
        )),
    ];
    if let Some(note) = coverage(&s, lang) {
        o.push(line(note));
    }

    o.push(line(""));
    o.push(heading(t("RECOMENDACIONES", "RECOMMENDATIONS")));
    if s.recommendations.is_empty() {
        o.push(line(t("Nada pendiente.", "Nothing pending.")));
    }
    for rec in &s.recommendations {
        o.push(line(format!("- {}", rec.replace('`', ""))));
    }

    o.push(line(""));
    o.push(heading(t("PROTECCIÓN", "PROTECTION")));
    for (d, m) in DOMAINS.iter().zip(st.modes.iter()) {
        o.push(line(format!("  {d:<10} {}", m.as_str())));
    }
    o.push(line(format!(
        "  {} {} · {} {} · {} {} · {} {} · {} {}",
        t("aprendizaje", "learning"),
        onoff(st.learning),
        t("anomalías", "anomaly"),
        onoff(r.anomaly_detection),
        t("privilegiadas", "privileged-deny"),
        onoff(r.privileged_deny),
        t("exigir hash", "require-hash"),
        onoff(r.require_hash),
        t("sello", "seal"),
        onoff(st.sealed)
    )));

    if !s.categories.is_empty() {
        o.push(line(""));
        o.push(heading(t(
            "DE DÓNDE VIENEN LAS ALERTAS",
            "WHERE THE ALERTS COME FROM",
        )));
        for (c, n) in &s.categories {
            o.push(line(format!("  {n:>5}  {c}")));
        }
        o.push(line(""));
        for (c, n) in &s.subjects {
            o.push(line(format!("  {n:>5}  {c}")));
        }
    }

    o.push(line(""));
    let alerts: Vec<_> = s
        .events
        .iter()
        .rev()
        .filter(|(_, e)| e.sev >= Severity::Warn)
        .collect();
    o.push(heading(format!(
        "{} ({})",
        t("ALERTAS", "ALERTS"),
        alerts.len()
    )));
    if alerts.is_empty() {
        o.push(line(t(
            "Ninguna alerta en el periodo.",
            "No alerts in the period.",
        )));
    }
    for (d, e) in alerts {
        o.push(line(format!(
            "{:<16} {:<4} {:<9} pid={:<5} {}",
            when(*d, e),
            e.sev.as_str(),
            e.category,
            e.pid,
            e.text
        )));
    }

    o.push(line(""));
    o.push(heading(format!(
        "{} ({})",
        t("CUARENTENA DEL PERIODO", "QUARANTINED IN THE PERIOD"),
        s.quarantined.len()
    )));
    for q in s.quarantined.iter().rev() {
        o.push(line(format!("{}  {}", q.date.replace('T', " "), q.path)));
    }

    o.push(line(""));
    o.push(heading(t("FIRMAS", "SIGNATURES")));
    for f in &inp.sig_files {
        o.push(line(format!(
            "  {:>6} hash {:>4} {}  {}  {}",
            f.hashes,
            f.patterns,
            t("patrones", "patterns"),
            f.mtime.map(civil_time).unwrap_or_default(),
            f.path
        )));
    }

    if !r.learned_programs.is_empty() {
        o.push(line(""));
        o.push(heading(format!(
            "{} ({})",
            t("PROGRAMAS APRENDIDOS", "LEARNED PROGRAMS"),
            r.learned_programs.len()
        )));
        for p in &r.learned_programs {
            o.push(line(format!("  {p}")));
        }
    }
    o
}

pub fn text(inp: &Input, lang: Lang) -> String {
    let mut s = String::new();
    for l in lines(inp, lang) {
        s.push_str(&l.text);
        s.push('\n');
    }
    s
}

/// Courier at this size fits this many characters in a line.
const PDF_COLS: usize = 96;
const PDF_ROWS: usize = 64;
const PDF_SIZE: f32 = 8.5;
const PDF_LEADING: f32 = 11.5;

/// A character in WinAnsiEncoding (cp1252), or `?`. The 0x80-0x9F row is
/// where cp1252 differs from Latin-1; of it, only what a report uses.
fn winansi(c: char) -> u8 {
    match c {
        '€' => 0x80,
        '…' => 0x85,
        '–' => 0x96,
        '—' => 0x97,
        '‘' | '’' => 0x27,
        '“' | '”' => 0x22,
        c if (c as u32) < 0x80 || (0xa0..=0xff).contains(&(c as u32)) => c as u8,
        _ => b'?',
    }
}

/// Cut a line into the columns of a page, at the last space when there is one.
fn wrap(s: &str, cols: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cols {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let mut end = (i + cols).min(chars.len());
        if end < chars.len() {
            if let Some(sp) = chars[i..end].iter().rposition(|&c| c == ' ') {
                if sp > cols / 2 {
                    end = i + sp + 1;
                }
            }
        }
        let piece: String = chars[i..end].iter().collect();
        out.push(if out.is_empty() {
            piece.trim_end().to_string()
        } else {
            format!("    {}", piece.trim())
        });
        i = end;
    }
    out
}

pub fn pdf(inp: &Input, lang: Lang) -> Vec<u8> {
    let mut rows: Vec<Line> = Vec::new();
    for l in lines(inp, lang) {
        for piece in wrap(&l.text, PDF_COLS) {
            rows.push(Line {
                heading: l.heading,
                text: piece,
            });
        }
    }
    let pages: Vec<&[Line]> = rows.chunks(PDF_ROWS).collect();
    let n = pages.len().max(1);

    // Objects: 1 catalog, 2 pages, 3 Courier, 4 Courier-Bold, then a page and
    // its content stream per page.
    let mut objs: Vec<Vec<u8>> = Vec::new();
    objs.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 5 + 2 * i)).collect();
    objs.push(format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")).into_bytes());
    objs.push(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>".to_vec(),
    );
    objs.push(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier-Bold /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    );
    for i in 0..n {
        let page = pages.get(i).copied().unwrap_or(&[]);
        let mut c: Vec<u8> = Vec::new();
        c.extend_from_slice(format!("BT {PDF_LEADING} TL 40 800 Td\n").as_bytes());
        for l in page {
            c.extend_from_slice(
                format!("/{} {PDF_SIZE} Tf (", if l.heading { "F2" } else { "F1" }).as_bytes(),
            );
            for ch in l.text.chars() {
                match winansi(ch) {
                    b @ (b'(' | b')' | b'\\') => c.extend_from_slice(&[b'\\', b]),
                    b => c.push(b),
                }
            }
            c.extend_from_slice(b") Tj T*\n");
        }
        c.extend_from_slice(
            format!("/F1 7 Tf 0 -{PDF_LEADING} Td ({}/{n}) Tj\nET", i + 1).as_bytes(),
        );
        objs.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] \
                 /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {} 0 R >>",
                6 + 2 * i
            )
            .into_bytes(),
        );
        let mut stream = format!("<< /Length {} >>\nstream\n", c.len()).into_bytes();
        stream.extend_from_slice(&c);
        stream.extend_from_slice(b"\nendstream");
        objs.push(stream);
    }

    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(objs.len());
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(o);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        )
        .as_bytes(),
    );
    out
}

pub fn render(inp: &Input, lang: Lang, fmt: Format) -> Vec<u8> {
    match fmt {
        Format::Html => html(inp, lang).into_bytes(),
        Format::Text => text(inp, lang).into_bytes(),
        Format::Pdf => pdf(inp, lang),
    }
}

// ── the disk ─────────────────────────────────────────────────────────────────

fn hostname() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|p| {
            let s = std::fs::read_to_string(p).ok()?;
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        })
        .unwrap_or_else(|| "eclipse".into())
}

fn uptime() -> Option<f64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Everything a report needs, read now. `None` when there is no hunter.
pub fn gather(days: u32) -> Option<Input> {
    use crate::hunter_report as report;
    Some(Input {
        report: report::read()?,
        quarantine: report::read_quarantine(),
        sig_files: report::read_signature_files(),
        feeds: std::fs::read_to_string(report::FEEDS)
            .map(|t| report::parse_feeds(&t))
            .unwrap_or_default(),
        pins: std::fs::read_to_string(report::HASHES)
            .map(|t| report::count_pins(&t))
            .unwrap_or(0),
        host: hostname(),
        now: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        uptime: uptime(),
        days,
        history: read_history(),
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .and_then(|s| s.trim().get(..8).map(str::to_string)),
    })
}

/// `events.log.4` (the oldest) to `events.log`, whatever of it can be read:
/// it is `0640`, so a reader that is not root or in its group gets nothing
/// and the report falls back to the ring.
fn read_history() -> Vec<Logged> {
    let mut paths: Vec<String> = (1..=HISTORY_ROTATIONS)
        .rev()
        .map(|i| format!("{HISTORY}.{i}"))
        .collect();
    paths.push(HISTORY.to_string());
    paths
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|t| parse_history(&t))
        .collect()
}

/// Write `bytes` to `path` through a temporary file and a rename, so a
/// reader never sees half a report.
pub fn write(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    if let Some((dir, _)) = path.rsplit_once('/') {
        if !dir.is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = format!("{path}.new");
    std::fs::write(&tmp, bytes)
        .and_then(|_| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

#[cfg(test)]
#[path = "hunter_export_tests.rs"]
mod tests;
