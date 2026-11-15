//! What passes between the kernel and this process: the layout the kernel
//! has in `linux-object/src/fs/served/wire.rs`, from the other side.
//!
//! A request is one message; its path is at the start of the request's
//! slot of the window, and what is answered with goes in the slot's data
//! area. The reply is the same message with `status` filled in: a count,
//! or a Linux `errno`, negated.

use udrivers::fifo::Message;

/// One element of the FIFO.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Msg {
    pub op: u32,
    /// The request's name. Its low bits are its slot of the window.
    pub reqid: u32,
    pub a: u64,
    pub b: u32,
    /// The bytes of the path at the start of the slot.
    pub names: u32,
    pub status: i64,
}

// SAFETY: `#[repr(C)]`, 32 bytes with no padding (two u32, a u64, two u32,
// an i64), and every bit pattern is a value of each field.
unsafe impl Message for Msg {}

/// The start of each slot: the request's path.
pub const NAME_AREA: usize = 4096;
/// The rest of the slot: what was read, or listed.
pub const DATA_AREA: usize = 64 * 1024;
/// A slot of the window, one request in flight on each.
pub const SLOT_LEN: usize = NAME_AREA + DATA_AREA;
/// The most slots the kernel cuts a window into.
pub const SLOTS: usize = 16;

/// The entries of the directory at the path, from entry `a` on.
pub const OP_LIST: u32 = 1;
/// Up to `b` bytes of the file at the path, from byte `a`.
pub const OP_READ: u32 = 2;
/// The filesystem's numbers.
pub const OP_STATFS: u32 = 3;

/// [`OP_LIST`]'s `a` in the reply that holds the directory's last entries.
pub const LIST_END: u64 = u64::MAX;
/// The bytes of an [`OP_STATFS`] reply: five u64.
pub const STATFS_LEN: usize = 40;

/// A listing record's fixed part: `rec_len, name_len` (u16 each), `mode`
/// (u32), `size` (u64), `mtime, atime, ctime` (i64 each, seconds), then the
/// name. `rec_len` is the whole record, a multiple of 8.
pub const RECORD_HEAD: usize = 40;
pub const MODE_DIR: u32 = 0o040755;
pub const MODE_FILE: u32 = 0o100644;

/// `eclipse_fs_serve` flag: names match whatever the case of their ASCII
/// letters.
pub const SERVE_FOLD_CASE: usize = 1;

/// One entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
}

impl Entry {
    /// The bytes this entry's record takes.
    pub fn record_len(&self) -> usize {
        (RECORD_HEAD + self.name.len()).next_multiple_of(8)
    }

    /// Write the record at the start of `out`, which holds it.
    fn put(&self, out: &mut [u8]) {
        let len = self.record_len();
        out[..len].fill(0);
        out[0..2].copy_from_slice(&(len as u16).to_ne_bytes());
        out[2..4].copy_from_slice(&(self.name.len() as u16).to_ne_bytes());
        let mode = if self.is_dir { MODE_DIR } else { MODE_FILE };
        out[4..8].copy_from_slice(&mode.to_ne_bytes());
        out[8..16].copy_from_slice(&self.size.to_ne_bytes());
        out[16..24].copy_from_slice(&self.mtime.to_ne_bytes());
        out[24..32].copy_from_slice(&self.atime.to_ne_bytes());
        out[32..40].copy_from_slice(&self.ctime.to_ne_bytes());
        out[RECORD_HEAD..RECORD_HEAD + self.name.len()].copy_from_slice(self.name.as_bytes());
    }
}

/// As many of `entries`, from `start` on, as `out` holds, as records:
/// `(records, bytes, next)`, where `next` is the entry the next request
/// starts at, or [`LIST_END`].
pub fn pack(entries: &[Entry], start: u64, out: &mut [u8]) -> (usize, usize, u64) {
    let start = usize::try_from(start)
        .unwrap_or(usize::MAX)
        .min(entries.len());
    let (mut count, mut used) = (0, 0);
    for entry in &entries[start..] {
        let len = entry.record_len();
        if used + len > out.len() {
            return (count, used, (start + count) as u64);
        }
        entry.put(&mut out[used..]);
        used += len;
        count += 1;
    }
    (count, used, LIST_END)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool, size: u64) -> Entry {
        Entry {
            name: name.to_string(),
            is_dir,
            size,
            mtime: 7,
            atime: 8,
            ctime: 9,
        }
    }

    #[test]
    fn a_message_is_the_kernels_thirty_two_bytes() {
        assert_eq!(core::mem::size_of::<Msg>(), 32);
        assert_eq!(core::mem::offset_of!(Msg, a), 8);
        assert_eq!(core::mem::offset_of!(Msg, names), 20);
        assert_eq!(core::mem::offset_of!(Msg, status), 24);
    }

    #[test]
    fn a_record_is_laid_out_as_the_kernel_reads_it() {
        let mut out = [0xffu8; 64];
        let (count, used, next) = pack(&[entry("notes.txt", false, 1234)], 0, &mut out);
        assert_eq!((count, used, next), (1, 56, LIST_END));
        assert_eq!(u16::from_ne_bytes([out[0], out[1]]), 56);
        assert_eq!(u16::from_ne_bytes([out[2], out[3]]), 9);
        assert_eq!(
            u32::from_ne_bytes([out[4], out[5], out[6], out[7]]),
            MODE_FILE
        );
        assert_eq!(u64::from_ne_bytes(out[8..16].try_into().unwrap()), 1234);
        assert_eq!(i64::from_ne_bytes(out[16..24].try_into().unwrap()), 7);
        assert_eq!(i64::from_ne_bytes(out[32..40].try_into().unwrap()), 9);
        assert_eq!(&out[40..49], b"notes.txt");
        // The padding is zeros, not what was in the window before.
        assert_eq!(&out[49..56], &[0; 7]);
    }

    #[test]
    fn a_listing_longer_than_the_area_goes_on_where_it_stopped() {
        let entries: Vec<Entry> = (0..10)
            .map(|i| entry(&format!("file{i}"), false, i))
            .collect();
        // Each record is 48 bytes: room for three.
        let mut out = [0u8; 150];
        assert_eq!(pack(&entries, 0, &mut out), (3, 144, 3));
        assert_eq!(pack(&entries, 3, &mut out), (3, 144, 6));
        assert_eq!(pack(&entries, 9, &mut out), (1, 48, LIST_END));
        // Past the end there is nothing, and that is the end.
        assert_eq!(pack(&entries, 10, &mut out), (0, 0, LIST_END));
        assert_eq!(pack(&entries, u64::MAX - 1, &mut out), (0, 0, LIST_END));
    }
}
