//! Arm SMMUv3, the arithmetic half (IHI 0070): registers, stream table and
//! context descriptor entries, commands, events, and the stage 1 page
//! tables (VMSAv8-64, 4 KiB granule, 48-bit input). The hardware half is
//! `bare/arch/aarch64/smmuv3.rs`.
//!
//! The design is VT-d's and AMD-Vi's: every stream bypasses but a claimed
//! function's, whose domain is a sparse identity map (address == physical
//! address) of what its driver pins. Stage 1, because that is what every
//! SMMUv3 has (QEMU's `virt` gives only stage 1 by default).

use crate::common::vtd::TableMem;
use crate::{HalError, HalResult, PhysAddr};

/// Register offsets (page 0, and page 1 at 64 KiB).
pub mod reg {
    pub const IDR0: usize = 0x00;
    pub const IDR1: usize = 0x04;
    pub const IDR5: usize = 0x14;
    pub const CR0: usize = 0x20;
    pub const CR0ACK: usize = 0x24;
    pub const CR1: usize = 0x28;
    pub const CR2: usize = 0x2c;
    pub const GBPA: usize = 0x44;
    pub const IRQ_CTRL: usize = 0x50;
    pub const GERROR: usize = 0x60;
    pub const STRTAB_BASE: usize = 0x80;
    pub const STRTAB_BASE_CFG: usize = 0x88;
    pub const CMDQ_BASE: usize = 0x90;
    pub const CMDQ_PROD: usize = 0x98;
    pub const CMDQ_CONS: usize = 0x9c;
    pub const EVTQ_BASE: usize = 0xa0;
    pub const EVTQ_PROD: usize = 0x1_00a8;
    pub const EVTQ_CONS: usize = 0x1_00ac;
    /// Bytes of registers this touches: both pages.
    pub const SPAN: usize = 0x2_0000;
}

/// CR0 bits.
pub mod cr0 {
    pub const SMMUEN: u32 = 1 << 0;
    pub const EVTQEN: u32 = 1 << 2;
    pub const CMDQEN: u32 = 1 << 3;
}

/// What the ID registers say this unit can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub two_level: bool,
    pub stage1: bool,
    pub aarch64_tables: bool,
    pub coherent: bool,
    pub asid16: bool,
    pub sid_bits: u32,
    pub cmdq_log2: u32,
    pub evtq_log2: u32,
    /// Output address size, as the IPS/OAS encoding (0 = 32 bits ... 5 = 48).
    pub oas: u32,
    pub gran4k: bool,
}

impl Caps {
    pub fn read(idr0: u32, idr1: u32, idr5: u32) -> Self {
        Caps {
            two_level: (idr0 >> 27) & 3 == 1,
            stage1: idr0 & 2 != 0,
            // TTF: 2 = AArch64, 3 = both.
            aarch64_tables: (idr0 >> 2) & 2 != 0,
            coherent: idr0 & (1 << 4) != 0,
            asid16: idr0 & (1 << 12) != 0,
            sid_bits: idr1 & 0x3f,
            cmdq_log2: (idr1 >> 21) & 0x1f,
            evtq_log2: (idr1 >> 16) & 0x1f,
            oas: idr5 & 7,
            gran4k: idr5 & (1 << 4) != 0,
        }
    }

    /// Why this kernel cannot use the unit, if it cannot.
    pub fn unusable(&self) -> Option<&'static str> {
        if !self.stage1 {
            Some("no stage 1 translation")
        } else if !self.aarch64_tables {
            Some("no AArch64 page tables")
        } else if !self.gran4k {
            Some("no 4 KiB granule")
        } else if self.sid_bits > 16 && !self.two_level {
            Some("a stream table too big to be linear")
        } else {
            None
        }
    }
}

/// CR1: tables and queues write-back cacheable, inner shareable.
pub const CR1_WB_ISH: u32 = 3 << 10 | 1 << 8 | 1 << 6 | 3 << 4 | 1 << 2 | 1;
/// CR2: record events for invalid stream ids.
pub const CR2_RECINVSID: u32 = 1 << 1;
/// Read-allocate (tables) and write-allocate (event queue) hints.
const RA: u64 = 1 << 62;

/// The stream table's split: each second-level table holds 2^SPLIT
/// entries, one PCI bus's worth.
pub const SPLIT: u32 = 8;
/// One stream table entry, and one context descriptor.
pub const STE_BYTES: usize = 64;
pub const CD_BYTES: usize = 64;

/// STRTAB_BASE for a table at `table`.
pub fn strtab_base(table: PhysAddr) -> u64 {
    RA | (table as u64 & 0x000f_ffff_ffff_ffc0)
}

/// STRTAB_BASE_CFG: two levels split at [`SPLIT`], or linear, for
/// `sid_bits` stream id bits.
pub fn strtab_cfg(two_level: bool, sid_bits: u32) -> u32 {
    if two_level {
        1 << 16 | SPLIT << 6 | sid_bits
    } else {
        sid_bits
    }
}

/// A first-level descriptor for a second-level table of 2^SPLIT entries.
pub fn l1_desc(l2: PhysAddr) -> u64 {
    (l2 as u64 & 0x000f_ffff_ffff_ffc0) | (SPLIT as u64 + 1)
}

/// A queue base register: `base` with 2^log2 entries.
pub fn queue_base(base: PhysAddr, log2: u32) -> u64 {
    RA | (base as u64 & 0x000f_ffff_ffff_ffe0) | log2 as u64
}

/// A stream that bypasses the SMMU: its transactions go out as they came.
pub fn ste_bypass() -> [u64; 8] {
    // Valid, config 0b100; shareability as incoming (SHCFG 1).
    [1 | 4 << 1, 1 << 44, 0, 0, 0, 0, 0, 0]
}

/// A stream whose every transaction is aborted.
pub fn ste_abort() -> [u64; 8] {
    // Valid, config 0b000.
    [1, 0, 0, 0, 0, 0, 0, 0]
}

/// A stream translated by stage 1 through the one context descriptor at
/// `cd`.
pub fn ste_s1(cd: PhysAddr) -> [u64; 8] {
    let dw0 = 1 | 5 << 1 | (cd as u64 & 0x000f_ffff_ffff_ffc0);
    // Context descriptor fetches write-back cacheable, inner shareable;
    // NS-EL1 stream world.
    let dw1 = 1 << 2 | 1 << 4 | 3 << 6;
    [dw0, dw1, 0, 0, 0, 0, 0, 0]
}

/// MAIR: index 0 normal write-back memory, the only attribute the tables use.
pub const MAIR: u64 = 0xff;

/// The context descriptor of a domain: translation table `ttb`, 48-bit
/// input, output size `ips` (the OAS encoding), address space `asid`. A
/// fault is recorded and the transaction aborted, never stalled.
pub fn cd(ttb: PhysAddr, ips: u32, asid: u16) -> [u64; 8] {
    let dw0 = 16 // T0SZ: 48 bits
        | 1 << 8 // IR0 write-back
        | 1 << 10 // OR0 write-back
        | 3 << 12 // SH0 inner shareable
        | 1 << 30 // EPD1: no TTB1
        | 1 << 31 // V
        | (ips.min(5) as u64) << 32
        | 1 << 41 // AA64
        | 1 << 45 // R: record faults
        | 1 << 46 // A: abort on fault
        | 1 << 47 // ASET: an ASID of its own, not the CPU's
        | (asid as u64) << 48;
    [dw0, ttb as u64 & 0x000f_ffff_ffff_fff0, 0, MAIR, 0, 0, 0, 0]
}

/// Commands, 16 bytes each.
pub mod cmd {
    pub fn cfgi_ste(sid: u32) -> [u64; 2] {
        [0x03 | (sid as u64) << 32, 1]
    }
    pub fn cfgi_cd(sid: u32) -> [u64; 2] {
        [0x05 | (sid as u64) << 32, 1]
    }
    pub fn cfgi_all() -> [u64; 2] {
        [0x04, 31]
    }
    pub fn tlbi_asid(asid: u16) -> [u64; 2] {
        [0x11 | (asid as u64) << 48, 0]
    }
    pub fn tlbi_nsnh_all() -> [u64; 2] {
        [0x30, 0]
    }
    /// A sync with no signal: completion is CONS passing it.
    pub fn sync() -> [u64; 2] {
        [0x46, 0]
    }
}

/// Queue arithmetic: an index register is the slot in its low `log2` bits
/// and a wrap flag above them.
pub fn queue_next(at: u32, log2: u32) -> u32 {
    let mask = (2u32 << log2) - 1;
    (at + 1) & mask
}

pub fn queue_slot(at: u32, log2: u32) -> usize {
    (at & ((1 << log2) - 1)) as usize
}

/// Whether the consumer at `cons` has caught up with the producer index
/// `prod` (both with wrap flags): everything before `prod` is done.
pub fn queue_passed(cons: u32, prod: u32, log2: u32) -> bool {
    let mask = (2u32 << log2) - 1;
    (cons & mask) == (prod & mask)
}

/// One event, as the event queue records it (32 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub code: u8,
    pub stream: u32,
    pub address: u64,
}

pub fn decode_event(e: [u64; 4]) -> Event {
    Event {
        code: e[0] as u8,
        stream: (e[0] >> 32) as u32,
        address: e[2],
    }
}

/// The name of an event code.
pub fn event_name(code: u8) -> &'static str {
    match code {
        0x02 => "bad stream id",
        0x04 => "bad stream table entry",
        0x06 => "stream disabled",
        0x0a => "bad context descriptor",
        0x10 => "translation fault (not in its domain)",
        0x11 => "address size fault",
        0x12 => "access flag fault",
        0x13 => "permission fault",
        _ => "event",
    }
}

/// Page table bits (VMSAv8-64 stage 1).
const VALID: u64 = 1;
const TABLE: u64 = 1 << 1;
const PAGE: u64 = 1 << 1;
/// Access flag: set, so no access flag fault.
const AF: u64 = 1 << 10;
/// Inner shareable.
const SH_ISH: u64 = 3 << 8;
/// AP[1]: unprivileged access allowed (a device's transactions are
/// unprivileged); AP[2]: read only.
const AP_UNPRIV: u64 = 1 << 6;
const AP_RDONLY: u64 = 1 << 7;
/// Execute never, for both privileges.
const XN: u64 = 3 << 53;
const ADDR: u64 = 0x0000_ffff_ffff_f000;
const LEVELS: usize = 4;
/// Input address bits.
pub const ADDR_BITS: u32 = 48;

pub const READ: u64 = 1;
pub const WRITE: u64 = 2;

fn index(iova: u64, level: usize) -> usize {
    ((iova >> (12 + 9 * (level - 1))) & 0x1ff) as usize
}

fn leaf(paddr: u64, perms: u64) -> u64 {
    let ap = if perms & WRITE != 0 { 0 } else { AP_RDONLY };
    (paddr & ADDR) | VALID | PAGE | AF | SH_ISH | AP_UNPRIV | ap | XN
}

/// Map the 4 KiB page `iova` to `paddr`. `perms` must hold `READ`: a
/// stage 1 page is always readable.
pub fn map_page(
    mem: &mut impl TableMem,
    root: PhysAddr,
    iova: u64,
    paddr: u64,
    perms: u64,
) -> HalResult {
    if perms & READ == 0 || iova >> ADDR_BITS != 0 {
        return Err(HalError);
    }
    let mut table = root;
    for level in (2..=LEVELS).rev() {
        let i = index(iova, level);
        let entry = mem.read(table, i);
        table = if entry & (VALID | TABLE) == VALID | TABLE {
            (entry & ADDR) as PhysAddr
        } else {
            let next = mem.alloc().ok_or(HalError)?;
            mem.write(table, i, next as u64 | VALID | TABLE);
            next
        };
    }
    mem.write(table, index(iova, 1), leaf(paddr, perms));
    Ok(())
}

pub fn unmap_page(mem: &mut impl TableMem, root: PhysAddr, iova: u64) {
    let mut table = root;
    for level in (2..=LEVELS).rev() {
        let entry = mem.read(table, index(iova, level));
        if entry & (VALID | TABLE) != VALID | TABLE {
            return;
        }
        table = (entry & ADDR) as PhysAddr;
    }
    let i = index(iova, 1);
    if mem.read(table, i) != 0 {
        mem.write(table, i, 0);
    }
}

/// Where `iova` goes and whether it may be written, by the tables.
pub fn translate(mem: &impl TableMem, root: PhysAddr, iova: u64) -> Option<(u64, bool)> {
    let mut table = root;
    for level in (2..=LEVELS).rev() {
        let entry = mem.read(table, index(iova, level));
        if entry & (VALID | TABLE) != VALID | TABLE {
            return None;
        }
        table = (entry & ADDR) as PhysAddr;
    }
    let leaf = mem.read(table, index(iova, 1));
    (leaf & VALID != 0).then_some((leaf & ADDR, leaf & AP_RDONLY == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{collections::BTreeMap, vec::Vec};

    struct Mem {
        tables: BTreeMap<PhysAddr, Vec<u64>>,
        next: PhysAddr,
    }

    impl TableMem for Mem {
        fn alloc(&mut self) -> Option<PhysAddr> {
            self.next += 0x1000;
            self.tables.insert(self.next, alloc::vec![0; 512]);
            Some(self.next)
        }
        fn read(&self, t: PhysAddr, i: usize) -> u64 {
            self.tables[&t][i]
        }
        fn write(&mut self, t: PhysAddr, i: usize, v: u64) {
            self.tables.get_mut(&t).unwrap()[i] = v;
        }
    }

    fn mem() -> (Mem, PhysAddr) {
        let mut m = Mem {
            tables: BTreeMap::new(),
            next: 0x10_0000,
        };
        let root = m.alloc().unwrap();
        (m, root)
    }

    #[test]
    fn a_page_maps_through_four_levels_and_unmaps() {
        let (mut m, root) = mem();
        map_page(&mut m, root, 0x4123_4000, 0x4123_4000, READ | WRITE).unwrap();
        map_page(&mut m, root, 0x7fff_f000, 0x7fff_f000, READ).unwrap();
        assert_eq!(m.tables.len(), 1 + 3 + 1);
        assert_eq!(translate(&m, root, 0x4123_4000), Some((0x4123_4000, true)));
        assert_eq!(translate(&m, root, 0x7fff_f000), Some((0x7fff_f000, false)));
        assert_eq!(translate(&m, root, 0x4123_5000), None);
        // The leaf is a page with its access flag set, never executable.
        let l1 = m.tables.values().last().unwrap();
        let e = l1.iter().copied().find(|&e| e != 0).unwrap();
        assert_eq!(e & 3, 3);
        assert_ne!(e & AF, 0);
        assert_eq!(e & XN, XN);
        unmap_page(&mut m, root, 0x4123_4000);
        assert_eq!(translate(&m, root, 0x4123_4000), None);
        assert!(map_page(&mut m, root, 0x1000, 0x1000, WRITE).is_err());
        assert!(map_page(&mut m, root, 1 << 48, 0, READ).is_err());
    }

    #[test]
    fn the_id_registers_decode() {
        // QEMU 8.2: two levels, stage 1, AArch64 tables, coherent, 16-bit
        // ASIDs; 16 stream id bits, queues of 2^19 and 2^19; 44-bit OAS,
        // 4 KiB granule.
        let c = Caps::read(
            1 << 27 | 2 | 2 << 2 | 1 << 4 | 1 << 12,
            16 | 19 << 16 | 19 << 21,
            4 | 1 << 4,
        );
        assert!(c.two_level && c.stage1 && c.aarch64_tables && c.coherent && c.asid16);
        assert_eq!(
            (c.sid_bits, c.cmdq_log2, c.evtq_log2, c.oas),
            (16, 19, 19, 4)
        );
        assert!(c.gran4k);
        assert_eq!(c.unusable(), None);
        let c = Caps::read(0, 16, 1 << 4);
        assert_eq!(c.unusable(), Some("no stage 1 translation"));
    }

    #[test]
    fn entries_say_what_they_should() {
        assert_eq!(ste_bypass()[0] & 0xf, 0b1001);
        assert_eq!(ste_abort()[0] & 0xf, 1);
        let s = ste_s1(0x4000_0040);
        assert_eq!(s[0] & 0xf, 0b1011);
        assert_eq!(s[0] & !0x3f, 0x4000_0040);
        let d = cd(0x5000_0000, 4, 7);
        assert_eq!(d[0] & 0x3f, 16);
        assert_ne!(d[0] & 1 << 31, 0);
        assert_eq!(d[0] >> 48, 7);
        assert_eq!((d[0] >> 32) & 7, 4);
        assert_eq!(d[1], 0x5000_0000);
        assert_eq!(d[3], MAIR);
        assert_eq!(l1_desc(0x6000_0000), 0x6000_0000 | 9);
        assert_eq!(strtab_cfg(true, 16), 1 << 16 | 8 << 6 | 16);
        assert_eq!(cmd::cfgi_ste(0x0108), [0x03 | 0x0108 << 32, 1]);
    }

    #[test]
    fn queue_indices_wrap_with_their_flag() {
        // A queue of 4 entries: slots 0..3, then the wrap flag (bit 2).
        let mut at = 0;
        for _ in 0..4 {
            at = queue_next(at, 2);
        }
        assert_eq!(at, 0b100);
        assert_eq!(queue_slot(at, 2), 0);
        assert_eq!(queue_next(0b111, 2), 0);
        assert!(queue_passed(0b101, 0b101, 2));
        assert!(!queue_passed(0b100, 0b101, 2));
    }
}
