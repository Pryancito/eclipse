//! hunter's signature feeds on disk: `/etc/hunter/signatures.d/<feed>` and,
//! for a signed feed, `<feed>.sig` beside it.
//!
//! The kernel does the checking (`hunter::feed`: Ed25519, serial, the feed's
//! name); this file only reads the bytes for it, at boot for every installed
//! feed and at run time for the one `hunterctl update-signatures` (or
//! `feed-rollback`) staged and names in a write to `/proc/hunter_feed`.
//!
//! The kernel reads the feed itself, from the path, instead of being handed
//! the lines by `hunterctl`: the signature covers the file, so the bytes that
//! are verified are the bytes that are loaded, with no userspace step in
//! between that could hand over one thing and have another checked.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use rcore_fs::vfs::{FileType, INode};

pub use hunter::feed::Install;

/// Where installed feeds live.
pub const FEED_DIR: &str = "/etc/hunter/signatures.d";
/// Largest feed read. A signed feed cut short would only fail its SHA-256, so
/// a feed above this is refused outright, said by name.
pub const MAX_FEED_BYTES: usize = 8 << 20;
/// Largest `.sig`: six short lines.
pub const MAX_SIG_BYTES: usize = 4096;

/// The whole of a regular file, or why not. Never a prefix: a feed is
/// verified and loaded as a whole.
pub fn read_file(inode: &dyn INode, max: usize) -> Result<Vec<u8>, String> {
    let meta = inode.metadata().map_err(|_| String::from("cannot stat"))?;
    if meta.type_ != FileType::File {
        return Err(String::from("not a regular file"));
    }
    if meta.size > max {
        return Err(alloc::format!("larger than {} bytes", max));
    }
    let mut buf = vec![0u8; meta.size];
    let mut n = 0;
    while n < buf.len() {
        match inode.read_at(n, &mut buf[n..]) {
            Ok(0) | Err(_) => return Err(String::from("short read")),
            Ok(got) => n += got,
        }
    }
    Ok(buf)
}

/// Reads feed `name` from `body` (and `sig`, if signed) and hands it to
/// `hunter::feed::install`. A file that cannot be read is a rejection like a
/// bad signature: recorded, and nothing installed.
pub fn install_from(
    name: &str,
    body: &dyn INode,
    sig: Option<&dyn INode>,
    how: Install,
    pid: u64,
) -> Result<hunter::feed::Report, String> {
    let read = |inode: &dyn INode, max: usize, what: &str| {
        read_file(inode, max).map_err(|e| alloc::format!("{}: {}", what, e))
    };
    let (body, sig) = match (
        read(body, MAX_FEED_BYTES, "feed"),
        sig.map(|s| read(s, MAX_SIG_BYTES, "signature")).transpose(),
    ) {
        (Ok(b), Ok(s)) => (b, s),
        (Err(e), _) | (_, Err(e)) => return Err(reject(name, &e, pid)),
    };
    let sig = match sig.map(String::from_utf8).transpose() {
        Ok(s) => s,
        Err(_) => return Err(reject(name, "signature: not UTF-8", pid)),
    };
    hunter::feed::install(name, &body, sig.as_deref(), how, pid)
}

/// Records a feed that never reached the verifier, the way the verifier
/// records its own rejections.
fn reject(name: &str, why: &str, pid: u64) -> String {
    hunter::event_log::record(
        pid,
        hunter::Severity::Critical,
        "CONTROL",
        "REJECTED",
        alloc::format!("feed {}: REJECTED: {}; nothing installed", name, why),
    );
    String::from(why)
}

/// Installs every feed in [`FEED_DIR`] at boot, in name order. Hidden files
/// are `hunterctl`'s staging copies and `.sig` files belong to the feed of
/// the same name; neither is a feed. Answers how many signatures came in.
pub fn load_all(root: &Arc<dyn INode>) -> usize {
    let dir = match root.lookup(FEED_DIR) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let mut names = dir.list().unwrap_or_default();
    names.sort();
    let mut total = 0;
    for name in names.iter().filter(|n| hunter::feed::valid_name(n)) {
        let body = match dir.find(name) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let sig = dir.find(&alloc::format!("{}.sig", name)).ok();
        match install_from(name, body.as_ref(), sig.as_deref(), Install::Update, 0) {
            Ok(r) => {
                total += r.total;
                kernel_hal::klog_info!(
                    "hunter: feed {}: {} signatures{}",
                    name,
                    r.total,
                    if sig.is_some() {
                        " (signed, verified)"
                    } else {
                        " (unsigned)"
                    }
                );
            }
            Err(e) => kernel_hal::klog_info!("hunter: feed {} REJECTED: {}", name, e),
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_dir() -> (Arc<dyn INode>, Arc<dyn INode>) {
        let fs = rcore_fs_ramfs::RamFS::new();
        let root = rcore_fs::vfs::FileSystem::root_inode(&*fs);
        core::mem::forget(fs);
        let etc = root.create("etc", FileType::Dir, 0o755).unwrap();
        let hunter = etc.create("hunter", FileType::Dir, 0o755).unwrap();
        let dir = hunter.create("signatures.d", FileType::Dir, 0o755).unwrap();
        (root, dir)
    }

    #[test]
    fn every_feed_loads_and_staging_and_sig_files_are_not_feeds() {
        let (root, dir) = feed_dir();
        dir.create("t-feeds-a", FileType::File, 0o644)
            .unwrap()
            .write_at(0, b"bytes:a7a7a7a7a7a7:Test.Feeds.A\n")
            .unwrap();
        // A half-written download hunterctl had not renamed yet.
        dir.create(".t-feeds-a.new", FileType::File, 0o644)
            .unwrap()
            .write_at(0, b"bytes:a7a7a7a7a7a8:Test.Feeds.Staged\n")
            .unwrap();
        // A broken feed is refused whole, and the others still load.
        dir.create("t-feeds-b", FileType::File, 0o644)
            .unwrap()
            .write_at(0, b"bytes:a7a7a7a7a7a9:Test.Feeds.B\nnonsense\n")
            .unwrap();
        assert_eq!(load_all(&root), 1);
        let report = hunter::feed::render();
        assert!(report.contains("\nfeed t-feeds-a serial=- key=- signatures=1"));
        assert!(!report.contains("t-feeds-b serial"));
        assert!(!report.contains(".t-feeds-a.new"));
    }

    #[test]
    fn a_sig_that_does_not_verify_keeps_the_feed_out() {
        let (root, dir) = feed_dir();
        dir.create("t-feeds-signed", FileType::File, 0o644)
            .unwrap()
            .write_at(0, b"bytes:b7b7b7b7b7b7:Test.Feeds.Signed\n")
            .unwrap();
        dir.create("t-feeds-signed.sig", FileType::File, 0o644)
            .unwrap()
            .write_at(0, b"not a signature\n")
            .unwrap();
        assert_eq!(load_all(&root), 0);
        assert!(!hunter::feed::render().contains("\nfeed t-feeds-signed "));
    }

    #[test]
    fn a_directory_is_not_a_feed() {
        let (_root, dir) = feed_dir();
        let sub = dir.create("t-feeds-dir", FileType::Dir, 0o755).unwrap();
        assert!(install_from("t-feeds-dir", sub.as_ref(), None, Install::Update, 0).is_err());
    }
}
