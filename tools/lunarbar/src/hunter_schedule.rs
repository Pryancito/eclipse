//! hunter's scheduled scans, as `lunarhunter` reads them: the jobs in
//! `/etc/hunter/schedule`, when each last started, and the history of runs.
//!
//! The scheduler is `hunterctl scheduler` (the `hunter-scheduler` service),
//! and the formats are its own: a job line is
//! `<job> <when> <HH:MM> <report|quarantine> <path>...`, `<job>.last` holds
//! the second its last run started, and `history` has one tab-separated line
//! per run. This module only reads them, pure where it can be, so the tests
//! need neither a kernel nor a scheduler. Changing a job goes through
//! `hunterctl schedule`, which keeps the operator's comments where they are.
//!
//! [`next_slot`] is the same arithmetic as `hunterctl`'s `next_slot`: the
//! window says when a scan will run, and it has to say what the scheduler
//! will do, not something close to it.

/// The jobs.
pub const SCHEDULE: &str = "/etc/hunter/schedule";
/// The scheduler's state: `<job>.last`, `history`, the reports and `lock`.
pub const SCANS: &str = "/var/lib/hunter/scans";

/// Monday first, as ISO counts them (and as `hunterctl` spells them).
pub const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Off,
    Daily,
    /// Once a week: 0 is Monday, 6 Sunday.
    Weekly(u8),
}

impl When {
    pub fn parse(s: &str) -> Option<When> {
        match s {
            "off" => Some(When::Off),
            "daily" => Some(When::Daily),
            _ => DAYS
                .iter()
                .position(|d| *d == s)
                .map(|i| When::Weekly(i as u8)),
        }
    }

    /// The word `hunterctl schedule` takes.
    pub fn word(self) -> &'static str {
        match self {
            When::Off => "off",
            When::Daily => "daily",
            When::Weekly(d) => DAYS[d as usize % 7],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub when: When,
    /// Minutes after local midnight.
    pub at: u32,
    /// `quarantine`: what a run finds is put away; `report`: only recorded.
    pub quarantine: bool,
    pub paths: Vec<String>,
    /// When its last run started (seconds since the epoch), if it has run.
    pub last: Option<u64>,
}

impl Job {
    /// `HH:MM`.
    pub fn at_str(&self) -> String {
        hhmm(self.at)
    }
}

pub fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60 % 24, minutes % 60)
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// The jobs in a schedule file. A line `hunterctl` would not run is left out
/// rather than shown as something it is not.
pub fn parse_schedule(text: &str) -> Vec<Job> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 5 {
                return None;
            }
            Some(Job {
                name: f[0].to_string(),
                when: When::parse(f[1])?,
                at: parse_hhmm(f[2])?,
                quarantine: match f[3] {
                    "quarantine" => true,
                    "report" => false,
                    _ => return None,
                },
                paths: f[4..].iter().map(|p| p.to_string()).collect(),
                last: None,
            })
        })
        .collect()
}

/// The first slot strictly after `last`, in seconds since the epoch; `off` is
/// the local timezone's offset from UTC in seconds. None for a job that is
/// off. Days are counted from 1970-01-01, a Thursday.
pub fn next_slot(last: u64, when: When, at: u32, off: i64) -> Option<u64> {
    let tod = at as i64 * 60;
    let loc = last as i64 + off;
    let day = loc.div_euclid(86_400);
    let cand = match when {
        When::Off => return None,
        When::Daily => {
            let c = day * 86_400 + tod;
            if c > loc {
                c
            } else {
                c + 86_400
            }
        }
        When::Weekly(want) => {
            let dow = (day + 3).rem_euclid(7);
            let c = (day + (want as i64 - dow + 7) % 7) * 86_400 + tod;
            if c > loc {
                c
            } else {
                c + 7 * 86_400
            }
        }
    };
    u64::try_from(cand - off).ok()
}

/// One run, as `hunterctl` appends it to `history`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// `YYYY-MM-DDTHH:MM:SS`, local time.
    pub date: String,
    pub job: String,
    /// `scheduled` or `manual`.
    pub trigger: String,
    pub secs: u64,
    pub files: u64,
    pub infected: u64,
    pub quarantined: u64,
    /// `clean`, `infected` or `failed`.
    pub status: String,
    /// The report's file name in [`SCANS`].
    pub report: String,
}

/// The history, oldest first. A line in any other shape is skipped.
pub fn parse_history(text: &str) -> Vec<Run> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() != 9 {
                return None;
            }
            let n = |s: &str| s.parse::<u64>().ok();
            Some(Run {
                date: f[0].to_string(),
                job: f[1].to_string(),
                trigger: f[2].to_string(),
                secs: n(f[3])?,
                files: n(f[4])?,
                infected: n(f[5])?,
                quarantined: n(f[6])?,
                status: f[7].to_string(),
                report: f[8].to_string(),
            })
        })
        .collect()
}

/// Everything the window shows about the schedule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schedule {
    pub jobs: Vec<Job>,
    pub history: Vec<Run>,
    /// The pid of the scheduled scan running now, if one is.
    pub running: Option<u32>,
    /// The schedule file exists (an image older than the scheduler has none).
    pub present: bool,
}

/// Read the jobs, their last runs, the history and the lock.
pub fn read() -> Schedule {
    let text = std::fs::read_to_string(SCHEDULE);
    let present = text.is_ok();
    let mut jobs = parse_schedule(&text.unwrap_or_default());
    for j in &mut jobs {
        j.last = std::fs::read_to_string(format!("{SCANS}/{}.last", j.name))
            .ok()
            .and_then(|t| t.trim().parse().ok());
    }
    let history = std::fs::read_to_string(format!("{SCANS}/history"))
        .map(|t| parse_history(&t))
        .unwrap_or_default();
    // `hunterctl` leaves its pid there while it scans; a stale one (a crash,
    // a power cut) names a process that is gone.
    let running = std::fs::read_to_string(format!("{SCANS}/lock"))
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok())
        .filter(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists());
    Schedule {
        jobs,
        history,
        running,
        present,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../../hunterctl/schedule");
    /// 2026-10-29T00:00Z, a Thursday.
    const T0: u64 = 1_793_232_000;
    const DAY: u64 = 86_400;

    #[test]
    fn the_shipped_schedule_parses_to_a_quick_daily_and_a_full_weekly() {
        let jobs = parse_schedule(SHIPPED);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].name, "quick");
        assert_eq!(jobs[0].when, When::Daily);
        assert!(jobs[0].paths.contains(&"/tmp".to_string()));
        assert_eq!(jobs[1].name, "full");
        assert_eq!(jobs[1].when, When::Weekly(6));
        assert_eq!(jobs[1].paths, ["/"]);
        assert!(!jobs[1].quarantine);
    }

    #[test]
    fn lines_hunterctl_would_not_run_are_left_out() {
        let jobs = parse_schedule(
            "a daily 12:00 report\n\
             b weekly 12:00 report /\n\
             c daily 24:00 report /\n\
             d daily 12:00 delete /\n\
             e mon 07:05 quarantine /x /y\n",
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].when, When::Weekly(0));
        assert_eq!(jobs[0].at_str(), "07:05");
        assert!(jobs[0].quarantine);
    }

    /// The same cases as xtask's test of `hunterctl`'s `next_slot`.
    #[test]
    fn next_slot_agrees_with_hunterctl() {
        let w = |s| When::parse(s).unwrap();
        assert_eq!(next_slot(T0, w("daily"), 720, 0), Some(T0 + 12 * 3600));
        assert_eq!(
            next_slot(T0 + 12 * 3600, w("daily"), 720, 0),
            Some(T0 + DAY + 12 * 3600)
        );
        assert_eq!(
            next_slot(T0 + 12 * 3600 - 1, w("daily"), 720, 0),
            Some(T0 + 12 * 3600)
        );
        assert_eq!(
            next_slot(T0, w("sun"), 780, 0),
            Some(T0 + 3 * DAY + 13 * 3600)
        );
        assert_eq!(next_slot(T0, w("thu"), 0, 0), Some(T0 + 7 * DAY));
        assert_eq!(next_slot(T0, w("thu"), 1, 0), Some(T0 + 60));
        assert_eq!(next_slot(T0, w("daily"), 720, 7200), Some(T0 + 10 * 3600));
        assert_eq!(next_slot(T0, w("daily"), 60, -18000), Some(T0 + 6 * 3600));
        assert_eq!(next_slot(T0, When::Off, 60, 0), None);
    }

    #[test]
    fn the_history_is_hunterctls_tab_separated_lines() {
        let h = parse_history(
            "2026-10-29T12:00:03\tquick\tscheduled\t41\t1840\t1\t0\tinfected\t20261029-120003-quick.log\n\
             garbage\n\
             2026-11-01T13:00:00\tfull\tmanual\t600\t90210\t0\t0\tclean\t20261101-130000-full.log\n",
        );
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].infected, 1);
        assert_eq!(h[1].files, 90210);
        assert_eq!(h[1].trigger, "manual");
    }
}
