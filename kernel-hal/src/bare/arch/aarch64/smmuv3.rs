//! Arm SMMUv3, the hardware half: bring the unit up with every stream
//! bypassing, and give a claimed PCI function a domain of its own (stage 1,
//! a sparse identity map of what its driver pins). When the domain goes
//! (the driver died), the stream is left aborting, not bypassing: VT-d's
//! and AMD-Vi's rule. The arithmetic is `common/smmuv3.rs`.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};

use lock::Mutex;

use crate::common::iommu::{DmaDomain, IommuUnit, PciRequester};
use crate::common::smmuv3::{self, cmd, cr0, reg, Caps};
use crate::common::vtd::TableMem;
use crate::mem::{phys_to_virt, PhysFrame};
use crate::{HalError, HalResult, MMUFlags, PhysAddr, VirtAddr, PAGE_SIZE};

/// How long a command sync or a CR0 change is given.
const SPINS: usize = 10_000_000;
/// The stream id bits this uses: a PCI requester id's.
const SID_BITS: u32 = 16;
/// Queue sizes, at most one page each: 256 commands, 128 events.
const CMDQ_LOG2: u32 = 8;
const EVTQ_LOG2: u32 = 7;
/// Frames of one second-level stream table: 2^SPLIT entries of 64 bytes.
const L2_FRAMES: usize = (1 << smmuv3::SPLIT) * smmuv3::STE_BYTES / PAGE_SIZE;

pub(super) struct Smmu {
    regs: VirtAddr,
    caps: Caps,
    two_level: bool,
    /// The first-level table (two levels), or the whole linear one.
    strtab: Vec<PhysFrame>,
    /// Second-level tables, by bus (the stream id's top 8 bits).
    l2: Mutex<BTreeMap<u8, Vec<PhysFrame>>>,
    cmdq: PhysFrame,
    cmdq_log2: u32,
    /// The command queue's producer index, and everything that writes it.
    prod: Mutex<u32>,
    evtq: PhysFrame,
    evtq_log2: u32,
    asids: Mutex<Vec<u16>>,
    faults: AtomicU64,
    /// The stream id of a requester id (the bridge's `iommu-map`).
    stream_of: fn(u16) -> Option<u32>,
}

fn barrier() {
    // SAFETY: a barrier has no operands.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) };
}

impl Smmu {
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: `regs` maps both register pages.
        unsafe { read_volatile((self.regs + off) as *const u32) }
    }
    fn w32(&self, off: usize, v: u32) {
        // SAFETY: as `r32`.
        unsafe { write_volatile((self.regs + off) as *mut u32, v) }
    }
    fn w64(&self, off: usize, v: u64) {
        // SAFETY: as `r32`.
        unsafe { write_volatile((self.regs + off) as *mut u64, v) }
    }

    /// Make `len` bytes at `paddr` visible to a unit whose walks do not
    /// snoop the CPU's caches.
    fn publish(&self, paddr: PhysAddr, len: usize) {
        if !self.caps.coherent {
            let mut line = phys_to_virt(paddr) & !63;
            let end = phys_to_virt(paddr) + len;
            while line < end {
                // SAFETY: a line of memory the linear map covers.
                unsafe {
                    core::arch::asm!("dc cvac, {0}", in(reg) line, options(nostack, preserves_flags))
                };
                line += 64;
            }
        }
        barrier();
    }

    fn set_cr0(&self, value: u32) -> bool {
        self.w32(reg::CR0, value);
        (0..SPINS).any(|_| self.r32(reg::CR0ACK) == value)
    }

    /// Queue `cmds` then a sync, and wait for the unit to consume it: when
    /// this returns it has done them all.
    fn run(&self, cmds: &[[u64; 2]]) {
        let mut prod = self.prod.lock();
        let ring = phys_to_virt(self.cmdq.paddr()) as *mut u64;
        let sync = cmd::sync();
        for c in cmds.iter().chain(core::iter::once(&sync)) {
            // Room: the queue is never let fill, as each batch waits.
            let slot = smmuv3::queue_slot(*prod, self.cmdq_log2);
            // SAFETY: an entry of the queue page, through the linear map.
            unsafe {
                write_volatile(ring.add(slot * 2), c[0]);
                write_volatile(ring.add(slot * 2 + 1), c[1]);
            }
            *prod = smmuv3::queue_next(*prod, self.cmdq_log2);
        }
        self.publish(self.cmdq.paddr(), PAGE_SIZE);
        self.w32(reg::CMDQ_PROD, *prod);
        let done = (0..SPINS)
            .any(|_| smmuv3::queue_passed(self.r32(reg::CMDQ_CONS), *prod, self.cmdq_log2));
        if !done {
            crate::klog_err!(
                "iommu: smmuv3: a command never completed (CONS {:#x}, GERROR {:#x})",
                self.r32(reg::CMDQ_CONS),
                self.r32(reg::GERROR)
            );
        }
    }

    /// Where stream `sid`'s entry is, the second-level table made first
    /// (every entry bypassing) when there is none yet.
    fn ste_at(&self, sid: u32) -> Option<PhysAddr> {
        if !self.two_level {
            return Some(self.strtab[0].paddr() + sid as usize * smmuv3::STE_BYTES);
        }
        let bus = (sid >> smmuv3::SPLIT) as u8;
        let index = sid as usize & ((1 << smmuv3::SPLIT) - 1);
        let mut l2 = self.l2.lock();
        if let Some(frames) = l2.get(&bus) {
            return Some(frames[0].paddr() + index * smmuv3::STE_BYTES);
        }
        let frames = PhysFrame::new_contiguous(L2_FRAMES, 12);
        let base = frames.first()?.paddr();
        for i in 0..1 << smmuv3::SPLIT {
            write_ste(base + i * smmuv3::STE_BYTES, smmuv3::ste_bypass());
        }
        self.publish(base, L2_FRAMES * PAGE_SIZE);
        let l1 = phys_to_virt(self.strtab[0].paddr()) as *mut u64;
        // SAFETY: entry `bus` of the first-level table.
        unsafe { write_volatile(l1.add(bus as usize), smmuv3::l1_desc(base)) };
        self.publish(self.strtab[0].paddr() + bus as usize * 8, 8);
        l2.insert(bus, frames);
        drop(l2);
        self.run(&[cmd::cfgi_all()]);
        Some(base + index * smmuv3::STE_BYTES)
    }

    /// Replace stream `sid`'s entry: invalid first, so the unit never reads
    /// half of each, then the rest, then the word with the valid bit.
    fn set_ste(&self, sid: u32, ste: [u64; 8]) -> bool {
        let Some(at) = self.ste_at(sid) else {
            return false;
        };
        let va = phys_to_virt(at) as *mut u64;
        // SAFETY: the entry `ste_at` found, through the linear map.
        unsafe { write_volatile(va, 0) };
        self.publish(at, smmuv3::STE_BYTES);
        self.run(&[cmd::cfgi_ste(sid)]);
        for (i, &w) in ste.iter().enumerate().skip(1) {
            // SAFETY: as above.
            unsafe { write_volatile(va.add(i), w) };
        }
        self.publish(at, smmuv3::STE_BYTES);
        // SAFETY: as above.
        unsafe { write_volatile(va, ste[0]) };
        self.publish(at, smmuv3::STE_BYTES);
        self.run(&[cmd::cfgi_ste(sid)]);
        true
    }

    fn take_asid(&self) -> Option<u16> {
        let max = if self.caps.asid16 { u16::MAX } else { 255 };
        let mut used = self.asids.lock();
        let asid = (1..=max).find(|a| !used.contains(a))?;
        used.push(asid);
        Some(asid)
    }

    fn give_asid(&self, asid: u16) {
        self.asids.lock().retain(|&a| a != asid);
    }
}

fn write_ste(at: PhysAddr, ste: [u64; 8]) {
    let va = phys_to_virt(at) as *mut u64;
    for (i, &w) in ste.iter().enumerate() {
        // SAFETY: an entry of a stream table this unit owns.
        unsafe { write_volatile(va.add(i), w) };
    }
}

struct Domain {
    smmu: Arc<Smmu>,
    sid: u32,
    asid: u16,
    root: PhysAddr,
    _cd: PhysFrame,
    tables: Mutex<Vec<PhysFrame>>,
}

struct Tables<'a> {
    smmu: &'a Smmu,
    frames: &'a mut Vec<PhysFrame>,
}

impl TableMem for Tables<'_> {
    fn alloc(&mut self) -> Option<PhysAddr> {
        let frame = PhysFrame::new_zero()?;
        let paddr = frame.paddr();
        self.smmu.publish(paddr, PAGE_SIZE);
        self.frames.push(frame);
        Some(paddr)
    }
    fn read(&self, table: PhysAddr, index: usize) -> u64 {
        // SAFETY: a table of this domain.
        unsafe { read_volatile((phys_to_virt(table) + index * 8) as *const u64) }
    }
    fn write(&mut self, table: PhysAddr, index: usize, value: u64) {
        // SAFETY: as `read`.
        unsafe { write_volatile((phys_to_virt(table) + index * 8) as *mut u64, value) };
        self.smmu.publish(table + index * 8, 8);
    }
}

impl DmaDomain for Domain {
    fn map(&self, paddr: PhysAddr, len: usize, flags: MMUFlags) -> HalResult {
        let mut perms = 0;
        if flags.contains(MMUFlags::READ) {
            perms |= smmuv3::READ;
        }
        if flags.contains(MMUFlags::WRITE) {
            // Stage 1 has no write-only page.
            perms |= smmuv3::READ | smmuv3::WRITE;
        }
        if perms == 0 {
            return Err(HalError);
        }
        let end = paddr.checked_add(len).ok_or(HalError)?;
        let mut frames = self.tables.lock();
        let mut mem = Tables {
            smmu: &self.smmu,
            frames: &mut frames,
        };
        for page in (paddr..end).step_by(PAGE_SIZE) {
            smmuv3::map_page(&mut mem, self.root, page as u64, page as u64, perms)?;
        }
        // An invalid entry is never cached, so nothing to forget.
        Ok(())
    }

    fn unmap(&self, paddr: PhysAddr, len: usize) {
        let mut frames = self.tables.lock();
        let mut mem = Tables {
            smmu: &self.smmu,
            frames: &mut frames,
        };
        for page in (paddr..paddr + len).step_by(PAGE_SIZE) {
            smmuv3::unmap_page(&mut mem, self.root, page as u64);
        }
        drop(frames);
        self.smmu.run(&[cmd::tlbi_asid(self.asid)]);
    }
}

impl Drop for Domain {
    fn drop(&mut self) {
        // Aborting, not back to bypass, before a single table frame goes:
        // the device must never walk a freed table, and one that had a
        // driver of its own never reaches every byte again.
        self.smmu.set_ste(self.sid, smmuv3::ste_abort());
        self.smmu.run(&[cmd::tlbi_asid(self.asid)]);
        self.smmu.give_asid(self.asid);
        crate::klog_info!(
            "iommu: stream {:#x} blocked until its next driver",
            self.sid
        );
    }
}

impl IommuUnit for Smmu {
    fn name(&self) -> &'static str {
        "smmuv3"
    }

    fn domain_for(&self, r: PciRequester) -> Option<Arc<dyn DmaDomain>> {
        if r.segment != 0 {
            return None;
        }
        let sid = (self.stream_of)(r.rid())?;
        if sid >> SID_BITS != 0 {
            return None;
        }
        let smmu = unit()?;
        let asid = self.take_asid()?;
        let root = PhysFrame::new_zero()?;
        let cd = PhysFrame::new_zero()?;
        let ips = self.caps.oas.min(5);
        let desc = smmuv3::cd(root.paddr(), ips, asid);
        let cd_va = phys_to_virt(cd.paddr()) as *mut u64;
        for (i, &w) in desc.iter().enumerate() {
            // SAFETY: the descriptor's own frame.
            unsafe { write_volatile(cd_va.add(i), w) };
        }
        self.publish(root.paddr(), PAGE_SIZE);
        self.publish(cd.paddr(), smmuv3::CD_BYTES);
        let root_pa = root.paddr();
        if !self.set_ste(sid, smmuv3::ste_s1(cd.paddr())) {
            self.give_asid(asid);
            return None;
        }
        self.run(&[cmd::cfgi_cd(sid), cmd::tlbi_asid(asid)]);
        crate::klog_info!(
            "iommu: {:02x}:{:02x}.{} has a domain of its own (stream {sid:#x}, asid {asid})",
            r.bus,
            r.device,
            r.function
        );
        Some(Arc::new(Domain {
            smmu,
            sid,
            asid,
            root: root_pa,
            _cd: cd,
            tables: Mutex::new(alloc::vec![root]),
        }))
    }

    fn poll_faults(&self) {
        let ring = phys_to_virt(self.evtq.paddr()) as *const u64;
        let prod = self.r32(reg::EVTQ_PROD);
        let mut cons = self.r32(reg::EVTQ_CONS);
        barrier();
        while !smmuv3::queue_passed(cons, prod, self.evtq_log2) {
            let slot = smmuv3::queue_slot(cons, self.evtq_log2);
            // SAFETY: an entry of the event queue.
            let e = core::array::from_fn(|i| unsafe { read_volatile(ring.add(slot * 4 + i)) });
            let ev = smmuv3::decode_event(e);
            let n = self.faults.fetch_add(1, Ordering::Relaxed);
            if n < 64 || n.is_power_of_two() {
                crate::klog_warn!(
                    "iommu: DMA fault #{}: {:02x}:{:02x}.{} {} ({:#x}) at {:#x}",
                    n + 1,
                    ev.stream >> 8,
                    (ev.stream >> 3) & 0x1f,
                    ev.stream & 7,
                    smmuv3::event_name(ev.code),
                    ev.code,
                    ev.address
                );
            }
            cons = smmuv3::queue_next(cons, self.evtq_log2);
        }
        self.w32(reg::EVTQ_CONS, cons);
    }
}

static UNIT: spin::Once<Arc<Smmu>> = spin::Once::new();

fn unit() -> Option<Arc<Smmu>> {
    UNIT.get().cloned()
}

/// Bring up the SMMUv3 whose registers `regs` maps, every stream
/// bypassing, and use it for claimed functions. `stream_of` gives a
/// requester id's stream id. Nothing when the unit cannot be used.
pub(super) fn init(regs: VirtAddr, base: PhysAddr, stream_of: fn(u16) -> Option<u32>) {
    // SAFETY: the caller mapped both register pages at `regs`.
    let idr = |off: usize| unsafe { read_volatile((regs + off) as *const u32) };
    let caps = Caps::read(idr(reg::IDR0), idr(reg::IDR1), idr(reg::IDR5));
    if let Some(why) = caps.unusable() {
        crate::klog_warn!("iommu: smmuv3 at {base:#x}: {why}; IOMMU left off");
        return;
    }
    let sid_bits = caps.sid_bits.min(SID_BITS);
    let two_level = caps.two_level && sid_bits > smmuv3::SPLIT;
    let strtab = if two_level {
        // 2^(sid_bits - SPLIT) descriptors of 8 bytes: one page.
        PhysFrame::new_contiguous(1, 12)
    } else {
        let bytes = (1usize << sid_bits) * smmuv3::STE_BYTES;
        PhysFrame::new_contiguous(bytes.div_ceil(PAGE_SIZE), 12)
    };
    let (Some(cmdq), Some(evtq), false) = (
        PhysFrame::new_zero(),
        PhysFrame::new_zero(),
        strtab.is_empty(),
    ) else {
        crate::klog_warn!("iommu: smmuv3 at {base:#x}: out of memory; IOMMU left off");
        return;
    };
    let smmu = Smmu {
        regs,
        caps,
        two_level,
        strtab,
        l2: Mutex::new(BTreeMap::new()),
        cmdq,
        cmdq_log2: caps.cmdq_log2.min(CMDQ_LOG2),
        prod: Mutex::new(0),
        evtq,
        evtq_log2: caps.evtq_log2.min(EVTQ_LOG2),
        asids: Mutex::new(Vec::new()),
        faults: AtomicU64::new(0),
        stream_of,
    };
    // A linear table: every entry bypassing. A two-level one starts with
    // every descriptor invalid, and `ste_at` fills a bus in when it is
    // first asked about it; buses that hold a function now are filled
    // before the unit goes on, below.
    if !two_level {
        let base = smmu.strtab[0].paddr();
        for i in 0..1usize << sid_bits {
            write_ste(base + i * smmuv3::STE_BYTES, smmuv3::ste_bypass());
        }
    }
    smmu.publish(smmu.strtab[0].paddr(), smmu.strtab.len() * PAGE_SIZE);
    // Off first, whatever the firmware left.
    if !smmu.set_cr0(0) {
        crate::klog_warn!("iommu: smmuv3 at {base:#x}: does not turn off; IOMMU left off");
        return;
    }
    smmu.w32(reg::IRQ_CTRL, 0);
    smmu.w32(reg::CR1, smmuv3::CR1_WB_ISH);
    smmu.w32(reg::CR2, smmuv3::CR2_RECINVSID);
    smmu.w64(
        reg::STRTAB_BASE,
        smmuv3::strtab_base(smmu.strtab[0].paddr()),
    );
    smmu.w32(
        reg::STRTAB_BASE_CFG,
        smmuv3::strtab_cfg(two_level, sid_bits),
    );
    smmu.w64(
        reg::CMDQ_BASE,
        smmuv3::queue_base(smmu.cmdq.paddr(), smmu.cmdq_log2),
    );
    smmu.w32(reg::CMDQ_PROD, 0);
    smmu.w32(reg::CMDQ_CONS, 0);
    smmu.w64(
        reg::EVTQ_BASE,
        smmuv3::queue_base(smmu.evtq.paddr(), smmu.evtq_log2),
    );
    smmu.w32(reg::EVTQ_PROD, 0);
    smmu.w32(reg::EVTQ_CONS, 0);
    if !smmu.set_cr0(cr0::CMDQEN | cr0::EVTQEN) {
        crate::klog_warn!("iommu: smmuv3 at {base:#x}: queues do not turn on; IOMMU left off");
        return;
    }
    smmu.run(&[cmd::cfgi_all(), cmd::tlbi_nsnh_all()]);
    // Every bus that holds a function: its streams bypass from the start.
    for bus in zcore_drivers::user::pci_buses() {
        for dev in 0..32u32 {
            for func in 0..8u32 {
                let rid = (bus as u16) << 8 | (dev << 3 | func) as u16;
                if let Some(sid) = stream_of(rid) {
                    smmu.ste_at(sid);
                }
            }
        }
    }
    if !smmu.set_cr0(cr0::CMDQEN | cr0::EVTQEN | cr0::SMMUEN) {
        crate::klog_warn!("iommu: smmuv3 at {base:#x}: does not turn on; IOMMU left off");
        let _ = smmu.set_cr0(0);
        return;
    }
    crate::klog_info!(
        "iommu: smmuv3 at {base:#x}: {} stream id bits, {} table, {} walks; translating claimed devices",
        sid_bits,
        if two_level { "two-level" } else { "linear" },
        if caps.coherent { "coherent" } else { "non-coherent" }
    );
    let smmu = UNIT.call_once(|| Arc::new(smmu)).clone();
    crate::common::iommu::install(smmu);
}
