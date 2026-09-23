//! The RISC-V Sv39/Sv48 page-table entry.
//!
//! Split out of `bare/arch/riscv/vm.rs`, which the host never compiles, so
//! the bit layout and the two flag conversions can be measured by a test.
//! See [`super`].

use core::fmt::{Debug, Formatter, Result};

use crate::utils::page_table::GenericPTE;
use crate::{MMUFlags, PhysAddr};

bitflags::bitflags! {
    /// Possible flags for a page table entry.
    struct PTF: usize {
        const VALID =        1 <<  0;
        const READABLE =     1 <<  1;
        const WRITABLE =     1 <<  2;
        const EXECUTABLE =   1 <<  3;
        const USER =         1 <<  4;
        const GLOBAL =       1 <<  5;
        const ACCESSED =     1 <<  6;
        const DIRTY =        1 <<  7;
        const RESERVED1 =    1 <<  8;
        const RESERVED2 =    1 <<  9;
        #[cfg(feature = "thead-maee")]
        const CACHEABLE =    1 << 62;
        #[cfg(feature = "thead-maee")]
        const STRONG_ORDER = 1 << 63;
    }
}

impl From<MMUFlags> for PTF {
    fn from(f: MMUFlags) -> Self {
        if f.is_empty() {
            return PTF::empty();
        }
        if !f.intersects(MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE) {
            return PTF::empty();
        }
        let mut flags = PTF::VALID;
        if f.contains(MMUFlags::WRITE) {
            flags |= PTF::READABLE | PTF::WRITABLE;
            #[cfg(feature = "thead-maee")]
            {
                flags |= PTF::CACHEABLE;
            }
        } else if f.contains(MMUFlags::READ) {
            flags |= PTF::READABLE;
            #[cfg(feature = "thead-maee")]
            {
                flags |= PTF::CACHEABLE;
            }
        }
        if f.contains(MMUFlags::EXECUTE) {
            flags |= PTF::EXECUTABLE;
        }
        if f.contains(MMUFlags::USER) {
            flags |= PTF::USER;
        }
        #[cfg(feature = "thead-maee")]
        if f.contains(MMUFlags::DEVICE) {
            flags |= PTF::STRONG_ORDER;
        }
        flags
    }
}

impl From<PTF> for MMUFlags {
    fn from(f: PTF) -> Self {
        let mut ret = Self::empty();
        if f.contains(PTF::READABLE) {
            ret |= Self::READ;
        }
        if f.contains(PTF::WRITABLE) {
            ret |= Self::WRITE;
        }
        if f.contains(PTF::EXECUTABLE) {
            ret |= Self::EXECUTE;
        }
        if f.contains(PTF::USER) {
            ret |= Self::USER;
        }
        ret
    }
}

const PHYS_ADDR_MASK: u64 = 0x003f_ffff_ffff_fc00; // 10..54

/// Sv39 and Sv48 page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Rv64PTE(u64);

impl GenericPTE for Rv64PTE {
    fn addr(&self) -> PhysAddr {
        ((self.0 & PHYS_ADDR_MASK) << 2) as _
    }
    fn flags(&self) -> MMUFlags {
        PTF::from_bits_truncate(self.0 as usize).into()
    }
    fn is_unused(&self) -> bool {
        self.0 == 0
    }
    fn is_present(&self) -> bool {
        PTF::from_bits_truncate(self.0 as usize).contains(PTF::VALID)
    }
    fn is_leaf(&self) -> bool {
        PTF::from_bits_truncate(self.0 as usize).intersects(PTF::READABLE | PTF::EXECUTABLE)
    }

    fn set_addr(&mut self, paddr: PhysAddr) {
        self.0 = (self.0 & !PHYS_ADDR_MASK) | ((paddr as u64 >> 2) & PHYS_ADDR_MASK);
    }
    fn set_flags(&mut self, flags: MMUFlags, _is_huge: bool) {
        // ACCESSED and DIRTY unconditionally: updating them in hardware is
        // optional in the privileged spec, and an implementation that does
        // not faults instead.
        //
        // There was a `debug_assert!` here demanding READABLE or EXECUTABLE.
        // A no-access mapping -- `mmap(PROT_NONE)`, which is the first thing
        // musl's `pthread_create` does for a thread's stack, guard and TLS --
        // has neither, so any process could panic a debug kernel through it.
        // What comes out instead is an entry with A and D and no V, which the
        // walker reads as claimed but not present. That is the right answer
        // for a 4 KiB page; see `a_no_access_mapping_is_claimed_but_absent`
        // for what it is not.
        let flags = PTF::from(flags) | PTF::ACCESSED | PTF::DIRTY;
        self.0 = (self.0 & PHYS_ADDR_MASK) | flags.bits() as u64;
    }
    fn set_table(&mut self, paddr: PhysAddr) {
        self.0 = ((paddr as u64 >> 2) & PHYS_ADDR_MASK) | PTF::VALID.bits() as u64;
    }
    fn clear(&mut self) {
        self.0 = 0
    }
}

impl Rv64PTE {
    /// Mark this entry global, so its translation survives an `satp` switch.
    ///
    /// Only for the kernel half, and only on an entry that is already in use:
    /// `pt_clone_kernel_space` copies the top-level kernel entries into every
    /// new address space, and the global bit is what keeps them out of the
    /// per-address-space part of the TLB. It is not part of `MMUFlags`, so it
    /// cannot go through `set_flags` -- which is also why this is a named
    /// operation rather than a caller reaching into the bits.
    pub fn set_global(&mut self) {
        self.0 |= PTF::GLOBAL.bits() as u64;
    }
}

impl Debug for Rv64PTE {
    fn fmt(&self, f: &mut Formatter) -> Result {
        let mut f = f.debug_struct("Rv64PTE");
        f.field("raw", &self.0);
        f.field("addr", &self.addr());
        f.field("flags", &self.flags());
        f.finish()
    }
}

#[cfg(test)]
impl Rv64PTE {
    /// A cleared entry, to build one from in a test.
    pub(crate) fn zeroed() -> Self {
        Self(0)
    }

    /// The raw descriptor, so a test can check it bit by bit against
    /// the RISC-V privileged spec.
    pub(crate) fn raw(&self) -> u64 {
        self.0
    }
}
