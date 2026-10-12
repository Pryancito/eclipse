//! Which number a new file descriptor gets, and the two range sweeps
//! `close_range(2)` is made of.
//!
//! A module of its own because `process.rs` is at its size ceiling
//! (`xtask tamano`), and because these four methods are one subject: they are
//! the only code that reads or writes `LinuxProcessInner::next_fd`, the lower
//! bound on the lowest free descriptor that the field's own documentation
//! states the invariant for. Everything else in the crate goes through
//! `add_file`, `close_file` and their neighbours.
//!
//! Tested in `process/fd_alloc_tests.rs` and measured in
//! `benches/process.rs`.

use super::*;

impl LinuxProcessInner {
    pub(super) fn get_free_fd(&mut self) -> FileDesc {
        self.get_free_fd_from(0)
    }

    pub(super) fn get_free_fd_from(&mut self, start: usize) -> FileDesc {
        // Walk the hint over anything that is occupied. Each step past a
        // descriptor is paid once for as long as that descriptor stays open,
        // which is what makes filling a table linear rather than quadratic.
        // It also makes the hint self-healing: the loop only ever advances
        // over OCCUPIED numbers, so a hint left too low by any path costs
        // probes and cannot cost correctness.
        while self.files.contains_key(&FileDesc::from(self.next_fd)) {
            self.next_fd += 1;
        }
        if start <= self.next_fd {
            // The lowest free descriptor at or above `start` is the lowest
            // free one outright, because `start` is at or below it.
            FileDesc::from(self.next_fd)
        } else {
            // `fcntl(F_DUPFD, start)` above the hint: the hint says nothing
            // about what is free up there, so this half still probes. It is
            // bounded by the descriptors held ABOVE `start`, and the hint is
            // left alone -- lowering it here would be wrong and raising it
            // would discard the hole it is pointing at.
            (start..)
                .map(|i| i.into())
                .find(|fd| !self.files.contains_key(fd))
                .unwrap()
        }
    }

    /// The open descriptors in `[first, last]`, which is what both halves of
    /// `close_range(2)` need before they touch anything.
    ///
    /// Two ways round, chosen by which is smaller. Walking the TABLE is what
    /// the two callers used to do unconditionally, and it costs one step per
    /// descriptor the process holds however narrow the range is: the three
    /// descriptors a child closes before `exec` cost 128 microseconds to find
    /// at eight thousand open descriptors (`benches/process.rs`,
    /// `close_range_three_of_8192`) -- ten times what marking the WHOLE table
    /// close-on-exec costs, for three of them. Walking the RANGE instead is
    /// one lookup per number in it, which is the better way round exactly
    /// when the range is the narrower of the two.
    ///
    /// Either way the answer is the same set, and the order is not promised
    /// by either (`files` is a hash map); nothing downstream depends on one.
    pub(super) fn open_fds_in_range(&self, first: FileDesc, last: FileDesc) -> Vec<FileDesc> {
        // The width is read off the i32 the descriptor actually is, not off
        // the `usize` it converts to: a negative number widens to about 2^64
        // there, and a width taken from that would send a range down the
        // wrong branch. An empty range leaves by the arm below rather than
        // by arithmetic that happens to come out unreachable.
        let width = match i64::from(i32::from(last)) - i64::from(i32::from(first)) {
            d if d >= 0 => d as usize + 1,
            // `last` below `first`: nothing is in the range. The syscall
            // refuses this outright (`close_range_action` answers `EINVAL`),
            // so it is here for the in-kernel callers and not for a program.
            _ => return Vec::new(),
        };
        if width <= self.files.len() {
            (usize::from(first)..=usize::from(last))
                .map(FileDesc::from)
                .filter(|fd| self.files.contains_key(fd))
                .collect()
        } else {
            self.files
                .keys()
                .filter(|&&fd| fd >= first && fd <= last)
                .cloned()
                .collect()
        }
    }

    /// Record that `fd` is no longer in `files`, so the next allocation can
    /// hand it back out. The one way the hint ever moves down one descriptor
    /// at a time.
    pub(super) fn note_fd_freed(&mut self, fd: FileDesc) {
        self.next_fd = self.next_fd.min(usize::from(fd));
    }

    /// Empty the descriptor table, returning what was in it so the caller can
    /// drop the files with no lock held. This is what a dying process does.
    ///
    /// The hint goes back to the bottom in the same breath, and that is why
    /// this is a method rather than the `mem::take` it replaced: every OTHER
    /// way a descriptor goes away reports itself through
    /// [`Self::note_fd_freed`], and a bulk removal that forgot to would leave
    /// the bound above the lowest free descriptor -- the one direction it is
    /// not allowed to be.
    pub(super) fn take_all_files(&mut self) -> HashMap<FileDesc, Arc<dyn FileLike>> {
        self.next_fd = 0;
        core::mem::take(&mut self.files)
    }
}
