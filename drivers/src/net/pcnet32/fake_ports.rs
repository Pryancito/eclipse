//! A PCnet's worth of I/O ports in memory, so the register sequences can be
//! driven by a test.
//!
//! This is not an emulation of the chip: it is the register file plus the
//! three behaviours the bring-up actually depends on -- RAP echoes what was
//! written to it, reading the reset port puts CSR0 back to `STOP`, and `INIT`
//! is answered with `IDON`. Every write is also recorded in order, which is
//! what lets a test assert that the software style was set BEFORE the init
//! block address: the kind of ordering bug that leaves no trace at runtime
//! because the chip just parses the wrong fields.

use alloc::vec::Vec;

use super::{PortAccess, Width};
use lock::Mutex;

/// One recorded port access.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Access {
    In16(u16),
    Out16(u16, u16),
    In32(u16),
    Out32(u16, u32),
}

/// A register write, as the fake decoded it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Wrote {
    Csr(u16, u16),
    Bcr(u16, u16),
    Reset,
}

pub struct FakePcnet {
    inner: Mutex<Inner>,
    /// The width this fake answers in. A probe of the other width fails, which
    /// is how [`super::detect_width`] is tested both ways round.
    width: Width,
}

struct Inner {
    rap: u16,
    csr: [u16; 128],
    bcr: [u16; 64],
    aprom: [u8; 16],
    log: Vec<Access>,
    decoded: Vec<Wrote>,
    /// Whether `INIT` is answered with `IDON`, as a real chip does once it has
    /// read the block in.
    idon_after_init: bool,
}

impl FakePcnet {
    pub fn new(width: Width) -> Self {
        let mut csr = [0u16; 128];
        csr[0] = super::CSR0_STOP;
        Self {
            width,
            inner: Mutex::new(Inner {
                rap: 0,
                csr,
                bcr: [0; 64],
                aprom: [
                    0x52, 0x54, 0x00, 0xab, 0xcd, 0xef, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ],
                log: Vec::new(),
                decoded: Vec::new(),
                idon_after_init: true,
            }),
        }
    }

    /// A chip that never acknowledges the initialization block.
    pub fn without_idon(width: Width) -> Self {
        let me = Self::new(width);
        me.inner.lock().idon_after_init = false;
        me
    }

    pub fn set_aprom(&self, bytes: [u8; 6]) {
        self.inner.lock().aprom[..6].copy_from_slice(&bytes);
    }

    pub fn log(&self) -> Vec<Access> {
        self.inner.lock().log.clone()
    }

    pub fn decoded(&self) -> Vec<Wrote> {
        self.inner.lock().decoded.clone()
    }

    /// Raise CSR0 status bits the way the chip does, bypassing the
    /// write-1-to-clear path a driver write goes through. A driver cannot set
    /// these, which is the whole reason this exists.
    pub fn raise_csr0(&self, bits: u16) {
        self.inner.lock().csr[0] |= bits;
    }

    pub fn csr(&self, n: usize) -> u16 {
        self.inner.lock().csr[n]
    }

    pub fn bcr(&self, n: usize) -> u16 {
        self.inner.lock().bcr[n]
    }

    /// The position of the first write matching `pred` in the decoded log.
    pub fn position_where(&self, pred: impl Fn(&Wrote) -> bool) -> Option<usize> {
        self.inner.lock().decoded.iter().position(pred)
    }
}

impl Inner {
    fn read_at(&mut self, off: u16, width: Width) -> u32 {
        if off < 0x10 {
            // The address PROM, readable as 16-bit words.
            let i = off as usize;
            return u16::from_le_bytes([self.aprom[i], self.aprom[i + 1]]) as u32;
        }
        if off == width.reset() {
            self.csr[0] = super::CSR0_STOP;
            self.decoded.push(Wrote::Reset);
            return 0;
        }
        if off == width.rap() {
            return self.rap as u32;
        }
        if off == width.rdp() {
            return self.csr[(self.rap & 0x7f) as usize] as u32;
        }
        if off == width.bdp() {
            return self.bcr[(self.rap & 0x3f) as usize] as u32;
        }
        // A port this width does not use: open bus.
        0xffff_ffff
    }

    fn write_at(&mut self, off: u16, value: u32, width: Width) {
        if off == width.rap() {
            self.rap = (value & 0xffff) as u16;
            return;
        }
        if off == width.rdp() {
            let n = (self.rap & 0x7f) as usize;
            let v = (value & 0xffff) as u16;
            self.decoded.push(Wrote::Csr(n as u16, v));
            if n == 0 {
                self.csr[0] = Self::csr0_after_write(self.csr[0], v, self.idon_after_init);
            } else {
                self.csr[n] = v;
            }
            return;
        }
        if off == width.bdp() {
            let n = (self.rap & 0x3f) as usize;
            let v = (value & 0xffff) as u16;
            self.decoded.push(Wrote::Bcr(n as u16, v));
            self.bcr[n] = v;
        }
    }

    /// CSR0 after a write of `v`: the control bits trigger, the status bits
    /// are write-1-to-clear, and `IENA` is an ordinary read/write bit.
    fn csr0_after_write(mut csr0: u16, v: u16, idon: bool) -> u16 {
        if v & super::CSR0_STOP != 0 {
            csr0 = super::CSR0_STOP;
        }
        if v & super::CSR0_INIT != 0 && idon {
            csr0 |= super::CSR0_IDON;
        }
        if v & super::CSR0_STRT != 0 {
            csr0 &= !super::CSR0_STOP;
        }
        csr0 &= !(v & !super::CSR0_CONTROL);
        (csr0 & !super::CSR0_IENA) | (v & super::CSR0_IENA)
    }
}

impl PortAccess for FakePcnet {
    fn in16(&self, off: u16) -> u16 {
        let mut inner = self.inner.lock();
        inner.log.push(Access::In16(off));
        let width = self.width;
        (inner.read_at(off, width) & 0xffff) as u16
    }

    fn out16(&self, off: u16, value: u16) {
        let mut inner = self.inner.lock();
        inner.log.push(Access::Out16(off, value));
        let width = self.width;
        // A 16-bit write to a chip running in 32-bit port mode does not reach
        // a register, which is exactly why the width probe can tell them
        // apart.
        if width == Width::Wio {
            inner.write_at(off, value as u32, width);
        }
    }

    fn in32(&self, off: u16) -> u32 {
        let mut inner = self.inner.lock();
        inner.log.push(Access::In32(off));
        let width = self.width;
        inner.read_at(off, width)
    }

    fn out32(&self, off: u16, value: u32) {
        let mut inner = self.inner.lock();
        inner.log.push(Access::Out32(off, value));
        let width = self.width;
        if width == Width::Dwio {
            inner.write_at(off, value, width);
        }
    }
}
