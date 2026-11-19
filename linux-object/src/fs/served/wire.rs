//! What passes between the kernel and the process that serves a filesystem:
//! the 32-byte message of the FIFO, how the window is cut, and the records
//! of a directory listing. `tools/eclipse-fatfs` has the same layout on the
//! other side.
//!
//! A request is one message; its names are at the start of the request's
//! slot of the window, and what goes with it -- the bytes to write, or what
//! the server answers with -- is in the slot's data area. The reply is the
//! same message with `status` filled in: a count, or a Linux `errno`,
//! negated.

use alloc::string::String;
use alloc::vec::Vec;
use rcore_fs::vfs::FsError;

/// One FIFO element: `op, reqid` (u32 each), `a` (u64), `b, names` (u32
/// each), `status` (i64).
pub const MSG: usize = 32;

/// The start of each slot: the request's path, `names` bytes of it.
pub const NAME_AREA: usize = 4096;
/// The rest of the slot: what the server read, or listed.
pub const DATA_AREA: usize = 64 * 1024;
/// A slot of the window, one request in flight on each.
pub const SLOT_LEN: usize = NAME_AREA + DATA_AREA;
/// The most slots a window is cut into, whatever its length.
pub const SLOTS: usize = 16;

/// The entries of the directory at the path, from entry `a` on, as records
/// in the data area. The reply: `status` records, `b` bytes of them, and in
/// `a` the entry the next request starts at, [`LIST_END`] after the last.
pub const OP_LIST: u32 = 1;
/// Up to `b` bytes of the file at the path, from byte `a`, into the data
/// area. The reply: `status` bytes, fewer than asked only at the end of the
/// file.
pub const OP_READ: u32 = 2;
/// The filesystem's numbers, as [`STATFS_LEN`] bytes in the data area: block
/// size, blocks, free blocks, longest name and `statfs`'s magic (u64 each).
pub const OP_STATFS: u32 = 3;

/// Write the `b` bytes of the data area into the file at the path, from
/// byte `a`; a file that ends before `a` is zeros up to it. The reply:
/// `status` bytes written, fewer than `b` when the volume filled up.
pub const OP_WRITE: u32 = 4;
/// Make the file at the path, or with [`MODE_DIR`] in `a` the directory.
/// The reply: its record in the data area, `b` bytes.
pub const OP_CREATE: u32 = 5;
/// Remove the file at the path, or the directory, which must be empty.
pub const OP_REMOVE: u32 = 6;
/// Two paths, a NUL between them: the first becomes the second, in place
/// of what was there (a file, or an empty directory). The reply: the
/// record of the entry under its new name.
pub const OP_RENAME: u32 = 7;
/// Make the file at the path `a` bytes long: cut, or grown with zeros.
pub const OP_TRUNCATE: u32 = 8;
/// What was written reaches the device, and the volume is left as an
/// unmount would leave it.
pub const OP_SYNC: u32 = 9;
/// The file at the path was last written at `a`, seconds as an `i64`.
pub const OP_SETTIME: u32 = 10;
/// Only on a [`SERVE_POSIX`](super::SERVE_POSIX) mount: the file at the
/// path belongs to the user `a >> 32` and the group `a & 0xffff_ffff`,
/// with the permissions `b` (the low twelve bits of a mode).
pub const OP_SETATTR: u32 = 11;
/// Only on a [`SERVE_POSIX`](super::SERVE_POSIX) mount: the record of the
/// file at the path itself, in the data area. What the top of the mount is
/// asked, which no listing has.
pub const OP_STAT: u32 = 12;

/// Only on a [`SERVE_EXTRAS`](super::SERVE_EXTRAS) mount: `ioctl(2)` on
/// the file at the path. The names: that path, then for each open file
/// the argument names a NUL, one byte of [`NAMED_BASE`] and its `NAMED_*`
/// bits, and its path (none when it is on another filesystem); the
/// argument has each descriptor in it replaced by the number of its name
/// among those, from 0, or by -1 for one that is no open file. The data
/// area: [`IOCTL_HEAD`] bytes (`cmd`, `euid`, `FACT_*` bits, and the bytes
/// of the argument, u32 each), then the argument. The reply: `status` is
/// what the ioctl returns, the argument as it goes back is in the data
/// area (`b` bytes), and `a` has the `CHANGED_*` bits of what the kernel
/// keeps that it made untrue.
pub const OP_IOCTL: u32 = 13;
/// Only on a [`SERVE_EXTRAS`](super::SERVE_EXTRAS) mount: an extended
/// attribute of the file at the path. The names: the path, a NUL, the
/// attribute's name (none to list them). `a`: an `XATTR_*` request, the
/// `setxattr` flags shifted by 8, `FACT_*` bits shifted by 16 and the
/// caller's `euid` shifted by 32; `b`: the bytes of the value to set, in
/// the data area. The reply: in
/// the data area the value, or the names each with a NUL after it, and
/// `status` of their bytes.
pub const OP_XATTR: u32 = 14;
/// Only on a [`SERVE_EXTRAS`](super::SERVE_EXTRAS) mount: `fallocate(2)`
/// of the file at the path from byte `a`, mode `b`, the length a u64 in
/// the data area.
pub const OP_FALLOCATE: u32 = 15;
/// Only on a [`SERVE_EXTRAS`](super::SERVE_EXTRAS) mount: `lseek(2)` of
/// the file at the path to `SEEK_DATA` or `SEEK_HOLE` (`b`) from `a` (an
/// `i64`). The reply: `status` is the new position.
pub const OP_SEEK: u32 = 16;

/// [`OP_IOCTL`]'s fixed part of its data area.
pub const IOCTL_HEAD: usize = 16;
/// What the caller has and the file it asks about is (`FACT_*` bits):
/// `CAP_FOWNER`, `CAP_LINUX_IMMUTABLE` and `CAP_SYS_ADMIN`; the file open
/// for reading, for writing, for appending; and on a read-only mount.
pub const FACT_FOWNER: u32 = 1 << 0;
pub const FACT_IMMUTABLE: u32 = 1 << 1;
pub const FACT_ADMIN: u32 = 1 << 2;
pub const FACT_READABLE: u32 = 1 << 3;
pub const FACT_WRITABLE: u32 = 1 << 4;
pub const FACT_APPEND: u32 = 1 << 5;
pub const FACT_READ_ONLY: u32 = 1 << 6;
/// The byte before an open file's path among [`OP_IOCTL`]'s names, with
/// its bits: open for reading, writing, appending; on another filesystem.
pub const NAMED_BASE: u8 = 0x40;
pub const NAMED_READABLE: u8 = 1 << 0;
pub const NAMED_WRITABLE: u8 = 1 << 1;
pub const NAMED_APPEND: u8 = 1 << 2;
pub const NAMED_ELSEWHERE: u8 = 1 << 3;
/// [`OP_IOCTL`]'s reply: the file's bytes or size changed; the entries of
/// the directory it is; every directory's names may read otherwise; the
/// bytes of a file the argument named changed.
pub const CHANGED_BYTES: u64 = 1 << 0;
pub const CHANGED_DIR: u64 = 1 << 1;
pub const CHANGED_ALL_DIRS: u64 = 1 << 2;
pub const CHANGED_NAMED: u64 = 1 << 3;
/// [`OP_XATTR`]'s requests.
pub const XATTR_GET: u64 = 0;
pub const XATTR_SET: u64 = 1;
pub const XATTR_LIST: u64 = 2;
pub const XATTR_REMOVE: u64 = 3;

/// [`OP_LIST`]'s `a` in a reply that holds the directory's last entries.
pub const LIST_END: u64 = u64::MAX;
/// The bytes of an [`OP_STATFS`] reply.
pub const STATFS_LEN: usize = 40;

/// A listing record's fixed part: `rec_len, name_len` (u16 each), `mode`
/// (u32), `size` (u64), `mtime, atime, ctime` (i64 each, seconds), then the
/// name. `rec_len` is the whole record, a multiple of 8.
pub const RECORD_HEAD: usize = 40;
/// `mode`'s bit for a directory: `S_IFDIR`. Anything else is a file.
pub const MODE_DIR: u32 = 0o040000;

/// The kind of file of a mode: its `S_IFMT` bits.
pub const MODE_KIND: u32 = 0o170000;
pub const MODE_FILE: u32 = 0o100000;
pub const MODE_LINK: u32 = 0o120000;
pub const MODE_FIFO: u32 = 0o010000;
pub const MODE_SOCKET: u32 = 0o140000;
pub const MODE_CHAR: u32 = 0o020000;
pub const MODE_BLOCK: u32 = 0o060000;

/// A record's fixed part on a [`SERVE_POSIX`](super::SERVE_POSIX) mount:
/// the [`RECORD_HEAD`] bytes, then `uid, gid, nlink` (u32 each), four bytes
/// of nothing, and `rdev` (u64), then the name. `mode` is the whole of the
/// file's mode there: its kind and its permissions.
pub const RECORD_HEAD_POSIX: usize = 64;

/// A message as it comes back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    pub reqid: u32,
    pub a: u64,
    pub b: u32,
    pub status: i64,
}

/// A request, as bytes.
pub fn encode(op: u32, reqid: u32, a: u64, b: u32, names: u32) -> [u8; MSG] {
    let mut m = [0u8; MSG];
    m[0..4].copy_from_slice(&op.to_ne_bytes());
    m[4..8].copy_from_slice(&reqid.to_ne_bytes());
    m[8..16].copy_from_slice(&a.to_ne_bytes());
    m[16..20].copy_from_slice(&b.to_ne_bytes());
    m[20..24].copy_from_slice(&names.to_ne_bytes());
    m
}

/// What a message that came back says.
pub fn decode(m: &[u8; MSG]) -> Reply {
    let u32_at = |i: usize| u32::from_ne_bytes([m[i], m[i + 1], m[i + 2], m[i + 3]]);
    let mut a = [0u8; 8];
    a.copy_from_slice(&m[8..16]);
    let mut status = [0u8; 8];
    status.copy_from_slice(&m[24..32]);
    Reply {
        reqid: u32_at(4),
        a: u64::from_ne_bytes(a),
        b: u32_at(16),
        status: i64::from_ne_bytes(status),
    }
}

/// How many slots a window of `len` bytes has: `None` when not even one
/// fits.
pub fn slots_of(len: usize) -> Option<usize> {
    match (len / SLOT_LEN).min(SLOTS) {
        0 => None,
        n => Some(n),
    }
}

/// What the server's `errno` is to the VFS. One it has no word for is the
/// device failing.
pub fn error_of(status: i64) -> FsError {
    match -status {
        2 => FsError::EntryNotFound,
        17 => FsError::EntryExist,
        18 => FsError::NotSameFs,
        20 => FsError::NotDir,
        21 => FsError::IsDir,
        22 => FsError::InvalidParam,
        28 => FsError::NoDeviceSpace,
        30 => FsError::ReadOnly,
        36 => FsError::NameTooLong,
        39 => FsError::DirNotEmpty,
        _ => FsError::DeviceError,
    }
}

/// The longest file a listing may claim: 64 TiB. A size is what the file's
/// page cache is made to, and one near the top of a `usize` would wrap the
/// arithmetic done on it.
pub const MAX_FILE: u64 = 1 << 46;

/// One entry of a directory, as the server listed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    /// Its kind and permissions. A server that is not POSIX's say nothing of
    /// either but file or directory: `0o100644` or `0o040755`.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// Its names: 1 for a file, 2 for a directory, from a server that is
    /// not POSIX's.
    pub nlink: u32,
    pub rdev: u64,
}

/// The kinds of file a POSIX server may list.
fn is_kind(mode: u32) -> bool {
    matches!(
        mode & MODE_KIND,
        MODE_FILE | MODE_DIR | MODE_LINK | MODE_FIFO | MODE_SOCKET | MODE_CHAR | MODE_BLOCK
    )
}

/// The `count` records in `data`, appended to `into`. `None` when they are
/// not what a server may send: a record that runs past the data, a size
/// past [`MAX_FILE`], a name that is empty, has a `/` or a NUL in it, is
/// `.` or `..`, or is not text. A name like those would be a way out of the
/// directory, or of the mount. With `posix` the records are the longer ones
/// of [`RECORD_HEAD_POSIX`], and a mode that is no kind of file is refused.
pub fn records(data: &[u8], count: usize, into: &mut Vec<Entry>, posix: bool) -> Option<()> {
    let head_len = if posix {
        RECORD_HEAD_POSIX
    } else {
        RECORD_HEAD
    };
    let mut at = 0;
    for _ in 0..count {
        let head = data.get(at..at + head_len)?;
        let u16_at = |i: usize| u16::from_ne_bytes([head[i], head[i + 1]]) as usize;
        let i64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&head[i..i + 8]);
            i64::from_ne_bytes(b)
        };
        let (rec_len, name_len) = (u16_at(0), u16_at(2));
        let u32_at =
            |i: usize| u32::from_ne_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]);
        let mode = u32_at(4);
        if rec_len < head_len + name_len || !rec_len.is_multiple_of(8) {
            return None;
        }
        if posix && !is_kind(mode) {
            return None;
        }
        let name = data.get(at + head_len..at + head_len + name_len)?;
        data.get(at..at + rec_len)?;
        let name = core::str::from_utf8(name).ok()?;
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return None;
        }
        let size = i64_at(8) as u64;
        if size > MAX_FILE {
            return None;
        }
        let is_dir = mode & MODE_KIND == MODE_DIR;
        let (mode, uid, gid, nlink, rdev) = if posix {
            let nlink = u32_at(48).max(1);
            (mode, u32_at(40), u32_at(44), nlink, i64_at(56) as u64)
        } else if is_dir {
            (MODE_DIR | 0o755, 0, 0, 2, 0)
        } else {
            (MODE_FILE | 0o644, 0, 0, 1, 0)
        };
        into.push(Entry {
            name: String::from(name),
            is_dir,
            size,
            mtime: i64_at(16),
            atime: i64_at(24),
            ctime: i64_at(32),
            mode,
            uid,
            gid,
            nlink,
            rdev,
        });
        at += rec_len;
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn record(name: &[u8], mode: u32, size: u64) -> Vec<u8> {
        let rec_len = (RECORD_HEAD + name.len()).next_multiple_of(8);
        let mut r = vec![0u8; rec_len];
        r[0..2].copy_from_slice(&(rec_len as u16).to_ne_bytes());
        r[2..4].copy_from_slice(&(name.len() as u16).to_ne_bytes());
        r[4..8].copy_from_slice(&mode.to_ne_bytes());
        r[8..16].copy_from_slice(&size.to_ne_bytes());
        r[16..24].copy_from_slice(&7i64.to_ne_bytes());
        r[24..32].copy_from_slice(&8i64.to_ne_bytes());
        r[32..40].copy_from_slice(&9i64.to_ne_bytes());
        r[RECORD_HEAD..RECORD_HEAD + name.len()].copy_from_slice(name);
        r
    }

    #[test]
    fn a_reply_is_the_request_with_its_status() {
        let mut m = encode(OP_READ, 0x35, 1 << 40, 4096, 9);
        assert_eq!(u32::from_ne_bytes([m[0], m[1], m[2], m[3]]), OP_READ);
        assert_eq!(u32::from_ne_bytes([m[20], m[21], m[22], m[23]]), 9);
        m[24..32].copy_from_slice(&(-2i64).to_ne_bytes());
        let reply = decode(&m);
        assert_eq!(
            reply,
            Reply {
                reqid: 0x35,
                a: 1 << 40,
                b: 4096,
                status: -2
            }
        );
        assert_eq!(error_of(reply.status), FsError::EntryNotFound);
    }

    #[test]
    fn the_window_is_cut_into_whole_slots() {
        assert_eq!(slots_of(SLOT_LEN - 1), None);
        assert_eq!(slots_of(SLOT_LEN), Some(1));
        assert_eq!(slots_of(3 * SLOT_LEN + 100), Some(3));
        assert_eq!(slots_of(40 * SLOT_LEN), Some(SLOTS));
    }

    #[test]
    fn a_listing_is_read_record_by_record() {
        let mut data = record(b"notes.txt", 0o100644, 1234);
        data.extend(record(b"photos", MODE_DIR | 0o755, 0));
        let mut entries = Vec::new();
        records(&data, 2, &mut entries, false).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "notes.txt");
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 1234);
        assert_eq!(
            (entries[0].mtime, entries[0].atime, entries[0].ctime),
            (7, 8, 9)
        );
        assert!(entries[1].is_dir);
    }

    #[test]
    fn a_name_that_leads_out_of_the_directory_is_refused() {
        for name in [&b".."[..], b".", b"", b"a/b", b"a\0b", b"\xff\xfe"] {
            let data = record(name, 0o100644, 1);
            assert!(
                records(&data, 1, &mut Vec::new(), false).is_none(),
                "{:?}",
                name
            );
        }
    }

    #[test]
    fn a_size_no_file_has_is_refused() {
        let data = record(b"file", 0o100644, MAX_FILE);
        assert!(records(&data, 1, &mut Vec::new(), false).is_some());
        let data = record(b"file", 0o100644, MAX_FILE + 1);
        assert!(records(&data, 1, &mut Vec::new(), false).is_none());
        let data = record(b"file", 0o100644, u64::MAX);
        assert!(records(&data, 1, &mut Vec::new(), false).is_none());
    }

    #[test]
    fn a_record_that_runs_past_the_data_is_refused() {
        let data = record(b"file", 0o100644, 1);
        // More records than there are.
        assert!(records(&data, 2, &mut Vec::new(), false).is_none());
        // A record cut short.
        assert!(records(&data[..data.len() - 8], 1, &mut Vec::new(), false).is_none());
        // A length that does not hold the name.
        let mut short = data.clone();
        short[0..2].copy_from_slice(&40u16.to_ne_bytes());
        assert!(records(&short, 1, &mut Vec::new(), false).is_none());
    }

    fn posix_record(name: &[u8], mode: u32, uid: u32, gid: u32, nlink: u32) -> Vec<u8> {
        let rec_len = (RECORD_HEAD_POSIX + name.len()).next_multiple_of(8);
        let mut r = vec![0u8; rec_len];
        r[0..2].copy_from_slice(&(rec_len as u16).to_ne_bytes());
        r[2..4].copy_from_slice(&(name.len() as u16).to_ne_bytes());
        r[4..8].copy_from_slice(&mode.to_ne_bytes());
        r[8..16].copy_from_slice(&11u64.to_ne_bytes());
        r[40..44].copy_from_slice(&uid.to_ne_bytes());
        r[44..48].copy_from_slice(&gid.to_ne_bytes());
        r[48..52].copy_from_slice(&nlink.to_ne_bytes());
        r[56..64].copy_from_slice(&0x0801u64.to_ne_bytes());
        r[RECORD_HEAD_POSIX..RECORD_HEAD_POSIX + name.len()].copy_from_slice(name);
        r
    }

    #[test]
    fn a_posix_record_carries_the_owner_the_links_and_the_kind() {
        let mut data = posix_record(b"link", MODE_LINK | 0o777, 1000, 100, 1);
        data.extend(posix_record(b"bin", MODE_DIR | 0o750, 0, 0, 3));
        let mut entries = Vec::new();
        records(&data, 2, &mut entries, true).unwrap();
        let link = &entries[0];
        assert_eq!(link.name, "link");
        assert_eq!(
            (link.mode, link.uid, link.gid, link.nlink, link.rdev),
            (MODE_LINK | 0o777, 1000, 100, 1, 0x0801)
        );
        assert!(!link.is_dir);
        assert!(entries[1].is_dir);
        assert_eq!(entries[1].nlink, 3);
    }

    #[test]
    fn a_short_record_is_refused_on_a_posix_mount_and_a_mode_of_no_kind_too() {
        // The record a FAT server sends is shorter than a POSIX one.
        let data = record(b"file", 0o100644, 1);
        assert!(records(&data, 1, &mut Vec::new(), true).is_none());
        let data = posix_record(b"file", 0o644, 0, 0, 1);
        assert!(records(&data, 1, &mut Vec::new(), true).is_none());
    }

    #[test]
    fn a_server_that_is_not_posix_says_file_or_directory_and_nothing_more() {
        let mut data = record(b"notes.txt", 0o104777, 1);
        data.extend(record(b"photos", MODE_DIR | 0o700, 0));
        let mut entries = Vec::new();
        records(&data, 2, &mut entries, false).unwrap();
        assert_eq!((entries[0].mode, entries[0].nlink), (MODE_FILE | 0o644, 1));
        assert_eq!((entries[1].mode, entries[1].nlink), (MODE_DIR | 0o755, 2));
    }
}
