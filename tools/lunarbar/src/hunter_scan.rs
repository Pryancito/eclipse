//! `lunarhunter`'s scanner: walk a tree and feed it to hunter's antivirus.
//!
//! The kernel does the scanning. `/proc/hunter_scan` takes paths, one per
//! line, scans each batch before the `write(2)` returns, and answers in
//! `clamscan` style (`path: OK`, `path: <Name> FOUND`, `path: ERROR <why>`).
//! That is the interface `hunterctl scan` drives from a shell; this drives the
//! same one from a thread, so the window can show the count going up and stop
//! halfway, which a terminal running `find | dd` cannot.
//!
//! What is here is the pure part plus one function that touches the kernel
//! ([`kernel_batch`]); [`run`] takes the batch function as an argument so the
//! tests walk a real temporary tree against a fake kernel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// hunter's scanner.
pub const PROC: &str = "/proc/hunter_scan";

/// Paths per write, the same batch `hunterctl` uses: each batch is scanned
/// before the write returns, so this is also how often the count moves.
pub const BATCH: usize = 256;

/// Never walked into: pseudo-filesystems whose "files" are the kernel's own
/// state (reading some of them blocks, and none of them is a program anyone
/// downloaded), and the quarantine, whose files are known-bad by definition
/// and mode 000.
pub const SKIP: [&str; 5] = [
    "/proc",
    "/sys",
    "/dev",
    "/run",
    crate::hunter_report::QUARANTINE_DIR,
];

/// A tree bigger than this is cut, with a note. A scan of `/` on a full
/// image is a few hundred thousand files; the list of paths has to fit in
/// memory before the first batch goes in.
pub const MAX_FILES: usize = 2_000_000;

/// What one result line says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    Found(String),
    Error(String),
}

/// One result line, parsed.
pub fn parse_line(line: &str) -> Option<(String, Verdict)> {
    if let Some(p) = line.strip_suffix(": OK") {
        return Some((p.to_string(), Verdict::Clean));
    }
    if let Some(rest) = line.strip_suffix(" FOUND") {
        // The name never has a space (`hunterctl` reads it as `[^ ]*`); the
        // path may have ": " in it, so the split is the LAST one.
        let (p, name) = rest.rsplit_once(": ")?;
        return Some((p.to_string(), Verdict::Found(name.to_string())));
    }
    let i = line.rfind(": ERROR ")?;
    Some((
        line[..i].to_string(),
        Verdict::Error(line[i + 8..].to_string()),
    ))
}

/// Something the scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    pub name: String,
}

/// Where a running scan is, shared between the scanning thread and the window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Still listing the tree: `total` is not known yet.
    pub walking: bool,
    pub total: usize,
    pub done: usize,
    pub found: Vec<Finding>,
    pub errors: usize,
    /// The first few unreadable paths, for the window to show.
    pub error_paths: Vec<String>,
    pub finished: bool,
    pub cancelled: bool,
    /// Why the scan stopped early (no scanner, not root, the tree was cut).
    pub failure: Option<String>,
}

fn skipped(path: &str, skip: &[&str]) -> bool {
    skip.iter()
        .any(|s| path == *s || path.strip_prefix(s).is_some_and(|r| r.starts_with('/')))
}

/// Every regular file under `roots`, in walk order, not following symlinks
/// (a link into `/proc` would undo [`SKIP`], and a link loop would never end).
/// A root that is itself a file is listed as it is. Stops at `max` files or
/// when `cancel` is set; returns whether the list is complete.
pub fn walk(
    roots: &[String],
    skip: &[&str],
    max: usize,
    cancel: &AtomicBool,
    out: &mut Vec<String>,
) -> bool {
    let mut stack: Vec<String> = roots.iter().rev().cloned().collect();
    while let Some(path) = stack.pop() {
        if cancel.load(Ordering::Relaxed) || out.len() >= max {
            return false;
        }
        if skipped(&path, skip) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_file() {
            out.push(path);
        } else if meta.is_dir() {
            let Ok(rd) = std::fs::read_dir(&path) else {
                continue;
            };
            let mut kids: Vec<String> = rd
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .map(|n| {
                    if path.ends_with('/') {
                        format!("{path}{n}")
                    } else {
                        format!("{path}/{n}")
                    }
                })
                .collect();
            // Sorted, then pushed in reverse so they come off the stack in
            // order: a scan that lists `/bin/a` before `/bin/b` reads the way
            // `ls` does, and the tests can name what they expect.
            kids.sort();
            stack.extend(kids.into_iter().rev());
        }
    }
    true
}

/// The scanners' lock: `/proc/hunter_scan` keeps ONE batch of results, the
/// last write's, so a scan from this window and a scheduled `hunterctl scan`
/// at the same time would read each other's verdicts -- a FOUND counted
/// against the wrong file, or lost. Whoever writes to it holds this
/// directory meanwhile (`mkdir` is atomic), with its pid inside so one left by
/// a crash is taken over once that pid is gone. The same protocol as
/// `hunterctl`'s `scan_lock`, which is the point.
pub const LOCK: &str = "/run/hunter/scan.lock";

/// How long to wait for the lock: a scan of the whole disk.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The lock, held until dropped.
pub struct ScanLock(std::path::PathBuf);

impl ScanLock {
    pub fn take(dir: &std::path::Path) -> std::io::Result<ScanLock> {
        if let Some(parent) = dir.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let start = std::time::Instant::now();
        loop {
            match std::fs::create_dir(dir) {
                Ok(()) => {
                    let _ = std::fs::write(dir.join("pid"), format!("{}\n", std::process::id()));
                    return Ok(ScanLock(dir.to_path_buf()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let owner: Option<u32> = std::fs::read_to_string(dir.join("pid"))
                        .ok()
                        .and_then(|s| s.trim().parse().ok());
                    // `kill -0`: is the owner still there?
                    let alive = match owner {
                        Some(pid) => {
                            let r = unsafe { libc::kill(pid as i32, 0) };
                            r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
                        }
                        None => true,
                    };
                    if !alive {
                        let _ = std::fs::remove_dir_all(dir);
                        continue;
                    }
                    if start.elapsed() > LOCK_WAIT {
                        return Err(std::io::Error::other(format!(
                            "another scan has held {} for 10 minutes (pid {})",
                            dir.display(),
                            owner.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for ScanLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Send one batch to the kernel's scanner and read its verdicts back. ONE
/// write: the kernel takes each write as one batch. The lock is held per
/// batch, so two scanners take turns instead of one waiting for the whole of
/// the other.
pub fn kernel_batch(paths: &[String]) -> std::io::Result<String> {
    use std::io::{Read, Write};
    let mut text = paths.join("\n");
    text.push('\n');
    let _lock = ScanLock::take(std::path::Path::new(LOCK))?;
    {
        let mut f = std::fs::OpenOptions::new().write(true).open(PROC)?;
        let n = f.write(text.as_bytes())?;
        if n != text.len() {
            return Err(std::io::Error::other("short write to /proc/hunter_scan"));
        }
    }
    let mut out = String::new();
    std::fs::File::open(PROC)?.read_to_string(&mut out)?;
    Ok(out)
}

/// Keep at most this many unreadable paths to show.
const ERROR_PATHS: usize = 8;

/// Scan `roots`: walk, then batch after batch through `batch`, publishing into
/// `prog` as it goes. Returns when done, failed or cancelled; `prog.finished`
/// is set last, whatever the outcome.
pub fn run(
    roots: &[String],
    prog: &Mutex<Progress>,
    cancel: &AtomicBool,
    mut batch: impl FnMut(&[String]) -> std::io::Result<String>,
) {
    let set = |f: &mut dyn FnMut(&mut Progress)| {
        let mut p = prog.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut p);
    };
    set(&mut |p| {
        *p = Progress {
            walking: true,
            ..Progress::default()
        }
    });
    let mut files = Vec::new();
    let complete = walk(roots, &SKIP, MAX_FILES, cancel, &mut files);
    let total = files.len();
    set(&mut |p| {
        p.walking = false;
        p.total = total;
        if !complete && !cancel.load(Ordering::Relaxed) {
            p.failure = Some(format!(
                "more than {MAX_FILES} files: the rest were not scanned"
            ));
        }
    });
    for chunk in files.chunks(BATCH) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match batch(chunk) {
            Ok(text) => {
                let verdicts: Vec<_> = text.lines().filter_map(parse_line).collect();
                set(&mut |p| {
                    p.done += chunk.len();
                    for (path, v) in &verdicts {
                        match v {
                            Verdict::Clean => {}
                            Verdict::Found(name) => p.found.push(Finding {
                                path: path.clone(),
                                name: name.clone(),
                            }),
                            Verdict::Error(_) => {
                                p.errors += 1;
                                if p.error_paths.len() < ERROR_PATHS {
                                    p.error_paths.push(path.clone());
                                }
                            }
                        }
                    }
                });
            }
            Err(e) => {
                let why = match e.raw_os_error() {
                    Some(libc::EACCES) | Some(libc::EPERM) => "root required".to_string(),
                    Some(libc::ENOENT) => format!("{PROC} not found: no antivirus in this kernel"),
                    _ => e.to_string(),
                };
                set(&mut |p| p.failure = Some(why.clone()));
                break;
            }
        }
    }
    let cancelled = cancel.load(Ordering::Relaxed);
    set(&mut |p| {
        p.cancelled = cancelled;
        p.finished = true;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scan_lock_is_exclusive_and_a_dead_owners_lock_is_taken_over() {
        let dir = std::env::temp_dir().join(format!("lunarhunter-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("scan.lock");
        // Left by a process that is gone (no pid is ever that large).
        std::fs::create_dir(&lock).unwrap();
        std::fs::write(lock.join("pid"), "4194304\n").unwrap();
        let held = ScanLock::take(&lock).expect("a dead owner's lock is taken over");
        assert_eq!(
            std::fs::read_to_string(lock.join("pid")).unwrap().trim(),
            std::process::id().to_string()
        );
        // Held by us (alive): a second taker waits until we drop it.
        let l2 = lock.clone();
        let t = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let _l = ScanLock::take(&l2).unwrap();
            started.elapsed()
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(held);
        let waited = t.join().unwrap();
        assert!(waited >= std::time::Duration::from_millis(400), "{waited:?}");
        assert!(!lock.exists(), "released when dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn result_lines_parse_in_all_three_shapes() {
        assert_eq!(
            parse_line("/bin/ls: OK"),
            Some(("/bin/ls".into(), Verdict::Clean))
        );
        assert_eq!(
            parse_line("/tmp/a: b.com: EICAR-Test-File FOUND"),
            Some((
                "/tmp/a: b.com".into(),
                Verdict::Found("EICAR-Test-File".into())
            ))
        );
        assert_eq!(
            parse_line("/nope: ERROR cannot open"),
            Some(("/nope".into(), Verdict::Error("cannot open".into())))
        );
        assert_eq!(parse_line("garbage"), None);
    }

    #[test]
    fn the_skip_list_matches_whole_components_only() {
        assert!(skipped("/proc", &SKIP));
        assert!(skipped("/proc/1/maps", &SKIP));
        assert!(!skipped("/processes", &SKIP));
        assert!(skipped("/var/lib/hunter/quarantine/x", &SKIP));
    }

    /// A made-up "malware" sample for the fake kernel below to find.
    const SAMPLE: &str = "lunarhunter test sample: harmless bytes";

    /// A small tree with one sample in it, its own per test (they run in
    /// threads).
    fn tree(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lunarhunter-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub/deeper")).unwrap();
        std::fs::write(d.join("a"), "clean").unwrap();
        std::fs::write(d.join("sub/sample"), SAMPLE).unwrap();
        std::fs::write(d.join("sub/deeper/z"), "clean").unwrap();
        // A link loop: never followed.
        let _ = std::os::unix::fs::symlink(&d, d.join("sub/loop"));
        d
    }

    #[test]
    fn the_walk_lists_files_in_order_and_does_not_follow_links() {
        let d = tree("walk");
        let root = d.to_string_lossy().to_string();
        let mut out = Vec::new();
        assert!(walk(
            &[root.clone()],
            &[],
            100,
            &AtomicBool::new(false),
            &mut out
        ));
        let rel: Vec<_> = out.iter().map(|p| p[root.len()..].to_string()).collect();
        assert_eq!(rel, ["/a", "/sub/deeper/z", "/sub/sample"]);
        // Cut at the cap, and says so.
        let mut few = Vec::new();
        assert!(!walk(&[root], &[], 2, &AtomicBool::new(false), &mut few));
        assert_eq!(few.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_scan_publishes_findings_errors_and_its_end() {
        let d = tree("scan");
        let root = d.to_string_lossy().to_string();
        let prog = Mutex::new(Progress::default());
        // The fake kernel: the sample is found, `z` cannot be read.
        run(&[root], &prog, &AtomicBool::new(false), |paths| {
            Ok(paths
                .iter()
                .map(|p| {
                    let body = std::fs::read_to_string(p).unwrap_or_default();
                    if body == SAMPLE {
                        format!("{p}: Sample FOUND\n")
                    } else if p.ends_with("/z") {
                        format!("{p}: ERROR read failed\n")
                    } else {
                        format!("{p}: OK\n")
                    }
                })
                .collect())
        });
        let p = prog.into_inner().unwrap();
        assert!(p.finished && !p.cancelled && p.failure.is_none());
        assert_eq!((p.total, p.done, p.errors), (3, 3, 1));
        assert_eq!(p.found.len(), 1);
        assert!(p.found[0].path.ends_with("/sub/sample"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_kernel_that_refuses_stops_the_scan_with_a_reason() {
        let d = tree("refuse");
        let prog = Mutex::new(Progress::default());
        run(
            &[d.to_string_lossy().to_string()],
            &prog,
            &AtomicBool::new(false),
            |_| Err(std::io::Error::from_raw_os_error(libc::EACCES)),
        );
        let p = prog.into_inner().unwrap();
        assert!(p.finished);
        assert_eq!(p.done, 0);
        assert_eq!(p.failure.as_deref(), Some("root required"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_cancelled_scan_ends_marked_cancelled() {
        let prog = Mutex::new(Progress::default());
        run(
            &["/".into()],
            &prog,
            &AtomicBool::new(true),
            |_| unreachable!(),
        );
        let p = prog.into_inner().unwrap();
        assert!(p.finished && p.cancelled);
        assert_eq!(p.done, 0);
    }
}
