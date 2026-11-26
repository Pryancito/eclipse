//! Intel VT-d, the part that is arithmetic: what the firmware's DMAR table
//! says, how the hardware's tables are laid out, and how a device's
//! second-level page table is walked. The hardware half (registers, frames,
//! the enable sequence) is `bare/arch/x86_64/vtd.rs`, which hands this
//! module bytes and a [`TableMem`] and gets decisions back.
//!
//! Everything here follows the VT-d specification (Intel order no.
//! D51397, "legacy" translation mode, not scalable mode): root table of 256
//! 16-byte entries, one per bus, each naming a context table of 256 16-byte
//! entries, one per device and function; a context entry either passes the
//! device through untranslated or names its second-level page table.

use alloc::vec::Vec;
use core::convert::TryInto;

use crate::{PhysAddr, PAGE_SIZE};

// ---------------------------------------------------------------------------
// The DMAR table.
// ---------------------------------------------------------------------------

/// Where the remapping structures start: the 36-byte header, then the host
/// address width, the flags and ten reserved bytes.
const DMAR_STRUCTS: usize = 48;

/// A device scope's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    /// One PCI function.
    Endpoint,
    /// A bridge and every function behind it.
    Bridge,
    /// An I/O APIC, an HPET, an ACPI namespace device: not a DMA requester
    /// a driver in a process can hold.
    Other,
}

/// One device scope: the function reached from `start_bus` by walking
/// `path`, each `(device, function)` but the last a bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub start_bus: u8,
    pub path: Vec<(u8, u8)>,
}

/// A remapping hardware unit (DRHD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drhd {
    pub segment: u16,
    /// Physical address of its 4 KiB register page.
    pub base: u64,
    /// It covers every function of the segment no other unit lists.
    pub include_all: bool,
    pub scopes: Vec<Scope>,
}

/// A reserved memory region (RMRR): memory the firmware keeps doing DMA to
/// for the devices in its scope (USB keyboard emulation, a GPU's stolen
/// memory). A device in its scope must keep reaching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rmrr {
    pub segment: u16,
    pub base: u64,
    /// The last byte, inclusive, as the table gives it.
    pub limit: u64,
    pub scopes: Vec<Scope>,
}

/// What the DMAR table describes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dmar {
    /// Physical address bits the platform uses, as the table counts them
    /// (the field is one less).
    pub host_address_width: u8,
    /// Bit 0: interrupt remapping is supported.
    pub flags: u8,
    pub units: Vec<Drhd>,
    pub rmrrs: Vec<Rmrr>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The device scopes in `bytes`, which is what follows a structure's fixed
/// part. A scope whose length runs past the end, or is too short for its
/// own header, ends the list.
fn scopes(mut bytes: &[u8]) -> Vec<Scope> {
    let mut out = Vec::new();
    while bytes.len() >= 6 {
        let len = bytes[1] as usize;
        if len < 6 || len > bytes.len() {
            break;
        }
        let kind = match bytes[0] {
            1 => ScopeKind::Endpoint,
            2 => ScopeKind::Bridge,
            _ => ScopeKind::Other,
        };
        let path = bytes[6..len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| (p[0], p[1]))
            .collect();
        out.push(Scope {
            kind,
            start_bus: bytes[5],
            path,
        });
        bytes = &bytes[len..];
    }
    out
}

/// Read a DMAR table. `None` when it is not one or is too short to be;
/// structures this code does not know (ATSR, RHSA, ANDD, SATC) are passed
/// over, and a structure whose length is impossible ends the walk with
/// what came before it.
pub fn parse_dmar(table: &[u8]) -> Option<Dmar> {
    if !super::acpi_tables::table_is(table, b"DMAR") || table.len() < DMAR_STRUCTS {
        return None;
    }
    let len = super::acpi_tables::table_length(table)?.min(table.len());
    let table = &table[..len];
    let mut dmar = Dmar {
        host_address_width: table[36].saturating_add(1),
        flags: table[37],
        ..Default::default()
    };
    let mut at = DMAR_STRUCTS;
    while at + 4 <= table.len() {
        let kind = u16_at(table, at)?;
        let slen = u16_at(table, at + 2)? as usize;
        if slen < 4 || at + slen > table.len() {
            break;
        }
        let s = &table[at..at + slen];
        match kind {
            0 if slen >= 16 => dmar.units.push(Drhd {
                include_all: s[4] & 1 != 0,
                segment: u16_at(s, 6)?,
                base: u64_at(s, 8)?,
                scopes: scopes(&s[16..]),
            }),
            1 if slen >= 24 => dmar.rmrrs.push(Rmrr {
                segment: u16_at(s, 6)?,
                base: u64_at(s, 8)?,
                limit: u64_at(s, 16)?,
                scopes: scopes(&s[24..]),
            }),
            _ => {}
        }
        at += slen;
    }
    Some(dmar)
}

/// What a scope names once its path is walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// One function: bus, device, function.
    Function(u8, u8, u8),
    /// Every function on these buses (a bridge's secondary to subordinate).
    Buses(u8, u8),
}

/// PCI configuration offsets of a bridge's bus numbers.
const SECONDARY_BUS: u16 = 0x19;
const SUBORDINATE_BUS: u16 = 0x1a;

impl Scope {
    /// Walk the path. `cfg(bus, device, function, offset)` reads one byte
    /// of configuration space; it is asked only for the bridges on the way
    /// (and the scope's own bridge, for [`ScopeKind::Bridge`]). `None` for a
    /// scope with no path, one that is not a DMA requester, or a bridge on
    /// the way that answers bus 0 (nothing behind it was numbered).
    pub fn resolve(&self, cfg: impl Fn(u8, u8, u8, u16) -> u8) -> Option<Target> {
        if self.kind == ScopeKind::Other {
            return None;
        }
        let (&last, hops) = self.path.split_last()?;
        let mut bus = self.start_bus;
        for &(dev, func) in hops {
            let next = cfg(bus, dev, func, SECONDARY_BUS);
            if next == 0 || next <= bus {
                return None;
            }
            bus = next;
        }
        let (dev, func) = last;
        match self.kind {
            ScopeKind::Endpoint => Some(Target::Function(bus, dev, func)),
            ScopeKind::Bridge => {
                let (sec, sub) = (
                    cfg(bus, dev, func, SECONDARY_BUS),
                    cfg(bus, dev, func, SUBORDINATE_BUS),
                );
                (sec != 0 && sub >= sec).then_some(Target::Buses(sec, sub))
            }
            ScopeKind::Other => None,
        }
    }

    /// Whether this scope covers `(bus, device, function)`.
    pub fn covers(&self, bus: u8, dev: u8, func: u8, cfg: impl Fn(u8, u8, u8, u16) -> u8) -> bool {
        match self.resolve(cfg) {
            Some(Target::Function(b, d, f)) => (b, d, f) == (bus, dev, func),
            Some(Target::Buses(lo, hi)) => (lo..=hi).contains(&bus),
            None => false,
        }
    }
}

impl Dmar {
    /// The index of the unit that translates `(segment, bus, dev, func)`: the
    /// one whose scopes name it, else the segment's include-all unit.
    pub fn unit_for(
        &self,
        segment: u16,
        bus: u8,
        dev: u8,
        func: u8,
        cfg: impl Fn(u8, u8, u8, u16) -> u8 + Copy,
    ) -> Option<usize> {
        let listed = self.units.iter().position(|u| {
            u.segment == segment
                && !u.include_all
                && u.scopes.iter().any(|s| s.covers(bus, dev, func, cfg))
        });
        listed.or_else(|| {
            self.units
                .iter()
                .position(|u| u.segment == segment && u.include_all)
        })
    }

    /// The reserved regions `(segment, bus, dev, func)` must keep reaching,
    /// as `(first byte, length)` in whole pages.
    pub fn reserved_for(
        &self,
        segment: u16,
        bus: u8,
        dev: u8,
        func: u8,
        cfg: impl Fn(u8, u8, u8, u16) -> u8 + Copy,
    ) -> Vec<(PhysAddr, usize)> {
        self.rmrrs
            .iter()
            .filter(|r| r.segment == segment && r.limit >= r.base)
            .filter(|r| r.scopes.iter().any(|s| s.covers(bus, dev, func, cfg)))
            .map(|r| {
                let start = r.base as usize & !(PAGE_SIZE - 1);
                let end = (r.limit as usize | (PAGE_SIZE - 1)) + 1;
                (start, end - start)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Registers.
// ---------------------------------------------------------------------------

/// Register offsets in a unit's page.
pub mod reg {
    pub const CAP: usize = 0x08;
    pub const ECAP: usize = 0x10;
    pub const GCMD: usize = 0x18;
    pub const GSTS: usize = 0x1c;
    pub const RTADDR: usize = 0x20;
    pub const CCMD: usize = 0x28;
    pub const FSTS: usize = 0x34;
    pub const FECTL: usize = 0x38;
}

/// Global command / status bits (GCMD sets, GSTS reports, same positions).
pub mod gcmd {
    pub const TE: u32 = 1 << 31;
    pub const SRTP: u32 = 1 << 30;
    pub const WBF: u32 = 1 << 27;
    pub const QIE: u32 = 1 << 26;
    pub const IRE: u32 = 1 << 25;
    pub const CFI: u32 = 1 << 23;
    /// The status bits that are states rather than one-shot commands: a
    /// write to GCMD must carry them, or it turns them off.
    pub const PERSISTENT: u32 = TE | QIE | IRE | CFI;
}

/// The GCMD value that issues `command` (or, with `on` false, clears a
/// persistent `command`) and leaves every other state as GSTS reports it.
pub fn gcmd_value(gsts: u32, command: u32, on: bool) -> u32 {
    let keep = gsts & gcmd::PERSISTENT;
    if on {
        keep | command
    } else {
        keep & !command
    }
}

/// What a unit can do, from its CAP and ECAP registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub cap: u64,
    pub ecap: u64,
}

impl Caps {
    /// How many domain ids it has.
    pub fn domains(&self) -> usize {
        1 << (4 + 2 * (self.cap & 7))
    }
    /// The write buffer has to be flushed before an invalidation.
    pub fn rwbf(&self) -> bool {
        self.cap & (1 << 4) != 0
    }
    /// Caching mode (an emulated unit): a non-present entry may be cached,
    /// so a map needs an invalidation too.
    pub fn caching_mode(&self) -> bool {
        self.cap & (1 << 7) != 0
    }
    /// The page-table depths it walks, bit 1 three levels, bit 2 four.
    pub fn sagaw(&self) -> u64 {
        (self.cap >> 8) & 0x1f
    }
    /// The widest guest address it takes.
    pub fn mgaw(&self) -> u32 {
        ((self.cap >> 16) & 0x3f) as u32 + 1
    }
    /// Where its fault recording registers are, in bytes.
    pub fn fault_offset(&self) -> usize {
        (((self.cap >> 24) & 0x3ff) as usize) * 16
    }
    /// How many fault recording registers it has.
    pub fn fault_count(&self) -> usize {
        ((self.cap >> 40) & 0xff) as usize + 1
    }
    /// Its page walks snoop the CPU's caches; without that every table
    /// write has to be flushed to memory.
    pub fn coherent(&self) -> bool {
        self.ecap & 1 != 0
    }
    /// Queued invalidation.
    pub fn queued_invalidation(&self) -> bool {
        self.ecap & (1 << 1) != 0
    }
    /// Pass-through context entries.
    pub fn pass_through(&self) -> bool {
        self.ecap & (1 << 6) != 0
    }
    /// Where its IOTLB registers are: IVA at this offset, IOTLB at +8.
    pub fn iotlb_offset(&self) -> usize {
        (((self.ecap >> 8) & 0x3ff) as usize) * 16
    }

    /// The table depth to use, as (levels, the context entry's AW field,
    /// address bits): four if it walks four, else three, else none.
    pub fn depth(&self) -> Option<(usize, u64, u32)> {
        let sagaw = self.sagaw();
        if sagaw & (1 << 2) != 0 {
            Some((4, 2, 48))
        } else if sagaw & (1 << 1) != 0 {
            Some((3, 1, 39))
        } else {
            None
        }
    }
}

/// CCMD: invalidate the context cache, all of it.
pub const CCMD_GLOBAL: u64 = 1 << 63 | 1 << 61;
/// CCMD: invalidate the context cache for one domain.
pub fn ccmd_domain(did: u16) -> u64 {
    1 << 63 | 2 << 61 | did as u64
}
/// IOTLB register: invalidate everything, draining reads and writes.
pub const IOTLB_GLOBAL: u64 = 1 << 63 | 1 << 60 | 1 << 49 | 1 << 48;
/// IOTLB register: invalidate one domain, draining reads and writes.
pub fn iotlb_domain(did: u16) -> u64 {
    1 << 63 | 2 << 60 | 1 << 49 | 1 << 48 | (did as u64) << 32
}

/// One fault record, as a unit logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    /// The requester: bus << 8 | device << 3 | function.
    pub source: u16,
    /// The page the device tried to reach.
    pub address: u64,
    /// The fault reason code (spec appendix A).
    pub reason: u8,
    /// A read (true) or a write.
    pub read: bool,
}

/// Decode a fault recording register, low qword then high. `None` when its
/// fault bit is clear (nothing recorded there).
pub fn decode_fault(lo: u64, hi: u64) -> Option<Fault> {
    if hi & (1 << 63) == 0 {
        return None;
    }
    Some(Fault {
        source: hi as u16,
        address: lo & !0xfff,
        reason: (hi >> 32) as u8,
        read: hi & (1 << 62) != 0,
    })
}

// ---------------------------------------------------------------------------
// Root, context and second-level entries.
// ---------------------------------------------------------------------------

/// Context entry translation types.
pub const TT_TRANSLATE: u64 = 0;
pub const TT_PASS_THROUGH: u64 = 2;

/// A root entry naming the context table at `table` (low qword; the high
/// one is zero in legacy mode).
pub fn root_entry(table: PhysAddr) -> u64 {
    (table as u64 & !0xfff) | 1
}

/// A context entry, `[low, high]`: present, of translation type `tt`, with
/// second-level table `table` (ignored for pass-through), address width
/// `aw` and domain id `did`.
pub fn context_entry(tt: u64, table: PhysAddr, aw: u64, did: u16) -> [u64; 2] {
    [
        (table as u64 & !0xfff) | (tt & 3) << 2 | 1,
        (aw & 7) | (did as u64) << 8,
    ]
}

/// Second-level entry bits.
pub const SL_READ: u64 = 1 << 0;
pub const SL_WRITE: u64 = 1 << 1;
const SL_ADDR: u64 = 0x000f_ffff_ffff_f000;

/// The memory a second-level table lives in, seen through 64-bit slots.
pub trait TableMem {
    /// A zeroed table, or `None` when out of frames.
    fn alloc(&mut self) -> Option<PhysAddr>;
    fn read(&self, table: PhysAddr, index: usize) -> u64;
    /// Write an entry, and make the write visible to the unit (a cache
    /// flush when its walks do not snoop).
    fn write(&mut self, table: PhysAddr, index: usize, value: u64);
}

/// The slot index of `iova` at `level` (1 = the 4 KiB leaf level).
fn index(iova: u64, level: usize) -> usize {
    ((iova >> (12 + 9 * (level - 1))) & 0x1ff) as usize
}

/// Map one 4 KiB page `iova` -> `paddr` with `perms` (`SL_READ`,
/// `SL_WRITE`) in the table rooted at `root`, `levels` deep, allocating
/// the tables on the way. A page already mapped is rewritten with the new
/// permissions.
pub fn map_page(
    mem: &mut impl TableMem,
    root: PhysAddr,
    levels: usize,
    iova: u64,
    paddr: u64,
    perms: u64,
) -> crate::HalResult {
    let mut table = root;
    for level in (2..=levels).rev() {
        let i = index(iova, level);
        let entry = mem.read(table, i);
        table = if entry & (SL_READ | SL_WRITE) != 0 {
            (entry & SL_ADDR) as PhysAddr
        } else {
            let next = mem.alloc().ok_or(crate::HalError)?;
            mem.write(table, i, next as u64 | SL_READ | SL_WRITE);
            next
        };
    }
    mem.write(table, index(iova, 1), (paddr & SL_ADDR) | perms);
    Ok(())
}

/// Remove the 4 KiB page at `iova`, if mapped. The tables on the way stay
/// (they are freed with the domain).
pub fn unmap_page(mem: &mut impl TableMem, root: PhysAddr, levels: usize, iova: u64) {
    let mut table = root;
    for level in (2..=levels).rev() {
        let entry = mem.read(table, index(iova, level));
        if entry & (SL_READ | SL_WRITE) == 0 {
            return;
        }
        table = (entry & SL_ADDR) as PhysAddr;
    }
    let i = index(iova, 1);
    if mem.read(table, i) != 0 {
        mem.write(table, i, 0);
    }
}

/// What `iova` translates to: the physical page and its permissions.
pub fn translate(
    mem: &impl TableMem,
    root: PhysAddr,
    levels: usize,
    iova: u64,
) -> Option<(u64, u64)> {
    let mut table = root;
    for level in (2..=levels).rev() {
        let entry = mem.read(table, index(iova, level));
        if entry & (SL_READ | SL_WRITE) == 0 {
            return None;
        }
        table = (entry & SL_ADDR) as PhysAddr;
    }
    let leaf = mem.read(table, index(iova, 1));
    (leaf & (SL_READ | SL_WRITE) != 0).then_some((leaf & SL_ADDR, leaf & (SL_READ | SL_WRITE)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, vec};

    /// A DMAR with a header, one include-all unit, one unit for the
    /// integrated GPU (00:02.0) and one RMRR for the xHCI at 00:14.0.
    fn sample() -> Vec<u8> {
        let mut t = vec![0u8; 48];
        t[..4].copy_from_slice(b"DMAR");
        t[36] = 38; // 39 bits
        t[37] = 1;
        // DRHD for the GPU: flags 0, segment 0, base 0xfed90000, scope
        // endpoint 00:02.0.
        let mut gpu = vec![0u8, 0, 0, 0, 0, 0, 0, 0];
        gpu.extend_from_slice(&0xfed9_0000u64.to_le_bytes());
        gpu.extend_from_slice(&[1, 8, 0, 0, 0, 0, 2, 0]);
        let len = gpu.len() as u16;
        gpu[2..4].copy_from_slice(&len.to_le_bytes());
        t.extend_from_slice(&gpu);
        // DRHD include-all at 0xfed91000.
        let mut all = vec![0u8, 0, 16, 0, 1, 0, 0, 0];
        all.extend_from_slice(&0xfed9_1000u64.to_le_bytes());
        t.extend_from_slice(&all);
        // RMRR 0x7c000000..=0x7c01ffff for 00:14.0.
        let mut rmrr = vec![1u8, 0, 0, 0, 0, 0, 0, 0];
        rmrr.extend_from_slice(&0x7c00_0000u64.to_le_bytes());
        rmrr.extend_from_slice(&0x7c01_ffffu64.to_le_bytes());
        rmrr.extend_from_slice(&[1, 8, 0, 0, 0, 0, 0x14, 0]);
        let len = rmrr.len() as u16;
        rmrr[2..4].copy_from_slice(&len.to_le_bytes());
        t.extend_from_slice(&rmrr);
        // An unknown structure (ATSR, type 2) of 8 bytes, passed over.
        t.extend_from_slice(&[2, 0, 8, 0, 0, 0, 0, 0]);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        seal(&mut t);
        t
    }

    /// Set the checksum byte so the table's bytes sum to zero.
    fn seal(t: &mut [u8]) {
        t[9] = 0;
        let sum = t.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        t[9] = 0u8.wrapping_sub(sum);
    }

    fn no_bridges(_: u8, _: u8, _: u8, _: u16) -> u8 {
        0
    }

    #[test]
    fn a_dmar_names_its_units_and_reserved_regions() {
        let d = parse_dmar(&sample()).unwrap();
        assert_eq!(d.host_address_width, 39);
        assert_eq!(d.units.len(), 2);
        assert_eq!(d.units[0].base, 0xfed9_0000);
        assert!(!d.units[0].include_all);
        assert_eq!(
            d.units[0].scopes,
            [Scope {
                kind: ScopeKind::Endpoint,
                start_bus: 0,
                path: vec![(2, 0)]
            }]
        );
        assert!(d.units[1].include_all);
        assert_eq!(d.rmrrs.len(), 1);
        assert_eq!(d.rmrrs[0].limit, 0x7c01_ffff);
    }

    /// The GPU goes to its own unit; everything else to the include-all one.
    #[test]
    fn a_function_goes_to_the_unit_that_lists_it_else_the_include_all_one() {
        let d = parse_dmar(&sample()).unwrap();
        assert_eq!(d.unit_for(0, 0, 2, 0, no_bridges), Some(0));
        assert_eq!(d.unit_for(0, 0, 0x14, 0, no_bridges), Some(1));
        assert_eq!(d.unit_for(0, 3, 0, 0, no_bridges), Some(1));
        assert_eq!(
            d.unit_for(1, 0, 2, 0, no_bridges),
            None,
            "no unit on segment 1"
        );
    }

    #[test]
    fn the_rmrr_goes_with_the_function_it_names_in_whole_pages() {
        let d = parse_dmar(&sample()).unwrap();
        assert_eq!(
            d.reserved_for(0, 0, 0x14, 0, no_bridges),
            [(0x7c00_0000, 0x2_0000)]
        );
        assert!(d.reserved_for(0, 0, 0x14, 1, no_bridges).is_empty());
    }

    /// A table cut short, or with a structure claiming more than is there,
    /// gives what came before and no more; a different signature, nothing.
    #[test]
    fn a_broken_dmar_gives_what_came_before_the_break() {
        let mut t = sample();
        assert!(parse_dmar(&t[..40]).is_none());
        t[48 + 2] = 0xff; // the first DRHD claims 255 bytes
        seal(&mut t);
        assert_eq!(parse_dmar(&t).unwrap().units.len(), 0);
        let mut other = sample();
        other[..4].copy_from_slice(b"IVRS");
        seal(&mut other);
        assert!(parse_dmar(&other).is_none());
    }

    /// A path through a bridge reads the bridge's secondary bus; a bridge
    /// scope covers its buses.
    #[test]
    fn a_scope_path_walks_through_its_bridges() {
        // Root port 00:1c.0 with buses 2..=4 behind it.
        let cfg = |bus: u8, dev: u8, func: u8, off: u16| match (bus, dev, func, off) {
            (0, 0x1c, 0, SECONDARY_BUS) => 2,
            (0, 0x1c, 0, SUBORDINATE_BUS) => 4,
            _ => 0,
        };
        let ep = Scope {
            kind: ScopeKind::Endpoint,
            start_bus: 0,
            path: vec![(0x1c, 0), (0, 0)],
        };
        assert_eq!(ep.resolve(cfg), Some(Target::Function(2, 0, 0)));
        let br = Scope {
            kind: ScopeKind::Bridge,
            start_bus: 0,
            path: vec![(0x1c, 0)],
        };
        assert_eq!(br.resolve(cfg), Some(Target::Buses(2, 4)));
        assert!(br.covers(3, 0, 0, cfg));
        assert!(!br.covers(5, 0, 0, cfg));
        // An unnumbered bridge on the way: nothing.
        assert_eq!(ep.resolve(no_bridges), None);
    }

    #[test]
    fn a_gcmd_write_keeps_the_states_and_drops_the_one_shot_bits() {
        let gsts = gcmd::TE | gcmd::SRTP | gcmd::QIE;
        assert_eq!(
            gcmd_value(gsts, gcmd::WBF, true),
            gcmd::TE | gcmd::QIE | gcmd::WBF
        );
        assert_eq!(gcmd_value(gsts, gcmd::TE, false), gcmd::QIE);
        assert_eq!(gcmd_value(0, gcmd::SRTP, true), gcmd::SRTP);
    }

    /// Each field from its own bits: 2^16 domains, three levels, 39-bit
    /// guest addresses, one fault register at 0x220, the IOTLB at 0x1a0.
    #[test]
    fn the_capability_fields_decode_from_their_bits() {
        let c = Caps {
            cap: 0x0000_008c_2226_0206,
            ecap: 0x0000_0000_00f0_1a4e,
        };
        assert_eq!(c.domains(), 1 << 16);
        assert_eq!(c.mgaw(), 39);
        assert_eq!(c.depth(), Some((3, 1, 39)));
        assert_eq!(c.fault_offset(), 0x220);
        assert_eq!(c.fault_count(), 1);
        assert_eq!(c.iotlb_offset(), 0x1a0);
        assert!(c.pass_through());
        assert!(c.queued_invalidation());
        assert!(!c.coherent());
        let four = Caps {
            cap: 0x4 << 8,
            ecap: 0,
        };
        assert_eq!(four.depth(), Some((4, 2, 48)));
        assert_eq!(Caps { cap: 0, ecap: 0 }.depth(), None);
    }

    #[test]
    fn entries_are_laid_out_as_the_spec_says() {
        assert_eq!(root_entry(0x1234_5000), 0x1234_5001);
        assert_eq!(
            context_entry(TT_PASS_THROUGH, 0, 1, 1),
            [0b1001, 1 | 1 << 8]
        );
        assert_eq!(
            context_entry(TT_TRANSLATE, 0xabc_d000, 2, 0x42),
            [0xabc_d001, 2 | 0x42 << 8]
        );
        assert_eq!(ccmd_domain(7), 1 << 63 | 2 << 61 | 7);
        assert_eq!(iotlb_domain(7) >> 32 & 0xffff, 7);
        assert_eq!(
            decode_fault(0x7654_3abc, 1 << 63 | 1 << 62 | 6 << 32 | 0x00a0),
            Some(Fault {
                source: 0xa0,
                address: 0x7654_3000,
                reason: 6,
                read: true
            })
        );
        assert_eq!(decode_fault(0x1000, 0xa0), None);
    }

    #[derive(Default)]
    struct Mem {
        tables: BTreeMap<PhysAddr, [u64; 512]>,
        next: PhysAddr,
    }

    impl TableMem for Mem {
        fn alloc(&mut self) -> Option<PhysAddr> {
            self.next += PAGE_SIZE;
            self.tables.insert(self.next, [0; 512]);
            Some(self.next)
        }
        fn read(&self, table: PhysAddr, index: usize) -> u64 {
            self.tables[&table][index]
        }
        fn write(&mut self, table: PhysAddr, index: usize, value: u64) {
            self.tables.get_mut(&table).unwrap()[index] = value;
        }
    }

    #[test]
    fn a_mapped_page_translates_to_itself_and_an_unmapped_one_does_not() {
        for levels in [3, 4] {
            let mut mem = Mem::default();
            let root = mem.alloc().unwrap();
            let page = 0x7_1234_5000u64;
            map_page(&mut mem, root, levels, page, page, SL_READ).unwrap();
            assert_eq!(translate(&mem, root, levels, page), Some((page, SL_READ)));
            assert_eq!(translate(&mem, root, levels, page + 0x1000), None);
            // Widened in place.
            map_page(&mut mem, root, levels, page, page, SL_READ | SL_WRITE).unwrap();
            assert_eq!(
                translate(&mem, root, levels, page),
                Some((page, SL_READ | SL_WRITE))
            );
            assert_eq!(mem.tables.len(), levels, "one table per level");
            // The neighbour shares every table on the way.
            map_page(
                &mut mem,
                root,
                levels,
                page + 0x1000,
                page + 0x1000,
                SL_WRITE,
            )
            .unwrap();
            assert_eq!(mem.tables.len(), levels);
            unmap_page(&mut mem, root, levels, page);
            assert_eq!(translate(&mem, root, levels, page), None);
            assert!(translate(&mem, root, levels, page + 0x1000).is_some());
            // Unmapping what was never there touches nothing.
            unmap_page(&mut mem, root, levels, 0x40_0000_0000 & ((1 << 39) - 1));
        }
    }
}
