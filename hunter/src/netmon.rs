//! The network monitor: which process holds which socket, and an alert for a
//! port that starts listening without anyone having said it would.
//!
//! hunter's other domains watch what runs. None of them watched what a
//! running program does on the network, and the first thing a backdoor does
//! once it runs is open a port (`nc -l -p 4444 -e /bin/sh`) or call home. The
//! kernel already rendered `/proc/net/tcp`, but that table says nothing about
//! WHICH process holds a socket, so `netstat -p` had nothing to answer with
//! and an operator could not tell `sshd` on 22 from a shell on 4444 without
//! walking every `/proc/<pid>/fd` by hand.
//!
//! ## What it does
//!
//! The kernel walks every process's descriptors and hands [`observe`] one
//! [`Sock`] per inet socket (TCP or UDP, v4 or v6) with its owner. `observe`
//! keeps that snapshot for [`render`] (`/proc/hunter_net`) and compares each
//! *listener* against the expected ones:
//!
//! * a TCP socket in `LISTEN`, or
//! * an unconnected UDP socket on a port below the ephemeral range
//!   ([`EPHEMERAL_START`]): a client's DNS query binds an ephemeral port and
//!   is not a service, while `bind(0.0.0.0:53)` is one.
//!
//! A listener no [`Rule`] expects is a `WARN` event in category `NET`, once per
//! process and port: the shield in lunarbar lights up and the event names the
//! program. A listener on loopback only is not exposed to the network and is
//! not an alert unless `ignore-loopback off` says it should be.
//!
//! The kernel also calls [`on_listen`] from `listen(2)`, so a TCP listener is
//! reported the moment it opens, not the next time someone reads the table.
//! UDP has no `listen`; it is caught by the next snapshot.
//!
//! ## Rules
//!
//! `/etc/hunter/ports` at boot and a root write to `/proc/hunter_net` speak
//! the same small language, one directive per line, `#` comments:
//!
//! | Directive                                  | Effect                                   |
//! |--------------------------------------------|------------------------------------------|
//! | `expect <tcp\|udp\|any> <port\|lo-hi> [<program>]` | that listener is expected (by that program only, if named) |
//! | `forget <tcp\|udp\|any> <port\|lo-hi> [<program>]` | drop a rule written the same way        |
//! | `alerts <on\|off>`                         | unexpected listeners raise events or not |
//! | `ignore-loopback <on\|off>`                | loopback-only listeners are exempt       |
//!
//! Like the control plane a script is all or nothing, and once hunter is
//! sealed (`seal`, see [`crate::control`]) the directives that relax are
//! refused: `expect`, `alerts off` and `ignore-loopback on`.
//!
//! ## What it does not do
//!
//! It watches; it does not block. Refusing a `bind`, a `listen` or a
//! `connect` is a firewall's job and a decision per program, which is a
//! separate domain. A monitor that only reads cannot break a service by being
//! wrong, which is what lets it be on by default.

extern crate alloc;

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lock::Mutex;

use crate::event_log::{record, Severity};
use crate::policy;

/// The first port of the dynamic range the kernel hands out for an unbound
/// socket (`get_ephemeral_port`, RFC 6335). A UDP socket on a port at or
/// above it was bound by the kernel for a client, not by a service.
pub const EPHEMERAL_START: u16 = 49152;

/// Transport of a socket. The family (v4 or v6) is kept apart in [`Sock::v6`]
/// because a rule does not care about it: `expect tcp 22` covers both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Proto {
    Tcp,
    Udp,
}

impl Proto {
    pub fn as_str(self, v6: bool) -> &'static str {
        match (self, v6) {
            (Proto::Tcp, false) => "tcp",
            (Proto::Tcp, true) => "tcp6",
            (Proto::Udp, false) => "udp",
            (Proto::Udp, true) => "udp6",
        }
    }
}

/// What a socket is doing, as far as the kernel can say without asking the
/// TCP state machine for every transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// A TCP socket after `listen(2)`.
    Listen,
    /// A TCP socket with a peer, or a UDP socket `connect`ed to one.
    Established,
    /// A UDP socket with a local port and no peer: it takes datagrams from
    /// anyone (`ss` says `UNCONN`).
    Unconnected,
    /// Neither bound nor connected yet, or already closed.
    Closed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Listen => "LISTEN",
            State::Established => "ESTAB",
            State::Unconnected => "UNCONN",
            State::Closed => "CLOSE",
        }
    }
}

/// One inet socket and the process that holds it. A socket shared by several
/// processes (inherited across `fork`) is one `Sock` per holder, as in
/// `netstat -p`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sock {
    pub pid: u64,
    /// The program the process runs (its `execve` path), or its name when it
    /// has none.
    pub program: String,
    pub proto: Proto,
    pub v6: bool,
    pub state: State,
    /// Local address, without the port (`0.0.0.0`, `::`, `10.0.2.15`).
    pub local_addr: String,
    pub local_port: u16,
    /// The peer, when there is one.
    pub remote: Option<(String, u16)>,
}

impl Sock {
    /// Whether this socket is a service waiting for anyone to talk to it.
    pub fn is_listener(&self) -> bool {
        match (self.proto, self.state) {
            (Proto::Tcp, State::Listen) => true,
            (Proto::Udp, State::Unconnected) => {
                self.local_port != 0 && self.local_port < EPHEMERAL_START
            }
            _ => false,
        }
    }

    /// Whether the listener only answers this machine (`127.0.0.0/8`, `::1`).
    pub fn is_loopback(&self) -> bool {
        is_loopback_addr(&self.local_addr)
    }
}

fn is_loopback_addr(addr: &str) -> bool {
    addr.starts_with("127.") || addr == "::1"
}

/// Which transport a rule covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleProto {
    Tcp,
    Udp,
    Any,
}

impl RuleProto {
    fn covers(self, p: Proto) -> bool {
        matches!(
            (self, p),
            (RuleProto::Any, _) | (RuleProto::Tcp, Proto::Tcp) | (RuleProto::Udp, Proto::Udp)
        )
    }

    fn as_str(self) -> &'static str {
        match self {
            RuleProto::Tcp => "tcp",
            RuleProto::Udp => "udp",
            RuleProto::Any => "any",
        }
    }
}

/// An expected listener: a transport, a port range and optionally the one
/// program allowed to hold it. `expect tcp 22 /usr/sbin/sshd` does not make
/// port 22 fine for a shell that grabbed it first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rule {
    pub proto: RuleProto,
    pub lo: u16,
    pub hi: u16,
    pub program: Option<String>,
}

impl Rule {
    pub fn matches(&self, s: &Sock) -> bool {
        self.proto.covers(s.proto)
            && (self.lo..=self.hi).contains(&s.local_port)
            && self.program.as_ref().is_none_or(|p| *p == s.program)
    }

    /// The rule as the language writes it, so `/proc/hunter_net` can be fed
    /// back to itself.
    pub fn to_line(&self) -> String {
        let mut out = format!("expect {} ", self.proto.as_str());
        if self.lo == self.hi {
            let _ = write!(out, "{}", self.lo);
        } else {
            let _ = write!(out, "{}-{}", self.lo, self.hi);
        }
        if let Some(p) = &self.program {
            out.push(' ');
            out.push_str(p);
        }
        out
    }
}

/// One parsed line of the netmon language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    Expect(Rule),
    Forget(Rule),
    Alerts(bool),
    IgnoreLoopback(bool),
}

/// Parses one line. `Ok(None)` for a blank line or a comment.
pub fn parse_line(line: &str) -> Result<Option<Directive>, String> {
    let line = match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
    .trim();
    if line.is_empty() {
        return Ok(None);
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let switch = |w: &[&str]| -> Result<bool, String> {
        match w {
            [_, "on"] => Ok(true),
            [_, "off"] => Ok(false),
            _ => Err(format!("'{}' takes 'on' or 'off'", w[0])),
        }
    };
    Ok(Some(match words[0] {
        "expect" => Directive::Expect(parse_rule(&words)?),
        "forget" => Directive::Forget(parse_rule(&words)?),
        "alerts" => Directive::Alerts(switch(&words)?),
        "ignore-loopback" => Directive::IgnoreLoopback(switch(&words)?),
        other => return Err(format!("unknown directive '{}'", other)),
    }))
}

fn parse_rule(words: &[&str]) -> Result<Rule, String> {
    if !(3..=4).contains(&words.len()) {
        return Err(format!(
            "'{}' takes '<tcp|udp|any> <port|lo-hi> [<program>]'",
            words[0]
        ));
    }
    let proto = match words[1] {
        "tcp" => RuleProto::Tcp,
        "udp" => RuleProto::Udp,
        "any" => RuleProto::Any,
        other => return Err(format!("unknown protocol '{}' (tcp, udp, any)", other)),
    };
    let port = |w: &str| -> Result<u16, String> {
        match w.parse::<u16>() {
            Ok(p) if p > 0 => Ok(p),
            _ => Err(format!("bad port '{}' (1-65535)", w)),
        }
    };
    let (lo, hi) = match words[2].split_once('-') {
        Some((a, b)) => (port(a)?, port(b)?),
        None => {
            let p = port(words[2])?;
            (p, p)
        }
    };
    if lo > hi {
        return Err(format!("empty port range '{}'", words[2]));
    }
    let program = match words.get(3) {
        None | Some(&"*") => None,
        Some(p) if p.starts_with('/') => Some(String::from(*p)),
        Some(p) => return Err(format!("program must be an absolute path: {}", p)),
    };
    Ok(Rule {
        proto,
        lo,
        hi,
        program,
    })
}

/// Parses a whole script, or answers the first bad line (1-based) and why.
pub fn parse_script(text: &str) -> Result<Vec<Directive>, (usize, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        match parse_line(line) {
            Ok(Some(d)) => out.push(d),
            Ok(None) => {}
            Err(e) => return Err((i + 1, e)),
        }
    }
    Ok(out)
}

/// What every boot starts with, before `/etc/hunter/ports`: the DHCP clients,
/// which bind their well-known port to hear the server's answer. Nothing else
/// on a fresh system listens on an inet socket (the desktop's services are
/// all AF_UNIX).
fn default_rules() -> Vec<Rule> {
    let rule = |port| Rule {
        proto: RuleProto::Udp,
        lo: port,
        hi: port,
        program: None,
    };
    alloc::vec![rule(68), rule(546)]
}

lazy_static::lazy_static! {
    static ref RULES: Mutex<Vec<Rule>> = Mutex::new(default_rules());
    /// The last snapshot [`observe`] was given.
    static ref SNAPSHOT: Mutex<Vec<Sock>> = Mutex::new(Vec::new());
    /// Listeners already reported, by (pid, transport, port): one event per
    /// listener, not one per read of the table.
    static ref ALERTED: Mutex<BTreeSet<(u64, Proto, u16)>> = Mutex::new(BTreeSet::new());
}

static ALERTS: AtomicBool = AtomicBool::new(true);
static IGNORE_LOOPBACK: AtomicBool = AtomicBool::new(true);
/// Unexpected listeners reported since boot.
static ALERT_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn alerts_enabled() -> bool {
    ALERTS.load(Ordering::SeqCst)
}

pub fn ignore_loopback() -> bool {
    IGNORE_LOOPBACK.load(Ordering::SeqCst)
}

/// Unexpected listeners reported since boot.
pub fn reported() -> u64 {
    ALERT_COUNT.load(Ordering::Relaxed)
}

pub fn rules() -> Vec<Rule> {
    RULES.lock().clone()
}

/// Whether a listener is one nobody said to expect.
pub fn is_unexpected(s: &Sock) -> bool {
    if !s.is_listener() || (s.is_loopback() && ignore_loopback()) {
        return false;
    }
    !RULES.lock().iter().any(|r| r.matches(s))
}

/// Reports `s` if it is an unexpected listener not yet reported.
fn check(s: &Sock) {
    if !alerts_enabled() || !is_unexpected(s) {
        return;
    }
    if !ALERTED.lock().insert((s.pid, s.proto, s.local_port)) {
        return;
    }
    ALERT_COUNT.fetch_add(1, Ordering::Relaxed);
    record(
        s.pid,
        Severity::Warning,
        "NET",
        "WARNING",
        format!(
            "unexpected listener: {} {}:{} by {}",
            s.proto.as_str(s.v6),
            s.local_addr,
            s.local_port,
            s.program
        ),
    );
    warn!(
        "hunter: unexpected listener {} {}:{} [pid={}] {}",
        s.proto.as_str(s.v6),
        s.local_addr,
        s.local_port,
        s.pid,
        s.program
    );
}

/// `listen(2)` hook: the kernel calls it once the socket is listening.
pub fn on_listen(s: &Sock) {
    check(s);
}

/// Takes a fresh snapshot of every inet socket and its holder, reports the
/// listeners nobody expected, and keeps it for [`render`].
///
/// A listener's "already reported" mark lives as long as its process holds
/// that port: once it is gone from a snapshot the mark goes too, so the same
/// program listening again later is a new event.
pub fn observe(socks: Vec<Sock>) {
    {
        let live: BTreeSet<(u64, Proto, u16)> = socks
            .iter()
            .filter(|s| s.is_listener())
            .map(|s| (s.pid, s.proto, s.local_port))
            .collect();
        ALERTED.lock().retain(|k| live.contains(k));
    }
    for s in &socks {
        check(s);
    }
    *SNAPSHOT.lock() = socks;
}

/// Refused once sealed: anything that silences an alert.
fn relaxes(d: &Directive) -> bool {
    matches!(
        d,
        Directive::Expect(_) | Directive::Alerts(false) | Directive::IgnoreLoopback(true)
    )
}

/// Applies one directive, recording it (or its refusal under the latch).
pub fn apply(d: &Directive, pid: u64) {
    if relaxes(d) && policy::is_tighten_only() {
        record(
            pid,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing netmon change: {} (tighten-only latch)",
                describe(d)
            ),
        );
        return;
    }
    match d {
        Directive::Expect(r) => {
            let mut rules = RULES.lock();
            if !rules.contains(r) {
                rules.push(r.clone());
            }
        }
        Directive::Forget(r) => RULES.lock().retain(|x| x != r),
        Directive::Alerts(on) => ALERTS.store(*on, Ordering::SeqCst),
        Directive::IgnoreLoopback(on) => IGNORE_LOOPBACK.store(*on, Ordering::SeqCst),
    }
    record(pid, Severity::Notice, "CONTROL", "CONFIG", describe(d));
}

fn describe(d: &Directive) -> String {
    let sw = |on: bool| if on { "on" } else { "off" };
    match d {
        Directive::Expect(r) => format!("netmon {}", r.to_line()),
        Directive::Forget(r) => format!("netmon forget{}", &r.to_line()["expect".len()..]),
        Directive::Alerts(on) => format!("netmon alerts {}", sw(*on)),
        Directive::IgnoreLoopback(on) => format!("netmon ignore-loopback {}", sw(*on)),
    }
}

/// Parses `text` and, only if every line parses, applies it in order.
/// `origin` names the file or `/proc` node, for the audit trail.
pub fn apply_script(text: &str, origin: &str, pid: u64) -> Result<usize, (usize, String)> {
    let directives = parse_script(text).inspect_err(|(line, why)| {
        record(
            pid,
            Severity::Critical,
            "CONTROL",
            "REJECTED",
            format!("{}: line {}: {}; nothing applied", origin, line, why),
        );
    })?;
    for d in &directives {
        apply(d, pid);
    }
    Ok(directives.len())
}

/// `/proc/hunter_net`: the settings, the rules (as directives, so the block
/// can be saved to `/etc/hunter/ports` as is) and one line per socket:
///
/// ```text
/// sock <pid> <proto> <state> <local> <remote|-> <ok|listen|ALERT|-> <program>
/// ```
///
/// The verdict column says `ALERT` for an unexpected listener, `listen` for an
/// expected one, `ok` for anything else with a peer and `-` for the rest. The
/// program is last because it is the one field that may hold a space.
pub fn render() -> String {
    let socks = SNAPSHOT.lock().clone();
    let rules = rules();
    let unexpected = socks.iter().filter(|s| is_unexpected(s)).count();
    let listeners = socks.iter().filter(|s| s.is_listener()).count();
    let sw = |on: bool| if on { "on" } else { "off" };
    let mut out = String::from("hunter netmon\n");
    let _ = writeln!(
        out,
        "alerts={} ignore-loopback={} sockets={} listeners={} unexpected={} reported={}",
        sw(alerts_enabled()),
        sw(ignore_loopback()),
        socks.len(),
        listeners,
        unexpected,
        ALERT_COUNT.load(Ordering::Relaxed),
    );
    for r in &rules {
        out.push_str(&r.to_line());
        out.push('\n');
    }
    for s in &socks {
        let verdict = if is_unexpected(s) {
            "ALERT"
        } else if s.is_listener() {
            "listen"
        } else if s.remote.is_some() {
            "ok"
        } else {
            "-"
        };
        let remote = match &s.remote {
            Some((a, p)) => join_addr(a, *p),
            None => String::from("-"),
        };
        let _ = writeln!(
            out,
            "sock {} {} {} {} {} {} {}",
            s.pid,
            s.proto.as_str(s.v6),
            s.state.as_str(),
            join_addr(&s.local_addr, s.local_port),
            remote,
            verdict,
            s.program,
        );
    }
    out
}

/// `addr:port`, with a v6 address in brackets so the last `:` is the port's.
pub fn join_addr(addr: &str, port: u16) -> String {
    if addr.contains(':') {
        format!("[{}]:{}", addr, port)
    } else {
        format!("{}:{}", addr, port)
    }
}

/// Test-only: back to the boot defaults.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    *RULES.lock() = default_rules();
    SNAPSHOT.lock().clear();
    ALERTED.lock().clear();
    ALERTS.store(true, Ordering::SeqCst);
    IGNORE_LOOPBACK.store(true, Ordering::SeqCst);
    ALERT_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
mod tests;
