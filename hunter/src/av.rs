//! Antivirus: signature scanning of what is about to run, and of any file on
//! demand.
//!
//! The other exec domains judge a program by its name, its location or a
//! digest the operator pinned for it. None of them knows what *malware* looks
//! like. This one does, the way a classic antivirus engine does: a database of
//! signatures, each one either
//!
//! * the SHA-256 of a known-bad file (`hash:<sha256>:<Name>`), or
//! * a byte pattern found inside one (`bytes:<hex>:<Name>`), where `??` stands
//!   for any byte -- `bytes:4d5a??0000:Example` matches `MZ`, any byte, two `\0`.
//!
//! ## On access
//!
//! Every image the kernel is about to run -- `execve`, the `PT_INTERP` dynamic
//! linker, `#!` interpreters, boot-time spawns -- is scanned before it is
//! mapped, on the same VMO the loader maps. A detection is a `CRIT` event
//! under [`policy::av_mode`] `Enforce` (and the exec fails with `EACCES`), a
//! `WARN` under `Report`. The verdict is cached by the image's kernel object
//! id together with the signature generation, so an unchanged binary is
//! scanned once, and scanned again when the database grows.
//!
//! ## On demand
//!
//! [`scan_file`] scans any byte stream and records the verdict in a result
//! list that `/proc/hunter_scan` renders (see `linux-object`'s procfs);
//! `hunterctl scan` drives it.
//!
//! ## Limits
//!
//! The hash covers the whole file. Byte patterns are searched in the first
//! [`MAX_PATTERN_SCAN`] bytes: a binary larger than that is still hash-checked,
//! but a pattern past the limit is not found. Matching is a plain anchored
//! search per pattern -- fine for the hundreds of patterns an operator curates,
//! not for a million-entry commercial database.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use lock::Mutex;
use sha2::{Digest as _, Sha256};

use crate::event_log::{record, Severity};
use crate::integrity::{self, Digest};
use crate::policy::{self, Mode};

/// Cap on hash signatures.
const MAX_HASH_SIGS: usize = 1 << 20;
/// Cap on byte-pattern signatures: each one is a pass over the image.
const MAX_BYTE_SIGS: usize = 4096;
/// Longest byte pattern, in bytes.
const MAX_PATTERN_LEN: usize = 1024;
/// Fewest fixed (non-`??`) bytes a pattern must have: shorter ones match
/// everything and would only produce false positives.
const MIN_FIXED_BYTES: usize = 4;
/// How far into an image byte patterns are searched.
pub const MAX_PATTERN_SCAN: usize = 128 * 1024 * 1024;
/// Bytes read per step of a scan (heap: this runs on the exec path's stack).
const CHUNK: usize = 64 * 1024;
/// Cached exec verdicts.
const MAX_CACHED: usize = 512;
/// Lines kept from the last on-demand scan.
const MAX_SCAN_RESULTS: usize = 4096;

/// One signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signature {
    /// SHA-256 of a whole known-bad file.
    Hash(Digest, String),
    /// A byte pattern; `None` is a `??` wildcard. Always starts with a fixed
    /// byte, which is what the search anchors on.
    Bytes(Vec<Option<u8>>, String),
}

impl Signature {
    pub fn name(&self) -> &str {
        match self {
            Signature::Hash(_, n) | Signature::Bytes(_, n) => n,
        }
    }
}

lazy_static::lazy_static! {
    static ref HASH_SIGS: Mutex<BTreeMap<Digest, String>> = Mutex::new(BTreeMap::new());
    static ref BYTE_SIGS: Mutex<Vec<(Vec<Option<u8>>, String)>> = Mutex::new(Vec::new());
    /// Exec verdicts by image id: (signature generation, detection).
    static ref CACHE: Mutex<BTreeMap<u64, (u64, Option<String>)>> = Mutex::new(BTreeMap::new());
    /// The last on-demand scan, one line per file.
    static ref SCAN_RESULTS: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

/// Bumped by every signature added, so a cached "clean" verdict made against
/// a smaller database is not trusted.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Detections since boot, on access and on demand.
static DETECTIONS: AtomicU64 = AtomicU64::new(0);

/// Parses one line of `/etc/hunter/signatures`. `Ok(None)` for a blank line
/// or a comment.
pub fn parse_signature(line: &str) -> Result<Option<Signature>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut parts = line.splitn(3, ':');
    let (kind, body, name) = match (parts.next(), parts.next(), parts.next()) {
        (Some(k), Some(b), Some(n)) if !n.trim().is_empty() => {
            (k, b.trim(), String::from(n.trim()))
        }
        _ => return Err(format!("expected '<hash|bytes>:<data>:<Name>': {}", line)),
    };
    match kind {
        "hash" => {
            let d = integrity::parse_hex(body)
                .ok_or_else(|| format!("not a SHA-256 digest: {}", body))?;
            Ok(Some(Signature::Hash(d, name)))
        }
        "bytes" => Ok(Some(Signature::Bytes(parse_pattern(body)?, name))),
        other => Err(format!("unknown signature kind '{}' (hash, bytes)", other)),
    }
}

/// `4d5a??00` -> `[Some(0x4d), Some(0x5a), None, Some(0x00)]` (before the
/// fixed-byte minimum is applied).
fn parse_pattern(hex: &str) -> Result<Vec<Option<u8>>, String> {
    let b = hex.as_bytes();
    if b.is_empty() || !b.len().is_multiple_of(2) {
        return Err(format!("pattern must be whole bytes of hex: {}", hex));
    }
    if b.len() / 2 > MAX_PATTERN_LEN {
        return Err(format!("pattern longer than {} bytes", MAX_PATTERN_LEN));
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        if pair == b"??" {
            out.push(None);
        } else {
            match (nibble(pair[0]), nibble(pair[1])) {
                (Some(h), Some(l)) => out.push(Some((h << 4) | l)),
                _ => return Err(format!("not hex: {}", hex)),
            }
        }
    }
    if out[0].is_none() {
        return Err(String::from(
            "pattern must start with a fixed byte, not '??'",
        ));
    }
    if out.iter().filter(|b| b.is_some()).count() < MIN_FIXED_BYTES {
        return Err(format!(
            "pattern needs at least {} fixed bytes",
            MIN_FIXED_BYTES
        ));
    }
    Ok(out)
}

/// Adds a signature to the database. Never gated by the tighten-only latch:
/// a signature can only make hunter refuse more.
pub fn add_signature(sig: Signature) -> bool {
    let added = match sig {
        Signature::Hash(d, name) => {
            let mut map = HASH_SIGS.lock();
            if map.len() >= MAX_HASH_SIGS && !map.contains_key(&d) {
                false
            } else {
                map.insert(d, name);
                true
            }
        }
        Signature::Bytes(p, name) => {
            let mut list = BYTE_SIGS.lock();
            if list.len() >= MAX_BYTE_SIGS || list.iter().any(|(q, _)| *q == p) {
                false
            } else {
                list.push((p, name));
                true
            }
        }
    };
    if added {
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    added
}

/// Takes a signature out of the database, matched by its data (the name does
/// not matter). Only [`crate::feed`] calls it, when a feed is replaced or
/// rolled back and a signature is held by nothing else; the seal never lets it
/// get this far. Bumps the generation, so a cached detection made by the
/// signature that is gone is scanned again instead of blocking forever.
pub fn remove_signature(sig: &Signature) -> bool {
    let removed = match sig {
        Signature::Hash(d, _) => HASH_SIGS.lock().remove(d).is_some(),
        Signature::Bytes(p, _) => {
            let mut list = BYTE_SIGS.lock();
            let before = list.len();
            list.retain(|(q, _)| q != p);
            list.len() != before
        }
    };
    if removed {
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
    removed
}

/// `(hash signatures, byte-pattern signatures, detections since boot)`.
pub fn stats() -> (usize, usize, u64) {
    (
        HASH_SIGS.lock().len(),
        BYTE_SIGS.lock().len(),
        DETECTIONS.load(Ordering::Relaxed),
    )
}

/// Whether the database is empty, in which case scanning is skipped.
fn database_is_empty() -> bool {
    HASH_SIGS.lock().is_empty() && BYTE_SIGS.lock().is_empty()
}

/// The bytes to scan could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unreadable;

/// Scans `len` bytes read through `read(offset, buf)` (which fills `buf` and
/// answers whether it could). `Ok(Some(name))` names the first signature that
/// matched, `Ok(None)` is clean, `Err` means the bytes could not be read.
pub fn scan(
    len: usize,
    read: &mut dyn FnMut(usize, &mut [u8]) -> bool,
) -> Result<Option<String>, Unreadable> {
    let patterns = BYTE_SIGS.lock().clone();
    let longest = patterns.iter().map(|(p, _)| p.len()).max().unwrap_or(0);
    let pattern_end = len.min(MAX_PATTERN_SCAN);
    let mut hasher = Sha256::new();
    // `window` holds the tail of the previous chunk (one byte shy of the
    // longest pattern) followed by the new one, so a pattern that straddles
    // two reads is still found, and found once.
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK + longest);
    let mut chunk = vec![0u8; CHUNK.min(len.max(1))];
    let mut off = 0;
    while off < len {
        let n = (len - off).min(chunk.len());
        if !read(off, &mut chunk[..n]) {
            return Err(Unreadable);
        }
        hasher.update(&chunk[..n]);
        if off < pattern_end && !patterns.is_empty() {
            let keep = longest.saturating_sub(1).min(window.len());
            let carried = window.len() - keep;
            window.drain(..carried);
            let usable = n.min(pattern_end - off);
            window.extend_from_slice(&chunk[..usable]);
            // Only starts not already tried in the previous window.
            let first_new = keep;
            for (p, name) in &patterns {
                if find(&window, p, first_new.saturating_sub(p.len() - 1)) {
                    // A match settles it; the rest of the file is not needed
                    // for the hash either.
                    return Ok(Some(name.clone()));
                }
            }
        }
        off += n;
    }
    let digest: Digest = hasher.finalize().into();
    Ok(HASH_SIGS.lock().get(&digest).cloned())
}

/// Whether `pat` occurs in `hay` at a start position `>= from`.
fn find(hay: &[u8], pat: &[Option<u8>], from: usize) -> bool {
    let first = match pat[0] {
        Some(b) => b,
        None => return false,
    };
    if hay.len() < pat.len() {
        return false;
    }
    let last_start = hay.len() - pat.len();
    let mut i = from;
    while i <= last_start {
        match hay[i..=last_start].iter().position(|&b| b == first) {
            None => return false,
            Some(rel) => {
                let s = i + rel;
                if pat
                    .iter()
                    .zip(&hay[s..s + pat.len()])
                    .all(|(p, h)| p.is_none_or(|p| p == *h))
                {
                    return true;
                }
                i = s + 1;
            }
        }
    }
    false
}

/// The exec hook: scans the image about to run. `names` are its paths (for
/// the record), `image_id` its kernel object id (`0` disables the cache).
/// Returns `true` if execution may proceed under [`policy::av_mode`].
pub fn check_exec_av(
    names: &[&str],
    image_id: u64,
    len: usize,
    read: &mut dyn FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    let mode = policy::av_mode();
    if mode == Mode::Off || database_is_empty() {
        return true;
    }
    let generation = GENERATION.load(Ordering::SeqCst);
    let cached = if image_id == 0 {
        None
    } else {
        CACHE
            .lock()
            .get(&image_id)
            .filter(|(g, _)| *g == generation)
            .map(|(_, v)| v.clone())
    };
    let verdict = match cached {
        Some(v) => v,
        None => {
            let v = match scan(len, read) {
                Ok(v) => v,
                // Unreadable: nothing was matched, and the loader is about to
                // fail on the same bytes anyway. Not a detection.
                Err(Unreadable) => return true,
            };
            if image_id != 0 {
                let mut cache = CACHE.lock();
                if cache.len() >= MAX_CACHED {
                    cache.pop_first();
                }
                cache.insert(image_id, (generation, v.clone()));
            }
            v
        }
    };
    let name = match verdict {
        None => return true,
        Some(n) => n,
    };
    DETECTIONS.fetch_add(1, Ordering::Relaxed);
    let shown = names.first().copied().unwrap_or("?");
    if mode == Mode::Enforce {
        warn!("hunter: BLOCKED exec of infected file {} ({})", shown, name);
        record(
            0,
            Severity::Critical,
            "AV",
            "BLOCKED",
            format!("malware {} in {}", name, shown),
        );
        false
    } else {
        warn!("hunter: malware {} in {} (report)", name, shown);
        record(
            0,
            Severity::Warning,
            "AV",
            "WARNING",
            format!("malware {} in {}", name, shown),
        );
        true
    }
}

/// Starts a new on-demand scan batch: forgets the previous results.
pub fn begin_scan_batch() {
    SCAN_RESULTS.lock().clear();
}

/// Scans one file on demand and appends its verdict to the batch, in
/// `clamscan` style: `path: OK`, `path: <Name> FOUND`, `path: ERROR <why>`.
/// A detection is also a `WARN` event, so it outlives the batch.
pub fn scan_file(path: &str, len: Option<usize>, read: &mut dyn FnMut(usize, &mut [u8]) -> bool) {
    let line = match len {
        None => format!("{}: ERROR cannot open", path),
        Some(len) => match scan(len, read) {
            Err(Unreadable) => format!("{}: ERROR read failed", path),
            Ok(None) => format!("{}: OK", path),
            Ok(Some(name)) => {
                DETECTIONS.fetch_add(1, Ordering::Relaxed);
                record(
                    0,
                    Severity::Warning,
                    "AV",
                    "DETECT",
                    format!("malware {} in {} (on-demand scan)", name, path),
                );
                format!("{}: {} FOUND", path, name)
            }
        },
    };
    let mut results = SCAN_RESULTS.lock();
    if results.len() < MAX_SCAN_RESULTS {
        results.push(line);
    }
}

/// The current batch's results, one line per file.
pub fn render_scan_results() -> String {
    let mut out = String::new();
    for l in SCAN_RESULTS.lock().iter() {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// Test-only: empty database, cache and results.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    HASH_SIGS.lock().clear();
    BYTE_SIGS.lock().clear();
    CACHE.lock().clear();
    SCAN_RESULTS.lock().clear();
    GENERATION.fetch_add(1, Ordering::SeqCst);
    DETECTIONS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A made-up "malware" sample. Not a real test string such as SAMPLE: no
    /// signature anywhere matches it, so it cannot become a false positive in
    /// the binaries that carry it.
    const SAMPLE: &[u8] = b"hunter-av unit test sample: harmless bytes";
    /// `SAMPLE`'s pattern signature: its middle, `test sample`.
    const SAMPLE_PATTERN: &str = "746573742073616d706c65";

    fn sample_sha256() -> String {
        integrity::to_hex(&Sha256::digest(SAMPLE).into())
    }

    fn reader(bytes: &[u8]) -> impl FnMut(usize, &mut [u8]) -> bool + '_ {
        move |off, buf| {
            buf.copy_from_slice(&bytes[off..off + buf.len()]);
            true
        }
    }

    fn sig(line: &str) -> Signature {
        parse_signature(line).unwrap().unwrap()
    }

    #[test]
    fn signatures_parse() {
        assert_eq!(
            sig(&format!("hash:{}:Sample-File", sample_sha256())),
            Signature::Hash(
                integrity::parse_hex(&sample_sha256()).unwrap(),
                String::from("Sample-File")
            )
        );
        assert_eq!(
            sig("bytes:4d5a??0000:Example.Name"),
            Signature::Bytes(
                vec![Some(0x4d), Some(0x5a), None, Some(0), Some(0)],
                String::from("Example.Name")
            )
        );
        assert_eq!(parse_signature(" # comment").unwrap(), None);
        for bad in [
            "bytes:4d5a:Short",
            "bytes:??4d5a0000:LeadingWildcard",
            "bytes:4d5a0:OddHex",
            "bytes:zz5a0000:NotHex",
            "hash:abcd:ShortHash",
            "md5:abcd:Kind",
            "bytes:4d5a0000",
            "bytes:4d5a0000: ",
        ] {
            assert!(parse_signature(bad).is_err(), "accepted: {}", bad);
        }
    }

    /// The database the image ships: every line parses, and none is a
    /// signature -- a sample one (SAMPLE's text) matched every file that
    /// merely quoted it, the shipped programs and documentation included.
    #[test]
    fn the_shipped_database_parses_and_holds_no_sample_signature() {
        let shipped = include_str!("../../tools/hunterctl/signatures");
        for line in shipped.lines() {
            assert_eq!(parse_signature(line).unwrap(), None, "{}", line);
        }
    }

    #[test]
    fn a_sample_is_found_by_hash_and_by_pattern() {
        let _g = crate::test_globals::lock();
        reset_for_test();
        add_signature(sig(&format!("hash:{}:Sample-Hash", sample_sha256())));
        assert_eq!(
            scan(SAMPLE.len(), &mut reader(SAMPLE)),
            Ok(Some(String::from("Sample-Hash")))
        );
        let mut longer = Vec::from(SAMPLE);
        longer.push(b'\n');
        assert_eq!(
            scan(longer.len(), &mut reader(&longer)),
            Ok(None),
            "a hash is the whole file"
        );

        add_signature(sig(&format!("bytes:{}:Sample-Pattern", SAMPLE_PATTERN)));
        assert_eq!(
            scan(longer.len(), &mut reader(&longer)),
            Ok(Some(String::from("Sample-Pattern")))
        );
        reset_for_test();
    }

    #[test]
    fn a_pattern_across_two_reads_is_found() {
        let _g = crate::test_globals::lock();
        reset_for_test();
        add_signature(sig("bytes:deadbeef??cafe:Straddler"));
        let mut img = vec![0u8; CHUNK * 2];
        // Starts three bytes before the chunk boundary.
        let at = CHUNK - 3;
        img[at..at + 7].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x11, 0xca, 0xfe]);
        assert_eq!(
            scan(img.len(), &mut reader(&img)),
            Ok(Some(String::from("Straddler")))
        );
        img[at + 5] = 0;
        assert_eq!(scan(img.len(), &mut reader(&img)), Ok(None));
        reset_for_test();
    }

    #[test]
    fn exec_is_blocked_only_under_enforce_and_scanned_once() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        reset_for_test();
        // Empty database: the reader is never asked for a byte.
        assert!(check_exec_av(&["/bin/x"], 7, 10, &mut |_, _| panic!()));

        add_signature(sig(&format!("hash:{}:Sample-Hash", sample_sha256())));
        assert!(
            check_exec_av(&["/tmp/e"], 0, SAMPLE.len(), &mut reader(SAMPLE)),
            "report"
        );
        policy::set_av_mode(Mode::Enforce);
        let mut reads = 0;
        let mut counting = |off: usize, buf: &mut [u8]| {
            reads += 1;
            buf.copy_from_slice(&SAMPLE[off..off + buf.len()]);
            true
        };
        assert!(!check_exec_av(&["/tmp/e"], 9, SAMPLE.len(), &mut counting));
        assert!(!check_exec_av(&["/tmp/e"], 9, SAMPLE.len(), &mut counting));
        assert_eq!(reads, 1, "the same image was scanned twice");
        assert_eq!(stats().2, 3);
        policy::reset_for_test();
        reset_for_test();
    }

    #[test]
    fn a_new_signature_invalidates_a_clean_verdict() {
        let _g = crate::test_globals::lock();
        policy::reset_for_test();
        reset_for_test();
        policy::set_av_mode(Mode::Enforce);
        add_signature(sig("bytes:01020304:Other"));
        assert!(check_exec_av(
            &["/tmp/e"],
            11,
            SAMPLE.len(),
            &mut reader(SAMPLE)
        ));
        add_signature(sig(&format!("hash:{}:Sample-Hash", sample_sha256())));
        assert!(
            !check_exec_av(&["/tmp/e"], 11, SAMPLE.len(), &mut reader(SAMPLE)),
            "the clean verdict from the smaller database was reused"
        );
        policy::reset_for_test();
        reset_for_test();
    }

    #[test]
    fn on_demand_results_read_like_clamscan() {
        let _g = crate::test_globals::lock();
        reset_for_test();
        add_signature(sig(&format!("hash:{}:Sample-Hash", sample_sha256())));
        begin_scan_batch();
        scan_file("/tmp/sample", Some(SAMPLE.len()), &mut reader(SAMPLE));
        scan_file("/bin/clean", Some(3), &mut reader(b"abc"));
        scan_file("/nope", None, &mut |_, _| false);
        scan_file("/bad", Some(3), &mut |_, _| false);
        assert_eq!(
            render_scan_results(),
            "/tmp/sample: Sample-Hash FOUND\n/bin/clean: OK\n/nope: ERROR cannot open\n/bad: ERROR read failed\n"
        );
        begin_scan_batch();
        assert_eq!(render_scan_results(), "");
        reset_for_test();
    }
}
