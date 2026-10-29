//! El doble de prueba del XGMAC, compartido por los dos modulos de
//! tests de este driver.
//!
//! Vive en su propio fichero porque el trinquete de tamano de `xtask`
//! limita cada `.rs` a 4000 lineas y los tests del datapath ya no caben
//! junto a los del arranque.
#![cfg(test)]

use super::*;

// ——— The sequences, against a fake XGMAC ———
//
// These are the first tests here that run real driver code rather than
// checking a number. The fake is deliberately not a stub that returns
// zero: a self-clearing bit that clears immediately would pass a reset
// routine that never polls at all, so the fake clears `SWR` and `FTQ`
// only after a configurable number of reads, and records every access in
// order.

use alloc::vec::Vec;
use core::cell::RefCell;

/// Every register touch, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    Read(usize),
    Write(usize, u32),
}

pub(super) struct FakeXgmac {
    /// Sparse register file.
    regs: RefCell<alloc::collections::BTreeMap<usize, u32>>,
    log: RefCell<Vec<Access>>,
    /// How many reads a self-clearing bit survives before the fake
    /// clears it. 0 means it clears on the first read.
    clear_after: RefCell<alloc::collections::BTreeMap<usize, u32>>,
    /// Never clear these, to exercise the timeout.
    stuck: RefCell<alloc::collections::BTreeSet<usize>>,
}

// The fake is only ever touched from the test thread; `MmioAccess`
// requires Send + Sync because a real driver holds one across CPUs.
unsafe impl Send for FakeXgmac {}
unsafe impl Sync for FakeXgmac {}

impl FakeXgmac {
    pub(super) fn new() -> Self {
        Self {
            regs: RefCell::new(alloc::collections::BTreeMap::new()),
            log: RefCell::new(Vec::new()),
            clear_after: RefCell::new(alloc::collections::BTreeMap::new()),
            stuck: RefCell::new(alloc::collections::BTreeSet::new()),
        }
    }

    /// Preload a register, as a chip with other fields already set would
    /// present it. Not logged: this is the chip's state, not an access.
    pub(super) fn preload(&self, off: usize, value: u32) {
        self.regs.borrow_mut().insert(off, value);
    }

    /// Make `off`'s bit 0 survive `n` reads before clearing.
    pub(super) fn clear_bit0_after(&self, off: usize, n: u32) {
        self.clear_after.borrow_mut().insert(off, n);
    }

    /// Make `off`'s bit 0 never clear.
    pub(super) fn stick_bit0(&self, off: usize) {
        self.stuck.borrow_mut().insert(off);
    }

    pub(super) fn log(&self) -> Vec<Access> {
        self.log.borrow().clone()
    }

    pub(super) fn writes_to(&self, off: usize) -> usize {
        self.log()
            .iter()
            .filter(|a| matches!(a, Access::Write(o, _) if *o == off))
            .count()
    }

    pub(super) fn reads_of(&self, off: usize) -> usize {
        self.log()
            .iter()
            .filter(|a| matches!(a, Access::Read(o) if *o == off))
            .count()
    }
}

impl MmioAccess for FakeXgmac {
    fn read32(&self, off: usize) -> u32 {
        self.log.borrow_mut().push(Access::Read(off));
        let mut regs = self.regs.borrow_mut();
        let value = *regs.get(&off).unwrap_or(&0);
        if value & 1 != 0 && !self.stuck.borrow().contains(&off) {
            let mut after = self.clear_after.borrow_mut();
            match after.get_mut(&off) {
                Some(0) | None => {
                    regs.insert(off, value & !1);
                }
                Some(left) => *left -= 1,
            }
        }
        value
    }

    fn write32(&self, off: usize, value: u32) {
        self.log.borrow_mut().push(Access::Write(off, value));
        self.regs.borrow_mut().insert(off, value);
    }
}
