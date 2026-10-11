//! The x86-64 page-table entry.
//!
//! Split out of `bare/arch/x86_64/vm.rs`, which the host never compiles, so
//! the bit layout and the two flag conversions can be measured by a test.
//! See [`super`].

use core::convert::TryFrom;
use core::fmt::{Debug, Formatter, Result};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
        // An entry with no PRESENT bit grants nothing, whatever else it
        // carries. Asking whether the flags are *empty* instead was wrong in
        // two ways, and both of them hand out access that was taken away:
        //
        // - ACCESSED and DIRTY are the hardware's bookkeeping, and
        //   [`GenericPTE::set_flags`] carries them across a flag rewrite (see
        //   there), so a guard band whose permissions have just been taken
        //   away is an entry with no PRESENT bit and, very often, D still set.
        //   That made it non-empty, so this fell through to `READ`, and
        //   `guard_band::took_effect` -- which exists precisely to verify
        //   that the permission really went away -- refused every coroutine
        //   stack's hard guard band as "still readable after clearing its
        //   permissions". Every stack ran on the soft canary instead, which
        //   only notices an overflow once the neighbouring heap is already
        //   overwritten.
        // - A no-access *huge* leaf keeps its PS bit, which is how this
        //   kernel spells `PROT_NONE` over 2 MiB. That was never empty
        //   either, so a PROT_NONE huge mapping read back as readable and
        //   executable -- and `split_huge_page` copies what it reads into 512
        //   present children, which turns a page the process may not touch
        //   into 512 it may read and execute.
        //
        // PRESENT is the one bit the CPU itself consults before any other, so
        // it is the one to ask about.
        if !f.contains(PTF::PRESENT) {
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
    /// The one format of the three whose hardware maintains this bit: the CPU
    /// sets D on the first write through the entry and never clears it by
    /// itself. Nothing in the kernel clears it either, which is what makes it
    /// usable without a shootdown — clearing D without invalidating every
    /// CPU's TLB would let an entry that already has D absorb a write, and the
    /// page would read clean after being written.
    ///
    /// Only meaningful on a leaf; an intermediate table entry's bit 6 is
    /// ignored by the walker. The one caller only ever asks about 4 KiB leaves.
    fn dirty(&self) -> Option<bool> {
        Some(PTF::from_bits_truncate(self.0).contains(PTF::DIRTY))
    }

    fn set_dirty(&mut self) {
        self.0 |= PTF::DIRTY.bits();
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
        // The store is a read-modify-write, and an ATOMIC one, because the
        // other writer of this `u64` is the CPU's own page-table walker.
        //
        // ACCESSED and DIRTY are the two bits the hardware maintains, and this
        // entry has to come out of a flag rewrite with them still set: DIRTY
        // is what `PageTableImpl::dirty_depth_from_top` reads to bound the
        // scheduler's stack poison, and the stack quarantine rewrites the
        // flags of a coroutine stack's usable region on every teardown and
        // every reuse. Reporting a stack full of a previous occupant's frames
        // as untouched is the one error the bound cannot afford.
        //
        // Reading `self.0`, OR-ing the bits in and storing would lose a D the
        // CPU set in between -- and once the TLB entry that write used has D
        // recorded, the hardware need not set it again, so the page would read
        // clean for the rest of its life. A `compare_exchange` loop cannot:
        // either it sees the CPU's D and carries it, or it lands first and the
        // CPU's own update sets D afterwards.
        //
        // Carrying a stale D onto an entry that was just repointed can only
        // over-report, which costs stores, and nothing else in the kernel
        // reads either bit.
        const HW: u64 = PTF::ACCESSED.bits() | PTF::DIRTY.bits();
        // SAFETY: `X86PTE` is a `repr(transparent)` `u64`, so an `AtomicU64`
        // describes the same object with the same layout; `&mut self` is
        // exclusive as far as Rust is concerned, and the CPU -- which is the
        // reason this has to be atomic at all -- is outside that model.
        let cell = unsafe { &*(self as *mut Self as *const AtomicU64) };
        let mut cur = cell.load(Ordering::Relaxed);
        loop {
            match cell.compare_exchange_weak(
                cur,
                bits | (cur & HW),
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => cur = seen,
            }
        }
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

    /// An entry this file did not write, to read back. `rboot` builds the
    /// first page tables and the kernel walks them afterwards, so `flags()`
    /// and `is_present()` are asked about bit patterns no code here ever
    /// emits.
    pub(crate) fn from_raw(bits: u64) -> Self {
        Self(bits)
    }
}
