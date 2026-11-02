//! There is exactly ONE `/dev` for the whole test binary.
//!
//! `DEVFS_ROOT` is process-wide and is what every `/dev` lookup resolves
//! against, so a `create_root_fs` that builds a second `DevFS` takes the
//! filesystem out from under any test standing in `/dev` -- which is what made
//! `process::cwd::tests::a_working_directory_in_dev_or_proc_is_the_one_absolute_paths_name`
//! read `None` where it had just put `Some("/dev")`.
//!
//! [`super::devfs_root_once`] is what stops that, and this is the test that
//! says so, because the defect it prevents is invisible until the suite runs in
//! parallel and then blames somebody else.

use super::*;

/// The identity of the `/dev` in the global slot, as a pointer, which is the
/// only thing that answers "the same one?" rather than "an equal one?".
fn dev_identity() -> *const DevINode {
    Arc::as_ptr(
        DEVFS_ROOT
            .lock()
            .as_ref()
            .expect("something has to have built /dev by now"),
    )
}

/// One process, then another: `/dev` is the same object, not a fresh one.
#[test]
fn making_a_process_does_not_rebuild_dev() {
    let _first = LinuxProcess::new(RamFS::new(), 0);
    let before = dev_identity();
    let _second = LinuxProcess::new(RamFS::new(), 0);
    assert_eq!(
        before,
        dev_identity(),
        "`LinuxProcess::new` built a second `DevFS` and put it in the \
         process-wide slot: every test standing in /dev just lost the \
         filesystem it was standing in"
    );
}

/// And the same from several threads at once, which is how the suite runs and
/// the only shape in which a check-then-store that is not under one lock goes
/// wrong: two threads both find the slot empty and both build.
#[test]
fn many_threads_making_processes_all_see_one_dev() {
    extern crate std;
    // Build it once up front, so what the threads race on is the reuse and not
    // which of them wins the first build.
    let _first = LinuxProcess::new(RamFS::new(), 0);
    let expected = dev_identity() as usize;

    let threads: alloc::vec::Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                let _proc = LinuxProcess::new(RamFS::new(), 0);
                dev_identity() as usize
            })
        })
        .collect();
    for (i, t) in threads.into_iter().enumerate() {
        assert_eq!(
            t.join().expect("the thread panicked"),
            expected,
            "thread {i} saw a different /dev",
        );
    }
    assert_eq!(dev_identity() as usize, expected, "and so does this one");
}

/// What the `/dev` of a second process actually resolves: the nodes the first
/// one made, by name, through the path a program takes.
#[test]
fn a_later_process_finds_the_dev_nodes_of_the_first() {
    let first = LinuxProcess::new(RamFS::new(), 0);
    let null_first = first
        .lookup_inode("/dev/null")
        .expect("/dev/null after the first process");
    let second = LinuxProcess::new(RamFS::new(), 0);
    let null_second = second
        .lookup_inode("/dev/null")
        .expect("/dev/null after the second");
    assert_eq!(
        null_first.metadata().unwrap().inode,
        null_second.metadata().unwrap().inode,
        "the second process got a different /dev/null, so it got a different \
         /dev"
    );
}
