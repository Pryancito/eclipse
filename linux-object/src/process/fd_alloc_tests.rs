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
