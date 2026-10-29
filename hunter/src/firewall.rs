//! Per-application firewall: which programs may reach the network, decided
//! once by the person at the desktop and kept as a rule.
//!
//! The other domains guard what runs and how it maps memory; none of them can
//! tell `curl` fetching a page from a dropper calling home, because both are
//! the same `connect(2)`. This domain asks the one question that separates
//! them -- *which program* is opening the connection -- and keeps the answer:
//!
//! * `connect(2)`, and `sendto(2)` / `sendmsg(2)` with a destination, are the
//!   **outgoing** direction ([`Direction::Out`]);
//! * `bind(2)` to a fixed port on a non-loopback address is the **incoming**
//!   one ([`Direction::In`]): the program wants to be a server.
//!
//! Loopback and unspecified-port client binds are never asked about: the
//! desktop itself (X11, Wayland, D-Bus, a local resolver) lives on them, and a
//! firewall that asks about `127.0.0.1` is one the person learns to click
//! through. Unix sockets never reach here at all.
//!
//! ## Rules and questions
//!
//! A rule is `allow` or `deny`, for a program (its canonical path, as
//! [`policy::canonicalize`] spells it) and a direction or both. A program with
//! no rule is a **question**: it goes to the pending list, which
//! `/proc/hunter_firewall` shows and lunarhunter turns into "¿Permitir que X se
//! conecte a la red?". The answer is an ordinary control directive
//! (`net allow /usr/bin/curl out`) written to `/proc/hunter`, and saved to
//! `/etc/hunter/firewall` so the next boot already knows it.
//!
//! What happens *while* a question is open depends on the mode:
//!
//! * `Report` (the default): the connection proceeds and the question waits.
//!   Nothing that worked before this domain existed stops working, and the
//!   person still gets to decide.
//! * `Enforce`: the system call **waits** for the answer, up to
//!   [`ask_timeout_secs`] (30 s by default), the way an interactive firewall
//!   holds a connection while its dialog is open. An answer of `allow` lets it
//!   through on the spot; `deny`, or no answer in time, fails it with
//!   `EACCES`. The question stays pending after a timeout, so the next attempt
//!   can still be allowed.
//!
//! Unlike the other domains, an explicit `deny` rule blocks under `Report`
//! too. A `Report` firewall that let a program through after the person
//! pressed "Bloquear" would be a button that lies; `Report` is about what
//! hunter does on its *own* judgement, and a rule is not hunter's judgement.
//! `Off` checks nothing, rules included.
//!
//! ## The latch
//!
//! An `allow` rule and `forget` (dropping a rule) relax the policy and are
//! refused once sealed; `deny` tightens and always goes through. Dismissing a
//! question changes no rule and is never gated.
//!
//! ## Names
//!
//! The program is the process's executable path as `execve` recorded it. A
//! script is its interpreter (`/usr/bin/python3`), which is coarser than one
//! would like and exactly as coarse as every other per-application firewall
//! that does not trust the script to say who it is.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use lock::Mutex;

use crate::clock;
use crate::event_log::{record, Severity};
use crate::policy::{self, Mode};

/// Which way a program is reaching the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    /// `connect`, or a datagram sent to an explicit address.
    Out,
    /// `bind` to a fixed port: the program wants to accept connections.
    In,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Out => "out",
            Direction::In => "in",
        }
    }

    fn index(self) -> usize {
        match self {
            Direction::Out => 0,
            Direction::In => 1,
        }
    }
}

/// What a rule says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Deny,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

/// What the kernel must do with one network operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Let it through.
    Allow,
    /// Fail it with `EACCES`.
    Deny,
    /// Enforce mode and no rule: wait for [`decided`] to answer, up to
    /// [`ask_timeout_secs`], then call [`expire`].
    Ask,
}

/// One `net ...` directive of the control language (and one line of
/// `/etc/hunter/firewall`, which is the same thing without the `net`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetDirective {
    /// `allow|deny <path> [in|out]`; `None` is both directions.
    Rule(Action, String, Option<Direction>),
    /// `forget <path>`: drop every rule for the program.
    Forget(String),
    /// `dismiss <path>`: drop its open questions, deciding nothing.
    Dismiss(String),
    /// `timeout <secs>`: how long an `Enforce` question holds the call.
    Timeout(u32),
    /// `mode <off|report|enforce>`: the same as `mode net ...`, so that
    /// `/etc/hunter/firewall` can carry the firewall's whole stance.
    Mode(Mode),
}

/// Default wait for an answer under `Enforce`.
const DEFAULT_TIMEOUT_SECS: u32 = 30;
/// Longest wait an operator may set: past this a client has long given up.
const MAX_TIMEOUT_SECS: u32 = 300;
/// Cap on rules, bounding kernel memory against a runaway rules file.
const MAX_RULES: usize = 4096;
/// Cap on open questions. A process that tries a thousand programs' worth of
/// connections is not going to be answered one by one; the oldest go first.
const MAX_PENDING: usize = 32;
/// Report starts in `Report`: nothing breaks, and the questions still come.
const DEFAULT_NET_MODE: Mode = Mode::Report;

static NET_MODE: AtomicU8 = AtomicU8::new(DEFAULT_NET_MODE.to_u8());
static TIMEOUT_SECS: AtomicU32 = AtomicU32::new(DEFAULT_TIMEOUT_SECS);

/// A program's rules, `[out, in]`.
type Rules = [Option<Action>; 2];

/// Blocked attempts under a rule, so a program retrying in a loop does not
/// fill the event ring: an event is recorded at 1, 2, 4, 8, ... blocks.
#[derive(Default)]
struct RuleState {
    rules: Rules,
    blocked: [u64; 2],
}

/// An open question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub program: String,
    pub dir: Direction,
    /// The last address it tried, as the kernel spelled it (`1.2.3.4:443`).
    pub peer: String,
    /// The last process that asked.
    pub pid: u64,
    /// How many attempts are behind the question.
    pub count: u64,
    /// When it was first and last asked, in seconds since boot.
    pub first: u64,
    pub last: u64,
}

lazy_static::lazy_static! {
    static ref RULES: Mutex<BTreeMap<String, RuleState>> = Mutex::new(BTreeMap::new());
    static ref PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
}

fn now_secs() -> u64 {
    clock::now_ns() / 1_000_000_000
}

/// Sets the firewall's mode.
pub fn set_net_mode(mode: Mode) {
    policy::apply_mode(&NET_MODE, "net", mode);
}

/// The firewall's mode.
pub fn net_mode() -> Mode {
    Mode::from_u8(NET_MODE.load(Ordering::SeqCst))
}

/// How long an `Enforce` question holds the system call, in seconds.
pub fn ask_timeout_secs() -> u32 {
    TIMEOUT_SECS.load(Ordering::SeqCst)
}

/// The rule for `program` in `dir`, if any.
pub fn rule(program: &str, dir: Direction) -> Option<Action> {
    RULES
        .lock()
        .get(&policy::canonicalize(program))
        .and_then(|s| s.rules[dir.index()])
}

/// Number of programs with at least one rule.
pub fn rule_count() -> usize {
    RULES.lock().len()
}

/// The open questions, oldest first.
pub fn pending() -> Vec<Pending> {
    PENDING.lock().clone()
}

/// The firewall hook. `program` is the caller's executable path, `peer` the
/// address as the kernel prints it (for the person, never parsed), and
/// `local_only` whether the operation stays on this machine (loopback, or a
/// client bind to port 0), which is never asked about.
pub fn check(pid: u64, program: &str, dir: Direction, peer: &str, local_only: bool) -> Verdict {
    let mode = net_mode();
    if mode == Mode::Off || local_only || program.is_empty() {
        return Verdict::Allow;
    }
    let canon = policy::canonicalize(program);
    {
        let mut rules = RULES.lock();
        if let Some(state) = rules.get_mut(&canon) {
            match state.rules[dir.index()] {
                Some(Action::Allow) => return Verdict::Allow,
                Some(Action::Deny) => {
                    let n = &mut state.blocked[dir.index()];
                    *n += 1;
                    let times = *n;
                    drop(rules);
                    if times.is_power_of_two() {
                        record(
                            pid,
                            Severity::Warning,
                            "NET",
                            "BLOCKED",
                            format!(
                                "firewall rule: {} {} {} ({} time{})",
                                canon,
                                dir.as_str(),
                                peer,
                                times,
                                if times == 1 { "" } else { "s" }
                            ),
                        );
                    }
                    return Verdict::Deny;
                }
                None => {}
            }
        }
    }
    // A new question is a `Warning`: it is something the person has to
    // answer, and the desktop shows warnings (lunarbar's alerts, the
    // notifications). It is raised once per program and direction, so it
    // cannot flood them.
    if ask(pid, &canon, dir, peer) {
        record(
            pid,
            Severity::Warning,
            "NET",
            "ASK",
            format!(
                "firewall: {} wants {} {} (no rule; {})",
                canon,
                if dir == Direction::Out {
                    "to connect to"
                } else {
                    "to listen on"
                },
                peer,
                if mode == Mode::Enforce {
                    "waiting for an answer"
                } else {
                    "allowed in report mode"
                }
            ),
        );
    }
    if mode == Mode::Enforce {
        Verdict::Ask
    } else {
        Verdict::Allow
    }
}

/// Opens (or refreshes) the question for `program` in `dir`. `true` when it
/// is a new question, which is the one worth an event.
fn ask(pid: u64, canon: &str, dir: Direction, peer: &str) -> bool {
    let now = now_secs();
    let mut pending = PENDING.lock();
    if let Some(p) = pending
        .iter_mut()
        .find(|p| p.dir == dir && p.program == canon)
    {
        p.peer = String::from(peer);
        p.pid = pid;
        p.count += 1;
        p.last = now;
        return false;
    }
    if pending.len() >= MAX_PENDING {
        // The least recently asked: a question nobody has repeated for a while
        // is the one least likely to be waited on.
        if let Some(i) = (0..pending.len()).min_by_key(|&i| pending[i].last) {
            pending.remove(i);
        }
    }
    pending.push(Pending {
        program: String::from(canon),
        dir,
        peer: String::from(peer),
        pid,
        count: 1,
        first: now,
        last: now,
    });
    true
}

/// While an `Enforce` call waits: `Some(true)` once the program is allowed in
/// `dir`, `Some(false)` once it is denied (or the domain was turned off, which
/// lets it through: `Some(true)`), `None` while nobody has answered.
pub fn decided(program: &str, dir: Direction) -> Option<bool> {
    if net_mode() != Mode::Enforce {
        return Some(true);
    }
    rule(program, dir).map(|a| a == Action::Allow)
}

/// An `Enforce` call waited [`ask_timeout_secs`] for an answer and got none:
/// it is refused, and said. The question stays open.
pub fn expire(pid: u64, program: &str, dir: Direction, peer: &str) {
    record(
        pid,
        Severity::Warning,
        "NET",
        "BLOCKED",
        format!(
            "firewall: no answer in {} s for {} {} {}",
            ask_timeout_secs(),
            policy::canonicalize(program),
            dir.as_str(),
            peer
        ),
    );
}

/// Parses the words of a `net` directive (without the `net`), which is also
/// one line of `/etc/hunter/firewall`. A path is the rest of the line, so a
/// program with a space in its name can still have a rule; the direction, if
/// any, is the last word.
pub fn parse_directive(text: &str) -> Result<NetDirective, String> {
    let text = text.trim();
    let (verb, rest) = match text.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (text, ""),
    };
    let path = |p: &str| -> Result<String, String> {
        if p.starts_with('/') {
            Ok(String::from(p))
        } else if p.is_empty() {
            Err(format!("'net {}' takes a program path", verb))
        } else {
            Err(format!("path must be absolute: {}", p))
        }
    };
    match verb {
        "allow" | "deny" => {
            let action = if verb == "allow" {
                Action::Allow
            } else {
                Action::Deny
            };
            let (p, dir) = match rest.rsplit_once(char::is_whitespace) {
                Some((p, "out")) => (p.trim_end(), Some(Direction::Out)),
                Some((p, "in")) => (p.trim_end(), Some(Direction::In)),
                Some((p, "both")) => (p.trim_end(), None),
                _ => (rest, None),
            };
            Ok(NetDirective::Rule(action, path(p)?, dir))
        }
        "forget" => Ok(NetDirective::Forget(path(rest)?)),
        "dismiss" => Ok(NetDirective::Dismiss(path(rest)?)),
        "timeout" => {
            let secs: u32 = rest
                .parse()
                .map_err(|_| format!("'net timeout' takes seconds, got '{}'", rest))?;
            if secs == 0 || secs > MAX_TIMEOUT_SECS {
                return Err(format!(
                    "'net timeout' must be 1..={} seconds, got {}",
                    MAX_TIMEOUT_SECS, secs
                ));
            }
            Ok(NetDirective::Timeout(secs))
        }
        "mode" => match rest {
            "off" => Ok(NetDirective::Mode(Mode::Off)),
            "report" => Ok(NetDirective::Mode(Mode::Report)),
            "enforce" => Ok(NetDirective::Mode(Mode::Enforce)),
            other => Err(format!("unknown mode '{}' (off, report, enforce)", other)),
        },
        "" => Err(String::from(
            "'net' takes allow|deny <path> [in|out], forget <path>, dismiss <path>, timeout <secs> or mode <mode>",
        )),
        other => Err(format!(
            "unknown net verb '{}' (allow, deny, forget, dismiss, timeout, mode)",
            other
        )),
    }
}

/// Parses one line of `/etc/hunter/firewall`: `Ok(None)` for a blank line or a
/// comment.
pub fn parse_rules_line(line: &str) -> Result<Option<NetDirective>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    parse_directive(line).map(Some)
}

/// Applies one `net` directive.
pub fn apply(d: &NetDirective) {
    match d {
        NetDirective::Rule(action, program, dir) => set_rule(*action, program, *dir),
        NetDirective::Forget(program) => forget(program),
        NetDirective::Dismiss(program) => {
            let canon = policy::canonicalize(program);
            PENDING.lock().retain(|p| p.program != canon);
        }
        NetDirective::Mode(m) => set_net_mode(*m),
        NetDirective::Timeout(secs) => {
            if TIMEOUT_SECS.swap(*secs, Ordering::SeqCst) != *secs {
                record(
                    0,
                    Severity::Notice,
                    "CONTROL",
                    "CONFIG",
                    format!("net timeout {}", secs),
                );
            }
        }
    }
}

/// Sets a rule, answering the matching questions. `allow` relaxes and is
/// gated by the latch; `deny` never is.
fn set_rule(action: Action, program: &str, dir: Option<Direction>) {
    if action == Action::Allow && !policy::allow_relaxing("net allow") {
        return;
    }
    let canon = policy::canonicalize(program);
    let dirs: &[Direction] = match dir {
        Some(Direction::Out) => &[Direction::Out],
        Some(Direction::In) => &[Direction::In],
        None => &[Direction::Out, Direction::In],
    };
    {
        let mut rules = RULES.lock();
        if rules.len() >= MAX_RULES && !rules.contains_key(&canon) {
            drop(rules);
            record(
                0,
                Severity::Warning,
                "CONTROL",
                "WARNING",
                format!(
                    "firewall rule list full ({}); not added: {}",
                    MAX_RULES, canon
                ),
            );
            return;
        }
        let state = rules.entry(canon.clone()).or_default();
        let mut changed = false;
        for d in dirs {
            if state.rules[d.index()] != Some(action) {
                state.rules[d.index()] = Some(action);
                state.blocked[d.index()] = 0;
                changed = true;
            }
        }
        if !changed {
            return;
        }
    }
    PENDING
        .lock()
        .retain(|p| !(p.program == canon && dirs.contains(&p.dir)));
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!(
            "net {} {}{}",
            action.as_str(),
            canon,
            dir.map(|d| format!(" {}", d.as_str())).unwrap_or_default()
        ),
    );
}

/// Drops every rule for `program`: it is asked about again. A relaxation when
/// one of them was a `deny`, so gated by the latch.
fn forget(program: &str) {
    let canon = policy::canonicalize(program);
    let has_deny = RULES
        .lock()
        .get(&canon)
        .is_some_and(|s| s.rules.contains(&Some(Action::Deny)));
    if has_deny && !policy::allow_relaxing("net forget") {
        return;
    }
    if RULES.lock().remove(&canon).is_some() {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("net forget {}", canon),
        );
    }
}

/// Renders `/proc/hunter_firewall`: one record per line, the path always last
/// so a path with spaces in it needs no quoting.
///
/// ```text
/// mode report
/// timeout 30
/// rule allow out /usr/bin/curl
/// pending out 3 42 120 125 93.184.216.34:443 /usr/bin/wget
/// ```
///
/// A pending line is `pending <dir> <count> <pid> <first> <last> <peer> <path>`,
/// times in seconds since boot, and `now <secs>` says what "now" was.
pub fn render() -> String {
    let mut out = String::new();
    out.push_str(&format!("mode {}\n", net_mode().as_str()));
    out.push_str(&format!("timeout {}\n", ask_timeout_secs()));
    out.push_str(&format!("now {}\n", now_secs()));
    for (program, state) in RULES.lock().iter() {
        match state.rules {
            [Some(a), Some(b)] if a == b => {
                out.push_str(&format!("rule {} both {}\n", a.as_str(), program))
            }
            rules => {
                for (i, r) in rules.iter().enumerate() {
                    if let Some(a) = r {
                        let d = if i == 0 { "out" } else { "in" };
                        out.push_str(&format!("rule {} {} {}\n", a.as_str(), d, program));
                    }
                }
            }
        }
    }
    for p in PENDING.lock().iter() {
        out.push_str(&format!(
            "pending {} {} {} {} {} {} {}\n",
            p.dir.as_str(),
            p.count,
            p.pid,
            p.first,
            p.last,
            p.peer,
            p.program
        ));
    }
    out
}

/// Test-only: back to boot defaults.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    NET_MODE.store(DEFAULT_NET_MODE.to_u8(), Ordering::SeqCst);
    TIMEOUT_SECS.store(DEFAULT_TIMEOUT_SECS, Ordering::SeqCst);
    RULES.lock().clear();
    PENDING.lock().clear();
}

#[cfg(test)]
#[path = "firewall_tests.rs"]
mod tests;
