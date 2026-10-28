//! Give the ELF image cache back under memory pressure.
//!
//! This cache is the kernel's largest discretionary one -- 128 MiB of warm
//! executables -- and until now nothing could make it give any of that back:
//! its only eviction was FIFO at insert time, so a kernel that was out of
//! memory stayed out of memory with 128 MiB of cached binaries in hand. That
//! is the shape that took the kernel heap from 256 MiB to 512 MiB.
//!
//! Registered with [`kernel_hal::shrinker`], whose one caller is an allocation
//! that has already failed. Two consequences for everything below:
//!
//! 1. **It must not allocate.** `BTreeMap::remove` and `Vec::remove` do not,
//!    and dropping an `Arc<VmObject>` returns frames through `frame_dealloc`,
//!    which is the frame allocator and not the heap. Nothing here builds a
//!    `Vec` of victims.
//! 2. **It must not wait.** `try_lock`: the allocation that failed may be the
//!    one this very cache's critical section was making, and waiting for a
//!    lock this CPU holds, with interrupts off, is the one outcome worse than
//!    an OOM report.
//!
//! The page cache (`fs::file`'s per-inode VMO registry) and the dentry cache
//! are deliberately NOT here yet. Evicting a page-cache entry runs
//! `writeback_shared_vmo` -- an inode write, a `vec![0u8; PAGE_SIZE]`, and the
//! page faults the VMO read triggers -- which is exactly what a failed
//! allocation cannot do; it needs a deferred worker, not a shrinker call.

use super::{Arc, ELF_VMO_CACHE};

/// Bytes this cache could give back *right now*.
///
/// Not `cache.bytes`: an entry whose VMO is still mapped by a running
/// process frees nothing when the cache drops it, because the process
/// holds the other reference. Counting those would promise the reclaim
/// path memory that does not exist, and a reclaim that reports bytes it
/// did not free makes the retry fail twice instead of once.
fn count() -> usize {
    let Some(cache) = ELF_VMO_CACHE.try_lock() else {
        return 0;
    };
    cache
        .map
        .iter()
        .filter(|(_, vmo)| Arc::strong_count(vmo) == 1)
        .map(|(key, _)| key.2)
        .sum()
}

/// Drop unshared entries, oldest first, until `want` bytes are gone.
///
/// Oldest first because the FIFO order is the only recency signal this
/// cache keeps, and the oldest warm binary is the one least likely to be
/// exec'd again in the next moment.
fn scan(want: usize) -> usize {
    let Some(mut cache) = ELF_VMO_CACHE.try_lock() else {
        return 0;
    };
    let mut freed = 0usize;
    let mut i = 0;
    while i < cache.fifo.len() && freed < want {
        let key = cache.fifo[i];
        let unshared = cache
            .map
            .get(&key)
            .is_some_and(|vmo| Arc::strong_count(vmo) == 1);
        if !unshared {
            // Still in use: leave it where it is in the FIFO so it is
            // still the oldest when it does become free.
            i += 1;
            continue;
        }
        cache.fifo.remove(i);
        if cache.map.remove(&key).is_some() {
            cache.bytes = cache.bytes.saturating_sub(key.2);
            freed += key.2;
        }
        // No `i += 1`: `remove` shifted the next entry into this index.
    }
    freed
}

pub static SHRINKER: kernel_hal::shrinker::Shrinker = kernel_hal::shrinker::Shrinker {
    name: "elf-image-cache",
    count,
    scan,
};
