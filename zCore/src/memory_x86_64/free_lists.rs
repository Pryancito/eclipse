//! The per-CPU front cache's free lists: see `free_lists` in `memory_x86_64.rs`.
//! Out of that file only to keep it under the size ceiling (`xtask tamano`).

use core::alloc::Layout;

// Cache buddy classes 2^4 (16 B, the buddy minimum: a free block holds
// the buddy's two tree links) .. 2^12 (4 KiB).
// That covers every hot fork object and the general small-allocation
// churn; larger buffers (I/O, 1 MiB readahead) churn rarely and would
// pin too much idle RAM, so they go straight to the buddy.
pub const MIN_CLASS: usize = 4; // 2^4 = 16 B
pub const MAX_CLASS: usize = 12; // 2^12 = 4 KiB
pub const NUM_CLASSES: usize = MAX_CLASS - MIN_CLASS + 1;
// Per-class ceiling on the number of cached blocks. A freed block past
// the cap is handed to the buddy (where its buddy may coalesce) instead
// of cached: bounding the cache keeps the buddy from starving of large
// contiguous blocks and stops idle RAM accumulating in it.
//
// This is the ceiling, not the cap: see [`cap_of`], which is what `push`
// asks. The cap used to be this number for every class, which is a cap on
// *blocks* and therefore a cap on bytes that grows with the class -- 8 KiB
// in the 8-byte class and 4 MiB in the 4 KiB one. With one cache in the
// system that came to ~8 MiB of a 512 MiB heap and nobody minded; with a
// cache per CPU (`PERCPU_SLOTS` of them) the same number would reserve
// ~520 MiB, which is the whole heap. So the cap is now a cap on BYTES.
pub const CAP: u32 = 1024;

/// Bytes a single class of a single CPU's cache may hold.
///
/// The quantity worth bounding, because what the cache costs the buddy is
/// RAM it cannot hand out, not list entries. 64 KiB per class is 10 classes
/// x 64 KiB ~ 504 KiB per CPU once [`CAP`] clips the small classes, so the
/// whole table is ~31 MiB of a 512 MiB heap at `MAX_CORE_NUM = 64` -- and
/// that is the worst case, every class of every CPU full at once.
pub const CLASS_BUDGET: usize = 64 * 1024;

/// Fewest blocks a class keeps whatever its size, so the top classes still
/// cache something. 4 KiB x 16 = 64 KiB, which is `CLASS_BUDGET` exactly;
/// it is a floor only if `CLASS_BUDGET` is ever lowered.
pub const MIN_CAP: u32 = 16;

/// How many blocks of class `i` one CPU's cache may hold.
///
/// `CLASS_BUDGET / class_size(i)`, clamped into `MIN_CAP..=CAP`. This is
/// the `limit` of Linux's per-CPU array caches, and for the same reason:
/// a small object churns far more than a 4 KiB one and is far cheaper to
/// keep, so the two must not get the same block count.
pub const fn cap_of(i: usize) -> u32 {
    let blocks = CLASS_BUDGET / class_size(i);
    if blocks < MIN_CAP as usize {
        MIN_CAP
    } else if blocks > CAP as usize {
        CAP
    } else {
        blocks as u32
    }
}

/// Bytes one CPU's cache may hold with every class full.
///
/// Nothing in the kernel reads it -- the figure it checks is a compile-time
/// property of the constants above, so the test is the only caller there
/// can be, and the crate denies warnings.
#[cfg(test)]
pub const fn budget_per_cpu() -> usize {
    let mut total = 0;
    let mut i = 0;
    while i < NUM_CLASSES {
        total += cap_of(i) as usize * class_size(i);
        i += 1;
    }
    total
}

/// What a free block of the cache holds in its second word. Mixed with
/// the block's own address, so a value copied out of one free block into
/// another live object can never pass for the tag of the second.
const FREE_TAG: usize = 0x5a1b_f4ee_b10c_c0de;

pub fn tag_of(block: usize) -> usize {
    FREE_TAG ^ block
}

/// Bytes in a block of class `i`.
pub const fn class_size(i: usize) -> usize {
    1 << (i + MIN_CLASS)
}

/// Cache-array index for `layout`, or `None` when it is larger than
/// the top cached class. The class formula matches
/// buddy_system_allocator exactly, so a cached block is byte-for-byte
/// what the buddy would have handed out.
#[inline]
pub fn cache_slot(layout: Layout) -> Option<usize> {
    // Guard before `next_power_of_two` so an oversize request can
    // never overflow it (and matches the buddy's own bypass of the
    // cache for large blocks).
    if layout.size() > (1 << MAX_CLASS) || layout.align() > (1 << MAX_CLASS) {
        return None;
    }
    // The buddy's own rounding (`buddy_system_allocator::block_size`,
    // asserted equal beside `buddy_block_size`), so a cached block is
    // byte-for-byte what the buddy would have handed out.
    let size = core::cmp::max(
        layout.size().next_power_of_two(),
        core::cmp::max(layout.align(), 1 << MIN_CLASS),
    );
    // size ∈ [16, 4096] given the guard, so class ∈ [4, 12].
    Some(size.trailing_zeros() as usize - MIN_CLASS)
}

/// What was wrong with a free block's link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkFault {
    /// Not a block of this class the heap owns.
    Outside,
    /// A block of the heap's, but one that does not carry the free mark.
    NotFree,
    /// Zero, with more free blocks still counted behind it.
    Zeroed,
    /// Non-zero, from the last free block of its class.
    PastEnd,
}

/// A block the cache refused, and what was found in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broken {
    /// `block`'s link to the next free block is `link`, which is wrong in
    /// the way `why` says: something wrote over the block's first word
    /// after it was freed. `lost` more blocks were behind it.
    Link {
        block: usize,
        link: usize,
        lost: u32,
        why: LinkFault,
    },
    /// `block`'s second word holds `found` instead of its free mark:
    /// something wrote into the block after it was freed. `lost` more
    /// blocks were behind it.
    Tag {
        block: usize,
        found: usize,
        lost: u32,
    },
    /// `block` was freed while already free.
    DoubleFree { block: usize },
}

impl Broken {
    pub fn block(self) -> usize {
        match self {
            Broken::Link { block, .. }
            | Broken::Tag { block, .. }
            | Broken::DoubleFree { block } => block,
        }
    }
}

pub struct SlabCache {
    // head[i]: first free block of class MIN_CLASS+i (0 = empty). The
    // block's first word holds the next pointer while it is cached.
    head: [usize; NUM_CLASSES],
    count: [u32; NUM_CLASSES],
}

impl SlabCache {
    pub const fn new() -> Self {
        SlabCache {
            head: [0; NUM_CLASSES],
            count: [0; NUM_CLASSES],
        }
    }

    /// Blocks cached in class `i`.
    pub fn count(&self, i: usize) -> u32 {
        self.count[i]
    }

    /// Take a block of class `i`; `Ok(None)` when the class is empty.
    ///
    /// `owns(addr, size)` answers whether `[addr, addr + size)` is a block
    /// of the heap's, aligned to `size`.
    ///
    /// # Safety
    /// Every block in the lists must have been given to [`push`] and be
    /// memory this cache may read and write.
    pub unsafe fn pop(
        &mut self,
        i: usize,
        owns: impl Fn(usize, usize) -> bool,
    ) -> Result<Option<usize>, Broken> {
        let block = self.head[i];
        if block == 0 {
            return Ok(None);
        }
        let size = class_size(i);
        let word = core::mem::size_of::<usize>();
        let tagged = size >= 2 * word;
        // Whatever is refused below, the rest of the list goes with it:
        // nothing behind this block can be reached without trusting a
        // word that was just shown to have been written by someone else.
        let lost = self.count[i].saturating_sub(1);
        if tagged {
            let found = unsafe { core::ptr::read((block + word) as *const usize) };
            if found != tag_of(block) {
                self.drop_class(i);
                return Err(Broken::Tag { block, found, lost });
            }
        }
        let link = unsafe { core::ptr::read(block as *const usize) };
        let why = if link == 0 {
            (lost > 0).then_some(LinkFault::Zeroed)
        } else if !owns(link, size) {
            Some(LinkFault::Outside)
        } else if tagged
            // A link that lands on a block of the heap's can still be
            // wrong -- a heap pointer is exactly what a stale owner
            // stores -- so the block it names must be free as well.
            && unsafe { core::ptr::read((link + word) as *const usize) } != tag_of(link)
        {
            Some(LinkFault::NotFree)
        } else if lost == 0 {
            Some(LinkFault::PastEnd)
        } else {
            None
        };
        if let Some(why) = why {
            self.drop_class(i);
            return Err(Broken::Link {
                block,
                link,
                lost,
                why,
            });
        }
        if tagged {
            // A live block must not carry the mark: `push` reads it as
            // "already free".
            unsafe { core::ptr::write((block + word) as *mut usize, 0) };
        }
        self.head[i] = link;
        self.count[i] -= 1;
        Ok(Some(block))
    }

    /// Forget class `i`'s list, leaking every block in it.
    fn drop_class(&mut self, i: usize) {
        self.head[i] = 0;
        self.count[i] = 0;
    }

    /// Cache a freed block of class `i`. `Ok(false)` means the class is
    /// full and the caller must hand the block to the buddy.
    ///
    /// # Safety
    /// `block` must be a block of class `i` that the caller is freeing.
    pub unsafe fn push(&mut self, i: usize, block: usize) -> Result<bool, Broken> {
        let size = class_size(i);
        let tagged = size >= 2 * core::mem::size_of::<usize>();
        let tag = (block + core::mem::size_of::<usize>()) as *mut usize;
        if block == self.head[i] || (tagged && unsafe { core::ptr::read(tag) } == tag_of(block)) {
            return Err(Broken::DoubleFree { block });
        }
        if self.count[i] >= cap_of(i) {
            return Ok(false);
        }
        // Push: stash the old head in the block's first word.
        unsafe { core::ptr::write(block as *mut usize, self.head[i]) };
        if tagged {
            unsafe { core::ptr::write(tag, tag_of(block)) };
        }
        self.head[i] = block;
        self.count[i] += 1;
        Ok(true)
    }
}
