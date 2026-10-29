//! Signature feeds: the antivirus databases `hunterctl update-signatures`
//! downloads, verified with Ed25519 before a single line of them is believed,
//! and replaced or rolled back as a whole.
//!
//! A feed used to be trusted because it arrived over HTTPS. That says who
//! *served* it, not who *wrote* it: a compromised mirror, a hijacked feed URL
//! or a TLS-intercepting proxy hands over whatever it likes, and an antivirus
//! signature is not harmless just because it can only make hunter refuse more.
//! `hash:<sha256 of /bin/sh>` or `bytes:7f454c46:x` (the ELF magic) under
//! `mode av enforce` stops the whole system from running anything. The bytes
//! that decide what may run deserve the same proof of origin as the programs.
//!
//! ## Signed feeds
//!
//! The publisher signs a small detached header, `<feed>.sig`:
//!
//! ```text
//! hunter-feed-signature 1
//! feed Eclipse.Main
//! serial 2026102918
//! key eclipse-main
//! sha256 <64 hex: the SHA-256 of the feed file>
//! ed25519 <128 hex: Ed25519 over every byte above this line>
//! ```
//!
//! The signature covers the header, and the header covers the body through
//! its SHA-256, so the body can be any size while the signed message stays a
//! few hundred bytes. Each field is there to close one hole:
//!
//! * `feed` -- a signed feed cannot be installed under another feed's name
//!   (the publisher's tiny test feed in place of the real one).
//! * `serial` -- a monotonic version, so an old but genuinely signed feed
//!   cannot be replayed to take back the signatures a newer one added. An
//!   older serial is refused unless the operator asks for a rollback.
//! * `key` -- which trusted key to check it with. Keys come from the
//!   `feed-key <name> <hex>` directive (`/etc/hunter/keys` at boot).
//!
//! Plain Ed25519 (RFC 8032), so `openssl pkeyutl -sign -rawin` on any Linux
//! box can sign one (`tools/hunterctl/hunter-feed-sign` does exactly that).
//!
//! ## Unsigned feeds
//!
//! abuse.ch signs nothing, so the default MalwareBazaar feed is unsigned, and
//! is accepted as before unless `require-signed-feeds on`. Two rules hold
//! even then: a feed that was installed signed is never replaced by an
//! unsigned one (stripping the `.sig` is not a way around the check), and a
//! key name, once trusted, keeps its key until reboot.
//!
//! ## Replacing and rolling back
//!
//! hunter remembers which signatures every feed brought. Installing a new
//! version adds what is new and takes out what the old version had and the
//! new one does not -- unless something else still holds it (another feed,
//! `/etc/hunter/signatures`, a `signature` directive). That is what makes a
//! rollback work live: a feed whose latest version blocks a legitimate
//! program is rolled back, and the program runs again without a reboot.
//!
//! The tighten-only latch keeps its promise here too: once sealed, no
//! signature ever leaves the database. Replacements and rollbacks still add,
//! and what they would have removed stays until reboot, said in the event.

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;
use sha2::{Digest as _, Sha256};

use crate::av::{self, Signature};
use crate::event_log::{record, Severity};
use crate::integrity::{self, Digest};
use crate::policy;

/// First line of a feed signature file, with its format version.
const MAGIC: &str = "hunter-feed-signature 1";
/// Longest feed or key name.
const MAX_NAME: usize = 64;
/// Trusted keys at most: a handful of publishers, not a PKI.
const MAX_KEYS: usize = 64;
/// Feeds at most.
const MAX_FEEDS: usize = 256;

/// An Ed25519 public key.
pub type PublicKey = [u8; 32];

/// A signature's identity in the database: its data, not its name. Two feeds
/// naming the same hash differently hold the same signature.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SigKey {
    Hash(Digest),
    Bytes(Vec<Option<u8>>),
}

impl SigKey {
    fn of(sig: &Signature) -> SigKey {
        match sig {
            Signature::Hash(d, _) => SigKey::Hash(*d),
            Signature::Bytes(p, _) => SigKey::Bytes(p.clone()),
        }
    }

    /// Back to a signature [`av::remove_signature`] can match (by data).
    fn to_signature(&self) -> Signature {
        match self {
            SigKey::Hash(d) => Signature::Hash(*d, String::new()),
            SigKey::Bytes(p) => Signature::Bytes(p.clone(), String::new()),
        }
    }
}

/// A parsed `<feed>.sig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedSignature {
    pub feed: String,
    pub serial: u64,
    pub key: String,
    pub sha256: Digest,
    signature: [u8; 64],
    /// How many leading bytes of the file the signature covers.
    signed_len: usize,
}

/// One installed feed.
struct Installed {
    /// `None` for an unsigned feed.
    serial: Option<u64>,
    key: Option<String>,
    sha256: Digest,
    sigs: BTreeSet<SigKey>,
}

lazy_static::lazy_static! {
    static ref KEYS: Mutex<BTreeMap<String, PublicKey>> = Mutex::new(BTreeMap::new());
    static ref FEEDS: Mutex<BTreeMap<String, Installed>> = Mutex::new(BTreeMap::new());
    /// Signatures added outside any feed: `/etc/hunter/signatures` and the
    /// `signature` directive. A feed going away never takes these with it.
    static ref LOCAL: Mutex<BTreeSet<SigKey>> = Mutex::new(BTreeSet::new());
    /// What the last install said, first line of `/proc/hunter_feed`, so
    /// `hunterctl` can tell the operator *why* without digging in the events.
    static ref LAST: Mutex<String> = Mutex::new(String::new());
}

/// Whether an unsigned feed is refused.
static REQUIRE_SIGNED: AtomicBool = AtomicBool::new(false);

/// How a feed is being installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// A download (or the boot-time load): its serial may not go backwards.
    Update,
    /// The operator going back to an older version on purpose.
    Rollback,
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Signatures the feed now holds.
    pub total: usize,
    /// New since the version it replaced.
    pub added: usize,
    /// In the old version and not in this one.
    pub dropped: usize,
    /// Of those, how many stayed in the database: still held by someone
    /// else, or the latch is engaged.
    pub kept: usize,
    /// The very same feed was already installed; nothing changed.
    pub unchanged: bool,
}

/// Feed and key names: what can be a file name in `/etc/hunter/signatures.d`
/// and is not a hidden staging file or a `.sig` beside a feed.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && !name.ends_with(".sig")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// Lower-case hex of any bytes.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// 128 hex digits into an Ed25519 signature.
fn parse_sig_hex(s: &str) -> Option<[u8; 64]> {
    if s.len() != 128 {
        return None;
    }
    let lo = integrity::parse_hex(&s[..64])?;
    let hi = integrity::parse_hex(&s[64..])?;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&lo);
    out[32..].copy_from_slice(&hi);
    Some(out)
}

/// Parses a `<feed>.sig` file. Strict on purpose: exactly the six lines, in
/// order, `\n`-terminated, so there is one way to read the signed bytes and
/// nothing unsigned can sit beside them looking like it was.
pub fn parse_feed_signature(text: &str) -> Result<FeedSignature, String> {
    let mut lines = text.split_inclusive('\n');
    let mut signed_len = 0;
    let mut next = |want: &str| -> Result<String, String> {
        let raw = lines
            .next()
            .ok_or_else(|| format!("signature file ends before '{}'", want))?;
        let line = raw
            .strip_suffix('\n')
            .ok_or_else(|| format!("'{}' line is not newline-terminated", want))?;
        let value = if want == MAGIC {
            if line != MAGIC {
                return Err(format!(
                    "not a hunter feed signature (expected '{}')",
                    MAGIC
                ));
            }
            String::new()
        } else {
            let value = line
                .strip_prefix(want)
                .and_then(|r| r.strip_prefix(' '))
                .ok_or_else(|| format!("expected '{} <value>', got '{}'", want, line))?;
            String::from(value)
        };
        if want != "ed25519" {
            signed_len += raw.len();
        }
        Ok(value)
    };
    next(MAGIC)?;
    let feed = next("feed")?;
    let serial = next("serial")?;
    let key = next("key")?;
    let sha256 = next("sha256")?;
    let sig = next("ed25519")?;
    if !valid_name(&feed) {
        return Err(format!("bad feed name '{}'", feed));
    }
    if !valid_name(&key) {
        return Err(format!("bad key name '{}'", key));
    }
    // Digits only: `u64::from_str` takes a leading `+`, and the same serial
    // must not have two spellings.
    if serial.is_empty() || serial.len() > 19 || !serial.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("serial must be a decimal number: '{}'", serial));
    }
    let serial = serial
        .parse()
        .map_err(|_| format!("serial out of range: '{}'", serial))?;
    let sha256 = integrity::parse_hex(&sha256)
        .ok_or_else(|| format!("not a SHA-256 digest: '{}'", sha256))?;
    let signature =
        parse_sig_hex(&sig).ok_or_else(|| String::from("ed25519 must be 128 hex digits"))?;
    if lines.any(|l| !l.trim().is_empty()) {
        return Err(String::from("text after the ed25519 line"));
    }
    Ok(FeedSignature {
        feed,
        serial,
        key,
        sha256,
        signature,
        signed_len,
    })
}

/// Checks `sig` (the text it was parsed from is `text`) against the trusted
/// key it names. Says which check failed.
fn verify_signature(sig: &FeedSignature, text: &str) -> Result<(), String> {
    let pk = KEYS
        .lock()
        .get(&sig.key)
        .copied()
        .ok_or_else(|| format!("signed with key '{}', which is not trusted", sig.key))?;
    let pk = ed25519_compact::PublicKey::from_slice(&pk)
        .map_err(|_| format!("key '{}' is not a valid Ed25519 key", sig.key))?;
    let s = ed25519_compact::Signature::from_slice(&sig.signature)
        .map_err(|_| String::from("malformed Ed25519 signature"))?;
    pk.verify(&text.as_bytes()[..sig.signed_len], &s)
        .map_err(|_| format!("Ed25519 signature does not verify with key '{}'", sig.key))
}

/// Trusts `key` for signed feeds under `name`. Refused once sealed (the seal
/// freezes who is believed), and a name that already has a different key
/// keeps it: replacing a trust anchor is not something one directive should
/// do quietly.
pub fn add_key(name: &str, key: PublicKey) -> bool {
    let refuse = |why: String| {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!("refused feed-key {}: {}", name, why),
        );
        false
    };
    if policy::is_tighten_only() {
        return refuse(String::from("tighten-only latch"));
    }
    if ed25519_compact::PublicKey::from_slice(&key).is_err() {
        return refuse(String::from("not an Ed25519 public key"));
    }
    let mut keys = KEYS.lock();
    match keys.get(name) {
        Some(k) if *k == key => return true,
        Some(_) => {
            drop(keys);
            return refuse(String::from(
                "that name already has another key (until reboot)",
            ));
        }
        None if keys.len() >= MAX_KEYS => {
            drop(keys);
            return refuse(format!("more than {} keys", MAX_KEYS));
        }
        None => {
            keys.insert(String::from(name), key);
        }
    }
    drop(keys);
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("feed-key {} {} trusted", name, hex(&key)),
    );
    true
}

/// `require-signed-feeds on|off`. Off is a relaxation, refused once sealed.
pub fn set_require_signed(on: bool) {
    if !on && policy::is_tighten_only() {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            String::from(
                "refused relaxing control change: require-signed-feeds off (tighten-only latch)",
            ),
        );
        return;
    }
    REQUIRE_SIGNED.store(on, Ordering::SeqCst);
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("require-signed-feeds {}", if on { "on" } else { "off" }),
    );
}

/// Whether unsigned feeds are refused.
pub fn require_signed() -> bool {
    REQUIRE_SIGNED.load(Ordering::SeqCst)
}

/// Adds a signature that belongs to no feed (`/etc/hunter/signatures`, the
/// `signature` directive), so a feed going away never takes it along.
pub fn add_local(sig: Signature) -> bool {
    LOCAL.lock().insert(SigKey::of(&sig));
    av::add_signature(sig)
}

/// Verifies and installs feed `name` from `body` (hunter's signature lines)
/// and, for a signed feed, the text of its `.sig`. All or nothing: a feed that
/// fails any check, or has one line that does not parse, changes nothing and
/// is a `CRIT` event -- a signature that does not verify is what tampering
/// looks like.
pub fn install(
    name: &str,
    body: &[u8],
    sig: Option<&str>,
    how: Install,
    pid: u64,
) -> Result<Report, String> {
    let result = try_install(name, body, sig, how);
    let line = match &result {
        Ok(r) if r.unchanged => format!("feed {}: already installed, unchanged", name),
        Ok(r) => format!(
            "feed {}{}: {} signature(s), +{} -{}{}",
            name,
            if how == Install::Rollback {
                " ROLLED BACK"
            } else {
                ""
            },
            r.total,
            r.added,
            r.dropped,
            if r.kept > 0 {
                format!(" ({} kept: still held, or sealed until reboot)", r.kept)
            } else {
                String::new()
            }
        ),
        Err(e) => format!("feed {}: REJECTED: {}; nothing installed", name, e),
    };
    let severity = match (&result, how) {
        (Err(_), _) => Severity::Critical,
        (Ok(r), _) if r.unchanged => Severity::Info,
        (Ok(_), Install::Rollback) => Severity::Warning,
        (Ok(_), Install::Update) => Severity::Notice,
    };
    record(
        pid,
        severity,
        "CONTROL",
        if result.is_err() {
            "REJECTED"
        } else {
            "CONFIG"
        },
        line.clone(),
    );
    *LAST.lock() = line;
    result
}

fn try_install(name: &str, body: &[u8], sig: Option<&str>, how: Install) -> Result<Report, String> {
    if !valid_name(name) {
        return Err(String::from("bad feed name"));
    }
    let sha256: Digest = Sha256::digest(body).into();
    let signed = match sig {
        Some(text) => {
            let s = parse_feed_signature(text)?;
            if s.feed != name {
                return Err(format!("signature is for feed '{}'", s.feed));
            }
            verify_signature(&s, text)?;
            if s.sha256 != sha256 {
                return Err(String::from(
                    "feed does not match its signature (SHA-256 differs)",
                ));
            }
            Some(s)
        }
        None if require_signed() => {
            return Err(String::from("unsigned, and require-signed-feeds is on"))
        }
        None => None,
    };
    let text = core::str::from_utf8(body).map_err(|_| String::from("not UTF-8"))?;
    let mut sigs = Vec::new();
    for (i, line) in text.lines().enumerate() {
        match av::parse_signature(line) {
            Ok(Some(s)) => sigs.push(s),
            Ok(None) => {}
            Err(e) => return Err(format!("line {}: {}", i + 1, e)),
        }
    }
    if sigs.is_empty() {
        return Err(String::from("no signatures in it"));
    }

    let mut feeds = FEEDS.lock();
    if let Some(cur) = feeds.get(name) {
        match (&signed, cur.serial) {
            (None, Some(_)) => {
                return Err(String::from(
                    "the installed version is signed; an unsigned one cannot replace it",
                ))
            }
            (Some(s), Some(old)) if s.serial < old && how == Install::Update => {
                return Err(format!(
                    "serial {} is older than the installed {} (a replayed feed?); \
                     roll back explicitly to go back",
                    s.serial, old
                ))
            }
            (Some(s), Some(old)) if s.serial == old && cur.sha256 != sha256 => {
                return Err(format!(
                    "serial {} is already installed with different content",
                    old
                ))
            }
            _ => {}
        }
        if cur.sha256 == sha256 {
            return Ok(Report {
                total: cur.sigs.len(),
                added: 0,
                dropped: 0,
                kept: 0,
                unchanged: true,
            });
        }
    } else if feeds.len() >= MAX_FEEDS {
        return Err(format!("more than {} feeds", MAX_FEEDS));
    }

    let new_set: BTreeSet<SigKey> = sigs.iter().map(SigKey::of).collect();
    let old = feeds.remove(name);
    let old_set = old.map(|o| o.sigs).unwrap_or_default();
    let added = new_set.difference(&old_set).count();
    for s in sigs {
        av::add_signature(s);
    }
    let mut dropped = 0;
    let mut kept = 0;
    let sealed = policy::is_tighten_only();
    {
        let local = LOCAL.lock();
        for k in old_set.difference(&new_set) {
            dropped += 1;
            let held = local.contains(k) || feeds.values().any(|f| f.sigs.contains(k));
            if held || sealed {
                kept += 1;
            } else {
                av::remove_signature(&k.to_signature());
            }
        }
    }
    let total = new_set.len();
    feeds.insert(
        String::from(name),
        Installed {
            serial: signed.as_ref().map(|s| s.serial),
            key: signed.map(|s| s.key),
            sha256,
            sigs: new_set,
        },
    );
    Ok(Report {
        total,
        added,
        dropped,
        kept,
        unchanged: false,
    })
}

/// `/proc/hunter_feed`: the last install's outcome, the switch, the trusted
/// keys and the installed feeds, one per line, for `hunterctl feeds`.
pub fn render() -> String {
    let mut out = String::new();
    let last = LAST.lock().clone();
    out.push_str(&format!(
        "last: {}\n",
        if last.is_empty() { "-" } else { &last }
    ));
    out.push_str(&format!(
        "require-signed-feeds: {}\n",
        if require_signed() { "on" } else { "off" }
    ));
    for (name, k) in KEYS.lock().iter() {
        out.push_str(&format!("key {} {}\n", name, hex(k)));
    }
    for (name, f) in FEEDS.lock().iter() {
        out.push_str(&format!(
            "feed {} serial={} key={} signatures={} sha256={}\n",
            name,
            f.serial
                .map(|s| format!("{}", s))
                .unwrap_or_else(|| String::from("-")),
            f.key.as_deref().unwrap_or("-"),
            f.sigs.len(),
            hex(&f.sha256)
        ));
    }
    out
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    KEYS.lock().clear();
    FEEDS.lock().clear();
    LOCAL.lock().clear();
    LAST.lock().clear();
    REQUIRE_SIGNED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests;
