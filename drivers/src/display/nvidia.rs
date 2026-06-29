use alloc::string::String;
use alloc::vec::Vec;

use crate::bus::pci_drivers::PciDriver;
use crate::prelude::{AccelCaps, ColorFormat, DisplayInfo, FrameBuffer};
use crate::utils::dma::DmaRegion;
use crate::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane, GemHandle};
use crate::scheme::{DisplayScheme, DrmScheme, Scheme};
use crate::{builder::IoMapper, Device, DeviceError, DeviceResult};
use alloc::sync::Arc;
use lock::Mutex;
use pci::{PCIDevice, BAR};

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
    pub const PMC_BOOT0_CHIPID_HOPPER_MIN: u32 = 0x1B0;
    pub const PMC_BOOT0_CHIPID_HOPPER_MAX: u32 = 0x1BF;
    pub const PMC_BOOT0_CHIPID_BLACKWELL_MIN: u32 = 0x200;

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
    // Channel / FIFO structures (filled in later steps).
    inst: DmaRegion,
    userd: DmaRegion,
    gpfifo: DmaRegion,
    pushbuf: DmaRegion,
    sem: DmaRegion,
    src: DmaRegion,
    dst: DmaRegion,
    /// Base GPU virtual address of the packed 2 MiB region.
    va_base: u64,
}

impl GpuBringup {
    /// Allocate the memory plan and build the GMMU page tables in RAM. No GPU
    /// register is touched here — only sysmem is written, so this is safe to run
    /// on demand. Returns `None` if the coherent DMA allocator is exhausted.
    fn build(va_base: u64) -> Option<Self> {
        let root = DmaRegion::alloc_coherent(0x1000)?;
        let pd3 = DmaRegion::alloc_coherent(0x1000)?;
        let pd2 = DmaRegion::alloc_coherent(0x1000)?;
        let pd0 = DmaRegion::alloc_coherent(0x1000)?;
        let spt = DmaRegion::alloc_coherent(0x1000)?;
        let inst = DmaRegion::alloc_coherent(0x1000)?;
        let userd = DmaRegion::alloc_coherent(0x1000)?;
        let gpfifo = DmaRegion::alloc_coherent(0x1000)?;
        let pushbuf = DmaRegion::alloc_coherent(0x1000)?;
        let sem = DmaRegion::alloc_coherent(0x1000)?;
        let src = DmaRegion::alloc_coherent(0x1000)?;
        let dst = DmaRegion::alloc_coherent(0x1000)?;

        // Pack the four engine-visible buffers into one 2 MiB region:
        //   src=+0x0  dst=+0x1000  sem=+0x2000  pushbuffer=+0x3000
        let src_va = va_base;
        let dst_va = va_base + 0x1000;
        let sem_va = va_base + 0x2000;
        let pb_va = va_base + 0x3000;

        // Leaf PTEs (SPT). idx = (va>>12)&0x1ff; the four pages occupy idx 0..3.
        let wr64 = |r: &DmaRegion, i: usize, v: u64| unsafe {
            core::ptr::write_volatile(r.as_ptr::<u64>().add(i), v)
        };
        wr64(&spt, ((src_va >> 12) & 0x1ff) as usize, gmmu::encode_pte_sys(src.paddr() as u64));
        wr64(&spt, ((dst_va >> 12) & 0x1ff) as usize, gmmu::encode_pte_sys(dst.paddr() as u64));
        wr64(&spt, ((sem_va >> 12) & 0x1ff) as usize, gmmu::encode_pte_sys(sem.paddr() as u64));
        wr64(&spt, ((pb_va >> 12) & 0x1ff) as usize, gmmu::encode_pte_sys(pushbuf.paddr() as u64));

        // PD0 dual-PDE: pdei = (va>>21)&0xff (== 1 for all four, same 2 MiB slot).
        // Low qword = BIG (unused, 0); high qword = SMALL = single-PDE form.
        let pdei = ((src_va >> 21) & 0xff) as usize;
        wr64(&pd0, pdei * 2, 0);
        wr64(&pd0, pdei * 2 + 1, gmmu::encode_pde_sys(spt.paddr() as u64));

        // PD2 / PD3 / root: single PDEs; idx == 0 at all three top levels here.
        wr64(&pd2, ((src_va >> 29) & 0x1ff) as usize, gmmu::encode_pde_sys(pd0.paddr() as u64));
        wr64(&pd3, ((src_va >> 38) & 0x1ff) as usize, gmmu::encode_pde_sys(pd2.paddr() as u64));
        wr64(&root, ((src_va >> 47) & 0x3) as usize, gmmu::encode_pde_sys(pd3.paddr() as u64));

        Some(Self {
            root,
            pd3,
            pd2,
            pd0,
            spt,
            inst,
            userd,
            gpfifo,
            pushbuf,
            sem,
            src,
            dst,
            va_base,
        })
    }

    /// Read-only dump of the allocated physical layout and every encoded
    /// page-table entry, for hand-verification against the spec before the GPU
    /// is ever pointed at these tables.
    fn dump(&self) -> String {
        use core::fmt::Write;
        let rd64 = |r: &DmaRegion, i: usize| unsafe {
            core::ptr::read_volatile(r.as_ptr::<u64>().add(i))
        };
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
            "[gpudbg]  buf phys: inst={:#x} userd={:#x} gpfifo={:#x} pb={:#x} sem={:#x} src={:#x} dst={:#x}",
            self.inst.paddr(),
            self.userd.paddr(),
            self.gpfifo.paddr(),
            self.pushbuf.paddr(),
            self.sem.paddr(),
            self.src.paddr(),
            self.dst.paddr()
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
        for (name, off) in [("src", 0u64), ("dst", 0x1000), ("sem", 0x2000), ("pb", 0x3000)] {
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
            "[gpudbg]  inst PD-base qword(@0x200) = {:#018x} (root|0xC06)",
            gmmu::inst_pd_base(self.root.paddr() as u64)
        );
        s
    }
}

static BOOT_FB_INFO: Mutex<Option<BootFbInfo>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
struct BootFbInfo {
    _phys: u64,
    width: u32,
    height: u32,
    pitch: u32,
}

pub fn set_boot_fb_info(phys: u64, width: u32, height: u32, pitch: u32) {
    *BOOT_FB_INFO.lock() = Some(BootFbInfo {
        _phys: phys,
        width,
        height,
        pitch,
    });
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

pub struct NvidiaGpu {
    name: String,
    info: DisplayInfo,
    architecture: NvidiaArchitecture,
    gpu_model: &'static str,
    vram_size_mb: u32,
    pitch_override: Option<u32>,
    _bar0: usize,
    _bar1: usize,
    vram_allocator: Mutex<Option<NvidiaVramAllocator>>,
    /// Copy-engine bring-up state (GMMU tables + channel structs). Built lazily
    /// on the first `/proc/gpudbg` read so the memory plan is only allocated
    /// when someone is actually debugging GPU bring-up.
    bringup: Mutex<Option<GpuBringup>>,
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
        let num_u64s = (num_pages + 63) / 64;
        Self {
            base_phys,
            total_size,
            bitmap: alloc::vec![0; num_u64s],
        }
    }

    fn _alloc(&mut self, size: usize, align: usize) -> Option<u64> {
        let num_pages = (size + 4095) / 4096;
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
        let num_pages = (size + 4095) / 4096;
        for i in 0..num_pages {
            let b = start_bit + i;
            if b / 64 < self.bitmap.len() {
                self.bitmap[b / 64] &= !(1 << (b % 64));
            }
        }
    }
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

    pub fn new(
        name: String,
        device_id: u16,
        bar0: usize,
        fb_vaddr: usize,
        fb_size: usize,
        default_width: u32,
        default_height: u32,
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

        Ok(Self {
            name,
            info,
            architecture: arch,
            gpu_model,
            vram_size_mb,
            pitch_override,
            _bar0: bar0,
            _bar1: final_fb_vaddr,
            vram_allocator: Mutex::new(Some(NvidiaVramAllocator::new(
                fb_vaddr as u64,
                fb_size as u64,
            ))),
            bringup: Mutex::new(None),
        })
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
    pub fn temperature(&self) -> Option<i32> {
        read_temperature(self._bar0)
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
}

#[allow(dead_code)] // used when deferred BAR0 MMIO probe is enabled
fn arch_from_pmc_boot0(boot0: u32) -> NvidiaArchitecture {
    let chip_id = (boot0 >> regs::PMC_BOOT0_CHIP_ID_SHIFT) & regs::PMC_BOOT0_CHIP_ID_MASK;
    if chip_id >= regs::PMC_BOOT0_CHIPID_BLACKWELL_MIN {
        NvidiaArchitecture::Blackwell
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_HOPPER_MIN
        && chip_id <= regs::PMC_BOOT0_CHIPID_HOPPER_MAX
    {
        NvidiaArchitecture::Hopper
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_ADA_MIN && chip_id <= regs::PMC_BOOT0_CHIPID_ADA_MAX
    {
        NvidiaArchitecture::AdaLovelace
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_AMPERE_MIN
        && chip_id <= regs::PMC_BOOT0_CHIPID_AMPERE_MAX
    {
        NvidiaArchitecture::Ampere
    } else if chip_id >= regs::PMC_BOOT0_CHIPID_TURING_MIN
        && chip_id <= regs::PMC_BOOT0_CHIPID_TURING_MAX
    {
        NvidiaArchitecture::Turing
    } else {
        NvidiaArchitecture::Unknown
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

impl DrmScheme for NvidiaGpu {
    /// Read-only GPU state dump (surfaced at `/proc/gpudbg`). Step 1 of the GPU
    /// copy-engine bring-up: confirm MMIO works bidirectionally, identify the
    /// exact chip, and record the VRAM/BAR layout we need for channel structs.
    /// All reads, no writes — safe to run on demand post-boot. With two GPUs
    /// this runs once per NvidiaGpu; `name` (PCI bus:dev.fn) tells them apart,
    /// and a matching BAR1/fb_vaddr marks the one actually driving the display.
    fn debug_dump(&self) -> String {
        use core::fmt::Write;
        let bar0 = self._bar0;
        let rd = |off: u32| unsafe {
            core::ptr::read_volatile((bar0 + off as usize) as *const u32)
        };
        // NV_PMC_BOOT_0: architecture/chipset id. NV_PCFG mirror at BAR0+0x88000
        // exposes PCI config dword 0 (vendor | device<<16) — reading 0x10de here
        // proves MMIO is alive. (Offsets per nouveau nvkm.)
        let boot0 = rd(regs::NV_PMC_BOOT_0);
        let chipset = (boot0 >> 20) & 0x1ff;
        let pcfg = rd(0x8_8000);
        let cstatus = rd(regs::NV_PFB_CSTATUS);
        let mut s = String::new();
        let _ = writeln!(s, "[gpudbg] === {} ({}) ===", self.name, self.gpu_model);
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
        let _ = writeln!(s, "[gpudbg]  PFB_CSTATUS(0x10020c)={:#010x}", cstatus);

        // --- Step 0: FIFO / MMU status (read-only "hang oracle") ---
        // Confirms which runlist owns the copy engine and that no MMU fault is
        // latched at boot, BEFORE any risky write. All reads. Offsets per
        // nouveau tu102 (vfn/fifo/mmu). A PRI-error sentinel (0xbadfxxxx) here
        // just means the engine block is in reset — still harmless to read.
        let doorbell_en = rd(0x00b6_5000);
        let _ = writeln!(
            s,
            "[gpudbg]  --- FIFO/MMU (Step 0, read-only) ---"
        );
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
        let _ = writeln!(
            s,
            "[gpudbg]  CHAN0_CFG(0x800004)={:#010x}",
            rd(0x0080_0004)
        );
        let _ = writeln!(
            s,
            "[gpudbg]  MMU flush PDB(0xb830a0)={:#010x} hi(0xb830a4)={:#010x} trigger(0xb830b0)={:#010x}",
            rd(0x00b8_30a0),
            rd(0x00b8_30a4),
            rd(0x00b8_30b0)
        );

        // --- Step 1: build the GMMU tables in RAM and dump them (no GPU writes) ---
        {
            let mut g = self.bringup.lock();
            if g.is_none() {
                // GPU VA base for the packed 2 MiB region (avoids null-VA).
                *g = GpuBringup::build(0x0020_0000);
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

    fn import_buffer(&self, _handle: GemHandle) -> bool {
        true
    }

    fn free_buffer(&self, handle: GemHandle) {
        if let Some(ref mut a) = *self.vram_allocator.lock() {
            a.free(handle.phys_addr, handle.size);
        }
    }

    fn create_fb(&self, handle_id: u32, _width: u32, _height: u32, _pitch: u32) -> Option<u32> {
        Some(handle_id)
    }

    fn page_flip(&self, _fb_id: u32) -> bool {
        true
    }

    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, flags: u32) -> bool {
        const DRM_CURSOR_MOVE: u32 = 0x02;
        if (flags & DRM_CURSOR_MOVE) != 0 {
            // Potential software cursor update here if supported
            return true;
        }
        false
    }

    fn wait_vblank(&self, _crtc_id: u32) -> bool {
        true
    }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (Vec::new(), alloc::vec![2001], alloc::vec![1001])
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        if id == 1001 {
            Some(DrmConnector {
                id,
                connected: true,
                mm_width: 0,
                mm_height: 0,
            })
        } else {
            None
        }
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        if id == 2001 {
            Some(DrmCrtc {
                id,
                fb_id: 0,
                x: 0,
                y: 0,
            })
        } else {
            None
        }
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        if id == 3001 {
            Some(DrmPlane {
                id,
                crtc_id: 2001,
                fb_id: 0,
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
        _plane_id: u32,
        _crtc_id: u32,
        _fb_id: u32,
        _x: i32,
        _y: i32,
        _w: u32,
        _h: u32,
        _src_x: u32,
        _src_y: u32,
        _src_w: u32,
        _src_h: u32,
    ) -> bool {
        true
    }

    fn ioctl(&self, request: u32, arg: usize) -> Result<usize, i32> {
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
                let p = arg as *const u32;
                unsafe {
                    self.fill_rect(*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4));
                }
                Ok(0)
            }
            0x10DE0011 => {
                // Blit Rect (arg is pointer to [u32; 6]: sx, sy, dx, dy, w, h)
                let p = arg as *const u32;
                unsafe {
                    self.blit_rect(*p, *p.add(1), *p.add(2), *p.add(3), *p.add(4), *p.add(5));
                }
                Ok(0)
            }
            _ => Err(38), // ENOSYS
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
        use crate::bus::PAGE_SIZE;
        #[cfg(target_arch = "x86_64")]
        const BAR0: u16 = 0x10;

        #[cfg(target_arch = "x86_64")]
        let bar0_addr = {
            if let Some(BAR::Memory(a, _, _, _)) = dev.bars[0] {
                if a != 0 {
                    a
                } else {
                    let ops = &PortOpsImpl;
                    unsafe { read_bar_addr(ops, PCI_ACCESS, dev.loc, BAR0) }
                }
            } else {
                let ops = &PortOpsImpl;
                unsafe { read_bar_addr(ops, PCI_ACCESS, dev.loc, BAR0) }
            }
        };
        #[cfg(not(target_arch = "x86_64"))]
        let bar0_addr = if let Some(BAR::Memory(a, _, _, _)) = dev.bars[0] {
            a
        } else {
            0
        };

        if bar0_addr == 0 {
            return Err(DeviceError::NoResources);
        }

        if let Some(m) = mapper {
            m.query_or_map(bar0_addr as usize, PAGE_SIZE * 1024);
        }
        let bar0_vaddr = phys_to_virt(bar0_addr as usize);

        let fb_bar = (1..6usize).find_map(|i| {
            if let Some(BAR::Memory(addr, len, _, _)) = dev.bars[i] {
                if addr == 0 {
                    return None;
                }
                // Do not probe BAR size at boot (writes 0xFFFFFFFF to BAR registers);
                // on some GPUs that wedges config space and hangs the machine.
                let actual_len: u64 = if len == 0 {
                    256 * 1024 * 1024
                } else {
                    len as u64
                };
                if actual_len >= (16 * 1024 * 1024) {
                    Some((addr, actual_len))
                } else {
                    None
                }
            } else {
                None
            }
        });

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
                "[NVIDIA] GPU at {} bar0={:#x} fb={:#x} fb_len={:#x}",
                gpu_name,
                bar0_addr,
                fb_addr,
                fb_len
            );
            let gpu = Arc::new(NvidiaGpu::new(
                gpu_name,
                dev.id.device_id,
                bar0_vaddr,
                fb_vaddr,
                fb_len as usize,
                1920,
                1080,
            )?);
            Ok(Device::Drm(gpu))
        } else {
            Err(DeviceError::NoResources)
        }
    }
}
