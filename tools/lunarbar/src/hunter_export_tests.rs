//! The three renderings of the report, read from a sample in the kernel's own
//! format, with no kernel.

use super::*;
use crate::hunter_report::parse_report;

/// 2026-10-29 12:00 UTC.
const NOW: u64 = 1_793_275_200;

const REPORT: &str = "hunter security subsystem v0.1.0\n\
enforcement: syscall=enforce wx=report exec=enforce anomaly=off integrity=report av=enforce\n\
events: total=9 blocked=2 warnings=3 reported=2 critical=2 dropped=4 critical_dropped=0\n\
active syscall policies: 0\n\
exec allowlist: active (configured=38, learned=1); blacklist=0; learning=on\n\
heuristics: anomaly-detection=on privileged-deny=off\n\
integrity: pinned=41 require-hash=off\n\
av: signatures=5002 (hash=5000 bytes=2) detections=1\n\
learned-programs (1):\n  /usr/bin/<firefox>\n\
\n\
recent events (oldest first):\n\
[     1] +    10.000s pid=1     NOTICE CONTROL   CONFIG: mode av enforce\n\
[     2] +   100.000s pid=733   WARN   EXEC      REPORTED: untrusted exec /tmp/a.out\n\
[     3] +   101.000s pid=733   WARN   WX        REPORTED: W+X mapping in /tmp/a.out\n\
[     4] +   200.000s pid=801   CRIT   AV        AV BLOCKED: /root/eicar.com (EICAR-Test-File)\n\
[     5] +   300.000s pid=802   CRIT   AV        AV BLOCKED: /root/eicar.com (EICAR-Test-File)\n";

fn input(uptime: f64) -> Input {
    Input {
        report: parse_report(REPORT).unwrap(),
        quarantine: vec![
            Quarantined {
                date: "2026-10-28T09:00:00".into(),
                sha256: "a".repeat(64),
                mode: "644".into(),
                path: "/root/eicar.com".into(),
            },
            Quarantined {
                date: "2026-09-01T09:00:00".into(),
                sha256: "b".repeat(64),
                mode: "644".into(),
                path: "/root/old".into(),
            },
        ],
        sig_files: vec![SigFile {
            path: "/etc/hunter/signatures".into(),
            hashes: 1,
            patterns: 1,
            mtime: Some(NOW - 20 * 86_400),
        }],
        feeds: Vec::new(),
        pins: 41,
        host: "eclipse".into(),
        now: NOW,
        uptime: Some(uptime),
        days: DEFAULT_DAYS,
    }
}

#[test]
fn civil_time_is_utc() {
    assert_eq!(civil_time(NOW), "2026-10-29 12:00");
    assert_eq!(civil_time(0), "1970-01-01 00:00");
    assert_eq!(file_name(NOW, Format::Pdf), "hunter-informe-2026-10-29.pdf");
}

#[test]
fn the_format_comes_from_the_extension() {
    assert_eq!(Format::from_path("/tmp/x.pdf"), Format::Pdf);
    assert_eq!(Format::from_path("/tmp/x.TXT"), Format::Text);
    assert_eq!(Format::from_path("/tmp/x.html"), Format::Html);
    assert_eq!(Format::from_path("/tmp.d/informe"), Format::Html);
}

#[test]
fn a_boot_inside_the_period_is_said_and_every_event_counts() {
    let s = summarize(&input(3600.0), Lang::Es);
    assert!(s.since_boot);
    assert_eq!(s.from, NOW - 3600);
    assert_eq!(s.events.len(), 5);
    assert_eq!(s.by_sev, [0, 1, 2, 2]);
    // The first event is dated from the boot.
    assert_eq!(s.events[0].0, Some(NOW - 3600 + 10));
    assert_eq!(s.categories[0], ("AV".to_string(), 2));
    // The subject loses the signature's name, so both hits count as one file.
    assert_eq!(s.subjects[0], ("/root/eicar.com".to_string(), 2));
}

#[test]
fn a_long_uptime_cuts_the_events_to_the_period() {
    // Booted 7 days and 150 s ago: events before t=150s are older than the week.
    let s = summarize(&input(7.0 * 86_400.0 + 150.0), Lang::Es);
    assert!(!s.since_boot);
    assert_eq!(s.events.len(), 2);
    assert_eq!(s.by_sev, [0, 0, 0, 2]);
}

#[test]
fn only_the_period_s_quarantine_is_listed() {
    let s = summarize(&input(3600.0), Lang::Es);
    assert_eq!(s.quarantined.len(), 1);
    assert_eq!(s.quarantined[0].path, "/root/eicar.com");
}

#[test]
fn the_recommendations_name_what_to_run() {
    let s = summarize(&input(3600.0), Lang::En);
    let all = s.recommendations.join("\n");
    assert!(all.contains("critical alerts"), "{all}");
    assert!(all.contains("hunterctl mode anomaly report"), "{all}");
    assert!(all.contains("Only reporting: integrity."), "{all}");
    assert!(all.contains("20 days old"), "{all}");
    assert!(all.contains("save-learned"), "{all}");
    assert!(all.contains("lost 4"), "{all}");
    assert!(all.contains("require-hash on"), "{all}");
    assert!(all.contains("lockdown"), "{all}");
}

#[test]
fn html_is_one_escaped_self_contained_page() {
    let h = html(&input(3600.0), Lang::Es);
    assert!(h.starts_with("<!doctype html>"));
    assert!(h.ends_with("</html>\n"));
    assert!(h.contains("Informe de seguridad de hunter"));
    assert!(h.contains("desde el arranque"));
    // Text from the ring is data, never markup.
    assert!(h.contains("/usr/bin/&lt;firefox&gt;"));
    assert!(!h.contains("<firefox>"));
    assert!(h.contains("<code>hunterctl mode anomaly report</code>"));
    // Nothing fetched from anywhere: it has to open on a machine offline.
    assert!(!h.contains("src=\"http") && !h.contains("href=\"http"));
}

#[test]
fn text_has_every_section() {
    let t = text(&input(3600.0), Lang::En);
    for s in [
        "HUNTER SECURITY REPORT",
        "RECOMMENDATIONS",
        "PROTECTION",
        "ALERTS (4)",
        "SIGNATURES",
        "LEARNED PROGRAMS (1)",
    ] {
        assert!(t.contains(s), "{s} missing:\n{t}");
    }
    assert!(!t.contains('`'));
}

#[test]
fn pdf_is_well_formed() {
    let p = pdf(&input(3600.0), Lang::Es);
    assert!(p.starts_with(b"%PDF-1.4\n"));
    assert!(p.ends_with(b"%%EOF\n"));
    let s = String::from_utf8_lossy(&p);
    // The xref table points at each object exactly.
    let xref: usize = s
        .rsplit_once("startxref\n")
        .unwrap()
        .1
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(p[xref..].starts_with(b"xref\n"));
    let table = &s[xref..];
    for (i, l) in table
        .lines()
        .skip(3)
        .take_while(|l| l.ends_with(" n "))
        .enumerate()
    {
        let off: usize = l[..10].parse().unwrap();
        assert!(
            p[off..].starts_with(format!("{} 0 obj", i + 1).as_bytes()),
            "object {}",
            i + 1
        );
    }
    // Spanish reaches the page as WinAnsi, not as UTF-8.
    assert!(p.windows(10).any(|w| w == b"PROTECCI\xd3N"));
}

#[test]
fn a_long_report_turns_the_page() {
    let mut inp = input(3600.0);
    let line = "[     9] +   300.000s pid=802   CRIT   AV        AV BLOCKED: /root/x";
    let ev = crate::hunter_report::parse_ring_line(line).unwrap();
    inp.report.events = vec![ev; 200];
    let p = pdf(&inp, Lang::En);
    let s = String::from_utf8_lossy(&p);
    assert!(s.contains("/Count 4 "), "200 alerts take four pages");
}

#[test]
fn wrapping_breaks_at_a_space_and_indents_the_rest() {
    let w = wrap(&format!("{} {}", "a".repeat(60), "b".repeat(60)), 96);
    assert_eq!(w.len(), 2);
    assert_eq!(w[0], "a".repeat(60));
    assert_eq!(w[1], format!("    {}", "b".repeat(60)));
    assert_eq!(wrap("corto", 96), vec!["corto".to_string()]);
}

#[test]
fn pdf_strings_are_escaped() {
    let mut inp = input(3600.0);
    inp.host = "a(b)c\\d".into();
    let p = pdf(&inp, Lang::En);
    assert!(p.windows(10).any(|w| w == b"a\\(b\\)c\\\\d"));
}
