//! What a claimed PCIe function may still do behind the IOMMU's back, and
//! how it is stopped: the arithmetic half (the MCFG table, the extended
//! capability list, the bits). The hardware half is
//! `bare/arch/x86_64/pcie.rs`.
//!
//! A domain of its own limits what a function's DMA reaches. Three PCIe
//! features could still go around it:
//!
//! - **Peer-to-peer.** A switch or a root port may route a request whose
//!   address is another function's BAR straight there, never up to the
//!   IOMMU. Access Control Services (ACS) on every port above the function
//!   send such requests upstream instead (source validation, request and
//!   completion redirect, upstream forwarding), where the IOMMU sees them.
//! - **Address translation (ATS) and page requests (PRI).** With ATS the
//!   function asks for translations and caches them, then sends requests
//!   already translated; PRI lets it ask for pages to be made present. The
//!   kernel uses neither: both are turned off.
//! - **PASID.** Requests tagged with a process address space id reach
//!   tables this kernel never sets up. Off too.
//!
//! The driver in a process reaches none of these registers: it has no
//! access to configuration space, so what is turned off here stays off.

use alloc::vec::Vec;
use core::convert::TryInto;

use crate::PhysAddr;

/// One ECAM window of the MCFG table: configuration space of `start_bus`
/// to `end_bus` of `segment`, 4 KiB per function, at `base`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ecam {
    pub base: PhysAddr,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

impl Ecam {
    /// The physical address of the configuration page of `bus:dev.func`,
    /// when this window covers it.
    pub fn function(&self, segment: u16, bus: u8, dev: u8, func: u8) -> Option<PhysAddr> {
        if segment != self.segment
            || bus < self.start_bus
            || bus > self.end_bus
            || dev > 31
            || func > 7
        {
            return None;
        }
        let at =
            ((bus - self.start_bus) as usize) << 20 | (dev as usize) << 15 | (func as usize) << 12;
        Some(self.base + at)
    }
}

/// Where the MCFG entries start: the header and 8 reserved bytes.
const MCFG_ENTRIES: usize = 44;
const MCFG_ENTRY_LEN: usize = 16;

/// Read an MCFG table. `None` when it is not one.
pub fn parse_mcfg(table: &[u8]) -> Option<Vec<Ecam>> {
    if !super::acpi_tables::table_is(table, b"MCFG") || table.len() < MCFG_ENTRIES {
        return None;
    }
    Some(
        table[MCFG_ENTRIES..]
            .chunks(MCFG_ENTRY_LEN)
            .filter(|e| e.len() == MCFG_ENTRY_LEN)
            .map(|e| Ecam {
                base: u64::from_le_bytes(e[..8].try_into().unwrap()) as PhysAddr,
                segment: u16::from_le_bytes([e[8], e[9]]),
                start_bus: e[10],
                end_bus: e[11],
            })
            .filter(|e| e.base != 0 && e.end_bus >= e.start_bus)
            .collect(),
    )
}

/// The ECAM window that covers `bus` of `segment`.
pub fn window_for(windows: &[Ecam], segment: u16, bus: u8) -> Option<&Ecam> {
    windows
        .iter()
        .find(|w| w.segment == segment && (w.start_bus..=w.end_bus).contains(&bus))
}

/// One function's 4 KiB of configuration space.
pub trait Config {
    fn read32(&self, offset: u16) -> u32;
    fn read16(&self, offset: u16) -> u16 {
        (self.read32(offset & !3) >> ((offset & 2) * 8)) as u16
    }
    fn write16(&mut self, offset: u16, value: u16);
}

/// Extended capability ids (PCIe 5.0 §7.6).
pub const CAP_ACS: u16 = 0x0d;
pub const CAP_ATS: u16 = 0x0f;
pub const CAP_PRI: u16 = 0x13;
pub const CAP_PASID: u16 = 0x1b;

/// Where the extended capability list starts.
const EXT_START: u16 = 0x100;
/// More than this many entries is a loop: 4 KiB of 4-byte headers, less
/// the legacy 256 bytes.
const EXT_MAX: usize = (4096 - 256) / 4;

/// The offset of each extended capability, with its id, in list order.
pub fn ext_caps(cfg: &impl Config) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let mut at = EXT_START;
    for _ in 0..EXT_MAX {
        let header = cfg.read32(at);
        if header == 0 || header == u32::MAX {
            break;
        }
        out.push((header as u16, at));
        let next = (header >> 20) as u16 & !3;
        if next < EXT_START || next <= at {
            break;
        }
        at = next;
    }
    out
}

fn find(cfg: &impl Config, id: u16) -> Option<u16> {
    ext_caps(cfg)
        .into_iter()
        .find(|&(cap, _)| cap == id)
        .map(|(_, at)| at)
}

/// ACS control bits (§7.7.8).
pub mod acs {
    /// Source validation: a request must carry a requester id below the port.
    pub const SV: u16 = 1 << 0;
    /// Translation blocking: requests marked translated are refused.
    pub const TB: u16 = 1 << 1;
    /// P2P request redirect.
    pub const RR: u16 = 1 << 2;
    /// P2P completion redirect.
    pub const CR: u16 = 1 << 3;
    /// Upstream forwarding.
    pub const UF: u16 = 1 << 4;
    /// What a port above a claimed function is given, where it has it. TB
    /// too: ATS is off, so a translated request is never legitimate.
    pub const WANTED: u16 = SV | TB | RR | CR | UF;
}

/// Turn on, in a port's ACS capability, every bit of [`acs::WANTED`] the
/// port implements. The bits now on, or `None` when it has no ACS.
pub fn enable_acs(cfg: &mut impl Config) -> Option<u16> {
    let at = find(cfg, CAP_ACS)?;
    let implemented = cfg.read16(at + 4);
    let control = cfg.read16(at + 6);
    let wanted = control | (implemented & acs::WANTED);
    if wanted != control {
        cfg.write16(at + 6, wanted);
    }
    Some(cfg.read16(at + 6) & acs::WANTED)
}

/// What [`quiet_translation`] found on: each was turned off.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Quieted {
    pub ats: bool,
    pub pri: bool,
    pub pasid: bool,
}

const ATS_ENABLE: u16 = 1 << 15;
const PRI_ENABLE: u16 = 1 << 0;
const PASID_ENABLE: u16 = 1 << 0;

/// Turn off ATS, PRI and PASID on a function. What was on before.
pub fn quiet_translation(cfg: &mut impl Config) -> Quieted {
    let mut q = Quieted::default();
    for (id, at) in ext_caps(cfg) {
        let (control, bit, flag) = match id {
            CAP_ATS => (at + 6, ATS_ENABLE, &mut q.ats),
            CAP_PRI => (at + 4, PRI_ENABLE, &mut q.pri),
            CAP_PASID => (at + 6, PASID_ENABLE, &mut q.pasid),
            _ => continue,
        };
        let value = cfg.read16(control);
        if value & bit != 0 {
            cfg.write16(control, value & !bit);
            *flag = true;
        }
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A function's configuration space as plain bytes.
    struct Fake([u8; 4096]);

    impl Config for Fake {
        fn read32(&self, o: u16) -> u32 {
            let o = o as usize;
            u32::from_le_bytes(self.0[o..o + 4].try_into().unwrap())
        }
        fn write16(&mut self, o: u16, v: u16) {
            let o = o as usize;
            self.0[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    impl Fake {
        fn cap(&mut self, at: u16, id: u16, next: u16) {
            let h = id as u32 | 1 << 16 | (next as u32) << 20;
            let o = at as usize;
            self.0[o..o + 4].copy_from_slice(&h.to_le_bytes());
        }
        fn w16(&mut self, at: u16, v: u16) {
            self.write16(at, v);
        }
    }

    fn device() -> Fake {
        let mut f = Fake([0; 4096]);
        f.cap(0x100, 0x01, 0x150); // AER
        f.cap(0x150, CAP_ATS, 0x160);
        f.w16(0x156, ATS_ENABLE | 0x1f);
        f.cap(0x160, CAP_PRI, 0x180);
        f.w16(0x164, PRI_ENABLE);
        f.cap(0x180, CAP_PASID, 0x1a0);
        f.w16(0x186, PASID_ENABLE | 1 << 1);
        f.cap(0x1a0, CAP_ACS, 0);
        f.w16(0x1a4, acs::SV | acs::RR | acs::CR | acs::UF | 1 << 6);
        f
    }

    #[test]
    fn the_list_is_walked_in_order_and_a_loop_ends_it() {
        let f = device();
        let ids: Vec<u16> = ext_caps(&f).iter().map(|c| c.0).collect();
        assert_eq!(ids, [0x01, CAP_ATS, CAP_PRI, CAP_PASID, CAP_ACS]);
        let mut looped = Fake([0; 4096]);
        looped.cap(0x100, 0x01, 0x140);
        looped.cap(0x140, CAP_ACS, 0x100);
        assert_eq!(ext_caps(&looped).len(), 2);
        assert!(ext_caps(&Fake([0xff; 4096])).is_empty());
    }

    #[test]
    fn translation_features_are_turned_off_and_nothing_else() {
        let mut f = device();
        let q = quiet_translation(&mut f);
        assert_eq!(
            q,
            Quieted {
                ats: true,
                pri: true,
                pasid: true
            }
        );
        assert_eq!(f.read16(0x156), 0x1f);
        assert_eq!(f.read16(0x164), 0);
        assert_eq!(f.read16(0x186), 1 << 1);
        assert_eq!(quiet_translation(&mut f), Quieted::default());
    }

    #[test]
    fn acs_gets_what_the_port_implements_of_what_is_wanted() {
        let mut f = device();
        assert_eq!(
            enable_acs(&mut f),
            Some(acs::SV | acs::RR | acs::CR | acs::UF)
        );
        assert_eq!(f.read16(0x1a6), acs::SV | acs::RR | acs::CR | acs::UF);
        assert_eq!(enable_acs(&mut Fake([0; 4096])), None);
    }

    fn seal(t: &mut [u8]) {
        t[9] = 0;
        let sum = t.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        t[9] = 0u8.wrapping_sub(sum);
    }

    #[test]
    fn an_mcfg_gives_its_windows_and_their_function_pages() {
        let mut t = alloc::vec![0u8; MCFG_ENTRIES + 2 * MCFG_ENTRY_LEN];
        t[..4].copy_from_slice(b"MCFG");
        let len = t.len() as u32;
        t[4..8].copy_from_slice(&len.to_le_bytes());
        t[44..52].copy_from_slice(&0xb000_0000u64.to_le_bytes());
        t[54] = 0;
        t[55] = 0xff;
        // An empty entry is dropped.
        seal(&mut t);
        let w = parse_mcfg(&t).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].function(0, 0, 3, 0), Some(0xb000_0000 + (3 << 15)));
        assert_eq!(
            w[0].function(0, 2, 1, 7),
            Some(0xb000_0000 + (2 << 20) + (1 << 15) + (7 << 12))
        );
        assert_eq!(w[0].function(1, 0, 0, 0), None);
        assert_eq!(window_for(&w, 0, 5), Some(&w[0]));
        t[0] = b'X';
        assert!(parse_mcfg(&t).is_none());
    }
}
