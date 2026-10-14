//! Which NUMBER a new descriptor gets, and the hint that is allowed to
//! shortcut the search for it.
//!
//! `alloc_fd` has one rule in Linux and POSIX alike: the number handed out is
//! the LOWEST free one. Programs depend on it to a degree that looks reckless
//! until you have seen it -- a shell writes `exec 3<file` and then talks about
//! fd 3, and every `fork`/`exec` pair in sight closes 0, 1 and 2 and reopens
//! them in order expecting to get 0, 1 and 2 back.
//!
//! Finding that number by probing upwards from zero costs one table lookup per
//! descriptor the process already holds, on every `open`, `dup`, `pipe`,
//! `socket` and `accept` -- 26.7 microseconds at eight thousand open
//! descriptors, and a quadratic bill for filling a table
//! (`benches/process.rs`). So `LinuxProcessInner::next_fd` keeps a LOWER BOUND
//! on the lowest free descriptor and the search starts there instead.
//!
//! The bound is one-sided, and these tests exist because of which side. A hint
//! that is too LOW only costs the probes it was meant to save. A hint that is
//! too HIGH hands out a number that is not the lowest free one, and the shell
//! above gets fd 7 where it asked for 3. Every case below is therefore some
//! way of freeing a descriptor and demanding the next allocation see it:
//! `close`, `close_range`, the execve sweep, `dup2` onto a live descriptor,
//! `fork`, and the socket path, which searches from 1000 and must not drag the
//! bound up there with it.

use super::dup_fd_tests::{a_process, an_open_log, Log};
use super::*;

/// Open `n` files on `proc` and return the descriptors, in the order the
/// table handed them out.
fn open_n(proc: &LinuxProcess, n: usize) -> Vec<FileDesc> {
    (0..n)
        .map(|_| {
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .expect("well inside the default limit")
        })
        .collect()
}

/// The rule itself, with nothing freed: a process that opens files and closes
/// none gets consecutive numbers from zero up.
///
/// `a_process()` is a bare table -- unlike `LinuxProcess::with_root`, which
/// opens its controlling terminal on 0, 1 and 2 -- so the first descriptor it
/// hands out is 0. That also makes this the test that the hint does not start
/// out somewhere other than the bottom.
#[test]
fn a_fresh_table_hands_out_consecutive_numbers_from_zero() {
    let proc = a_process();
    let fds = open_n(&proc, 4);
    assert_eq!(
        fds.iter().map(|fd| usize::from(*fd)).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
}

/// The case the hint exists to get wrong, and the one it must not: a hole
/// opened in the MIDDLE of the table is the lowest free descriptor, and the
/// next allocation has to find it even though the hint is already above it.
#[test]
fn a_hole_in_the_middle_is_what_the_next_open_gets() {
    let proc = a_process();
    let fds = open_n(&proc, 6);
    proc.close_file(fds[1]).expect("it is open");
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        usize::from(fds[1]),
        "the descriptor just closed is the lowest free one; a stale hint would \
         have skipped past it and handed out a number off the top of the table"
    );
}

/// The shell's idiom, which is the one that would have broken loudly: close
/// 0, 1 and 2, reopen three files, get 0, 1 and 2 back in order.
#[test]
fn closing_the_standard_three_and_reopening_gets_them_back_in_order() {
    let proc = a_process();
    open_n(&proc, 4);
    for fd in 0..3 {
        proc.close_file(FileDesc::from(fd)).expect("it is open");
    }
    let reopened = open_n(&proc, 3);
    assert_eq!(
        reopened
            .iter()
            .map(|fd| usize::from(*fd))
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

/// Closing the LOWEST descriptor repeatedly: each close has to move the bound
/// back down, not just the first one.
#[test]
fn every_close_of_the_lowest_descriptor_is_seen_by_the_next_open() {
    let proc = a_process();
    open_n(&proc, 8);
    for _ in 0..5 {
        proc.close_file(FileDesc::from(0)).expect("it is open");
        assert_eq!(
            usize::from(
                proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                    .unwrap()
            ),
            0
        );
    }
}

/// `close(2)` on a descriptor that is not open changes nothing, and must not
/// move the bound: a program closing a number it does not hold would otherwise
/// drag the search back down to it on every allocation.
#[test]
fn closing_a_descriptor_that_is_not_open_leaves_the_numbering_alone() {
    let proc = a_process();
    let fds = open_n(&proc, 3);
    let next = usize::from(*fds.last().unwrap()) + 1;
    assert_eq!(
        proc.close_file(FileDesc::from(next as i32)),
        Err(LxError::EBADF)
    );
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        next
    );
}

/// `close_range(2)`: the whole swept range becomes free, and the lowest of
/// those numbers is what comes next.
#[test]
fn a_swept_range_is_free_from_its_first_number_up() {
    let proc = a_process();
    open_n(&proc, 10);
    proc.close_range(FileDesc::from(4), FileDesc::from(7));
    let reopened = open_n(&proc, 4);
    assert_eq!(
        reopened
            .iter()
            .map(|fd| usize::from(*fd))
            .collect::<Vec<_>>(),
        vec![4, 5, 6, 7]
    );
}

/// The execve sweep (`remove_cloexec_files`) frees descriptors too, and the
/// new image's first `open` has to land on the lowest one it freed.
#[test]
fn the_exec_sweep_frees_the_numbers_it_closed() {
    let proc = a_process();
    open_n(&proc, 4);
    // 1 and 3 close on exec; 0 and 2 stay.
    proc.set_fd_cloexec(FileDesc::from(1), true).unwrap();
    proc.set_fd_cloexec(FileDesc::from(3), true).unwrap();
    proc.remove_cloexec_files();
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        1
    );
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        3
    );
}

/// `dup2(fd, target)` onto a LIVE descriptor closes it and installs the new
/// one under the same number. Nothing is freed, so the numbering must not
/// move: the old `replace_file` removes an entry before it inserts, and a hint
/// that read that removal as a free slot would then hand out an occupied
/// number.
#[test]
fn a_dup2_onto_a_live_descriptor_frees_nothing() {
    let proc = a_process();
    let fds = open_n(&proc, 4);
    let next = usize::from(*fds.last().unwrap()) + 1;
    let replaced = proc
        .replace_file(fds[0], an_open_log(Log::new(), OpenFlags::RDWR), false)
        .expect("dup2 onto an open descriptor");
    assert!(replaced.is_some(), "it closed the one that was there");
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        next,
        "dup2 replaced a descriptor rather than freeing one"
    );
}

/// `dup2` onto a number ABOVE the table does install a descriptor the
/// allocator never chose, and the next allocation must step over it rather
/// than hand it out twice.
#[test]
fn a_descriptor_installed_by_number_is_not_handed_out_again() {
    let proc = a_process();
    open_n(&proc, 4);
    let high = FileDesc::from(4);
    proc.add_file_at(high, an_open_log(Log::new(), OpenFlags::RDWR))
        .expect("inside the limit");
    // 0..=3 are taken by `open_n` and 4 by the explicit install, so 5 is next
    // -- and the two allocations below must be different numbers whatever
    // happens.
    let a = proc
        .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
        .unwrap();
    let b = proc
        .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
        .unwrap();
    assert_eq!(usize::from(a), 5);
    assert_ne!(usize::from(a), usize::from(b));
    assert!(proc.get_file_like(high).is_ok(), "4 is still the one at 4");
}

/// `fcntl(F_DUPFD, start)` above the lowest free descriptor: the search is
/// told where to begin and the number it returns must be at or above there --
/// and it must not pull the process's own numbering up with it, because the
/// next plain `open` is still entitled to the hole down below.
#[test]
fn a_dupfd_from_a_high_start_leaves_the_low_holes_for_everyone_else() {
    let proc = a_process();
    open_n(&proc, 4);
    proc.close_file(FileDesc::from(3)).expect("it is open");
    let high = proc
        .add_file_from_cloexec(an_open_log(Log::new(), OpenFlags::RDWR), false, 100)
        .expect("inside the default limit of 1024");
    assert_eq!(usize::from(high), 100);
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        3,
        "the hole at 3 survived a dup to 100"
    );
}

/// The socket path searches from 1000 for the same reason, through the same
/// function, and must leave the low numbering alone just as `F_DUPFD` does.
#[test]
fn a_socket_at_the_socket_base_leaves_the_low_holes_alone() {
    let proc = a_process();
    open_n(&proc, 4);
    proc.close_file(FileDesc::from(2)).expect("it is open");
    let sock = proc
        .add_socket(an_open_log(Log::new(), OpenFlags::RDWR))
        .expect("the default limit reaches 1000");
    assert_eq!(usize::from(sock), SOCKET_FD);
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        2
    );
}

/// `fork` copies the table, so the child's numbering starts where the
/// parent's did -- including a hole the parent had left open.
#[test]
fn a_forked_child_sees_its_parent_hole() {
    let parent = a_process();
    open_n(&parent, 6);
    parent.close_file(FileDesc::from(2)).expect("it is open");

    let child = {
        let inner = parent.inner.lock();
        let forked = inner.forked_child(0, 0, 0);
        drop(inner);
        LinuxProcess {
            root_inode: parent.root_inode().clone(),
            parent: Mutex::new(Weak::default()),
            vt: 0,
            // A fresh child: nothing is pending at the process level and it is
            // not job-stopped, so the mirror the exit path reads starts empty,
            // exactly as both real constructors leave it.
            exit_path: Default::default(),
            perf: crate::perf::ProcPerf::new(),
            futexes: Default::default(),
            itimers: Default::default(),
            aspace_lock: Mutex::new(()),
            inner: Mutex::new(forked),
        }
    };
    assert_eq!(
        usize::from(
            child
                .add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        2,
        "the child inherited the table, so it inherits the hole in it"
    );
}

/// The allocator still answers past the soft `RLIMIT_NOFILE`, and
/// `insert_file` is what refuses: `get_free_fd_from` returning a number the
/// process may not hold is the case the refusal-before-insert exists for, and
/// the hint must not turn it into a silently different number.
#[test]
fn a_full_table_is_emfile_and_not_a_recycled_descriptor() {
    let proc = a_process();
    {
        let mut inner = proc.inner.lock();
        inner.limits.0[RLIMIT_NOFILE] = RLimit { cur: 8, max: 8 };
    }
    // Eight descriptors is the limit, and the table starts empty.
    open_n(&proc, 8);
    assert_eq!(
        proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR)),
        Err(LxError::EMFILE)
    );
    // And a close makes room again, at the number that was freed.
    proc.close_file(FileDesc::from(5)).expect("it is open");
    assert_eq!(
        usize::from(
            proc.add_file(an_open_log(Log::new(), OpenFlags::RDWR))
                .unwrap()
        ),
        5
    );
}

/// The invariant itself, stated the way the production code relies on it: the
/// bound is never above the lowest free descriptor. Exercised over a run of
/// opens and closes at numbers chosen to leave holes in every position, so
/// that a bound left behind by any one of those paths is caught here rather
/// than in one of the named cases above.
#[test]
fn the_hint_is_never_above_the_lowest_free_descriptor() {
    let proc = a_process();
    open_n(&proc, 20);
    // A non-trivial pattern of holes: 5, 7, 11, 13, 17, 19 -- then a sweep,
    // then a refill, so the bound is pushed up and dragged back down
    // repeatedly.
    for fd in [5, 7, 11, 13, 17, 19] {
        proc.close_file(FileDesc::from(fd)).expect("it is open");
        let inner = proc.inner.lock();
        let lowest = (0..)
            .find(|i| !inner.files.contains_key(&FileDesc::from(*i)))
            .unwrap();
        assert!(
            inner.next_fd <= lowest,
            "the hint ({}) is above the lowest free descriptor ({}) after closing {}",
            inner.next_fd,
            lowest,
            fd
        );
    }
    // The refill has to produce exactly those numbers, in order.
    let refilled: Vec<_> = open_n(&proc, 6).iter().map(|fd| usize::from(*fd)).collect();
    assert_eq!(refilled, vec![5, 7, 11, 13, 17, 19]);
}

/// Emptying the whole table at once is the one removal that cannot report
/// itself descriptor by descriptor, so it resets the bound outright. A dying
/// process does it, and `take_all_files` is the only way it is done.
#[test]
fn emptying_the_table_puts_the_bound_back_at_the_bottom() {
    let proc = a_process();
    open_n(&proc, 6);
    let mut inner = proc.inner.lock();
    assert!(inner.next_fd > 0, "six opens moved the bound up");
    let taken = inner.take_all_files();
    assert_eq!(taken.len(), 6, "it hands back what it emptied");
    assert!(inner.files.is_empty());
    assert_eq!(
        inner.get_free_fd(),
        FileDesc::from(0),
        "an empty table's lowest free descriptor is zero"
    );
}

/// The two ways `open_fds_in_range` can walk a range have to answer the same
/// thing, and the only way to be sure is to ask it both ways over the same
/// table. The table below has holes in it and the ranges below straddle them,
/// one range narrower than the table and one wider, with the boundaries
/// included on purpose: `close_range(2)` is INCLUSIVE at both ends, and an
/// off-by-one at either would close a descriptor the caller still wanted or
/// leave one it asked to be rid of.
#[test]
fn both_ways_round_the_range_find_the_same_descriptors() {
    let proc = a_process();
    open_n(&proc, 12);
    for fd in [2, 5, 6, 9] {
        proc.close_file(FileDesc::from(fd)).expect("it is open");
    }
    let inner = proc.inner.lock();
    // Open now: 0, 1, 3, 4, 7, 8, 10, 11 -- eight descriptors, so a range of
    // eight numbers or fewer takes the range walk and a wider one takes the
    // table walk.
    for (first, last, expected) in [
        // Narrower than the table: the range walk.
        (3usize, 8usize, vec![3usize, 4, 7, 8]),
        // A single number, open and closed.
        (4, 4, vec![4]),
        (5, 5, vec![]),
        // Both ends open, and both ends included.
        (0, 1, vec![0, 1]),
        // Wider than the table: the table walk, same answer.
        (3, 100, vec![3, 4, 7, 8, 10, 11]),
        // Everything, which is what `close_range(0, ~0u32)` asks for --
        // clamped to `i32::MAX` by `close_range_action`, because `FileDesc`
        // is an i32 and the raw `~0u32` would wrap to -1 and match nothing.
        (0, i32::MAX as usize, vec![0, 1, 3, 4, 7, 8, 10, 11]),
    ] {
        let mut got: Vec<usize> = inner
            .open_fds_in_range(FileDesc::from(first), FileDesc::from(last))
            .iter()
            .map(|fd| usize::from(*fd))
            .collect();
        // Neither walk promises an order; `files` is a hash map.
        got.sort_unstable();
        assert_eq!(got, expected, "range {}..={}", first, last);
    }
}

/// A range that starts BELOW zero finds the same descriptors whichever way
/// round it is walked, which is the one case where the two disagreed.
///
/// The range walk counted in `usize`, where a negative `first` is about 2^64,
/// so `[-1, 1]` came out EMPTY -- while the table walk, which compares
/// descriptors, still found 0 and 1 in it. Which of the two runs is decided
/// by the size of the table, so the answer changed with how many files the
/// process happened to have open: that is what makes this worth a test of its
/// own rather than a note. Found by Copilot on #1856.
///
/// The syscall cannot ask for this (`close_range_action` answers `EINVAL`
/// when the raw arguments run backwards), so the caller it protects is an
/// in-kernel one.
#[test]
fn a_range_that_starts_below_zero_finds_the_same_descriptors_both_ways() {
    // Two tables, chosen so that the SAME range takes a different branch in
    // each: the range `[-1, 1]` is three numbers wide, so a table with three
    // or more descriptors open takes the range walk and a smaller one takes
    // the table walk.
    let wide = a_process();
    open_n(&wide, 8);
    let narrow = a_process();
    open_n(&narrow, 2);
    for (proc, what) in [(&wide, "the range walk"), (&narrow, "the table walk")] {
        let inner = proc.inner.lock();
        let mut got: Vec<i32> = inner
            .open_fds_in_range(FileDesc::from(-1), FileDesc::from(1))
            .iter()
            .map(|fd| i32::from(*fd))
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec![0, 1], "{what}");
    }
}

/// An empty range (`last` below `first`) is not a sweep of everything, which
/// is what a width computed without a floor would turn it into.
#[test]
fn a_range_that_runs_backwards_contains_nothing() {
    let proc = a_process();
    open_n(&proc, 6);
    let inner = proc.inner.lock();
    assert!(inner
        .open_fds_in_range(FileDesc::from(4), FileDesc::from(2))
        .is_empty());
    drop(inner);
    // And the sweeps themselves leave the table alone.
    proc.close_range(FileDesc::from(4), FileDesc::from(2));
    proc.set_range_cloexec(FileDesc::from(4), FileDesc::from(2));
    assert_eq!(proc.get_files().unwrap().len(), 6);
    for fd in 0..6 {
        assert!(!proc.fd_cloexec(FileDesc::from(fd)).unwrap());
    }
}

/// Which descriptors hold an epoll, which is what a close consults instead of
/// walking the table to find them.
///
/// This invariant is exact and not one-sided like `next_fd`'s: a descriptor
/// missing from the set is an epoll a close would fail to tell, and a
/// descriptor left in it is a plan built for an epoll that is not there. So
/// every case below checks the set against the TABLE rather than against what
/// the test expects, and `dup2` over a live epoll is in there because that is
/// the one path that has to REMOVE a number the insert did not add.
mod epoll_fds {
    use super::*;
    use crate::fs::Epoll;

    /// The set, read off the table itself: the descriptors whose file really
    /// is an epoll. Whatever the bookkeeping says, this is the answer.
    fn epolls_in_the_table(proc: &LinuxProcess) -> Vec<usize> {
        let inner = proc.inner.lock();
        let mut fds: Vec<usize> = inner
            .files
            .iter()
            .filter(|(_, f)| (*f).clone().downcast_arc::<Epoll>().is_ok())
            .map(|(fd, _)| usize::from(*fd))
            .collect();
        fds.sort_unstable();
        fds
    }

    /// And what the bookkeeping says.
    fn bookkeeping(proc: &LinuxProcess) -> Vec<usize> {
        let inner = proc.inner.lock();
        let mut fds: Vec<usize> = inner.epoll_fds.iter().map(|fd| usize::from(*fd)).collect();
        fds.sort_unstable();
        fds
    }

    fn assert_in_step(proc: &LinuxProcess, step: &str) {
        assert_eq!(
            bookkeeping(proc),
            epolls_in_the_table(proc),
            "the epoll set and the table disagree after {}",
            step
        );
    }

    /// A process with no epoll has an empty set, which is the case that makes
    /// a close cost nothing at all.
    #[test]
    fn a_process_with_no_epoll_has_nothing_in_the_set() {
        let proc = a_process();
        open_n(&proc, 5);
        assert!(bookkeeping(&proc).is_empty());
        assert_in_step(&proc, "five ordinary opens");
    }

    /// `epoll_create` puts its descriptor in, and a close takes it out again.
    #[test]
    fn an_epoll_goes_in_when_opened_and_out_when_closed() {
        let proc = a_process();
        open_n(&proc, 3);
        let ep = proc.add_file(Epoll::new(OpenFlags::empty())).unwrap();
        assert_eq!(bookkeeping(&proc), vec![usize::from(ep)]);
        assert_in_step(&proc, "epoll_create");

        proc.close_file(ep).expect("it is open");
        assert!(bookkeeping(&proc).is_empty());
        assert_in_step(&proc, "closing the epoll");
    }

    /// `dup2(other, epollfd)`: the epoll is gone from that number and an
    /// ordinary file is there instead. Nothing was ADDED, so only a removal
    /// keeps the books straight -- and if it does not, every later close
    /// builds a plan for an epoll the table no longer holds.
    #[test]
    fn a_dup2_over_a_live_epoll_takes_it_out_of_the_set() {
        let proc = a_process();
        let ep = proc.add_file(Epoll::new(OpenFlags::empty())).unwrap();
        assert_eq!(bookkeeping(&proc), vec![usize::from(ep)]);

        proc.replace_file(ep, an_open_log(Log::new(), OpenFlags::RDWR), false)
            .expect("dup2 onto an open descriptor");
        assert!(
            bookkeeping(&proc).is_empty(),
            "the number still claims to hold an epoll"
        );
        assert_in_step(&proc, "dup2 of a file over the epoll");
    }

    /// And the other way round: `dup2(epollfd, other)` puts an epoll under a
    /// number that held an ordinary file.
    #[test]
    fn a_dup2_of_an_epoll_over_a_file_puts_it_in_the_set() {
        let proc = a_process();
        let fds = open_n(&proc, 3);
        proc.replace_file(fds[1], Epoll::new(OpenFlags::empty()), false)
            .expect("dup2 onto an open descriptor");
        assert_eq!(bookkeeping(&proc), vec![usize::from(fds[1])]);
        assert_in_step(&proc, "dup2 of an epoll over a file");
    }

    /// Two epolls under three descriptors, two of them the SAME epoll through
    /// `dup`: both numbers are in the set, and closing one leaves the other.
    #[test]
    fn a_dup_of_an_epoll_is_two_descriptors_in_the_set() {
        let proc = a_process();
        let ep = Epoll::new(OpenFlags::empty());
        let a = proc.add_file(ep.clone()).unwrap();
        let b = proc.add_file_cloexec(ep, false).unwrap();
        assert_eq!(bookkeeping(&proc), vec![usize::from(a), usize::from(b)]);

        proc.close_file(a).expect("it is open");
        assert_eq!(bookkeeping(&proc), vec![usize::from(b)]);
        assert_in_step(&proc, "closing one of two descriptors for one epoll");
    }

    /// The sweeps take their descriptors out too: `close_range` over a range
    /// holding an epoll, and the execve sweep over an `O_CLOEXEC` one.
    #[test]
    fn the_sweeps_take_their_epolls_out_of_the_set() {
        let proc = a_process();
        open_n(&proc, 3);
        let swept = proc.add_file(Epoll::new(OpenFlags::empty())).unwrap();
        let on_exec = proc
            .add_file_cloexec(Epoll::new(OpenFlags::empty()), true)
            .unwrap();
        assert_eq!(bookkeeping(&proc).len(), 2);

        proc.close_range(swept, swept);
        assert_eq!(bookkeeping(&proc), vec![usize::from(on_exec)]);
        assert_in_step(&proc, "close_range over an epoll");

        proc.remove_cloexec_files();
        assert!(bookkeeping(&proc).is_empty());
        assert_in_step(&proc, "the execve sweep over an O_CLOEXEC epoll");
    }

    /// Emptying the table at death clears the set with it, the same way it
    /// resets the allocator's bound.
    #[test]
    fn emptying_the_table_clears_the_set() {
        let proc = a_process();
        proc.add_file(Epoll::new(OpenFlags::empty())).unwrap();
        let mut inner = proc.inner.lock();
        inner.take_all_files();
        assert!(inner.epoll_fds.is_empty());
    }

    /// `fork` copies the table, so the child's epoll descriptors are the
    /// parent's. A child whose set came up empty would never tell its
    /// inherited epoll about a close.
    #[test]
    fn a_forked_child_inherits_which_descriptors_are_epolls() {
        let parent = a_process();
        open_n(&parent, 2);
        let ep = parent.add_file(Epoll::new(OpenFlags::empty())).unwrap();

        let inner = parent.inner.lock();
        let child = inner.forked_child(0, 0, 0);
        assert_eq!(
            child
                .epoll_fds
                .iter()
                .map(|fd| usize::from(*fd))
                .collect::<Vec<_>>(),
            vec![usize::from(ep)]
        );
    }
}

/// The CABLING between a close and the epolls watching what it closed, which
/// is what `epoll_forget_plan` is for.
///
/// `Epoll::forget_closed` has its own tests in `fs::epoll`; what had none is
/// the wiring -- that a `close(2)` on this process reaches the epoll at all,
/// and that it reaches it only when the description really is gone. The test
/// reads the effect through `forget_closed`'s own count: it answers how many
/// entries it dropped, so **zero** means the close already did it and **one**
/// means the close did not.
mod a_close_tells_the_epolls {
    use super::*;
    use crate::fs::{Epoll, EventFd};

    const EPOLL_CTL_ADD: i32 = 1;

    fn watch(ep: &Arc<Epoll>, fd: FileDesc, file: Arc<dyn FileLike>) {
        ep.ctl(
            EPOLL_CTL_ADD,
            fd,
            crate::fs::EpollEvent { events: 1, data: 0 },
            Some(file),
        )
        .expect("an eventfd can be watched");
    }

    /// The plain case: one epoll, one watched descriptor, closed.
    #[test]
    fn closing_a_watched_descriptor_drops_its_entry() {
        let proc = a_process();
        let ep = Epoll::new(OpenFlags::empty());
        proc.add_file(ep.clone()).unwrap();
        let watched: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::empty());
        let fd = proc.add_file(watched.clone()).unwrap();
        watch(&ep, fd, watched.clone());

        proc.close_file(fd).expect("it is open");
        assert_eq!(
            ep.forget_closed(&watched),
            0,
            "the close did not tell the epoll"
        );
    }

    /// `close_range` over the watched descriptor does it too.
    #[test]
    fn a_swept_range_tells_the_epolls_about_what_it_closed() {
        let proc = a_process();
        let ep = Epoll::new(OpenFlags::empty());
        proc.add_file(ep.clone()).unwrap();
        let watched: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::empty());
        let fd = proc.add_file(watched.clone()).unwrap();
        watch(&ep, fd, watched.clone());

        proc.close_range(fd, fd);
        assert_eq!(ep.forget_closed(&watched), 0);
    }

    /// And the rule that makes the whole plan more than "drop it on close":
    /// a `dup` of the watched descriptor still holds the description, so
    /// closing one of the two must NOT drop the entry. epoll(7) says the
    /// program has to `EPOLL_CTL_DEL` in that case, and Linux drops the entry
    /// from `__fput`, when the last reference goes.
    #[test]
    fn closing_one_of_two_descriptors_for_a_watched_file_keeps_the_entry() {
        let proc = a_process();
        let ep = Epoll::new(OpenFlags::empty());
        proc.add_file(ep.clone()).unwrap();
        let watched: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::empty());
        let first = proc.add_file(watched.clone()).unwrap();
        let second = proc.add_file_cloexec(watched.clone(), false).unwrap();
        watch(&ep, first, watched.clone());

        proc.close_file(first).expect("it is open");
        assert_eq!(
            ep.forget_closed(&watched),
            1,
            "the dup at {} still holds the description, so the entry stays",
            usize::from(second)
        );
    }

    /// A process with no epoll in its table: nothing to tell, and the close
    /// must not pretend otherwise. This is the path the `epoll_fds` set made
    /// free, so it is also the one that would silently stop working if the
    /// set were ever wrong in the other direction.
    #[test]
    fn a_close_with_no_epoll_in_the_table_tells_nobody() {
        let proc = a_process();
        // The epoll exists but belongs to no descriptor of this table, which
        // is as far as this process's close is concerned the same as nothing.
        let ep = Epoll::new(OpenFlags::empty());
        let watched: Arc<dyn FileLike> = EventFd::new(0, OpenFlags::empty());
        let fd = proc.add_file(watched.clone()).unwrap();
        watch(&ep, fd, watched.clone());

        proc.close_file(fd).expect("it is open");
        assert_eq!(
            ep.forget_closed(&watched),
            1,
            "a close told an epoll no descriptor of this table holds"
        );
    }
}

/// How many descriptors hold each open file description, which is the one
/// question a close asks before it tells an epoll anything.
///
/// Like `epoll_fds`, the invariant is exact, and every case checks the map
/// against the TABLE rather than against what the test expects. The cases
/// that matter are the ones where a descriptor and a description part company
/// at different moments: a `dup` (one description, two numbers), and a `dup2`
/// over a live descriptor, where one description loses a holder and another
/// gains one under the SAME number.
mod holders {
    use super::*;
    use alloc::collections::BTreeMap;

    /// The count read off the table itself: address of the description ->
    /// how many of its descriptors point at it.
    fn counted_in_the_table(proc: &LinuxProcess) -> BTreeMap<usize, usize> {
        let inner = proc.inner.lock();
        let mut counts = BTreeMap::new();
        for f in inner.files.values() {
            *counts
                .entry(Arc::as_ptr(f) as *const () as usize)
                .or_insert(0) += 1;
        }
        counts
    }

    fn bookkeeping(proc: &LinuxProcess) -> BTreeMap<usize, usize> {
        proc.inner
            .lock()
            .holders
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect()
    }

    fn assert_in_step(proc: &LinuxProcess, step: &str) {
        assert_eq!(
            bookkeeping(proc),
            counted_in_the_table(proc),
            "the holder counts and the table disagree after {}",
            step
        );
    }

    /// Distinct files: one holder each, and the map empties as they close.
    #[test]
    fn distinct_files_are_one_holder_each() {
        let proc = a_process();
        let fds = open_n(&proc, 4);
        assert_eq!(bookkeeping(&proc).len(), 4);
        assert!(bookkeeping(&proc).values().all(|n| *n == 1));
        assert_in_step(&proc, "four distinct opens");

        for fd in fds {
            proc.close_file(fd).expect("it is open");
        }
        assert!(
            bookkeeping(&proc).is_empty(),
            "a description nobody holds must not stay in the map"
        );
    }

    /// A `dup`: one description, two numbers, and it is still held after the
    /// first of them closes. This is the rule epoll(7) is built on.
    #[test]
    fn a_dup_is_two_holders_of_one_description() {
        let proc = a_process();
        let file = an_open_log(Log::new(), OpenFlags::RDWR);
        let first = proc.add_file(file.clone()).unwrap();
        let second = proc.add_file_cloexec(file.clone(), false).unwrap();
        assert_eq!(
            bookkeeping(&proc).values().copied().collect::<Vec<_>>(),
            vec![2]
        );
        assert_in_step(&proc, "a dup");

        proc.close_file(first).expect("it is open");
        assert!(
            proc.inner.lock().holds_description(&file),
            "the descriptor at {} still holds it",
            usize::from(second)
        );
        assert_in_step(&proc, "closing one of the two");

        proc.close_file(second).expect("it is open");
        assert!(!proc.inner.lock().holds_description(&file));
        assert_in_step(&proc, "closing the second");
    }

    /// `dup2` over a live descriptor: under one number, one description loses
    /// its holder and another gains one. Counting only the insert would leave
    /// the replaced description claimed for ever, and every epoll watching it
    /// would keep an entry for a file nobody holds.
    #[test]
    fn a_dup2_moves_the_holder_from_one_description_to_the_other() {
        let proc = a_process();
        let replaced = an_open_log(Log::new(), OpenFlags::RDWR);
        let fd = proc.add_file(replaced.clone()).unwrap();
        let installed = an_open_log(Log::new(), OpenFlags::RDWR);

        proc.replace_file(fd, installed.clone(), false)
            .expect("dup2 onto an open descriptor");
        {
            let inner = proc.inner.lock();
            assert!(
                !inner.holds_description(&replaced),
                "the replaced description is still claimed"
            );
            assert!(inner.holds_description(&installed));
        }
        assert_in_step(&proc, "a dup2 over a live descriptor");
    }

    /// `dup2(fd, fd)` -- Linux's no-op. The number keeps the same description,
    /// so the count has to come out at one and not at zero or two.
    #[test]
    fn a_dup2_of_a_descriptor_onto_itself_leaves_one_holder() {
        let proc = a_process();
        let file = an_open_log(Log::new(), OpenFlags::RDWR);
        let fd = proc.add_file(file.clone()).unwrap();
        proc.replace_file(fd, file.clone(), false)
            .expect("dup2(fd, fd)");
        assert!(proc.inner.lock().holds_description(&file));
        assert_in_step(&proc, "dup2 of a descriptor onto itself");
        proc.close_file(fd).expect("it is open");
        assert!(!proc.inner.lock().holds_description(&file));
    }

    /// The sweeps and `fork`, which are the paths that move many descriptors
    /// at once. The child's table is a copy, so every description it holds is
    /// held twice in total -- but each table counts only its OWN descriptors,
    /// because an epoll of this process is told about this process's closes.
    #[test]
    fn the_sweeps_and_a_fork_keep_the_counts_in_step() {
        let proc = a_process();
        let shared = an_open_log(Log::new(), OpenFlags::RDWR);
        proc.add_file(shared.clone()).unwrap();
        proc.add_file_cloexec(shared.clone(), true).unwrap();
        open_n(&proc, 4);

        let child = proc.inner.lock().forked_child(0, 0, 0);
        assert_eq!(
            child.holders,
            proc.inner.lock().holders,
            "the child's table is a copy, so its counts are the parent's"
        );

        proc.remove_cloexec_files();
        assert_in_step(&proc, "the execve sweep");
        assert!(
            proc.inner.lock().holds_description(&shared),
            "the descriptor that was not close-on-exec still holds it"
        );

        proc.close_range(FileDesc::from(0), FileDesc::from(i32::MAX));
        assert!(bookkeeping(&proc).is_empty());
        assert_in_step(&proc, "closing everything");
    }
}

/// `add_file_at` installs under a number the CALLER chose, and nothing
/// promises that number is free. Installing over a live descriptor therefore
/// has to free what was there, the way `dup2` does.
///
/// A bare `HashMap::insert` drops the old file and leaves its `holders` count
/// behind, and that count is the whole of what a close asks before it tells
/// an epoll anything: a description that looks held forever is one whose
/// epoll entries are never dropped. The leak is not visible in the table --
/// the right file is at the right number either way -- so it takes asking
/// `holds_description` directly.
#[test]
fn installing_over_a_live_number_does_not_leave_the_old_description_held() {
    let proc = a_process();
    let fds = open_n(&proc, 3);
    let old = proc.get_file_like(fds[1]).expect("it is open");
    assert!(
        proc.inner.lock().holds_description(&old),
        "the table has to START holding it, or nothing below is testing anything"
    );

    proc.add_file_at(fds[1], an_open_log(Log::new(), OpenFlags::RDWR))
        .expect("well inside the default limit");

    assert!(
        !proc.inner.lock().holds_description(&old),
        "the description that got dropped is still counted as held, so every \
         later close will think some descriptor of ours still has it open"
    );
    // And it was a replace, not a leak of the NUMBER too: the new file is at
    // that number and the table is the same size it was.
    assert!(
        proc.get_file_like(fds[1]).is_ok(),
        "nothing is at the number"
    );
    assert_eq!(proc.inner.lock().files.len(), 3);
}
