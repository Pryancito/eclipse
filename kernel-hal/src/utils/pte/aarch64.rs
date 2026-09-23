//! The AArch64 stage-1 VMSAv8-64 descriptor.
//!
//! Split out of `bare/arch/aarch64/vm.rs`, which the host never compiles, so
//! the bit layout and the two flag conversions can be measured by a test.
//! See [`super`].

use core::fmt::{Debug, Formatter, Result};

use crate::utils::page_table::GenericPTE;
use crate::{MMUFlags, PhysAddr};

/// Physical addresses are 40 bits (`PA_1TB_BITS`), page aligned. Kept here
/// rather than in `imp::config`, which only exists in an AArch64 build;
/// that module re-exports this one.
pub const PHYS_ADDR_MASK: usize = ((1 << 40) - 1) & !(crate::PAGE_SIZE - 1);

bitflags::bitflags! {
    /// Possible flags for a page table entry.
    struct PTF: usize {
        // Attribute fields in stage 1 VMSAv8-64 Block and Page descriptors:
        /// Whether the descriptor is valid.
        const VALID =       1 << 0;
        /// The descriptor gives the address of the next level of translation table or 4KB page.
        /// (not a 2M, 1G block)
        const NON_BLOCK =   1 << 1;
        /// Memory attributes index field.
        const ATTR_INDX =   0b111 << 2;
        /// Non-secure bit. For memory accesses from Secure state, specifies whether the output
        /// address is in Secure or Non-secure memory.
        const NS =          1 << 5;
        /// Access permission: accessable at EL0.
        const AP_EL0 =      1 << 6;
        /// Access permission: read-only.
        const AP_RO =       1 << 7;
        /// Shareability: Inner Shareable (otherwise Outer Shareable).
        const INNER =       1 << 8;
        /// Shareability: Inner or Outer Shareable (otherwise Non-shareable).
        const SHAREABLE =   1 << 9;
        /// The Access flag.
        const AF =          1 << 10;
        /// The not global bit.
        const NG =          1 << 11;
        /// Indicates that 16 adjacent translation table entries point to contiguous memory regions.
        const CONTIGUOUS =  1 <<  52;
        /// The Privileged execute-never field.
        const PXN =         1 <<  53;
        /// The Execute-never or Unprivileged execute-never field.
        const UXN =         1 <<  54;

        // Next-level attributes in stage 1 VMSAv8-64 Table descriptors:

        /// PXN limit for subsequent levels of lookup.
        const PXN_TABLE =           1 << 59;
        /// XN limit for subsequent levels of lookup.
        const XN_TABLE =            1 << 60;
        /// Access permissions limit for subsequent levels of lookup: access at EL0 not permitted.
        const AP_NO_EL0_TABLE =     1 << 61;
        /// Access permissions limit for subsequent levels of lookup: write access not permitted.
        const AP_NO_WRITE_TABLE =   1 << 62;
        /// For memory accesses from Secure state, specifies the Security state for subsequent
        /// levels of lookup.
        const NS_TABLE =            1 << 63;
    }
}

#[repr(u64)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MemType {
    #[allow(dead_code)]
    Device = 0,
    Normal = 1,
}

impl PTF {
    const ATTR_INDEX_MASK: u64 = 0b1_1100;

    const fn from_mem_type(mem_type: MemType) -> Self {
        let mut bits = (mem_type as u64) << 2;
        if matches!(mem_type, MemType::Normal) {
            bits |= (Self::INNER.bits() | Self::SHAREABLE.bits()) as u64;
        }
        Self::from_bits_truncate(bits as usize)
    }

    #[allow(dead_code)]
    fn mem_type(&self) -> MemType {
        let idx = (self.bits() as u64 & Self::ATTR_INDEX_MASK) >> 2;
        match idx {
            0 => MemType::Device,
            1 => MemType::Normal,
            _ => panic!("Invalid memory attribute index"),
        }
    }
}

impl From<MMUFlags> for PTF {
    fn from(f: MMUFlags) -> Self {
        let mut flags = Self::from_mem_type(if f.contains(MMUFlags::DEVICE) {
            MemType::Device
        } else {
            MemType::Normal
        });
        if f.is_empty() {
            return flags;
        }
        // AArch64 has no execute-only leaf encoding: executable mappings are
        // valid and implicitly readable even when the ELF segment has PF_X
        // without PF_R (as current Fuchsia userboot does).
        if f.intersects(MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE) {
            flags |= PTF::VALID;
        }
        if !f.contains(MMUFlags::WRITE) {
            flags |= PTF::AP_RO;
        }
        if f.contains(MMUFlags::USER) {
            flags |= PTF::AP_EL0 | PTF::PXN;
            if !f.contains(MMUFlags::EXECUTE) {
                flags |= PTF::UXN;
            }
        } else {
            flags |= PTF::UXN;
            if !f.contains(MMUFlags::EXECUTE) {
                flags |= PTF::PXN;
            }
        }
        flags
    }
}

impl From<PTF> for MMUFlags {
    fn from(f: PTF) -> Self {
        // Every permission bit on AArch64 says what is *forbidden* on top of
        // an access the descriptor already allows, so none of them means
        // anything until the descriptor is valid. Reading them out of an
        // invalid entry is how `stack_guard` lost its guard bands here: a band
        // whose flags it had just cleared still reported `WRITE`, because
        // AP_RO is absent from an all-zero entry exactly as it is from a
        // writable one, and the readback check refused the band. An invalid
        // entry grants nothing; say so, and leave DEVICE out too, since
        // attribute index 0 is Device and a cleared entry has index 0 without
        // ever having been a device mapping.
        if !f.contains(PTF::VALID) {
            return Self::empty();
        }
        // Valid implies readable: AArch64 has no read-disable bit, and
        // `From<MMUFlags>` above maps an executable-but-not-readable mapping
        // onto a plain valid leaf for the same reason.
        let mut ret = Self::READ;
        if !f.contains(PTF::AP_RO) {
            ret |= Self::WRITE;
        }
        if f.contains(PTF::AP_EL0) {
            ret |= Self::USER;
            if !f.contains(PTF::UXN) {
                ret |= Self::EXECUTE;
            }
        } else if !f.contains(PTF::PXN) {
            // PXN is Privileged eXecute Never, so a kernel mapping is
            // executable when it is *absent*. This read the bit the other way
            // round, which inverted EXECUTE on every kernel mapping:
            // `From<MMUFlags>` sets PXN precisely when EXECUTE was not asked
            // for. Round-tripping a mapping through `flags()` -- which is what
            // splitting a huge page into smaller leaves does -- therefore made
            // the kernel's read-only text non-executable and its heap
            // executable.
            ret |= Self::EXECUTE;
        }
        if f.mem_type() == MemType::Device {
            ret |= Self::DEVICE;
        }
        ret
    }
}

/// Page table entry.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct AARCH64PTE(u64);
impl GenericPTE for AARCH64PTE {
    fn addr(&self) -> PhysAddr {
        (self.0 as usize & PHYS_ADDR_MASK) as _
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
        // A block descriptor is one that is IN USE and does not point at
        // another table. The `NON_BLOCK` test alone answered `true` for a
        // cleared entry, because bit 1 is absent from all-zero exactly as it
        // is from a real block -- the same shape of mistake as reading the
        // permission bits out of an invalid entry, below. Every caller in
        // `page_table.rs` happened to ask `is_unused()` first, so it never
        // showed; since a leaf now stops `next_table_mut`, say it properly.
        !self.is_unused() && !PTF::from_bits_truncate(self.0 as usize).contains(PTF::NON_BLOCK)
    }
    fn set_addr(&mut self, paddr: PhysAddr) {
        // Keep the attributes. This assigned the address alone, which wiped
        // VALID and every permission bit with it -- and the walker's
        // `update(vaddr, Some(paddr), None)` repoints a live mapping without
        // passing flags, so on AArch64 that would have left the page present
        // in the tree, pointing at the new frame, and unreachable. x86 and
        // RISC-V both preserve here.
        self.0 = (self.0 & !PHYS_ADDR_MASK as u64) | (paddr & PHYS_ADDR_MASK) as u64;
    }
    fn set_flags(&mut self, flags: MMUFlags, is_huge: bool) {
        let mut flags = PTF::from(flags) | PTF::AF;
        if !is_huge {
            flags |= PTF::NON_BLOCK
        }
        self.0 = (self.0 & PHYS_ADDR_MASK as u64) | flags.bits() as u64;
    }
    fn set_table(&mut self, paddr: PhysAddr) {
        self.0 = ((paddr & PHYS_ADDR_MASK) | PTF::VALID.bits() | PTF::NON_BLOCK.bits()) as u64;
    }
    fn clear(&mut self) {
        self.0 = 0
    }
}

impl Debug for AARCH64PTE {
    fn fmt(&self, f: &mut Formatter) -> Result {
        let mut f = f.debug_struct("AARCH64PTE");
        f.field("raw", &self.0);
        f.field("addr", &self.addr());
        f.field("flags", &self.flags());
        f.finish()
    }
}

#[cfg(test)]
impl AARCH64PTE {
    /// A cleared entry, to build one from in a test.
    pub(crate) fn zeroed() -> Self {
        Self(0)
    }

    /// The raw descriptor, so a test can check it bit by bit against
    /// the ARM ARM.
    pub(crate) fn raw(&self) -> u64 {
        self.0
    }
}
