//! Boot-time loader for hunter's `/etc/hunter/` policy files.
//!
//! The kernel reads (never writes) two optional newline-delimited lists from
//! the root filesystem at boot:
//!
//! * `/etc/hunter/whitelist` — trusted programs that may always run.
//! * `/etc/hunter/blacklist` — denied programs that must never run.
//!
//! Each non-empty, non-`#` line is one entry. A trailing `/` makes it a
//! directory prefix (everything beneath it); otherwise it is an exact program
//! path. Missing files are fine — the lists simply stay empty.
//!
//! Learned programs (trust-on-first-use) live in kernel memory and are surfaced
//! at `/proc/hunter`; a userspace helper is expected to append them back to
//! `/etc/hunter/whitelist`. The kernel deliberately does not write the FS.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use rcore_fs::vfs::INode;

/// Largest config file we will read (1 MiB), bounding boot-time memory.
const MAX_CONFIG_BYTES: usize = 1 << 20;

/// Loads `/etc/hunter/{whitelist,blacklist}` from `root` into hunter's policy
/// and enables exec learning so safe programs are auto-trusted without denial.
pub fn load(root: &Arc<dyn INode>) {
    // Learning on by default: a "whitelist that never denies" which builds
    // itself from observed-safe execs. The blacklist remains the only deny.
    hunter::policy::set_exec_learning(true);
    let n_white = load_list(root, "/etc/hunter/whitelist", false);
    let n_black = load_list(root, "/etc/hunter/blacklist", true);
    kernel_hal::klog_info!(
        "hunter: loaded /etc/hunter (whitelist={}, blacklist={}, learning=on)",
        n_white,
        n_black
    );
}

/// Reads one list file and registers its entries. Returns how many were added.
/// A missing file yields `0` without error.
///
/// All three ways this used to give up failed **open**, which for a deny list is
/// the wrong direction, and all three were silent: the `klog` line says
/// `blacklist=0`, which reads as "no list configured" and not as "your list was
/// thrown away".
///
/// * `from_utf8` ran over the **whole** buffer, so one byte that is not UTF-8
///   anywhere in the file discarded every entry in it — an accented path in
///   latin-1 was enough. Each line is decoded on its own now, and only the line
///   that will not decode is dropped, by name in the log.
/// * A single `read_at` was taken for the whole file, so a short read lost the
///   tail without a word: the last programs in the deny list simply were not in
///   it. It reads in a loop now.
/// * The file is cut at [`MAX_CONFIG_BYTES`], and the cut can land in the
///   middle of a line. Half a path is not a shorter path, it is a **different**
///   one: `/usr/bin/evilprog` cut to `/usr/bin/evil` left the real program
///   unlisted, and a cut that lands just after a `/` turns the entry into a
///   directory prefix — `/usr/bin/foo` cut to `/usr/` in the whitelist trusts
///   the whole tree. A truncated file drops its last, incomplete line.
fn load_list(root: &Arc<dyn INode>, path: &str, blacklist: bool) -> usize {
    let inode = match root.lookup(path) {
        Ok(i) => i,
        Err(_) => return 0,
    };
    let full_size = match inode.metadata() {
        Ok(m) => m.size,
        Err(_) => return 0,
    };
    let size = full_size.min(MAX_CONFIG_BYTES);
    if size == 0 {
        return 0;
    }
    let mut buf = vec![0u8; size];
    let n = read_all(inode.as_ref(), &mut buf);
    register_entries(
        &buf[..n],
        full_size > MAX_CONFIG_BYTES || n < full_size,
        path,
        blacklist,
    )
}

/// Fills `buf` from the start of `inode`, and answers how many bytes it got.
///
/// One `read_at` was taken for the whole file. A short read is a normal answer
/// --- nothing in the `vfs` trait promises otherwise --- and it lost the tail of
/// the policy file without a word, which for a deny list means the last programs
/// in it were simply not denied.
fn read_all(inode: &dyn INode, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match inode.read_at(n, &mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(got) => n += got,
        }
    }
    n
}

/// Registers the entries of one already-read policy file.
///
/// `cut` says the bytes are not all of it (the file was longer than
/// [`MAX_CONFIG_BYTES`], or the read stopped short), in which case the last line
/// has no terminator of its own and is not known to be whole.
fn register_entries(bytes: &[u8], cut: bool, path: &str, blacklist: bool) -> usize {
    let mut lines: alloc::vec::Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    // `split` always yields a last element; when the bytes end in a newline it
    // is empty, and then nothing was cut in half whatever `cut` says.
    if cut && lines.last().map(|l| !l.is_empty()).unwrap_or(false) {
        kernel_hal::klog_info!(
            "hunter: {} is longer than {} bytes; dropping its last, incomplete line",
            path,
            MAX_CONFIG_BYTES
        );
        lines.pop();
    }
    let mut count = 0;
    for raw in lines {
        let line = match core::str::from_utf8(raw) {
            Ok(t) => t.trim(),
            Err(_) => {
                // Not the whole file: just this line, and said out loud.
                kernel_hal::klog_info!("hunter: {}: skipping a line that is not UTF-8", path);
                continue;
            }
        };
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let is_prefix = line.ends_with('/');
        let entry = String::from(line);
        match (blacklist, is_prefix) {
            (false, false) => hunter::policy::add_trusted_exec_path(entry),
            (false, true) => hunter::policy::add_trusted_exec_prefix(entry),
            (true, false) => hunter::policy::add_blacklisted_exec_path(entry),
            (true, true) => hunter::policy::add_blacklisted_exec_prefix(entry),
        }
        count += 1;
    }
    count
}

#[cfg(test)]
mod hunter_config_tests {
    //! `load_list` gave up in three ways and every one of them failed **open**:
    //! it threw away the whole file over one byte that is not UTF-8, it took a
    //! short read for the whole file, and it registered the half of a line that
    //! the 1 MiB cut left behind.
    //!
    //! hunter's policy lives in process-wide globals and its reset is not
    //! visible from here, so every test below uses a path prefix of its own and
    //! asks only about its own entries: the lists are additive and nothing a
    //! neighbouring test adds can make one of these pass or fail.

    use super::*;

    /// A line that is not UTF-8: a lone `0xff`, which is what a latin-1
    /// accented path looks like to `from_utf8`.
    const BAD: &[u8] = b"/usr/bin/caf\xff";

    fn bytes(lines: &[&[u8]]) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec::Vec::new();
        for l in lines {
            v.extend_from_slice(l);
            v.push(b'\n');
        }
        v
    }

    #[test]
    fn one_byte_that_is_not_utf8_no_longer_throws_away_the_whole_list() {
        // This is the one that matters: with `from_utf8` over the whole buffer
        // the deny list came out EMPTY, and the boot log said `blacklist=0`,
        // which reads as "none configured".
        let file = bytes(&[b"/a1/denied-one", BAD, b"/a1/denied-two"]);
        let added = register_entries(&file, false, "/etc/hunter/blacklist", true);
        assert_eq!(added, 2, "the two good lines, and only the bad one dropped");
        assert!(hunter::policy::is_exec_blacklisted("/a1/denied-one"));
        assert!(
            hunter::policy::is_exec_blacklisted("/a1/denied-two"),
            "the line AFTER the bad byte is the one that used to disappear"
        );
    }

    #[test]
    fn a_cut_file_drops_its_last_incomplete_line() {
        // `/a2/evilprog` cut to `/a2/evil` is not a shorter path, it is a
        // different one: the real program ends up unlisted and a program that
        // does not exist gets denied instead.
        let mut file = bytes(&[b"/a2/whole"]);
        file.extend_from_slice(b"/a2/evil");
        let added = register_entries(&file, true, "/etc/hunter/blacklist", true);
        assert_eq!(added, 1);
        assert!(hunter::policy::is_exec_blacklisted("/a2/whole"));
        assert!(
            !hunter::policy::is_exec_blacklisted("/a2/evil"),
            "half a path was registered as an entry of its own"
        );
    }

    #[test]
    fn a_cut_that_lands_after_a_slash_does_not_become_a_directory_prefix() {
        // The worst shape of the same cut, and it is the whitelist that pays:
        // `/a3/bin/tool` cut to `/a3/` ends with a slash, so it used to be read
        // as a directory prefix and trusted the entire tree.
        let mut file = bytes(&[b"/a3/kept"]);
        file.extend_from_slice(b"/a3/");
        assert_eq!(
            register_entries(&file, true, "/etc/hunter/whitelist", false),
            1
        );
        assert!(hunter::policy::is_exec_listed("/a3/kept"));
        assert!(
            !hunter::policy::is_exec_listed("/a3/anything-at-all"),
            "the cut turned one entry into trust for a whole directory"
        );
    }

    #[test]
    fn a_whole_file_is_not_treated_as_cut_and_keeps_its_last_line() {
        // `cut` is about the bytes, not about the file: a file that ends in a
        // newline has no half line whatever the caller says, and one that is
        // not cut keeps its last line even without a terminator.
        let ending_in_newline = bytes(&[b"/a4/first", b"/a4/last"]);
        assert_eq!(
            register_entries(&ending_in_newline, true, "/etc/hunter/blacklist", true),
            2,
            "a trailing newline means nothing was cut in half"
        );
        assert!(hunter::policy::is_exec_blacklisted("/a4/last"));

        let no_terminator = alloc::vec::Vec::from(&b"/a5/only"[..]);
        assert_eq!(
            register_entries(&no_terminator, false, "/etc/hunter/blacklist", true),
            1
        );
        assert!(hunter::policy::is_exec_blacklisted("/a5/only"));
    }

    #[test]
    fn comments_blank_lines_and_prefixes_still_read_the_same() {
        let file = bytes(&[
            b"# a comment",
            b"",
            b"   ",
            b"  /a6/spaced  ",
            b"/a6/tree/",
            b"\t# indented comment",
        ]);
        assert_eq!(
            register_entries(&file, false, "/etc/hunter/whitelist", false),
            2
        );
        assert!(hunter::policy::is_exec_listed("/a6/spaced"));
        assert!(hunter::policy::is_exec_listed("/a6/tree/anything"));
        assert!(!hunter::policy::is_exec_listed("/a6/a-comment"));
    }

    /// An inode that answers every read with at most `chunk` bytes, which is
    /// what the old single `read_at` took for the whole file.
    struct ShortReader {
        content: alloc::vec::Vec<u8>,
        chunk: usize,
    }

    impl rcore_fs::vfs::INode for ShortReader {
        fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
            if offset >= self.content.len() {
                return Ok(0);
            }
            let len = (self.content.len() - offset).min(buf.len()).min(self.chunk);
            buf[..len].copy_from_slice(&self.content[offset..offset + len]);
            Ok(len)
        }
        fn write_at(&self, _offset: usize, _buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
            Err(rcore_fs::vfs::FsError::NotSupported)
        }
        fn poll(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::PollStatus> {
            Err(rcore_fs::vfs::FsError::NotSupported)
        }
        fn metadata(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::Metadata> {
            Err(rcore_fs::vfs::FsError::NotSupported)
        }
        fn as_any_ref(&self) -> &dyn core::any::Any {
            self
        }
    }

    #[test]
    fn a_short_read_no_longer_loses_the_tail_of_the_file() {
        // Seven bytes at a time over a file of three entries: the old code kept
        // whatever the first read happened to give and dropped the rest.
        let content = bytes(&[b"/a7/one", b"/a7/two", b"/a7/three"]);
        let dev = ShortReader {
            content: content.clone(),
            chunk: 7,
        };
        let mut buf = alloc::vec::Vec::from(&[0u8][..]).repeat(content.len());
        assert_eq!(read_all(&dev, &mut buf), content.len());
        assert_eq!(buf, content);

        assert_eq!(
            register_entries(&buf, false, "/etc/hunter/blacklist", true),
            3
        );
        assert!(hunter::policy::is_exec_blacklisted("/a7/three"));
    }

    #[test]
    fn a_reader_that_stops_early_is_reported_as_short_and_not_as_whole() {
        // `read_all` answering less than the buffer is what tells `load_list`
        // the bytes are cut, so the half line at the end is dropped rather than
        // registered.
        let dev = ShortReader {
            content: alloc::vec::Vec::from(&b"/a8/kept\n/a8/half"[..]),
            chunk: 64,
        };
        let mut buf = [0u8; 40];
        let n = read_all(&dev, &mut buf);
        assert_eq!(n, 17, "the file is shorter than the buffer");
        assert!(n < buf.len(), "which is how the caller learns it is cut");
        assert_eq!(
            register_entries(&buf[..n], true, "/etc/hunter/blacklist", true),
            1
        );
        assert!(!hunter::policy::is_exec_blacklisted("/a8/half"));
    }
}
