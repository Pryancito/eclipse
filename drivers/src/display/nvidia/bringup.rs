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
