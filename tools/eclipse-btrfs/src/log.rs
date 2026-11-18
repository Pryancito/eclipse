//! The changes made since btrfs last wrote its trees: what a server after
//! this one makes again.
//!
//! Writing the trees after every change -- each change a dozen tree blocks
//! and the superblock, checksummed and copied into the kept memory -- costs
//! more than most changes. So, as the kernel's btrfs does, they are written
//! every so many changes ([`Volume`](crate::volume::Volume) says how many),
//! and the changes in between are written down here, in memory the mount
//! holds, as the kernel asked for them. A server that takes the mount over
//! finds the volume as it was when the trees were last written, and makes
//! these changes again, in order: the volume is then what the kernel was
//! told.
//!
//! A record is `op, names` (u32 each), `a` (u64), `b, data` (u32 each), the
//! names (a path, or two with a NUL between) and the data, the whole padded
//! to 8 bytes. Its bytes go in before the count of bytes used that takes it
//! in, so the memory is true whenever the process stops.

/// The bytes of the memory.
pub const LEN: usize = 4 << 20;

/// "eclbtrl1".
const MAGIC: u64 = 0x316c_7274_626c_6365;
/// `magic, used, records, base` (u64 each).
const HEAD: usize = 64;
const RECORD_HEAD: usize = 24;

/// One change, as the kernel asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub op: u32,
    pub names: String,
    pub a: u64,
    pub b: u32,
    pub data: Vec<u8>,
}

pub struct Log {
    base: *mut u8,
    _own: Option<Vec<u64>>,
}

impl Log {
    /// Memory of this process's own.
    pub fn private() -> Self {
        let mut own = vec![0u64; LEN / 8];
        let mut log = Log {
            base: own.as_mut_ptr().cast(),
            _own: Some(own),
        };
        log.restart(0);
        log.set_word(0, MAGIC);
        log
    }

    /// The memory at `base`: zeros, or what a server before this one left.
    ///
    /// # Safety
    /// `base` is the start of [`LEN`] bytes, aligned to 8, readable and
    /// writable for as long as this is used, and nothing else uses them.
    pub unsafe fn at(base: *mut u8) -> Self {
        let mut log = Log { base, _own: None };
        if log.word(0) != MAGIC {
            log.restart(0);
            log.set_word(0, MAGIC);
        }
        log
    }

    fn word(&self, i: usize) -> u64 {
        // SAFETY: one of the head's words, inside the memory and aligned.
        unsafe { core::ptr::read_volatile(self.base.cast::<u64>().add(i)) }
    }

    fn set_word(&mut self, i: usize, v: u64) {
        // SAFETY: as `word`.
        unsafe { core::ptr::write_volatile(self.base.cast::<u64>().add(i), v) }
    }

    fn used(&self) -> usize {
        (self.word(1) as usize).min(LEN - HEAD)
    }

    /// How many changes are written down.
    pub fn records(&self) -> usize {
        self.word(2) as usize
    }

    /// The bytes written down, of the room there is.
    pub fn bytes(&self) -> usize {
        self.used()
    }

    /// The bytes a record of `names` and `data` takes.
    pub fn record_len(names: &str, data: &[u8]) -> usize {
        (RECORD_HEAD + names.len() + data.len()).next_multiple_of(8)
    }

    /// Whether a record of `len` bytes has room.
    pub fn has_room(&self, len: usize) -> bool {
        self.used() + len <= LEN - HEAD
    }

    /// Write `change` down: `false` when there is no room.
    pub fn push(&mut self, op: u32, names: &str, a: u64, b: u32, data: &[u8]) -> bool {
        let len = Self::record_len(names, data);
        if !self.has_room(len) {
            return false;
        }
        let at = HEAD + self.used();
        // SAFETY: `len` bytes from `at`, inside the memory (`has_room`).
        let out = unsafe { core::slice::from_raw_parts_mut(self.base.add(at), len) };
        out[0..4].copy_from_slice(&op.to_ne_bytes());
        out[4..8].copy_from_slice(&(names.len() as u32).to_ne_bytes());
        out[8..16].copy_from_slice(&a.to_ne_bytes());
        out[16..20].copy_from_slice(&b.to_ne_bytes());
        out[20..24].copy_from_slice(&(data.len() as u32).to_ne_bytes());
        out[RECORD_HEAD..RECORD_HEAD + names.len()].copy_from_slice(names.as_bytes());
        out[RECORD_HEAD + names.len()..RECORD_HEAD + names.len() + data.len()]
            .copy_from_slice(data);
        // The count of records first: one past the bytes is not read.
        self.set_word(2, self.records() as u64 + 1);
        self.set_word(1, (self.used() + len) as u64);
        true
    }

    /// The changes written down, in order. A record that does not hold
    /// together ends them: what follows it was never taken in.
    pub fn changes(&self) -> Vec<Change> {
        let used = self.used();
        // SAFETY: the used part of the memory.
        let bytes = unsafe { core::slice::from_raw_parts(self.base.add(HEAD), used) };
        let mut out = Vec::new();
        let mut at = 0;
        while at + RECORD_HEAD <= used {
            let u32_at =
                |i: usize| u32::from_ne_bytes(bytes[at + i..at + i + 4].try_into().unwrap());
            let (op, names_len, b, data_len) = (
                u32_at(0),
                u32_at(4) as usize,
                u32_at(16),
                u32_at(20) as usize,
            );
            let a = u64::from_ne_bytes(bytes[at + 8..at + 16].try_into().unwrap());
            let len = (RECORD_HEAD + names_len + data_len).next_multiple_of(8);
            if at + len > used {
                break;
            }
            let names = &bytes[at + RECORD_HEAD..at + RECORD_HEAD + names_len];
            let Ok(names) = core::str::from_utf8(names) else {
                break;
            };
            let from = at + RECORD_HEAD + names_len;
            out.push(Change {
                op,
                names: names.to_string(),
                a,
                b,
                data: bytes[from..from + data_len].to_vec(),
            });
            at += len;
        }
        out
    }

    /// Nothing is written down: the trees have it all.
    pub fn clear(&mut self) {
        self.set_word(1, 0);
        self.set_word(2, 0);
    }

    /// Which writing of the trees the changes written down follow: the
    /// number the disk keeps ([`fatserve::disk::Disk::word`]) for the last
    /// one. A log whose number is behind the disk's was written before the
    /// trees were last written, and they have it all.
    pub fn base(&self) -> u64 {
        self.word(3)
    }

    /// The trees were written for the `base`th time: nothing is written
    /// down after it.
    pub fn restart(&mut self, base: u64) {
        self.clear();
        self.set_word(3, base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_changes_are_read_back_in_order_by_the_next_server() {
        let mut memory = vec![0u64; LEN / 8];
        let base = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: LEN bytes, aligned, one log over them at a time.
        let mut log = unsafe { Log::at(base) };
        assert!(log.changes().is_empty());
        assert!(log.push(5, "a/b", 0o100644, 0, &[]));
        assert!(log.push(4, "a/b", 7, 3, b"xyz"));
        assert!(log.push(7, "a/b\0a/c", 0, 0, &[]));
        drop(log);
        let log = unsafe { Log::at(base) };
        let changes = log.changes();
        assert_eq!(log.records(), 3);
        assert_eq!(changes.len(), 3);
        assert_eq!(
            changes[1],
            Change {
                op: 4,
                names: "a/b".into(),
                a: 7,
                b: 3,
                data: b"xyz".to_vec()
            }
        );
        assert_eq!(changes[2].names, "a/b\0a/c");
    }

    #[test]
    fn a_full_log_takes_nothing_more_and_a_cleared_one_has_nothing() {
        let mut log = Log::private();
        let big = vec![1u8; LEN / 3];
        assert!(log.push(4, "f", 0, big.len() as u32, &big));
        assert!(log.push(4, "f", 0, big.len() as u32, &big));
        assert!(!log.push(4, "f", 0, big.len() as u32, &big));
        assert_eq!(log.changes().len(), 2);
        log.clear();
        assert!(log.changes().is_empty());
        assert_eq!(log.bytes(), 0);
    }
}
