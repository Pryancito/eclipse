//! The ELF image cache's shrinker runs on a thread whose allocation has just
//! failed, so the two things it has to get right are both about honesty: it
//! must not count memory that dropping an entry would not actually free, and
//! it must not report bytes it did not free. A reclaim that lies makes the
//! retry fail twice instead of once, and then the OOM report quotes the lie.

use super::{ElfVmoKey, ELF_VMO_CACHE};
use alloc::sync::Arc;
use kernel_hal::shrinker::Shrinker;
use zircon_object::vm::VmObject;

/// The cache is one `static`; serialise, and start from empty.
#[must_use = "bind it to `_alone`: a bare `_` releases the turnstile at once"]
fn alone() -> spin::MutexGuard<'static, ()> {
    static GUARD: spin::Mutex<()> = spin::Mutex::new(());
    let guard = GUARD.lock();
    let mut c = ELF_VMO_CACHE.lock();
    c.map.clear();
    c.fifo.clear();
    c.bytes = 0;
    guard
}

const PAGE: usize = 4096;

fn key(inode: usize, pages: usize) -> ElfVmoKey {
    (1, inode, pages * PAGE, 0, 0)
}

/// Put an entry in the cache by hand, as `insert` would, and answer the VMO so
/// a test can decide whether anyone else still holds it.
fn put(inode: usize, pages: usize) -> (ElfVmoKey, Arc<VmObject>) {
    let k = key(inode, pages);
    let vmo = VmObject::new_paged(pages);
    let mut c = ELF_VMO_CACHE.lock();
    c.fifo.push(k);
    c.map.insert(k, vmo.clone());
    c.bytes += k.2;
    (k, vmo)
}

fn shrinker() -> &'static Shrinker {
    &super::elf_vmo_shrinker::SHRINKER
}

fn count() -> usize {
    (shrinker().count)()
}
fn scan(want: usize) -> usize {
    (shrinker().scan)(want)
}

#[test]
fn an_empty_cache_offers_nothing_and_gives_nothing() {
    let _alone = alone();
    assert_eq!(count(), 0);
    assert_eq!(scan(16 * PAGE), 0);
}

/// The honesty that matters: an entry whose VMO a running process still maps
/// frees NOTHING when the cache drops it, because the process holds the other
/// reference. Counting it promises the reclaim path memory that is not there.
#[test]
fn an_entry_somebody_else_still_holds_is_not_counted() {
    let _alone = alone();
    let (_k, held) = put(1, 4);
    assert_eq!(
        count(),
        0,
        "an entry with a live second reference was counted as reclaimable"
    );
    drop(held);
    assert_eq!(
        count(),
        4 * PAGE,
        "the entry became reclaimable when its other holder went away, and \
         the count did not notice"
    );
}

#[test]
fn the_count_is_the_bytes_of_the_entries_nobody_else_holds() {
    let _alone = alone();
    let (_k1, held) = put(1, 2);
    let (_k2, v2) = put(2, 3);
    let (_k3, v3) = put(3, 5);
    drop(v2);
    drop(v3);
    assert_eq!(count(), (3 + 5) * PAGE);
    drop(held);
    assert_eq!(count(), (2 + 3 + 5) * PAGE);
}

/// Oldest first, because the FIFO order is the only recency signal this cache
/// keeps. Dropping the newest entry evicts the binary most likely to be exec'd
/// again in the next moment.
#[test]
fn entries_are_dropped_oldest_first() {
    let _alone = alone();
    let (old, v_old) = put(1, 2);
    let (new, v_new) = put(2, 2);
    drop(v_old);
    drop(v_new);

    assert_eq!(scan(2 * PAGE), 2 * PAGE);
    let c = ELF_VMO_CACHE.lock();
    assert!(
        !c.map.contains_key(&old),
        "the oldest entry survived the reclaim"
    );
    assert!(
        c.map.contains_key(&new),
        "the newest entry was dropped while an older one was still cached"
    );
}

/// An entry still in use is skipped and LEFT WHERE IT IS in the FIFO, so it is
/// still the oldest when it does become free. Dropping it from the queue while
/// keeping it in the map would leak it: nothing would ever consider it again.
#[test]
fn an_entry_in_use_is_skipped_and_keeps_its_place_in_the_queue() {
    let _alone = alone();
    let (used, held) = put(1, 2);
    let (free, v_free) = put(2, 2);
    drop(v_free);

    assert_eq!(scan(2 * PAGE), 2 * PAGE, "the free entry was not reclaimed");
    {
        let c = ELF_VMO_CACHE.lock();
        assert!(c.map.contains_key(&used), "the entry in use was dropped");
        assert!(!c.map.contains_key(&free));
        assert_eq!(
            c.fifo.first(),
            Some(&used),
            "the entry in use lost its place at the front of the queue"
        );
        assert_eq!(c.fifo.len(), 1, "the queue and the map disagree");
    }
    // And once nobody holds it, the same shrinker takes it.
    drop(held);
    assert_eq!(scan(2 * PAGE), 2 * PAGE);
    assert_eq!(count(), 0);
}

/// Stop at the target. A shrinker that empties the cache whatever it was asked
/// for turns one shortage into a cold cache for the rest of the boot.
#[test]
fn the_scan_stops_once_the_target_is_met() {
    let _alone = alone();
    for inode in 1..=8 {
        let (_k, v) = put(inode, 2);
        drop(v);
    }
    assert_eq!(count(), 16 * PAGE);
    let freed = scan(4 * PAGE);
    assert_eq!(freed, 4 * PAGE, "the scan did not stop at the target");
    assert_eq!(
        count(),
        12 * PAGE,
        "the scan emptied more of the cache than it was asked for"
    );
}

/// The byte total the cache keeps for its own FIFO eviction has to come down
/// with the entries, or the next `insert` evicts against a figure that counts
/// memory the cache no longer holds.
#[test]
fn the_caches_own_byte_total_comes_down_with_what_was_reclaimed() {
    let _alone = alone();
    let (_k, v) = put(1, 4);
    drop(v);
    assert_eq!(ELF_VMO_CACHE.lock().bytes, 4 * PAGE);
    assert_eq!(scan(4 * PAGE), 4 * PAGE);
    assert_eq!(
        ELF_VMO_CACHE.lock().bytes,
        0,
        "the cache still thinks it holds the bytes it gave back"
    );
}
