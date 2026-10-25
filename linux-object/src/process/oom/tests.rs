//! Who may move the OOM adjustment, and how far (`__set_oom_adj`, Linux
//! 6.18, `procfiles-survey`). Where it goes with the process is checked
//! with the rest of a fork's inheritance (`fork_inheritance_tests`) and
//! here for an exec.

use super::*;

fn at(adj: i16, min: i16) -> OomAdj {
    OomAdj { adj, min }
}

#[test]
fn without_the_capability_it_goes_down_to_its_floor_and_no_further() {
    let mut oom = at(100, 50);
    assert!(oom.set(50, false, false));
    assert_eq!(oom, at(50, 50));
    assert!(!oom.set(49, false, false));
    assert_eq!(oom, at(50, 50), "a refused write changes nothing");
    assert!(oom.set(1000, false, false));
    assert_eq!(oom, at(1000, 50), "nor does an allowed one move the floor");
}

#[test]
fn with_it_the_value_goes_anywhere_and_takes_the_floor_along() {
    let mut oom = at(100, 50);
    assert!(oom.set(-1000, false, true));
    assert_eq!(oom, at(-1000, -1000));
    // Up as well: the floor is wherever a capable writer last put it.
    assert!(oom.set(300, false, true));
    assert_eq!(oom, at(300, 300));
}

#[test]
fn the_old_file_may_not_lower_it_at_all_without_the_capability() {
    let mut oom = at(100, 0);
    assert!(
        !oom.set(99, true, false),
        "above the floor, below the value"
    );
    assert_eq!(oom, at(100, 0));
    assert!(oom.set(100, true, false));
    assert!(oom.set(470, true, false));
    assert_eq!(oom, at(470, 0));
    assert!(oom.set(-1000, true, true));
    assert_eq!(oom, at(-1000, 0), "and with it, it never moves the floor");
}

#[test]
fn an_exec_keeps_the_adjustment_and_its_floor() {
    // `signal_struct` outlives an exec; the survey's `cat` reads the value
    // its shell wrote.
    let mut inner = LinuxProcessInner {
        oom: at(123, 45),
        ..Default::default()
    };
    inner.reset_for_exec(true);
    assert_eq!(inner.oom, at(123, 45));
}

#[test]
fn every_thread_of_the_process_sees_one_value() {
    let lp = LinuxProcess::new(rcore_fs_ramfs::RamFS::new(), 0);
    assert_eq!(lp.oom_adj(), at(0, 0), "a new process asks nothing");
    assert!(lp.set_oom_adj(-500, false, true));
    assert!(!lp.set_oom_adj(-600, false, false));
    assert_eq!(lp.oom_adj(), at(-500, -500));
}
