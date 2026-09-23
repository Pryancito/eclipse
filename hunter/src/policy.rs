//! Policy engine for the hunter security subsystem.
//!
//! Holds the independently-tunable enforcement domains plus the data that
//! drives them:
//!
//! * **syscall** — per-process syscall whitelists (a lightweight seccomp).
//! * **wx**      — write-xor-execute memory policy (mmap / mprotect).
//! * **exec**    — which filesystem paths a binary may be executed from.
//! * **anomaly** — whether detected floods / fork bombs are blocked or only logged.
//!
//! Each domain has its own [`Mode`] (`Off` / `Report` / `Enforce`) so the
//! subsystem can be rolled out audit-first and tightened per-domain.
//!
//! Hardening (P13): every mutator now records an audit event of the
//! `old -> new` transition, mode loads/stores use `SeqCst` (these gate
//! enforcement; a racy read must not silently drop a check), and the control
//! plane can be put into a one-way **tighten-only** mode so a single relaxing
//! store cannot quietly neutralise hunter after boot.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use lock::Mutex;

use crate::event_log::{record, Severity};

/// Mirror of `GLOBAL_POLICIES.len()`, maintained by every mutator while it
/// still holds the map lock. Lets the per-syscall check skip the globally
/// contended lock entirely in the default system state (no whitelists
/// registered), which is semantically identical to a map miss.
static POLICY_COUNT: AtomicUsize = AtomicUsize::new(0);

lazy_static::lazy_static! {
    /// Registry of process-specific syscall whitelists. Whitelists are stored
    /// sorted so the per-syscall check is a binary search. Mutators must
    /// refresh `POLICY_COUNT` before releasing the lock.
    pub static ref GLOBAL_POLICIES: Mutex<BTreeMap<u64, Vec<u32>>> = Mutex::new(BTreeMap::new());

    /// Path prefixes a binary must never be executed from (untrusted, writable
    /// world locations). Configurable at runtime.
    pub static ref UNTRUSTED_EXEC_PREFIXES: Mutex<Vec<String>> =
        Mutex::new(default_untrusted_prefixes());

    /// Allowlist of trusted programs: exact canonical executable paths that are
    /// explicitly permitted to run. Empty by default (allowlist inactive).
    pub static ref TRUSTED_EXEC_PATHS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Allowlist of trusted directories: any executable whose canonical path
    /// starts with one of these prefixes is permitted. Empty by default.
    pub static ref TRUSTED_EXEC_PREFIXES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Programs auto-learned at runtime (trust-on-first-use). Kept separate from
    /// the operator-configured allowlist so a userspace helper can read them
    /// from `/proc/hunter` and persist them to `/etc/hunter/whitelist`.
    pub static ref LEARNED_EXEC_PATHS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Blacklist of denied programs: exact canonical paths that must never run.
    pub static ref BLACKLISTED_EXEC_PATHS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Blacklist of denied directories: any executable under one of these
    /// prefixes is denied.
    pub static ref BLACKLISTED_EXEC_PREFIXES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Optional default whitelist applied to new images at exec time. `None`
    /// keeps the seccomp domain opt-in (default-permissive), preserving boot.
    pub static ref DEFAULT_WHITELIST: Mutex<Option<Vec<u32>>> = Mutex::new(None);
}

/// What hunter does when a policy in a given domain is violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Ignore the domain entirely (no checks, no logging).
    Off,
    /// Log the violation but allow the action (audit / IDS mode).
    Report,
    /// Log the violation and block the action (active enforcement).
    Enforce,
}

impl Mode {
    fn to_u8(self) -> u8 {
        match self {
            Mode::Off => 0,
            Mode::Report => 1,
            Mode::Enforce => 2,
        }
    }
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Mode::Off,
            1 => Mode::Report,
            _ => Mode::Enforce,
        }
    }
    /// Human-readable tag for the `/proc/hunter` header.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

// Default stance: enforce explicit syscall whitelists (opt-in per process, so
// safe), but only *report* W^X / exec-path / anomaly violations so real
// dynamic linkers, JITs and the base system are never broken by default.
static SYSCALL_MODE: AtomicU8 = AtomicU8::new(2); // Enforce
static WX_MODE: AtomicU8 = AtomicU8::new(1); // Report
static EXEC_MODE: AtomicU8 = AtomicU8::new(1); // Report
static ANOMALY_MODE: AtomicU8 = AtomicU8::new(1); // Report

/// One-way latch: once set, modes may only move towards stricter enforcement.
static TIGHTEN_ONLY: AtomicBool = AtomicBool::new(false);

/// Whether exec learning (trust-on-first-use) is enabled: safe programs are
/// auto-added to the allowlist and never denied. Off by default (the crate
/// changes nothing until the kernel opts in at boot).
static EXEC_LEARN: AtomicBool = AtomicBool::new(false);

/// Cap on auto-learned entries, bounding kernel memory if exec churns through
/// many distinct binaries.
const MAX_LEARNED_EXEC: usize = 8192;

/// The built-in world-writable locations. Kept in canonical prefix form (see
/// [`canonicalize_prefix`]) -- `default_untrusted_directories_are_canonical`
/// in the tests is what holds a future entry to it.
fn default_untrusted_prefixes() -> Vec<String> {
    alloc::vec![
        String::from("/tmp/"),
        String::from("/var/tmp/"),
        String::from("/dev/shm/"),
    ]
}

/// Refuses a relaxing control-plane change while the tighten-only latch is
/// engaged, recording the refusal. Returns `true` when the caller may proceed.
///
/// The latch's promise is that "a single relaxing store cannot quietly
/// neutralise hunter after boot", but it was only ever consulted by
/// [`apply_mode`] — i.e. by the four mode words. Every other way of relaxing
/// the policy walked straight past it, and they are the effective ones:
/// `add_trusted_exec_prefix("/")` trusts the whole filesystem,
/// `clear_trusted_exec()` deactivates the allowlist outright,
/// `set_exec_learning(true)` turns exec into a domain that never denies, and
/// `set_default_whitelist(None)` drops the seccomp default — all with the
/// modes still reading `enforce` in `/proc/hunter`, which is the part that
/// makes it quiet.
///
/// Tightening changes (adding an untrusted or blacklisted location, disabling
/// learning, raising a mode) are never gated, and neither is
/// [`remove_policy`], which is process teardown rather than a policy change.
fn allow_relaxing(what: &str) -> bool {
    if !TIGHTEN_ONLY.load(Ordering::SeqCst) {
        return true;
    }
    record(
        0,
        Severity::Warning,
        "CONTROL",
        "WARNING",
        format!(
            "refused relaxing control change: {} (tighten-only latch)",
            what
        ),
    );
    false
}

/// Applies a mode transition for one domain, honouring the tighten-only latch
/// and recording an audit event. Returns the mode actually in effect after.
fn apply_mode(slot: &AtomicU8, domain: &'static str, requested: Mode) -> Mode {
    let current = Mode::from_u8(slot.load(Ordering::SeqCst));
    if current == requested {
        return current;
    }
    // Under the tighten-only latch, refuse any relaxation.
    if TIGHTEN_ONLY.load(Ordering::SeqCst) && requested.to_u8() < current.to_u8() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "refused relaxing {} mode {} -> {} (tighten-only latch)",
                domain,
                current.as_str(),
                requested.as_str()
            ),
        );
        return current;
    }
    slot.store(requested.to_u8(), Ordering::SeqCst);
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!(
            "{} mode {} -> {}",
            domain,
            current.as_str(),
            requested.as_str()
        ),
    );
    requested
}

/// Sets the enforcement mode for the syscall-filtering domain.
pub fn set_syscall_mode(mode: Mode) {
    apply_mode(&SYSCALL_MODE, "syscall", mode);
}
/// Returns the current syscall-filtering mode.
pub fn syscall_mode() -> Mode {
    Mode::from_u8(SYSCALL_MODE.load(Ordering::SeqCst))
}

/// Sets the enforcement mode for the W^X memory domain.
pub fn set_wx_mode(mode: Mode) {
    apply_mode(&WX_MODE, "wx", mode);
}
/// Returns the current W^X mode.
pub fn wx_mode() -> Mode {
    Mode::from_u8(WX_MODE.load(Ordering::SeqCst))
}

/// Sets the enforcement mode for the executable-path domain.
pub fn set_exec_mode(mode: Mode) {
    apply_mode(&EXEC_MODE, "exec", mode);
}
/// Returns the current executable-path mode.
pub fn exec_mode() -> Mode {
    Mode::from_u8(EXEC_MODE.load(Ordering::SeqCst))
}

/// Sets the enforcement mode for the anomaly (flood / fork-bomb) domain.
pub fn set_anomaly_mode(mode: Mode) {
    apply_mode(&ANOMALY_MODE, "anomaly", mode);
}
/// Returns the current anomaly mode.
pub fn anomaly_mode() -> Mode {
    Mode::from_u8(ANOMALY_MODE.load(Ordering::SeqCst))
}

/// Engages the one-way tighten-only latch: after this, no domain can be
/// relaxed (only moved towards `Enforce`). Typically called once boot is done.
pub fn seal_tighten_only() {
    if !TIGHTEN_ONLY.swap(true, Ordering::SeqCst) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            String::from("tighten-only latch engaged"),
        );
    }
}
/// Whether the tighten-only latch is engaged.
pub fn is_tighten_only() -> bool {
    TIGHTEN_ONLY.load(Ordering::SeqCst)
}

/// Test-only: returns the whole control plane to its boot defaults, latch
/// included. The latch is one-way by design, so without this a test binary
/// could only ever hold one latch scenario; never compiled into the kernel.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    TIGHTEN_ONLY.store(false, Ordering::SeqCst);
    SYSCALL_MODE.store(Mode::Enforce.to_u8(), Ordering::SeqCst);
    WX_MODE.store(Mode::Report.to_u8(), Ordering::SeqCst);
    EXEC_MODE.store(Mode::Report.to_u8(), Ordering::SeqCst);
    ANOMALY_MODE.store(Mode::Report.to_u8(), Ordering::SeqCst);
    EXEC_LEARN.store(false, Ordering::SeqCst);
    TRUSTED_EXEC_PATHS.lock().clear();
    TRUSTED_EXEC_PREFIXES.lock().clear();
    LEARNED_EXEC_PATHS.lock().clear();
    BLACKLISTED_EXEC_PATHS.lock().clear();
    BLACKLISTED_EXEC_PREFIXES.lock().clear();
    *UNTRUSTED_EXEC_PREFIXES.lock() = default_untrusted_prefixes();
    *DEFAULT_WHITELIST.lock() = None;
    let mut map = GLOBAL_POLICIES.lock();
    map.clear();
    POLICY_COUNT.store(map.len(), Ordering::Release);
}

// ---- Back-compat shims for the original boolean enforcement switch --------

/// Sets whether syscall violations block (`true`) or warn (`false`).
pub fn set_enforcement_mode(enabled: bool) {
    set_syscall_mode(if enabled { Mode::Enforce } else { Mode::Report });
}
/// Returns `true` when syscall violations are blocked.
pub fn get_enforcement_mode() -> bool {
    syscall_mode() == Mode::Enforce
}

// ---- Syscall whitelists ---------------------------------------------------

/// Registers a whitelist of allowed syscall numbers for a process.
pub fn register_policy(pid: u64, mut allowed_syscalls: Vec<u32>) {
    allowed_syscalls.sort_unstable();
    let mut map = GLOBAL_POLICIES.lock();
    map.insert(pid, allowed_syscalls);
    POLICY_COUNT.store(map.len(), Ordering::Release);
}

/// Removes the security policy for a process (e.g. when it exits).
pub fn remove_policy(pid: u64) {
    let mut map = GLOBAL_POLICIES.lock();
    map.remove(&pid);
    POLICY_COUNT.store(map.len(), Ordering::Release);
}

/// Number of processes that currently have a syscall whitelist registered.
pub fn active_policy_count() -> usize {
    GLOBAL_POLICIES.lock().len()
}

/// Sets (or clears) the default whitelist applied to freshly-exec'd images.
/// `None` keeps the syscall domain opt-in / default-permissive.
pub fn set_default_whitelist(list: Option<Vec<u32>>) {
    // Clearing the default is unambiguously a relaxation. Replacing one list
    // with another may go either way and is left to the operator.
    if list.is_none() && !allow_relaxing("clear default syscall whitelist") {
        return;
    }
    *DEFAULT_WHITELIST.lock() = list;
}

/// Applies the default whitelist to `pid` if one is configured (P4): makes the
/// seccomp domain reachable without changing behaviour when unset.
pub fn apply_default_policy(pid: u64) {
    if let Some(list) = DEFAULT_WHITELIST.lock().as_ref() {
        let mut sorted = list.clone();
        sorted.sort_unstable();
        let mut map = GLOBAL_POLICIES.lock();
        map.insert(pid, sorted);
        POLICY_COUNT.store(map.len(), Ordering::Release);
    }
}

/// Inherits the parent's whitelist into a forked child so a process cannot
/// shed its policy merely by forking (P4).
pub fn inherit_policy(parent_pid: u64, child_pid: u64) {
    let mut map = GLOBAL_POLICIES.lock();
    if let Some(list) = map.get(&parent_pid).cloned() {
        map.insert(child_pid, list);
        POLICY_COUNT.store(map.len(), Ordering::Release);
    }
}

/// Checks whether a syscall is allowed for a given process.
///
/// Returns `Ok(())` when allowed, or `Err(enforce)` on a violation where
/// `enforce` is `true` if the action should be blocked.
pub fn is_syscall_allowed(pid: u64, syscall_num: u32) -> Result<(), bool> {
    let mode = syscall_mode();
    if mode == Mode::Off {
        return Ok(());
    }
    // Lock-free fast path for the default system state: no whitelist is
    // registered for ANY process, so every pid resolves to the permissive
    // map-miss arm below. Skips a globally-contended, IRQ-off lock that would
    // otherwise serialize every CPU on every syscall. A racing first
    // registration is indistinguishable from this call having run before it.
    if POLICY_COUNT.load(Ordering::Acquire) == 0 {
        return Ok(());
    }
    let policies = GLOBAL_POLICIES.lock();
    match policies.get(&pid) {
        // Whitelists are kept sorted by the registration paths.
        Some(allowed) if allowed.binary_search(&syscall_num).is_ok() => Ok(()),
        Some(_) => Err(mode == Mode::Enforce),
        // No policy registered for this pid: default permissive.
        None => Ok(()),
    }
}

// ---- Executable-path policy ----------------------------------------------

/// Adds a path prefix to the untrusted-execution list (idempotent).
pub fn add_untrusted_exec_prefix(prefix: String) {
    let canon = canonicalize_prefix(&prefix);
    let mut list = UNTRUSTED_EXEC_PREFIXES.lock();
    if !list.iter().any(|p| *p == canon) {
        list.push(canon);
    }
}

/// Lexically canonicalizes an absolute path: collapses `//`, drops `.`, and
/// resolves `..` against earlier components (without touching the filesystem).
/// `/bin/../tmp/x` becomes `/tmp/x`, so a traversal cannot smuggle an execution
/// past the untrusted-prefix check (P6, finding ELF-3).
pub fn canonicalize(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            c => stack.push(c),
        }
    }
    let mut out = String::from("/");
    out.push_str(&stack.join("/"));
    out
}

/// Normalizes a *directory prefix* the way every query path is normalized.
///
/// Every membership test in this module canonicalizes the path it is given and
/// then compares it against the stored list, but only the exact-path mutators
/// canonicalized what they stored: the prefix mutators pushed the operator's
/// raw string. A prefix that was not already canonical therefore could not
/// match anything, ever — and on the blacklist, which is the one hard deny in
/// the whole exec policy, a rule that never matches fails *open*. Nothing
/// reported it, because a deny rule that never fires looks exactly like a deny
/// rule nobody tripped.
///
/// The trailing slash is the second half: `starts_with` does not know about
/// path components, so a prefix of `/usr/bin` let `/usr/binaries/evil` inherit
/// `/usr/bin`'s trust. Canonicalization drops a trailing slash, so it is put
/// back here and a prefix always means "this directory and what is under it".
fn canonicalize_prefix(prefix: &str) -> String {
    let mut canon = canonicalize(prefix);
    if !canon.ends_with('/') {
        canon.push('/');
    }
    canon
}

/// Whether `canon` is a `/proc/<pid>/fd/<n>` magic link.
///
/// Takes an already-canonicalized path on purpose. This test used to be two
/// copies of `path.starts_with("/proc/") && path.contains("/fd/")` run on the
/// *raw* path, one in each of the functions below, while everything around it
/// worked on the canonical form. So `//proc/self/fd/3` and
/// `/bin/../proc/self/fd/3` — which the loader opens exactly like
/// `/proc/self/fd/3` — were not magic links as far as hunter was concerned:
/// they are not world-writable either, so with learning on (which is how the
/// kernel boots it, see `/etc/hunter`) the *first* such exec put
/// `/proc/self/fd/3` into the permanent trusted allowlist, and from then on any
/// process could execute whatever inode it had open on fd 3.
fn is_proc_fd_link(canon: &str) -> bool {
    canon.starts_with("/proc/") && canon.contains("/fd/")
}

/// Returns `true` when executing from `path` should be treated as untrusted:
/// a world-writable location (after canonicalization) or a `/proc/*/fd/*`
/// magic-link that smuggles past prefix matching (P6, finding ELF-2/ELF-3).
pub fn is_untrusted_exec_path(path: &str) -> bool {
    // A relative exec path is resolved against cwd at the FS layer; without that
    // context we cannot prove it lands somewhere trusted, so flag it.
    if !path.starts_with('/') {
        return true;
    }
    let canon = canonicalize(path);
    // /proc/self/fd/N and /proc/<pid>/fd/N resolve to an arbitrary opened
    // inode, defeating textual prefix checks — always treat as untrusted.
    if is_proc_fd_link(&canon) {
        return true;
    }
    let list = UNTRUSTED_EXEC_PREFIXES.lock();
    list.iter().any(|p| canon.starts_with(p.as_str()))
}

// ---- Trusted-program allowlist (application allow-listing) ----------------
//
// An *allowlist* inverts the deny-by-location model into deny-by-default: when
// active, only programs whose canonical path is explicitly trusted (an exact
// match, or under a trusted directory) may execute; everything else is a
// violation handled by `exec_mode` (logged in Report, blocked in Enforce).
//
// The allowlist is **inactive** while both trusted sets are empty, so by
// default nothing changes — an operator opts in by registering trusted
// programs (and typically raising `exec_mode` to `Enforce`).

/// Adds an exact canonical executable path to the trusted-program allowlist.
pub fn add_trusted_exec_path(path: String) {
    if !allow_relaxing("add trusted program") {
        return;
    }
    let canon = canonicalize(&path);
    let mut list = TRUSTED_EXEC_PATHS.lock();
    if !list.iter().any(|p| *p == canon) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("trusted program added: {}", canon),
        );
        list.push(canon);
    }
}

/// Adds a trusted directory prefix: any executable under it is allowed.
pub fn add_trusted_exec_prefix(prefix: String) {
    if !allow_relaxing("add trusted exec directory") {
        return;
    }
    let canon = canonicalize_prefix(&prefix);
    let mut list = TRUSTED_EXEC_PREFIXES.lock();
    if !list.iter().any(|p| *p == canon) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("trusted exec directory added: {}", canon),
        );
        list.push(canon);
    }
}

/// Seeds the allowlist with the standard read-only system program directories,
/// so the base system keeps working when an operator flips `exec_mode` to
/// `Enforce`. Not called by default — opt-in.
pub fn install_default_trusted_exec() {
    for d in [
        "/bin/",
        "/sbin/",
        "/usr/bin/",
        "/usr/sbin/",
        "/usr/local/bin/",
        "/usr/local/sbin/",
        "/lib/",
        "/lib64/",
        "/usr/lib/",
        "/usr/lib64/",
    ] {
        add_trusted_exec_prefix(String::from(d));
    }
}

/// Clears the trusted-program allowlist, deactivating it.
pub fn clear_trusted_exec() {
    if !allow_relaxing("clear trusted-program allowlist") {
        return;
    }
    TRUSTED_EXEC_PATHS.lock().clear();
    TRUSTED_EXEC_PREFIXES.lock().clear();
}

/// Number of trusted entries (exact paths + directory prefixes).
pub fn trusted_exec_count() -> usize {
    TRUSTED_EXEC_PATHS.lock().len() + TRUSTED_EXEC_PREFIXES.lock().len()
}

/// Whether the allowlist is active (any configured or learned entry). While
/// inactive, [`is_exec_allowed`] permits everything, preserving default boot.
pub fn exec_allowlist_active() -> bool {
    !TRUSTED_EXEC_PATHS.lock().is_empty()
        || !TRUSTED_EXEC_PREFIXES.lock().is_empty()
        || !LEARNED_EXEC_PATHS.lock().is_empty()
}

/// Explicit membership test (no "inactive ⇒ allow" shortcut): `true` only if
/// `path` actually matches a configured or learned trusted entry. The path is
/// canonicalized first so a traversal cannot masquerade as a trusted program.
pub fn is_exec_listed(path: &str) -> bool {
    let canon = canonicalize(path);
    TRUSTED_EXEC_PATHS.lock().iter().any(|p| *p == canon)
        || TRUSTED_EXEC_PREFIXES
            .lock()
            .iter()
            .any(|p| canon.starts_with(p.as_str()))
        || LEARNED_EXEC_PATHS.lock().iter().any(|p| *p == canon)
}

/// Returns `true` if `path` is trusted, or the allowlist is inactive.
pub fn is_exec_allowed(path: &str) -> bool {
    if !exec_allowlist_active() {
        return true;
    }
    is_exec_listed(path)
}

// ---- Exec learning (trust-on-first-use) -----------------------------------

/// Enables or disables exec learning. When enabled, a *safe* program (valid
/// format, not blacklisted, not from a world-writable location) seen at exec is
/// auto-added to the allowlist and never denied — a learning allowlist that
/// builds itself without breaking anything.
pub fn set_exec_learning(enabled: bool) {
    // Turning learning ON is a relaxation (it makes exec a domain that never
    // denies); turning it OFF is a tightening and always allowed.
    if enabled && !allow_relaxing("enable exec learning") {
        return;
    }
    if EXEC_LEARN.swap(enabled, Ordering::SeqCst) != enabled {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!(
                "exec learning {}",
                if enabled { "enabled" } else { "disabled" }
            ),
        );
    }
}
/// Whether exec learning is enabled.
pub fn exec_learning_enabled() -> bool {
    EXEC_LEARN.load(Ordering::SeqCst)
}

/// Number of auto-learned programs.
pub fn learned_exec_count() -> usize {
    LEARNED_EXEC_PATHS.lock().len()
}

/// Snapshot of the learned programs, for `/proc/hunter` so a userspace helper
/// can persist them to `/etc/hunter/whitelist`.
pub fn learned_exec_paths() -> Vec<String> {
    LEARNED_EXEC_PATHS.lock().clone()
}

/// Adds `path` to the learned set if not already trusted and the cap allows.
/// Returns `true` if a new entry was learned (so the caller can log it once).
pub fn learn_exec_path(path: &str) -> bool {
    // Runs on the exec path for every program, so it declines silently rather
    // than recording a refusal per exec. The caller still allows the exec; what
    // the latch stops is the allowlist growing new permanent entries after the
    // control plane was sealed.
    if TIGHTEN_ONLY.load(Ordering::SeqCst) {
        return false;
    }
    if is_exec_listed(path) {
        return false;
    }
    let canon = canonicalize(path);
    let mut learned = LEARNED_EXEC_PATHS.lock();
    if learned.len() >= MAX_LEARNED_EXEC || learned.iter().any(|p| *p == canon) {
        return false;
    }
    learned.push(canon);
    true
}

// ---- Exec blacklist (operator-curated hard deny) --------------------------

/// Adds an exact canonical program path to the exec blacklist.
pub fn add_blacklisted_exec_path(path: String) {
    let canon = canonicalize(&path);
    let mut list = BLACKLISTED_EXEC_PATHS.lock();
    if !list.iter().any(|p| *p == canon) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("blacklisted program: {}", canon),
        );
        list.push(canon);
    }
}

/// Adds a denied directory prefix to the exec blacklist.
pub fn add_blacklisted_exec_prefix(prefix: String) {
    let canon = canonicalize_prefix(&prefix);
    let mut list = BLACKLISTED_EXEC_PREFIXES.lock();
    if !list.iter().any(|p| *p == canon) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("blacklisted directory: {}", canon),
        );
        list.push(canon);
    }
}

/// Number of blacklist entries (exact paths + directory prefixes).
pub fn blacklisted_exec_count() -> usize {
    BLACKLISTED_EXEC_PATHS.lock().len() + BLACKLISTED_EXEC_PREFIXES.lock().len()
}

/// Returns `true` if `path` is on the exec blacklist (canonicalized first).
pub fn is_exec_blacklisted(path: &str) -> bool {
    let canon = canonicalize(path);
    BLACKLISTED_EXEC_PATHS.lock().iter().any(|p| *p == canon)
        || BLACKLISTED_EXEC_PREFIXES
            .lock()
            .iter()
            .any(|p| canon.starts_with(p.as_str()))
}

/// Returns `true` when `path` is in a world-writable location (`/tmp`,
/// `/var/tmp`, `/dev/shm`) or a `/proc/*/fd/*` magic-link — i.e. unsafe to
/// auto-trust. Unlike [`is_untrusted_exec_path`] this does *not* flag merely
/// relative paths, so a package manager exec'ing `lib/apk/.../busybox` with a
/// relative path is still learnable.
pub fn is_world_writable_exec_path(path: &str) -> bool {
    let canon = canonicalize(path);
    if is_proc_fd_link(&canon) {
        return true;
    }
    UNTRUSTED_EXEC_PREFIXES
        .lock()
        .iter()
        .any(|p| canon.starts_with(p.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_globals;

    fn start() -> impl Drop {
        let g = test_globals::lock();
        reset_for_test();
        g
    }

    // ---- canonicalization of stored prefixes (the query side always did) --

    #[test]
    fn a_prefix_is_stored_canonical_and_always_means_a_directory() {
        assert_eq!(canonicalize_prefix("/usr/bin"), String::from("/usr/bin/"));
        assert_eq!(canonicalize_prefix("/usr/bin/"), String::from("/usr/bin/"));
        assert_eq!(
            canonicalize_prefix("//opt//danger//"),
            String::from("/opt/danger/")
        );
        assert_eq!(
            canonicalize_prefix("/opt/../opt/danger/"),
            String::from("/opt/danger/")
        );
        assert_eq!(canonicalize_prefix("/"), String::from("/"));
    }

    #[test]
    fn a_blacklisted_directory_written_with_a_dotdot_still_denies() {
        let _g = start();
        // The blacklist is the only hard deny in the exec policy -- it blocks
        // even in Report mode -- and it was the one list whose prefixes were
        // stored exactly as the operator typed them while every lookup
        // canonicalized. A rule that cannot match fails open, and a deny rule
        // that never fires is indistinguishable from one nobody tripped.
        add_blacklisted_exec_prefix(String::from("/opt/../opt/danger/"));
        assert!(is_exec_blacklisted("/opt/danger/payload"));
        assert!(is_exec_blacklisted("/opt/danger/sub/payload"));
        assert!(!is_exec_blacklisted("/opt/safe/payload"));
    }

    #[test]
    fn a_blacklisted_directory_written_with_a_double_slash_still_denies() {
        let _g = start();
        add_blacklisted_exec_prefix(String::from("//opt//danger//"));
        assert!(is_exec_blacklisted("/opt/danger/payload"));
        assert_eq!(blacklisted_exec_count(), 1);
        // The same rule typed the canonical way is the same rule, not a second.
        add_blacklisted_exec_prefix(String::from("/opt/danger/"));
        assert_eq!(blacklisted_exec_count(), 1);
    }

    #[test]
    fn an_untrusted_directory_written_loosely_still_flags() {
        let _g = start();
        add_untrusted_exec_prefix(String::from("/srv/../srv/upload/"));
        assert!(is_untrusted_exec_path("/srv/upload/x"));
        assert!(is_world_writable_exec_path("/srv/upload/x"));
        assert!(!is_untrusted_exec_path("/srv/safe/x"));
    }

    #[test]
    fn default_untrusted_directories_are_canonical() {
        for p in default_untrusted_prefixes() {
            assert_eq!(canonicalize_prefix(&p), p, "default prefix not canonical");
        }
        let _g = start();
        assert!(is_untrusted_exec_path("/tmp/payload"));
        assert!(is_untrusted_exec_path("//tmp/payload"));
        assert!(is_untrusted_exec_path("/usr/../tmp/payload"));
        assert!(is_untrusted_exec_path("/var/tmp/payload"));
        assert!(is_untrusted_exec_path("/dev/shm/payload"));
    }

    #[test]
    fn a_trusted_directory_does_not_lend_its_trust_across_a_name_boundary() {
        let _g = start();
        // `starts_with` knows nothing about path components: a trusted prefix
        // of "/usr/bin" used to cover "/usr/binaries-of-mine/evil" too.
        add_trusted_exec_prefix(String::from("/usr/bin"));
        assert!(is_exec_listed("/usr/bin/ls"));
        assert!(!is_exec_listed("/usr/binaries-of-mine/evil"));
        assert!(!is_exec_listed("/usr/bin-backup/evil"));
    }

    // ---- /proc/<pid>/fd/<n> magic links -----------------------------------

    #[test]
    fn a_proc_fd_link_is_untrusted_however_it_is_spelled() {
        let _g = start();
        // All four of these are the same file to the loader. Only the first
        // was a magic link as far as hunter was concerned, because the test
        // ran on the raw path while everything around it ran on the canonical
        // one.
        for p in [
            "/proc/self/fd/3",
            "//proc/self/fd/3",
            "/bin/../proc/self/fd/3",
            "/./proc/1234/fd/7",
        ] {
            assert!(is_untrusted_exec_path(p), "should be untrusted: {}", p);
            assert!(
                is_world_writable_exec_path(p),
                "should not be auto-trustable: {}",
                p
            );
        }
        // And an ordinary /proc path is not one.
        assert!(!is_world_writable_exec_path("/proc/self/exe"));
    }

    #[test]
    fn a_disguised_proc_fd_link_is_never_learned() {
        let _g = start();
        // This is what the miss cost. The kernel boots with learning on (see
        // linux-object's /etc/hunter loader), a magic link is not in any
        // world-writable directory, so the first exec of one put
        // `/proc/self/fd/3` into the permanent trusted allowlist -- after
        // which any process could execute whatever inode it had open there.
        set_exec_learning(true);
        assert!(learn_exec_path("/bin/sh"), "a real program is learnable");
        // The learning gate in `check_exec_path` is this predicate, so it is
        // the one that has to see through the spelling.
        for p in [
            "/proc/self/fd/3",
            "//proc/self/fd/3",
            "/bin/../proc/self/fd/3",
        ] {
            assert!(
                is_world_writable_exec_path(p),
                "must never be auto-trusted: {}",
                p
            );
        }
        assert!(!is_exec_listed("/proc/self/fd/3"));
        assert_eq!(learned_exec_count(), 1);
    }

    // ---- the tighten-only latch beyond the four mode words ----------------

    #[test]
    fn the_latch_still_refuses_to_relax_a_mode() {
        let _g = start();
        set_exec_mode(Mode::Enforce);
        seal_tighten_only();
        set_exec_mode(Mode::Off);
        assert_eq!(exec_mode(), Mode::Enforce);
    }

    #[test]
    fn the_latch_refuses_to_trust_a_new_directory_or_program() {
        let _g = start();
        add_trusted_exec_prefix(String::from("/bin/"));
        seal_tighten_only();
        // "/" would trust the entire filesystem while /proc/hunter still reads
        // `exec=enforce`, which is the quiet part.
        add_trusted_exec_prefix(String::from("/"));
        add_trusted_exec_path(String::from("/tmp/payload"));
        assert_eq!(trusted_exec_count(), 1);
        assert!(!is_exec_listed("/tmp/payload"));
        assert!(is_exec_listed("/bin/ls"));
    }

    #[test]
    fn the_latch_refuses_to_empty_the_allowlist() {
        let _g = start();
        add_trusted_exec_path(String::from("/bin/sh"));
        seal_tighten_only();
        clear_trusted_exec();
        assert!(exec_allowlist_active());
        assert_eq!(trusted_exec_count(), 1);
    }

    #[test]
    fn the_latch_refuses_to_turn_learning_on_but_lets_it_be_turned_off() {
        let _g = start();
        seal_tighten_only();
        set_exec_learning(true);
        assert!(
            !exec_learning_enabled(),
            "learning never denies; turning it on is a relaxation"
        );
        // A system sealed with learning already on can still tighten.
        reset_for_test();
        set_exec_learning(true);
        seal_tighten_only();
        set_exec_learning(false);
        assert!(!exec_learning_enabled());
    }

    #[test]
    fn the_latch_stops_the_allowlist_growing_by_learning() {
        let _g = start();
        set_exec_learning(true);
        assert!(learn_exec_path("/bin/sh"));
        seal_tighten_only();
        assert!(!learn_exec_path("/opt/app/new"));
        assert_eq!(learned_exec_count(), 1);
        assert!(!is_exec_listed("/opt/app/new"));
    }

    #[test]
    fn the_latch_refuses_to_drop_the_default_syscall_whitelist() {
        let _g = start();
        set_default_whitelist(Some(alloc::vec![1, 2, 3]));
        seal_tighten_only();
        set_default_whitelist(None);
        apply_default_policy(77);
        assert!(is_syscall_allowed(77, 1).is_ok());
        assert!(
            is_syscall_allowed(77, 9).is_err(),
            "the default whitelist must survive the latch"
        );
        // Replacing one list with another may tighten or relax; that call is
        // the operator's, so the latch does not take it away.
        set_default_whitelist(Some(alloc::vec![9]));
        apply_default_policy(78);
        assert!(is_syscall_allowed(78, 9).is_ok());
        assert!(is_syscall_allowed(78, 1).is_err());
        remove_policy(77);
        remove_policy(78);
    }

    #[test]
    fn the_latch_does_not_stand_in_the_way_of_tightening() {
        let _g = start();
        seal_tighten_only();
        add_blacklisted_exec_prefix(String::from("/opt/danger/"));
        add_blacklisted_exec_path(String::from("/usr/bin/evil"));
        add_untrusted_exec_prefix(String::from("/srv/upload/"));
        assert!(is_exec_blacklisted("/opt/danger/x"));
        assert!(is_exec_blacklisted("/usr/bin/evil"));
        assert!(is_untrusted_exec_path("/srv/upload/x"));
    }

    #[test]
    fn the_latch_does_not_stand_in_the_way_of_a_process_exiting() {
        let _g = start();
        register_policy(88, alloc::vec![1]);
        seal_tighten_only();
        assert_eq!(active_policy_count(), 1);
        // Teardown is not a policy change. Gating it would leak one whitelist
        // per process for the rest of the uptime, and hand a recycled pid the
        // dead process's policy.
        remove_policy(88);
        assert_eq!(active_policy_count(), 0);
        // And a fresh process can still be given one.
        register_policy(89, alloc::vec![1]);
        inherit_policy(89, 90);
        assert!(is_syscall_allowed(90, 2).is_err());
        remove_policy(89);
        remove_policy(90);
    }
}
