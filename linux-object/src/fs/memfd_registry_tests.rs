//! The registry of live memfds is walked on every `write(2)` of every
//! file and grew with every memfd ever created: only the OOM report
//! pruned it. Now `new_memfd` prunes the dead entries as it goes.
//!
//! The bounds here are loose on purpose: other tests of this binary
//! create memfds too, and CI runs with `--test-threads=1` while a
//! developer may not.
use super::*;

fn registry_len() -> usize {
    MEMFD_LIVE.lock().len()
}

fn churn(n: usize) {
    for _ in 0..n {
        drop(new_memfd("churn", 0).unwrap());
    }
}

#[test]
fn dead_memfds_leave_the_registry_as_new_ones_are_created() {
    churn(5000);
    let len = registry_len();
    assert!(
        len < 1000,
        "{} entries in the registry after five thousand memfds came and went",
        len
    );
}

#[test]
fn a_live_memfd_survives_the_prune_with_its_seals() {
    let f = new_memfd("keymap", MFD_ALLOW_SEALING).unwrap();
    memfd_add_seals(&f.inode(), F_SEAL_SHRINK).unwrap();
    churn(5000);
    assert_eq!(
        memfd_seals(&f.inode()),
        Some(F_SEAL_SHRINK),
        "a memfd still open was pruned, or lost its seals"
    );
}

#[test]
fn the_registry_stays_within_a_multiple_of_what_is_alive() {
    let held: Vec<_> = (0..100).map(|_| new_memfd("held", 0).unwrap()).collect();
    churn(5000);
    let len = registry_len();
    // Two hundred is what a prune leaves room for with a hundred alive
    // (twice the survivors), and the rest is what other tests of this
    // binary may be holding.
    assert!(len <= 100 * 2 + MEMFD_PRUNE_MIN + 200, "{} entries", len);
    drop(held);
}
