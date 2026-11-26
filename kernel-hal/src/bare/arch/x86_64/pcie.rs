//! A claimed function's way around the IOMMU, closed: ATS, PRI and PASID
//! off on the function, ACS on every port above it. The arithmetic is
//! `common/pcie.rs`; this finds the ports and reaches their extended
//! configuration space through the MCFG's ECAM windows.

use alloc::vec::Vec;
use core::ptr::{read_volatile, write_volatile};

use acpi::sdt::Signature;
use spin::Once;

use crate::common::iommu::PciRequester;
use crate::common::pcie::{self, Config, Ecam};
use crate::{VirtAddr, PAGE_SIZE};

use super::vtd::{cfg_read, map_registers};

static WINDOWS: Once<Vec<Ecam>> = Once::new();

fn windows() -> &'static [Ecam] {
    WINDOWS.call_once(|| {
        super::drivers::acpi_table(Signature::MCFG)
            .and_then(pcie::parse_mcfg)
            .unwrap_or_default()
    })
}

/// One function's configuration page, mapped uncached.
struct Ecam4k(VirtAddr);

impl Config for Ecam4k {
    fn read32(&self, offset: u16) -> u32 {
        // SAFETY: an aligned dword of the function's mapped 4 KiB page.
        unsafe { read_volatile((self.0 + (offset as usize & 0xffc)) as *const u32) }
    }
    fn read16(&self, offset: u16) -> u16 {
        // SAFETY: as above; ECAM takes 16-bit accesses.
        unsafe { read_volatile((self.0 + (offset as usize & 0xffe)) as *const u16) }
    }
    fn write16(&mut self, offset: u16, value: u16) {
        // SAFETY: as above.
        unsafe { write_volatile((self.0 + (offset as usize & 0xffe)) as *mut u16, value) }
    }
}

fn function(segment: u16, bus: u8, dev: u8, func: u8) -> Option<Ecam4k> {
    let page = pcie::window_for(windows(), segment, bus)?.function(segment, bus, dev, func)?;
    map_registers(page, PAGE_SIZE).map(Ecam4k)
}

/// Header type byte, and the bus numbers of a bridge.
const HEADER_TYPE: u16 = 0x0e;
const SECONDARY_BUS: u16 = 0x19;
const SUBORDINATE_BUS: u16 = 0x1a;

/// The bridges from bus 0 down to `bus`, top first: each `(bus, dev, func)`.
fn ports_above(bus: u8) -> Vec<(u8, u8, u8)> {
    let mut path = Vec::new();
    let mut at = 0u8;
    // A bus is at most 255 bridges deep; each step goes one bus down.
    while at != bus && path.len() < 256 {
        let mut next = None;
        'scan: for dev in 0..32u8 {
            for func in 0..8u8 {
                if cfg_read(at, dev, func, 0) == 0xff && cfg_read(at, dev, func, 1) == 0xff {
                    if func == 0 {
                        break;
                    }
                    continue;
                }
                let header = cfg_read(at, dev, func, HEADER_TYPE);
                if header & 0x7f == 1 {
                    let sec = cfg_read(at, dev, func, SECONDARY_BUS);
                    let sub = cfg_read(at, dev, func, SUBORDINATE_BUS);
                    if sec > at && (sec..=sub).contains(&bus) {
                        next = Some((dev, func, sec));
                        break 'scan;
                    }
                }
                if func == 0 && header & 0x80 == 0 {
                    break;
                }
            }
        }
        let Some((dev, func, sec)) = next else {
            break;
        };
        path.push((at, dev, func));
        at = sec;
    }
    path
}

/// Close what `r` could do around its domain. Nothing when the firmware
/// gives no ECAM window for it (its extended capabilities are out of reach:
/// a plain PCI function has none).
pub(super) fn isolate(r: PciRequester) {
    let Some(mut cfg) = function(r.segment, r.bus, r.device, r.function) else {
        return;
    };
    let q = pcie::quiet_translation(&mut cfg);
    if q != pcie::Quieted::default() {
        crate::klog_info!(
            "iommu: {:02x}:{:02x}.{}: turned off{}{}{}",
            r.bus,
            r.device,
            r.function,
            if q.ats { " ATS" } else { "" },
            if q.pri { " PRI" } else { "" },
            if q.pasid { " PASID" } else { "" }
        );
    }
    for (bus, dev, func) in ports_above(r.bus) {
        let Some(mut port) = function(r.segment, bus, dev, func) else {
            continue;
        };
        if let Some(bits) = pcie::enable_acs(&mut port) {
            crate::klog_info!(
                "iommu: port {bus:02x}:{dev:02x}.{func} above {:02x}:{:02x}.{}: ACS {bits:#x}",
                r.bus,
                r.device,
                r.function
            );
        }
    }
}

/// Legacy capability list pointer, and the PCI Express capability id.
const CAP_PTR: u16 = 0x34;
const CAP_PCIE: u8 = 0x10;
/// Device/port type of a PCI Express to PCI/PCI-X bridge.
const TYPE_PCIE_TO_PCI: u8 = 0x7;

/// The PCI Express device/port type of a function, or `None` when it has
/// no PCI Express capability (a conventional PCI function).
fn pcie_type(bus: u8, dev: u8, func: u8) -> Option<u8> {
    let mut at = cfg_read(bus, dev, func, CAP_PTR) & !3;
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        if cfg_read(bus, dev, func, at as u16) == CAP_PCIE {
            return Some(cfg_read(bus, dev, func, at as u16 + 2) >> 4);
        }
        at = cfg_read(bus, dev, func, at as u16 + 1) & !3;
    }
    None
}

/// When a function on `bus` sits behind a conventional PCI bridge (a PCI
/// Express to PCI one, or a plain PCI-to-PCI bridge), its requests reach
/// the IOMMU carrying an id of that bridge's secondary side rather than its
/// own: the buses, inclusive, they may carry, from the topmost such bridge.
/// `None` when every bridge above is PCI Express, and its own id is seen.
pub(super) fn conventional_bridge_above(bus: u8) -> Option<(u8, u8)> {
    ports_above(bus).into_iter().find_map(|(b, d, f)| {
        let conventional = match pcie_type(b, d, f) {
            None => true,
            Some(t) => t == TYPE_PCIE_TO_PCI,
        };
        conventional.then(|| {
            (
                cfg_read(b, d, f, SECONDARY_BUS),
                cfg_read(b, d, f, SUBORDINATE_BUS),
            )
        })
    })
}
