//! AMD-Vi, the hardware half: find the IOMMUs the IVRS table names, give
//! every requester a pass-through device table entry, turn them on, and
//! give a device a domain of its own when a process claims it. The
//! arithmetic is `common/amdvi.rs`; the default (on, `iommu=off` leaves
//! it off) and the all-or-nothing rule are VT-d's (`vtd.rs`).

use alloc::{sync::Arc, vec::Vec};
use core::convert::TryFrom;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

use acpi::sdt::Signature;
use lock::Mutex;

use super::vtd::{flush_lines, map_registers};
use crate::common::amdvi::{self, ctl, reg, Ivrs};
use crate::common::iommu::{DmaDomain, IommuUnit, PciRequester};
use crate::common::vtd::TableMem;
use crate::mem::{phys_to_virt, PhysFrame};
use crate::{HalError, HalResult, MMUFlags, PhysAddr, VirtAddr, KCONFIG, PAGE_SIZE};

/// Domain id of the pass-through entries; claimed devices get 2 and up.
const PASS_THROUGH_DID: u16 = 1;
/// AMD-Vi domain ids are 16 bits.
const DOMAINS: u16 = u16::MAX;
/// Bytes of registers this touches (up to the status register).
const REGS: usize = 0x3000;
/// Ring sizes: 2^8 entries of 16 bytes, one page each.
const RING_LOG2: u64 = 8;
const RING_ENTRIES: usize = 1 << RING_LOG2;
/// How long a completion wait is given.
const SPINS: usize = 10_000_000;

struct Unit {
    index: usize,
    regs: VirtAddr,
    /// The device table (2 MiB, 65536 entries), command ring, event ring
    /// and the word completion waits store to.
    devtab: Vec<PhysFrame>,
    cmdbuf: PhysFrame,
    evtlog: PhysFrame,
    sem: PhysFrame,
    /// The command ring's tail, and everything that writes the ring.
    tail: Mutex<usize>,
    stamp: AtomicU64,
    dids: Mutex<Vec<u16>>,
    faults: AtomicU64,
    /// It remaps interrupts: the IVRS names an IOAPIC behind it
    /// ([`Ivhd::ioapic`](amdvi::Ivhd)).
    remaps: bool,
}

impl Unit {
    fn r64(&self, off: usize) -> u64 {
        // SAFETY: `regs` maps the unit's register span.
        unsafe { read_volatile((self.regs + off) as *const u64) }
    }
    fn w64(&self, off: usize, v: u64) {
        // SAFETY: as `r64`.
        unsafe { write_volatile((self.regs + off) as *mut u64, v) }
    }

    /// Queue `cmds` then a completion wait, and wait for it: when this
    /// returns the unit has done them all.
    fn run(&self, cmds: &[[u32; 4]]) {
        let mut tail = self.tail.lock();
        let stamp = self.stamp.fetch_add(1, Ordering::Relaxed) + 1;
        let sem = self.sem.paddr();
        let ring = phys_to_virt(self.cmdbuf.paddr()) as *mut u32;
        let wait = amdvi::cmd_completion_wait(sem, stamp);
        for cmd in cmds.iter().chain(core::iter::once(&wait)) {
            // Room: the ring is never let fill, as each batch waits.
            for (i, &w) in cmd.iter().enumerate() {
                // SAFETY: an entry of the ring page, through the linear map.
                unsafe { write_volatile(ring.add(*tail * 4 + i), w) };
            }
            *tail = (*tail + 1) % RING_ENTRIES;
        }
        // SAFETY: a fence has no operands.
        unsafe { core::arch::x86_64::_mm_mfence() };
        self.w64(reg::CMD_TAIL, (*tail * 16) as u64);
        let word = phys_to_virt(sem) as *const u64;
        // SAFETY: the unit stores `stamp` there when it reaches the wait.
        if !(0..SPINS).any(|_| unsafe { read_volatile(word) } >= stamp) {
            crate::klog_err!(
                "iommu: amd-vi unit {}: a command never completed",
                self.index
            );
        }
    }

    /// Write the first two qwords of `rid`'s device table entry (the high
    /// one first, so the unit never pairs a new root with an old domain),
    /// and have the unit forget the old one.
    fn set_dte(&self, rid: u16, dte: [u64; 2], old_did: u16) {
        let at = rid as usize * 32;
        let frame = &self.devtab[at / PAGE_SIZE];
        let va = phys_to_virt(frame.paddr()) + at % PAGE_SIZE;
        // SAFETY: an entry of the device table.
        unsafe {
            write_volatile((va + 8) as *mut u64, dte[1]);
            write_volatile(va as *mut u64, dte[0]);
        }
        self.run(&[
            amdvi::cmd_invalidate_dte(rid),
            amdvi::cmd_invalidate_domain(old_did),
        ]);
    }

    /// Write the interrupt qword of `rid`'s device table entry, and have
    /// the unit forget the entry and its interrupt table.
    fn set_interrupts(&self, rid: u16, qword: u64) {
        let at = rid as usize * 32;
        let frame = &self.devtab[at / PAGE_SIZE];
        let va = phys_to_virt(frame.paddr()) + at % PAGE_SIZE;
        // SAFETY: an entry of the device table.
        unsafe { write_volatile((va + 16) as *mut u64, qword) };
        self.run(&[
            amdvi::cmd_invalidate_dte(rid),
            amdvi::cmd_invalidate_interrupts(rid),
        ]);
    }

    fn take_did(&self) -> Option<u16> {
        let mut used = self.dids.lock();
        let did = (PASS_THROUGH_DID + 1..DOMAINS).find(|d| !used.contains(d))?;
        used.push(did);
        Some(did)
    }

    fn give_did(&self, did: u16) {
        self.dids.lock().retain(|&d| d != did);
    }

    fn poll_faults(&self) {
        let ring = phys_to_virt(self.evtlog.paddr()) as *const u32;
        let mut head = self.r64(reg::EVT_HEAD) as usize / 16;
        let tail = self.r64(reg::EVT_TAIL) as usize / 16;
        while head != tail {
            // SAFETY: an entry of the event ring.
            let e = unsafe { core::array::from_fn(|i| read_volatile(ring.add(head * 4 + i))) };
            let ev = amdvi::decode_event(e);
            let n = self.faults.fetch_add(1, Ordering::Relaxed);
            if n < 64 || n.is_power_of_two() {
                crate::klog_warn!(
                    "iommu: DMA fault #{}: {:02x}:{:02x}.{} event {:#x} at {:#x}, flags {:#x}{}",
                    n + 1,
                    ev.device >> 8,
                    (ev.device >> 3) & 0x1f,
                    ev.device & 7,
                    ev.code,
                    ev.address,
                    ev.flags,
                    if ev.code == amdvi::EVENT_IO_PAGE_FAULT {
                        " (not in its domain)"
                    } else {
                        ""
                    },
                );
            }
            head = (head + 1) % RING_ENTRIES;
        }
        self.w64(reg::EVT_HEAD, (head * 16) as u64);
        // Overflow and the event interrupt bit are write-one-to-clear.
        let status = self.r64(reg::STATUS);
        if status & 0b11 != 0 {
            self.w64(reg::STATUS, status & 0b11);
        }
    }
}

struct Domain {
    unit: Arc<Unit>,
    rid: u16,
    did: u16,
    root: PhysAddr,
    tables: Mutex<Vec<PhysFrame>>,
    /// Its interrupt remapping table, once its interrupts are limited.
    interrupts: Mutex<Vec<PhysFrame>>,
}

/// The tables of one domain. The unit's walks are coherent (the control
/// register says so), so a write needs no cache flush.
struct Tables<'a> {
    frames: &'a mut Vec<PhysFrame>,
}

impl TableMem for Tables<'_> {
    fn alloc(&mut self) -> Option<PhysAddr> {
        let frame = PhysFrame::new_zero()?;
        let paddr = frame.paddr();
        self.frames.push(frame);
        Some(paddr)
    }
    fn read(&self, table: PhysAddr, index: usize) -> u64 {
        // SAFETY: a table of this domain.
        unsafe { read_volatile((phys_to_virt(table) + index * 8) as *const u64) }
    }
    fn write(&mut self, table: PhysAddr, index: usize, value: u64) {
        // SAFETY: as `read`.
        unsafe { write_volatile((phys_to_virt(table) + index * 8) as *mut u64, value) }
    }
}

impl Domain {
    fn map_pages(&self, paddr: PhysAddr, len: usize, perms: u64) -> HalResult {
        let end = paddr.checked_add(len).ok_or(HalError)?;
        if end as u64 > 1u64 << amdvi::ADDR_BITS {
            return Err(HalError);
        }
        let mut frames = self.tables.lock();
        let mut mem = Tables {
            frames: &mut frames,
        };
        for page in (paddr..end).step_by(PAGE_SIZE) {
            amdvi::map_page(&mut mem, self.root, page as u64, page as u64, perms)?;
        }
        Ok(())
    }
}

impl DmaDomain for Domain {
    fn map(&self, paddr: PhysAddr, len: usize, flags: MMUFlags) -> HalResult {
        let mut perms = 0;
        if flags.contains(MMUFlags::READ) {
            perms |= amdvi::READ;
        }
        if flags.contains(MMUFlags::WRITE) {
            perms |= amdvi::WRITE;
        }
        if perms == 0 {
            return Err(HalError);
        }
        self.map_pages(paddr, len, perms)?;
        // The unit may have cached the absence of these pages.
        self.unit.run(&[amdvi::cmd_invalidate_domain(self.did)]);
        Ok(())
    }

    fn unmap(&self, paddr: PhysAddr, len: usize) {
        let mut frames = self.tables.lock();
        let mut mem = Tables {
            frames: &mut frames,
        };
        for page in (paddr..paddr + len).step_by(PAGE_SIZE) {
            amdvi::unmap_page(&mut mem, self.root, page as u64);
        }
        drop(frames);
        self.unit.run(&[amdvi::cmd_invalidate_domain(self.did)]);
    }

    fn limit_interrupts(&self, vectors: &[u8]) -> bool {
        if !self.unit.remaps {
            return false;
        }
        let dest = zcore_drivers::irq::x86::Apic::bsp_apic_id();
        let mut table = self.interrupts.lock();
        if table.is_empty() {
            *table = PhysFrame::new_contiguous(amdvi::IRT_FRAMES, 12);
        }
        let Some(base) = table.first().map(|f| f.paddr()) else {
            return false;
        };
        let entries = phys_to_virt(base) as *mut u32;
        for index in 0..amdvi::IRT_ENTRIES {
            let entry = match u8::try_from(index) {
                Ok(v) if vectors.contains(&v) => amdvi::irte(v, dest),
                _ => 0,
            };
            // SAFETY: the frames are contiguous and hold IRT_ENTRIES
            // 4-byte entries; the linear map covers them.
            unsafe { write_volatile(entries.add(index), entry) };
        }
        self.unit
            .set_interrupts(self.rid, amdvi::dte_interrupts(base));
        true
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // No interrupts either, before the table they went through goes.
        self.unit
            .set_interrupts(self.rid, amdvi::dte_no_interrupts());
        // Blocked, not back to pass-through, before a single table frame
        // goes: the device must never walk a freed table, and one that had
        // a driver of its own never reaches every byte again. Its next
        // driver gets a new domain.
        self.unit
            .set_dte(self.rid, amdvi::dte_blocked(PASS_THROUGH_DID), self.did);
        self.unit.give_did(self.did);
        crate::klog_info!(
            "iommu: {:02x}:{:02x}.{} blocked until its next driver",
            self.rid >> 8,
            (self.rid >> 3) & 0x1f,
            self.rid & 7
        );
    }
}

struct AmdVi {
    ivrs: Ivrs,
    units: Vec<Arc<Unit>>,
}

impl IommuUnit for AmdVi {
    fn name(&self) -> &'static str {
        "amd-vi"
    }

    fn domain_for(&self, r: PciRequester) -> Option<Arc<dyn DmaDomain>> {
        let rid = r.rid();
        let unit = self.units[self.ivrs.unit_for(r.segment, rid)?].clone();
        let did = unit.take_did()?;
        let Some(root) = PhysFrame::new_zero() else {
            unit.give_did(did);
            return None;
        };
        let root_pa = root.paddr();
        let domain = Domain {
            unit: unit.clone(),
            rid,
            did,
            root: root_pa,
            tables: Mutex::new(alloc::vec![root]),
            interrupts: Mutex::new(Vec::new()),
        };
        for (start, len) in self.ivrs.reserved_for(rid) {
            if domain
                .map_pages(start, len, amdvi::READ | amdvi::WRITE)
                .is_err()
            {
                crate::klog_warn!("iommu: could not map unity region {start:#x}+{len:#x}; device stays in pass-through");
                unit.give_did(did);
                core::mem::forget(domain);
                return None;
            }
        }
        // Its ways around the domain closed before the domain is on.
        super::pcie::isolate(r);
        unit.set_dte(rid, amdvi::dte_translate(root_pa, did), PASS_THROUGH_DID);
        crate::klog_info!(
            "iommu: {:02x}:{:02x}.{} has a domain of its own (unit {}, id {})",
            r.bus,
            r.device,
            r.function,
            unit.index,
            did
        );
        Some(Arc::new(domain))
    }

    fn poll_faults(&self) {
        for unit in &self.units {
            unit.poll_faults();
        }
    }
}

fn bring_up(index: usize, base: u64, remaps: bool) -> Result<Unit, &'static str> {
    let regs = map_registers(base as PhysAddr, REGS).ok_or("its registers could not be mapped")?;
    // SAFETY: just mapped.
    let control = unsafe { read_volatile((regs + reg::CONTROL) as *const u64) };
    if control & ctl::IOMMU_EN != 0 {
        crate::klog_warn!(
            "iommu: amd-vi unit {index}: the firmware left it on; turning it off first"
        );
        // SAFETY: as above.
        unsafe { write_volatile((regs + reg::CONTROL) as *mut u64, 0) };
    }
    // SAFETY: as above.
    let features = unsafe { read_volatile((regs + reg::EXT_FEATURES) as *const u64) };
    let devtab = PhysFrame::new_contiguous(512, 12);
    if devtab.len() != 512 {
        return Err("no 2 MiB of contiguous memory for its device table");
    }
    let (cmdbuf, evtlog, sem) = (
        PhysFrame::new_zero().ok_or("no frame for its command ring")?,
        PhysFrame::new_zero().ok_or("no frame for its event ring")?,
        PhysFrame::new_zero().ok_or("no frame for its completion word")?,
    );
    let unit = Unit {
        index,
        regs,
        devtab,
        cmdbuf,
        evtlog,
        sem,
        tail: Mutex::new(0),
        stamp: AtomicU64::new(0),
        dids: Mutex::new(Vec::new()),
        faults: AtomicU64::new(0),
        remaps,
    };
    // Every requester in pass-through.
    let pt = amdvi::dte_pass_through(PASS_THROUGH_DID);
    for frame in &unit.devtab {
        let va = phys_to_virt(frame.paddr()) as *mut u64;
        for e in 0..PAGE_SIZE / 32 {
            // SAFETY: a frame of the device table, 32-byte entries.
            unsafe {
                write_volatile(va.add(e * 4 + 3), 0);
                write_volatile(va.add(e * 4 + 2), 0);
                write_volatile(va.add(e * 4 + 1), pt[1]);
                write_volatile(va.add(e * 4), pt[0]);
            }
        }
    }
    flush_lines(unit.devtab[0].paddr(), 512 * PAGE_SIZE);
    unit.w64(reg::DEVTAB, amdvi::devtab_register(unit.devtab[0].paddr()));
    unit.w64(
        reg::CMDBUF,
        amdvi::ring_register(unit.cmdbuf.paddr(), RING_LOG2),
    );
    unit.w64(
        reg::EVTLOG,
        amdvi::ring_register(unit.evtlog.paddr(), RING_LOG2),
    );
    unit.w64(reg::CMD_HEAD, 0);
    unit.w64(reg::CMD_TAIL, 0);
    unit.w64(reg::EVT_HEAD, 0);
    unit.w64(reg::EVT_TAIL, 0);
    unit.w64(
        reg::CONTROL,
        ctl::IOMMU_EN | ctl::CMD_BUF_EN | ctl::EVENT_LOG_EN | ctl::COHERENT,
    );
    // Whatever the unit cached from before is gone: all of it when it
    // knows the command (IASup, bit 6), else every entry one by one.
    if features & (1 << 6) != 0 {
        unit.run(&[amdvi::cmd_invalidate_all()]);
    } else {
        for chunk in (0..=0xffffu32).collect::<Vec<_>>().chunks(64) {
            let cmds: Vec<_> = chunk
                .iter()
                .map(|&rid| amdvi::cmd_invalidate_dte(rid as u16))
                .collect();
            unit.run(&cmds);
        }
    }
    Ok(unit)
}

/// Find and take over the machine's AMD-Vi units, unless the command line
/// says `iommu=off` or VT-d took the machine.
pub(super) fn init() {
    if crate::cmdline::value(KCONFIG.cmdline, "iommu") == Some("off")
        || crate::common::iommu::unit().is_some()
    {
        return;
    }
    let Some(ivrs) = super::drivers::acpi_table(Signature::IVRS).and_then(amdvi::parse_ivrs) else {
        crate::klog_info!(
            "iommu: the firmware describes no IOMMU (no DMAR, no IVRS): devices keep full DMA"
        );
        return;
    };
    if ivrs.units.is_empty() {
        crate::klog_warn!("iommu: the IVRS table names no unit");
        return;
    }
    if let Some(bad) = ivrs.units.iter().find(|u| u.segment != 0) {
        crate::klog_warn!(
            "iommu: unit at {:#x} is on PCI segment {}, which this kernel does not reach; IOMMU left off",
            bad.base,
            bad.segment
        );
        return;
    }
    let mut units = Vec::new();
    for (i, ivhd) in ivrs.units.iter().enumerate() {
        match bring_up(i, ivhd.base, ivhd.ioapic) {
            Ok(unit) => {
                crate::klog_info!(
                    "iommu: amd-vi unit {i} at {:#x}: 4 levels, 48 address bits, {} device ranges{}",
                    ivhd.base,
                    ivhd.devices.len(),
                    if ivhd.ioapic {
                        ", interrupts remapped per claimed device"
                    } else {
                        ", no interrupt remapping (the IVRS names no IOAPIC)"
                    }
                );
                units.push(Arc::new(unit));
            }
            Err(why) => {
                crate::klog_warn!(
                    "iommu: amd-vi unit {i} at {:#x}: {why}; IOMMU left off",
                    ivhd.base
                );
                for unit in &units {
                    unit.w64(reg::CONTROL, 0);
                }
                return;
            }
        }
    }
    crate::common::iommu::install(Arc::new(AmdVi { ivrs, units }));
}
