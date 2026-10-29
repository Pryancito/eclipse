//! `/proc/hunter_net`, hunter's network monitor, as lunarhunter's *Red* tab
//! reads it: every inet socket with the process that holds it, and which
//! listeners nobody said to expect.
//!
//! The kernel takes a fresh snapshot on every read and raises the alerts
//! itself, so this side only parses and words things. Changing a rule goes
//! through `hunterctl expect-port` / `forget-port`, which write the kernel and
//! `/etc/hunter/ports` both, so an *Allow* here survives a reboot.

use std::io::Write;

/// The kernel's table.
pub const PROC_NET: &str = "/proc/hunter_net";

/// What hunter thinks of one socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A listener nobody expected.
    Alert,
    /// An expected listener.
    Listen,
    /// A connection with a peer.
    Connected,
    /// Anything else (a socket not yet bound or connected).
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetSock {
    pub pid: u64,
    /// `tcp`, `tcp6`, `udp`, `udp6`.
    pub proto: String,
    /// `LISTEN`, `ESTAB`, `UNCONN`, `CLOSE`.
    pub state: String,
    pub local: String,
    pub remote: Option<String>,
    pub verdict: Verdict,
    pub program: String,
}

impl NetSock {
    /// The local port, the number after the last `:`.
    pub fn port(&self) -> Option<u16> {
        self.local.rsplit_once(':')?.1.parse().ok()
    }

    /// `tcp` or `udp`, which is what a rule names: a rule covers both
    /// families.
    pub fn transport(&self) -> &str {
        self.proto.trim_end_matches('6')
    }

    /// The `hunterctl expect-port` arguments that make this listener
    /// expected, for this program only: allowing `nc` on 4444 must not allow
    /// whatever else takes 4444 next.
    pub fn expect_args(&self) -> Option<Vec<String>> {
        let port = self.port()?;
        let mut args = vec![
            "expect-port".to_string(),
            self.transport().to_string(),
            port.to_string(),
        ];
        if self.program.starts_with('/') {
            args.push(self.program.clone());
        }
        Some(args)
    }

    /// The program's file name, which is what a narrow column can show.
    pub fn short_program(&self) -> &str {
        self.program.rsplit('/').next().unwrap_or(&self.program)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetTable {
    pub alerts: bool,
    pub ignore_loopback: bool,
    /// Unexpected listeners reported since boot.
    pub reported: u64,
    /// Each rule as the language writes it (`expect tcp 22 /usr/sbin/sshd`).
    pub rules: Vec<String>,
    pub socks: Vec<NetSock>,
}

impl NetTable {
    pub fn unexpected(&self) -> usize {
        self.socks
            .iter()
            .filter(|s| s.verdict == Verdict::Alert)
            .count()
    }

    pub fn listeners(&self) -> usize {
        self.socks
            .iter()
            .filter(|s| matches!(s.verdict, Verdict::Alert | Verdict::Listen))
            .count()
    }

    /// The sockets in the order the tab lists them: the alerts first, then
    /// the listeners, then the connections, each by process.
    pub fn sorted(&self) -> Vec<&NetSock> {
        let rank = |v: Verdict| match v {
            Verdict::Alert => 0,
            Verdict::Listen => 1,
            Verdict::Connected => 2,
            Verdict::Idle => 3,
        };
        let mut v: Vec<&NetSock> = self.socks.iter().collect();
        v.sort_by_key(|s| (rank(s.verdict), s.pid, s.port()));
        v
    }

    /// How many distinct processes hold a socket.
    pub fn processes(&self) -> usize {
        let mut pids: Vec<u64> = self.socks.iter().map(|s| s.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        pids.len()
    }
}

/// The `hunterctl forget-port` arguments for a rule line.
pub fn forget_args(rule: &str) -> Option<Vec<String>> {
    let rest = rule.strip_prefix("expect ")?;
    let mut args = vec!["forget-port".to_string()];
    args.extend(rest.split_whitespace().map(str::to_string));
    (3..=4).contains(&args.len()).then_some(args)
}

/// Parse the kernel's table. `None` when it is not one (no such file, or a
/// kernel whose hunter has no network monitor).
pub fn parse(text: &str) -> Option<NetTable> {
    let mut lines = text.lines();
    if lines.next()? != "hunter netmon" {
        return None;
    }
    let mut t = NetTable::default();
    for line in lines {
        if line.starts_with("alerts=") {
            for kv in line.split_whitespace() {
                match kv.split_once('=') {
                    Some(("alerts", v)) => t.alerts = v == "on",
                    Some(("ignore-loopback", v)) => t.ignore_loopback = v == "on",
                    Some(("reported", v)) => t.reported = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
        } else if line.starts_with("expect ") {
            t.rules.push(line.to_string());
        } else if let Some(rest) = line.strip_prefix("sock ") {
            // pid proto state local remote verdict program (program last: it
            // is the one field that may hold a space).
            let mut f = rest.splitn(7, ' ');
            let (Some(pid), Some(proto), Some(state), Some(local), Some(remote), Some(v)) =
                (f.next(), f.next(), f.next(), f.next(), f.next(), f.next())
            else {
                continue;
            };
            let Ok(pid) = pid.parse() else { continue };
            let verdict = match v {
                "ALERT" => Verdict::Alert,
                "listen" => Verdict::Listen,
                "ok" => Verdict::Connected,
                _ => Verdict::Idle,
            };
            t.socks.push(NetSock {
                pid,
                proto: proto.to_string(),
                state: state.to_string(),
                local: local.to_string(),
                remote: (remote != "-").then(|| remote.to_string()),
                verdict,
                program: f.next().unwrap_or("").to_string(),
            });
        }
    }
    Some(t)
}

/// Read the kernel's table (which also refreshes its alerts).
pub fn read() -> Option<NetTable> {
    parse(&std::fs::read_to_string(PROC_NET).ok()?)
}

/// Send one netmon script in one write (`alerts off`, ...), all or nothing as
/// for `/proc/hunter`.
pub fn send(script: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(PROC_NET)?;
    let n = f.write(script.as_bytes())?;
    if n == script.len() {
        Ok(())
    } else {
        Err(std::io::Error::other("short write"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "hunter netmon\n\
alerts=on ignore-loopback=on sockets=4 listeners=3 unexpected=1 reported=2\n\
expect udp 68\n\
expect tcp 22 /usr/sbin/sshd\n\
sock 12 tcp LISTEN 0.0.0.0:22 - listen /usr/sbin/sshd\n\
sock 42 tcp LISTEN 0.0.0.0:4444 - ALERT /usr/bin/nc\n\
sock 77 tcp6 ESTAB [fe80::1]:50000 [2001:db8::1]:443 ok /opt/My App/bin/app\n\
sock 3 udp UNCONN 0.0.0.0:68 - listen /sbin/edhcpc\n";

    #[test]
    fn the_kernel_table_parses() {
        let t = parse(TABLE).unwrap();
        assert!(t.alerts && t.ignore_loopback);
        assert_eq!(t.reported, 2);
        assert_eq!(t.rules, ["expect udp 68", "expect tcp 22 /usr/sbin/sshd"]);
        assert_eq!(t.socks.len(), 4);
        assert_eq!((t.unexpected(), t.listeners(), t.processes()), (1, 3, 4));
        let app = &t.socks[2];
        assert_eq!(app.program, "/opt/My App/bin/app");
        assert_eq!(app.remote.as_deref(), Some("[2001:db8::1]:443"));
        assert_eq!(app.port(), Some(50000));
        assert_eq!(app.transport(), "tcp");
        assert_eq!(app.short_program(), "app");
    }

    #[test]
    fn alerts_sort_first() {
        let t = parse(TABLE).unwrap();
        let order: Vec<u64> = t.sorted().iter().map(|s| s.pid).collect();
        assert_eq!(order, [42, 3, 12, 77]);
    }

    #[test]
    fn allowing_a_listener_names_its_program() {
        let t = parse(TABLE).unwrap();
        assert_eq!(
            t.socks[1].expect_args().unwrap(),
            ["expect-port", "tcp", "4444", "/usr/bin/nc"]
        );
        assert_eq!(
            forget_args("expect tcp 22 /usr/sbin/sshd").unwrap(),
            ["forget-port", "tcp", "22", "/usr/sbin/sshd"]
        );
        assert_eq!(forget_args("expect any 6000-6010").unwrap().len(), 3);
        assert_eq!(forget_args("alerts on"), None);
    }

    #[test]
    fn not_a_netmon_table_is_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("hunter security subsystem v0.4.0\n"), None);
    }
}
