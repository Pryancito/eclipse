//! Helpers the unit tests share: a RAM disk, a filesystem on it, and a
//! deterministic random source (no `rand` in a `no_std` crate).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::device::BlockDevice;
use crate::journal::Spin;
use crate::mkfs::{self, MkfsOptions};
use crate::{Error, Result};

/// A disk in memory. Out-of-range accesses fail with `Io`, as a partition
/// would.
pub(crate) struct Ram(Spin<Vec<u8>>);

/// Every log record, formatted and dropped, so the arguments of `warn!`
/// and `debug!` run in tests too.
struct Sink;

impl log::Log for Sink {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        core::hint::black_box(alloc::format!("{}", r.args()));
    }
    fn flush(&self) {}
}

impl Ram {
    pub(crate) fn new(bytes: usize) -> Arc<Self> {
        if log::set_logger(&Sink).is_ok() {
            log::set_max_level(log::LevelFilter::Trace);
        }
        Arc::new(Self(Spin::new(vec![0xA5; bytes])))
    }

    /// The bytes at `at..at + n` as they are now.
    pub(crate) fn peek(&self, at: usize, n: usize) -> Vec<u8> {
        self.0.with(|d| d[at..at + n].to_vec())
    }

    /// Flip the bits of `mask` in the byte at `at`.
    pub(crate) fn poke(&self, at: usize, mask: u8) {
        self.0.with(|d| d[at] ^= mask)
    }
}

impl BlockDevice for Ram {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.0.with(|d| {
            let o = offset as usize;
            buf.copy_from_slice(d.get(o..o + buf.len()).ok_or(Error::Io)?);
            Ok(())
        })
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.0.with(|d| {
            let o = offset as usize;
            d.get_mut(o..o + buf.len())
                .ok_or(Error::Io)?
                .copy_from_slice(buf);
            Ok(())
        })
    }
    fn sync(&self) -> Result<()> {
        Ok(())
    }
    fn size(&self) -> u64 {
        self.0.with(|d| d.len() as u64)
    }
}

pub(crate) const MIB: usize = 1024 * 1024;

pub(crate) fn opts() -> MkfsOptions {
    MkfsOptions {
        label: String::from("unit"),
        fsid: [7; 16],
        chunk_uuid: [0; 16],
        dev_uuid: [0; 16],
        subvol_uuid: [0; 16],
        now: (1_700_000_000, 0),
    }
}

pub(crate) fn clock() -> (u64, u32) {
    (1_800_000_000, 0)
}

/// A freshly formatted v0.4 disk of `mib` MiB.
pub(crate) fn formatted(mib: usize) -> Arc<Ram> {
    let d = Ram::new(mib * MIB);
    mkfs::format(&*d, &opts()).unwrap();
    d
}

/// xorshift64*: the same sequence on every run, so a failure reproduces.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub(crate) fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}
