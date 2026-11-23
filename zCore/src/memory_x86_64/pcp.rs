//! The per-CPU frame lists; see the note on `mod pcp` in `memory_x86_64.rs`.

use super::{Mutex, FRAMES_CACHED, FRAME_ALLOCATOR, PAGE_BITS};
use bitmap_allocator::BitAlloc;
use kernel_hal::cpu_topology::{percpu_slot, PERCPU_SLOTS};

/// `pcp->high`: frames one CPU may park. 65 slots x 128 frames is 32 MiB
/// of RAM the bitmap cannot hand out, which is why [`drain_all`] exists
/// and why it runs before any allocation failure is believed.
pub const CAPACITY: usize = 128;
/// `pcp->batch`: frames moved per trip to the bitmap. The whole point is
/// that one lock acquisition serves `CAPACITY / 1` allocations instead of
/// one, so this is what sets the amortisation.
pub const BATCH: usize = 32;

/// A frame index, not an address: `BitAlloc16M` tops out at 2^24 frames,
/// so a `u32` holds any of them and the whole table costs 33 KiB of BSS.
pub struct Pcp {
    frames: [u32; CAPACITY],
    len: usize,
}

impl Pcp {
    pub const fn new() -> Self {
        Pcp {
            frames: [0; CAPACITY],
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Park `idx`, or `false` when the list is full and the caller must
    /// make room.
    pub fn push(&mut self, idx: usize) -> bool {
        if self.len == CAPACITY || idx > u32::MAX as usize {
            return false;
        }
        self.frames[self.len] = idx as u32;
        self.len += 1;
        true
    }

    pub fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.frames[self.len] as usize)
    }
}

static LISTS: [Mutex<Pcp>; PERCPU_SLOTS] = [const { Mutex::new(Pcp::new()) }; PERCPU_SLOTS];

/// This CPU's list. Indexed by [`percpu_slot`], the dense slot the rest of
/// the kernel indexes per-CPU tables with, so a `cpu_id()` that names no
/// CPU lands in the quarantine slot rather than out of bounds.
#[inline]
fn mine() -> &'static Mutex<Pcp> {
    &LISTS[percpu_slot(kernel_hal::cpu::cpu_id() as usize)]
}

/// Bytes parked in all the lists. Not part of `FRAMES_USED`: nobody holds
/// these frames, so to a caller of `frame_stats` they are free -- which is
/// true, and is exactly why a reader of a failed allocation needs to be
/// told they exist.
pub fn cached_bytes() -> usize {
    FRAMES_CACHED.get()
}

/// A frame from this CPU's list, refilling it from the bitmap when empty.
///
/// `try_lock`, for the reason the heap's front cache uses it: a fault taken
/// inside this critical section would otherwise wait, with interrupts off,
/// for a release only this CPU could perform. A miss is a trip to the
/// bitmap, which is where every allocation used to go anyway.
pub fn alloc() -> Option<usize> {
    let mut p = mine().try_lock()?;
    if let Some(idx) = p.pop() {
        FRAMES_CACHED.sub(1 << PAGE_BITS);
        return Some(idx);
    }
    // Refill with the list still held and the bitmap taken inside it. The
    // order is always this way round -- list outer, bitmap inner -- in
    // every function here, which is what keeps the pair deadlock-free.
    let mut g = FRAME_ALLOCATOR.lock();
    let mut batch = [0usize; BATCH];
    let mut taken = 0;
    for slot in batch.iter_mut() {
        let Some(f) = g.alloc() else { break };
        *slot = f;
        taken += 1;
    }
    if taken == 0 {
        return None;
    }
    // Parked HIGHEST FIRST, so the pops that follow hand the batch out in
    // ascending order: the bitmap gave it ascending, and a list that is
    // popped from the top would reverse it. The order matters to libos,
    // where a guest page is one host `mmap` of the frame file at the
    // frame's offset and the host merges neighbouring mappings only when
    // their offsets ascend with the addresses. Handed out in reverse,
    // every page of a 64-page thread stack was a host VMA of its own and
    // `vm.max_map_count` (65530) ran out 2048 threads into
    // `RseqTest.ManyConcurrent`; ascending, a batch is one VMA.
    let mut parked = 0;
    for &f in batch[1..taken].iter().rev() {
        if !p.push(f) {
            g.dealloc(f);
            continue;
        }
        parked += 1;
    }
    if parked > 0 {
        FRAMES_CACHED.add(parked << PAGE_BITS);
    }
    drop(g);
    // A refill is where the level is looked at: once per batch, not per page.
    super::note_frames_level();
    Some(batch[0])
}

/// Park a freed frame, or `false` when the caller must free it to the
/// bitmap itself.
pub fn free(idx: usize) -> bool {
    let Some(mut p) = mine().try_lock() else {
        return false;
    };
    if p.len() == CAPACITY {
        // Full: give a batch back rather than refusing, so a CPU that
        // frees far more than it allocates does not fall back to the
        // global lock on every single page from here on.
        let mut g = FRAME_ALLOCATOR.lock();
        let mut given = 0;
        for _ in 0..BATCH {
            let Some(f) = p.pop() else { break };
            g.dealloc(f);
            given += 1;
        }
        drop(g);
        FRAMES_CACHED.sub(given << PAGE_BITS);
    }
    if !p.push(idx) {
        return false;
    }
    FRAMES_CACHED.add(1 << PAGE_BITS);
    true
}

/// Give every list back to the bitmap, and answer how many frames moved.
///
/// Linux drains the per-CPU lists before it believes a zone is empty, and
/// so must this: without it a contiguous request can be refused with up to
/// 32 MiB of single frames parked where the bitmap cannot see them, and
/// the refusal surfaces as an ENOMEM with free RAM on the meter. Best
/// effort on purpose -- a list whose lock is held is skipped rather than
/// waited for, because the caller is already on a failure path.
pub fn drain_all() -> usize {
    let mut moved = 0;
    for list in LISTS.iter() {
        let Some(mut p) = list.try_lock() else {
            continue;
        };
        if p.len() == 0 {
            continue;
        }
        let mut g = FRAME_ALLOCATOR.lock();
        while let Some(f) = p.pop() {
            g.dealloc(f);
            moved += 1;
        }
    }
    if moved > 0 {
        FRAMES_CACHED.sub(moved << PAGE_BITS);
    }
    moved
}
