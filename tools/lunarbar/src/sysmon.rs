//! What the system monitor (`lunarmon`) reads, as pure parsers over the text
//! of `/proc` and of eclipse-init's state file, plus the two things it does:
//! signal a process, and ask init to start, stop or restart a service.
//!
//! Every reader takes the text and not the path, so the arithmetic (a CPU
//! share from two samples of tick counters, a rate from two sector counts)
//! is tested without a `/proc`.

use crate::i18n::Lang;
use std::path::Path;

/// `USER_HZ`: the unit of the tick counters in `/proc/stat` and
/// `/proc/<pid>/stat`.
pub const TICKS: u64 = 100;
/// Pages to KiB.
pub const PAGE_KIB: u64 = 4;

// ── processes ────────────────────────────────────────────────────────────────

/// One `/proc/<pid>/stat`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: i32,
    pub comm: String,
    /// `R`, `S`, `D`, `Z`, `T`…
    pub state: char,
    pub ppid: i32,
    /// utime + stime, in [`TICKS`].
    pub ticks: u64,
    pub threads: u32,
    /// Resident set, in pages.
    pub rss: u64,
    /// When it started, in ticks after boot.
    pub start: u64,
}

/// `/proc/<pid>/stat`. The comm is in parentheses and may hold spaces and
/// parentheses of its own, so the fields are counted from the LAST `)`.
pub fn parse_pid_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text[..open].trim().parse().ok()?;
    let comm = text.get(open + 1..close)?.to_string();
    let f: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // f[0] is field 3 (state); field n is f[n - 3].
    let n = |k: usize| -> Option<u64> { f.get(k - 3)?.parse().ok() };
    Some(ProcStat {
        pid,
        comm,
        state: f.first()?.chars().next()?,
        ppid: f.get(1)?.parse().ok()?,
        ticks: n(14)? + n(15)?,
        threads: n(20)? as u32,
        start: n(22)?,
        rss: n(24)?,
    })
}

/// `/proc/<pid>/cmdline`: the arguments, NUL-separated, as one line.
pub fn cmdline_text(raw: &[u8]) -> String {
    raw.split(|b| *b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every process now: its stat and its command line.
pub fn processes(proc_dir: &Path) -> Vec<(ProcStat, String)> {
    let Ok(rd) = std::fs::read_dir(proc_dir) else { return Vec::new() };
    let mut out: Vec<(ProcStat, String)> = rd
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit())))
        .filter_map(|e| {
            let p = e.path();
            let st = parse_pid_stat(&std::fs::read_to_string(p.join("stat")).ok()?)?;
            let cmd = std::fs::read(p.join("cmdline")).map(|r| cmdline_text(&r)).unwrap_or_default();
            Some((st, cmd))
        })
        .collect();
    out.sort_by_key(|(s, _)| s.pid);
    out
}

/// A process's share of the whole machine between two samples, in percent:
/// `dticks` of its CPU time over `dt` seconds on `cpus` CPUs.
pub fn cpu_share(dticks: u64, dt: f64, cpus: usize) -> f32 {
    if dt <= 0.0 || cpus == 0 {
        return 0.0;
    }
    let pct = dticks as f64 / (dt * TICKS as f64 * cpus as f64) * 100.0;
    pct.clamp(0.0, 100.0) as f32
}

/// Whether a window's `app_id` is this process: `firefox` is `firefox`,
/// `org.gnome.Nautilus` is `nautilus`, `foot` is `foot`. Case aside, and the
/// kernel's 15-character comm compared with as much of the name as it holds.
pub fn app_matches(app_id: &str, comm: &str) -> bool {
    let a = app_id.rsplit('.').next().unwrap_or(app_id).to_ascii_lowercase();
    let c = comm.to_ascii_lowercase();
    !a.is_empty() && (a == c || (c.len() >= 15 && a.starts_with(&c)))
}

/// The windows the bar knows of (`$XDG_RUNTIME_DIR/lunar/desktops`):
/// `(app_id, title)` per line.
pub fn parse_windows(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let _desk = f.next()?;
            let app = f.next()?.to_string();
            let title = f.next().unwrap_or("").to_string();
            (!app.is_empty()).then_some((app, title))
        })
        .collect()
}

// ── the machine ──────────────────────────────────────────────────────────────

/// `(busy, total)` ticks of each `cpuN` line of `/proc/stat`, in order. The
/// summary `cpu` line is left out: the cores are summed by the caller.
pub fn parse_cpus(stat: &str) -> Vec<(u64, u64)> {
    stat.lines()
        .filter(|l| l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit))
        .map(|l| {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().sum();
            // idle + iowait are not busy.
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            (total.saturating_sub(idle), total)
        })
        .collect()
}

/// How busy each core was between two samples, in percent.
pub fn core_load(prev: &[(u64, u64)], now: &[(u64, u64)]) -> Vec<f32> {
    now.iter()
        .zip(prev)
        .map(|((b1, t1), (b0, t0))| {
            let dt = t1.saturating_sub(*t0);
            if dt == 0 {
                0.0
            } else {
                (b1.saturating_sub(*b0) as f32 / dt as f32 * 100.0).clamp(0.0, 100.0)
            }
        })
        .collect()
}

/// `/proc/meminfo`, in KiB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mem {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Mem {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

pub fn parse_meminfo(text: &str) -> Mem {
    let mut m = Mem::default();
    for l in text.lines() {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v: u64 = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
        match k {
            "MemTotal" => m.total = v,
            "MemAvailable" => m.available = v,
            "MemFree" => m.free = v,
            "Cached" => m.cached = v,
            "SwapTotal" => m.swap_total = v,
            "SwapFree" => m.swap_free = v,
            _ => {}
        }
    }
    if m.available == 0 {
        m.available = m.free + m.cached;
    }
    m
}

/// `(name, sectors read, sectors written)` of each disk in `/proc/diskstats`.
/// Partitions (a name that is a disk's name plus a number) are left out when
/// their disk is listed, so nothing is counted twice.
pub fn parse_diskstats(text: &str) -> Vec<(String, u64, u64)> {
    let all: Vec<(String, u64, u64)> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some((f.get(2)?.to_string(), f.get(5)?.parse().ok()?, f.get(9)?.parse().ok()?))
        })
        .collect();
    all.iter()
        .filter(|(n, _, _)| {
            !all.iter().any(|(d, _, _)| {
                d != n
                    && n.starts_with(d.as_str())
                    && n[d.len()..].trim_start_matches('p').bytes().all(|b| b.is_ascii_digit())
            })
        })
        .cloned()
        .collect()
}

// ── services ─────────────────────────────────────────────────────────────────

pub const SERVICES_FILE: &str = "/run/eclipse-init/services";
pub const CTL_DIR: &str = "/run/eclipse-init/ctl";

/// One line of eclipse-init's state file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Svc {
    pub name: String,
    /// `respawn` or `oneshot`.
    pub kind: String,
    /// `running`, `waiting`, `stopped`, `given-up` or `idle`.
    pub state: String,
    pub pid: Option<i32>,
    pub starts: u32,
    /// Wall-clock second the current run began.
    pub started: Option<u64>,
    pub log: Option<String>,
    pub exec: String,
}

pub fn parse_services(text: &str) -> Vec<Svc> {
    let opt = |s: &str| (s != "-").then(|| s.to_string());
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 8 {
                return None;
            }
            Some(Svc {
                name: f[0].to_string(),
                kind: f[1].to_string(),
                state: f[2].to_string(),
                pid: f[3].parse().ok(),
                starts: f[4].parse().unwrap_or(0),
                started: f[5].parse().ok(),
                log: opt(f[6]),
                exec: f[7].to_string(),
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Start,
    Stop,
    Restart,
}

impl Verb {
    pub fn as_str(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
        }
    }
}

/// The request file for `verb` on `name`, or None for a name that could not be
/// one of init's (it would not be a file name either).
pub fn request_path(ctl: &Path, verb: Verb, name: &str) -> Option<std::path::PathBuf> {
    let ok = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b));
    ok.then(|| ctl.join(format!("{}.{name}", verb.as_str())))
}

/// Ask init: the request file, then SIGHUP to PID 1.
pub fn request(verb: Verb, name: &str) -> std::io::Result<()> {
    let path = request_path(Path::new(CTL_DIR), verb, name)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    std::fs::write(path, b"")?;
    // SAFETY: a signal to init, which only sets a flag on SIGHUP.
    if unsafe { libc::kill(1, libc::SIGHUP) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Signal a process: SIGTERM to ask it to end, SIGKILL to end it.
pub fn signal(pid: i32, kill: bool) -> std::io::Result<()> {
    if pid <= 1 {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    let sig = if kill { libc::SIGKILL } else { libc::SIGTERM };
    // SAFETY: kill(2) on a pid from /proc.
    if unsafe { libc::kill(pid, sig) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The last `n` lines of a log file, read from its end so a large log costs
/// nothing.
pub fn log_tail(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(16 * 1024);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.take(16 * 1024).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<String> = text
        .lines()
        .skip(usize::from(from > 0))
        .map(|l| l.chars().map(|c| if c.is_control() { ' ' } else { c }).collect())
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

// ── words ────────────────────────────────────────────────────────────────────

/// `612 MiB`, `2,1 GiB`, from KiB.
pub fn fmt_kib(kib: u64, lang: Lang) -> String {
    if kib >= 1024 * 1024 {
        lang.decimals(format!("{:.1} GiB", kib as f64 / (1024.0 * 1024.0)))
    } else if kib >= 1024 {
        format!("{} MiB", kib / 1024)
    } else {
        format!("{kib} KiB")
    }
}

/// `3 h 12 min`, `4 min`, `12 s`.
pub fn fmt_secs(s: u64) -> String {
    if s >= 86_400 {
        format!("{} d {} h", s / 86_400, s % 86_400 / 3600)
    } else if s >= 3600 {
        format!("{} h {} min", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

/// CPU time as `h:mm:ss`, from ticks.
pub fn fmt_ticks(t: u64) -> String {
    let s = t / TICKS;
    format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// The state word a person reads, and whether it is a problem.
pub fn state_word(state: &str, lang: crate::i18n::Lang) -> (&'static str, bool) {
    let (es, en, bad) = match state {
        "running" => ("en marcha", "running", false),
        "waiting" => ("reintentando", "retrying", true),
        "stopped" => ("parado", "stopped", false),
        "given-up" => ("abandonado", "given up", true),
        "idle" => ("sin proceso", "no process", false),
        _ => ("desconocido", "unknown", true),
    };
    (if lang == crate::i18n::Lang::En { en } else { es }, bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stat_line_is_read_past_a_comm_with_spaces_and_parens() {
        let line = "1412 (Web Content (x)) S 1 1412 1412 0 -1 4194560 100 0 0 0 2500 300 0 0 20 0 58 0 9000 640000000 156672 18446744073709551615";
        let s = parse_pid_stat(line).unwrap();
        assert_eq!((s.pid, s.comm.as_str(), s.state, s.ppid), (1412, "Web Content (x)", 'S', 1));
        assert_eq!((s.ticks, s.threads, s.start, s.rss), (2800, 58, 9000, 156672));
        assert!(parse_pid_stat("garbage").is_none());
    }

    #[test]
    fn shares_and_loads_are_percent_and_never_more_than_all_of_it() {
        // 100 ticks in one second on 4 CPUs is one CPU of four: 25 %.
        assert_eq!(cpu_share(100, 1.0, 4), 25.0);
        assert_eq!(cpu_share(9999, 1.0, 1), 100.0);
        assert_eq!(cpu_share(5, 0.0, 4), 0.0);
        let stat0 = "cpu  10 0 10 80 0 0 0 0 0 0\ncpu0 5 0 5 40 0\ncpu1 5 0 5 40 0\nintr 1\n";
        let stat1 = "cpu  40 0 10 100 0 0 0 0 0 0\ncpu0 35 0 5 40 0\ncpu1 5 0 5 60 0\n";
        let (p, n) = (parse_cpus(stat0), parse_cpus(stat1));
        assert_eq!(p, vec![(10, 50), (10, 50)]);
        assert_eq!(core_load(&p, &n), vec![100.0, 0.0]);
    }

    #[test]
    fn memory_and_disks_are_read_as_linux_writes_them() {
        let m = parse_meminfo("MemTotal:  8000 kB\nMemFree: 3000 kB\nMemAvailable: 5000 kB\nCached: 1500 kB\nSwapTotal: 0 kB\n");
        assert_eq!((m.total, m.used(), m.cached), (8000, 3000, 1500));
        let old = parse_meminfo("MemTotal: 100 kB\nMemFree: 20 kB\nCached: 30 kB\n");
        assert_eq!(old.available, 50, "no MemAvailable: free plus cache");
        let d = parse_diskstats(
            "   3       0 sda 10 0 800 0 2 0 16 0 0 0 0\n   3       1 sda1 9 0 700 0 1 0 8 0 0 0 0\n   3      16 nvme0n1 1 0 8 0 0 0 0 0 0 0 0\n   3      17 nvme0n1p1 1 0 8 0 0 0 0 0 0 0 0\n",
        );
        assert_eq!(d, vec![("sda".into(), 800, 16), ("nvme0n1".into(), 8, 0)]);
    }

    #[test]
    fn the_init_state_file_round_trips_its_fields() {
        let t = "# name\tkind\tstate\tpid\tstarts\tstarted\tlog\texec\nntpd\trespawn\trunning\t455\t1\t1793865120\t/tmp/ntpd.log\t/usr/local/bin/eclipse-ntpd\nmounts\toneshot\tidle\t-\t1\t-\t-\t/bin/mount -a\nbroken\n";
        let s = parse_services(t);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].pid, s[0].started, s[0].log.as_deref()), (Some(455), Some(1793865120), Some("/tmp/ntpd.log")));
        assert_eq!((s[1].pid, s[1].log.clone(), s[1].exec.as_str()), (None, None, "/bin/mount -a"));
        assert_eq!(state_word("given-up", crate::i18n::Lang::Es), ("abandonado", true));
        assert_eq!(state_word("given-up", crate::i18n::Lang::En), ("given up", true));
    }

    #[test]
    fn a_request_names_a_file_init_reads_and_nothing_else() {
        let ctl = Path::new("/run/eclipse-init/ctl");
        assert_eq!(request_path(ctl, Verb::Stop, "ntpd").unwrap(), Path::new("/run/eclipse-init/ctl/stop.ntpd"));
        assert!(request_path(ctl, Verb::Start, "../x").is_none());
        assert!(request_path(ctl, Verb::Start, "").is_none());
        assert!(signal(1, false).is_err(), "init is not signalled from here");
    }

    #[test]
    fn windows_are_matched_to_their_processes() {
        assert!(app_matches("firefox", "firefox"));
        assert!(app_matches("org.gnome.Nautilus", "nautilus"));
        assert!(app_matches("org.libreoffice.LibreOffice", "libreoffice"));
        assert!(app_matches("thunderbird-esr", "thunderbird-esr"));
        assert!(app_matches("averyveryverylongname", "averyveryverylo"), "the kernel's comm is cut at 15");
        assert!(!app_matches("foot", "footclient"));
        assert!(!app_matches("", "x"));
        assert_eq!(parse_windows("0\tfirefox\tInicio — Mozilla\n1\t\t?\n"), vec![("firefox".into(), "Inicio — Mozilla".into())]);
    }

    #[test]
    fn the_numbers_read_like_a_person_says_them() {
        assert_eq!(fmt_kib(612 * 1024, Lang::Es), "612 MiB");
        assert_eq!(fmt_kib(2_202_009, Lang::Es), "2,1 GiB");
        assert_eq!(fmt_kib(900, Lang::Es), "900 KiB");
        // La fracción se escribe como la escribe el idioma: `2,1` y `2.1`.
        assert_eq!(fmt_kib(2_202_009, Lang::En), "2.1 GiB");
        assert_eq!(fmt_kib(612 * 1024, Lang::En), "612 MiB");
        assert_eq!(fmt_secs(3 * 3600 + 12 * 60), "3 h 12 min");
        assert_eq!(fmt_ticks(25_100), "0:04:11");
        assert_eq!(cmdline_text(b"/usr/bin/foot\0-e\0sh\0"), "/usr/bin/foot -e sh");
    }

    #[test]
    fn a_log_tail_is_the_last_lines() {
        let dir = std::env::temp_dir().join(format!("sysmon-tail-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("x.log");
        std::fs::write(&p, (0..50).map(|i| format!("línea {i}\n")).collect::<String>()).unwrap();
        assert_eq!(log_tail(&p, 2), vec!["línea 48".to_string(), "línea 49".to_string()]);
        assert!(log_tail(&dir.join("no"), 3).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
