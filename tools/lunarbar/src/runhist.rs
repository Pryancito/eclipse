//! What lunarrun remembers: every command it launched, how often and when
//! last, so what you use most comes first and what you typed once at the
//! prompt comes back as a row of its own.
//!
//! Kept in `$XDG_STATE_HOME/lunar/run-history` (`~/.local/state/...` when that
//! is unset), one `count<TAB>last<TAB>command` line per command, at most
//! [`MAX`] of them: the least used go first when it is full.
//!
//! The order is by *frecency*, the way a browser's address bar ranks: uses
//! weighed by how recent the last one was, so a program used every day last
//! year does not stay above the one used every day this week.

use std::path::PathBuf;

/// How many commands are remembered.
pub const MAX: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Use {
    pub exec: String,
    pub count: u32,
    /// Seconds since the epoch of the last launch.
    pub last: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    pub uses: Vec<Use>,
}

/// How much one use counts, `age` seconds after it.
pub fn weight(age: i64) -> f32 {
    const DAY: i64 = 86_400;
    match age {
        a if a <= 4 * DAY => 1.0,
        a if a <= 14 * DAY => 0.7,
        a if a <= 31 * DAY => 0.5,
        a if a <= 90 * DAY => 0.3,
        _ => 0.1,
    }
}

impl History {
    /// Read the file's text. A line that does not parse is skipped, not fatal:
    /// a history is a convenience, never a reason not to open.
    pub fn decode(s: &str) -> Self {
        let mut uses = Vec::new();
        for line in s.lines() {
            let mut f = line.splitn(3, '\t');
            let (Some(c), Some(l), Some(e)) = (f.next(), f.next(), f.next()) else {
                continue;
            };
            let (Ok(count), Ok(last)) = (c.parse(), l.parse()) else {
                continue;
            };
            if !e.trim().is_empty() && !e.contains('\n') {
                uses.push(Use { exec: e.to_string(), count, last });
            }
        }
        uses.truncate(MAX);
        Self { uses }
    }

    pub fn encode(&self) -> String {
        self.uses
            .iter()
            .map(|u| format!("{}\t{}\t{}\n", u.count, u.last, u.exec))
            .collect()
    }

    /// One more launch of `exec` at `now`.
    pub fn record(&mut self, exec: &str, now: i64) {
        let exec = exec.trim();
        if exec.is_empty() || exec.contains('\n') {
            return;
        }
        match self.uses.iter_mut().find(|u| u.exec == exec) {
            Some(u) => {
                u.count = u.count.saturating_add(1);
                u.last = now;
            }
            None => {
                if self.uses.len() >= MAX {
                    // Room is made by the one that ranks lowest.
                    if let Some(i) = (0..self.uses.len()).min_by(|&a, &b| {
                        self.score_of(&self.uses[a], now).total_cmp(&self.score_of(&self.uses[b], now))
                    }) {
                        self.uses.remove(i);
                    }
                }
                self.uses.push(Use { exec: exec.to_string(), count: 1, last: now });
            }
        }
    }

    fn score_of(&self, u: &Use, now: i64) -> f32 {
        u.count as f32 * weight(now - u.last)
    }

    /// How `exec` ranks at `now`; 0 for a command never launched.
    pub fn score(&self, exec: &str, now: i64) -> f32 {
        self.uses.iter().find(|u| u.exec == exec).map_or(0.0, |u| self.score_of(u, now))
    }

    /// Where the history lives.
    pub fn path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
        Some(base.join("lunar").join("run-history"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| Self::decode(&s))
            .unwrap_or_default()
    }

    /// Write it back, through a temporary file so a crash mid-write leaves the
    /// old history and not half of a new one.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = Self::path() else {
            return Ok(());
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, self.encode())?;
        std::fs::rename(tmp, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    #[test]
    fn it_reads_back_what_it_wrote_and_skips_what_it_cannot_read() {
        let mut h = History::default();
        h.record("firefox", NOW);
        h.record("firefox", NOW + 5);
        h.record("top -d 1", NOW);
        let back = History::decode(&h.encode());
        assert_eq!(back, h);
        assert_eq!(back.uses[0].count, 2);
        let junk = History::decode("x\ty\tz\n3\t10\n\n2\t5\tfoot\n");
        assert_eq!(junk.uses, [Use { exec: "foot".into(), count: 2, last: 5 }]);
    }

    #[test]
    fn recent_use_outranks_old_use() {
        let mut h = History::default();
        for _ in 0..10 {
            h.record("old", NOW - 200 * DAY);
        }
        for _ in 0..3 {
            h.record("new", NOW - DAY);
        }
        assert!(h.score("new", NOW) > h.score("old", NOW));
        assert_eq!(h.score("never", NOW), 0.0);
    }

    #[test]
    fn a_full_history_forgets_its_least_used_command() {
        let mut h = History::default();
        for i in 0..MAX {
            h.record(&format!("c{i}"), NOW);
            h.record(&format!("c{i}"), NOW);
        }
        h.record("c0", NOW);
        h.uses.iter_mut().find(|u| u.exec == "c7").unwrap().count = 1;
        h.record("fresh", NOW);
        assert_eq!(h.uses.len(), MAX);
        assert!(h.uses.iter().all(|u| u.exec != "c7"));
        assert!(h.uses.iter().any(|u| u.exec == "fresh"));
        // Blank or multi-line commands are not remembered.
        h.record("  ", NOW);
        h.record("a\nb", NOW);
        assert_eq!(h.uses.len(), MAX);
    }
}
