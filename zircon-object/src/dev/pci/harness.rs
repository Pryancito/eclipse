//! A PCI configuration space made of ordinary memory, for the tests.
//!
//! Everything [`PciConfig`] does for [`PciAddrSpace::MMIO`] is a plain
//! (non-volatile) load or store at `base + offset`, so a buffer answers exactly
//! as an ECAM window does: seed a register and the driver reads what a device
//! would have said; let the driver write and read the buffer back to see what
//! it put in the chip. It lives here rather than in one test module because
//! `nodes.rs`, `caps.rs` and `config.rs` all need it, and three copies of a
//! device is three devices that can disagree.

use super::config::{PciConfig, PCIE_EXTENDED_CONFIG_SIZE};
use super::PciAddrSpace;
use alloc::boxed::Box;
use alloc::sync::Arc;

/// The 4 KiB of one function's configuration space, aligned as an ECAM window
/// is: the accessors are plain loads and stores, so a misaligned buffer would
/// make a dword read of a register straddle two cache lines and, on an
/// architecture that cares, fault.
#[repr(C, align(4096))]
pub(super) struct ConfigSpace(pub(super) [u8; PCIE_EXTENDED_CONFIG_SIZE]);

impl ConfigSpace {
    pub(super) fn new() -> Box<Self> {
        Box::new(ConfigSpace([0; PCIE_EXTENDED_CONFIG_SIZE]))
    }

    /// Hand out the accessor the driver will use. Takes `&mut self` so the
    /// pointer it keeps may be written through, as a real device's is.
    pub(super) fn config(&mut self) -> Arc<PciConfig> {
        Arc::new(PciConfig {
            addr_space: PciAddrSpace::MMIO,
            base: self.0.as_mut_ptr() as usize,
        })
    }

    /// Seed a register, as firmware would have left it.
    pub(super) fn poke32(&mut self, offset: usize, val: u32) {
        self.0[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
    }

    pub(super) fn poke16(&mut self, offset: usize, val: u16) {
        self.0[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
    }

    pub(super) fn poke8(&mut self, offset: usize, val: u8) {
        self.0[offset] = val;
    }

    pub(super) fn peek32(&self, offset: usize) -> u32 {
        u32::from_le_bytes([
            self.0[offset],
            self.0[offset + 1],
            self.0[offset + 2],
            self.0[offset + 3],
        ])
    }

    pub(super) fn peek16(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.0[offset], self.0[offset + 1]])
    }

    pub(super) fn peek8(&self, offset: usize) -> u8 {
        self.0[offset]
    }
}
