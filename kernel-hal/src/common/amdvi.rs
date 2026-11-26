//! AMD-Vi (the AMD I/O Virtualization Technology IOMMU), the part that is
//! arithmetic: what the firmware's IVRS table says, how device table
//! entries, commands and page-table entries are laid out, and the page
//! walk. The hardware half is `bare/arch/x86_64/amdvi.rs`.
//!
//! Layouts follow the AMD I/O Virtualization Technology (IOMMU)
//! Specification, order no. 48882. One device table of 65536 32-byte
//! entries, indexed by requester id, each passing its device through
//! untranslated or naming the root of its page table; a command ring to
//! invalidate what the unit caches; an event ring where it logs faults.

use alloc::vec::Vec;
use core::convert::TryInto;

use super::vtd::TableMem;
use crate::{HalError, HalResult, PhysAddr, PAGE_SIZE};

/// Where the IVHD and IVMD blocks start: the 36-byte header, IVinfo and
/// eight reserved bytes.
const IVRS_BLOCKS: usize = 48;

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// One IOMMU, as its IVHD block describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ivhd {
    /// The block type it was read from (0x10, 0x11 or 0x40).
    pub kind: u8,
    pub segment: u16,
    /// Physical address of its registers.
    pub base: u64,
    /// The requester ids it translates, as inclusive ranges.
    pub devices: Vec<(u16, u16)>,
    /// Its device entries name an IOAPIC (a special entry, 0x48, of
    /// variety 1): the firmware's sign that the unit remaps interrupts.
    /// Linux too remaps none without one.
    pub ioapic: bool,
}

/// A memory region some devices keep reaching untranslated (IVMD with the
/// unity bit): `(first requester, last requester)`, inclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ivmd {
    pub devices: (u16, u16),
    pub start: u64,
    pub len: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ivrs {
    pub units: Vec<Ivhd>,
    pub unity: Vec<Ivmd>,
}

/// A special device entry, and the variety of one that is an IOAPIC.
const SPECIAL: u8 = 0x48;
const SPECIAL_IOAPIC: u8 = 1;

/// The requester ids an IVHD's device entries name, and in `ioapic` whether
/// one is an IOAPIC. Entries of a kind
/// this does not know are passed over by their size (4 bytes below 64, 8
/// below 128, and a length byte for the rest).
fn devices(mut e: &[u8], ioapic: &mut bool) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let mut start: Option<u16> = None;
    while !e.is_empty() {
        let kind = e[0];
        let size = match kind {
            0..=63 => 4,
            64..=127 => 8,
            _ => match e.get(1) {
                Some(&n) if n as usize >= 2 => n as usize,
                _ => break,
            },
        };
        if e.len() < size {
            break;
        }
        let id = u16_at(e, 1).unwrap_or(0);
        if kind == SPECIAL && e[7] == SPECIAL_IOAPIC {
            *ioapic = true;
        }
        match kind {
            // Every device.
            1 => out.push((0, 0xffff)),
            // One device (also its alias and extended forms).
            2 | 66 | 70 => out.push((id, id)),
            // The start of a range.
            3 | 67 | 71 => start = Some(id),
            // Its end.
            4 => {
                if let Some(s) = start.take() {
                    if id >= s {
                        out.push((s, id));
                    }
                }
            }
            _ => {}
        }
        e = &e[size..];
    }
    out
}

/// Read an IVRS table. `None` when it is not one. Of the blocks that
/// describe the same unit (one of each type the firmware supports), the
/// newest type wins, up to 0x11; unknown blocks are passed over.
pub fn parse_ivrs(table: &[u8]) -> Option<Ivrs> {
    if !super::acpi_tables::table_is(table, b"IVRS") || table.len() < IVRS_BLOCKS {
        return None;
    }
    let mut ivrs = Ivrs::default();
    let mut at = IVRS_BLOCKS;
    while at + 4 <= table.len() {
        let kind = table[at];
        let len = u16_at(table, at + 2)? as usize;
        if len < 4 || at + len > table.len() {
            break;
        }
        let b = &table[at..at + len];
        match kind {
            0x10 | 0x11 | 0x40 if len >= 24 => {
                let entries = if kind == 0x10 { 24 } else { 40 };
                let mut ioapic = false;
                let unit = Ivhd {
                    kind,
                    base: u64_at(b, 8)?,
                    segment: u16_at(b, 16)?,
                    devices: devices(b.get(entries..).unwrap_or(&[]), &mut ioapic),
                    ioapic,
                };
                match ivrs.units.iter_mut().find(|u| u.base == unit.base) {
                    Some(old) => {
                        let ioapic = old.ioapic || unit.ioapic;
                        if old.kind < unit.kind && unit.kind <= 0x11 {
                            *old = unit;
                        }
                        old.ioapic = ioapic;
                    }
                    None => ivrs.units.push(unit),
                }
            }
            0x20..=0x22 if len >= 32 && b[1] & 1 != 0 => {
                let id = u16_at(b, 4)?;
                let devices = match kind {
                    0x20 => (0, 0xffff),
                    0x21 => (id, id),
                    _ => (id, u16_at(b, 6)?),
                };
                ivrs.unity.push(Ivmd {
                    devices,
                    start: u64_at(b, 16)?,
                    len: u64_at(b, 24)?,
                });
            }
            _ => {}
        }
        at += len;
    }
    Some(ivrs)
}

impl Ivrs {
    /// The unit that translates requester `rid` on `segment`.
    pub fn unit_for(&self, segment: u16, rid: u16) -> Option<usize> {
        self.units.iter().position(|u| {
            u.segment == segment && u.devices.iter().any(|&(lo, hi)| (lo..=hi).contains(&rid))
        })
    }

    /// The unity regions `rid` must keep reaching, in whole pages.
    pub fn reserved_for(&self, rid: u16) -> Vec<(PhysAddr, usize)> {
        self.unity
            .iter()
            .filter(|m| m.len > 0 && (m.devices.0..=m.devices.1).contains(&rid))
            .map(|m| {
                let start = m.start as usize & !(PAGE_SIZE - 1);
                let end = (m.start as usize + m.len as usize + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
                (start, end - start)
            })
            .collect()
    }
}

/// Register offsets.
pub mod reg {
    pub const DEVTAB: usize = 0x0000;
    pub const CMDBUF: usize = 0x0008;
    pub const EVTLOG: usize = 0x0010;
    pub const CONTROL: usize = 0x0018;
    pub const EXT_FEATURES: usize = 0x0030;
    pub const CMD_HEAD: usize = 0x2000;
    pub const CMD_TAIL: usize = 0x2008;
    pub const EVT_HEAD: usize = 0x2010;
    pub const EVT_TAIL: usize = 0x2018;
    pub const STATUS: usize = 0x2020;
}

/// Control register bits.
pub mod ctl {
    pub const IOMMU_EN: u64 = 1 << 0;
    pub const EVENT_LOG_EN: u64 = 1 << 2;
    pub const COHERENT: u64 = 1 << 10;
    pub const CMD_BUF_EN: u64 = 1 << 12;
}

/// The device table register for a table of 65536 entries (2 MiB) at
/// `base`.
pub fn devtab_register(base: PhysAddr) -> u64 {
    (base as u64 & 0x000f_ffff_ffff_f000) | 0x1ff
}

/// A ring base register: `base` and 2^`log2` 16-byte entries.
pub fn ring_register(base: PhysAddr, log2: u64) -> u64 {
    (base as u64 & 0x000f_ffff_ffff_f000) | log2 << 56
}

/// The paging mode (and table depth) every domain here uses: four levels,
/// 48-bit device addresses, which every AMD-Vi unit walks.
pub const LEVELS: usize = 4;
pub const ADDR_BITS: u32 = 48;

const DTE_V: u64 = 1 << 0;
const DTE_TV: u64 = 1 << 1;
const DTE_IR: u64 = 1 << 61;
const DTE_IW: u64 = 1 << 62;

/// A device table entry's first two qwords (the other two, interrupt
/// remapping, stay zero): pass-through.
pub fn dte_pass_through(did: u16) -> [u64; 2] {
    [DTE_V | DTE_TV | DTE_IR | DTE_IW, did as u64]
}

/// A device table entry that lets the device reach nothing: valid and
/// translated, with no table and neither read nor write allowed. Where a
/// device whose driver died is left, so it never gets every byte back.
pub fn dte_blocked(did: u16) -> [u64; 2] {
    [DTE_V | DTE_TV, did as u64]
}

/// The third qword of a device table entry: interrupts valid (IV), and
/// how they are handled (IntCtl, bits 61:60 of the qword).
const DTE_IV: u64 = 1 << 0;
const INTCTL_REMAPPED: u64 = 2 << 60;
/// Entries of an interrupt remapping table here: every index an MSI's data
/// can carry (bits 10:0), so whatever a device writes picks an entry of
/// this table and never memory past its end. The kernel's own MSIs carry
/// their vector, which picks the entry of that vector.
pub const IRT_ENTRIES: usize = 2048;
const IRT_LEN_LOG2: u64 = 11;
/// The table's frames: 2048 4-byte entries.
pub const IRT_FRAMES: usize = IRT_ENTRIES * 4 / PAGE_SIZE;

/// A device table entry's interrupt qword that drops every interrupt the
/// device sends (IntCtl 00b: target abort).
pub fn dte_no_interrupts() -> u64 {
    DTE_IV
}

/// A device table entry's interrupt qword that remaps the device's
/// interrupts through the table at `table` (128-byte aligned,
/// [`IRT_ENTRIES`] entries).
pub fn dte_interrupts(table: PhysAddr) -> u64 {
    DTE_IV | IRT_LEN_LOG2 << 1 | (table as u64 & 0x000f_ffff_ffff_ffc0) | INTCTL_REMAPPED
}

/// A 32-bit interrupt remapping table entry: fixed delivery of `vector` to
/// the APIC `dest`, physical mode.
pub fn irte(vector: u8, dest: u8) -> u32 {
    1 | (dest as u32) << 8 | (vector as u32) << 16
}

/// Have the unit forget what it cached of `rid`'s interrupt table.
pub fn cmd_invalidate_interrupts(rid: u16) -> [u32; 4] {
    [rid as u32, 5 << 28, 0, 0]
}

/// A device table entry that translates through the table at `root`.
pub fn dte_translate(root: PhysAddr, did: u16) -> [u64; 2] {
    [
        DTE_V
            | DTE_TV
            | (LEVELS as u64) << 9
            | (root as u64 & 0x000f_ffff_ffff_f000)
            | DTE_IR
            | DTE_IW,
        did as u64,
    ]
}

/// Commands, as four dwords.
pub fn cmd_completion_wait(store: PhysAddr, data: u64) -> [u32; 4] {
    [
        (store as u32 & !7) | 1,
        ((store as u64 >> 32) as u32 & 0xf_ffff) | 1 << 28,
        data as u32,
        (data >> 32) as u32,
    ]
}

pub fn cmd_invalidate_dte(rid: u16) -> [u32; 4] {
    [rid as u32, 2 << 28, 0, 0]
}

/// Invalidate every translation of domain `did`.
pub fn cmd_invalidate_domain(did: u16) -> [u32; 4] {
    [0, did as u32 | 3 << 28, 0xffff_f000 | 0b11, 0x7fff_ffff]
}

pub fn cmd_invalidate_all() -> [u32; 4] {
    [0, 8 << 28, 0, 0]
}

/// One event the unit logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub code: u8,
    pub device: u16,
    pub flags: u16,
    pub address: u64,
}

pub fn decode_event(e: [u32; 4]) -> Event {
    Event {
        code: (e[1] >> 28) as u8,
        device: e[0] as u16,
        flags: ((e[1] >> 16) & 0xfff) as u16,
        address: e[2] as u64 | (e[3] as u64) << 32,
    }
}

/// The event code of a device's DMA to a page it has no right to.
pub const EVENT_IO_PAGE_FAULT: u8 = 2;

const PTE_PR: u64 = 1 << 0;
const PTE_IR: u64 = 1 << 61;
const PTE_IW: u64 = 1 << 62;
const PTE_ADDR: u64 = 0x000f_ffff_ffff_f000;

/// Leaf permissions.
pub const READ: u64 = PTE_IR;
pub const WRITE: u64 = PTE_IW;

fn index(iova: u64, level: usize) -> usize {
    ((iova >> (12 + 9 * (level - 1))) & 0x1ff) as usize
}

/// Map the 4 KiB page `iova` to `paddr` with `perms` (`READ`, `WRITE`).
pub fn map_page(
    mem: &mut impl TableMem,
    root: PhysAddr,
    iova: u64,
    paddr: u64,
    perms: u64,
) -> HalResult {
    let mut table = root;
    for level in (2..=LEVELS).rev() {
        let i = index(iova, level);
        let entry = mem.read(table, i);
        table = if entry & PTE_PR != 0 {
            (entry & PTE_ADDR) as PhysAddr
        } else {
            let next = mem.alloc().ok_or(HalError)?;
            mem.write(
                table,
                i,
                next as u64 | ((level - 1) as u64) << 9 | PTE_IR | PTE_IW | PTE_PR,
            );
            next
        };
    }
    mem.write(table, index(iova, 1), (paddr & PTE_ADDR) | perms | PTE_PR);
    Ok(())
}

pub fn unmap_page(mem: &mut impl TableMem, root: PhysAddr, iova: u64) {
    let mut table = root;
    for level in (2..=LEVELS).rev() {
        let entry = mem.read(table, index(iova, level));
        if entry & PTE_PR == 0 {
            return;
        }
        table = (entry & PTE_ADDR) as PhysAddr;
    }
    let i = index(iova, 1);
    if mem.read(table, i) != 0 {
        mem.write(table, i, 0);
    }
}

pub fn translate(mem: &impl TableMem, root: PhysAddr, iova: u64) -> Option<(u64, u64)> {
    let mut table = root;
    for level in (2..=LEVELS).rev() {
        let entry = mem.read(table, index(iova, level));
        if entry & PTE_PR == 0 || (entry >> 9) & 7 != (level - 1) as u64 {
            return None;
        }
        table = (entry & PTE_ADDR) as PhysAddr;
    }
    let leaf = mem.read(table, index(iova, 1));
    (leaf & PTE_PR != 0).then_some((leaf & PTE_ADDR, leaf & (PTE_IR | PTE_IW)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, vec};

    fn seal(t: &mut [u8]) {
        t[9] = 0;
        let sum = t.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        t[9] = 0u8.wrapping_sub(sum);
    }

    fn ivhd(kind: u8, base: u64, entries: &[u8]) -> Vec<u8> {
        let fixed = if kind == 0x10 { 24 } else { 40 };
        let mut b = vec![0u8; fixed];
        b[0] = kind;
        b[8..16].copy_from_slice(&base.to_le_bytes());
        b.extend_from_slice(entries);
        let len = b.len() as u16;
        b[2..4].copy_from_slice(&len.to_le_bytes());
        b
    }

    /// One unit described twice (types 0x10 and 0x11), two units on the
    /// segment by range, and a unity region for 00:14.0.
    fn sample() -> Vec<u8> {
        let mut t = vec![0u8; 48];
        t[..4].copy_from_slice(b"IVRS");
        // Unit A: 00:00.0 .. 00:1f.7 (start 3, end 4), plus 0x0300.
        let a = [3, 0x00, 0x00, 0, 4, 0xff, 0x00, 0, 2, 0x00, 0x03, 0];
        t.extend(ivhd(0x10, 0xfeb8_0000, &a));
        t.extend(ivhd(0x11, 0xfeb8_0000, &a));
        // Unit B: everything (select all), listed after A.
        t.extend(ivhd(0x11, 0xfeb9_0000, &[1, 0, 0, 0]));
        // An 8-byte special entry (IOAPIC) in a third block, passed over.
        t.extend(ivhd(0x40, 0xfeba_0000, &[72, 0, 0, 0, 0, 0, 0, 0]));
        // Unit B's IOAPIC, in a fourth block of the same unit.
        t.extend(ivhd(
            0x40,
            0xfeb9_0000,
            &[1, 0, 0, 0, 72, 0, 0, 0, 0, 0xa0, 0, 1],
        ));
        // IVMD type 0x21, unity, for 00:14.0 (rid 0x00a0).
        let mut m = vec![0u8; 32];
        m[0] = 0x21;
        m[1] = 1;
        m[2] = 32;
        m[4..6].copy_from_slice(&0x00a0u16.to_le_bytes());
        m[16..24].copy_from_slice(&0x9c00_0000u64.to_le_bytes());
        m[24..32].copy_from_slice(&0x1800u64.to_le_bytes());
        t.extend(m);
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        seal(&mut t);
        t
    }

    #[test]
    fn an_ivrs_names_each_unit_once_with_its_devices() {
        let r = parse_ivrs(&sample()).unwrap();
        assert_eq!(r.units.len(), 3);
        assert_eq!(r.units[0].kind, 0x11, "the newer description of unit A");
        assert_eq!(r.units[0].devices, [(0, 0xff), (0x300, 0x300)]);
        assert_eq!(r.units[1].devices, [(0, 0xffff)]);
        assert!(r.units[2].devices.is_empty());
        assert!(!r.units[0].ioapic && !r.units[2].ioapic);
        assert!(r.units[1].ioapic, "named in another block of the same unit");
        assert_eq!(r.units[1].devices, [(0, 0xffff)]);
        assert_eq!(r.unity.len(), 1);
    }

    #[test]
    fn a_requester_goes_to_the_first_unit_that_names_it() {
        let r = parse_ivrs(&sample()).unwrap();
        assert_eq!(r.unit_for(0, 0x00a0), Some(0));
        assert_eq!(r.unit_for(0, 0x0300), Some(0));
        assert_eq!(r.unit_for(0, 0x0400), Some(1));
        assert_eq!(r.unit_for(1, 0x00a0), None);
    }

    #[test]
    fn a_unity_region_goes_with_its_device_in_whole_pages() {
        let r = parse_ivrs(&sample()).unwrap();
        assert_eq!(r.reserved_for(0x00a0), [(0x9c00_0000, 0x2000)]);
        assert!(r.reserved_for(0x00a1).is_empty());
    }

    #[test]
    fn a_broken_ivrs_gives_what_came_before_the_break() {
        let mut t = sample();
        t[48 + 2] = 0xff;
        t[48 + 3] = 0xff;
        seal(&mut t);
        assert!(parse_ivrs(&t).unwrap().units.is_empty());
        let mut d = sample();
        d[..4].copy_from_slice(b"DMAR");
        seal(&mut d);
        assert!(parse_ivrs(&d).is_none());
    }

    #[test]
    fn entries_and_commands_are_laid_out_as_the_spec_says() {
        assert_eq!(devtab_register(0x1_2340_0000), 0x1_2340_01ff);
        assert_eq!(ring_register(0x5000, 8), 0x5000 | 8 << 56);
        assert_eq!(dte_pass_through(1), [0b11 | 3 << 61, 1]);
        assert_eq!(dte_blocked(7), [0b11, 7]);
        assert_eq!(dte_no_interrupts(), 1);
        assert_eq!(
            dte_interrupts(0x1234_5000),
            1 | 11 << 1 | 0x1234_5000 | 2 << 60
        );
        assert_eq!(irte(0x41, 3), 1 | 3 << 8 | 0x41 << 16);
        assert_eq!(cmd_invalidate_interrupts(0x0108), [0x0108, 5 << 28, 0, 0]);
        let t = dte_translate(0xabc_d000, 7);
        assert_eq!(t[0] >> 9 & 7, 4, "four levels");
        assert_eq!(t[0] & PTE_ADDR, 0xabc_d000);
        assert_eq!(t[1], 7);
        assert_eq!(
            cmd_completion_wait(0x1_2345_6788, 0xdead_beef_0000_0001),
            [0x2345_6789, 1 | 1 << 28, 1, 0xdead_beef]
        );
        assert_eq!(cmd_invalidate_dte(0x00a0), [0xa0, 2 << 28, 0, 0]);
        assert_eq!(cmd_invalidate_domain(5)[1], 5 | 3 << 28);
        let e = decode_event([0x0038, 2 << 28 | 0x020 << 16, 0x7654_3000, 0]);
        assert_eq!(
            e,
            Event {
                code: EVENT_IO_PAGE_FAULT,
                device: 0x38,
                flags: 0x20,
                address: 0x7654_3000
            }
        );
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
    fn a_mapped_page_translates_to_itself_with_its_next_levels_right() {
        let mut mem = Mem::default();
        let root = mem.alloc().unwrap();
        let page = 0x7_1234_5000u64;
        map_page(&mut mem, root, page, page, READ).unwrap();
        assert_eq!(translate(&mem, root, page), Some((page, READ)));
        map_page(&mut mem, root, page, page, READ | WRITE).unwrap();
        assert_eq!(translate(&mem, root, page), Some((page, READ | WRITE)));
        assert_eq!(translate(&mem, root, page + 0x1000), None);
        assert_eq!(mem.tables.len(), LEVELS);
        unmap_page(&mut mem, root, page);
        assert_eq!(translate(&mem, root, page), None);
    }
}
