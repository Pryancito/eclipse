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

/// Frame-allocation fault injection for tests, like Linux's `fail_page_alloc`.
///
/// Off unless the kernel command line asks; off, it costs one relaxed load per
/// frame allocation. Options (colon-separated like the rest of the cmdline):
///
/// - `FAILPAGE=N`: refuse about one frame allocation in `N`, chosen by a
///   counter hashed with splitmix64, so a run is spread evenly and has no
///   pattern a caller could fall into step with.
/// - `FAILPAGEK=1`: kernel frames too (page tables, coroutine stacks, heap
///   growth, vmalloc); without it only user frames (`frame_alloc_user`).
/// - `FAILPAGENOUSER=1`: kernel frames only.
/// - `FAILPAGEAFTER=S` / `FAILPAGEUNTIL=S`: only between S seconds of uptime,
///   so the boot and a final check run with every allocation honoured.
///
/// `/proc/kheap` counts the refusals (`FailPageInjected:`). x86_64 only: the
/// hooks are in that architecture's frame allocator.
///
/// What it is for: every caller of a frame allocation has a failure path, and
/// almost none of them run in a normal boot. On 2026-11-25 ~30 000 injected
/// refusals over a mixed workload (mbench, oom-stress cases, `cp -r`) left no
/// panic, no lost RAM and no half-built mapping -- a result worth keeping
/// checkable, and the way to find the next path that does break.
pub mod failpage {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static EVERY: AtomicU64 = AtomicU64::new(0);
    static KERNEL: AtomicBool = AtomicBool::new(false);
    static NO_USER: AtomicBool = AtomicBool::new(false);
    static START_NS: AtomicU64 = AtomicU64::new(0);
    static END_NS: AtomicU64 = AtomicU64::new(0);
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static INJECTED_USER: AtomicU64 = AtomicU64::new(0);
    static INJECTED_KERNEL: AtomicU64 = AtomicU64::new(0);

    /// What the command line asked for, parsed.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub struct Config {
        /// Refuse one in `every`; 0 is off.
        pub every: u64,
        /// Kernel frames as well as user ones.
        pub kernel: bool,
        /// Kernel frames only.
        pub no_user: bool,
        /// Uptime window in seconds; 0 leaves that end open.
        pub after_s: u64,
        pub until_s: u64,
    }

    impl Config {
        /// Read the `FAILPAGE*` options; `None` when `FAILPAGE` is absent or 0.
        pub fn parse(cmdline: &str) -> Option<Self> {
            use crate::cmdline::{flag, parse_number, value};
            let number = |key| value(cmdline, key).and_then(parse_number).unwrap_or(0);
            let every = number("FAILPAGE");
            (every != 0).then(|| Config {
                every,
                kernel: flag(cmdline, "FAILPAGEK") || flag(cmdline, "FAILPAGENOUSER"),
                no_user: flag(cmdline, "FAILPAGENOUSER"),
                after_s: number("FAILPAGEAFTER"),
                until_s: number("FAILPAGEUNTIL"),
            })
        }
    }

    /// Turn injection on as `config` says.
    pub fn configure(config: Config) {
        KERNEL.store(config.kernel, Ordering::Relaxed);
        NO_USER.store(config.no_user, Ordering::Relaxed);
        START_NS.store(
            config.after_s.saturating_mul(1_000_000_000),
            Ordering::Relaxed,
        );
        END_NS.store(
            config.until_s.saturating_mul(1_000_000_000),
            Ordering::Relaxed,
        );
        EVERY.store(config.every, Ordering::Relaxed);
    }

    /// Whether injection was asked for at all (the window may be shut).
    pub fn enabled() -> bool {
        EVERY.load(Ordering::Relaxed) != 0
    }

    /// Whether draw number `draw` of a 1-in-`every` run is a refusal.
    pub fn hits(draw: u64, every: u64) -> bool {
        // splitmix64: consecutive draws land far apart.
        let mut x = draw.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        every != 0 && x.is_multiple_of(every)
    }

    #[inline]
    fn roll() -> bool {
        let every = EVERY.load(Ordering::Relaxed);
        if every == 0 {
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
        hits(COUNTER.fetch_add(1, Ordering::Relaxed), every)
    }

    /// Whether this user frame allocation is to be refused.
    #[inline]
    pub fn user() -> bool {
        if NO_USER.load(Ordering::Relaxed) || !roll() {
            return false;
        }
        INJECTED_USER.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Whether this kernel frame allocation is to be refused.
    #[inline]
    pub fn kernel() -> bool {
        if !KERNEL.load(Ordering::Relaxed) || !roll() {
            return false;
        }
        INJECTED_KERNEL.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Refusals so far: `(user, kernel)`.
    pub fn stats() -> (u64, u64) {
        (
            INJECTED_USER.load(Ordering::Relaxed),
            INJECTED_KERNEL.load(Ordering::Relaxed),
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nothing_asked_is_off() {
            assert_eq!(Config::parse("LOG=warn:ROOT=/dev/vda"), None);
            assert_eq!(Config::parse("FAILPAGE=0"), None);
        }

        #[test]
        fn every_option_is_read() {
            let c = Config::parse(
                "LOG=warn:FAILPAGE=2000:FAILPAGEK=1:FAILPAGEAFTER=50:FAILPAGEUNTIL=450",
            )
            .unwrap();
            assert_eq!(
                c,
                Config {
                    every: 2000,
                    kernel: true,
                    no_user: false,
                    after_s: 50,
                    until_s: 450
                }
            );
        }

        #[test]
        fn kernel_only_implies_kernel() {
            let c = Config::parse("FAILPAGE=300:FAILPAGENOUSER=1").unwrap();
            assert!(c.kernel && c.no_user);
        }

        #[test]
        fn the_rate_is_the_one_asked_for() {
            for every in [2u64, 100, 2000] {
                let n = 400_000u64;
                let hit = (0..n).filter(|&d| hits(d, every)).count() as f64;
                let want = n as f64 / every as f64;
                assert!(
                    (hit - want).abs() < want * 0.15 + 3.0,
                    "1/{}: {} vs {}",
                    every,
                    hit,
                    want
                );
            }
            assert!(!(0..1000).any(|d| hits(d, 0)));
        }
    }
}
