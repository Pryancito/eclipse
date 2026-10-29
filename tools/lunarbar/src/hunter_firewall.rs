//! hunter's per-application firewall, as the desktop sees it: what
//! `/proc/hunter_firewall` says (the mode, the rules, the open questions), the
//! directives that answer a question, and `/etc/hunter/firewall`, the file
//! that makes the answers survive a reboot.
//!
//! The kernel decides; this only reads and writes the two places the kernel
//! reads. An answer goes to `/proc/hunter` as a `net allow|deny` directive, so
//! it takes effect on the spot -- a program waiting on the question under
//! `enforce` goes through or fails right then -- and the whole live rule set
//! is then written back to `/etc/hunter/firewall`. Writing the live set rather
//! than appending the one new line keeps the file equal to what the kernel
//! holds: a rule forgotten here is gone from the file too, and a file edited by
//! hand is not doubled up.

use crate::hunter::Mode;

/// Read: the rules and the open questions.
pub const PROC: &str = "/proc/hunter_firewall";
/// The rules the kernel loads at boot.
pub const RULES_FILE: &str = "/etc/hunter/firewall";

/// Which way a program is reaching the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// It connects out.
    Out,
    /// It listens: a server.
    In,
}

impl Dir {
    pub fn parse(s: &str) -> Option<Dir> {
        match s {
            "out" => Some(Dir::Out),
            "in" => Some(Dir::In),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Dir::Out => "out",
            Dir::In => "in",
        }
    }
}

/// One rule. `dir` None is both directions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub allow: bool,
    pub dir: Option<Dir>,
    pub program: String,
}

/// One open question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub dir: Dir,
    pub count: u64,
    pub pid: u64,
    /// Seconds since boot.
    pub first: u64,
    pub last: u64,
    /// The address it last tried, `1.2.3.4:443`.
    pub peer: String,
    pub program: String,
}

impl Question {
    /// The program's file name, which is what a person recognises.
    pub fn name(&self) -> &str {
        self.program.rsplit('/').next().unwrap_or(&self.program)
    }

    /// The port it wants, from the peer.
    pub fn port(&self) -> &str {
        self.peer.rsplit(':').next().unwrap_or("")
    }
}

/// `/proc/hunter_firewall`, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Firewall {
    pub mode: Mode,
    pub timeout: u32,
    /// The kernel's clock when it was read, seconds since boot.
    pub now: u64,
    pub rules: Vec<Rule>,
    pub questions: Vec<Question>,
}

/// Parses `/proc/hunter_firewall`. `None` when it does not say its mode, which
/// is the first thing the kernel writes: that is not the file.
pub fn parse(text: &str) -> Option<Firewall> {
    let mut mode = None;
    let mut timeout = 30;
    let mut now = 0;
    let mut rules = Vec::new();
    let mut questions = Vec::new();
    for line in text.lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "mode" => mode = Mode::parse(rest),
            "timeout" => timeout = rest.parse().unwrap_or(timeout),
            "now" => now = rest.parse().unwrap_or(0),
            "rule" => {
                // rule <allow|deny> <out|in|both> <path...>
                let mut it = rest.splitn(3, ' ');
                let (Some(a), Some(d), Some(p)) = (it.next(), it.next(), it.next()) else {
                    continue;
                };
                let allow = match a {
                    "allow" => true,
                    "deny" => false,
                    _ => continue,
                };
                let dir = match d {
                    "both" => None,
                    d => match Dir::parse(d) {
                        Some(d) => Some(d),
                        None => continue,
                    },
                };
                rules.push(Rule {
                    allow,
                    dir,
                    program: p.to_string(),
                });
            }
            "pending" => {
                // pending <dir> <count> <pid> <first> <last> <peer> <path...>
                let f: Vec<&str> = rest.splitn(7, ' ').collect();
                if f.len() != 7 {
                    continue;
                }
                let (Some(dir), Ok(count), Ok(pid), Ok(first), Ok(last)) = (
                    Dir::parse(f[0]),
                    f[1].parse(),
                    f[2].parse(),
                    f[3].parse(),
                    f[4].parse(),
                ) else {
                    continue;
                };
                questions.push(Question {
                    dir,
                    count,
                    pid,
                    first,
                    last,
                    peer: f[5].to_string(),
                    program: f[6].to_string(),
                });
            }
            _ => {}
        }
    }
    Some(Firewall {
        mode: mode?,
        timeout,
        now,
        rules,
        questions,
    })
}

/// Reads the kernel's firewall. `None` on a kernel without one.
pub fn read() -> Option<Firewall> {
    parse(&std::fs::read_to_string(PROC).ok()?)
}

/// The directive that answers a question: a rule for that program in that
/// direction.
pub fn answer_script(q: &Question, allow: bool) -> String {
    format!(
        "net {} {} {}\n",
        if allow { "allow" } else { "deny" },
        q.program,
        q.dir.as_str()
    )
}

/// The directive that drops a program's rules, so it is asked about again.
pub fn forget_script(program: &str) -> String {
    format!("net forget {program}\n")
}

/// The directive that drops a program's questions without deciding.
pub fn dismiss_script(program: &str) -> String {
    format!("net dismiss {program}\n")
}

/// The directive that sets the firewall's mode.
pub fn mode_script(mode: Mode) -> String {
    format!("mode net {}\n", mode.as_str())
}

/// `/etc/hunter/firewall` for the live firewall: its mode, its timeout and
/// every rule, in the file's own syntax (a `net` directive without the
/// `net`), the path last so a space in it needs no quoting.
pub fn rules_file(fw: &Firewall, when: &str) -> String {
    let mut out = format!(
        "# hunter: reglas del cortafuegos por aplicación.\n\
         # Escrito por lunarhunter el {when}; se cargan al arrancar.\n\
         # allow|deny <programa> [in|out]  -- sin dirección, las dos.\n\
         mode {}\n\
         timeout {}\n",
        fw.mode.as_str(),
        fw.timeout
    );
    for r in &fw.rules {
        out.push_str(if r.allow { "allow " } else { "deny " });
        out.push_str(&r.program);
        if let Some(d) = r.dir {
            out.push(' ');
            out.push_str(d.as_str());
        }
        out.push('\n');
    }
    out
}

/// Writes [`rules_file`] to `path` through a temporary file and a rename, so a
/// crash halfway leaves the old rules and not half of the new ones.
pub fn save(fw: &Firewall, when: &str, path: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.new");
    std::fs::write(&tmp, rules_file(fw, when))
        .and_then(|_| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "mode enforce\ntimeout 30\nnow 130\n\
        rule allow both /usr/bin/curl\n\
        rule deny in /opt/My App/srv\n\
        rule allow out /opt/My App/srv\n\
        pending out 3 42 100 125 93.184.216.34:443 /usr/bin/wget\n\
        pending in 1 7 120 120 0.0.0.0:8080 /opt/Other App/web\n";

    #[test]
    fn the_proc_file_parses() {
        let fw = parse(SAMPLE).unwrap();
        assert_eq!(fw.mode, Mode::Enforce);
        assert_eq!(fw.timeout, 30);
        assert_eq!(fw.now, 130);
        assert_eq!(fw.rules.len(), 3);
        assert_eq!(
            fw.rules[1],
            Rule {
                allow: false,
                dir: Some(Dir::In),
                program: "/opt/My App/srv".into()
            }
        );
        assert_eq!(fw.rules[0].dir, None);
        assert_eq!(fw.questions.len(), 2);
        let q = &fw.questions[0];
        assert_eq!((q.dir, q.count, q.pid), (Dir::Out, 3, 42));
        assert_eq!(q.name(), "wget");
        assert_eq!(q.port(), "443");
        assert_eq!(fw.questions[1].program, "/opt/Other App/web");
        assert!(
            parse("rule allow both /x\n").is_none(),
            "no mode, not the file"
        );
    }

    #[test]
    fn answers_are_net_directives() {
        let fw = parse(SAMPLE).unwrap();
        assert_eq!(
            answer_script(&fw.questions[0], true),
            "net allow /usr/bin/wget out\n"
        );
        assert_eq!(
            answer_script(&fw.questions[1], false),
            "net deny /opt/Other App/web in\n"
        );
        assert_eq!(forget_script("/usr/bin/curl"), "net forget /usr/bin/curl\n");
        assert_eq!(dismiss_script("/bin/x"), "net dismiss /bin/x\n");
        assert_eq!(mode_script(Mode::Report), "mode net report\n");
    }

    #[test]
    fn the_rules_file_holds_the_live_stance() {
        let fw = parse(SAMPLE).unwrap();
        let text = rules_file(&fw, "2026-10-29 19:00");
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            [
                "mode enforce",
                "timeout 30",
                "allow /usr/bin/curl",
                "deny /opt/My App/srv in",
                "allow /opt/My App/srv out",
            ]
        );
    }

    #[test]
    fn saving_replaces_the_file() {
        let dir = std::env::temp_dir().join(format!("lunarhunter-fw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("firewall");
        let path = path.to_str().unwrap();
        std::fs::write(path, "old\n").unwrap();
        save(&parse(SAMPLE).unwrap(), "hoy", path).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("allow /usr/bin/curl\n"));
        assert!(!text.contains("old"));
        assert!(!std::path::Path::new(&format!("{path}.new")).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
