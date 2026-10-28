//! The two decisions a kill is: whom, and whether to kill at all. Both are
//! taken on numbers rather than on a machine, which is what lets them be
//! tested here -- a host test has no job tree to fill with processes.

use super::*;

/// Serializes the tests that drive [`DYING`], which is one figure for the
/// machine and cannot be built per test.
static DYING_TEST_LOCK: lock::Mutex<()> = lock::Mutex::new(());

fn v(pid: KoID, score: u64, held_bytes: u64) -> Victim {
    Victim {
        pid,
        score,
        held_bytes,
    }
}

#[test]
fn with_nobody_to_pick_there_is_no_victim() {
    assert_eq!(worst_of([]), None);
}

#[test]
fn the_worst_score_is_the_one_picked() {
    let worst = worst_of([
        v(10, 700, 1 << 20),
        v(11, 1400, 2 << 20),
        v(12, 900, 9 << 20),
    ]);
    assert_eq!(worst, Some(v(11, 1400, 2 << 20)), "not the worst score");
}

/// A score of zero is not a candidate at all: that is how `init` and a process
/// at `OOM_SCORE_ADJ_MIN` are kept unpickable rather than merely unlikely, and
/// `oom_badness` answers zero for exactly those two.
#[test]
fn a_process_scoring_zero_is_never_picked() {
    assert_eq!(worst_of([v(1, 0, 512 << 20)]), None, "init was picked");
    assert_eq!(
        worst_of([v(1, 0, 512 << 20), v(7, 4, 1 << 20)]),
        Some(v(7, 4, 1 << 20)),
        "the only candidate was passed over for one scoring zero"
    );
    assert_eq!(
        oom_badness(INIT_PID, 1000, 1 << 20, 1 << 20),
        0,
        "init scored above zero"
    );
    assert_eq!(
        oom_badness(42, OOM_SCORE_ADJ_MIN, 1 << 20, 1 << 20),
        0,
        "a process at the floor scored above zero"
    );
}

/// Ties are broken on held bytes and then on the pid, so that the same machine
/// in the same state kills the same process. A tie broken by the order the job
/// tree happened to be walked in makes an OOM report nobody can reproduce.
#[test]
fn a_tie_is_broken_on_what_is_held_and_then_on_the_pid() {
    // The LOWER pid holds more, so held bytes and pid disagree and only a
    // ranking that reads held bytes can pick the right one.
    assert_eq!(
        worst_of([v(10, 1400, 8 << 20), v(11, 1400, 1 << 20)]),
        Some(v(10, 1400, 8 << 20)),
        "the tie did not go to the one holding more"
    );
    // Reversed input order, same answer: the ranking is not the walk's.
    assert_eq!(
        worst_of([v(11, 1400, 1 << 20), v(10, 1400, 8 << 20)]),
        Some(v(10, 1400, 8 << 20)),
        "the answer depended on the order the rows came in"
    );
    // A complete tie, both ways round: the higher pid, whichever order the
    // rows came in. Without the pid in the key the answer is simply the last
    // row, so one order would agree by accident.
    for rows in [
        [v(10, 1400, 4 << 20), v(11, 1400, 4 << 20)],
        [v(11, 1400, 4 << 20), v(10, 1400, 4 << 20)],
    ] {
        assert_eq!(
            worst_of(rows),
            Some(v(11, 1400, 4 << 20)),
            "a complete tie was not broken on the pid"
        );
    }
}

/// Nobody has been killed yet, so nothing is being waited for.
#[test]
fn with_nobody_killed_yet_a_kill_is_not_held_back() {
    let _g = DYING_TEST_LOCK.lock();
    DYING.store(0, Ordering::Relaxed);
    assert!(!still_dying([1, 2, 3]));
    // ... and pid 0 is not a pid: a machine whose process 0 was somehow alive
    // must not hold every kill back for ever.
    assert!(!still_dying([0]));
}

/// A victim that is still on the census is still on its way out, and a second
/// kill waits for it. `SIGKILL` is asynchronous -- it has to reach the
/// process, every thread has to leave the kernel, and only then do the pages
/// come back -- so without this a machine briefly short of memory loses every
/// process it has, in order of size.
#[test]
fn a_victim_that_has_not_left_yet_holds_the_next_kill_back() {
    let _g = DYING_TEST_LOCK.lock();
    DYING.store(77, Ordering::Relaxed);
    assert!(
        still_dying([12, 77, 90]),
        "a kill was allowed while the last victim was still alive"
    );
    DYING.store(0, Ordering::Relaxed);
}

/// ... and once it is gone, the next kill may go ahead. The census is what
/// says so, not a flag the dying process clears: a process that is leaving
/// cannot be relied on to say so, and a flag nothing clears is a killer that
/// kills once per boot.
#[test]
fn once_the_victim_is_gone_the_next_kill_may_go_ahead() {
    let _g = DYING_TEST_LOCK.lock();
    DYING.store(77, Ordering::Relaxed);
    assert!(
        !still_dying([12, 90]),
        "the killer kept waiting for a process that had already gone"
    );
    DYING.store(0, Ordering::Relaxed);
}

/// The badness itself, on the scale `/proc/<pid>/oom_score` reports: a process
/// of no size and no adjustment is 666, and the adjustment moves it by its
/// thousandths of all memory.
#[test]
fn the_badness_is_the_share_of_memory_held_plus_the_adjustment() {
    let total = 1_000_000u64;
    assert_eq!(oom_badness(9, 0, 0, total), 666, "the floor of the scale");
    assert_eq!(
        oom_badness(9, 0, total, total),
        (1000 + 1000) * 2 / 3,
        "a process holding all of memory is not at the top"
    );
    // An adjustment of +1000 counts as all of memory again, so the top of the
    // scale -- 2000 -- is a process that holds everything AND asks to be
    // picked first.
    assert_eq!(
        oom_badness(9, 1000, 0, total),
        (1000 + 1000) * 2 / 3,
        "the adjustment is not counted as all of memory"
    );
    assert_eq!(
        oom_badness(9, 1000, total, total),
        2000,
        "the top of the scale"
    );
    // A negative adjustment takes it down, and never below zero.
    assert!(
        oom_badness(9, -500, 0, total) < 666,
        "a negative adjustment did not help"
    );
}
