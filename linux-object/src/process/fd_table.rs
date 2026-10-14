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
//!
//! ## `next_fd`, the lower bound on the lowest free descriptor
//!
//! Linux keeps the same thing (`files_struct::next_fd`) for the same reason:
//! `alloc_fd` must return the lowest free number, and finding it by probing
//! upwards from zero costs one lookup per descriptor the process already
//! holds. That is paid on every `open`, `dup`, `pipe`, `socket` and `accept`,
//! so filling a table is quadratic in its size -- 10.9 ns on an empty table
//! and 26.7 **micro**seconds at eight thousand open descriptors, one `accept`
//! of a server's steady state (`fd_alloc_of_*`).
//!
//! **The invariant is one-sided and that is deliberate**: the bound is only
//! ever allowed to be at or BELOW the lowest free descriptor. Too low merely
//! costs the probes it saved; too high would hand out a number that is not
//! the lowest free one, which is a visible ABI difference -- a shell writes
//! `exec 3<file` and then talks about fd 3. So every removal lowers it
//! ([`LinuxProcessInner::note_fd_freed`]) and nothing raises it except
//! walking over descriptors found to be occupied.
//!
//! ## `epoll_fds`, which descriptors are an epoll
//!
//! `epoll_forget_plan` runs on every `close(2)` and used to find the epolls
//! by cloning and downcasting EVERY entry of the table -- an atomic increment
//! and a vtable check per open descriptor, on a path a program takes once per
//! file it reads. That is 114 microseconds for one close at eight thousand
//! open descriptors (`close_and_reopen_of_8192`), and the great majority of
//! processes have no epoll at all to find.
//!
//! This invariant is **exact and not one-sided**: these are EXACTLY the
//! descriptors whose file is an epoll. One missing is an epoll a close fails
//! to tell; one left over is a plan built for an epoll that is not there.
//!
//! ## `holders`, how many descriptors hold each description
//!
//! It answers the one question a close has to ask before it tells an epoll
//! anything: is the description I just closed still held by some surviving
//! descriptor of mine? Linux drops an epoll entry from `__fput`, when the
//! LAST reference goes, so a `dup` of a watched descriptor keeps its entry
//! alive through the close of the original. Asking it by walking `files` and
//! comparing pointers made a close linear in the table all over again once
//! any epoll was open: 65.7 microseconds at eight thousand open descriptors
//! (`close_and_reopen_watched_of_8192`), and a `close_range` of k descriptors
//! over a table of n was k*n comparisons.
//!
//! The address is kept as a **number** and not as a pointer: it is an
//! identity and never dereferenced, and a raw pointer in the struct would
//! make the whole process object not `Send`. And it is a safe key **because
//! the table holds a strong reference to everything it counts**: nothing
//! counted here can be freed, so no address in the map can be reused by a
//! different description while it is in it. The count reaching zero and the
//! entry leaving are the same event, which is what lets
//! [`LinuxProcessInner::holds_description`] be a plain `contains_key`.
//!
//! All three are kept in step by the same two hooks,
//! [`LinuxProcessInner::note_fd_inserted`] and
//! [`LinuxProcessInner::note_fd_freed`]: the two places a descriptor appears
//! in or leaves `files`.

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
    ///
    /// Both halves count in `i32`, which is what a descriptor IS. Counting
    /// the range in `usize` is what made the two halves disagree: a negative
    /// `first` converts to about 2^64 there, so the range walk came out
    /// EMPTY while the table walk -- which compares descriptors -- still
    /// found 0 and 1 in `[-1, 1]`. Which of the two runs is decided by the
    /// size of the table, so that disagreement is an answer that changes
    /// with how many files the process happens to have open.
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
            (i32::from(first)..=i32::from(last))
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
    pub(super) fn note_fd_freed(&mut self, fd: FileDesc, file: &Arc<dyn FileLike>) {
        self.next_fd = self.next_fd.min(usize::from(fd));
        self.epoll_fds.remove(&fd);
        let key = Self::description_key(file);
        match self.holders.get_mut(&key) {
            // The count reaching zero and the entry leaving are one event, so
            // that `holds_description` is a plain `contains_key`.
            Some(1) | None => {
                self.holders.remove(&key);
            }
            Some(n) => *n -= 1,
        }
    }

    /// Whether some descriptor of this table still holds `file`'s open file
    /// description. `None` left in the map means nobody does.
    pub(super) fn holds_description(&self, file: &Arc<dyn FileLike>) -> bool {
        self.holders.contains_key(&Self::description_key(file))
    }

    /// The address of the open file description, which is what makes two
    /// descriptors "the same open file". The `dyn` pointer is fat and its
    /// vtable half is not part of the identity, so the cast to a thin pointer
    /// is what the comparison wants, not a detail.
    fn description_key(file: &Arc<dyn FileLike>) -> usize {
        Arc::as_ptr(file) as *const () as usize
    }

    /// Record that `file` is about to go into `files` under `fd`. The other
    /// half of [`Self::note_fd_freed`]: it keeps
    /// `LinuxProcessInner::epoll_fds` exactly the set of descriptors whose
    /// file is an epoll.
    ///
    /// Call it BEFORE the insert, with the file that is going in. It has to
    /// remove as well as add: an epoll replaced by an ordinary file under the
    /// same number would otherwise leave that number in the set, and every
    /// later close would then build a plan for an epoll that is not there.
    ///
    /// The caller that needs the removal is not `dup2`, which this comment
    /// used to name: `LinuxProcess::replace_file` frees the old entry
    /// (`note_fd_freed`, which takes the number out of the set) before it
    /// inserts the new one. It is `insert_file` over a number that is
    /// ALREADY open, and the only way in is `LinuxProcess::add_file_at`:
    /// that one hands `insert_file` a number the caller chose and nothing
    /// checks that it is free, so a `HashMap::insert` quietly drops whatever
    /// was there. This `else` is what keeps `epoll_fds` straight when it
    /// does; the `holders` count of the dropped description is NOT kept
    /// straight, because no `note_fd_freed` runs for it, so a caller that
    /// installs over a live number leaks a holder. `add_file_at` has no
    /// caller outside the tests today -- whoever gives it one should use
    /// `replace_file` instead, which is dup2's path and frees first.
    ///
    /// A mutation run removed this `else` and left the whole crate green,
    /// which is what sent somebody looking for the caller the comment
    /// claimed.
    pub(super) fn note_fd_inserted(&mut self, fd: FileDesc, file: &Arc<dyn FileLike>) {
        if file.clone().downcast_arc::<crate::fs::Epoll>().is_ok() {
            self.epoll_fds.insert(fd);
        } else {
            self.epoll_fds.remove(&fd);
        }
        *self.holders.entry(Self::description_key(file)).or_insert(0) += 1;
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
        self.epoll_fds.clear();
        self.holders.clear();
        core::mem::take(&mut self.files)
    }
}
