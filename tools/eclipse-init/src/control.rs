//! What init says about its services, and what it can be asked to do with
//! them.
//!
//! **State.** `/run/eclipse-init/services` is rewritten (through a temporary
//! beside it, so a reader never sees half) every time the supervision loop
//! wakes: one line per service, tab-separated,
//!
//! ```text
//! name  kind  state  pid  starts  started  log  exec
//! ntpd  respawn  running  455  1  1793865120  /tmp/ntpd.log  /usr/local/bin/eclipse-ntpd
//! ```
//!
//! `state` is `running`, `waiting` (a restart is due after its backoff),
//! `stopped` (stopped on request), `given-up` (written off for this boot) or
//! `idle` (a oneshot, or a respawn not started yet). `pid` and `started` (the
//! wall-clock second the current run began) are `-` when there is none;
//! `starts` counts the starts of this boot.
//!
//! **Requests.** A file `/run/eclipse-init/ctl/<verb>.<name>`, then SIGHUP to
//! PID 1. `verb` is `start`, `stop` or `restart`; only `respawn` services take
//! them. The directory is root's alone (0700), which is the whole access
//! check: whoever can write there could as well signal the service itself.
//! What is asked lasts this boot: a stopped service starts again on the next.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::{log, Kind, Service, MIN_BACKOFF};

pub(crate) const STATE_DIR: &str = "/run/eclipse-init";
pub(crate) const STATE_FILE: &str = "/run/eclipse-init/services";
pub(crate) const CTL_DIR: &str = "/run/eclipse-init/ctl";

/// Set by SIGHUP: there are requests to read.
pub(crate) static WANT_CONTROL: AtomicBool = AtomicBool::new(false);

pub(crate) extern "C" fn on_sighup(_sig: libc::c_int) {
    WANT_CONTROL.store(true, Ordering::SeqCst);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Start,
    Stop,
    Restart,
}

/// `start.ntpd` → `(Start, "ntpd")`. Anything else is not a request.
pub(crate) fn parse_request(file_name: &str) -> Option<(Verb, &str)> {
    let (verb, name) = file_name.split_once('.')?;
    let verb = match verb {
        "start" => Verb::Start,
        "stop" => Verb::Stop,
        "restart" => Verb::Restart,
        _ => return None,
    };
    let ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b));
    ok.then_some((verb, name))
}

/// The state word for `svc` at `now`.
pub(crate) fn state_of(svc: &Service, now: Instant) -> &'static str {
    if svc.pid.is_some() {
        "running"
    } else if svc.held {
        "stopped"
    } else if svc.given_up {
        "given-up"
    } else if svc.kind == Kind::Respawn && svc.restart_at.is_some_and(|t| t > now) {
        "waiting"
    } else {
        "idle"
    }
}

fn field(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if t.trim().is_empty() {
        "-".into()
    } else {
        t.trim().to_string()
    }
}

/// The state file's text.
pub(crate) fn render_state(
    services: &BTreeMap<String, Service>,
    now: Instant,
    wall: u64,
) -> String {
    let mut s = String::from("# name\tkind\tstate\tpid\tstarts\tstarted\tlog\texec\n");
    for svc in services.values() {
        let kind = match svc.kind {
            Kind::Oneshot => "oneshot",
            Kind::Respawn => "respawn",
        };
        let pid = svc.pid.map_or("-".to_string(), |p| p.to_string());
        let started = match (svc.pid, svc.started_at) {
            (Some(_), Some(t)) => wall
                .saturating_sub(now.saturating_duration_since(t).as_secs())
                .to_string(),
            _ => "-".into(),
        };
        s += &format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            field(&svc.name),
            kind,
            state_of(svc, now),
            pid,
            svc.total_starts,
            started,
            field(svc.log.as_deref().unwrap_or("")),
            field(&svc.exec.join(" ")),
        );
    }
    s
}

/// Rewrite the state file. Best effort: a read-only `/run` costs the monitor
/// its list, never a service.
pub(crate) fn write_state(services: &BTreeMap<String, Service>) {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let text = render_state(services, Instant::now(), wall);
    if fs::read_to_string(STATE_FILE).ok().as_deref() == Some(text.as_str()) {
        return;
    }
    let _ = fs::create_dir_all(STATE_DIR);
    let tmp = format!("{STATE_FILE}.tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, STATE_FILE);
    }
}

/// Make the request directory, root's alone.
pub(crate) fn prepare() {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::create_dir_all(CTL_DIR);
    let _ = fs::set_permissions(STATE_DIR, fs::Permissions::from_mode(0o755));
    let _ = fs::set_permissions(CTL_DIR, fs::Permissions::from_mode(0o700));
}

/// Do one request to `svc`. Returns what to log, or why it was refused.
/// `kill` is the signal sender, passed in so the tests send nothing.
pub(crate) fn apply(svc: &mut Service, verb: Verb, kill: &mut dyn FnMut(i32)) -> String {
    if svc.kind != Kind::Respawn {
        return format!("control: {}: a oneshot cannot be {verb:?}ed", svc.name);
    }
    match verb {
        Verb::Stop => {
            svc.held = true;
            svc.restart_now = false;
            if let Some(pid) = svc.pid {
                kill(pid);
            }
            format!("control: stop {}", svc.name)
        }
        Verb::Start => {
            forgive(svc);
            format!("control: start {}", svc.name)
        }
        Verb::Restart => {
            forgive(svc);
            if let Some(pid) = svc.pid {
                svc.restart_now = true;
                kill(pid);
            }
            format!("control: restart {}", svc.name)
        }
    }
}

/// Undo what this boot held against a service: asked for by name, it gets a
/// clean start whatever its record says.
fn forgive(svc: &mut Service) {
    svc.held = false;
    svc.given_up = false;
    svc.crash_starts = 0;
    svc.missing_starts = 0;
    svc.starts.clear();
    svc.restart_at = None;
    svc.backoff = MIN_BACKOFF;
}

/// After the exits of this wake are noted: a service killed by a restart
/// request starts again now, without the crash accounting its quick death
/// would otherwise earn.
pub(crate) fn settle_restarts(services: &mut BTreeMap<String, Service>) {
    for svc in services.values_mut() {
        if svc.restart_now && svc.pid.is_none() {
            svc.restart_now = false;
            forgive(svc);
        }
    }
}

/// Read and do every pending request. Each file is removed whether or not it
/// named something real, so a bad one is not read again.
pub(crate) fn take_requests(services: &mut BTreeMap<String, Service>) {
    let Ok(rd) = fs::read_dir(CTL_DIR) else {
        return;
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for f in names {
        let _ = fs::remove_file(Path::new(CTL_DIR).join(&f));
        let Some((verb, name)) = parse_request(&f) else {
            log(&format!("control: ignored request {f:?}"));
            continue;
        };
        let Some(svc) = services.get_mut(name) else {
            log(&format!("control: no service called {name}"));
            continue;
        };
        let mut kill = |pid: i32| {
            // SAFETY: a signal to one child of ours.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        };
        log(&apply(svc, verb, &mut kill));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_service;

    fn respawn(name: &str) -> Service {
        parse_service(name, "exec = /bin/foo\ntype = respawn\nlog = /tmp/x.log\n").unwrap()
    }

    #[test]
    fn a_request_is_a_verb_and_a_service_name() {
        assert_eq!(parse_request("stop.ntpd"), Some((Verb::Stop, "ntpd")));
        assert_eq!(
            parse_request("restart.dbus-system"),
            Some((Verb::Restart, "dbus-system"))
        );
        assert_eq!(parse_request("start.a.b"), Some((Verb::Start, "a.b")));
        assert_eq!(parse_request("kill.ntpd"), None);
        assert_eq!(parse_request("stop."), None);
        assert_eq!(parse_request("stop.x y"), None);
        // A name is looked up in the service map and nowhere else, so dots are
        // harmless; a slash cannot be in a file name at all.
    }

    #[test]
    fn stop_holds_the_service_and_signals_it_and_start_lets_it_go() {
        let mut s = respawn("ntpd");
        s.pid = Some(455);
        let mut sent = Vec::new();
        apply(&mut s, Verb::Stop, &mut |p| sent.push(p));
        assert_eq!(sent, vec![455]);
        assert!(s.held);
        s.pid = None;
        assert_eq!(state_of(&s, Instant::now()), "stopped");
        let mut map = BTreeMap::new();
        map.insert("ntpd".to_string(), s);
        assert!(
            crate::due_names(&map, Instant::now()).is_empty(),
            "a held service is not restarted"
        );
        let s = map.get_mut("ntpd").unwrap();
        s.given_up = true;
        s.crash_starts = 7;
        apply(s, Verb::Start, &mut |_| panic!("nothing to signal"));
        assert!(!s.held && !s.given_up && s.crash_starts == 0);
        assert_eq!(
            crate::due_names(&map, Instant::now()),
            vec!["ntpd".to_string()]
        );
    }

    #[test]
    fn a_restart_comes_back_at_once_and_not_as_a_crash() {
        let mut map = BTreeMap::new();
        let mut s = respawn("ntpd");
        s.pid = Some(9);
        apply(&mut s, Verb::Restart, &mut |_| {});
        assert!(s.restart_now);
        // What note_exit leaves after a quick death: a backoff deadline.
        s.pid = None;
        s.crash_starts = 1;
        s.restart_at = Some(Instant::now() + Duration::from_secs(8));
        map.insert("ntpd".to_string(), s);
        settle_restarts(&mut map);
        let s = &map["ntpd"];
        assert!(!s.restart_now && s.crash_starts == 0 && s.restart_at.is_none());
        assert_eq!(
            crate::due_names(&map, Instant::now()),
            vec!["ntpd".to_string()]
        );
    }

    #[test]
    fn a_oneshot_is_not_controlled() {
        let mut s = parse_service("mounts", "exec = /bin/mount -a\ntype = oneshot\n").unwrap();
        let msg = apply(&mut s, Verb::Stop, &mut |_| panic!("no signal"));
        assert!(msg.contains("oneshot") && !s.held);
    }

    #[test]
    fn the_state_file_is_one_tab_separated_line_per_service() {
        let now = Instant::now();
        let mut map = BTreeMap::new();
        let mut a = respawn("ntpd");
        a.pid = Some(455);
        a.started_at = Some(now - Duration::from_secs(30));
        a.total_starts = 2;
        map.insert("ntpd".to_string(), a);
        let mut b = respawn("portal");
        b.restart_at = Some(now + Duration::from_secs(4));
        map.insert("portal".to_string(), b);
        let text = render_state(&map, now, 1_000_000);
        let lines: Vec<Vec<&str>> = text
            .lines()
            .skip(1)
            .map(|l| l.split('\t').collect())
            .collect();
        assert_eq!(
            lines[0],
            vec![
                "ntpd",
                "respawn",
                "running",
                "455",
                "2",
                "999970",
                "/tmp/x.log",
                "/bin/foo"
            ]
        );
        assert_eq!(&lines[1][..4], &["portal", "respawn", "waiting", "-"]);
        assert!(text.starts_with('#'));
    }
}
