//! Maps a fully-resolved absolute path (symlinks followed) to its final inode
//! and what that inode is named.
//!
//! Only paths that reach the real-FS branch of `lookup_inode_at_walk` are
//! ever cached — `/proc`, `/sys` and `/dev` are intercepted earlier by
//! `lookup_virtual_fs`. A link on the real FS can still lead into `/proc`
//! (to `/proc/self/stat`, say), or through one of its magic links, and what
//! such a walk finds is another file to each process, so it is not kept: the
//! first process to look used to hand its own to every other.
//!
//! Coherence is a single global epoch: every namespace-mutating operation
//! (unlink/rename/mkdir/mknod/symlink/link, mount/umount, `O_CREAT` opens,
//! unix-socket binds — see `dcache_invalidate` call sites in linux-syscall)
//! bumps it, and a bumped epoch empties the cache on the next touch. Coarse,
//! but mutations are rare next to the lookup storms this exists for (a desktop
//! session start does thousands of absolute open/stat calls over immutable
//! library/config paths), and correctness never depends on fine-grained
//! invalidation.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use hashbrown::HashMap;
use lock::Mutex;
use rcore_fs::vfs::{INode, Walked};

/// Generation counter; see module doc.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// An inode, the root the walk that found it started from, and its name
/// where that is not the path it was looked up by.
///
/// The root is part of what a cached path MEANS: `/etc/passwd` names one
/// inode under one root and another inode under another, so an entry put in
/// from one root is not an answer for a lookup from a different one. The
/// kernel has a single root today, where this never misses. The test binary
/// does not: it gives every process its own `RamFS`, and without the root in
/// the entry one test's absolute `open` was served another test's inode --
/// `/` among them, which is the parent an `O_CREAT` resolves through, so the
/// new file landed in the wrong filesystem and the next open of it came back
/// `ENOENT`. That was one red round in four of `linux-syscall --lib` at 16
/// threads.
struct Entry {
    inode: Arc<dyn INode>,
    /// Held, not compared as a bare address: a freed root could be replaced
    /// by a new one at the same address, and `ptr_eq` would then say yes to
    /// the wrong root.
    root: Arc<dyn INode>,
    name: Option<String>,
}

struct Inner {
    /// Epoch the map contents belong to; a mismatch clears the map.
    epoch: u64,
    map: HashMap<String, Entry>,
}

lazy_static::lazy_static! {
    static ref CACHE: Mutex<Inner> = Mutex::new(Inner {
        epoch: 0,
        map: HashMap::new(),
    });
}

/// Entry bound: pins each cached inode `Arc` in memory, so keep it to the
/// working set of a session start rather than letting it grow unbounded.
/// 4096, not 1024: a desktop session start (ldso across ~50 libraries +
/// xkb + fontconfig + icon themes) touches more than 1024 distinct paths,
/// and hitting the cap drops the WHOLE cache mid-storm — exactly when the
/// hits matter most. An inode Arc is small; 4096 entries stay in the
/// hundreds of KiB. A name is kept only for a path that went through a link
/// or a `.`, `..` or doubled slash, which a library search path seldom does.
const MAX_ENTRIES: usize = 4096;

/// Invalidate every cached path (a namespace mutation happened).
pub fn invalidate() {
    EPOCH.fetch_add(1, Ordering::Release);
}

/// The inode `path` leads to from `root`, with `walked` at it.
pub fn get(root: &Arc<dyn INode>, path: &str, walked: &mut Walked) -> Option<Arc<dyn INode>> {
    let mut c = CACHE.lock();
    let now = EPOCH.load(Ordering::Acquire);
    if c.epoch != now {
        c.map.clear();
        c.epoch = now;
        return None;
    }
    let entry = c.map.get(path)?;
    // Another root's answer to the same path is not this root's.
    if !Arc::ptr_eq(&entry.root, root) {
        return None;
    }
    walked.set(entry.name.as_deref().unwrap_or(path));
    Some(entry.inode.clone())
}

/// Keeps what `path` leads to from `root`, and `walked`, where the walk to
/// it ended, unless it is another file to each process.
pub fn put(root: &Arc<dyn INode>, path: &str, inode: &Arc<dyn INode>, walked: &Walked) {
    if walked.jumped || walked.names.first().is_some_and(|top| top == "proc") {
        return;
    }
    let mut c = CACHE.lock();
    let now = EPOCH.load(Ordering::Acquire);
    if c.epoch != now {
        c.map.clear();
        c.epoch = now;
    }
    if c.map.len() >= MAX_ENTRIES {
        // Full: drop everything rather than tracking recency — refill is
        // one miss per path and the storm patterns are bursty anyway.
        c.map.clear();
    }
    let name = walked.path();
    let name = (name != path).then_some(name);
    c.map.insert(
        String::from(path),
        Entry {
            inode: inode.clone(),
            root: root.clone(),
            name,
        },
    );
}

#[cfg(test)]
mod epoch_tests;
