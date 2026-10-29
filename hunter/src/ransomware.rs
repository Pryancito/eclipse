//! Ransomware detection by behaviour (part of the [`behavior`] domain).
//!
//! Signatures catch ransomware somebody has already seen; this catches what
//! any of it has to *do*, whoever wrote it: read the victim's files and put
//! ciphertext in their place, fast, many files in a row. The kernel reports
//! three operations on regular files -- a write ([`on_write`]), a rename
//! ([`on_rename`]) and an unlink ([`on_unlink`]) -- and each process gets a
//! 10-second window of counters. An alarm is raised when, within one window:
//!
//! 1. **Encrypt and replace** -- at least [`ENCRYPTED_FILES`] distinct files
//!    were written with contents that look like random bytes *and* at least
//!    [`DESTROYED_FILES`] files were unlinked whose names those copies were
//!    made from: `a.docx` deleted beside a new `a.docx.locked` (an extension
//!    appended) or `a.locked` (the extension replaced), in either order.
//!    Writing the ciphertext beside the original and deleting the original is
//!    the commonest shape.
//! 2. **Encrypt in place** -- at least [`ENCRYPTED_FILES`] files that already
//!    had contents were overwritten with random-looking bytes.
//! 3. **Mass renaming** -- at least [`SAME_EXTENSION_RENAMES`] renames in a
//!    row gave files one and the same new extension, by appending it or by
//!    replacing the old one (`a.docx` -> `a.docx.locked`, `b.jpg` ->
//!    `b.jpg.locked`, ...).
//! 4. **Canary** -- any write, rename or unlink of a decoy file the operator
//!    declared with `canary <path>`. Nothing legitimate touches a file nobody
//!    knows is there, so this one fires on the first operation.
//!
//! Why each rule needs two signals, not one: unpacking an archive of photos
//! writes dozens of random-looking files a second (compressed data *is*
//! random-looking), and `rm -rf` unlinks thousands -- but neither destroys
//! the files it read. It is the combination, or the uniform new extension,
//! that ordinary programs do not produce. The *names* matter as much: a
//! package manager writes `x.png.dpkg-new` (random-looking), renames it over
//! `x.png` and deletes `x.png.dpkg-tmp` -- many renames and unlinks of
//! random-looking files -- but it never deletes the name a new file was made
//! from, nor gives a run of files one new extension, so it trips nothing.
//!
//! What still looks exactly like ransomware, because it *is* the same shape,
//! is `gzip` run over a dozen files at once (`a.log` -> `a.log.gz`, original
//! deleted). Under `Report` that is one warning; an operator who does it
//! routinely can `exempt /usr/bin/gzip`.
//!
//! Rule 2 is told apart from an image editor saving over its files by the
//! truncation: an editor opens with `O_TRUNC`, so the file is empty when the
//! first write arrives, while ransomware that encrypts in place overwrites
//! the old bytes where they are.
//!
//! "Random-looking" is a cheap estimate, not an entropy computation: the
//! number of distinct byte values in the first [`SAMPLE`] bytes of a write.
//! Uniform random data shows about 221 of the 256 values there; text shows a
//! few dozen and machine code well under 200. Writes shorter than the sample
//! are not judged at all.
//!
//! Cost: with the domain off, the kernel asks [`behavior::watching`] and does
//! nothing else. On, a write to a regular file costs a path hash, one sharded
//! lock and a scan of at most [`SEEN_SLOTS`] slots; the inode's size is
//! asked once per file per window, and the byte census runs only on writes of
//! at least [`SAMPLE`] bytes.
//!
//! [`behavior`]: crate::behavior

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lock::Mutex;

use crate::behavior::{self, Detector};
use crate::clock;
use crate::event_log::{record, Severity};

/// Length of the per-process window.
const WINDOW_NS: u64 = 10_000_000_000; // 10 seconds
/// Operations after which the window rolls even if the clock has not moved,
/// so a frozen clock cannot keep one window -- and its spent alarm -- open for
/// ever (the same backstop the rate heuristics have, P12).
const OPS_BACKSTOP: u32 = 100_000;
/// Distinct random-looking files in a window for rules 1 and 2.
pub const ENCRYPTED_FILES: u32 = 12;
/// Unlinked originals of random-looking files in a window, for rule 1.
pub const DESTROYED_FILES: u32 = 12;
/// Consecutive renames to one new extension for rule 3.
pub const SAME_EXTENSION_RENAMES: u32 = 12;
/// Files remembered per process per window. Past this many, further files
/// are not counted, which is harmless: every rule fires well before it.
pub const SEEN_SLOTS: usize = 32;
/// Writes to one file that are judged before it is given up as not random.
/// A file's first chunk may be a plain header with ciphertext after it.
const JUDGED_WRITES: u8 = 4;
/// Bytes of a write the census looks at.
pub const SAMPLE: usize = 512;
/// Distinct byte values in [`SAMPLE`] bytes from which a write is taken for
/// ciphertext. Uniform random bytes give ~221 with a spread of about 4.5, so
/// this is more than four spreads below; see the module heading.
const RANDOM_DISTINCT: u32 = 200;
/// Longest extension taken as one (`.locked`, `.encrypted`, `.WNCRY` all fit);
/// anything longer is a name with a dot in it.
const MAX_EXTENSION: usize = 16;
/// Processes tracked before the least recently active one is dropped. A
/// window is about 3 KiB, so this bounds the detector at about 1.5 MiB.
const MAX_TRACKED: usize = 512;
/// Dot-cut prefixes kept per written file (see [`Names::prefixes`]). Six
/// covers `a.docx.id-1234.[mail@host.tld].locked`, where the dots of the
/// attacker's address count too -- the longest decoration in common use.
const PREFIXES: usize = 6;
/// Independently locked shards, as in the rate heuristics: writes come from
/// every CPU and unrelated processes should not queue on one lock.
const SHARDS: usize = 16;
/// Canaries the operator may declare.
const MAX_CANARIES: usize = 256;

/// How a path names a file, as far as telling an encrypted copy from the
/// original it was made from goes. All of it hashes of parts of the path.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Names {
    /// The whole path.
    key: u64,
    /// The path cut at each of the last [`PREFIXES`] dots of its last
    /// component (`/h/a.docx.locked` -> `/h/a.docx`, `/h/a`): the names it may
    /// have been made from by appending extensions. 0 where there is none.
    prefixes: [u64; PREFIXES],
    /// For a last component with exactly one extension, the path without it
    /// (`/h/a.locked` -> `/h/a`); 0 otherwise. Two such names with one stem
    /// and different keys are one file with its extension replaced.
    single_stem: u64,
}

impl Names {
    pub fn of(path: &str) -> Names {
        let start = path.rfind('/').map_or(0, |i| i + 1);
        let last = &path[start..];
        let mut n = Names {
            key: hash(path),
            ..Names::default()
        };
        // Dots of the last component, not counting a leading one (`.bashrc`).
        let dots = last
            .char_indices()
            .filter(|&(i, c)| c == '.' && i > 0)
            .map(|(i, _)| i);
        let mut count = 0;
        let mut first = 0;
        for (k, i) in dots.collect::<Vec<_>>().into_iter().rev().enumerate() {
            if k < PREFIXES {
                n.prefixes[k] = hash(&path[..start + i]);
            }
            count += 1;
            first = i;
        }
        if count == 1 && last.len() - first - 1 <= MAX_EXTENSION && first + 1 < last.len() {
            n.single_stem = hash(&path[..start + first]);
        }
        n
    }

    /// Whether `self` names a file a copy named `copy` may have been made
    /// from: `copy` is `self` with extensions appended, or `self` with its one
    /// extension replaced.
    fn is_original_of(&self, copy: &Names) -> bool {
        self.as_original().is_original_of(copy)
    }

    /// The part of the names an original is recognised by.
    fn as_original(&self) -> Original {
        Original {
            key: self.key,
            single_stem: self.single_stem,
        }
    }
}

/// What is kept of an unlinked file's [`Names`]: an original is only ever
/// compared against copies, never the other way, so its prefixes are not.
#[derive(Clone, Copy, Default)]
struct Original {
    key: u64,
    single_stem: u64,
}

impl Original {
    fn is_original_of(&self, copy: &Names) -> bool {
        if self.key == copy.key {
            return false;
        }
        copy.prefixes.contains(&self.key)
            || (self.single_stem != 0 && self.single_stem == copy.single_stem)
    }
}

/// One file written in the current window.
#[derive(Clone, Copy, Default)]
struct Seen {
    names: Names,
    /// It had contents before this window's first write to it.
    existed: bool,
    /// Its contents were judged random-looking (counted once).
    random: bool,
    /// Writes judged so far (see [`JUDGED_WRITES`]).
    judged: u8,
}

/// One file unlinked in the current window.
#[derive(Clone, Copy, Default)]
struct Gone {
    original: Original,
    /// Already counted as the original of a random-looking copy.
    linked: bool,
}

/// What the alarm says was seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    EncryptAndReplace,
    EncryptInPlace,
    MassRename,
}

/// A process's counters for the current window.
pub struct Activity {
    window_start: u64,
    ops: u32,
    seen: [Seen; SEEN_SLOTS],
    seen_len: usize,
    gone: [Gone; SEEN_SLOTS],
    gone_len: usize,
    /// Distinct files whose new contents look random.
    random_files: u32,
    /// Of those, the ones that had contents before (overwritten in place).
    random_overwrites: u32,
    /// Unlinked files that were the originals of random-looking copies.
    replaced: u32,
    /// Hash of the extension the current run of renames gives files, and how
    /// long the run is.
    ext_run: u64,
    ext_run_len: u32,
    alerted: bool,
}

impl Activity {
    pub fn new(now: u64) -> Self {
        Activity {
            window_start: now,
            ops: 0,
            seen: [Seen::default(); SEEN_SLOTS],
            seen_len: 0,
            gone: [Gone::default(); SEEN_SLOTS],
            gone_len: 0,
            random_files: 0,
            random_overwrites: 0,
            replaced: 0,
            ext_run: 0,
            ext_run_len: 0,
            alerted: false,
        }
    }

    /// Starts a new window if this one is over by the clock or the backstop.
    pub fn roll(&mut self, now: u64) {
        if now.saturating_sub(self.window_start) >= WINDOW_NS || self.ops >= OPS_BACKSTOP {
            // In place, not `*self = Activity::new(now)`: the slot arrays are
            // a few KiB, and that would build them on the kernel stack of the
            // syscall to copy them over. Slots past the lengths are dead.
            self.window_start = now;
            self.ops = 0;
            self.seen_len = 0;
            self.gone_len = 0;
            self.random_files = 0;
            self.random_overwrites = 0;
            self.replaced = 0;
            self.ext_run = 0;
            self.ext_run_len = 0;
            self.alerted = false;
        }
    }

    fn find(&self, key: u64) -> Option<usize> {
        self.seen[..self.seen_len]
            .iter()
            .position(|s| s.names.key == key)
    }

    /// Counts a write of a file already seen this window, or answers `false`
    /// when the file is new to the window -- the caller then asks the inode
    /// whether it had contents and calls [`Self::write_new`].
    pub fn write_seen(&mut self, key: u64, random: Option<bool>) -> bool {
        self.ops = self.ops.saturating_add(1);
        match self.find(key) {
            Some(i) => {
                self.judge(i, random);
                true
            }
            None => false,
        }
    }

    /// Counts the first write of a file this window.
    pub fn write_new(&mut self, names: Names, existed: bool, random: Option<bool>) {
        if let Some(i) = self.find(names.key) {
            // Another CPU of the same process got here first.
            self.judge(i, random);
            return;
        }
        if self.seen_len == SEEN_SLOTS {
            return;
        }
        self.seen[self.seen_len] = Seen {
            names,
            existed,
            ..Seen::default()
        };
        self.seen_len += 1;
        self.judge(self.seen_len - 1, random);
    }

    /// `random` is the census of this write: `None` when it was too short to
    /// say anything.
    fn judge(&mut self, i: usize, random: Option<bool>) {
        let s = &mut self.seen[i];
        let random = match random {
            Some(r) if !s.random && s.judged < JUDGED_WRITES => r,
            _ => return,
        };
        s.judged += 1;
        if random {
            s.random = true;
            self.random_files += 1;
            if s.existed {
                self.random_overwrites += 1;
            }
            // The original may have been deleted before its copy was written.
            let copy = s.names;
            for g in self.gone[..self.gone_len].iter_mut() {
                if !g.linked && g.original.is_original_of(&copy) {
                    g.linked = true;
                    self.replaced += 1;
                    break;
                }
            }
        }
    }

    /// Counts a rename from `old` to `new` (paths or bare names).
    pub fn rename(&mut self, old: &str, new: &str) {
        self.ops = self.ops.saturating_add(1);
        let (old_names, new_names) = (
            Names::of(last_component(old)),
            Names::of(last_component(new)),
        );
        match extension(new) {
            Some(n) if old_names.is_original_of(&new_names) => {
                let h = hash(n);
                if self.ext_run_len > 0 && self.ext_run == h {
                    self.ext_run_len += 1;
                } else {
                    self.ext_run = h;
                    self.ext_run_len = 1;
                }
            }
            // Any other rename (one that keeps the name's stem but not as an
            // original of the new name, a temporary file published under its
            // real name, ...) breaks the run: the rule is about renames *in a
            // row*.
            _ => self.ext_run_len = 0,
        }
    }

    /// Counts an unlink of `path`.
    pub fn unlink(&mut self, path: &str) {
        self.ops = self.ops.saturating_add(1);
        let original = Names::of(path).as_original();
        let copy_exists = self.seen[..self.seen_len]
            .iter()
            .any(|s| s.random && original.is_original_of(&s.names));
        if copy_exists {
            self.replaced += 1;
        } else if self.gone_len < SEEN_SLOTS {
            // Kept in case its copy is written after it is gone.
            self.gone[self.gone_len] = Gone {
                original,
                linked: false,
            };
            self.gone_len += 1;
        }
    }

    /// The pattern this window now shows, once per window.
    pub fn check(&mut self) -> Option<Pattern> {
        if self.alerted {
            return None;
        }
        let found = if self.ext_run_len >= SAME_EXTENSION_RENAMES {
            Some(Pattern::MassRename)
        } else if self.random_files >= ENCRYPTED_FILES && self.replaced >= DESTROYED_FILES {
            Some(Pattern::EncryptAndReplace)
        } else if self.random_overwrites >= ENCRYPTED_FILES {
            Some(Pattern::EncryptInPlace)
        } else {
            None
        };
        self.alerted = found.is_some();
        found
    }

    fn describe(&self, p: Pattern) -> String {
        let secs = WINDOW_NS / 1_000_000_000;
        match p {
            Pattern::EncryptAndReplace => format!(
                "possible ransomware: {} files written with random-looking contents and {} originals deleted within {}s",
                self.random_files, self.replaced, secs
            ),
            Pattern::EncryptInPlace => format!(
                "possible ransomware: {} existing files overwritten with random-looking contents within {}s",
                self.random_overwrites, secs
            ),
            Pattern::MassRename => format!(
                "possible ransomware: {} files in a row renamed to one new extension within {}s",
                self.ext_run_len, secs
            ),
        }
    }
}

/// Whether `data` looks like ciphertext (or other random bytes); `None` when
/// it is shorter than [`SAMPLE`] and so says nothing either way.
pub fn looks_random(data: &[u8]) -> Option<bool> {
    if data.len() < SAMPLE {
        return None;
    }
    let mut present = [0u64; 4];
    for &b in &data[..SAMPLE] {
        present[(b >> 6) as usize] |= 1 << (b & 63);
    }
    let distinct: u32 = present.iter().map(|w| w.count_ones()).sum();
    Some(distinct >= RANDOM_DISTINCT)
}

/// The extension of the last component of `path`: after its last dot, not
/// counting a leading one (`.bashrc` has none), at most [`MAX_EXTENSION`]
/// bytes and without a further `/`.
fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    let ext = &name[dot + 1..];
    if ext.is_empty() || ext.len() > MAX_EXTENSION {
        return None;
    }
    Some(ext)
}

/// The last component of `path`.
fn last_component(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// FNV-1a; the paths are only ever compared within one process's window, so
/// a collision costs at worst one file counted once instead of twice.
fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    }) | 1
}

/// Paths the detector does not look at: pseudo-files are not anybody's data,
/// and a write to `/dev/pts/N` is a terminal, not a file.
fn is_data_path(path: &str) -> bool {
    !(path.is_empty()
        || path.starts_with("/proc/")
        || path.starts_with("/sys/")
        || path.starts_with("/dev/"))
}

/// One shard. The windows are boxed: a `BTreeMap` stores its values in its
/// nodes, eleven to a node, and a node of inline windows would be a 33 KiB
/// allocation.
#[repr(align(64))]
struct Shard(Mutex<BTreeMap<u64, Box<Activity>>>);

lazy_static::lazy_static! {
    static ref ACTIVITY: [Shard; SHARDS] =
        core::array::from_fn(|_| Shard(Mutex::new(BTreeMap::new())));
    static ref CANARIES: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

/// Canaries declared; checked first so a system without any never takes the
/// canary lock on the write path.
static CANARY_COUNT: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn shard(pid: u64) -> &'static Mutex<BTreeMap<u64, Box<Activity>>> {
    &ACTIVITY[(pid as usize) % SHARDS].0
}

/// Runs `f` on `pid`'s rolled window, creating it (and evicting the least
/// recently active process of the shard) if needed.
fn with_activity<R>(pid: u64, now: u64, f: impl FnOnce(&mut Activity) -> R) -> R {
    let mut map = shard(pid).lock();
    if !map.contains_key(&pid) && map.len() >= MAX_TRACKED / SHARDS {
        if let Some((&victim, _)) = map.iter().min_by_key(|(_, a)| a.window_start) {
            map.remove(&victim);
        }
    }
    let a = map
        .entry(pid)
        .or_insert_with(|| Box::new(Activity::new(now)));
    a.roll(now);
    f(a)
}

/// Declares a decoy file. Adding one only ever tightens, so it is allowed
/// after the seal too.
pub fn add_canary(path: String) {
    let mut list = CANARIES.lock();
    if list.contains(&path) || list.len() >= MAX_CANARIES {
        return;
    }
    record(
        0,
        Severity::Notice,
        "CONTROL",
        "CONFIG",
        format!("ransomware canary: {}", path),
    );
    list.push(path);
    CANARY_COUNT.store(list.len(), Ordering::Relaxed);
}

pub fn canary_count() -> usize {
    CANARY_COUNT.load(Ordering::Relaxed)
}

fn is_canary(path: &str) -> bool {
    if CANARY_COUNT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    CANARIES.lock().iter().any(|p| p == path)
}

/// What every hook asks first: is the domain on, is the path data, is the
/// process contained, is it a canary. `Some(verdict)` ends the check there.
fn preflight(pid: u64, op: &str, path: &str) -> Option<bool> {
    if !behavior::watching() || !is_data_path(path) {
        return Some(true);
    }
    if behavior::is_contained(pid) {
        return Some(behavior::deny_contained(pid, op, path));
    }
    if is_canary(path) {
        return Some(behavior::alarm(
            pid,
            Detector::Ransomware,
            format!("possible ransomware: canary file {} ({})", path, op),
        ));
    }
    None
}

/// Raises the alarm for a pattern found under the shard lock, outside it.
fn raise(pid: u64, found: Option<(Pattern, String)>) -> bool {
    match found {
        Some((_, what)) => behavior::alarm(pid, Detector::Ransomware, what),
        None => true,
    }
}

/// Write hook: `pid` is about to write `data` to the regular file opened as
/// `path`. `existing_len` answers the file's current size; it is asked at most
/// once per file per window, and never under a hunter lock. Answers whether
/// the write may proceed.
pub fn on_write(pid: u64, path: &str, data: &[u8], existing_len: &mut dyn FnMut() -> u64) -> bool {
    if let Some(v) = preflight(pid, "write", path) {
        return v;
    }
    let key = hash(path);
    let random = looks_random(data);
    let now = clock::now_ns();
    let known = with_activity(pid, now, |a| a.write_seen(key, random));
    if !known {
        let existed = existing_len() > 0;
        let names = Names::of(path);
        with_activity(pid, now, |a| a.write_new(names, existed, random));
    }
    let found = with_activity(pid, now, |a| a.check().map(|p| (p, a.describe(p))));
    raise(pid, found)
}

/// Rename hook: `pid` is about to rename `old` to `new` (absolute paths).
pub fn on_rename(pid: u64, old: &str, new: &str) -> bool {
    if let Some(v) = preflight(pid, "rename", old) {
        return v;
    }
    if is_canary(new) {
        return behavior::alarm(
            pid,
            Detector::Ransomware,
            format!("possible ransomware: canary file {} (rename over)", new),
        );
    }
    let found = with_activity(pid, clock::now_ns(), |a| {
        a.rename(old, new);
        a.check().map(|p| (p, a.describe(p)))
    });
    raise(pid, found)
}

/// Unlink hook: `pid` is about to unlink `path` (absolute).
pub fn on_unlink(pid: u64, path: &str) -> bool {
    if let Some(v) = preflight(pid, "unlink", path) {
        return v;
    }
    let found = with_activity(pid, clock::now_ns(), |a| {
        a.unlink(path);
        a.check().map(|p| (p, a.describe(p)))
    });
    raise(pid, found)
}

/// Drops `pid`'s window when it exits.
pub fn forget(pid: u64) {
    shard(pid).lock().remove(&pid);
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    for s in ACTIVITY.iter() {
        s.0.lock().clear();
    }
    CANARIES.lock().clear();
    CANARY_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{self, Mode};
    use crate::test_globals;

    /// xorshift64*: deterministic stand-in for ciphertext.
    fn random_bytes(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8
            })
            .collect()
    }

    fn text(n: usize) -> Vec<u8> {
        b"Quarterly report: revenue grew 4% while costs held steady. "
            .iter()
            .cycle()
            .take(n)
            .copied()
            .collect()
    }

    #[test]
    fn the_census_tells_ciphertext_from_text_and_code() {
        for seed in 1..200 {
            assert_eq!(
                looks_random(&random_bytes(seed, 4096)),
                Some(true),
                "seed {}",
                seed
            );
        }
        assert_eq!(looks_random(&text(4096)), Some(false));
        // Code-like data: a skewed distribution (opcodes and small immediates
        // repeat, zeros are common), the shape of an ELF's text.
        let code: Vec<u8> = random_bytes(7, 4096)
            .iter()
            .map(|b| if b % 3 == 0 { 0 } else { b % 160 })
            .collect();
        assert_eq!(looks_random(&code), Some(false));
        assert_eq!(looks_random(&random_bytes(1, SAMPLE - 1)), None);
    }

    #[test]
    fn extensions() {
        assert_eq!(extension("/home/a/report.docx"), Some("docx"));
        assert_eq!(extension("/home/a/report.docx.locked"), Some("locked"));
        assert_eq!(extension("/home/a/.bashrc"), None);
        assert_eq!(extension("/home/a.d/Makefile"), None);
        assert_eq!(extension("x.averyveryverylongextension"), None);
    }

    fn names(path: &str) -> Names {
        Names::of(path)
    }

    #[test]
    fn names_link_a_copy_to_its_original() {
        let orig = names("/h/a.docx");
        assert!(orig.is_original_of(&names("/h/a.docx.locked")));
        assert!(orig.is_original_of(&names("/h/a.locked")));
        assert!(orig.is_original_of(&names("/h/a.docx.id-77.[x@y.z].crypt")));
        assert!(names("/h/README").is_original_of(&names("/h/README.enc")));
        assert!(!orig.is_original_of(&orig), "a file is not its own copy");
        assert!(!orig.is_original_of(&names("/h/b.locked")));
        assert!(!orig.is_original_of(&names("/other/a.locked")));
        // dpkg's temporaries are not each other's originals.
        assert!(!names("/u/x.png.dpkg-tmp").is_original_of(&names("/u/x.png.dpkg-new")));
        assert!(!names("/u/x.png.dpkg-new").is_original_of(&names("/u/x.png")));
        // Edge cases do not panic.
        for p in ["", "/", "/h/.", "/h/a.", "/h/.bashrc", "/h/a..b"] {
            let _ = names(p);
        }
    }

    #[test]
    fn encrypt_and_replace_fires_once_per_window() {
        let mut a = Activity::new(0);
        for i in 0..ENCRYPTED_FILES {
            a.write_new(names(&format!("/h/f{}.docx.locked", i)), false, Some(true));
            a.unlink(&format!("/h/f{}.docx", i));
            if i + 1 < ENCRYPTED_FILES {
                assert_eq!(a.check(), None, "fired early at file {}", i);
            }
        }
        assert_eq!(a.check(), Some(Pattern::EncryptAndReplace));
        a.unlink("/h/another.docx");
        assert_eq!(a.check(), None, "once per window");
        a.roll(WINDOW_NS);
        assert_eq!(a.random_files, 0);
    }

    #[test]
    fn the_original_may_go_before_its_copy_is_written() {
        let mut a = Activity::new(0);
        for i in 0..ENCRYPTED_FILES {
            a.unlink(&format!("/h/p{}.jpg", i));
            a.write_new(names(&format!("/h/p{}.enc", i)), false, Some(true));
        }
        assert_eq!(a.check(), Some(Pattern::EncryptAndReplace));
    }

    #[test]
    fn a_package_upgrade_is_quiet() {
        // dpkg unpacking an icon theme: each PNG (random-looking) written as
        // `.dpkg-new`, renamed over the real name, the backup link removed.
        let mut a = Activity::new(0);
        for i in 0..200 {
            let real = format!("/usr/share/icons/i{}.png", i);
            let new = format!("{}.dpkg-new", real);
            a.write_new(names(&new), false, Some(true));
            a.rename(&new, &real);
            a.unlink(&format!("{}.dpkg-tmp", real));
            assert_eq!(a.check(), None, "fired at file {}", i);
            if i % 16 == 15 {
                a.roll(a.window_start + WINDOW_NS);
            }
        }
    }

    #[test]
    fn unpacking_random_files_or_deleting_a_tree_alone_is_quiet() {
        // `tar xf photos.tar`: many new random-looking files, nothing destroyed.
        let mut a = Activity::new(0);
        for i in 0..SEEN_SLOTS {
            a.write_new(names(&format!("/h/IMG_{}.jpg", i)), false, Some(true));
        }
        assert_eq!(a.check(), None);
        // `rm -rf build/`: many unlinks, nothing written.
        let mut b = Activity::new(0);
        for i in 0..10_000 {
            b.unlink(&format!("/h/build/obj{}.o", i));
        }
        assert_eq!(b.check(), None);
    }

    #[test]
    fn overwriting_existing_files_with_random_bytes_fires() {
        let mut a = Activity::new(0);
        for i in 0..ENCRYPTED_FILES {
            // A plain header first, then the ciphertext: judged on the second.
            let n = names(&format!("/h/d{}.xlsx", i));
            a.write_new(n, true, Some(false));
            assert!(a.write_seen(n.key, Some(true)));
        }
        assert_eq!(a.check(), Some(Pattern::EncryptInPlace));
    }

    #[test]
    fn one_file_written_many_times_counts_once() {
        let mut a = Activity::new(0);
        let n = names("/h/big.iso");
        a.write_new(n, true, Some(true));
        for _ in 0..1000 {
            assert!(a.write_seen(n.key, Some(true)));
        }
        assert_eq!(a.random_files, 1);
        assert_eq!(a.check(), None);
    }

    #[test]
    fn mass_rename_needs_one_new_extension_in_a_row() {
        let mut a = Activity::new(0);
        for i in 0..SAME_EXTENSION_RENAMES - 1 {
            a.rename(&format!("/h/f{}.jpg", i), &format!("/h/f{}.jpg.locked", i));
        }
        assert_eq!(a.check(), None);
        a.rename("/h/last.pdf", "/h/last.pdf.locked");
        assert_eq!(a.check(), Some(Pattern::MassRename));

        // An editor's save dance (`x.swp` -> `x`), rsync publishing its
        // temporaries, dpkg's `.dpkg-new`, and renames that keep the
        // extension never build a run.
        let mut b = Activity::new(0);
        for i in 0..100 {
            b.rename(&format!("/h/.f{}.txt.swp", i), &format!("/h/f{}.txt", i));
            b.rename(&format!("/h/.p{}.jpg.Xa9f2Q", i), &format!("/h/p{}.jpg", i));
            b.rename(&format!("/u/m{}.py.dpkg-new", i), &format!("/u/m{}.py", i));
            b.rename(&format!("/h/a{}.txt", i), &format!("/h/b{}.txt", i));
        }
        assert_eq!(b.check(), None);
        // The extension replaced instead of appended is the same pattern.
        let mut c = Activity::new(0);
        for i in 0..SAME_EXTENSION_RENAMES {
            c.rename(&format!("/h/f{}.pdf", i), &format!("/h/f{}.WNCRY", i));
        }
        assert_eq!(c.check(), Some(Pattern::MassRename));
    }

    #[test]
    fn a_frozen_clock_still_rolls_the_window() {
        let mut a = Activity::new(0);
        a.alerted = true;
        a.ops = OPS_BACKSTOP;
        a.roll(0);
        assert!(!a.alerted);
    }

    fn reset() {
        policy::reset_for_test();
        behavior::reset_for_test();
    }

    #[test]
    fn hooks_report_then_contain_under_enforce() {
        let _g = test_globals::lock();
        reset();
        let blob = random_bytes(3, 4096);
        let mut size = || 100u64;
        // Report: the whole run is allowed, one alarm recorded.
        for i in 0..20 {
            let p = format!("/home/u/doc{}.txt", i);
            assert!(on_write(60, &format!("{}.crypt", p), &blob, &mut size));
            assert!(on_unlink(60, &p));
        }
        assert_eq!(behavior::alerts_for_test().0, 1);

        // Enforce: the operation that crosses the line is refused, and so is
        // every later one, even of an unrelated kind.
        behavior::set_mode(Mode::Enforce);
        let mut refused_at = None;
        for i in 0..20 {
            let p = format!("/home/u/pic{}.jpg", i);
            if !on_rename(61, &p, &format!("{}.crypt", p)) {
                refused_at = Some(i);
                break;
            }
        }
        assert_eq!(refused_at, Some(SAME_EXTENSION_RENAMES - 1));
        assert!(behavior::is_contained(61));
        assert!(!on_write(61, "/home/u/other", b"hi", &mut size));
        assert!(!on_unlink(61, "/home/u/other"));
        // Pseudo-files stay writable: containment is about the user's data.
        assert!(on_write(61, "/dev/pts/0", b"hi", &mut size));
        reset();
    }

    #[test]
    fn a_canary_fires_on_the_first_touch() {
        let _g = test_globals::lock();
        reset();
        add_canary(String::from("/home/u/.~budget.xlsx"));
        let mut size = || 0u64;
        assert!(on_write(70, "/home/u/notes.txt", b"x", &mut size));
        assert_eq!(behavior::alerts_for_test().0, 0);
        assert!(on_write(70, "/home/u/.~budget.xlsx", b"x", &mut size));
        assert_eq!(behavior::alerts_for_test().0, 1);
        behavior::set_mode(Mode::Enforce);
        assert!(!on_rename(71, "/home/u/a", "/home/u/.~budget.xlsx"));
        assert!(!on_unlink(72, "/home/u/.~budget.xlsx"));
        reset();
    }

    #[test]
    fn off_means_nothing_is_looked_at() {
        let _g = test_globals::lock();
        reset();
        add_canary(String::from("/c"));
        behavior::set_mode(Mode::Off);
        let mut size = || -> u64 { panic!("asked the inode with the domain off") };
        assert!(on_write(80, "/c", b"x", &mut size));
        assert_eq!(behavior::alerts_for_test().0, 0);
        reset();
    }
}
