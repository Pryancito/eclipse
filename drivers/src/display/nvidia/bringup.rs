//! The Turing copy-engine bring-up plan (`GpuBringup`): the coherent
//! sysmem structures and the VRAM-resident ones the host walks by
//! physical address. A child of `nvidia` with no logic change: the
//! parent is at its size ceiling (`xtask tamano`); `pub(super)` keeps
//! the struct, its fields and its methods visible to the parent.

use super::*;

/// Coherent-sysmem structures for the Turing copy-engine bring-up (the verified
/// memory plan). All allocated via `DmaRegion::alloc_coherent` (page-aligned,
/// zeroed, UC). Built and dumped read-only at `/proc/gpudbg` for hand
/// verification BEFORE any GPU state is changed. The four buffers the *engine*
/// dereferences by VA (src/dst/sem/pushbuffer) are packed into a single 2 MiB
/// GMMU region so one SPT leaf and one PD0 entry cover everything.
#[allow(dead_code)] // inst/userd/gpfifo are wired up in later bring-up steps
pub(super) struct GpuBringup {
    // 5-level page-directory chain (sysmem-coherent, one 4 KiB page each).
    pub(super) root: DmaRegion, // desc_12[4], PGD 2-bit, the PDB given to the GPU
    pub(super) pd3: DmaRegion,  // desc_12[3], PGD 9-bit
    pub(super) pd2: DmaRegion,  // desc_12[2], PGD 9-bit
    pub(super) pd0: DmaRegion,  // desc_12[1], dual-PDE 8-bit
    pub(super) spt: DmaRegion,  // desc_12[0], SPT leaf, 512×8 B PTEs
    // Sysmem structures the engine reaches through the GMMU (by GPU VA), so
    // they stay in coherent sysmem and are mapped into the channel page tables.
    pub(super) gpfifo: DmaRegion,
    pub(super) pushbuf: DmaRegion,
    pub(super) sem: DmaRegion,
    pub(super) src: DmaRegion,
    pub(super) dst: DmaRegion,
    /// Copy-engine fault-method buffer (sysmem). Only dereferenced by the CE
    /// engine on a faulting method; a red herring for channel load, kept mapped
    /// at va_base+0x5000 but its instance-block pointer is left disarmed.
    pub(super) ce_fault: DmaRegion,
    /// HUB MMU non-replayable fault buffer (sysmem). On Volta+ the host requires
    /// a fault buffer armed (NV_VIRTUAL_FUNCTION_PRIV_MMU_FAULT_BUFFER, 0xb83000)
    /// before any channel can run — nouveau arms it in the `fault` subdev before
    /// the FIFO. We arm it in PHYSICAL/SYS_COH mode so no BAR2 mapping is needed.
    pub(super) fault_buf: DmaRegion,
    /// Base GPU virtual address of the packed 2 MiB region.
    pub(super) va_base: u64,
    /// Base VRAM offset (0-based into VRAM) for the structures the host reads by
    /// raw physical address — instance block, runlist, USERD. Turing's host
    /// scheduler walks these as VRAM-physical (the 0x002b00 runlist path has no
    /// target field), so they cannot live in sysmem. They are CPU-written via
    /// the PRAMIN window. Layout: inst=+0, runlist=+0x1000, userd=+0x2000.
    pub(super) vram_base: u64,
}

impl GpuBringup {
    #[inline]
    pub(super) fn inst_vram(&self) -> u64 {
        self.vram_base
    }
    #[inline]
    pub(super) fn runlist_vram(&self) -> u64 {
        self.vram_base + 0x1000
    }
    #[inline]
    pub(super) fn userd_vram(&self) -> u64 {
        self.vram_base + 0x2000
    }
    /// BAR2 instance block VRAM offset (shares the channel's page tables).
    #[inline]
    pub(super) fn bar2_inst_vram(&self) -> u64 {
        self.vram_base + 0x3000
    }
    #[inline]
    pub(super) fn gpfifo_va(&self) -> u64 {
        self.va_base + 0x4000
    }
    /// GPU/BAR2 VA of the CE fault-method buffer. Used once we arm the real CE
    /// engine context (after HOST/GP_GET is brought up).
    #[allow(dead_code)]
    #[inline]
    pub(super) fn ce_fault_va(&self) -> u64 {
        self.va_base + 0x5000
    }
}

impl GpuBringup {
    /// Allocate the memory plan and build the GMMU page tables in RAM. No GPU
    /// register is touched here — only sysmem is written, so this is safe to run
    /// on demand. Returns `None` if the coherent DMA allocator is exhausted.
    pub(super) fn build(va_base: u64, vram_base: u64) -> Option<Self> {
        let root = DmaRegion::alloc_coherent(0x1000)?;
        let pd3 = DmaRegion::alloc_coherent(0x1000)?;
        let pd2 = DmaRegion::alloc_coherent(0x1000)?;
        let pd0 = DmaRegion::alloc_coherent(0x1000)?;
        let spt = DmaRegion::alloc_coherent(0x1000)?;
        let gpfifo = DmaRegion::alloc_coherent(0x1000)?;
        let pushbuf = DmaRegion::alloc_coherent(0x1000)?;
        let sem = DmaRegion::alloc_coherent(0x1000)?;
        let src = DmaRegion::alloc_coherent(0x1000)?;
        let dst = DmaRegion::alloc_coherent(0x1000)?;
        // CE fault-method buffer: 8 pages (32 KiB) covers the nouveau size
        // formula for any realistic PCE count.
        let ce_fault = DmaRegion::alloc_coherent(0x8000)?;
        // HUB MMU fault buffer: 256 KiB (8192 × 32 B entries) — generous.
        let fault_buf = DmaRegion::alloc_coherent(0x4_0000)?;

        // Pack the engine-visible buffers into one 2 MiB region:
        //  src=+0x0 dst=+0x1000 sem=+0x2000 pushbuffer=+0x3000 gpfifo=+0x4000
        //  ce_fault=+0x5000 (8 pages). The GPFIFO ring and CE fault buffer are
        // referenced by GPU/BAR2 VA, so they are GMMU-mapped like the pushbuffer.
        let src_va = va_base;
        let dst_va = va_base + 0x1000;
        let sem_va = va_base + 0x2000;
        let pb_va = va_base + 0x3000;
        let gpfifo_va = va_base + 0x4000;
        let ce_fault_va = va_base + 0x5000;

        // Leaf PTEs (SPT). idx = (va>>12)&0x1ff.
        let wr64 = |r: &DmaRegion, i: usize, v: u64| unsafe {
            core::ptr::write_volatile(r.as_ptr::<u64>().add(i), v)
        };
        wr64(
            &spt,
            ((src_va >> 12) & 0x1ff) as usize,
            gmmu::encode_pte_sys(src.paddr() as u64),
        );
        wr64(
            &spt,
            ((dst_va >> 12) & 0x1ff) as usize,
            gmmu::encode_pte_sys(dst.paddr() as u64),
        );
        wr64(
            &spt,
            ((sem_va >> 12) & 0x1ff) as usize,
            gmmu::encode_pte_sys(sem.paddr() as u64),
        );
        wr64(
            &spt,
            ((pb_va >> 12) & 0x1ff) as usize,
            gmmu::encode_pte_sys(pushbuf.paddr() as u64),
        );
        wr64(
            &spt,
            ((gpfifo_va >> 12) & 0x1ff) as usize,
            gmmu::encode_pte_sys(gpfifo.paddr() as u64),
        );
        // CE fault buffer: 8 contiguous pages.
        for p in 0..8u64 {
            let va = ce_fault_va + p * 0x1000;
            wr64(
                &spt,
                ((va >> 12) & 0x1ff) as usize,
                gmmu::encode_pte_sys(ce_fault.paddr() as u64 + p * 0x1000),
            );
        }

        // PD0 dual-PDE: pdei = (va>>21)&0xff (== 1 for all, same 2 MiB slot).
        // Low qword = BIG (unused, 0); high qword = SMALL = single-PDE form.
        let pdei = ((src_va >> 21) & 0xff) as usize;
        wr64(&pd0, pdei * 2, 0);
        wr64(&pd0, pdei * 2 + 1, gmmu::encode_pde_sys(spt.paddr() as u64));

        // PD2 / PD3 / root: single PDEs; idx == 0 at all three top levels here.
        wr64(
            &pd2,
            ((src_va >> 29) & 0x1ff) as usize,
            gmmu::encode_pde_sys(pd0.paddr() as u64),
        );
        wr64(
            &pd3,
            ((src_va >> 38) & 0x1ff) as usize,
            gmmu::encode_pde_sys(pd2.paddr() as u64),
        );
        wr64(
            &root,
            ((src_va >> 47) & 0x3) as usize,
            gmmu::encode_pde_sys(pd3.paddr() as u64),
        );

        Some(Self {
            root,
            pd3,
            pd2,
            pd0,
            spt,
            gpfifo,
            pushbuf,
            sem,
            src,
            dst,
            ce_fault,
            fault_buf,
            va_base,
            vram_base,
        })
    }

    /// Read-only dump of the allocated physical layout and every encoded
    /// page-table entry, for hand-verification against the spec before the GPU
    /// is ever pointed at these tables.
    pub(super) fn dump(&self) -> String {
        use core::fmt::Write;
        let rd64 =
            |r: &DmaRegion, i: usize| unsafe { core::ptr::read_volatile(r.as_ptr::<u64>().add(i)) };
        let mut s = String::new();
        let _ = writeln!(
            s,
            "[gpudbg]  --- GMMU tables (Step 1, built in RAM; GPU not yet pointed at them) ---"
        );
        let _ = writeln!(
            s,
            "[gpudbg]  PD  phys: root={:#x} pd3={:#x} pd2={:#x} pd0={:#x} spt={:#x}",
            self.root.paddr(),
            self.pd3.paddr(),
            self.pd2.paddr(),
            self.pd0.paddr(),
            self.spt.paddr()
        );
        let _ = writeln!(
            s,
            "[gpudbg]  sysmem phys: gpfifo={:#x} pb={:#x} sem={:#x} src={:#x} dst={:#x}",
            self.gpfifo.paddr(),
            self.pushbuf.paddr(),
            self.sem.paddr(),
            self.src.paddr(),
            self.dst.paddr()
        );
        let _ = writeln!(
            s,
            "[gpudbg]  VRAM off: inst={:#x} runlist={:#x} userd={:#x} (host-read via PRAMIN)",
            self.inst_vram(),
            self.runlist_vram(),
            self.userd_vram()
        );
        let va = self.va_base;
        let ri = ((va >> 47) & 0x3) as usize;
        let d3 = ((va >> 38) & 0x1ff) as usize;
        let d2 = ((va >> 29) & 0x1ff) as usize;
        let pdei = ((va >> 21) & 0xff) as usize;
        let _ = writeln!(
            s,
            "[gpudbg]  VA base={:#x} idx[root={} pd3={} pd2={} pd0={}]",
            va, ri, d3, d2, pdei
        );
        let _ = writeln!(s, "[gpudbg]  root[{}] = {:#018x}", ri, rd64(&self.root, ri));
        let _ = writeln!(s, "[gpudbg]  pd3 [{}] = {:#018x}", d3, rd64(&self.pd3, d3));
        let _ = writeln!(s, "[gpudbg]  pd2 [{}] = {:#018x}", d2, rd64(&self.pd2, d2));
        let _ = writeln!(
            s,
            "[gpudbg]  pd0 [{}] big={:#018x} small={:#018x}",
            pdei,
            rd64(&self.pd0, pdei * 2),
            rd64(&self.pd0, pdei * 2 + 1)
        );
        for (name, off) in [
            ("src", 0u64),
            ("dst", 0x1000),
            ("sem", 0x2000),
            ("pb", 0x3000),
            ("gpfifo", 0x4000),
        ] {
            let v = va + off;
            let si = ((v >> 12) & 0x1ff) as usize;
            let _ = writeln!(
                s,
                "[gpudbg]  spt [{:3}] {} va={:#x} pte={:#018x}",
                si,
                name,
                v,
                rd64(&self.spt, si)
            );
        }
        let _ = writeln!(
            s,
            "[gpudbg]  inst PD-base qword(@0x200) = {:#018x} (root|0xC06, points at sysmem PDs)",
            gmmu::inst_pd_base(self.root.paddr() as u64)
        );
        s
    }

    /// Step 4: write a minimal method stream into the pushbuffer — just
    /// `SET_OBJECT(TURING_DMA_COPY_A=0xC5B5)` on subchannel 4. Returns the dword
    /// count. Header `(mthd>>2)|(subc<<13)|(count<<16)|(INC=1<<29)`; for
    /// mthd 0x0, subc 4, count 1 that is 0x20018000. No GPU register touched.
    pub(super) fn write_setobject_pushbuffer(&self) -> u32 {
        let pb = self.pushbuf.vaddr();
        let w32 =
            |i: usize, v: u32| unsafe { core::ptr::write_volatile((pb as *mut u32).add(i), v) };
        w32(0, 0x2001_8000); // INC subc4 mthd 0x000 (SET_OBJECT) count1
        w32(1, 0x0000_c5b5); // TURING_DMA_COPY_A class
        2
    }

    /// Write a GPFIFO launch entry into ring `slot` pointing at pushbuffer GPU
    /// VA `pb_va` of `n` dwords. entry0 = GET (pb[31:2]); entry1 = GET_HI |
    /// LENGTH<<10. Verified against clc36f.h NVC36F_GP_ENTRY*.
    pub(super) fn write_gpfifo_entry(&self, slot: usize, pb_va: u64, n: u32) {
        let gp = self.gpfifo.vaddr();
        let w32 =
            |i: usize, v: u32| unsafe { core::ptr::write_volatile((gp as *mut u32).add(i), v) };
        let entry0 = (pb_va as u32) & 0xFFFF_FFFC;
        let entry1 = ((pb_va >> 32) as u32 & 0xFF) | (n << 10);
        w32(slot * 2, entry0);
        w32(slot * 2 + 1, entry1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::host_hooks as shim;
    use core::sync::atomic::Ordering;

    fn rd64(r: &DmaRegion, i: usize) -> u64 {
        unsafe { core::ptr::read_volatile(r.as_ptr::<u64>().add(i)) }
    }
    fn rd32(r: &DmaRegion, i: usize) -> u32 {
        unsafe { core::ptr::read_volatile(r.as_ptr::<u32>().add(i)) }
    }

    // 2 MiB: the packed region's slot is pdei 1, and every leaf index
    // starts at 0, so a table written at the wrong level or index reads
    // as a zero where a value is expected.
    const VA: u64 = 0x20_0000;
    const VRAM: u64 = 0x4000_0000;

    /// Twelve coherent regions (ten pages, the 8-page CE fault buffer and
    /// the 64-page HUB fault buffer), the five engine-visible buffers and
    /// the fault-method pages mapped at their slots of the one 2 MiB
    /// region through a five-level chain (leaf, the SMALL half of pd0's
    /// dual PDE, pd2, pd3, root) that the dump prints by the same numbers;
    /// the VRAM-resident structures at fixed page offsets of `vram_base`.
    #[test]
    fn the_plan_chains_five_tables_to_the_packed_region_and_lays_the_vram_side_out() {
        shim::alone_with_the_allocator(|| {
            let plan = GpuBringup::build(VA, VRAM).expect("twelve regions");
            assert_eq!(shim::ALLOC_CALLS.load(Ordering::SeqCst), 12);
            assert_eq!(
                shim::ALLOC_PAGES.load(Ordering::SeqCst),
                10 + 8 + 64,
                "ten pages, 32 KiB of CE fault methods, 256 KiB of HUB faults"
            );
            assert_eq!(shim::MARK_CALLS.load(Ordering::SeqCst), 12, "all coherent");
            let leaf = [
                (0, plan.src.paddr(), "src"),
                (1, plan.dst.paddr(), "dst"),
                (2, plan.sem.paddr(), "sem"),
                (3, plan.pushbuf.paddr(), "pushbuffer"),
                (4, plan.gpfifo.paddr(), "gpfifo"),
            ];
            for (i, pa, what) in leaf {
                assert_eq!(
                    rd64(&plan.spt, i),
                    gmmu::encode_pte_sys(pa as u64),
                    "{what}"
                );
            }
            for p in 0..8usize {
                assert_eq!(
                    rd64(&plan.spt, 5 + p),
                    gmmu::encode_pte_sys(plan.ce_fault.paddr() as u64 + (p as u64) * 0x1000),
                    "CE fault page {p}"
                );
            }
            assert_eq!(rd64(&plan.spt, 13), 0, "nothing past the fault methods");
            assert_eq!(rd64(&plan.spt, 511), 0);
            assert_eq!(rd64(&plan.pd0, 2), 0, "the BIG half of pdei 1 is unused");
            assert_eq!(
                rd64(&plan.pd0, 3),
                gmmu::encode_pde_sys(plan.spt.paddr() as u64),
                "the SMALL half points at the leaf"
            );
            assert_eq!(rd64(&plan.pd0, 0), 0, "pdei 0 is nobody's");
            assert_eq!(rd64(&plan.pd0, 1), 0);
            assert_eq!(
                rd64(&plan.pd2, 0),
                gmmu::encode_pde_sys(plan.pd0.paddr() as u64)
            );
            assert_eq!(
                rd64(&plan.pd3, 0),
                gmmu::encode_pde_sys(plan.pd2.paddr() as u64)
            );
            assert_eq!(
                rd64(&plan.root, 0),
                gmmu::encode_pde_sys(plan.pd3.paddr() as u64)
            );
            assert_eq!(rd64(&plan.root, 1), 0);
            assert_eq!(
                (
                    plan.inst_vram(),
                    plan.runlist_vram(),
                    plan.userd_vram(),
                    plan.bar2_inst_vram()
                ),
                (VRAM, VRAM + 0x1000, VRAM + 0x2000, VRAM + 0x3000)
            );
            assert_eq!(
                (plan.gpfifo_va(), plan.ce_fault_va()),
                (VA + 0x4000, VA + 0x5000)
            );

            let d = plan.dump();
            for line in [
                alloc::format!(
                    "PD  phys: root={:#x} pd3={:#x} pd2={:#x} pd0={:#x} spt={:#x}",
                    plan.root.paddr(),
                    plan.pd3.paddr(),
                    plan.pd2.paddr(),
                    plan.pd0.paddr(),
                    plan.spt.paddr()
                ),
                alloc::format!(
                    "sysmem phys: gpfifo={:#x} pb={:#x} sem={:#x} src={:#x} dst={:#x}",
                    plan.gpfifo.paddr(),
                    plan.pushbuf.paddr(),
                    plan.sem.paddr(),
                    plan.src.paddr(),
                    plan.dst.paddr()
                ),
                alloc::format!(
                    "VRAM off: inst={:#x} runlist={:#x} userd={:#x}",
                    VRAM,
                    VRAM + 0x1000,
                    VRAM + 0x2000
                ),
                alloc::format!("VA base={:#x} idx[root=0 pd3=0 pd2=0 pd0=1]", VA),
                alloc::format!("root[0] = {:#018x}", rd64(&plan.root, 0)),
                alloc::format!("pd3 [0] = {:#018x}", rd64(&plan.pd3, 0)),
                alloc::format!("pd2 [0] = {:#018x}", rd64(&plan.pd2, 0)),
                alloc::format!(
                    "pd0 [1] big={:#018x} small={:#018x}",
                    0u64,
                    rd64(&plan.pd0, 3)
                ),
                alloc::format!(
                    "spt [  3] pb va={:#x} pte={:#018x}",
                    VA + 0x3000,
                    rd64(&plan.spt, 3)
                ),
                alloc::format!(
                    "spt [  4] gpfifo va={:#x} pte={:#018x}",
                    VA + 0x4000,
                    rd64(&plan.spt, 4)
                ),
                alloc::format!(
                    "inst PD-base qword(@0x200) = {:#018x} (root|0xC06",
                    plan.root.paddr() as u64 | 0xC06
                ),
            ] {
                assert!(d.contains(&line), "missing {:?} in:\n{}", line, d);
            }
            assert_eq!(d.lines().count(), 15);
        });
    }

    /// The SET_OBJECT stream is two words (`INC` subchannel 4 method 0,
    /// count 1; the TURING_DMA_COPY_A class) and nothing after them; a
    /// GPFIFO entry is the pushbuffer VA with its two low bits dropped and
    /// the VA's bits above 32 beside the dword count shifted by ten, at
    /// its own slot only.
    #[test]
    fn the_pushbuffer_and_the_gpfifo_entry_are_written_as_the_host_decodes_them() {
        shim::alone_with_the_allocator(|| {
            let plan = GpuBringup::build(VA, VRAM).unwrap();
            assert_eq!(plan.write_setobject_pushbuffer(), 2);
            assert_eq!(
                [
                    rd32(&plan.pushbuf, 0),
                    rd32(&plan.pushbuf, 1),
                    rd32(&plan.pushbuf, 2)
                ],
                [0x2001_8000, 0x0000_c5b5, 0]
            );
            plan.write_gpfifo_entry(3, 0x1_2345_678b, 2);
            assert_eq!(
                [rd32(&plan.gpfifo, 6), rd32(&plan.gpfifo, 7)],
                [0x2345_6788, 0x01 | (2 << 10)],
                "slot 3: GET without its low bits, GET_HI and LENGTH << 10"
            );
            assert_eq!(
                [rd32(&plan.gpfifo, 0), rd32(&plan.gpfifo, 1)],
                [0, 0],
                "slot 0 untouched"
            );
            assert_eq!(
                [rd32(&plan.gpfifo, 4), rd32(&plan.gpfifo, 5)],
                [0, 0],
                "slot 2 untouched"
            );
            plan.write_gpfifo_entry(0, VA + 0x3000, 0x3ff);
            assert_eq!(
                [rd32(&plan.gpfifo, 0), rd32(&plan.gpfifo, 1)],
                [(VA + 0x3000) as u32, 0x3ff << 10]
            );
        });
    }

    /// A region the allocator cannot give is the whole plan refused, and
    /// every region taken before it goes back; the first refusal costs
    /// nothing.
    #[test]
    fn a_plan_the_allocator_cannot_finish_gives_every_page_back() {
        shim::alone_with_the_allocator(|| {
            shim::FAIL_ALLOC_AFTER.store(11, Ordering::SeqCst);
            assert!(
                GpuBringup::build(VA, VRAM).is_none(),
                "the HUB fault buffer"
            );
            assert_eq!(shim::ALLOC_CALLS.load(Ordering::SeqCst), 12);
            assert_eq!(shim::DEALLOC_CALLS.load(Ordering::SeqCst), 11);
            assert_eq!(shim::DEALLOC_PAGES.load(Ordering::SeqCst), 10 + 8);
        });
        shim::alone_with_the_allocator(|| {
            shim::FAIL_ALLOC_AFTER.store(0, Ordering::SeqCst);
            assert!(GpuBringup::build(VA, VRAM).is_none(), "the root table");
            assert_eq!(shim::DEALLOC_CALLS.load(Ordering::SeqCst), 0);
        });
    }
}
