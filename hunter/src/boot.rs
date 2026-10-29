//! Boot integrity: a lightweight measured boot, and a watch on the kernel's
//! own code once it runs.
//!
//! The [`crate::integrity`] domain appraises every program at `execve`, and
//! nothing appraises what runs before the first `execve`: the kernel, its
//! command line, the initramfs that holds `/sbin/init`. An attacker with
//! write access to the ESP does not need to touch a single pinned binary --
//! a `cmdline` without hunter's switches, a `kernel.elf` with the exec gate
//! patched out, or an initramfs whose `/etc/hunter/policy.conf` says
//! `mode exec off` all boot a system on which every later check is moot. This
//! domain is that question:
//!
//! * **measured boot** -- the loader (rboot) hashes each input it hands over
//!   and the kernel compares the digests against the ones the operator pinned
//!   in `/etc/hunter/boot`;
//! * **kernel code** -- the kernel hashes its own `.text` and `.rodata` once at
//!   boot and again on demand (and periodically); a digest that changes while
//!   the machine runs is code patched in memory, which is what a kernel rootkit
//!   does and nothing legitimate in this kernel does.
//!
//! ## Items
//!
//! | Item          | Measured by | What                                      |
//! |---------------|-------------|-------------------------------------------|
//! | `kernel`      | rboot       | the kernel ELF file on the ESP            |
//! | `initramfs`   | rboot       | the initramfs file (`measure=all` only)   |
//! | `rboot.conf`  | rboot       | the loader config, which names the rest   |
//! | `cmdline`     | rboot       | the kernel command line                   |
//! | `kernel-code` | the kernel  | `.text` + `.rodata` as mapped, at boot    |
//!
//! `/etc/hunter/boot` is `sha256sum`-shaped, with an item where the path
//! would be (`<sha256>  kernel`), and `hunterctl boot pin` writes it from the
//! running boot's own measurements (trust on first use, like `hunterctl
//! hash`). Only the items listed there are appraised.
//!
//! ## Verdicts
//!
//! For each pinned item: its measurement matches (ok), differs (**mismatch**),
//! or is missing (**unmeasured**). Missing is a violation, not a pass: a
//! loader told `measure=off`, or an older rboot that never measured, must not
//! be the way around a pin. An item that is measured and not pinned is shown
//! and not judged.
//!
//! ## Mode
//!
//! This domain shares [`policy::integrity_mode`] with exec integrity -- it is
//! the same question about earlier bytes. `Off` checks nothing; `Report`
//! records a violation as a `Critical` event; `Enforce` records it and then
//! **locks hunter down**: exec, integrity and av to `enforce`, learning off,
//! and the tighten-only latch engaged. A boot cannot be un-booted, so this is
//! the one response left that means something: a system that started from
//! inputs nobody pinned keeps every guard it has up and cannot be talked out
//! of them (not by root, not by a `policy.conf` the tampered initramfs ships)
//! until a clean reboot.
//!
//! ## Limits
//!
//! The chain starts at rboot and nothing here measures rboot: an rboot an
//! attacker replaced reports whatever digests it likes. That link belongs to
//! UEFI Secure Boot. Likewise the golden values live on the root filesystem,
//! which in a live boot *is* the initramfs: there, pin from a trusted copy of
//! the file, not from the running system. What this domain does catch on its
//! own is the rest of the ESP changing under an rboot that did not, and the
//! kernel's code changing after it started.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lock::Mutex;

use crate::event_log::{record, Severity};
use crate::integrity::{parse_hex, to_hex, Digest};
use crate::policy::{self, Mode};

/// Every item this domain knows. A pin for anything else is a typo, and a typo
/// that silently pinned nothing would read as a protected boot.
pub const ITEMS: [&str; 5] = [
    "kernel",
    "initramfs",
    "rboot.conf",
    "cmdline",
    "kernel-code",
];

/// The item the kernel measures itself (see [`set_code_measurer`]).
pub const KERNEL_CODE: &str = "kernel-code";

/// A measurement: digest and size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    pub digest: Digest,
    pub size: u64,
}

/// How one item stands against its pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Pinned, measured, equal.
    Ok,
    /// Pinned, measured, different.
    Mismatch,
    /// Pinned and not measured on this boot.
    Unmeasured,
    /// Measured and not pinned: shown, not judged.
    Unpinned,
}

impl ItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Ok => "ok",
            ItemStatus::Mismatch => "MISMATCH",
            ItemStatus::Unmeasured => "UNMEASURED",
            ItemStatus::Unpinned => "unpinned",
        }
    }

    fn is_violation(self) -> bool {
        matches!(self, ItemStatus::Mismatch | ItemStatus::Unmeasured)
    }
}

/// The boot as a whole, worst item first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The domain is `off`.
    Off,
    /// Nothing was measured and nothing is pinned.
    Unmeasured,
    /// Measurements exist; nothing is pinned to judge them by.
    Unpinned,
    /// Every pinned item matches, and the kernel's code is as it booted.
    Ok,
    /// A pinned item is missing or different, or the kernel's code changed.
    Tampered,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Off => "off",
            Verdict::Unmeasured => "unmeasured",
            Verdict::Unpinned => "unpinned",
            Verdict::Ok => "ok",
            Verdict::Tampered => "TAMPERED",
        }
    }
}

lazy_static::lazy_static! {
    /// This boot's measurements, by item.
    static ref MEASURED: Mutex<BTreeMap<&'static str, Measured>> = Mutex::new(BTreeMap::new());
    /// The operator's golden digests, by item.
    static ref PINS: Mutex<BTreeMap<&'static str, Digest>> = Mutex::new(BTreeMap::new());
    /// The kernel-code digest of the most recent re-check that differed from
    /// the boot one, so a patch is reported once and not every period.
    static ref LAST_BAD_CODE: Mutex<Option<Digest>> = Mutex::new(None);
}

/// Hashes the kernel's code as mapped right now (registered by the kernel).
static CODE_MEASURER: Mutex<Option<fn() -> Option<Digest>>> = Mutex::new(None);

/// Which loader measured this boot, if any (for the report).
static LOADER: Mutex<Option<&'static str>> = Mutex::new(None);

/// Whether the kernel's code has differed from its boot digest since boot.
/// Sticky: code that was patched and patched back is still a compromised boot.
static CODE_MODIFIED: AtomicBool = AtomicBool::new(false);

/// Re-checks of the kernel's code, and when the last one ran (ns since boot).
static CODE_CHECKS: AtomicU64 = AtomicU64::new(0);
static CODE_CHECKED_AT: AtomicU64 = AtomicU64::new(0);

/// Whether the boot appraisal found a violation (sticky, like the above).
static BOOT_TAMPERED: AtomicBool = AtomicBool::new(false);

/// Whether a violation already locked hunter down, so it is done once.
static LOCKED_DOWN: AtomicBool = AtomicBool::new(false);

/// The `&'static` spelling of a known item.
fn item(name: &str) -> Option<&'static str> {
    ITEMS.iter().copied().find(|i| *i == name)
}

/// Records the loader's measurement of `name`. Unknown items are dropped: the
/// set is fixed by [`ITEMS`] on both ends.
pub fn record_measurement(name: &str, digest: Digest, size: u64) {
    if let Some(i) = item(name) {
        MEASURED.lock().insert(i, Measured { digest, size });
    }
}

/// Names the loader that measured this boot (`"rboot"`), for the report.
pub fn set_loader(name: &'static str) {
    *LOADER.lock() = Some(name);
}

/// This boot's measurement of `name`, if there is one.
pub fn measured(name: &str) -> Option<Measured> {
    MEASURED.lock().get(name).copied()
}

/// Every measurement, in item order (for `hunterctl boot pin`).
pub fn measurements() -> Vec<(&'static str, Measured)> {
    let m = MEASURED.lock();
    ITEMS
        .iter()
        .filter_map(|i| m.get(i).map(|v| (*i, *v)))
        .collect()
}

/// Registers the kernel's code measurer and takes the boot digest with it:
/// the `kernel-code` measurement every later [`verify_code`] compares to.
/// `size` is how many bytes the measurer hashes, for the report.
pub fn set_code_measurer(f: fn() -> Option<Digest>, size: u64) {
    *CODE_MEASURER.lock() = Some(f);
    if let Some(digest) = f() {
        MEASURED
            .lock()
            .insert(KERNEL_CODE, Measured { digest, size });
    }
}

/// Parses one line of `/etc/hunter/boot`: `<sha256>  <item>`. `Ok(None)` for a
/// blank line or a comment.
pub fn parse_pin_line(line: &str) -> Result<Option<(&'static str, Digest)>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (hex, rest) = line
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("expected '<sha256> <item>': {}", line))?;
    let digest = parse_hex(hex).ok_or_else(|| format!("not a SHA-256 digest: {}", hex))?;
    let name = rest.trim_start();
    let name = name.strip_prefix('*').unwrap_or(name).trim_end();
    let i = item(name).ok_or_else(|| {
        format!(
            "unknown boot item '{}' (one of: {})",
            name,
            ITEMS.join(", ")
        )
    })?;
    Ok(Some((i, digest)))
}

/// Parses a whole pin file, all or nothing (the same rule as the control
/// language: half a set of golden values is a boot nobody pinned).
pub fn parse_pins(text: &str) -> Result<Vec<(&'static str, Digest)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        match parse_pin_line(line) {
            Ok(Some(p)) => out.push(p),
            Ok(None) => {}
            Err(e) => return Err(format!("line {}: {}", n + 1, e)),
        }
    }
    Ok(out)
}

/// Pins `digest` for `name`. A first pin tightens and is always taken; a
/// changed pin admits different bytes under the same item and goes through
/// the tighten-only latch, like [`crate::integrity::add_exec_hash`].
pub fn pin(name: &str, digest: Digest) -> Result<(), String> {
    let i = item(name).ok_or_else(|| format!("unknown boot item '{}'", name))?;
    let mut pins = PINS.lock();
    match pins.get(i) {
        Some(d) if *d == digest => return Ok(()),
        Some(_) => {
            // `allow_relaxing` records the refusal; it takes no lock of ours.
            if !policy::allow_relaxing("re-pin boot item") {
                return Err(format!("boot item '{}' is pinned and sealed", i));
            }
        }
        None => {}
    }
    pins.insert(i, digest);
    drop(pins);
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("boot item pinned: {} {}", to_hex(&digest), i),
    );
    Ok(())
}

/// Number of pinned items.
pub fn pin_count() -> usize {
    PINS.lock().len()
}

/// The digest pinned for `name`, if any.
pub fn pinned(name: &str) -> Option<Digest> {
    PINS.lock().get(name).copied()
}

/// How `name` stands, or `None` when it is neither pinned nor measured.
pub fn item_status(name: &str) -> Option<ItemStatus> {
    let want = pinned(name);
    let have = measured(name);
    match (want, have) {
        (None, None) => None,
        (None, Some(_)) => Some(ItemStatus::Unpinned),
        (Some(_), None) => Some(ItemStatus::Unmeasured),
        (Some(w), Some(h)) if w == h.digest => Some(ItemStatus::Ok),
        (Some(_), Some(_)) => Some(ItemStatus::Mismatch),
    }
}

/// Appraises this boot against the pins: records each violation, and under
/// `Enforce` locks hunter down. Called once by the kernel after
/// `/etc/hunter/boot` and `policy.conf` are loaded (the mode has to be the
/// operator's, not the default), and again whenever pins are added.
pub fn appraise() -> Verdict {
    let mode = policy::integrity_mode();
    if mode == Mode::Off {
        return Verdict::Off;
    }
    let mut bad = Vec::new();
    for i in ITEMS {
        let status = match item_status(i) {
            Some(s) => s,
            None => continue,
        };
        if !status.is_violation() {
            continue;
        }
        let want = pinned(i).map(|d| to_hex(&d)).unwrap_or_default();
        let what = match (status, measured(i)) {
            (ItemStatus::Mismatch, Some(m)) => format!(
                "boot item {} is {} but {} was pinned",
                i,
                to_hex(&m.digest),
                want
            ),
            _ => format!(
                "boot item {} is pinned ({}) but was not measured on this boot",
                i, want
            ),
        };
        bad.push(what);
    }
    if bad.is_empty() {
        return verdict();
    }
    BOOT_TAMPERED.store(true, Ordering::SeqCst);
    for what in bad {
        violation(mode, what);
    }
    verdict()
}

/// Re-hashes the kernel's code and compares it with the boot digest. `None`
/// when there is nothing to compare (no measurer, or the domain is off);
/// otherwise whether the code is as it booted. A change is recorded once per
/// new digest, and under `Enforce` locks hunter down.
pub fn verify_code() -> Option<bool> {
    let mode = policy::integrity_mode();
    if mode == Mode::Off {
        return None;
    }
    let f = (*CODE_MEASURER.lock())?;
    let boot = measured(KERNEL_CODE)?;
    let now = f()?;
    CODE_CHECKS.fetch_add(1, Ordering::Relaxed);
    CODE_CHECKED_AT.store(crate::clock::now_ns(), Ordering::Relaxed);
    if now == boot.digest {
        return Some(true);
    }
    CODE_MODIFIED.store(true, Ordering::SeqCst);
    let fresh = {
        let mut last = LAST_BAD_CODE.lock();
        let fresh = *last != Some(now);
        *last = Some(now);
        fresh
    };
    if fresh {
        violation(
            mode,
            format!(
                "kernel code changed in memory since boot: {} is now {}",
                to_hex(&boot.digest),
                to_hex(&now)
            ),
        );
    }
    Some(false)
}

/// The boot as a whole, as of the last appraisal and code check.
pub fn verdict() -> Verdict {
    if policy::integrity_mode() == Mode::Off {
        return Verdict::Off;
    }
    let tampered = BOOT_TAMPERED.load(Ordering::SeqCst)
        || CODE_MODIFIED.load(Ordering::SeqCst)
        || ITEMS
            .iter()
            .any(|i| item_status(i).is_some_and(ItemStatus::is_violation));
    if tampered {
        Verdict::Tampered
    } else if pin_count() > 0 {
        Verdict::Ok
    } else if MEASURED.lock().is_empty() {
        Verdict::Unmeasured
    } else {
        Verdict::Unpinned
    }
}

/// Records a violation and, under `Enforce`, locks hunter down (once).
fn violation(mode: Mode, what: String) {
    warn!("hunter: boot integrity violation: {}", what);
    let enforce = mode == Mode::Enforce;
    record(
        0,
        Severity::Critical,
        "BOOT",
        if enforce { "BLOCKED" } else { "WARNING" },
        what,
    );
    if enforce && !LOCKED_DOWN.swap(true, Ordering::SeqCst) {
        lockdown();
    }
}

/// What `hunterctl lockdown` does, from inside: exec, integrity and av to
/// `enforce`, learning off, the latch engaged. Nothing here relaxes, so the
/// latch never refuses any of it.
fn lockdown() {
    record(
        0,
        Severity::Critical,
        "BOOT",
        "BLOCKED",
        String::from("boot integrity violation under integrity=enforce: locking hunter down"),
    );
    policy::set_exec_mode(Mode::Enforce);
    policy::set_integrity_mode(Mode::Enforce);
    policy::set_av_mode(Mode::Enforce);
    policy::set_exec_learning(false);
    policy::seal_tighten_only();
}

/// `/proc/hunter_boot`: the verdict, then one line per item.
///
/// ```text
/// verdict ok
/// loader rboot
/// kernel-code checks=3 last=600.000s modified=no
/// item kernel ok size=12345 measured=<sha256> pinned=<sha256>
/// item initramfs unmeasured size=- measured=- pinned=-
/// ```
///
/// Every item has a line, pinned or not, so a reader never has to know the
/// list; `-` is "none". The `item` lines are what `hunterctl boot pin` reads.
pub fn render() -> String {
    let mut out = String::new();
    out.push_str(&format!("verdict {}\n", verdict().as_str()));
    out.push_str(&format!(
        "loader {}\n",
        LOADER
            .lock()
            .unwrap_or("none (this boot was not measured by its loader)")
    ));
    out.push_str(&format!(
        "kernel-code checks={} last={} modified={}\n",
        CODE_CHECKS.load(Ordering::Relaxed),
        match CODE_CHECKED_AT.load(Ordering::Relaxed) {
            0 => String::from("never"),
            ns => format!("{}.{:03}s", ns / 1_000_000_000, (ns / 1_000_000) % 1000),
        },
        if CODE_MODIFIED.load(Ordering::SeqCst) {
            "YES"
        } else {
            "no"
        },
    ));
    for i in ITEMS {
        let m = measured(i);
        let p = pinned(i);
        let status = item_status(i)
            .map(ItemStatus::as_str)
            .unwrap_or("unmeasured");
        out.push_str(&format!(
            "item {} {} size={} measured={} pinned={}\n",
            i,
            status,
            m.map(|m| format!("{}", m.size))
                .unwrap_or_else(|| String::from("-")),
            m.map(|m| to_hex(&m.digest))
                .unwrap_or_else(|| String::from("-")),
            p.map(|d| to_hex(&d)).unwrap_or_else(|| String::from("-")),
        ));
    }
    out
}

/// A write to `/proc/hunter_boot`: `verify` re-checks the kernel's code now;
/// anything else is pin lines (`<sha256>  <item>`), applied all or nothing
/// and then appraised.
pub fn apply_command(text: &str) -> Result<(), String> {
    if text.trim() == "verify" {
        verify_code();
        return Ok(());
    }
    let pins = parse_pins(text)?;
    // All or nothing against the latch too: check every re-pin first.
    if policy::is_tighten_only() {
        let held = PINS.lock();
        if let Some((i, _)) = pins
            .iter()
            .find(|(i, d)| held.get(i).is_some_and(|h| h != d))
        {
            let i = *i;
            drop(held);
            policy::allow_relaxing("re-pin boot item");
            return Err(format!("boot item '{}' is pinned and sealed", i));
        }
    }
    for (i, d) in pins {
        pin(i, d)?;
    }
    appraise();
    Ok(())
}

/// Test-only: forget every measurement, pin and verdict.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    MEASURED.lock().clear();
    PINS.lock().clear();
    *LAST_BAD_CODE.lock() = None;
    *CODE_MEASURER.lock() = None;
    *LOADER.lock() = None;
    CODE_MODIFIED.store(false, Ordering::SeqCst);
    CODE_CHECKS.store(0, Ordering::SeqCst);
    CODE_CHECKED_AT.store(0, Ordering::SeqCst);
    BOOT_TAMPERED.store(false, Ordering::SeqCst);
    LOCKED_DOWN.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU8;

    /// SHA-256("abc").
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn d(byte: u8) -> Digest {
        [byte; 32]
    }

    fn fresh() -> self::std::sync::MutexGuard<'static, ()> {
        let g = crate::test_globals::lock();
        policy::reset_for_test();
        g
    }

    extern crate std;

    #[test]
    fn pin_lines_are_sha256sum_shaped_with_an_item_for_a_path() {
        let digest = parse_hex(ABC).unwrap();
        assert_eq!(
            parse_pin_line(&format!("{}  kernel", ABC)).unwrap(),
            Some(("kernel", digest))
        );
        assert_eq!(
            parse_pin_line(&format!("{} *rboot.conf", ABC)).unwrap(),
            Some(("rboot.conf", digest))
        );
        assert_eq!(parse_pin_line("   # golden values").unwrap(), None);
        assert!(parse_pin_line(&format!("{}  kernal", ABC)).is_err());
        assert!(parse_pin_line("abc  kernel").is_err());
        // One bad line rejects the file.
        let text = format!("{}  kernel\n{}  nonsense\n", ABC, ABC);
        assert!(parse_pins(&text).unwrap_err().starts_with("line 2:"));
    }

    #[test]
    fn a_boot_with_nothing_pinned_is_not_judged() {
        let _g = fresh();
        assert_eq!(verdict(), Verdict::Unmeasured);
        record_measurement("kernel", d(1), 10);
        assert_eq!(appraise(), Verdict::Unpinned);
        assert_eq!(item_status("kernel"), Some(ItemStatus::Unpinned));
        assert_eq!(item_status("initramfs"), None);
        policy::reset_for_test();
    }

    #[test]
    fn a_pinned_item_must_match_and_must_be_measured() {
        let _g = fresh();
        record_measurement("kernel", d(1), 10);
        record_measurement("cmdline", d(2), 3);
        pin("kernel", d(1)).unwrap();
        assert_eq!(appraise(), Verdict::Ok);

        pin("cmdline", d(9)).unwrap();
        assert_eq!(item_status("cmdline"), Some(ItemStatus::Mismatch));
        assert_eq!(appraise(), Verdict::Tampered);
        policy::reset_for_test();

        // Pinned and missing: `measure=off`, or an rboot that never measured.
        record_measurement("kernel", d(1), 10);
        pin("initramfs", d(3)).unwrap();
        assert_eq!(item_status("initramfs"), Some(ItemStatus::Unmeasured));
        assert_eq!(appraise(), Verdict::Tampered);
        policy::reset_for_test();
    }

    #[test]
    fn off_judges_nothing_and_report_changes_no_mode() {
        let _g = fresh();
        record_measurement("kernel", d(1), 10);
        pin("kernel", d(2)).unwrap();
        policy::set_integrity_mode(Mode::Off);
        assert_eq!(appraise(), Verdict::Off);
        policy::set_integrity_mode(Mode::Report);
        assert_eq!(appraise(), Verdict::Tampered);
        assert_eq!(policy::exec_mode(), Mode::Report, "report mode locked down");
        assert!(!policy::is_tighten_only());
        policy::reset_for_test();
    }

    #[test]
    fn enforce_locks_hunter_down_on_a_tampered_boot() {
        let _g = fresh();
        policy::set_integrity_mode(Mode::Enforce);
        record_measurement("kernel", d(1), 10);
        pin("kernel", d(2)).unwrap();
        assert_eq!(appraise(), Verdict::Tampered);
        assert_eq!(policy::exec_mode(), Mode::Enforce);
        assert_eq!(policy::av_mode(), Mode::Enforce);
        assert!(!policy::exec_learning_enabled());
        assert!(policy::is_tighten_only());
        // And it cannot be talked out of it.
        policy::set_integrity_mode(Mode::Report);
        assert_eq!(policy::integrity_mode(), Mode::Enforce);
        policy::reset_for_test();
    }

    /// The kernel's code, as the fake measurer sees it: 0 is the boot image.
    static CODE: AtomicU8 = AtomicU8::new(0);

    fn code_now() -> Option<Digest> {
        Some(d(CODE.load(Ordering::SeqCst)))
    }

    #[test]
    fn patched_kernel_code_is_caught_and_stays_caught() {
        let _g = fresh();
        CODE.store(0, Ordering::SeqCst);
        set_code_measurer(code_now, 4096);
        assert_eq!(measured(KERNEL_CODE).map(|m| m.size), Some(4096));
        assert_eq!(verify_code(), Some(true));
        assert_eq!(verdict(), Verdict::Unpinned);

        let before = crate::event_log::stats();
        CODE.store(7, Ordering::SeqCst);
        assert_eq!(verify_code(), Some(false));
        assert_eq!(verify_code(), Some(false));
        let after = crate::event_log::stats();
        assert_eq!(
            after.total - before.total,
            1,
            "one patch, reported once per new digest"
        );
        // Patched back: still a compromised boot.
        CODE.store(0, Ordering::SeqCst);
        assert_eq!(verify_code(), Some(true));
        assert_eq!(verdict(), Verdict::Tampered);
        assert!(render().contains("modified=YES"));
        policy::reset_for_test();
    }

    #[test]
    fn a_sealed_pin_cannot_be_changed_and_a_write_is_all_or_nothing() {
        let _g = fresh();
        let text = format!("{}  kernel\n", ABC);
        apply_command(&text).unwrap();
        assert_eq!(pinned("kernel"), parse_hex(ABC));
        policy::seal_tighten_only();
        // Same pin again is fine; a new item still tightens.
        apply_command(&format!("{}  kernel\n{}  cmdline\n", ABC, ABC)).unwrap();
        assert_eq!(pin_count(), 2);
        // A changed pin is refused, and so is the new item riding with it.
        let other = to_hex(&d(5));
        assert!(apply_command(&format!("{}  initramfs\n{}  kernel\n", other, other)).is_err());
        assert_eq!(pinned("kernel"), parse_hex(ABC));
        assert_eq!(pinned("initramfs"), None);
        policy::reset_for_test();
    }

    #[test]
    fn the_report_lists_every_item() {
        let _g = fresh();
        record_measurement("kernel", parse_hex(ABC).unwrap(), 3);
        set_loader("rboot");
        let r = render();
        assert!(r.starts_with("verdict unpinned\n"), "{}", r);
        assert!(r.contains("loader rboot\n"));
        assert!(r.contains(&format!(
            "item kernel unpinned size=3 measured={} pinned=-\n",
            ABC
        )));
        for i in ITEMS {
            assert!(r.contains(&format!("item {} ", i)), "no line for {}", i);
        }
        policy::reset_for_test();
    }
}
