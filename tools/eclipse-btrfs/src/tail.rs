//! The end of a file being written in order, before btrfs has it.
//!
//! A file written front to back comes a window at a time (64 KiB), or a
//! `write(2)` at a time, and btrfs makes each write an extent of its own,
//! with the tree changes that go with it. So bytes that only add to where
//! the last write ended are kept here until there is a mebibyte of them,
//! or the file is used otherwise, or a second has gone by, and btrfs is
//! handed them in one write -- what the kernel's btrfs does for itself.
//!
//! They are kept in memory the mount holds, beside the disk's: what the
//! kernel was told is written is there, for the next server, when this one
//! dies. The bytes go in before the count that says they are there, and
//! the file and where its bytes start are only set while the count is 0,
//! so the memory is true whenever the process stops.

/// The most bytes kept.
pub const TAIL_MAX: usize = 1 << 20;
/// The longest path a tail is kept for: a longer one is written at once.
pub const PATH_MAX: usize = 4096 - HEAD;
/// The bytes of the memory.
pub const LEN: usize = 4096 + TAIL_MAX;

/// "eclbtrt1".
const MAGIC: u64 = 0x3174_7274_626c_6365;
/// `magic, start, held, path_len` (u64 each), then the path.
const HEAD: usize = 32;

pub struct Tail {
    base: *mut u8,
    _own: Option<Vec<u64>>,
}

impl Tail {
    /// Memory of this process's own.
    pub fn private() -> Self {
        let mut own = vec![0u64; LEN / 8];
        let mut tail = Tail {
            base: own.as_mut_ptr().cast(),
            _own: Some(own),
        };
        tail.lay_out();
        tail
    }

    /// The memory at `base`: zeros, or what a server before this one left.
    ///
    /// # Safety
    /// `base` is the start of [`LEN`] bytes, aligned to 8, readable and
    /// writable for as long as this is used, and nothing else uses them.
    pub unsafe fn at(base: *mut u8) -> Self {
        let mut tail = Tail { base, _own: None };
        if tail.word(0) != MAGIC {
            tail.lay_out();
        }
        tail
    }

    fn lay_out(&mut self) {
        self.set_word(2, 0);
        self.set_word(0, MAGIC);
    }

    fn word(&self, i: usize) -> u64 {
        // SAFETY: one of the head's words, inside the memory and aligned.
        unsafe { core::ptr::read_volatile(self.base.cast::<u64>().add(i)) }
    }

    fn set_word(&mut self, i: usize, v: u64) {
        // SAFETY: as `word`.
        unsafe { core::ptr::write_volatile(self.base.cast::<u64>().add(i), v) }
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: the data part of the memory.
        unsafe { core::slice::from_raw_parts(self.base.add(4096), TAIL_MAX) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `bytes`.
        unsafe { core::slice::from_raw_parts_mut(self.base.add(4096), TAIL_MAX) }
    }

    fn held(&self) -> usize {
        (self.word(2) as usize).min(TAIL_MAX)
    }

    /// The path, as it was set: `None` when it is not text, which only a
    /// memory that was never a tail's has.
    fn path(&self) -> Option<&str> {
        let len = (self.word(3) as usize).min(PATH_MAX);
        // SAFETY: the path part of the head.
        let bytes = unsafe { core::slice::from_raw_parts(self.base.add(HEAD), len) };
        core::str::from_utf8(bytes).ok()
    }

    /// What is kept: the file, where the bytes go in it, and the bytes.
    pub fn kept(&self) -> Option<(&str, u64, &[u8])> {
        let held = self.held();
        if held == 0 {
            return None;
        }
        Some((self.path()?, self.word(1), &self.bytes()[..held]))
    }

    /// Whether the file kept is `path`.
    pub fn is_of(&self, path: &str) -> bool {
        self.held() > 0 && self.path() == Some(path)
    }

    /// Whether `len` bytes written to `path` at `offset` go on where the
    /// bytes kept end, and there is room for them.
    pub fn continues(&self, path: &str, offset: u64, len: usize) -> bool {
        let held = self.held();
        self.is_of(path) && self.word(1) + held as u64 == offset && held + len <= TAIL_MAX
    }

    /// Keep `data`, to go at `offset` in `path`. Nothing is kept: see
    /// [`clear`](Self::clear). `false` when it is not something kept.
    pub fn start(&mut self, path: &str, offset: u64, data: &[u8]) -> bool {
        if self.held() != 0 || path.len() > PATH_MAX || data.is_empty() || data.len() > TAIL_MAX {
            return false;
        }
        // SAFETY: the path part of the head, which nothing reads while the
        // count is 0.
        unsafe {
            core::ptr::copy_nonoverlapping(path.as_ptr(), self.base.add(HEAD), path.len());
        }
        self.set_word(3, path.len() as u64);
        self.set_word(1, offset);
        self.bytes_mut()[..data.len()].copy_from_slice(data);
        self.set_word(2, data.len() as u64);
        true
    }

    /// Keep `data` after what is kept, which [`continues`](Self::continues)
    /// said it may.
    pub fn append(&mut self, data: &[u8]) {
        let held = self.held();
        assert!(held > 0 && held + data.len() <= TAIL_MAX);
        self.bytes_mut()[held..held + data.len()].copy_from_slice(data);
        self.set_word(2, (held + data.len()) as u64);
    }

    /// Nothing is kept: btrfs has it.
    pub fn clear(&mut self) {
        self.set_word(2, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_kept_is_what_was_written_in_order() {
        let mut tail = Tail::private();
        assert!(tail.kept().is_none());
        assert!(tail.start("a/b", 100, b"hello"));
        assert!(!tail.start("a/b", 200, b"again"));
        assert!(tail.continues("a/b", 105, 3));
        assert!(!tail.continues("a/b", 104, 3));
        assert!(!tail.continues("a/c", 105, 3));
        assert!(!tail.continues("a/b", 105, TAIL_MAX));
        tail.append(b" you");
        assert_eq!(tail.kept(), Some(("a/b", 100, &b"hello you"[..])));
        tail.clear();
        assert!(tail.kept().is_none());
        assert!(!tail.is_of("a/b"));
    }

    #[test]
    fn the_next_server_finds_what_the_last_one_kept() {
        let mut memory = vec![0u64; LEN / 8];
        let base = memory.as_mut_ptr().cast::<u8>();
        // SAFETY: the memory is LEN bytes, aligned, and used by one at a
        // time.
        let mut first = unsafe { Tail::at(base) };
        assert!(first.kept().is_none());
        first.start("file", 7, b"abc");
        drop(first);
        let next = unsafe { Tail::at(base) };
        assert_eq!(next.kept(), Some(("file", 7, &b"abc"[..])));
    }
}
