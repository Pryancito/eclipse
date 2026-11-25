use alloc::vec::Vec;

use crate::{PhysAddr, KHANDLER, PAGE_SIZE};

/// A 4K size physical frame.
#[derive(Debug)]
pub struct PhysFrame {
    paddr: PhysAddr,
    allocated: bool,
}

impl PhysFrame {
    /// Allocate one physical frame.
    pub fn new() -> Option<Self> {
        KHANDLER.frame_alloc().map(|paddr| Self {
            paddr,
            allocated: true,
        })
    }

    /// Allocate one physical frame for user memory (a VMO page). Unlike
    /// [`new`](Self::new) it may be refused while RAM is down to what the
    /// kernel keeps for itself.
    pub fn new_user() -> Option<Self> {
        KHANDLER.frame_alloc_user().map(|paddr| Self {
            paddr,
            allocated: true,
        })
    }

    /// [`new_user`](Self::new_user), filled with zero.
    pub fn new_user_zero() -> Option<Self> {
        Self::new_user().map(|mut f| {
            f.zero();
            f
        })
    }

    /// Allocate one physical frame and fill with zero.
    pub fn new_zero() -> Option<Self> {
        Self::new().map(|mut f| {
            f.zero();
            f
        })
    }

    fn alloc_contiguous_base(frame_count: usize, align_log2: usize) -> Option<PhysAddr> {
        KHANDLER.frame_alloc_contiguous(frame_count, align_log2)
    }

    /// Allocate contiguous physical frames.
    pub fn new_contiguous(frame_count: usize, align_log2: usize) -> Vec<Self> {
        Self::alloc_contiguous_base(frame_count, align_log2).map_or(Vec::new(), |base| {
            (0..frame_count)
                .map(|i| Self {
                    paddr: base + i * PAGE_SIZE,
                    allocated: true,
                })
                .collect()
        })
    }

    /// # Safety
    ///
    /// This function is unsafe because the user must ensure that this is an available physical
    /// frame.
    pub unsafe fn from_paddr(paddr: PhysAddr) -> Self {
        assert!(crate::addr::is_aligned(paddr));
        Self {
            paddr,
            allocated: false,
        }
    }

    /// Reclaim a frame previously obtained from [`Self::new`] / [`Self::new_zero`]
    /// whose ownership was forgotten at the kernel virt address of that frame
    /// (tmpfs/ramfs page backing). Dropping the returned value frees it.
    ///
    /// # Safety
    ///
    /// `vaddr` must be `phys_to_virt` of a frame this CPU still owns and that
    /// was allocated through the frame allocator (not a borrowed mapping).
    pub unsafe fn from_virt_owned(vaddr: crate::VirtAddr) -> Self {
        let paddr = crate::mem::virt_to_phys(vaddr);
        assert!(crate::addr::is_aligned(paddr));
        Self {
            paddr,
            allocated: true,
        }
    }

    /// Get the start physical address of this frame.
    pub fn paddr(&self) -> PhysAddr {
        self.paddr
    }

    /// convert to raw a pointer.
    pub fn as_ptr(&self) -> *const u8 {
        crate::mem::phys_to_virt(self.paddr) as *const u8
    }

    /// convert to a mutable raw pointer.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        crate::mem::phys_to_virt(self.paddr) as *mut u8
    }

    /// Fill `self` with zero.
    pub fn zero(&mut self) {
        crate::mem::pmem_zero(self.paddr, PAGE_SIZE);
    }
}

impl Drop for PhysFrame {
    fn drop(&mut self) {
        if self.allocated {
            KHANDLER.frame_dealloc(self.paddr)
        }
    }
}

lazy_static! {
    /// The global physical frame contains all zeros.
    pub static ref ZERO_FRAME: PhysFrame = PhysFrame::new_zero().expect("failed to alloc zero frame");
}

/// PROTOTYPE (measurement only): frame-allocation fault injection, like
/// Linux's `fail_page_alloc`. `FAILPAGE=N` fails about one frame allocation
/// in N; `FAILPAGEK=1` extends it from user frames to kernel ones. Off (0)
/// unless the cmdline asks.
pub mod failpage {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    static EVERY: AtomicU64 = AtomicU64::new(0);
    static KERNEL: AtomicBool = AtomicBool::new(false);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static INJECTED_USER: AtomicU64 = AtomicU64::new(0);
    static INJECTED_KERNEL: AtomicU64 = AtomicU64::new(0);
    static START_NS: AtomicU64 = AtomicU64::new(0);
    static END_NS: AtomicU64 = AtomicU64::new(0);
    pub fn set_end_ns(ns: u64) {
        END_NS.store(ns, Ordering::Relaxed);
    }
    pub fn set_start_ns(ns: u64) {
        START_NS.store(ns, Ordering::Relaxed);
    }
    pub fn set(every: u64, kernel: bool) {
        KERNEL.store(kernel, Ordering::Relaxed);
        EVERY.store(every, Ordering::Relaxed);
    }
    pub fn enabled() -> bool {
        EVERY.load(Ordering::Relaxed) != 0
    }
    #[inline]
    fn roll() -> bool {
        let n = EVERY.load(Ordering::Relaxed);
        if n == 0 {
            return false;
        }
        let start = START_NS.load(Ordering::Relaxed);
        let end = END_NS.load(Ordering::Relaxed);
        if start != 0 || end != 0 {
            let now = crate::timer::timer_now().as_nanos() as u64;
            if now < start || (end != 0 && now >= end) {
                return false;
            }
        }
        let mut x = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        x % n == 0
    }
    pub fn user() -> bool {
        let hit = roll();
        if hit {
            INJECTED_USER.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }
    pub fn kernel() -> bool {
        if !KERNEL.load(Ordering::Relaxed) {
            return false;
        }
        let hit = roll();
        if hit {
            INJECTED_KERNEL.fetch_add(1, Ordering::Relaxed);
        }
        hit
    }
    pub fn stats() -> (u64, u64) {
        (
            INJECTED_USER.load(Ordering::Relaxed),
            INJECTED_KERNEL.load(Ordering::Relaxed),
        )
    }
}
