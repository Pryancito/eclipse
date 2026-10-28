//! Exec integrity: SHA-256 appraisal of the image about to run, against a set
//! of digests the operator pinned (IMA-style, without the TPM).
//!
//! The exec domain answers *where* a program may run from and *which* names
//! are trusted; neither can tell `/usr/bin/ssh` from a file of the same name
//! that someone with write access to `/usr/bin` replaced. That is this
//! domain's question, and the only one it asks:
//!
//! * a program with a pinned digest runs only if its bytes still hash to it;
//! * with `require-hash on`, a program with **no** pinned digest does not run
//!   either (the integrity analogue of a deny-by-default allowlist).
//!
//! Both are decided by [`policy::integrity_mode`]: `Report` records a mismatch
//! and lets it run, `Enforce` blocks it. With nothing pinned the domain checks
//! nothing at all, so the default costs one lock and a map miss per exec.
//!
//! ## What is hashed
//!
//! The kernel hands over the **image** it is about to map -- the same VMO the
//! loader reads the segments from -- not the file. A check that re-read the file
//! would race a writer swapping it between the hash and the load; hashing the
//! mapped bytes leaves no such window.
//!
//! ## Cost
//!
//! Only images with a pinned digest (or every image, under `require-hash`) are
//! hashed, and each image is hashed once: the digest is cached by the image's
//! kernel object id. Ids are never reused, and an exec'd image is a private
//! copy of the file that nothing writes, so a cached digest cannot go stale.
//! The kernel's own image cache hands the same VMO to every exec of an
//! unchanged binary, so a shell running `ls` a thousand times hashes it once.
//!
//! ## Names
//!
//! A program is looked up by every name the caller passes, canonicalized: the
//! name as executed and the path it resolves to through links. A digest pinned
//! as `/bin/ls` and one pinned as `/bin/busybox` both apply to `ls` when one is
//! a link to the other, and if both are pinned both must match.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use lock::Mutex;
use sha2::{Digest as _, Sha256};

use crate::event_log::{record, Severity};
use crate::policy::{self, Mode};

/// A SHA-256 digest.
pub type Digest = [u8; 32];

/// Cap on pinned digests, bounding kernel memory against a runaway list.
const MAX_HASHES: usize = 65_536;

/// Cap on cached image digests. Kernel object ids only grow, so evicting the
/// smallest key evicts the oldest image.
const MAX_CACHED: usize = 512;

/// Bytes hashed per read of the image: heap, not stack -- this runs on the
/// guard-page-less coroutine stack of every `execve`.
const CHUNK: usize = 64 * 1024;

lazy_static::lazy_static! {
    /// Pinned digests by canonical program path.
    static ref HASHES: Mutex<BTreeMap<String, Digest>> = Mutex::new(BTreeMap::new());
    /// Digests already computed, by image (kernel object) id.
    static ref CACHE: Mutex<BTreeMap<u64, Digest>> = Mutex::new(BTreeMap::new());
}

/// Whether a program with no pinned digest is a violation too.
static REQUIRE_HASH: AtomicBool = AtomicBool::new(false);

/// Lower-case hex of a digest, for the audit trail.
pub fn to_hex(d: &Digest) -> String {
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Parses 64 hex digits (either case) into a digest.
pub fn parse_hex(s: &str) -> Option<Digest> {
    let s = s.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    for (i, pair) in s.chunks(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

/// Parses one line of `sha256sum` output -- `<hex>  <path>`, or `<hex> *<path>`
/// in binary mode -- which is the format of `/etc/hunter/hashes`, so the file
/// can be written by `sha256sum` itself. `Ok(None)` for a blank line or a
/// comment.
pub fn parse_sums_line(line: &str) -> Result<Option<(Digest, String)>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let (hex, rest) = line
        .split_once(char::is_whitespace)
        .ok_or_else(|| format!("expected '<sha256> <path>': {}", line))?;
    let digest = parse_hex(hex).ok_or_else(|| format!("not a SHA-256 digest: {}", hex))?;
    let path = rest.trim_start();
    let path = path.strip_prefix('*').unwrap_or(path);
    if !path.starts_with('/') {
        return Err(format!("path must be absolute: {}", path));
    }
    Ok(Some((digest, String::from(path))))
}

/// Pins `digest` for the program at `path`, replacing any earlier pin.
///
/// Gated by the tighten-only latch whatever it replaces: a new pin under
/// `require-hash` admits a program, and a changed one admits different bytes
/// under an old name -- both relax the policy.
pub fn add_exec_hash(path: &str, digest: Digest) {
    if !policy::allow_relaxing("pin exec hash") {
        return;
    }
    let canon = policy::canonicalize(path);
    let mut map = HASHES.lock();
    if map.len() >= MAX_HASHES && !map.contains_key(&canon) {
        record(
            0,
            Severity::Warning,
            "CONTROL",
            "WARNING",
            format!(
                "exec hash list full ({}); not pinned: {}",
                MAX_HASHES, canon
            ),
        );
        return;
    }
    if map.insert(canon.clone(), digest) != Some(digest) {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("exec hash pinned: {} {}", to_hex(&digest), canon),
        );
    }
}

/// Number of pinned digests.
pub fn exec_hash_count() -> usize {
    HASHES.lock().len()
}

/// The digest pinned for `path`, if any.
pub fn pinned(path: &str) -> Option<Digest> {
    HASHES.lock().get(&policy::canonicalize(path)).copied()
}

/// Turns `require-hash` on or off. Off is a relaxation and refused once sealed.
pub fn set_require_hash(on: bool) {
    if !on && !policy::allow_relaxing("require-hash off") {
        return;
    }
    if REQUIRE_HASH.swap(on, Ordering::SeqCst) != on {
        record(
            0,
            Severity::Notice,
            "CONTROL",
            "CONFIG",
            format!("require-hash {}", if on { "on" } else { "off" }),
        );
    }
}

/// Whether a program with no pinned digest is a violation.
pub fn require_hash() -> bool {
    REQUIRE_HASH.load(Ordering::SeqCst)
}

/// SHA-256 of an image of `len` bytes, read through `read(offset, buf)`,
/// which fills `buf` from `offset` and answers whether it could.
pub fn sha256_image(len: usize, read: &mut dyn FnMut(usize, &mut [u8]) -> bool) -> Option<Digest> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK.min(len.max(1))];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len());
        if !read(off, &mut buf[..n]) {
            return None;
        }
        hasher.update(&buf[..n]);
        off += n;
    }
    Some(hasher.finalize().into())
}

/// The exec hook: appraises the image about to run.
///
/// `names` are the program's paths (as executed, and resolved through links);
/// `image_id` the kernel object id of the image, used to hash each image once
/// (`0` disables the cache); `len` and `read` give its bytes. Returns `true`
/// if execution may proceed under [`policy::integrity_mode`].
pub fn check_exec_integrity(
    names: &[&str],
    image_id: u64,
    len: usize,
    read: &mut dyn FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    let mode = policy::integrity_mode();
    if mode == Mode::Off {
        return true;
    }
    let expected: Vec<(String, Digest)> = {
        let map = HASHES.lock();
        if map.is_empty() && !require_hash() {
            return true;
        }
        names
            .iter()
            .map(|n| policy::canonicalize(n))
            .filter_map(|c| map.get(&c).map(|d| (c, *d)))
            .collect()
    };
    let shown = names.first().copied().unwrap_or("?");
    if expected.is_empty() {
        if !require_hash() {
            return true;
        }
        return violation(mode, format!("no pinned SHA-256 (require-hash): {}", shown));
    }
    let actual = match cached(image_id) {
        Some(d) => d,
        None => match sha256_image(len, read) {
            Some(d) => {
                remember(image_id, d);
                d
            }
            None => return violation(mode, format!("image unreadable for hashing: {}", shown)),
        },
    };
    for (path, want) in &expected {
        if *want != actual {
            return violation(
                mode,
                format!(
                    "SHA-256 mismatch: {} is {} but {} was pinned",
                    path,
                    to_hex(&actual),
                    to_hex(want)
                ),
            );
        }
    }
    true
}

fn cached(image_id: u64) -> Option<Digest> {
    if image_id == 0 {
        return None;
    }
    CACHE.lock().get(&image_id).copied()
}

fn remember(image_id: u64, d: Digest) {
    if image_id == 0 {
        return;
    }
    let mut cache = CACHE.lock();
    if cache.len() >= MAX_CACHED {
        cache.pop_first();
    }
    cache.insert(image_id, d);
}

/// Records an integrity violation and answers whether the exec may proceed.
fn violation(mode: Mode, what: String) -> bool {
    if mode == Mode::Enforce {
        warn!("hunter: BLOCKED exec ({})", what);
        record(0, Severity::Critical, "INTEGRITY", "BLOCKED", what);
        false
    } else {
        warn!("hunter: integrity violation (report): {}", what);
        record(0, Severity::Warning, "INTEGRITY", "WARNING", what);
        true
    }
}

/// Test-only: forget every pin, every cached digest and `require-hash`.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    HASHES.lock().clear();
    CACHE.lock().clear();
    REQUIRE_HASH.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256("abc"), FIPS 180-2 appendix B.1.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn reader(bytes: &'static [u8]) -> impl FnMut(usize, &mut [u8]) -> bool {
        move |off, buf| {
            buf.copy_from_slice(&bytes[off..off + buf.len()]);
            true
        }
    }

    fn check(names: &[&str], id: u64, bytes: &'static [u8]) -> bool {
        check_exec_integrity(names, id, bytes.len(), &mut reader(bytes))
    }

    #[test]
    fn the_digest_is_sha256() {
        assert_eq!(to_hex(&sha256_image(3, &mut reader(b"abc")).unwrap()), ABC);
        assert_eq!(parse_hex(ABC).map(|d| to_hex(&d)).as_deref(), Some(ABC));
        assert_eq!(parse_hex(&ABC.to_uppercase()), parse_hex(ABC));
        assert!(parse_hex(&ABC[1..]).is_none());
        assert!(parse_hex(&ABC.replace('a', "g")).is_none());
    }

    #[test]
    fn sha256sum_lines_parse_in_both_modes() {
        let text = format!("{}  /bin/abc", ABC);
        assert_eq!(
            parse_sums_line(&text).unwrap(),
            Some((parse_hex(ABC).unwrap(), String::from("/bin/abc")))
        );
        let binary = format!("{} */bin/abc", ABC);
        assert_eq!(
            parse_sums_line(&binary).unwrap(),
            parse_sums_line(&text).unwrap()
        );
        assert_eq!(parse_sums_line("  # comment").unwrap(), None);
        assert!(parse_sums_line(&format!("{}  bin/abc", ABC)).is_err());
        assert!(parse_sums_line("deadbeef  /bin/abc").is_err());
    }

    #[test]
    fn nothing_pinned_checks_nothing() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        policy::set_integrity_mode(Mode::Enforce);
        // The reader would panic if it were ever asked for a byte.
        assert!(check_exec_integrity(
            &["/bin/x"],
            1,
            10,
            &mut |_, _| panic!()
        ));
        policy::reset_for_test();
    }

    #[test]
    fn a_pinned_program_runs_only_with_its_own_bytes() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        add_exec_hash("/bin/abc", parse_hex(ABC).unwrap());

        assert!(check(&["/bin/abc"], 0, b"abc"));
        // Report: a swapped binary is recorded but still runs.
        assert!(check(&["/bin/abc"], 0, b"abd"));
        policy::set_integrity_mode(Mode::Enforce);
        assert!(!check(&["/bin/abc"], 0, b"abd"));
        assert!(check(&["/bin/abc"], 0, b"abc"));
        // Through a non-canonical name, and through a link whose target is pinned.
        assert!(!check(&["/bin/../bin/abc"], 0, b"abd"));
        assert!(!check(&["/usr/local/bin/link", "/bin/abc"], 0, b"abd"));
        // An unpinned program is none of this domain's business by default.
        assert!(check(&["/bin/other"], 0, b"anything"));
        policy::reset_for_test();
    }

    #[test]
    fn require_hash_denies_the_unpinned() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        policy::set_integrity_mode(Mode::Enforce);
        set_require_hash(true);
        assert!(!check(&["/bin/other"], 0, b"anything"));
        add_exec_hash("/bin/abc", parse_hex(ABC).unwrap());
        assert!(check(&["/bin/abc"], 0, b"abc"));
        policy::reset_for_test();
    }

    #[test]
    fn each_image_is_hashed_once() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        policy::set_integrity_mode(Mode::Enforce);
        add_exec_hash("/bin/abc", parse_hex(ABC).unwrap());
        let mut reads = 0;
        let mut count = |off: usize, buf: &mut [u8]| {
            reads += 1;
            buf.copy_from_slice(&b"abc"[off..off + buf.len()]);
            true
        };
        assert!(check_exec_integrity(&["/bin/abc"], 42, 3, &mut count));
        assert!(check_exec_integrity(&["/bin/abc"], 42, 3, &mut count));
        assert_eq!(reads, 1, "the second exec of the same image read it again");
        policy::reset_for_test();
    }

    #[test]
    fn an_unreadable_image_is_a_violation() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        policy::set_integrity_mode(Mode::Enforce);
        add_exec_hash("/bin/abc", parse_hex(ABC).unwrap());
        assert!(!check_exec_integrity(&["/bin/abc"], 0, 3, &mut |_, _| {
            false
        }));
        policy::reset_for_test();
    }

    #[test]
    fn the_latch_stops_new_pins_and_require_hash_off() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        set_require_hash(true);
        policy::seal_tighten_only();
        add_exec_hash("/bin/abc", parse_hex(ABC).unwrap());
        assert_eq!(
            exec_hash_count(),
            0,
            "a pin after the seal admits new bytes"
        );
        set_require_hash(false);
        assert!(require_hash());
        policy::reset_for_test();
    }
}
