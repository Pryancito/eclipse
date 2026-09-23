use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::bus::pci_drivers::PciDriver;
use crate::prelude::{AccelCaps, ColorFormat, DisplayInfo, FrameBuffer};
use crate::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane, GemHandle};
use crate::scheme::{DisplayScheme, DrmScheme, Scheme};
use crate::utils::dma::DmaRegion;
use crate::{builder::IoMapper, Device, DeviceError, DeviceResult};
use alloc::sync::Arc;
use lock::Mutex;
use pci::{PCIDevice, BAR};

/// Busy-wait heartbeat for the GPU register/fence poll loops in this driver.
///
/// Like the RM's `osSpinLoop` (nvidia-rm-sys/src/os_boundary.rs), this drains
/// **this CPU's TLB-shootdown queue** at a coarse cadence before spinning, so a
/// long GPU wait cannot starve a peer CPU's shootdown ack and wedge the whole
/// machine. Many of these polls run behind a `lock::Mutex` (interrupts off) or
/// otherwise cannot service the shootdown IPI; without pumping, a CPU doing a
/// `munmap`/VM_BIND-unmap then spins forever inside `remote_flush_tlb_aspace`
/// holding the VMAR + page-table locks and every later address-space op convoys
/// behind it — the on-hardware `DEADLOCK`/`KERNEL STOP` (shootdown starvation,
/// "Not AB-BA"). `lock::pump()` is cheap (one relaxed load when the queue is
/// empty) and safe to call under locks; the RM already relies on exactly that.
#[inline]
fn gpu_spin() {
    static SPIN: AtomicUsize = AtomicUsize::new(0);
    let n = SPIN.fetch_add(1, Ordering::Relaxed) + 1;
    if n & 511 == 0 {
        lock::pump();
    }
    core::hint::spin_loop();
}

/// Direct-submit state of one GPU context (see `nouveau_uapi::FastCtx` and
/// `eclipse_rm_exec_fast_prepare`). `Failed` pins the context to the RM
/// per-submit path for its lifetime instead of retrying the RM on every EXEC.
enum FastSlot {
    Unprepared,
    /// A thread is inside `exec_fast_prepare` for this context. Claimed under
    /// the slot lock BEFORE entering the RM, so a second thread of the same
    /// process (NVK is multithreaded; labwc runs two Vulkan instances) waits
    /// for that build instead of running its own and overwriting the first
    /// `FastCtx` -- which restarted `next_payload` at 1 and zeroed the landing
    /// zone under fences already attached with higher payloads, so a stale
    /// fence read as landed when the second stream reached its number, and
    /// leaked the first USERD mapping.
    Preparing,
    Ready(super::nouveau_uapi::FastCtx),
    Failed,
}

/// x86 store fence: make the GP entries / method stream (cached sysmem
/// stores) globally visible before the GPPut and doorbell writes that let
/// the GPU fetch them. Same `osFlushCpuWriteCombineBuffer` the RM path used.
#[inline]
fn store_fence() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("sfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    core::sync::atomic::fence(Ordering::SeqCst);
}

/// `syncobj`'s fence-timeout upcall: a pending hardware fence did not land in
/// 10 s. Route it to the GPU whose context buffer holds the landing zone.
fn nouveau_fence_timeout_hook(
    ctx_idx: u32,
    fence_va: usize,
    payload: u32,
    handle: u32,
    point: u64,
) {
    let gpus: Vec<Arc<NvidiaGpu>> = NVIDIA_GPUS.lock().clone();
    for gpu in gpus {
        gpu.fast_fence_timeout(ctx_idx, fence_va, payload, handle, point);
    }
}

// --- Registers and Constants (aligned with Nova / open-gpu-kernel-modules) ---
#[allow(dead_code)]
mod regs {
    pub const NV_PMC_BOOT_0: u32 = 0x0000_0000;
    pub const PMC_BOOT0_CHIP_ID_SHIFT: u32 = 20;
    pub const PMC_BOOT0_CHIP_ID_MASK: u32 = 0xFFF;

    pub const PMC_BOOT0_CHIPID_TURING_MIN: u32 = 0x160;
    pub const PMC_BOOT0_CHIPID_TURING_MAX: u32 = 0x16F;
    pub const PMC_BOOT0_CHIPID_AMPERE_MIN: u32 = 0x170;
    pub const PMC_BOOT0_CHIPID_AMPERE_MAX: u32 = 0x17F;
    pub const PMC_BOOT0_CHIPID_ADA_MIN: u32 = 0x190;
    pub const PMC_BOOT0_CHIPID_ADA_MAX: u32 = 0x19F;
    // The chip-id field is NINE bits wide (nouveau reads it as
    // `(boot0 & 0x1ff00000) >> 20`, which `nouveau_chipset_id` below does
    // too), so no real part can ever report an id of 0x200 or above. Hopper
    // used to sit at 0x1B0..=0x1BF and Blackwell at `>= 0x200`, which made the
    // Blackwell arm unreachable and handed every consumer Blackwell id to
    // Hopper. The two bounds here are the ones this file's own
    // `nouveau_chipset_id` fallback table already names as representative:
    // 0x180 = GH100 (Hopper) and 0x1b2 = GB202 (Blackwell).
    pub const PMC_BOOT0_CHIPID_HOPPER_MIN: u32 = 0x180;
    pub const PMC_BOOT0_CHIPID_HOPPER_MAX: u32 = 0x18F;
    pub const PMC_BOOT0_CHIPID_BLACKWELL_MIN: u32 = 0x1A0;

    pub const NV_PFB_CSTATUS: u32 = 0x0010_020C;
    pub const NV_PFB_CSTATUS_MEM_SIZE_MASK: u32 = 0x7FFF;

    pub const NV_THERM_TEMP: u32 = 0x0002_0400;
    pub const NV_THERM_TEMP_VALUE_MASK: u32 = 0x1FF;
    pub const NV_THERM_TEMP_VALUE_SIGN_BIT: u32 = 0x100;

    // Display resolution registers (legacy/fallback)
    pub const NV50_HEAD0_RASTER_SIZE: u32 = 0x610798;
    pub const NV40_PCRTC_HEAD0_SIZE: u32 = 0x60002C;
}

/// TU106 (Turing) GMMU encode helpers — NV_MMU_VER2 page-table format.
///
/// Verified against nouveau `vmmgp100.c` / open-gpu `gp100/dev_mmu.h` (Turing
/// reuses the gp100 VER2 VMM verbatim). These build page tables in *RAM only*;
/// the GPU never sees them until the instance block is written and the GMMU is
/// flushed (a later, riskier step). Critical fact: the leaf PTE address field
/// is `phys >> 4` (the 53:8 field stores `phys>>12`, and `(phys>>12)<<8 ==
/// phys>>4`); writing `phys>>12` directly hangs the GPU.
mod gmmu {
    /// SYSTEM_COHERENT aperture (HOST). VRAM=0, HOST=2, NCOH=3.
    pub const AP_HOST: u64 = 2;
    /// PITCH (uncompressed) kind.
    pub const KIND_PITCH: u64 = 0x00;

    /// Leaf PTE for a 4 KiB sysmem page, read-write, uncompressed.
    /// VALID(0) | APERTURE 2:1 = HOST | VOL(3) | ADDRESS=phys>>4 | KIND 63:56.
    #[inline]
    pub fn encode_pte_sys(phys: u64) -> u64 {
        (phys >> 4) | (1 << 0) | (AP_HOST << 1) | (1 << 3) | (KIND_PITCH << 56)
    }

    /// Single PDE (PD1/PD2/PD3 levels) pointing at the next table in sysmem.
    /// APERTURE 2:1 = HOST (aperture != 0 ⇒ present; there is no VALID bit) |
    /// VOL(3) | ADDRESS_SYS 53:8 = next>>4. The dual-PDE SMALL half is encoded
    /// identically and stored in the high qword at byte `pdei*0x10 + 8`.
    #[inline]
    pub fn encode_pde_sys(next_table_phys: u64) -> u64 {
        (next_table_phys >> 4) | (AP_HOST << 1) | (1 << 3)
    }

    /// Instance-block PD-base qword (@0x200): root PD phys OR'd with
    /// VER2(1<<10) | 64KiB(1<<11) | HOST_target(2<<0) | VOL(1<<2) == `|0xC06`.
    #[inline]
    pub fn inst_pd_base(root_phys: u64) -> u64 {
        root_phys | 0xC06
    }
}

/// Coherent-sysmem structures for the Turing copy-engine bring-up (the verified
/// memory plan). All allocated via `DmaRegion::alloc_coherent` (page-aligned,
/// zeroed, UC). Built and dumped read-only at `/proc/gpudbg` for hand
/// verification BEFORE any GPU state is changed. The four buffers the *engine*
/// dereferences by VA (src/dst/sem/pushbuffer) are packed into a single 2 MiB
/// GMMU region so one SPT leaf and one PD0 entry cover everything.
#[allow(dead_code)] // inst/userd/gpfifo are wired up in later bring-up steps
struct GpuBringup {
    // 5-level page-directory chain (sysmem-coherent, one 4 KiB page each).
    root: DmaRegion, // desc_12[4], PGD 2-bit, the PDB given to the GPU
    pd3: DmaRegion,  // desc_12[3], PGD 9-bit
    pd2: DmaRegion,  // desc_12[2], PGD 9-bit
    pd0: DmaRegion,  // desc_12[1], dual-PDE 8-bit
    spt: DmaRegion,  // desc_12[0], SPT leaf, 512×8 B PTEs
    // Sysmem structures the engine reaches through the GMMU (by GPU VA), so
    // they stay in coherent sysmem and are mapped into the channel page tables.
    gpfifo: DmaRegion,
    pushbuf: DmaRegion,
    sem: DmaRegion,
    src: DmaRegion,
    dst: DmaRegion,
    /// Copy-engine fault-method buffer (sysmem). Only dereferenced by the CE
    /// engine on a faulting method; a red herring for channel load, kept mapped
    /// at va_base+0x5000 but its instance-block pointer is left disarmed.
    ce_fault: DmaRegion,
    /// HUB MMU non-replayable fault buffer (sysmem). On Volta+ the host requires
    /// a fault buffer armed (NV_VIRTUAL_FUNCTION_PRIV_MMU_FAULT_BUFFER, 0xb83000)
    /// before any channel can run — nouveau arms it in the `fault` subdev before
    /// the FIFO. We arm it in PHYSICAL/SYS_COH mode so no BAR2 mapping is needed.
    fault_buf: DmaRegion,
    /// Base GPU virtual address of the packed 2 MiB region.
    va_base: u64,
    /// Base VRAM offset (0-based into VRAM) for the structures the host reads by
    /// raw physical address — instance block, runlist, USERD. Turing's host
    /// scheduler walks these as VRAM-physical (the 0x002b00 runlist path has no
    /// target field), so they cannot live in sysmem. They are CPU-written via
    /// the PRAMIN window. Layout: inst=+0, runlist=+0x1000, userd=+0x2000.
    vram_base: u64,
}

impl GpuBringup {
    #[inline]
    fn inst_vram(&self) -> u64 {
        self.vram_base
    }
    #[inline]
    fn runlist_vram(&self) -> u64 {
        self.vram_base + 0x1000
    }
    #[inline]
    fn userd_vram(&self) -> u64 {
        self.vram_base + 0x2000
    }
    /// BAR2 instance block VRAM offset (shares the channel's page tables).
    #[inline]
    fn bar2_inst_vram(&self) -> u64 {
        self.vram_base + 0x3000
    }
    #[inline]
    fn gpfifo_va(&self) -> u64 {
        self.va_base + 0x4000
    }
    /// GPU/BAR2 VA of the CE fault-method buffer. Used once we arm the real CE
    /// engine context (after HOST/GP_GET is brought up).
    #[allow(dead_code)]
    #[inline]
    fn ce_fault_va(&self) -> u64 {
        self.va_base + 0x5000
    }
}

impl GpuBringup {
    /// Allocate the memory plan and build the GMMU page tables in RAM. No GPU
    /// register is touched here — only sysmem is written, so this is safe to run
    /// on demand. Returns `None` if the coherent DMA allocator is exhausted.
    fn build(va_base: u64, vram_base: u64) -> Option<Self> {
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
    fn dump(&self) -> String {
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
    fn write_setobject_pushbuffer(&self) -> u32 {
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
    fn write_gpfifo_entry(&self, slot: usize, pb_va: u64, n: u32) {
        let gp = self.gpfifo.vaddr();
        let w32 =
            |i: usize, v: u32| unsafe { core::ptr::write_volatile((gp as *mut u32).add(i), v) };
        let entry0 = (pb_va as u32) & 0xFFFF_FFFC;
        let entry1 = ((pb_va >> 32) as u32 & 0xFF) | (n << 10);
        w32(slot * 2, entry0);
        w32(slot * 2 + 1, entry1);
    }
}

static BOOT_FB_INFO: Mutex<Option<BootFbInfo>> = Mutex::new(None);

/// Runs `nvidia_rm_sys::rm_init::init_core()` (constructs the real OBJSYS
/// singleton + RM resource server) at most once, regardless of how many
/// GPUs attach or how many times a caller asks. Safe to call from every
/// `NvidiaGpu::debug_dump()`; only the first call actually invokes RM.
static RM_CORE_INIT_STATUS: Mutex<Option<u32>> = Mutex::new(None);

/// Set before invoking RM init, never cleared. If it's already set while
/// `RM_CORE_INIT_STATUS` is still `None`, a previous attempt started and
/// DIED mid-initialization (bring-up faults kill the reading task, not
/// the machine) -- RM's global C state (nvport/TLS init counts, g_pSys,
/// half-constructed OBJSYS children, rm locks) is debris at that point,
/// and re-running real NVIDIA init over it fails nondeterministically at
/// unrelated-looking places. Cost us a full diagnostic cycle: a re-run on
/// a dirty boot "regressed" three trace lines earlier than the previous
/// run and looked like a new bug. Refuse instead; only a reboot resets it.
static RM_CORE_INIT_ATTEMPTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Distinctive sentinel (not a real NV_STATUS) reported when RM init is
/// refused because a prior in-boot attempt died partway through.
const RM_INIT_POISONED: u32 = 0xDEAD_1417;

fn rm_core_init_once() -> u32 {
    use core::sync::atomic::Ordering;
    let mut status = RM_CORE_INIT_STATUS.lock();
    if let Some(s) = *status {
        return s;
    }
    if RM_CORE_INIT_ATTEMPTED.swap(true, Ordering::SeqCst) {
        log::error!(
            "[NVIDIA] rm_core_init_once: a previous RM init attempt this boot died \
             mid-initialization; refusing to re-enter over its half-initialized \
             global state. Reboot to retry (status={:#x}).",
            RM_INIT_POISONED
        );
        return RM_INIT_POISONED;
    }
    let s = nvidia_rm_sys::rm_init::init_core();
    *status = Some(s);
    s
}

/// One-shot guard so the first CE-offloaded present logs once (a console photo
/// then confirms the desktop is being composited by the copy engine).
static CE_PRESENT_LOGGED: AtomicBool = AtomicBool::new(false);

/// Latched when a CE-offload present returns a failure status. Submit is
/// under the RM gate; the completion poll is not. A faulting or hung CE
/// (a P2P write the GMMU can't map, an ACS-blocked peer DMA, a 100 ms
/// timeout) would still be a bad per-frame tax. Once latched, `ce_present`
/// stands down immediately and the proven CPU blit takes over for the rest
/// of the boot -- one loud failure line, then a stable fallback instead of
/// a wedge. This is what makes `nvidia.cepresent` safe to switch on for a
/// real-hardware test.
static CE_PRESENT_WEDGED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy)]
struct BootFbInfo {
    phys: u64,
    width: u32,
    height: u32,
    pitch: u32,
}

pub fn set_boot_fb_info(phys: u64, width: u32, height: u32, pitch: u32) {
    *BOOT_FB_INFO.lock() = Some(BootFbInfo {
        phys,
        width,
        height,
        pitch,
    });
}

/// Raw EDID of the active display captured by the UEFI bootloader
/// (`EFI_EDID_ACTIVE_PROTOCOL`), stashed at driver init. This is the real
/// panel on the GPU that drives the GOP console -- available with no GPU
/// display bring-up at all. `len` is the valid byte count (0 = none).
static BOOT_EDID: Mutex<Option<([u8; 128], u32)>> = Mutex::new(None);

/// Record the boot-time EDID (called from kernel-hal with the bootloader's
/// `GraphicInfo.edid`). A zero length is stored as "no EDID".
pub fn set_boot_edid(edid: &[u8], len: u32) {
    if len == 0 || edid.is_empty() {
        return;
    }
    let mut buf = [0u8; 128];
    let n = (len as usize).min(edid.len()).min(128);
    buf[..n].copy_from_slice(&edid[..n]);
    // Whatever the firmware last read off the DDC line lands here, and an
    // unpowered sink, a flaky line or a GOP that never filled its buffer all
    // produce something that looks like an EDID. Storing it would hand every
    // reader -- the DRM connector property, procfs, the physical size every
    // DPI-aware client scales by -- a panel invented out of line noise, so a
    // block that fails its own header or checksum is dropped here instead.
    if !crate::display::edid::block_valid(&buf[..n]) {
        warn!(
            "[edid] firmware handed over {} bytes that are not a valid EDID block; ignoring",
            n
        );
        return;
    }
    *BOOT_EDID.lock() = Some((buf, n as u32));
}

/// The captured UEFI EDID (bytes, valid length), if the firmware exposed one.
pub fn boot_edid() -> Option<([u8; 128], u32)> {
    *BOOT_EDID.lock()
}

/// Physical address of the boot (UEFI GOP) framebuffer, if known. The GPU whose
/// BAR1 aperture contains this address is the one driving the console.
fn boot_fb_phys() -> Option<u64> {
    BOOT_FB_INFO.lock().map(|b| b.phys)
}

/// Byte size of the boot (UEFI GOP) framebuffer (`pitch * height`), if known.
/// Used by the P2P CE path/tests, which run on the compute GPU and therefore
/// cannot read the console FB geometry from their own `self.info`.
fn boot_fb_size() -> Option<u64> {
    BOOT_FB_INFO
        .lock()
        .map(|b| b.pitch as u64 * b.height as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvidiaArchitecture {
    Unknown,
    Turing,      // RTX 20 series
    Ampere,      // RTX 30 series
    AdaLovelace, // RTX 40 series
    Hopper,      // H100/H200
    Blackwell,   // RTX 50 series
}

#[derive(Clone, Copy)]
struct ImportedGemHandle {
    id: u32,
    phys_addr: u64,
    size: usize,
}

#[derive(Clone, Copy)]
struct NvidiaKmsFramebuffer {
    id: u32,
    handle_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    phys_addr: u64,
    size: usize,
    /// RM `NV01_MEMORY_*` handle for ISO ctxdma (0 = unknown / dumb-only).
    h_memory: u32,
    /// FBMEM offset (`AT_GPU`) for VRAM BOs; `None` for sysmem.
    vram_offset: Option<u64>,
}

#[derive(Clone, Copy)]
struct NvidiaKmsState {
    crtc_fb: u32,
    plane_fb: u32,
    last_vblank_us: u64,
}

pub struct NvidiaGpu {
    name: String,
    info: DisplayInfo,
    architecture: NvidiaArchitecture,
    gpu_model: &'static str,
    /// Raw PCI device id, kept for `NOUVEAU_GETPARAM_PCI_DEVICE` (see
    /// `nouveau_uapi.rs`) -- `identify_gpu` already consumes this once at
    /// construction but didn't need to retain it before now.
    device_id: u16,
    vram_size_mb: u32,
    pitch_override: Option<u32>,
    _bar0: usize,
    _bar1: usize,
    /// Physical base of BAR1 (the VRAM aperture). Used to decide whether this GPU
    /// backs the boot framebuffer (i.e. drives the console) and must therefore be
    /// spared from the risky copy-engine bring-up writes.
    bar1_phys: u64,
    /// Physical base and mapped length of BAR0 (the MMIO register aperture),
    /// and this GPU's real PCI location -- needed to attach it to the real
    /// vendored RM core via nvidia_rm_sys::rm_init (GPUATTACHARG wants the
    /// same info NVIDIA's own osInitNvMapping packages from nv_state_t).
    bar0_phys: u64,
    bar0_len: u64,
    /// Physical base and size of BAR2 (NVIDIA logical index `IMEM`, the small
    /// ~32 MiB "instance memory" aperture -- PCI BAR3 on Turing). RM needs this
    /// as `GPUATTACHARG.instPhysAddr`/`instLength`: without it, `kbusVerifyBar2`
    /// (kern_bus_gm107.c) has no BAR2 physical aperture to program, its MMU
    /// self-test write never lands in VRAM, and `gpumgrStateInitGpu` fails with
    /// NV_ERR_MEMORY_ERROR (0x72). Matches osinit.c:708
    /// (`nv->bars[NV_GPU_BAR_INDEX_IMEM]`).
    bar2_phys: u64,
    bar2_len: u64,
    pci_domain: u32,
    pci_bus: u8,
    pci_device: u8,
    vram_allocator: Mutex<Option<NvidiaVramAllocator>>,
    /// Copy-engine bring-up state (GMMU tables + channel structs). Built lazily
    /// on the first `/proc/gpudbg` read so the memory plan is only allocated
    /// when someone is actually debugging GPU bring-up.
    bringup: Mutex<Option<GpuBringup>>,
    /// Result of the real RM attach attempt (nvidia_rm_sys::rm_init), cached
    /// after the first `/proc/gpudbg` read triggers it so repeated reads
    /// don't re-run RM's own object-construction logic.
    rm_attach_result: Mutex<Option<String>>,
    /// Real RM device instance from a successful attach, needed to look the
    /// `OBJGPU*` back up (`gpumgrGetGpu`) for the GSP init step below.
    rm_device_instance: Mutex<Option<u32>>,
    /// Boot-stable KMS topology snapshot: the FIRST successful RM display
    /// query, frozen for the rest of the boot. Every KMS-visible fact
    /// (connector ids, supported/connected masks, EDID head) is served from
    /// this one snapshot, so the connector id space can never flip between
    /// the legacy synthetic connector (1001) and the real per-output ids
    /// (1001+100*instance+bit) in the middle of one client's probe. That
    /// flip is fatal in practice: mesa's `wsi_get_connectors()` treats a
    /// GETCONNECTOR miss on ANY id GETRESOURCES advertised as a blanket
    /// `VK_ERROR_OUT_OF_HOST_MEMORY` on both `VK_KHR_display` entry points
    /// (no errno inspection at all), and the pre-cache behaviour -- a fresh
    /// NV0073 query per ioctl that returns `None` on `EDID_IN_FLIGHT`
    /// contention or before bring-up -- did exactly that, with vulkaninfo's
    /// own first ioctls triggering bring-up mid-probe. Trade-off: display
    /// hotplug after the first query is invisible until reboot (fine here:
    /// one fixed monitor, probed at session start).
    rm_display_snap: Mutex<Option<(u32, nvidia_rm_sys::rm_init::GrEdid)>>,
    /// [auto-bringup] One-shot latch: the console GPU's on-demand full bring-up
    /// (`bringup_step14`) is attempted at most once, on the first GPU client, so
    /// `EXEC` works without a manual `cat /proc/gpustep14`. A failed or wedged
    /// GSP boot must never be retried on a later ioctl.
    auto_bringup_done: AtomicBool,
    /// Real GSP-RM firmware (`gsp.bin`), pushed down by `zCore`'s boot code
    /// via `set_gsp_firmware` once the rootfs is mounted -- this driver runs
    /// during early PCI enumeration, well before any filesystem exists, so
    /// it cannot read the file itself (see DrmScheme::set_gsp_firmware).
    gsp_firmware: Mutex<Option<Vec<u8>>>,
    /// Human-readable outcome of the boot-time firmware load (set even when it
    /// failed), so `bringup_step6` can explain a missing blob. See
    /// `DrmScheme::set_gsp_firmware_status`.
    gsp_fw_status: Mutex<Option<String>>,
    /// Result of the real kgspInitRm attempt, cached the same way as
    /// `rm_attach_result`.
    gsp_init_result: Mutex<Option<String>>,
    /// Cached step-9 result (gpuState PreInit/Init/Load). One-shot per boot:
    /// the RM state machine is not re-runnable, so the first outcome is
    /// what /proc/gpustep9 keeps reporting.
    state_init_result: Mutex<Option<String>>,
    /// Cached step-10 result (CE memset/copy + readback verify). Cached like
    /// the others so repeated `cat`s don't re-run CE work; a reboot re-arms.
    step10_result: Mutex<Option<String>>,
    /// Imported GEM handles from the DRM core; indexed by core handle id.
    imported_handles: Mutex<Vec<ImportedGemHandle>>,
    /// Nouveau-uAPI state (see `nouveau_uapi.rs`), opt-in via
    /// `nvidia.nouveau_uapi`. `None` until `CHANNEL_ALLOC` succeeds; this
    /// milestone supports exactly one channel, backed by the existing
    /// step16+step17 bring-up ladder.
    nouveau_channels: Mutex<Vec<super::nouveau_uapi::NouveauChannelState>>,
    /// GEM objects allocated through the nouveau-uAPI `GEM_NEW`, distinct
    /// from `imported_handles` (which tracks buffers the generic DRM core
    /// allocated via `CREATE_DUMB`).
    nouveau_gem: Mutex<Vec<super::nouveau_uapi::NouveauGemObject>>,
    /// Next handle to hand out from `GEM_NEW`, within this GPU's private
    /// slice of the driver-private handle range (see
    /// [`crate::scheme::gem_mmap::alloc_handle_slice`]). Per-GPU rather than
    /// global because the registries these ids key into -- `gem_mmap`'s
    /// physical-address table and `linux-object`'s `NOUVEAU_CPU_VMOS` -- have
    /// no GPU in their key, so two cards counting from the same base would
    /// alias each other's buffers.
    nouveau_gem_next_handle: AtomicU32,
    /// One past the last handle this GPU may hand out. `GEM_NEW` refuses
    /// instead of walking into the next card's slice.
    nouveau_gem_handle_end: u32,
    /// Active `VM_BIND` GPU-VA mappings, so `UNMAP` can find the RM handle
    /// to tear down.
    nouveau_vm_mappings: Mutex<Vec<super::nouveau_uapi::NouveauVmMapping>>,
    /// Per-process GPU context assignment: `(owner_pid, ctx_idx, h_vas,
    /// h_notifier)`. The compositor (context 0) is NOT tracked here. A GL
    /// client's context is built on its FIRST GPU touch -- `VM_BIND` or
    /// `CHANNEL_ALLOC`, whichever comes first (NVK issues `VM_BIND` during
    /// device creation, BEFORE `CHANNEL_ALLOC`) -- and reused for all its later
    /// `VM_BIND`/`EXEC`, so a client's binds and its channel always share ONE VA
    /// space. Freed and dropped on process exit. This is the authority for
    /// pid->context routing (`ensure_ctx_for_pid`/`ctx_idx_for_pid`); tying it
    /// to the channel list was the bug that let a client's binds land in the
    /// compositor's VAS (MMU fault on the client's first submission).
    ///
    /// The final `bool` is READY: `false` while the owning thread is still in
    /// `ctx_alloc`/`ctx_prime`. The entry is pushed not-ready up front so a
    /// concurrent first-touch of the same pid (NVK is multithreaded) reserves
    /// exactly one slot -- but no caller is handed the ctx until the golden
    /// context is primed. Before this flag, the entry was published BEFORE the
    /// prime and a sibling thread could race a graphics EXEC onto the unprimed
    /// channel: the 3D draw then cold-loaded the GR context and hung FECS in
    /// RESTORE (idle GR, drained PBDMA, no fault -- the real-RTX signature).
    nouveau_pid_ctx: Mutex<Vec<(u64, u32, u32, u32, bool)>>,
    /// Per-context direct-submit state, indexed by ctx_idx (see `FastSlot`).
    nouveau_fast: Mutex<Vec<FastSlot>>,
    /// `(consumer ctx, producer ctx)` -> the producer's fence semaphore as
    /// the consumer's channel addresses it (`map_peer_fence`). Mirrors the
    /// RM's own table, which is dropped when either context is freed: so is
    /// this one (`forget_peer_fences`), or the next tenant of that index
    /// would be waited on through a VA the RM has already unmapped.
    nouveau_peer_fence: Mutex<alloc::collections::BTreeMap<(u32, u32), u64>>,
    /// Driver-private framebuffer objects keyed by driver fb id.
    kms_framebuffers: Mutex<Vec<NvidiaKmsFramebuffer>>,
    /// Driver-side ids for framebuffer objects.
    next_kms_fb_id: AtomicU32,
    /// Current KMS state exposed by GETCRTC/GETPLANE and used by wait_vblank.
    kms_state: Mutex<NvidiaKmsState>,
    /// Sticky owner of the compositor singleton (GPU context 0 / step16+17
    /// ladder). `0` = unclaimed. Set when a process successfully takes the
    /// ctx-0 `CHANNEL_ALLOC` path; cleared only on that process's exit /
    /// `reset_ctx0_singleton`. Inferring the role from "who currently has an
    /// rm_backed ctx-0 channel" let a client steal ctx 0 after the compositor
    /// freed a throwaway channel or crashed mid-session (VM_BIND → client VAS,
    /// EXEC → ctx 0 → MMU fault / FECS RESTORE hang).
    ctx0_owner: AtomicU64,
    /// MSI interrupt vector assigned by the PCI scan (`irq + 32`), or
    /// `usize::MAX` if this GPU has no MSI. Used only by the console-GPU GSP
    /// boot to bring the GPU's MSI delivery online across the SEC2-resume
    /// window (the Linux-faithful interrupt path). Set once, after construction.
    msi_vector: AtomicUsize,
}

/// Simple bitmap-based VRAM allocator for BAR1 aperture (4KB page granularity)
struct NvidiaVramAllocator {
    base_phys: u64,
    total_size: u64,
    bitmap: Vec<u64>,
}

impl NvidiaVramAllocator {
    fn new(base_phys: u64, total_size: u64) -> Self {
        let num_pages = (total_size / 4096) as usize;
        let num_u64s = num_pages.div_ceil(64);
        Self {
            base_phys,
            total_size,
            bitmap: alloc::vec![0; num_u64s],
        }
    }

    /// Unused (kept for a future purely-Rust-side allocation need): the
    /// nouveau-uAPI `GEM_NEW` handler (`nvidia.rs` `ioctl`) allocates
    /// through the real RM heap instead (`nvidia_rm_sys::rm_init::
    /// gem_alloc`) so it shares RM's own VRAM bookkeeping rather than
    /// carving up the same physical range out-of-band with a second,
    /// independent allocator.
    #[allow(dead_code)]
    fn _alloc(&mut self, size: usize, align: usize) -> Option<u64> {
        let num_pages = size.div_ceil(4096);
        let align_pages = (align.max(4096) / 4096).max(1);
        let total_bits = (self.total_size / 4096) as usize;

        let mut count = 0;
        let mut start_bit = 0;

        for bit in 0..total_bits {
            let uidx = bit / 64;
            let ubit = bit % 64;
            let is_free = (self.bitmap[uidx] & (1 << ubit)) == 0;

            if is_free {
                if count == 0 {
                    if bit % align_pages != 0 {
                        continue;
                    }
                    start_bit = bit;
                }
                count += 1;
                if count >= num_pages {
                    for i in 0..num_pages {
                        let b = start_bit + i;
                        self.bitmap[b / 64] |= 1 << (b % 64);
                    }
                    return Some(self.base_phys + (start_bit as u64 * 4096));
                }
            } else {
                count = 0;
            }
        }
        None
    }

    fn free(&mut self, phys_addr: u64, size: usize) {
        let offset = phys_addr.saturating_sub(self.base_phys);
        if offset >= self.total_size {
            return;
        }
        let start_bit = (offset / 4096) as usize;
        let num_pages = size.div_ceil(4096);
        for i in 0..num_pages {
            let b = start_bit + i;
            if b / 64 < self.bitmap.len() {
                self.bitmap[b / 64] &= !(1 << (b % 64));
            }
        }
    }
}

/// Last outcome of the HDMI/DP audio enable, for `/proc/gpusnd`.
///
/// On a GPU the HDA codec accepting a stream proves nothing: the display
/// engine is what puts audio packets on the cable. When playback is silent
/// with no error anywhere, this line is what says whether that half ever ran.
static HDMI_AUDIO_STATUS: lock::Mutex<Option<alloc::string::String>> = lock::Mutex::new(None);

/// Every NVIDIA GPU this driver probed, in PCI order. The HDA driver's
/// stream-start kick walks this instead of guessing RM instance numbers:
/// a GPU that was never RM-attached has no instance at all, and that case
/// (the console GPU, which is never auto-booted) is precisely the one that
/// needs naming in `/proc/gpusnd`.
static NVIDIA_GPUS: lock::Mutex<Vec<Arc<NvidiaGpu>>> = lock::Mutex::new(Vec::new());

/// Live bytes across every entry in every GPU's `nouveau_gem` table.
/// Updated on `GEM_NEW` / `GEM_CLOSE` / process-exit free. Quotas in the
/// `GEM_NEW` arm consult this before calling `gem_alloc`.
static NOUVEAU_GEM_BYTES: AtomicU64 = AtomicU64::new(0);

/// Hard cap on a single `GEM_NEW` allocation.
const GEM_NEW_MAX_SINGLE: u64 = 256 * 1024 * 1024; // 256 MiB
/// Cap on live `nouveau_gem` bytes owned by one pid.
const GEM_NEW_MAX_PER_PID: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB
/// Soft global ceiling; actual cap is `min(this, vram_bytes * 2)`.
const GEM_NEW_MAX_GLOBAL: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

fn record_hdmi_audio_status(s: alloc::string::String) {
    *HDMI_AUDIO_STATUS.lock() = Some(s);
}

/// What the last HDMI/DP audio enable did, or why it never ran.
pub fn hdmi_audio_status() -> alloc::string::String {
    match &*HDMI_AUDIO_STATUS.lock() {
        Some(s) => alloc::format!("{}\n", s),
        None => alloc::string::String::from(
            "[hdmi-audio] never ran — no RM display query and no digital stream start yet,\n\
             [hdmi-audio]   so the display engine was never told to transmit audio.\n",
        ),
    }
}

/// Linux-style HDMI topology: one NVIDIA HDA function, the GPU whose BAR1
/// holds the GOP framebuffer (the monitor). Extra GPUs on a dual-RTX board
/// stay silent HDMI cards in Linux too unless a display is plugged in; we
/// simply do not probe them.
pub fn nvidia_hda_is_monitor_gpu(bus: u8, device: u8) -> bool {
    let gpus = NVIDIA_GPUS.lock();
    if let Some(g) = gpus
        .iter()
        .find(|g| g.pci_bus == bus && g.pci_device == device)
    {
        return g.drives_boot_display();
    }
    drop(gpus);
    sibling_bar1_holds_boot_fb(bus, device)
}

/// Function 0 of `bus:dev` is the GPU; BAR1 (config 0x18, 64-bit) is the
/// framebuffer aperture. The GOP scanout physical address sits inside that
/// window on the console GPU only.
fn sibling_bar1_holds_boot_fb(bus: u8, device: u8) -> bool {
    let Some(fb) = boot_fb_phys() else {
        return false;
    };
    if fb == 0 {
        return false;
    }
    let loc = pci::Location {
        bus,
        device,
        function: 0,
    };
    let bar1 = unsafe {
        crate::bus::pci::read_bar_addr(
            &crate::bus::pci::PortOpsImpl,
            crate::bus::pci::PCI_ACCESS,
            loc,
            0x18,
        )
    };
    if bar1 == 0 {
        return false;
    }
    fb >= bar1 && fb.saturating_sub(bar1) < (512 << 20)
}

/// Build a 96-byte ELD from a 128-byte base EDID (nvkms FillELDBuffer layout).
/// CEA SADs live in extension block 1, which the bootloader does not keep;
/// a basic-audio 2ch LPCM SAD is used so the sink still plays 48 kHz stereo.
fn build_eld_from_base_edid(edid: &[u8], display_id: u32, is_dp: bool) -> [u8; 96] {
    let mut eld = [0u8; 96];
    if edid.len() < 128 {
        return eld;
    }
    let mut mnl = 0u32;
    const DESC: [usize; 4] = [54, 72, 90, 108];
    for off in DESC {
        if edid[off] == 0 && edid[off + 1] == 0 && edid[off + 2] == 0 && edid[off + 3] == 0xFC {
            eld[20..33].copy_from_slice(&edid[off + 5..off + 18]);
            mnl = 13;
            break;
        }
    }
    // LPCM, 2ch, 32/44.1/48 kHz, 16/20/24-bit (HDMI "basic audio").
    let sad = [0x09u8, 0x07, 0x07];
    let sad_count = 1u32;
    let spk_alloc = 0x01u8;
    eld[0] = 2 << 3;
    eld[4] = mnl as u8;
    eld[5] = ((sad_count << 4) | if is_dp { 1 << 2 } else { 0 }) as u8;
    eld[7] = spk_alloc;
    eld[8] = display_id as u8;
    eld[9] = (display_id >> 8) as u8;
    eld[10] = (display_id >> 16) as u8;
    eld[11] = (display_id >> 24) as u8;
    eld[16] = edid[8];
    eld[17] = edid[9];
    eld[18] = edid[10];
    eld[19] = edid[11];
    let sad_off = 20 + mnl as usize;
    eld[sad_off..sad_off + 3].copy_from_slice(&sad);
    eld[2] = (16 + mnl + sad_count * 3).div_ceil(4) as u8;
    eld
}

impl NvidiaGpu {
    fn pitch_pixels(&self) -> usize {
        if let Some(p) = self.pitch_override {
            return (p / 4) as usize;
        }

        let width = self.info.width as usize;
        let height = self.info.height as usize;
        if width == 0 || height == 0 {
            return width;
        }

        // Accept moderately padded scanlines (for example 2048-wide alignment on
        // a 1920-wide mode) while rejecting BAR apertures that are far larger
        // than the visible framebuffer and would produce a bogus inferred pitch.
        const MAX_PITCH_PADDING_PIXELS: usize = 4096;
        let bytes_per_pixel = self.info.format.bytes() as usize;

        // If fb_size is suspiciously large (entire BAR), don't infer pitch from it.
        // A typical 1080p framebuffer is ~8MB. BARs are usually 256MB+.
        if self.info.fb_size >= 16 * 1024 * 1024 {
            return width;
        }

        let visible_size = width.saturating_mul(height).saturating_mul(bytes_per_pixel);

        if self.info.fb_size >= visible_size {
            let inferred = self.info.fb_size / height / bytes_per_pixel;
            if inferred >= width && inferred <= width + MAX_PITCH_PADDING_PIXELS {
                return inferred;
            }
        }

        width
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        device_id: u16,
        bar0: usize,
        fb_vaddr: usize,
        fb_size: usize,
        bar1_phys: u64,
        default_width: u32,
        default_height: u32,
        bar0_phys: u64,
        bar0_len: u64,
        bar2_phys: u64,
        bar2_len: u64,
        pci_domain: u32,
        pci_bus: u8,
        pci_device: u8,
    ) -> DeviceResult<Self> {
        // Boot path: identify from PCI ID only. BAR0 MMIO reads during early
        // driver init can stall the CPU indefinitely on some firmware/GPU combos
        // (screen frozen at 80%). PMC/VRAM/resolution probes are deferred.
        let (arch, gpu_model, vram_size_mb) = identify_gpu(device_id);

        let mut w = default_width;
        let mut h = default_height;
        let mut pitch_override = None;
        let final_fb_vaddr = fb_vaddr;

        // Check if this GPU matches the boot framebuffer (UEFI GOP)
        if let Some(boot_info) = *BOOT_FB_INFO.lock() {
            // How do we know the physical address of fb_vaddr?
            // In zCore/drivers, we usually don't have a direct way back to phys,
            // but we can assume fb_vaddr is mapped to a BAR.
            // We'll trust the PCI scan to have passed the correct bar1_phys in some way,
            // but since we only have fb_vaddr here, we might need more info.
            // However, we can use a heuristic: if we have 2 GPUs, and boot_info.phys
            // is within the range of this GPU's BAR1, then this is the primary GPU.

            // For now, let's assume the caller will set the correct resolution
            // if it knows it. But if it doesn't, we can try to match.
            // Since we don't have the phys address of fb_vaddr here easily
            // without a page table lookup, let's rely on the fact that
            // KCONFIG info is usually more accurate than hardcoded 1920x1080.

            // If the default provided is the "magic" 1920x1080 from pci.rs,
            // and we have boot_info, use boot_info.
            if default_width == 1920 && default_height == 1080 {
                w = boot_info.width;
                h = boot_info.height;
                pitch_override = Some(boot_info.pitch);

                // If the boot phys is within this aperture, we might need to adjust fb_vaddr
                // But usually fb_vaddr is the start of the BAR. GOP might be offset.
                // In eclipse-old: fb_phys = boot_info.phys; offset = fb_phys - bar1_phys;
                // Here we'll just assume the pitch is the main fix needed for now.
                log::info!(
                    "[NVIDIA] Inheriting boot resolution: {}x{} (pitch: {})",
                    w,
                    h,
                    boot_info.pitch
                );
            }
        }

        let temperature = read_temperature(bar0);

        log::warn!(
            "[NVIDIA] Detected {} ({:?}), VRAM: {} MB, Temp: {:?}°C, Res: {}x{}",
            gpu_model,
            arch,
            vram_size_mb,
            temperature,
            w,
            h
        );

        let pitch = pitch_override.unwrap_or(w * 4);

        let info = DisplayInfo {
            width: w,
            height: h,
            pitch,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: final_fb_vaddr,
            fb_size,
        };

        // One disjoint slice of the driver-private GEM handle range per GPU;
        // the tables these handles key into are global and have no GPU in
        // their key (see drivers/src/scheme/gem_mmap.rs).
        let gem_handle_slice = crate::scheme::gem_mmap::alloc_handle_slice();

        Ok(Self {
            name,
            info,
            architecture: arch,
            gpu_model,
            device_id,
            vram_size_mb,
            pitch_override,
            _bar0: bar0,
            _bar1: final_fb_vaddr,
            bar1_phys,
            bar0_phys,
            bar0_len,
            bar2_phys,
            bar2_len,
            pci_domain,
            pci_bus,
            pci_device,
            vram_allocator: Mutex::new(Some(NvidiaVramAllocator::new(
                fb_vaddr as u64,
                fb_size as u64,
            ))),
            bringup: Mutex::new(None),
            rm_attach_result: Mutex::new(None),
            rm_device_instance: Mutex::new(None),
            rm_display_snap: Mutex::new(None),
            auto_bringup_done: AtomicBool::new(false),
            gsp_firmware: Mutex::new(None),
            gsp_fw_status: Mutex::new(None),
            gsp_init_result: Mutex::new(None),
            state_init_result: Mutex::new(None),
            step10_result: Mutex::new(None),
            imported_handles: Mutex::new(Vec::new()),
            nouveau_channels: Mutex::new(Vec::new()),
            nouveau_gem: Mutex::new(Vec::new()),
            // High half of u32, disjoint from linux-object's own DRM_STATE
            // handle ids (CREATE_DUMB/PRIME, sequential starting at 1) --
            // both id spaces are decoded from the same fake-mmap-offset
            // bits by DrmDev::get_vmo, so a collision would resolve a
            // mmap() to the wrong physical range. Within that half, each
            // GPU takes its own slice, because the registries keyed by
            // these ids are global. See drivers/src/scheme/gem_mmap.rs.
            nouveau_gem_next_handle: AtomicU32::new(gem_handle_slice.base()),
            nouveau_gem_handle_end: gem_handle_slice.end(),
            nouveau_vm_mappings: Mutex::new(Vec::new()),
            nouveau_pid_ctx: Mutex::new(Vec::new()),
            nouveau_fast: Mutex::new(
                (0..super::nouveau_uapi::MAX_CTX)
                    .map(|_| FastSlot::Unprepared)
                    .collect(),
            ),
            nouveau_peer_fence: Mutex::new(alloc::collections::BTreeMap::new()),
            kms_framebuffers: Mutex::new(Vec::new()),
            next_kms_fb_id: AtomicU32::new(1),
            kms_state: Mutex::new(NvidiaKmsState {
                crtc_fb: 0,
                plane_fb: 0,
                last_vblank_us: 0,
            }),
            msi_vector: AtomicUsize::new(usize::MAX),
            ctx0_owner: AtomicU64::new(0),
        })
    }

    /// Record the MSI vector the PCI scan assigned this GPU (`irq + 32`). Called
    /// once from the probe; `None` (no MSI cap) leaves it as `usize::MAX`.
    pub fn set_msi_vector(&self, irq: Option<usize>) {
        if let Some(irq) = irq {
            self.msi_vector.store(irq + 32, Ordering::Relaxed);
        }
    }

    pub fn architecture(&self) -> NvidiaArchitecture {
        self.architecture
    }
    pub fn model(&self) -> &'static str {
        self.gpu_model
    }
    pub fn vram_size_mb(&self) -> u32 {
        self.vram_size_mb
    }

    /// VRAM size in MiB for *reporting and sizing*, never zero for a GPU this
    /// driver can drive.
    ///
    /// `identify_gpu()` runs from the PCI device-id ALONE (BAR0 MMIO is unsafe
    /// that early — it stalled boot at 80%), and returns 0 MiB for any id not
    /// in its table: a mere board variant of a supported chip. But
    /// `nouveau_arch()` still recovers the real architecture from PMC_BOOT_0 at
    /// runtime, so such a GPU is otherwise handed to NVK as a valid Turing+
    /// device whose VRAM heap is EMPTY — GETPARAM_FB_SIZE = 0 and the RM's
    /// `attach_gpu` fbSize = 0. NVK builds the device, then walks a NULL heap
    /// the instant zink creates its timeline semaphore ("failed to create
    /// timeline semaphore", the crash at libvulkan_nouveau.so+0x9cc48).
    ///
    /// Floor it to a conservative per-architecture value so the heap is never
    /// empty. Under-reporting is safe (NVK and the RM simply manage less FB
    /// than exists); adding the variant's device-id to `identify_gpu` restores
    /// the exact size. BAR0 is safe to read here — every caller is well past
    /// boot (a GETPARAM ioctl, or the RM attach).
    fn effective_vram_mb(&self) -> u32 {
        if self.vram_size_mb != 0 {
            return self.vram_size_mb;
        }
        let floor = match self.nouveau_arch() {
            NvidiaArchitecture::Turing => 4096,
            NvidiaArchitecture::Ampere => 8192,
            NvidiaArchitecture::AdaLovelace => 8192,
            NvidiaArchitecture::Hopper => 16384,
            NvidiaArchitecture::Blackwell => 12288,
            // Truly unrecognized: NVK skips this GPU anyway (engine classes =
            // None), so 0 changes nothing and we don't invent VRAM for a chip
            // this driver cannot drive.
            NvidiaArchitecture::Unknown => return 0,
        };
        static LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::klog_warn!(
                "[nouveau-uapi] PCI device-id {:#06x} not in identify_gpu table -- flooring VRAM \
                 to {} MiB for {:?}; add the id for the exact size",
                self.device_id,
                floor,
                self.nouveau_arch()
            );
        }
        floor
    }

    pub fn temperature(&self) -> Option<i32> {
        read_temperature(self._bar0)
    }

    /// True if this GPU's BAR1 aperture contains the boot framebuffer — i.e. it
    /// is the GPU scanning out to the monitor. Such a GPU is spared from the
    /// copy-engine bring-up writes so a wedge can never blank the console.
    fn drives_boot_display(&self) -> bool {
        match boot_fb_phys() {
            Some(phys) if phys != 0 => {
                let lo = self.bar1_phys;
                let hi = lo.saturating_add(self.info.fb_size as u64);
                phys >= lo && phys < hi
            }
            _ => false,
        }
    }

    /// This GPU's PCI config-space location (Eclipse is single-segment, so
    /// domain is dropped; RM only ever runs function 0 of the GPU).
    fn cfg_loc(&self) -> pci::Location {
        pci::Location {
            bus: self.pci_bus,
            device: self.pci_device,
            function: 0,
        }
    }

    fn cfg_read16(&self, off: u16) -> u16 {
        unsafe {
            crate::bus::pci::PCI_ACCESS.read16(&crate::bus::pci::PortOpsImpl, self.cfg_loc(), off)
        }
    }

    fn cfg_read32(&self, off: u16) -> u32 {
        unsafe {
            crate::bus::pci::PCI_ACCESS.read32(&crate::bus::pci::PortOpsImpl, self.cfg_loc(), off)
        }
    }

    fn cfg_write16(&self, off: u16, val: u16) {
        unsafe {
            crate::bus::pci::PCI_ACCESS.write16(
                &crate::bus::pci::PortOpsImpl,
                self.cfg_loc(),
                off,
                val,
            )
        }
    }

    /// Offset of the PCI Express capability (cap id 0x10) in config space, or
    /// 0 if the function has none. Walks the standard capabilities list.
    fn pcie_cap_offset(&self) -> u8 {
        // Status register (0x06) bit 4 = capabilities list present.
        if self.cfg_read16(0x06) & (1 << 4) == 0 {
            return 0;
        }
        let mut ptr = (self.cfg_read16(0x34) & 0xFC) as u8; // capabilities pointer
        let mut guard = 0;
        while ptr != 0 && guard < 48 {
            let hdr = self.cfg_read16(ptr as u16);
            if (hdr & 0xFF) as u8 == 0x10 {
                return ptr;
            }
            ptr = ((hdr >> 8) & 0xFC) as u8; // next-capability pointer
            guard += 1;
        }
        0
    }

    /// Issue a PCIe Function Level Reset on this GPU. Returns true if issued.
    /// Follows the PCIe spec: confirm FLR capability, wait for pending
    /// transactions to drain, set Initiate FLR, then wait 100 ms for the reset
    /// to complete. Config state is intentionally NOT restored -- the caller
    /// resets the CPU immediately after, so the GPU only has to survive to the
    /// next firmware POST, which re-inits it from cold.
    fn pcie_flr(&self) -> bool {
        let cap = self.pcie_cap_offset();
        if cap == 0 {
            return false;
        }
        // Device Capabilities (cap+0x04) bit 28 = Function Level Reset capable.
        if self.cfg_read32((cap as u16) + 0x04) & (1 << 28) == 0 {
            return false;
        }
        // Wait (bounded) for Transactions Pending (Device Status cap+0x0A bit 5).
        let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };
        while self.cfg_read16((cap as u16) + 0x0A) & (1 << 5) != 0 {
            if unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0) > 100_000 {
                break;
            }
            gpu_spin();
        }
        // Set Initiate FLR (Device Control cap+0x08 bit 15).
        let devctl = self.cfg_read16((cap as u16) + 0x08);
        self.cfg_write16((cap as u16) + 0x08, devctl | (1 << 15));
        // PCIe requires up to 100 ms before the function is usable again.
        let t1 = unsafe { crate::bus::drivers_timer_now_as_micros() };
        while unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t1) < 100_000 {
            gpu_spin();
        }
        true
    }

    fn imported_handle(&self, handle_id: u32) -> Option<ImportedGemHandle> {
        self.imported_handles
            .lock()
            .iter()
            .find(|h| h.id == handle_id)
            .copied()
    }

    /// Drop every KMS framebuffer built on `handle_id`, and unlatch it from
    /// the CRTC and the plane if it was still scanning out.
    ///
    /// This must run BEFORE the backing memory goes back to its allocator.
    /// A `NvidiaKmsFramebuffer` caches `phys_addr`/`h_memory` at `create_fb`
    /// time, and `present_kms_fb`/`page_flip` read them with no further
    /// lookup, so an fb left behind scans out whatever the allocator hands
    /// the next caller.
    ///
    /// Returns the ids it removed, for logging.
    fn drop_kms_fbs_for_handle(&self, handle_id: u32) -> Vec<u32> {
        let removed_ids: Vec<u32> = {
            let mut fbs = self.kms_framebuffers.lock();
            let ids: Vec<u32> = fbs
                .iter()
                .filter(|fb| fb.handle_id == handle_id)
                .map(|fb| fb.id)
                .collect();
            fbs.retain(|fb| fb.handle_id != handle_id);
            ids
        };
        if !removed_ids.is_empty() {
            let mut state = self.kms_state.lock();
            if removed_ids.iter().any(|id| *id == state.crtc_fb) {
                state.crtc_fb = 0;
            }
            if removed_ids.iter().any(|id| *id == state.plane_fb) {
                state.plane_fb = 0;
            }
        }
        removed_ids
    }

    fn kms_fb(&self, fb_id: u32) -> Option<NvidiaKmsFramebuffer> {
        self.kms_framebuffers
            .lock()
            .iter()
            .find(|f| f.id == fb_id)
            .copied()
    }

    fn present_kms_fb(&self, fb_id: u32) -> bool {
        use crate::bus::phys_to_virt;
        let Some(fb) = self.kms_fb(fb_id) else {
            return false;
        };
        if fb.phys_addr == 0 || fb.size < 4 || fb.pitch == 0 {
            return false;
        }
        let src_vaddr = phys_to_virt(fb.phys_addr as usize);
        let src = unsafe { core::slice::from_raw_parts(src_vaddr as *const u32, fb.size / 4) };
        let width = fb.width.min(self.info.width);
        let height = fb.height.min(self.info.height);
        let src_stride = (fb.pitch / 4) as usize;
        self.blit_from(0, 0, src, src_stride, width, height);
        let _ = self.flush();
        let now = unsafe { crate::bus::drivers_timer_now_as_micros() };
        let mut state = self.kms_state.lock();
        state.crtc_fb = fb.id;
        state.plane_fb = fb.id;
        state.last_vblank_us = now;
        true
    }

    /// Pre-boot hardware-state snapshot (Copilot/checklist item: "comparar
    /// dump de config-space/PMC/display regs primaria vs secundaria justo
    /// antes del resume"). Raw BAR0 reads only -- no RM involvement. Emitted
    /// into the /proc block AND live at ERROR level so the console GPU's
    /// values survive a wedge on screen. The interesting delta vs. the
    /// secondary: PDISP_VGA_WORKSPACE_BASE (live VGA workspace => bit0
    /// VALID) and BSI_SECURE_SCRATCH_14 (BRSS handoff state).
    fn dump_preboot_state(&self, tag: &str) -> String {
        use core::fmt::Write;
        let bar0 = self._bar0;
        let rd =
            |off: usize| -> u32 { unsafe { core::ptr::read_volatile((bar0 + off) as *const u32) } };
        let regs: [(&str, usize); 5] = [
            ("PMC_ENABLE", 0x000200),
            ("PDISP_VGA_WORKSPACE_BASE", 0x625F04),
            ("BSI_SECURE_SCRATCH_14", 0x1180F8),
            ("PBUS_BAR0_WINDOW", 0x001700),
            ("PMC_BOOT_0", 0x000000),
        ];
        let mut s = String::new();
        for (name, off) in regs {
            let v = rd(off);
            let line = alloc::format!("[{}] preboot {} ({:#08x}) = {:#010x}", tag, name, off, v);
            log::error!("{}", line);
            let _ = writeln!(s, "{}", line);
        }
        let cmd = {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            use pci::Location;
            let loc = Location {
                bus: self.pci_bus,
                device: self.pci_device,
                function: 0,
            };
            unsafe { PCI_ACCESS.read16(&PortOpsImpl, loc, 0x04) }
        };
        let line = alloc::format!("[{}] preboot PCI COMMAND = {:#06x}", tag, cmd);
        log::error!("{}", line);
        let _ = writeln!(s, "{}", line);
        // PCIe link config of this GPU and its root port: MPS (Max Payload
        // Size) and MRRS (Max Read Request Size) from the PCIe capability's
        // Device Control register. Linux's PCI core NORMALIZES MPS across
        // every tree at boot; Eclipse inherits whatever UEFI programmed, and
        // the two GPUs hang off DIFFERENT root ports. A GPU-vs-root-port MPS
        // mismatch on the primary's port (absent on the secondary's) would
        // explain a TLP-level stall no driver knob can fix -- the top
        // remaining hypothesis now that every Linux-visible knob is matched.
        // Pure config reads, zero risk.
        {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            use pci::Location;
            let ops = &PortOpsImpl;
            let pcie_dump = |loc: Location, label: &str| -> String {
                // Walk the capability list for the PCIe capability (ID 0x10).
                let status = unsafe { PCI_ACCESS.read16(ops, loc, 0x06) };
                if status & (1 << 4) == 0 {
                    return alloc::format!("[{}] preboot PCIe {}: no cap list", tag, label);
                }
                let mut ptr = unsafe { PCI_ACCESS.read8(ops, loc, 0x34) } as u16;
                let mut hops = 0;
                while ptr != 0 && hops < 48 {
                    let id = unsafe { PCI_ACCESS.read8(ops, loc, ptr) };
                    if id == 0x10 {
                        let devcap = unsafe { PCI_ACCESS.read32(ops, loc, ptr + 0x04) };
                        let devctl = unsafe { PCI_ACCESS.read16(ops, loc, ptr + 0x08) };
                        let devsta = unsafe { PCI_ACCESS.read16(ops, loc, ptr + 0x0A) };
                        let lnksta = unsafe { PCI_ACCESS.read16(ops, loc, ptr + 0x12) };
                        // MPS/MRRS encode as 128 << field.
                        let mps_cap = 128u32 << (devcap & 0x7);
                        let mps = 128u32 << ((devctl >> 5) & 0x7);
                        let mrrs = 128u32 << ((devctl >> 12) & 0x7);
                        return alloc::format!(
                            "[{}] preboot PCIe {}: DevCtl={:#06x} (MPS={} MRRS={}, cap {}), DevSta={:#06x}, LnkSta={:#06x} (gen{} x{})",
                            tag, label, devctl, mps, mrrs, mps_cap, devsta, lnksta,
                            lnksta & 0xF,
                            (lnksta >> 4) & 0x3F
                        );
                    }
                    ptr = unsafe { PCI_ACCESS.read8(ops, loc, ptr + 1) } as u16;
                    hops += 1;
                }
                alloc::format!("[{}] preboot PCIe {}: cap not found", tag, label)
            };
            let gpu_loc = Location {
                bus: self.pci_bus,
                device: self.pci_device,
                function: 0,
            };
            let line = pcie_dump(gpu_loc, "GPU");
            log::error!("{}", line);
            let _ = writeln!(s, "{}", line);
            if let Some((b, d, f)) = self.find_parent_bridge() {
                let rp_loc = Location {
                    bus: b,
                    device: d,
                    function: f,
                };
                let line = pcie_dump(rp_loc, "root-port");
                log::error!("{}", line);
                let _ = writeln!(s, "{}", line);
            }
        }
        // Sysmem flush buffer target (kern_mem_sys_gm107.c programs it; a
        // zero/garbage value on one GPU would resurrect that theory).
        let flush = rd(0x100C10);
        let line = alloc::format!(
            "[{}] preboot PFB_NISO_FLUSH_SYSMEM_ADDR (0x100c10) = {:#010x}",
            tag,
            flush
        );
        log::error!("{}", line);
        let _ = writeln!(s, "{}", line);
        // Display liveness: the scanout theory REQUIRES the primary's heads
        // to be AWAKE with an advancing raster before it can explain the
        // wedge. NV_PDISP_FE_CORE_HEAD_STATE(i)=0x612078+i*2048, mode bits
        // 9:8 (0=SLEEP,1=SNOOZE,2=AWAKE, dev_disp.h v04_00:31-35);
        // NV_PDISP_RG_DPCA(i)=0x616330+i*2048 (v03_00 header -- read-only
        // probe, 0xBADFxxxx = not present at that offset on v04) read twice
        // ~30ms apart: FRM/LINE counters advancing = live raster fetch.
        // Gated on PMC_ENABLE bit30 (PDISP engine enabled) to avoid priv
        // errors on a display-less config.
        if rd(0x000200) & (1 << 30) != 0 {
            let mut dpca_a = [0u32; 4];
            for (i, slot) in dpca_a.iter_mut().enumerate() {
                *slot = rd(0x616330 + i * 2048);
            }
            // ~30ms spin so a live raster visibly advances its counters.
            let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };
            while unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0) < 30_000 {
                gpu_spin();
            }
            for i in 0..4usize {
                let head = rd(0x612078 + i * 2048);
                let mode = (head >> 8) & 0x3;
                let dpca_b = rd(0x616330 + i * 2048);
                let line = alloc::format!(
                    "[{}] preboot head{} STATE={:#010x} (mode={} {}) DPCA {:#010x} -> {:#010x} ({})",
                    tag,
                    i,
                    head,
                    mode,
                    match mode { 0 => "SLEEP", 1 => "SNOOZE", 2 => "AWAKE", _ => "?" },
                    dpca_a[i],
                    dpca_b,
                    if dpca_a[i] != dpca_b { "ADVANCING = live raster" } else { "frozen" }
                );
                log::error!("{}", line);
                let _ = writeln!(s, "{}", line);
            }
        } else {
            let line = alloc::format!(
                "[{}] preboot PDISP disabled in PMC_ENABLE (no head dump)",
                tag
            );
            log::error!("{}", line);
            let _ = writeln!(s, "{}", line);
        }
        s
    }

    /// Read-only discriminating dump for `/proc/gpudump`: labels the GPU by
    /// role (console vs secondary) and its PCI location, then the full
    /// register snapshot. Safe -- pure BAR0/config reads, no boot.
    fn hw_dump_impl(&self) -> String {
        let role = if self.drives_boot_display() {
            "CONSOLE/primary"
        } else {
            "secondary/headless"
        };
        let mut s = alloc::format!(
            "[gpudump] === {} GPU {:02x}:{:02x}.0 (bar0_phys={:#x} bar1_phys={:#x} vram={}MB) ===\n",
            role, self.pci_bus, self.pci_device, self.bar0_phys, self.bar1_phys, self.vram_size_mb
        );
        s.push_str(&self.dump_preboot_state("gpudump"));
        s
    }

    /// Packed config-space handle for THIS GPU (os_pci_init_handle format:
    /// valid-tag | bus<<16 | device<<8 | function).
    fn config_handle(&self) -> usize {
        0x8000_0000usize | ((self.pci_bus as usize) << 16) | ((self.pci_device as usize) << 8)
    }

    /// Packed config-space handle for the immediate upstream bridge, 0 if
    /// none found.
    fn parent_config_handle(&self) -> usize {
        self.find_parent_bridge()
            .map(|(b, d, f)| {
                0x8000_0000usize | ((b as usize) << 16) | ((d as usize) << 8) | f as usize
            })
            .unwrap_or(0)
    }

    /// Immediate upstream bridge (the one whose secondary bus IS this GPU's
    /// bus) -- the root port for a directly-attached GPU.
    fn find_parent_bridge(&self) -> Option<(u8, u8, u8)> {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use pci::Location;
        let ops = &PortOpsImpl;
        for bus in 0..=self.pci_bus {
            for dev in 0..32u8 {
                for func in 0..8u8 {
                    let loc = Location {
                        bus,
                        device: dev,
                        function: func,
                    };
                    let vend = unsafe { PCI_ACCESS.read16(ops, loc, 0x00) };
                    if vend == 0xFFFF {
                        // Config-space miss still touched 0xcf8/0xcfc under
                        // PIO_LOCK (IRQ-off). Pump so a shootdown peer is not
                        // starved across a full bus×dev×func walk.
                        lock::pump();
                        continue;
                    }
                    let hdr = unsafe { PCI_ACCESS.read8(ops, loc, 0x0E) };
                    if hdr & 0x7F != 0x01 {
                        lock::pump();
                        continue;
                    }
                    let sec = unsafe { PCI_ACCESS.read8(ops, loc, 0x19) };
                    if sec == self.pci_bus {
                        return Some((bus, dev, func));
                    }
                    lock::pump();
                }
            }
        }
        None
    }

    /// Containment: program the root port's PCIe Completion Timeout so a
    /// dead endpoint turns CPU reads into bounded all-ones completions
    /// instead of an unbounded core stall. Best-effort -- logs what it
    /// found; if the platform doesn't support CTO ranges (DevCap2[3:0]==0)
    /// nothing is written.
    fn arm_completion_timeout(&self) -> String {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use core::fmt::Write;
        use pci::Location;
        let mut s = String::new();
        let Some((b, d, f)) = self.find_parent_bridge() else {
            let _ = writeln!(s, "[gpustep11] CTO: no parent bridge found");
            return s;
        };
        let ops = &PortOpsImpl;
        let loc = Location {
            bus: b,
            device: d,
            function: f,
        };
        // Walk the capability list for the PCIe capability (ID 0x10).
        let mut ptr = unsafe { PCI_ACCESS.read8(ops, loc, 0x34) };
        let mut cap = 0u8;
        for _ in 0..16 {
            if ptr == 0 || ptr == 0xFF {
                break;
            }
            let id = unsafe { PCI_ACCESS.read8(ops, loc, ptr as u16) };
            if id == 0x10 {
                cap = ptr;
                break;
            }
            ptr = unsafe { PCI_ACCESS.read8(ops, loc, ptr as u16 + 1) };
        }
        if cap == 0 {
            let _ = writeln!(
                s,
                "[gpustep11] CTO: root port {:02x}:{:02x}.{} has no PCIe cap?",
                b, d, f
            );
            return s;
        }
        let devcap2 = unsafe { PCI_ACCESS.read32(ops, loc, cap as u16 + 0x24) };
        let ranges = devcap2 & 0xF;
        let dc2 = unsafe { PCI_ACCESS.read16(ops, loc, cap as u16 + 0x28) };
        if ranges == 0 {
            let _ = writeln!(
                s,
                "[gpustep11] CTO: root port {:02x}:{:02x}.{} supports no timeout ranges (DevCap2={:#010x}, DC2={:#06x}) -- containment unavailable",
                b, d, f, devcap2, dc2
            );
            return s;
        }
        // Pick the shortest supported range: A(bit0)->0b0001, B->0b0101,
        // C->0b1001, D->0b1101 (PCIe base spec encoding).
        let val: u16 = if ranges & 1 != 0 {
            0b0001
        } else if ranges & 2 != 0 {
            0b0101
        } else if ranges & 4 != 0 {
            0b1001
        } else {
            0b1101
        };
        let new_dc2 = (dc2 & !0x001F) | val; // clear CTO-disable (bit4) + set range
        unsafe { PCI_ACCESS.write16(ops, loc, cap as u16 + 0x28, new_dc2) };
        let _ = writeln!(
            s,
            "[gpustep11] CTO armed on root port {:02x}:{:02x}.{}: DevCap2={:#010x} DC2 {:#06x} -> {:#06x} (reads of a dead endpoint now complete all-ones instead of hanging, chipset permitting)",
            b, d, f, devcap2, dc2, new_dc2
        );
        s
    }

    /// Disable (or restore) legacy VGA routing on every PCI bridge between
    /// the root and this GPU -- PCI Bridge Control (offset 0x3E) bit 3
    /// "VGA Enable". Copilot/checklist item: the earlier experiment only
    /// cleared the GPU function's own I/O decode; the full chain includes
    /// the root port/bridges that forward VGA cycles. Returns the list of
    /// (bus, device, function, old bridge-control value) actually changed,
    /// for the caller to restore afterwards.
    fn set_path_vga_routing(
        &self,
        disable: bool,
        restore: &[(u8, u8, u8, u16)],
    ) -> (String, Vec<(u8, u8, u8, u16)>) {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use core::fmt::Write;
        use pci::Location;
        let ops = &PortOpsImpl;
        let mut changed: Vec<(u8, u8, u8, u16)> = Vec::new();
        let mut s = String::new();
        if disable {
            // Walk every bus below the GPU's: any bridge whose
            // [secondary..subordinate] window routes the GPU's bus is on the
            // path (covers nested switches too, not just the root port).
            for bus in 0..=self.pci_bus {
                for dev in 0..32u8 {
                    for func in 0..8u8 {
                        let loc = Location {
                            bus,
                            device: dev,
                            function: func,
                        };
                        let vend = unsafe { PCI_ACCESS.read16(ops, loc, 0x00) };
                        if vend == 0xFFFF {
                            lock::pump();
                            continue;
                        }
                        let hdr = unsafe { PCI_ACCESS.read8(ops, loc, 0x0E) };
                        if hdr & 0x7F != 0x01 {
                            lock::pump();
                            continue; // not a PCI-PCI bridge
                        }
                        let sec = unsafe { PCI_ACCESS.read8(ops, loc, 0x19) };
                        let sub = unsafe { PCI_ACCESS.read8(ops, loc, 0x1A) };
                        if !(sec <= self.pci_bus && self.pci_bus <= sub) {
                            lock::pump();
                            continue;
                        }
                        let bctl = unsafe { PCI_ACCESS.read16(ops, loc, 0x3E) };
                        if bctl & (1 << 3) != 0 {
                            unsafe { PCI_ACCESS.write16(ops, loc, 0x3E, bctl & !(1 << 3)) };
                            changed.push((bus, dev, func, bctl));
                            let _ = writeln!(
                                s,
                                "[gpustep11] bridge {:02x}:{:02x}.{} VGA routing disabled (BRIDGE_CTL {:#06x} -> {:#06x})",
                                bus, dev, func, bctl, bctl & !(1 << 3)
                            );
                        }
                    }
                }
            }
            if changed.is_empty() {
                let _ = writeln!(
                    s,
                    "[gpustep11] no bridge on the path had VGA routing enabled"
                );
            }
        } else {
            for &(bus, dev, func, old) in restore {
                let loc = Location {
                    bus,
                    device: dev,
                    function: func,
                };
                unsafe { PCI_ACCESS.write16(ops, loc, 0x3E, old) };
                let _ = writeln!(
                    s,
                    "[gpustep11] bridge {:02x}:{:02x}.{} VGA routing restored",
                    bus, dev, func
                );
            }
        }
        (s, changed)
    }

    /// Shared GSP-boot body used by `bringup_step6` (secondary GPU) and
    /// `bringup_step11` (console GPU, with the graphic console frozen by the
    /// /proc generator around this call): INTx mask, kgspInitRm, narration
    /// capture, per-GPU result cache. `tag` labels the output lines
    /// ("gpustep6"/"gpustep11") so each proc file reads naturally.
    /// PBUS PRI-error pre-boot diagnose + engine-level retire (console GPU).
    /// The workflow research identified the pre-STARTCPU pending LEAF[4] bit28
    /// (CPU vector 156, mirrored in legacy PMC_INTR0 bit 28) as PBUS -- the
    /// PRI (priv bus) error collector: nouveau maps legacy PMC bit 28 to
    /// NVKM_SUBDEV_BUS on this whole lineage, and its unit-level status is
    /// NV_PBUS_INTR_0 @ 0x1100 (PRI_SQUASH bit1 / PRI_FECSERR bit2 /
    /// PRI_TIMEOUT bit3), with the FAULTING PRI ADDRESS latched in 0x9084 and
    /// the write data in 0x9088 (nouveau gf100_bus_intr, valid through Turing:
    /// tu102 uses gf100_bus in non-GSP nouveau). A leaf W1C can never retire
    /// it -- the level line follows the unit latch, which is why EXP3's leaf
    /// clear re-asserted "within microseconds". nouveau's documented quench:
    /// read 0x9084/0x9088 for diagnosis, write 0x9084=0, then W1C the handled
    /// bits into 0x1100. Runs BEFORE kgspInitRm with full logging (safe: no
    /// SEC2 resume in flight), so one boot both names the original PRI fault
    /// (smoking gun for WHY only the GOP/primary GPU has it latched) and
    /// retires the level source for real instead of racing it.
    fn pbus_pri_diagnose_and_clear(&self, tag: &str) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let bar0 = self._bar0;
        let rd = |off: usize| unsafe { core::ptr::read_volatile((bar0 + off) as *const u32) };
        let wr =
            |off: usize, v: u32| unsafe { core::ptr::write_volatile((bar0 + off) as *mut u32, v) };
        // Looks like the 0xBADFxxxx PRI-error sentinel (register absent /
        // priv fault)? Never write to a register that read back as one.
        let is_badf = |v: u32| (v & 0xFFFF_0000) == 0xBADF_0000;

        let intr0 = rd(0x1100);
        let save0 = rd(0x9084);
        let save1 = rd(0x9088);
        // LR10-lineage candidates (the only offsets published in this tree);
        // reads may themselves fault (sentinel) -- we clear PBUS right after,
        // so a probe-induced PRI_TIMEOUT is retired too.
        let lr_save0 = rd(0x1984);
        let lr_save1 = rd(0x1988);
        let lr_errc = rd(0x198C);
        let leaf4 = rd(0x00B8_1010);
        let pmc0 = rd(0x100);
        let _ = writeln!(
            s,
            "[{}] PBUS pre-boot: INTR_0={:#010x} (SQUASH={} FECSERR={} TIMEOUT={}) SAVE_0(0x9084)={:#010x} SAVE_1(0x9088)={:#010x}",
            tag,
            intr0,
            (intr0 >> 1) & 1,
            (intr0 >> 2) & 1,
            (intr0 >> 3) & 1,
            save0,
            save1
        );
        if save0 != 0 && !is_badf(save0) {
            let _ = writeln!(
                s,
                "[{}] PBUS latched PRI fault: {} of data {:#010x} at PRI address {:#08x}",
                tag,
                if save0 & 0x2 != 0 { "WRITE" } else { "READ" },
                save1,
                save0 & 0x00FF_FFFC
            );
        }
        let _ = writeln!(
            s,
            "[{}] PBUS alt regs: 0x1984={:#010x} 0x1988={:#010x} 0x198C={:#010x}; LEAF[4]={:#010x} PMC_INTR0={:#010x}",
            tag, lr_save0, lr_save1, lr_errc, leaf4, pmc0
        );
        log::error!("{}", s.trim_end());
        // Retire at the unit: clear the fault latch, then W1C the status.
        if !is_badf(save0) {
            wr(0x9084, 0);
        }
        // 0x1984 (LR10-lineage SAVE_0) stays READ-ONLY: that offset is only
        // published for NVSwitch/LR10 and was never verified on Turing -- a
        // blind write could hit an unrelated register. The nouveau-documented
        // Turing clear path (0x9084=0 + W1C 0x1100) above already covers it.
        let intr0_now = rd(0x1100);
        if intr0_now != 0 && !is_badf(intr0_now) {
            wr(0x1100, intr0_now);
        }
        // Leaf W1C after the unit clear, then verify the level line dropped.
        let leaf4_mid = rd(0x00B8_1010);
        if leaf4_mid != 0 && !is_badf(leaf4_mid) {
            wr(0x00B8_1010, leaf4_mid);
        }
        let leaf4_after = rd(0x00B8_1010);
        let pmc0_after = rd(0x100);
        let intr0_after = rd(0x1100);
        let verdict = if leaf4_after & 0x1000_0000 == 0 && pmc0_after & 0x1000_0000 == 0 {
            "RETIRED (level source quenched at the unit)"
        } else {
            "STILL PENDING (source is not PBUS-latch-only; see values)"
        };
        let tail = alloc::format!(
            "[{}] PBUS after clear: INTR_0={:#010x} LEAF[4]={:#010x} PMC_INTR0={:#010x} -> {}",
            tag,
            intr0_after,
            leaf4_after,
            pmc0_after,
            verdict
        );
        log::error!("{}", tail);
        let _ = writeln!(s, "{}", tail);
        s
    }

    /// Find the PCIe capability (ID 0x10) offset in config space, or None.
    fn pcie_cap_ptr(loc: pci::Location) -> Option<u16> {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        let ops = &PortOpsImpl;
        let status = unsafe { PCI_ACCESS.read16(ops, loc, 0x06) };
        if status & (1 << 4) == 0 {
            return None;
        }
        let mut ptr = unsafe { PCI_ACCESS.read8(ops, loc, 0x34) } as u16;
        let mut hops = 0;
        while ptr != 0 && hops < 48 {
            let id = unsafe { PCI_ACCESS.read8(ops, loc, ptr) };
            if id == 0x10 {
                return Some(ptr);
            }
            ptr = unsafe { PCI_ACCESS.read8(ops, loc, ptr + 1) } as u16;
            hops += 1;
        }
        None
    }

    /// PCIe MPS normalization -- Eclipse's equivalent of Linux's
    /// pcie_bus_config tree walk, applied to the one link that matters.
    /// ROOT CAUSE (gpudump on real hardware): UEFI left BOTH GPUs with
    /// DevCtl MPS=256 while BOTH root ports sit at MPS=128 -- a protocol
    /// violation. A GPU-sourced upstream TLP with a >128-byte payload is a
    /// Malformed TLP at the root port; with no OS AER handling the port stops
    /// releasing flow-control credits and every subsequent access through it
    /// stalls -- the CPU then wedges on its next posted write, which is
    /// EXACTLY the observed physics (the Linux-byte-parity bare store to
    /// STARTCPU wedged; every driver-level knob had been equalized). The
    /// secondary GPU survives because its display-less SEC2-HS resume never
    /// generates such bursts. Linux never sees any of this because its PCI
    /// core normalizes MPS across every tree at boot -- the one Linux
    /// behavior Eclipse hadn't replicated. Clamp the GPU's MPS down to the
    /// root port's (lowering is always protocol-safe); MRRS is left alone
    /// (mismatched MRRS is legal -- completers split completions).
    fn normalize_mps(&self, tag: &str) -> String {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use pci::Location;
        let ops = &PortOpsImpl;
        let gpu_loc = Location {
            bus: self.pci_bus,
            device: self.pci_device,
            function: 0,
        };
        let Some((b, d, f)) = self.find_parent_bridge() else {
            return alloc::format!(
                "[{}] MPS normalize: no parent bridge found (skipped)\n",
                tag
            );
        };
        let rp_loc = Location {
            bus: b,
            device: d,
            function: f,
        };
        let (Some(gpu_cap), Some(rp_cap)) =
            (Self::pcie_cap_ptr(gpu_loc), Self::pcie_cap_ptr(rp_loc))
        else {
            return alloc::format!("[{}] MPS normalize: PCIe cap not found (skipped)\n", tag);
        };
        let gpu_devctl = unsafe { PCI_ACCESS.read16(ops, gpu_loc, gpu_cap + 0x08) };
        let rp_devctl = unsafe { PCI_ACCESS.read16(ops, rp_loc, rp_cap + 0x08) };
        let gpu_mps = (gpu_devctl >> 5) & 0x7;
        let rp_mps = (rp_devctl >> 5) & 0x7;
        if gpu_mps <= rp_mps {
            return alloc::format!(
                "[{}] MPS already consistent (GPU {} <= root-port {}); nothing to do\n",
                tag,
                128u32 << gpu_mps,
                128u32 << rp_mps
            );
        }
        let new_devctl = (gpu_devctl & !(0x7 << 5)) | (rp_mps << 5);
        unsafe { PCI_ACCESS.write16(ops, gpu_loc, gpu_cap + 0x08, new_devctl) };
        let rb = unsafe { PCI_ACCESS.read16(ops, gpu_loc, gpu_cap + 0x08) };
        let line = alloc::format!(
            "[{}] MPS NORMALIZED (Linux pcie_bus_config equivalent): GPU DevCtl {:#06x} -> {:#06x} (readback {:#06x}); MPS {} -> {} to match root-port {}\n",
            tag,
            gpu_devctl,
            new_devctl,
            rb,
            128u32 << gpu_mps,
            128u32 << ((new_devctl >> 5) & 0x7),
            128u32 << rp_mps
        );
        log::error!("{}", line.trim_end());
        line
    }

    /// Secondary-bus-reset recovery after a detected post-STARTCPU fabric
    /// wedge (see os_boundary's wedge containment). All bridge/GPU accesses
    /// here are CONFIG space (root-complex-completed, can't hang the core)
    /// until the device answers config again; only then is fake-MMIO cleared
    /// and one BAR0 probe attempted. Returns (recovered, log).
    fn sbr_recover(&self, tag: &str, attempt: u32) -> (bool, String) {
        use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
        use core::fmt::Write;
        use pci::Location;
        let mut s = String::new();
        let ops = &PortOpsImpl;
        let _ = writeln!(
            s,
            "[{}] WEDGE DETECTED after STARTCPU (config space went all-ones); machine kept ALIVE; secondary-bus-reset recovery attempt #{}",
            tag, attempt
        );
        let Some((bb, bd, bf)) = self.find_parent_bridge() else {
            let _ = writeln!(s, "[{}] recovery: no parent bridge found; cannot SBR", tag);
            return (false, s);
        };
        let bridge = Location {
            bus: bb,
            device: bd,
            function: bf,
        };
        let gpu = Location {
            bus: self.pci_bus,
            device: self.pci_device,
            function: 0,
        };
        let spin_ms = |ms: u64| {
            let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };
            while unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0) < ms * 1000
            {
                gpu_spin();
            }
        };
        unsafe {
            let bc = PCI_ACCESS.read16(ops, bridge, 0x3E);
            PCI_ACCESS.write16(ops, bridge, 0x3E, bc | 0x40); // Secondary Bus Reset
            spin_ms(5);
            PCI_ACCESS.write16(ops, bridge, 0x3E, bc);
        }
        spin_ms(250); // link retrain + device-ready time
                      // Restore the GPU's config: BARs (address bits; RO flag bits are
                      // ignored by the device), COMMAND (MEM+BME, INTx masked). The GPU's
                      // ROM-based GFW/IFR re-runs its own boot after a hot reset;
                      // kgspInitRm's kgspWaitForGfwBootOk then waits for it like on a
                      // cold boot (the vfio/VM-passthrough flow relies on exactly this).
        unsafe {
            PCI_ACCESS.write32(ops, gpu, 0x10, self.bar0_phys as u32);
            PCI_ACCESS.write32(ops, gpu, 0x14, self.bar1_phys as u32);
            PCI_ACCESS.write32(ops, gpu, 0x18, (self.bar1_phys >> 32) as u32);
            PCI_ACCESS.write32(ops, gpu, 0x1C, self.bar2_phys as u32);
            PCI_ACCESS.write32(ops, gpu, 0x20, (self.bar2_phys >> 32) as u32);
            PCI_ACCESS.write16(ops, gpu, 0x04, 0x0406);
        }
        s.push_str(&self.normalize_mps(tag));
        let id = unsafe { PCI_ACCESS.read32(ops, gpu, 0x00) };
        let _ = writeln!(s, "[{}] recovery: post-SBR config ID = {:#010x}", tag, id);
        if id & 0xFFFF != 0x10DE {
            let _ = writeln!(
                s,
                "[{}] recovery FAILED (no config answer); console rendering stays suppressed -- capture this /proc output to a file and reboot",
                tag
            );
            return (false, s);
        }
        // Device answers config again: clear fake-MMIO and risk ONE BAR0
        // probe (without it no retry is possible anyway).
        nvidia_rm_sys::os_boundary::wedge_fake_mmio_clear();
        let boot0 = unsafe { core::ptr::read_volatile(self._bar0 as *const u32) };
        let _ = writeln!(
            s,
            "[{}] recovery: BAR0 PMC_BOOT_0 = {:#010x}; re-enabling console rendering, retrying GSP boot",
            tag, boot0
        );
        nvidia_rm_sys::os_interface::console_quiet_end();
        log::error!("{}", s.trim_end());
        (true, s)
    }

    fn gsp_boot_run(&self, tag: &str, quiet: bool) -> String {
        use core::fmt::Write;
        let device_instance = *self.rm_device_instance.lock();

        // Check the cache before touching gsp_firmware's lock at all, so
        // the two locks are never nested across the FFI call below (same
        // reasoning as bringup_step5).
        let cached = self.gsp_init_result.lock().clone();

        // Cache the ENTIRE block (captured GSP-RM boot narration + result
        // line) so the /proc generator is idempotent across cat's chunked
        // reads -- same requirement (and same fix) as bringup_step5.
        if let Some(cached) = cached {
            cached
        } else if let Some(device_instance) = device_instance {
            let fw = self.gsp_firmware.lock();
            if let Some(fw_bytes) = fw.as_ref() {
                // Snapshot the pre-boot hardware state first (diffable
                // primary-vs-secondary; survives a wedge via the live echo).
                let preboot = self.dump_preboot_state(tag);
                // Normalize this GPU's PCIe MPS to its root port BEFORE any
                // GSP traffic -- the root-cause fix (see normalize_mps).
                let mps_log = self.normalize_mps(tag);
                // Mask this GPU's legacy INTx at the PCI level before booting
                // GSP-RM. On real hardware the boot now gets all the way to
                // "GSP FW RM ready." and THEN the machine livelocks: once
                // GSP-RM is alive it asserts interrupts (RPC completions, log
                // buffers, NOCAT posts), and Eclipse has no ISR for the GPU --
                // nobody acks or masks a level-triggered INTx, so it screams
                // and starves the CPU. Linux never sees this because the RM
                // registers its ISR before RmInitAdapter. Eclipse's bring-up
                // is 100% polled (the RPC message queue is read directly), so
                // the correct equivalent is to keep the device's INTx
                // disabled: PCI COMMAND register (offset 4) bit 10 (Interrupt
                // Disable), the standard way a polled driver quiesces a
                // function. MSI/MSI-X were never enabled, so INTx is the only
                // line it can raise.
                {
                    use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
                    use pci::Location;
                    let loc = Location {
                        bus: self.pci_bus,
                        device: self.pci_device,
                        function: 0,
                    };
                    let ops = &PortOpsImpl;
                    let cmd = unsafe { PCI_ACCESS.read16(ops, loc, 0x04) };
                    unsafe { PCI_ACCESS.write16(ops, loc, 0x04, cmd | (1 << 10)) };
                    log::warn!(
                        "[NVIDIA] {}: PCI INTx disabled before GSP boot (COMMAND {:#06x} -> {:#06x})",
                        tag,
                        cmd,
                        cmd | (1 << 10)
                    );
                }
                // Capture kgspInitRm's own nv_printf / assert / ECLIPSE_TRACE
                // narration -- the GSP boot is the deepest step and its RM
                // LEVEL_ERROR failure lines only reach the user folded in
                // here (the kernel log::warn! stream is invisible on the
                // bring-up box; see bringup_step5).
                nvidia_rm_sys::os_interface::capture_begin();
                // Arm the sequencer register trace for EVERY GSP boot: it goes
                // live at the RUN_CPU_SEQUENCER RPC and records each register
                // access into the capture buffer (readable later in this
                // /proc file) -- and onto the live screen too when live_echo
                // is armed (step 11's console-GPU boot). The successful
                // secondary boot thus yields a full reference sequence to
                // diff against the console GPU's wedge point.
                nvidia_rm_sys::os_boundary::seq_trace_arm();
                // Console-GPU SEC2-resume mitigation stack. History: the
                // primary GPU (live GOP scanout) wedged at the SEC2 STARTCPU
                // posted write with CPU vector 156 (LEAF[4] bit28, mirrored in
                // legacy PMC_INTR0 bit28) pending-but-masked. Research
                // identified that source as PBUS (the PRI-error collector; a
                // PRI fault sits latched in NV_PBUS_INTR_0 until cleared at
                // the unit -- leaf W1C can never retire it, hence EXP3's
                // "re-asserts within microseconds"). It is retired for real by
                // pbus_pri_diagnose_and_clear() below, BEFORE the boot. The
                // pre-STARTCPU leaf drain stays armed as belt-and-braces (it
                // correlated with the one lucky pre-fix success), and the real
                // Linux-faithful fix is the console-quiet window (see `quiet`):
                // prior-art (nouveau r535 / RM / nova-core) does the STARTCPU
                // write unconditionally -- what they all ALSO do, and we
                // didn't, is never touch the console framebuffer (this GPU's
                // BAR1!) during the boot. The secondary/headless GPU needs
                // none of this.
                let drain_for_console = self.drives_boot_display();
                if drain_for_console {
                    if quiet {
                        // Linux byte-parity: the STARTCPU bracket contributes
                        // ZERO extra MMIO (no BSI pre-read, no intr snapshot,
                        // no drain) -- stock kflcnStartCpu is read CPUCTL then
                        // write CPUCTL_ALIAS with nothing between. The
                        // console-silent + PBUS-clean run still wedged, and
                        // the one successful boot ran WITHOUT the display/
                        // priv-ring probe reads the snapshot later added, so
                        // our own in-window MMIO is the prime remaining
                        // suspect. step13 (loud) keeps the drain+diagnostics.
                        nvidia_rm_sys::os_boundary::linux_parity_arm();
                    } else {
                        nvidia_rm_sys::os_boundary::sec2_drain_arm();
                    }
                }
                // Console GPU: diagnose + retire the latched PBUS PRI error at
                // the unit BEFORE the boot (fully logged -- no SEC2 window in
                // flight yet), so the LEAF[4] bit28 level source is quenched
                // for real instead of raced at STARTCPU time. See the method's
                // doc comment for the research trail.
                let pbus_log = if drain_for_console {
                    self.pbus_pri_diagnose_and_clear(tag)
                } else {
                    String::new()
                };
                // Console-quiet window (Linux console_lock equivalent) when the
                // caller asked for it: the console framebuffer lives in THIS
                // GPU's BAR1, and every prior console-GPU boot interleaved live
                // seq-trace pixel writes with the sequencer MMIO -- the one
                // thing Linux explicitly forbids around kgspInitRm
                // (osinit.c:1841: "to ensure no console writes through BAR1
                // can interfere"). Everything is still captured and folded
                // into this /proc read afterwards; only live rendering stops.
                if quiet && drain_for_console {
                    log::error!(
                        "[NVIDIA] {}: entering console-silent GSP boot window (Linux console_lock equivalent) -- next render after kgspInitRm returns",
                        tag
                    );
                    nvidia_rm_sys::os_interface::console_quiet_begin();
                }
                // Arm the post-STARTCPU wedge watch (console GPU only): if
                // the fabric dies, the machine survives with fake-MMIO and
                // we get to attempt SBR recovery + retry -- converting the
                // ~25-30% per-boot race into up to 3 chances per boot.
                if drain_for_console {
                    nvidia_rm_sys::os_boundary::wedge_watch_arm(self.config_handle());
                    // GPU-independent survival breadcrumb: mark that a console
                    // boot began and zero the RM narration counter, so a wedge
                    // is legible next boot via /proc/gpusurvive even if nothing
                    // else survives (no serial, no /proc, dark framebuffer).
                    nvidia_rm_sys::survival::reset_narration();
                    nvidia_rm_sys::survival::checkpoint(
                        nvidia_rm_sys::survival::milestone::INITRM_CALL,
                    );
                }
                // Linux-faithful interrupt path: bring the GPU's MSI delivery
                // online for the SEC2-resume window instead of running fully
                // INTx-masked. The wedge (foto 1) is the STARTCPU posted store
                // stalling with every CPU-visible interrupt source already
                // clean — consistent with the SEC2/GSP needing an interrupt
                // *delivered* (posted to the LAPIC) for forward progress, which
                // a fully INTx-masked GPU can never provide. The ISR closure
                // must NOT touch BAR0 (a CPU->GPU access wedges in the window):
                // it only counts and self-limits, since the mere MSI delivery
                // (outbound GPU->LAPIC) is the forward-progress signal and the
                // IRQ framework EOIs after it returns.
                let msi_vec = if drain_for_console {
                    self.msi_vector.load(Ordering::Relaxed)
                } else {
                    usize::MAX
                };
                if msi_vec != usize::MAX {
                    nvidia_rm_sys::survival::msi_set_online(msi_vec);
                    let v = msi_vec;
                    let handler: crate::scheme::IrqHandler = alloc::sync::Arc::new(move || {
                        const STORM_CAP: usize = 200_000;
                        // Counter lives in nvidia-rm-sys so the STARTCPU bracket
                        // (os_boundary) can print it on the frozen screen.
                        if nvidia_rm_sys::survival::msi_tick() == STORM_CAP {
                            // Runaway source we cannot clear at the engine
                            // without a wedge-prone BAR access: self-mask so a
                            // storm can never peg the CPU into a hang.
                            crate::net::msi_mask(v);
                        }
                    });
                    let ok = crate::net::msi_register_and_unmask(msi_vec, handler);
                    log::error!(
                        "[NVIDIA] {}: MSI delivery ONLINE for the GSP boot (vector {}, registered={})",
                        tag,
                        msi_vec,
                        ok
                    );
                } else {
                    log::error!(
                        "[NVIDIA] {}: NO MSI vector on this GPU (msi_vector unset) -- GSP boot runs INTx-masked as before; pci.rs found no legacy MSI cap (0x05). NVIDIA may expose only MSI-X (0x11).",
                        tag
                    );
                }
                let mut recovery_log = String::new();
                let mut attempt = 1u32;
                let computed = loop {
                    match nvidia_rm_sys::rm_init::init_gsp(device_instance, fw_bytes) {
                        Ok(()) => break String::from("kgspInitRm OK"),
                        Err(status) => {
                            let msg = alloc::format!(
                                "kgspInitRm FAILED, NV_STATUS={:#x} (attempt {})",
                                status,
                                attempt
                            );
                            if drain_for_console
                                && nvidia_rm_sys::os_boundary::wedge_detected()
                                && attempt < 3
                            {
                                let (recovered, rlog) = self.sbr_recover(tag, attempt);
                                recovery_log.push_str(&rlog);
                                if recovered {
                                    attempt += 1;
                                    nvidia_rm_sys::os_boundary::sec2_drain_arm();
                                    nvidia_rm_sys::os_boundary::seq_trace_arm();
                                    nvidia_rm_sys::os_boundary::wedge_watch_arm(
                                        self.config_handle(),
                                    );
                                    continue;
                                }
                            }
                            break msg;
                        }
                    }
                };
                nvidia_rm_sys::os_boundary::wedge_watch_disarm();
                if drain_for_console {
                    // Past kgspInitRm (OK or a clean NV_STATUS) — so any freeze
                    // recorded from here on was NOT the SEC2-window wedge.
                    nvidia_rm_sys::survival::checkpoint(
                        nvidia_rm_sys::survival::milestone::INITRM_RETURN,
                    );
                }
                // Take the GPU's MSI delivery back offline and report how many
                // MSIs were serviced during the boot — the empirical answer to
                // "do MSIs even fire, and does turning them on shift the wedge?".
                if msi_vec != usize::MAX {
                    crate::net::msi_mask_and_unregister(msi_vec);
                    let (_v, n) = nvidia_rm_sys::survival::msi_status();
                    nvidia_rm_sys::survival::msi_offline();
                    log::error!(
                        "[NVIDIA] {}: MSI delivery offline; {} MSI(s) serviced during the GSP boot{}",
                        tag,
                        n,
                        if n >= 200_000 {
                            " (STORM CAP hit -- source was masked mid-boot)"
                        } else {
                            ""
                        }
                    );
                }
                if quiet && drain_for_console {
                    nvidia_rm_sys::os_interface::console_quiet_end();
                    log::error!(
                        "[NVIDIA] {}: console-silent window exited (kgspInitRm returned)",
                        tag
                    );
                }
                if drain_for_console {
                    nvidia_rm_sys::os_boundary::linux_parity_disarm();
                    nvidia_rm_sys::os_boundary::sec2_drain_disarm();
                }
                nvidia_rm_sys::os_boundary::seq_trace_disarm();
                let captured = nvidia_rm_sys::os_interface::capture_take();
                drop(fw);
                let mut block = String::new();
                block.push_str(&preboot);
                block.push_str(&mps_log);
                block.push_str(&pbus_log);
                block.push_str(&recovery_log);
                if nvidia_rm_sys::os_boundary::wedge_fake_mmio_on() {
                    let _ = writeln!(
                        block,
                        "[{}] NOTE: fabric wedge unrecovered -- console rendering suppressed to keep the machine alive; this /proc output is intact (capture to a file + sync), then reboot.",
                        tag
                    );
                }
                if let Some(log) = captured {
                    if !log.is_empty() {
                        let _ = writeln!(block, "[{}]  --- GSP-RM narration (captured) ---", tag);
                        for line in log.lines() {
                            let _ = writeln!(block, "[{}]  | {}", tag, line);
                        }
                        let _ = writeln!(block, "[{}]  --- end GSP-RM narration ---", tag);
                    }
                }
                let _ = writeln!(block, "[{}]  --- Real GSP-RM boot: {} ---", tag, computed);
                let mut gsp = self.gsp_init_result.lock();
                if gsp.is_none() {
                    *gsp = Some(block.clone());
                }
                block
            } else {
                let status = self
                    .gsp_fw_status
                    .lock()
                    .clone()
                    .unwrap_or_else(|| String::from("no status recorded (loader never ran?)"));
                alloc::format!(
                    "[{}]  --- Real GSP-RM boot: skipped (no gsp.bin in driver) ---\n\
                     [{}]  boot-time firmware load: {}\n",
                    tag,
                    tag,
                    status
                )
            }
        } else {
            alloc::format!(
                "[{}]  --- Real GSP-RM boot: skipped (run /proc/gpustep5 (RM attach) first) ---\n",
                tag
            )
        }
    }

    /// Issue the tu102 GMMU invalidate for our channel's PDB and poll for
    /// completion. Returns `(pre, post, ok)` — the trigger register before and
    /// after, and whether bit31 cleared. Aborts (no write) if a flush is already
    /// in flight. This is the only GPU register write of Step 2.
    /// CPU-write a u32 into VRAM at raw VRAM offset `vram_off` via the PRAMIN
    /// window: point the window base (BAR0+0x1700 = off>>16), then access
    /// BAR0+0x700000+(off&0xFFFF). The window is 64 KiB; we re-point per access
    /// for simplicity. This is how the CPU reaches instmem (BAR1 is GMMU-remapped
    /// and cannot give a known VRAM-physical address).
    fn pramin_w32(&self, vram_off: u64, val: u32) {
        let bar0 = self._bar0;
        unsafe {
            core::ptr::write_volatile((bar0 + 0x1700) as *mut u32, (vram_off >> 16) as u32);
            core::ptr::write_volatile(
                (bar0 + 0x0070_0000 + (vram_off & 0xFFFF) as usize) as *mut u32,
                val,
            );
        }
    }

    fn pramin_r32(&self, vram_off: u64) -> u32 {
        let bar0 = self._bar0;
        unsafe {
            core::ptr::write_volatile((bar0 + 0x1700) as *mut u32, (vram_off >> 16) as u32);
            core::ptr::read_volatile(
                (bar0 + 0x0070_0000 + (vram_off & 0xFFFF) as usize) as *const u32,
            )
        }
    }

    fn pramin_zero(&self, vram_off: u64, len: usize) {
        for i in (0..len).step_by(4) {
            self.pramin_w32(vram_off + i as u64, 0);
        }
    }

    /// Write the channel instance block into VRAM (via PRAMIN). The host reads it
    /// as VRAM-physical. The PD-base at 0x200 points at the *sysmem* page tables
    /// (target=2). USERD pointer is VRAM-physical; the GPFIFO base is a GPU VA
    /// (GMMU-translated). Offsets per nouveau gv100_vmm_join / ramfc_write.
    /// Write the Turing VER2 PDB join (gv100_vmm_join) into a VRAM instance
    /// block via PRAMIN: PD-base @0x200, VA limit @0x208, and the 0x2a0
    /// subcontext descriptor table (entry 0 = real PDB, 1..63 = 0x1/0x1/0).
    /// Shared by the channel and BAR2 instance blocks. Assumes already zeroed.
    fn write_pdb_join_vram(&self, inst: u64, root_phys: u64) {
        let w32 = |off: u64, v: u32| self.pramin_w32(inst + off, v);
        let base = gmmu::inst_pd_base(root_phys); // root | 0xC06 (sysmem target)
        w32(0x200, base as u32);
        w32(0x204, (base >> 32) as u32);
        w32(0x208, ((1u64 << 49) - 1) as u32);
        w32(0x20c, (((1u64 << 49) - 1) >> 32) as u32);
        w32(0x21c, 0);
        w32(0x2a0, base as u32);
        w32(0x2a4, (base >> 32) as u32);
        w32(0x2a8, 0);
        for i in 1..64u64 {
            let o = 0x2a0 + i * 0x10;
            w32(o, 0x1);
            w32(o + 4, 0x1);
            w32(o + 8, 0);
        }
        w32(0x298, 0x1);
        w32(0x29c, 0x0);
    }

    fn write_instance_block_vram(&self, b: &GpuBringup) {
        let inst = b.inst_vram();
        self.pramin_zero(inst, 0x1000);
        let w32 = |off: u64, v: u32| self.pramin_w32(inst + off, v);
        // PD-base + VA limit + Turing PDB descriptor table.
        self.write_pdb_join_vram(inst, b.root.paddr() as u64);
        // RAMFC: USERD (VRAM phys), GPFIFO (GPU VA), ids.
        let userd = b.userd_vram();
        let gpfifo_va = b.gpfifo_va();
        let limit2 = (b.gpfifo.byte_len() as u64 / 8).trailing_zeros();
        w32(0x008, userd as u32);
        w32(0x00c, (userd >> 32) as u32);
        w32(0x010, 0x0000_face);
        w32(0x030, 0x7fff_f902);
        w32(0x048, gpfifo_va as u32);
        w32(0x04c, ((gpfifo_va >> 32) as u32) | (limit2 << 16));
        w32(0x084, 0x2040_0000);
        w32(0x094, 0x3000_0000 | 0xfff);
        // Fetched the real source (nvkm subdev/fifo/gv100.c, gv100_chan_ramfc):
        //   const struct nvkm_chan_func_ramfc gv100_chan_ramfc = {
        //       .write = gv100_chan_ramfc_write, .devm = 0xfff, .priv = true,
        //   };
        // `priv` is a FIXED property of the ramfc func table for this chip
        // generation, not a per-channel choice — EVERY gv100/tu102 channel
        // (client or kernel) uses priv=true. A previous commit here reasoned
        // priv should be false for a "normal client channel" and set
        // 0x0e4=0/0x0f4=0x1000; that directly contradicts the real source,
        // which always writes 0x0e4=(priv?0x20:0)=0x20 and
        // 0x0f4=0x1000|(priv?0x100:0)=0x1100 for this ramfc variant. Fixing
        // to match verbatim.
        w32(0x0e4, 0x0000_0020);
        w32(0x0e8, 0x0000_0000); // chan_id 0
        w32(0x0f4, 0x0000_1100);
        w32(0x0f8, 0x1000_3080);
        // CE/GR engine-context pointers (0x210-0x224, arm bits 0x10000/0x20000
        // at 0x0ac) are left ZERO: HOST never reads them during channel load —
        // only the engine does, on a faulting method, which never happens before
        // GP_GET advances. Arming a CE context with a BAR2 pointer is a red
        // herring for the load-time fault, so we bring up HOST first.
    }

    /// Arm the HUB MMU non-replayable fault buffer (buffer 0) so the host will
    /// schedule channels. NV_VIRTUAL_FUNCTION_PRIV_MMU_FAULT_BUFFER at 0xb83000:
    /// LO = addr|aperture|mode, HI = addr_hi, SIZE = count|ENABLE. We use
    /// PHYSICAL mode + SYS_COH aperture so the buffer is plain sysmem (no BAR2).
    /// Returns (hw_count, lo, hi, size) for reporting.
    fn setup_fault_buffer(&self, b: &GpuBringup) -> (u32, u32, u32, u32) {
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let wr = |off: u32, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off as usize) as *mut u32, v)
        };
        // Latch + read the HW-reported entry count (set bit30, clear ENABLE).
        wr(0x00b8_3010, (rd(0x00b8_3010) & !0xc000_0000) | 0x4000_0000);
        let hw_count = rd(0x00b8_3010) & 0x000f_ffff;
        // Our buffer holds at most 0x40000/32 = 0x2000 entries.
        let cap = (b.fault_buf.byte_len() / 32) as u32;
        let count = hw_count.min(cap);
        let phys = b.fault_buf.paddr() as u64;
        // LO: PHYSICAL(bit0=1) | PHYS_APERTURE SYS_COH(2<<1) | VOL(1<<3) | ADDR.
        let lo = (phys as u32 & 0xffff_f000) | 0x1 | (2 << 1) | (1 << 3);
        wr(0x00b8_3004, (phys >> 32) as u32);
        wr(0x00b8_3000, lo);
        // SIZE: entry count + ENABLE(bit31).
        wr(0x00b8_3010, count | 0x8000_0000);
        (hw_count, lo, (phys >> 32) as u32, rd(0x00b8_3010))
    }

    /// Set BAR2 live so the host can dereference the CE fault-method-buffer
    /// pointer (read by the BAR2 MMU as engine_id=BAR2/client=HOST_CPU). The
    /// BAR2 instance block (VRAM, via PRAMIN) points at the SAME page tables as
    /// the channel, so BAR2 VA == channel VA. Register per tu102_bar_bar2_init.
    fn setup_bar2(&self, b: &GpuBringup) -> (u32, u32, u32) {
        // Build the BAR2 instance block in VRAM with the FULL Turing VER2 PDB
        // join (PD-base + VA limit + the 0x2a0 descriptor table), same as a
        // channel — on Turing even a BAR vmm uses the VER2 join. Shared root.
        let bi = b.bar2_inst_vram();
        self.pramin_zero(bi, 0x1000);
        self.write_pdb_join_vram(bi, b.root.paddr() as u64);

        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let wr = |off: u32, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off as usize) as *mut u32, v)
        };
        let before = rd(0x00b8_0f48);
        // 0xb80f48 = 0x80000000 | (bar2_inst_vram >> 12).
        wr(0x00b8_0f48, 0x8000_0000 | (bi >> 12) as u32);
        let after = rd(0x00b8_0f48);
        // Wait for the BAR2 bind to settle (0xb80f50 bits 0xc).
        let mut wait = 0;
        for _ in 0..1_000_000u64 {
            wait = rd(0x00b8_0f50);
            if wait & 0x0000_000c == 0 {
                break;
            }
            gpu_spin();
        }
        (before, after, wait)
    }

    /// Write the runlist into VRAM (via PRAMIN): cgrp entry + chan entry. The
    /// USERD/inst pointers in the chan entry are VRAM-physical. Per nouveau
    /// gv100_runl_insert_cgrp/chan (chan_id=0, cgrp_id=0, chan_nr=1, runq=0).
    fn write_runlist_vram(&self, b: &GpuBringup) {
        let rl = b.runlist_vram();
        self.pramin_zero(rl, 0x20);
        let w32 = |off: u64, v: u32| self.pramin_w32(rl + off, v);
        let userd = b.userd_vram();
        let inst = b.inst_vram();
        w32(0x00, 0x8003_0001);
        w32(0x04, 1); // chan_nr
        w32(0x08, 0); // cgrp_id
        w32(0x0c, 0);
        w32(0x10, userd as u32); // | (runq<<1), runq=0
        w32(0x14, (userd >> 32) as u32);
        w32(0x18, inst as u32); // | chan_id, chan_id=0
        w32(0x1c, (inst >> 32) as u32);
    }

    /// Global FIFO + per-PBDMA init — the bring-up nouveau does in the fifo
    /// subdev BEFORE any channel commit, which we had skipped. Un-SUSPENDs the
    /// PBDMAs so the host will load a committed channel onto one. Order &
    /// values per nvkm fifo: tu102_fifo_init_pbdmas + gk208/gk104/gf100_runq_init
    /// + gk104_fifo_init. Idempotent.
    fn setup_fifo(&self) {
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let wr = |off: u32, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off as usize) as *mut u32, v)
        };
        // (0) PMC reset pulse for FIFO (nvkm_mc_reset, gk104_mc_reset[]: FIFO =
        // mask 0x00000100 at NV_PMC_ENABLE 0x000200). This is the FIRST thing
        // nouveau does for any engine before touching its registers — disable
        // then re-enable the bit, deasserting reset. We never did this: the
        // register *file* tolerates R/W while clock/reset-gated (writes latch,
        // reads echo them back), but the scheduler FSM that walks
        // PENDING -> ON_PBDMA never actually runs while FIFO sits in reset,
        // which matches every symptom seen so far (clean fault, clean writes,
        // zero scheduling progress). Idempotent — safe to repeat.
        wr(0x0000_0200, rd(0x0000_0200) & !0x0000_0100);
        let _ = rd(0x0000_0200);
        wr(0x0000_0200, rd(0x0000_0200) | 0x0000_0100);
        let _ = rd(0x0000_0200);
        // (A) doorbell-enable (tu102_fifo_init_pbdmas).
        wr(0x00b6_5000, rd(0x00b6_5000) | 0x8000_0000);
        // (B) per-PBDMA (runq) init, stride id*0x2000. NV_PFIFO_PBDMA_MAP has
        // up to 12 entries (same __SIZE_1=12 as the PBDMA_MAP scan elsewhere
        // in this file) -- 0..6 was NOT generous enough: a real-hardware run
        // discovered our CE's runlist is served by PBDMA9, which this loop
        // never touched. Its INTR_STALL/INTR_0/INTR_EN/TIMEOUT were left at
        // whatever the hardware defaulted to, and its GET/GP_GET registers
        // still held stale non-zero values from some prior context -- exactly
        // consistent with SCHED_STATUS.runlist_fetch_busy staying stuck at 1
        // forever and PBDMA9's CHANNEL register reading 0 (nothing ever
        // loaded). Cover the full range; writes to absent PBDMAs are harmless.
        for q in 0..12u32 {
            let s = q * 0x2000;
            // INTR_STALL: clear 0x10000100.
            wr(0x0004_013c + s, rd(0x0004_013c + s) & !0x1000_0100);
            wr(0x0004_0108 + s, 0xffff_ffff); // INTR_0   clear
            wr(0x0004_010c + s, 0xffff_feff); // INTR_EN_0
            wr(0x0004_0148 + s, 0xffff_ffff); // INTR_1   clear
            wr(0x0004_014c + s, 0xffff_ffff); // INTR_EN_1
            wr(0x0004_012c + s, 0x000f_4240); // TIMEOUT = 1000000
        }
        // (C) global fifo init (gk104_fifo_init).
        wr(0x0000_2100, 0xffff_ffff); // PFIFO INTR_0     clear
        wr(0x0000_2140, 0x7fff_ffff); // PFIFO INTR_EN_0
    }

    fn gmmu_flush(&self, root_phys: u64) -> (u32, u32, bool) {
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let wr = |off: u32, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off as usize) as *mut u32, v)
        };
        let pre = rd(0x00b8_30b0);
        if pre & 0x8000_0000 != 0 {
            return (pre, pre, false); // flush already pending — never stack
        }
        wr(0x00b8_30a0, (root_phys >> 8) as u32);
        wr(0x00b8_30a4, 0);
        wr(0x00b8_30b0, 0x8000_0001); // trigger PAGE_ALL invalidate
        let mut post = pre;
        let mut ok = false;
        for _ in 0..5_000_000u64 {
            post = rd(0x00b8_30b0);
            if post & 0x8000_0000 == 0 {
                ok = true;
                break;
            }
            gpu_spin();
        }
        (pre, post, ok)
    }

    /// Scan the PTOP device-info table (0x022700+i*4, 64 slots) for the copy
    /// engine's runlist id. Volta+ gives EVERY engine its own dedicated
    /// runlist (discovered, not fixed) — we had been assuming runlist 0 is
    /// the copy engine's without ever checking. Mirrors nvkm's
    /// gk104_top_parse exactly: each logical device spans 1+ consecutive
    /// 32-bit words (continuation while bit31 is set; the final word of an
    /// entry, bit31 clear, carries the ENGINE_TYPE -> NVKM engine dispatch).
    ///
    /// On this chip PTOP reports MULTIPLE CE-type entries (type 0x1/0x2/0x3/
    /// 0x13) with DIFFERENT runlist ids — some sharing GR's runlist (almost
    /// certainly a "GRCE", a copy engine reserved for GR context-switch use,
    /// not general DMA) and others standalone. Picking the first one blindly
    /// landed on the GRCE (runlist 0 == GR's runlist), which is plausibly
    /// why nothing ever go scheduled: GRCE's runlist may not be a normal
    /// user-DMA path at all. Prefer a CE runlist that does NOT match GR's.
    /// Returns (runlist_id, engine_id) for the chosen CE. `engine_id` is the
    /// PTOP ENUM word's "engine" field (bits 29:26, gated by bit5=0x20) — a
    /// THIRD id namespace, distinct from both runlist id and PBDMA index,
    /// used to index NV_PFIFO_ENGINE_STATUS(i) = 0x2640+i*8 (per-engine
    /// scheduler status: CTX_STATUS, FAULTED, ENGINE busy/idle). We had
    /// never read this register at all.
    fn find_ce_runlist(&self) -> Option<(u32, u32)> {
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let mut ty: u32 = !0;
        let mut have_entry = false;
        let mut runlist: u32 = 0;
        let mut have_runlist = false;
        let mut engine: u32 = 0;
        let mut have_engine = false;
        let mut gr_runlist: Option<u32> = None;
        let mut first_ce: Option<(u32, u32)> = None;
        let mut standalone_ce: Option<(u32, u32)> = None;
        for i in 0..64u32 {
            if !have_entry {
                ty = !0;
                have_runlist = false;
                have_engine = false;
                have_entry = true;
            }
            let data = rd(0x0002_2700 + i * 4);
            match data & 0x3 {
                0 => continue, // NOT_VALID — skip, keep accumulating this entry
                1 => {}        // DATA — addr/fault/inst, unused here
                2 => {
                    if data & 0x20 != 0 {
                        engine = (data >> 26) & 0xf;
                        have_engine = true;
                    }
                    if data & 0x10 != 0 {
                        runlist = (data >> 21) & 0xf;
                        have_runlist = true;
                    }
                }
                3 => ty = (data >> 2) & 0x1fff_ffff, // ENGINE_TYPE
                _ => unreachable!(),
            }
            if data & 0x8000_0000 != 0 {
                continue; // more words follow for this same entry
            }
            if have_runlist {
                if ty == 0x0 {
                    gr_runlist = Some(runlist);
                } else if matches!(ty, 0x1 | 0x2 | 0x3 | 0x13) {
                    let eng = if have_engine { engine } else { u32::MAX };
                    if first_ce.is_none() {
                        first_ce = Some((runlist, eng));
                    }
                    if standalone_ce.is_none() && Some(runlist) != gr_runlist {
                        standalone_ce = Some((runlist, eng));
                    }
                }
            }
            have_entry = false;
        }
        // Re-check standalone candidates against GR's runlist now that GR
        // (which can appear before OR after CE entries in the table) is
        // fully known — a single forward pass may have picked a CE entry
        // that only *looked* standalone before GR's own entry was parsed.
        if let Some(gr) = gr_runlist {
            if standalone_ce.map(|(rl, _)| rl) == Some(gr) {
                standalone_ce = None;
            }
        }
        standalone_ce.or(first_ce)
    }

    /// Same scan as `find_ce_runlist` but reports every finalized entry
    /// (type, inst, runlist) as text, for hardware visibility — does this
    /// chip even expose a runlist field for CE, and what does GR's look like
    /// for comparison.
    fn ptop_report(&self) -> alloc::string::String {
        use core::fmt::Write;
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let mut out = alloc::string::String::new();
        let mut ty: u32 = !0;
        let mut have_entry = false;
        let mut runlist: u32 = 0;
        let mut have_runlist = false;
        for i in 0..64u32 {
            if !have_entry {
                ty = !0;
                have_runlist = false;
                have_entry = true;
            }
            let data = rd(0x0002_2700 + i * 4);
            match data & 0x3 {
                0 => continue,
                1 => {}
                2 => {
                    if data & 0x10 != 0 {
                        runlist = (data >> 21) & 0xf;
                        have_runlist = true;
                    }
                }
                3 => ty = (data >> 2) & 0x1fff_ffff,
                _ => unreachable!(),
            }
            if data & 0x8000_0000 != 0 {
                continue;
            }
            let name = match ty {
                0x0 => "GR",
                0x1 | 0x2 | 0x3 | 0x13 => "CE",
                0x8 => "MSPDEC",
                0x9 => "MSPPP",
                0xa => "MSVLD",
                0xb => "MSENC",
                0xc => "VIC",
                0xd => "SEC2",
                0xe | 0xf => "NVENC",
                0x10 => "NVDEC",
                0x14 => "GSP",
                0x15 => "NVJPG",
                _ if ty == !0 => "?",
                _ => "OTHER",
            };
            if ty != !0 {
                let _ = write!(
                    out,
                    " {}(ty={:#x})/rl={}",
                    name,
                    ty,
                    if have_runlist { runlist as i64 } else { -1 }
                );
            }
            have_entry = false;
        }
        out
    }

    /// Idempotently bring the channel to the committed + enabled state (the
    /// Step 3 end-state): instance block, GMMU flush, runlist commit, doorbell
    /// and channel enable. Returns (commit_ok, runlist_id_used). Safe to
    /// repeat — used by Step 4+ so each is self-contained across reboots.
    fn setup_channel(&self, b: &GpuBringup) -> (bool, u32) {
        let runl_id = self.find_ce_runlist().map(|(rl, _)| rl).unwrap_or(0);
        const CHID: u32 = 0;
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let wr = |off: u32, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off as usize) as *mut u32, v)
        };

        self.write_instance_block_vram(b);
        self.write_runlist_vram(b);
        // Arm the HUB MMU fault buffer — required before any channel can run.
        let _ = self.setup_fault_buffer(b);
        let _ = self.setup_bar2(b);
        let _ = self.gmmu_flush(b.root.paddr() as u64);

        // Global FIFO + PBDMA init (un-SUSPEND the PBDMAs) — must precede the
        // runlist commit, else the host leaves the channel at STATUS=PENDING.
        self.setup_fifo();

        // Bind the channel's instance block in CHRAM so the host can find it
        // (gk104_chan_bind_inst: 0x800000+chid*8 = BIND | inst>>12, VRAM target).
        let inst_vram = b.inst_vram();
        wr(
            0x0080_0000 + CHID * 8,
            0x8000_0000 | (inst_vram >> 12) as u32,
        );

        // Ensure runlist scheduling is allowed (NV_PFIFO_SCHED_DISABLE bit=runl
        // id; gk104_runl_allow clears it). Default is 0, but clear it to be sure.
        wr(0x0000_2630, rd(0x0000_2630) & !(1u32 << runl_id));

        // Enable the channel BEFORE committing the runlist (nouveau order is
        // bind -> start(enable) -> commit; the commit is what loads the channel,
        // so it must see an enabled channel). gk104_chan_start: 0x800004 |= 0x400.
        wr(
            0x0080_0004 + CHID * 8,
            rd(0x0080_0004 + CHID * 8) | 0x0000_0400,
        );

        // tu102_chan_start does MORE than gk104_chan_start: right after the
        // PCCSR enable write it ALSO rings the doorbell immediately, with the
        // SAME token a later GPFIFO push would use (runl_id<<16 | chid). This
        // is the actual kick that wakes the HW scheduler to notice a freshly
        // enabled channel and pull it off PENDING — without it the channel
        // can sit at PENDING forever even after a clean runlist commit, which
        // is exactly the symptom we hit. device->vfn->addr.user + 0x0090 ==
        // BAR0 + 0xb80000(priv) + 0x030000(user) + 0x90 == 0xbb0090.
        let token = (runl_id << 16) | CHID;
        wr(0x00bb_0090, token);

        // Runlist commit LAST (2 entries). The runlist lives in VRAM; the host
        // reads it VRAM-physical, no target field needed (tu102_runl_commit).
        let base = 0x0000_2b00 + runl_id * 0x10;
        let runlist_vram = b.runlist_vram();
        wr(base, runlist_vram as u32);
        wr(base + 4, (runlist_vram >> 32) as u32);
        wr(base + 8, 2);
        let mut ok = false;
        for _ in 0..5_000_000u64 {
            if rd(base + 0xc) & 0x0000_8000 == 0 {
                ok = true;
                break;
            }
            gpu_spin();
        }
        (ok, runl_id)
    }

    pub fn fill_rect(&self, x: u32, y: u32, w: u32, h: u32, color: u32) {
        let width = self.info.width;
        let height = self.info.height;
        let x = x.min(width);
        let y = y.min(height);
        let w = w.min(width.saturating_sub(x));
        let h = h.min(height.saturating_sub(y));
        if w == 0 || h == 0 {
            return;
        }

        let ptr = self.info.fb_base_vaddr as *mut u32;
        let pitch_u32 = self.pitch_pixels();

        for py in 0..h {
            let row_start = (y + py) as usize * pitch_u32 + (x as usize);
            for px in 0..w {
                unsafe {
                    core::ptr::write_volatile(ptr.add(row_start + px as usize), color);
                }
            }
        }
    }

    pub fn blit_rect(&self, src_x: u32, src_y: u32, dst_x: u32, dst_y: u32, w: u32, h: u32) {
        let width = self.info.width;
        let height = self.info.height;
        let w = w
            .min(width.saturating_sub(src_x))
            .min(width.saturating_sub(dst_x));
        let h = h
            .min(height.saturating_sub(src_y))
            .min(height.saturating_sub(dst_y));
        if w == 0 || h == 0 {
            return;
        }

        let ptr = self.info.fb_base_vaddr as *mut u32;
        let pitch_u32 = self.pitch_pixels();

        let same_row_overlap = dst_y == src_y && dst_x > src_x && dst_x < src_x + w;
        let overlap_down = dst_y > src_y && dst_y < src_y + h;

        if same_row_overlap {
            for py in 0..h {
                let src_row = (src_y + py) as usize * pitch_u32 + (src_x as usize);
                let dst_row = (dst_y + py) as usize * pitch_u32 + (dst_x as usize);
                unsafe {
                    for i in (0..w as usize).rev() {
                        core::ptr::write(
                            ptr.add(dst_row + i),
                            core::ptr::read(ptr.add(src_row + i)),
                        );
                    }
                }
            }
        } else if overlap_down {
            for py in (0..h).rev() {
                let src_row = (src_y + py) as usize * pitch_u32 + (src_x as usize);
                let dst_row = (dst_y + py) as usize * pitch_u32 + (dst_x as usize);
                unsafe {
                    core::ptr::copy(ptr.add(src_row), ptr.add(dst_row), w as usize);
                }
            }
        } else {
            for py in 0..h {
                let src_row = (src_y + py) as usize * pitch_u32 + (src_x as usize);
                let dst_row = (dst_y + py) as usize * pitch_u32 + (dst_x as usize);
                unsafe {
                    core::ptr::copy(ptr.add(src_row), ptr.add(dst_row), w as usize);
                }
            }
        }
    }

    /// Live RM display state for this GPU: outputs, connected mask and EDID
    /// head, straight from the NV0073 query (per-instance cached on the C
    /// side, so repeated calls are cheap). `None` until this GPU's bring-up
    /// chain has run, or when the GPU has no display engine.
    fn rm_display_state(&self) -> Option<(u32, nvidia_rm_sys::rm_init::GrEdid)> {
        use core::sync::atomic::{AtomicBool, Ordering};
        // Boot-stable snapshot: the first successful query wins and every
        // caller after it sees exactly that topology -- never a downgrade
        // back to `None`/legacy ids once real ids have been advertised. See
        // the `rm_display_snap` field doc for why this is load-bearing for
        // VK_KHR_display.
        if let Some(snap) = *self.rm_display_snap.lock() {
            Self::rm_enable_hdmi_audio_once(snap.0, &snap.1);
            return Some(snap);
        }
        let instance = (*self.rm_device_instance.lock())?;
        // The FIRST query runs the full RM/GSP control chain plus a DDC/EDID
        // probe (the C side then caches it for the rest of the boot). That can
        // take a long time and is not reentrant, so serialize opportunistically:
        // a caller that finds another query in flight reports "no RM topology"
        // and the DRM layer falls back to the synthetic connector, instead of
        // parking a second CPU behind it. (`get_connector()` keeps 1001 alive
        // as an alias afterwards, so a client that started on the fallback
        // still finishes its probe consistently once the snapshot lands.)
        static EDID_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
        if EDID_IN_FLIGHT.swap(true, Ordering::Acquire) {
            return None;
        }
        let d = nvidia_rm_sys::rm_init::edid(instance).ok();
        EDID_IN_FLIGHT.store(false, Ordering::Release);
        let d = d?;
        if !(d.supported_status == 0 && d.display_mask != 0) {
            return None;
        }
        Self::rm_enable_hdmi_audio_once(instance, &d);
        let snap = (instance, d);
        *self.rm_display_snap.lock() = Some(snap);
        Some(snap)
    }

    /// Per GPU: push each connected display's ELD to the GPU's HDA codec and
    /// enable audio packet transmission (SET_HDMI_ENABLE + SET_ELD_AUDIO_CAPS
    /// + SET_AUDIO_ENABLE + HDMI/DP unmute + GCP). The UEFI GOP modeset that
    /// this driver scans out on never enables audio, so without this the HDA
    /// function's pins stay at PD=0/ELDV=0 and HDMI audio is silent. Runs
    /// piggybacked on RM display queries once DispCommon handles exist.
    /// Latched only after a successful ELD push so a first call with
    /// `connected_mask == 0` or a transient RM failure still retries.
    fn rm_enable_hdmi_audio_once(instance: u32, d: &nvidia_rm_sys::rm_init::GrEdid) {
        Self::rm_enable_hdmi_audio(instance, d, false);
    }

    /// Push ELD + unmute HDMI/DP audio packets. `force` re-sends even after a
    /// successful first pass (stream start: GOP never enables audio, and the
    /// first DRM query can run before the head is scanning out).
    /// Returns the one-line outcome it also records for `/proc/gpusnd`.
    fn rm_enable_hdmi_audio(
        instance: u32,
        d: &nvidia_rm_sys::rm_init::GrEdid,
        force: bool,
    ) -> String {
        use core::sync::atomic::{AtomicU32, Ordering};
        static DONE: AtomicU32 = AtomicU32::new(0);
        if instance >= 32 {
            return alloc::format!("[hdmi-audio] gpu{}: RM instance out of range", instance);
        }
        if !force && (DONE.load(Ordering::Acquire) & (1 << instance)) != 0 {
            return alloc::format!("[hdmi-audio] gpu{}: already enabled", instance);
        }
        if d.connected_mask == 0 {
            let line = alloc::format!(
                "[hdmi-audio] gpu{}: no connected outputs — will retry",
                instance
            );
            record_hdmi_audio_status(line.clone());
            log::info!("{}", line);
            return line;
        }
        // TMDS/HDMI outputs get SET_HDMI_ENABLE + GCP un-mute on top of ELD.
        // DP / USB-C (DP alt-mode) take the DP unmute path inside RM.
        let n = (d.conn_type_count as usize).min(d.conn_type_display_id.len());
        let hdmi_mask: u32 = (0..n)
            .filter(|&i| matches!(d.conn_type[i], 0x61 | 0x63))
            .map(|i| d.conn_type_display_id[i])
            .fold(0, |m, id| m | id)
            & d.connected_mask;
        let line = match nvidia_rm_sys::rm_init::hdmi_audio(
            instance,
            d.connected_mask,
            hdmi_mask,
            force,
        ) {
            Ok(out) => {
                let line = alloc::format!(
                    "[hdmi-audio] gpu{}: displays {:#x} (hdmi {:#x}) — ELD ok {:#x}, audio-enable ok {:#x}, gcp ok {:#x} (sads={}, maxFreq={})",
                    instance,
                    out.attempted_mask,
                    hdmi_mask,
                    out.eld_ok_mask,
                    out.enable_ok_mask,
                    out.gcp_ok_mask,
                    out.sad_count,
                    out.max_freq,
                );
                log::warn!(
                    "[hdmi-audio] gpu{}: displays {:#x} (hdmi {:#x}) — ELD ok {:#x}, audio-enable ok {:#x}, gcp ok {:#x} (sads={}, maxFreq={}){}",
                    instance,
                    out.attempted_mask,
                    hdmi_mask,
                    out.eld_ok_mask,
                    out.enable_ok_mask,
                    out.gcp_ok_mask,
                    out.sad_count,
                    out.max_freq,
                    if force { " [stream]" } else { "" },
                );
                if out.eld_ok_mask != 0 {
                    DONE.fetch_or(1 << instance, Ordering::AcqRel);
                }
                line
            }
            Err(st) => {
                let line = alloc::format!(
                    "[hdmi-audio] gpu{}: enable FAILED, NV_STATUS={:#x} (will retry)",
                    instance,
                    st
                );
                log::warn!("{}", line);
                line
            }
        };
        record_hdmi_audio_status(line.clone());
        line
    }

    /// Enable HDMI/DP audio on the GOP modeset without GSP-RM.
    ///
    /// Linux nouveau does this with the same BAR0 registers (`gf119_sor_hda_*`
    /// + GV100 SF_USER GCP unmute) when the display engine is already scanning
    /// out. The console GPU cannot take a GSP boot (SEC2 wedges the bus), so
    /// this is the path that makes the monitor speakers work the way they do
    /// under Linux: ELD into the HDA codec, presence on the live SOR, GCP
    /// audio unmute. Video timing is left untouched.
    fn gop_enable_hdmi_audio(&self) -> String {
        use core::fmt::Write;
        let bar0 = self._bar0;
        let rd =
            |off: usize| -> u32 { unsafe { core::ptr::read_volatile((bar0 + off) as *const u32) } };
        let wr = |off: usize, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off) as *mut u32, v);
        };
        let rmw = |off: usize, mask: u32, val: u32| {
            wr(off, (rd(off) & !mask) | (val & mask));
        };

        let edid = match boot_edid() {
            Some((buf, n)) if n >= 128 => buf,
            _ => {
                return String::from("[hdmi-audio] GOP path: no UEFI EDID — cannot build an ELD");
            }
        };

        // Armed NVDisplay core channel (v03_00): SOR_SET_CONTROL lives at
        // NVC37D_SOR_SET_CONTROL(i) = 0x300 + i*0x20. GOP has already armed
        // the live SOR; owner_mask names the head, protocol is TMDS or DP.
        const ARMED: usize = 0x0068_8000;
        const ASSY: usize = 0x0068_0000;
        let mut found: Vec<(u32, u32, bool)> = Vec::new(); // (sor, head, is_hdmi)
        for base in [ARMED, ASSY] {
            for sor in 0u32..8 {
                let ctrl = rd(base + 0x300 + (sor as usize) * 0x20);
                let owner = ctrl & 0xff;
                if owner == 0 {
                    continue;
                }
                let head = owner.trailing_zeros();
                if head > 7 {
                    continue;
                }
                let proto = (ctrl >> 8) & 0xf;
                let is_hdmi = matches!(proto, 0x1 | 0x2 | 0x5);
                let is_dp = matches!(proto, 0x8 | 0x9);
                if !is_hdmi && !is_dp {
                    continue;
                }
                if !found.iter().any(|&(s, _, _)| s == sor) {
                    found.push((sor, head, is_hdmi));
                }
            }
            if !found.is_empty() {
                break;
            }
        }
        if found.is_empty() {
            // Single-monitor GOP almost always uses head 0 / SOR 0 HDMI.
            found.push((0, 0, true));
        }

        let mut report = String::new();
        for (sor, head, is_hdmi) in found.iter().copied() {
            let eld = build_eld_from_base_edid(&edid, 1u32 << sor, !is_hdmi);
            let soff = 0x030 * sor as usize + head as usize * 4;
            let hoff = head as usize * 0x800;
            for i in 0..96u32 {
                wr(0x10ec00 + soff, (i << 8) | eld[i as usize] as u32);
            }
            // PD + ELDV (nouveau gf119_sor_hda_eld).
            rmw(0x10ec10 + soff, 0x8000_0002, 0x8000_0002);
            // HDA device entry = this head (gf119 0x616548 / gv100 0x616528).
            rmw(0x616548 + hoff, 0x0000_0070, head << 4);
            rmw(0x616528 + hoff, 0x0000_0070, head << 4);
            // Presence (gf119_sor_hda_hpd).
            rmw(0x10ec10 + soff, 0x8000_0001, 0x8000_0001);

            if is_hdmi {
                // GV100 SF_USER GCP unmute (nouveau r535_sor_hdmi_audio). Head
                // 0 is offset 0, so the 0x400 stride does not matter for GOP.
                let hdmi = head as usize * 0x400;
                rmw(0x6f00c0 + hdmi, 0x1, 0x0);
                wr(0x6f00cc + hdmi, 0x0000_0010);
                rmw(0x6f00c0 + hdmi, 0x1, 0x1);
            } else {
                rmw(0x616618 + hoff, 0x8000_000d, 0x8000_0001);
            }

            if !report.is_empty() {
                report.push('\n');
            }
            let _ = write!(
                report,
                "[hdmi-audio] GOP {:02x}:{:02x}.0: SOR{} head{} {} ELD+PD+{}unmute (no GSP, like nouveau)",
                self.pci_bus,
                self.pci_device,
                sor,
                head,
                if is_hdmi { "HDMI" } else { "DP" },
                if is_hdmi { "GCP " } else { "SF " },
            );
        }
        // Give the HDA codec a couple of milliseconds to latch PD/ELDV
        // before score_path / pin-sense runs.
        {
            let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };
            while unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0) < 2_000 {
                gpu_spin();
            }
        }
        log::warn!("{}", report);
        report
    }

    /// Re-push ELD/unmute on the GPU that drives the monitor — the Linux
    /// model: one HDMI sink, not every HDA function on the board.
    ///
    /// The console GPU has no GSP (SEC2 wedges), so HDMI audio is enabled
    /// through BAR0 the way nouveau does on a live GOP modeset. A GPU with
    /// RM attached still uses the RM controls. Extra GPUs with no display
    /// are skipped (they are not exposed as ALSA cards either).
    pub(crate) fn kick_hdmi_audio_all() {
        use core::fmt::Write;
        let gpus: Vec<Arc<NvidiaGpu>> = NVIDIA_GPUS.lock().clone();
        let mut report = String::new();
        if gpus.is_empty() {
            report.push_str("[hdmi-audio] stream start: no NVIDIA GPU probed");
        }
        for gpu in gpus.iter() {
            if !gpu.drives_boot_display() {
                let _ = write!(
                    report,
                    "{}[hdmi-audio] {:02x}:{:02x}.0 {}: skipped — no monitor on this GPU \
                     (Linux also leaves its HDMI pins without ELD)",
                    if report.is_empty() { "" } else { "\n" },
                    gpu.pci_bus,
                    gpu.pci_device,
                    gpu.gpu_model,
                );
                continue;
            }
            let who = alloc::format!(
                "{:02x}:{:02x}.0 {} [console GPU, drives the monitor]",
                gpu.pci_bus,
                gpu.pci_device,
                gpu.gpu_model,
            );
            let instance = *gpu.rm_device_instance.lock();
            let Some(instance) = instance else {
                // No GSP: enable audio on the existing GOP modeset. A
                // write(2) to /dev/snd must never boot GSP (that can hang
                // the machine); BAR0 ELD/GCP is the Linux-nouveau equivalent.
                let gop = gpu.gop_enable_hdmi_audio();
                let _ = write!(
                    report,
                    "{}[hdmi-audio] {}: RM not attached, GOP HDMI audio path\n{}",
                    if report.is_empty() { "" } else { "\n" },
                    who,
                    gop,
                );
                continue;
            };
            let line = match nvidia_rm_sys::rm_init::edid(instance) {
                Err(st) => alloc::format!(
                    "[hdmi-audio] {} (rm{}): display query FAILED NV_STATUS={:#x}{}",
                    who,
                    instance,
                    st,
                    match st {
                        0x40 => " — GSP not initialized on this GPU",
                        0x1f => " — no GPU at this RM instance",
                        _ => "",
                    }
                ),
                Ok(d) if d.alloc_status == 0x56 => alloc::format!(
                    "[hdmi-audio] {} (rm{}): no display engine (headless)",
                    who,
                    instance
                ),
                Ok(d) if d.alloc_status != 0 => alloc::format!(
                    "[hdmi-audio] {} (rm{}): RM DispCommon unavailable NV_STATUS={:#x}",
                    who,
                    instance,
                    d.alloc_status
                ),
                Ok(d) if d.supported_status != 0 => alloc::format!(
                    "[hdmi-audio] {} (rm{}): GET_SUPPORTED FAILED NV_STATUS={:#x}",
                    who,
                    instance,
                    d.supported_status
                ),
                Ok(d) if d.connected_mask == 0 => alloc::format!(
                    "[hdmi-audio] {} (rm{}): outputs {:#x} (ddc {:#x}), connected 0 — no monitor on this GPU (connect status {:#x})",
                    who,
                    instance,
                    d.display_mask,
                    d.display_mask_ddc,
                    d.connect_status
                ),
                Ok(d) => {
                    let outcome = Self::rm_enable_hdmi_audio(instance, &d, true);
                    alloc::format!("[hdmi-audio] {} (rm{}): connected {:#x}\n{}", who, instance, d.connected_mask, outcome)
                }
            };
            if !report.is_empty() {
                report.push('\n');
            }
            report.push_str(&line);
        }
        log::info!("{}", report);
        record_hdmi_audio_status(report);
    }

    /// DRM connector id for output bit `bit` on RM instance `instance`.
    /// Both are < 32, so ids are unique across GPUs and never collide with
    /// the synthetic software-KMS ids (1..3) or the legacy fallback (1001).
    fn rm_connector_id(instance: u32, bit: u32) -> u32 {
        1001 + 100 * instance + bit
    }

    /// Resolve a DRM connector id back to the RM output bit it names.
    /// `1001` stays as a boot-stable alias for the first supported output so a
    /// client that started probing before RM topology was cached can finish the
    /// rest of that probe against the same advertised id.
    fn rm_connector_bit(instance: u32, id: u32, d: &nvidia_rm_sys::rm_init::GrEdid) -> Option<u32> {
        if id == 1001 {
            return (0..32u32).find(|&b| d.display_mask & (1u32 << b) != 0);
        }
        let bit = id.checked_sub(Self::rm_connector_id(instance, 0))?;
        if bit >= 32 || d.display_mask & (1u32 << bit) == 0 {
            return None;
        }
        Some(bit)
    }

    /// RM connector type (NV0073_CTRL_SPECIFIC_CONNECTOR_DATA_TYPE_*) for a
    /// single-bit displayId, from the cached GET_CONNECTOR_DATA sweep.
    fn rm_conn_type(d: &nvidia_rm_sys::rm_init::GrEdid, did: u32) -> Option<u32> {
        let n = (d.conn_type_count as usize).min(d.conn_type_display_id.len());
        (0..n)
            .find(|&i| d.conn_type_display_id[i] == did)
            .map(|i| d.conn_type[i])
    }
}

/// NV0073_CTRL_SPECIFIC_CONNECTOR_DATA_TYPE_* -> DRM_MODE_CONNECTOR_*.
fn nv_conn_type_to_drm(t: u32) -> u32 {
    match t {
        0x00 => 1,                              // VGA_15_PIN
        0x30 | 0x38 | 0x39 => 2,                // DVI_I / LFH_DVI_I_{1,2}
        0x31 => 3,                              // DVI_D
        0x46 | 0x47 | 0x49 | 0x64 | 0x65 => 10, // DP ext/int/serializer, LFH_DP
        0x48 => 10,                             // DP_MINI_EXT
        0x61 | 0x63 => 11,                      // HDMI_A / HDMI_C_MINI
        0x70 => 15,                             // VIRTUAL_WFD
        0x71 | 0x74 => 10,                      // USB_C (DP alt mode)
        0x72 => 16,                             // DSI
        _ => 0,                                 // Unknown
    }
}

/// Short human name for the /proc/gpuedid dump.
fn nv_conn_type_name(t: u32) -> &'static str {
    match t {
        0x00 => "VGA",
        0x30 | 0x38 | 0x39 => "DVI-I",
        0x31 => "DVI-D",
        0x46 | 0x47 | 0x49 | 0x64 | 0x65 => "DP",
        0x48 => "miniDP",
        0x61 => "HDMI",
        0x63 => "miniHDMI",
        0x70 => "virtual",
        0x71 | 0x74 => "USB-C",
        0x72 => "DSI",
        0xFFFF_FFFF => "?",
        _ => "other",
    }
}

#[allow(dead_code)] // used when deferred BAR0 MMIO probe is enabled
fn arch_from_pmc_boot0(boot0: u32) -> NvidiaArchitecture {
    let chip_id = (boot0 >> regs::PMC_BOOT0_CHIP_ID_SHIFT) & regs::PMC_BOOT0_CHIP_ID_MASK;
    if chip_id >= regs::PMC_BOOT0_CHIPID_BLACKWELL_MIN {
        NvidiaArchitecture::Blackwell
    } else if (regs::PMC_BOOT0_CHIPID_HOPPER_MIN..=regs::PMC_BOOT0_CHIPID_HOPPER_MAX)
        .contains(&chip_id)
    {
        NvidiaArchitecture::Hopper
    } else if (regs::PMC_BOOT0_CHIPID_ADA_MIN..=regs::PMC_BOOT0_CHIPID_ADA_MAX).contains(&chip_id) {
        NvidiaArchitecture::AdaLovelace
    } else if (regs::PMC_BOOT0_CHIPID_AMPERE_MIN..=regs::PMC_BOOT0_CHIPID_AMPERE_MAX)
        .contains(&chip_id)
    {
        NvidiaArchitecture::Ampere
    } else if (regs::PMC_BOOT0_CHIPID_TURING_MIN..=regs::PMC_BOOT0_CHIPID_TURING_MAX)
        .contains(&chip_id)
    {
        NvidiaArchitecture::Turing
    } else {
        NvidiaArchitecture::Unknown
    }
}

/// NV_PFAULT_FAULT_TYPE ([4:0] of INFO1) decode (Turing dev_fault.ref.txt).
fn fault_reason_name(r: u32) -> &'static str {
    match r {
        0 => "PDE",
        1 => "PDE_SIZE",
        2 => "PTE(unmapped)",
        3 => "VA_LIMIT",
        4 => "UNBOUND_INST",
        5 => "PRIV",
        6 => "RO",
        7 => "WO",
        0xa => "BAD_APERTURE",
        _ => "?",
    }
}

/// NV_PFAULT_ACCESS_TYPE ([19:16] of INFO1) decode.
fn fault_access_name(a: u32) -> &'static str {
    match a {
        0 => "READ",
        1 => "WRITE",
        2 => "ATOMIC",
        3 => "PREFETCH",
        8 => "PHYS_READ",
        9 => "PHYS_WRITE",
        0xa => "PHYS_ATOMIC",
        _ => "?",
    }
}

fn read_temperature(bar0: usize) -> Option<i32> {
    let raw =
        unsafe { core::ptr::read_volatile((bar0 + regs::NV_THERM_TEMP as usize) as *const u32) };
    if raw == 0 || raw == 0xFFFF_FFFF {
        return None;
    }
    let raw9 = raw & regs::NV_THERM_TEMP_VALUE_MASK;
    if (raw9 & regs::NV_THERM_TEMP_VALUE_SIGN_BIT) != 0 {
        Some((raw9 as i32) - 512)
    } else {
        Some(raw9 as i32)
    }
}

#[allow(dead_code)]
unsafe fn probe_resolution_from_bar0(bar0: usize) -> Option<(u32, u32)> {
    let reg =
        core::ptr::read_volatile((bar0 + regs::NV50_HEAD0_RASTER_SIZE as usize) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 {
        return Some((w, h));
    }

    let reg = core::ptr::read_volatile((bar0 + regs::NV40_PCRTC_HEAD0_SIZE as usize) as *const u32);
    let (w, h) = (reg & 0xFFFF, reg >> 16);
    if w > 0 && h > 0 && w <= 16384 && h <= 16384 {
        return Some((w, h));
    }
    None
}

/// Identify GPU based on PCI device ID.
/// Returns (architecture, name, memory_mb).
fn identify_gpu(device_id: u16) -> (NvidiaArchitecture, &'static str, u32) {
    match device_id {
        // Blackwell
        0x2B85 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5090", 32768),
        0x2B89 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5080", 16384),
        0x2C00 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070 Ti", 16384),
        0x2C20 => (NvidiaArchitecture::Blackwell, "GeForce RTX 5070", 12288),

        // Ada Lovelace
        0x2684 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4090", 24576),
        0x2704 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4080", 16384),
        0x2782 => (
            NvidiaArchitecture::AdaLovelace,
            "GeForce RTX 4070 Ti",
            12288,
        ),
        0x2786 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4070", 12288),
        0x2803 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060 Ti", 8192),
        0x2882 => (NvidiaArchitecture::AdaLovelace, "GeForce RTX 4060", 8192),

        // Ampere
        0x2204 => (NvidiaArchitecture::Ampere, "GeForce RTX 3090", 24576),
        0x2206 => (NvidiaArchitecture::Ampere, "GeForce RTX 3080", 10240),
        0x2484 => (NvidiaArchitecture::Ampere, "GeForce RTX 3070", 8192),
        0x2489 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060 Ti", 8192),
        0x2503 => (NvidiaArchitecture::Ampere, "GeForce RTX 3060", 12288),
        0x2571 => (NvidiaArchitecture::Ampere, "GeForce RTX 3050", 8192),

        // Turing
        0x1E02 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Ti", 11264),
        0x1E04 => (NvidiaArchitecture::Turing, "GeForce RTX 2080 Super", 8192),
        0x1E07 => (NvidiaArchitecture::Turing, "GeForce RTX 2080", 8192),
        0x1E82 => (NvidiaArchitecture::Turing, "GeForce RTX 2070 Super", 8192),
        0x1E84 => (NvidiaArchitecture::Turing, "GeForce RTX 2070", 8192),
        0x1F02 | 0x1F06 | 0x1F07 => (NvidiaArchitecture::Turing, "GeForce RTX 2060 Super", 8192),
        0x1F03 | 0x1F08 | 0x1F0A | 0x1F0B => (NvidiaArchitecture::Turing, "GeForce RTX 2060", 6144),
        0x1F36 => (NvidiaArchitecture::Turing, "GeForce GTX 1660 Super", 6144),
        0x1F82 => (NvidiaArchitecture::Turing, "GeForce GTX 1660", 6144),
        0x1F91 => (NvidiaArchitecture::Turing, "GeForce GTX 1650 Super", 4096),
        0x1F99 => (NvidiaArchitecture::Turing, "GeForce GTX 1650", 4096),

        _ => (NvidiaArchitecture::Unknown, "Unknown NVIDIA GPU", 0),
    }
}

impl Scheme for NvidiaGpu {
    fn name(&self) -> &str {
        &self.name
    }
    fn handle_irq(&self, _irq_num: usize) {}
}

impl DisplayScheme for NvidiaGpu {
    fn info(&self) -> DisplayInfo {
        self.info
    }
    fn fb(&self) -> FrameBuffer<'_> {
        unsafe {
            FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size)
        }
    }

    /// BAR1 scanout is PAT write-combining after `enable_framebuffer_wc`.
    #[inline]
    fn fb_write_combining(&self) -> bool {
        true
    }

    /// The framebuffer is the GPU's own VRAM, mapped through the PCI BAR. The
    /// generic 2D primitives (`fill_rect` / `copy_rect` / `blit_from`) therefore
    /// write straight into video memory in bulk — already far cheaper than the
    /// per-pixel MMIO path — so we advertise them as accelerated. (A future step
    /// would offload these to the GPU's own copy engine via command channels.)
    fn accel_caps(&self) -> AccelCaps {
        AccelCaps {
            fill: true,
            copy: true,
            blit: true,
        }
    }
}

/// Clip a hardware page-flip's copy geometry against BOTH the source
/// framebuffer and the destination mode, returning `(row_bytes, lines)`.
///
/// `scanout_region` has always clipped with `fb.width.min(info.width)` and
/// `fb.height.min(info.height)`; the hwflip path passed `fb.width * 4` and
/// `fb.height` straight through, so a client framebuffer larger than the mode
/// had two ways to go wrong:
///
/// - Wider than the destination pitch: the RM rejects the 2D copy with
///   `NV_ERR_INVALID_ARGUMENT`, and `CE_PRESENT_WEDGED` latches for the REST OF
///   THE BOOT -- every later present silently degrades to the CPU blit. Refused
///   here instead, so the caller falls back for this frame only.
/// - Taller than the mode: the copy engine writes past the scanout framebuffer
///   inside BAR1. `ce_present_2d_pitched`'s own `fb_size` guard catches the
///   total overrun and declines, which is safe but also silently gives up the
///   whole fast path; clipping the line count keeps it usable.
///
/// `None` means "nothing safely copyable" -- the caller must fall back.
fn hwflip_geometry(
    fb_width: u32,
    fb_height: u32,
    fb_pitch: u32,
    dst_width: u32,
    dst_height: u32,
    dst_pitch: u32,
) -> Option<(u32, u32)> {
    if fb_pitch == 0 || dst_pitch == 0 {
        return None;
    }
    // `checked_mul`, not `saturating_mul`: a saturated byte count UNDERSTATES
    // the real one, and would then compare as fitting inside a stride it does
    // not fit in.
    let row_bytes = fb_width.min(dst_width).checked_mul(4)?;
    let lines = fb_height.min(dst_height);
    if row_bytes == 0 || lines == 0 {
        return None;
    }
    // A row must fit in both strides: in the source's, or we would read the
    // next row's pixels as this row's tail; in the destination's, or the RM
    // rejects the copy and wedges the engine.
    if row_bytes > fb_pitch || row_bytes > dst_pitch {
        return None;
    }
    Some((row_bytes, lines))
}

/// Pull `elapsed: N ns` out of a `/proc/gpubench` report. 0 if absent.
fn parse_gpubench_elapsed_ns(report: &str) -> u64 {
    for line in report.lines() {
        let Some(rest) = line.split("elapsed:").nth(1) else {
            continue;
        };
        let digits = rest
            .trim()
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or("");
        if let Ok(n) = digits.parse() {
            return n;
        }
    }
    0
}

/// Whether `pid` may act on GEM object `o`: it created it, or it holds a
/// PRIME reference to it (`gem_mmap::holds`), or there is no current thread.
/// Handles come from one global counter, so without this any process could
/// `GEM_INFO`/`VM_BIND`/`CPU_PREP`/`GEM_CLOSE` the compositor's buffers by
/// guessing a number -- and map them into its own VAS for the GPU to read
/// and write. An object with no phys mapping was never exportable, so only
/// its creator can hold it.
fn gem_usable_by(o: &super::nouveau_uapi::NouveauGemObject, pid: u64) -> bool {
    pid == 0
        || o.owner_pid == pid
        || (o.phys_addr.is_some() && crate::scheme::gem_mmap::holds(o.handle, pid))
}

/// `access_ok()` for a user array a nouveau ioctl is about to read directly:
/// `count` `T`s at `ptr` must lie in the user half (a zero count needs no
/// pointer at all, matching how the arms treat `*_count == 0`). Without this,
/// `op_ptr`/`push_ptr`/`wait_ptr`/`sig_ptr` were kernel-side reads at any
/// address the (0666) render node's caller chose.
fn user_slice_ok<T>(ptr: u64, count: u32) -> bool {
    match (count as usize).checked_mul(core::mem::size_of::<T>()) {
        Some(bytes) => super::nouveau_uapi::user_range_ok(ptr as usize, bytes),
        None => false,
    }
}

impl DrmScheme for NvidiaGpu {
    fn pci_bdf(&self) -> Option<(u32, u8, u8, u8)> {
        // RM only ever drives function 0 of the GPU (see `cfg_loc`).
        Some((self.pci_domain, self.pci_bus, self.pci_device, 0))
    }

    fn is_console_gpu(&self) -> bool {
        self.drives_boot_display()
    }

    fn is_compute_gpu(&self) -> bool {
        !self.drives_boot_display()
    }

    fn gpu_role_line(&self) -> String {
        let role = if self.drives_boot_display() {
            "console"
        } else {
            "compute"
        };
        let rm = if self.rm_device_instance.lock().is_some() {
            "rm=ready"
        } else {
            "rm=cold"
        };
        let nodes = if self.drives_boot_display() {
            "(GOP scanout; GSP not auto-booted)"
        } else {
            "nodes=/dev/dri/card0,/dev/dri/card1,/dev/dri/renderD128,/dev/dri/renderD129"
        };
        alloc::format!(
            "{:02x}:{:02x}.0 {} role={} {} {}\n",
            self.pci_bus,
            self.pci_device,
            self.gpu_model,
            role,
            rm,
            nodes
        )
    }

    fn compute_launch(&self, op: u32) -> crate::scheme::drm::ComputeLaunchResult {
        use crate::scheme::drm::{
            ComputeLaunchResult, COMPUTE_OP_BENCH, COMPUTE_OP_INFO, COMPUTE_OP_SAXPY,
        };
        if self.drives_boot_display() {
            return ComputeLaunchResult {
                status: -19, // -ENODEV: console GPU is not the compute device
                elapsed_ns: 0,
                grid_threads: 0,
                report: alloc::format!(
                    "GPU {:02x}:{:02x}.0 is the console GPU — compute lives on the secondary\n",
                    self.pci_bus,
                    self.pci_device
                ),
            };
        }
        match op {
            COMPUTE_OP_INFO => {
                let ready = self.rm_device_instance.lock().is_some();
                let nvk_uapi = if super::nouveau_uapi_enabled() {
                    "on"
                } else {
                    "off"
                };
                let exec_fast = if super::exec_fast_enabled() {
                    "on"
                } else {
                    "off"
                };
                ComputeLaunchResult {
                    status: if ready { 0 } else { -19 },
                    elapsed_ns: 0,
                    grid_threads: 0,
                    report: alloc::format!(
                        "{}nouveau_uapi={nvk_uapi}\nexec_fast={exec_fast}\n\
                         nvk_compute=/dev/dri/card0 (nouveau); this ioctl is eclipse-compute \
                         (Mesa skips card1)\n",
                        self.gpu_role_line()
                    ),
                }
            }
            COMPUTE_OP_SAXPY => {
                // Channel ladder is idempotent; boot auto-bringup may already
                // have run it. Doing it here means `ecl-compute saxpy` works
                // even with `nvidia.noautoboot`.
                let _ = self.bringup_step16();
                let _ = self.bringup_step17();
                let report = self.bringup_step23();
                let ok = report.contains("LOAD-COMPUTE-STORE PROVEN");
                ComputeLaunchResult {
                    status: if ok { 0 } else { 5 },
                    elapsed_ns: 0,
                    grid_threads: 32,
                    report,
                }
            }
            COMPUTE_OP_BENCH => {
                let _ = self.bringup_step16();
                let _ = self.bringup_step17();
                let report = self.bringup_bench();
                let ok = report.contains("GIOPS");
                let elapsed_ns = parse_gpubench_elapsed_ns(&report);
                ComputeLaunchResult {
                    status: if ok { 0 } else { 5 },
                    elapsed_ns,
                    grid_threads: 0,
                    report,
                }
            }
            _ => ComputeLaunchResult {
                status: -22, // -EINVAL
                elapsed_ns: 0,
                grid_threads: 0,
                report: alloc::format!("unknown compute op {op}\n"),
            },
        }
    }

    /// This is the driver that serves the nouveau-compatible ioctls (see
    /// `nouveau_ioctl` below), so `nvidia.nouveau_uapi` may take effect.
    fn nouveau_uapi_capable(&self) -> bool {
        true
    }

    /// Receives `gsp.bin` read from the mounted rootfs by `zCore`'s boot
    /// code (see `zCore/src/main.rs`, right after rootfs mount) -- stored
    /// for the real `kgspInitRm` call made lazily on the first
    /// `/proc/gpudbg` read, same trigger as the RM attach itself.
    fn set_gsp_firmware(&self, bytes: Vec<u8>) {
        *self.gsp_firmware.lock() = Some(bytes);
    }

    fn set_gsp_firmware_status(&self, status: String) {
        *self.gsp_fw_status.lock() = Some(status);
    }

    /// Read-only GPU state dump (surfaced at `/proc/gpudbg`). Step 1 of the GPU
    /// copy-engine bring-up: confirm MMIO works bidirectionally, identify the
    /// exact chip, and record the VRAM/BAR layout we need for channel structs.
    /// All reads, no writes — safe to run on demand post-boot. With two GPUs
    /// this runs once per NvidiaGpu; `name` (PCI bus:dev.fn) tells them apart,
    /// and a matching BAR1/fb_vaddr marks the one actually driving the display.
    fn debug_dump(&self) -> String {
        use core::fmt::Write;
        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        // NV_PMC_BOOT_0: architecture/chipset id. NV_PCFG mirror at BAR0+0x88000
        // exposes PCI config dword 0 (vendor | device<<16) — reading 0x10de here
        // proves MMIO is alive. (Offsets per nouveau nvkm.)
        let boot0 = rd(regs::NV_PMC_BOOT_0);
        let chipset = (boot0 >> 20) & 0x1ff;
        let pcfg = rd(0x8_8000);
        let cstatus = rd(regs::NV_PFB_CSTATUS);
        let mut s = String::new();
        let _ = writeln!(s, "[gpudbg] === {} ({}) ===", self.name, self.gpu_model);
        // nvidia-rm-sys bring-up: first real-hardware exercise of the C-compile
        // + FFI-link pipeline that will host vendored NVIDIA open-gpu-kernel-
        // modules source. Not NVIDIA code yet -- see nvidia-rm-sys/build.rs.
        // A prior isolated (non-workspace) build already confirmed the object
        // code and cross-language linkage are correct; this is the first time
        // it runs inside the actual kernel binary/linker script/panic handler.
        let (nvrm_result, nvrm_logged) = nvidia_rm_sys::smoke_test(17, 25);
        let _ = writeln!(
            s,
            "[gpudbg]  nvrm-sys smoke test: C-add(17,25)={} C->Rust-callback-saw={} (both should be 42)",
            nvrm_result, nvrm_logged
        );
        // First REAL vendored NVIDIA C (src/nvidia/src/libraries/fnv_hash/
        // fnv_hash.c, MIT) exercised on real hardware, not the hand-written
        // smoke test above. fnv1Hash64 on an empty slice can't touch the
        // hash loop at all (zero-length buffer), so it must return the raw
        // FNV-1 64-bit offset basis unchanged: 0xcbf29ce484222325. Any other
        // value means either the wrong function ran or something is broken
        // in the real NVIDIA source path, not something we wrote.
        let nvrm_fnv_empty = nvidia_rm_sys::fnv_hash::fnv1_hash64(&[]);
        let nvrm_fnv_hello = nvidia_rm_sys::fnv_hash::fnv1_hash64(b"hello");
        let _ = writeln!(
            s,
            "[gpudbg]  nvrm-sys REAL NVIDIA fnv1Hash64(\"\")={:#018x} (expect 0xcbf29ce484222325) fnv1Hash64(\"hello\")={:#018x}",
            nvrm_fnv_empty, nvrm_fnv_hello
        );
        let _ = writeln!(
            s,
            "[gpudbg]  arch={:?} BAR0={:#x} BAR1/fb_vaddr={:#x} fb_size={:#x} VRAM={}MB",
            self.architecture, bar0, self._bar1, self.info.fb_size, self.vram_size_mb
        );
        let _ = writeln!(
            s,
            "[gpudbg]  PMC_BOOT_0(0x0)={:#010x} -> chipset=0x{:03x}",
            boot0, chipset
        );
        let _ = writeln!(
            s,
            "[gpudbg]  PCFG(0x88000)={:#010x} vendor={:#06x} device={:#06x}",
            pcfg,
            pcfg & 0xffff,
            pcfg >> 16
        );
        let _ = writeln!(
            s,
            "[gpudbg]  PFB_CSTATUS(0x10020c)={:#010x} drives_console={}",
            cstatus,
            self.drives_boot_display()
        );

        // --- Step 0: FIFO / MMU status (read-only "hang oracle") ---
        // Confirms which runlist owns the copy engine and that no MMU fault is
        // latched at boot, BEFORE any risky write. All reads. Offsets per
        // nouveau tu102 (vfn/fifo/mmu). A PRI-error sentinel (0xbadfxxxx) here
        // just means the engine block is in reset — still harmless to read.
        let doorbell_en = rd(0x00b6_5000);
        let _ = writeln!(s, "[gpudbg]  --- FIFO/MMU (Step 0, read-only) ---");
        let _ = writeln!(
            s,
            "[gpudbg]  DOORBELL_EN(0xb65000)={:#010x} (bit31={})",
            doorbell_en,
            doorbell_en >> 31
        );
        for rl in 0..2u32 {
            let base = 0x0000_2b00 + rl * 0x10;
            let _ = writeln!(
                s,
                "[gpudbg]  RUNL{} base_lo(0x{:x})={:#010x} base_hi={:#010x} submit={:#010x} cfg(0x{:x})={:#010x}",
                rl,
                base,
                rd(base),
                rd(base + 4),
                rd(base + 8),
                base + 0xc,
                rd(base + 0xc)
            );
        }
        // RUNL0/1 above are only ever the console/GR runlists on this chip —
        // a real-hardware run discovered the CE's actual runlist is 8 (not
        // 0/1), so its own commit/submit registers had never been shown here.
        // find_ce_runlist is a read-only PTOP scan; safe in this always-on dump.
        if let Some((ce_rl, _)) = self.find_ce_runlist() {
            if ce_rl >= 2 {
                let base = 0x0000_2b00 + ce_rl * 0x10;
                let _ = writeln!(
                    s,
                    "[gpudbg]  RUNL{}(CE) base_lo(0x{:x})={:#010x} base_hi={:#010x} submit={:#010x} cfg(0x{:x})={:#010x}",
                    ce_rl,
                    base,
                    rd(base),
                    rd(base + 4),
                    rd(base + 8),
                    base + 0xc,
                    rd(base + 0xc)
                );
            }
        }
        let _ = writeln!(s, "[gpudbg]  CHAN0_CFG(0x800004)={:#010x}", rd(0x0080_0004));
        let _ = writeln!(
            s,
            "[gpudbg]  MMU flush PDB(0xb830a0)={:#010x} hi(0xb830a4)={:#010x} trigger(0xb830b0)={:#010x}",
            rd(0x00b8_30a0),
            rd(0x00b8_30a4),
            rd(0x00b8_30b0)
        );

        // --- MMU fault snapshot (Turing tu102: 0xb83080..0xb83094, read-only) ---
        // These latch the most recent non-replayable fault. We never write the
        // clear reg (0xb83094) so the fault stays pinned for inspection.
        let f_info1 = rd(0x00b8_3090);
        let _ = writeln!(s, "[gpudbg]  --- MMU fault snapshot (read-only) ---");
        let _ = writeln!(
            s,
            "[gpudbg]  FAULT_INFO1(0xb83090)={:#010x} valid={} hub={} access={}({}) client={:#x} reason={}({})",
            f_info1,
            f_info1 >> 31,
            (f_info1 >> 20) & 1,
            (f_info1 >> 16) & 0xf,
            fault_access_name((f_info1 >> 16) & 0xf),
            (f_info1 >> 8) & 0x7f,
            f_info1 & 0x1f,
            fault_reason_name(f_info1 & 0x1f),
        );
        if f_info1 & 0x8000_0000 != 0 {
            let addr_lo = rd(0x00b8_3080);
            let addr_hi = rd(0x00b8_3084);
            let info0 = rd(0x00b8_3088);
            let inst_hi = rd(0x00b8_308c);
            let _ = writeln!(
                s,
                "[gpudbg]  FAULT_VA={:#x}{:08x} engine_id={:#x} inst={:#x}{:08x}",
                addr_hi,
                addr_lo & 0xffff_f000,
                info0 & 0xff,
                inst_hi,
                info0 & 0xffff_f000,
            );
        }

        // --- Per-channel (PCCSR) + per-PBDMA status (read-only) ---
        let pccsr = rd(0x0080_0004);
        let _ = writeln!(
            s,
            "[gpudbg]  PCCSR0(0x800004)={:#010x} enable={} busy={} status={} pbdma_faulted={} eng_faulted={}",
            pccsr,
            pccsr & 1,
            (pccsr >> 28) & 1,
            (pccsr >> 24) & 0xf,
            (pccsr >> 22) & 1,
            (pccsr >> 23) & 1,
        );
        let _ = writeln!(
            s,
            "[gpudbg]  PCCSR0_INST(0x800000)={:#010x}",
            rd(0x0080_0000)
        );
        for i in 0..2u32 {
            let pb = 0x0004_0000 + i * 0x2000;
            let _ = writeln!(
                s,
                "[gpudbg]  PBDMA{} STATUS(0x{:x})={:#010x} CHANNEL={:#010x} GP_GET={:#010x} GP_PUT={:#010x} GET={:#010x} INTR_0={:#010x}",
                i,
                pb + 0x100,
                rd(pb + 0x100),
                rd(pb + 0x120),
                rd(pb + 0x14),
                rd(pb),
                rd(pb + 0x18),
                rd(pb + 0x108),
            );
        }
        // PBDMA0/1 above are not necessarily the PBDMA(s) that serve the CE's
        // runlist (discovered as PBDMA9 on the last real-hardware run). Dump
        // whichever PBDMA(s) NV_PFIFO_PBDMA_MAP actually routes the CE's
        // runlist to, so a stuck/never-armed PBDMA is visible without needing
        // the opt-in /proc/gpustep4.
        if let Some((ce_rl, _)) = self.find_ce_runlist() {
            for i in 0..12u32 {
                if i < 2 {
                    continue; // already shown above
                }
                let map = rd(0x0000_2390 + i * 4) & 0xffff;
                if map & (1 << ce_rl) == 0 {
                    continue;
                }
                let pb = 0x0004_0000 + i * 0x2000;
                let _ = writeln!(
                    s,
                    "[gpudbg]  PBDMA{}(serves CE runl{}) STATUS(0x{:x})={:#010x} CHANNEL={:#010x} GP_GET={:#010x} GP_PUT={:#010x} GET={:#010x} INTR_0={:#010x}",
                    i,
                    ce_rl,
                    pb + 0x100,
                    rd(pb + 0x100),
                    rd(pb + 0x120),
                    rd(pb + 0x14),
                    rd(pb),
                    rd(pb + 0x18),
                    rd(pb + 0x108),
                );
            }
        }

        // --- Engine -> runlist map (NV_PTOP_DEVICE_INFO 0x022700, read-only) ---
        // Walk the device-info table; dump non-zero raw entries so we can decode
        // which runlist owns the copy engines.
        let _ = writeln!(s, "[gpudbg]  --- PTOP device-info (0x022700, non-zero) ---");
        for i in 0..64u32 {
            let e = rd(0x0002_2700 + i * 4);
            if e != 0 {
                let _ = writeln!(s, "[gpudbg]  DEVINFO[{:2}]={:#010x}", i, e);
            }
        }

        // --- Step 1: build the GMMU tables in RAM and dump them (no GPU writes) ---
        {
            let mut g = self.bringup.lock();
            if g.is_none() {
                // GPU VA base for the packed 2 MiB region (avoids null-VA).
                *g = GpuBringup::build(0x0020_0000, 0x0300_0000);
            }
            match g.as_ref() {
                Some(b) => s.push_str(&b.dump()),
                None => {
                    let _ = writeln!(
                        s,
                        "[gpudbg]  GMMU: alloc_coherent FAILED (DMA pool exhausted)"
                    );
                }
            }
        }

        // The last GL/Vulkan client draw-submit outcome, readable from the
        // terminal that launched the client (`cat /proc/gpudbg`) -- no dmesg.
        s.push_str(&super::nouveau_uapi::format_last_exec());
        // Per-client GEM/VM_BIND memory summary: distinguishes a compressed PTE
        // kind mapped uncompressed from plain tiled sysmem when a client renders
        // structured garbage (e.g. vkcube) rather than crashing.
        s.push_str(&super::nouveau_uapi::format_client_mem());
        // Where a frame's kernel time goes: per-ioctl counts/latencies, the
        // direct-submit vs RM split, syncobj spin time, fence latency.
        s.push_str(&super::nouveau_uapi::format_exec_profile());

        s
    }

    /// Step 5 (`/proc/gpustep5`), NOT read-only and NOT part of `/proc/gpudbg`:
    /// first real invocation of the vendored RM core's own object
    /// construction (`nvidia_rm_sys::rm_init`, OBJSYS/resource-server/OBJGPU
    /// via NVOC). Moved out of `debug_dump` after it hung the machine on a
    /// plain `cat /proc/gpudbg` on real hardware -- this does real HAL
    /// bind/attach work, not a safe register read, so it gets its own
    /// deliberate opt-in trigger like bringup_step2/3/4. Cached after the
    /// first attempt so repeated reads don't re-run it.
    fn bringup_step5(&self) -> String {
        use core::fmt::Write;
        // TEMPORARY: absolute-first-line checkpoint, using the exact same
        // log::warn! mechanism already proven visible at driver-init time
        // ("[NVIDIA] GPU at ..."), bypassing nv_printf/C entirely -- two
        // real-hardware tests in a row (with confirmed-fresh binaries)
        // produced zero output even after fixing the info->warn level
        // bug, so this determines whether the function is even entered/
        // whether ANY print is visible from this exact call context
        // before reaching the lock or any real RM code.
        log::warn!("[NVIDIA] bringup_step5: entered");
        let bar0 = self._bar0;
        log::warn!("[NVIDIA] bringup_step5: read self._bar0 = {:#x}", { bar0 });
        let mut s = String::new();
        {
            // TEMPORARY chip-ID probe: read PMC_BOOT_0 (offset 0) and
            // PMC_BOOT_42 (offset 0xA00) directly through our mapped BAR0,
            // the exact registers RM's gpumgrGetGpuHalFactor reads to
            // identify the chip. gpumgrAttachGpu now returns 0x56
            // (NV_ERR_NOT_SUPPORTED) -- which is exactly what
            // halmgrGetHalForGpu returns when the chip ID matches no known
            // HAL, so the leading theory is our BAR0 reads don't return the
            // real chip ID. For a TU106 the real values are: PMC_BOOT_42
            // bits 29:24 (ARCHITECTURE) == 0x16 and the IMPLEMENTATION
            // nibble (bits 23:20) == 6. 0x0 or 0xFFFFFFFF here means BAR0
            // MMIO is not actually reaching the GPU (mapping/decode wrong),
            // which is the whole ballgame. Written into the RETURNED string
            // (not just log::warn) so it survives the RM init log spew and
            // is always visible in the `cat` output.
            let boot0 = unsafe { core::ptr::read_volatile(bar0 as *const u32) };
            let boot42 = unsafe { core::ptr::read_volatile((bar0 + 0xA00) as *const u32) };
            // PMC_BOOT_1 @ 0x4: gpuDetermineVirtualMode (gpu.c:4552) asserts
            // that the VGPU field (bits 17:16) read at attach time matches
            // the value read later through the IoAperture; a mismatch is the
            // 0x40 (NV_ERR_INVALID_STATE). _VF==0x2, _PV==0x1, _REAL==0x0;
            // a bare-metal PF TU106 must read _REAL (0x0) in bits 17:16.
            let boot1 = unsafe { core::ptr::read_volatile((bar0 + 0x4) as *const u32) };
            let arch = (boot42 >> 24) & 0x3F;
            let impl_ = (boot42 >> 20) & 0xF;
            let vgpu = (boot1 >> 16) & 0x3;
            let _ = writeln!(
                s,
                "[gpustep5]  BAR0 chip-ID probe: PMC_BOOT_0={:#010x} PMC_BOOT_42={:#010x} \
                 (arch={:#x} impl={:#x}; TU106 expects arch=0x16 impl=0x6)",
                boot0, boot42, arch, impl_
            );
            let _ = writeln!(
                s,
                "[gpustep5]  PMC_BOOT_1={:#010x} VGPU(bits17:16)={:#x} \
                 (0=REAL/bare-metal, 1=PV, 2=VF; bare-metal PF must be 0)",
                boot1, vgpu
            );
            log::warn!(
                "[NVIDIA] bringup_step5: BAR0 probe: PMC_BOOT_0={:#010x} \
                 PMC_BOOT_42={:#010x} PMC_BOOT_1={:#010x} (arch={:#x} impl={:#x} vgpu={:#x})",
                boot0,
                boot42,
                boot1,
                arch,
                impl_,
                vgpu
            );
        }

        // The /proc read is served by seq_read_at, which re-invokes this
        // generator for EVERY chunk `cat` requests. So the returned String
        // must be byte-for-byte identical across calls: cat's first read
        // (offset 0) runs the attach and yields the full string incl.
        // narration; its second read (offset = first-chunk length) calls us
        // again. If that second string is a different length -- which it was
        // when the narration only got appended on the non-cached path -- the
        // offset lands past its end, read returns 0/EOF, and the output is
        // truncated mid-line (exactly what hid the RM narration and the
        // real result last run). The BAR0 probe above is deterministic; the
        // attach + narration is not (runs once, then cached), so cache the
        // ENTIRE post-probe block -- narration and result line together --
        // and emit it verbatim on every call.
        log::warn!("[NVIDIA] bringup_step5: checking cached result");
        let cached = self.rm_attach_result.lock().clone();
        log::warn!(
            "[NVIDIA] bringup_step5: cache check done, cached={}",
            cached.is_some()
        );

        let block = if let Some(cached) = cached {
            cached
        } else {
            // Capture the RM's own nv_printf / assert / ECLIPSE_TRACE
            // narration into an in-memory buffer for the duration of core
            // init + attach. On this bring-up box the kernel `log::warn!`
            // stream never reaches the monitor -- only this returned String
            // (the `cat /proc/gpustep5` stdout) does -- so folding the RM's
            // narration in here is the only way it's actually visible. The
            // RmMsg rule set in eclipse_rm_init_core makes gpu.c/gpu_mgr.c
            // narrate every step, so the last captured line pins where a
            // graceful failure (e.g. 0x40) originates inside gpumgrAttachGpu.
            nvidia_rm_sys::os_interface::capture_begin();
            let core_status = rm_core_init_once();
            let computed = if core_status != 0 {
                alloc::format!("eclipse_rm_init_core FAILED, NV_STATUS={:#x}", core_status)
            } else {
                match nvidia_rm_sys::rm_init::attach_gpu(
                    self.pci_domain,
                    self.pci_bus,
                    self.pci_device,
                    self.bar0_phys,
                    bar0 as *mut core::ffi::c_void,
                    self.bar0_len,
                    self.bar1_phys,
                    self.effective_vram_mb() as u64 * 1024 * 1024,
                    self.bar2_phys,
                    self.bar2_len,
                ) {
                    Ok(device_instance) => {
                        *self.rm_device_instance.lock() = Some(device_instance);
                        alloc::format!("gpumgrAttachGpu OK, deviceInstance={}", device_instance)
                    }
                    Err(status) => {
                        alloc::format!("gpumgrAttachGpu FAILED, NV_STATUS={:#x}", status)
                    }
                }
            };
            let captured = nvidia_rm_sys::os_interface::capture_take();
            // Build the full post-probe block: captured RM narration first
            // (each line prefixed for the `cat` reader), then the result.
            let mut block = String::new();
            if let Some(log) = captured {
                if !log.is_empty() {
                    let _ = writeln!(block, "[gpustep5]  --- RM narration (captured) ---");
                    for line in log.lines() {
                        let _ = writeln!(block, "[gpustep5]  | {}", line);
                    }
                    let _ = writeln!(block, "[gpustep5]  --- end RM narration ---");
                }
            }
            let _ = writeln!(block, "[gpustep5]  --- Real RM attach: {} ---", computed);
            // Publish; harmless if two callers race here (single-shell
            // manual testing only today) since both compute the same block
            // and either write wins.
            let mut attach = self.rm_attach_result.lock();
            if attach.is_none() {
                *attach = Some(block.clone());
            }
            block
        };

        s.push_str(&block);
        s
    }

    /// Step 7: read back the `GspStaticConfigInfo` the live GSP-RM returned
    /// during step 6's GET_GSP_STATIC_INFO RPC. Pure readback -- no RPCs, no
    /// register writes -- so it is safe to run any number of times. All-zero
    /// name means step 6 has not completed on this GPU.
    fn bringup_step7(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from("[gpustep7]  skipped (run /proc/gpustep5 (RM attach) first)\n");
        };
        match nvidia_rm_sys::rm_init::get_gsp_info(device_instance) {
            Ok(info) => {
                let name_len = info.gpu_name.iter().position(|&b| b == 0).unwrap_or(64);
                let short_len = info
                    .gpu_short_name
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(64);
                let name = core::str::from_utf8(&info.gpu_name[..name_len]).unwrap_or("<non-utf8>");
                let short =
                    core::str::from_utf8(&info.gpu_short_name[..short_len]).unwrap_or("<non-utf8>");
                if name.is_empty() {
                    let _ = writeln!(
                        s,
                        "[gpustep7]  GSP static info is all zeros -- GSP-RM not booted on this GPU yet (run /proc/gpustep6)"
                    );
                } else {
                    let _ = writeln!(s, "[gpustep7]  --- Firmware-reported GPU info (from live GSP-RM via GET_GSP_STATIC_INFO) ---");
                    let _ = writeln!(s, "[gpustep7]  GPU name:   {}", name);
                    let _ = writeln!(s, "[gpustep7]  Short name: {}", short);
                    let _ = writeln!(
                        s,
                        "[gpustep7]  VRAM:       {} MiB ({} bytes), bus width {} bits, ram type {}",
                        info.fb_length / (1024 * 1024),
                        info.fb_length,
                        info.fb_bus_width,
                        info.fb_ram_type
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep7]  L2 cache:   {} KiB",
                        info.l2_cache_size / 1024
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep7]  VBIOS:      valid={} subvendor={:#06x} subdevice={:#06x}",
                        info.vbios_valid != 0,
                        info.vbios_sub_vendor,
                        info.vbios_sub_device
                    );
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep7]  eclipse_rm_get_gsp_info FAILED, NV_STATUS={:#x}",
                    status
                );
            }
        }
        s
    }

    /// Step 8: three read-only RM API controls answered by the live GSP-RM's
    /// resource server (GSP_RM_CONTROL RPC): GPU name, GID/UUID, FB heap
    /// total/free. heap_free is dynamic firmware bookkeeping -- proof of a
    /// live, working RM API path end-to-end. Safe to run repeatedly.
    fn bringup_step8(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from("[gpustep8]  skipped (run /proc/gpustep5 (RM attach) first)\n");
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::rm_api_demo(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep8]  | {}", line);
            }
        }
        match result {
            Ok(demo) => {
                let _ = writeln!(s, "[gpustep8]  --- RM API controls served by live GSP-RM (GSP_RM_CONTROL RPC) ---");
                if demo.name_status == 0 {
                    let n = demo.name.iter().position(|&b| b == 0).unwrap_or(64);
                    let _ = writeln!(
                        s,
                        "[gpustep8]  GET_NAME_STRING: {}",
                        core::str::from_utf8(&demo.name[..n]).unwrap_or("<non-utf8>")
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "[gpustep8]  GET_NAME_STRING: NV_STATUS={:#x}",
                        demo.name_status
                    );
                }
                if demo.gid_status == 0 {
                    let n = (demo.gid_length as usize).min(demo.gid.len());
                    let _ = writeln!(
                        s,
                        "[gpustep8]  GET_GID_INFO (UUID): {}",
                        core::str::from_utf8(&demo.gid[..n]).unwrap_or("<non-utf8>")
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "[gpustep8]  GET_GID_INFO: NV_STATUS={:#x}",
                        demo.gid_status
                    );
                }
                if demo.fb_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep8]  FB_GET_INFO_V2: heap {} MiB total, {} MiB free, bus width {} bits",
                        demo.heap_size_kb / 1024,
                        demo.heap_free_kb / 1024,
                        demo.bus_width
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "[gpustep8]  FB_GET_INFO_V2: NV_STATUS={:#x}",
                        demo.fb_status
                    );
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep8]  eclipse_rm_step8 FAILED, NV_STATUS={:#x} (GSP not booted? run /proc/gpustep6)",
                    status
                );
            }
        }
        s
    }

    /// Step 9: gpuStatePreInit + gpuStateInit + gpuStateLoad -- the rest of
    /// the real RmInitAdapter device bring-up, run against the live GSP.
    /// One-shot per boot (the RM state machine is not re-runnable), so the
    /// whole block (captured narration + per-phase result) is cached and
    /// re-served on subsequent reads, like step 6.
    fn bringup_step9(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from("[gpustep9]  skipped (run /proc/gpustep5 (RM attach) first)\n");
        };
        let cached = self.state_init_result.lock().clone();
        let block = if let Some(cached) = cached {
            cached
        } else {
            nvidia_rm_sys::os_interface::capture_begin();
            // Live-echo state-init narration at ERROR level so it survives the
            // default LOG=error filter and lands on the console AS IT RUNS.
            // gpuStateInit currently faults on real hardware (NULL write at
            // vaddr 0x90); the captured buffer is only folded into this /proc
            // read on a clean return, so without live echo a fault prints
            // nothing and we can't see which engine died. The last live
            // "[nvidia-rm] ..." line before the panic pinpoints it.
            nvidia_rm_sys::os_interface::live_echo_begin();
            let result = nvidia_rm_sys::rm_init::state_init(device_instance);
            nvidia_rm_sys::os_interface::live_echo_end();
            let captured = nvidia_rm_sys::os_interface::capture_take();
            let mut block = String::new();
            if let Some(log) = captured {
                if !log.is_empty() {
                    let _ = writeln!(block, "[gpustep9]  --- state-init narration (captured) ---");
                    for line in log.lines() {
                        let _ = writeln!(block, "[gpustep9]  | {}", line);
                    }
                    let _ = writeln!(block, "[gpustep9]  --- end narration ---");
                }
            }
            let early_err = result.is_err();
            match result {
                Ok(r) => {
                    let phase = |st: u32| -> String {
                        match st {
                            0 => String::from("OK"),
                            0xFFFF_FFFF => String::from("not reached"),
                            e => alloc::format!("FAILED NV_STATUS={:#x}", e),
                        }
                    };
                    let _ = writeln!(
                        block,
                        "[gpustep9]  gpuStatePreInit: {}",
                        phase(r.pre_init_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep9]  gpuStateInit:    {}",
                        phase(r.init_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep9]  gpuStateLoad:    {}",
                        phase(r.load_status)
                    );
                    if r.pre_init_status == 0 && r.init_status == 0 && r.load_status == 0 {
                        let _ = writeln!(block, "[gpustep9]  --- FULL RmInitAdapter-equivalent bring-up COMPLETE: GPU is state-loaded ---");
                    }
                }
                Err(status) => {
                    let _ = writeln!(
                        block,
                        "[gpustep9]  eclipse_rm_state_init FAILED, NV_STATUS={:#x} (GSP not booted? run /proc/gpustep6)",
                        status
                    );
                }
            }
            // Cache only when the C call actually ran (Ok). An early Err --
            // e.g. "GSP not booted" from gpuinit's pass over the console GPU
            // -- has no RM side effects, and caching it shadowed the real
            // step14 attempt later in the same boot (r16 run: stage 4
            // replayed gpuinit's 0x40 even though step8 had just proven the
            // GSP live).
            if !early_err {
                let mut cache = self.state_init_result.lock();
                if cache.is_none() {
                    *cache = Some(block.clone());
                }
            }
            block
        };
        s.push_str(&block);
        s
    }

    /// Step 10 (`/proc/gpustep10`): first real DATA MOVEMENT through the
    /// copy engine on the state-loaded GPU -- CE memset of a pattern into
    /// vidmem buffer A (and a poison into B), CE copy A->B, then CPU
    /// readback of B through BAR2 verifying every dword. Uses the RM's own
    /// internal CeUtils channel (the VRAM scrubber's machinery), driving the
    /// exact doorbell path the step-9 osMapGPU fix repaired. Requires a
    /// successful step 9 first (gpuStateLoad OK).
    fn bringup_step10(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        // The console-GPU guard is gone: the SEC2-resume wedge that once
        // blocked its GSP boot is fixed (gsp_boot_run's console drain), so the
        // primary can be state-loaded and run the CE test like the secondary.
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep10] skipped (run steps 5/6/9 first: attach, GSP boot, state init)\n",
            );
        };
        let cached = self.step10_result.lock().clone();
        let block = if let Some(cached) = cached {
            cached
        } else {
            nvidia_rm_sys::os_interface::capture_begin();
            // Live-echo like step 9: CE submission exercises channel +
            // doorbell paths for the first time; if anything faults, the
            // last live line names the phase.
            nvidia_rm_sys::os_interface::live_echo_begin();
            let result = nvidia_rm_sys::rm_init::step10(device_instance);
            nvidia_rm_sys::os_interface::live_echo_end();
            let captured = nvidia_rm_sys::os_interface::capture_take();
            let mut block = String::new();
            if let Some(log) = captured {
                if !log.is_empty() {
                    let _ = writeln!(block, "[gpustep10] --- CE-test narration (captured) ---");
                    for line in log.lines() {
                        let _ = writeln!(block, "[gpustep10] | {}", line);
                    }
                    let _ = writeln!(block, "[gpustep10] --- end narration ---");
                }
            }
            let early_err = result.is_err();
            match result {
                Ok(r) => {
                    let phase = |st: u32| -> String {
                        match st {
                            0 => String::from("OK"),
                            0xFFFF_FFFF => String::from("not reached"),
                            e => alloc::format!("FAILED NV_STATUS={:#x}", e),
                        }
                    };
                    let _ = writeln!(
                        block,
                        "[gpustep10] buffers: {} KiB each, A PA={:#x} B PA={:#x} (VRAM)",
                        r.buffer_size / 1024,
                        r.pa_a,
                        r.pa_b
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep10] CeUtils channel:   {}",
                        phase(r.ce_utils_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep10] alloc A:           {}",
                        phase(r.alloc_a_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep10] alloc B:           {}",
                        phase(r.alloc_b_status)
                    );
                    // CE memset writes only the pattern's LOW BYTE replicated
                    // (SET_REMAP_COMPONENTS _COMPONENT_SIZE_ONE,
                    // channel_utils.c) -- spot checks in C already account
                    // for that; show the byte the hardware actually wrote.
                    let _ = writeln!(
                        block,
                        "[gpustep10] CE memset B (byte {:#04x}) + spot-check: {}",
                        r.poison & 0xFF,
                        phase(r.poison_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep10] CE memset A (byte {:#04x}) + spot-check: {}",
                        r.pattern & 0xFF,
                        phase(r.memset_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep10] CPU unique-fill A + CE copy A->B: {}",
                        phase(r.copy_status)
                    );
                    let _ = writeln!(
                        block,
                        "[gpustep10] CPU verify B (per-dword unique): {} ({} dwords checked, {} mismatches)",
                        phase(r.verify_status),
                        r.dwords_checked,
                        r.mismatch_count
                    );
                    if r.mismatch_count > 0 {
                        // Expected value mirrors the C's ECLIPSE_FILL(i).
                        let expected = r.pattern ^ r.first_mismatch_idx.wrapping_mul(0x0100_0193);
                        let _ = writeln!(
                            block,
                            "[gpustep10] first mismatch: dword {} = {:#010x} (expected {:#010x})",
                            r.first_mismatch_idx, r.first_mismatch_val, expected
                        );
                    }
                    if r.verify_status == 0 {
                        let _ = writeln!(
                            block,
                            "[gpustep10] --- COPY ENGINE DATA MOVEMENT VERIFIED: pattern written, copied and read back through real hardware ---"
                        );
                    }
                }
                Err(status) => {
                    let _ = writeln!(
                        block,
                        "[gpustep10] eclipse_rm_step10 FAILED, NV_STATUS={:#x} (state not loaded? run /proc/gpustep9)",
                        status
                    );
                }
            }
            // Same rule as step9: an early Err ran nothing in the RM, so
            // caching it would shadow a later real attempt in this boot.
            if !early_err {
                let mut cache = self.step10_result.lock();
                if cache.is_none() {
                    *cache = Some(block.clone());
                }
            }
            block
        };
        s.push_str(&block);
        s
    }

    /// Step 6 (`/proc/gpustep6`), NOT read-only and NOT part of `/proc/gpudbg`:
    /// first real invocation of `kgspInitRm` (kernel_gsp.c) -- the deepest,
    /// riskiest bring-up step yet (VBIOS/FWSEC extraction, Booter ucode
    /// secure boot on SEC2, WPR2 setup). Kept on its own explicit trigger,
    /// same reasoning as `bringup_step5`. Requires a successful
    /// `bringup_step5` first AND gsp.bin already pushed down by
    /// `set_gsp_firmware` (zCore's boot code, after rootfs mount) --
    /// reports which is missing rather than erroring if either is absent.
    fn bringup_step6(&self) -> String {
        let mut s = String::new();

        // EXPERIMENT (SEC2 CORE_RESUME wedge): starting SEC2 to resume GSP-RM
        // permanently wedges the GPU's bus interface on the CONSOLE GPU -- even
        // a raw BSI read after 500 ms of total MMIO silence never returns. The
        // software sequence is byte-for-byte what nouveau/Linux run successfully
        // on Turing, so the suspect is console-GPU-specific state: it is the
        // VBIOS-POSTed primary with GOP scanout live, and its BAR1 is being
        // written by this very console (GSP-RM's devinit sequencer may also
        // reconfigure apertures under our feet). The second RTX 2060 Super has
        // none of that baggage. So: boot GSP only on the GPU(s) NOT driving the
        // boot display. If the secondary boots clean, the driver stack is
        // proven end-to-end and the console-GPU collision is isolated as the
        // remaining problem (likely fix: stop console rendering during its GSP
        // boot). If the secondary wedges identically, the console theory dies.
        if self.drives_boot_display() {
            return String::from(
                "[gpustep6]  --- Real GSP-RM boot: SKIPPED on console GPU (SEC2 resume wedges its bus \
                 while the console renders into its BAR1; use /proc/gpustep11, which freezes the \
                 graphic console around the boot) ---\n",
            );
        }

        s.push_str(&self.gsp_boot_run("gpustep6", false));
        s
    }

    /// Step 11 (`/proc/gpustep11`): GSP-RM boot on the CONSOLE GPU -- the one
    /// step 6 refuses to touch. The wedge theory, refined by the secondary
    /// GPU booting flawlessly with byte-identical software: during the SEC2
    /// GSP-RM resume window, CPU writes into this GPU's BAR1 (which is
    /// exactly where the graphic console framebuffer lives -- and step 6's
    /// RmMsg narration prints DOZENS of lines, each one drawing pixels) stall
    /// the bus for good. NVIDIA's own driver avoids this class of collision
    /// with os_disable_console_access() around init. Eclipse's equivalent:
    /// the /proc/gpustep11 generator (linux-object procfs) puts the active VT
    /// into KD_GRAPHICS around this call -- pixel presentation stops (the VT
    /// shadow buffer keeps accumulating; serial/dmesg unaffected), and the
    /// return to KD_TEXT repaints everything that happened meanwhile.
    fn bringup_step11(&self) -> String {
        if !self.drives_boot_display() {
            return String::from(
                "[gpustep11] SKIPPED on secondary GPU (already boots via /proc/gpustep6)\n",
            );
        }
        let mut s = String::new();
        // Declare this GPU's real identity to RM BEFORE the GSP boot, exactly
        // where Linux does (RmDeterminePrimaryDevice /
        // RmSetConsolePreservationParams right before kgspInitRm): it is the
        // PRIMARY device with a live UEFI GOP console in its BAR1. Without
        // this, the SET_GUEST_SYSTEM_INFO RPC told GSP-RM `bIsPrimary=false`
        // and no console region was reserved -- the one remaining difference
        // vs. the (working) secondary GPU after the console-freeze experiment
        // exonerated CPU pixel writes. Idempotent: plain property/field
        // writes, safe to repeat on a cached re-read.
        if let Some(device_instance) = *self.rm_device_instance.lock() {
            let (console_size, at_bar1_base) = match *BOOT_FB_INFO.lock() {
                Some(fb) => (
                    fb.pitch as u64 * fb.height as u64,
                    fb.phys == self.bar1_phys,
                ),
                None => (0, false),
            };
            let mark = nvidia_rm_sys::rm_init::mark_console_gpu(
                device_instance,
                console_size,
                at_bar1_base,
            );
            match mark {
                Ok(()) => {
                    s.push_str(&alloc::format!(
                        "[gpustep11] console-GPU identity declared to RM (PRIMARY_DEVICE, console {} KiB, at BAR1 base: {})\n",
                        console_size / 1024,
                        at_bar1_base
                    ));
                }
                Err(status) => {
                    s.push_str(&alloc::format!(
                        "[gpustep11] mark_console_gpu FAILED, NV_STATUS={:#x} (continuing to boot anyway)\n",
                        status
                    ));
                }
            }
        }
        // EXPERIMENT (SEC2-RTOS resume wedge, round 3): the live trace showed
        // the console GPU boots GSP-RM fine all the way to the CPU sequencer's
        // CORE_RESUME -- Booter Load clean, RISC-V started, RUN_CPU_SEQUENCER
        // RPC received -- and then the FIRST BAR0 read after restarting SEC2
        // (whose VBIOS SEC2-RTOS/BSI payload runs display/VGA restore phases
        // on a PRIMARY device) never completes. NVIDIA's own primary-VGA
        // detection keys on PCI I/O decode (kbifIsPciIoAccessEnabled,
        // osinit.c:900) -- the one config-space difference vs. the (working)
        // secondary GPU, and one the GPU firmware can see through its own
        // config mirror. So: clear PCI COMMAND bit 0 (I/O Space Enable) for
        // the duration of the boot, making the console GPU indistinguishable
        // from a secondary to the SEC2-RTOS, and restore it afterwards.
        // Console rendering is untouched (BAR1 is MEM space, bit 1).
        let io_cmd_old = {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            use pci::Location;
            let loc = Location {
                bus: self.pci_bus,
                device: self.pci_device,
                function: 0,
            };
            let ops = &PortOpsImpl;
            let cmd = unsafe { PCI_ACCESS.read16(ops, loc, 0x04) };
            unsafe { PCI_ACCESS.write16(ops, loc, 0x04, cmd & !0x0001) };
            s.push_str(&alloc::format!(
                "[gpustep11] PCI I/O decode disabled for the boot (COMMAND {:#06x} -> {:#06x}; SEC2-RTOS should now see a non-primary GPU)\n",
                cmd,
                cmd & !0x0001
            ));
            cmd
        };
        // Full-chain legacy-VGA routing disable (device I/O decode above +
        // every bridge on the path here): the SEC2-RTOS display/VGA handoff
        // suspects legacy routing state; the earlier round only cleared the
        // function-level bit. Restored after the boot returns.
        let (bridge_log, bridges_changed) = self.set_path_vga_routing(true, &[]);
        s.push_str(&bridge_log);
        // Containment (root-port completion timeout) + post-STARTCPU bus
        // autopsy instrumentation -- see their doc comments.
        s.push_str(&self.arm_completion_timeout());
        nvidia_rm_sys::os_boundary::autopsy_arm(self.config_handle(), self.parent_config_handle());
        // Diagnostics stay on: live_echo lifts RM narration (and the
        // sequencer register trace, armed inside gsp_boot_run) to ERROR so
        // it renders live at LOG=error -- a wedge leaves the exact hanging
        // register access as the last line on screen.
        nvidia_rm_sys::os_interface::live_echo_begin();
        let boot = self.gsp_boot_run("gpustep11", false);
        nvidia_rm_sys::os_interface::live_echo_end();
        nvidia_rm_sys::os_boundary::autopsy_disarm();
        let (restore_log, _) = self.set_path_vga_routing(false, &bridges_changed);
        s.push_str(&restore_log);
        {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            use pci::Location;
            let loc = Location {
                bus: self.pci_bus,
                device: self.pci_device,
                function: 0,
            };
            let ops = &PortOpsImpl;
            // Restore the original COMMAND value, except INTx stays masked
            // (gsp_boot_run masked it; Eclipse is fully polled).
            unsafe { PCI_ACCESS.write16(ops, loc, 0x04, io_cmd_old | (1 << 10)) };
            s.push_str("[gpustep11] PCI I/O decode restored after boot\n");
        }
        s.push_str(&boot);
        s
    }

    /// Step 12 (`/proc/gpustep12`): EXP 1 -- console-GPU GSP boot with the
    /// DISPLAY ENGINE HELD IN RESET (PMC_ENABLE bit 30 cleared right after
    /// kgspCalculateFbLayout consumes NV_PDISP_VGA_WORKSPACE_BASE, via the
    /// register-shim trigger in os_boundary). Zero isochronous scanout
    /// traffic during the SEC2-RTOS resume: if the wedge is live-display FB
    /// fetch vs. the HS payload, this boot COMPLETES. THE SCREEN GOES DARK
    /// at the trigger and stays dark until reboot -- run it blind:
    ///   `cat /proc/gpustep12 > /r12.txt; sync`
    /// then hard-reset and read /r12.txt. Skip this experiment entirely if
    /// the step-11 preboot dump showed the primary's heads already
    /// SLEEP/frozen (theory pre-falsified).
    fn hw_dump(&self) -> String {
        self.hw_dump_impl()
    }

    fn bringup_step12(&self) -> String {
        if !self.drives_boot_display() {
            return String::from(
                "[gpustep12] SKIPPED on secondary GPU (already boots via /proc/gpustep6)\n",
            );
        }
        let mut s = String::new();
        s.push_str(
            "[gpustep12] EXP1c: PDISP reset ONLY for the SEC2-resume window, then restored at 'RISCV started' -- screen blanks then comes back; capture with `cat /proc/gpustep12 > /r12.txt; sync`\n",
        );
        // EXP1c: EXP1b proved being non-primary doesn't fix the
        // kgspWaitForRmInitDone timeout -- so PDISP-in-reset itself is what
        // stalls GSP-RM's init (it touches the display engine before
        // GSP_INIT_DONE). Fix: os_boundary holds PDISP in reset only across
        // the SEC2 HS-resume (the wedge window) and restores it on the
        // "RISCV started" narration marker, so GSP-RM finds the display alive.
        // Still non-primary for now to isolate the timeout fix.
        s.push_str(&self.arm_completion_timeout());
        nvidia_rm_sys::os_boundary::autopsy_arm(self.config_handle(), self.parent_config_handle());
        nvidia_rm_sys::os_boundary::pdisp_kill_arm();
        nvidia_rm_sys::os_interface::live_echo_begin();
        let boot = self.gsp_boot_run("gpustep12", false);
        nvidia_rm_sys::os_interface::live_echo_end();
        nvidia_rm_sys::os_boundary::pdisp_kill_disarm();
        nvidia_rm_sys::os_boundary::autopsy_disarm();
        s.push_str(&boot);
        s
    }

    /// Step 13 (`/proc/gpustep13`): EXP2 -- console-GPU GSP boot with a
    /// pre-STARTCPU interrupt-drain "pseudo-ISR service loop" (Copilot's
    /// leading hypothesis). Eclipse is 100% polled with INTx masked and no RM
    /// ISR, so during the SEC2 CORE_RESUME window a fabric/display interrupt
    /// the GPU raises is never serviced -- the prime suspect for the STARTCPU
    /// posted-write stall (flow-control credit exhaustion) that Linux, whose
    /// ISR drains it, never hits. Right before the STARTCPU store, os_boundary
    /// snapshots the CPU-facing top-level interrupt tree (ERROR level, survives
    /// the wedge -> names the pending vector) and write-1-to-clears the leaves
    /// until quiescent, then lets the store through. Unlike EXP1 it does NOT
    /// touch PDISP, so it can't break GSP-RM's early boot. Two outcomes, both
    /// useful in one boot: STARTCPU drains (autopsy runs, boot continues) =>
    /// hypothesis confirmed, drain is the fix; still wedges => the snapshot
    /// tells us exactly which interrupt Linux services in that window. The
    /// screen is untouched (no display reset) -- but capture to a file anyway
    /// in case STARTCPU still wedges:
    ///   `cat /proc/gpustep13 > /r13.txt; sync` then read /r13.txt.
    fn bringup_step13(&self) -> String {
        if !self.drives_boot_display() {
            return String::from(
                "[gpustep13] SKIPPED on secondary GPU (already boots via /proc/gpustep6)\n",
            );
        }
        let mut s = String::new();
        s.push_str(
            "[gpustep13] EXP3: pre-STARTCPU interrupt snapshot + UNCONDITIONAL W1C drain (classifies latched vs live-level source); no PDISP/display touch -- snapshot at ERROR survives a wedge; capture with `cat /proc/gpustep13 > /r13.txt; sync`\n",
        );
        // Same containment + autopsy instrumentation as step11/12 so the
        // post-STARTCPU physics are classified either way. The ONLY new
        // variable vs. a plain console boot is the interrupt drain armed below.
        s.push_str(&self.arm_completion_timeout());
        nvidia_rm_sys::os_boundary::autopsy_arm(self.config_handle(), self.parent_config_handle());
        nvidia_rm_sys::os_boundary::sec2_drain_arm();
        nvidia_rm_sys::os_interface::live_echo_begin();
        let boot = self.gsp_boot_run("gpustep13", false);
        nvidia_rm_sys::os_interface::live_echo_end();
        nvidia_rm_sys::os_boundary::sec2_drain_disarm();
        nvidia_rm_sys::os_boundary::autopsy_disarm();
        s.push_str(&boot);
        s
    }

    /// Step 14 (`/proc/gpustep14`): the CONSOLE GPU's full bring-up chained in
    /// one shot -- RM attach, GSP-RM boot (with the permanent console SEC2
    /// drain in gsp_boot_run), RM-client controls, gpuStatePreInit/Init/Load,
    /// and the copy-engine data-movement test -- so the primary reaches the
    /// same state-loaded, CE-verified state the secondary already has, in a
    /// single `cat`. Each sub-step is cached and live-echoed, so a wedge or
    /// failure at any stage leaves its trail on the console and in the capture.
    /// Blanks nothing and needs no display reset. Capture with
    /// `cat /proc/gpustep14 > /r14.txt; sync`. On the secondary GPU this is a
    /// no-op (it already boots via gpustep6 and runs 8/9/10 directly).
    fn bringup_step14(&self) -> String {
        if !self.drives_boot_display() {
            return String::from(
                "[gpustep14] SKIPPED on secondary GPU (use gpustep5/6/8/9/10 directly)\n",
            );
        }
        let mut s = String::new();
        s.push_str(
            "[gpustep14] === CONSOLE GPU full bring-up: attach -> GSP boot (drain) -> RM controls -> state-load -> CE ===\n",
        );
        // 1. RM attach (sets rm_device_instance). bringup_step5 is idempotent
        //    (cached); safe to always call -- it no-ops if already attached.
        if self.rm_device_instance.lock().is_none() {
            s.push_str("[gpustep14] --- stage 1: RM attach (gpustep5) ---\n");
            s.push_str(&self.bringup_step5());
        }
        // 1.5 REMOVED: do NOT declare PRIMARY_DEVICE/console to RM before the
        //    boot. Cross-build statistics over every console-GPU boot ever
        //    made: with mark_console_gpu (bIsPrimary=true + consoleMemSize in
        //    SET_SYSTEM_INFO -- all step11 runs and every step14 run since
        //    da884def): 0 successes in 9+ boots. WITHOUT it (bIsPrimary=false,
        //    no console reservation -- the pre-da884def step13/14 runs): 2
        //    successes in 3, INCLUDING the full attach->boot->state-load->CE
        //    chain with the console visibly still working afterwards. The
        //    mechanism matches the cross-cluster model exactly: declaring a
        //    primary/console GPU makes the SEC2-HS CORE_RESUME payload run
        //    its display/VGA/console-preservation path -- display-domain PRI
        //    traffic while the head is actively scanning, the precise
        //    SYS<->DISP forward-progress hazard that wedges the fabric. Linux
        //    tolerates it via something environmental we haven't identified;
        //    our polled bring-up doesn't need the reservation (the GSP's FB
        //    carving demonstrably left the scanout surface intact on the
        //    successful full-chain run). Revisit console preservation later,
        //    post-boot, if FB carving ever eats the console.
        // 2. GSP-RM boot. gsp_boot_run arms the console SEC2 drain internally
        //    now, so this is the proven path; cached after the first boot.
        s.push_str("[gpustep14] --- stage 2: GSP-RM boot (kgspInitRm, console-SILENT, Linux-parity STARTCPU, VGA decode off, PBUS pre-clear) ---\n");
        // Renounce legacy VGA decode for the boot, like Linux does at PCI
        // probe (nv-pci.c:855-858: vga_tryget + vga_set_legacy_decoding
        // VGA_RSRC_NONE): clear the function's I/O decode + every bridge VGA
        // routing bit on the path. Restored after the boot. Console rendering
        // is untouched (BAR1 is MEM space). With console-silence and the
        // Linux-parity STARTCPU bracket this makes the boot environment
        // converge on Linux's in every knob Linux is known to set.
        let io_cmd_old = {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            use pci::Location;
            let loc = Location {
                bus: self.pci_bus,
                device: self.pci_device,
                function: 0,
            };
            let ops = &PortOpsImpl;
            let cmd = unsafe { PCI_ACCESS.read16(ops, loc, 0x04) };
            unsafe { PCI_ACCESS.write16(ops, loc, 0x04, cmd & !0x0001) };
            s.push_str(&alloc::format!(
                "[gpustep14] PCI I/O decode disabled for the boot (COMMAND {:#06x} -> {:#06x})\n",
                cmd,
                cmd & !0x0001
            ));
            cmd
        };
        let (bridge_log, bridges_changed) = self.set_path_vga_routing(true, &[]);
        s.push_str(&bridge_log);
        // LOUD + the empirical 0058b6f4 bracket (tight leaf-W1C before the
        // store, NO display/priv-ring reads). The falsification ladder is
        // complete: console silence, PBUS unit clear, VGA decode off, MPS
        // normalization and full Linux byte-parity (empty bracket) ALL
        // wedged; the only two boots that ever survived STARTCPU ran the
        // tight-W1C bracket without the EXP4 display-cluster reads (r13 on
        // 359cef1e, full r14 chain on 0058b6f4). Mechanism is only partially
        // understood (a hub-leaf W1C posted immediately before the store,
        // plausibly flushing/fencing the PRI path across the SEC2 handoff),
        // but the correlation is 2-for-3 vs 0-for-everything-else, so run
        // the exact recipe with all the new pre-boot hygiene (console-mark,
        // MPS normalize, PBUS clear, VGA off) layered on top. gsp_boot_run
        // arms the drain for console GPUs when quiet=false.
        nvidia_rm_sys::os_interface::live_echo_begin();
        s.push_str(&self.gsp_boot_run("gpustep14", false));
        nvidia_rm_sys::os_interface::live_echo_end();
        let (restore_log, _) = self.set_path_vga_routing(false, &bridges_changed);
        s.push_str(&restore_log);
        {
            use crate::bus::pci::{PortOpsImpl, PCI_ACCESS};
            use pci::Location;
            let loc = Location {
                bus: self.pci_bus,
                device: self.pci_device,
                function: 0,
            };
            let ops = &PortOpsImpl;
            // Restore the original COMMAND value, except INTx stays masked
            // (gsp_boot_run masked it; Eclipse is fully polled).
            unsafe { PCI_ACCESS.write16(ops, loc, 0x04, io_cmd_old | (1 << 10)) };
            s.push_str("[gpustep14] PCI I/O decode restored after boot\n");
        }
        // 3-5. RM controls, state pre-init/init/load, CE data movement -- reuse
        //    the exact same code paths proven on the secondary GPU.
        s.push_str("[gpustep14] --- stage 3: RM API controls (gpustep8) ---\n");
        s.push_str(&self.bringup_step8());
        s.push_str("[gpustep14] --- stage 4: gpuStatePreInit/Init/Load (gpustep9) ---\n");
        s.push_str(&self.bringup_step9());
        s.push_str("[gpustep14] --- stage 5: copy-engine data movement (gpustep10) ---\n");
        s.push_str(&self.bringup_step10());
        s.push_str("[gpustep14] === console GPU bring-up chain complete (see per-stage results above) ===\n");
        s
    }

    /// CE-offloaded present: dumb buffer (sysmem) -> scanout FB (VRAM) via the
    /// persistent CeUtils channel. Called from the DRM `scanout()` per frame in
    /// place of the CPU blit, when the console GPU is state-loaded. Takes the RM
    /// locks internally (safe: `scanout()` holds no DRM lock here). Returns true
    /// only if the CE copy actually ran, so the caller can fall back to CPU.
    /// Automatic boot-time compute-GPU bring-up (see the `DrmScheme` trait
    /// doc). Runs the proven `/proc/gpustep5;6;8;9` chain on this GPU — but
    /// only if it does NOT drive the boot display. The console GPU is skipped
    /// unconditionally: its GSP-RM boot wedges at the SEC2 STARTCPU store
    /// (see `bringup_step6`), so it is never auto-booted; the compute GPU(s)
    /// are the reliable path and drive the console's scanout FB over PCIe P2P.
    fn auto_bringup_compute(&self) -> String {
        // The console GPU is never auto-booted (its GSP boot wedges at the SEC2
        // STARTCPU store) and drives the display fine via the GOP framebuffer.
        // Return nothing so the quiet boot path prints no line for it.
        if self.drives_boot_display() {
            return String::new();
        }
        // Run the proven state-load chain (the same sequence a user triggers as
        // `cat /proc/gpustep5;6;8;9`), executed once at boot before any
        // userspace or scanout touches RM (fixed RM thread-id 0, no concurrent
        // access -> no reentrancy hazard). The verbose per-step narration is
        // DISCARDED here -- it still lands in the /proc/gpustep* capture buffers
        // for debugging, but must not flood the desktop console. The caller
        // suppresses the driver's own log output around this call; all this
        // method emits is a single clean status line.
        let _ = self.bringup_step5();
        let _ = self.bringup_step6();
        let _ = self.bringup_step8();
        let _ = self.bringup_step9();
        // Compute channel (TSG + GPFIFO + TURING_COMPUTE_A). Idempotent; the
        // same ladder CHANNEL_ALLOC/NVK reuse. Running it here means
        // `ecl-compute saxpy` and the first GL client do not pay the ~500 ms
        // RM alloc on first touch.
        let _ = self.bringup_step16();
        let _ = self.bringup_step17();
        if self.rm_device_instance.lock().is_some() {
            alloc::format!(
                "GPU {:02x}:{:02x}.0 {} listo — present CE/P2P + canal de cómputo (card1)",
                self.pci_bus,
                self.pci_device,
                self.gpu_model,
            )
        } else {
            alloc::format!(
                "GPU {:02x}:{:02x}.0 {} sin aceleración de present — se usa copia por CPU",
                self.pci_bus,
                self.pci_device,
                self.gpu_model,
            )
        }
    }

    /// Leave this GPU cold for the next firmware POST (see the `DrmScheme`
    /// trait doc). Only a GPU we actually state-loaded carries a live GSP-RM /
    /// locked WPR2 that a warm reboot would strand; others are no-ops. Issues a
    /// PCIe Function Level Reset, which on Turing resets the engines and
    /// falcons and lets the next VBIOS devinit re-run cleanly.
    fn quiesce_for_reboot(&self) -> String {
        if self.rm_device_instance.lock().is_none() {
            return String::new();
        }
        if self.pcie_flr() {
            alloc::format!(
                "[gpureset] FLR emitido en {:02x}:{:02x}.0 (estado limpio para el POST)\n",
                self.pci_bus,
                self.pci_device,
            )
        } else {
            alloc::format!(
                "[gpureset] {:02x}:{:02x}.0 sin capacidad FLR; GPU sin resetear\n",
                self.pci_bus,
                self.pci_device,
            )
        }
    }

    fn ce_present_ready(&self) -> bool {
        // State-loaded is the whole condition. It used to also require
        // `!drives_boot_display()`, because the console GPU was never brought
        // up at all — but that made the predicate encode a bring-up policy
        // rather than a capability, so a console GPU brought up by
        // `nvidia.console_gpu` (or a manual `/proc/gpustep14`) still reported
        // "not ready" and the desktop kept presenting with the CPU.
        //
        // A state-loaded console GPU is in fact the BEST CE presenter of the
        // two: `ce_present` already has the console/FBMEM branch, and it
        // writes its own framebuffer, so the copy never crosses PCIe and
        // never depends on P2P surviving ACS/IOMMU.
        self.rm_device_instance.lock().is_some()
    }

    fn deferred_console_bringup(&self) -> String {
        if !self.drives_boot_display() {
            return String::new();
        }
        if self.rm_device_instance.lock().is_some() {
            return alloc::format!(
                "GPU consola {:02x}:{:02x}.0 {} ya state-loaded",
                self.pci_bus,
                self.pci_device,
                self.gpu_model,
            );
        }
        // One-shot: ensure_console_gpu_brought_up runs bringup_step14.
        self.ensure_console_gpu_brought_up();
        if self.rm_device_instance.lock().is_some() {
            alloc::format!(
                "GPU consola {:02x}:{:02x}.0 {} lista (bring-up diferido) — present CE local y cursor HW",
                self.pci_bus,
                self.pci_device,
                self.gpu_model,
            )
        } else {
            alloc::format!(
                "GPU consola {:02x}:{:02x}.0 {} sin RM tras bring-up diferido — se sigue con present por CPU y cursor software",
                self.pci_bus,
                self.pci_device,
                self.gpu_model,
            )
        }
    }

    fn ce_present(&self, src_sysmem_pa: u64, size: u64) -> bool {
        if src_sysmem_pa == 0 || size == 0 {
            return false;
        }
        // Stood down after a confirmed CE failure this boot -- fall straight to
        // the CPU blit without touching the RM (see CE_PRESENT_WEDGED).
        if CE_PRESENT_WEDGED.load(Ordering::Relaxed) {
            return false;
        }
        let device_instance = match *self.rm_device_instance.lock() {
            Some(d) => d,
            None => {
                // One-shot: this GPU cannot CE-copy because it never
                // state-loaded. When every GPU declines, cepresent degrades to
                // the CPU blit; without this line the degradation is silent.
                static NOT_LOADED_LOGGED: AtomicBool = AtomicBool::new(false);
                if !NOT_LOADED_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::klog_info!(
                        "[NVIDIA] ce_present: GPU {:02x}:{:02x}.0 not state-loaded -- cannot CE-copy",
                        self.pci_bus,
                        self.pci_device
                    );
                }
                return false;
            }
        };
        let fb_phys = match boot_fb_phys() {
            Some(p) if p != 0 => p,
            _ => {
                static NO_BOOTFB_LOGGED: AtomicBool = AtomicBool::new(false);
                if !NO_BOOTFB_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::klog_warn!(
                        "[NVIDIA] ce_present: boot framebuffer phys unknown -- cannot CE-copy"
                    );
                }
                return false;
            }
        };
        // Capture the RM's own `[eclipse-rm-trace] ce_blit*` narration (the
        // ceutilsMemcopy status, and any Xid/MMU-fault burst the P2P write
        // triggers) so a failure REPLAYS it through klog instead of vanishing
        // behind a bare `false`. Time the call too: a synchronous full-frame
        // copy that runs long is either the bottleneck (efficiency) or a hang
        // (destabilization) -- the number tells them apart.
        nvidia_rm_sys::os_interface::capture_begin();
        let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };
        let (st, how) = if self.drives_boot_display() {
            // Console GPU: its own CE writes its own VRAM (ADDR_FBMEM). Direct,
            // but only when the console GPU is state-loaded (its bring-up is
            // unreliable), so this rarely fires in practice.
            let bar1 = self.bar1_phys;
            if fb_phys < bar1 {
                let _ = nvidia_rm_sys::os_interface::capture_take();
                static FB_OUTSIDE_BAR1_LOGGED: AtomicBool = AtomicBool::new(false);
                if !FB_OUTSIDE_BAR1_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::klog_warn!(
                        "[NVIDIA] ce_present: boot FB {:#x} below console BAR1 {:#x} -- cannot CE-copy",
                        fb_phys,
                        bar1
                    );
                }
                return false;
            }
            (
                nvidia_rm_sys::rm_init::ce_blit(
                    device_instance,
                    fb_phys - bar1,
                    src_sysmem_pa,
                    size,
                ),
                "console/FBMEM",
            )
        } else {
            // Compute GPU: P2P copy into the console GPU's scanout FB (its BAR1
            // host physical address, ADDR_SYSMEM). The reliable path — the
            // compute GPU always boots. Depends on PCIe P2P not being ACS-blocked.
            (
                nvidia_rm_sys::rm_init::ce_blit_p2p(device_instance, fb_phys, src_sysmem_pa, size),
                "compute/P2P",
            )
        };
        let elapsed_us = unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0);
        let narration = nvidia_rm_sys::os_interface::capture_take();
        if st == 0 {
            if !CE_PRESENT_LOGGED.swap(true, Ordering::Relaxed) {
                crate::klog_info!(
                    "[NVIDIA] CE-offload present ACTIVE ({}): src {:#x} -> FB {:#x} size {:#x} in {}us -- desktop composited by the copy engine, CPU freed",
                    how,
                    src_sysmem_pa,
                    fb_phys,
                    size,
                    elapsed_us
                );
            }
            // A full-frame P2P copy should be well under a 60 Hz frame (~16 ms).
            // If it isn't, the copy -- not the CPU -- is now the bottleneck; say
            // so ONCE so the efficiency win can be judged against real numbers.
            if elapsed_us > 8_000 {
                static SLOW_LOGGED: AtomicBool = AtomicBool::new(false);
                if !SLOW_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::klog_warn!(
                        "[NVIDIA] CE-offload present SLOW: {}us for {:#x} bytes ({}) -- the copy is the bottleneck, not the CPU",
                        elapsed_us,
                        size,
                        how
                    );
                }
            }
            true
        } else {
            // Confirmed failure. Name it, replay the RM trace, and LATCH OFF so
            // the desktop degrades to the CPU blit instead of repeating a
            // faulting/hung CE op every frame. The wait is outside RmGate now,
            // but a wedged CE is still abandoned for the rest of the boot.
            CE_PRESENT_WEDGED.store(true, Ordering::Relaxed);
            crate::klog_warn!(
                "[NVIDIA] CE-offload present FAILED ({}): ceutilsMemcopy status={:#x} (src={:#x} dst_fb={:#x} size={:#x}) in {}us -- latching OFF, CPU blit for the rest of this boot",
                how,
                st,
                src_sysmem_pa,
                fb_phys,
                size,
                elapsed_us
            );
            if let Some(text) = narration {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    crate::klog_warn!("[NVIDIA] CE rm: {}", line);
                }
            }
            false
        }
    }

    /// Pitched 2D CE present: copy `line_count` rows of `row_bytes` bytes from
    /// `src_sysmem_pa + r * src_pitch` into the console GPU's scanout FB at
    /// `dst_byte_offset + r * dst_pitch` over PCIe P2P — without any CPU
    /// staging repack. `dst_byte_offset` is non-zero when the caller is
    /// presenting a damage rectangle rather than a whole frame.
    /// Uses the same `CE_PRESENT_WEDGED` latch as [`ce_present`].
    fn ce_present_2d_pitched(
        &self,
        src_sysmem_pa: u64,
        src_pitch: u32,
        dst_byte_offset: u64,
        dst_pitch: u32,
        row_bytes: u32,
        line_count: u32,
    ) -> bool {
        if src_sysmem_pa == 0 || row_bytes == 0 || line_count == 0 {
            return false;
        }
        if CE_PRESENT_WEDGED.load(Ordering::Relaxed) {
            return false;
        }
        let device_instance = match *self.rm_device_instance.lock() {
            Some(d) => d,
            None => return false,
        };
        let fb_phys = match boot_fb_phys() {
            Some(p) if p != 0 => p,
            _ => return false,
        };
        // The destination the engine actually writes: the scanout base biased
        // into the damaged region. Checked for overflow rather than wrapped --
        // a bad offset here would have the CE DMA into whatever follows the
        // framebuffer.
        //
        // The last row needs `row_bytes`, not a whole `dst_pitch`: charging it
        // a full stride would reject every damage rect touching the bottom row
        // at a non-zero x, since `(y + h) * pitch + x * 4` overshoots a
        // framebuffer that is exactly `pitch * height` bytes.
        let last_row_start = (dst_pitch as u64).saturating_mul(line_count.saturating_sub(1) as u64);
        let dst_end = dst_byte_offset
            .saturating_add(last_row_start)
            .saturating_add(row_bytes as u64);
        let Some(dst_phys) = fb_phys.checked_add(dst_byte_offset) else {
            return false;
        };
        if dst_end > self.info.fb_size as u64 {
            static CE_2D_OOB: AtomicBool = AtomicBool::new(false);
            if !CE_2D_OOB.swap(true, Ordering::Relaxed) {
                crate::klog_warn!(
                    "[NVIDIA] CE-offload present 2D: dst offset {:#x} + {} rows (pitch {}, {} B/row) ends at {:#x}, past FB size {:#x} -- declining",
                    dst_byte_offset, line_count, dst_pitch, row_bytes, dst_end, self.info.fb_size
                );
            }
            return false;
        }

        nvidia_rm_sys::os_interface::capture_begin();
        let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };

        let (st, how) = if self.drives_boot_display() {
            let bar1 = self.bar1_phys;
            if fb_phys < bar1 {
                let _ = nvidia_rm_sys::os_interface::capture_take();
                return false;
            }
            (
                nvidia_rm_sys::rm_init::ce_blit_p2p_2d(
                    device_instance,
                    dst_phys,
                    dst_pitch,
                    src_sysmem_pa,
                    src_pitch,
                    row_bytes,
                    line_count,
                ),
                "console/FBMEM-2D",
            )
        } else {
            (
                nvidia_rm_sys::rm_init::ce_blit_p2p_2d(
                    device_instance,
                    dst_phys,
                    dst_pitch,
                    src_sysmem_pa,
                    src_pitch,
                    row_bytes,
                    line_count,
                ),
                "compute/P2P-2D",
            )
        };

        let elapsed_us = unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0);
        let narration = nvidia_rm_sys::os_interface::capture_take();

        if st == 0 {
            static CE_2D_P_LOGGED: AtomicBool = AtomicBool::new(false);
            if !CE_2D_P_LOGGED.swap(true, Ordering::Relaxed) {
                crate::klog_info!(
                    "[NVIDIA] CE-offload present ACTIVE ({}): src {:#x} src_pitch={} dst_pitch={} -> FB {:#x} {}x{}rows in {}us",
                    how,
                    src_sysmem_pa,
                    src_pitch,
                    dst_pitch,
                    fb_phys,
                    row_bytes,
                    line_count,
                    elapsed_us
                );
            }
            if elapsed_us > 8_000 {
                static CE_2D_P_SLOW_LOGGED: AtomicBool = AtomicBool::new(false);
                if !CE_2D_P_SLOW_LOGGED.swap(true, Ordering::Relaxed) {
                    crate::klog_warn!(
                        "[NVIDIA] CE-offload present SLOW ({}): {}us for {}x{}B rows (src_pitch={} dst_pitch={})",
                        how,
                        elapsed_us,
                        line_count,
                        row_bytes,
                        src_pitch,
                        dst_pitch
                    );
                }
            }
            true
        } else {
            CE_PRESENT_WEDGED.store(true, Ordering::Relaxed);
            crate::klog_warn!(
                "[NVIDIA] CE-offload present 2D FAILED ({}): status={:#x} (src={:#x} src_pitch={} dst_pitch={} {}x{}rows) in {}us -- latching CE OFF",
                how,
                st,
                src_sysmem_pa,
                src_pitch,
                dst_pitch,
                line_count,
                row_bytes,
                elapsed_us
            );
            if let Some(text) = narration {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    crate::klog_warn!("[NVIDIA] CE rm: {}", line);
                }
            }
            false
        }
    }

    /// `/proc/gpucefill`: CE-offload visual test. On the state-loaded console
    /// GPU, CE-memset the scanout framebuffer to a solid colour via the
    /// persistent CeUtils channel (`eclipse_rm_ce_fill_fb`). If the screen turns
    /// that colour, the BAR1->VRAM offset (`fb_phys - bar1_phys`) is correct and
    /// the CE can drive the display — the green light to wire the full per-frame
    /// `ce_blit` present path. The low byte of the pattern is what the CE writes
    /// (byte-remap), so a replicated-byte colour (here 0xFF -> white) results.
    fn bringup_ce_fill_fb(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        if !self.drives_boot_display() {
            return String::from(
                "[gpucefill] SKIPPED on secondary GPU (it has no scanout framebuffer)\n",
            );
        }
        let device_instance = match *self.rm_device_instance.lock() {
            Some(d) => d,
            None => {
                return String::from(
                    "[gpucefill] skipped: console GPU not state-loaded -- run `cat /proc/gpustep14` first\n",
                );
            }
        };
        let fb_phys = match boot_fb_phys() {
            Some(p) if p != 0 => p,
            _ => {
                return String::from("[gpucefill] no boot framebuffer physical address recorded\n")
            }
        };
        let bar1 = self.bar1_phys;
        if fb_phys < bar1 {
            let _ = writeln!(
                s,
                "[gpucefill] fb_phys {:#x} < bar1_phys {:#x} -- unexpected; aborting (would underflow the VRAM offset)",
                fb_phys, bar1
            );
            return s;
        }
        let fb_vram_offset = fb_phys - bar1;
        let size = (self.info.pitch as u64) * (self.info.height as u64);
        // Low byte replicated by the CE: 0xFF -> every byte 0xFF -> white.
        let pattern: u32 = 0x0000_00FF;
        let _ = writeln!(
            s,
            "[gpucefill] fb_phys={:#x} bar1_phys={:#x} => fb_vram_offset={:#x}  size={:#x} ({}x{} pitch {})  pattern={:#x} (low byte -> white)",
            fb_phys, bar1, fb_vram_offset, size, self.info.width, self.info.height, self.info.pitch, pattern
        );
        let st = nvidia_rm_sys::rm_init::ce_fill_fb(device_instance, fb_vram_offset, size, pattern);
        let _ = writeln!(
            s,
            "[gpucefill] ce_fill_fb -> {:#x} ({})",
            st,
            if st == 0 {
                "OK -- if the screen is now WHITE, the VRAM offset is correct and the CE drives the display"
            } else {
                "FAILED -- CE submit did not complete"
            }
        );
        s
    }

    /// `/proc/gpucefillp2p`: P2P CE-offload visual test. On the state-loaded
    /// COMPUTE GPU (the reliable one), CE-memset the CONSOLE GPU's scanout
    /// framebuffer to white via PCIe peer-to-peer (`eclipse_rm_ce_fill_fb_p2p`
    /// with dst = the console FB's host physical address). If the screen turns
    /// white, PCIe P2P works and we can drive the display from the compute GPU
    /// without ever bringing up the flaky console GPU — the whole point of
    /// via-A. If the CE returns OK but the screen does NOT change, P2P is
    /// ACS-blocked. Requires the compute GPU state-loaded (its own bring-up).
    fn bringup_ce_fill_fb_p2p(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        if self.drives_boot_display() {
            return String::from(
                "[gpucefillp2p] skipped on the CONSOLE GPU -- this test drives it FROM the compute GPU via P2P\n",
            );
        }
        let device_instance = match *self.rm_device_instance.lock() {
            Some(d) => d,
            None => {
                return String::from(
                    "[gpucefillp2p] compute GPU not state-loaded -- bring it up first (gpustep5/6/8/9 on the secondary)\n",
                );
            }
        };
        let fb_phys = match boot_fb_phys() {
            Some(p) if p != 0 => p,
            _ => {
                return String::from(
                    "[gpucefillp2p] no boot framebuffer physical address recorded\n",
                )
            }
        };
        let size = match boot_fb_size() {
            Some(s) if s != 0 => s,
            _ => return String::from("[gpucefillp2p] no boot framebuffer size recorded\n"),
        };
        let pattern: u32 = 0x0000_00FF; // low byte -> white
        let _ = writeln!(
            s,
            "[gpucefillp2p] compute GPU instance={} -> console FB host_pa={:#x} size={:#x} pattern={:#x} (P2P)",
            device_instance, fb_phys, size, pattern
        );
        let st = nvidia_rm_sys::rm_init::ce_fill_fb_p2p(device_instance, fb_phys, size, pattern);
        let _ = writeln!(
            s,
            "[gpucefillp2p] ce_fill_fb_p2p -> {:#x} ({})",
            st,
            if st == 0 {
                "CE OK -- if the screen is now WHITE, PCIe P2P works and the compute GPU can drive the display"
            } else {
                "FAILED -- CE submit did not complete"
            }
        );
        if st == 0 {
            s.push_str("[gpucefillp2p] NOTE: CE OK but screen UNCHANGED => P2P is ACS-blocked (writes routed away from the console BAR1)\n");
        }
        s
    }

    /// `/proc/gpusurvive`: read + clear the CMOS survival breadcrumb from the
    /// previous console-GPU boot attempt. Only the console GPU reports (it is
    /// the one that wedges, and the breadcrumb is global — a second reader would
    /// just see the already-cleared slate).
    fn survival_report(&self) -> String {
        if self.drives_boot_display() {
            nvidia_rm_sys::survival::read_report_and_clear()
        } else {
            String::new()
        }
    }

    /// Step 15 (`/proc/gpustep15`): probe the GR (graphics/compute) engine's
    /// shader config on a state-loaded GPU via the live GSP-RM's resource
    /// server (GR_GET_GPC_MASK / GR_GET_TPC_MASK controls) -- the first read of
    /// the SM array the compute engine runs on. Read-only, repeatable, no
    /// channel machinery; groundwork toward a real compute launch. Works on
    /// any GPU that has completed state-load (secondary via gpustep9, console
    /// via gpustep14).
    fn bringup_step15(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep15] skipped (bring the GPU up first: gpustep5/6/8/9 on the secondary, or gpustep14 on the console)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step15(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep15] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            if st == 0 {
                String::from("OK")
            } else {
                alloc::format!("NV_STATUS={:#x}", st)
            }
        };
        match result {
            Ok(gr) => {
                let _ = writeln!(
                    s,
                    "[gpustep15] --- GR (graphics/compute) engine config from live GSP-RM ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep15] GR_GET_GPC_MASK: {} mask={:#010x} ({} GPCs)",
                    phase(gr.gpc_mask_status),
                    gr.gpc_mask,
                    gr.num_gpc
                );
                if gr.gpc_mask_status == 0 {
                    for gpc in 0..8usize {
                        if (gr.gpc_mask >> gpc) & 1 == 1 {
                            let _ = writeln!(
                                s,
                                "[gpustep15]   GPC{}: {} TPCs",
                                gpc, gr.per_gpc_tpc[gpc]
                            );
                        }
                    }
                    let _ = writeln!(
                        s,
                        "[gpustep15] GR_GET_TPC_MASK: {}",
                        phase(gr.tpc_mask_status)
                    );
                    // Turing packs TWO SMs per TPC (Volta+; the 1-SM/TPC layout
                    // was consumer Pascal). RTX 2060 Super: 17 TPCs => 34 SMs.
                    let _ = writeln!(
                        s,
                        "[gpustep15] --- {} TPCs total => {} usable SMs (Turing: 2 SMs/TPC) ---",
                        gr.total_tpc,
                        gr.total_tpc * 2
                    );
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep15] eclipse_rm_step15 FAILED, NV_STATUS={:#x} (GR not state-loaded? run gpustep9 or gpustep14)",
                    status
                );
            }
        }
        // Interrupt kernel table: the GSP's own authoritative vector->engine
        // map (the same control kernel RM uses to build its interrupt table:
        // NV2080_CTRL_CMD_INTERNAL_INTR_GET_KERNEL_TABLE). Settles empirically
        // which engine owns CPU vector 156 (the LEAF[4] bit28 level source
        // behind the console GPU's SEC2 wedge) and which engine drives legacy
        // PMC mask 0x10000000 -- research says PBUS for both; this is the
        // ground truth from this exact GPU.
        fn engine_name(idx: u32) -> &'static str {
            match idx {
                0 => "NULL",
                1 => "TMR",
                2 => "DISP",
                3 => "FB",
                4 => "FIFO",
                7 => "BUS",
                8 => "PMGR",
                11 => "BIF",
                13 => "PRIVRING",
                14 => "PMU",
                15 => "CE0",
                16 => "CE1",
                17 => "CE2",
                18 => "CE3",
                19 => "CE4",
                20 => "CE5",
                43 => "LTC",
                44 => "FBHUB",
                45 => "HDACODEC",
                46 => "GMMU",
                47 => "SEC2",
                49 => "NVLINK",
                50 => "GSP",
                59 => "REPLAYABLE_FAULT",
                60 => "ACCESS_CNTR",
                61 => "NON_REPLAYABLE_FAULT",
                64 => "INFO_FAULT",
                65 => "NVDEC0",
                73 => "CPU_DOORBELL",
                74 => "PRIV_DOORBELL",
                75 => "MMU_ECC_ERROR",
                77 => "PERFMON",
                84 => "GR0",
                156 => "GR_FECS_LOG",
                164 => "TMR_SWRL",
                165 => "DISP_GSP",
                166 => "REPLAYABLE_FAULT_CPU",
                167 => "NON_REPLAYABLE_FAULT_CPU",
                _ => "?",
            }
        }
        match nvidia_rm_sys::rm_init::intr_table(device_instance) {
            Ok(t) => {
                if t.ctrl_status != 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep15] INTR_GET_KERNEL_TABLE control FAILED, NV_STATUS={:#x} (table below is empty)",
                        t.ctrl_status
                    );
                }
                let _ = writeln!(
                    s,
                    "[gpustep15] --- GSP interrupt kernel table ({} entries; rows with a vector or legacy PMC mask; >>> = vector 156 or mask 0x10000000) ---",
                    t.table_len
                );
                for e in t.entries.iter().take(t.table_len as usize) {
                    let hot = e.vector_stall == 156
                        || e.vector_non_stall == 156
                        || e.pmc_intr_mask & 0x1000_0000 != 0;
                    let has_vec = e.vector_stall != u32::MAX || e.vector_non_stall != u32::MAX;
                    if hot || e.pmc_intr_mask != 0 || has_vec {
                        let vs = if e.vector_stall == u32::MAX {
                            String::from("-")
                        } else {
                            alloc::format!("{}", e.vector_stall)
                        };
                        let vn = if e.vector_non_stall == u32::MAX {
                            String::from("-")
                        } else {
                            alloc::format!("{}", e.vector_non_stall)
                        };
                        let _ = writeln!(
                            s,
                            "[gpustep15] {} engine={:3} ({:<22}) pmcMask={:#010x} vecStall={:>5} vecNonStall={:>5}",
                            if hot { ">>>" } else { "   " },
                            e.engine_idx,
                            engine_name(e.engine_idx),
                            e.pmc_intr_mask,
                            vs,
                            vn
                        );
                    }
                }
            }
            Err(status) => {
                let _ = writeln!(s, "[gpustep15] intr_table FAILED, NV_STATUS={:#x}", status);
            }
        }
        s
    }

    /// Step 16 (`/proc/gpustep16`): the GR allocation ladder on a
    /// state-loaded GPU -- client -> device -> subdevice -> VA space -> TSG
    /// bound to the GRAPHICS engine -> context share (SYNC/VEID0), all
    /// through the vendored resource server against the live GSP. The first
    /// allocations Eclipse makes itself (everything before adopted GSP
    /// internal handles), and the front half of a real compute launch;
    /// step17 adds the GPFIFO channel + TURING_COMPUTE_A (golden context).
    /// Idempotent: the C side keeps the ladder alive and caches the result.
    fn bringup_step16(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep16] skipped (bring the GPU up first: gpustep5/6/8/9 on the secondary, or gpustep14 on the console)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step16(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep16] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(g) => {
                let _ = writeln!(
                    s,
                    "[gpustep16] --- GR allocation ladder (resource server on live GSP) ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep16] NV01_ROOT client:        {} (hClient={:#010x})",
                    phase(g.client_status),
                    g.h_client
                );
                let _ = writeln!(
                    s,
                    "[gpustep16] NV01_DEVICE_0:           {} (hDevice={:#010x})",
                    phase(g.device_status),
                    g.h_device
                );
                let _ = writeln!(
                    s,
                    "[gpustep16] NV20_SUBDEVICE_0:        {} (hSubdevice={:#010x})",
                    phase(g.subdev_status),
                    g.h_subdevice
                );
                let _ = writeln!(
                    s,
                    "[gpustep16] FERMI_VASPACE_A:         {} (hVas={:#010x})",
                    phase(g.vas_status),
                    g.h_vas
                );
                let _ = writeln!(
                    s,
                    "[gpustep16] KEPLER_CHANNEL_GROUP_A:  {} (hTsg={:#010x}, engineType=GRAPHICS)",
                    phase(g.tsg_status),
                    g.h_tsg
                );
                let _ = writeln!(
                    s,
                    "[gpustep16] FERMI_CONTEXT_SHARE_A:   {} (hCtxShare={:#010x})",
                    phase(g.ctxshare_status),
                    g.h_ctxshare
                );
                if g.ctxshare_status == 0 {
                    let _ = writeln!(s, "[gpustep16] --- GR ALLOCATION LADDER COMPLETE: TSG on the GRAPHICS runlist with a live subcontext; step17 = GPFIFO channel + TURING_COMPUTE_A (golden context) ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep16] eclipse_rm_step16 FAILED, NV_STATUS={:#x} (GPU not GSP-booted/state-loaded?)",
                    status
                );
            }
        }
        s
    }

    /// Step 17 (`/proc/gpustep17`): compute channel on the step-16 ladder --
    /// USERD (vidmem) + 64 KiB pushbuffer/GPFIFO memory mapped in our VAS +
    /// error notifier + GPFIFO channel (chip class, e.g. TURING_CHANNEL_
    /// GPFIFO_A) inside the TSG with our ctxshare + TURING_COMPUTE_A object
    /// + GPFIFO_SCHEDULE. After this the channel is live on the GRAPHICS
    /// runlist; step18 = QMD + SASS kernel + doorbell = first Eclipse-
    /// authored compute launch. Requires gpustep16 first. Idempotent.
    fn bringup_step17(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from("[gpustep17] skipped (bring the GPU up first, then gpustep16)\n");
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step17(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep17] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(
                    s,
                    "[gpustep17] --- compute channel on the step-16 ladder ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] USERD (vidmem, {} B):     {} (hUserd={:#010x})",
                    c.userd_size,
                    phase(c.userd_status),
                    c.h_userd
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] sysmem buf 64K:           {} (hPhysBuf={:#010x})",
                    phase(c.buf_status),
                    c.h_phys_buf
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] virtual in hVas:          {} (hVirtBuf={:#010x})",
                    phase(c.virt_status),
                    c.h_virt_buf
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] Map -> GPU VA:            {} (VA={:#x})",
                    phase(c.map_status),
                    c.buf_gpu_va
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] error notifier 4K:        {} (hNotifier={:#010x})",
                    phase(c.notif_status),
                    c.h_notifier
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] GPFIFO channel (class {:#06x}): {} (hChannel={:#010x})",
                    c.channel_class,
                    phase(c.chan_status),
                    c.h_channel
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] TURING_COMPUTE_A:         {} (hCompute={:#010x})",
                    phase(c.compute_status),
                    c.h_compute
                );
                let _ = writeln!(
                    s,
                    "[gpustep17] GPFIFO_SCHEDULE:          {}",
                    phase(c.sched_status)
                );
                if c.sched_status == 0 {
                    let _ = writeln!(s, "[gpustep17] --- COMPUTE CHANNEL LIVE ON THE GRAPHICS RUNLIST: step18 = QMD + SASS kernel + doorbell (first Eclipse compute launch) ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep17] eclipse_rm_step17 FAILED, NV_STATUS={:#x} (run gpustep16 first; GPU state-loaded?)",
                    status
                );
            }
        }
        s
    }

    /// `/proc/gpuedid`: real display query via the RM's NV04_DISPLAY_COMMON.
    fn bringup_edid(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return alloc::format!(
                "[gpuedid] === {} === skipped (run /proc/gpuinit first)\n",
                self.name
            );
        };
        let _ = writeln!(
            s,
            "[gpuedid] === {} (rm instance {}) ===",
            self.name, device_instance
        );
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::edid(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpuedid] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(
                    s,
                    "[gpuedid] --- real display query (RM internal NV04_DISPLAY_COMMON) ---"
                );
                // 0x56 NV_ERR_NOT_SUPPORTED here is the intentional "no
                // display engine on this GPU" early-out, not a failure.
                if c.alloc_status == 0x56 {
                    let _ = writeln!(s, "[gpuedid] DispCommon handle:         none -- no display engine (headless GPU)");
                } else {
                    let _ = writeln!(
                        s,
                        "[gpuedid] DispCommon handle:         {}",
                        phase(c.alloc_status)
                    );
                }
                let _ = writeln!(
                    s,
                    "[gpuedid] GET_SUPPORTED:            {} (outputs={:#x}, DDC-capable={:#x})",
                    phase(c.supported_status),
                    c.display_mask,
                    c.display_mask_ddc
                );
                let _ = writeln!(
                    s,
                    "[gpuedid] GET_CONNECT_STATE:        {} (connected={:#x})",
                    phase(c.connect_status),
                    c.connected_mask
                );
                // The DRM view: connector ids GETRESOURCES/GETCONNECTOR now
                // serve for this GPU (real topology, not the 1001 stub).
                if c.supported_status == 0 && c.display_mask != 0 {
                    let _ = write!(s, "[gpuedid] DRM connectors:");
                    for b in 0..32u32 {
                        if c.display_mask & (1 << b) != 0 {
                            let _ = write!(
                                s,
                                " {}{}",
                                Self::rm_connector_id(device_instance, b),
                                if c.connected_mask & (1 << b) != 0 {
                                    "*"
                                } else {
                                    ""
                                }
                            );
                        }
                    }
                    let _ = writeln!(s, " (*=connected)");
                }
                if c.conn_type_count > 0 {
                    let _ = write!(s, "[gpuedid] connector types:");
                    let n = (c.conn_type_count as usize).min(c.conn_type_display_id.len());
                    for i in 0..n {
                        let bit = c.conn_type_display_id[i].trailing_zeros();
                        let _ = write!(
                            s,
                            " {}={}",
                            Self::rm_connector_id(device_instance, bit),
                            nv_conn_type_name(c.conn_type[i])
                        );
                    }
                    let _ = writeln!(s);
                }
                if c.connected_mask == 0 && c.connect_status == 0 {
                    let _ = writeln!(s, "[gpuedid] no monitor connected to this GPU's outputs (expected on the headless compute GPU)");
                } else if c.edid_status != 0xFFFF_FFFF {
                    let _ = writeln!(
                        s,
                        "[gpuedid] GET_EDID (id={:#x}):         {} ({} bytes, header {})",
                        c.edid_display_id,
                        phase(c.edid_status),
                        c.edid_size,
                        if c.edid_valid == 1 {
                            "VALID"
                        } else {
                            "invalid"
                        }
                    );
                    if c.edid_valid == 1 {
                        // EDID bytes 8-9 = PNP manufacturer id (5-bit packed letters); 10-11 = product code.
                        let m = ((c.edid_head[8] as u16) << 8) | c.edid_head[9] as u16;
                        let l1 = (b'A' - 1 + ((m >> 10) & 0x1f) as u8) as char;
                        let l2 = (b'A' - 1 + ((m >> 5) & 0x1f) as u8) as char;
                        let l3 = (b'A' - 1 + (m & 0x1f) as u8) as char;
                        let prod = ((c.edid_head[11] as u16) << 8) | c.edid_head[10] as u16;
                        let year = 1990u32 + c.edid_head[17] as u32;
                        let _ = writeln!(
                            s,
                            "[gpuedid] MONITOR: {}{}{} product={:#06x} year={} (EDID v{}.{})",
                            l1, l2, l3, prod, year, c.edid_head[18], c.edid_head[19]
                        );
                        let _ = write!(s, "[gpuedid] EDID head:");
                        for b in c.edid_head.iter() {
                            let _ = write!(s, " {:02x}", b);
                        }
                        let _ = writeln!(s);
                    }
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpuedid] eclipse_rm_edid FAILED, NV_STATUS={:#x} (run /proc/gpuinit first)",
                    status
                );
            }
        }
        s
    }

    /// Step 18 (`/proc/gpustep18`): the first Eclipse-authored GPU
    /// execution. Writes a method stream (host semaphore RELEASE +
    /// SET_OBJECT(TURING_COMPUTE_A) + compute report semaphore RELEASE)
    /// into the step-17 pushbuffer, submits it (GP entry, GPPut, work-
    /// submit token, usermode doorbell) and CPU-polls both semaphore
    /// landing zones. Host sem OK = ESCHED/PBDMA fetched and ran our
    /// pushbuffer; engine sem OK = the compute engine context-switched
    /// into our channel and processed class methods. Requires gpustep17.
    fn bringup_step18(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep18] skipped (bring the GPU up first, then gpustep16/17)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step18(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep18] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                0x65 => String::from("TIMEOUT (never landed)"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(l) => {
                let _ = writeln!(
                    s,
                    "[gpustep18] --- first Eclipse-authored submission on the live channel ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep18] lookup (chan/buf/USERD):  {}",
                    phase(l.lookup_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep18] CPU map (buf + USERD):    {}",
                    phase(l.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep18] work-submit token:        {} (token={:#010x}, runlist={})",
                    phase(l.token_status),
                    l.work_token,
                    l.runlist_id
                );
                let _ = writeln!(
                    s,
                    "[gpustep18] submit ({} dw + doorbell): {}",
                    l.push_dwords,
                    phase(l.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep18] HOST semaphore (PBDMA):   {} (value={:#010x}, {} ms)",
                    phase(l.host_sem_status),
                    l.host_sem_value,
                    l.host_poll_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep18] ENGINE semaphore (compute FE): {} (value={:#010x}, {} ms)",
                    phase(l.eng_sem_status),
                    l.eng_sem_value,
                    l.eng_poll_iters
                );
                if l.host_sem_status == 0 && l.eng_sem_status == 0 {
                    let _ = writeln!(s, "[gpustep18] --- THE GPU RAN OUR PUSHBUFFER: PBDMA fetch + compute-engine context switch both proven; step19 = QMD + SASS kernel (real compute launch) ---");
                } else if l.host_sem_status == 0 {
                    let _ = writeln!(s, "[gpustep18] --- PBDMA ran our methods but the compute engine never reported: suspect ctxsw/golden-context or SET_OBJECT path ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep18] eclipse_rm_step18 FAILED, NV_STATUS={:#x} (run gpustep17 first in this boot; GPU state-loaded?)",
                    status
                );
            }
        }
        s
    }

    /// Step 19 (`/proc/gpustep19`): the first real compute launch. Builds a
    /// Turing (Volta V02_02) QMD pointing at a minimal SM75 EXIT kernel and
    /// submits it through the live step-17/18 channel via SEND_PCAS; the
    /// QMD's RELEASE0 semaphore landing in sysmem proves the SMs ran our
    /// program. Requires gpustep17 (and is happiest after gpustep18) first.
    fn bringup_step19(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep19] skipped (bring the GPU up first, then gpustep16/17)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step19(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep19] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                0x65 => String::from("TIMEOUT (grid never released)"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(
                    s,
                    "[gpustep19] --- first Eclipse-authored COMPUTE LAUNCH (QMD + SM75 kernel) ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] lookup (chan/buf/USERD):  {}",
                    phase(c.lookup_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] CPU map (buf + USERD):    {}",
                    phase(c.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] work-submit token:        {} (token={:#010x}, runlist={})",
                    phase(c.token_status),
                    c.work_token,
                    c.runlist_id
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] QMD @ {:#x}, kernel @ {:#x}",
                    c.qmd_va, c.kernel_va
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] launch ({} dw + SEND_PCAS + doorbell): {}",
                    c.push_dwords,
                    phase(c.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] post-PCAS host fence:     {} (value={:#010x}, {} ms)",
                    phase(c.fence_status),
                    c.fence_value,
                    c.fence_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep19] QMD RELEASE0 semaphore:   {} (value={:#010x}, {} ms)",
                    phase(c.sem_status),
                    c.sem_value,
                    c.poll_iters
                );
                if c.sem_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep19] ============================================================"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep19]  THE 34 SMs RAN OUR SASS KERNEL. Eclipse launched compute"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep19]  on the TU106 and the grid completed. step20 = kernel that"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep19]  stores a computed result to memory (params + STG)."
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep19] ============================================================"
                    );
                } else if c.fence_status == 0 {
                    let _ = writeln!(s, "[gpustep19] --- PBDMA consumed the whole compute stream (fence landed) but the grid never released: QMD scheduling or SM execution is stuck. The RM/GSP capture above should carry any SM exception. ---");
                } else if c.submit_status == 0 {
                    let _ = writeln!(s, "[gpustep19] --- doorbell rung but the post-PCAS fence never landed: the PBDMA did not consume the compute stream (channel faulted on an earlier method?). ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep19] eclipse_rm_step19 FAILED, NV_STATUS={:#x} (run gpustep17 first in this boot; GPU state-loaded?)",
                    status
                );
            }
        }
        s
    }

    /// Step 20 (`/proc/gpustep20`): the first kernel that computes an
    /// observable effect for Eclipse — MOV dest/value immediates (patched
    /// into the SASS at runtime) + STG.E.SYS + EXIT on the proven step-19
    /// QMD harness. Triple verification: post-PCAS fence, QMD RELEASE0,
    /// and CPU readback of the stored dword. Requires gpustep17 first.
    fn bringup_step20(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep20] skipped (bring the GPU up first, then gpustep16/17)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step20(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep20] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                0x65 => String::from("TIMEOUT"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(s, "[gpustep20] --- kernel STORE: GPU writes a value we chose to memory we chose ---");
                let _ = writeln!(
                    s,
                    "[gpustep20] lookup / CPU map:         {} / {}",
                    phase(c.lookup_status),
                    phase(c.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep20] token:                    {} (token={:#010x}, runlist={})",
                    phase(c.token_status),
                    c.work_token,
                    c.runlist_id
                );
                let _ = writeln!(
                    s,
                    "[gpustep20] QMD @ {:#x}, kernel @ {:#x}, dest @ {:#x}",
                    c.qmd_va, c.kernel_va, c.dest_va
                );
                let _ = writeln!(
                    s,
                    "[gpustep20] launch ({} dw):            {}",
                    c.push_dwords,
                    phase(c.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep20] post-PCAS host fence:     {} (value={:#010x}, {} ms)",
                    phase(c.fence_status),
                    c.fence_value,
                    c.fence_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep20] QMD RELEASE0 semaphore:   {} (value={:#010x}, {} ms)",
                    phase(c.sem_status),
                    c.sem_value,
                    c.sem_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep20] stored dword @ dest:      {} (value={:#010x}, expect 0xec0de520)",
                    phase(c.store_status),
                    c.store_value
                );
                if c.sem_status == 0 && c.store_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep20] ============================================================"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep20]  THE GPU COMPUTED FOR ECLIPSE: our SASS ran on an SM and"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep20]  stored our value at our address. MOV+STG+EXIT verified."
                    );
                    let _ = writeln!(s, "[gpustep20]  The compute bring-up ladder is COMPLETE.");
                    let _ = writeln!(
                        s,
                        "[gpustep20] ============================================================"
                    );
                } else if c.sem_status == 0 {
                    let _ = writeln!(s, "[gpustep20] --- grid completed but the store is missing/wrong: STG encoding or GMMU write path suspect ---");
                } else if c.fence_status == 0 {
                    let _ = writeln!(s, "[gpustep20] --- methods consumed but grid never released: MOV/STG encoding suspect (SM trap); RELEASE0 did not land ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep20] eclipse_rm_step20 FAILED, NV_STATUS={:#x} (run gpustep17 first in this boot)",
                    status
                );
            }
        }
        s
    }

    /// Step 21 (`/proc/gpustep21`): multi-thread computation — 32 threads
    /// each compute out[tid] = tid*3+7 (S2R thread-ID with real write-
    /// barrier scoreboarding, IMAD math, IMAD.WIDE per-thread addressing,
    /// per-thread STG), CPU-verifies all 32 slots. Requires gpustep17.
    fn bringup_step21(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep21] skipped (bring the GPU up first, then gpustep16/17)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step21(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep21] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                0x65 => String::from("TIMEOUT"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(
                    s,
                    "[gpustep21] --- 32-THREAD kernel: out[tid] = tid*3 + 7 ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] lookup / CPU map:         {} / {}",
                    phase(c.lookup_status),
                    phase(c.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] token:                    {} (token={:#010x}, runlist={})",
                    phase(c.token_status),
                    c.work_token,
                    c.runlist_id
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] QMD @ {:#x}, kernel @ {:#x}, out[] @ {:#x}",
                    c.qmd_va, c.kernel_va, c.out_va
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] launch ({} dw):            {}",
                    c.push_dwords,
                    phase(c.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] post-PCAS host fence:     {} ({} ms)",
                    phase(c.fence_status),
                    c.fence_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] QMD RELEASE0 semaphore:   {} ({} ms)",
                    phase(c.sem_status),
                    c.sem_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep21] per-thread verification:  {} ({}/32 slots correct)",
                    phase(c.verify_status),
                    c.match_count
                );
                if c.first_bad_idx != 0xFFFF_FFFF {
                    let _ = writeln!(
                        s,
                        "[gpustep21] first mismatch: out[{}]={:#010x} (expected {:#x})",
                        c.first_bad_idx,
                        c.first_bad_val,
                        3 * c.first_bad_idx + 7
                    );
                }
                if c.sem_status == 0 && c.verify_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep21] ============================================================"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep21]  32 THREADS, 32 CORRECT RESULTS: per-thread IDs, integer"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep21]  math, per-thread addressing and stores, and real Volta"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep21]  scoreboarding all verified. Eclipse now runs real"
                    );
                    let _ = writeln!(s, "[gpustep21]  parallel compute on the TU106.");
                    let _ = writeln!(
                        s,
                        "[gpustep21] ============================================================"
                    );
                } else if c.sem_status == 0 {
                    let _ = writeln!(s, "[gpustep21] --- grid completed but results are wrong: math/addressing path suspect (check first mismatch above) ---");
                } else if c.fence_status == 0 {
                    let _ = writeln!(s, "[gpustep21] --- methods consumed but grid never released: S2R/IMAD encoding or scoreboard suspect (SM trap) ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep21] eclipse_rm_step21 FAILED, NV_STATUS={:#x} (run gpustep17 first in this boot)",
                    status
                );
            }
        }
        s
    }

    /// Step 22 (`/proc/gpustep22`): chip-scale grid — 68 CTAs x 32 threads
    /// = 2176 threads across all 34 SMs (two waves), out[gid] = gid*3+7
    /// with gid = ctaid*32 + tid, all 2176 slots CPU-verified.
    fn bringup_step22(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep22] skipped (bring the GPU up first, then gpustep16/17)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step22(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep22] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                0x65 => String::from("TIMEOUT"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(
                    s,
                    "[gpustep22] --- CHIP-SCALE grid: 68 CTAs x 32 threads over all 34 SMs ---"
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] lookup / CPU map:         {} / {}",
                    phase(c.lookup_status),
                    phase(c.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] token:                    {} (token={:#010x}, runlist={})",
                    phase(c.token_status),
                    c.work_token,
                    c.runlist_id
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] QMD @ {:#x}, kernel @ {:#x}, out[] @ {:#x}",
                    c.qmd_va, c.kernel_va, c.out_va
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] launch ({} dw):            {}",
                    c.push_dwords,
                    phase(c.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] post-PCAS host fence:     {} ({} ms)",
                    phase(c.fence_status),
                    c.fence_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] QMD RELEASE0 semaphore:   {} ({} ms)",
                    phase(c.sem_status),
                    c.sem_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep22] per-thread verification:  {} ({}/2176 slots correct)",
                    phase(c.verify_status),
                    c.match_count
                );
                if c.first_bad_idx != 0xFFFF_FFFF {
                    let _ = writeln!(
                        s,
                        "[gpustep22] first mismatch: out[{}]={:#010x} (expected {:#x})",
                        c.first_bad_idx,
                        c.first_bad_val,
                        3 * c.first_bad_idx + 7
                    );
                }
                if c.sem_status == 0 && c.verify_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep22] ============================================================"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep22]  2176 THREADS, 68 CTAs, ALL 34 SMs: the whole TU106 chip"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep22]  computed for Eclipse in one dispatch and every result"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep22]  verified. Chip-scale parallel compute is proven."
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep22] ============================================================"
                    );
                } else if c.sem_status == 0 {
                    let _ = writeln!(s, "[gpustep22] --- grid completed but results wrong (check first mismatch: CTA scheduling/addressing suspect) ---");
                } else if c.fence_status == 0 {
                    let _ = writeln!(s, "[gpustep22] --- methods consumed but grid never released: multi-CTA dispatch suspect ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep22] eclipse_rm_step22 FAILED, NV_STATUS={:#x} (run gpustep17 first in this boot)",
                    status
                );
            }
        }
        s
    }

    /// Step 23 (`/proc/gpustep23`): integer SAXPY — 32 threads each load
    /// x[tid] and y[tid] from GPU arrays, compute y = a*x + y (LDG global
    /// loads + IMAD + STG), CPU-verified per element. The load-compute-
    /// store canon; the first kernel that reads from memory.
    fn bringup_step23(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from(
                "[gpustep23] skipped (bring the GPU up first, then gpustep16/17)\n",
            );
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::step23(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpustep23] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                0x65 => String::from("TIMEOUT"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(s, "[gpustep23] --- integer SAXPY: y[i] = 3*x[i] + y[i], x[i]=0x1000+i, y[i]=100+i ---");
                let _ = writeln!(
                    s,
                    "[gpustep23] lookup / CPU map:         {} / {}",
                    phase(c.lookup_status),
                    phase(c.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep23] token:                    {} (token={:#010x}, runlist={})",
                    phase(c.token_status),
                    c.work_token,
                    c.runlist_id
                );
                let _ = writeln!(
                    s,
                    "[gpustep23] QMD @ {:#x}, kernel @ {:#x}, y[] @ {:#x}",
                    c.qmd_va, c.kernel_va, c.out_va
                );
                let _ = writeln!(
                    s,
                    "[gpustep23] launch ({} dw):            {}",
                    c.push_dwords,
                    phase(c.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpustep23] post-PCAS host fence:     {} ({} ms)",
                    phase(c.fence_status),
                    c.fence_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep23] QMD RELEASE0 semaphore:   {} ({} ms)",
                    phase(c.sem_status),
                    c.sem_iters
                );
                let _ = writeln!(
                    s,
                    "[gpustep23] SAXPY verification:       {} ({}/32 elements = 0x3064+4i)",
                    phase(c.verify_status),
                    c.match_count
                );
                if c.fault_ctrl_status != 0xFFFF_FFFF {
                    let _ = writeln!(
                        s,
                        "[gpustep23] MMU fault query:          ctrl={:#x} addr={:#x}_{:08x} type={:#x}",
                        c.fault_ctrl_status, c.fault_addr_hi, c.fault_addr_lo, c.fault_type
                    );
                }
                if c.first_bad_idx != 0xFFFF_FFFF {
                    let _ = writeln!(
                        s,
                        "[gpustep23] first mismatch: y[{}]={:#x} ({}) expected {:#x}",
                        c.first_bad_idx,
                        c.first_bad_val,
                        c.first_bad_val,
                        0x3064 + 4 * c.first_bad_idx
                    );
                }
                if c.sem_status == 0 && c.verify_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpustep23] ============================================================"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep23]  LOAD-COMPUTE-STORE PROVEN: the GPU read two arrays from"
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep23]  memory, did a*x+y per element, and wrote the results back."
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep23]  Eclipse has the full GPU compute primitive."
                    );
                    let _ = writeln!(
                        s,
                        "[gpustep23] ============================================================"
                    );
                } else if c.sem_status == 0 {
                    let _ = writeln!(s, "[gpustep23] --- grid completed but results wrong: LDG address/data path suspect (check first mismatch) ---");
                } else if c.fence_status == 0 {
                    let _ = writeln!(s, "[gpustep23] --- methods consumed but grid never released: LDG encoding or load scoreboard suspect (SM trap) ---");
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpustep23] eclipse_rm_step23 FAILED, NV_STATUS={:#x} (run gpustep17 first in this boot)",
                    status
                );
            }
        }
        s
    }

    /// `/proc/gpubench`: integer-ALU GIOPS benchmark — a big grid of
    /// dependent-IMAD chains timed by the GPU PTIMER. GIOPS is computed
    /// here (u128) to avoid a 64-bit divide in the C.
    fn bringup_bench(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        let device_instance = *self.rm_device_instance.lock();
        let Some(device_instance) = device_instance else {
            return String::from("[gpubench] skipped (run /proc/gpuinit first)\n");
        };
        nvidia_rm_sys::os_interface::capture_begin();
        let result = nvidia_rm_sys::rm_init::bench(device_instance);
        let captured = nvidia_rm_sys::os_interface::capture_take();
        if let Some(log) = captured {
            for line in log.lines() {
                let _ = writeln!(s, "[gpubench] | {}", line);
            }
        }
        let phase = |st: u32| -> String {
            match st {
                0 => String::from("OK"),
                0xFFFF_FFFF => String::from("not reached"),
                e => alloc::format!("FAILED NV_STATUS={:#x}", e),
            }
        };
        match result {
            Ok(c) => {
                let _ = writeln!(
                    s,
                    "[gpubench] --- integer-ALU throughput (IMAD.U32 dependent chain) ---"
                );
                let _ = writeln!(
                    s,
                    "[gpubench] lookup/map:   {} / {}",
                    phase(c.lookup_status),
                    phase(c.map_status)
                );
                let _ = writeln!(
                    s,
                    "[gpubench] launch ({} dw): {}",
                    c.push_dwords,
                    phase(c.submit_status)
                );
                let _ = writeln!(
                    s,
                    "[gpubench] grid:         {} threads x {} IMAD = {} ops",
                    c.num_threads, c.imads_per_thread, c.total_ops
                );
                let _ = writeln!(
                    s,
                    "[gpubench] timestamp sem: {} (@{} ms)",
                    phase(c.sem_status),
                    c.sem_iters
                );
                if c.sem_status == 0 && c.elapsed_ns > 0 {
                    // GIOPS = total_ops / elapsed_ns (ops/ns == giga-ops/s).
                    // x1000 for three decimals, u128 to avoid overflow.
                    let giops_milli = (c.total_ops as u128 * 1000u128) / (c.elapsed_ns as u128);
                    let _ = writeln!(
                        s,
                        "[gpubench] elapsed:      {} ns ({}.{:03} ms)",
                        c.elapsed_ns,
                        c.elapsed_ns / 1_000_000,
                        (c.elapsed_ns / 1000) % 1000
                    );
                    let _ = writeln!(
                        s,
                        "[gpubench] ============================================================"
                    );
                    let _ = writeln!(
                        s,
                        "[gpubench]  {}.{:03} GIOPS (integer multiply-add) on the RTX 2060 Super",
                        giops_milli / 1000,
                        giops_milli % 1000
                    );
                    let _ = writeln!(
                        s,
                        "[gpubench] ============================================================"
                    );
                } else if c.sem_status == 0 {
                    let _ = writeln!(
                        s,
                        "[gpubench] grid ran but timestamps were zero (t0={:#x} t1={:#x})",
                        c.t0_ns, c.t1_ns
                    );
                } else {
                    let _ = writeln!(
                        s,
                        "[gpubench] --- grid did not signal within poll window ---"
                    );
                }
            }
            Err(status) => {
                let _ = writeln!(
                    s,
                    "[gpubench] eclipse_rm_bench FAILED, NV_STATUS={:#x} (run /proc/gpuinit first)",
                    status
                );
            }
        }
        s
    }

    /// Step 2: instance block + GMMU flush — the first GPU register writes.
    /// TEMPORARY: the secondary (non-console) GPU has its own unrelated
    /// problems (USB breaks in Eclipse when it's made primary; likely never
    /// got a VBIOS devinit replay since it's never POSTed), so for now we
    /// target the ONLY GPU available — the one driving the console — and
    /// skip the other one instead. This trades away the original safety net
    /// (a hang here now means losing the only display and a hard reboot);
    /// the user has explicitly accepted that risk. Opt-in (`/proc/gpustep2`).
    fn bringup_step2(&self) -> String {
        use core::fmt::Write;
        let mut s = String::new();
        if !self.drives_boot_display() {
            let _ = writeln!(
                s,
                "[gpustep2] {} ({}) SKIPPED — not the console GPU (bar1_phys={:#x}); only testing the single available GPU",
                self.name, self.gpu_model, self.bar1_phys
            );
            return s;
        }

        let mut g = self.bringup.lock();
        if g.is_none() {
            *g = GpuBringup::build(0x0020_0000, 0x0300_0000);
        }
        let b = match g.as_ref() {
            Some(b) => b,
            None => {
                let _ = writeln!(s, "[gpustep2] {} alloc_coherent FAILED", self.name);
                return s;
            }
        };

        let _ = writeln!(
            s,
            "[gpustep2] === {} ({}) — Step 2: instance block + GMMU flush ===",
            self.name, self.gpu_model
        );

        // Part 1: read-only PRAMIN accessibility ladder. PRAMIN works (rt@0
        // round-tripped) but VRAM at 2 GiB read back the 0xBAD0ACxx PRI-error
        // sentinel, so probe which offsets the window actually reaches. An
        // inaccessible offset reads the sentinel; real VRAM does not. No writes.
        let ladder = [
            ("0", 0u64),
            ("1M", 0x10_0000),
            ("4M", 0x40_0000),
            ("16M", 0x100_0000),
            ("64M", 0x400_0000),
            ("256M", 0x1000_0000),
            ("512M", 0x2000_0000),
            ("1G", 0x4000_0000),
            ("2G", 0x8000_0000),
        ];
        let _ = write!(s, "[gpustep2]  PRAMIN ladder:");
        let mut last_ok = 0u64;
        for (name, off) in ladder {
            let v = self.pramin_r32(off);
            let bad = (v & 0xFFFF_FF00) == 0xBAD0_AC00;
            if !bad {
                last_ok = off;
            }
            let _ = write!(s, " {}={}", name, if bad { "BAD" } else { "ok" });
        }
        let _ = writeln!(s, " (highest ok={:#x})", last_ok);

        let inst = b.inst_vram();
        let st = self.pramin_r32(inst);
        let pramin_ok = (st & 0xFFFF_FF00) != 0xBAD0_AC00;
        self.write_instance_block_vram(b);
        let rb = |off: u64| self.pramin_r32(inst + off);
        let _ = writeln!(
            s,
            "[gpustep2]  PRAMIN self-test={} inst@VRAM {:#x}",
            pramin_ok, inst
        );
        let _ = writeln!(
            s,
            "[gpustep2]  inst@0x200={:08x}{:08x} @0x208={:08x}{:08x} userd@0x008={:08x}{:08x}",
            rb(0x204),
            rb(0x200),
            rb(0x20c),
            rb(0x208),
            rb(0x00c),
            rb(0x008)
        );
        let _ = writeln!(
            s,
            "[gpustep2]  CE ctx (disarmed): inst@0x220={:08x}{:08x} @0x0ac={:08x}",
            rb(0x224),
            rb(0x220),
            rb(0x0ac),
        );
        // Arm the HUB MMU fault buffer (the likely root cause) and report it.
        let (fb_count, fb_lo, fb_hi, fb_size) = self.setup_fault_buffer(b);
        let _ = writeln!(
            s,
            "[gpustep2]  FAULT_BUF: hw_count={:#x} buf_phys={:#x} LO(0xb83000)={:#010x} HI={:#010x} SIZE(0xb83010)={:#010x}",
            fb_count,
            b.fault_buf.paddr(),
            fb_lo,
            fb_hi,
            fb_size
        );
        // Make BAR2 live and report the bind, plus the PCE map (CE buffer size).
        let (b2_before, b2_after, b2_wait) = self.setup_bar2(b);
        let pce_map = unsafe { core::ptr::read_volatile((self._bar0 + 0x0010_4028) as *const u32) };
        let _ = writeln!(
            s,
            "[gpustep2]  BAR2(0xb80f48) {:#010x}->{:#010x} wait(0xb80f50)={:#010x} PCE_MAP(0x104028)={:#010x}",
            b2_before, b2_after, b2_wait, pce_map
        );

        // Part 2: the only GPU register write — flush our PDB.
        let root_phys = b.root.paddr() as u64;
        let (pre, post, ok) = self.gmmu_flush(root_phys);
        let _ = writeln!(
            s,
            "[gpustep2]  flush PDB=(root>>8)={:#x}  trigger(0xb830b0) pre={:#010x} post={:#010x} bit31_cleared={}",
            root_phys >> 8,
            pre,
            post,
            ok
        );
        if ok {
            let _ = writeln!(
                s,
                "[gpustep2]  OK — GMMU accepted the PDB, MMU not wedged. Ready for Step 3 (runlist + doorbell)."
            );
        } else if pre & 0x8000_0000 != 0 {
            let _ = writeln!(
                s,
                "[gpustep2]  ABORTED — a flush was already in flight (bit31 set); no write performed."
            );
        } else {
            let _ = writeln!(
                s,
                "[gpustep2]  TIMEOUT — bit31 never cleared. Suspect bad PDB; inspect /proc/gpudbg fault regs (do NOT re-trigger)."
            );
        }
        s
    }

    /// Step 3: doorbell-enable + runlist commit + channel enable (empty GPFIFO).
    /// Auto-skips the console GPU. Opt-in (`/proc/gpustep3`). Requires Step 2 to
    /// have built the instance block; runs it here if not already done.
    fn bringup_step3(&self) -> String {
        use core::fmt::Write;
        // runlist 0 (GR/CE runlist) and channel 0.
        const RUNL_ID: u32 = 0;
        const CHID: u32 = 0;

        let mut s = String::new();
        // TEMPORARY: targeting the console GPU instead of skipping it — see
        // the comment on bringup_step2 for why.
        if !self.drives_boot_display() {
            let _ = writeln!(
                s,
                "[gpustep3] {} SKIPPED — not the console GPU; only testing the single available GPU",
                self.name
            );
            return s;
        }

        let mut g = self.bringup.lock();
        if g.is_none() {
            *g = GpuBringup::build(0x0020_0000, 0x0300_0000);
        }
        let b = match g.as_ref() {
            Some(b) => b,
            None => {
                let _ = writeln!(s, "[gpustep3] {} alloc_coherent FAILED", self.name);
                return s;
            }
        };

        let _ = writeln!(
            s,
            "[gpustep3] === {} ({}) — Step 3: doorbell + runlist commit (empty GPFIFO) ===",
            self.name, self.gpu_model
        );

        // Ensure the instance block + runlist exist in VRAM (idempotent).
        self.write_instance_block_vram(b);
        self.write_runlist_vram(b);
        let runlist_vram = b.runlist_vram();

        let bar0 = self._bar0;
        let rd =
            |off: u32| unsafe { core::ptr::read_volatile((bar0 + off as usize) as *const u32) };
        let wr = |off: u32, v: u32| unsafe {
            core::ptr::write_volatile((bar0 + off as usize) as *mut u32, v)
        };

        // 1) Enable the doorbell (mask bit31).
        let db_before = rd(0x00b6_5000);
        wr(0x00b6_5000, db_before | 0x8000_0000);
        let db_after = rd(0x00b6_5000);
        let _ = writeln!(
            s,
            "[gpustep3]  doorbell-en(0xb65000) {:#010x} -> {:#010x} (bit31={})",
            db_before,
            db_after,
            db_after >> 31
        );

        // 2) Commit the runlist (base lo/hi + count=2 submits; poll bit15).
        let base = 0x0000_2b00 + RUNL_ID * 0x10;
        wr(base, runlist_vram as u32);
        wr(base + 4, (runlist_vram >> 32) as u32);
        wr(base + 8, 2); // 2 entries (cgrp + chan) — this write submits
        let mut cfg_post = rd(base + 0xc);
        let mut commit_ok = false;
        for _ in 0..5_000_000u64 {
            cfg_post = rd(base + 0xc);
            if cfg_post & 0x0000_8000 == 0 {
                commit_ok = true;
                break;
            }
            gpu_spin();
        }
        let _ = writeln!(
            s,
            "[gpustep3]  runlist@{:#x} commit RUNL{} cfg(0x{:x})={:#010x} pending_cleared={}",
            runlist_vram,
            RUNL_ID,
            base + 0xc,
            cfg_post,
            commit_ok
        );

        // 3) Enable the channel in the scheduler (mask 0x400).
        let ce = 0x0080_0004 + CHID * 8;
        let chan_before = rd(ce);
        wr(ce, chan_before | 0x0000_0400);
        let chan_after = rd(ce);
        let _ = writeln!(
            s,
            "[gpustep3]  chan{}-cfg(0x{:x}) {:#010x} -> {:#010x}",
            CHID, ce, chan_before, chan_after
        );

        if commit_ok {
            let _ = writeln!(
                s,
                "[gpustep3]  OK — scheduler accepted the runlist, no fault. Ready for Step 4 (ring doorbell, empty PB)."
            );
        } else {
            let _ = writeln!(
                s,
                "[gpustep3]  TIMEOUT — runlist pending bit never cleared. Inspect /proc/gpudbg; runl_id 0 may be wrong (do NOT re-commit)."
            );
        }
        s
    }

    /// Step 4: ring the doorbell with a SET_OBJECT(0xC5B5) pushbuffer. Exercises
    /// doorbell -> PBDMA -> GMMU-translated pushbuffer fetch -> method parse.
    /// Auto-skips the console GPU. Opt-in (`/proc/gpustep4`).
    fn bringup_step4(&self) -> String {
        use core::fmt::Write;
        const CHID: u32 = 0;

        let mut s = String::new();
        // TEMPORARY: targeting the console GPU instead of skipping it — see
        // the comment on bringup_step2 for why.
        if !self.drives_boot_display() {
            let _ = writeln!(
                s,
                "[gpustep4] {} SKIPPED — not the console GPU; only testing the single available GPU",
                self.name
            );
            return s;
        }

        let mut g = self.bringup.lock();
        if g.is_none() {
            *g = GpuBringup::build(0x0020_0000, 0x0300_0000);
        }
        let b = match g.as_ref() {
            Some(b) => b,
            None => {
                let _ = writeln!(s, "[gpustep4] {} alloc_coherent FAILED", self.name);
                return s;
            }
        };

        let _ = writeln!(
            s,
            "[gpustep4] === {} ({}) — Step 4: ring doorbell with SET_OBJECT(0xC5B5) ===",
            self.name, self.gpu_model
        );

        // PMC_ENABLE before/after: confirms whether FIFO (mask 0x100) was
        // actually sitting in reset before setup_channel's reset pulse.
        let pmc_pre = unsafe { core::ptr::read_volatile((self._bar0 + 0x0000_0200) as *const u32) };

        // Bring the channel live (idempotent; covers a fresh boot). Volta+
        // gives every engine its OWN runlist id, discovered via PTOP — using
        // a hardcoded runlist 0 was an unverified assumption (it might
        // belong to GR instead of CE); setup_channel now discovers the
        // actual CE runlist id and commits to that.
        let (commit_ok, runl_id) = self.setup_channel(b);
        let pmc_post =
            unsafe { core::ptr::read_volatile((self._bar0 + 0x0000_0200) as *const u32) };
        let _ = writeln!(
            s,
            "[gpustep4]  PMC_ENABLE(0x200) pre={:#010x} post={:#010x} (FIFO bit 0x100: pre={} post={})",
            pmc_pre,
            pmc_post,
            (pmc_pre >> 8) & 1,
            (pmc_post >> 8) & 1
        );
        let ce = self.find_ce_runlist();
        let engine_id = ce.map(|(_, e)| e).unwrap_or(u32::MAX);
        let _ = writeln!(
            s,
            "[gpustep4]  PTOP-discovered CE runlist id={} engine_id={} (fallback-to-0={}) channel setup: runlist_commit={}",
            runl_id,
            engine_id,
            ce.is_none(),
            commit_ok
        );
        let _ = writeln!(s, "[gpustep4]  PTOP entries:{}", self.ptop_report());

        // PCE_MAP (0x104028): maps each LOGICAL copy engine (what PTOP/runlist
        // enumerate, e.g. our engine_id=8) to a PHYSICAL copy engine, or marks
        // it unmapped. Already read in bringup_step2 but never shown here —
        // across two real-hardware runs PBDMA9 (runl8's PBDMA) was COMPLETELY
        // inert (its aggregate PFIFO_PBDMA_STATUS read bit-for-bit identical
        // both times, unlike PBDMA0/1 which changed), i.e. the host scheduler
        // never touched it even once. If engine_id=8's nibble here reads as
        // the unmapped sentinel, that would explain why nothing ever gets
        // scheduled regardless of how correctly the runlist/channel is set up.
        let pce_map = unsafe { core::ptr::read_volatile((self._bar0 + 0x0010_4028) as *const u32) };
        let _ = writeln!(
            s,
            "[gpustep4]  PCE_MAP(0x104028)={:#010x} (raw; per-LCE nibble layout not yet decoded)",
            pce_map
        );

        // Real nouveau (nvkm subdev/devinit/tu102.c, tu102_devinit_wait): on
        // Turing+, devinit's VBIOS init-table execution runs on a HARDWARE
        // sequencer automatically at POST, before any OS/driver runs at all.
        // The host driver's only job is to *wait* for it, checking exactly:
        //   (rd(0x118128) & 1) != 0 && (rd(0x118234) & 0xff) == 0xff
        // We have NEVER checked this. If it never completed (e.g. this OS's
        // boot path skipped something a full firmware POST normally does),
        // downstream engines could be left un-floorplanned/un-clocked —
        // which would explain a logical CE that never faults, never shows
        // scheduler activity, and whose PBDMA is never touched at all,
        // regardless of how correctly we set up the channel/runlist on top.
        // Read-only; safe to check every time.
        let di_128 = unsafe { core::ptr::read_volatile((self._bar0 + 0x0011_8128) as *const u32) };
        let di_234 = unsafe { core::ptr::read_volatile((self._bar0 + 0x0011_8234) as *const u32) };
        let devinit_done = (di_128 & 1) != 0 && (di_234 & 0xff) == 0xff;
        let _ = writeln!(
            s,
            "[gpustep4]  DEVINIT_WAIT: 0x118128={:#010x}(bit0={}) 0x118234={:#010x}(low8={:#04x}) devinit_done={}",
            di_128,
            di_128 & 1,
            di_234,
            di_234 & 0xff,
            devinit_done
        );

        // NV_PFIFO_SCHED_STATUS (0x263c): global scheduler status — is the
        // runlist-fetch unit even busy/idle, is a channel switch in
        // progress. NV_PFIFO_ENGINE_STATUS(engine_id) (0x2640+id*8): the
        // per-ENGINE (a third id space, distinct from runlist id and PBDMA
        // index) scheduler status — CTX_STATUS, FAULTED, ENGINE busy/idle,
        // currently-loaded ID. Neither had ever been read before.
        let sched_status =
            unsafe { core::ptr::read_volatile((self._bar0 + 0x0000_263c) as *const u32) };
        let _ = writeln!(
            s,
            "[gpustep4]  SCHED_STATUS(0x263c)={:#010x} chsw_in_progress={} runlist_fetch_busy={}",
            sched_status,
            (sched_status >> 1) & 1,
            (sched_status >> 2) & 1
        );
        if engine_id != u32::MAX {
            let eoff = engine_id as usize * 8;
            let eng_status = unsafe {
                core::ptr::read_volatile((self._bar0 + 0x0000_2640 + eoff) as *const u32)
            };
            let eng_debug = unsafe {
                core::ptr::read_volatile((self._bar0 + 0x0000_2644 + eoff) as *const u32)
            };
            let _ = writeln!(
                s,
                "[gpustep4]  ENGINE_STATUS(engine{})={:#010x} ctx_status={} id={:#x} id_type={} engine_busy={} faulted={} eng_reload={}  DEBUG={:#010x}",
                engine_id,
                eng_status,
                (eng_status >> 13) & 0x7,
                eng_status & 0xfff,
                (eng_status >> 12) & 1,
                (eng_status >> 31) & 1,
                (eng_status >> 30) & 1,
                (eng_status >> 29) & 1,
                eng_debug
            );
        }

        // Build the method stream (sysmem pushbuffer) + a GPFIFO launch entry at
        // the current PUT slot. The GPFIFO entry points at the pushbuffer GPU VA.
        let n = b.write_setobject_pushbuffer();
        let pb_va = b.va_base + 0x3000;
        // USERD lives in VRAM — GP_PUT/GP_GET are accessed via PRAMIN.
        let userd = b.userd_vram();
        let put_before = self.pramin_r32(userd + 0x8c);
        let get_before = self.pramin_r32(userd + 0x88);
        let ring_entries = (b.gpfifo.byte_len() / 8) as u32;
        let slot = (put_before % ring_entries) as usize;
        b.write_gpfifo_entry(slot, pb_va, n);
        let target = put_before + 1;

        // Clear any latched MMU fault so the one we read after is OURS, not
        // stale (write bit31 to the fault-clear reg 0xb83094).
        unsafe { core::ptr::write_volatile((self._bar0 + 0x00b8_3094) as *mut u32, 0x8000_0000) };

        // PFIFO_INTR_0 before the ring — did a prior interrupt condition
        // latch (e.g. a scheduler/runlist-update completion) that we never
        // acked, possibly stalling forward progress.
        let intr0_pre =
            unsafe { core::ptr::read_volatile((self._bar0 + 0x0000_2100) as *const u32) };

        // Advance GP_PUT (VRAM USERD, via PRAMIN), fence, ring the doorbell.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.pramin_w32(userd + 0x8c, target);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let token = (runl_id << 16) | CHID;
        unsafe { core::ptr::write_volatile((self._bar0 + 0x00bb_0090) as *mut u32, token) };

        // Poll GP_GET (VRAM USERD) catching up to GP_PUT.
        let mut get_after = get_before;
        let mut advanced = false;
        for _ in 0..5_000_000u64 {
            get_after = self.pramin_r32(userd + 0x88);
            if get_after == target {
                advanced = true;
                break;
            }
            gpu_spin();
        }

        let intr0_post =
            unsafe { core::ptr::read_volatile((self._bar0 + 0x0000_2100) as *const u32) };
        let _ = writeln!(
            s,
            "[gpustep4]  PFIFO_INTR_0(0x2100) pre={:#010x} post={:#010x} (new bits={:#010x})",
            intr0_pre,
            intr0_post,
            intr0_post & !intr0_pre
        );

        // Speculative retry: ack any latched interrupt, re-commit the
        // runlist (idempotent), and ring the doorbell again — on the off
        // chance the very first commit on a cold/never-scheduled-before
        // FIFO needs a second nudge to actually wake the arbiter, even
        // though the register-level sequence matches real driver source
        // exactly. Cheap and safe (everything here is designed by NVIDIA
        // to be re-entrant/idempotent); only attempted if the first try
        // timed out.
        let mut retried = false;
        let mut retry_advanced = false;
        if !advanced {
            unsafe {
                core::ptr::write_volatile((self._bar0 + 0x0000_2100) as *mut u32, intr0_post);
            }
            let (retry_commit_ok, _) = self.setup_channel(b);
            unsafe { core::ptr::write_volatile((self._bar0 + 0x00bb_0090) as *mut u32, token) };
            retried = true;
            for _ in 0..2_000_000u64 {
                get_after = self.pramin_r32(userd + 0x88);
                if get_after == target {
                    retry_advanced = true;
                    advanced = true;
                    break;
                }
                gpu_spin();
            }
            let _ = writeln!(
                s,
                "[gpustep4]  retry: ack_intr + re-commit({}) + re-ring -> advanced={}",
                retry_commit_ok, retry_advanced
            );
        }
        let _ = (retried, retry_advanced);

        // SCHED_STATUS was sampled ONCE, before the ring (runlist_fetch_busy=1
        // in the last real-hardware run). A single snapshot can't tell a
        // fetch unit that is genuinely wedged apart from one merely caught
        // mid-cycle — those point at different bugs (a broken runlist-fetch
        // memory path vs. a fetch that completes fine but still never loads
        // the channel). Poll it here so the next run distinguishes the two.
        let mut fetch_busy_cleared = false;
        let mut fetch_busy_iters = 0u64;
        let mut sched_status_repoll = sched_status;
        for i in 0..2_000_000u64 {
            sched_status_repoll =
                unsafe { core::ptr::read_volatile((self._bar0 + 0x0000_263c) as *const u32) };
            if (sched_status_repoll >> 2) & 1 == 0 {
                fetch_busy_cleared = true;
                fetch_busy_iters = i;
                break;
            }
            gpu_spin();
        }
        let _ = writeln!(
            s,
            "[gpustep4]  SCHED_STATUS re-poll(0x263c)={:#010x} runlist_fetch_busy_cleared={} after_iters={}",
            sched_status_repoll, fetch_busy_cleared, fetch_busy_iters
        );

        // Read the MMU fault THIS step generated (cleared just before the ring).
        let rd = |off: u32| unsafe {
            core::ptr::read_volatile((self._bar0 + off as usize) as *const u32)
        };
        let f_info1 = rd(0x00b8_3090);
        let f_alo = rd(0x00b8_3080);
        let f_ahi = rd(0x00b8_3084);
        let f_info0 = rd(0x00b8_3088);
        let _ = writeln!(
            s,
            "[gpustep4]  fresh fault: INFO1={:#010x} valid={} access={} reason={} VA={:#x}{:08x} eng={:#x}",
            f_info1,
            f_info1 >> 31,
            (f_info1 >> 16) & 0xf,
            f_info1 & 0x1f,
            f_ahi,
            f_alo & 0xffff_f000,
            f_info0 & 0xff,
        );

        let chan_cfg =
            unsafe { core::ptr::read_volatile((self._bar0 + 0x0080_0004) as *const u32) };
        let _ = writeln!(
            s,
            "[gpustep4]  pb_va={:#x} n={} slot={} GP_PUT {}->{} GP_GET {}->{} advanced={} doorbell=0xbb0090 token={:#x}",
            pb_va, n, slot, put_before, target, get_before, get_after, advanced, token
        );
        let _ = writeln!(
            s,
            "[gpustep4]  chan{}-cfg(0x800004)={:#010x} status={}",
            CHID,
            chan_cfg,
            (chan_cfg >> 24) & 0xf
        );
        // PBDMA state: did the init un-SUSPEND them (STATUS != 0x10011111), who
        // serves runlist 0 (PBDMA_MAP RUNLISTS mask), is our channel loaded?
        let _ = writeln!(
            s,
            "[gpustep4]  PBDMA0 st(0x40100)={:#010x} ch={:#010x}  PBDMA1 st(0x42100)={:#010x} ch={:#010x}",
            rd(0x0004_0100),
            rd(0x0004_0120),
            rd(0x0004_2100),
            rd(0x0004_2120)
        );
        // PBDMA0/1 above are stale from the runlist-0 era and, per the last
        // real-hardware run, are NOT the PBDMA our channel goes through
        // (PBDMA_MAP showed only p9 serving a runl_id=8). Their own block
        // registers (STATUS/CHANNEL/GP_GET/GP_PUT/GET/INTR_0 — same offsets
        // as debug_dump's Step-1 report) had never actually been read for
        // whichever PBDMA(s) serve runl_id. Dump them here, dynamically.
        let _ = write!(s, "[gpustep4]  PBDMA(runl{}'s, raw block):", runl_id);
        for i in 0..12u32 {
            let map = rd(0x0000_2390 + i * 4) & 0xffff;
            if map & (1 << runl_id) == 0 {
                continue;
            }
            let pb = 0x0004_0000 + i * 0x2000;
            let _ = write!(
                s,
                " p{}[STATUS={:#010x} CHANNEL={:#010x} GP_GET={:#010x} GP_PUT={:#010x} GET={:#010x} INTR_0={:#010x}]",
                i,
                rd(pb + 0x100),
                rd(pb + 0x120),
                rd(pb + 0x14),
                rd(pb),
                rd(pb + 0x18),
                rd(pb + 0x108),
            );
        }
        let _ = writeln!(s);
        // 0x040100 is NV_PPBDMA_STATUS — all-SUSPENDED (0x10011111) is just the
        // idle/reset value, not a fault signal; nouveau's actual liveness check
        // (gk104_runq_idle) polls NV_PFIFO_PBDMA_STATUS at 0x003080+id*4,
        // CHAN_STATUS = bits 15:13 (0=INVALID/idle,1=VALID,5=LOAD,6=SAVE,7=SWITCH),
        // ID = bits 11:0 (loaded chid).
        let pfs0 = rd(0x0000_3080);
        let pfs1 = rd(0x0000_3084);
        let _ = writeln!(
            s,
            "[gpustep4]  PFIFO_PBDMA_STATUS q0(0x3080)={:#010x} chan_status={} id={:#x}  q1(0x3084)={:#010x} chan_status={} id={:#x}",
            pfs0,
            (pfs0 >> 13) & 0x7,
            pfs0 & 0xfff,
            pfs1,
            (pfs1 >> 13) & 0x7,
            pfs1 & 0xfff
        );
        // Same status register, but for whichever PBDMA index(es) actually
        // serve our runl_id (may not be q0/q1 at all for a non-zero runlist).
        let _ = write!(
            s,
            "[gpustep4]  PFIFO_PBDMA_STATUS(runl{}'s PBDMAs):",
            runl_id
        );
        for i in 0..12u32 {
            let m = rd(0x0000_2390 + i * 4) & 0xffff;
            if m & (1 << runl_id) != 0 {
                let v = rd(0x0000_3080 + i * 4);
                let _ = write!(
                    s,
                    " q{}={:#010x}(chan_status={} id={:#x})",
                    i,
                    v,
                    (v >> 13) & 0x7,
                    v & 0xfff
                );
            }
        }
        let _ = writeln!(s);
        // NV_PFIFO_PBDMA_MAP has up to 12 entries (__SIZE_1=12 per NVIDIA's
        // manual) — we'd only ever looked at p0-p3. If our discovered
        // runl_id (8/9/10, a standalone CE) isn't served by ANY of them,
        // that's a dead end: no hardware PBDMA route exists for it at all.
        let _ = write!(s, "[gpustep4]  PBDMA_MAP servers-of-runl{}:", runl_id);
        let mut any_serves = false;
        for i in 0..12u32 {
            let m = rd(0x0000_2390 + i * 4) & 0xffff;
            if m & (1 << runl_id) != 0 {
                let _ = write!(s, " p{}", i);
                any_serves = true;
            }
        }
        if !any_serves {
            let _ = write!(s, " NONE(!)");
        }
        let _ = write!(s, "  all-nonzero:");
        for i in 0..12u32 {
            let m = rd(0x0000_2390 + i * 4) & 0xffff;
            if m != 0 {
                let _ = write!(s, " p{}={:#06x}", i, m);
            }
        }
        let _ = writeln!(s);
        // Scheduler gate + the runlist entries as the host sees them in VRAM.
        let rl = b.runlist_vram();
        let _ = writeln!(
            s,
            "[gpustep4]  SCHED_DISABLE(0x2630)={:#010x} (runl{} bit={})  runlist@{:#x} cgrp[{:08x} {:08x} {:08x} {:08x}] chan[{:08x} {:08x} {:08x} {:08x}]",
            rd(0x0000_2630),
            runl_id,
            (rd(0x0000_2630) >> runl_id) & 1,
            rl,
            self.pramin_r32(rl),
            self.pramin_r32(rl + 0x4),
            self.pramin_r32(rl + 0x8),
            self.pramin_r32(rl + 0xc),
            self.pramin_r32(rl + 0x10),
            self.pramin_r32(rl + 0x14),
            self.pramin_r32(rl + 0x18),
            self.pramin_r32(rl + 0x1c)
        );
        if advanced {
            let _ = writeln!(
                s,
                "[gpustep4]  OK — channel fetched the pushbuffer via GMMU and bound the copy class, no fault. Ready for Step 5 (real copy)."
            );
        } else {
            let _ = writeln!(
                s,
                "[gpustep4]  TIMEOUT — GP_GET did not advance; PBDMA likely faulted (GPFIFO/pushbuffer mapping). Inspect /proc/gpudbg (do NOT re-ring)."
            );
        }
        s
    }

    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: true,
            has_cursor: true,
            max_width: self.info.width,
            max_height: self.info.height,
        }
    }

    fn has_hardware_kms(&self) -> bool {
        // Claim hardware KMS only when NVC57E surface-flip is opted in AND the
        // ladder is READY. Until then software scanout (GOP blit) must remain
        // the path that lights the panel.
        super::nouveau_uapi::surfaceflip_enabled() && nvidia_rm_sys::rm_init::hwflip_ready()
    }

    fn nouveau_gem_close(&self, handle: u32, owner_pid: u64) -> bool {
        // PRIME share-count gate. A swapchain buffer is referenced by more than
        // one holder at once: the compositor's original GEM_NEW owner plus every
        // PRIME self-import NVK/EGL made of the same buffer (each handed back
        // THIS same nouveau handle). NVK imports a buffer, uses it, then
        // GEM_CLOSEs it while wlroots still owns the very same handle -- so
        // freeing on the first close tore the mapping out from under wlroots:
        // the next self-import's reverse lookup missed, fell back to a generic
        // handle, and nouveau GEM_INFO ENOENT'd it (zink "couldn't allocate
        // memory heap=0" / "createImageFromDmaBufs failed"). dec_ref only frees
        // when the LAST holder closes; until then keep the GEM object, its
        // VM_BIND mappings, and its RM memory alive.
        match crate::scheme::gem_mmap::dec_ref(handle, owner_pid) {
            crate::scheme::gem_mmap::DecRef::NotHolder => {
                log::warn!(
                    "[nouveau-uapi] GEM_CLOSE handle={} by pid={} -> not a holder, refused",
                    handle,
                    owner_pid
                );
                return false;
            }
            crate::scheme::gem_mmap::DecRef::StillReferenced(n) => {
                log::info!(
                    "[nouveau-uapi] GEM_CLOSE handle={} -> still shared (refcount={}), kept alive",
                    handle,
                    n
                );
                // The object stays in `nouveau_gem` and in the RM; this close
                // was just one of several holders letting go.
                return true;
            }
            // Last reference dropped -- the gem_mmap entry was already removed by
            // dec_ref (BEFORE the RM free below, preserving the "no mmap-able
            // mapping outlives its VRAM" ordering the old unregister-first code
            // kept). Fall through to the real free.
            crate::scheme::gem_mmap::DecRef::Freed => {}
            // A GEM object with no phys mapping (never registered), or an
            // unknown handle. Fall through: the nouveau_gem lookup below decides
            // whether it existed at all, exactly as before.
            crate::scheme::gem_mmap::DecRef::NotTracked => {}
        }
        let removed = {
            let mut gem = self.nouveau_gem.lock();
            // An untracked (never exported) object belongs to its creator
            // alone; a tracked one just had its last holder verified above.
            gem.iter()
                .position(|o| {
                    o.handle == handle && (o.phys_addr.is_some() || gem_usable_by(o, owner_pid))
                })
                .map(|pos| gem.remove(pos))
        };
        let Some(obj) = removed else {
            return false;
        };
        // Drop any KMS framebuffer built on this handle BEFORE `gem_free`
        // below hands its VRAM back. `create_fb` accepts a nouveau GEM object
        // as fb backing and caches its `phys_addr`/`h_memory`, but only the
        // PRIME path (`free_buffer`) used to clear those fbs again -- so a
        // compositor doing the normal ADDFB -> GEM_CLOSE dance on a nouveau
        // buffer left an fb pointing into freed VRAM, and the next present or
        // flip scanned out whatever had since been allocated there.
        let dropped_fbs = self.drop_kms_fbs_for_handle(handle);
        if !dropped_fbs.is_empty() {
            log::info!(
                "[nouveau-uapi] GEM_CLOSE handle={} dropped {} KMS fb(s): {:?}",
                handle,
                dropped_fbs.len(),
                dropped_fbs
            );
        }
        // Drain any VM_BIND mappings still referencing this handle BEFORE
        // freeing the backing memory below -- the real nouveau contract
        // expects UNMAP before CLOSE, but a caller that skips it shouldn't
        // leak the VA reservation (h_virt) in RM forever.
        self.drain_vm_mappings(
            &alloc::format!("GEM_CLOSE handle={}", handle),
            |m| m.gem_handle == handle,
            true,
        );
        NOUVEAU_GEM_BYTES.fetch_sub(obj.size, Ordering::Relaxed);
        let status = (*self.rm_device_instance.lock())
            .map(|device_instance| nvidia_rm_sys::rm_init::gem_free(device_instance, obj.h_memory));
        log::info!(
            "[nouveau-uapi] GEM_CLOSE handle={} h_memory={:#010x} -> gem_free status={:?}",
            handle,
            obj.h_memory,
            status
        );
        true
    }

    fn get_connector_edid(&self, id: u32) -> Option<[u8; 128]> {
        let (instance, d) = self.rm_display_state()?;
        let bit = Self::rm_connector_bit(instance, id, &d)?;
        let did = 1u32 << bit;
        let connected = d.connected_mask & did != 0;
        if connected && d.edid_valid == 1 && d.edid_display_id == did {
            if let Some((boot_e, boot_len)) = boot_edid() {
                if boot_len >= 128 && boot_e[8..12] == d.edid_head[8..12] {
                    return Some(boot_e);
                }
            }
            let mut edid = [0u8; 128];
            edid[..32].copy_from_slice(&d.edid_head);
            return Some(edid);
        }
        None
    }

    fn import_buffer(&self, handle: GemHandle) -> bool {
        let mut handles = self.imported_handles.lock();
        if let Some(existing) = handles.iter_mut().find(|h| h.id == handle.id) {
            *existing = ImportedGemHandle {
                id: handle.id,
                phys_addr: handle.phys_addr,
                size: handle.size,
            };
            return true;
        }
        handles.push(ImportedGemHandle {
            id: handle.id,
            phys_addr: handle.phys_addr,
            size: handle.size,
        });
        true
    }

    fn free_buffer(&self, handle: GemHandle) {
        self.imported_handles.lock().retain(|h| h.id != handle.id);
        self.drop_kms_fbs_for_handle(handle.id);
        if let Some(ref mut a) = *self.vram_allocator.lock() {
            a.free(handle.phys_addr, handle.size);
        }
    }

    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32> {
        if width == 0 || height == 0 || pitch == 0 {
            return None;
        }
        let size = (pitch as usize).checked_mul(height as usize)?;
        if size == 0 {
            return None;
        }

        // Prefer nouveau GEM (VRAM/sysmem from GEM_NEW); fall back to PRIME
        // imported handles (dumb/generic).
        let (phys_addr, buf_size, h_memory, vram_offset) = {
            let gem = self.nouveau_gem.lock();
            if let Some(obj) = gem.iter().find(|o| o.handle == handle_id) {
                if size > obj.size as usize {
                    return None;
                }
                (
                    obj.phys_addr.unwrap_or(0),
                    obj.size as usize,
                    obj.h_memory,
                    obj.vram_offset,
                )
            } else {
                drop(gem);
                let handle = self.imported_handle(handle_id)?;
                if size > handle.size {
                    return None;
                }
                (handle.phys_addr, handle.size, 0u32, None)
            }
        };

        let fb_id = self.next_kms_fb_id.fetch_add(1, Ordering::Relaxed);
        self.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
            id: fb_id,
            handle_id,
            width,
            height,
            pitch,
            phys_addr,
            size: buf_size.min(size),
            h_memory,
            vram_offset,
        });
        Some(fb_id)
    }

    fn page_flip(&self, fb_id: u32) -> bool {
        let Some(fb) = self.kms_fb(fb_id) else {
            return false;
        };

        // 1) NVC57E ISO surface flip (opt-in nvidia.surfaceflip) when we have
        //    a VRAM GEM. Bring the ladder up lazily on first flip.
        if super::nouveau_uapi::surfaceflip_enabled() {
            // Need a VRAM GEM (vram_offset populated at GEM_NEW); ISO ctxdma
            // covers the BO from 0, so plane origin passed to RM is 0.
            if fb.vram_offset.is_some() && fb.h_memory != 0 && self.drives_boot_display() {
                if let Some(dev) = *self.rm_device_instance.lock() {
                    use core::sync::atomic::AtomicU8;
                    static SF_STATE: AtomicU8 = AtomicU8::new(0); // 0 untried, 1 ready, 2 fail
                    let mut state = SF_STATE.load(Ordering::Acquire);
                    if state == 0 {
                        let (st, info) = nvidia_rm_sys::rm_init::hwflip_init(dev, 0);
                        if st == 0 && nvidia_rm_sys::rm_init::hwflip_ready() {
                            crate::klog_info!(
                                "[NVIDIA] surfaceflip: READY win={} core=0x{:x} chan=0x{:x} owner=0x{:x}",
                                info.window_idx,
                                info.core_ensure_status,
                                info.win_chan_status,
                                info.owner_status
                            );
                            SF_STATE.store(1, Ordering::Release);
                            state = 1;
                        } else {
                            crate::klog_info!(
                                "[NVIDIA] surfaceflip: init failed st=0x{:x} core=0x{:x} chan=0x{:x} owner=0x{:x} -- CE/software fallback",
                                st,
                                info.core_ensure_status,
                                info.win_chan_status,
                                info.owner_status
                            );
                            SF_STATE.store(2, Ordering::Release);
                            state = 2;
                        }
                    }
                    if state == 1 {
                        // Plane offset within the GEM/ctxdma (0 = whole BO).
                        // Never pass absolute AT_GPU `vram_offset` here —
                        // that programmed the FE past the buffer and tore
                        // the desktop into diagonal snow.
                        let st = nvidia_rm_sys::rm_init::hwflip_surface(
                            dev,
                            fb.h_memory,
                            0,
                            fb.width,
                            fb.height,
                            fb.pitch,
                        );
                        if st == 0 {
                            let now = unsafe { crate::bus::drivers_timer_now_as_micros() };
                            let mut kms = self.kms_state.lock();
                            kms.crtc_fb = fb.id;
                            kms.plane_fb = fb.id;
                            kms.last_vblank_us = now;
                            static SF_FLIP_LOG: AtomicBool = AtomicBool::new(false);
                            if !SF_FLIP_LOG.swap(true, Ordering::Relaxed) {
                                crate::klog_info!(
                                    "[NVIDIA] surfaceflip: OK fb={} hMem={:#x} {}x{} pitch={} (plane off=0)",
                                    fb_id,
                                    fb.h_memory,
                                    fb.width,
                                    fb.height,
                                    fb.pitch
                                );
                            }
                            return true;
                        }
                        static SF_FAIL_LOG: AtomicBool = AtomicBool::new(false);
                        if !SF_FAIL_LOG.swap(true, Ordering::Relaxed) {
                            crate::klog_info!(
                                "[NVIDIA] surfaceflip: flip failed st=0x{:x} fb={} -- fallback",
                                st,
                                fb_id
                            );
                        }
                    }
                }
            }
        }

        // 2) Opt-in CE present into the GOP (`nvidia.hwflip`).
        //
        // NEVER use a flat `ce_present(pa, size)` here: client pitch often
        // differs from the UEFI GOP pitch (e.g. 5504 vs 8192). A flat copy
        // ignores row stride and paints the classic diagonal-snow desktop.
        // Match `scanout_region`'s pitched path, and only accept sysmem
        // sources — VRAM GEM `phys_addr` is not a CE sysmem PA.
        if !super::nouveau_uapi::hwflip_enabled() {
            return false;
        }
        if fb.phys_addr == 0 || fb.width == 0 || fb.height == 0 || fb.pitch == 0 {
            return false;
        }
        if fb.vram_offset.is_some() {
            static VRAM_REFUSE: AtomicBool = AtomicBool::new(false);
            if !VRAM_REFUSE.swap(true, Ordering::Relaxed) {
                crate::klog_info!(
                    "[NVIDIA] hwflip: refusing VRAM GEM fb={} (CE needs sysmem PA) -- software/cepresent fallback",
                    fb_id
                );
            }
            return false;
        }
        let dst_pitch = self.info.pitch;
        let Some((row_bytes, lines)) = hwflip_geometry(
            fb.width,
            fb.height,
            fb.pitch,
            self.info.width,
            self.info.height,
            dst_pitch,
        ) else {
            return false;
        };
        static HWFLIP_TRIED: AtomicBool = AtomicBool::new(false);
        let ok = self.ce_present_2d_pitched(fb.phys_addr, fb.pitch, 0, dst_pitch, row_bytes, lines);
        if ok {
            let now = unsafe { crate::bus::drivers_timer_now_as_micros() };
            let mut state = self.kms_state.lock();
            state.crtc_fb = fb.id;
            state.plane_fb = fb.id;
            state.last_vblank_us = now;
            if !HWFLIP_TRIED.swap(true, Ordering::Relaxed) {
                crate::klog_info!(
                    "[NVIDIA] hwflip: CE 2D pitched OK fb={} {}x{} src_pitch={} dst_pitch={} -> GOP",
                    fb_id,
                    fb.width,
                    fb.height,
                    fb.pitch,
                    dst_pitch
                );
            }
        } else if !HWFLIP_TRIED.swap(true, Ordering::Relaxed) {
            crate::klog_info!(
                "[NVIDIA] hwflip: CE 2D pitched failed for fb={} -- software scanout fallback",
                fb_id
            );
        }
        ok
    }

    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, flags: u32) -> bool {
        const DRM_CURSOR_MOVE: u32 = 0x02;
        if (flags & DRM_CURSOR_MOVE) != 0 {
            // Potential software cursor update here if supported
            return true;
        }
        false
    }

    /// REAL display-engine cursor: bring the NVC570/NVC57D/NVC57A ladder up
    /// on first use (once; a failure latches and the software cursor stays in
    /// charge for the whole boot), then upload the image and enable the
    /// plane. The caller (linux-object's MODE_CURSOR path) only tries this
    /// when the `nvidia.hwcursor` cmdline opt-in is present.
    fn hw_cursor_set(&self, argb: &[u32], w: u32, h: u32) -> bool {
        use core::sync::atomic::AtomicU8;
        static HWCUR_STATE: AtomicU8 = AtomicU8::new(0); // 0 untried, 1 ready, 2 failed
        if w == 0 || h == 0 || w > 64 || h > 64 {
            return false;
        }
        // Only the GPU driving the monitor has a display-engine cursor plane.
        // Do NOT call ensure_console_gpu_brought_up() here: GSP-RM on the
        // console GPU can wedge the bus, and a cursor ioctl must never hang
        // the compositor. Need RM already up (`cat /proc/gpustep14`).
        if !self.drives_boot_display() {
            return false;
        }
        let Some(dev) = *self.rm_device_instance.lock() else {
            return false;
        };
        match HWCUR_STATE.load(Ordering::Relaxed) {
            2 => return false,
            1 => {}
            _ => {
                let (status, st) = nvidia_rm_sys::rm_init::hwcursor_init(dev, 0);
                if status != 0 {
                    crate::klog_warn!(
                        "[nouveau-uapi] hwcursor: init failed status={:#x} (disp={:#x} pbMem={:#x} \
                         pbDma={:#x} notMem={:#x} notDma={:#x} core={:#x} map={:#x} cursMem={:#x} \
                         cursDma={:#x} cursChan={:#x} heads={}) -- staying on the software cursor",
                        status,
                        st.disp_status,
                        st.pb_mem_status,
                        st.pb_dma_status,
                        st.not_mem_status,
                        st.not_dma_status,
                        st.core_status,
                        st.core_map_status,
                        st.curs_mem_status,
                        st.curs_dma_status,
                        st.curs_chan_status,
                        st.num_heads
                    );
                    HWCUR_STATE.store(2, Ordering::Relaxed);
                    return false;
                }
                crate::klog_warn!(
                    "[nouveau-uapi] hwcursor: display cursor plane READY (heads={})",
                    st.num_heads
                );
                HWCUR_STATE.store(1, Ordering::Relaxed);
            }
        }
        let s = nvidia_rm_sys::rm_init::hwcursor_image(dev, argb, w, h);
        if s != 0 {
            crate::klog_warn!(
                "[nouveau-uapi] hwcursor: image upload failed status={:#x} -- software cursor takes over",
                s
            );
            return false;
        }
        true
    }

    fn hw_cursor_move(&self, x: i32, y: i32) -> bool {
        // Pure PIO into the cursor-immediate channel -- no RM entry, no gate.
        nvidia_rm_sys::rm_init::hwcursor_move(x, y) == 0
    }

    fn hw_cursor_hide(&self) -> bool {
        let Some(dev) = *self.rm_device_instance.lock() else {
            return false;
        };
        nvidia_rm_sys::rm_init::hwcursor_hide(dev) == 0
    }

    fn wait_vblank(&self, _crtc_id: u32) -> bool {
        const FRAME_US: u64 = 1_000_000 / 60;
        let state = self.kms_state.lock();
        let now = unsafe { crate::bus::drivers_timer_now_as_micros() };
        let target = if state.last_vblank_us == 0 {
            now.saturating_add(FRAME_US)
        } else {
            state.last_vblank_us.saturating_add(FRAME_US)
        };
        drop(state);
        while unsafe { crate::bus::drivers_timer_now_as_micros() } < target {
            gpu_spin();
        }
        self.kms_state.lock().last_vblank_us = target;
        true
    }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        // Real connector topology straight from the RM (NV0073
        // GET_SUPPORTED): one connector per physical output bit. Until this
        // GPU's bring-up chain has run, fall back to the legacy synthetic
        // connector so pre-init userspace behaviour is unchanged.
        if let Some((instance, d)) = self.rm_display_state() {
            let conns: Vec<u32> = (0..32u32)
                .filter(|b| d.display_mask & (1 << b) != 0)
                .map(|b| Self::rm_connector_id(instance, b))
                .collect();
            return (Vec::new(), alloc::vec![2001], conns);
        }
        (Vec::new(), alloc::vec![2001], alloc::vec![1001])
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        if let Some((instance, d)) = self.rm_display_state() {
            // Legacy alias: a client whose GETRESOURCES ran before bring-up
            // finished was advertised the synthetic connector 1001; if the
            // real topology lands mid-probe (vulkaninfo's own ioctls trigger
            // bring-up), its GETCONNECTOR(1001) must still answer -- mesa
            // treats a miss on any advertised id as fatal (see
            // `rm_display_snap`). Serve 1001 as the first supported output.
            // When instance == 0 and bit 0 is supported this is the same
            // answer the id arithmetic below would give; the explicit alias
            // also covers masks that start at a higher bit and instances > 0.
            let bit = Self::rm_connector_bit(instance, id, &d)?;
            let did = 1u32 << bit;
            let connected = d.connected_mask & did != 0;
            // EDID bytes 21/22 = max image size in cm; only known for the
            // output whose EDID the RM actually read.
            let (mm_width, mm_height) =
                if connected && d.edid_valid == 1 && d.edid_display_id == did {
                    (
                        u32::from(d.edid_head[21]) * 10,
                        u32::from(d.edid_head[22]) * 10,
                    )
                } else {
                    (0, 0)
                };
            return Some(DrmConnector {
                id,
                connected,
                mm_width,
                mm_height,
                connector_type: Self::rm_conn_type(&d, did)
                    .map(nv_conn_type_to_drm)
                    .unwrap_or(0),
            });
        }
        if id == 1001 {
            Some(DrmConnector {
                id,
                connected: true,
                mm_width: 0,
                mm_height: 0,
                connector_type: 11,
            })
        } else {
            None
        }
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        if id == 2001 {
            let state = self.kms_state.lock();
            Some(DrmCrtc {
                id,
                fb_id: state.crtc_fb,
                x: 0,
                y: 0,
            })
        } else {
            None
        }
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        if id == 3001 {
            let state = self.kms_state.lock();
            Some(DrmPlane {
                id,
                crtc_id: 2001,
                fb_id: state.plane_fb,
                possible_crtcs: 1,
                plane_type: 1,
            })
        } else {
            None
        }
    }

    fn get_planes(&self) -> Vec<u32> {
        alloc::vec![3001]
    }

    fn set_plane(
        &self,
        plane_id: u32,
        _crtc_id: u32,
        fb_id: u32,
        _x: i32,
        _y: i32,
        _w: u32,
        _h: u32,
        _src_x: u32,
        _src_y: u32,
        _src_w: u32,
        _src_h: u32,
    ) -> bool {
        if plane_id != 3001 {
            return false;
        }
        if fb_id == 0 {
            self.kms_state.lock().plane_fb = 0;
            return true;
        }
        let ok = self.present_kms_fb(fb_id);
        if ok {
            self.kms_state.lock().plane_fb = fb_id;
        }
        ok
    }

    fn ioctl(&self, request: u32, arg: usize) -> Result<usize, i32> {
        // No known caller pid on this path -- see `ioctl_owned`. `nouveau_ioctl`
        // only actually uses it for CHANNEL_ALLOC's ownership bookkeeping, so
        // callers that only reach `ioctl` (bypassing the pid-aware dispatch in
        // `linux-object`'s `drm_scheme.rs`, if any exist) just get an
        // unreclaimable-on-exit channel -- the same behavior this driver had
        // before `nouveau_release_process` existed.
        self.ioctl_owned(request, arg, 0)
    }

    fn ioctl_owned(&self, request: u32, arg: usize, owner_pid: u64) -> Result<usize, i32> {
        match request {
            0x10DE0001 => {
                // Get Temperature
                if let Some(t) = self.temperature() {
                    Ok(t as usize)
                } else {
                    Err(22) // EINVAL
                }
            }
            0x10DE0002 => {
                // Get VRAM size MB
                Ok(self.vram_size_mb as usize)
            }
            0x10DE0010 => {
                // Fill Rect (arg is pointer to [u32; 5]: x, y, w, h, color)
                if !super::nouveau_uapi::user_range_ok(arg, 5 * 4) {
                    return Err(super::nouveau_uapi::EFAULT);
                }
                let p = arg as *const u32;
                unsafe {
                    self.fill_rect(*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4));
                }
                Ok(0)
            }
            0x10DE0011 => {
                // Blit Rect (arg is pointer to [u32; 6]: sx, sy, dx, dy, w, h)
                if !super::nouveau_uapi::user_range_ok(arg, 6 * 4) {
                    return Err(super::nouveau_uapi::EFAULT);
                }
                let p = arg as *const u32;
                unsafe {
                    self.blit_rect(*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4), *p.add(5));
                }
                Ok(0)
            }
            _ => {
                let t0 = unsafe { crate::bus::drivers_timer_now_as_micros() };
                let r = self.nouveau_ioctl(request, arg, owner_pid);
                // Per-NR count/latency for /proc/gpudbg's ioctl profile
                // (driver-private range only: type byte 'd' = 0x64).
                if (request >> 8) & 0xff == 0x64 {
                    let dt = unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(t0);
                    super::nouveau_uapi::profile_ioctl(request & 0xff, dt);
                }
                // Record the LAST client nouveau-ioctl error into /proc/gpudbg.
                // The EXEC record can say OK while the client still dies with
                // DEVICE_LOST -- because the errno NVK collapses into
                // device-lost is coming from some OTHER ioctl in the submit
                // flow. This names which one. (EXEC's own rich failure line is
                // recorded separately with its per-stage detail; this catches
                // everything else -- VM_BIND, GEM, syncobj, PRIME.)
                if let Err(errno) = r {
                    super::nouveau_uapi::record_ioctl_err(owner_pid, request, errno);
                }
                r
            }
        }
    }

    fn nouveau_release_process(&self, pid: u64) {
        if pid == 0 {
            return;
        }
        // Per-process GPU teardown, OWNER-SCOPED. Stop the GPU before ripping
        // out VM_BIND / GEM: the old order unmapped and freed buffers while the
        // channel was still on the runlist, so a dying glxgears (window close
        // or ^C) left the GPU writing into memory the next shootdown was
        // tearing down — HOLDER waits TLB ack, 8s panic.
        let device_instance = *self.rm_device_instance.lock();
        // 1. Engine-class objects are RM children of the channel. Free them
        //    while the channel is still alive (child-before-parent). ctx_free
        //    would reap them; doing it here avoids a double-free of the shared
        //    handle table. Scope to THIS pid.
        {
            use super::nouveau_uapi as nv;
            let leftovers = nv::class_objects_drain_pid(pid);
            if !leftovers.is_empty() {
                if let Some(device_instance) = device_instance {
                    for (_, h_object) in &leftovers {
                        lock::pump();
                        let _ = nvidia_rm_sys::rm_init::class_free(device_instance, *h_object);
                    }
                }
                log::info!(
                    "[nouveau-uapi] process exit pid={}: freed {} leftover class object(s) before channel",
                    pid,
                    leftovers.len()
                );
            }
        }
        // 2. Take the channel off the runlist (and drop its VAS) BEFORE any
        //    GEM/VM_BIND teardown. ctx 0 is the compositor singleton and is
        //    never freed here.
        let my_ctx = {
            let mut map = self.nouveau_pid_ctx.lock();
            map.iter().position(|t| t.0 == pid).map(|i| map.remove(i).1)
        };
        let ctx_freed = if let (Some(ctx_idx), Some(device_instance)) = (my_ctx, device_instance) {
            if ctx_idx >= 1 {
                self.fast_release(device_instance, ctx_idx);
                self.forget_peer_fences(ctx_idx);
                lock::pump();
                let status = nvidia_rm_sys::rm_init::ctx_free(device_instance, ctx_idx);
                super::nouveau_uapi::ctx_clear_wedged(ctx_idx);
                log::info!(
                    "[nouveau-uapi] process exit pid={}: freed CTX {} -> status={:#x}",
                    pid,
                    ctx_idx,
                    status
                );
                // Only skip the later RM unmap when ctx_free actually
                // destroyed the VAS. A failed free leaves the context (and
                // its VAS) alive; treating it as gone then gem_free's the
                // backing while RM still has h_virt — UAF in the vendor RM.
                status == 0
            } else {
                false
            }
        } else {
            false
        };
        // 3. Drop local VM_BIND bookkeeping. Skip the RM unmap when ctx_free
        //    already destroyed the VAS (a second vm_bind_unmap on a stale
        //    h_virt is a use-after-free in RM).
        let dropped_maps = self.drain_vm_mappings(
            &alloc::format!("process exit pid={}", pid),
            |m| m.owner_pid == pid,
            !ctx_freed,
        );
        // 4. Release this process's GEM objects, RESPECTING the PRIME share
        //    count. A buffer this process created may still be imported by
        //    ANOTHER holder: the compositor self-imports a client's on-screen
        //    buffer to composite it (gem_mmap refcount > 1). The old code freed
        //    every owned object unconditionally (`gem_mmap::unregister`, which
        //    ignores the count) + `gem_free`, so a client exiting while the
        //    compositor still displayed its window tore the buffer out from
        //    under the compositor -- its next GEM_INFO/VM_BIND/EXEC on that
        //    handle ENOENT'd or touched freed RM memory -> the COMPOSITOR's own
        //    VK_ERROR_DEVICE_LOST. That is the "state accumulates over a
        //    session" wedge: one client exit poisons a live compositor buffer,
        //    and from then on every client looks broken.
        //
        //    Mirror `nouveau_gem_close` instead: `dec_ref`, and free for real
        //    ONLY when this drops the LAST reference. A still-shared buffer is
        //    kept alive with its owner detached (`owner_pid = 0`) so no future
        //    process exit re-reaps it; the last holder's GEM_CLOSE -- which
        //    finds the entry by handle -- frees it. `dec_ref` (the gem_mmap
        //    lock) runs with the `nouveau_gem` lock RELEASED, the same order
        //    GEM_CLOSE takes them, so a concurrent client GEM_CLOSE cannot
        //    invert the lock order and deadlock.
        //
        //    Known residual (benign, documented): a client that self-imports
        //    its OWN buffer (GBM export -> EGL/NVK re-import; refcount held
        //    entirely by that one process) and then dies WITHOUT GEM_CLOSE
        //    (^C/crash) leaks it -- we drop only its creator reference here, not
        //    its own import references (those are not tracked per-pid). A leak
        //    until reboot, never a use-after-free; the full fix is per-pid
        //    reference accounting in gem_mmap. A clean exit closes every
        //    reference and frees normally.
        let (freed_gems, freed_bytes) = {
            // Phase 1+2: drop EVERY reference this pid holds -- its own
            // creations and every PRIME self-import it never closed (the
            // per-pid holder list makes those attributable now, so a client
            // that died mid-frame no longer leaks its imports until reboot).
            // `release_pid` takes only the gem_mmap lock; the nouveau_gem lock
            // is taken afterwards, the same order GEM_CLOSE uses.
            let mut to_free: Vec<u32> = Vec::new();
            let mut to_orphan: Vec<u32> = Vec::new();
            for (handle, freed) in crate::scheme::gem_mmap::release_pid(pid) {
                if freed {
                    to_free.push(handle);
                } else {
                    to_orphan.push(handle);
                }
            }
            // A no-phys object was never PRIME-registered (cannot be shared),
            // so it is always its creator's alone -> free.
            {
                let gem = self.nouveau_gem.lock();
                for o in gem.iter() {
                    if o.owner_pid == pid && o.phys_addr.is_none() && !to_free.contains(&o.handle) {
                        to_free.push(o.handle);
                    }
                }
            }
            // Phase 3: apply under the nouveau_gem lock. Collect h_memory ONLY
            // for entries actually removed here -- a racing GEM_CLOSE that
            // already reaped one leaves it absent, so it is never double-freed.
            let mut to_free_mem: Vec<(u32, u32, u64)> = Vec::new(); // (handle, h_memory, size)
            {
                let mut gem = self.nouveau_gem.lock();
                for handle in &to_free {
                    if let Some(pos) = gem.iter().position(|o| o.handle == *handle) {
                        let obj = gem.remove(pos);
                        to_free_mem.push((obj.handle, obj.h_memory, obj.size));
                    }
                }
                for handle in &to_orphan {
                    // Detach the creator only if it was this pid: an import
                    // this pid held of a LIVE owner's buffer keeps its owner.
                    if let Some(o) = gem.iter_mut().find(|o| o.handle == *handle) {
                        if o.owner_pid == pid {
                            o.owner_pid = 0;
                        }
                    }
                }
            }
            // Phase 4: free RM memory outside every lock.
            //
            // Drop each handle's KMS framebuffers first, for the reason spelled
            // out in `nouveau_gem_close`: an fb caches the backing
            // `phys_addr`/`h_memory`, so one left behind scans out VRAM that
            // `gem_free` has already returned to the allocator. This path is
            // the likelier way to hit that -- a client killed or crashed
            // mid-session never issues the GEM_CLOSE that would have cleaned up.
            for (handle, _, _) in &to_free_mem {
                let dropped = self.drop_kms_fbs_for_handle(*handle);
                if !dropped.is_empty() {
                    log::info!(
                        "[nouveau-uapi] process exit pid={}: handle={} dropped {} KMS fb(s): {:?}",
                        pid,
                        handle,
                        dropped.len(),
                        dropped
                    );
                }
            }
            let mut bytes = 0u64;
            for (handle, h_memory, size) in &to_free_mem {
                lock::pump();
                if let Some(device_instance) = device_instance {
                    let status = nvidia_rm_sys::rm_init::gem_free(device_instance, *h_memory);
                    if status != 0 {
                        log::warn!(
                            "[nouveau-uapi] process exit pid={}: gem_free handle={} h_memory={:#010x} failed, NV_STATUS={:#x}",
                            pid, handle, h_memory, status
                        );
                    }
                }
                bytes += size;
            }
            if bytes != 0 {
                NOUVEAU_GEM_BYTES.fetch_sub(bytes, Ordering::Relaxed);
            }
            if !to_orphan.is_empty() {
                log::info!(
                    "[nouveau-uapi] process exit pid={}: freed {} GEM object(s), kept {} still-imported by another holder (PRIME refcount > 0)",
                    pid, to_free_mem.len(), to_orphan.len()
                );
            }
            (to_free_mem.len(), bytes)
        };
        // Channel bookkeeping. Only the RM-backed channel carries real GPU state,
        // so a process that merely enumerated (discovery channels) is reclaimed
        // without the class-object cleanup below.
        //
        // The sticky ctx-0 owner gives the singleton back whether or not it
        // still holds a channel. A compositor that exits cleanly frees its
        // channels first (NVK destroys its contexts before the device
        // closes), and inferring the role from the channel table here left
        // ctx 0 owned by a dead pid: the respawned compositor then ran as a
        // GL client, on a context of its own, while the singleton's channel
        // and direct-submit window stayed as the dead one had left them.
        let owns_ctx0 = self.ctx0_is_owner(pid);
        let (had_rm_backed, released_ctx0) = {
            let mut chans = self.nouveau_channels.lock();
            let before = chans.len();
            let mut rm_backed = false;
            let mut ctx0 = owns_ctx0;
            chans.retain(|c| {
                if c.owner_pid == pid {
                    rm_backed |= c.rm_backed;
                    ctx0 |= c.rm_backed && c.ctx_idx == 0;
                    false
                } else {
                    true
                }
            });
            if before == chans.len() && !owns_ctx0 {
                if dropped_maps > 0 || freed_gems > 0 || my_ctx.is_some() {
                    log::info!(
                        "[nouveau-uapi] process exit pid={}: reclaimed {} mapping(s), {} GEM object(s) ({} KiB), ctx={:?}",
                        pid, dropped_maps, freed_gems, freed_bytes / 1024, my_ctx
                    );
                }
                return;
            }
            (rm_backed, ctx0)
        };
        if !had_rm_backed && !released_ctx0 {
            log::info!(
                "[nouveau-uapi] process exit pid={}: released discovery channel(s); reclaimed {} mapping(s), {} GEM object(s)",
                pid, dropped_maps, freed_gems
            );
            return;
        }
        if released_ctx0 {
            if let Some(device_instance) = device_instance {
                self.reset_ctx0_singleton(device_instance, "process exit", pid);
            } else {
                crate::klog_warn!(
                    "[nouveau-uapi] ctx0 reset: process exit pid={} but no RM device instance is attached",
                    pid
                );
            }
        }
        // Class objects and ctx_free already ran (GPU off the runlist, VAS
        // gone). Only channel bookkeeping remains.
        log::info!(
            "[nouveau-uapi] process exit pid={}: released nouveau channel + {} GEM object(s), {} KiB, {} mapping(s)",
            pid,
            freed_gems,
            freed_bytes / 1024,
            dropped_maps
        );
    }
}

/// Nouveau-compatible driver-specific ioctls -- see `nouveau_uapi.rs` for
/// the ioctl numbers/structs and the module doc there for what is real vs.
/// deliberately refused in this milestone. Entirely opt-in
/// (`nvidia.nouveau_uapi`); returns the same ENOSYS as before when off.
impl NvidiaGpu {
    /// [auto-bringup] Bring the CONSOLE GPU fully up on demand — attach RM, GSP
    /// boot, RM controls, state-load, CE — the first time a GPU client shows up,
    /// so NVK's `CHANNEL_ALLOC` gets a real RM-backed channel and `EXEC` stops
    /// returning `ENODEV`, WITHOUT the operator running `cat /proc/gpustep14`.
    ///
    /// Why this is safe to automate now, when `auto_bringup_compute` still skips
    /// the console GPU: that skip predates `bringup_step14`, which dropped the
    /// PRIMARY_DEVICE/console declaration that made the SEC2 STARTCPU store wedge
    /// (see step14's stage-1.5 note: 2/3 boots survived without it vs 0/9 with
    /// it), and predates the GSP-boot TLB-shootdown deadlock fixes (NMI-ack).
    /// The target also has no disk to capture a manual `cat`, so automating this
    /// is the only path to a working console GPU there. Default **OFF** —
    /// enable with `nvidia.console_gsp` (safer boots keep manual
    /// `/proc/gpustep14`). `nvidia.console_gpu` schedules the deferred
    /// bring-up instead, so the console GPU comes up even when no GPU client
    /// ever appears; it opens this same gate, but only from inside that task
    /// and only once scanout is paused, because a gate opened at boot lets the
    /// first client run the whole bring-up with the display live. The whole
    /// nouveau-uAPI surface is still gated by `nvidia.nouveau_uapi`.
    ///
    /// Strictly one-shot: the console GSP boot must never be attempted twice (a
    /// second STARTCPU on a half-booted GSP is precisely how it wedges). If the
    /// single attempt does not attach the RM, we stay discovery-only (EXEC =
    /// ENODEV) until the next reboot rather than re-running the boot under a
    /// later ioctl. Callers MUST invoke this with no DRM lock held — the boot
    /// takes RM/PCI paths, and `bringup_step14` is a `DrmScheme` trait method on
    /// `self` (in scope here), never the `nouveau_channels`/VM-state locks.
    /// Reserve the next `GEM_NEW` handle from this GPU's private slice of the
    /// driver-private handle range, or `None` once the slice is used up.
    ///
    /// A CAS loop rather than `fetch_add`, because the bound must hold
    /// exactly: `fetch_add` past the end would hand out an id belonging to
    /// the next GPU, and the registries these ids key into (`gem_mmap`,
    /// `NOUVEAU_CPU_VMOS`) are global, so that id would resolve to the other
    /// card's memory.
    fn next_gem_handle(&self) -> Option<u32> {
        let mut cur = self.nouveau_gem_next_handle.load(Ordering::Relaxed);
        loop {
            if cur >= self.nouveau_gem_handle_end {
                return None;
            }
            match self.nouveau_gem_next_handle.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(cur),
                Err(actual) => cur = actual,
            }
        }
    }

    fn ensure_console_gpu_brought_up(&self) {
        // Secondary GPUs are already state-loaded at boot by
        // `auto_bringup_compute`; only the console GPU is skipped there, so it is
        // the one that still needs this on-demand path.
        if !self.drives_boot_display() {
            return;
        }
        // Default OFF: safer boot. Opt in with `nvidia.console_gsp`.
        if !super::nouveau_uapi::console_gsp_enabled() {
            return;
        }
        // Already up — the steady-state fast path (brief lock, no boot).
        if self.rm_device_instance.lock().is_some() {
            return;
        }
        // One attempt, ever.
        if self.auto_bringup_done.swap(true, Ordering::SeqCst) {
            return;
        }
        crate::klog_warn!(
            "[auto-bringup] console GPU not attached; running the full bring-up \
             automatically on first GPU client (attach -> GSP boot -> state-load \
             -> CE) so EXEC works without `cat /proc/gpustep14`"
        );
        // step14 self-guards to the console GPU and chains step5/boot/8/9/10; its
        // internal live-echo streams each stage to the console, so a wedge leaves
        // its last stage visible even though this call would then never return.
        let report = self.bringup_step14();
        for line in report.lines() {
            crate::klog_warn!("[auto-bringup] {}", line);
        }
        if self.rm_device_instance.lock().is_some() {
            crate::klog_warn!(
                "[auto-bringup] console GPU RM-attached and state-loaded — EXEC enabled"
            );
        } else {
            crate::klog_warn!(
                "[auto-bringup] console GPU bring-up did NOT attach the RM; \
                 GEM/VM_BIND/EXEC stay ENODEV until reboot (see the stage trail above)"
            );
        }
    }

    /// Drains every `nouveau_vm_mappings` entry for which `matches` returns
    /// true. When `rm_unmap` is set, each is `vm_bind_unmap`'d via RM -- the
    /// same real RM call VM_BIND's own UNMAP op uses. Pass `false` after
    /// `ctx_free` has already destroyed the VAS (process exit): a second unmap
    /// on a stale `h_virt` is a use-after-free in RM. `context` is a short
    /// label prefixed onto each log line so it's clear which caller triggered
    /// the drain.
    fn drain_vm_mappings(
        &self,
        context: &str,
        mut matches: impl FnMut(&super::nouveau_uapi::NouveauVmMapping) -> bool,
        rm_unmap: bool,
    ) -> usize {
        let device_instance = *self.rm_device_instance.lock();
        let stale = {
            let mut maps = self.nouveau_vm_mappings.lock();
            let mut drained = Vec::new();
            let mut i = 0;
            while i < maps.len() {
                if matches(&maps[i]) {
                    drained.push(maps.remove(i));
                } else {
                    i += 1;
                }
            }
            drained
        };
        let drained_count = stale.len();
        if !rm_unmap {
            if drained_count > 0 {
                log::info!(
                    "[nouveau-uapi] {}: dropped {} VM_BIND mapping(s) locally (VAS already freed)",
                    context,
                    drained_count
                );
            }
            return drained_count;
        }
        for mapping in stale {
            lock::pump();
            let Some(device_instance) = device_instance else {
                log::warn!(
                    "[nouveau-uapi] {}: VA={:#x} (gem_handle={}) leaked -- GPU not attached to RM, can't vm_bind_unmap",
                    context, mapping.va, mapping.gem_handle
                );
                continue;
            };
            let status = nvidia_rm_sys::rm_init::vm_bind_unmap(
                device_instance,
                mapping.h_virt,
                mapping.size,
                mapping.va,
            );
            log::info!(
                "[nouveau-uapi] {}: dropped stale VM_BIND VA={:#x} (gem_handle={}) -> vm_bind_unmap status={:#x}",
                context, mapping.va, mapping.gem_handle, status
            );
        }
        drained_count
    }

    /// Applies a single `VM_BIND` op (`MAP` or `UNMAP`). Factored out so
    /// `DRM_IOCTL_NOUVEAU_VM_BIND` can loop it over an `op_count > 1`
    /// array -- see that arm's own comment on why this isn't atomic
    /// across ops.
    fn vm_bind_op(
        &self,
        device_instance: u32,
        ctx_idx: u32,
        owner_pid: u64,
        op: &super::nouveau_uapi::DrmNouveauVmBindOp,
    ) -> Result<(), i32> {
        use super::nouveau_uapi as nv;
        // Sparse regions have no GEM object behind them (`handle` is 0), so the
        // generic MAP path below -- which resolves `handle` to an RM memory
        // object and calls `Map` -- cannot serve them. Say so explicitly
        // instead of failing with a confusing "no such handle": NVK only asks
        // for these for sparse Vulkan resources, so a client that hits this is
        // using a feature this milestone does not have, not tripping over a
        // bookkeeping bug.
        let pte_kind = nv::vm_bind_pte_kind(op.flags);
        // On the Turing+ architectures this driver supports (Volta and later),
        // the GPU MMU addresses the uncompressed "generic" block-linear kind
        // (0x06, NV_MMU_VER2_PTE_KIND_GENERIC_MEMORY) IDENTICALLY to pitch
        // (0x00): the block-linear swizzle is performed by the ENGINE, through
        // the surface's block-height methods, not by the page table. The PTE
        // kind only changes how the L2/MMU interprets the bytes for COMPRESSED
        // surfaces (comptag/PLC lookups) -- and those we genuinely cannot
        // program yet. So a 0x06 surface can be mapped exactly like pitch and
        // reads back byte-for-byte correctly; only a compressed kind would
        // render garbage under a pitch mapping.
        //
        // Confirmed on real RTX hardware (dmesg): NVK's `nil` layout library
        // picks GENERIC_MEMORY (0x06) for every uncompressed tiled surface on
        // Turing/Ampere/Ada -- colour, depth and stencil alike -- and a
        // compositor's swapchain is always uncompressed. Refusing 0x06 here
        // was the SOLE reason vkBindImageMemory failed: labwc's swapchain for
        // HDMI-A-1 never allocated (VA=0x3ffd09c000 range=0x408000), Mesa fell
        // through gbm_bo_create, and the Wayland session could not start.
        // (Pre-Turing GPUs, which this driver does not claim to support, WOULD
        // need the MMU to do the tiling -- that is the case the old blanket
        // refusal was really guarding against.)
        let supported_pte_kind = nv::pte_kind_is_supported(pte_kind);
        // The kind actually handed to the RM map (programmed into the PTEs):
        //
        //  * 0x00..=0x06 -- the Turing UNCOMPRESSED kinds (PITCH, the Z/S
        //    family Z16/S8/S8Z24/ZF32_X24S8/Z24S8, GENERIC_MEMORY) -- are
        //    programmed VERBATIM, exactly like Linux nouveau (nouveau_uvmm
        //    passes the VM_BIND op's kind untouched down to the PTE writer).
        //    NVK bakes the SAME kind into its ZETA/texture descriptors, so
        //    the PTE and engine views must agree: substituting generic here
        //    was the "compositor fine / glxgears+vkcube garbage" split on
        //    real TU106 -- depth surfaces ask for Z16(0x01)/S8Z24(0x03) and
        //    were silently downgraded, so depth reads went through the wrong
        //    kind. These kinds have NO comptag requirement; programming them
        //    costs nothing.
        //  * 0x08..=0x0F -- the COMPRESSIBLE kinds -- are passed through and
        //    converted to their uncompressed pair by the RM's own HAL
        //    (memmgrGetUncompressedKind_TU102) on the C side: correct bytes,
        //    no comptags needed (this driver has no comptag allocator), and
        //    the Z/S identity of the kind is PRESERVED (Z16_COMPRESSIBLE ->
        //    Z16), unlike the old blanket generic downgrade.
        //  * anything else (0x07 = INVALID, or out of table) -- mapped with
        //    the RM default (pitch/generic), loudly.
        //
        // We never REFUSE a kind, and that is load-bearing: the old refusal
        // left the surface UNMAPPED and NVK's first draw that referenced it
        // froze the channel (PBDMA stall, no MMU fault). An imperfect mapping
        // is strictly better than a hang.
        let rm_pte_kind = if supported_pte_kind || (0x08..=0x0F).contains(&pte_kind) {
            pte_kind
        } else {
            nv::PTE_KIND_PITCH
        };
        if !supported_pte_kind {
            crate::klog_warn!(
                "[nouveau-uapi] VM_BIND: PTE kind {:#04x} {} handle={} VA={:#x} range={:#x}",
                pte_kind,
                if rm_pte_kind != 0 {
                    "is compressible -- programming its uncompressed pair (no comptag support here)"
                } else {
                    "is not in the Turing kind table -- mapping with the default kind"
                },
                op.handle,
                op.addr,
                op.range
            );
        }
        // Feed the /proc/gpudbg memory summary before the sparse/map dispatch,
        // so every op is counted (including the SPARSE ones refused just below).
        super::nouveau_uapi::record_vm_bind(
            op.op == nv::VM_BIND_OP_MAP,
            op.flags & nv::VM_BIND_SPARSE != 0,
            pte_kind,
            !supported_pte_kind,
        );
        // `rm_pte_kind` (an uncompressed kind verbatim, a compressible kind
        // for the C side to convert to its uncompressed pair, or 0 for the RM
        // default) is handed to rm_init::vm_bind_map below, which programs it
        // into the GEM memory's descriptor before the Map so it lands in the
        // PTEs -- Linux-nouveau-faithful kind handling.
        if op.flags & nv::VM_BIND_SPARSE != 0 {
            crate::klog_warn!(
                "[nouveau-uapi] VM_BIND: SPARSE regions are not implemented (op={} addr={:#x} \
                 range={:#x}) -- sparse Vulkan resources will fail, everything else is unaffected",
                op.op,
                op.addr,
                op.range
            );
            return Err(nv::EOPNOTSUPP);
        }
        match op.op {
            nv::VM_BIND_OP_MAP => {
                // MAP with handle=0 is how Mesa spells "unmap this range but
                // keep the VA reservation" (nvkmd_nouveau_va_unbind builds
                // exactly this op). Resolving handle 0 against the GEM table
                // used to ENOENT it -- another unmap shape that never
                // worked. It is a range unmap; treat it as one.
                if op.handle == 0 {
                    self.drain_vm_mappings(
                        &alloc::format!("VM_BIND MAP-nothing VA={:#x}+{:#x}", op.addr, op.range),
                        |m| {
                            m.owner_pid == owner_pid
                                && m.va < op.addr.wrapping_add(op.range)
                                && op.addr < m.va.wrapping_add(m.size)
                        },
                        true,
                    );
                    return Ok(());
                }
                let h_memory = {
                    let gem = self.nouveau_gem.lock();
                    // Only a holder may bind the object into its VAS: binding
                    // another process's buffer is a GPU read/write of it.
                    let Some(obj) = gem
                        .iter()
                        .find(|o| o.handle == op.handle && gem_usable_by(o, owner_pid))
                    else {
                        return Err(nv::ENOENT);
                    };
                    obj.h_memory
                };
                // REPLACE semantics, like Linux's gpuvm: a MAP over an
                // already-mapped range unmaps the old mapping first instead
                // of failing. On real hardware the missing half of this bit:
                // every Mesa VA heap starts at the SAME top address, this
                // driver's VAS is one global space, and the RM refuses a
                // fixed-VA reservation over a live one (eheap fixed-address
                // alloc -> 0x51) -- so the FIRST device of a boot worked and
                // every later one died at its first bind (vkCreateDevice
                // -13 in both labwc renderers). The displaced owner cannot
                // be executing anyway: EXEC is restricted to the RM
                // channel's owner, so the last binder is the one that runs.
                let replaced = self.drain_vm_mappings(
                    &alloc::format!(
                        "VM_BIND MAP replace VA={:#x}+{:#x} (this ctx, last binder wins)",
                        op.addr,
                        op.range
                    ),
                    // Scope to THIS process's context: two clients each have their
                    // own VA space, so the SAME VA in a different context is not a
                    // conflict -- replacing it would corrupt the other client.
                    |m| {
                        m.owner_pid == owner_pid
                            && m.va < op.addr.wrapping_add(op.range)
                            && op.addr < m.va.wrapping_add(m.size)
                    },
                    true,
                );
                if replaced > 0 {
                    crate::klog_warn!(
                        "[nouveau-uapi] VM_BIND MAP VA={:#x}+{:#x}: replaced {} stale mapping(s) \
                         from an earlier device generation (single global VAS, last binder wins)",
                        op.addr,
                        op.range,
                        replaced
                    );
                }
                // Capture the RM's own narration around the call. Those
                // `[eclipse-rm-trace] vm_bind_map: ...` lines carry the exact
                // per-stage NV_STATUS, but they are logged at DEBUG and so are
                // dropped at the boot log level real hardware uses -- they only
                // survive in the capture buffer. On failure, replay them through
                // klog, which bypasses the level filter entirely, so one boot
                // shows WHY the RM refused without needing a special LOG=.
                nvidia_rm_sys::os_interface::capture_begin();
                let rm_result = nvidia_rm_sys::rm_init::vm_bind_map(
                    device_instance,
                    ctx_idx,
                    h_memory,
                    op.range,
                    op.addr,
                    op.bo_offset,
                    rm_pte_kind,
                );
                let rm_narration = nvidia_rm_sys::os_interface::capture_take();
                let replay_rm = |narration: Option<alloc::string::String>| {
                    if let Some(text) = narration {
                        for line in text.lines().filter(|l| !l.trim().is_empty()) {
                            crate::klog_warn!("[nouveau-uapi] rm: {}", line);
                        }
                    }
                };
                match rm_result {
                    Ok(b) if b.map_status == 0 => {
                        self.nouveau_vm_mappings.lock().push(nv::NouveauVmMapping {
                            gem_handle: op.handle,
                            h_virt: b.h_virt,
                            owner_pid,
                            va: b.actual_va,
                            size: op.range,
                            bo_offset: op.bo_offset,
                        });
                        log::info!(
                            "[nouveau-uapi] VM_BIND MAP handle={} -> VA={:#x} ({} bytes)",
                            op.handle,
                            b.actual_va,
                            op.range
                        );
                        Ok(())
                    }
                    Ok(b) => {
                        replay_rm(rm_narration);
                        // Print WHAT was asked for, not just that it failed:
                        // the RM refuses a fixed-VA reservation whose address
                        // or size does not suit the VA space (alignment, or a
                        // range outside it), and those are exactly the numbers
                        // needed to tell those cases apart from a log alone.
                        crate::klog_warn!(
                            "[nouveau-uapi] VM_BIND MAP failed: handle={} VA={:#x} range={:#x} \
                             bo_offset={:#x} -> virt_status={:#x} map_status={:#x}",
                            op.handle,
                            op.addr,
                            op.range,
                            op.bo_offset,
                            b.virt_status,
                            b.map_status
                        );
                        Err(nv::EIO)
                    }
                    Err(status) => {
                        replay_rm(rm_narration);
                        crate::klog_warn!(
                            "[nouveau-uapi] VM_BIND MAP failed: handle={} VA={:#x} range={:#x} \
                             -- NV_STATUS={:#x}",
                            op.handle,
                            op.addr,
                            op.range,
                            status
                        );
                        Err(nv::EIO)
                    }
                }
            }
            nv::VM_BIND_OP_UNMAP => {
                // The real uAPI unmaps by VA RANGE -- the op carries no GEM
                // handle (Mesa's nvkmd_nouveau_va_free sends handle=0). The
                // old lookup required `gem_handle == op.handle`, which a
                // real client can never satisfy, so EVERY explicit unmap
                // returned ENOENT (the hardware log's "errno 2"), the
                // mapping stayed live in the shared VAS, and the next
                // device's fixed-VA reserve collided (0x51 -> EIO -> -13).
                // Match by overlap instead, and like Linux, unmapping a
                // range with nothing in it is a SUCCESS, not ENOENT --
                // Mesa's va_free unconditionally unmaps even reserve-only
                // VAs it never bound, and treats a refusal as "leak the VA".
                self.drain_vm_mappings(
                    &alloc::format!("VM_BIND UNMAP VA={:#x}+{:#x}", op.addr, op.range),
                    |m| {
                        m.owner_pid == owner_pid
                            && m.va < op.addr.wrapping_add(op.range)
                            && op.addr < m.va.wrapping_add(m.size)
                    },
                    true,
                );
                Ok(())
            }
            other => {
                log::warn!("[nouveau-uapi] VM_BIND: unknown op {:#x}", other);
                Err(nv::EINVAL)
            }
        }
    }

    /// Submits a single pushbuffer with no fence -- shared by `EXEC`'s
    /// `sig_count == 0` path (every push, since none needs a fence) and
    /// its `sig_count > 0` path (every push but the last, which gets
    /// `exec_submit_signaled` instead -- see that arm's own comment).
    fn submit_push_plain(
        &self,
        device_instance: u32,
        ctx_idx: u32,
        push: &super::nouveau_uapi::DrmNouveauExecPush,
    ) -> Result<(), i32> {
        use super::nouveau_uapi as nv;
        // Capture the RM's own narration (the `[eclipse-rm-trace] exec_submit:`
        // lines, including the ring state on a RING FULL) and replay it
        // through klog when the submit fails -- klog has no level filter, so
        // one boot at the hardware's default LOG shows WHY, not just that.
        nvidia_rm_sys::os_interface::capture_begin();
        let rm_result =
            nvidia_rm_sys::rm_init::exec_submit(device_instance, ctx_idx, push.va, push.va_len);
        let rm_narration = nvidia_rm_sys::os_interface::capture_take();
        let replay_rm = |narration: Option<alloc::string::String>| {
            if let Some(text) = narration {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    crate::klog_warn!("[nouveau-uapi] rm: {}", line);
                }
            }
        };
        match rm_result {
            Ok(r) if r.submit_status == 0 => {
                log::info!(
                    "[nouveau-uapi] EXEC pushVA={:#x} len={} -> submitted (ring slot after={})",
                    push.va,
                    push.va_len,
                    r.gp_put_after
                );
                Ok(())
            }
            Ok(r) => {
                let sig = nv::exec_failure_sig(
                    0x01,
                    &[
                        r.lookup_status,
                        r.map_status,
                        r.token_status,
                        r.submit_status,
                    ],
                );
                if nv::exec_failure_changed(sig) {
                    replay_rm(rm_narration);
                    crate::klog_warn!(
                        "[nouveau-uapi] EXEC submit failed: lookup={:#x} map={:#x} token={:#x} submit={:#x} (identical repeats suppressed)",
                        r.lookup_status,
                        r.map_status,
                        r.token_status,
                        r.submit_status
                    );
                }
                Err(nv::EIO)
            }
            Err(status) => {
                let sig = nv::exec_failure_sig(0x02, &[status]);
                if nv::exec_failure_changed(sig) {
                    replay_rm(rm_narration);
                    crate::klog_warn!(
                        "[nouveau-uapi] EXEC failed, NV_STATUS={:#x} (identical repeats suppressed)",
                        status
                    );
                }
                Err(nv::EIO)
            }
        }
    }

    /// One-shot submission self-test, run on the first RM-backed
    /// CHANNEL_ALLOC of the boot while the GPFIFO ring is still empty.
    ///
    /// Why it exists: the hardware kept dying with RC error 31
    /// (`ROBUST_CHANNEL_FIFO_ERROR_MMU_ERR_FLT` -- the GPU touched an
    /// unmapped VA) on NVK's very first submission, with every uAPI call
    /// reporting success, and the RM's Xid narration (which would name the
    /// faulting address) never surfaces in this port. This test removes Mesa
    /// from the equation: the KERNEL authors a minimal, known-good push --
    /// one host-class semaphore RELEASE, the exact 6-dword stream the C
    /// fence builder uses -- places it in a GART GEM object bound through
    /// OUR OWN `VM_BIND` path at a VA in the kernel-reserved half of NVK's
    /// map (so it can never collide), and submits it through the same
    /// `exec_submit_signaled` EXEC uses.
    ///
    /// Three verdicts, each pinning the bug to a different layer:
    /// * **stage A passes** (fence lands AND the semaphore payload appears
    ///   in the GEM page): the whole path -- GART alloc, VM_BIND PTEs, PBDMA
    ///   fetch from a bound VA, method execution, fence -- works for a
    ///   kernel push. The MMU fault must then come from the CONTENT of
    ///   Mesa's pushes (a VA its methods reference), not from the plumbing.
    /// * **stage A fails but stage B passes** (same submit, but the push
    ///   fetched from the channel's own RM-mapped buffer): the channel
    ///   executes fine from RM-created mappings and the bug is precisely
    ///   that OUR VM_BIND PTEs are not GPU-visible.
    /// * **both fail**: the channel executes nothing at all -- the problem
    ///   is channel-level (step17: scheduling/runlist/USERD), and everything
    ///   VM_BIND/Mesa is a red herring.
    fn nouveau_channel_selftest(&self, device_instance: u32, buf_gpu_va: u64) {
        use super::nouveau_uapi as nv;
        use nvidia_rm_sys::rm_init as rm;
        // Kernel-reserved half of NVK's VA layout ([1<<39, 1<<40)): NVK's own
        // heaps never allocate here, so a fixed address is collision-free.
        const TEST_VA: u64 = 1u64 << 39;
        const TEST_SIZE: u64 = 0x2000; // page 0: pushbuffer; page 1: semaphore
        const SEM_OFF: u64 = 0x1000;
        const PAYLOAD: u32 = 0x5e1f_7e57;
        // ECLIPSE_PUSH_HDR(subch=0, NVC46F_SEM_ADDR_LO=0x5c, count=5):
        // SEC_OP=INC_METHOD (1<<29) | count<<16 | subch<<13 | mthd>>2.
        const HDR_SEM: u32 = (1 << 29) | (5 << 16) | (0x5c >> 2);

        let alloc = match rm::gem_alloc(device_instance, TEST_SIZE, true) {
            Ok(a) if a.alloc_status == 0 => a,
            other => {
                crate::klog_warn!(
                    "[nouveau-uapi] SELFTEST: GART alloc failed ({:?}) -- cannot run",
                    other.map(|a| a.alloc_status)
                );
                return;
            }
        };
        let pa = match rm::gem_map_cpu(device_instance, alloc.h_memory) {
            Ok(m) if m.lookup_status == 0 => m.phys_addr,
            _ => {
                crate::klog_warn!("[nouveau-uapi] SELFTEST: gem_map_cpu failed -- cannot run");
                rm::gem_free(device_instance, alloc.h_memory);
                return;
            }
        };
        let bind =
            match rm::vm_bind_map(device_instance, 0, alloc.h_memory, TEST_SIZE, TEST_VA, 0, 0) {
                Ok(b) if b.map_status == 0 => b,
                _ => {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST: VM_BIND of the test page failed -- cannot run"
                    );
                    rm::gem_free(device_instance, alloc.h_memory);
                    return;
                }
            };
        let base = crate::bus::phys_to_virt(pa as usize);
        let sem_va = TEST_VA + SEM_OFF;
        unsafe {
            core::ptr::write_volatile((base + SEM_OFF as usize) as *mut u32, 0);
            let p = base as *mut u32;
            core::ptr::write_volatile(p, HDR_SEM);
            core::ptr::write_volatile(p.add(1), sem_va as u32);
            core::ptr::write_volatile(p.add(2), ((sem_va >> 32) & 0xff) as u32);
            core::ptr::write_volatile(p.add(3), PAYLOAD);
            core::ptr::write_volatile(p.add(4), 0);
            // SEM_EXECUTE: OPERATION_RELEASE, 32-bit payload, no WFI, no
            // timestamp -- all other fields zero.
            core::ptr::write_volatile(p.add(5), 1);
        }
        let stage_a = rm::exec_submit_signaled(
            device_instance,
            0,
            TEST_VA,
            24,
            nv::next_fence_payload(),
            500,
        );
        let landed = unsafe { core::ptr::read_volatile((base + SEM_OFF as usize) as *const u32) };
        let a_ok = matches!(
            &stage_a,
            Ok(r) if r.submit_status == 0 && r.fence_submit_status == 0 && r.fence_wait_status == 0
        );
        if a_ok && landed == PAYLOAD {
            crate::klog_info!(
                "[nouveau-uapi] SELFTEST stage A PASS: kernel push executed from VM_BIND VA {:#x}, fence landed, semaphore={:#x} -- the submission plumbing works; an MMU fault after this points at the CONTENT of Mesa's pushes",
                TEST_VA,
                landed
            );
        } else {
            let (ss, fs, fw) = match &stage_a {
                Ok(r) => (r.submit_status, r.fence_submit_status, r.fence_wait_status),
                Err(e) => (*e, 0xffff_ffff, 0xffff_ffff),
            };
            crate::klog_warn!(
                "[nouveau-uapi] SELFTEST stage A FAIL: submit={:#x} fenceSubmit={:#x} fenceWait={:#x} semaphore={:#x} (expected {:#x}) -- kernel push from a VM_BIND VA did not execute",
                ss,
                fs,
                fw,
                landed,
                PAYLOAD
            );
            // Stage B: identical submit, but the caller push is the fence
            // method stream stage A just wrote into the CHANNEL's own
            // RM-mapped buffer (re-executing a stale semaphore release is
            // harmless). Distinguishes "our PTEs are invisible" from "the
            // channel executes nothing".
            let fence_pb_va = buf_gpu_va + 0x9000;
            let stage_b = rm::exec_submit_signaled(
                device_instance,
                0,
                fence_pb_va,
                24,
                nv::next_fence_payload(),
                500,
            );
            match stage_b {
                Ok(r)
                    if r.submit_status == 0
                        && r.fence_submit_status == 0
                        && r.fence_wait_status == 0 =>
                {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST stage B PASS: the same push executed from the channel's RM-mapped buffer ({:#x}) -- the channel is fine and OUR VM_BIND PTEs are NOT GPU-visible (the bug is in vm_bind_map's Map)",
                        fence_pb_va
                    );
                }
                Ok(r) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST stage B FAIL too: submit={:#x} fenceSubmit={:#x} fenceWait={:#x} -- the channel executes NOTHING (kernel pushes from RM-mapped VAs included); the problem is channel-level (step17), not VM_BIND/Mesa",
                        r.submit_status,
                        r.fence_submit_status,
                        r.fence_wait_status
                    );
                }
                Err(e) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] SELFTEST stage B FAIL too (NV_STATUS={:#x}) -- channel-level problem",
                        e
                    );
                }
            }
        }
        let _ = rm::vm_bind_unmap(device_instance, bind.h_virt, TEST_SIZE, TEST_VA);
        rm::gem_free(device_instance, alloc.h_memory);
    }

    /// Whether the RM ladder actually ran, i.e. `step16`/`step17` built the
    /// VA space and the GR channel.
    ///
    /// This is what VA-space work (`VM_BIND`) and VRAM allocation (`GEM_NEW`)
    /// need: both operate on the VAS, which this driver models as a SINGLE
    /// global object shared by every client, so it does not matter which
    /// client's CHANNEL_ALLOC happened to run the ladder -- only that some did.
    /// A boot where no client ever got an RM-backed channel still has no VAS,
    /// and binding into it would be binding into nothing.
    fn nouveau_rm_vas_ready(&self) -> bool {
        self.nouveau_channels.lock().iter().any(|c| c.rm_backed)
    }

    /// Whether THIS process owns the RM-backed channel.
    ///
    /// Submission is different from VA work: `EXEC` pushes into the GPFIFO
    /// ring that `step17` built, and this milestone has exactly one. Two
    /// clients pushing into the same ring corrupt each other, so submission
    /// stays restricted to the channel's owner even though the VAS is shared.
    /// GR-engine hang probe: at the moment a fence wait expired, snapshot the
    /// BAR0 registers that distinguish the four mutually-exclusive failure
    /// modes:
    ///   (a) MMU fault  — latched in 0xb83090 (valid=1)
    ///   (b) GR method stall — 0x400700 non-zero + 0x400704
    ///   (c) PBDMA never fetched the push — GP_GET != GP_PUT
    ///   (d) FECS/GPCCS ctx-switch hang — 0x409c00 / 0x41a000
    ///
    /// No RM calls: two prior attempts to involve the RM on this path crashed
    /// the machine. Pure BAR0 reads only. Stored in LAST_GR_HANG_PROBE for
    /// /proc/gpudbg; first timeout per boot wins (the ring stays wedged
    /// after). Shared by the RM per-submit path (inline poll timeout) and the
    /// direct-submit path (`syncobj` fence timeout / ring-full timeout).
    fn gr_hang_probe(
        &self,
        ctx_idx: u32,
        owner_pid: u64,
        work_token: u32,
        runlist_id: u32,
        timeout_ms: u32,
    ) {
        use super::nouveau_uapi as nv;
        let bar0 = self._bar0;
        let rd = |off: usize| unsafe { core::ptr::read_volatile((bar0 + off) as *const u32) };
        let wr =
            |off: usize, v: u32| unsafe { core::ptr::write_volatile((bar0 + off) as *mut u32, v) };

        // work_token encodes runlistId|chId (Turing: bits [19:16]
        // = runlistId, bits [11:0] = chId). Capture from exec
        // result so the probe names the EXACT channel that timed
        // out, not a generic PBDMA0 snapshot that may belong to
        // a different concurrent context.
        // Channel ID: low 12 bits of the work-submit token
        // (Turing/Ampere kernel_fifo_tu102.c: chId = token & 0xfff).
        let ch_id = work_token & 0x0fff;

        // NV_PGRAPH_STATUS: overall GR busy/idle.
        // 0 = idle; any bit set = a sub-engine is active/stalled.
        let gr_status = rd(0x0040_0700);
        // NV_PGRAPH_TRAPPED_ADDR: the method that stalled GR.
        //   bits [20:16] = subchannel, bits [12:0] = method
        let gr_trap_addr = rd(0x0040_0704);
        let gr_trap_lo = rd(0x0040_0708);
        let gr_trap_hi = rd(0x0040_070c);

        // PBDMA0 ring pointers (GR runlist is usually PBDMA0).
        // GP_GET == GP_PUT means the PBDMA finished fetching;
        // GP_GET < GP_PUT means it never fetched the push at all.
        let pb0_put = rd(0x0004_0000);
        let pb0_get = rd(0x0004_0014);
        // PBDMA execution state — decodes WHY GP_GET is stuck,
        // the one thing GP_PUT/GP_GET alone cannot tell apart:
        //   STATUS(0x40100): the PBDMA channel/exec state machine.
        //   GET(0x40018): pushbuffer-level get. If it advanced
        //     into the pending segment the host DID fetch the
        //     entry and is executing/blocked inside the push
        //     (a semaphore-acquire NVK baked in -> explicit-sync).
        //   INTR_0(0x40108): pending PBDMA interrupts. A blocked
        //     semaphore-acquire that exceeds its timeout raises
        //     one here; a channel that was simply never scheduled
        //     onto the runlist raises none. So INTR_0!=0 => the
        //     push was fetched and blocked (semaphore/PB error);
        //     INTR_0==0 with GP_GET frozen => never scheduled
        //     (runlist/doorbell), NOT a semaphore block.
        let pb0_status = rd(0x0004_0100);
        let pb0_pbget = rd(0x0004_0018);
        let pb0_intr = rd(0x0004_0108);

        // NV_PFIFO_CHRAM_CHANNEL(ch_id): per-channel PCCSR register.
        // Stride is 8 bytes.  The FIRST word (+0x0) is the instance
        // pointer; the SECOND word (+0x4) carries channel state:
        //   bit 0       = ENABLE
        //   bits [27:24] = STATUS (0=IDLE, 1=PENDING, 2=CTX_RELOAD,
        //                          3=BUSY/ACTIVE, 4=PENDING_CTX_RELOAD,
        //                          5=PENDING_ACQ, 6=ENG_SEL_PENDING)
        //   bit 28      = BUSY
        // Reading at timeout tells us whether the channel was visible
        // to the host FIFO scheduler. Matches the existing decode at
        // drivers/src/display/nvidia.rs:3164 (PCCSR0(0x800004)).
        let pccsr_off = 0x0080_0004usize + (ch_id as usize) * 8;
        let pccsr_val = rd(pccsr_off);
        let pccsr_enable = pccsr_val & 1;
        let pccsr_busy = (pccsr_val >> 28) & 1;
        let pccsr_status = (pccsr_val >> 24) & 0xf;

        // HUB MMU fault latch (non-replayable, TU102 0xb83080..90).
        // valid = bit 31 of INFO1; reason = bits [4:0].
        let f_info1 = rd(0x00b8_3090);
        let f_addr_lo = rd(0x00b8_3080);
        let f_addr_hi = rd(0x00b8_3084);
        let f_info0 = rd(0x00b8_3088);

        // NV_PGRAPH_PRI_FECS_CTXSW_STATUS_FE_0: FECS state machine.
        // Non-zero when FECS is mid ctx-switch (save/restore).
        // 0x1 = SAVE_CONTEXT, 0x2 = RESTORE_CONTEXT (loading grctx).
        let fecs_status = rd(0x0040_9c00);
        // NV_PGRAPH_PRI_FECS_HOST_INT_STATUS: FECS interrupt flags.
        let fecs_intr = rd(0x0040_9c14);
        // NV_PGRAPH_PRI_FECS_CTXSW_MAILBOX(0/1): last FECS ucode
        // message and error detail. Ucode updates MAILBOX0 at each
        // step of the context-save/restore microcode; a non-zero
        // MAILBOX0 with fecs_status==0x2 (RESTORE) shows exactly
        // which grctx step the ucode reached before hanging. These
        // are the registers an rc_dump uses to diagnose FECS hangs.
        let fecs_mb0 = rd(0x0040_9800);
        let fecs_mb1 = rd(0x0040_9804);
        // NV_PGRAPH_PRI_FECS_CTXSW_PRIV_ERROR_FE_0: privilege
        // error flags set if FECS ucode tried to read an invalid
        // register (context-image addresses wrong / buffer too small).
        let fecs_priv_err = rd(0x0040_9c10);

        // NV_PGRAPH_PRI_GPC0_GPCCS_CTXSW_STATUS_GPC_0: GPCCS state.
        let gpccs_status = rd(0x0041_a000);

        let drained = pb0_get == pb0_put;
        let mmu_valid = (f_info1 >> 31) & 1;
        let mmu_reason = f_info1 & 0x1f;
        let gr_subchan = (gr_trap_addr >> 16) & 0x1f;
        let gr_method = gr_trap_addr & 0x1fff;

        // Diagnosis hint: one word that names the most likely
        // root cause, to guide which fix the maintainer applies.
        let hint = if mmu_valid != 0 {
            "MMU-FAULT: GPU touched an unmapped VA -- check VM_BIND mappings"
        } else if gr_status != 0 && gr_method != 0 {
            "GR-STALL: GR engine stuck on a method -- golden-ctx/GR-init incomplete?"
        } else if !drained && pb0_intr != 0 {
            "PBDMA-STALL (fetched, then BLOCKED): a PBDMA interrupt is pending -- the host fetched the push and stalled inside it, i.e. a semaphore-acquire NVK baked in never released (explicit-sync), or a PB error. Decode INTR_0/STATUS."
        } else if !drained {
            "PBDMA-STALL (never fetched): GP_GET frozen with no PBDMA interrupt -- the channel is not runlist-resident, so the doorbell/runlist scheduling never ran this push (NOT a semaphore block)."
        } else if fecs_status != 0 || gpccs_status != 0 {
            "FECS/GPCCS: ctx-switch hang -- GR context-image incomplete or global ctx buffers not mapped; check ctx_prime outcome and grctx init for this client channel's class object"
        } else {
            "GR-IDLE-NOFENCE: GR finished but fence-semaphore write never arrived -- WFI/coherency?"
        };

        // Recovery attempt for the "never fetched, no PBDMA
        // interrupt" case: re-ring the doorbell with the exact
        // work-submit token the RM issued for THIS submit.
        // Rationale: the PBDMA may have missed the original
        // doorbell (GSP-to-host signalling is not guaranteed
        // lossless at high submission rates on Turing). Writing
        // the token to 0xbb0090 a second time is idempotent if
        // the channel is already running, and may unblock it if
        // the doorbell was lost. Pure BAR0 write; no RM call.
        // Only attempted when GP_GET is frozen AND INTR_0 == 0
        // (i.e., the push never made it to the PBDMA at all).
        if !drained && pb0_intr == 0 && work_token != 0 {
            wr(0x00bb_0090, work_token);
            crate::klog_warn!(
                "[nouveau-uapi] EXEC: ctx={} PBDMA-stall recovery: re-rang doorbell token={:#010x} runlist={} ch={}",
                ctx_idx, work_token, runlist_id, ch_id
            );
        }

        // FECS ctx-switch hang: log a targeted klog line so
        // dmesg immediately points at grctx/golden-context as
        // the root. CTXSW_STATUS==0x2 = RESTORE_CONTEXT: FECS
        // ucode is stuck trying to load the client channel's GR
        // context image. Most likely causes: ctx_prime timed out
        // (golden context was never loaded before NVK's first
        // push hit the cold load), or a global-ctx-buffer
        // (patch/attribute/pagepool) was not mapped for this ctx.
        if fecs_status != 0 || gpccs_status != 0 {
            crate::klog_warn!(
                "[nouveau-uapi] EXEC: ctx={} FECS/GPCCS ctx-switch hang \
                 (FECS_STATUS={:#010x} GPCCS_STATUS={:#010x} \
                 MAILBOX0={:#010x} MAILBOX1={:#010x} PRIV_ERR={:#010x}) -- \
                 GR context-image for this client channel is incomplete; \
                 check whether ctx_prime timed out at CHANNEL_ALLOC \
                 (see /proc/gpudbg last PRIME line for ctx={})",
                ctx_idx,
                fecs_status,
                gpccs_status,
                fecs_mb0,
                fecs_mb1,
                fecs_priv_err,
                ctx_idx
            );
        }

        // Whether THIS ctx was ever primed: the decisive
        // datum for a FECS RESTORE hang. "never recorded"
        // here means the EXEC ran on a ctx whose build/prime
        // path did not complete -- the exact anomaly a real
        // RTX repro exhibited (ctx=3 hung with no prime
        // line at all).
        let prime_state = nv::prime_line_for(ctx_idx).unwrap_or_else(|| {
            alloc::string::String::from(
                "NEVER RECORDED -- this ctx reached EXEC without its build/prime completing",
            )
        });
        nv::record_gr_hang_probe(alloc::format!(
            "ctx={ctx} pid={pid} fence TIMEOUT after {ms}ms\n\
             HINT: {hint}\n\
             prime-state: {prime_state}\n\
             submit: work_token={work_token:#010x} runlist_id={runlist_id} ch_id={ch_id}\n\
             GR_STATUS(0x400700)={gr_status:#010x} (0=idle)\n\
             TRAPPED_ADDR(0x400704)={gr_trap_addr:#010x} subchan={gr_subchan} method={gr_method:#06x}\n\
             TRAPPED_DATA lo={gr_trap_lo:#010x} hi={gr_trap_hi:#010x}\n\
             PBDMA0 GP_PUT(0x40000)={pb0_put:#010x} GP_GET(0x40014)={pb0_get:#010x} drained={drained}\n\
             PBDMA0 STATUS(0x40100)={pb0_status:#010x} GET(0x40018)={pb0_pbget:#010x} INTR_0(0x40108)={pb0_intr:#010x}\n\
             PCCSR ch{ch_id}(0x{pccsr_off:x})={pccsr_val:#010x} enable={pccsr_enable} busy={pccsr_busy} status={pccsr_status} (0=IDLE,1=PENDING,3=ACTIVE)\n\
             MMU FAULT_INFO1(0xb83090)={f_info1:#010x} valid={mmu_valid} reason={mmu_reason:#04x}\n\
               FAULT_ADDR={f_addr_hi:#010x}_{f_addr_lo:#010x} engine_id={engine_id:#04x}\n\
             FECS CTXSW_STATUS(0x409c00)={fecs_status:#010x} (0=IDLE,1=SAVE,2=RESTORE) HOST_INT(0x409c14)={fecs_intr:#010x}\n\
             FECS MAILBOX0(0x409800)={fecs_mb0:#010x} MAILBOX1(0x409804)={fecs_mb1:#010x} PRIV_ERR(0x409c10)={fecs_priv_err:#010x}\n\
             GPCCS GPC0_STATUS(0x41a000)={gpccs_status:#010x}",
            ctx = ctx_idx,
            pid = owner_pid,
            ms = timeout_ms,
            hint = hint,
            work_token = work_token,
            runlist_id = runlist_id,
            ch_id = ch_id,
            gr_status = gr_status,
            gr_trap_addr = gr_trap_addr,
            gr_subchan = gr_subchan,
            gr_method = gr_method,
            gr_trap_lo = gr_trap_lo,
            gr_trap_hi = gr_trap_hi,
            pb0_put = pb0_put,
            pb0_get = pb0_get,
            pb0_status = pb0_status,
            pb0_pbget = pb0_pbget,
            pb0_intr = pb0_intr,
            drained = drained,
            pccsr_off = pccsr_off,
            pccsr_val = pccsr_val,
            pccsr_enable = pccsr_enable,
            pccsr_busy = pccsr_busy,
            pccsr_status = pccsr_status,
            f_info1 = f_info1,
            mmu_valid = mmu_valid,
            mmu_reason = mmu_reason,
            f_addr_hi = f_addr_hi,
            f_addr_lo = f_addr_lo,
            engine_id = f_info0 & 0xff,
            fecs_status = fecs_status,
            fecs_intr = fecs_intr,
            fecs_mb0 = fecs_mb0,
            fecs_mb1 = fecs_mb1,
            fecs_priv_err = fecs_priv_err,
            gpccs_status = gpccs_status,
        ));
    }

    /// Whether context `ctx_idx` can take the direct-submit path, preparing
    /// it on first touch: ONE RM entry (`exec_fast_prepare`) per channel
    /// lifetime, then EXEC never enters the RM again for it.
    fn fast_ctx_ready(&self, device_instance: u32, ctx_idx: u32) -> bool {
        use super::nouveau_uapi as nv;
        if !nv::exec_fast_enabled() || ctx_idx >= nv::MAX_CTX {
            return false;
        }
        const PREPARE_WAIT_US: u64 = 2_000_000;
        let start = unsafe { crate::bus::drivers_timer_now_as_micros() };
        loop {
            {
                let mut slots = self.nouveau_fast.lock();
                match &slots[ctx_idx as usize] {
                    FastSlot::Ready(_) => return true,
                    FastSlot::Failed => return false,
                    FastSlot::Preparing => {}
                    FastSlot::Unprepared => {
                        // Claim the build; every other caller waits below.
                        slots[ctx_idx as usize] = FastSlot::Preparing;
                        break;
                    }
                }
            }
            // Another thread is building this context: wait for its verdict
            // (the RM prepare is hundreds of microseconds to a few ms) rather
            // than falling back to the RM path, which would submit on the
            // same channel behind the direct ring's back.
            if unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(start)
                >= PREPARE_WAIT_US
            {
                crate::klog_warn!(
                    "[nouveau-uapi] ctx{}: another thread's exec_fast_prepare did not finish in 2s -- RM per-submit path for this EXEC",
                    ctx_idx
                );
                return false;
            }
            gpu_spin();
        }
        // Prepare OUTSIDE the slot lock: this enters the RM (RmGate, hundreds
        // of microseconds) and the slot lock is an IRQ-off spinlock.
        nvidia_rm_sys::os_interface::capture_begin();
        let prepared = nvidia_rm_sys::rm_init::exec_fast_prepare(device_instance, ctx_idx);
        let narration = nvidia_rm_sys::os_interface::capture_take();
        let replay_rm = |narration: Option<alloc::string::String>| {
            if let Some(text) = narration {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    crate::klog_warn!("[nouveau-uapi] rm: {}", line);
                }
            }
        };
        let built = match prepared {
            Ok(f) if f.status == 0 => match nv::check_encodings(&f) {
                Ok(()) => Some(f),
                Err((what, ours, theirs)) => {
                    crate::klog_warn!(
                        "[nouveau-uapi] ctx{}: direct submit encoder self-check FAILED on {}: kernel={:#010x} RM(DRF)={:#010x} -- refusing the direct path for this context",
                        ctx_idx, what, ours, theirs
                    );
                    None
                }
            },
            Ok(f) => {
                replay_rm(narration);
                crate::klog_warn!(
                    "[nouveau-uapi] ctx{}: exec_fast_prepare stage failed (status={:#x}) -- RM per-submit path for this context",
                    ctx_idx, f.status
                );
                None
            }
            Err(status) => {
                replay_rm(narration);
                crate::klog_warn!(
                    "[nouveau-uapi] ctx{}: exec_fast_prepare NV_STATUS={:#x} -- RM per-submit path for this context",
                    ctx_idx, status
                );
                None
            }
        };
        let mut slots = self.nouveau_fast.lock();
        if !matches!(slots[ctx_idx as usize], FastSlot::Preparing) {
            // The context was torn down (`fast_release`) while we were in the
            // RM: our claim is gone, so drop what we built instead of
            // publishing state for a freed channel.
            drop(slots);
            if built.is_some() {
                lock::pump();
                let _ = nvidia_rm_sys::rm_init::exec_fast_release(device_instance, ctx_idx);
            }
            return false;
        }
        match built {
            Some(f) => {
                let ctx = nv::FastCtx {
                    userd_gpget: f.userd_cpu as usize + f.userd_gpget_off as usize,
                    userd_gpput: f.userd_cpu as usize + f.userd_gpput_off as usize,
                    gpfifo_va: crate::bus::phys_to_virt(f.gpfifo_phys as usize),
                    fence_pb_va: crate::bus::phys_to_virt(f.fence_pb_phys as usize),
                    fence_sem_va: crate::bus::phys_to_virt(f.fence_sem_phys as usize),
                    buf_gpu_va: f.buf_gpu_va,
                    fence_pb_off: f.fence_pb_off,
                    fence_sem_off: f.fence_sem_off,
                    slot_bytes: f.slot_bytes,
                    entries: f.gpfifo_entries,
                    work_token: f.work_token,
                    runlist_id: f.runlist_id,
                    doorbell_va: self._bar0 + f.doorbell_reg as usize,
                    next_payload: 1,
                    submits: 0,
                    fenced: 0,
                    last_fence: None,
                };
                // Landing zone starts BELOW every payload; slot page cleared.
                unsafe {
                    core::ptr::write_volatile(ctx.fence_sem_va as *mut u32, 0);
                    core::ptr::write_bytes(
                        ctx.fence_pb_va as *mut u8,
                        0,
                        (ctx.entries as usize) * (ctx.slot_bytes as usize),
                    );
                }
                let (put, get) = unsafe {
                    (
                        core::ptr::read_volatile(ctx.userd_gpput as *const u32),
                        core::ptr::read_volatile(ctx.userd_gpget as *const u32),
                    )
                };
                crate::klog_info!(
                    "[nouveau-uapi] ctx{}: direct submit ACTIVE (token={:#x} runlist={} doorbell=BAR0+{:#x} ring={} entries GPPut={} GPGet={}) -- EXEC no longer enters the RM nor waits for the GPU; boot with nvidia.exec_rm to compare",
                    ctx_idx, ctx.work_token, ctx.runlist_id, f.doorbell_reg, ctx.entries, put, get
                );
                slots[ctx_idx as usize] = FastSlot::Ready(ctx);
                true
            }
            None => {
                slots[ctx_idx as usize] = FastSlot::Failed;
                false
            }
        }
    }

    /// The direct submit itself: optional host SEM ACQUIRE streams first
    /// (`acquires`: `(sem_gpu_va, payload)`), then `pushes` as consecutive GP
    /// entries, then (if `with_fence`) one more entry pointing at a per-slot
    /// host SEM RELEASE stream, GPPut bumped through the persistent USERD
    /// window, doorbell poked. Returns `(fence_va_cpu, fence_gpu_va, payload)`
    /// for the syncobj layer. Waits (bounded, 10 s) for ring space if the
    /// channel is behind.
    fn fast_submit(
        &self,
        ctx_idx: u32,
        pushes: &[super::nouveau_uapi::DrmNouveauExecPush],
        with_fence: bool,
        acquires: &[(u64, u32)],
    ) -> Result<Option<(usize, u64, u32)>, super::nouveau_uapi::FastSubmitError> {
        use super::nouveau_uapi as nv;
        const RING_TIMEOUT_US: u64 = 10_000_000;
        let needed = pushes.len() as u32 + with_fence as u32 + acquires.len() as u32;
        let start = unsafe { crate::bus::drivers_timer_now_as_micros() };
        let mut waited = false;
        loop {
            let (put, get) = {
                let mut slots = self.nouveau_fast.lock();
                let FastSlot::Ready(f) = &mut slots[ctx_idx as usize] else {
                    return Err(nv::FastSubmitError::Gone);
                };
                let entries = f.entries;
                // GPPut is re-read from the USERD rather than cached: the
                // /proc/gpustepNN self-tests still submit on ctx 0 through the
                // RM path and move it behind our back.
                let put_raw = unsafe { core::ptr::read_volatile(f.userd_gpput as *const u32) };
                let get_raw = unsafe { core::ptr::read_volatile(f.userd_gpget as *const u32) };
                // A channel whose ring has no entries is one the RM handed us
                // without a usable GPFIFO: the wrap arithmetic would divide by
                // zero, which from an `EXEC` ioctl is a kernel panic any
                // process with the device open could raise. Report the
                // direct-submit state as gone so `EXEC` falls back to the RM.
                let Some(nv::RingState { put, get, room }) =
                    nv::ring_state(put_raw, get_raw, entries, needed)
                else {
                    return Err(nv::FastSubmitError::Gone);
                };
                if room {
                    let mut slot = put;
                    // Same-ctx wait fences: GPU ACQUIRE before user pushes so
                    // the channel stalls in hardware instead of the CPU spinning.
                    for &(sem_gpu_va, payload) in acquires {
                        let stream_va = f.fence_pb_va + slot as usize * f.slot_bytes as usize;
                        let stream_gpu_va = f.buf_gpu_va
                            + f.fence_pb_off as u64
                            + slot as u64 * f.slot_bytes as u64;
                        let words = nv::sem_acquire_stream(sem_gpu_va, payload);
                        for (i, w) in words.iter().enumerate() {
                            unsafe {
                                core::ptr::write_volatile((stream_va as *mut u32).add(i), *w)
                            };
                        }
                        let gp = (f.gpfifo_va + slot as usize * 8) as *mut u32;
                        unsafe {
                            core::ptr::write_volatile(gp, nv::gp_entry0(stream_gpu_va));
                            core::ptr::write_volatile(
                                gp.add(1),
                                nv::gp_entry1(stream_gpu_va, (words.len() * 4) as u32),
                            );
                        }
                        slot = (slot + 1) % entries;
                    }
                    for p in pushes {
                        let gp = (f.gpfifo_va + slot as usize * 8) as *mut u32;
                        unsafe {
                            core::ptr::write_volatile(gp, nv::gp_entry0(p.va));
                            core::ptr::write_volatile(gp.add(1), nv::gp_entry1(p.va, p.va_len));
                        }
                        slot = (slot + 1) % entries;
                    }
                    let mut fence = None;
                    if with_fence {
                        let payload = f.next_payload;
                        f.next_payload = f.next_payload.wrapping_add(1);
                        if f.next_payload == 0 {
                            f.next_payload = 1;
                        }
                        let stream_va = f.fence_pb_va + slot as usize * f.slot_bytes as usize;
                        let stream_gpu_va = f.buf_gpu_va
                            + f.fence_pb_off as u64
                            + slot as u64 * f.slot_bytes as u64;
                        let sem_gpu_va = f.buf_gpu_va + f.fence_sem_off as u64;
                        let words = nv::sem_release_stream(sem_gpu_va, payload);
                        for (i, w) in words.iter().enumerate() {
                            unsafe {
                                core::ptr::write_volatile((stream_va as *mut u32).add(i), *w)
                            };
                        }
                        let gp = (f.gpfifo_va + slot as usize * 8) as *mut u32;
                        unsafe {
                            core::ptr::write_volatile(gp, nv::gp_entry0(stream_gpu_va));
                            core::ptr::write_volatile(
                                gp.add(1),
                                nv::gp_entry1(stream_gpu_va, (words.len() * 4) as u32),
                            );
                        }
                        slot = (slot + 1) % entries;
                        fence = Some((f.fence_sem_va, sem_gpu_va, payload));
                        f.fenced += 1;
                    }
                    store_fence();
                    unsafe { core::ptr::write_volatile(f.userd_gpput as *mut u32, slot) };
                    store_fence();
                    unsafe { core::ptr::write_volatile(f.doorbell_va as *mut u32, f.work_token) };
                    f.submits += 1;
                    if let Some((_, _, payload)) = fence {
                        f.last_fence = Some((f.submits, payload));
                    }
                    if waited {
                        nv::EXEC_RING_WAIT_US.fetch_add(
                            unsafe { crate::bus::drivers_timer_now_as_micros() }
                                .wrapping_sub(start),
                            Ordering::Relaxed,
                        );
                    }
                    return Ok(fence);
                }
                (put, get)
            };
            waited = true;
            if unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(start)
                >= RING_TIMEOUT_US
            {
                return Err(nv::FastSubmitError::RingFull { put, get, needed });
            }
            gpu_spin();
        }
    }

    /// `GEM_CPU_PREP`: wait until every submission `owner_pid` queued on its
    /// direct-submit channel has finished executing.
    ///
    /// A fence-only GP entry is appended behind the queued work and waited
    /// for: with `RELEASE_WFI_EN` the host writes its payload only once the
    /// channel's engines are idle, so it proves completion of everything in
    /// front of it (including submissions that carried no fence of their
    /// own). The RM per-submit path is synchronous and needs no wait. `nowait`
    /// answers EBUSY instead of blocking; a fence that never lands within the
    /// usual 10 s also answers EBUSY (Linux: a timed-out reservation wait is
    /// EBUSY too).
    fn cpu_prep_wait(&self, owner_pid: u64, nowait: bool) -> Result<usize, i32> {
        use super::nouveau_uapi as nv;
        const CPU_PREP_TIMEOUT_US: u64 = 10_000_000;
        // Resolve the channel this pid submits on WITHOUT defaulting to ctx 0:
        // a client whose own context is not ready yet has queued nothing, and
        // only the compositor (no per-pid entry, owner of the RM channel)
        // legitimately submits on ctx 0.
        let ctx_idx = {
            let entry = self
                .nouveau_pid_ctx
                .lock()
                .iter()
                .find(|t| t.0 == owner_pid)
                .map(|t| (t.1, t.4));
            match entry {
                Some((ctx, true)) => ctx,
                Some((_, false)) => return Ok(0),
                None if self.nouveau_owns_rm_channel(owner_pid) => 0,
                None => return Ok(0),
            }
        };
        // A context latched wedged (a fence of its timed out: the ring is
        // jammed for good) has nothing that will ever land. nouveau answers
        // CPU_PREP on a killed channel with 0 -- its fences are signaled with
        // an error, and `dma_resv_wait_timeout` counts that as signaled --
        // and the client learns the truth from its next submit or health
        // probe (ENODEV). Spinning here instead cost the caller the full
        // timeout per call, on the CPU, and wrote a probe fence into a ring
        // the GPU stopped reading.
        if ctx_idx >= 1 && nv::ctx_is_wedged(ctx_idx) {
            return Ok(0);
        }
        let queued = match self.nouveau_fast.lock().get(ctx_idx as usize) {
            // Idle when the last thing on the ring is a fence that landed
            // (WFI: the engines were idle when it did): nothing to wait for
            // and no probe to append. Without this, every NOWAIT prep on an
            // idle channel appended a probe and lost the race against the
            // PBDMA -- EBUSY for a buffer nothing was touching, and a ring
            // entry per poll.
            Some(FastSlot::Ready(f)) => {
                f.submits > 0
                    && !f.last_fence.is_some_and(|(at, payload)| {
                        at == f.submits
                            && crate::scheme::syncobj::hw_fence_landed(f.fence_sem_va, payload)
                    })
            }
            _ => false,
        };
        if !queued {
            return Ok(0);
        }
        let (fence_va, _fence_gpu_va, payload) = match self.fast_submit(ctx_idx, &[], true, &[]) {
            Ok(Some(fence)) => fence,
            Ok(None) => return Ok(0),
            Err(_) => {
                // Ring full or context gone: fall back to the last fence
                // that was issued, which covers all but a trailing
                // unfenced tail.
                match self.nouveau_fast.lock().get(ctx_idx as usize) {
                    Some(FastSlot::Ready(f)) if f.fenced > 0 => (
                        f.fence_sem_va,
                        f.buf_gpu_va + f.fence_sem_off as u64,
                        f.next_payload.wrapping_sub(1),
                    ),
                    _ => return Ok(0),
                }
            }
        };
        let start = unsafe { crate::bus::drivers_timer_now_as_micros() };
        loop {
            if crate::scheme::syncobj::hw_fence_landed(fence_va, payload) {
                return Ok(0);
            }
            if nowait {
                return Err(nv::EBUSY);
            }
            if unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(start)
                >= CPU_PREP_TIMEOUT_US
            {
                crate::klog_warn!(
                    "[nouveau-uapi] CPU_PREP: ctx{} fence payload {} did not land in {}s -> EBUSY",
                    ctx_idx,
                    payload,
                    CPU_PREP_TIMEOUT_US / 1_000_000
                );
                return Err(nv::EBUSY);
            }
            gpu_spin();
        }
    }

    /// Token/runlist of a prepared context, for the hang probe.
    fn fast_token(&self, ctx_idx: u32) -> (u32, u32) {
        match &self.nouveau_fast.lock()[ctx_idx as usize] {
            FastSlot::Ready(f) => (f.work_token, f.runlist_id),
            _ => (0, 0),
        }
    }

    /// Split EXEC waits into same-ctx HW ACQUIREs `(sem_gpu_va, payload)` and
    /// CPU-wait lists. Same-ctx uses this channel's local fence semaphore VA.
    /// Cross-ctx tries [`Self::map_peer_fence_sem`] when the producer published
    /// a `fence_gpu_va`; on failure falls back to CPU wait.
    fn partition_exec_waits(
        &self,
        handles: &[u32],
        points: &[u64],
        ctx_idx: u32,
    ) -> (
        alloc::vec::Vec<(u64, u32)>,
        alloc::vec::Vec<u32>,
        alloc::vec::Vec<u64>,
    ) {
        let sem_gpu_va = match self.nouveau_fast.lock().get(ctx_idx as usize) {
            Some(FastSlot::Ready(f)) => Some(f.buf_gpu_va + f.fence_sem_off as u64),
            _ => None,
        };
        let mut acquires = alloc::vec::Vec::new();
        let mut cpu_h = alloc::vec::Vec::new();
        let mut cpu_p = alloc::vec::Vec::new();
        for (i, &h) in handles.iter().enumerate() {
            let point = points.get(i).copied().unwrap_or(1);
            match crate::scheme::syncobj::pending_hw_fence(h, point) {
                Some((_fence_va, _fence_gpu_va, payload, fence_ctx)) if fence_ctx == ctx_idx => {
                    if let Some(va) = sem_gpu_va {
                        acquires.push((va, payload));
                    } else {
                        cpu_h.push(h);
                        cpu_p.push(point);
                    }
                }
                Some((_fence_va, fence_gpu_va, payload, fence_ctx)) if fence_gpu_va != 0 => {
                    if let Some(local_va) =
                        self.map_peer_fence_sem(ctx_idx, fence_ctx, fence_gpu_va)
                    {
                        acquires.push((local_va, payload));
                    } else {
                        cpu_h.push(h);
                        cpu_p.push(point);
                    }
                }
                Some(_) | None => {
                    cpu_h.push(h);
                    cpu_p.push(point);
                }
            }
        }
        (acquires, cpu_h, cpu_p)
    }

    /// Map a producer's fence semaphore GPU VA into the consumer channel's
    /// VAS. Cached in `nouveau_peer_fence` for the life of both contexts.
    /// Best-effort via RM; returns `None` (CPU wait) when peer mapping is
    /// unsupported.
    fn map_peer_fence_sem(
        &self,
        consumer_ctx: u32,
        producer_ctx: u32,
        fence_gpu_va_producer: u64,
    ) -> Option<u64> {
        if let Some(&va) = self
            .nouveau_peer_fence
            .lock()
            .get(&(consumer_ctx, producer_ctx))
        {
            return Some(va);
        }
        let device = (*self.rm_device_instance.lock())?;
        let local = nvidia_rm_sys::rm_init::map_peer_fence_sem(
            device,
            consumer_ctx,
            producer_ctx,
            fence_gpu_va_producer,
        )?;
        self.nouveau_peer_fence
            .lock()
            .insert((consumer_ctx, producer_ctx), local);
        Some(local)
    }

    /// Context `ctx_idx` is being freed: every peer-fence mapping it takes
    /// part in, as consumer (its VAS goes) or as producer (its buffer goes),
    /// is freed by the RM with it. Forget them here too, BEFORE `ctx_free`,
    /// so a wait on the index's next tenant asks the RM for a mapping of the
    /// new buffer instead of emitting an ACQUIRE at a VA that is no longer
    /// mapped (an MMU fault on the consumer's channel).
    fn forget_peer_fences(&self, ctx_idx: u32) {
        self.nouveau_peer_fence
            .lock()
            .retain(|&(consumer, producer), _| consumer != ctx_idx && producer != ctx_idx);
    }

    /// The EXEC ioctl body on the direct-submit path (waits already honoured
    /// by the caller — CPU waits done, same-ctx HW acquires passed in).
    /// Attaches every `sig` syncobj to the kernel fence of the LAST push --
    /// GPFIFO order means one fence covers the batch.
    fn exec_fast(
        &self,
        ctx_idx: u32,
        owner_pid: u64,
        req: &super::nouveau_uapi::DrmNouveauExec,
        pushes: &[super::nouveau_uapi::DrmNouveauExecPush],
        acquires: &[(u64, u32)],
    ) -> Result<usize, i32> {
        use super::nouveau_uapi as nv;
        let with_fence = req.sig_count > 0 && req.sig_ptr != 0;
        match self.fast_submit(ctx_idx, pushes, with_fence, acquires) {
            Ok(fence) => {
                nv::EXEC_FAST_SUBMITS.fetch_add(1, Ordering::Relaxed);
                if let Some((fence_va, fence_gpu_va, payload)) = fence {
                    nv::EXEC_FAST_FENCED.fetch_add(1, Ordering::Relaxed);
                    // `sig_ptr`/`sig_count` were range-checked by the EXEC arm
                    // before it dispatched here (see `user_slice_ok`).
                    let sigs = unsafe {
                        core::slice::from_raw_parts(
                            req.sig_ptr as *const nv::DrmNouveauSync,
                            req.sig_count as usize,
                        )
                    };
                    for sig in sigs {
                        let timeline = sig.flags & nv::SYNC_TYPE_MASK == nv::SYNC_TIMELINE_SYNCOBJ;
                        let target = if timeline { sig.timeline_value } else { 1 };
                        if !crate::scheme::syncobj::attach_hw_fence(
                            sig.handle,
                            target,
                            fence_va,
                            fence_gpu_va,
                            payload,
                            ctx_idx,
                            !timeline,
                        ) {
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC(direct): submitted, but sig syncobj handle={} is unknown (ENOENT)",
                                sig.handle
                            );
                            return Err(nv::ENOENT);
                        }
                    }
                }
                static FIRST_FAST_OK: AtomicBool = AtomicBool::new(false);
                if !FIRST_FAST_OK.swap(true, Ordering::Relaxed) {
                    crate::klog_info!(
                        "[nouveau-uapi] EXEC OK (first, direct submit): {} push(es) on ctx{} -- fence resolved lazily by syncobj",
                        req.push_count, ctx_idx
                    );
                }
                static CLIENT_FAST_OK: AtomicU32 = AtomicU32::new(0);
                if (1..32).contains(&ctx_idx) {
                    let bit = 1u32 << ctx_idx;
                    if CLIENT_FAST_OK.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                        crate::klog_info!(
                            "[nouveau-uapi] first CLIENT EXEC (direct submit): ctx={} pid={}",
                            ctx_idx,
                            owner_pid
                        );
                    }
                    nv::record_client_exec(alloc::format!(
                        "ctx={} pid={} SUBMITTED (direct): {} push(es), {} syncobj(s) attached to fence payload {:?}",
                        ctx_idx, owner_pid, req.push_count, req.sig_count, fence.map(|f| f.2)
                    ));
                }
                Ok(0)
            }
            Err(nv::FastSubmitError::Gone) => {
                crate::klog_warn!(
                    "[nouveau-uapi] EXEC(direct): ctx{} lost its direct-submit state mid-call (context freed?) pid={}",
                    ctx_idx, owner_pid
                );
                Err(nv::ENODEV)
            }
            Err(nv::FastSubmitError::RingFull { put, get, needed }) => {
                // GPGet froze for a full second: the channel is wedged, the
                // same condition the RM path reported as BUSY_RETRY.
                if ctx_idx >= 1 {
                    nv::ctx_set_wedged(ctx_idx);
                }
                crate::klog_warn!(
                    "[nouveau-uapi] EXEC(direct): ctx{} ring did not free {} slot(s) in 1s (GPPut={} GPGet={}) -- channel wedged, EIO (NVK: device-lost) pid={} {}",
                    ctx_idx, needed, put, get, owner_pid, self.ctx_registry_summary()
                );
                let (token, runlist) = self.fast_token(ctx_idx);
                self.gr_hang_probe(ctx_idx, owner_pid, token, runlist, 1000);
                if ctx_idx >= 1 {
                    nv::record_client_exec(alloc::format!(
                        "ctx={} pid={} FAILED (direct): ring full for 1s GPPut={} GPGet={} needed={} | {}",
                        ctx_idx, owner_pid, put, get, needed, self.ctx_registry_summary()
                    ));
                }
                Err(nv::EIO)
            }
        }
    }

    /// `syncobj` fence-timeout upcall for this GPU: a fence submitted on
    /// `ctx_idx` never landed. Only acts if the landing zone is ours.
    fn fast_fence_timeout(
        &self,
        ctx_idx: u32,
        fence_va: usize,
        payload: u32,
        handle: u32,
        point: u64,
    ) {
        use super::nouveau_uapi as nv;
        let (token, runlist) = {
            let slots = self.nouveau_fast.lock();
            match slots.get(ctx_idx as usize) {
                Some(FastSlot::Ready(f)) if f.fence_sem_va == fence_va => {
                    (f.work_token, f.runlist_id)
                }
                _ => return,
            }
        };
        let current = unsafe { core::ptr::read_volatile(fence_va as *const u32) };
        if ctx_idx >= 1 {
            nv::ctx_set_wedged(ctx_idx);
        }
        crate::klog_warn!(
            "[nouveau-uapi] fence TIMEOUT (direct submit): ctx{} payload={} never landed in {}s (landing zone={}); syncobj handle={} point={} released so its waiter can fail; {}",
            ctx_idx, payload, crate::scheme::syncobj::FENCE_TIMEOUT_US / 1_000_000, current, handle, point,
            if ctx_idx >= 1 { "context latched WEDGED (next submit EIO -> device-lost)" } else { "ctx 0 (compositor) is never latched" }
        );
        self.gr_hang_probe(ctx_idx, 0, token, runlist, 1000);
        if ctx_idx >= 1 {
            nv::record_client_exec(alloc::format!(
                "ctx={} FAILED (direct): fence payload {} never landed (landing zone={}) | {}",
                ctx_idx,
                payload,
                current,
                self.ctx_registry_summary()
            ));
        }
    }

    /// Tear down the direct-submit state of `ctx_idx` BEFORE `ctx_free`:
    /// unmap the USERD window and abandon fences that can never land now
    /// (their waiters are released, as a killed channel's fences would be).
    fn fast_release(&self, device_instance: u32, ctx_idx: u32) {
        let old = {
            let mut slots = self.nouveau_fast.lock();
            match slots.get_mut(ctx_idx as usize) {
                Some(slot) => core::mem::replace(slot, FastSlot::Unprepared),
                None => return,
            }
        };
        match old {
            FastSlot::Ready(f) => {
                let abandoned = crate::scheme::syncobj::abandon_fences(f.fence_sem_va);
                lock::pump();
                let status = nvidia_rm_sys::rm_init::exec_fast_release(device_instance, ctx_idx);
                log::info!(
                    "[nouveau-uapi] ctx{}: direct-submit state released (status={:#x}, {} submits, {} fenced, {} fence(s) abandoned)",
                    ctx_idx, status, f.submits, f.fenced, abandoned
                );
            }
            FastSlot::Failed => {
                // A setup that failed may still have mapped the USERD window:
                // the RM maps it before the stages that can fail, and the
                // encoder self-check runs after every one of them. `ctx_free`
                // does not unmap it, and the next `exec_fast_prepare` on this
                // index (the next process to get the channel) finds a window
                // over a memdesc that is gone and refuses ("stale USERD map"),
                // so the direct path would be lost for this index for good.
                // The release is a no-op when nothing was mapped.
                lock::pump();
                let _ = nvidia_rm_sys::rm_init::exec_fast_release(device_instance, ctx_idx);
            }
            FastSlot::Unprepared | FastSlot::Preparing => {}
        }
    }

    /// Rebuild path for the compositor singleton (ctx 0): release its direct
    /// submit state and tear down step17's cached channel, so the next
    /// CHANNEL_ALLOC/step17 recreates a fresh ctx0 channel instead of reusing a
    /// wedged one across labwc respawns.
    fn reset_ctx0_singleton(&self, device_instance: u32, reason: &str, owner_pid: u64) {
        self.ctx0_release(owner_pid);
        self.fast_release(device_instance, 0);
        // The RM's `ctx0_reset` frees every peer-fence mapping context 0
        // takes part in, as consumer and as producer, along with the fence
        // page they map. So must this cache, or the respawned compositor
        // waits on its clients -- and is waited on by them -- through VAs
        // of the channel that is gone: an MMU fault on the first frame.
        self.forget_peer_fences(0);
        lock::pump();
        let status = nvidia_rm_sys::rm_init::ctx0_reset(device_instance);
        if status == 0 {
            crate::klog_warn!(
                "[nouveau-uapi] ctx0 reset: {reason} pid={} -> step17 singleton cleared; next compositor CHANNEL_ALLOC rebuilds a fresh channel",
                owner_pid
            );
        } else {
            crate::klog_warn!(
                "[nouveau-uapi] ctx0 reset: {reason} pid={} failed, NV_STATUS={:#x} (respawn may reuse a dead channel; #1192 pixman fallback remains the safety net)",
                owner_pid,
                status
            );
        }
    }

    /// Sticky compositor (ctx 0) ownership — see `ctx0_owner` field docs.
    fn ctx0_is_owner(&self, pid: u64) -> bool {
        let cur = self.ctx0_owner.load(Ordering::Acquire);
        cur != 0 && cur == pid
    }

    /// Claim ctx 0 for `pid`, or confirm it already owns it. Returns `false`
    /// when another process holds the sticky role.
    fn ctx0_try_claim(&self, pid: u64) -> bool {
        match self
            .ctx0_owner
            .compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(cur) => cur == pid,
        }
    }

    /// Drop sticky ctx-0 ownership when `pid` is the current owner.
    fn ctx0_release(&self, pid: u64) {
        let _ = self
            .ctx0_owner
            .compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Relaxed);
    }

    /// `CHANNEL_ALLOC` for a GL/Vulkan client (not the sticky ctx-0 owner).
    fn channel_alloc_client(&self, owner_pid: u64, arg: usize) -> Result<usize, i32> {
        use super::nouveau_uapi as nv;
        let (ctx_idx, h_vas_out, notif_out) = self.ensure_ctx_for_pid(owner_pid, false);
        let ok = ctx_idx != 0;
        let mut chan = self.nouveau_channels.lock();
        if chan.len() >= nv::MAX_CHANNELS {
            log::warn!(
                "[nouveau-uapi] CHANNEL_ALLOC: {} channels already live",
                chan.len()
            );
            return Err(nv::EBUSY);
        }
        let new_id = (0i32..)
            .find(|i| !chan.iter().any(|c| c.id == *i))
            .unwrap_or(0);
        chan.push(nv::NouveauChannelState {
            id: new_id,
            h_vas: h_vas_out,
            notifier_handle: notif_out,
            rm_backed: ok,
            ctx_idx,
            owner_pid,
        });
        drop(chan);
        let req = unsafe { &mut *(arg as *mut nv::DrmNouveauChannelAlloc) };
        req.channel = new_id;
        req.notifier_handle = notif_out;
        req.pushbuf_domains = nv::NOUVEAU_GEM_DOMAIN_VRAM;
        req.nr_subchan = 0;
        crate::klog_warn!(
            "[nouveau-uapi] CHANNEL_ALLOC owner_pid={} -> channel={} {}",
            owner_pid,
            new_id,
            if ok {
                alloc::format!("CTX {} (hVas={:#x})", ctx_idx, h_vas_out)
            } else {
                alloc::string::String::from(
                    "DISCOVERY ONLY (no own context; submission ENODEV -> software)",
                )
            }
        );
        Ok(0)
    }

    /// Software-only discovery channel when the RM is not attached yet.
    fn channel_alloc_discovery(&self, owner_pid: u64, arg: usize) -> Result<usize, i32> {
        use super::nouveau_uapi as nv;
        let mut chan = self.nouveau_channels.lock();
        if chan.len() >= nv::MAX_CHANNELS {
            log::warn!(
                "[nouveau-uapi] CHANNEL_ALLOC: {} channels already live",
                chan.len()
            );
            return Err(nv::EBUSY);
        }
        let new_id = (0i32..)
            .find(|i| !chan.iter().any(|c| c.id == *i))
            .unwrap_or(0);
        chan.push(nv::NouveauChannelState {
            id: new_id,
            h_vas: 0,
            notifier_handle: 0,
            rm_backed: false,
            ctx_idx: 0,
            owner_pid,
        });
        drop(chan);
        let req = unsafe { &mut *(arg as *mut nv::DrmNouveauChannelAlloc) };
        req.channel = new_id;
        req.notifier_handle = 0;
        req.pushbuf_domains = nv::NOUVEAU_GEM_DOMAIN_VRAM;
        req.nr_subchan = 0;
        crate::klog_warn!(
            "[nouveau-uapi] CHANNEL_ALLOC owner_pid={} -> channel={} DISCOVERY ONLY \
             (GPU not attached to the RM; class enumeration works, but GEM/VM_BIND/\
             EXEC will return ENODEV until `cat /proc/gpustep14` runs)",
            owner_pid,
            new_id
        );
        Ok(0)
    }

    fn nouveau_owns_rm_channel(&self, owner_pid: u64) -> bool {
        self.nouveau_channels
            .lock()
            .iter()
            .any(|c| c.rm_backed && (c.owner_pid == owner_pid || owner_pid == 0))
    }

    /// Which GPU context (independent VAS + GPFIFO channel) the calling process
    /// owns, for routing `VM_BIND`/`EXEC`. The compositor is context 0 (the
    /// singleton step16/step17 ladder); each GL client gets its own >= 1 from
    /// `ctx_alloc` (see `CHANNEL_ALLOC`). A process with no RM-backed channel
    /// (unknown pid, discovery-only) maps to 0, which targets the compositor's
    /// singleton -- the pre-multi-context behavior.
    /// The GPU context a process's `VM_BIND`/`EXEC` route to. Reads the per-pid
    /// registry (the authority), so it is correct even before the process's
    /// `CHANNEL_ALLOC` -- NVK issues `VM_BIND` first, and tying this to the
    /// channel list returned 0 (the compositor's VAS) in that window, which put
    /// the client's buffers in the wrong VA space and MMU-faulted its channel.
    /// Not in the registry -> context 0 (the compositor, or a client that fell
    /// back to software).
    /// One-line census of the per-client GPU context registry: how many of the
    /// `MAX_CTX - 1` client slots are taken and by which pids.
    ///
    /// This is the state a long session degrades into. A slot is released when
    /// its owner exits (`nouveau_release_process`), so slots outliving their
    /// processes -- a teardown that did not run, an `RM` context that failed to
    /// free -- eventually leave nothing for a NEW client, and
    /// `ensure_ctx_for_pid` then hands it context 0. Every GL client failing at
    /// its first submit while the desktop keeps running looks identical from
    /// userspace whatever the cause, so print the census wherever a client's
    /// EXEC gives up: exhaustion names itself instead of being inferred.
    fn ctx_registry_summary(&self) -> alloc::string::String {
        use super::nouveau_uapi as nv;
        let map = self.nouveau_pid_ctx.lock();
        let mut s = alloc::format!("{}/{} client slots used:", map.len(), nv::MAX_CTX - 1);
        for t in map.iter() {
            let _ = core::fmt::Write::write_fmt(
                &mut s,
                format_args!(
                    " pid={}->ctx{}{}",
                    t.0,
                    t.1,
                    if nv::ctx_is_wedged(t.1) {
                        "(WEDGED)"
                    } else {
                        ""
                    }
                ),
            );
        }
        s
    }

    fn ctx_idx_for_pid(&self, owner_pid: u64) -> u32 {
        // Only READY entries route: a mid-build ctx must not receive class
        // objects or submissions (callers that can legitimately race a build
        // go through `ensure_ctx_for_pid`, which waits for readiness).
        self.nouveau_pid_ctx
            .lock()
            .iter()
            .find(|t| t.0 == owner_pid && t.4)
            .map(|t| t.1)
            .unwrap_or(0)
    }

    /// Return this process's own GPU context, building it on first touch, as
    /// `(ctx_idx, h_vas, h_notifier)`. Called from both `VM_BIND` and a GL
    /// client's `CHANNEL_ALLOC` -- whichever the process reaches first assigns
    /// its context; the other reuses it (this is idempotent per pid). Distinct
    /// pids get distinct contexts, so concurrent clients never share a VA space.
    ///
    /// `is_compositor` is passed by the caller (rather than derived here) so this
    /// can run while `CHANNEL_ALLOC` holds the `nouveau_channels` lock without a
    /// re-entrant deadlock: the compositor owns context 0 and never gets a
    /// client context. On any failure (no free slot, `ctx_alloc` error) it
    /// returns context 0 -- the client then falls back to software (llvmpipe),
    /// never worse than before.
    fn ensure_ctx_for_pid(&self, owner_pid: u64, is_compositor: bool) -> (u32, u32, u32) {
        use super::nouveau_uapi as nv;
        if is_compositor {
            return (0, 0, 0);
        }
        let dev = match *self.rm_device_instance.lock() {
            Some(d) => d,
            None => return (0, 0, 0),
        };
        // Hold the registry across ctx_alloc: RmGate (taken inside ctx_alloc) is
        // a leaf lock, and the CHANNEL_ALLOC path already nests chan -> RmGate,
        // so chan -> pid_ctx -> RmGate here introduces no lock cycle. But the
        // golden-context prime below MUST run with the registry lock RELEASED:
        // it drives the compute engine's cold golden-context load (up to ~500 ms)
        // and ctx_idx_for_pid -- the EXEC hot path, including the compositor's
        // own submissions -- shares this lock. So: build + register under the
        // lock, drop it, then prime.
        let mut map = self.nouveau_pid_ctx.lock();
        if let Some(t) = map.iter().find(|t| t.0 == owner_pid) {
            if t.4 {
                return (t.1, t.2, t.3);
            }
            // Another thread of this pid is mid-build (entry reserved, not
            // READY). Wait for it instead of handing out the unprimed ctx --
            // racing an EXEC onto it cold-loads the GR context and hangs FECS.
            // Bounded (~3s covers ctx_alloc + the 500ms prime with margin) and
            // pumping (gpu_spin drains our TLB-shootdown queue) so the wait
            // can never wedge the machine. On timeout or build failure fall
            // back to (0,0,0) -- software, same as any other build failure.
            drop(map);
            let start = unsafe { crate::bus::drivers_timer_now_as_micros() };
            loop {
                {
                    let m = self.nouveau_pid_ctx.lock();
                    match m.iter().find(|t| t.0 == owner_pid) {
                        Some(t) if t.4 => return (t.1, t.2, t.3),
                        Some(_) => {}
                        // Build failed (reservation removed) or the pid exited.
                        None => return (0, 0, 0),
                    }
                }
                if unsafe { crate::bus::drivers_timer_now_as_micros() }.wrapping_sub(start)
                    > 3_000_000
                {
                    crate::klog_warn!(
                        "[nouveau-uapi] ctx: pid={} waited >3s for its own in-flight ctx build -- software fallback",
                        owner_pid
                    );
                    return (0, 0, 0);
                }
                gpu_spin();
            }
        }
        let Some(idx) = (1u32..nv::MAX_CTX).find(|i| !map.iter().any(|t| t.1 == *i)) else {
            crate::klog_warn!(
                "[nouveau-uapi] ctx: no free context slot (max {}) for pid={} -- client falls back to software",
                nv::MAX_CTX,
                owner_pid
            );
            return (0, 0, 0);
        };
        // RESERVE the slot + pid up front (not READY): concurrent first-touch
        // threads of this pid then wait above instead of double-building, and
        // no other pid can pick this idx. Published as usable only AFTER the
        // golden-context prime below -- publishing before the prime let a
        // sibling thread race a graphics EXEC onto the unprimed channel (FECS
        // RESTORE hang, the real-RTX signature). Release the lock across the
        // slow RM work; on any failure the reservation is removed.
        map.push((owner_pid, idx, 0, 0, false));
        drop(map);
        let unreserve = || {
            let mut m = self.nouveau_pid_ctx.lock();
            if let Some(i) = m.iter().position(|t| t.0 == owner_pid && !t.4) {
                m.remove(i);
            }
        };
        let (ctx_idx, h_vas, h_notifier) = match nvidia_rm_sys::rm_init::ctx_alloc(dev, idx) {
            Ok(c) if c.sched_status == 0 => {
                // Fresh channel: clear any stale wedged latch from a prior
                // tenant of this slot, so this client starts un-wedged.
                nv::ctx_clear_wedged(idx);
                crate::klog_warn!(
                    "[nouveau-uapi] ctx: pid={} -> CTX {} (own context: hVas={:#x} hChannel={:#x} bufGpuVA={:#x})",
                    owner_pid,
                    idx,
                    c.h_vas,
                    c.h_channel,
                    c.buf_gpu_va
                );
                (idx, c.h_vas, c.h_notifier)
            }
            Ok(c) => {
                crate::klog_warn!(
                    "[nouveau-uapi] ctx: pid={} CTX {} INCOMPLETE (chan={:#x} compute={:#x} sched={:#x}) -- software fallback",
                    owner_pid,
                    idx,
                    c.chan_status,
                    c.compute_status,
                    c.sched_status
                );
                unreserve();
                return (0, 0, 0);
            }
            Err(s) => {
                crate::klog_warn!(
                    "[nouveau-uapi] ctx: pid={} CTX {} ctx_alloc failed NV_STATUS={:#x} -- software fallback",
                    owner_pid,
                    idx,
                    s
                );
                unreserve();
                return (0, 0, 0);
            }
        };

        // Persist "prime STARTED" BEFORE the call. A real-RTX repro showed a
        // fence timeout on a ctx that had NO prime record at all -- which this
        // code cannot produce unless ctx_prime never returned (blocked inside
        // the RM while another thread of the same client raced past the
        // registry map-hit above straight to EXEC on the unprimed channel), or
        // the record was lost some other way. With this start-marker, a stuck
        // prime is directly visible in /proc/gpudbg as a line that still says
        // STARTED; the outcome record below replaces it when the call returns.
        // `dev` names the RM device instance so a cross-GPU slot collision
        // (the C-side g_ctxAlloc cache is global, the Rust pid registry is
        // per-GPU -- this box has TWO RTX cards) is visible too.
        nv::record_prime(
            ctx_idx,
            alloc::format!(
                "ctx={} pid={} PRIME STARTED on rm-device {} (if this line persists, ctx_prime never returned)",
                ctx_idx,
                owner_pid,
                dev
            ),
        );
        // Prime compute + GRAPHICS golden contexts up front. Soft-fail used to
        // publish READY anyway; the client's first SET_OBJECT(TURING_A) then
        // cold-loaded GR and hung FECS mid-RESTORE. A failed prime now tears
        // the reservation down so the client falls back to software instead of
        // a wedged first draw.
        let prime = nvidia_rm_sys::rm_init::ctx_prime(dev, ctx_idx);
        if prime == 0 {
            crate::klog_warn!(
                "[nouveau-uapi] ctx: pid={} CTX {} golden context primed OK (compute+GRAPHICS)",
                owner_pid,
                ctx_idx
            );
        } else {
            crate::klog_warn!(
                "[nouveau-uapi] ctx: pid={} CTX {} prime NV_STATUS={:#x} -- rejecting context (software fallback; avoids FECS RESTORE hang on first 3D draw)",
                owner_pid,
                ctx_idx,
                prime
            );
            nv::record_prime(
                ctx_idx,
                alloc::format!(
                    "ctx={} pid={} PRIME FAIL on rm-device {} (NV_STATUS={:#x}) -- context discarded <== predicts FECS hang if kept",
                    ctx_idx, owner_pid, dev, prime
                ),
            );
            self.forget_peer_fences(ctx_idx);
            let _ = nvidia_rm_sys::rm_init::ctx_free(dev, ctx_idx);
            unreserve();
            return (0, 0, 0);
        }
        nv::record_prime(
            ctx_idx,
            alloc::format!(
                "ctx={} pid={} PRIME OK -- compute+GRAPHICS golden context loaded on rm-device {}",
                ctx_idx,
                owner_pid,
                dev
            ),
        );
        // Publish READY only now: both golden contexts are primed, so a
        // sibling thread waking from the wait above -- or any later EXEC --
        // can no longer land on an unprimed channel. If the entry is gone the
        // pid exited mid-build (its reservation was reaped); serve the
        // software fallback rather than resurrecting it.
        {
            let mut m = self.nouveau_pid_ctx.lock();
            match m.iter_mut().find(|t| t.0 == owner_pid) {
                Some(t) => {
                    t.2 = h_vas;
                    t.3 = h_notifier;
                    t.4 = true;
                }
                None => return (0, 0, 0),
            }
        }
        (ctx_idx, h_vas, h_notifier)
    }

    /// This GPU's architecture as the HARDWARE reports it.
    ///
    /// `self.architecture` comes from `identify_gpu`, a ~25-entry PCI
    /// device-id table whose default arm is `Unknown` -- so a Super refresh, a
    /// laptop part or a Ti variant that is not listed would make
    /// `nouveau_engine_classes` refuse and cost the client every Vulkan GPU,
    /// even though NV_PMC_BOOT_0 identifies the chip perfectly well. Prefer the
    /// register, fall back to the table only when it is unreadable.
    fn nouveau_arch(&self) -> NvidiaArchitecture {
        let boot0 = unsafe { core::ptr::read_volatile(self._bar0 as *const u32) };
        if boot0 != 0xffff_ffff && boot0 != 0 {
            let arch = arch_from_pmc_boot0(boot0);
            if arch != NvidiaArchitecture::Unknown {
                return arch;
            }
        }
        self.architecture
    }

    /// NV_PMC_BOOT_0's chip id -- what real nouveau reports as
    /// `nv_device_info_v0.chipset`, and the ONLY chipset source NVK 26.x uses
    /// (it never issues `GETPARAM_CHIPSET_ID`). Mesa maps it to an SM version
    /// through a `>=`-range table, so the value must be the real one: the
    /// per-architecture `*_MIN` constants land on the datacenter part for
    /// Ampere (0x170 = GA100 -> SM80), which mis-targets every consumer GA10x
    /// (those need >= 0x172 -> SM86) and would make NAK emit wrong code.
    fn nouveau_chipset_id(&self) -> u16 {
        // BAR0+0 is NV_PMC_BOOT_0 -- the same plain 32-bit read the probe and
        // the GSP recovery path already do.
        let boot0 = unsafe { core::ptr::read_volatile(self._bar0 as *const u32) };
        // 9-bit chip-id field, per nouveau: (boot0 & 0x1ff00000) >> 20.
        let chip = ((boot0 >> regs::PMC_BOOT0_CHIP_ID_SHIFT) & 0x1ff) as u16;
        if boot0 != 0xffff_ffff && chip != 0 {
            return chip;
        }
        // Device off the bus / register unreadable: fall back to a
        // REPRESENTATIVE CONSUMER id for the architecture rather than a value
        // that decodes to the wrong SM.
        match self.nouveau_arch() {
            NvidiaArchitecture::Turing => 0x162,      // TU102
            NvidiaArchitecture::Ampere => 0x172,      // GA102 (SM86, not GA100)
            NvidiaArchitecture::AdaLovelace => 0x192, // AD102
            NvidiaArchitecture::Hopper => 0x180,      // GH100
            NvidiaArchitecture::Blackwell => 0x1b2,   // GB202
            NvidiaArchitecture::Unknown => 0,
        }
    }

    /// NV_PMC_BOOT_0's revision nibble (`nv_device_info_v0.revision`).
    fn nouveau_chip_revision(&self) -> u8 {
        let boot0 = unsafe { core::ptr::read_volatile(self._bar0 as *const u32) };
        if boot0 == 0xffff_ffff {
            0
        } else {
            (boot0 & 0xff) as u8
        }
    }

    /// `NOUVEAU_GETPARAM_GRAPH_UNITS`, packed exactly like Linux's
    /// `gf100_gr_units()`: `gpc_nr | tpc_total << 8 | rop_nr << 32`.
    ///
    /// Mesa unpacks `gpc_count = v & 0xff` and `tpc_count = (v >> 8) & 0xffff`
    /// and sizes shader-local memory from them, and this getparam is
    /// **enumeration-fatal** (`goto out_err` on failure), so it can never
    /// return EINVAL as an earlier milestone did.
    /// `NOUVEAU_GETPARAM_VRAM_USED`: bytes of VRAM backing live GEM objects
    /// on this GPU, every client's included, like Linux's
    /// `ttm_resource_manager_usage(vram_mgr)`. GART objects live in system
    /// memory and do not count.
    fn nouveau_vram_used(&self) -> u64 {
        self.nouveau_gem
            .lock()
            .iter()
            .filter(|o| o.domain & super::nouveau_uapi::NOUVEAU_GEM_DOMAIN_VRAM != 0)
            .map(|o| o.size)
            .sum()
    }

    fn nouveau_graph_units(&self) -> u64 {
        // Real topology, straight from the live GSP-RM, whenever the GPU is
        // attached (this is the same GR_GET_GPC_MASK/TPC_MASK probe as
        // `/proc/gpustep15`).
        // Copy the instance out and DROP the guard before the FFI call: an
        // `if let` scrutinee keeps its temporary guard alive for the whole
        // block, which would hold this IRQ-disabling spinlock across a GSP-RM
        // control round-trip. Every other call site does it this way.
        let dev = *self.rm_device_instance.lock();
        if let Some(dev) = dev {
            if let Ok(p) = nvidia_rm_sys::rm_init::step15(dev) {
                if p.gpc_mask_status == 0 && p.tpc_mask_status == 0 && p.num_gpc > 0 {
                    return (p.num_gpc as u64 & 0xff) | ((p.total_tpc as u64 & 0xffff) << 8);
                }
            }
        }
        // No RM yet: report the FULL-DIE configuration for the architecture.
        // Erring high is the safe direction -- Mesa sizes the shader TLS from
        // these, so over-reporting merely over-allocates, while
        // under-reporting leaves real SMs without scratch and faults the GPU.
        let (gpc, tpc) = match self.nouveau_arch() {
            NvidiaArchitecture::Turing => (6u64, 36u64), // TU102
            NvidiaArchitecture::Ampere => (7, 42),       // GA102
            NvidiaArchitecture::AdaLovelace => (12, 72), // AD102
            NvidiaArchitecture::Hopper => (8, 72),       // GH100
            NvidiaArchitecture::Blackwell => (12, 96),   // GB202
            NvidiaArchitecture::Unknown => (8, 64),
        };
        log::warn!(
            "[nouveau-uapi] GRAPH_UNITS: RM not attached -- reporting the full-die \
             {:?} topology (gpc={} tpc={}) instead of the floorswept truth; attach \
             the RM (/proc/gpustep14) for the real GR probe",
            self.nouveau_arch(),
            gpc,
            tpc
        );
        (gpc & 0xff) | ((tpc & 0xffff) << 8)
    }

    /// The engine classes advertised through `NVIF SCLASS`.
    ///
    /// Mesa picks, per engine type, the HIGHEST class whose LOW BYTE matches:
    /// 0xb5 copy, 0x2d 2d, 0x97 3d, 0x40 (else 0x39) m2mf, 0xc0 compute. A
    /// type with no match yields oclass 0, which mesa turns into -EINVAL and
    /// the device is dropped -- so all five must be present.
    fn nouveau_engine_classes(&self) -> Option<[i32; 5]> {
        use super::nouveau_uapi as nv;
        let (eng3d, compute, copy) = match self.nouveau_arch() {
            NvidiaArchitecture::Turing => nv::CLASSES_TURING,
            NvidiaArchitecture::Ampere => nv::CLASSES_AMPERE,
            NvidiaArchitecture::AdaLovelace => nv::CLASSES_ADA,
            NvidiaArchitecture::Hopper => nv::CLASSES_HOPPER,
            NvidiaArchitecture::Blackwell => nv::CLASSES_BLACKWELL,
            // Refuse rather than guess: a wrong 3D class means Mesa encodes
            // methods this chip does not implement, which faults the GPU. An
            // unadvertised class makes NVK skip the device -- the honest
            // outcome for hardware this driver does not recognize.
            NvidiaArchitecture::Unknown => {
                log::warn!(
                    "[nouveau-uapi] NVIF SCLASS: unknown GPU architecture -- refusing to \
                     guess engine classes (NVK will skip this GPU)"
                );
                return None;
            }
        };
        let classes = [
            nv::CLASS_FERMI_TWOD_A,
            nv::CLASS_KEPLER_INLINE_TO_MEMORY_B,
            eng3d,
            compute,
            copy,
        ];
        // One-shot, VISIBLE snapshot of every enumeration-fatal value NVK reads
        // to build its physical device: architecture, chipset/revision (its ONLY
        // SM source in NVK 26.x), the five engine classes it demands non-zero,
        // and the GRAPH_UNITS gpc/tpc it sizes shader-local memory from. NVK
        // crashing INSIDE vkCreateDevice / timeline-semaphore setup with NO
        // failing ioctl on the console means it accepted all of these and then
        // dereferenced a NULL built from one of them -- so a wrong value here (a
        // zero class, an arch mismatch, gpc/tpc reading low) is the prime
        // suspect, and this makes the whole set readable from one console photo.
        {
            static LOGGED: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);
            if !LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                let gu = self.nouveau_graph_units();
                crate::klog_info!(
                    "[nouveau-uapi] NVK enum: arch={:?} chipset={:#x} rev={:#x} \
                     classes=[2d={:#x} m2mf={:#x} 3d={:#x} comp={:#x} copy={:#x}] \
                     graph_units={:#x} (gpc={} tpc={})",
                    self.nouveau_arch(),
                    self.nouveau_chipset_id(),
                    self.nouveau_chip_revision(),
                    classes[0],
                    classes[1],
                    classes[2],
                    classes[3],
                    classes[4],
                    gu,
                    gu & 0xff,
                    (gu >> 8) & 0xffff
                );
            }
        }
        Some(classes)
    }

    /// `DRM_NOUVEAU_NVIF` (nr 0x47) -- nouveau's generic object-model ioctl.
    ///
    /// NVK's winsys needs this during *physical-device enumeration*, long
    /// before any rendering: `nouveau_ws_device_new()` allocates an NV_DEVICE
    /// object, reads its INFO (the sole source of chipset/VRAM/type), then per
    /// channel enumerates engine classes (SCLASS) and allocates five
    /// subchannel objects. Every one of those is fatal on failure, so an
    /// unimplemented NVIF meant zero Vulkan GPUs.
    ///
    /// Objects here are pure bookkeeping: mesa passes its OWN pointers as
    /// `token`/`object` cookies and never asks the kernel to mint handles, so
    /// accepting NEW/DEL without allocating hardware state is faithful for
    /// this path (real per-object state is created by CHANNEL_ALLOC/EXEC).
    fn nouveau_nvif(&self, arg: usize, size: usize, owner_pid: u64) -> Result<usize, i32> {
        use super::nouveau_uapi as nv;
        const HDR: usize = core::mem::size_of::<nv::NvifIoctlV0>();
        if size < HDR {
            log::warn!("[nouveau-uapi] NVIF: payload {} < 24-byte header", size);
            return Err(nv::EINVAL);
        }
        // Read, never reference: `arg` is a raw userspace pointer with no
        // alignment guarantee, and forming a reference to a misaligned address
        // is UB even if the read would have worked.
        let hdr = unsafe { core::ptr::read_unaligned(arg as *const nv::NvifIoctlV0) };
        let body = arg + HDR;
        let body_len = size - HDR;

        match hdr.type_ {
            nv::NVIF_IOCTL_V0_NEW => {
                if body_len < core::mem::size_of::<nv::NvifIoctlNewV0>() {
                    return Err(nv::EINVAL);
                }
                let new = unsafe { core::ptr::read_unaligned(body as *const nv::NvifIoctlNewV0) };
                if new.oclass == nv::NVIF_CLASS_NV_DEVICE {
                    // The NEW body is followed by class data -- `nv_device_v0`
                    // for NV_DEVICE, whose `device` selects which GPU the
                    // client wants (mesa passes ~0 = "client default", i.e.
                    // the device behind this fd, which is the only one this
                    // node exposes).
                    let sel = if body_len - core::mem::size_of::<nv::NvifIoctlNewV0>()
                        >= core::mem::size_of::<nv::NvDeviceV0>()
                    {
                        let d = unsafe {
                            core::ptr::read_unaligned(
                                (body + core::mem::size_of::<nv::NvifIoctlNewV0>())
                                    as *const nv::NvDeviceV0,
                            )
                        };
                        d.device
                    } else {
                        u64::MAX
                    };
                    if sel != u64::MAX {
                        log::warn!(
                            "[nouveau-uapi] NVIF NEW NV_DEVICE: selector {:#x} is not the client \
                             default (~0); this node exposes exactly one GPU",
                            sel
                        );
                        return Err(nv::EINVAL);
                    }
                    log::warn!(
                        "[nouveau-uapi] NVIF NEW NV_DEVICE (token={:#x}) -- device object accepted",
                        new.token
                    );
                } else if new.oclass == 0 {
                    // Mesa only reaches this if SCLASS gave it nothing usable.
                    log::warn!("[nouveau-uapi] NVIF NEW with oclass=0 -- rejecting");
                    return Err(nv::EINVAL);
                } else {
                    // A subchannel NEW names an ENGINE CLASS (3D/compute/
                    // copy/2D/inline) on the channel in `hdr.token`. On the
                    // RM-backed channel this MUST create a real RM object:
                    // allocating the class is what makes the RM/GSP build
                    // that engine's channel context -- for GR, the golden
                    // context image, patch buffer and global buffers, mapped
                    // into the channel's VAS. Serving it as bookkeeping-only
                    // (as this arm used to) leaves the 3D context unbuilt, so
                    // NVK's very first 3D method (its init push opens with
                    // SET_OBJECT(0, 0xC597), per the first-push dump) made
                    // the GR engine load a context that did not exist: MMU
                    // fault attributed to engine GRAPHICS (notifier
                    // info32=0x1f info16=0x1), robust-channel recovery, dead
                    // channel, RING FULL -- every boot.
                    //
                    // Discovery channels (no RM) keep the bookkeeping-only
                    // answer: enumeration must work without hardware.
                    //
                    // The channel must exist and be the caller's, like
                    // SCLASS: Linux resolves the token to one of the calling
                    // file's own channels (`nouveau_abi16_chan`) and fails
                    // EINVAL otherwise. Accepting a NEW on a channel nobody
                    // allocated, or on another process's, used to answer
                    // "bookkeeping only" -- which on an RM-backed channel is
                    // exactly the unbuilt engine context described above.
                    let rm_backed = {
                        let chans = self.nouveau_channels.lock();
                        let Some(c) = chans.iter().find(|c| {
                            c.id >= 0
                                && c.id as u64 == hdr.token
                                && (owner_pid == 0 || c.owner_pid == owner_pid)
                        }) else {
                            log::warn!(
                                "[nouveau-uapi] NVIF NEW oclass={:#06x}: no channel with token={} \
                                 for pid={} -- CHANNEL_ALLOC must come first",
                                new.oclass,
                                hdr.token,
                                owner_pid
                            );
                            return Err(nv::EINVAL);
                        };
                        c.rm_backed
                    };
                    if rm_backed {
                        let Some(device_instance) = *self.rm_device_instance.lock() else {
                            crate::klog_warn!(
                                "[nouveau-uapi] NVIF NEW oclass={:#06x}: channel claims RM \
                                 backing but no device instance -- refusing",
                                new.oclass
                            );
                            return Err(nv::ENODEV);
                        };
                        // Build the class on the CALLER'S channel: the compositor
                        // is ctx 0 (unregistered pids also map to 0), each GL
                        // client its own ctx >= 1. A client's 3D class on ctx 0's
                        // channel leaves the client channel's GR context unbuilt
                        // and hangs its first 3D method.
                        let ctx_idx = self.ctx_idx_for_pid(owner_pid);
                        match nvidia_rm_sys::rm_init::class_alloc(
                            device_instance,
                            ctx_idx,
                            new.oclass as u32,
                        ) {
                            Ok((h_object, 0)) => {
                                nv::class_object_insert(hdr.token, new.object, h_object, owner_pid);
                                crate::klog_info!(
                                    "[nouveau-uapi] NVIF NEW oclass={:#06x} pid={} -> RM object \
                                     {:#010x} on CTX {} channel token={} (engine context will be built)",
                                    new.oclass,
                                    owner_pid,
                                    h_object,
                                    ctx_idx,
                                    hdr.token
                                );
                            }
                            Ok((_, alloc_status)) => {
                                crate::klog_warn!(
                                    "[nouveau-uapi] NVIF NEW oclass={:#06x}: RM refused the class \
                                     object, status={:#x} -- failing the NEW (submitting methods \
                                     of this class would MMU-fault the channel)",
                                    new.oclass,
                                    alloc_status
                                );
                                return Err(nv::EINVAL);
                            }
                            Err(status) => {
                                crate::klog_warn!(
                                    "[nouveau-uapi] NVIF NEW oclass={:#06x}: class_alloc failed, \
                                     NV_STATUS={:#x}",
                                    new.oclass,
                                    status
                                );
                                return Err(nv::ENODEV);
                            }
                        }
                    } else {
                        log::warn!(
                            "[nouveau-uapi] NVIF NEW subchannel oclass={:#06x} on channel token={} \
                             -- accepted (discovery channel, bookkeeping only)",
                            new.oclass,
                            hdr.token
                        );
                    }
                }
                Ok(0)
            }

            nv::NVIF_IOCTL_V0_MTHD => {
                const MB: usize = core::mem::size_of::<nv::NvifIoctlMthdV0>();
                if body_len < MB {
                    return Err(nv::EINVAL);
                }
                let mthd = unsafe { core::ptr::read_unaligned(body as *const nv::NvifIoctlMthdV0) };
                if mthd.method != nv::NV_DEVICE_V0_INFO {
                    log::warn!(
                        "[nouveau-uapi] NVIF MTHD: method {:#04x} not implemented",
                        mthd.method
                    );
                    return Err(nv::ENOSYS);
                }
                if body_len - MB < core::mem::size_of::<nv::NvDeviceInfoV0>() {
                    return Err(nv::EINVAL);
                }
                // Report the board's REAL VRAM size. GEM_NEW honours a VRAM-only
                // request as `NV01_MEMORY_LOCAL_USER`; GART and GART|VRAM stay
                // CPU-visible sysmem (see GEM_NEW). This threads the needle the
                // pure zero-VRAM model could not:
                //
                //   * Zero-VRAM (ram_user=0) made NVK advertise a SINGLE
                //     sysmem type HOST_VISIBLE|HOST_COHERENT|HOST_CACHED with
                //     NO DEVICE_LOCAL bit (nvk_physical_device.c's iGPU path).
                //     wlroots' Vulkan renderer asks `find_mem_type(DEVICE_LOCAL)`
                //     for its render targets and found none -- "Failed to find
                //     suitable memory type" (render/vulkan/renderer.c), no
                //     compositor.
                //   * With `ram_user > 0` NVK advertises the discrete types,
                //     including (Maxwell+, so every board here) a
                //     DEVICE_LOCAL|HOST_VISIBLE|HOST_COHERENT type. wlroots is
                //     satisfied. And NVK maps a DEVICE_LOCAL type to
                //     `NVKMD_MEM_LOCAL`, whose nouveau domain is `GART | VRAM`
                //     (nvkmd_nouveau_mem.c) -- GART is always in the set, so
                //     the kernel is free to place it in CPU-visible sysmem.
                //
                // The old danger -- advertising VRAM made the swapchain land
                // in true VRAM, gem_map_cpu published the FB OFFSET as a host
                // address, and userspace rendered into LOW KERNEL RAM (the
                // vmar-teardown deadlock) -- is gone because GART|VRAM stays
                // sysmem (HOST_VISIBLE) and VRAM-only LOCAL never publishes a
                // CPU map. Present/scanout remains CREATE_DUMB sysmem.
                // Floor a 0 (device-id not in identify_gpu) like GETPARAM_FB_SIZE
                // does — NVIF device INFO is the OTHER VRAM size NVK reads, and a
                // 0 here is the same empty-heap NULL walk. See effective_vram_mb.
                let vram_bytes = (self.effective_vram_mb() as u64) * 1024 * 1024;
                let chipset = self.nouveau_chipset_id();
                let mut info = nv::NvDeviceInfoV0 {
                    version: 0,
                    // PCI/AGP/PCIE all map to NV_DEVICE_TYPE_DIS (discrete) in
                    // mesa, which NVK's conformance gate requires; IGP/SOC do
                    // not. Every GPU this driver binds is a discrete PCIe part.
                    platform: nv::NV_DEVICE_INFO_V0_PCIE,
                    chipset,
                    revision: self.nouveau_chip_revision(),
                    // `family` only enumerates pre-Pascal families upstream and
                    // mesa does not read it; 0 is honest.
                    family: 0,
                    pad06: [0; 2],
                    ram_size: vram_bytes,
                    // `ram_user` is what mesa takes as vram_size_B.
                    ram_user: vram_bytes,
                    chip: [0; 16],
                    name: [0; 64],
                };
                // Display strings only (mesa copies them verbatim into
                // device_name/chipset_name).
                let chip_tag = match self.nouveau_arch() {
                    NvidiaArchitecture::Turing => b"TU1xx".as_slice(),
                    NvidiaArchitecture::Ampere => b"GA1xx".as_slice(),
                    NvidiaArchitecture::AdaLovelace => b"AD1xx".as_slice(),
                    NvidiaArchitecture::Hopper => b"GH1xx".as_slice(),
                    NvidiaArchitecture::Blackwell => b"GB2xx".as_slice(),
                    NvidiaArchitecture::Unknown => b"NV".as_slice(),
                };
                let n = chip_tag.len().min(info.chip.len() - 1);
                info.chip[..n].copy_from_slice(&chip_tag[..n]);
                let name_src = self.name.as_bytes();
                let n = name_src.len().min(info.name.len() - 1);
                info.name[..n].copy_from_slice(&name_src[..n]);

                unsafe {
                    core::ptr::write_unaligned((body + MB) as *mut nv::NvDeviceInfoV0, info);
                }
                log::warn!(
                    "[nouveau-uapi] NVIF MTHD NV_DEVICE_V0_INFO -> chipset={:#05x} rev={:#04x} \
                     ram_user={} MiB (discrete memory types advertised so wlroots finds a \
                     DEVICE_LOCAL type; every allocation still backed by CPU-visible sysmem in \
                     GEM_NEW) platform=PCIE",
                    chipset,
                    info.revision,
                    self.vram_size_mb
                );
                Ok(0)
            }

            nv::NVIF_IOCTL_V0_SCLASS => {
                const SB: usize = core::mem::size_of::<nv::NvifIoctlSclassV0>();
                const EB: usize = core::mem::size_of::<nv::NvifSclassOclassV0>();
                if body_len < SB {
                    return Err(nv::EINVAL);
                }
                let mut sclass =
                    unsafe { core::ptr::read_unaligned(body as *const nv::NvifIoctlSclassV0) };
                // Class enumeration is per CHANNEL: mesa sends route=0xff with
                // token=<channel> straight after CHANNEL_ALLOC, mirroring real
                // nouveau where these objects are children of the channel.
                // Answering without one would advertise engines on a channel
                // that does not exist.
                // Class enumeration is per CHANNEL. Mesa sends route=0xff and
                // token=<the channel id CHANNEL_ALLOC handed back>, mirroring
                // real nouveau, where these objects are children of the channel
                // object (`nouveau_abi16_ioctl_sclass` resolves ioctl->token to
                // an abi16 channel and rejects anything else). Resolve it for
                // real rather than assuming there is exactly one.
                if hdr.route != 0xff {
                    log::warn!(
                        "[nouveau-uapi] NVIF SCLASS: route={:#04x}, expected 0xff (channel-scoped)",
                        hdr.route
                    );
                    return Err(nv::EINVAL);
                }
                // The caller's own channel, as in Linux (`nouveau_abi16_chan`
                // walks the calling file's channels): another process's
                // token is EINVAL, not its engine list.
                let chan = self.nouveau_channels.lock();
                let Some(st) = chan.iter().find(|c| {
                    c.id >= 0
                        && c.id as u64 == hdr.token
                        && (owner_pid == 0 || c.owner_pid == owner_pid)
                }) else {
                    log::warn!(
                        "[nouveau-uapi] NVIF SCLASS: no channel with token={} for pid={} -- \
                         CHANNEL_ALLOC must come first",
                        hdr.token,
                        owner_pid
                    );
                    return Err(nv::EINVAL);
                };
                let (h_vas, h_notifier, rm_backed) = (st.h_vas, st.notifier_handle, st.rm_backed);
                drop(chan);
                let Some(classes) = self.nouveau_engine_classes() else {
                    return Err(nv::EINVAL);
                };
                // Honour the caller's advertised slot count, the real payload
                // length, AND the protocol's own ceiling.
                let room = (sclass.count as usize)
                    .min((body_len - SB) / EB)
                    .min(nv::NVIF_SCLASS_MAX);
                let n = classes.len().min(room);
                let arr = (body + SB) as *mut nv::NvifSclassOclassV0;
                for i in 0..n {
                    unsafe {
                        core::ptr::write_unaligned(
                            arr.add(i),
                            nv::NvifSclassOclassV0 {
                                oclass: classes[i],
                                minver: 0,
                                maxver: 0,
                            },
                        );
                    }
                }
                // Mesa reads ALL `NOUVEAU_WS_CONTEXT_MAX_CLASSES` slots
                // regardless of the count we report, so leave no stale entries
                // behind in the tail.
                for i in n..room {
                    unsafe {
                        core::ptr::write_unaligned(
                            arr.add(i),
                            nv::NvifSclassOclassV0 {
                                oclass: 0,
                                minver: 0,
                                maxver: 0,
                            },
                        );
                    }
                }
                sclass.count = n as u8;
                unsafe { core::ptr::write_unaligned(body as *mut nv::NvifIoctlSclassV0, sclass) };
                log::warn!(
                    "[nouveau-uapi] NVIF SCLASS on {} channel token={} (hVas={:#010x} \
                     hNotifier={:#010x}) -> {} classes {:#06x?}",
                    if rm_backed { "RM-backed" } else { "discovery" },
                    hdr.token,
                    h_vas,
                    h_notifier,
                    n,
                    &classes[..n]
                );
                Ok(0)
            }

            nv::NVIF_IOCTL_V0_DEL => {
                // DEL identifies the object by `hdr.object` (the same cookie
                // the NEW carried in `new.object`). If it was one of the
                // RM-backed class objects, free the real RM object too --
                // NVK deallocates its five subchannels on every context
                // destroy, and each successive context re-allocates them.
                // Scoped to the calling pid: the cookie is a userspace heap
                // pointer, equal values across two Mesa processes are
                // possible, and an unscoped removal would free another live
                // client's class object (see `class_object_remove`).
                if let Some(h_object) = nv::class_object_remove(hdr.object, owner_pid) {
                    if let Some(device_instance) = *self.rm_device_instance.lock() {
                        let status = nvidia_rm_sys::rm_init::class_free(device_instance, h_object);
                        if status != 0 {
                            crate::klog_warn!(
                                "[nouveau-uapi] NVIF DEL: class_free({:#010x}) -> NV_STATUS={:#x}",
                                h_object,
                                status
                            );
                        }
                    }
                }
                Ok(0)
            }

            other => {
                log::warn!(
                    "[nouveau-uapi] NVIF: type {:#04x} not implemented -- returning ENOSYS",
                    other
                );
                Err(nv::ENOSYS)
            }
        }
    }

    fn nouveau_ioctl(&self, request: u32, arg: usize, owner_pid: u64) -> Result<usize, i32> {
        // Catch-all errno reporter around the real dispatch. Hardware showed
        // labwc's vkCreateDevice dying with -13 while the console carried NOT
        // ONE [nouveau-uapi] failure line: several arms return errors without
        // their own klog, so "which ioctl failed?" was unanswerable from a
        // photo. One line per DISTINCT (nr, errno) pair -- repeats collapse,
        // an ioctl storm cannot own the UART. ENOSYS stays out: the
        // unhandled-NR arm already names those with more detail.
        let res = self.nouveau_ioctl_dispatch(request, arg, owner_pid);
        if let Err(e) = res {
            use super::nouveau_uapi as nv;
            if e != nv::ENOSYS && (request >> 8) & 0xff == 0x64 {
                let (_dir, nr, _size) = nv::decode_ioc(request);
                let sig = ((nr as u64) << 32) ^ (e as u32 as u64);
                static LAST_ERR_SIG: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(u64::MAX);
                if LAST_ERR_SIG.swap(sig, core::sync::atomic::Ordering::Relaxed) != sig {
                    crate::klog_warn!(
                        "[nouveau-uapi] {} (nr={:#04x}) -> errno {} to userspace \
                         (identical repeats suppressed)",
                        nv::nouveau_ioctl_name(nr),
                        nr,
                        e
                    );
                }
            }
        }
        res
    }

    fn nouveau_ioctl_dispatch(
        &self,
        request: u32,
        arg: usize,
        owner_pid: u64,
    ) -> Result<usize, i32> {
        use super::nouveau_uapi as nv;
        if !nv::enabled() {
            return Err(nv::ENOSYS);
        }
        // Name every distinct ioctl the first time Mesa issues it, so one
        // real-hardware boot reveals the full vocabulary and, above all, the
        // submission path (legacy GEM_PUSHBUF vs new EXEC). Bounded/de-duped.
        nv::trace_first_sight(request);
        // Dispatch by NR, exactly like Linux:
        //   nouveau_drm.c: `switch (_IOC_NR(cmd) - DRM_COMMAND_BASE)`
        // The caller's direction and size bits are ADVISORY. Matching the full
        // request number (as this driver used to) silently loses any ioctl
        // whose encoding differs from ours -- which is precisely what happened
        // with VM_INIT: mesa issues it through drmCommandWrite (_IOW,
        // 0x40106450) while we only accepted the _IOWR form (0xC0106450), so
        // it fell through to ENOSYS, mesa cleared `has_vm_bind`, and NVK
        // dropped the GPU with VK_ERROR_INCOMPATIBLE_DRIVER -- zero Vulkan
        // devices, no diagnostic. NVIF makes NR dispatch mandatory anyway: it
        // multiplexes five different payload sizes and directions onto nr 0x47.
        // `nouveau_ioctl` is the fall-through for ANY unrecognised ioctl on
        // /dev/dri/*, not just DRM ones, so NR alone is not a safe key: the
        // terminal/file families collide (FIONCLEX 0x5450 -> VM_INIT's nr,
        // FIOCLEX 0x5451 -> VM_BIND, FIOASYNC 0x5452 -> EXEC). Linux never has
        // this problem because drm_ioctl() only ever sees type 'd'. Require it.
        if (request >> 8) & 0xff != 0x64 {
            return Err(nv::ENOSYS);
        }
        let (_dir, nr, size) = nv::decode_ioc(request);
        // Dispatching by NR deliberately ignores the caller's DIRECTION bits,
        // but the SIZE still has to be honoured: every arm below casts `arg` to
        // a fixed struct and writes results back into it, so a caller that
        // declared a shorter payload than our struct would have memory written
        // past the end of its buffer. The old full-request match rejected those
        // implicitly (the size is baked into the request number); with NR
        // dispatch that guard has to be explicit. Linux does the same thing --
        // drm_ioctl() copies in/out against its own table's size, never the
        // caller's word.
        if let Some(need) = nv::min_payload_for_nr(nr) {
            if (size as usize) < need {
                log::warn!(
                    "[nouveau-uapi] {} (nr={:#04x}): caller payload {} < {} required -- \
                     refusing rather than writing past the caller's buffer",
                    nv::nouveau_ioctl_name(nr),
                    nr,
                    size,
                    need
                );
                return Err(nv::EINVAL);
            }
        }
        match nr {
            nv::NR_GETPARAM => {
                let req = unsafe { &mut *(arg as *mut nv::DrmNouveauGetparam) };
                // effective_vram_mb() floors a 0 (device-id not in identify_gpu)
                // up to a per-arch value so NVK's VRAM heap is never empty — see
                // its doc for the NULL heap-list crash this prevents.
                let eff_vram_mb = self.effective_vram_mb();
                let vram_bytes = (eff_vram_mb as u64) * 1024 * 1024;
                // One-shot, VISIBLE memory picture NVK builds its heaps from.
                // FB_SIZE = 0 was the classic cause of the NULL heap-list walk
                // that crashes NVK right after device creation ("failed to
                // create timeline semaphore"); the floor above stops it, and
                // this line shows both the raw table value and the effective one
                // plus the device-id, so an unlisted variant is named for a
                // follow-up identify_gpu entry with its exact size.
                {
                    static LOGGED: core::sync::atomic::AtomicBool =
                        core::sync::atomic::AtomicBool::new(false);
                    if !LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                        let rm_attached = self.rm_device_instance.lock().is_some();
                        crate::klog_info!(
                            "[nouveau-uapi] NVK mem: device={:#06x} ({}) FB_SIZE={} MiB \
                             (table vram_size_mb={}, effective={}) VRAM_BAR_SIZE={} bytes \
                             rm_attached={}",
                            self.device_id,
                            self.gpu_model,
                            vram_bytes / (1024 * 1024),
                            self.vram_size_mb,
                            eff_vram_mb,
                            self.info.fb_size as u64,
                            rm_attached
                        );
                    }
                }
                req.value = match req.param {
                    nv::NOUVEAU_GETPARAM_PCI_VENDOR => 0x10de,
                    nv::NOUVEAU_GETPARAM_PCI_DEVICE => self.device_id as u64,
                    // Real nouveau distinguishes AGP/PCI/PCIE; every GPU this
                    // driver recognizes (Turing+) is PCIe-only.
                    nv::NOUVEAU_GETPARAM_BUS_TYPE => 2,
                    nv::NOUVEAU_GETPARAM_FB_SIZE => vram_bytes,
                    // The BAR1 *aperture*, which is NOT the VRAM size on a
                    // non-ReBAR system (typically 256 MiB). NVK compares the
                    // two to decide whether to expose a second, smaller
                    // host-visible heap; reporting them equal makes it treat
                    // ALL VRAM as CPU-mappable and mmap past the aperture.
                    nv::NOUVEAU_GETPARAM_VRAM_BAR_SIZE => self.info.fb_size as u64,
                    nv::NOUVEAU_GETPARAM_AGP_SIZE => 0,
                    // The REAL chip id, not the architecture's lower bound:
                    // gallium's nouveau GL still reads this, and a bound value
                    // decodes to the wrong SM (0x170 is GA100, not GA10x).
                    nv::NOUVEAU_GETPARAM_CHIPSET_ID => self.nouveau_chipset_id() as u64,
                    nv::NOUVEAU_GETPARAM_HAS_BO_USAGE => 0,
                    nv::NOUVEAU_GETPARAM_HAS_PAGEFLIP => 0,
                    // 1: this driver accepts the tile_mode/tile_flags fields
                    // on GEM_NEW and the PTE kind on VM_BIND. It gates
                    // NVK's VK_EXT_image_drm_format_modifier, which wlroots
                    // REQUIRES -- with 0 here the Vulkan renderer refuses to
                    // start at all ("required device extension
                    // VK_EXT_image_drm_format_modifier not found"). A PTE kind
                    // this driver cannot program is rejected at GEM_NEW when
                    // requested via tile_flags, else mapped uncompressed at
                    // VM_BIND (refusing it there hangs the channel).
                    nv::NOUVEAU_GETPARAM_HAS_VMA_TILEMODE => 1,
                    // Linux reports the VRAM manager's usage: bytes of VRAM
                    // held by every client's live objects, not just the
                    // caller's. This driver's VRAM objects are the
                    // VRAM-domain entries of `nouveau_gem`, so their sum is
                    // that number; a constant 0 read as "nothing allocated"
                    // to the GL HUD and to anything that budgets by it.
                    nv::NOUVEAU_GETPARAM_VRAM_USED => self.nouveau_vram_used(),
                    // A monotonically-rising nanosecond counter. Real nouveau
                    // returns the GPU's PTIMER; Mesa uses this for GL_TIMESTAMP,
                    // which only needs a rising clock, so a CPU-derived
                    // monotonic (safe -- no BAR0 read) is an honest stand-in.
                    nv::NOUVEAU_GETPARAM_PTIMER_TIME => {
                        let now_us = unsafe { crate::bus::drivers_timer_now_as_micros() };
                        now_us * 1000
                    }
                    // This driver's EXEC ioctl caps at 64 pushbuffers per call.
                    nv::NOUVEAU_GETPARAM_EXEC_PUSH_MAX => 64,
                    // Enumeration-fatal in mesa (`goto out_err`), so this can
                    // never be EINVAL: see `nouveau_graph_units`, which uses the
                    // live GSP-RM GR probe when the GPU is attached.
                    nv::NOUVEAU_GETPARAM_GRAPH_UNITS => self.nouveau_graph_units(),
                    _ => {
                        // warn, not debug: at the default LOG=warn boot level a
                        // real client (NVK) querying a param this milestone
                        // doesn't know about would otherwise fail EINVAL with
                        // zero trace -- exactly the case a first real-hardware
                        // run most needs visible.
                        log::warn!(
                            "[nouveau-uapi] GETPARAM: unknown param {:#x} -- returning EINVAL",
                            req.param
                        );
                        return Err(nv::EINVAL);
                    }
                };
                Ok(0)
            }

            nv::NR_CHANNEL_ALLOC => {
                // [auto-bringup] Only THIS GPU. On dual-GPU boxes card0 is the
                // compute card: ensure_console_gpu_brought_up is a no-op there
                // (drives_boot_display == false). Never walk other GPUs here —
                // that used to start console GSP-RM from the first GL client and
                // freeze the machine the same way boot-time auto-bringup did.
                self.ensure_console_gpu_brought_up();
                let chan = self.nouveau_channels.lock();
                if chan.len() >= nv::MAX_CHANNELS {
                    log::warn!(
                        "[nouveau-uapi] CHANNEL_ALLOC: {} channels already live",
                        chan.len()
                    );
                    return Err(nv::EBUSY);
                }
                // Sticky ctx-0 owner wins over "who currently has an rm_backed
                // channel": a throwaway CHANNEL_FREE must not let a client take
                // the compositor ladder (F-M15). Fall back to the live table
                // only while nobody has claimed yet (first boot claimer).
                let sticky = self.ctx0_owner.load(Ordering::Acquire);
                let other_holds_ctx0 = if sticky != 0 {
                    sticky != owner_pid
                } else {
                    chan.iter().any(|c| c.rm_backed && c.owner_pid != owner_pid)
                };
                drop(chan);
                if other_holds_ctx0 {
                    // The compositor holds context 0. This is a GL CLIENT: give it
                    // its OWN GPU context (own VAS + GPFIFO channel), built on its
                    // first GPU touch and reused here. NVK usually issues VM_BIND
                    // (which builds the context) BEFORE this CHANNEL_ALLOC, so this
                    // is normally a reuse; ensure_ctx_for_pid is idempotent per
                    // pid. Its VM_BIND/EXEC route to this SAME context, so the
                    // client's pushbuffers live in the SAME VA space its channel
                    // executes in -- the fix for the MMU fault its first
                    // submission hit when the binds went to the compositor's VAS.
                    // On no free slot / ctx_alloc failure ensure returns 0 and we
                    // hand back a discovery channel (EXEC ENODEV -> llvmpipe),
                    // never worse than before.
                    //
                    // The channel-table lock is RELEASED across ensure_ctx_for_pid:
                    // building a first-touch context ends in ctx_prime -- a cold
                    // golden-context load that can take ~500 ms of real RM work --
                    // and `nouveau_channels` sits on EXEC's hot path (the
                    // ownership check), taken with IRQs off (lock::Mutex). Held
                    // here, every EXEC in the system -- the COMPOSITOR's included
                    // -- span-blocked behind a starting GL client for the whole
                    // prime (a frozen frame and cursor per client launch).
                    return self.channel_alloc_client(owner_pid, arg);
                }
                let Some(device_instance) = *self.rm_device_instance.lock() else {
                    // No RM yet. NVK allocates a channel during *enumeration*
                    // (nouveau_ws_context_create inside nouveau_ws_device_new)
                    // only to run NVIF SCLASS and five subchannel NEWs, then
                    // frees it -- it never submits. Refusing here therefore
                    // costs the whole physical device (vkEnumeratePhysicalDevices
                    // reports 0 GPUs) even though nothing about that sequence
                    // needs hardware. Serve it from software instead, and let
                    // the paths that DO need the RM (GEM_NEW/VM_BIND/EXEC) fail
                    // with their own explicit ENODEV.
                    //
                    // Attaching the RM implicitly here is deliberately NOT done:
                    // the ladder boots GSP-RM and does real bring-up that can
                    // hang the machine, so it stays an explicit operator action
                    // (`cat /proc/gpustep14`).
                    return self.channel_alloc_discovery(owner_pid, arg);
                };
                // F-M14: step16/17 + selftest take ~1-2 s of RM work. Never hold
                // `nouveau_channels` (IRQ-off spinlock, EXEC hot path) across it.
                nvidia_rm_sys::os_interface::capture_begin();
                let ladder = nvidia_rm_sys::rm_init::step16(device_instance);
                let _ = nvidia_rm_sys::os_interface::capture_take();
                let ladder = match ladder {
                    Ok(g) if g.ctxshare_status == 0 => g,
                    Ok(g) => {
                        crate::klog_warn!(
                            "[nouveau-uapi] CHANNEL_ALLOC: GR allocation ladder incomplete (ctxshare status {:#x})",
                            g.ctxshare_status
                        );
                        return Err(nv::ENODEV);
                    }
                    Err(status) => {
                        crate::klog_warn!(
                            "[nouveau-uapi] CHANNEL_ALLOC: step16 failed, NV_STATUS={:#x}",
                            status
                        );
                        return Err(nv::ENODEV);
                    }
                };
                nvidia_rm_sys::os_interface::capture_begin();
                let channel = nvidia_rm_sys::rm_init::step17(device_instance);
                let _ = nvidia_rm_sys::os_interface::capture_take();
                let channel = match channel {
                    Ok(c) if c.sched_status == 0 => c,
                    Ok(c) => {
                        crate::klog_warn!(
                            "[nouveau-uapi] CHANNEL_ALLOC: compute channel incomplete (sched status {:#x})",
                            c.sched_status
                        );
                        return Err(nv::ENODEV);
                    }
                    Err(status) => {
                        crate::klog_warn!(
                            "[nouveau-uapi] CHANNEL_ALLOC: step17 failed, NV_STATUS={:#x}",
                            status
                        );
                        return Err(nv::ENODEV);
                    }
                };
                // Cache the channel's error-notifier PA now, in the same
                // context that just ran step16+step17 (the RM's locks were
                // taken and released twice already, sequentially) -- NEVER
                // from the EXEC failure path: an RM lock acquire inside a
                // failure storm is what wedged the machine on real hardware
                // (DEADLOCK: spinlock(s) stuck >8s). The failure path only
                // does a lock-free phys read of this cached PA.
                match nvidia_rm_sys::rm_init::chan_notifier_pa(device_instance) {
                    Ok(pa) => nv::set_chan_notifier_pa(pa),
                    Err(status) => crate::klog_warn!(
                        "[nouveau-uapi] CHANNEL_ALLOC: error-notifier PA lookup failed, \
                         NV_STATUS={:#x} (RC dump on EXEC failure unavailable)",
                        status
                    ),
                }
                // One-shot, first RM-backed channel of the boot, ring still
                // empty: prove (or disprove) the whole submission path with a
                // KERNEL-authored push before Mesa gets a turn -- see the
                // method's doc for the three verdicts and what each one pins.
                static SELFTEST_DONE: AtomicBool = AtomicBool::new(false);
                if !SELFTEST_DONE.swap(true, Ordering::Relaxed) {
                    self.nouveau_channel_selftest(device_instance, channel.buf_gpu_va);
                }
                // Two processes can race the unclaimed path; only the CAS winner
                // keeps ctx 0. The loser becomes a normal client context.
                if !self.ctx0_try_claim(owner_pid) {
                    crate::klog_warn!(
                        "[nouveau-uapi] CHANNEL_ALLOC owner_pid={} lost ctx0 race to pid={} -- client context",
                        owner_pid,
                        self.ctx0_owner.load(Ordering::Acquire)
                    );
                    return self.channel_alloc_client(owner_pid, arg);
                }
                let mut chan = self.nouveau_channels.lock();
                if chan.len() >= nv::MAX_CHANNELS {
                    // Sticky claim already taken; release so a later compositor
                    // can retry (table full is transient under discovery churn).
                    self.ctx0_release(owner_pid);
                    log::warn!(
                        "[nouveau-uapi] CHANNEL_ALLOC: {} channels already live",
                        chan.len()
                    );
                    return Err(nv::EBUSY);
                }
                let new_id = (0i32..)
                    .find(|i| !chan.iter().any(|c| c.id == *i))
                    .unwrap_or(0);
                chan.push(nv::NouveauChannelState {
                    id: new_id,
                    h_vas: ladder.h_vas,
                    notifier_handle: channel.h_notifier,
                    rm_backed: true,
                    // The sticky compositor owner is context 0 -- the singleton
                    // step16/step17 ladder.
                    ctx_idx: 0,
                    owner_pid,
                });
                drop(chan);
                let req = unsafe { &mut *(arg as *mut nv::DrmNouveauChannelAlloc) };
                req.channel = new_id;
                req.notifier_handle = channel.h_notifier;
                req.pushbuf_domains = nv::NOUVEAU_GEM_DOMAIN_VRAM;
                req.nr_subchan = 0;
                log::info!(
                    "[nouveau-uapi] CHANNEL_ALLOC owner_pid={} -> channel={} ctx0 sticky (hVas={:#010x} hNotifier={:#010x})",
                    owner_pid,
                    new_id,
                    ladder.h_vas,
                    channel.h_notifier
                );
                Ok(0)
            }

            nv::NR_CHANNEL_FREE => {
                let req = unsafe { &*(arg as *const nv::DrmNouveauChannelFree) };
                let (was_rm_backed, channel_id) = {
                    let mut chans = self.nouveau_channels.lock();
                    // Only the channel's own process may free it: freeing the
                    // compositor's channel 0 from a client made its next EXEC
                    // fail EINVAL.
                    let Some(pos) = chans.iter().position(|c| {
                        c.id == req.channel && (owner_pid == 0 || c.owner_pid == owner_pid)
                    }) else {
                        log::warn!(
                            "[nouveau-uapi] CHANNEL_FREE: no such channel {} (for pid={})",
                            req.channel,
                            owner_pid
                        );
                        return Err(nv::EINVAL);
                    };
                    let c = chans.remove(pos);
                    (c.rm_backed, c.id)
                };
                // CHANNEL_FREE must NOT touch the VM: in the nouveau uAPI,
                // VM_BIND mappings belong to the DRM FILE's VA space, not to
                // any channel -- real nouveau reclaims them at VM_FINI/file
                // close, never on channel teardown. An earlier milestone
                // drained every mapping here ("so a new CHANNEL_ALLOC starts
                // from an empty VM"), a leftover of the one-channel model,
                // and once the RM backing became per-process that turned
                // fatal: NVK's `nouveau_ws_device_new` creates a THROWAWAY
                // context just to read the engine classes and destroys it
                // (context_create -> read cls_* -> context_destroy), and
                // labwc runs TWO Vulkan instances in one process -- so the
                // second instance's enumeration-time CHANNEL_FREE landed
                // AFTER the first instance had bound its buffers, wiped the
                // whole VAS, and the next EXEC touched unmapped VAs.
                //
                // Class objects ARE per-channel (NVIF NEW keyed by channel
                // token). Reap any that Mesa left behind without DEL -- the
                // throwaway-enumeration leak -- without touching another
                // live channel's classes. Do NOT ctx_free / reset ctx0 here:
                // VAS is shared across channels of the same pid.
                if was_rm_backed {
                    if let Some(device_instance) = *self.rm_device_instance.lock() {
                        let leftovers =
                            nv::class_objects_drain_channel(channel_id as u64, owner_pid);
                        for (_token, h_object) in leftovers.iter() {
                            let status =
                                nvidia_rm_sys::rm_init::class_free(device_instance, *h_object);
                            if status != 0 {
                                log::warn!(
                                    "[nouveau-uapi] CHANNEL_FREE channel={}: class_free \
                                     h={:#010x} -> NV_STATUS={:#x}",
                                    channel_id,
                                    h_object,
                                    status
                                );
                            }
                        }
                        if !leftovers.is_empty() {
                            log::info!(
                                "[nouveau-uapi] CHANNEL_FREE channel={}: reaped {} class \
                                 object(s) left without NVIF DEL",
                                channel_id,
                                leftovers.len()
                            );
                        }
                    }
                }
                Ok(0)
            }

            nv::NR_VM_INIT => {
                // VM_INIT initialises the GPU VA space for THIS drm file and is
                // the FIRST driver-private ioctl NVK issues: its
                // nouveau_ws_device_new() -> nouveau_ws_device_alloc() calls it
                // during physical-device creation, BEFORE any CHANNEL_ALLOC.
                // Requiring a channel here (as an earlier milestone did) makes
                // that call fail EINVAL, so nouveau_ws_device_new() aborts and
                // vkEnumeratePhysicalDevices returns 0 GPUs with no further
                // trace -- exactly the "NVK sees nothing" symptom on real
                // hardware. VM_INIT is standalone by design; accept it here.
                //
                // It is DRM_IOW (client -> kernel): the client passes the VA
                // sub-range it wants the KERNEL to manage (the rest it manages
                // itself). Real per-mapping VA carving happens in VM_BIND against
                // the RM; VM_INIT only has to acknowledge the reservation, so read
                // the requested range for the log and return success. Do NOT
                // write the struct back (write-only ioctl).
                let req = unsafe { &*(arg as *const nv::DrmNouveauVmInit) };
                log::warn!(
                    "[nouveau-uapi] VM_INIT kernel_managed_addr={:#x} size={:#x} -> accepted (standalone, no channel required)",
                    req.kernel_managed_addr,
                    req.kernel_managed_size
                );
                Ok(0)
            }

            nv::NR_VM_BIND => {
                if !self.nouveau_rm_vas_ready() {
                    crate::klog_warn!(
                        "[nouveau-uapi] VM_BIND: no RM-backed channel exists on this GPU, so no \
                         VA space was ever built -- nothing to bind into. Attach the RM \
                         (/proc/gpustep14) and CHANNEL_ALLOC again."
                    );
                    return Err(nv::ENODEV);
                }
                let req = unsafe { &*(arg as *const nv::DrmNouveauVmBind) };
                if req.wait_count != 0 || req.sig_count != 0 {
                    log::warn!(
                        "[nouveau-uapi] VM_BIND: wait_count/sig_count must be 0 -- VM_BIND ops complete synchronously within this ioctl (real RM calls, not queued GPU work), so there is nothing async to wait for or signal after (got wait_count={} sig_count={})",
                        req.wait_count, req.sig_count
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                const MAX_VM_BIND_OPS: u32 = 64;
                if req.op_count == 0 || req.op_ptr == 0 {
                    return Err(nv::EINVAL);
                }
                if req.op_count > MAX_VM_BIND_OPS {
                    log::warn!(
                        "[nouveau-uapi] VM_BIND: op_count={} exceeds the {} this milestone supports per call",
                        req.op_count, MAX_VM_BIND_OPS
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                let Some(device_instance) = *self.rm_device_instance.lock() else {
                    crate::klog_warn!("[nouveau-uapi] VM_BIND: GPU not attached to the RM yet");
                    return Err(nv::ENODEV);
                };
                // Ops are applied in order, one real RM call each -- NOT
                // atomic across the array: if op[i] fails, op[0..i] already
                // happened and stay applied, and op[i+1..] never run. Real
                // nouveau's own VM_BIND jobs behave the same way (each op
                // is validated/applied as it's processed, not as a single
                // all-or-nothing transaction).
                if !user_slice_ok::<nv::DrmNouveauVmBindOp>(req.op_ptr, req.op_count) {
                    return Err(nv::EFAULT);
                }
                let ops = unsafe {
                    core::slice::from_raw_parts(
                        req.op_ptr as *const nv::DrmNouveauVmBindOp,
                        req.op_count as usize,
                    )
                };
                // Sticky ctx-0 owner (F-M15): do not re-infer from live channels —
                // a throwaway CHANNEL_FREE must not rebuild a client context for
                // the compositor or hand ctx 0's VAS to a stranger.
                let is_compositor = self.ctx0_is_owner(owner_pid);
                let (ctx_idx, _, _) = self.ensure_ctx_for_pid(owner_pid, is_compositor);
                for (i, op) in ops.iter().enumerate() {
                    if let Err(e) = self.vm_bind_op(device_instance, ctx_idx, owner_pid, op) {
                        if req.op_count > 1 {
                            log::warn!(
                                "[nouveau-uapi] VM_BIND: op[{}] of {} failed, stopping ({} earlier op(s) already applied)",
                                i, req.op_count, i
                            );
                        }
                        return Err(e);
                    }
                }
                Ok(0)
            }

            nv::NR_EXEC => {
                if !self.nouveau_owns_rm_channel(owner_pid) {
                    crate::klog_warn!(
                        "[nouveau-uapi] EXEC: this client does not own the RM-backed channel (no GR \
                         channel/GPFIFO was ever built for it) -- refusing to submit against \
                         uninitialised hardware; free it and CHANNEL_ALLOC again now that the \
                         RM is attached"
                    );
                    return Err(nv::ENODEV);
                }
                let req = unsafe { &*(arg as *const nv::DrmNouveauExec) };
                const MAX_EXEC_PUSH: u32 = 64;
                const MAX_EXEC_SYNC: u32 = 64;
                if req.wait_count > MAX_EXEC_SYNC || (req.wait_count > 0 && req.wait_ptr == 0) {
                    crate::klog_warn!(
                        "[nouveau-uapi] EXEC: wait_count={} exceeds the {} this milestone supports (or wait_ptr is null)",
                        req.wait_count, MAX_EXEC_SYNC
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                if req.sig_count > MAX_EXEC_SYNC || (req.sig_count > 0 && req.sig_ptr == 0) {
                    crate::klog_warn!(
                        "[nouveau-uapi] EXEC: sig_count={} exceeds the {} this milestone supports (or sig_ptr is null)",
                        req.sig_count, MAX_EXEC_SYNC
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                // `access_ok()` for the three user arrays EXEC walks -- the
                // waits/signals of the probe path right below, the pushes,
                // and the signals consumed by both the direct-submit
                // (`exec_fast`) and the RM path. Checked once, up front.
                if !user_slice_ok::<nv::DrmNouveauExecPush>(req.push_ptr, req.push_count)
                    || !user_slice_ok::<nv::DrmNouveauSync>(req.wait_ptr, req.wait_count)
                    || !user_slice_ok::<nv::DrmNouveauSync>(req.sig_ptr, req.sig_count)
                {
                    return Err(nv::EFAULT);
                }
                if req.push_count == 0 {
                    // An EXEC with no pushes is nouveau's CHANNEL HEALTH
                    // PROBE, not a degenerate submission. NVK sends exactly
                    // this from `nvkmd_nouveau_exec_ctx_sync` after every
                    // syncobj wait ("Push an empty again, just to check for
                    // errors"): the real driver returns 0 on a live channel
                    // and -ENODEV on a killed one, and NVK maps ANY error to
                    // VK_ERROR_DEVICE_LOST. Refusing it with EOPNOTSUPP (as
                    // this arm used to) made every vkQueueWaitIdle report the
                    // device lost on a perfectly healthy channel -- zink fell
                    // over on the first frame while dmesg showed no failure
                    // at all. Waits/signals attached to an empty EXEC still
                    // count: NVK uses empty submits to chain syncobjs.
                    //
                    // Answer the probe HONESTLY on a wedged context: the whole
                    // point of nouveau's empty submit is "0 = channel alive,
                    // -ENODEV = channel killed", and a context latched wedged
                    // (fence timeout — its ring is jammed for good) is exactly
                    // the killed case. Reporting it alive made NVK bounce off
                    // the NEXT real submit's EIO instead, one round later,
                    // with a less truthful errno.
                    {
                        let probe_ctx = self.ctx_idx_for_pid(owner_pid);
                        if probe_ctx >= 1 && nv::ctx_is_wedged(probe_ctx) {
                            return Err(nv::ENODEV);
                        }
                    }
                    //
                    // The WAITS must be honoured BEFORE the signals, exactly
                    // like the non-empty path below: an empty EXEC with
                    // waits=[A] sigs=[B] is "B signals once A has signaled".
                    // Signaling B immediately (as this arm first did) is a
                    // premature signal whenever A is still pending — which
                    // can genuinely happen across processes, e.g. a syncobj
                    // carrying an imported sync_file fence another client has
                    // not signaled yet. Same bounded wait, same EIO-on-timeout
                    // contract as a real submission.
                    if req.wait_count > 0 && req.wait_ptr != 0 {
                        let waits = unsafe {
                            core::slice::from_raw_parts(
                                req.wait_ptr as *const nv::DrmNouveauSync,
                                req.wait_count as usize,
                            )
                        };
                        let handles: Vec<u32> = waits.iter().map(|s| s.handle).collect();
                        let points: Vec<u64> = waits
                            .iter()
                            .map(|s| {
                                let timeline =
                                    s.flags & nv::SYNC_TYPE_MASK == nv::SYNC_TIMELINE_SYNCOBJ;
                                if timeline {
                                    s.timeline_value
                                } else {
                                    1
                                }
                            })
                            .collect();
                        const WAIT_TIMEOUT_US: u64 = 10_000_000; // 10 s (Linux-like; was 1 s / F-M16)
                        let deadline_us =
                            unsafe { crate::bus::drivers_timer_now_as_micros() } + WAIT_TIMEOUT_US;
                        // Empty EXEC has no user pushes to hang a GPU ACQUIRE
                        // on; prefer same-ctx HW ACQUIRE only when a fast ctx
                        // can submit acquire(+fence) before signaling. Else
                        // CPU-wait the full set (10 s), including same-ctx
                        // pending fences — never premature-signal.
                        let ctx_idx = self.ctx_idx_for_pid(owner_pid);
                        let (acquires, cpu_h, cpu_p) =
                            self.partition_exec_waits(&handles, &points, ctx_idx);
                        let device_instance = *self.rm_device_instance.lock();
                        let with_fence = req.sig_count > 0 && req.sig_ptr != 0;
                        // Only HW-acquire when we also fence+attach sigs; an
                        // empty wait-only EXEC must still CPU-block until the
                        // fences land (ioctl contract).
                        let used_hw = with_fence
                            && !acquires.is_empty()
                            && device_instance
                                .map(|d| self.fast_ctx_ready(d, ctx_idx))
                                .unwrap_or(false);
                        if used_hw {
                            if !cpu_h.is_empty() {
                                match crate::scheme::syncobj::wait(
                                    &cpu_h,
                                    Some(&cpu_p),
                                    true,
                                    deadline_us,
                                ) {
                                    crate::scheme::syncobj::WaitOutcome::Signaled { .. } => {}
                                    crate::scheme::syncobj::WaitOutcome::Timeout => {
                                        crate::klog_warn!(
                                            "[nouveau-uapi] EXEC(empty): {} wait syncobj(s) still unsignaled after {}us -- not signaling its sig list (EIO) pid={}:{}",
                                            req.wait_count,
                                            WAIT_TIMEOUT_US,
                                            owner_pid,
                                            crate::scheme::syncobj::describe(&handles, Some(&points))
                                        );
                                        return Err(nv::EIO);
                                    }
                                    crate::scheme::syncobj::WaitOutcome::Invalid => {
                                        return Err(nv::ENOENT);
                                    }
                                }
                            }
                            match self.fast_submit(ctx_idx, &[], with_fence, &acquires) {
                                Ok(Some((fence_va, fence_gpu_va, payload))) => {
                                    let sigs = unsafe {
                                        core::slice::from_raw_parts(
                                            req.sig_ptr as *const nv::DrmNouveauSync,
                                            req.sig_count as usize,
                                        )
                                    };
                                    for sig in sigs {
                                        let timeline = sig.flags & nv::SYNC_TYPE_MASK
                                            == nv::SYNC_TIMELINE_SYNCOBJ;
                                        let target = if timeline { sig.timeline_value } else { 1 };
                                        if !crate::scheme::syncobj::attach_hw_fence(
                                            sig.handle,
                                            target,
                                            fence_va,
                                            fence_gpu_va,
                                            payload,
                                            ctx_idx,
                                            !timeline,
                                        ) {
                                            return Err(nv::ENOENT);
                                        }
                                    }
                                    return Ok(0);
                                }
                                _ => {
                                    // Fall through to full CPU wait below.
                                }
                            }
                        }
                        match crate::scheme::syncobj::wait(
                            &handles,
                            Some(&points),
                            true,
                            deadline_us,
                        ) {
                            crate::scheme::syncobj::WaitOutcome::Signaled { .. } => {}
                            crate::scheme::syncobj::WaitOutcome::Timeout => {
                                crate::klog_warn!(
                                    "[nouveau-uapi] EXEC(empty): {} wait syncobj(s) still unsignaled after {}us -- not signaling its sig list (EIO) pid={}:{}",
                                    req.wait_count,
                                    WAIT_TIMEOUT_US,
                                    owner_pid,
                                    crate::scheme::syncobj::describe(&handles, Some(&points))
                                );
                                return Err(nv::EIO);
                            }
                            crate::scheme::syncobj::WaitOutcome::Invalid => {
                                return Err(nv::ENOENT);
                            }
                        }
                    }
                    if req.sig_count > 0 && req.sig_ptr != 0 {
                        let sigs = unsafe {
                            core::slice::from_raw_parts(
                                req.sig_ptr as *const nv::DrmNouveauSync,
                                req.sig_count as usize,
                            )
                        };
                        for sig in sigs {
                            let timeline =
                                sig.flags & nv::SYNC_TYPE_MASK == nv::SYNC_TIMELINE_SYNCOBJ;
                            let target = if timeline { sig.timeline_value } else { 1 };
                            if !crate::scheme::syncobj::timeline_signal(sig.handle, target) {
                                return Err(nv::ENOENT);
                            }
                        }
                    }
                    return Ok(0);
                }
                if req.push_count > MAX_EXEC_PUSH || req.push_ptr == 0 {
                    crate::klog_warn!(
                        "[nouveau-uapi] EXEC: push_count={} must be between 1 and {} (got push_ptr={:#x})",
                        req.push_count, MAX_EXEC_PUSH, req.push_ptr
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                // The channel id NVK submits against is the one CHANNEL_ALLOC
                // handed it, and ids are assigned "lowest free" -- so only the
                // FIRST channel of a boot is 0. Demanding 0 here rejected every
                // later one with a bare EINVAL and no log line at all. Check
                // what actually matters instead: the caller owns this channel
                // AND it is the RM-backed one.
                {
                    let chans = self.nouveau_channels.lock();
                    // `drm_nouveau_exec.channel` is __u32 while
                    // `drm_nouveau_channel_alloc.channel` is __s32 -- that
                    // asymmetry is in nouveau_drm.h itself. Ids we hand out are
                    // never negative, so compare in the unsigned domain.
                    let mine = chans.iter().find(|c| {
                        c.id >= 0 && c.id as u32 == req.channel && c.owner_pid == owner_pid
                    });
                    match mine {
                        Some(c) if c.rm_backed => {}
                        Some(_) => {
                            let rm_owner = chans.iter().find(|c| c.rm_backed).map(|c| c.owner_pid);
                            drop(chans);
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC: channel={} belongs to pid={} but is a DISCOVERY channel (no GR channel/GPFIFO behind it); the RM-backed channel is held by pid={:?}",
                                req.channel,
                                owner_pid,
                                rm_owner
                            );
                            return Err(nv::ENODEV);
                        }
                        None => {
                            let known: Vec<i32> = chans.iter().map(|c| c.id).collect();
                            drop(chans);
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC: channel={} is not owned by pid={} (live channels: {:?})",
                                req.channel,
                                owner_pid,
                                known
                            );
                            return Err(nv::EINVAL);
                        }
                    }
                }
                let pushes = unsafe {
                    core::slice::from_raw_parts(
                        req.push_ptr as *const nv::DrmNouveauExecPush,
                        req.push_count as usize,
                    )
                };
                for push in pushes {
                    if push.va_len == 0 || push.va_len % 4 != 0 {
                        crate::klog_warn!(
                            "[nouveau-uapi] EXEC: push va={:#x} va_len={} is empty or not a multiple of 4 (pushbuffers are dword streams)",
                            push.va,
                            push.va_len
                        );
                        return Err(nv::EINVAL);
                    }
                }
                // One-shot: dump the head of the FIRST push Mesa ever submits.
                // With the self-test proving (or disproving) the plumbing, the
                // next question is what NVK's first methods ARE -- which
                // classes it SET_OBJECTs and which VAs its methods reference.
                // Translate the push VA back to CPU-readable memory through
                // our own tables (mapping va -> gem handle -> phys), the same
                // proven windows everything else uses; no RM calls.
                static FIRST_PUSH_DUMPED: AtomicBool = AtomicBool::new(false);
                if !FIRST_PUSH_DUMPED.swap(true, Ordering::Relaxed) {
                    if let Some(p0) = pushes.first() {
                        let phys = {
                            let maps = self.nouveau_vm_mappings.lock();
                            maps.iter()
                                .find(|m| p0.va >= m.va && p0.va < m.va + m.size)
                                .and_then(|m| {
                                    let gems = self.nouveau_gem.lock();
                                    gems.iter()
                                        .find(|g| g.handle == m.gem_handle)
                                        .and_then(|g| g.phys_addr)
                                        .map(|pa| pa + m.bo_offset + (p0.va - m.va))
                                })
                        };
                        match phys {
                            Some(pa) => {
                                let words = (p0.va_len as usize / 4).min(16);
                                let base = crate::bus::phys_to_virt(pa as usize);
                                let mut line = alloc::string::String::new();
                                for i in 0..words {
                                    let w = unsafe {
                                        core::ptr::read_volatile((base + i * 4) as *const u32)
                                    };
                                    let _ = core::fmt::write(&mut line, format_args!(" {:08x}", w));
                                }
                                crate::klog_info!(
                                    "[nouveau-uapi] first EXEC push: va={:#x} len={}B, first {} dwords:{}",
                                    p0.va,
                                    p0.va_len,
                                    words,
                                    line
                                );
                            }
                            None => {
                                crate::klog_warn!(
                                    "[nouveau-uapi] first EXEC push: va={:#x} len={}B is NOT inside any live VM_BIND mapping (or its GEM has no CPU address) -- that by itself would MMU-fault",
                                    p0.va,
                                    p0.va_len
                                );
                            }
                        }
                    }
                }
                let Some(device_instance) = *self.rm_device_instance.lock() else {
                    crate::klog_warn!("[nouveau-uapi] EXEC: GPU not attached to the RM yet");
                    return Err(nv::ENODEV);
                };

                // wait_count: prefer same-ctx HW ACQUIRE (GPU stalls on the
                // channel semaphore) over CPU spin. Cross-ctx / non-pending
                // waits still block THIS CALL (CPU-side, 10 s) before submit.
                // Real nouveau puts ACQUIRE in the GPFIFO so the ioctl returns
                // immediately; we do that for same-ctx pending fences and keep
                // a CPU wait for everything else.
                let mut hw_acquires: alloc::vec::Vec<(u64, u32)> = alloc::vec::Vec::new();
                if req.wait_count > 0 {
                    let waits = unsafe {
                        core::slice::from_raw_parts(
                            req.wait_ptr as *const nv::DrmNouveauSync,
                            req.wait_count as usize,
                        )
                    };
                    let handles: Vec<u32> = waits.iter().map(|s| s.handle).collect();
                    let points: Vec<u64> = waits
                        .iter()
                        .map(|s| {
                            let timeline =
                                s.flags & nv::SYNC_TYPE_MASK == nv::SYNC_TIMELINE_SYNCOBJ;
                            if timeline {
                                s.timeline_value
                            } else {
                                1
                            }
                        })
                        .collect();
                    const WAIT_TIMEOUT_US: u64 = 10_000_000; // 10 s (Linux-like; was 1 s / F-M16)
                    let wait_start = unsafe { crate::bus::drivers_timer_now_as_micros() };
                    let deadline_us = wait_start + WAIT_TIMEOUT_US;
                    let wait_ctx = self.ctx_idx_for_pid(owner_pid);
                    let (acquires, cpu_h, cpu_p) =
                        self.partition_exec_waits(&handles, &points, wait_ctx);
                    // Only emit ACQUIRE when the direct-submit path will run;
                    // otherwise reunite into a full CPU wait.
                    let use_hw =
                        !acquires.is_empty() && self.fast_ctx_ready(device_instance, wait_ctx);
                    if use_hw {
                        hw_acquires = acquires;
                    }
                    let (wait_h, wait_p): (Vec<u32>, Vec<u64>) = if use_hw {
                        (cpu_h, cpu_p)
                    } else {
                        (handles.clone(), points.clone())
                    };
                    let outcome = if wait_h.is_empty() {
                        crate::scheme::syncobj::WaitOutcome::Signaled {
                            first_signaled_index: 0,
                        }
                    } else {
                        crate::scheme::syncobj::wait(&wait_h, Some(&wait_p), true, deadline_us)
                    };
                    nv::EXEC_WAIT_US.fetch_add(
                        unsafe { crate::bus::drivers_timer_now_as_micros() }
                            .wrapping_sub(wait_start),
                        Ordering::Relaxed,
                    );
                    match outcome {
                        crate::scheme::syncobj::WaitOutcome::Signaled { .. } => {
                            log::info!(
                                "[nouveau-uapi] EXEC: {} wait(s) ok ({} hw-acquire, {} cpu) -- proceeding to submit",
                                req.wait_count,
                                hw_acquires.len(),
                                wait_h.len()
                            );
                        }
                        crate::scheme::syncobj::WaitOutcome::Timeout => {
                            // klog (not log::warn): the rig boots LOG=error, and
                            // this EIO is one NVK maps straight to
                            // VK_ERROR_DEVICE_LOST -- exactly the kind of client
                            // death that used to leave dmesg spotless.
                            //
                            // Name the fences, not just how many. The generic
                            // stall reporter in `syncobj` only fires past 2 s;
                            // this 10 s deadline does reach it, but the
                            // reporter is budgeted per boot, so always name
                            // the fences here -- "which fence never arrived,
                            // and who was supposed to signal it" is the whole
                            // question on this path.
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC: {} wait syncobj(s) still unsignaled after {}us -- NOT submitting (EIO -> NVK device-lost) pid={} ctx={}:{} (handle:target/current)",
                                req.wait_count,
                                WAIT_TIMEOUT_US,
                                owner_pid,
                                self.ctx_idx_for_pid(owner_pid),
                                crate::scheme::syncobj::describe(&handles, Some(&points))
                            );
                            crate::klog_warn!(
                                "[nouveau-uapi] ctx registry: {}",
                                self.ctx_registry_summary()
                            );
                            return Err(nv::EIO);
                        }
                        crate::scheme::syncobj::WaitOutcome::Invalid => {
                            return Err(nv::ENOENT);
                        }
                    }
                }

                // Submit on THIS process's own channel (context 0 = compositor
                // singleton; >= 1 = a GL client's own GPFIFO), so two processes
                // never push into the same ring.
                let ctx_idx = self.ctx_idx_for_pid(owner_pid);
                // If this client context already timed out an EXEC, its ring is
                // wedged (GPGet frozen) and it will never make progress until
                // the process exits. Fast-fail its submits WITHOUT the
                // gate-holding fence poll: a hung GL client that keeps retrying
                // would otherwise hold the RM gate for the full timeout on every
                // attempt, starving the compositor's own rendering and freezing
                // the desktop/cursor. ctx 0 (the compositor) is never wedged.
                if ctx_idx >= 1 && nv::ctx_is_wedged(ctx_idx) {
                    // Silent until now, and this is the arm a client stays in:
                    // once its ring jams, EVERY later submit dies here, so the
                    // app reports device-lost forever while dmesg says nothing
                    // (the line explaining the ORIGINAL jam scrolled past long
                    // before, or was suppressed as a repeat). One line per ctx
                    // per boot, with the registry census.
                    use core::sync::atomic::{AtomicU32, Ordering};
                    static REPORTED: AtomicU32 = AtomicU32::new(0);
                    if ctx_idx < 32 {
                        let bit = 1u32 << ctx_idx;
                        if REPORTED.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC: pid={} submitting on CTX {}, latched WEDGED by an earlier fence timeout -- every submit fast-fails EIO (NVK: device-lost) until this process exits. {}",
                                owner_pid,
                                ctx_idx,
                                self.ctx_registry_summary()
                            );
                        }
                    }
                    return Err(nv::EIO);
                }
                // A client running on context 0 is running on the COMPOSITOR's
                // ring, in the compositor's VA space -- where its own VM_BIND
                // mappings do not exist. That is what `ensure_ctx_for_pid`
                // falls back to when no slot is free or `ctx_alloc` failed, and
                // it is documented there as "the client falls back to
                // software", which is only true if the client's submits then
                // fail. They may not: they may execute against the wrong VA
                // space. Either way it is worth one line, because from the app
                // it is indistinguishable from every other device-lost.
                if ctx_idx == 0 && !self.ctx0_is_owner(owner_pid) {
                    use core::sync::atomic::{AtomicU32, Ordering};
                    static NO_CTX_REPORTS: AtomicU32 = AtomicU32::new(0);
                    if NO_CTX_REPORTS.fetch_add(1, Ordering::Relaxed) < 8 {
                        crate::klog_warn!(
                            "[nouveau-uapi] EXEC: pid={} has NO context of its own and is about to submit on CTX 0 (the compositor's). {}",
                            owner_pid,
                            self.ctx_registry_summary()
                        );
                    }
                }
                // Direct-submit path (default): GP entries + GPPut + doorbell
                // written by the kernel itself, fence resolved lazily by the
                // syncobj layer. Falls through to the RM per-submit path only
                // when the context could not be prepared (or `nvidia.exec_rm`).
                if self.fast_ctx_ready(device_instance, ctx_idx) {
                    return self.exec_fast(ctx_idx, owner_pid, req, pushes, &hw_acquires);
                }
                nv::EXEC_LEGACY_SUBMITS.fetch_add(1, Ordering::Relaxed);
                if req.sig_count == 0 {
                    // No fence needed -- submit every push plainly, in order.
                    for push in pushes {
                        self.submit_push_plain(device_instance, ctx_idx, push)?;
                    }
                    log::info!(
                        "[nouveau-uapi] EXEC: {} push(es) submitted (no signal)",
                        req.push_count
                    );
                    return Ok(0);
                }

                // sig_count > 0: submit every push but the last plainly, then
                // append the kernel's own tracking fence to the LAST one and
                // poll it (see eclipse_rm_exec_submit_signaled's doc for
                // exactly what a landed fence does and does not prove --
                // PBDMA fetch, not necessarily engine completion). GPFIFO is
                // strictly ordered, so a fence queued after the last push
                // only lands once every earlier push was fetched too -- one
                // fence still honestly covers the whole batch. Only once
                // that's confirmed do we advance the syncobjs -- never
                // before, so a signaled syncobj is never a lie. (Signaling
                // itself is NOT atomic across sig_count > 1: if syncobj i
                // has gone-bad handle, syncobjs before it are already
                // signaled and syncobjs after it never get a chance --
                // same as a single bad handle already behaved before this
                // milestone, just now with more than one to potentially fail.)
                let (last, rest) = pushes
                    .split_last()
                    .expect("push_count > 0 already checked above");
                for push in rest {
                    self.submit_push_plain(device_instance, ctx_idx, push)?;
                }
                const TIMEOUT_MS: u32 = 1000;
                let fence_payload = nv::next_fence_payload();
                nvidia_rm_sys::os_interface::capture_begin();
                let mut signaled = nvidia_rm_sys::rm_init::exec_submit_async(
                    device_instance,
                    ctx_idx,
                    last.va,
                    last.va_len,
                    fence_payload,
                );
                let rm_narration = nvidia_rm_sys::os_interface::capture_take();
                // The RM gate is already released (exec_submit_async submitted
                // and returned in microseconds). Wait for the fence by polling
                // its PHYSICAL address directly -- no gate, no RM lock -- so a
                // slow or hung fence spins only THIS client's own thread (which
                // is preemptible), never the gate the compositor and every other
                // client need to make progress. `fence_sem_phys` is the same
                // sysmem u32 the RM's old inline poll read; phys_to_virt maps it
                // through the kernel's physmap, valid for pinned sysmem.
                if let Ok(r) = signaled.as_mut() {
                    if r.submit_status == 0 && r.fence_submit_status == 0 && r.fence_sem_phys != 0 {
                        let fence_va =
                            crate::bus::phys_to_virt(r.fence_sem_phys as usize) as *const u32;
                        let start = unsafe { crate::bus::drivers_timer_now_as_micros() };
                        let deadline = start.wrapping_add((TIMEOUT_MS as u64) * 1000);
                        let landed = loop {
                            if unsafe { core::ptr::read_volatile(fence_va) } == fence_payload {
                                break true;
                            }
                            if unsafe { crate::bus::drivers_timer_now_as_micros() } >= deadline {
                                break false;
                            }
                            gpu_spin();
                        };
                        nv::EXEC_LEGACY_FENCE_US.fetch_add(
                            unsafe { crate::bus::drivers_timer_now_as_micros() }
                                .wrapping_sub(start),
                            Ordering::Relaxed,
                        );
                        r.fence_value = unsafe { core::ptr::read_volatile(fence_va) };
                        // 0 = landed (NV_OK), 0x65 = timeout -- the same codes the
                        // C inline poll used, so the match arms below are unchanged.
                        r.fence_wait_status = if landed { 0 } else { 0x65 };

                        // GR-engine hang probe: at the exact moment the 1 s poll
                        // expired, snapshot the BAR0 registers that distinguish
                        // the four mutually-exclusive failure modes:
                        //   (a) MMU fault  — latched in 0xb83090 (valid=1)
                        //   (b) GR method stall — 0x400700 non-zero + 0x400704
                        //   (c) PBDMA never fetched the push — GP_GET != GP_PUT
                        //   (d) FECS/GPCCS ctx-switch hang — 0x409c00 / 0x41a000
                        //
                        // No RM calls: two prior attempts to involve the RM on
                        // this path crashed the machine. Pure BAR0 reads only.
                        // Stored in LAST_GR_HANG_PROBE for /proc/gpudbg; first
                        // timeout per boot wins (the ring stays wedged after).
                        if !landed {
                            self.gr_hang_probe(
                                ctx_idx,
                                owner_pid,
                                r.work_token,
                                r.runlist_id,
                                TIMEOUT_MS,
                            );
                        }
                    }
                }
                let replay_rm = |narration: Option<alloc::string::String>| {
                    if let Some(text) = narration {
                        for line in text.lines().filter(|l| !l.trim().is_empty()) {
                            crate::klog_warn!("[nouveau-uapi] rm: {}", line);
                        }
                    }
                };
                match signaled {
                    Ok(r)
                        if r.submit_status == 0
                            && r.fence_submit_status == 0
                            && r.fence_wait_status == 0 =>
                    {
                        let sigs = unsafe {
                            core::slice::from_raw_parts(
                                req.sig_ptr as *const nv::DrmNouveauSync,
                                req.sig_count as usize,
                            )
                        };
                        for sig in sigs {
                            let timeline =
                                sig.flags & nv::SYNC_TYPE_MASK == nv::SYNC_TIMELINE_SYNCOBJ;
                            let target = if timeline { sig.timeline_value } else { 1 };
                            if !crate::scheme::syncobj::timeline_signal(sig.handle, target) {
                                crate::klog_warn!(
                                    "[nouveau-uapi] EXEC: GPU work completed but signaling syncobj handle={} failed (unknown handle)",
                                    sig.handle
                                );
                                return Err(nv::ENOENT);
                            }
                        }
                        // Once per boot, loudly: this line is the first proof
                        // that Mesa-built work reached the GPU and the fence
                        // came back. Every EXEC after it is per-draw traffic,
                        // so it stays at info level.
                        static FIRST_EXEC_OK: AtomicBool = AtomicBool::new(false);
                        if !FIRST_EXEC_OK.swap(true, Ordering::Relaxed) {
                            crate::klog_info!(
                                "[nouveau-uapi] EXEC OK (first): {} push(es) submitted and fence confirmed, {} syncobj(s) signaled -- Mesa work is reaching the GPU",
                                req.push_count, req.sig_count
                            );
                        }
                        // Once per CLIENT context too (the global first above is
                        // always claimed by the compositor): with vkcube/eglgears
                        // "hanging" after device creation, whether a client's
                        // draws ever complete is THE fork in the road -- yes
                        // means the hang is in the Wayland present dance with
                        // labwc, no means it never got a frame through. One
                        // klog line per ctx per boot.
                        static CLIENT_EXEC_OK: core::sync::atomic::AtomicU32 =
                            core::sync::atomic::AtomicU32::new(0);
                        if (1..32).contains(&ctx_idx) {
                            let bit = 1u32 << ctx_idx;
                            if CLIENT_EXEC_OK.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
                                crate::klog_info!(
                                    "[nouveau-uapi] first CLIENT EXEC OK: ctx={} pid={} -- this client's GPU work completes; if it still hangs, look at present/wayland, not the ring",
                                    ctx_idx, owner_pid
                                );
                            }
                        }
                        // Mirror to /proc/gpudbg (readable from the terminal the
                        // client was launched in -- no dmesg needed).
                        if ctx_idx >= 1 {
                            nv::record_client_exec(alloc::format!(
                                "ctx={} pid={} OK: {} push(es) + fence landed, {} syncobj(s) signaled",
                                ctx_idx, owner_pid, req.push_count, req.sig_count
                            ));
                        }
                        log::info!(
                            "[nouveau-uapi] EXEC: {} push(es) submitted and fence confirmed ({} syncobj(s) signaled)",
                            req.push_count, req.sig_count
                        );
                        Ok(0)
                    }
                    Ok(r) => {
                        // A fence-wait timeout (not a fast lookup/map/token
                        // error) means the ring is jammed: the 1 s poll just
                        // elapsed while holding the RM gate. Latch this client
                        // context as wedged so its subsequent submits fast-fail
                        // instead of each starving the compositor for another
                        // second -- that starvation is what froze the desktop
                        // and cursor when a hung GL client kept resubmitting.
                        // ctx 0 (the compositor) is never latched.
                        //
                        // Only a wait that actually ran counts. The RM leaves
                        // every stage it never reached at 0xFFFF_FFFF, and the
                        // poll above only runs once the submit and the fence
                        // doorbell succeeded; a submit refused earlier (a full
                        // GPFIFO's BUSY_RETRY, a lookup failure) also has
                        // `fence_wait_status != 0`, and latching on that turned
                        // one transient full ring into a permanent device-lost
                        // for the client.
                        if ctx_idx >= 1
                            && r.submit_status == 0
                            && r.fence_submit_status == 0
                            && r.fence_wait_status != 0
                        {
                            nv::ctx_set_wedged(ctx_idx);
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC: CTX {} wedged (fence timeout) -- fast-failing \
                                 its submits until it exits so the compositor keeps running",
                                ctx_idx
                            );
                        }
                        let sig = nv::exec_failure_sig(
                            0x03,
                            &[
                                r.lookup_status,
                                r.map_status,
                                r.token_status,
                                r.submit_status,
                                r.fence_submit_status,
                                r.fence_wait_status,
                            ],
                        );
                        // Which stage broke, in one word, for the terminal-side
                        // /proc/gpudbg line. Order matches the submission path.
                        let stage = if r.lookup_status != 0 {
                            "lookup(handles/channel)"
                        } else if r.map_status != 0 {
                            "cpu-map(pushbuf/userd)"
                        } else if r.token_status != 0 {
                            "worksubmit-token"
                        } else if r.submit_status != 0 {
                            "submit(ring-full/BUSY_RETRY)"
                        } else if r.fence_submit_status != 0 {
                            "doorbell(fence-submit)"
                        } else if r.fence_wait_status != 0 {
                            "fence-wait TIMEOUT (push executed but fence never landed -> cold-load/WFI hang or coherency)"
                        } else {
                            "unknown"
                        };
                        if ctx_idx >= 1 {
                            nv::record_client_exec(alloc::format!(
                                "ctx={} pid={} FAILED at {} | lookup={:#x} map={:#x} token={:#x} submit={:#x} fenceSubmit={:#x} fenceWait={:#x} fence={:#x}/exp={:#x} work_token={:#010x} runlist={} | {}",
                                ctx_idx, owner_pid, stage,
                                r.lookup_status, r.map_status, r.token_status, r.submit_status,
                                r.fence_submit_status, r.fence_wait_status, r.fence_value, fence_payload,
                                r.work_token, r.runlist_id,
                                self.ctx_registry_summary()
                            ));
                        }
                        if nv::exec_failure_changed(sig) {
                            replay_rm(rm_narration);
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC (signaled) failed: pid={} ctx={} pushes={} waits={} sigs={} | lookup={:#x} map={:#x} token={:#x} submit={:#x} fenceSubmit={:#x} fenceWait={:#x} (fence value={:#x} expected={:#x}; work_token={:#010x} runlist={}; identical repeats suppressed)",
                                owner_pid, ctx_idx, req.push_count, req.wait_count, req.sig_count,
                                r.lookup_status, r.map_status, r.token_status, r.submit_status,
                                r.fence_submit_status, r.fence_wait_status, r.fence_value, fence_payload,
                                r.work_token, r.runlist_id
                            );
                            crate::klog_warn!(
                                "[nouveau-uapi] ctx registry: {}",
                                self.ctx_registry_summary()
                            );
                        }
                        // If the ring is jammed (BUSY_RETRY with GPGet
                        // frozen), the one artifact that says WHY is the
                        // channel's error notifier: step17 registers it as
                        // `hObjectError`, so the RM writes an NvNotification
                        // there when robust-channel recovery tears the channel
                        // down (MMU fault / PBDMA error / GR exception). Read
                        // the PA cached at CHANNEL_ALLOC through
                        // `crate::bus::phys_to_virt` -- the same window the
                        // framebuffer blit uses. NO RM calls here: both
                        // previous attempts to involve the RM on this path
                        // took the machine down (a kernel page fault from its
                        // transfer mapping, then a >8s spinlock deadlock from
                        // its API lock inside the failure storm). Once per
                        // boot: the notifier doesn't change after the channel
                        // dies.
                        //
                        // Also on a FENCE-WAIT timeout with a submit the RM
                        // accepted: that is the shape of a channel the GPU
                        // killed while running (MMU fault on a client's
                        // buffer, GR exception), where the submit call itself
                        // reported success and only the fence never landed.
                        // Gating solely on BUSY_RETRY left exactly that case
                        // -- the one a GL client hits -- with no notifier.
                        static NOTIFIER_DUMPED: AtomicBool = AtomicBool::new(false);
                        if (r.submit_status == nvidia_rm_sys::types::NV_ERR_BUSY_RETRY
                            || r.fence_wait_status != 0)
                            && !NOTIFIER_DUMPED.swap(true, Ordering::Relaxed)
                        {
                            match nv::chan_notifier_pa_cached() {
                                Some(pa) => {
                                    // NvNotification (nvgputypes.h): u32 ts[2],
                                    // u32 info32, u16 info16, u16 status.
                                    let base = crate::bus::phys_to_virt(pa as usize);
                                    let info32 = unsafe {
                                        core::ptr::read_volatile((base + 8) as *const u32)
                                    };
                                    let info16 = unsafe {
                                        core::ptr::read_volatile((base + 12) as *const u16)
                                    };
                                    let nstatus = unsafe {
                                        core::ptr::read_volatile((base + 14) as *const u16)
                                    };
                                    // Names from the vendored nverror.h.
                                    let rc_name = match info32 {
                                        13 => "GR_ERROR_SW_NOTIFY (GR exception)",
                                        31 => "FIFO_ERROR_MMU_ERR_FLT (MMU fault: GPU touched an unmapped VA)",
                                        32 => "PBDMA_ERROR",
                                        69 => "GR_CLASS_ERROR (method for a class not on the channel)",
                                        _ => "see nverror.h",
                                    };
                                    crate::klog_warn!(
                                        "[nouveau-uapi] chan error notifier @PA {:#x}: status={:#x} info32={:#x} ({}) info16={:#x}",
                                        pa,
                                        nstatus,
                                        info32,
                                        rc_name,
                                        info16
                                    );
                                    // One-shot correlation dump: every mapping
                                    // still live in the VAS, so the Xid line's
                                    // faultAddr (promoted to ERROR by the RM
                                    // print sink) can be checked against what
                                    // was actually mapped. Bounded and once.
                                    let maps = self.nouveau_vm_mappings.lock();
                                    crate::klog_warn!(
                                        "[nouveau-uapi] live VM mappings at first ring-full: {}",
                                        maps.len()
                                    );
                                    for m in maps.iter().take(32) {
                                        crate::klog_warn!(
                                            "[nouveau-uapi]   VA {:#x}..{:#x} handle={}",
                                            m.va,
                                            m.va + m.size,
                                            m.gem_handle
                                        );
                                    }
                                }
                                None => {
                                    crate::klog_warn!(
                                        "[nouveau-uapi] chan error notifier: no PA cached at CHANNEL_ALLOC -- RC dump unavailable"
                                    );
                                }
                            }
                        }
                        Err(nv::EIO)
                    }
                    Err(status) => {
                        let sig = nv::exec_failure_sig(0x04, &[status]);
                        if nv::exec_failure_changed(sig) {
                            replay_rm(rm_narration);
                            crate::klog_warn!(
                                "[nouveau-uapi] EXEC (signaled) failed, NV_STATUS={:#x} (identical repeats suppressed)",
                                status
                            );
                        }
                        Err(nv::EIO)
                    }
                }
            }

            nv::NR_GEM_NEW => {
                let req = unsafe { &mut *(arg as *mut nv::DrmNouveauGemNew) };
                // VRAM or GART -- NVK's `nvkmd_nouveau_alloc_tiled_mem` picks
                // exactly ONE:
                //
                //     if (flags & NVKMD_MEM_GART)      domains |= ..._GART;
                //     else if (flags & NVKMD_MEM_VRAM) domains |= ..._VRAM;
                //
                // so a GART request carries no VRAM bit at all. Refusing those
                // (this arm used to demand DOMAIN_VRAM) made
                // `nouveau_ws_bo_new_tiled` return NULL and vkCreateDevice die
                // with VK_ERROR_OUT_OF_DEVICE_MEMORY -- which only became
                // visible once VM_BIND started working and NVK got far enough
                // to want host memory. If both bits are set we keep GART
                // (CPU-visible sysmem); VRAM-only is real LOCAL.
                let want_vram = req.info.domain & nv::NOUVEAU_GEM_DOMAIN_VRAM != 0;
                let want_gart = req.info.domain & nv::NOUVEAU_GEM_DOMAIN_GART != 0;
                if !want_vram && !want_gart {
                    crate::klog_warn!(
                        "[nouveau-uapi] GEM_NEW: neither VRAM nor GART requested (domain={:#x})",
                        req.info.domain
                    );
                    return Err(nv::EOPNOTSUPP);
                }
                // GEM_NEW does NOT reject a compressed `tile_flags` kind. An
                // earlier revision did (to give NVK a chance to fall back to an
                // uncompressed layout at allocation), but the real cause of
                // GPU-client corruption turned out to be broken explicit-sync
                // (SYNCOBJ_EVENTFD), NOT compression -- so the reject bought
                // nothing and risked failing a LIVE GPU client's allocation
                // (a compositor surface such as lunarbg's wallpaper) that today
                // renders fine mapped-uncompressed. A compressed kind is still
                // mapped uncompressed at VM_BIND (the anti-hang fallback); the
                // real fix for a genuinely-compressed surface is comptag/PLC
                // support. `record_gem_new` still notes the kind for
                // /proc/gpudbg.
                // Honour VRAM-only as true LOCAL (`NV01_MEMORY_LOCAL_USER`).
                // GART, or GART|VRAM (NVK DEVICE_LOCAL|HOST_VISIBLE maps to
                // that), stays sysmem so gem_map_cpu can publish a host PA.
                // VRAM-only is DEVICE_LOCAL without HOST_VISIBLE: map_handle=0,
                // never publish the FBMEM offset as a CPU address (that hole
                // let userspace paint into low kernel RAM). Present/scanout
                // stays on CREATE_DUMB sysmem; no BAR1 path is required here.
                let sysmem = want_gart || !want_vram;
                if req.info.size == 0 || req.info.size > u32::MAX as u64 {
                    return Err(nv::EINVAL);
                }
                // Quotas before gem_alloc: one BO, per-pid live, and global live.
                if req.info.size > GEM_NEW_MAX_SINGLE {
                    crate::klog_warn!(
                        "[nouveau-uapi] GEM_NEW: size={} exceeds single-alloc cap {} MiB (pid={})",
                        req.info.size,
                        GEM_NEW_MAX_SINGLE / (1024 * 1024),
                        owner_pid
                    );
                    return Err(nv::ENOMEM);
                }
                let vram_bytes = (self.effective_vram_mb() as u64).saturating_mul(1024 * 1024);
                let global_cap = if vram_bytes == 0 {
                    GEM_NEW_MAX_GLOBAL
                } else {
                    GEM_NEW_MAX_GLOBAL.min(vram_bytes.saturating_mul(2))
                };
                {
                    let gem = self.nouveau_gem.lock();
                    let pid_live: u64 = gem
                        .iter()
                        .filter(|o| o.owner_pid == owner_pid)
                        .map(|o| o.size)
                        .sum();
                    if pid_live.saturating_add(req.info.size) > GEM_NEW_MAX_PER_PID {
                        crate::klog_warn!(
                            "[nouveau-uapi] GEM_NEW: per-pid quota hit (pid={} live={} + {} > {} GiB)",
                            owner_pid,
                            pid_live,
                            req.info.size,
                            GEM_NEW_MAX_PER_PID / (1024 * 1024 * 1024)
                        );
                        return Err(nv::ENOMEM);
                    }
                    let global_live = NOUVEAU_GEM_BYTES.load(Ordering::Relaxed);
                    if global_live.saturating_add(req.info.size) > global_cap {
                        crate::klog_warn!(
                            "[nouveau-uapi] GEM_NEW: global quota hit (live={} + {} > cap={} MiB, vram={} MiB)",
                            global_live,
                            req.info.size,
                            global_cap / (1024 * 1024),
                            self.effective_vram_mb()
                        );
                        return Err(nv::ENOMEM);
                    }
                }
                let Some(device_instance) = *self.rm_device_instance.lock() else {
                    crate::klog_warn!("[nouveau-uapi] GEM_NEW: GPU not attached to the RM yet");
                    return Err(nv::ENODEV);
                };
                // Reserved BEFORE the RM allocation so an exhausted slice
                // costs nothing to unwind. Burning one id on a later failure
                // is free: the slice holds 32Mi of them.
                let Some(handle) = self.next_gem_handle() else {
                    crate::klog_warn!(
                        "[nouveau-uapi] GEM_NEW: this GPU's GEM handle slice is exhausted -- \
                         refusing rather than handing out another card's handle"
                    );
                    return Err(nv::ENOMEM);
                };
                let alloc =
                    match nvidia_rm_sys::rm_init::gem_alloc(device_instance, req.info.size, sysmem)
                    {
                        Ok(a) if a.alloc_status == 0 => a,
                        Ok(a) => {
                            crate::klog_warn!(
                            "[nouveau-uapi] GEM_NEW: RM alloc failed ({}), size={} status={:#x}",
                            if sysmem { "sysmem/GART" } else { "vidmem/VRAM" },
                            req.info.size,
                            a.alloc_status
                        );
                            return Err(nv::ENOMEM);
                        }
                        Err(status) => {
                            crate::klog_warn!(
                                "[nouveau-uapi] GEM_NEW: gem_alloc ({}) failed, NV_STATUS={:#x}",
                                if sysmem { "sysmem/GART" } else { "vidmem/VRAM" },
                                status
                            );
                            return Err(nv::ENOMEM);
                        }
                    };
                if !sysmem {
                    static FIRST_VRAM_GEM: AtomicBool = AtomicBool::new(false);
                    if !FIRST_VRAM_GEM.swap(true, Ordering::Relaxed) {
                        crate::klog_info!(
                            "[nouveau-uapi] GEM_NEW: first VRAM-only LOCAL alloc size={} hMemory={:#010x}",
                            req.info.size,
                            alloc.h_memory
                        );
                    }
                }
                // Real host physical address for sysmem/GART objects. VRAM-only
                // (ADDR_FBMEM) is not CPU-mmap-able: gem_map_cpu refuses FBMEM
                // because memdescGetPhysAddr(AT_CPU) is a VRAM offset, not BAR1.
                // Skipping the lookup avoids a NOT_SUPPORTED trace per LOCAL BO.
                let phys_addr = if sysmem {
                    match nvidia_rm_sys::rm_init::gem_map_cpu(device_instance, alloc.h_memory) {
                        Ok(m)
                            if m.lookup_status == 0
                                && m.address_space == nvidia_rm_sys::rm_init::ADDR_SYSMEM =>
                        {
                            Some(m.phys_addr)
                        }
                        Ok(m) => {
                            crate::klog_warn!(
                            "[nouveau-uapi] GEM_NEW handle={}: gem_map_cpu lookup_status={:#x} address_space={} -- not CPU-mmap-able",
                            handle, m.lookup_status, m.address_space
                        );
                            None
                        }
                        Err(status) => {
                            crate::klog_warn!(
                            "[nouveau-uapi] GEM_NEW handle={}: gem_map_cpu failed, NV_STATUS={:#x} -- not CPU-mmap-able",
                            handle, status
                        );
                            None
                        }
                    }
                } else {
                    None
                };
                // VRAM FBMEM offset (AT_GPU) for future CE/scanout; never
                // published as a host PA (see gem_map_cpu FBMEM refusal).
                // TODO: if AT_GPU lookup fails on some boards, this stays
                // None and the VRAM GEM remains usable for VM_BIND/EXEC
                // without scanout-by-offset.
                let vram_offset = if !sysmem {
                    nvidia_rm_sys::rm_init::gem_fbmem_offset(device_instance, alloc.h_memory).ok()
                } else {
                    None
                };
                let map_handle = if let Some(pa) = phys_addr {
                    crate::scheme::gem_mmap::register(handle, pa, req.info.size, owner_pid);
                    (handle as u64) << 12
                } else {
                    0
                };
                let used_domain = if sysmem {
                    nv::NOUVEAU_GEM_DOMAIN_GART
                } else {
                    nv::NOUVEAU_GEM_DOMAIN_VRAM
                };
                self.nouveau_gem.lock().push(nv::NouveauGemObject {
                    handle,
                    h_memory: alloc.h_memory,
                    owner_pid,
                    size: req.info.size,
                    phys_addr,
                    vram_offset,
                    domain: used_domain,
                    // Remember what was asked for so GEM_INFO round-trips it.
                    // The allocation itself is linear; a non-zero PTE kind is
                    // programmed (or refused) at VM_BIND time, which is where
                    // the new uAPI carries it.
                    tile_mode: req.info.tile_mode,
                    tile_flags: req.info.tile_flags,
                });
                NOUVEAU_GEM_BYTES.fetch_add(req.info.size, Ordering::Relaxed);
                // Feed the /proc/gpudbg memory summary. `req.info.domain` is
                // still the client's REQUEST here (overwritten to the domain
                // actually used a few lines below); phys_addr.is_some() means a
                // CPU-mmap-able host PA resolved for it.
                super::nouveau_uapi::record_gem_new(
                    req.info.domain,
                    phys_addr.is_some(),
                    req.info.tile_flags,
                    !sysmem,
                );
                req.info.handle = handle;
                // Report the domain actually used, not the one requested:
                // mesa reads this back to decide where the object lives.
                req.info.domain = used_domain;
                // Unbound until VM_BIND MAPs it -- GPU VA and the CPU mmap
                // offset above are independent in real nouveau too.
                req.info.offset = 0;
                req.info.map_handle = map_handle;
                log::info!(
                    "[nouveau-uapi] GEM_NEW handle={} size={} domain={} -> RM hMemory={:#010x} phys_addr={:?} map_handle={:#x}",
                    handle,
                    req.info.size,
                    if sysmem { "GART" } else { "VRAM" },
                    alloc.h_memory,
                    phys_addr,
                    map_handle
                );
                Ok(0)
            }

            nv::NR_GEM_INFO => {
                let req = unsafe { &mut *(arg as *mut nv::DrmNouveauGemInfo) };
                // Scoped and dropped before touching nouveau_vm_mappings
                // below -- same discipline VM_BIND's own MAP op follows,
                // so the two locks are never held nested in either order.
                let (size, phys_addr, obj_tile_mode, obj_tile_flags, obj_domain) = {
                    let gem = self.nouveau_gem.lock();
                    let Some(obj) = gem
                        .iter()
                        .find(|o| o.handle == req.handle && gem_usable_by(o, owner_pid))
                    else {
                        // [dmabuf-diag] error!-visible: NVK runs GEM_INFO on the
                        // handle it got back from PRIME import. If that handle is
                        // not a nouveau GEM object (a dma-buf that came back as a
                        // GENERIC handle), this ENOENTs and NVK's dma-buf import
                        // dies -> zink "couldn't allocate memory". Cross-check the
                        // handle here against the "[drm] PRIME import ... handle="
                        // line to see whether the self-import matched.
                        crate::klog_err!(
                            "[nouveau-uapi] GEM_INFO handle={:#x} -> ENOENT (not in nouveau_gem; \
                             imported-as-generic dma-buf? NVK will fail its image import)",
                            req.handle
                        );
                        return Err(nv::ENOENT);
                    };
                    (
                        obj.size,
                        obj.phys_addr,
                        obj.tile_mode,
                        obj.tile_flags,
                        obj.domain,
                    )
                };
                // GPU VA, if VM_BIND has mapped this object -- bookkeeping
                // independent from nouveau_gem, same as VM_BIND itself.
                // Linux looks the VMA up in the CALLER's vmm
                // (`nouveau_vma_find(nvbo, cli->vmm)`): each client has its
                // own VA space, so a PRIME importer that has not bound the
                // object gets 0, never the creator's VA, which means
                // nothing in the importer's context. The kernel (pid 0)
                // has no VA space of its own and sees the first binding.
                let offset = self
                    .nouveau_vm_mappings
                    .lock()
                    .iter()
                    .find(|m| {
                        m.gem_handle == req.handle && (owner_pid == 0 || m.owner_pid == owner_pid)
                    })
                    .map(|m| m.va)
                    .unwrap_or(0);
                let map_handle = phys_addr.map(|_| (req.handle as u64) << 12).unwrap_or(0);
                log::debug!(
                    "[nouveau-uapi] GEM_INFO handle={} -> size={} offset={:#x} map_handle={:#x}",
                    req.handle,
                    size,
                    offset,
                    map_handle
                );
                // Honour the backing: GART objects are HOST_VISIBLE sysmem,
                // VRAM-only objects are DEVICE_LOCAL without a CPU map.
                req.domain = obj_domain;
                req.size = size;
                req.offset = offset;
                req.map_handle = map_handle;
                // Echo back what GEM_NEW was asked for, not a hardcoded 0:
                // mesa reads these to recover a BO's layout.
                req.tile_mode = obj_tile_mode;
                req.tile_flags = obj_tile_flags;
                Ok(0)
            }

            nv::NR_GEM_CPU_PREP => {
                let req = unsafe { &*(arg as *const nv::DrmNouveauGemCpuPrep) };
                let known = self
                    .nouveau_gem
                    .lock()
                    .iter()
                    .any(|o| o.handle == req.handle && gem_usable_by(o, owner_pid));
                if !known {
                    return Err(nv::ENOENT);
                }
                // Linux waits for the BO's reservation fences (up to 30 s;
                // `NOWAIT` -> EBUSY if busy). There is no per-BO fence here,
                // so wait for everything this process queued on its channel
                // instead -- a superset of the BO's fences. EXEC is
                // asynchronous on the direct-submit path, so returning
                // immediately (as this did) let Mesa's `nouveau_ws_bo_wait`
                // read a buffer the GPU was still writing.
                const NOUVEAU_GEM_CPU_PREP_NOWAIT: u32 = 0x2;
                self.cpu_prep_wait(owner_pid, req.flags & NOUVEAU_GEM_CPU_PREP_NOWAIT != 0)
            }

            nv::NR_GEM_CPU_FINI => {
                let req = unsafe { &*(arg as *const nv::DrmNouveauGemCpuFini) };
                let gem = self.nouveau_gem.lock();
                if gem
                    .iter()
                    .any(|o| o.handle == req.handle && gem_usable_by(o, owner_pid))
                {
                    Ok(0)
                } else {
                    Err(nv::ENOENT)
                }
            }

            nv::NR_GEM_PUSHBUF => {
                // The submission path of the classic **nvc0 Gallium** driver
                // (Mesa OpenGL for Turing) -- the one the shipped image uses.
                // Real submission is a hardware-validated follow-up (it needs
                // GART-domain GEM, the 3D class bound to the channel, and
                // relocation handling), so this milestone PARSES and LOGS the
                // whole request, then honestly returns EOPNOTSUPP. The dump is
                // the anatomy needed to build real submission: how many buffers
                // (and their domains), relocs, and pushes Mesa hands us per call.
                let pb = unsafe { &*(arg as *const nv::DrmNouveauGemPushbuf) };
                log::warn!(
                    "[nouveau-uapi] GEM_PUSHBUF: channel={} nr_buffers={} nr_relocs={} \
                     nr_push={} suffix0={:#x} suffix1={:#x} vram_avail={:#x} gart_avail={:#x}",
                    pb.channel,
                    pb.nr_buffers,
                    pb.nr_relocs,
                    pb.nr_push,
                    pb.suffix0,
                    pb.suffix1,
                    pb.vram_available,
                    pb.gart_available
                );
                // Log a bounded prefix of each array so the trace shows the shape
                // without flooding: which BOs (handle + domains) and which pushes
                // (BO + offset/len) make up this submission. Bound reads too --
                // never walk an unbounded user-supplied count.
                const DUMP_MAX: usize = 8;
                let nb = (pb.nr_buffers as usize).min(DUMP_MAX);
                if pb.buffers != 0 && nb > 0 {
                    let bos = unsafe {
                        core::slice::from_raw_parts(
                            pb.buffers as *const nv::DrmNouveauGemPushbufBo,
                            nb,
                        )
                    };
                    for (i, bo) in bos.iter().enumerate() {
                        log::warn!(
                            "[nouveau-uapi] GEM_PUSHBUF bo[{}/{}]: handle={} read_dom={:#x} \
                             write_dom={:#x} valid_dom={:#x} presumed(valid={} dom={:#x} off={:#x})",
                            i,
                            pb.nr_buffers,
                            bo.handle,
                            bo.read_domains,
                            bo.write_domains,
                            bo.valid_domains,
                            bo.presumed.valid,
                            bo.presumed.domain,
                            bo.presumed.offset
                        );
                    }
                }
                let np = (pb.nr_push as usize).min(DUMP_MAX);
                if pb.push != 0 && np > 0 {
                    let pushes = unsafe {
                        core::slice::from_raw_parts(
                            pb.push as *const nv::DrmNouveauGemPushbufPush,
                            np,
                        )
                    };
                    for (i, p) in pushes.iter().enumerate() {
                        log::warn!(
                            "[nouveau-uapi] GEM_PUSHBUF push[{}/{}]: bo_index={} offset={:#x} length={:#x}",
                            i,
                            pb.nr_push,
                            p.bo_index,
                            p.offset,
                            p.length
                        );
                    }
                }
                log::warn!(
                    "[nouveau-uapi] GEM_PUSHBUF: not submitted (real submission needs \
                     GART GEM + 3D class + relocs -- follow-up); returning EOPNOTSUPP"
                );
                Err(nv::EOPNOTSUPP)
            }

            nv::NR_NVIF => self.nouveau_nvif(arg, size as usize, owner_pid),

            // GET_ZCULL_INFO: mesa 26.x probes it and TOLERATES failure
            // (`has_zcull_info` just stays false), so ENOSYS is a correct,
            // honest answer -- named here only so the trace does not read as
            // an unknown ioctl.
            nv::NR_GET_ZCULL_INFO => Err(nv::ENOSYS),

            _ => {
                // warn, not debug: same reasoning as GETPARAM's unknown-param
                // arm above -- a real client hitting an ioctl this milestone
                // never implemented at all must be visible at the default
                // boot log level, not silently ENOSYS.
                let name = nv::nouveau_ioctl_name(nr);
                log::warn!(
                    "[nouveau-uapi] unhandled {} request={:#010x} (nr={:#04x} size={}) \
                     -- returning ENOSYS",
                    name,
                    request,
                    nr,
                    size
                );
                Err(nv::ENOSYS)
            }
        }
    }
}

#[allow(dead_code)]
pub struct NvidiaGpuDriverPci;

impl PciDriver for NvidiaGpuDriverPci {
    fn name(&self) -> &str {
        "Nvidia GPU"
    }

    fn matched(&self, vendor_id: u16, _device_id: u16) -> bool {
        vendor_id == 0x10DE
    }

    fn matched_dev(&self, dev: &PCIDevice) -> bool {
        dev.id.vendor_id == 0x10DE && dev.id.class == 0x03
    }

    fn init(
        &self,
        dev: &PCIDevice,
        mapper: &Option<Arc<dyn IoMapper>>,
        _irq: Option<usize>,
    ) -> DeviceResult<Device> {
        #[cfg(target_arch = "x86_64")]
        use crate::bus::pci::{read_bar_addr, PortOpsImpl, PCI_ACCESS};
        use crate::bus::phys_to_virt;
        #[cfg(target_arch = "x86_64")]
        const BAR0: u16 = 0x10;

        // Turing's real BAR0 register aperture is 16 MiB (0x0-0xFFFFFF);
        // used as a fallback only when the PCI-enumerated BAR length is
        // unavailable (e.g. the direct config-space re-read fallback
        // path below has no length to report). Do NOT re-probe BAR sizes
        // here (see the "do not probe BAR size at boot" note below) --
        // `dev.bars[0]`'s length already comes from the bus's own
        // one-time enumeration, same as every other driver's BAR1+
        // handling (e1000e, ixgbe, virtio_pci) already reads directly.
        const NVIDIA_BAR0_APERTURE_FALLBACK: u64 = 16 * 1024 * 1024;

        #[cfg(target_arch = "x86_64")]
        let (bar0_addr, bar0_map_len) = {
            if let Some(BAR::Memory(a, len, _, _)) = dev.bars[0] {
                if a != 0 {
                    (
                        a,
                        if len == 0 {
                            NVIDIA_BAR0_APERTURE_FALLBACK
                        } else {
                            len as u64
                        },
                    )
                } else {
                    let ops = &PortOpsImpl;
                    (
                        unsafe { read_bar_addr(ops, PCI_ACCESS, dev.loc, BAR0) },
                        NVIDIA_BAR0_APERTURE_FALLBACK,
                    )
                }
            } else {
                let ops = &PortOpsImpl;
                (
                    unsafe { read_bar_addr(ops, PCI_ACCESS, dev.loc, BAR0) },
                    NVIDIA_BAR0_APERTURE_FALLBACK,
                )
            }
        };
        #[cfg(not(target_arch = "x86_64"))]
        let (bar0_addr, bar0_map_len) = if let Some(BAR::Memory(a, len, _, _)) = dev.bars[0] {
            (
                a,
                if len == 0 {
                    NVIDIA_BAR0_APERTURE_FALLBACK
                } else {
                    len as u64
                },
            )
        } else {
            (0, NVIDIA_BAR0_APERTURE_FALLBACK)
        };

        if bar0_addr == 0 {
            return Err(DeviceError::NoResources);
        }

        // Wire up nvidia-rm-sys's KernelHooks facade so any real vendored
        // NVIDIA C file that reaches through os-interface.h for PCI config
        // space, MMIO mappings, port I/O, or timing gets Eclipse's actual
        // hardware primitives instead of the crate's safe-default stubs.
        super::nvidia_hooks::install(mapper);

        if let Some(m) = mapper {
            m.query_or_map(bar0_addr as usize, bar0_map_len as usize);
        }
        let bar0_vaddr = phys_to_virt(bar0_addr as usize);

        // Compact the six raw PCI BAR slots into the ordered list of populated
        // *memory* BARs, exactly as NVIDIA's own nv-pci.c does (it walks the PCI
        // resources and assigns each valid memory BAR to nv->bars[j++]). A
        // 64-bit BAR occupies one slot here and leaves the next `None`, so this
        // walk yields the same logical ordering NVIDIA uses:
        //   index 0 = REGS (16 MiB registers), 1 = FB (VRAM window),
        //   index 2 = IMEM (the ~32 MiB instance-memory aperture).
        // Do NOT probe BAR sizes here (writing 0xFFFFFFFF to a BAR register can
        // wedge config space on some GPUs and hang the machine); the lengths
        // already came from the bus's one-time enumeration.
        let mem_bars: Vec<(u64, u64)> = (0..6usize)
            .filter_map(|i| {
                if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[i] {
                    if addr != 0 {
                        return Some((addr, len as u64));
                    }
                }
                None
            })
            .collect();

        // FB is the second memory BAR (index 1); fall back to a size-based
        // search for the first >= 16 MiB aperture past REGS if the ordering is
        // unexpected, matching the previous behaviour.
        let fb_bar = mem_bars
            .get(1)
            .map(|&(addr, len)| (addr, if len == 0 { 256 * 1024 * 1024 } else { len }))
            .filter(|&(_, len)| len >= (16 * 1024 * 1024))
            .or_else(|| {
                mem_bars.iter().skip(1).find_map(|&(addr, len)| {
                    let actual_len = if len == 0 { 256 * 1024 * 1024 } else { len };
                    (actual_len >= (16 * 1024 * 1024)).then_some((addr, actual_len))
                })
            });

        // IMEM/BAR2 is the third memory BAR (index 2). RM needs its physical
        // base+size as GPUATTACHARG.instPhysAddr/instLength for the BAR2 MMU
        // self-test in gpuStateInit; 0/0 if the GPU somehow exposes fewer than
        // three memory BARs (then the BAR2 test will still fail, but attach and
        // the earlier steps stay intact).
        let (imem_phys, imem_len) = mem_bars
            .get(2)
            .map(|&(addr, len)| (addr, if len == 0 { 32 * 1024 * 1024 } else { len }))
            .unwrap_or((0, 0));

        if let Some((fb_addr, fb_len)) = fb_bar {
            if let Some(m) = mapper {
                m.query_or_map(fb_addr as usize, fb_len as usize);
            }
            let fb_vaddr = phys_to_virt(fb_addr as usize);

            let gpu_name = alloc::format!(
                "nvidia-gpu-{}:{}.{}",
                dev.loc.bus,
                dev.loc.device,
                dev.loc.function
            );
            log::warn!(
                "[NVIDIA] GPU at {} bar0={:#x} fb={:#x} fb_len={:#x} imem={:#x} imem_len={:#x}",
                gpu_name,
                bar0_addr,
                fb_addr,
                fb_len,
                imem_phys,
                imem_len
            );
            let gpu = Arc::new(NvidiaGpu::new(
                gpu_name,
                dev.id.device_id,
                bar0_vaddr,
                fb_vaddr,
                fb_len as usize,
                fb_addr,
                1920,
                1080,
                bar0_addr,
                bar0_map_len,
                imem_phys,
                imem_len,
                0, // PCI domain: Eclipse only tracks bus/device/function, single-segment system
                dev.loc.bus,
                dev.loc.device,
            )?);
            gpu.set_msi_vector(_irq);
            NVIDIA_GPUS.lock().push(gpu.clone());
            crate::scheme::syncobj::set_fence_timeout_hook(nouveau_fence_timeout_hook);
            Ok(Device::DrmDisplay(gpu.clone(), gpu))
        } else {
            Err(DeviceError::NoResources)
        }
    }
}

/// Tests for the pure decode/validation helpers of the NVIDIA driver: the two
/// independent ways this file identifies a GPU (the PCI device-id table and
/// NV_PMC_BOOT_0), the `access_ok()` guards on the nouveau uAPI, and the
/// ELD the HDMI audio path derives from the boot EDID. Everything here runs on
/// the host with no GPU: the driver's hardware paths are unreachable from a
/// unit test, but these helpers decide what the hardware paths are *told*, and
/// each one has a documented way of being wrong.
#[cfg(test)]
mod decode_tests {
    use super::*;

    /// A NV_PMC_BOOT_0 word carrying `chip_id` in its 9-bit field.
    fn boot0(chip_id: u32) -> u32 {
        chip_id << regs::PMC_BOOT0_CHIP_ID_SHIFT
    }

    /// The box this is developed against: two RTX 2060 Super. Both of the
    /// device ids that part ships under must land on Turing with its real
    /// 8 GiB, or `nouveau_engine_classes` refuses and the client loses every
    /// Vulkan GPU (see `nouveau_arch`).
    #[test]
    fn the_rtx_2060_super_is_recognised_under_every_device_id_it_ships_as() {
        for id in [0x1F02u16, 0x1F06, 0x1F07] {
            let (arch, name, vram) = identify_gpu(id);
            assert_eq!(arch, NvidiaArchitecture::Turing, "device id {:#06x}", id);
            assert_eq!(name, "GeForce RTX 2060 Super");
            assert_eq!(vram, 8192, "8 GiB, in MiB");
        }
        // The plain 2060 shares the 0x1F0x block and must NOT be conflated
        // with the Super: different VRAM size.
        assert_eq!(identify_gpu(0x1F03).2, 6144);
    }

    /// `Unknown` is the table's "I have no idea" arm, and a VRAM size of 0 is
    /// what the probe treats as "ask the hardware". They must agree: a named
    /// part with 0 MiB, or an unknown part claiming a size, would both be
    /// read as facts downstream.
    #[test]
    fn a_table_entry_is_either_fully_known_or_fully_unknown() {
        for id in 0x0000u16..=0xFFFF {
            let (arch, name, vram) = identify_gpu(id);
            if arch == NvidiaArchitecture::Unknown {
                assert_eq!(vram, 0, "unknown part {:#06x} claims {} MiB", id, vram);
                assert_eq!(name, "Unknown NVIDIA GPU", "device id {:#06x}", id);
            } else {
                assert!(vram > 0, "{} ({:#06x}) reports no VRAM", name, id);
                assert_ne!(name, "Unknown NVIDIA GPU", "device id {:#06x}", id);
            }
        }
    }

    /// The RTX 2060 Super is a TU106, chip id 0x166 — dead centre of the
    /// Turing range, and the only decode that matters on this hardware.
    #[test]
    fn a_turing_boot0_decodes_to_turing() {
        assert_eq!(
            arch_from_pmc_boot0(boot0(0x166)),
            NvidiaArchitecture::Turing
        );
        // The revision nibble and every other low bit are not part of the id.
        assert_eq!(
            arch_from_pmc_boot0(boot0(0x166) | 0x000f_ffff),
            NvidiaArchitecture::Turing
        );
    }

    /// The regression this guards. `nouveau_chipset_id` carries, per
    /// architecture, the chip id it reports when NV_PMC_BOOT_0 is unreadable,
    /// each annotated with the real part (0x162 TU102 ... 0x1b2 GB202). Those
    /// ids and the range table are two statements about the same numbering, so
    /// feeding each id back through the decoder has to return the
    /// architecture it stands for. It did not: Hopper's range was 0x1B0..=0x1BF
    /// — which is where consumer Blackwell actually lives — so a GB202 decoded
    /// as Hopper and got Hopper's DMA-copy classes instead of `BLACKWELL_B`,
    /// while GH100's own 0x180 fell in the gap and decoded as Unknown.
    #[test]
    fn every_representative_chip_id_decodes_to_its_own_architecture() {
        for (chip_id, arch) in [
            (0x162u32, NvidiaArchitecture::Turing),   // TU102
            (0x166, NvidiaArchitecture::Turing),      // TU106
            (0x172, NvidiaArchitecture::Ampere),      // GA102
            (0x192, NvidiaArchitecture::AdaLovelace), // AD102
            (0x180, NvidiaArchitecture::Hopper),      // GH100
            (0x1b2, NvidiaArchitecture::Blackwell),   // GB202
        ] {
            assert_eq!(
                arch_from_pmc_boot0(boot0(chip_id)),
                arch,
                "chip id {:#x}",
                chip_id
            );
        }
    }

    /// Why the old `BLACKWELL_MIN = 0x200` could never fire: nouveau reads the
    /// chip id as `(boot0 & 0x1ff00000) >> 20`, and `nouveau_chipset_id` does
    /// the same, so the largest id any part can report is 0x1FF. A lower bound
    /// of 0x200 made the Blackwell arm dead code. Assert the bound stays
    /// inside the field the hardware actually has.
    #[test]
    fn no_architecture_bound_sits_outside_the_nine_bit_chip_id_field() {
        const CHIP_ID_MAX: u32 = 0x1FF; // 9 bits, as nouveau reads it
        for bound in [
            regs::PMC_BOOT0_CHIPID_TURING_MIN,
            regs::PMC_BOOT0_CHIPID_TURING_MAX,
            regs::PMC_BOOT0_CHIPID_AMPERE_MIN,
            regs::PMC_BOOT0_CHIPID_AMPERE_MAX,
            regs::PMC_BOOT0_CHIPID_ADA_MIN,
            regs::PMC_BOOT0_CHIPID_ADA_MAX,
            regs::PMC_BOOT0_CHIPID_HOPPER_MIN,
            regs::PMC_BOOT0_CHIPID_HOPPER_MAX,
            regs::PMC_BOOT0_CHIPID_BLACKWELL_MIN,
        ] {
            assert!(bound <= CHIP_ID_MAX, "bound {:#x} is unreachable", bound);
        }
    }

    /// The ranges must not overlap, or the `else if` chain silently decides by
    /// source order rather than by the numbering.
    #[test]
    fn the_architecture_ranges_do_not_overlap() {
        let mut seen: alloc::vec::Vec<(u32, NvidiaArchitecture)> = alloc::vec::Vec::new();
        for chip_id in 0..=0x1FFu32 {
            let arch = arch_from_pmc_boot0(boot0(chip_id));
            if arch != NvidiaArchitecture::Unknown {
                seen.push((chip_id, arch));
            }
        }
        // Each known id belongs to exactly one architecture, and the
        // architectures form contiguous blocks in increasing order.
        let mut blocks: alloc::vec::Vec<NvidiaArchitecture> = alloc::vec::Vec::new();
        for (_, arch) in &seen {
            if blocks.last() != Some(arch) {
                assert!(
                    !blocks.contains(arch),
                    "{:?} appears in two separate blocks",
                    arch
                );
                blocks.push(*arch);
            }
        }
        assert_eq!(
            blocks,
            alloc::vec![
                NvidiaArchitecture::Turing,
                NvidiaArchitecture::Ampere,
                NvidiaArchitecture::Hopper,
                NvidiaArchitecture::AdaLovelace,
                NvidiaArchitecture::Blackwell,
            ]
        );
    }

    /// `user_slice_ok` is the `access_ok()` on the arrays a nouveau ioctl
    /// dereferences directly (`op_ptr`, `push_ptr`, `wait_ptr`, `sig_ptr`).
    /// The render node is 0666, so anything it lets through is a read at an
    /// address an unprivileged caller chose.
    #[test]
    fn user_slice_ok_rejects_what_a_render_node_caller_must_not_reach() {
        // A plain user array is fine.
        assert!(user_slice_ok::<u64>(0x1000, 8));
        // Zero elements need no pointer at all — the ioctl arms skip the read.
        assert!(user_slice_ok::<u64>(0, 0));
        // ... but a non-empty array at NULL is not a slice.
        assert!(!user_slice_ok::<u64>(0, 1));
        // A kernel-half address: the kernel is mapped in every address space,
        // so this used to resolve and turn the copy into an arbitrary read.
        assert!(!user_slice_ok::<u64>(0xffff_8000_0000_0000, 1));
        // The canonical split itself is the first address that is not user.
        const USER_MAX: u64 = 0x0000_8000_0000_0000;
        assert!(user_slice_ok::<u8>(USER_MAX - 1, 1));
        assert!(!user_slice_ok::<u8>(USER_MAX, 1));
        // A range that starts low and ENDS above the split must be refused as
        // a whole, not clipped.
        assert!(!user_slice_ok::<u8>(USER_MAX - 4, 8));
        // A huge element count must fail closed rather than wrap: the byte
        // length is `count * size_of::<T>()`, and `checked_mul` catches the
        // cases a 32-bit host would wrap on while the range check catches the
        // rest. Either way the answer has to be "no".
        assert!(!user_slice_ok::<[u8; 0x1_0000]>(0x1000, u32::MAX));
    }

    /// The regression this guards. `page_flip`'s copy-engine path passed
    /// `fb.width * 4` and `fb.height` straight to the CE without ever comparing
    /// them against the mode, unlike `scanout_region`, which has always clipped
    /// with `.min(info.width)` / `.min(info.height)`.
    #[test]
    fn a_hardware_flip_is_clipped_to_the_mode_it_is_flipping_into() {
        // A client framebuffer wider and taller than the mode: clipped to the
        // mode on both axes, not passed through.
        assert_eq!(
            hwflip_geometry(2560, 1440, 2560 * 4, 1920, 1080, 1920 * 4),
            Some((1920 * 4, 1080))
        );
        // The ordinary case is untouched: fb exactly the mode.
        assert_eq!(
            hwflip_geometry(1920, 1080, 1920 * 4, 1920, 1080, 1920 * 4),
            Some((1920 * 4, 1080))
        );
        // A padded destination pitch is fine -- the row is narrower than the
        // stride, which is exactly what a pitched 2D copy is for. This is the
        // documented dual-RTX case: client pitch 5504, GOP pitch 8192.
        assert_eq!(
            hwflip_geometry(1376, 1080, 5504, 1376, 1080, 8192),
            Some((1376 * 4, 1080))
        );
        // A framebuffer SMALLER than the mode copies only what it has; the rest
        // of the screen is not this flip's business.
        assert_eq!(
            hwflip_geometry(800, 600, 800 * 4, 1920, 1080, 1920 * 4),
            Some((800 * 4, 600))
        );
    }

    /// Refusing is the safe answer, and it matters more than it looks: a row
    /// wider than the destination pitch makes the RM reject the copy with
    /// `NV_ERR_INVALID_ARGUMENT`, and `CE_PRESENT_WEDGED` then latches for the
    /// whole boot -- every later present silently degrades to the CPU blit. A
    /// `None` here costs one frame's fast path instead.
    #[test]
    fn a_flip_that_cannot_be_expressed_as_a_pitched_copy_is_refused() {
        // Row wider than the destination stride: would shear each row into the
        // next inside the scanout framebuffer.
        assert_eq!(
            hwflip_geometry(1920, 1080, 1920 * 4, 1920, 1080, 1024),
            None
        );
        // Row wider than the SOURCE stride: would read the next row's pixels as
        // this row's tail.
        assert_eq!(
            hwflip_geometry(1920, 1080, 1024, 1920, 1080, 1920 * 4),
            None
        );
        // Degenerate geometry on either side.
        assert_eq!(
            hwflip_geometry(0, 1080, 1920 * 4, 1920, 1080, 1920 * 4),
            None
        );
        assert_eq!(
            hwflip_geometry(1920, 0, 1920 * 4, 1920, 1080, 1920 * 4),
            None
        );
        assert_eq!(hwflip_geometry(1920, 1080, 0, 1920, 1080, 1920 * 4), None);
        assert_eq!(hwflip_geometry(1920, 1080, 1920 * 4, 1920, 1080, 0), None);
        assert_eq!(
            hwflip_geometry(1920, 1080, 1920 * 4, 0, 1080, 1920 * 4),
            None
        );
        assert_eq!(
            hwflip_geometry(1920, 1080, 1920 * 4, 1920, 0, 1920 * 4),
            None
        );
        // A width whose byte count would overflow must not wrap into a small
        // row that then passes the stride checks.
        assert_eq!(
            hwflip_geometry(u32::MAX, 1, u32::MAX, u32::MAX, 1, u32::MAX),
            None
        );
    }

    /// `elapsed: N ns` is how `/proc/gpubench` reports a launch, and 0 is the
    /// "no measurement" answer the caller expects for anything unparseable.
    #[test]
    fn gpubench_elapsed_is_parsed_or_reported_as_zero() {
        assert_eq!(parse_gpubench_elapsed_ns("elapsed: 1234 ns"), 1234);
        // The interesting line is not the first one.
        assert_eq!(
            parse_gpubench_elapsed_ns("saxpy ok\ngrid: 2176 threads\nelapsed: 98765 ns\n"),
            98765
        );
        // No such field, an empty report, and a field with no digits all mean
        // "nothing measured" rather than a made-up number.
        assert_eq!(parse_gpubench_elapsed_ns("saxpy ok\n"), 0);
        assert_eq!(parse_gpubench_elapsed_ns(""), 0);
        assert_eq!(parse_gpubench_elapsed_ns("elapsed: unknown"), 0);
        // The unit suffix is not part of the number.
        assert_eq!(parse_gpubench_elapsed_ns("elapsed: 42ns"), 42);
    }

    /// Connector types cross into DRM's own numbering, which userspace reads
    /// back from GETCONNECTOR. Two things must hold: an unmapped NVIDIA type
    /// comes out as DRM `Unknown` (0) rather than as whatever NVIDIA's number
    /// happens to be, and the numeric table and the name table agree on which
    /// types they know -- they are maintained by hand, side by side, and a
    /// connector named in one but not the other is a display Eclipse either
    /// mislabels in `/proc/gpuedid` or hands userspace as type 0.
    #[test]
    fn the_connector_type_tables_agree_on_what_they_know() {
        assert_eq!(nv_conn_type_to_drm(0xDEAD_BEEF), 0);
        assert_eq!(nv_conn_type_name(0xDEAD_BEEF), "other");

        for t in 0u32..=0xFFFF {
            let drm = nv_conn_type_to_drm(t);
            let named = nv_conn_type_name(t) != "other";
            assert_eq!(
                drm != 0,
                named,
                "NVIDIA connector type {:#x}: drm={} name={:?}",
                t,
                drm,
                nv_conn_type_name(t)
            );
        }
        // Moebius's monitors hang off DisplayPort and HDMI; those two are the
        // ones that must not regress.
        assert_eq!(
            nv_conn_type_to_drm(0x46),
            10,
            "DRM_MODE_CONNECTOR_DisplayPort"
        );
        assert_eq!(nv_conn_type_to_drm(0x61), 11, "DRM_MODE_CONNECTOR_HDMIA");
    }

    /// The ELD the HDMI audio path builds when the bootloader kept only the
    /// 128-byte base EDID. The baseline length in byte 2 is counted in
    /// dwords from byte 4, so it has to cover every byte actually written —
    /// the monitor name and the SAD included — or the codec stops reading
    /// before the sample rates.
    #[test]
    fn the_eld_baseline_length_covers_the_bytes_written_into_it() {
        // An EDID with no monitor-name descriptor: MNL 0, SAD at byte 20.
        let mut edid = [0u8; 128];
        edid[8..12].copy_from_slice(&[0x04, 0x21, 0x37, 0x13]); // manufacturer + product
        let eld = build_eld_from_base_edid(&edid, 0x1234_5678, false);
        assert_eq!(eld[4], 0, "no name descriptor, so MNL is 0");
        let baseline_end = 4 + eld[2] as usize * 4;
        assert!(
            baseline_end >= 20 + 3,
            "baseline ({} bytes) cuts off the SAD at 20..23",
            baseline_end - 4
        );
        assert_eq!(&eld[20..23], &[0x09, 0x07, 0x07], "2ch LPCM SAD");
        assert_eq!(eld[5] >> 4, 1, "exactly one SAD");
        assert_eq!(eld[5] & (1 << 2), 0, "HDMI, not DisplayPort");
        // The EDID vendor/product bytes are forwarded verbatim.
        assert_eq!(&eld[16..20], &edid[8..12]);
        // The display id is stored little-endian across bytes 8..12.
        assert_eq!(
            u32::from_le_bytes([eld[8], eld[9], eld[10], eld[11]]),
            0x1234_5678
        );

        // Now with a 0xFC monitor-name descriptor: MNL 13, so the SAD moves to
        // byte 33 and the baseline length has to grow with it.
        let mut named = [0u8; 128];
        named[54..58].copy_from_slice(&[0, 0, 0, 0xFC]);
        named[59..72].copy_from_slice(b"Eclipse Disp\0");
        let eld = build_eld_from_base_edid(&named, 0, true);
        assert_eq!(eld[4], 13, "a 0xFC descriptor gives a 13-byte name");
        assert_eq!(&eld[20..33], b"Eclipse Disp\0");
        assert_eq!(&eld[33..36], &[0x09, 0x07, 0x07], "the SAD moved past it");
        let baseline_end = 4 + eld[2] as usize * 4;
        assert!(
            baseline_end >= 36,
            "baseline ({} bytes) cuts off the SAD at 33..36",
            baseline_end - 4
        );
        assert_ne!(eld[5] & (1 << 2), 0, "DisplayPort was requested");

        // A short EDID is not parsed at all: an all-zero ELD, never a read
        // past the end of the buffer.
        assert_eq!(build_eld_from_base_edid(&[0u8; 127], 0, false), [0u8; 96]);
    }
}

#[cfg(test)]
impl NvidiaGpu {
    /// A GPU with no PCI device behind it: BAR0 reads as zero (so the chip
    /// id falls back to `architecture`), no RM instance, no boot
    /// framebuffer. That is the state every nouveau ioctl sees on a GPU the
    /// RM never attached, and it is enough for every arm that is
    /// bookkeeping rather than hardware.
    pub(super) fn for_test(device_id: u16, vram_size_mb: u32) -> Self {
        // Zeros, 16 MiB of them: the GR hang probe reads BAR0 up to
        // 0x00bb_0090. `vec![0; n]` is `alloc_zeroed`, so the pages that are
        // never touched cost nothing.
        let bar0 = alloc::boxed::Box::leak(alloc::vec![0u8; 16 << 20].into_boxed_slice()).as_ptr()
            as usize;
        let gem_handle_slice = crate::scheme::gem_mmap::alloc_handle_slice();
        // The id table decides the architecture as it does for real; an id
        // it does not know stands in for a Turing board it cannot size.
        let (architecture, gpu_model) = match identify_gpu(device_id) {
            (NvidiaArchitecture::Unknown, _, _) => (NvidiaArchitecture::Turing, "test"),
            (arch, model, _) => (arch, model),
        };
        Self {
            name: String::from("nvidia-test"),
            info: DisplayInfo {
                width: 0,
                height: 0,
                pitch: 0,
                format: ColorFormat::ARGB8888,
                fb_base_vaddr: 0,
                fb_size: 256 << 20,
            },
            architecture,
            gpu_model,
            device_id,
            vram_size_mb,
            pitch_override: None,
            _bar0: bar0,
            _bar1: 0,
            bar1_phys: 0,
            bar0_phys: 0,
            bar0_len: 0,
            bar2_phys: 0,
            bar2_len: 0,
            pci_domain: 0,
            pci_bus: 0,
            pci_device: 0,
            vram_allocator: Mutex::new(None),
            bringup: Mutex::new(None),
            rm_attach_result: Mutex::new(None),
            rm_device_instance: Mutex::new(None),
            rm_display_snap: Mutex::new(None),
            auto_bringup_done: AtomicBool::new(false),
            gsp_firmware: Mutex::new(None),
            gsp_fw_status: Mutex::new(None),
            gsp_init_result: Mutex::new(None),
            state_init_result: Mutex::new(None),
            step10_result: Mutex::new(None),
            imported_handles: Mutex::new(Vec::new()),
            nouveau_channels: Mutex::new(Vec::new()),
            nouveau_gem: Mutex::new(Vec::new()),
            nouveau_gem_next_handle: AtomicU32::new(gem_handle_slice.base()),
            nouveau_gem_handle_end: gem_handle_slice.end(),
            nouveau_vm_mappings: Mutex::new(Vec::new()),
            nouveau_pid_ctx: Mutex::new(Vec::new()),
            nouveau_fast: Mutex::new(
                (0..super::nouveau_uapi::MAX_CTX)
                    .map(|_| FastSlot::Unprepared)
                    .collect(),
            ),
            nouveau_peer_fence: Mutex::new(alloc::collections::BTreeMap::new()),
            kms_framebuffers: Mutex::new(Vec::new()),
            next_kms_fb_id: AtomicU32::new(1),
            kms_state: Mutex::new(NvidiaKmsState {
                crtc_fb: 0,
                plane_fb: 0,
                last_vblank_us: 0,
            }),
            msi_vector: AtomicUsize::new(usize::MAX),
            ctx0_owner: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
mod nouveau_bookkeeping_tests {
    //! The nouveau-uAPI arms that are bookkeeping rather than hardware, on
    //! a GPU the RM never attached: the state every ioctl sees before
    //! `/proc/gpustep14`, and the only state a host test can reach. Every
    //! test takes `LOCK`: the uAPI switch, the live-bytes counter and the
    //! `gem_mmap` registry are process globals.

    extern crate std;

    use super::super::nouveau_uapi as nv;
    use super::*;
    use core::mem::size_of;
    use lock::Mutex as TestMutex;

    static LOCK: TestMutex<()> = TestMutex::new(());

    const A: u64 = 66_001;
    const B: u64 = 66_002;
    const STRANGER: u64 = 66_003;
    const MIB: u64 = 1024 * 1024;

    const IOC_WRITE: u32 = 1;
    const IOC_READ: u32 = 2;
    const DRM_IOCTL_TYPE: u32 = 0x64;

    fn ioc(dir: u32, ty: u32, nr: u32, size: usize) -> u32 {
        (dir << 30) | ((size as u32) << 16) | (ty << 8) | nr
    }

    fn wr<T>(nr: u32) -> u32 {
        ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_TYPE, nr, size_of::<T>())
    }

    fn gpu() -> NvidiaGpu {
        nv::set_enabled(true);
        NvidiaGpu::for_test(0x1f06, 8192)
    }

    fn call<T>(gpu: &NvidiaGpu, request: u32, req: &mut T, pid: u64) -> Result<usize, i32> {
        gpu.nouveau_ioctl(request, req as *mut T as usize, pid)
    }

    fn getparam(gpu: &NvidiaGpu, param: u64) -> Result<u64, i32> {
        let mut r = nv::DrmNouveauGetparam { param, value: 0 };
        call(
            gpu,
            wr::<nv::DrmNouveauGetparam>(nv::NR_GETPARAM),
            &mut r,
            A,
        )
        .map(|_| r.value)
    }

    fn gem_new(gpu: &NvidiaGpu, size: u64, domain: u32, pid: u64) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauGemNew {
            info: nv::DrmNouveauGemInfo {
                handle: 0,
                domain,
                size,
                offset: 0,
                map_handle: 0,
                tile_mode: 0,
                tile_flags: 0,
            },
            channel_hint: 0,
            align: 0,
        };
        call(gpu, wr::<nv::DrmNouveauGemNew>(nv::NR_GEM_NEW), &mut r, pid)
    }

    fn gem_info(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<nv::DrmNouveauGemInfo, i32> {
        let mut r = nv::DrmNouveauGemInfo {
            handle,
            domain: 0,
            size: 0,
            offset: 0,
            map_handle: 0,
            tile_mode: 0,
            tile_flags: 0,
        };
        call(
            gpu,
            wr::<nv::DrmNouveauGemInfo>(nv::NR_GEM_INFO),
            &mut r,
            pid,
        )
        .map(|_| r)
    }

    fn cpu_fini(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauGemCpuFini { handle };
        call(
            gpu,
            wr::<nv::DrmNouveauGemCpuFini>(nv::NR_GEM_CPU_FINI),
            &mut r,
            pid,
        )
    }

    fn cpu_prep_nowait(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauGemCpuPrep { handle, flags: 0x2 };
        call(
            gpu,
            wr::<nv::DrmNouveauGemCpuPrep>(nv::NR_GEM_CPU_PREP),
            &mut r,
            pid,
        )
    }

    fn channel_alloc(gpu: &NvidiaGpu, pid: u64) -> Result<nv::DrmNouveauChannelAlloc, i32> {
        let mut r = nv::DrmNouveauChannelAlloc {
            fb_ctxdma_handle: 0,
            tt_ctxdma_handle: 0,
            channel: -1,
            pushbuf_domains: 0,
            notifier_handle: 0xffff_ffff,
            subchan: [nv::DrmNouveauChannelAllocSubchan {
                handle: 0,
                grclass: 0,
            }; 8],
            nr_subchan: 7,
        };
        call(
            gpu,
            wr::<nv::DrmNouveauChannelAlloc>(nv::NR_CHANNEL_ALLOC),
            &mut r,
            pid,
        )
        .map(|_| r)
    }

    fn channel_free(gpu: &NvidiaGpu, channel: i32, pid: u64) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauChannelFree { channel };
        call(
            gpu,
            wr::<nv::DrmNouveauChannelFree>(nv::NR_CHANNEL_FREE),
            &mut r,
            pid,
        )
    }

    fn vm_bind(gpu: &NvidiaGpu, pid: u64) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauVmBind {
            op_count: 1,
            flags: 0,
            wait_count: 0,
            sig_count: 0,
            wait_ptr: 0,
            sig_ptr: 0,
            op_ptr: 0x1000,
        };
        call(gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, pid)
    }

    /// Restores the global live-bytes counter, panic or not, so one test's
    /// objects never count against another's quota.
    struct LiveBytes(u64);

    impl LiveBytes {
        fn hold() -> Self {
            Self(NOUVEAU_GEM_BYTES.load(Ordering::Relaxed))
        }
        fn delta(&self) -> i64 {
            NOUVEAU_GEM_BYTES.load(Ordering::Relaxed) as i64 - self.0 as i64
        }
    }

    impl Drop for LiveBytes {
        fn drop(&mut self) {
            NOUVEAU_GEM_BYTES.store(self.0, Ordering::Relaxed);
        }
    }

    /// A GEM object as `GEM_NEW` would have left it: in the table, counted,
    /// and registered with `gem_mmap` when it has a CPU mapping.
    fn object(gpu: &NvidiaGpu, owner: u64, size: u64, phys: Option<u64>) -> u32 {
        let handle = gpu.next_gem_handle().unwrap();
        gpu.nouveau_gem.lock().push(nv::NouveauGemObject {
            handle,
            h_memory: 0xcafe_0000 | (handle & 0xffff),
            owner_pid: owner,
            size,
            phys_addr: phys,
            vram_offset: None,
            domain: nv::NOUVEAU_GEM_DOMAIN_GART,
            tile_mode: 0x10,
            tile_flags: 0x600,
        });
        NOUVEAU_GEM_BYTES.fetch_add(size, Ordering::Relaxed);
        if let Some(pa) = phys {
            crate::scheme::gem_mmap::register(handle, pa, size, owner);
        }
        handle
    }

    /// A VRAM-domain object with no CPU aperture, like a tiled render
    /// target NVK allocates.
    fn vram_object(gpu: &NvidiaGpu, owner: u64, size: u64) -> u32 {
        let handle = object(gpu, owner, size, None);
        let mut gem = gpu.nouveau_gem.lock();
        let obj = gem.iter_mut().find(|o| o.handle == handle).unwrap();
        obj.domain = nv::NOUVEAU_GEM_DOMAIN_VRAM;
        obj.vram_offset = Some(0x100_0000 * u64::from(handle & 0xff));
        handle
    }

    fn mapping(gpu: &NvidiaGpu, owner: u64, gem_handle: u32, va: u64, size: u64) {
        gpu.nouveau_vm_mappings.lock().push(nv::NouveauVmMapping {
            gem_handle,
            h_virt: 0xbeef,
            owner_pid: owner,
            va,
            size,
            bo_offset: 0,
        });
    }

    fn framebuffer(gpu: &NvidiaGpu, handle_id: u32) -> u32 {
        let id = gpu.next_kms_fb_id.fetch_add(1, Ordering::Relaxed);
        gpu.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
            id,
            handle_id,
            width: 64,
            height: 64,
            pitch: 256,
            phys_addr: 0,
            size: 16384,
            h_memory: 0,
            vram_offset: None,
        });
        id
    }

    fn has_object(gpu: &NvidiaGpu, handle: u32) -> bool {
        gpu.nouveau_gem.lock().iter().any(|o| o.handle == handle)
    }

    fn owner_of(gpu: &NvidiaGpu, handle: u32) -> Option<u64> {
        gpu.nouveau_gem
            .lock()
            .iter()
            .find(|o| o.handle == handle)
            .map(|o| o.owner_pid)
    }

    fn mappings_of(gpu: &NvidiaGpu, handle: u32) -> usize {
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.gem_handle == handle)
            .count()
    }

    fn channels_of(gpu: &NvidiaGpu, pid: u64) -> usize {
        gpu.nouveau_channels
            .lock()
            .iter()
            .filter(|c| c.owner_pid == pid)
            .count()
    }

    #[test]
    fn the_dispatch_gates_on_the_switch_the_type_and_the_payload_floor() {
        let _g = LOCK.lock();
        let gpu = gpu();
        nv::set_enabled(false);
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_VENDOR),
            Err(nv::ENOSYS),
            "off by default: byte for byte the old NvidiaGpu::ioctl"
        );
        nv::set_enabled(true);
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_VENDOR), Ok(0x10de));
        let mut r = nv::DrmNouveauGetparam {
            param: nv::NOUVEAU_GETPARAM_PCI_VENDOR,
            value: 0,
        };
        // Not the DRM ioctl type at all.
        assert_eq!(
            call(
                &gpu,
                ioc(IOC_READ | IOC_WRITE, 0x63, nv::NR_GETPARAM, 16),
                &mut r,
                A
            ),
            Err(nv::ENOSYS)
        );
        // A caller whose struct is shorter than the one the arm writes.
        assert_eq!(
            call(
                &gpu,
                ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_TYPE, nv::NR_GETPARAM, 8),
                &mut r,
                A
            ),
            Err(nv::EINVAL)
        );
        // The direction bits are advisory: Linux dispatches by NR alone.
        assert_eq!(
            call(
                &gpu,
                ioc(IOC_WRITE, DRM_IOCTL_TYPE, nv::NR_GETPARAM, 16),
                &mut r,
                A
            ),
            Ok(0)
        );
        assert_eq!(r.value, 0x10de);
        // An NR nouveau never published.
        assert_eq!(
            call(
                &gpu,
                ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_TYPE, 0x40 + 0x50, 16),
                &mut r,
                A
            ),
            Err(nv::ENOSYS)
        );
    }

    #[test]
    fn getparam_enumerates_the_gpu_without_the_rm() {
        let _g = LOCK.lock();
        let gpu = gpu();
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_VENDOR), Ok(0x10de));
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_PCI_DEVICE), Ok(0x1f06));
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_BUS_TYPE), Ok(2));
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_FB_SIZE), Ok(8192 * MIB));
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_BAR_SIZE),
            Ok(256 * MIB),
            "the BAR1 aperture, not the board's VRAM"
        );
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_AGP_SIZE), Ok(0));
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_CHIPSET_ID),
            Ok(0x162),
            "BAR0 reads as zero, so the architecture's flagship chip stands in"
        );
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_HAS_VMA_TILEMODE), Ok(1));
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_HAS_PAGEFLIP), Ok(0));
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_EXEC_PUSH_MAX), Ok(64));
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_GRAPH_UNITS),
            Ok(6 | (36 << 8)),
            "no RM: the full TU102 die, gpc in the low byte, tpc above"
        );
        assert_eq!(getparam(&gpu, 99), Err(nv::EINVAL));
        // A GPU the id table does not know still reports VRAM: the
        // architecture floor, never zero (NVK would skip it).
        let unknown = NvidiaGpu::for_test(0x1fff, 0);
        assert_eq!(
            getparam(&unknown, nv::NOUVEAU_GETPARAM_FB_SIZE),
            Ok(4096 * MIB)
        );
    }

    #[test]
    fn gem_new_is_refused_for_its_own_reason_before_it_needs_the_rm() {
        let _g = LOCK.lock();
        let live = LiveBytes::hold();
        let gpu = gpu();
        let gart = nv::NOUVEAU_GEM_DOMAIN_GART;
        assert_eq!(gem_new(&gpu, 4096, 0, A), Err(nv::EOPNOTSUPP));
        assert_eq!(gem_new(&gpu, 0, gart, A), Err(nv::EINVAL));
        assert_eq!(gem_new(&gpu, u32::MAX as u64 + 1, gart, A), Err(nv::EINVAL));
        assert_eq!(
            gem_new(&gpu, GEM_NEW_MAX_SINGLE + 1, gart, A),
            Err(nv::ENOMEM),
            "single-allocation cap"
        );
        assert_eq!(gem_new(&gpu, GEM_NEW_MAX_SINGLE, gart, A), Err(nv::ENODEV));
        // Per-pid quota: what A already holds plus this request.
        object(&gpu, A, GEM_NEW_MAX_PER_PID - 4096, None);
        assert_eq!(gem_new(&gpu, 8192, gart, A), Err(nv::ENOMEM));
        assert_eq!(gem_new(&gpu, 4096, gart, A), Err(nv::ENODEV));
        assert_eq!(
            gem_new(&gpu, 8192, gart, B),
            Err(nv::ENODEV),
            "B's quota is B's"
        );
        // Global quota: min(4 GiB, twice the VRAM), across every pid.
        object(&gpu, B, GEM_NEW_MAX_PER_PID, None);
        assert_eq!(gem_new(&gpu, 8192, gart, STRANGER), Err(nv::ENOMEM));
        assert_eq!(gem_new(&gpu, 4096, gart, STRANGER), Err(nv::ENODEV));
        // Twice a 1 GiB board is the tighter cap.
        let small = NvidiaGpu::for_test(0x1f06, 1024);
        NOUVEAU_GEM_BYTES.store(2048 * MIB - 4096, Ordering::Relaxed);
        assert_eq!(gem_new(&small, 8192, gart, STRANGER), Err(nv::ENOMEM));
        assert_eq!(gem_new(&small, 4096, gart, STRANGER), Err(nv::ENODEV));
        // A request the RM never saw burns no handle.
        let next = gpu.nouveau_gem_next_handle.load(Ordering::Relaxed);
        assert_eq!(gem_new(&gpu, 4096, gart, STRANGER), Err(nv::ENODEV));
        assert_eq!(gpu.nouveau_gem_next_handle.load(Ordering::Relaxed), next);
        drop(live);
    }

    #[test]
    fn a_gem_object_is_seen_by_its_creator_a_prime_holder_and_the_kernel() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu();
        let h1 = object(&gpu, A, 65536, Some(0x1000_0000));
        let h2 = object(&gpu, A, 4096, None);
        mapping(&gpu, A, h1, 0x4000_0000, 65536);

        let info = gem_info(&gpu, h1, A).unwrap();
        assert_eq!(info.size, 65536);
        assert_eq!(info.domain, nv::NOUVEAU_GEM_DOMAIN_GART);
        assert_eq!(info.offset, 0x4000_0000, "the GPU VA it is bound at");
        assert_eq!(info.map_handle, (h1 as u64) << 12);
        assert_eq!((info.tile_mode, info.tile_flags), (0x10, 0x600));
        assert_eq!(
            gem_info(&gpu, h1, B).map(|i| i.size),
            Err(nv::ENOENT),
            "not B's, not shared"
        );
        assert!(crate::scheme::gem_mmap::add_ref(h1, B).is_some());
        assert_eq!(
            gem_info(&gpu, h1, B).map(|i| i.size),
            Ok(65536),
            "a PRIME holder"
        );
        assert_eq!(
            gem_info(&gpu, h1, STRANGER).map(|i| i.size),
            Err(nv::ENOENT)
        );
        assert_eq!(
            gem_info(&gpu, h1, 0).map(|i| i.size),
            Ok(65536),
            "the kernel"
        );
        // Never CPU-mappable: nothing to import, so only the creator.
        let info = gem_info(&gpu, h2, A).unwrap();
        assert_eq!((info.offset, info.map_handle), (0, 0));
        assert!(crate::scheme::gem_mmap::add_ref(h2, B).is_none());
        assert_eq!(gem_info(&gpu, h2, B).map(|i| i.size), Err(nv::ENOENT));
        assert_eq!(
            gem_info(&gpu, h2, 0).map(|i| i.size),
            Ok(4096),
            "the kernel sees it without a holder entry"
        );
        assert_eq!(
            gem_info(&gpu, h2 + 1000, A).map(|i| i.size),
            Err(nv::ENOENT)
        );
        // CPU_PREP / CPU_FINI apply the same rule.
        assert_eq!(cpu_fini(&gpu, h1, A), Ok(0));
        assert_eq!(cpu_fini(&gpu, h1, B), Ok(0));
        assert_eq!(cpu_fini(&gpu, h1, STRANGER), Err(nv::ENOENT));
        assert_eq!(cpu_fini(&gpu, h2, B), Err(nv::ENOENT));
        assert_eq!(
            cpu_prep_nowait(&gpu, h1, A),
            Ok(0),
            "nothing queued: nothing to wait"
        );
        assert_eq!(cpu_prep_nowait(&gpu, h1, STRANGER), Err(nv::ENOENT));
        assert!(crate::scheme::gem_mmap::unregister(h1));
    }

    #[test]
    fn gem_close_frees_on_the_last_holder_and_takes_its_mappings_and_framebuffers() {
        let _g = LOCK.lock();
        let live = LiveBytes::hold();
        let gpu = gpu();
        let h1 = object(&gpu, A, 65536, Some(0x2000_0000));
        let h2 = object(&gpu, A, 4096, None);
        let h3 = object(&gpu, B, 4096, Some(0x3000_0000));
        crate::scheme::gem_mmap::add_ref(h1, B).unwrap();
        mapping(&gpu, A, h1, 0x1_0000, 65536);
        mapping(&gpu, B, h1, 0x2_0000, 65536);
        mapping(&gpu, B, h3, 0x3_0000, 4096);
        let fb = framebuffer(&gpu, h1);
        let fb3 = framebuffer(&gpu, h3);
        gpu.kms_state.lock().crtc_fb = fb;
        let counted = live.delta();

        assert!(!gpu.nouveau_gem_close(h1, STRANGER), "not a holder");
        assert!(has_object(&gpu, h1));
        assert!(gpu.nouveau_gem_close(h1, B), "one holder letting go");
        assert!(has_object(&gpu, h1), "A still holds it");
        assert_eq!(mappings_of(&gpu, h1), 2);
        assert_eq!(live.delta(), counted);
        assert!(gpu.nouveau_gem_close(h1, A), "the last holder");
        assert!(!has_object(&gpu, h1));
        assert_eq!(
            mappings_of(&gpu, h1),
            0,
            "its VM_BIND mappings went with it"
        );
        assert_eq!(mappings_of(&gpu, h3), 1, "another object's did not");
        let fbs: Vec<u32> = gpu.kms_framebuffers.lock().iter().map(|f| f.id).collect();
        assert_eq!(
            fbs,
            [fb3],
            "the fb built on it is gone, the other one stays"
        );
        assert_eq!(
            gpu.kms_state.lock().crtc_fb,
            0,
            "and is no longer being scanned out"
        );
        assert_eq!(live.delta(), counted - 65536);
        assert!(!gpu.nouveau_gem_close(h1, A), "gone is gone");
        // Never exported: its creator alone, and nobody else can even see it.
        assert!(!gpu.nouveau_gem_close(h2, B));
        assert!(gpu.nouveau_gem_close(h2, A));
        assert_eq!(live.delta(), counted - 65536 - 4096);
        assert!(
            gpu.nouveau_gem_close(h3, 0),
            "the kernel closes on anyone's behalf"
        );
        assert!(!gpu.nouveau_gem_close(h3 + 1000, A));
    }

    #[test]
    fn the_gem_handle_slice_is_never_overrun() {
        let _g = LOCK.lock();
        let gpu = gpu();
        let end = gpu.nouveau_gem_handle_end;
        gpu.nouveau_gem_next_handle
            .store(end - 2, Ordering::Relaxed);
        assert_eq!(gpu.next_gem_handle(), Some(end - 2));
        assert_eq!(gpu.next_gem_handle(), Some(end - 1));
        assert_eq!(gpu.next_gem_handle(), None, "the next id is another card's");
        assert_eq!(gpu.next_gem_handle(), None);
        assert_eq!(gpu.nouveau_gem_next_handle.load(Ordering::Relaxed), end);
    }

    #[test]
    fn channels_without_the_rm_are_discovery_only_and_free_is_owner_scoped() {
        let _g = LOCK.lock();
        let gpu = gpu();
        let c = channel_alloc(&gpu, A).unwrap();
        assert_eq!(c.channel, 0);
        assert_eq!(c.notifier_handle, 0, "no RM notifier");
        assert_eq!(c.pushbuf_domains, nv::NOUVEAU_GEM_DOMAIN_VRAM);
        assert_eq!(c.nr_subchan, 0);
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 1);
        assert_eq!(channel_alloc(&gpu, B).unwrap().channel, 2);
        assert!(!gpu.nouveau_rm_vas_ready());
        assert_eq!(
            vm_bind(&gpu, A),
            Err(nv::ENODEV),
            "no VA space was ever built"
        );
        assert_eq!(channel_free(&gpu, 0, B), Err(nv::EINVAL), "not B's channel");
        assert_eq!(channel_free(&gpu, 7, A), Err(nv::EINVAL));
        assert_eq!(channel_free(&gpu, 0, A), Ok(0));
        assert_eq!(channels_of(&gpu, A), 1);
        assert_eq!(
            channel_alloc(&gpu, B).unwrap().channel,
            0,
            "the lowest free id"
        );
        assert_eq!(channel_free(&gpu, 2, 0), Ok(0), "the kernel frees anyone's");
        assert_eq!(channels_of(&gpu, B), 1);
        while gpu.nouveau_channels.lock().len() < nv::MAX_CHANNELS {
            channel_alloc(&gpu, STRANGER).unwrap();
        }
        assert_eq!(channel_alloc(&gpu, A).map(|c| c.channel), Err(nv::EBUSY));
        assert_eq!(channel_free(&gpu, 1, A), Ok(0));
        assert!(channel_alloc(&gpu, A).is_ok());
    }

    #[test]
    fn process_exit_reclaims_only_the_exiting_pids_channels_objects_and_mappings() {
        let _g = LOCK.lock();
        let live = LiveBytes::hold();
        let gpu = gpu();
        channel_alloc(&gpu, A).unwrap();
        channel_alloc(&gpu, A).unwrap();
        channel_alloc(&gpu, B).unwrap();
        let h1 = object(&gpu, A, 4096, None);
        let h2 = object(&gpu, A, 65536, Some(0x2000_0000));
        let h3 = object(&gpu, A, 8192, Some(0x3000_0000));
        crate::scheme::gem_mmap::add_ref(h3, B).unwrap();
        let h4 = object(&gpu, B, 4096, Some(0x4000_0000));
        let h5 = object(&gpu, B, 4096, None);
        let h6 = object(&gpu, STRANGER, 4096, Some(0x6000_0000));
        crate::scheme::gem_mmap::add_ref(h6, B).unwrap();
        let kernels = object(&gpu, 0, 4096, None);
        mapping(&gpu, A, h1, 0x1_0000, 4096);
        mapping(&gpu, A, h2, 0x2_0000, 65536);
        mapping(&gpu, B, h3, 0x3_0000, 8192);
        let fb = framebuffer(&gpu, h2);
        gpu.kms_state.lock().plane_fb = fb;
        let counted = live.delta();

        gpu.nouveau_release_process(0);
        assert_eq!(channels_of(&gpu, A), 2, "pid 0 is nobody");
        assert!(has_object(&gpu, kernels), "and owns nothing to reclaim");

        gpu.nouveau_release_process(A);
        assert_eq!(channels_of(&gpu, A), 0);
        assert_eq!(channels_of(&gpu, B), 1);
        assert!(!has_object(&gpu, h1), "never exported: freed");
        assert!(!has_object(&gpu, h2), "exported, A the only holder: freed");
        assert!(crate::scheme::gem_mmap::lookup(h2).is_none());
        assert_eq!(
            owner_of(&gpu, h3),
            Some(0),
            "B still imports it: orphaned, not freed"
        );
        assert!(crate::scheme::gem_mmap::holds(h3, B));
        assert_eq!(owner_of(&gpu, h4), Some(B));
        assert!(has_object(&gpu, h5), "B's unexported object is B's");
        assert_eq!(mappings_of(&gpu, h1) + mappings_of(&gpu, h2), 0);
        assert_eq!(
            mappings_of(&gpu, h3),
            1,
            "B's mapping of the shared object stays"
        );
        assert!(gpu.kms_framebuffers.lock().is_empty());
        assert_eq!(gpu.kms_state.lock().plane_fb, 0);
        assert_eq!(
            live.delta(),
            counted - 4096 - 65536,
            "the orphan is still live"
        );
        // The orphan now belongs to B alone: B's own GEM_CLOSE frees it,
        // owner or not (the last holder is who Linux frees for).
        assert!(gpu.nouveau_gem_close(h3, B));
        assert!(!has_object(&gpu, h3));
        assert_eq!(mappings_of(&gpu, h3), 0);
        assert_eq!(live.delta(), counted - 4096 - 65536 - 8192);
        // B's exit: its own objects go, its import of a LIVE owner's
        // buffer is let go without detaching that owner.
        gpu.nouveau_release_process(B);
        assert!(!has_object(&gpu, h4));
        assert!(!has_object(&gpu, h5));
        assert_eq!(owner_of(&gpu, h6), Some(STRANGER), "still the owner's");
        assert!(!crate::scheme::gem_mmap::holds(h6, B));
        assert!(crate::scheme::gem_mmap::holds(h6, STRANGER));
        assert!(gpu.nouveau_channels.lock().is_empty());
        gpu.nouveau_release_process(STRANGER);
        assert!(!has_object(&gpu, h6));
        assert!(
            has_object(&gpu, kernels),
            "nobody's exit reclaims the kernel's"
        );
        gpu.nouveau_gem.lock().retain(|o| o.handle != kernels);
        NOUVEAU_GEM_BYTES.fetch_sub(4096, Ordering::Relaxed);
        assert_eq!(live.delta(), 0);
    }

    #[test]
    fn drain_vm_mappings_takes_exactly_what_matches_and_keeps_the_rest_in_order() {
        let _g = LOCK.lock();
        let gpu = gpu();
        mapping(&gpu, A, 1, 0x1000, 4096);
        mapping(&gpu, B, 2, 0x2000, 4096);
        mapping(&gpu, A, 3, 0x3000, 4096);
        mapping(&gpu, B, 4, 0x4000, 4096);
        assert_eq!(
            gpu.drain_vm_mappings("test", |m| m.owner_pid == A, false),
            2
        );
        let left: Vec<u32> = gpu
            .nouveau_vm_mappings
            .lock()
            .iter()
            .map(|m| m.gem_handle)
            .collect();
        assert_eq!(left, [2, 4]);
        assert_eq!(gpu.drain_vm_mappings("test", |_| false, true), 0);
        assert_eq!(gpu.drain_vm_mappings("test", |m| m.va == 0x4000, true), 1);
        let left: Vec<u32> = gpu
            .nouveau_vm_mappings
            .lock()
            .iter()
            .map(|m| m.gem_handle)
            .collect();
        assert_eq!(left, [2]);
    }

    #[test]
    fn gem_info_reports_the_callers_own_binding_never_another_contexts_va() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu();
        let h = object(&gpu, A, 65536, Some(0x5000_0000));
        assert!(crate::scheme::gem_mmap::add_ref(h, B).is_some());
        mapping(&gpu, A, h, 0x4000_0000, 65536);
        assert_eq!(gem_info(&gpu, h, A).unwrap().offset, 0x4000_0000);
        // B imported the object but never bound it: in B's VA space the
        // object is nowhere, and A's VA would point at whatever B has
        // there. Linux: `nouveau_vma_find(nvbo, cli->vmm)` -> NULL -> 0.
        assert_eq!(
            gem_info(&gpu, h, B).unwrap().offset,
            0,
            "the creator's VA means nothing in the importer's context"
        );
        mapping(&gpu, B, h, 0x7000_0000, 65536);
        assert_eq!(gem_info(&gpu, h, B).unwrap().offset, 0x7000_0000);
        assert_eq!(
            gem_info(&gpu, h, A).unwrap().offset,
            0x4000_0000,
            "A keeps its own, whatever B bound"
        );
        assert_eq!(
            gem_info(&gpu, h, 0).unwrap().offset,
            0x4000_0000,
            "the kernel has no VA space: the first binding"
        );
        assert!(crate::scheme::gem_mmap::unregister(h));
    }

    #[test]
    fn vram_used_is_the_sum_of_every_clients_live_vram_objects() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu();
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(0));
        let in_gart = object(&gpu, A, 65536, None);
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED),
            Ok(0),
            "GART is system memory"
        );
        let mine = vram_object(&gpu, A, 3 * MIB);
        let theirs = vram_object(&gpu, B, 5 * MIB);
        assert_eq!(
            getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED),
            Ok(8 * MIB),
            "the VRAM manager's usage: every client's, as Linux reports it"
        );
        // Another GPU's objects are that GPU's VRAM, not this one's.
        let other = NvidiaGpu::for_test(0x1f06, 8192);
        vram_object(&other, A, 7 * MIB);
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(8 * MIB));
        assert_eq!(
            getparam(&other, nv::NOUVEAU_GETPARAM_VRAM_USED),
            Ok(7 * MIB)
        );
        // Freed objects stop counting.
        gpu.nouveau_gem.lock().retain(|o| o.handle != theirs);
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(3 * MIB));
        gpu.nouveau_gem.lock().retain(|o| o.handle != mine);
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(0));
        gpu.nouveau_gem.lock().retain(|o| o.handle != in_gart);
    }

    // ----- NVIF: five payloads on one nr, resolved by the header's type -----

    const HDR: usize = size_of::<nv::NvifIoctlV0>();
    const NEW: usize = size_of::<nv::NvifIoctlNewV0>();
    const MTHD: usize = size_of::<nv::NvifIoctlMthdV0>();
    const SCLASS: usize = size_of::<nv::NvifIoctlSclassV0>();
    const OCLASS: usize = size_of::<nv::NvifSclassOclassV0>();
    const INFO: usize = size_of::<nv::NvDeviceInfoV0>();

    /// A raw NVIF request: mesa's anonymous `struct { ioctl; body; data }`
    /// as bytes, written unaligned exactly as the arm reads it.
    struct Nvif(Vec<u8>);

    impl Nvif {
        fn new(type_: u8, route: u8, token: u64, object: u64, len: usize) -> Self {
            let mut b = alloc::vec![0u8; len];
            let hdr = nv::NvifIoctlV0 {
                version: 0,
                type_,
                pad02: [0; 4],
                owner: 0,
                route,
                token,
                object,
            };
            unsafe { core::ptr::write_unaligned(b.as_mut_ptr() as *mut nv::NvifIoctlV0, hdr) };
            Nvif(b)
        }

        fn put<T: Copy>(mut self, at: usize, v: T) -> Self {
            assert!(at + size_of::<T>() <= self.0.len());
            unsafe { core::ptr::write_unaligned(self.0.as_mut_ptr().add(at) as *mut T, v) };
            self
        }

        /// Shortens the declared length, keeping the bytes behind it
        /// allocated (and zero): a driver that reads past the length reads
        /// a zero, not the heap, so the only thing a test observes is what
        /// the driver decided from the length itself.
        fn cut(mut self, len: usize) -> Self {
            self.0.truncate(len);
            self
        }

        fn get<T: Copy>(&self, at: usize) -> T {
            assert!(at + size_of::<T>() <= self.0.len());
            unsafe { core::ptr::read_unaligned(self.0.as_ptr().add(at) as *const T) }
        }

        fn send(&mut self, gpu: &NvidiaGpu, pid: u64) -> Result<usize, i32> {
            let req = ioc(IOC_WRITE, DRM_IOCTL_TYPE, nv::NR_NVIF, self.0.len());
            gpu.nouveau_ioctl(req, self.0.as_mut_ptr() as usize, pid)
        }
    }

    fn new_body(oclass: i32, object: u64) -> nv::NvifIoctlNewV0 {
        nv::NvifIoctlNewV0 {
            version: 0,
            pad01: [0; 6],
            route: 0,
            token: object,
            object,
            handle: 0,
            oclass,
        }
    }

    /// `nouveau_ws_device_alloc`: 72 bytes, NEW of NV_DEVICE with a selector.
    fn device_new(device: u64) -> Nvif {
        Nvif::new(
            nv::NVIF_IOCTL_V0_NEW,
            0,
            0,
            0,
            HDR + NEW + size_of::<nv::NvDeviceV0>(),
        )
        .put(HDR, new_body(nv::NVIF_CLASS_NV_DEVICE, 0xd0d0))
        .put(
            HDR + NEW,
            nv::NvDeviceV0 {
                version: 0,
                pad01: [0; 7],
                device,
            },
        )
    }

    /// `nouveau_ws_subchan_alloc`: 56 bytes, NEW of an engine class on a
    /// channel (route 0xff, token = the channel id).
    fn subchan_new(channel: u64, oclass: i32, object: u64) -> Nvif {
        Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0xff, channel, 0, HDR + NEW)
            .put(HDR, new_body(oclass, object))
    }

    /// `nouveau_ws_device_info`: 136 bytes, MTHD NV_DEVICE_V0_INFO.
    fn device_info(method: u8) -> Nvif {
        Nvif::new(nv::NVIF_IOCTL_V0_MTHD, 0, 0, 0xd0d0, HDR + MTHD + INFO).put(
            HDR,
            nv::NvifIoctlMthdV0 {
                version: 0,
                method,
                pad02: [0; 6],
            },
        )
    }

    /// `nouveau_ws_context_query_classes`: SCLASS with `slots` entries of
    /// room, every one pre-filled so a stale slot is visible.
    fn sclass(channel: u64, route: u8, count: u8, slots: usize) -> Nvif {
        let mut r = Nvif::new(
            nv::NVIF_IOCTL_V0_SCLASS,
            route,
            channel,
            0,
            HDR + SCLASS + slots * OCLASS,
        )
        .put(
            HDR,
            nv::NvifIoctlSclassV0 {
                version: 0,
                count,
                pad02: [0; 6],
            },
        );
        for i in 0..slots {
            r = r.put(
                HDR + SCLASS + i * OCLASS,
                nv::NvifSclassOclassV0 {
                    oclass: 0x7777,
                    minver: 7,
                    maxver: 7,
                },
            );
        }
        r
    }

    fn classes_in(r: &Nvif) -> (u8, Vec<i32>) {
        let count = r.get::<nv::NvifIoctlSclassV0>(HDR).count;
        let slots = (r.0.len() - HDR - SCLASS) / OCLASS;
        let list = (0..slots)
            .map(|i| {
                r.get::<nv::NvifSclassOclassV0>(HDR + SCLASS + i * OCLASS)
                    .oclass
            })
            .collect();
        (count, list)
    }

    fn cstr(b: &[u8]) -> &str {
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        core::str::from_utf8(&b[..end]).unwrap()
    }

    #[test]
    fn nvif_refuses_a_short_payload_and_an_unknown_type_before_reading_a_body() {
        let _g = LOCK.lock();
        let gpu = gpu();
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0, 0, 0, HDR - 1).send(&gpu, A),
            Err(nv::EINVAL),
            "shorter than the header"
        );
        assert_eq!(
            Nvif::new(0x09, 0, 0, 0, HDR + NEW).send(&gpu, A),
            Err(nv::ENOSYS),
            "a type this driver has no arm for"
        );
        assert_eq!(
            device_new(u64::MAX).cut(HDR + NEW - 1).send(&gpu, A),
            Err(nv::EINVAL),
            "NEW without its 32-byte body, however acceptable the bytes behind"
        );
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_MTHD, 0, 0, 0, HDR + MTHD - 1).send(&gpu, A),
            Err(nv::EINVAL),
            "MTHD without its 8-byte body"
        );
        assert_eq!(
            device_info(nv::NV_DEVICE_V0_INFO)
                .cut(HDR + MTHD + INFO - 1)
                .send(&gpu, A),
            Err(nv::EINVAL),
            "INFO with no room for its 104-byte reply"
        );
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
        assert_eq!(
            sclass(0, 0xff, 16, 16).cut(HDR + SCLASS - 1).send(&gpu, A),
            Err(nv::EINVAL),
            "SCLASS without its 8-byte body, on a channel that exists"
        );
        // An NVIF request has no fixed floor at the dispatch: the 24-byte
        // DEL is a complete request.
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0, 0, 0x1234, HDR).send(&gpu, A),
            Ok(0)
        );
    }

    #[test]
    fn nvif_new_of_the_device_object_takes_only_the_client_default() {
        let _g = LOCK.lock();
        let gpu = gpu();
        assert_eq!(device_new(u64::MAX).send(&gpu, A), Ok(0), "mesa's ~0");
        assert_eq!(
            device_new(0).send(&gpu, A),
            Err(nv::EINVAL),
            "this node exposes one GPU; selecting another is an error"
        );
        assert_eq!(device_new(1).send(&gpu, A), Err(nv::EINVAL));
        // No class data at all (a 56-byte NEW of NV_DEVICE): the default.
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_NEW, 0, 0, 0, HDR + NEW)
                .put(HDR, new_body(nv::NVIF_CLASS_NV_DEVICE, 1))
                .send(&gpu, A),
            Ok(0)
        );
        // oclass 0 is what mesa sends when SCLASS gave it nothing: refused
        // even on a channel of the caller's, where any real class is fine.
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
        assert_eq!(subchan_new(0, 0xc597, 1).send(&gpu, A), Ok(0));
        assert_eq!(subchan_new(0, 0, 1).send(&gpu, A), Err(nv::EINVAL));
    }

    #[test]
    fn nvif_device_info_reports_the_board_and_floors_its_vram() {
        let _g = LOCK.lock();
        let gpu = gpu();
        let mut r = device_info(nv::NV_DEVICE_V0_INFO);
        assert_eq!(r.send(&gpu, A), Ok(0));
        let info: nv::NvDeviceInfoV0 = r.get(HDR + MTHD);
        assert_eq!(info.version, 0);
        assert_eq!(
            info.platform,
            nv::NV_DEVICE_INFO_V0_PCIE,
            "discrete: NVK's conformance gate needs DIS"
        );
        assert_eq!(info.chipset, 0x162, "the same chip GETPARAM reports");
        assert_eq!(info.revision, 0, "BAR0 reads as zero");
        assert_eq!(info.family, 0);
        assert_eq!(
            (info.ram_size, info.ram_user),
            (8192 * MIB, 8192 * MIB),
            "ram_user is what mesa takes as vram_size_B"
        );
        assert_eq!(cstr(&info.chip), "TU1xx");
        assert_eq!(cstr(&info.name), "nvidia-test");
        assert_eq!(
            device_info(0x05).send(&gpu, A),
            Err(nv::ENOSYS),
            "the only method is INFO"
        );
        // A board the id table does not know: the architecture's floor,
        // never a 0 that would leave NVK with an empty VRAM heap.
        let unknown = NvidiaGpu::for_test(0x1fff, 0);
        let mut r = device_info(nv::NV_DEVICE_V0_INFO);
        assert_eq!(r.send(&unknown, A), Ok(0));
        let info: nv::NvDeviceInfoV0 = r.get(HDR + MTHD);
        assert_eq!(info.ram_user, 4096 * MIB);
    }

    #[test]
    fn nvif_sclass_lists_the_callers_channels_engines_within_the_room_offered() {
        let _g = LOCK.lock();
        let gpu = gpu();
        assert_eq!(
            sclass(0, 0xff, 16, 16).send(&gpu, A),
            Err(nv::EINVAL),
            "no channel yet"
        );
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
        assert_eq!(
            sclass(0, 0x00, 16, 16).send(&gpu, A),
            Err(nv::EINVAL),
            "classes hang off a channel: route must be 0xff"
        );
        assert_eq!(
            sclass(0, 0xff, 16, 16).send(&gpu, B),
            Err(nv::EINVAL),
            "not B's channel"
        );
        assert_eq!(sclass(7, 0xff, 16, 16).send(&gpu, A), Err(nv::EINVAL));
        // Mesa's call: 16 slots offered, all five engines come back and
        // the unused tail is cleared (mesa reads every slot).
        let turing = [0x902d, 0xa140, 0xc597, 0xc5c0, 0xc5b5];
        let mut r = sclass(0, 0xff, 16, 16);
        assert_eq!(r.send(&gpu, A), Ok(0));
        let (count, list) = classes_in(&r);
        assert_eq!(count, 5);
        assert_eq!(&list[..5], &turing);
        assert!(
            list[5..].iter().all(|&c| c == 0),
            "no stale slot: {:?}",
            list
        );
        let mut r = sclass(0, 0xff, 16, 16);
        assert_eq!(r.send(&gpu, 0), Ok(0), "the kernel reads anyone's");
        // Less room than engines, said by count: the first ones, and
        // nothing written past what the caller offered.
        let mut r = sclass(0, 0xff, 3, 16);
        assert_eq!(r.send(&gpu, A), Ok(0));
        let (count, list) = classes_in(&r);
        assert_eq!(count, 3);
        assert_eq!(&list[..3], &turing[..3]);
        assert!(
            list[3..].iter().all(|&c| c == 0x7777),
            "beyond count is the caller's: {:?}",
            list
        );
        // Less room than count, said by the payload itself.
        let mut r = sclass(0, 0xff, 16, 2);
        assert_eq!(r.send(&gpu, A), Ok(0));
        assert_eq!(classes_in(&r), (2, alloc::vec![0x902d, 0xa140]));
        // More slots than the protocol's ceiling: capped at 16.
        let mut r = sclass(0, 0xff, 40, 40);
        assert_eq!(r.send(&gpu, A), Ok(0));
        let (count, list) = classes_in(&r);
        assert_eq!(count, 5);
        assert!(list[5..16].iter().all(|&c| c == 0));
        assert!(
            list[16..].iter().all(|&c| c == 0x7777),
            "past 16 is never touched"
        );
        // A second GPU of another architecture answers with its own triple.
        let ampere = NvidiaGpu::for_test(0x2204, 24576);
        assert_eq!(channel_alloc(&ampere, A).unwrap().channel, 0);
        let mut r = sclass(0, 0xff, 16, 16);
        assert_eq!(r.send(&ampere, A), Ok(0));
        assert_eq!(
            &classes_in(&r).1[..5],
            &[0x902d, 0xa140, 0xc797, 0xc7c0, 0xc7b5]
        );
    }

    #[test]
    fn nvif_new_of_a_subchannel_needs_the_callers_channel_and_records_nothing_without_the_rm() {
        let _g = LOCK.lock();
        let gpu = gpu();
        assert_eq!(
            subchan_new(0, 0xc597, 0x1000).send(&gpu, A),
            Err(nv::EINVAL),
            "no channel: Linux's abi16 finds no object for the token"
        );
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
        assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
        assert_eq!(
            subchan_new(0, 0xc5c0, 0x1008).send(&gpu, 0),
            Ok(0),
            "the kernel"
        );
        assert_eq!(
            subchan_new(0, 0xc597, 0x1000).send(&gpu, B),
            Err(nv::EINVAL),
            "A's channel is not B's"
        );
        assert_eq!(
            subchan_new(3, 0xc597, 0x1000).send(&gpu, A),
            Err(nv::EINVAL)
        );
        assert!(
            nv::class_objects_drain_pid(A).is_empty(),
            "a discovery channel builds no RM object, so there is nothing to reap"
        );
        assert_eq!(channel_free(&gpu, 0, A), Ok(0));
        assert_eq!(
            subchan_new(0, 0xc597, 0x1000).send(&gpu, A),
            Err(nv::EINVAL),
            "freed: the token means nothing again"
        );
    }

    #[test]
    fn nvif_del_and_the_class_registry_are_scoped_to_the_owner_and_the_channel() {
        let _g = LOCK.lock();
        let gpu = gpu();
        nv::class_object_insert(3, 0x1000, 0x5a5a, A);
        nv::class_object_insert(3, 0x1000, 0x6b6b, B);
        nv::class_object_insert(4, 0x2000, 0x7c7c, A);
        nv::class_object_insert(3, 0x3000, 0x8d8d, A);
        // A DEL names the object by the cookie mesa passed at NEW (a heap
        // pointer, so equal across processes): only the caller's goes.
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 3, 0x1000, HDR).send(&gpu, STRANGER),
            Ok(0),
            "nothing of STRANGER's: a no-op, as in Linux's nvif_object_dtor"
        );
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 3, 0x1000, HDR).send(&gpu, A),
            Ok(0)
        );
        assert_eq!(nv::class_object_remove(0x1000, A), None, "gone");
        assert_eq!(
            nv::class_object_remove(0x1000, B),
            Some(0x6b6b),
            "B's survived A's DEL"
        );
        // CHANNEL_FREE reaps what a process left on THAT channel.
        assert_eq!(nv::class_objects_drain_channel(3, B), alloc::vec![]);
        assert_eq!(
            nv::class_objects_drain_channel(3, A),
            alloc::vec![(0x3000, 0x8d8d)]
        );
        assert_eq!(nv::class_objects_drain_channel(4, B), alloc::vec![]);
        // Process exit reaps everything of that pid, across channels.
        nv::class_object_insert(5, 0x5000, 0x9e9e, A);
        nv::class_object_insert(5, 0x6000, 0xafaf, B);
        assert_eq!(
            nv::class_objects_drain_pid(A),
            alloc::vec![(0x2000, 0x7c7c), (0x5000, 0x9e9e)],
            "in insertion order"
        );
        assert_eq!(nv::class_objects_drain_pid(A), alloc::vec![]);
        assert_eq!(
            nv::class_objects_drain_pid(B),
            alloc::vec![(0x6000, 0xafaf)]
        );
    }

    // ----- With the fake RM: the arms a GL client reaches once attached -----

    use super::rm_host_shims::{
        fake_fbmem_offset, notifier_pa, reset_fake_rm, FastChan, FAKE_DOORBELL, FAKE_RM,
        FAST_ENTRIES, FAST_GPFIFO_OFF, FAST_PB_OFF, FAST_SEM_OFF,
    };

    /// The compositor's pid: it owns ctx 0, so every other pid is a GL
    /// client and gets a context of its own.
    const COMP: u64 = 66_000;
    /// The compositor respawned: a new pid for the same role.
    const COMP2: u64 = 66_004;

    fn gpu_rm() -> NvidiaGpu {
        let gpu = gpu();
        reset_fake_rm();
        *gpu.rm_device_instance.lock() = Some(0);
        gpu.ctx0_owner.store(COMP, Ordering::Release);
        gpu
    }

    fn gem_new_rm(
        gpu: &NvidiaGpu,
        size: u64,
        domain: u32,
        pid: u64,
    ) -> Result<nv::DrmNouveauGemInfo, i32> {
        let mut r = nv::DrmNouveauGemNew {
            info: nv::DrmNouveauGemInfo {
                handle: 0,
                domain,
                size,
                offset: 0,
                map_handle: 0,
                tile_mode: 0,
                tile_flags: 0,
            },
            channel_hint: 0,
            align: 0,
        };
        call(gpu, wr::<nv::DrmNouveauGemNew>(nv::NR_GEM_NEW), &mut r, pid).map(|_| r.info)
    }

    fn op(op: u32, flags: u32, handle: u32, addr: u64, range: u64) -> nv::DrmNouveauVmBindOp {
        nv::DrmNouveauVmBindOp {
            op,
            flags,
            handle,
            pad: 0,
            addr,
            bo_offset: 0,
            range,
        }
    }

    fn map(handle: u32, addr: u64, range: u64) -> nv::DrmNouveauVmBindOp {
        op(
            nv::VM_BIND_OP_MAP,
            nv::PTE_KIND_GENERIC,
            handle,
            addr,
            range,
        )
    }

    fn map_at(handle: u32, addr: u64, range: u64, bo_offset: u64) -> nv::DrmNouveauVmBindOp {
        nv::DrmNouveauVmBindOp {
            bo_offset,
            ..map(handle, addr, range)
        }
    }

    fn unmap(addr: u64, range: u64) -> nv::DrmNouveauVmBindOp {
        op(nv::VM_BIND_OP_UNMAP, 0, 0, addr, range)
    }

    fn vm_bind_ops(
        gpu: &NvidiaGpu,
        pid: u64,
        ops: &mut [nv::DrmNouveauVmBindOp],
    ) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauVmBind {
            op_count: ops.len() as u32,
            flags: 0,
            wait_count: 0,
            sig_count: 0,
            wait_ptr: 0,
            sig_ptr: 0,
            op_ptr: ops.as_mut_ptr() as u64,
        };
        call(gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, pid)
    }

    fn ctx_of(gpu: &NvidiaGpu, pid: u64) -> Option<(u32, bool)> {
        gpu.nouveau_pid_ctx
            .lock()
            .iter()
            .find(|t| t.0 == pid)
            .map(|t| (t.1, t.4))
    }

    fn rm_backed_channels(gpu: &NvidiaGpu, pid: u64) -> usize {
        gpu.nouveau_channels
            .lock()
            .iter()
            .filter(|c| c.owner_pid == pid && c.rm_backed)
            .count()
    }

    fn driver_maps(gpu: &NvidiaGpu, pid: u64) -> Vec<(u32, u64, u64, u32)> {
        gpu.nouveau_vm_mappings
            .lock()
            .iter()
            .filter(|m| m.owner_pid == pid)
            .map(|m| (m.gem_handle, m.va, m.size, m.h_virt))
            .collect()
    }

    #[test]
    fn with_the_rm_a_clients_channel_builds_its_own_context_once_and_falls_back_when_the_rm_refuses(
    ) {
        let _g = LOCK.lock();
        let gpu = gpu_rm();
        let c = channel_alloc(&gpu, A).unwrap();
        assert_eq!(c.channel, 0);
        assert_eq!(c.notifier_handle, 0x6001, "the notifier of CTX 1");
        assert_eq!(
            ctx_of(&gpu, A),
            Some((1, true)),
            "built, primed, published READY"
        );
        assert_eq!(rm_backed_channels(&gpu, A), 1);
        assert_eq!(FAKE_RM.lock().calls, ["ctx_alloc", "ctx_prime"]);
        // A second channel of the same pid reuses the context: NVK opens
        // several per process (labwc runs two Vulkan instances).
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 1);
        assert_eq!(rm_backed_channels(&gpu, A), 2);
        assert_eq!(FAKE_RM.lock().ctxs, [1], "still one context");
        assert_eq!(channel_alloc(&gpu, B).unwrap().notifier_handle, 0x6002);
        assert_eq!(ctx_of(&gpu, B), Some((2, true)));
        assert!(gpu.nouveau_rm_vas_ready());
        // The compositor's own channel is the step16/17 ladder, which the
        // fake does not carry: the honest answer is ENODEV, not a client
        // context in disguise.
        assert_eq!(
            channel_alloc(&gpu, COMP).map(|c| c.channel),
            Err(nv::ENODEV)
        );
        assert_eq!(ctx_of(&gpu, COMP), None);
        // ctx_alloc refused: a discovery channel, nothing reserved.
        FAKE_RM.lock().fail_ctx_alloc = true;
        let c = channel_alloc(&gpu, STRANGER).unwrap();
        assert_eq!(c.notifier_handle, 0, "no RM notifier");
        assert_eq!(rm_backed_channels(&gpu, STRANGER), 0);
        assert_eq!(ctx_of(&gpu, STRANGER), None, "the reservation was removed");
        assert_eq!(FAKE_RM.lock().ctxs, [1, 2]);
        FAKE_RM.lock().fail_ctx_alloc = false;
        // ctx_alloc returned NV_OK with a failed stage: the C side already
        // freed what it built, so the driver only drops the reservation and
        // must neither prime nor ctx_free a context that does not exist.
        FAKE_RM.lock().incomplete_ctx = true;
        assert_eq!(channel_alloc(&gpu, STRANGER).unwrap().notifier_handle, 0);
        assert_eq!(ctx_of(&gpu, STRANGER), None);
        assert_eq!(
            FAKE_RM.lock().calls.last(),
            Some(&"ctx_alloc"),
            "half-built: not primed, and nothing to free"
        );
        assert_eq!(FAKE_RM.lock().ctx_frees, 0);
        FAKE_RM.lock().incomplete_ctx = false;
        // Prime failed: the context is torn down again (a kept one hangs
        // FECS on the first 3D draw) and the client falls back to software.
        FAKE_RM.lock().fail_prime = true;
        assert_eq!(channel_alloc(&gpu, STRANGER).unwrap().notifier_handle, 0);
        assert_eq!(ctx_of(&gpu, STRANGER), None);
        {
            let f = FAKE_RM.lock();
            assert_eq!(f.ctx_frees, 1, "ctx_free after the failed prime");
            assert_eq!(f.ctxs, [1, 2], "slot 3 is free again");
        }
        FAKE_RM.lock().fail_prime = false;
        assert_eq!(
            ctx_of(&gpu, A),
            Some((1, true)),
            "A's context is untouched by all that"
        );
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        gpu.nouveau_release_process(STRANGER);
    }

    #[test]
    fn with_the_rm_gem_new_registers_what_is_cpu_mappable_and_close_gives_the_memory_back() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        let gart = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A).unwrap();
        let h_gart = FAKE_RM.lock().gems[0].0;
        assert_eq!(FAKE_RM.lock().gems, [(h_gart, 65536, true)]);
        assert_eq!(gart.domain, nv::NOUVEAU_GEM_DOMAIN_GART);
        assert_eq!(
            gart.map_handle,
            u64::from(gart.handle) << 12,
            "CPU-mappable"
        );
        assert_eq!(
            crate::scheme::gem_mmap::lookup(gart.handle),
            Some((FAKE_RM.lock().pa_of(h_gart).unwrap(), 65536)),
            "registered for mmap and PRIME at the RM's host PA"
        );
        assert!(crate::scheme::gem_mmap::holds(gart.handle, A));
        // GART|VRAM (NVK's DEVICE_LOCAL|HOST_VISIBLE) stays system memory.
        let both = gem_new_rm(
            &gpu,
            4096,
            nv::NOUVEAU_GEM_DOMAIN_GART | nv::NOUVEAU_GEM_DOMAIN_VRAM,
            A,
        )
        .unwrap();
        assert_eq!(both.domain, nv::NOUVEAU_GEM_DOMAIN_GART);
        assert_ne!(both.map_handle, 0);
        // VRAM-only is DEVICE_LOCAL: no host PA is ever published for it.
        let vram = gem_new_rm(&gpu, 3 * MIB, nv::NOUVEAU_GEM_DOMAIN_VRAM, A).unwrap();
        let h_vram = FAKE_RM.lock().gems[2].0;
        assert_eq!(FAKE_RM.lock().gems[2], (h_vram, 3 * MIB, false));
        assert_eq!(
            (vram.domain, vram.map_handle),
            (nv::NOUVEAU_GEM_DOMAIN_VRAM, 0)
        );
        assert!(crate::scheme::gem_mmap::lookup(vram.handle).is_none());
        assert_eq!(
            gpu.nouveau_gem
                .lock()
                .iter()
                .find(|o| o.handle == vram.handle)
                .and_then(|o| o.vram_offset),
            Some(fake_fbmem_offset(h_vram)),
            "its FBMEM offset, for scanout by offset"
        );
        assert_eq!(getparam(&gpu, nv::NOUVEAU_GETPARAM_VRAM_USED), Ok(3 * MIB));
        assert_eq!(
            _live.delta(),
            65536 + 4096 + 3 * MIB as i64,
            "every byte the RM holds is counted against the quotas"
        );
        assert_eq!(
            &FAKE_RM.lock().calls[4..],
            ["gem_alloc", "gem_fbmem_offset"],
            "VRAM: never asked for a CPU mapping"
        );
        assert_eq!(
            gem_info(&gpu, gart.handle, A).map(|i| (i.size, i.map_handle)),
            Ok((65536, gart.map_handle))
        );
        // The RM put a "GART" object in an aperture the CPU cannot reach:
        // a live object, but not mmap-able and not registered as such.
        FAKE_RM.lock().map_cpu_elsewhere = true;
        let far = gem_new_rm(&gpu, 8192, nv::NOUVEAU_GEM_DOMAIN_GART, A).unwrap();
        FAKE_RM.lock().map_cpu_elsewhere = false;
        assert_eq!(far.map_handle, 0);
        assert!(crate::scheme::gem_mmap::lookup(far.handle).is_none());
        assert!(has_object(&gpu, far.handle));
        assert!(gpu.nouveau_gem_close(far.handle, A));
        assert_eq!(FAKE_RM.lock().gem_frees, 1);
        // The RM refuses: nothing is left behind, not even the bytes.
        let counted = _live.delta();
        FAKE_RM.lock().fail_gem_alloc = true;
        assert_eq!(
            gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A).map(|i| i.handle),
            Err(nv::ENOMEM)
        );
        FAKE_RM.lock().fail_gem_alloc = false;
        assert_eq!(_live.delta(), counted);
        assert_eq!(gpu.nouveau_gem.lock().len(), 3);
        // GEM_CLOSE hands the RM memory back, once, and the registry entry
        // goes with it.
        assert!(gpu.nouveau_gem_close(gart.handle, A));
        assert!(crate::scheme::gem_mmap::lookup(gart.handle).is_none());
        assert!(gpu.nouveau_gem_close(vram.handle, A));
        {
            let f = FAKE_RM.lock();
            assert_eq!(f.gem_frees, 3);
            assert_eq!(f.gems.len(), 1, "only the GART|VRAM one is still allocated");
            assert_eq!(f.bad, 0);
        }
        assert!(
            !gpu.nouveau_gem_close(gart.handle, A),
            "closed twice: refused, not freed twice"
        );
        assert_eq!(FAKE_RM.lock().bad, 0);
        gpu.nouveau_release_process(A);
        assert_eq!(FAKE_RM.lock().gems, [], "process exit freed the last one");
    }

    #[test]
    fn vm_bind_maps_into_the_callers_own_context_and_a_map_over_a_live_range_replaces_it() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        const VA: u64 = 0x3f_f000_0000;
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(1, VA, 4096)]),
            Err(nv::ENODEV),
            "no RM-backed channel on this GPU: no VA space to bind into"
        );
        assert_eq!(channel_alloc(&gpu, A).unwrap().channel, 0);
        let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
            .unwrap()
            .handle;
        let h_mem_a = FAKE_RM.lock().gems[0].0;
        assert_eq!(vm_bind_ops(&gpu, A, &mut [map(ha, VA, 65536)]), Ok(0));
        let f_maps = FAKE_RM.lock().maps.clone();
        assert_eq!(f_maps.len(), 1);
        let (h_virt, ctx, h_mem, va, size, bo_offset, kind) = f_maps[0];
        assert_eq!(
            (ctx, h_mem, va, size, bo_offset, kind),
            (1, h_mem_a, VA, 65536, 0, 0x06),
            "A's context, its memory, the kind verbatim"
        );
        assert_eq!(
            driver_maps(&gpu, A),
            [(ha, VA, 65536, h_virt)],
            "the driver's record names the RM's h_virt"
        );
        assert_eq!(gem_info(&gpu, ha, A).unwrap().offset, VA);
        // REPLACE: a MAP over a live range of the same context unmaps it
        // first (Linux gpuvm semantics; the RM would refuse the fixed VA).
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(ha, VA + 0x8000, 65536)]),
            Ok(0)
        );
        {
            let f = FAKE_RM.lock();
            assert_eq!(f.unmaps, 1, "the old binding was unmapped in the RM");
            assert_eq!(f.maps_of_ctx(1), [(VA + 0x8000, 65536, 0x06)]);
            assert_eq!(f.bad, 0);
        }
        assert_eq!(driver_maps(&gpu, A).len(), 1);
        // A mapping that starts exactly where the new one ends is a
        // neighbour, not an overlap: it stays. The offset into the object
        // reaches the RM and the record.
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map_at(ha, VA + 0x18000, 4096, 0x3000)]),
            Ok(0)
        );
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(ha, VA + 0x8000, 65536)]),
            Ok(0),
            "re-bound over itself"
        );
        {
            let f = FAKE_RM.lock();
            assert_eq!(f.unmaps, 2, "only the overlapping one was replaced");
            assert_eq!(
                f.maps_of_ctx(1),
                [(VA + 0x18000, 4096, 0x06), (VA + 0x8000, 65536, 0x06)]
            );
            assert_eq!(
                f.maps.iter().find(|m| m.3 == VA + 0x18000).map(|m| m.5),
                Some(0x3000),
                "bo_offset handed to the RM"
            );
        }
        assert_eq!(
            gpu.nouveau_vm_mappings
                .lock()
                .iter()
                .find(|m| m.va == VA + 0x18000)
                .map(|m| m.bo_offset),
            Some(0x3000)
        );
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x18000, 4096)]),
            Ok(0)
        );
        assert_eq!(driver_maps(&gpu, A).len(), 1);
        assert_eq!(FAKE_RM.lock().unmaps, 3);
        // Another process, the same VA: its own context, so no conflict.
        assert_eq!(channel_alloc(&gpu, B).unwrap().channel, 1);
        let hb = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, B)
            .unwrap()
            .handle;
        assert_eq!(
            vm_bind_ops(&gpu, B, &mut [map(hb, VA + 0x8000, 4096)]),
            Ok(0)
        );
        {
            let f = FAKE_RM.lock();
            assert_eq!(f.maps.len(), 2);
            assert_eq!(f.maps_of_ctx(2), [(VA + 0x8000, 4096, 0x06)]);
            assert_eq!(f.unmaps, 3, "B replaced nothing of A's");
        }
        assert_eq!(driver_maps(&gpu, A).len(), 1);
        // Binding another process's buffer is a GPU read/write of it: only
        // a holder may. PRIME makes A a holder.
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(hb, VA + 0x10_0000, 4096)]),
            Err(nv::ENOENT)
        );
        assert!(crate::scheme::gem_mmap::add_ref(hb, A).is_some());
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(hb, VA + 0x10_0000, 4096)]),
            Ok(0)
        );
        assert_eq!(driver_maps(&gpu, A).len(), 2);
        // UNMAP is by range, scoped to the caller: A's overlapping mapping
        // goes, B's identical VA stays. An empty range is a success.
        assert_eq!(vm_bind_ops(&gpu, A, &mut [unmap(VA + 0x8000, 4096)]), Ok(0));
        assert_eq!(
            driver_maps(&gpu, A).iter().map(|m| m.0).collect::<Vec<_>>(),
            [hb]
        );
        assert_eq!(
            FAKE_RM.lock().maps_of_ctx(2).len(),
            1,
            "B's binding is still live"
        );
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [unmap(0x1000, 0x1000)]),
            Ok(0),
            "nothing there: fine"
        );
        assert_eq!(FAKE_RM.lock().unmaps, 4);
        // MAP with handle 0 is Mesa's "unbind, keep the reservation", and
        // it is scoped like UNMAP: B's mapping at the same VA is not A's.
        assert_eq!(
            vm_bind_ops(&gpu, B, &mut [map(hb, VA + 0x10_0000, 4096)]),
            Ok(0)
        );
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(0, VA + 0x10_0000, 4096)]),
            Ok(0)
        );
        assert_eq!(driver_maps(&gpu, A), []);
        assert_eq!(FAKE_RM.lock().maps_of_ctx(1), []);
        assert_eq!(driver_maps(&gpu, B).len(), 2, "B's two bindings untouched");
        assert_eq!(FAKE_RM.lock().maps_of_ctx(2).len(), 2);
        // What the arm refuses before touching the RM.
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [op(7, 0, ha, VA, 4096)]),
            Err(nv::EINVAL),
            "unknown op"
        );
        assert_eq!(
            vm_bind_ops(
                &gpu,
                A,
                &mut [op(nv::VM_BIND_OP_MAP, nv::VM_BIND_SPARSE, 0, VA, 4096)]
            ),
            Err(nv::EOPNOTSUPP),
            "sparse regions"
        );
        assert_eq!(vm_bind_ops(&gpu, A, &mut []), Err(nv::EINVAL), "no ops");
        let mut many: Vec<_> = (0..65).map(|_| map(ha, VA, 4096)).collect();
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut many),
            Err(nv::EOPNOTSUPP),
            "65 ops"
        );
        let mut r = nv::DrmNouveauVmBind {
            op_count: 1,
            flags: 0,
            wait_count: 1,
            sig_count: 0,
            wait_ptr: 0x1000,
            sig_ptr: 0,
            op_ptr: many.as_mut_ptr() as u64,
        };
        assert_eq!(
            call(&gpu, wr::<nv::DrmNouveauVmBind>(nv::NR_VM_BIND), &mut r, A),
            Err(nv::EOPNOTSUPP),
            "VM_BIND is synchronous here: no syncobj waits"
        );
        assert_eq!(
            FAKE_RM.lock().calls.len(),
            before,
            "none of those reached the RM"
        );
        // Ops apply in order and stop at the first failure.
        let mut ops = [
            map(ha, VA, 4096),
            map(ha + 1000, VA + 0x1000, 4096),
            map(ha, VA + 0x2000, 4096),
        ];
        assert_eq!(vm_bind_ops(&gpu, A, &mut ops), Err(nv::ENOENT));
        assert_eq!(
            driver_maps(&gpu, A).len(),
            1,
            "op[0] applied, op[2] never ran"
        );
        assert_eq!(FAKE_RM.lock().maps_of_ctx(1), [(VA, 4096, 0x06)]);
        // The RM refused the map (a fixed VA it will not reserve): EIO, and
        // no binding is recorded for a mapping that does not exist.
        FAKE_RM.lock().refuse_map = true;
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(ha, VA + 0x4000, 4096)]),
            Err(nv::EIO)
        );
        FAKE_RM.lock().refuse_map = false;
        assert_eq!(driver_maps(&gpu, A).len(), 1);
        assert_eq!(gem_info(&gpu, ha, A).unwrap().offset, VA);
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn vm_bind_programs_uncompressed_kinds_verbatim_and_hands_the_rest_to_the_rm_default() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        channel_alloc(&gpu, A).unwrap();
        let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
            .unwrap()
            .handle;
        // (kind asked, kind programmed)
        let cases = [
            (0x00, 0x00),
            (0x01, 0x01),
            (0x03, 0x03),
            (0x06, 0x06),
            (0x0a, 0x0a),
            (0x0f, 0x0f),
            (0x07, 0x00),
            (0x20, 0x00),
            (0xff, 0x00),
        ];
        for (i, (asked, _)) in cases.iter().enumerate() {
            let va = 0x1000_0000 + (i as u64) * 0x1_0000;
            assert_eq!(
                vm_bind_ops(&gpu, A, &mut [op(nv::VM_BIND_OP_MAP, *asked, h, va, 4096)]),
                Ok(0),
                "a kind is never refused: kind {:#x}",
                asked
            );
        }
        let programmed: Vec<u32> = FAKE_RM.lock().maps_of_ctx(1).iter().map(|m| m.2).collect();
        let expected: Vec<u32> = cases.iter().map(|c| c.1).collect();
        assert_eq!(programmed, expected);
        // Bits above the kind byte (bit 8 is SPARSE, refused elsewhere) are
        // not a kind: 0x0200 programs the default, not 0x02, and 0x0206 is
        // still GENERIC.
        assert_eq!(
            vm_bind_ops(
                &gpu,
                A,
                &mut [op(nv::VM_BIND_OP_MAP, 0x0200, h, 0x2000_0000, 4096)]
            ),
            Ok(0)
        );
        assert_eq!(
            FAKE_RM.lock().maps_of_ctx(1).last().map(|m| m.2),
            Some(0x00)
        );
        assert_eq!(
            vm_bind_ops(
                &gpu,
                A,
                &mut [op(nv::VM_BIND_OP_MAP, 0x0206, h, 0x2001_0000, 4096)]
            ),
            Ok(0)
        );
        assert_eq!(
            FAKE_RM.lock().maps_of_ctx(1).last().map(|m| m.2),
            Some(0x06)
        );
        gpu.nouveau_release_process(A);
    }

    #[test]
    fn with_the_rm_a_subchannel_new_builds_the_class_on_the_callers_context_and_channel_free_reaps_it(
    ) {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        channel_alloc(&gpu, A).unwrap();
        channel_alloc(&gpu, B).unwrap();
        assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
        assert_eq!(subchan_new(0, 0xc5c0, 0x1008).send(&gpu, A), Ok(0));
        assert_eq!(
            subchan_new(1, 0xc597, 0x1000).send(&gpu, B),
            Ok(0),
            "B's own, same cookie"
        );
        {
            let f = FAKE_RM.lock();
            assert_eq!(
                f.classes.iter().map(|c| (c.1, c.2)).collect::<Vec<_>>(),
                [(1, 0xc597), (1, 0xc5c0), (2, 0xc597)],
                "each on its caller's context, never on ctx 0"
            );
        }
        // DEL frees the RM object, the caller's only.
        assert_eq!(
            Nvif::new(nv::NVIF_IOCTL_V0_DEL, 0xff, 0, 0x1000, HDR).send(&gpu, B),
            Ok(0)
        );
        assert_eq!(FAKE_RM.lock().class_frees, 1);
        assert_eq!(
            FAKE_RM
                .lock()
                .classes
                .iter()
                .map(|c| c.1)
                .collect::<Vec<_>>(),
            [1, 1]
        );
        // CHANNEL_FREE reaps what was left without DEL, on that channel
        // only, and leaves the context and its bindings alone (the VAS is
        // shared by every channel of the pid).
        let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
            .unwrap()
            .handle;
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(h, 0x5000_0000, 4096)]),
            Ok(0)
        );
        assert_eq!(channel_free(&gpu, 0, A), Ok(0));
        {
            let f = FAKE_RM.lock();
            assert_eq!(f.class_frees, 3);
            assert_eq!(f.classes, []);
            assert_eq!(f.ctxs, [1, 2], "no ctx_free on CHANNEL_FREE");
            assert_eq!(f.maps.len(), 1);
            assert_eq!(f.bad, 0);
        }
        assert!(nv::class_objects_drain_pid(A).is_empty());
        // The RM refuses the class: the NEW fails rather than leaving a
        // class NVK will submit methods of.
        channel_alloc(&gpu, A).unwrap();
        FAKE_RM.lock().refuse_class = true;
        assert_eq!(
            subchan_new(0, 0xc597, 0x2000).send(&gpu, A),
            Err(nv::EINVAL)
        );
        FAKE_RM.lock().refuse_class = false;
        assert!(nv::class_objects_drain_pid(A).is_empty());
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
    }

    #[test]
    fn process_exit_with_the_rm_frees_classes_then_the_context_then_memory_and_never_unmaps_a_dead_vas(
    ) {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        channel_alloc(&gpu, A).unwrap();
        channel_alloc(&gpu, B).unwrap();
        let ha = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, A)
            .unwrap()
            .handle;
        let hb = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_VRAM, B)
            .unwrap()
            .handle;
        assert_eq!(
            vm_bind_ops(&gpu, A, &mut [map(ha, 0x1000_0000, 65536)]),
            Ok(0)
        );
        assert_eq!(
            vm_bind_ops(&gpu, B, &mut [map(hb, 0x1000_0000, 4096)]),
            Ok(0)
        );
        assert_eq!(subchan_new(0, 0xc597, 0x1000).send(&gpu, A), Ok(0));
        let before = FAKE_RM.lock().calls.len();
        gpu.nouveau_release_process(A);
        let f = FAKE_RM.lock();
        assert_eq!(
            &f.calls[before..],
            ["class_free", "ctx_free", "gem_free"],
            "child before parent, GPU stopped before its memory goes"
        );
        assert_eq!(
            f.unmaps, 0,
            "ctx_free took the VAS: a second unmap would be a use-after-free"
        );
        assert_eq!(f.ctxs, [2]);
        assert_eq!(f.maps_of_ctx(2).len(), 1, "B's binding is untouched");
        assert_eq!(f.gems.len(), 1);
        assert_eq!(f.classes, []);
        assert_eq!(f.bad, 0);
        drop(f);
        assert_eq!(ctx_of(&gpu, A), None);
        assert_eq!(driver_maps(&gpu, A), []);
        assert!(!has_object(&gpu, ha));
        assert!(has_object(&gpu, hb));
        assert_eq!(rm_backed_channels(&gpu, A), 0);
        assert_eq!(rm_backed_channels(&gpu, B), 1);
        // A comes back: a fresh context in the freed slot, primed again.
        channel_alloc(&gpu, A).unwrap();
        assert_eq!(ctx_of(&gpu, A), Some((1, true)));
        assert_eq!(FAKE_RM.lock().primed, [1, 2, 1]);
        gpu.nouveau_release_process(A);
        // ctx_free refused for B: its VAS is still alive in the RM, so each
        // binding IS unmapped before its memory is freed (freeing the backing
        // under a live h_virt is a use-after-free in the vendor RM).
        FAKE_RM.lock().fail_ctx_free = true;
        let before = FAKE_RM.lock().calls.len();
        gpu.nouveau_release_process(B);
        FAKE_RM.lock().fail_ctx_free = false;
        {
            let f = FAKE_RM.lock();
            assert_eq!(
                &f.calls[before..],
                ["ctx_free", "vm_bind_unmap", "gem_free"]
            );
            assert_eq!(f.unmaps, 1);
            assert_eq!(f.maps, [], "B's binding went through the RM");
            assert_eq!(f.ctxs, [2], "the context the RM would not free stays");
            assert_eq!(f.bad, 0);
            assert_eq!(f.gems, []);
        }
        assert_eq!(ctx_of(&gpu, B), None, "forgotten locally either way");
    }

    // ----- EXEC through the fake: the RM per-submit path -----
    //
    // `exec_fast_prepare` stays refused in this binary, so every EXEC goes
    // the way it does on a context whose direct-submit setup failed: one
    // RM call per push, the last one carrying the fence the syncobjs of
    // `sig` wait for. The fake's fence lands before the call returns
    // unless told to stall.

    use crate::scheme::syncobj;

    /// Where the client's pushbuffer object is bound.
    const PUSH_VA: u64 = 0x7f_0000_0000;

    fn sync(handle: u32) -> nv::DrmNouveauSync {
        nv::DrmNouveauSync {
            flags: 0,
            handle,
            timeline_value: 0,
        }
    }

    fn sync_tl(handle: u32, point: u64) -> nv::DrmNouveauSync {
        nv::DrmNouveauSync {
            flags: nv::SYNC_TIMELINE_SYNCOBJ,
            handle,
            timeline_value: point,
        }
    }

    fn push(va: u64, va_len: u32) -> nv::DrmNouveauExecPush {
        nv::DrmNouveauExecPush {
            va,
            va_len,
            flags: 0,
        }
    }

    fn ptr_of<T>(items: &[T]) -> u64 {
        if items.is_empty() {
            0
        } else {
            items.as_ptr() as u64
        }
    }

    fn exec(
        gpu: &NvidiaGpu,
        pid: u64,
        channel: u32,
        pushes: &[nv::DrmNouveauExecPush],
        waits: &[nv::DrmNouveauSync],
        sigs: &[nv::DrmNouveauSync],
    ) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauExec {
            channel,
            push_count: pushes.len() as u32,
            wait_count: waits.len() as u32,
            sig_count: sigs.len() as u32,
            wait_ptr: ptr_of(waits),
            sig_ptr: ptr_of(sigs),
            push_ptr: ptr_of(pushes),
        };
        exec_raw(gpu, pid, &mut r)
    }

    fn exec_raw(gpu: &NvidiaGpu, pid: u64, r: &mut nv::DrmNouveauExec) -> Result<usize, i32> {
        call(gpu, wr::<nv::DrmNouveauExec>(nv::NR_EXEC), r, pid)
    }

    /// A client with its own context (channel 0 of this GPU when called
    /// first) and a 64 KiB GART object bound at `PUSH_VA`.
    fn client_with_pushbuf(gpu: &NvidiaGpu, pid: u64) -> u32 {
        let channel = channel_alloc(gpu, pid).unwrap().channel;
        let h = gem_new_rm(gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, pid)
            .unwrap()
            .handle;
        assert_eq!(vm_bind_ops(gpu, pid, &mut [map(h, PUSH_VA, 65536)]), Ok(0));
        channel as u32
    }

    fn rm_calls_since(before: usize) -> Vec<&'static str> {
        FAKE_RM.lock().calls[before..].to_vec()
    }

    #[test]
    fn exec_needs_the_callers_own_rm_channel_and_a_well_formed_request() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        let p = [push(PUSH_VA, 16)];
        // No RM-backed channel on this GPU at all.
        assert_eq!(exec(&gpu, A, 0, &p, &[], &[]), Err(nv::ENODEV));
        // A discovery channel is not a GPFIFO.
        FAKE_RM.lock().fail_ctx_alloc = true;
        let disc = channel_alloc(&gpu, STRANGER).unwrap().channel as u32;
        FAKE_RM.lock().fail_ctx_alloc = false;
        assert_eq!(exec(&gpu, STRANGER, disc, &p, &[], &[]), Err(nv::ENODEV));
        let ch = client_with_pushbuf(&gpu, A);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, STRANGER, disc, &p, &[], &[]),
            Err(nv::ENODEV),
            "still discovery-only, whoever else has a real one"
        );
        assert_eq!(
            exec(&gpu, B, ch, &p, &[], &[]),
            Err(nv::ENODEV),
            "B owns no RM channel"
        );
        channel_alloc(&gpu, B).unwrap();
        assert_eq!(
            exec(&gpu, B, ch, &p, &[], &[]),
            Err(nv::EINVAL),
            "B has one, but this channel id is A's"
        );
        assert_eq!(exec(&gpu, A, ch + 7, &p, &[], &[]), Err(nv::EINVAL));
        // A channel opened before the RM attached is discovery-only for
        // good, even once its owner has a real one: the client must free it
        // and CHANNEL_ALLOC again (the driver's own log says so).
        *gpu.rm_device_instance.lock() = None;
        let old = channel_alloc(&gpu, B).unwrap().channel as u32;
        *gpu.rm_device_instance.lock() = Some(0);
        assert_eq!(exec(&gpu, B, old, &p, &[], &[]), Err(nv::ENODEV));
        assert_eq!(channel_free(&gpu, old as i32, B), Ok(0));
        // The shape of the request, before any push is looked at.
        let too_many: Vec<_> = (0..65).map(|_| push(PUSH_VA, 16)).collect();
        assert_eq!(
            exec(&gpu, A, ch, &too_many, &[], &[]),
            Err(nv::EOPNOTSUPP),
            "65 pushes"
        );
        let mut r = nv::DrmNouveauExec {
            channel: ch,
            push_count: 1,
            wait_count: 0,
            sig_count: 0,
            wait_ptr: 0,
            sig_ptr: 0,
            push_ptr: 0,
        };
        assert_eq!(
            exec_raw(&gpu, A, &mut r),
            Err(nv::EFAULT),
            "null pushes: a user range check, like any null array"
        );
        r.push_ptr = p.as_ptr() as u64;
        r.wait_count = 1;
        assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "null waits");
        // Sixty-five real, satisfied syncs: a cap that let them through
        // would submit, not crash.
        let ready = syncobj::create(true);
        let many_syncs: Vec<_> = (0..65).map(|_| sync(ready)).collect();
        r.wait_count = 65;
        r.wait_ptr = many_syncs.as_ptr() as u64;
        assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "65 waits");
        r.wait_count = 0;
        r.wait_ptr = 0;
        r.sig_count = 1;
        assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "null sigs");
        r.sig_count = 65;
        r.sig_ptr = many_syncs.as_ptr() as u64;
        assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EOPNOTSUPP), "65 sigs");
        assert!(syncobj::destroy(ready));
        r.sig_count = 0;
        r.push_ptr = 0xffff_ffff_ffff_0000;
        assert_eq!(exec_raw(&gpu, A, &mut r), Err(nv::EFAULT), "kernel address");
        // Pushes are dword streams.
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 0)], &[], &[]),
            Err(nv::EINVAL)
        );
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16), push(PUSH_VA + 16, 6)],
                &[],
                &[]
            ),
            Err(nv::EINVAL),
            "checked for every push before any is submitted"
        );
        assert_eq!(
            rm_calls_since(before),
            ["ctx_alloc", "ctx_prime"],
            "none of that reached the ring (B's context build did)"
        );
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        gpu.nouveau_release_process(STRANGER);
    }

    #[test]
    fn exec_submits_every_push_in_order_and_signals_the_syncobjs_after_the_fence_lands() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        let ch = client_with_pushbuf(&gpu, A);
        let legacy = nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed);
        let before = FAKE_RM.lock().calls.len();
        // No signal: every push is a plain submit, nothing to wait for.
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 32)],
                &[],
                &[]
            ),
            Ok(0)
        );
        assert_eq!(
            rm_calls_since(before),
            ["exec_fast_prepare", "exec_submit", "exec_submit"],
            "the direct-submit setup is tried once and refused here: one RM entry per push"
        );
        assert_eq!(
            FAKE_RM.lock().submits,
            [(1, PUSH_VA, 16, None), (1, PUSH_VA + 0x100, 32, None)],
            "on the caller's own context, in order"
        );
        assert_eq!(nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed), legacy + 1);
        // With signals: the LAST push carries the fence; the syncobjs are
        // signaled only once it landed, a binary one to 1 and a timeline
        // one to the point asked for.
        let bin = syncobj::create(false);
        let tl = syncobj::create(false);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[
                    push(PUSH_VA, 16),
                    push(PUSH_VA + 0x100, 32),
                    push(PUSH_VA + 0x200, 8)
                ],
                &[],
                &[sync(bin), sync_tl(tl, 5)]
            ),
            Ok(0)
        );
        assert_eq!(
            rm_calls_since(before),
            ["exec_submit", "exec_submit", "exec_submit_signaled"]
        );
        {
            let f = FAKE_RM.lock();
            let last = f.submits.last().copied().unwrap();
            assert_eq!((last.0, last.1, last.2), (1, PUSH_VA + 0x200, 8));
            assert!(
                last.3.is_some_and(|p| p & 0x8000_0000 != 0),
                "a fresh fence payload"
            );
            assert!(
                f.submits[2..4].iter().all(|s| s.3.is_none()),
                "the earlier pushes carry none"
            );
        }
        assert_eq!(syncobj::query(bin), Some(1));
        assert_eq!(syncobj::query(tl), Some(5));
        // A signal on a handle nobody has: the work ran, the ioctl says so.
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(0xdead_0000)]),
            Err(nv::ENOENT)
        );
        assert_eq!(rm_calls_since(before), ["exec_submit_signaled"]);
        // Two channels of one process share the context and the ring.
        let ch2 = channel_alloc(&gpu, A).unwrap().channel as u32;
        assert_ne!(ch2, ch);
        assert_eq!(exec(&gpu, A, ch2, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(FAKE_RM.lock().submits.last().unwrap().0, 1);
        assert!(syncobj::destroy(bin));
        assert!(syncobj::destroy(tl));
        gpu.nouveau_release_process(A);
    }

    #[test]
    fn exec_waits_on_the_cpu_before_submitting_and_never_submits_after_a_wait_fails() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        let ch = client_with_pushbuf(&gpu, A);
        let done = syncobj::create(true);
        let tl = syncobj::create(false);
        assert!(syncobj::timeline_signal(tl, 3));
        let out = syncobj::create(false);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16)],
                &[sync(done), sync_tl(tl, 3)],
                &[sync(out)]
            ),
            Ok(0),
            "both waits already satisfied"
        );
        assert_eq!(
            rm_calls_since(before),
            ["exec_fast_prepare", "exec_submit_signaled"]
        );
        assert_eq!(syncobj::query(out), Some(1));
        // An unknown wait handle: ENOENT, and the ring never heard of it.
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16)],
                &[sync(0xdead_0001)],
                &[sync(out)]
            ),
            Err(nv::ENOENT)
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        // A wait that never comes: the 10 s deadline (the clock advances
        // 1 ms per read here) ends in EIO, the pushes are NOT submitted and
        // the sig list is NOT signaled -- NVK sees device-lost, not a
        // frame that was never drawn.
        crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16)],
                &[sync_tl(tl, 4)],
                &[sync_tl(out, 7)]
            ),
            Err(nv::EIO),
            "the timeline is at 3: point 4 is a wait, not a pass"
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        let never = syncobj::create(false);
        let waited = nv::EXEC_WAIT_US.load(Ordering::Relaxed);
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16)],
                &[sync_tl(tl, 4), sync(never)],
                &[sync_tl(out, 7)]
            ),
            Err(nv::EIO)
        );
        crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        assert_eq!(syncobj::query(out), Some(1), "not signaled");
        assert!(
            nv::EXEC_WAIT_US.load(Ordering::Relaxed) - waited >= 10_000_000,
            "the wait is accounted"
        );
        for h in [done, tl, out, never] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
    }

    #[test]
    fn exec_with_no_pushes_is_the_health_probe_that_waits_and_signals_without_the_ring() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        let ch = client_with_pushbuf(&gpu, A);
        let a = syncobj::create(true);
        let b = syncobj::create(false);
        let out = syncobj::create(false);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, A, ch, &[], &[sync(a)], &[sync(out), sync_tl(b, 9)]),
            Ok(0)
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0], "no RM call at all");
        assert_eq!(syncobj::query(out), Some(1));
        assert_eq!(syncobj::query(b), Some(9));
        assert_eq!(
            exec(&gpu, A, ch, &[], &[sync(0xdead_0002)], &[sync(out)]),
            Err(nv::ENOENT)
        );
        assert_eq!(
            exec(&gpu, A, ch, &[], &[], &[sync(0xdead_0003)]),
            Err(nv::ENOENT)
        );
        // Still a health probe: a wedged context answers ENODEV.
        nv::ctx_set_wedged(1);
        assert_eq!(exec(&gpu, A, ch, &[], &[], &[sync(out)]), Err(nv::ENODEV));
        nv::ctx_clear_wedged(1);
        assert_eq!(exec(&gpu, A, ch, &[], &[], &[sync_tl(out, 2)]), Ok(0));
        assert_eq!(syncobj::query(out), Some(2));
        for h in [a, b, out] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
    }

    #[test]
    fn exec_failures_of_the_ring_are_eio_and_only_a_lost_fence_wedges_the_context_until_exit() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm();
        let ch = client_with_pushbuf(&gpu, A);
        let out = syncobj::create(false);
        // A push outside the caller's bindings: the RM's lookup refuses it
        // (on hardware it would MMU-fault the channel).
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + 0x10000, 16)], &[], &[]),
            Err(nv::EIO)
        );
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + 0xfff8, 16)], &[], &[]),
            Err(nv::EIO),
            "crossing the end of the binding"
        );
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[
                    push(PUSH_VA, 16),
                    push(PUSH_VA + 0x10000, 16),
                    push(PUSH_VA, 16)
                ],
                &[],
                &[]
            ),
            Err(nv::EIO)
        );
        assert_eq!(
            FAKE_RM.lock().submits.len(),
            4,
            "stopped at the refused push; the third never went"
        );
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[
                    push(PUSH_VA, 16),
                    push(PUSH_VA + 0x10000, 16),
                    push(PUSH_VA, 16)
                ],
                &[],
                &[sync(out)]
            ),
            Err(nv::EIO),
            "the same with a fence on the last push"
        );
        assert_eq!(FAKE_RM.lock().submits.len(), 6);
        assert_eq!(syncobj::query(out), Some(0));
        // The ring is full: EIO too, and the fence was never asked for.
        FAKE_RM.lock().ring_full = true;
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Err(nv::EIO)
        );
        FAKE_RM.lock().ring_full = false;
        assert_eq!(syncobj::query(out), Some(0));
        assert!(!nv::ctx_is_wedged(1), "a refused submit is not a hang");
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0),
            "and the next one is fine"
        );
        // The push went in but its fence never lands: after the 1 s poll
        // (1 ms per clock read) the submit is EIO and the context is
        // latched WEDGED, so the client fast-fails from then on instead of
        // hanging the compositor's ring behind it.
        FAKE_RM.lock().fence_stalls = true;
        crate::nvme::nvme_queue::test_clock::set_auto_advance(1000);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync_tl(out, 2)]),
            Err(nv::EIO)
        );
        crate::nvme::nvme_queue::test_clock::set_auto_advance(0);
        FAKE_RM.lock().fence_stalls = false;
        assert_eq!(syncobj::query(out), Some(1), "not signaled");
        assert!(nv::ctx_is_wedged(1));
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
            Err(nv::EIO),
            "wedged: nothing more reaches the ring"
        );
        assert_eq!(exec(&gpu, A, ch, &[], &[], &[]), Err(nv::ENODEV));
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        // B is unaffected: its own context.
        client_with_pushbuf(&gpu, B);
        assert_eq!(exec(&gpu, B, 1, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        // A exits and comes back: the slot is clean again.
        gpu.nouveau_release_process(A);
        assert!(!nv::ctx_is_wedged(1));
        let ch = client_with_pushbuf(&gpu, A);
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert!(syncobj::destroy(out));
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    // ---- The direct-submit path -------------------------------------------
    //
    // With the fake's `fast` switch on, `exec_fast_prepare` hands the driver
    // a channel: a 64 KiB buffer (fence slot page, landing zone, GPFIFO
    // ring) and a USERD window, host memory the driver reaches through the
    // identity `phys_to_virt`. From then on an EXEC never enters the RM: it
    // writes GP entries and semaphore streams, bumps GPPut and pokes the
    // doorbell in BAR0. `run_gpu` is the PBDMA: it walks GPGet up to GPPut,
    // decodes each entry, executes the host semaphore methods (a RELEASE
    // writes its payload, an ACQUIRE stalls the channel until the payload
    // is there) and hands back what it fetched, in order.

    use crate::nvme::nvme_queue::test_clock;
    use std::time::Duration;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fetched {
        Push {
            va: u64,
            len: u32,
        },
        Release {
            sem_va: u64,
            payload: u32,
        },
        Acquire {
            sem_va: u64,
            payload: u32,
        },
        /// A semaphore at a VA the channel has no mapping for: the MMU
        /// fault that kills the channel on hardware.
        Fault {
            sem_va: u64,
        },
    }

    fn gpu_rm_fast() -> NvidiaGpu {
        let gpu = gpu_rm();
        FAKE_RM.lock().fast = true;
        gpu
    }

    fn chan(ctx: u32) -> FastChan {
        *FAKE_RM
            .lock()
            .fast_ctxs
            .iter()
            .find(|c| c.ctx == ctx)
            .expect("no direct-submit channel behind this context")
    }

    fn has_chan(ctx: u32) -> bool {
        FAKE_RM.lock().fast_ctxs.iter().any(|c| c.ctx == ctx)
    }

    fn peek(addr: usize) -> u32 {
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn poke(addr: usize, v: u32) {
        unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
    }

    /// `(GPGet, GPPut)` of a channel's USERD.
    fn userd(c: &FastChan) -> (u32, u32) {
        (peek(c.userd + 0x88), peek(c.userd + 0x8c))
    }

    fn doorbell(gpu: &NvidiaGpu) -> u32 {
        peek(gpu._bar0 + FAKE_DOORBELL as usize)
    }

    fn landing_zone(c: &FastChan) -> u32 {
        peek(c.buf + FAST_SEM_OFF as usize)
    }

    /// The channel's fence semaphore, as the GPU addresses it.
    fn sem_va(c: &FastChan) -> u64 {
        c.gpu_va + u64::from(FAST_SEM_OFF)
    }

    fn cpu_prep(gpu: &NvidiaGpu, handle: u32, pid: u64) -> Result<usize, i32> {
        let mut r = nv::DrmNouveauGemCpuPrep { handle, flags: 0 };
        call(
            gpu,
            wr::<nv::DrmNouveauGemCpuPrep>(nv::NR_GEM_CPU_PREP),
            &mut r,
            pid,
        )
    }

    /// The PBDMA of context `ctx`: fetch every entry from GPGet up to GPPut.
    fn run_gpu(ctx: u32) -> Vec<Fetched> {
        let c = chan(ctx);
        let mut out = Vec::new();
        loop {
            let (get, put) = userd(&c);
            if get == put {
                break;
            }
            let gp = c.buf + FAST_GPFIFO_OFF as usize + get as usize * 8;
            let (e0, e1) = (peek(gp), peek(gp + 4));
            let va = (u64::from(e1 & 0xff) << 32) | u64::from(e0);
            let len = ((e1 >> 10) & 0x1f_ffff) * 4;
            let streams = c.gpu_va + u64::from(FAST_PB_OFF)..c.gpu_va + u64::from(FAST_SEM_OFF);
            let item = if streams.contains(&va) {
                assert_eq!(len, 24, "a host semaphore stream is six dwords");
                let words = c.buf + (va - c.gpu_va) as usize;
                let w: [u32; 6] = core::array::from_fn(|i| peek(words + i * 4));
                assert_eq!(
                    w[0],
                    nv::push_hdr(0, nv::NVC46F_SEM_ADDR_LO, 5),
                    "SEM_ADDR_LO..SEM_EXECUTE, five methods, incrementing"
                );
                let sem_va = (u64::from(w[2] & 0xff) << 32) | u64::from(w[1]);
                assert_eq!(w[4], 0, "SEM_PAYLOAD_HI");
                let sem = if (c.gpu_va..c.gpu_va + 0x10000).contains(&sem_va) {
                    c.buf + (sem_va - c.gpu_va) as usize
                } else {
                    // Another channel's semaphore, through a peer mapping of
                    // this channel's -- or a VA nothing maps any more.
                    let producer = FAKE_RM
                        .lock()
                        .peer_maps
                        .iter()
                        .find(|m| m.0 == ctx && m.3 == sem_va)
                        .map(|m| m.1);
                    match producer.and_then(|p| {
                        FAKE_RM
                            .lock()
                            .fast_ctxs
                            .iter()
                            .find(|c| c.ctx == p)
                            .copied()
                    }) {
                        Some(pc) => pc.buf + FAST_SEM_OFF as usize,
                        None => {
                            out.push(Fetched::Fault { sem_va });
                            break;
                        }
                    }
                };
                let payload = w[3];
                match w[5] {
                    nv::NVC46F_SEM_EXECUTE_RELEASE => {
                        poke(sem, payload);
                        Fetched::Release { sem_va, payload }
                    }
                    nv::NVC46F_SEM_EXECUTE_ACQUIRE => {
                        if (peek(sem).wrapping_sub(payload) as i32) < 0 {
                            // The channel stalls here: GPGet stays.
                            break;
                        }
                        Fetched::Acquire { sem_va, payload }
                    }
                    other => panic!(
                        "SEM_EXECUTE {:#x} is neither a release nor an acquire",
                        other
                    ),
                }
            } else {
                Fetched::Push { va, len }
            };
            out.push(item);
            poke(c.userd + 0x88, (get + 1) % FAST_ENTRIES);
        }
        out
    }

    #[test]
    fn a_direct_submit_writes_the_ring_and_rings_the_doorbell_without_the_rm() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let fast = nv::EXEC_FAST_SUBMITS.load(Ordering::Relaxed);
        let legacy = nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 32)],
                &[],
                &[]
            ),
            Ok(0)
        );
        assert_eq!(
            rm_calls_since(before),
            ["exec_fast_prepare"],
            "the channel is prepared once; the submit itself never enters the RM"
        );
        assert!(FAKE_RM.lock().submits.is_empty());
        let c = chan(1);
        assert_eq!(
            userd(&c),
            (0, 2),
            "GPGet is the GPU's; GPPut past the two entries"
        );
        assert_eq!(
            doorbell(&gpu),
            c.token,
            "the channel's work token in the usermode doorbell"
        );
        // The raw entries, as the PBDMA reads them: GET in bits 31:2 of the
        // low word, GET_HI and the length in dwords in the high one.
        let gp = c.buf + FAST_GPFIFO_OFF as usize;
        assert_eq!(peek(gp), PUSH_VA as u32);
        assert_eq!(peek(gp + 4), (((PUSH_VA >> 32) as u32) & 0xff) | (4 << 10));
        assert_eq!(peek(gp + 12), (((PUSH_VA >> 32) as u32) & 0xff) | (8 << 10));
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Push {
                    va: PUSH_VA + 0x100,
                    len: 32
                }
            ]
        );
        assert_eq!(userd(&c), (2, 2));
        // The next EXEC: no prepare, the next slot.
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + 0x200, 8)], &[], &[]),
            Ok(0)
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        assert_eq!(userd(&c), (2, 3));
        assert_eq!(
            run_gpu(1),
            [Fetched::Push {
                va: PUSH_VA + 0x200,
                len: 8
            }]
        );
        assert_eq!(nv::EXEC_FAST_SUBMITS.load(Ordering::Relaxed), fast + 2);
        assert_eq!(nv::EXEC_LEGACY_SUBMITS.load(Ordering::Relaxed), legacy);
        // A second client: a channel of its own, and A's ring untouched.
        let ch_b = client_with_pushbuf(&gpu, B);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(rm_calls_since(before), ["exec_fast_prepare"]);
        let b = chan(2);
        assert_ne!(b.buf, c.buf);
        assert_eq!(userd(&b), (0, 1));
        assert_eq!(userd(&c), (3, 3));
        assert_eq!(doorbell(&gpu), b.token);
        assert_eq!(
            run_gpu(2),
            [Fetched::Push {
                va: PUSH_VA,
                len: 16
            }]
        );
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_syncobj_signals_only_when_the_gpu_reaches_the_fence_behind_the_pushes() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let bin = syncobj::create(false);
        let tl = syncobj::create(false);
        let fenced = nv::EXEC_FAST_FENCED.load(Ordering::Relaxed);
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16), push(PUSH_VA + 0x100, 32)],
                &[],
                &[sync(bin), sync_tl(tl, 5)]
            ),
            Ok(0),
            "returns at once: the fence is the GPU's to write"
        );
        let c = chan(1);
        assert_eq!(
            userd(&c),
            (0, 3),
            "two pushes and the fence entry behind them"
        );
        assert_eq!(landing_zone(&c), 0);
        // Submitted, not signaled: NVK asks for both.
        assert_eq!(syncobj::query_submitted(bin), Some(1));
        assert_eq!(syncobj::query_submitted(tl), Some(5));
        assert_eq!(syncobj::query(bin), Some(0));
        assert_eq!(syncobj::query(tl), Some(0));
        test_clock::set_auto_advance(100);
        let deadline = test_clock::now() + 5_000;
        assert!(
            matches!(
                syncobj::wait(&[bin, tl], Some(&[1, 5]), true, deadline),
                syncobj::WaitOutcome::Timeout
            ),
            "5 ms of waiting: the GPU has not run"
        );
        test_clock::set_auto_advance(0);
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Push {
                    va: PUSH_VA + 0x100,
                    len: 32
                },
                Fetched::Release {
                    sem_va: sem_va(&c),
                    payload: 1
                }
            ]
        );
        assert_eq!(landing_zone(&c), 1);
        assert_eq!(syncobj::query(bin), Some(1));
        assert_eq!(syncobj::query(tl), Some(5));
        assert!(!syncobj::has_pending());
        // The payloads are the channel's own sequence: the next fence is 2,
        // written into the same landing zone from the next slot's stream.
        let out = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&c),
                    payload: 2
                }
            ]
        );
        assert_eq!(syncobj::query(out), Some(1));
        // A signal on a handle nobody has: the work is on the ring by the
        // time the handle is looked up, and the ioctl says so.
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(0xdead_0000)]),
            Err(nv::ENOENT)
        );
        assert_eq!(userd(&c), (5, 7));
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&c),
                    payload: 3
                }
            ]
        );
        assert_eq!(nv::EXEC_FAST_FENCED.load(Ordering::Relaxed), fenced + 3);
        for h in [bin, tl, out] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_wait_on_the_same_channel_is_a_gpu_acquire_and_one_on_another_channel_a_cpu_wait() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch_a = client_with_pushbuf(&gpu, A);
        let ch_b = client_with_pushbuf(&gpu, B);
        let out = syncobj::create(false);
        let out2 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        let before = FAKE_RM.lock().calls.len();
        // 1 us per clock read: a CPU wait here (the wrong path, nothing
        // runs the GPU yet) ends in EIO after 10 s virtual, a failure
        // rather than a hang.
        test_clock::set_auto_advance(1);
        let t0 = test_clock::now();
        assert_eq!(
            exec(
                &gpu,
                A,
                ch_a,
                &[push(PUSH_VA + 0x100, 16)],
                &[sync(out)],
                &[sync(out2)]
            ),
            Ok(0),
            "the wait is a fence pending on this very channel: no CPU wait"
        );
        test_clock::set_auto_advance(0);
        assert!(test_clock::now() - t0 < 1_000, "submitted without waiting");
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        let a = chan(1);
        assert_eq!(userd(&a), (0, 5), "push, fence, acquire, push, fence");
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&a),
                    payload: 1
                },
                Fetched::Acquire {
                    sem_va: sem_va(&a),
                    payload: 1
                },
                Fetched::Push {
                    va: PUSH_VA + 0x100,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&a),
                    payload: 2
                }
            ],
            "the acquire sits in front of the push it guards"
        );
        assert_eq!(syncobj::query(out), Some(1));
        assert_eq!(syncobj::query(out2), Some(1));
        // B waits on A's fence. This RM cannot map A's semaphore into B's
        // VAS, so B's EXEC blocks on the CPU until A's GPU gets there, and
        // B's ring carries no acquire.
        let out3 = syncobj::create(false);
        let out4 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
            Ok(0)
        );
        let now = test_clock::now();
        let gpu_ref = &gpu;
        std::thread::scope(|s| {
            let t = s.spawn(move || {
                // 1 us per clock read: B's wait ends in EIO after 10 s
                // virtual should A's fence never land (a failure, not a
                // hang).
                test_clock::set(now);
                test_clock::set_auto_advance(1);
                exec(
                    gpu_ref,
                    B,
                    ch_b,
                    &[push(PUSH_VA, 16)],
                    &[sync(out3)],
                    &[sync(out4)],
                )
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(!t.is_finished(), "B is waiting for A's fence");
            assert!(
                !has_chan(2),
                "and has queued nothing yet: its channel is not even prepared"
            );
            assert_eq!(
                run_gpu(1),
                [
                    Fetched::Push {
                        va: PUSH_VA,
                        len: 16
                    },
                    Fetched::Release {
                        sem_va: sem_va(&a),
                        payload: 3
                    }
                ]
            );
            assert_eq!(t.join().unwrap(), Ok(0));
        });
        let b = chan(2);
        assert_eq!(
            run_gpu(2),
            [
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&b),
                    payload: 1
                }
            ],
            "no acquire on B's ring"
        );
        assert_eq!(syncobj::query(out4), Some(1));
        for h in [out, out2, out3, out4] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_full_ring_waits_for_the_gpu_and_one_that_never_drains_is_eio_and_wedges_the_channel() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        // One slot is always kept free, so 127 entries fill the 128-entry ring.
        for i in 0..127 {
            assert_eq!(
                exec(&gpu, A, ch, &[push(PUSH_VA + i * 16, 16)], &[], &[]),
                Ok(0),
                "entry {}",
                i
            );
        }
        let c = chan(1);
        assert_eq!(userd(&c), (0, 127));
        // The GPU drains the ring while the 128th submit waits for room:
        // the entry goes into the last slot and GPPut wraps to 0.
        let now = test_clock::now();
        // 1 us per clock read: the submit gives up (EIO) after 10 s virtual
        // should the GPU never make room, a failure rather than a hang.
        test_clock::set_auto_advance(1);
        let mut drained = std::thread::scope(|s| {
            let t = s.spawn(move || {
                test_clock::set(now);
                std::thread::sleep(Duration::from_millis(30));
                run_gpu(1)
            });
            assert_eq!(
                exec(&gpu, A, ch, &[push(PUSH_VA + 0x800, 16)], &[], &[]),
                Ok(0),
                "room came while the submit waited"
            );
            t.join().unwrap()
        });
        test_clock::set_auto_advance(0);
        assert_eq!(userd(&c).1, 0, "wrapped");
        // The GPU may or may not have reached the 128th entry before it
        // stopped: either way it is the last thing fetched.
        drained.extend(run_gpu(1));
        assert_eq!(drained.len(), 128);
        assert_eq!(
            drained[127],
            Fetched::Push {
                va: PUSH_VA + 0x800,
                len: 16
            }
        );
        assert_eq!(userd(&c), (0, 0));
        assert!(!nv::ctx_is_wedged(1));
        // Fill it again, and this time nothing drains it: after 10 s
        // (1 ms per clock read) the submit is EIO and the channel is
        // latched WEDGED, so the client fast-fails instead of hanging the
        // compositor's ring behind it.
        for i in 0..127 {
            assert_eq!(
                exec(&gpu, A, ch, &[push(PUSH_VA + i * 16, 16)], &[], &[]),
                Ok(0)
            );
        }
        assert_eq!(userd(&c), (0, 127));
        let out = syncobj::create(false);
        test_clock::set_auto_advance(1000);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Err(nv::EIO)
        );
        test_clock::set_auto_advance(0);
        assert!(nv::ctx_is_wedged(1));
        assert_eq!(userd(&c), (0, 127), "nothing was written over the ring");
        assert_eq!(syncobj::query(out), Some(0), "and nothing was attached");
        assert!(!syncobj::has_pending());
        // The GPU catching up later does not unlatch the context.
        assert_eq!(run_gpu(1).len(), 127);
        assert_eq!(userd(&c), (127, 127));
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
            Err(nv::EIO),
            "wedged until the process goes away"
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        assert_eq!(userd(&c), (127, 127));
        // B is unaffected: its own channel.
        let ch_b = client_with_pushbuf(&gpu, B);
        assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        // A exits and comes back: a fresh channel, ring at 0.
        gpu.nouveau_release_process(A);
        assert!(!nv::ctx_is_wedged(1));
        assert_eq!(FAKE_RM.lock().fast_releases, [1]);
        let ch = client_with_pushbuf(&gpu, A);
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        let c2 = chan(1);
        assert_ne!(c2.userd, c.userd);
        assert_eq!(userd(&c2), (0, 1));
        assert!(syncobj::destroy(out));
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn exit_releases_the_window_before_freeing_the_context_and_lets_go_of_the_fences_in_flight() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let out = syncobj::create(false);
        let tl = syncobj::create(false);
        assert_eq!(
            exec(
                &gpu,
                A,
                ch,
                &[push(PUSH_VA, 16)],
                &[],
                &[sync(out), sync_tl(tl, 4)]
            ),
            Ok(0)
        );
        assert_eq!(syncobj::query(out), Some(0));
        assert!(syncobj::has_pending());
        let before = FAKE_RM.lock().calls.len();
        gpu.nouveau_release_process(A);
        let calls = rm_calls_since(before);
        let released = calls
            .iter()
            .position(|c| *c == "exec_fast_release")
            .expect("the USERD window is unmapped at exit");
        let freed = calls
            .iter()
            .position(|c| *c == "ctx_free")
            .expect("the channel is freed at exit");
        assert!(
            released < freed,
            "the window goes before the channel it maps: {:?}",
            calls
        );
        assert_eq!(FAKE_RM.lock().fast_releases, [1]);
        assert!(FAKE_RM.lock().fast_ctxs.is_empty());
        // A fence that can never land now is signaled, as a killed channel's
        // would be: a compositor waiting on the dead client's buffer moves on.
        assert!(!syncobj::has_pending());
        assert_eq!(syncobj::query(out), Some(1));
        assert_eq!(syncobj::query(tl), Some(4));
        assert!(!nv::ctx_is_wedged(1), "abandoned, not timed out");
        // Back: a fresh window and ring.
        let ch = client_with_pushbuf(&gpu, A);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(rm_calls_since(before), ["exec_fast_prepare"]);
        assert_eq!(userd(&chan(1)), (0, 1));
        for h in [out, tl] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_setup_that_fails_pins_the_rm_path_and_still_releases_its_window_at_exit() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        // The SDK's DRF value for SEM_EXECUTE disagrees with the kernel's
        // encoder: the RM path, for this context, for good.
        FAKE_RM.lock().fast_bad_encoding = true;
        let ch = client_with_pushbuf(&gpu, A);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(
            rm_calls_since(before),
            ["exec_fast_prepare", "exec_submit"],
            "the self-check failed: this push goes through the RM"
        );
        let before = FAKE_RM.lock().calls.len();
        FAKE_RM.lock().fast_bad_encoding = false;
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(
            rm_calls_since(before),
            ["exec_submit"],
            "the verdict is pinned: no second prepare, even one that would pass"
        );
        assert!(
            has_chan(1),
            "the RM mapped the USERD before the check that failed"
        );
        // A exits. The window it never used still has to go: `ctx_free`
        // does not unmap it, and the RM refuses to prepare the next channel
        // behind an index whose window maps a freed one -- which would take
        // the direct path away from every later owner of this index.
        let before = FAKE_RM.lock().calls.len();
        gpu.nouveau_release_process(A);
        assert!(
            rm_calls_since(before).contains(&"exec_fast_release"),
            "{:?}",
            rm_calls_since(before)
        );
        assert!(!has_chan(1));
        let ch = client_with_pushbuf(&gpu, B);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(exec(&gpu, B, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(
            rm_calls_since(before),
            ["exec_fast_prepare"],
            "the next owner of the index gets the direct path"
        );
        assert_eq!(userd(&chan(1)), (0, 1));
        gpu.nouveau_release_process(B);
        // A prepare the RM refuses outright pins the RM path the same way.
        FAKE_RM.lock().fast = false;
        let ch = client_with_pushbuf(&gpu, A);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(rm_calls_since(before), ["exec_fast_prepare", "exec_submit"]);
        FAKE_RM.lock().fast = true;
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(rm_calls_since(before), ["exec_submit"]);
        gpu.nouveau_release_process(A);
        // `nvidia.exec_rm`: the direct path switched off never prepares at
        // all, and switching it back on prepares on the next EXEC.
        nv::set_exec_fast_enabled(false);
        let ch = client_with_pushbuf(&gpu, STRANGER);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, STRANGER, ch, &[push(PUSH_VA, 16)], &[], &[]),
            Ok(0)
        );
        assert_eq!(rm_calls_since(before), ["exec_submit"]);
        nv::set_exec_fast_enabled(true);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, STRANGER, ch, &[push(PUSH_VA, 16)], &[], &[]),
            Ok(0)
        );
        assert_eq!(rm_calls_since(before), ["exec_fast_prepare"]);
        gpu.nouveau_release_process(STRANGER);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn cpu_prep_waits_for_everything_queued_on_the_channel_behind_a_probe_fence_of_its_own() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
            .unwrap()
            .handle;
        // Nothing queued: nothing to wait for, and no channel is prepared
        // just to find that out.
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
        assert_eq!(cpu_prep(&gpu, h, A), Ok(0));
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        assert!(!has_chan(1));
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        let c = chan(1);
        assert_eq!(userd(&c), (0, 1));
        // NOWAIT with the push still queued: EBUSY at once, and a
        // fence-only entry behind the push is what will prove it finished.
        // The clock moves 1 us per read from here on, so a wait that
        // blocks shows up as virtual time, and a wait for a GPU that never
        // comes ends (EBUSY after 10 s virtual) instead of hanging.
        test_clock::set_auto_advance(1);
        let t0 = test_clock::now();
        assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::EBUSY));
        assert!(test_clock::now() - t0 < 1_000, "answered without waiting");
        assert_eq!(userd(&c), (0, 2), "a probe fence behind the push");
        assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::EBUSY));
        assert_eq!(userd(&c), (0, 3));
        // A blocking prep returns once the GPU has run past its probe.
        let now = test_clock::now();
        let sem = sem_va(&c);
        std::thread::scope(|s| {
            let t = s.spawn(move || {
                test_clock::set(now);
                let mut fetched = Vec::new();
                for _ in 0..5_000 {
                    fetched.extend(run_gpu(1));
                    if fetched.contains(&Fetched::Release {
                        sem_va: sem,
                        payload: 3,
                    }) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                fetched
            });
            assert_eq!(cpu_prep(&gpu, h, A), Ok(0));
            assert_eq!(
                t.join().unwrap(),
                [
                    Fetched::Push {
                        va: PUSH_VA,
                        len: 16
                    },
                    Fetched::Release {
                        sem_va: sem,
                        payload: 1
                    },
                    Fetched::Release {
                        sem_va: sem,
                        payload: 2
                    },
                    Fetched::Release {
                        sem_va: sem,
                        payload: 3
                    }
                ]
            );
        });
        assert_eq!(userd(&c), (4, 4));
        assert_eq!(landing_zone(&c), 3);
        // Idle: the last thing on the ring is a fence that landed. Neither
        // prep appends a probe, and NOWAIT is not busy.
        assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
        assert_eq!(cpu_prep(&gpu, h, A), Ok(0));
        assert_eq!(userd(&c), (4, 4), "no probe needed: the last one landed");
        // A push behind that fence makes the channel busy again. Nothing
        // runs the GPU: after 10 s (1 ms per clock read) the wait is EBUSY,
        // as Linux answers a reservation wait that timed out.
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(cpu_prep_nowait(&gpu, h, A), Err(nv::EBUSY));
        assert_eq!(userd(&c), (4, 6), "the push and its probe");
        // A GPU that arrives two real seconds late, an eternity next to
        // the 10 s virtual: only there so a wait that ignored its bound
        // would fail here instead of hanging the test.
        test_clock::set_auto_advance(1000);
        let now = test_clock::now();
        let (late, _) = std::thread::scope(|s| {
            let t = s.spawn(move || {
                test_clock::set(now);
                std::thread::sleep(Duration::from_secs(2));
                run_gpu(1)
            });
            (cpu_prep(&gpu, h, A), t.join().unwrap())
        });
        assert_eq!(late, Err(nv::EBUSY));
        test_clock::set_auto_advance(0);
        assert!(!nv::ctx_is_wedged(1), "a slow GPU is not a hung one");
        // A process without a channel of its own has queued nothing.
        let hs = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, STRANGER)
            .unwrap()
            .handle;
        assert_eq!(cpu_prep(&gpu, hs, STRANGER), Ok(0));
        assert_eq!(
            cpu_prep(&gpu, h, STRANGER),
            Err(nv::ENOENT),
            "not its buffer"
        );
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(STRANGER);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    // ---- The fence-timeout upcall -----------------------------------------
    //
    // A fence the GPU never writes is `syncobj`'s to give up on: after
    // `FENCE_TIMEOUT_US` it advances the point (so the waiter can fail on
    // its next probe instead of parking forever) and calls the driver back
    // with `(ctx, landing zone, payload, handle, point)`. The driver's side
    // of that call, `fast_fence_timeout`, is what these tests drive: the
    // values come from `pending_hw_fence`, exactly what `syncobj` would pass,
    // so the hook itself (registered once at boot, shared by every test
    // binary) stays out of the picture.

    /// The landing zone of context `ctx`, as the CPU (and `syncobj`) sees it.
    fn landing_zone_va(c: &FastChan) -> usize {
        c.buf + FAST_SEM_OFF as usize
    }

    #[test]
    fn a_fence_that_never_lands_wedges_the_channel_and_the_clients_next_submit_is_device_lost() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let ch_b = client_with_pushbuf(&gpu, B);
        let out = syncobj::create(false);
        let out_b = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out_b)]),
            Ok(0)
        );
        let c = chan(1);
        // What syncobj holds for A's fence names A's channel and its zone.
        let (fence_va, fence_gpu_va, payload, ctx) =
            syncobj::pending_hw_fence(out, 1).expect("A's fence is pending on the ring");
        assert_eq!(ctx, 1);
        assert_eq!(fence_va, landing_zone_va(&c));
        assert_eq!(fence_gpu_va, sem_va(&c));
        assert_eq!(payload, 1);
        // B's GPU runs; A's never does.
        assert_eq!(run_gpu(2).len(), 2);
        assert_eq!(syncobj::poll_pending(), 1);
        assert_eq!(syncobj::query(out_b), Some(1));
        test_clock::advance(crate::scheme::syncobj::FENCE_TIMEOUT_US - 1);
        assert_eq!(
            syncobj::poll_pending(),
            1,
            "one microsecond short of the timeout: still the GPU's"
        );
        assert_eq!(syncobj::query(out), Some(0));
        test_clock::advance(1);
        assert_eq!(syncobj::poll_pending(), 0, "given up on");
        assert_eq!(
            syncobj::query(out),
            Some(1),
            "released, so a waiter fails on its next probe instead of parking"
        );
        assert!(matches!(
            syncobj::wait(&[out], None, true, test_clock::now()),
            syncobj::WaitOutcome::Signaled { .. }
        ));
        assert_eq!(landing_zone(&c), 0, "nothing landed");
        assert!(
            !nv::ctx_is_wedged(1),
            "syncobj released the waiter; latching is the driver's, on the upcall"
        );
        // The upcall, as syncobj makes it.
        gpu.fast_fence_timeout(ctx, fence_va, payload, out, 1);
        assert!(nv::ctx_is_wedged(1));
        assert!(!nv::ctx_is_wedged(2), "B's channel is B's");
        assert_eq!(FAKE_RM.lock().bad, 0);
        // A's next submit: device lost, and nothing more reaches the ring.
        let (get, put) = userd(&c);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA + 0x100, 16)], &[], &[]),
            Err(nv::EIO)
        );
        assert_eq!(userd(&c), (get, put));
        assert_eq!(
            exec(&gpu, A, ch, &[], &[], &[]),
            Err(nv::ENODEV),
            "the health probe says killed"
        );
        // B keeps going.
        assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(run_gpu(2).len(), 1);
        // The GPU catching up later does not unlatch A.
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(landing_zone(&c), 1);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
            Err(nv::EIO)
        );
        // Exit clears it; A comes back on a fresh channel.
        gpu.nouveau_release_process(A);
        assert!(!nv::ctx_is_wedged(1));
        assert_eq!(FAKE_RM.lock().fast_releases, [1]);
        let ch = client_with_pushbuf(&gpu, A);
        assert_eq!(exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_ne!(chan(1).buf, c.buf);
        for h in [out, out_b] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_timeout_for_a_landing_zone_that_is_not_this_channels_does_not_wedge_it() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let out = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        let c = chan(1);
        let zone = landing_zone_va(&c);
        // A zone that is nobody's on this GPU (another GPU's, on a machine
        // with two: the hook fans out to every GPU).
        let elsewhere = 0u32;
        gpu.fast_fence_timeout(1, &elsewhere as *const u32 as usize, 1, out, 1);
        assert!(!nv::ctx_is_wedged(1));
        // A's zone named under another index: ctx 0 (the compositor, never
        // prepared here), one with no slot at all, and B's, not prepared yet.
        for idx in [0, 2, 40] {
            gpu.fast_fence_timeout(idx, zone, 1, out, 1);
            assert!(!nv::ctx_is_wedged(idx), "ctx{}", idx);
        }
        assert!(!nv::ctx_is_wedged(1));
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[]),
            Ok(0),
            "A is untouched"
        );
        // A goes away; a late call naming its old zone finds no channel.
        gpu.nouveau_release_process(A);
        gpu.fast_fence_timeout(1, zone, 1, out, 1);
        assert!(!nv::ctx_is_wedged(1));
        // B inherits the index with a zone of its own; A's old one is not it,
        // and a latch anything left on the index is not B's either.
        nv::ctx_set_wedged(1);
        let ch_b = client_with_pushbuf(&gpu, B);
        assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(ctx_of(&gpu, B), Some((1, true)));
        assert_ne!(landing_zone_va(&chan(1)), zone);
        gpu.fast_fence_timeout(1, zone, 1, out, 1);
        assert!(!nv::ctx_is_wedged(1), "A's stale timeout is not B's");
        assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        // Its own zone is.
        gpu.fast_fence_timeout(1, landing_zone_va(&chan(1)), 1, out, 1);
        assert!(nv::ctx_is_wedged(1));
        assert!(syncobj::destroy(out));
        gpu.nouveau_release_process(B);
        assert!(!nv::ctx_is_wedged(1));
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_client_that_exits_or_closes_its_syncobj_mid_frame_leaves_nothing_to_time_out() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let kept = syncobj::create(false);
        let closed = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(kept)]),
            Ok(0)
        );
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(closed)]),
            Ok(0)
        );
        let c = chan(1);
        assert_eq!(syncobj::poll_pending(), 2);
        // Closing the handle with its submit in flight takes the fence with
        // it: there is nothing left to time out ten seconds later, and a
        // client that drops a fence it stopped caring about (a resized
        // swapchain) is not a hung one.
        assert!(syncobj::destroy(closed));
        assert_eq!(syncobj::poll_pending(), 1);
        assert_eq!(
            syncobj::pending_hw_fence(kept, 1),
            Some((landing_zone_va(&c), sem_va(&c), 1, 1)),
            "the kept fence is still the GPU's"
        );
        // Exit mid-frame: the kept fence is abandoned (signaled, no timeout).
        gpu.nouveau_release_process(A);
        assert!(!syncobj::has_pending());
        assert_eq!(syncobj::query(kept), Some(1));
        assert!(!nv::ctx_is_wedged(1));
        test_clock::advance(crate::scheme::syncobj::FENCE_TIMEOUT_US + 1);
        assert_eq!(syncobj::poll_pending(), 0);
        assert!(!nv::ctx_is_wedged(1));
        // B on the same index in the meantime: no ghost from A reaches it.
        let ch_b = client_with_pushbuf(&gpu, B);
        assert_eq!(exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[]), Ok(0));
        assert_eq!(ctx_of(&gpu, B), Some((1, true)));
        assert_eq!(syncobj::poll_pending(), 0);
        assert!(!nv::ctx_is_wedged(1));
        assert!(syncobj::destroy(kept));
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn cpu_prep_on_a_wedged_channel_answers_at_once_and_writes_nothing_to_the_jammed_ring() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        let ch = client_with_pushbuf(&gpu, A);
        let h = gem_new_rm(&gpu, 4096, nv::NOUVEAU_GEM_DOMAIN_GART, A)
            .unwrap()
            .handle;
        let out = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        let c = chan(1);
        let (fence_va, _, payload, ctx) = syncobj::pending_hw_fence(out, 1).unwrap();
        test_clock::advance(crate::scheme::syncobj::FENCE_TIMEOUT_US);
        assert_eq!(syncobj::poll_pending(), 0);
        gpu.fast_fence_timeout(ctx, fence_va, payload, out, 1);
        assert!(nv::ctx_is_wedged(1));
        assert_eq!(userd(&c), (0, 2));
        let bell = doorbell(&gpu);
        // The clock moves 1 ms per read from here on: a prep that waits shows
        // up as virtual time, and one that waits for a GPU that never comes
        // ends (EBUSY after 10 s virtual) instead of hanging.
        test_clock::set_auto_advance(1_000);
        let t0 = test_clock::now();
        assert_eq!(
            cpu_prep(&gpu, h, A),
            Ok(0),
            "a killed channel's fences are done: nouveau says 0, the next submit says why"
        );
        assert!(
            test_clock::now() - t0 < 100_000,
            "answered at once, not after the timeout"
        );
        assert_eq!(cpu_prep_nowait(&gpu, h, A), Ok(0));
        test_clock::set_auto_advance(0);
        assert_eq!(
            userd(&c),
            (0, 2),
            "no probe fence on a ring the GPU stopped reading"
        );
        assert_eq!(doorbell(&gpu), bell);
        assert!(!syncobj::has_pending());
        assert_eq!(
            exec(&gpu, A, ch, &[], &[], &[]),
            Err(nv::ENODEV),
            "and the truth comes from the probe"
        );
        assert!(syncobj::destroy(out));
        gpu.nouveau_release_process(A);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    // ---- Waits across channels ---------------------------------------------
    //
    // A wait on another client's fence is a GPU ACQUIRE too, once the RM has
    // mapped the producer's semaphore into the consumer's VAS
    // (`map_peer_fence`: the compositor waiting on a client's frame, a client
    // waiting on the compositor's release). With the fake's `peer` switch on,
    // the shim hands out one consumer VA per (consumer, producer) pair and
    // remembers it until either context is freed, as the C does; `run_gpu`
    // resolves an ACQUIRE at such a VA to the producer's landing zone, and an
    // ACQUIRE at a VA the channel has no mapping for is the MMU fault it
    // would be on hardware.

    fn peer_map(consumer: u32, producer: u32) -> Option<(u64, u64)> {
        FAKE_RM
            .lock()
            .peer_maps
            .iter()
            .find(|m| m.0 == consumer && m.1 == producer)
            .map(|m| (m.2, m.3))
    }

    fn peer_maps_made() -> usize {
        FAKE_RM
            .lock()
            .calls
            .iter()
            .filter(|c| **c == "map_peer_fence")
            .count()
    }

    #[test]
    fn a_wait_on_another_channel_is_a_gpu_acquire_on_the_producers_fence_mapped_into_the_consumer()
    {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        FAKE_RM.lock().peer = true;
        let ch_a = client_with_pushbuf(&gpu, A);
        let ch_b = client_with_pushbuf(&gpu, B);
        let out = syncobj::create(false);
        let out2 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        let a = chan(1);
        // 1 ms per clock read from here on: a CPU wait anywhere below (the
        // wrong path, nothing runs the GPU) ends after 10 s virtual instead
        // of hanging.
        test_clock::set_auto_advance(1_000);
        let t0 = test_clock::now();
        assert_eq!(
            exec(
                &gpu,
                B,
                ch_b,
                &[push(PUSH_VA, 16)],
                &[sync(out)],
                &[sync(out2)]
            ),
            Ok(0),
            "the wait is the GPU's: no CPU wait"
        );
        assert!(
            test_clock::now() - t0 < 1_000_000,
            "submitted without waiting"
        );
        assert_eq!(
            peer_maps_made(),
            1,
            "A's semaphore mapped into B's VAS once"
        );
        let (producer_va, local_va) = peer_map(2, 1).expect("the mapping the RM made");
        assert_eq!(producer_va, sem_va(&a), "of A's fence semaphore");
        let b = chan(2);
        assert_ne!(local_va, sem_va(&b));
        assert_eq!(userd(&b), (0, 3), "acquire, push, fence");
        assert_eq!(
            run_gpu(2),
            [],
            "B's channel stalls on the acquire: A has not run"
        );
        assert_eq!(userd(&b), (0, 3));
        assert_eq!(syncobj::query(out2), Some(0));
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(
            run_gpu(2),
            [
                Fetched::Acquire {
                    sem_va: local_va,
                    payload: 1
                },
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                },
                Fetched::Release {
                    sem_va: sem_va(&b),
                    payload: 1
                }
            ],
            "the acquire in front of the push it guards, on B's own ring"
        );
        assert_eq!(syncobj::query(out2), Some(1));
        // The next wait on A reuses the mapping: nothing more from the RM.
        let out3 = syncobj::create(false);
        let out4 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
            Ok(0)
        );
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(
                &gpu,
                B,
                ch_b,
                &[push(PUSH_VA, 16)],
                &[sync(out3)],
                &[sync(out4)]
            ),
            Ok(0)
        );
        assert_eq!(rm_calls_since(before), [] as [&str; 0]);
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(
            run_gpu(2)[0],
            Fetched::Acquire {
                sem_va: local_va,
                payload: 2
            }
        );
        assert_eq!(syncobj::query(out4), Some(1));
        // The other way round is a mapping of its own.
        let out5 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out5)]),
            Ok(0)
        );
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out5)], &[]),
            Ok(0)
        );
        assert_eq!(rm_calls_since(before), ["map_peer_fence"]);
        let (producer_va_b, local_va_b) = peer_map(1, 2).unwrap();
        assert_eq!(producer_va_b, sem_va(&b));
        assert_ne!(local_va_b, local_va);
        assert_eq!(run_gpu(2).len(), 2);
        assert_eq!(
            run_gpu(1),
            [
                Fetched::Acquire {
                    sem_va: local_va_b,
                    payload: 3
                },
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                }
            ]
        );
        test_clock::set_auto_advance(0);
        for h in [out, out2, out3, out4, out5] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
        assert!(
            FAKE_RM.lock().peer_maps.is_empty(),
            "both mappings involve A: gone with its context"
        );
        gpu.nouveau_release_process(B);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_context_that_goes_away_takes_its_peer_mappings_with_it_and_the_next_tenant_gets_fresh_ones(
    ) {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_fast();
        FAKE_RM.lock().peer = true;
        // 1 ms per clock read: every wait below is meant to be the GPU's;
        // one that falls to the CPU ends after 10 s virtual, not never.
        test_clock::set_auto_advance(1_000);
        let ch_a = client_with_pushbuf(&gpu, A);
        let ch_b = client_with_pushbuf(&gpu, B);
        let out = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out)], &[]),
            Ok(0)
        );
        let (_, stale) = peer_map(2, 1).unwrap();
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(run_gpu(2).len(), 2);
        // A exits: the RM frees the mapping of A's buffer in B's VAS along
        // with A's context. A comes back on the same index with a new
        // channel; B waiting on it needs a mapping of THAT buffer -- an
        // acquire at the old VA is an MMU fault on B's channel (the
        // compositor's, on the desktop: one client come and gone and the
        // next one's frame kills the compositor).
        gpu.nouveau_release_process(A);
        assert_eq!(peer_map(2, 1), None);
        let ch_a = client_with_pushbuf(&gpu, A);
        let out2 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out2)]),
            Ok(0)
        );
        let a2 = chan(1);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
            Ok(0)
        );
        assert_eq!(
            rm_calls_since(before),
            ["map_peer_fence"],
            "a mapping of the new tenant's buffer"
        );
        let (producer_va, fresh) = peer_map(2, 1).unwrap();
        assert_eq!(producer_va, sem_va(&a2));
        assert_ne!(fresh, stale);
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(
            run_gpu(2),
            [
                Fetched::Acquire {
                    sem_va: fresh,
                    payload: 1
                },
                Fetched::Push {
                    va: PUSH_VA,
                    len: 16
                }
            ],
            "the acquire resolves on the new channel's landing zone"
        );
        // The consumer going away is the same: B's VAS is gone, and the
        // next B on that index needs a mapping in ITS VAS.
        gpu.nouveau_release_process(B);
        assert_eq!(peer_map(2, 1), None);
        let ch_b = client_with_pushbuf(&gpu, B);
        let out3 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
            Ok(0)
        );
        let made = peer_maps_made();
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out3)], &[]),
            Ok(0)
        );
        assert_eq!(peer_maps_made(), made + 1);
        let (_, newer) = peer_map(2, 1).unwrap();
        assert_ne!(newer, fresh);
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(
            run_gpu(2)[0],
            Fetched::Acquire {
                sem_va: newer,
                payload: 2
            }
        );
        test_clock::set_auto_advance(0);
        for h in [out, out2, out3] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        assert!(FAKE_RM.lock().peer_maps.is_empty());
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    // ----- The compositor's singleton: claimed through the ladder, reset at exit -----

    /// A GPU whose fake RM carries the compositor's ladder too, so ctx 0 is
    /// claimed the way the desktop claims it: through `CHANNEL_ALLOC`.
    fn gpu_rm_ladder() -> NvidiaGpu {
        let gpu = gpu_rm_fast();
        gpu.ctx0_owner.store(0, Ordering::Release);
        FAKE_RM.lock().ladder = true;
        gpu
    }

    fn ctx0_owner(gpu: &NvidiaGpu) -> u64 {
        gpu.ctx0_owner.load(Ordering::Acquire)
    }

    fn ctx0_resets() -> u32 {
        FAKE_RM.lock().ctx0_resets
    }

    fn step17_builds() -> u32 {
        FAKE_RM.lock().step17_builds
    }

    #[test]
    fn the_compositor_claims_ctx0_through_the_ladder_once_and_keeps_it_across_its_throwaway_channels(
    ) {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_ladder();
        assert_eq!(ctx0_owner(&gpu), 0);
        // The ladder refusing at either step, or stopping part-way: no
        // channel, no claim, nothing built behind index 0.
        FAKE_RM.lock().fail_step16 = true;
        assert_eq!(
            channel_alloc(&gpu, COMP).map(|c| c.channel),
            Err(nv::ENODEV)
        );
        FAKE_RM.lock().fail_step16 = false;
        FAKE_RM.lock().incomplete_step16 = true;
        assert_eq!(
            channel_alloc(&gpu, COMP).map(|c| c.channel),
            Err(nv::ENODEV),
            "a ladder with no context share"
        );
        FAKE_RM.lock().incomplete_step16 = false;
        FAKE_RM.lock().fail_step17 = true;
        assert_eq!(
            channel_alloc(&gpu, COMP).map(|c| c.channel),
            Err(nv::ENODEV)
        );
        FAKE_RM.lock().fail_step17 = false;
        FAKE_RM.lock().incomplete_step17 = true;
        assert_eq!(
            channel_alloc(&gpu, COMP).map(|c| c.channel),
            Err(nv::ENODEV),
            "a channel that never got scheduled"
        );
        FAKE_RM.lock().incomplete_step17 = false;
        assert_eq!(ctx0_owner(&gpu), 0, "nothing claimed");
        assert_eq!(rm_backed_channels(&gpu, COMP), 0);
        assert_eq!(step17_builds(), 0);
        assert!(!FAKE_RM.lock().ctxs.contains(&0));
        // The first compositor channel: the ladder, the singleton channel,
        // the notifier cached for the failure path, and the sticky claim.
        nv::set_chan_notifier_pa(0);
        assert_eq!(nv::chan_notifier_pa_cached(), None);
        let before = FAKE_RM.lock().calls.len();
        let c = channel_alloc(&gpu, COMP).unwrap();
        assert_eq!(c.channel, 0);
        assert_eq!(
            c.notifier_handle, 0x6000,
            "the singleton channel's notifier"
        );
        assert_eq!(ctx0_owner(&gpu), COMP);
        assert_eq!(
            ctx_of(&gpu, COMP),
            None,
            "no client context: the compositor IS context 0"
        );
        assert_eq!(rm_backed_channels(&gpu, COMP), 1);
        let calls = rm_calls_since(before);
        assert_eq!(&calls[..2], ["step16", "step17"]);
        assert!(calls.contains(&"chan_notifier_pa"));
        assert!(!calls.contains(&"ctx_alloc"), "no client context built");
        assert_eq!(nv::chan_notifier_pa_cached(), Some(notifier_pa()));
        // labwc runs two Vulkan instances: the second channel of the same
        // pid rides the cached ladder.
        assert_eq!(channel_alloc(&gpu, COMP).unwrap().channel, 1);
        assert_eq!(step17_builds(), 1, "the ladder is a singleton");
        assert_eq!(rm_backed_channels(&gpu, COMP), 2);
        // A client meanwhile is a client: a context of its own.
        assert_eq!(channel_alloc(&gpu, A).unwrap().notifier_handle, 0x6001);
        assert_eq!(ctx_of(&gpu, A), Some((1, true)));
        // The compositor freeing every channel it has (NVK's throwaway
        // enumeration context, in both instances) keeps the sticky role:
        // the next client is still a client, and the compositor's next
        // channel is still ctx 0, on the same singleton channel.
        assert_eq!(channel_free(&gpu, 0, COMP), Ok(0));
        assert_eq!(channel_free(&gpu, 1, COMP), Ok(0));
        assert_eq!(rm_backed_channels(&gpu, COMP), 0);
        assert_eq!(ctx0_owner(&gpu), COMP, "sticky");
        assert_eq!(channel_alloc(&gpu, B).unwrap().notifier_handle, 0x6002);
        assert_eq!(ctx_of(&gpu, B), Some((2, true)));
        let again = channel_alloc(&gpu, COMP).unwrap();
        assert_eq!(again.notifier_handle, 0x6000);
        assert_eq!(ctx_of(&gpu, COMP), None);
        assert_eq!(step17_builds(), 1);
        assert_eq!(ctx0_resets(), 0, "a CHANNEL_FREE is not an exit");
        // And its submits go down the singleton's own ring, direct, from a
        // buffer bound in the ladder's VAS.
        let h = gem_new_rm(&gpu, 65536, nv::NOUVEAU_GEM_DOMAIN_GART, COMP)
            .unwrap()
            .handle;
        assert_eq!(
            vm_bind_ops(&gpu, COMP, &mut [map(h, PUSH_VA, 65536)]),
            Ok(0)
        );
        assert_eq!(
            FAKE_RM
                .lock()
                .maps_of_ctx(0)
                .iter()
                .map(|m| (m.0, m.1))
                .collect::<Vec<_>>(),
            [(PUSH_VA, 65536)],
            "bound in context 0"
        );
        let out = syncobj::create(false);
        assert_eq!(
            exec(
                &gpu,
                COMP,
                again.channel as u32,
                &[push(PUSH_VA, 16)],
                &[],
                &[sync(out)]
            ),
            Ok(0)
        );
        let ring = run_gpu(0);
        assert_eq!(ring.len(), 2, "the push and its fence");
        assert_eq!(
            ring[0],
            Fetched::Push {
                va: PUSH_VA,
                len: 16
            }
        );
        assert!(syncobj::destroy(out));
        gpu.nouveau_release_process(A);
        gpu.nouveau_release_process(B);
        gpu.nouveau_release_process(COMP);
        assert_eq!(ctx0_owner(&gpu), 0);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn the_compositors_exit_gives_ctx0_back_and_the_respawn_gets_a_fresh_channel_and_fresh_peer_mappings(
    ) {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_ladder();
        FAKE_RM.lock().peer = true;
        // 1 ms per clock read: every wait below is meant to be the GPU's;
        // one that falls to the CPU ends after 10 s virtual, not never.
        test_clock::set_auto_advance(1_000);
        let ch_c = client_with_pushbuf(&gpu, COMP);
        assert_eq!(ctx0_owner(&gpu), COMP);
        let ch_a = client_with_pushbuf(&gpu, A);
        // A frame each way: the compositor waits on the client's, and the
        // client on the compositor's (the buffer's release).
        let out = syncobj::create(false);
        let out2 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[], &[sync(out)]),
            Ok(0)
        );
        assert_eq!(
            exec(
                &gpu,
                COMP,
                ch_c,
                &[push(PUSH_VA, 16)],
                &[sync(out)],
                &[sync(out2)]
            ),
            Ok(0)
        );
        assert_eq!(
            exec(&gpu, A, ch_a, &[push(PUSH_VA, 16)], &[sync(out2)], &[]),
            Ok(0)
        );
        assert_eq!(peer_maps_made(), 2, "one mapping each way");
        let (_, stale) = peer_map(0, 1).unwrap();
        let (_, stale_a) = peer_map(1, 0).unwrap();
        assert_eq!(run_gpu(1).len(), 2);
        assert_eq!(run_gpu(0).len(), 3, "acquire, push, fence");
        assert_eq!(run_gpu(1).len(), 2, "acquire, push");
        let old = chan(0);
        // The compositor dies mid-session. Its window is released before
        // the channel behind it, the singleton is torn down, the role is
        // free, and the RM has dropped every mapping context 0 took part
        // in -- both directions.
        let before = FAKE_RM.lock().calls.len();
        gpu.nouveau_release_process(COMP);
        assert_eq!(ctx0_owner(&gpu), 0, "the role is free again");
        let calls = rm_calls_since(before);
        let released = calls
            .iter()
            .position(|c| *c == "exec_fast_release")
            .expect("the window released");
        let reset = calls
            .iter()
            .position(|c| *c == "ctx0_reset")
            .expect("the singleton reset");
        assert!(released < reset, "the window before the channel behind it");
        assert_eq!(ctx0_resets(), 1);
        assert!(!has_chan(0));
        assert_eq!(peer_map(0, 1), None);
        assert_eq!(peer_map(1, 0), None);
        // The RM dropped both mappings with the channel; so must the driver's
        // cache, or a client that outlives the compositor is waited on --
        // and waits -- through the VAs the RM has already freed.
        assert!(
            gpu.nouveau_peer_fence
                .lock()
                .keys()
                .all(|&(consumer, producer)| consumer != 0 && producer != 0),
            "no mapping of context 0 is remembered past its reset"
        );
        // The clients go with it (their socket is gone)...
        gpu.nouveau_release_process(A);
        // ...and it comes back. Its CHANNEL_ALLOC rebuilds the singleton
        // channel: a new ring, a new window, not the dead compositor's. A
        // new client takes the dead one's index, and the two wait on each
        // other through mappings of the NEW channels' fence pages.
        let ch_c = client_with_pushbuf(&gpu, COMP2);
        assert_eq!(ctx0_owner(&gpu), COMP2);
        assert_eq!(step17_builds(), 2, "a new channel behind index 0");
        let ch_b = client_with_pushbuf(&gpu, B);
        assert_eq!(ctx_of(&gpu, B), Some((1, true)), "the dead client's index");
        let out3 = syncobj::create(false);
        let out4 = syncobj::create(false);
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[], &[sync(out3)]),
            Ok(0)
        );
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(
                &gpu,
                COMP2,
                ch_c,
                &[push(PUSH_VA, 16)],
                &[sync(out3)],
                &[sync(out4)]
            ),
            Ok(0)
        );
        assert_ne!(chan(0).buf, old.buf, "a fresh window");
        assert!(
            rm_calls_since(before).contains(&"map_peer_fence"),
            "the client's fence mapped into the new compositor's VAS"
        );
        let (producer_va, fresh) = peer_map(0, 1).expect("a mapping the RM holds");
        assert_eq!(producer_va, sem_va(&chan(1)));
        assert_ne!(fresh, stale);
        let before = FAKE_RM.lock().calls.len();
        assert_eq!(
            exec(&gpu, B, ch_b, &[push(PUSH_VA, 16)], &[sync(out4)], &[]),
            Ok(0)
        );
        assert!(
            rm_calls_since(before).contains(&"map_peer_fence"),
            "and the new compositor's fence into the client's"
        );
        let (producer_va, fresh_a) = peer_map(1, 0).expect("a mapping the RM holds");
        assert_eq!(producer_va, sem_va(&chan(0)), "of the NEW channel's fence");
        assert_ne!(fresh_a, stale_a);
        assert_eq!(run_gpu(1).len(), 2);
        let ring = run_gpu(0);
        assert_eq!(ring.len(), 3, "acquire, push, fence: no fault");
        assert!(
            matches!(ring[0], Fetched::Acquire { sem_va, .. } if sem_va == fresh),
            "the acquire resolves on the client's landing zone: {:?}",
            ring[0]
        );
        let ring = run_gpu(1);
        assert_eq!(ring.len(), 2, "acquire, push: no fault");
        assert!(
            matches!(ring[0], Fetched::Acquire { sem_va, .. } if sem_va == fresh_a),
            "the acquire resolves on the new compositor's landing zone: {:?}",
            ring[0]
        );
        test_clock::set_auto_advance(0);
        for h in [out, out2, out3, out4] {
            assert!(syncobj::destroy(h));
        }
        gpu.nouveau_release_process(B);
        gpu.nouveau_release_process(COMP2);
        assert_eq!(ctx0_owner(&gpu), 0);
        assert_eq!(ctx0_resets(), 2);
        assert!(FAKE_RM.lock().peer_maps.is_empty());
        assert_eq!(FAKE_RM.lock().bad, 0);
    }

    #[test]
    fn a_compositor_that_freed_its_channels_before_exiting_still_gives_ctx0_back() {
        let _g = LOCK.lock();
        let _live = LiveBytes::hold();
        let gpu = gpu_rm_ladder();
        let c = channel_alloc(&gpu, COMP).unwrap().channel;
        assert_eq!(ctx0_owner(&gpu), COMP);
        // A client freeing its channel and leaving is its own business.
        let ca = channel_alloc(&gpu, A).unwrap().channel;
        assert_eq!(channel_free(&gpu, ca, A), Ok(0));
        gpu.nouveau_release_process(A);
        assert_eq!(ctx0_owner(&gpu), COMP);
        assert_eq!(ctx0_resets(), 0);
        // A clean compositor exit: NVK destroys its contexts (CHANNEL_FREE)
        // before the device closes, so by the time the process goes there
        // is no channel of its own left in the table to infer the role
        // from. The role is the sticky claim, and that is what exits.
        assert_eq!(channel_free(&gpu, c, COMP), Ok(0));
        assert_eq!(rm_backed_channels(&gpu, COMP), 0);
        gpu.nouveau_release_process(COMP);
        assert_eq!(ctx0_owner(&gpu), 0, "the singleton is given back");
        assert_eq!(ctx0_resets(), 1, "and its channel torn down");
        // The respawn is the compositor again: ctx 0 on a rebuilt channel,
        // not a GL client on a context of its own beside a singleton the
        // dead one still holds.
        let c2 = channel_alloc(&gpu, COMP2).unwrap();
        assert_eq!(c2.notifier_handle, 0x6000);
        assert_eq!(ctx0_owner(&gpu), COMP2);
        assert_eq!(ctx_of(&gpu, COMP2), None);
        assert_eq!(step17_builds(), 2);
        // A stranger's exit, or the same exit twice, resets nothing more.
        gpu.nouveau_release_process(STRANGER);
        assert_eq!(ctx0_resets(), 1);
        assert_eq!(ctx0_owner(&gpu), COMP2);
        gpu.nouveau_release_process(COMP2);
        assert_eq!(ctx0_resets(), 2);
        gpu.nouveau_release_process(COMP2);
        assert_eq!(ctx0_resets(), 2);
        assert_eq!(ctx0_owner(&gpu), 0);
        assert_eq!(FAKE_RM.lock().bad, 0);
    }
}

/// The RM entry points the host test binary has no C code for: every
/// `eclipse_rm_*` the `nvidia-rm-sys` crate declares, generated from its
/// `extern "C"` blocks. The hardware ladder (GSP, display, CE) answers
/// `NV_ERR_NOT_SUPPORTED`: a test GPU never reaches it. What a GL client's
/// own path goes through -- context build and prime, GEM alloc/map/free,
/// VM_BIND map/unmap, class objects, submission -- and, behind `ladder`,
/// the compositor's step16/17 singleton and its reset, are a small stateful
/// fake (`FakeRm`), so `VM_BIND`, `GEM_NEW`, `EXEC` and the RM-backed
/// `CHANNEL_ALLOC` can be exercised on the host, with the RM's real
/// refusals (a VA already taken, an object it never handed out).
/// Global to the test binary, like the `drivers_*` shims in `net/e1000e.rs`.
#[cfg(test)]
mod rm_host_shims {
    const NV_ERR_NOT_SUPPORTED: u32 = 0x56;

    #[no_mangle]
    extern "C" fn eclipse_rm_attach_gpu(
        _a0: u32,
        _a1: u8,
        _a2: u8,
        _a3: u64,
        _a4: *mut u8,
        _a5: u64,
        _a6: u64,
        _a7: u64,
        _a8: u64,
        _a9: u64,
        _a10: *mut u8,
    ) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_bench(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_blit(_a0: u32, _a1: u64, _a2: u64, _a3: u64, _a4: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_blit_p2p(
        _a0: u32,
        _a1: u64,
        _a2: u64,
        _a3: u64,
        _a4: *mut u8,
    ) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_blit_p2p_2d(
        _a0: u32,
        _a1: u64,
        _a2: u32,
        _a3: u64,
        _a4: u32,
        _a5: u32,
        _a6: u32,
        _a7: *mut u8,
    ) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_fill_fb(_a0: u32, _a1: u64, _a2: u64, _a3: u32) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_fill_fb_p2p(_a0: u32, _a1: u64, _a2: u64, _a3: u32) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_release_inflight() -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ce_wait(_a0: u32, _a1: u64) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    /// The singleton channel's error notifier (`NvNotification`, 16 bytes).
    /// Real memory: the EXEC failure path reads it through `phys_to_virt`,
    /// the identity here, with no RM call.
    static NOTIFIER: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];
    pub(super) fn notifier_pa() -> u64 {
        NOTIFIER.as_ptr() as u64
    }
    /// Cached at `CHANNEL_ALLOC` for that failure path; there is one only
    /// while step 17's channel stands.
    #[no_mangle]
    extern "C" fn eclipse_rm_chan_notifier_pa(_inst: u32, out: *mut u64) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("chan_notifier_pa");
        if !f.chan_built {
            return NV_ERR_INVALID_STATE;
        }
        unsafe { *out = notifier_pa() };
        NV_OK
    }
    /// Tear down the singleton channel and clear step 17's cache, so the
    /// next `step17` builds a new channel behind index 0; the ladder's VAS
    /// stays. Drops every peer-fence mapping context 0 takes part in, as
    /// the C does. A no-op before step 17, also as the C.
    #[no_mangle]
    extern "C" fn eclipse_rm_ctx0_reset(_inst: u32) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("ctx0_reset");
        if !f.chan_built {
            return NV_OK;
        }
        f.chan_built = false;
        f.ctx0_resets += 1;
        f.ctxs.retain(|c| *c != 0);
        f.peer_maps.retain(|m| m.0 != 0 && m.1 != 0);
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_edid(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    /// A direct-submit channel the fake handed out: the pages the driver
    /// writes GP entries, semaphore streams and GPPut into and reads GPGet
    /// from. Real memory, since `phys_to_virt` is the identity here.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct FastChan {
        pub ctx: u32,
        /// 64 KiB: the fence slot page at `FAST_PB_OFF`, the landing zone
        /// at `FAST_SEM_OFF`, the GPFIFO ring at `FAST_GPFIFO_OFF`.
        pub buf: usize,
        /// 256 B USERD window: GPGet at 0x88, GPPut at 0x8c.
        pub userd: usize,
        /// Where `buf` is bound in the context's VAS.
        pub gpu_va: u64,
        pub token: u32,
        /// Which build of the context this window belongs to: the RM
        /// refuses a window left over from a channel that was freed.
        build: u32,
    }

    /// BAR0 offset of the usermode doorbell the fake reports.
    pub(super) const FAKE_DOORBELL: u32 = 0x0081_0000;
    pub(super) const FAST_PB_OFF: u32 = 0xA000;
    pub(super) const FAST_SEM_OFF: u32 = 0xB000;
    pub(super) const FAST_GPFIFO_OFF: u32 = 0xC000;
    pub(super) const FAST_ENTRIES: u32 = 128;
    pub(super) const FAST_SLOT_BYTES: u32 = 32;
    /// `NV_ERR_INVALID_STATE`: what the C side answers for a channel that
    /// is not ready, and for a USERD window it mapped for a channel that
    /// has since been freed.
    const NV_ERR_INVALID_STATE: u32 = 0x40;

    /// The constant half of a direct submit, once per context, the way
    /// `eclipse_rm_exec_fast_prepare` computes it: `NV_OK` with the
    /// verdict in `status`, as the C side does past its argument checks.
    #[no_mangle]
    extern "C" fn eclipse_rm_exec_fast_prepare(
        _inst: u32,
        ctx_idx: u32,
        out: *mut ExecFast,
    ) -> u32 {
        use super::super::nouveau_uapi::{gp_entry0, gp_entry1, sem_release_stream};
        use nvidia_rm_sys::rm_init::{EXEC_FAST_CHK_LEN, EXEC_FAST_CHK_VA};
        let mut f = FAKE_RM.lock();
        f.calls.push("exec_fast_prepare");
        if !f.fast {
            return NV_ERR_NOT_SUPPORTED;
        }
        let build = f.ctx_build.iter().find(|b| b.0 == ctx_idx).map(|b| b.1);
        let chan = match (f.ctxs.contains(&ctx_idx), build) {
            (true, Some(build)) => match f.fast_ctxs.iter().find(|c| c.ctx == ctx_idx) {
                Some(c) if c.build == build => Some(*c),
                // The window still maps the USERD of a channel that was
                // freed: the RM refuses rather than poke freed BAR1.
                Some(_) => None,
                None => {
                    let buf = alloc::boxed::Box::leak(alloc::vec![0u64; 8192].into_boxed_slice())
                        .as_ptr() as usize;
                    let userd = alloc::boxed::Box::leak(alloc::vec![0u64; 32].into_boxed_slice())
                        .as_ptr() as usize;
                    let c = FastChan {
                        ctx: ctx_idx,
                        buf,
                        userd,
                        gpu_va: 0x7000_0000 + (u64::from(ctx_idx) << 20),
                        token: 0xC0DE_0000 | ctx_idx,
                        build,
                    };
                    f.fast_ctxs.push(c);
                    Some(c)
                }
            },
            _ => None,
        };
        let result = if let Some(c) = chan {
            let stream = sem_release_stream(EXEC_FAST_CHK_VA, 0);
            let sem_execute = if f.fast_bad_encoding {
                stream[5] ^ (1 << 20)
            } else {
                stream[5]
            };
            ExecFast {
                status: NV_OK,
                work_token: c.token,
                runlist_id: 7,
                userd_size: 0x100,
                userd_cpu: c.userd as u64,
                fence_pb_phys: (c.buf + FAST_PB_OFF as usize) as u64,
                fence_sem_phys: (c.buf + FAST_SEM_OFF as usize) as u64,
                gpfifo_phys: (c.buf + FAST_GPFIFO_OFF as usize) as u64,
                buf_gpu_va: c.gpu_va,
                gpfifo_entries: FAST_ENTRIES,
                doorbell_reg: FAKE_DOORBELL,
                fence_pb_off: FAST_PB_OFF,
                fence_sem_off: FAST_SEM_OFF,
                gpfifo_off: FAST_GPFIFO_OFF,
                slot_bytes: FAST_SLOT_BYTES,
                chk_gp_entry0: gp_entry0(EXEC_FAST_CHK_VA),
                chk_gp_entry1: gp_entry1(EXEC_FAST_CHK_VA, EXEC_FAST_CHK_LEN),
                chk_sem_hdr: stream[0],
                chk_sem_addr_hi: stream[2],
                chk_sem_execute: sem_execute,
                userd_gpget_off: 0x88,
                userd_gpput_off: 0x8c,
            }
        } else {
            ExecFast {
                status: NV_ERR_INVALID_STATE,
                ..ExecFast::default()
            }
        };
        unsafe { *out = result };
        NV_OK
    }
    /// Drop the window of `ctx_idx`; a no-op when there is none, as in C.
    #[no_mangle]
    extern "C" fn eclipse_rm_exec_fast_release(_inst: u32, ctx_idx: u32) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("exec_fast_release");
        if let Some(i) = f.fast_ctxs.iter().position(|c| c.ctx == ctx_idx) {
            f.fast_ctxs.remove(i);
            f.fast_releases.push(ctx_idx);
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_get_gsp_info(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hdmi_audio(
        _a0: u32,
        _a1: u32,
        _a2: u32,
        _a3: u8,
        _a4: *mut u8,
    ) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwcursor_hide(_a0: u32) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwcursor_image(_a0: u32, _a1: *const u8, _a2: u32, _a3: u32) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwcursor_init(_a0: u32, _a1: u32, _a2: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwcursor_move(_a0: i32, _a1: i32) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwflip_init(_a0: u32, _a1: u32, _a2: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwflip_ready() -> u8 {
        0
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_hwflip_surface(
        _a0: u32,
        _a1: u32,
        _a2: u64,
        _a3: u32,
        _a4: u32,
        _a5: u32,
    ) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_init_core() -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_init_gsp(_a0: u32, _a1: *const u8, _a2: u32) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_intr_table(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    /// The consumer VAs `map_peer_fence` hands out start here: above every
    /// channel's own buffer, and each mapping ever made gets its own, so a
    /// stale one can never alias a fresh one by accident.
    const PEER_VA_BASE: u64 = 0x7800_0000;
    #[no_mangle]
    extern "C" fn eclipse_rm_map_peer_fence(
        _inst: u32,
        consumer: u32,
        producer: u32,
        producer_va: u64,
        out: *mut u64,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("map_peer_fence");
        if !f.peer {
            return NV_ERR_NOT_SUPPORTED;
        }
        if consumer == producer {
            unsafe { *out = producer_va };
            return NV_OK;
        }
        // Cached for the life of both contexts, whatever VA is asked for
        // now: the C compares nothing but the pair.
        if let Some(m) = f
            .peer_maps
            .iter()
            .find(|m| m.0 == consumer && m.1 == producer)
        {
            unsafe { *out = m.3 };
            return NV_OK;
        }
        // The producer's buffer and the consumer's VAS must both exist.
        if !f.fast_ctxs.iter().any(|c| c.ctx == producer) || !f.ctxs.contains(&consumer) {
            return NV_ERR_INVALID_STATE;
        }
        f.peer_maps_made += 1;
        let local = PEER_VA_BASE + (u64::from(f.peer_maps_made) << 16) + u64::from(FAST_SEM_OFF);
        f.peer_maps.push((consumer, producer, producer_va, local));
        unsafe { *out = local };
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_mark_console_gpu(_a0: u32, _a1: u64, _a2: u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_state_init(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step10(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step15(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    /// The VAS of the compositor's ladder: context 0's.
    pub(super) const LADDER_H_VAS: u32 = 0x5000;
    /// Step 16, the compositor's allocation ladder (client, device,
    /// subdevice, VAS, TSG, context share). Idempotent: a repeat call
    /// answers the cached, still-alive allocation.
    #[no_mangle]
    extern "C" fn eclipse_rm_step16(_inst: u32, out: *mut GrAlloc) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("step16");
        if !f.ladder {
            return NV_ERR_NOT_SUPPORTED;
        }
        if f.fail_step16 {
            return NV_ERR_INVALID_STATE;
        }
        let ctxshare_status = if f.incomplete_step16 {
            NV_ERR_INVALID_STATE
        } else {
            f.ladder_built = true;
            NV_OK
        };
        unsafe {
            *out = GrAlloc {
                client_status: 0,
                device_status: 0,
                subdev_status: 0,
                vas_status: 0,
                tsg_status: 0,
                ctxshare_status,
                h_client: 0x4000,
                h_device: 0x4001,
                h_subdevice: 0x4002,
                h_vas: LADDER_H_VAS,
                h_tsg: 0x4004,
                h_ctxshare: if ctxshare_status == NV_OK { 0x4005 } else { 0 },
            };
        }
        NV_OK
    }
    /// Step 17 on the cached ladder: the singleton channel of context 0.
    /// Idempotent until `ctx0_reset`; a rebuild is a new channel behind
    /// index 0, so a direct-submit window of the old one is refused.
    #[no_mangle]
    extern "C" fn eclipse_rm_step17(_inst: u32, out: *mut GrChannel) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("step17");
        if !f.ladder {
            return NV_ERR_NOT_SUPPORTED;
        }
        if !f.ladder_built || f.fail_step17 {
            return NV_ERR_INVALID_STATE;
        }
        let sched_status = if f.incomplete_step17 {
            NV_ERR_INVALID_STATE
        } else {
            if !f.chan_built {
                f.chan_built = true;
                f.step17_builds += 1;
                if !f.ctxs.contains(&0) {
                    f.ctxs.push(0);
                }
                if let Some(b) = f.ctx_build.iter_mut().find(|b| b.0 == 0) {
                    b.1 += 1;
                } else {
                    f.ctx_build.push((0, 1));
                }
            }
            NV_OK
        };
        unsafe {
            *out = GrChannel {
                userd_status: 0,
                buf_status: 0,
                virt_status: 0,
                map_status: 0,
                notif_status: 0,
                chan_status: 0,
                compute_status: 0,
                sched_status,
                h_userd: 0x4100,
                h_phys_buf: 0x4101,
                h_virt_buf: 0x4102,
                h_notifier: 0x6000,
                h_channel: 0x7000,
                h_compute: 0x4106,
                channel_class: 0xc46f,
                userd_size: 0x100,
                buf_gpu_va: 0x8_0000,
            };
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step18(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step19(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step20(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step21(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step22(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step23(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_step8(_a0: u32, _a1: *mut u8) -> u32 {
        NV_ERR_NOT_SUPPORTED
    }

    // ----- A fake RM with state, for the arms that cannot run without one -----
    //
    // Enough of `eclipse_rm_*` to walk the path a GL client takes: a context
    // per pid (`ctx_alloc`/`ctx_prime`/`ctx_free`), GEM memory
    // (`gem_alloc`/`gem_map_cpu`/`gem_fbmem_offset`/`gem_free`), VA bindings
    // (`vm_bind_map`/`vm_bind_unmap`) and engine classes
    // (`class_alloc`/`class_free`). It refuses what the real RM refuses (a
    // fixed VA over a live range of the same VAS, a memory handle it never
    // handed out) and counts every free or unmap of something it does not
    // hold, which on hardware is a use-after-free inside the vendor RM. The
    // compositor's own ladder (`step16`/`step17`) is not faked: the
    // compositor's channel keeps answering ENODEV, and every test client is
    // a GL client with a context of its own.
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU32, Ordering};
    use nvidia_rm_sys::rm_init::{
        CtxAlloc, ExecFast, ExecSignal, ExecSubmit, GemAlloc, GemMapCpu, GrAlloc, GrChannel,
        VmBind, ADDR_SYSMEM,
    };

    const NV_OK: u32 = 0;
    const NV_ERR_BUSY_RETRY: u32 = nvidia_rm_sys::types::NV_ERR_BUSY_RETRY;

    /// The channel's fence semaphore. The driver polls it through
    /// `phys_to_virt(fence_sem_phys)`, which is the identity in this binary,
    /// so its "physical" address is simply its address.
    static FENCE_SEM: AtomicU32 = AtomicU32::new(0);
    /// `NV_ERR_OBJECT_NOT_FOUND`.
    const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;
    /// What the RM's eheap answers to a fixed-address allocation over a
    /// live range: the status the VM_BIND replace semantics were written for.
    const RM_VA_TAKEN: u32 = 0x51;

    pub(super) struct FakeRm {
        next: u32,
        /// Live context indices.
        pub ctxs: Vec<u32>,
        pub primed: Vec<u32>,
        /// `(h_memory, size, sysmem)`.
        pub gems: Vec<(u32, u64, bool)>,
        /// The host memory behind each sysmem object: `(h_memory, address)`.
        /// Real, because `phys_to_virt` is the identity here and the driver
        /// reads the first pushbuffer through it.
        bufs: Vec<(u32, usize)>,
        /// Every submit, in order: `(ctx, push_va, push_len, fence payload)`.
        pub submits: Vec<(u32, u64, u32, Option<u32>)>,
        /// `(h_virt, ctx, h_memory, va, size, bo_offset, pte_kind)`.
        pub maps: Vec<(u32, u32, u32, u64, u64, u64, u32)>,
        /// `(h_object, ctx, class)`.
        pub classes: Vec<(u32, u32, u32)>,
        pub unmaps: usize,
        pub gem_frees: usize,
        pub ctx_frees: usize,
        pub class_frees: usize,
        /// Frees and unmaps of something this RM does not hold.
        pub bad: usize,
        /// Every entry point in call order.
        pub calls: Vec<&'static str>,
        pub fail_ctx_alloc: bool,
        /// `ctx_alloc` returns `NV_OK` with a non-zero `sched_status`: the
        /// C side's shape for a ladder that failed part-way. It has already
        /// freed every handle it made, so nothing of this context lives.
        pub incomplete_ctx: bool,
        pub fail_prime: bool,
        /// `ctx_free` refuses: the context, and its VAS, stay alive.
        pub fail_ctx_free: bool,
        /// `gem_alloc` returns `NV_OK` with `alloc_status` set: the C side's
        /// shape for an allocation the RM refused.
        pub fail_gem_alloc: bool,
        /// `gem_map_cpu` answers with an aperture that is not system memory:
        /// the object exists but the CPU cannot map it.
        pub map_cpu_elsewhere: bool,
        pub refuse_map: bool,
        pub refuse_class: bool,
        /// The GPFIFO has no room: `submit_status = NV_ERR_BUSY_RETRY`.
        pub ring_full: bool,
        /// The submit goes through but the fence never lands.
        pub fence_stalls: bool,
        /// `exec_fast_prepare` hands out a channel instead of
        /// `NV_ERR_NOT_SUPPORTED`: EXEC takes the direct-submit path.
        pub fast: bool,
        /// The SDK's `SEM_EXECUTE` disagrees with the kernel's encoder.
        pub fast_bad_encoding: bool,
        /// The direct-submit channels handed out and not released.
        pub fast_ctxs: Vec<FastChan>,
        /// Every `exec_fast_release` that found a window, in order.
        pub fast_releases: Vec<u32>,
        /// `(ctx, build)`: how many times each index was built.
        ctx_build: Vec<(u32, u32)>,
        /// `map_peer_fence` maps instead of answering `NV_ERR_NOT_SUPPORTED`.
        pub peer: bool,
        /// The live peer-fence mappings: `(consumer, producer, producer VA,
        /// consumer VA)`. Dropped with either context, as the C does.
        pub peer_maps: Vec<(u32, u32, u64, u64)>,
        /// Mappings ever made: each gets a consumer VA of its own.
        peer_maps_made: u32,
        /// `step16`/`step17` build the compositor's ladder instead of
        /// answering `NV_ERR_NOT_SUPPORTED`.
        pub ladder: bool,
        pub fail_step16: bool,
        /// `step16` returns `NV_OK` with the context share unallocated:
        /// the C side's shape for a ladder that stopped part-way.
        pub incomplete_step16: bool,
        pub fail_step17: bool,
        /// `step17` returns `NV_OK` with the channel never scheduled.
        pub incomplete_step17: bool,
        /// Step 16 ran to the end (`g_grAllocDone`).
        ladder_built: bool,
        /// Step 17's channel stands (`g_grChanDone`): cleared by `ctx0_reset`.
        pub chan_built: bool,
        /// How many channels step 17 built behind index 0.
        pub step17_builds: u32,
        /// How many times `ctx0_reset` found a channel to tear down.
        pub ctx0_resets: u32,
    }

    const EMPTY_RM: FakeRm = FakeRm {
        next: 0x100,
        ctxs: Vec::new(),
        primed: Vec::new(),
        gems: Vec::new(),
        bufs: Vec::new(),
        submits: Vec::new(),
        maps: Vec::new(),
        classes: Vec::new(),
        unmaps: 0,
        gem_frees: 0,
        ctx_frees: 0,
        class_frees: 0,
        bad: 0,
        calls: Vec::new(),
        fail_ctx_alloc: false,
        incomplete_ctx: false,
        fail_prime: false,
        fail_ctx_free: false,
        fail_gem_alloc: false,
        map_cpu_elsewhere: false,
        refuse_map: false,
        refuse_class: false,
        ring_full: false,
        fence_stalls: false,
        fast: false,
        fast_bad_encoding: false,
        fast_ctxs: Vec::new(),
        fast_releases: Vec::new(),
        ctx_build: Vec::new(),
        peer: false,
        peer_maps: Vec::new(),
        peer_maps_made: 0,
        ladder: false,
        fail_step16: false,
        incomplete_step16: false,
        fail_step17: false,
        incomplete_step17: false,
        ladder_built: false,
        chan_built: false,
        step17_builds: 0,
        ctx0_resets: 0,
    };

    pub(super) static FAKE_RM: lock::Mutex<FakeRm> = lock::Mutex::new(EMPTY_RM);

    pub(super) fn reset_fake_rm() {
        *FAKE_RM.lock() = EMPTY_RM;
    }

    impl FakeRm {
        fn fresh(&mut self) -> u32 {
            self.next += 1;
            self.next
        }

        fn gem(&self, h_memory: u32) -> Option<(u32, u64, bool)> {
            self.gems.iter().copied().find(|g| g.0 == h_memory)
        }

        /// Whether `[va, va + size)` meets a live range of context `ctx`.
        fn va_taken(&self, ctx: u32, va: u64, size: u64) -> bool {
            self.maps
                .iter()
                .any(|m| m.1 == ctx && m.3 < va.wrapping_add(size) && va < m.3.wrapping_add(m.4))
        }

        /// The host address of a sysmem object's memory.
        pub fn pa_of(&self, h_memory: u32) -> Option<u64> {
            self.bufs
                .iter()
                .find(|b| b.0 == h_memory)
                .map(|b| b.1 as u64)
        }

        /// Whether `[va, va + len)` lies inside one binding of context
        /// `ctx`: what the RM's own lookup answers before it rings the
        /// doorbell.
        fn push_mapped(&self, ctx: u32, va: u64, len: u32) -> bool {
            self.maps.iter().any(|m| {
                m.1 == ctx && va >= m.3 && va.wrapping_add(u64::from(len)) <= m.3.wrapping_add(m.4)
            })
        }

        /// One submit through the fake: the RM's lookup of the push VA in
        /// the context's VAS, then the ring. Returns the four stage statuses
        /// of `ExecSubmit` the way the C side reports them: a stage that
        /// was never reached stays at `0xFFFF_FFFF`.
        fn submit(&mut self, ctx: u32, va: u64, len: u32, payload: Option<u32>) -> [u32; 4] {
            const UNREACHED: u32 = 0xFFFF_FFFF;
            self.submits.push((ctx, va, len, payload));
            if !self.push_mapped(ctx, va, len) {
                return [NV_ERR_OBJECT_NOT_FOUND, UNREACHED, UNREACHED, UNREACHED];
            }
            if self.ring_full {
                return [0, 0, 0, NV_ERR_BUSY_RETRY];
            }
            [0, 0, 0, 0]
        }

        pub fn maps_of_ctx(&self, ctx: u32) -> Vec<(u64, u64, u32)> {
            self.maps
                .iter()
                .filter(|m| m.1 == ctx)
                .map(|m| (m.3, m.4, m.6))
                .collect()
        }
    }

    #[no_mangle]
    extern "C" fn eclipse_rm_ctx_alloc(_inst: u32, ctx_idx: u32, out: *mut CtxAlloc) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("ctx_alloc");
        if f.fail_ctx_alloc {
            return NV_ERR_NOT_SUPPORTED;
        }
        let sched_status = if f.incomplete_ctx {
            NV_ERR_NOT_SUPPORTED
        } else {
            if !f.ctxs.contains(&ctx_idx) {
                f.ctxs.push(ctx_idx);
                // A new channel behind this index: the USERD of the old one
                // went with it.
                if let Some(b) = f.ctx_build.iter_mut().find(|b| b.0 == ctx_idx) {
                    b.1 += 1;
                } else {
                    f.ctx_build.push((ctx_idx, 1));
                }
            }
            NV_OK
        };
        unsafe {
            *out = CtxAlloc {
                vas_status: 0,
                tsg_status: 0,
                ctxshare_status: 0,
                userd_status: 0,
                buf_status: 0,
                virt_status: 0,
                map_status: 0,
                notif_status: 0,
                chan_status: 0,
                compute_status: 0,
                sched_status,
                h_vas: 0x5000 + ctx_idx,
                h_tsg: 0,
                h_ctxshare: 0,
                h_userd: 0,
                h_phys_buf: 0,
                h_virt_buf: 0,
                h_notifier: 0x6000 + ctx_idx,
                h_channel: 0x7000 + ctx_idx,
                h_compute: 0,
                channel_class: 0xc46f,
                userd_size: 0,
                buf_gpu_va: 0x10_0000 * u64::from(ctx_idx),
            };
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ctx_prime(_inst: u32, ctx_idx: u32) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("ctx_prime");
        if f.fail_prime {
            return NV_ERR_NOT_SUPPORTED;
        }
        f.primed.push(ctx_idx);
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_ctx_free(_inst: u32, ctx_idx: u32) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("ctx_free");
        if f.fail_ctx_free {
            return NV_ERR_NOT_SUPPORTED;
        }
        // Destroying the VAS takes every binding in it with it.
        if let Some(pos) = f.ctxs.iter().position(|c| *c == ctx_idx) {
            f.ctxs.remove(pos);
            f.maps.retain(|m| m.1 != ctx_idx);
            // And every peer-fence mapping it takes part in, as the C does.
            f.peer_maps.retain(|m| m.0 != ctx_idx && m.1 != ctx_idx);
            f.ctx_frees += 1;
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_gem_alloc(
        _inst: u32,
        size: u64,
        sysmem: u32,
        out: *mut GemAlloc,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("gem_alloc");
        if f.fail_gem_alloc {
            unsafe {
                *out = GemAlloc {
                    alloc_status: NV_ERR_NOT_SUPPORTED,
                    h_memory: 0,
                };
            }
            return NV_OK;
        }
        let h = f.fresh();
        f.gems.push((h, size, sysmem != 0));
        if sysmem != 0 {
            let buf = alloc::boxed::Box::leak(alloc::vec![0u8; size as usize].into_boxed_slice());
            f.bufs.push((h, buf.as_ptr() as usize));
        }
        unsafe {
            *out = GemAlloc {
                alloc_status: 0,
                h_memory: h,
            };
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_gem_map_cpu(_inst: u32, h_memory: u32, out: *mut GemMapCpu) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("gem_map_cpu");
        let elsewhere = f.map_cpu_elsewhere;
        let r = match f.gem(h_memory) {
            Some((_, size, true)) if elsewhere => GemMapCpu {
                lookup_status: 0,
                address_space: 2,
                phys_addr: 0xdead_0000,
                size,
            },
            // A host PA only for system memory; vidmem is FBMEM (0).
            Some((h, size, true)) => GemMapCpu {
                lookup_status: 0,
                address_space: ADDR_SYSMEM,
                phys_addr: f.pa_of(h).expect("a sysmem object has memory"),
                size,
            },
            Some((_, _, false)) => GemMapCpu {
                lookup_status: 0,
                address_space: 0,
                phys_addr: 0,
                size: 0,
            },
            None => GemMapCpu {
                lookup_status: NV_ERR_OBJECT_NOT_FOUND,
                address_space: 0,
                phys_addr: 0,
                size: 0,
            },
        };
        unsafe { *out = r };
        NV_OK
    }
    /// The host PA the fake gives a sysmem object.
    /// The FBMEM offset the fake gives a vidmem object.
    pub(super) fn fake_fbmem_offset(h_memory: u32) -> u64 {
        u64::from(h_memory) << 20
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_gem_fbmem_offset(
        _inst: u32,
        h_memory: u32,
        p_offset: *mut u64,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("gem_fbmem_offset");
        match f.gem(h_memory) {
            Some((h, _, false)) => {
                unsafe { *p_offset = fake_fbmem_offset(h) };
                NV_OK
            }
            _ => NV_ERR_NOT_SUPPORTED,
        }
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_gem_free(_inst: u32, h_memory: u32) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("gem_free");
        match f.gems.iter().position(|g| g.0 == h_memory) {
            Some(pos) => {
                f.gems.remove(pos);
                f.gem_frees += 1;
                NV_OK
            }
            None => {
                f.bad += 1;
                NV_ERR_OBJECT_NOT_FOUND
            }
        }
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_vm_bind_map(
        _inst: u32,
        ctx_idx: u32,
        h_memory: u32,
        size: u64,
        requested_va: u64,
        bo_offset: u64,
        pte_kind: u32,
        out: *mut VmBind,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("vm_bind_map");
        let r = if f.gem(h_memory).is_none() {
            VmBind {
                virt_status: 0,
                map_status: NV_ERR_OBJECT_NOT_FOUND,
                h_virt: 0,
                actual_va: 0,
            }
        } else if f.refuse_map || f.va_taken(ctx_idx, requested_va, size) {
            VmBind {
                virt_status: RM_VA_TAKEN,
                map_status: RM_VA_TAKEN,
                h_virt: 0,
                actual_va: 0,
            }
        } else {
            let h_virt = f.fresh();
            f.maps.push((
                h_virt,
                ctx_idx,
                h_memory,
                requested_va,
                size,
                bo_offset,
                pte_kind,
            ));
            VmBind {
                virt_status: 0,
                map_status: 0,
                h_virt,
                actual_va: requested_va,
            }
        };
        unsafe { *out = r };
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_vm_bind_unmap(_inst: u32, h_virt: u32, _size: u64, _va: u64) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("vm_bind_unmap");
        f.unmaps += 1;
        match f.maps.iter().position(|m| m.0 == h_virt) {
            Some(pos) => {
                f.maps.remove(pos);
                NV_OK
            }
            None => {
                f.bad += 1;
                NV_ERR_OBJECT_NOT_FOUND
            }
        }
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_class_alloc(
        _inst: u32,
        ctx_idx: u32,
        class_id: u32,
        h_object: *mut u32,
        alloc_status: *mut u32,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("class_alloc");
        if f.refuse_class {
            unsafe {
                *h_object = 0;
                *alloc_status = NV_ERR_NOT_SUPPORTED;
            }
            return NV_OK;
        }
        let h = f.fresh();
        f.classes.push((h, ctx_idx, class_id));
        unsafe {
            *h_object = h;
            *alloc_status = 0;
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_class_free(_inst: u32, h_object: u32) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("class_free");
        match f.classes.iter().position(|c| c.0 == h_object) {
            Some(pos) => {
                f.classes.remove(pos);
                f.class_frees += 1;
                NV_OK
            }
            None => {
                f.bad += 1;
                NV_ERR_OBJECT_NOT_FOUND
            }
        }
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_exec_submit(
        _inst: u32,
        ctx_idx: u32,
        push_va: u64,
        push_len: u32,
        out: *mut ExecSubmit,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("exec_submit");
        let [lookup, map, token, submit] = f.submit(ctx_idx, push_va, push_len, None);
        unsafe {
            *out = ExecSubmit {
                lookup_status: lookup,
                map_status: map,
                token_status: token,
                submit_status: submit,
                work_token: 0x1000 + ctx_idx,
                runlist_id: 0,
                gp_put_after: f.submits.len() as u32,
            };
        }
        NV_OK
    }
    #[no_mangle]
    extern "C" fn eclipse_rm_exec_submit_signaled(
        _inst: u32,
        ctx_idx: u32,
        push_va: u64,
        push_len: u32,
        fence_payload: u32,
        _timeout_ms: u32,
        out: *mut ExecSignal,
    ) -> u32 {
        let mut f = FAKE_RM.lock();
        f.calls.push("exec_submit_signaled");
        let [lookup, map, token, submit] =
            f.submit(ctx_idx, push_va, push_len, Some(fence_payload));
        let submitted = lookup == 0 && submit == 0;
        // The GPU "runs" the push before the call returns: the fence lands
        // unless told to stall. The driver polls it itself (async path).
        if submitted && !f.fence_stalls {
            FENCE_SEM.store(fence_payload, Ordering::Release);
        }
        unsafe {
            *out = ExecSignal {
                lookup_status: lookup,
                map_status: map,
                token_status: token,
                submit_status: submit,
                fence_submit_status: if submitted { 0 } else { 0xFFFF_FFFF },
                fence_wait_status: 0xFFFF_FFFF,
                fence_value: 0,
                work_token: 0x1000 + ctx_idx,
                runlist_id: 0,
                fence_sem_phys: if submitted {
                    &FENCE_SEM as *const AtomicU32 as u64
                } else {
                    0
                },
            };
        }
        NV_OK
    }
}
