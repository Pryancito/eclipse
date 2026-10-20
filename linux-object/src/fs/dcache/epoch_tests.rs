//! The cache that decides what inode an absolute `open` gets back, and what
//! that inode is called.
//!
//! It had no test of its own. Of the two things it has to get right, only
//! one was reached at all, and from far away: three tests elsewhere call
//! `dcache_invalidate` and so notice if an invalidation does nothing. The
//! other -- that a hit gives back the inode that was put in, under the name
//! the walk ended at -- was reached by nothing, so a cache that never hit,
//! or that emptied itself on every insert, was indistinguishable from this
//! one by the whole suite.
//!
//! The cache is GLOBAL and `cargo test` runs in parallel, so no assertion
//! here asks whether something is absent *because of us*: an absence a
//! concurrent namespace mutation could also have caused is asserted
//! directly, and anything that has to be PRESENT is asked for in a loop
//! that keeps the first round another test did not clear.

use super::*;
use alloc::format;
use rcore_fs::vfs::FileSystem;
use rcore_fs_ramfs::RamFS;

fn an_inode() -> Arc<dyn INode> {
    RamFS::new().root_inode()
}

#[test]
fn a_hit_gives_back_the_inode_under_the_name_the_walk_ended_at() {
    let inode = an_inode();
    // A path that is its own name, and one that went through a link, which
    // is the only reason a name is kept beside the inode.
    let plain = "/dcache-test/by-its-own-name";
    let link = "/dcache-test/through-a-link";
    let leads_to = "/dcache-test/where-the-link-leads";
    let mut rounds = 0;
    for _ in 0..64 {
        put(plain, &inode, &Walked::at(plain));
        put(link, &inode, &Walked::at(leads_to));
        let root = an_inode();
        let mut one = Walked {
            root: Some(root.clone()),
            ..Walked::default()
        };
        let mut two = Walked::at("/");
        // Both in the same round: a cache that cleared itself before every
        // insert would answer one of the two and never both.
        let (Some(a), Some(b)) = (get(plain, &mut one), get(link, &mut two)) else {
            continue;
        };
        assert!(Arc::ptr_eq(&a, &inode), "the inode that was put in");
        assert!(Arc::ptr_eq(&b, &inode));
        assert_eq!(one.path(), plain, "a path that is its own name");
        assert_eq!(
            two.path(),
            leads_to,
            "the name the walk ended at, not the path asked for"
        );
        // Where an absolute symlink target starts over is the root of whoever
        // walks, which a hit leaves as it was.
        assert!(one.root.as_ref().is_some_and(|r| Arc::ptr_eq(r, &root)));
        assert!(two.root.is_none());
        rounds += 1;
    }
    assert!(rounds > 0, "not one round of 64 cached anything");
}

#[test]
fn an_invalidation_is_not_undone_by_the_lookup_that_notices_it() {
    let inode = an_inode();
    let path = "/dcache-test/unlinked-right-after";
    put(path, &inode, &Walked::at(path));
    invalidate();
    // The lookup that sees the epoch move answers `None`. What must not
    // happen is that it leaves the entry behind, because the lookup after
    // that one is in the new epoch with the old contents and would serve the
    // inode of a path that has just been unlinked, renamed or mounted over.
    assert!(
        get(path, &mut Walked::default()).is_none(),
        "the lookup that notices"
    );
    assert!(
        get(path, &mut Walked::default()).is_none(),
        "the one after it"
    );
    assert!(
        get(path, &mut Walked::default()).is_none(),
        "and the one after that"
    );
}

#[test]
fn a_session_start_fits_and_a_full_cache_goes_whole() {
    let inode = an_inode();
    let first = "/dcache-test/cap/the-first-one";
    // 4096 entries, and the number is the point: a desktop session start
    // (ldso across some fifty libraries, xkb, fontconfig, icon themes)
    // touches more than 1024 distinct paths, and reaching the cap drops the
    // WHOLE cache in the middle of exactly that storm. Written out here
    // rather than taken from `MAX_ENTRIES`, which is the constant this pins.
    let fits = 2000;
    let mut held = false;
    for _ in 0..4 {
        // From an empty map, as any namespace mutation leaves it.
        invalidate();
        put(first, &inode, &Walked::at(first));
        for i in 0..fits {
            let p = format!("/dcache-test/cap/{}", i);
            put(&p, &inode, &Walked::at(&p));
        }
        if get(first, &mut Walked::default()).is_some() {
            held = true;
            break;
        }
    }
    assert!(held, "2001 paths have to fit under a cap of 4096");

    // And past the cap everything goes at once: refilling is one miss per
    // path, which is cheaper than keeping track of which path was used last.
    for i in 0..4100 {
        let p = format!("/dcache-test/over/{}", i);
        put(&p, &inode, &Walked::at(&p));
    }
    assert!(
        get(first, &mut Walked::default()).is_none(),
        "a full cache is dropped whole"
    );
}

#[test]
fn a_walk_through_proc_or_a_magic_link_is_not_kept() {
    let inode = an_inode();
    let plain = "/dcache-test/kept-beside-them";
    let into_proc = "/dcache-test/link-to-proc-self-stat";
    let jumped = "/dcache-test/link-to-proc-self-cwd-f";
    let mut rounds = 0;
    for _ in 0..64 {
        // What each finds is another file to each process: `/proc/self/stat`
        // is the stat of whoever opens it, and a magic link leads to what the
        // process holds.
        put(into_proc, &inode, &Walked::at("/proc/45/stat"));
        let mut through = Walked::at("/home/f");
        through.jumped = true;
        put(jumped, &inode, &through);
        put(plain, &inode, &Walked::at(plain));
        // Only a round where the plain one was kept says anything about the
        // other two: an invalidation from a test alongside empties all three.
        if get(plain, &mut Walked::default()).is_none() {
            continue;
        }
        assert!(
            get(into_proc, &mut Walked::default()).is_none(),
            "into /proc"
        );
        assert!(
            get(jumped, &mut Walked::default()).is_none(),
            "through a magic link"
        );
        rounds += 1;
    }
    assert!(rounds > 0, "not one round of 64 cached anything");
}
