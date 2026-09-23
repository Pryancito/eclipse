//! The x86-64 page-table entry.
//!
//! Split out of `bare/arch/x86_64/vm.rs`, which the host never compiles, so
//! the bit layout and the two flag conversions can be measured by a test.
//! See [`super`].

use core::convert::TryFrom;
use core::fmt::{Debug, Formatter, Result};
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::structures::paging::page_table::PageTableFlags as PTF;

use crate::utils::page_table::GenericPTE;
use crate::{CachePolicy, MMUFlags, PhysAddr};

/// Set once the BSP has programmed a write-combining entry into its PAT; APs
/// replicate the same value before they can touch any WC mapping. Read by
/// [`X86PTE::set_flags`] so the WriteCombining PAT bit is only ever emitted
/// when PAT entry 7 really is WC. Lives here, beside its only reader, so a
/// test can drive both sides of the branch; `arch::pat::init_this_cpu` is the
/// one caller of the setter.
static PAT_WC_READY: AtomicBool = AtomicBool::new(false);

/// Whether PAT entry 7 has been redefined to write-combining.
#[inline]
pub fn pat_wc_ready() -> bool {
    PAT_WC_READY.load(Ordering::Acquire)
}

/// Record that this CPU's PAT entry 7 is now write-combining.
#[inline]
pub fn set_pat_wc_ready(ready: bool) {
    PAT_WC_READY.store(ready, Ordering::Release);
}

impl From<MMUFlags> for PTF {
    fn from(f: MMUFlags) -> Self {
        if f.is_empty() {
            return PTF::empty();
        }
        // PROT_NONE / no-access: any non-empty flags used to stamp PRESENT
        // (USER-only, cache bits, ...). x86 then treated the page as readable.
        if !f.intersects(MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE) {
            return PTF::empty();
        }
        let mut flags = PTF::PRESENT;
        if f.contains(MMUFlags::WRITE) {
            flags |= PTF::WRITABLE;
        }
        if !f.contains(MMUFlags::EXECUTE) {
            flags |= PTF::NO_EXECUTE;
        }
        if f.contains(MMUFlags::USER) {
            flags |= PTF::USER_ACCESSIBLE;
        }
        let cache_policy = (f.bits() & 3) as u32; // 最低三位用于储存缓存策略
        match CachePolicy::try_from(cache_policy) {
            Ok(CachePolicy::Cached) => {
                flags.remove(PTF::WRITE_THROUGH);
            }
            Ok(CachePolicy::Uncached) | Ok(CachePolicy::UncachedDevice) => {
                flags |= PTF::NO_CACHE | PTF::WRITE_THROUGH;
            }
            Ok(CachePolicy::WriteCombining) => {
                flags |= PTF::NO_CACHE | PTF::WRITE_THROUGH;
                // 当位于level=1时，页面更大，在1<<12位上（0x100）为1
                // 但是bitflags里面没有这一位。由页表自行管理标记位去吧
            }
            Err(_) => unreachable!("invalid cache policy"),
        }
        flags
    }
}

impl From<PTF> for MMUFlags {
    fn from(f: PTF) -> Self {
        if f.is_empty() {
            return Self::empty();
        }
        let mut ret = Self::READ;
        if f.contains(PTF::WRITABLE) {
            ret |= Self::WRITE;
        }
        if !f.contains(PTF::NO_EXECUTE) {
            ret |= Self::EXECUTE;
        }
        if f.contains(PTF::USER_ACCESSIBLE) {
            ret |= Self::USER;
        }
        if f.contains(PTF::NO_CACHE | PTF::WRITE_THROUGH) {
            ret |= Self::CACHE_1;
        }
        ret
    }
}

const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000; // 12..52

/// Page table entry on x86.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct X86PTE(u64);

impl GenericPTE for X86PTE {
    fn addr(&self) -> PhysAddr {
        (self.0 & PHYS_ADDR_MASK) as _
    }
    fn flags(&self) -> MMUFlags {
        PTF::from_bits_truncate(self.0).into()
    }
    fn is_unused(&self) -> bool {
        self.0 == 0
    }
    fn is_present(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::PRESENT)
    }
    fn is_leaf(&self) -> bool {
        PTF::from_bits_truncate(self.0).contains(PTF::HUGE_PAGE)
    }

    fn set_addr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !PHYS_ADDR_MASK) | (paddr as u64 & PHYS_ADDR_MASK);
    }
    fn set_flags(&mut self, flags: MMUFlags, is_huge: bool) {
        let mmu_flags = flags;
        let mut flags: PTF = flags.into();
        if is_huge {
            flags |= PTF::HUGE_PAGE;
        }
        let mut bits = self.addr() as u64 | flags.bits();
        // WriteCombining selects PAT entry 7 (PAT|PCD|PWT). `From<MMUFlags>`
        // already contributed PCD|PWT; the PAT bit cannot live in `PTF`
        // because its position is level-dependent — bit 7 in a 4 KiB PTE,
        // bit 12 in a 2 MiB/1 GiB leaf (bit 7 there is PS). Only emitted once
        // `pat` has actually redefined entry 7 to WC; before that, index 7 is
        // UC and plain PCD|PWT (index 3, also UC) is the honest encoding.
        let cache_policy = (mmu_flags.bits() & 3) as u32;
        if cache_policy == CachePolicy::WriteCombining as u32 && pat_wc_ready() {
            bits |= if is_huge { 1 << 12 } else { 1 << 7 };
        }
        self.0 = bits;
    }
    fn set_table(&mut self, paddr: PhysAddr) {
        self.0 = (paddr as u64 & PHYS_ADDR_MASK)
            | (PTF::PRESENT | PTF::WRITABLE | PTF::USER_ACCESSIBLE).bits();
    }
    fn clear(&mut self) {
        self.0 = 0
    }
}

impl Debug for X86PTE {
    fn fmt(&self, f: &mut Formatter) -> Result {
        let mut f = f.debug_struct("X86PTE");
        f.field("raw", &self.0);
        f.field("addr", &self.addr());
        f.field("flags", &self.flags());
        f.finish()
    }
}

#[cfg(test)]
impl X86PTE {
    /// A cleared entry, to build one from in a test.
    pub(crate) fn zeroed() -> Self {
        Self(0)
    }

    /// The raw descriptor, so a test can check it bit by bit against
    /// the Intel SDM.
    pub(crate) fn raw(&self) -> u64 {
        self.0
    }
}
