//! Intel VT-d, the hardware half: find the units the DMAR table names, put
//! every device in pass-through, turn translation on, and give a device a
//! domain of its own when a process claims it. The arithmetic (the table,
//! the entries, the page walk) is `common/vtd.rs`.
//!
//! On by default on x86_64: `iommu=off` on the command line leaves it off,
//! and every device with full DMA, as before. A unit that covers only a
//! graphics device is left off (see [`graphics_only`]). Any other
//! unit this cannot drive (no pass-through, no table depth it walks) leaves
//! the whole IOMMU off, as before: a half-enabled IOMMU is a machine where
//! some claimed devices are isolated and some are not, without saying so.

use alloc::{sync::Arc, vec::Vec};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use acpi::sdt::Signature;
use lock::Mutex;
use x86_64::instructions::port::Port;

use crate::common::iommu::{DmaDomain, IommuUnit, PciRequester};
use crate::common::vtd::{self, gcmd, reg, Caps, Dmar, TableMem};
use crate::mem::{phys_to_virt, PhysFrame};
use crate::{HalError, HalResult, MMUFlags, PhysAddr, VirtAddr, KCONFIG, PAGE_SIZE};

/// Domain id of the pass-through context entries. 0 is avoided: with
/// caching mode it is reserved.
const PASS_THROUGH_DID: u16 = 1;

/// How long a command is given to complete before the unit is called dead.
const SPINS: usize = 10_000_000;

/// One byte of PCI configuration space, through the legacy ports, for the
/// bridges a DMAR device scope walks through.
pub(super) fn cfg_read(bus: u8, dev: u8, func: u8, off: u16) -> u8 {
    let addr = 0x8000_0000u32
        | (bus as u32) << 16
        | ((dev as u32) & 0x1f) << 11
        | ((func as u32) & 7) << 8
        | (off as u32 & 0xfc);
    // SAFETY: 0xcf8/0xcfc are the PC's configuration mechanism #1; a read
    // of a function that is not there answers all ones.
    let word = unsafe {
        Port::<u32>::new(0xcf8).write(addr);
        Port::<u32>::new(0xcfc).read()
    };
    (word >> ((off & 3) * 8)) as u8
}

/// `size` bytes of a unit's registers, mapped uncached: the pages that are
/// not in the kernel's map already.
pub(super) fn map_registers(paddr: PhysAddr, size: usize) -> Option<VirtAddr> {
    use crate::vm::{GenericPageTable, PageTable};
    use crate::CachePolicy;
    let flags = MMUFlags::READ
        | MMUFlags::WRITE
        | MMUFlags::DEVICE
        | MMUFlags::from_bits_truncate(CachePolicy::UncachedDevice as usize);
    let mut pt = PageTable::from_current();
    let mut ok = true;
    for page in (paddr..paddr + size).step_by(PAGE_SIZE) {
        let vaddr = phys_to_virt(page);
        match pt.query(vaddr) {
            Ok((mapped, _, _)) if mapped == page => {}
            _ => ok &= pt.map_cont(vaddr, PAGE_SIZE, page, flags).is_ok(),
        }
    }
    core::mem::forget(pt);
    ok.then(|| phys_to_virt(paddr))
}

/// Write back the cache lines of `len` bytes at `paddr`, for a unit whose
/// page walks do not snoop.
pub(super) fn flush_lines(paddr: PhysAddr, len: usize) {
    let va = phys_to_virt(paddr);
    let mut line = va & !63;
    while line < va + len {
        // SAFETY: the linear map covers every frame this module allocated.
        unsafe { core::arch::x86_64::_mm_clflush(line as *const u8) };
        line += 64;
    }
    // SAFETY: a fence has no operands.
    unsafe { core::arch::x86_64::_mm_mfence() };
}

/// A remapping unit, as programmed.
struct Unit {
    index: usize,
    regs: VirtAddr,
    caps: Caps,
    levels: usize,
    aw: u64,
    /// The widest device address a domain here takes: the narrower of the
    /// table depth and what the unit says it walks.
    addr_bits: u32,
    /// The root table and one context table per bus, for the unit's life.
    root: PhysFrame,
    contexts: Vec<PhysFrame>,
    /// Register commands are one at a time.
    commands: Mutex<()>,
    /// Domain ids in use (beyond the pass-through one).
    dids: Mutex<Vec<u16>>,
    faults: AtomicU64,
    /// The invalidation queue, once turned on: from then on every
    /// invalidation goes through it (the registers may not be used).
    queue: Option<Queue>,
}

/// An invalidation queue: 256 descriptors, and the word wait descriptors
/// store to.
struct Queue {
    ring: PhysFrame,
    status: PhysFrame,
    /// The next free slot, and everything that writes the ring.
    tail: Mutex<usize>,
    stamp: AtomicU32,
}

impl Unit {
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: `regs` maps the unit's whole register page.
        unsafe { read_volatile((self.regs + off) as *const u32) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as `r32`.
        unsafe { write_volatile((self.regs + off) as *mut u32, v) }
    }
    fn r64(&self, off: usize) -> u64 {
        // SAFETY: as `r32`.
        unsafe { read_volatile((self.regs + off) as *const u64) }
    }
    fn w64(&self, off: usize, v: u64) {
        // SAFETY: as `r32`.
        unsafe { write_volatile((self.regs + off) as *mut u64, v) }
    }

    /// Issue a GCMD command (or clear a persistent one) and wait for GSTS to
    /// show it done: the bit set for a state turned on or a one-shot that
    /// reports completion by setting it, clear for one turned off or a
    /// write-buffer flush.
    fn gcmd(&self, bit: u32, on: bool) -> bool {
        let gsts = self.r32(reg::GSTS);
        self.w32(reg::GCMD, vtd::gcmd_value(gsts, bit, on));
        let want_set = on && bit != gcmd::WBF;
        (0..SPINS).any(|_| (self.r32(reg::GSTS) & bit != 0) == want_set)
    }

    fn write_buffer_flush(&self) {
        if self.caps.rwbf() && !self.gcmd(gcmd::WBF, true) {
            crate::klog_err!(
                "iommu: vt-d unit {}: write buffer flush never completed",
                self.index
            );
        }
    }

    /// Run `descs` through the invalidation queue, followed by a wait, and
    /// return once the unit has done them all.
    fn submit(&self, descs: &[[u64; 2]]) {
        let Some(q) = &self.queue else {
            return;
        };
        let mut tail = q.tail.lock();
        let stamp = q.stamp.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let ring = phys_to_virt(q.ring.paddr()) as *mut u64;
        let wait = vtd::qi::wait(q.status.paddr(), stamp);
        for d in descs.iter().chain(core::iter::once(&wait)) {
            // SAFETY: a slot of the queue's page, 16-byte descriptors.
            unsafe {
                write_volatile(ring.add(*tail * 2), d[0]);
                write_volatile(ring.add(*tail * 2 + 1), d[1]);
            }
            *tail = (*tail + 1) % vtd::qi::ENTRIES;
        }
        if !self.caps.coherent() {
            flush_lines(q.ring.paddr(), PAGE_SIZE);
        }
        self.w64(reg::IQT, vtd::qi::tail(*tail));
        let word = phys_to_virt(q.status.paddr()) as *const u32;
        // SAFETY: the status frame, which the unit writes.
        if !(0..SPINS).any(|_| unsafe { read_volatile(word) } == stamp) {
            crate::klog_err!(
                "iommu: vt-d unit {}: an invalidation never completed (fault status {:#x})",
                self.index,
                self.r32(reg::FSTS)
            );
        }
    }

    /// Invalidate the context cache with `cmd`, and wait for it.
    fn flush_context(&self, cmd: u64) {
        if self.queue.is_some() {
            return self.submit(&[vtd::qi::context_global()]);
        }
        self.w64(reg::CCMD, cmd);
        if !(0..SPINS).any(|_| self.r64(reg::CCMD) & (1 << 63) == 0) {
            crate::klog_err!(
                "iommu: vt-d unit {}: context invalidation never completed",
                self.index
            );
        }
    }

    /// Invalidate the IOTLB with `cmd`, and wait for it: when this returns
    /// no translation it dropped can be used any more.
    fn flush_iotlb(&self, cmd: u64) {
        if self.queue.is_some() {
            let desc = if cmd == vtd::IOTLB_GLOBAL {
                vtd::qi::iotlb_global()
            } else {
                vtd::qi::iotlb_domain((cmd >> 32) as u16)
            };
            return self.submit(&[desc]);
        }
        let at = self.caps.iotlb_offset() + 8;
        self.w64(at, cmd);
        if !(0..SPINS).any(|_| self.r64(at) & (1 << 63) == 0) {
            crate::klog_err!(
                "iommu: vt-d unit {}: IOTLB invalidation never completed",
                self.index
            );
        }
    }

    fn table_write(&self, table: PhysAddr, index: usize, words: &[u64]) {
        let at = phys_to_virt(table) + index * 8 * words.len();
        for (i, &w) in words.iter().enumerate().rev() {
            // SAFETY: a frame this unit owns, through the linear map. The
            // low word (present bit) goes last.
            unsafe { write_volatile((at + i * 8) as *mut u64, w) };
        }
        if !self.caps.coherent() {
            flush_lines(table + index * 8 * words.len(), 8 * words.len());
        }
    }

    /// Point the context entry of `rid` at `entry`, taking it out first so
    /// the unit never sees half of the old one and half of the new.
    fn set_context(&self, rid: u16, entry: [u64; 2], old_did: u16) {
        let _cmd = self.commands.lock();
        let table = self.contexts[(rid >> 8) as usize].paddr();
        let slot = (rid & 0xff) as usize;
        self.table_write(table, slot, &[0, 0]);
        self.write_buffer_flush();
        self.flush_context(vtd::CCMD_GLOBAL);
        self.flush_iotlb(vtd::iotlb_domain(old_did));
        self.table_write(table, slot, &entry);
        self.write_buffer_flush();
        self.flush_context(vtd::CCMD_GLOBAL);
    }

    fn pass_through_entry(&self) -> [u64; 2] {
        vtd::context_entry(vtd::TT_PASS_THROUGH, 0, self.aw, PASS_THROUGH_DID)
    }

    fn take_did(&self) -> Option<u16> {
        let mut used = self.dids.lock();
        let did = (PASS_THROUGH_DID + 1..self.caps.domains().min(u16::MAX as usize) as u16)
            .find(|d| !used.contains(d))?;
        used.push(did);
        Some(did)
    }

    fn give_did(&self, did: u16) {
        self.dids.lock().retain(|&d| d != did);
    }

    /// Log and clear what the unit recorded.
    fn poll_faults(&self) {
        let fsts = self.r32(reg::FSTS);
        if fsts & 0b11 == 0 {
            return;
        }
        for i in 0..self.caps.fault_count() {
            let at = self.caps.fault_offset() + 16 * i;
            let (lo, hi) = (self.r64(at), self.r64(at + 8));
            let Some(fault) = vtd::decode_fault(lo, hi) else {
                continue;
            };
            let n = self.faults.fetch_add(1, Ordering::Relaxed);
            if (n < 64 || n.is_power_of_two()) && fault.reason >= 0x20 {
                crate::klog_warn!(
                    "iommu: interrupt fault #{}: {:02x}:{:02x}.{} handle {:#x}, reason {:#x} (an interrupt it may not raise)",
                    n + 1,
                    fault.source >> 8,
                    (fault.source >> 3) & 0x1f,
                    fault.source & 7,
                    fault.address >> 48,
                    fault.reason,
                );
            } else if n < 64 || n.is_power_of_two() {
                crate::klog_warn!(
                    "iommu: DMA fault #{}: {:02x}:{:02x}.{} {} {:#x}, reason {:#x} (not in its domain)",
                    n + 1,
                    fault.source >> 8,
                    (fault.source >> 3) & 0x1f,
                    fault.source & 7,
                    if fault.read { "read" } else { "write" },
                    fault.address,
                    fault.reason,
                );
            }
            // The fault bit is write-one-to-clear, in the top dword.
            self.w32(at + 12, 1 << 31);
        }
        // Overflow, write-one-to-clear.
        self.w32(reg::FSTS, fsts & 1);
    }
}

/// A claimed device's domain on one unit.
struct Domain {
    unit: Arc<Unit>,
    rid: u16,
    did: u16,
    root: PhysAddr,
    /// Every table frame, the root first: freed with the domain.
    tables: Mutex<Vec<PhysFrame>>,
}

/// The second-level tables of one domain, as `common::vtd` walks them.
struct Tables<'a> {
    unit: &'a Unit,
    frames: &'a mut Vec<PhysFrame>,
}

impl TableMem for Tables<'_> {
    fn alloc(&mut self) -> Option<PhysAddr> {
        let frame = PhysFrame::new_zero()?;
        let paddr = frame.paddr();
        if !self.unit.caps.coherent() {
            flush_lines(paddr, PAGE_SIZE);
        }
        self.frames.push(frame);
        Some(paddr)
    }
    fn read(&self, table: PhysAddr, index: usize) -> u64 {
        // SAFETY: a table of this domain, through the linear map.
        unsafe { read_volatile((phys_to_virt(table) + index * 8) as *const u64) }
    }
    fn write(&mut self, table: PhysAddr, index: usize, value: u64) {
        self.unit.table_write(table, index, &[value]);
    }
}

impl Domain {
    fn map_pages(&self, paddr: PhysAddr, len: usize, perms: u64) -> HalResult {
        let end = paddr.checked_add(len).ok_or(HalError)?;
        if end as u64 > 1u64 << self.unit.addr_bits {
            crate::klog_warn!(
                "iommu: {:#x}..{:#x} is past the {} bits a vt-d domain reaches",
                paddr,
                end,
                self.unit.addr_bits
            );
            return Err(HalError);
        }
        let mut frames = self.tables.lock();
        let mut mem = Tables {
            unit: &self.unit,
            frames: &mut frames,
        };
        for page in (paddr..end).step_by(PAGE_SIZE) {
            vtd::map_page(
                &mut mem,
                self.root,
                self.unit.levels,
                page as u64,
                page as u64,
                perms,
            )
            .map_err(|_| HalError)?;
        }
        drop(frames);
        let _cmd = self.unit.commands.lock();
        self.unit.write_buffer_flush();
        if self.unit.caps.caching_mode() {
            self.unit.flush_iotlb(vtd::iotlb_domain(self.did));
        }
        Ok(())
    }
}

impl DmaDomain for Domain {
    fn map(&self, paddr: PhysAddr, len: usize, flags: MMUFlags) -> HalResult {
        let mut perms = 0;
        if flags.contains(MMUFlags::READ) {
            perms |= vtd::SL_READ;
        }
        if flags.contains(MMUFlags::WRITE) {
            perms |= vtd::SL_WRITE;
        }
        if perms == 0 {
            return Err(HalError);
        }
        self.map_pages(paddr, len, perms)
    }

    fn unmap(&self, paddr: PhysAddr, len: usize) {
        let mut frames = self.tables.lock();
        let mut mem = Tables {
            unit: &self.unit,
            frames: &mut frames,
        };
        for page in (paddr..paddr + len).step_by(PAGE_SIZE) {
            vtd::unmap_page(&mut mem, self.root, self.unit.levels, page as u64);
        }
        drop(frames);
        let _cmd = self.unit.commands.lock();
        self.unit.write_buffer_flush();
        self.unit.flush_iotlb(vtd::iotlb_domain(self.did));
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // Blocked (no context entry: every request faults), not back to
        // pass-through, before a single table frame goes: the device must
        // never walk a freed table, and one that had a driver of its own
        // never reaches every byte again. Its next driver gets a new
        // domain.
        self.unit.set_context(self.rid, [0, 0], self.did);
        self.unit.give_did(self.did);
        crate::klog_info!(
            "iommu: {:02x}:{:02x}.{} blocked until its next driver",
            self.rid >> 8,
            (self.rid >> 3) & 0x1f,
            self.rid & 7
        );
    }
}

/// Every unit of the machine.
struct VtD {
    dmar: Dmar,
    /// One per DMAR unit, in its order; `None` for a unit left off.
    units: Vec<Option<Arc<Unit>>>,
}

impl IommuUnit for VtD {
    fn name(&self) -> &'static str {
        "vt-d"
    }

    fn domain_for(&self, r: PciRequester) -> Option<Arc<dyn DmaDomain>> {
        let index = self
            .dmar
            .unit_for(r.segment, r.bus, r.device, r.function, cfg_read)?;
        let unit = self.units[index].clone()?;
        let did = unit.take_did()?;
        let Some(root) = PhysFrame::new_zero() else {
            unit.give_did(did);
            return None;
        };
        let root_pa = root.paddr();
        if !unit.caps.coherent() {
            flush_lines(root_pa, PAGE_SIZE);
        }
        let domain = Domain {
            unit: unit.clone(),
            rid: r.rid(),
            did,
            root: root_pa,
            tables: Mutex::new(alloc::vec![root]),
        };
        // What the firmware keeps doing DMA to for this device.
        for (start, len) in self
            .dmar
            .reserved_for(r.segment, r.bus, r.device, r.function, cfg_read)
        {
            if domain
                .map_pages(start, len, vtd::SL_READ | vtd::SL_WRITE)
                .is_err()
            {
                crate::klog_warn!("iommu: could not map reserved region {start:#x}+{len:#x}; device stays in pass-through");
                unit.give_did(did);
                core::mem::forget(domain);
                return None;
            }
            crate::klog_info!(
                "iommu: reserved region {start:#x}+{len:#x} kept for {:04x}",
                r.rid()
            );
        }
        // Its ways around the domain closed before the domain is on.
        super::pcie::isolate(r);
        unit.set_context(
            r.rid(),
            vtd::context_entry(vtd::TT_TRANSLATE, root_pa, unit.aw, did),
            PASS_THROUGH_DID,
        );
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
        for unit in self.units.iter().flatten() {
            unit.poll_faults();
        }
    }
}

/// Take over one unit: turn it off if the firmware left it on, program
/// every context in pass-through, and turn translation on.
fn bring_up(index: usize, base: u64) -> Result<Unit, &'static str> {
    let regs =
        map_registers(base as PhysAddr, PAGE_SIZE).ok_or("its registers could not be mapped")?;
    // SAFETY: just mapped.
    let caps = unsafe {
        Caps {
            cap: read_volatile((regs + reg::CAP) as *const u64),
            ecap: read_volatile((regs + reg::ECAP) as *const u64),
        }
    };
    if !caps.pass_through() {
        return Err("it has no pass-through contexts");
    }
    let (levels, aw, bits) = caps
        .depth()
        .ok_or("it walks no table depth this kernel builds")?;
    let root = PhysFrame::new_zero().ok_or("no frame for its root table")?;
    let mut contexts = Vec::with_capacity(256);
    for _ in 0..256 {
        contexts.push(PhysFrame::new_zero().ok_or("no frame for a context table")?);
    }
    let unit = Unit {
        index,
        regs,
        caps,
        levels,
        aw,
        addr_bits: bits.min(caps.mgaw()),
        root,
        contexts,
        commands: Mutex::new(()),
        dids: Mutex::new(Vec::new()),
        faults: AtomicU64::new(0),
        queue: None,
    };
    let gsts = unit.r32(reg::GSTS);
    if gsts & gcmd::TE != 0 {
        crate::klog_warn!(
            "iommu: vt-d unit {index}: the firmware left translation on; turning it off first"
        );
        if !unit.gcmd(gcmd::TE, false) {
            return Err("translation would not turn off");
        }
    }
    if gsts & gcmd::QIE != 0 && !unit.gcmd(gcmd::QIE, false) {
        return Err("queued invalidation would not turn off");
    }
    if gsts & gcmd::IRE != 0 {
        crate::klog_warn!(
            "iommu: vt-d unit {index}: interrupt remapping was left on by the firmware"
        );
    }
    // Every function of every bus in pass-through.
    let pt = unit.pass_through_entry();
    for (bus, table) in unit.contexts.iter().enumerate() {
        let va = phys_to_virt(table.paddr()) as *mut u64;
        for slot in 0..256 {
            // SAFETY: a fresh frame of 256 16-byte entries.
            unsafe {
                write_volatile(va.add(slot * 2 + 1), pt[1]);
                write_volatile(va.add(slot * 2), pt[0]);
            }
        }
        let root_va = phys_to_virt(unit.root.paddr()) as *mut u64;
        // SAFETY: the root table, 256 16-byte entries.
        unsafe { write_volatile(root_va.add(bus * 2), vtd::root_entry(table.paddr())) };
        if !caps.coherent() {
            flush_lines(table.paddr(), PAGE_SIZE);
        }
    }
    if !caps.coherent() {
        flush_lines(unit.root.paddr(), PAGE_SIZE);
    }
    unit.w64(reg::RTADDR, unit.root.paddr() as u64);
    if !unit.gcmd(gcmd::SRTP, true) {
        return Err("the root table pointer was not taken");
    }
    // The invalidation queue, where there is one: interrupt remapping
    // needs it. Without it, the registers.
    let mut unit = unit;
    if caps.queued_invalidation() {
        match (PhysFrame::new_zero(), PhysFrame::new_zero()) {
            (Some(ring), Some(status)) => {
                unit.w64(reg::IQT, 0);
                unit.w64(reg::IQA, vtd::qi::queue_register(ring.paddr()));
                if unit.gcmd(gcmd::QIE, true) {
                    unit.queue = Some(Queue {
                        ring,
                        status,
                        tail: Mutex::new(0),
                        stamp: AtomicU32::new(0),
                    });
                } else {
                    crate::klog_warn!(
                        "iommu: vt-d unit {index}: queued invalidation would not turn on; registers instead"
                    );
                }
            }
            _ => return Err("no frames for its invalidation queue"),
        }
    }
    unit.write_buffer_flush();
    unit.flush_context(vtd::CCMD_GLOBAL);
    unit.flush_iotlb(vtd::IOTLB_GLOBAL);
    // Faults are polled, not interrupted on.
    unit.w32(reg::FECTL, 1 << 31);
    unit.poll_faults();
    if !unit.gcmd(gcmd::TE, true) {
        return Err("translation would not turn on");
    }
    Ok(unit)
}

/// PCI class of display controllers.
const CLASS_DISPLAY: u8 = 0x03;
/// Configuration offset of the base class byte.
const CLASS_OFFSET: u16 = 0x0b;

/// Whether `drhd` is a unit for an integrated GPU alone: not the catch-all
/// unit, and every device it lists a display controller. Such a unit is
/// left off, as Linux does by default on many chipsets (`igfx_off`): the
/// GPU is the kernel's, never a process's, and turning translation on in
/// front of it, even in pass-through, is where firmware and chipset quirks
/// live. Nothing of what a process drives is behind it.
fn graphics_only(drhd: &vtd::Drhd) -> bool {
    !drhd.include_all
        && !drhd.scopes.is_empty()
        && drhd.scopes.iter().all(|scope| {
            matches!(
                scope.resolve(cfg_read),
                Some(vtd::Target::Function(bus, dev, func))
                    if cfg_read(bus, dev, func, CLASS_OFFSET) == CLASS_DISPLAY
            )
        })
}

/// Find and take over the machine's VT-d units, unless the command line
/// says `iommu=off`.
pub(super) fn init() {
    if crate::cmdline::value(KCONFIG.cmdline, "iommu") == Some("off") {
        return;
    }
    let Some(dmar) = super::drivers::acpi_table(Signature::DMAR).and_then(vtd::parse_dmar) else {
        // Not an Intel machine, or one without VT-d: AMD-Vi looks next.
        return;
    };
    if dmar.units.is_empty() {
        crate::klog_warn!("iommu: the DMAR table names no unit");
        return;
    }
    if let Some(bad) = dmar.units.iter().find(|u| u.segment != 0) {
        crate::klog_warn!("iommu: unit at {:#x} is on PCI segment {}, which this kernel does not reach; IOMMU left off", bad.base, bad.segment);
        return;
    }
    // Check every unit can be driven before turning any on.
    let mut units = Vec::new();
    for (i, drhd) in dmar.units.iter().enumerate() {
        if graphics_only(drhd) {
            crate::klog_info!(
                "iommu: vt-d unit {i} at {:#x} covers only a GPU; left off",
                drhd.base
            );
            units.push(None);
            continue;
        }
        match bring_up(i, drhd.base) {
            Ok(unit) => {
                crate::klog_info!(
                    "iommu: vt-d unit {i} at {:#x}: {} levels, {} address bits, {} domains{}",
                    drhd.base,
                    unit.levels,
                    unit.addr_bits,
                    unit.caps.domains(),
                    if unit.caps.coherent() {
                        ""
                    } else {
                        ", non-coherent walks"
                    }
                );
                units.push(Some(Arc::new(unit)));
            }
            Err(why) => {
                crate::klog_warn!(
                    "iommu: vt-d unit {i} at {:#x}: {why}; IOMMU left off",
                    drhd.base
                );
                for unit in units.iter().flatten() {
                    unit.gcmd(gcmd::TE, false);
                }
                return;
            }
        }
    }
    let remap = remap_interrupts(&dmar, &units);
    crate::common::iommu::install(Arc::new(VtD {
        dmar: dmar.clone(),
        units,
    }));
    if let Some(remap) = remap {
        zcore_drivers::irq::remap::install(remap);
    }
}

/// Turn interrupt remapping on, on every unit, with one table they share,
/// and blocking every interrupt in the compatibility format: from then on
/// only an interrupt the kernel put in the table is delivered, and only
/// from the requester the entry names. `None`, and nothing changed, unless
/// every unit that is on has a queue and remaps (`iommu=noir` says no).
fn remap_interrupts(dmar: &Dmar, units: &[Option<Arc<Unit>>]) -> Option<Arc<Remap>> {
    if crate::cmdline::value(KCONFIG.cmdline, "iommu") == Some("noir") {
        crate::klog_info!("iommu: iommu=noir: interrupts not remapped");
        return None;
    }
    let on: Vec<Arc<Unit>> = units.iter().flatten().cloned().collect();
    if on.is_empty() {
        return None;
    }
    if let Some(u) = on
        .iter()
        .find(|u| u.queue.is_none() || !u.caps.interrupt_remapping())
    {
        crate::klog_info!(
            "iommu: vt-d unit {} does not remap interrupts (or has no queue); none are remapped",
            u.index
        );
        return None;
    }
    let table = PhysFrame::new_zero()?;
    let irta = vtd::ir::table_register(table.paddr());
    let mut done = Vec::new();
    for u in &on {
        u.w64(reg::IRTA, irta);
        let ok = u.gcmd(gcmd::SIRTP, true) && {
            u.submit(&[vtd::qi::iec_global()]);
            u.gcmd(gcmd::CFI, false) && u.gcmd(gcmd::IRE, true)
        };
        if !ok {
            crate::klog_warn!(
                "iommu: vt-d unit {}: interrupt remapping would not turn on; none are remapped",
                u.index
            );
            for d in done.iter().chain(core::iter::once(u)) {
                let d: &Arc<Unit> = d;
                d.gcmd(gcmd::IRE, false);
            }
            return None;
        }
        done.push(u.clone());
    }
    crate::klog_info!("iommu: interrupts remapped; compatibility-format interrupts blocked");
    Some(Arc::new(Remap {
        dmar: dmar.clone(),
        units: units.to_vec(),
        table,
        lock: Mutex::new(()),
    }))
}

/// The shared interrupt remapping table. Each vector is its own handle:
/// the kernel never gives one vector to two sources.
struct Remap {
    dmar: Dmar,
    units: Vec<Option<Arc<Unit>>>,
    table: PhysFrame,
    lock: Mutex<()>,
}

impl Remap {
    fn set(&self, handle: u8, entry: [u64; 2]) {
        let _g = self.lock.lock();
        let at = phys_to_virt(self.table.paddr()) + handle as usize * 16;
        // SAFETY: an entry of the table, through the linear map. The high
        // qword first: the low one holds the present bit.
        unsafe {
            write_volatile((at + 8) as *mut u64, entry[1]);
            write_volatile(at as *mut u64, entry[0]);
        }
        flush_lines(self.table.paddr() + handle as usize * 16, 16);
        for unit in self.units.iter().flatten() {
            unit.submit(&[vtd::qi::iec_global()]);
        }
    }

    fn dest() -> u8 {
        zcore_drivers::irq::x86::Apic::bsp_apic_id()
    }
}

impl zcore_drivers::irq::remap::Remapper for Remap {
    fn msi(&self, rid: u16, vector: u8) -> Option<(u64, u32)> {
        // A device behind a unit left off sends what it is given as it is:
        // the compatibility format.
        let unit = self.dmar.unit_for(
            0,
            (rid >> 8) as u8,
            ((rid >> 3) & 0x1f) as u8,
            (rid & 7) as u8,
            cfg_read,
        )?;
        self.units.get(unit)?.as_ref()?;
        let source = match super::pcie::conventional_bridge_above((rid >> 8) as u8) {
            Some((first, last)) => vtd::ir::Source::Buses(first, last),
            None => vtd::ir::Source::Requester(rid),
        };
        self.set(vector, vtd::ir::entry(vector, Self::dest(), false, source));
        Some((vtd::ir::msi_address(vector as u16), 0))
    }

    fn ioapic(&self, ioapic: u8, vector: u8, level: bool, active_low: bool) -> Option<(u32, u32)> {
        // Only the I/O APIC may raise its lines. One no unit names is let
        // through from anyone rather than left dumb.
        let source = match self.dmar.ioapic_source(ioapic, cfg_read) {
            Some(sid) => vtd::ir::Source::Requester(sid),
            None => vtd::ir::Source::Any,
        };
        self.set(vector, vtd::ir::entry(vector, Self::dest(), level, source));
        Some(vtd::ir::ioapic_entry(
            vector as u16,
            vector,
            level,
            active_low,
        ))
    }
}
