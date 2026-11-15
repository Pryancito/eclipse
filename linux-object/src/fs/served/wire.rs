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
}

/// The `count` records in `data`, appended to `into`. `None` when they are
/// not what a server may send: a record that runs past the data, a size
/// past [`MAX_FILE`], a name that is empty, has a `/` or a NUL in it, is
/// `.` or `..`, or is not text. A name like those would be a way out of the
/// directory, or of the mount.
pub fn records(data: &[u8], count: usize, into: &mut Vec<Entry>) -> Option<()> {
    let mut at = 0;
    for _ in 0..count {
        let head = data.get(at..at + RECORD_HEAD)?;
        let u16_at = |i: usize| u16::from_ne_bytes([head[i], head[i + 1]]) as usize;
        let i64_at = |i: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&head[i..i + 8]);
            i64::from_ne_bytes(b)
        };
        let (rec_len, name_len) = (u16_at(0), u16_at(2));
        let mode = u32::from_ne_bytes([head[4], head[5], head[6], head[7]]);
        if rec_len < RECORD_HEAD + name_len || !rec_len.is_multiple_of(8) {
            return None;
        }
        let name = data.get(at + RECORD_HEAD..at + RECORD_HEAD + name_len)?;
        data.get(at..at + rec_len)?;
        let name = core::str::from_utf8(name).ok()?;
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
            return None;
        }
        let size = i64_at(8) as u64;
        if size > MAX_FILE {
            return None;
        }
        into.push(Entry {
            name: String::from(name),
            is_dir: mode & 0o170000 == MODE_DIR,
            size,
            mtime: i64_at(16),
            atime: i64_at(24),
            ctime: i64_at(32),
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
        records(&data, 2, &mut entries).unwrap();
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
            assert!(records(&data, 1, &mut Vec::new()).is_none(), "{:?}", name);
        }
    }

    #[test]
    fn a_size_no_file_has_is_refused() {
        let data = record(b"file", 0o100644, MAX_FILE);
        assert!(records(&data, 1, &mut Vec::new()).is_some());
        let data = record(b"file", 0o100644, MAX_FILE + 1);
        assert!(records(&data, 1, &mut Vec::new()).is_none());
        let data = record(b"file", 0o100644, u64::MAX);
        assert!(records(&data, 1, &mut Vec::new()).is_none());
    }

    #[test]
    fn a_record_that_runs_past_the_data_is_refused() {
        let data = record(b"file", 0o100644, 1);
        // More records than there are.
        assert!(records(&data, 2, &mut Vec::new()).is_none());
        // A record cut short.
        assert!(records(&data[..data.len() - 8], 1, &mut Vec::new()).is_none());
        // A length that does not hold the name.
        let mut short = data.clone();
        short[0..2].copy_from_slice(&40u16.to_ne_bytes());
        assert!(records(&short, 1, &mut Vec::new()).is_none());
    }
}
