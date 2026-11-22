use super::madvise_advice_known;

#[test]
fn known_advice_is_accepted() {
    // NORMAL/RANDOM/SEQUENTIAL/WILLNEED/DONTNEED/FREE and a few higher ones.
    for a in [0usize, 1, 2, 3, 4, 8, 12, 17, 21, 100, 101] {
        assert!(madvise_advice_known(a), "advice {} should be known", a);
    }
}

/// An advice this kernel cannot honour is refused, not answered 0: a
/// `MADV_WIPEONFORK` page that a child would keep is the difference
/// between a DRBG that reseeds after `fork` and one that repeats.
#[test]
fn advice_this_kernel_cannot_honour_is_refused_not_faked() {
    const MADV_REMOVE: usize = 9;
    const MADV_DONTFORK: usize = 10;
    const MADV_DOFORK: usize = 11;
    const MADV_WIPEONFORK: usize = 18;
    const MADV_KEEPONFORK: usize = 19;
    for a in [
        MADV_REMOVE,
        MADV_DONTFORK,
        MADV_DOFORK,
        MADV_WIPEONFORK,
        MADV_KEEPONFORK,
    ] {
        assert!(!madvise_advice_known(a), "advice {} is faked", a);
        assert!(super::UNHONOURED_MADVISE.contains(&a));
    }
    // And the two lists are disjoint: nothing is both honoured and not.
    for a in super::UNHONOURED_MADVISE {
        assert!(!super::KNOWN_MADVISE.contains(a), "{} in both lists", a);
    }
}

#[test]
fn unknown_advice_is_rejected() {
    // Gaps in the numbering (5,6,7) and out-of-range values are unknown.
    for a in [5usize, 6, 7, 22, 99, 102, usize::MAX] {
        assert!(!madvise_advice_known(a), "advice {} should be unknown", a);
    }
}
