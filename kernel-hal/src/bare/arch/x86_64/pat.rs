//! IA32_PAT programming and the write-combining retrofit for the framebuffer.
//!
//! Out of reset the PAT holds `[WB, WT, UC-, UC, WB, WT, UC-, UC]`, and the
//! page-table flag conversion in `vm.rs` used to emit plain `PCD|PWT` (PAT
//! index 3 = UC) for *every* non-cached policy — including
//! [`CachePolicy::WriteCombining`], whose PAT bit was never set and whose PAT
//! entry never existed. Net effect: the GOP framebuffer (reached through the
//! physmap alias, which rboot maps with no cache attribute at all → WB, or
//! through VMOs requesting WC → UC) was blitted through **uncached** stores,
//! where every 8-byte write is its own serialized bus transaction. That is the
//! difference between ~50-100 MB/s and multiple GB/s of blit throughput, paid
//! on every console line and every compositor frame.
//!
//! Two pieces fix it:
//! - [`init_this_cpu`] rewrites PAT entry 7 (selected by `PAT|PCD|PWT`) from
//!   UC to WC. Entry 7 is chosen because nothing can currently reference it
//!   (no code sets the PTE PAT bit), so redefining it cannot change the type
//!   of any existing mapping. Runs on the BSP and on every AP — the PAT MSR
//!   is per-core and Intel requires it consistent across cores.
//! - [`enable_framebuffer_wc`] walks the live boot page table and flips the
//!   4 KiB PTEs covering the framebuffer's physmap alias to `PAT|PCD|PWT`
//!   (index 7 = WC). rboot maps the physmap exclusively with 4 KiB pages
//!   (`rboot/src/page_table.rs`, `Mapper<Size4KiB>`), and every process page
//!   table shares the kernel-half PDPTs by PML4E cloning
//!   (`pt_clone_kernel_space`), so a single in-place edit propagates to every
//!   address space. `invlpg` alone is NOT enough: the range was WB and was
//!   written (the boot logo, the early framebuffer console), so it holds dirty
//!   lines whose later eviction would land on top of the new write-combining
//!   stores — see the `clflush_range` call in [`enable_framebuffer_wc`].
//!
//! New mappings that ask for [`CachePolicy::WriteCombining`] get the PAT bit
//! from `X86PTE::set_flags` in `vm.rs` once [`pat_wc_ready`] reports true.

use x86_64::instructions::tlb;
use x86_64::registers::model_specific::Msr;

use crate::mem::phys_to_virt;
use crate::KCONFIG;

const IA32_PAT: u32 = 0x277;
/// Memory-type encodings for PAT entries (Intel SDM vol. 3A, table 11-10).
const PAT_TYPE_WC: u64 = 0x01;

// The flag itself lives beside its reader, in `utils::pte::x86_64`, so a host
// test can drive both sides of the branch that consults it.
pub use crate::utils::pte::x86_64::{pat_wc_ready, set_pat_wc_ready};

/// Program PAT entry 7 = WC on the calling CPU. Idempotent, per-core.
///
/// Entry 7 is unreferenced until `vm.rs`/`enable_framebuffer_wc` start
/// emitting the PTE PAT bit, so this rewrite cannot retype an existing
/// mapping and needs none of the SDM's cache-disable ceremony.
pub fn init_this_cpu() {
    let mut msr = Msr::new(IA32_PAT);
    // SAFETY: IA32_PAT exists on every x86_64 CPU this kernel supports
    // (P4+/all 64-bit parts); rewriting an unused entry is side-effect free.
    unsafe {
        let old = msr.read();
        let new = (old & !(0xffu64 << 56)) | (PAT_TYPE_WC << 56);
        if old != new {
            msr.write(new);
        }
    }
    set_pat_wc_ready(true);
}

const PTE_PWT: u64 = 1 << 3;
const PTE_PCD: u64 = 1 << 4;
/// PAT bit position in a 4 KiB PTE (bit 7; in PDE/PDPTE leaves bit 7 is PS
/// and the PAT bit moves to bit 12).
const PTE_PAT_4K: u64 = 1 << 7;
const PTE_PRESENT: u64 = 1 << 0;
const PTE_HUGE: u64 = 1 << 7;
const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Flip the leaf PTEs covering the framebuffer's physmap alias to PAT
/// entry 7 (WC). Call on the BSP, after [`init_this_cpu`], before the
/// graphic console starts pushing frames. Safe to call again (idempotent) —
/// and it IS called again after the PCI scan: on real hardware the GOP
/// surface sits inside the console GPU's BAR1, which is not premapped by
/// rboot (this pass finds NotMapped) and is later mapped UncachedDevice by
/// the PCI `query_or_map`, so only a re-run after PCI can retype it.
///
/// Only 4 KiB leaves are converted; a huge-page leaf (which rboot never
/// creates for the physmap) is left untouched — UC/WB there is slower, not
/// incorrect — and reported once.
pub fn enable_framebuffer_wc() {
    if !pat_wc_ready() || KCONFIG.fb_addr == 0 || KCONFIG.fb_size == 0 {
        return;
    }
    let root = x86_64::registers::control::Cr3::read()
        .0
        .start_address()
        .as_u64() as usize;
    let va_base = phys_to_virt(KCONFIG.fb_addr as usize);
    let pages = (KCONFIG.fb_size as usize).div_ceil(4096);
    let mut converted = 0usize;
    let mut skipped_huge = 0usize;
    // Write back and invalidate every page we retype, the way Linux's
    // `set_memory_wc` follows its TLB flush with `clflush_cache_range`.
    //
    // This used to be skipped, on the reasoning that the range had only ever
    // been *written* through the WB alias, so nothing needed pulling back.
    // That has it backwards: a write-back range that was written and not read
    // is precisely the one holding DIRTY lines. rboot's logo,
    // `early_fb_console::prime` and every early klog line store into this
    // physmap alias before we get here, and those lines sit in L1/L2 until
    // something evicts them. Retyping only the PTE leaves them there, and the
    // eviction — whenever it comes, seconds or minutes later — writes that
    // stale pixel data back ON TOP of whatever the write-combining stores
    // have since drawn. The symptom is rectangles of boot-logo or boot-text
    // debris reappearing over a live console or desktop at random moments.
    // (The SDM also leaves the effective type undefined for a page touched
    // under two memory types with no flush between them.)
    //
    // Flushing commits those pending writes at a defined point, before the
    // first WC store, and leaves the range clean. Per converted PAGE, not
    // over `first..last`: this pass legitimately finds unmapped holes (the
    // post-PCI re-run is documented to), and `CLFLUSH` on an unmapped address
    // faults like any other access.
    let mut fenced = false;
    for i in 0..pages {
        let va = va_base + i * 4096;
        match convert_pte_to_wc(root, va) {
            PteConvert::Converted => {
                tlb::flush(x86_64::VirtAddr::new(va as u64));
                if !fenced {
                    // SAFETY: plain fence, no memory operand.
                    unsafe { core::arch::x86_64::_mm_mfence() };
                    fenced = true;
                }
                clflush_page(va);
                converted += 1;
            }
            PteConvert::AlreadyWc => {}
            PteConvert::HugeLeaf => skipped_huge += 1,
            PteConvert::NotMapped => {}
        }
    }
    if fenced {
        // Order every flush ahead of the write-combining stores to come.
        // SAFETY: plain fence, no memory operand.
        unsafe { core::arch::x86_64::_mm_mfence() };
    }
    if converted > 0 || skipped_huge > 0 {
        // klog, not log::warn!: at the default LOG=error boot this line is the
        // only record of whether the framebuffer is WC or stuck at UC, and it
        // was being filtered out on exactly the hardware where it mattered.
        crate::klog_info!(
            "pat: framebuffer {:#x}..{:#x} write-combining ({} PTEs converted{})",
            KCONFIG.fb_addr,
            KCONFIG.fb_addr + KCONFIG.fb_size,
            converted,
            if skipped_huge > 0 {
                ", huge leaves skipped"
            } else {
                ""
            },
        );
    }
}

/// Write back and invalidate the cache lines of the 4 KiB page at `va`.
///
/// Caller brackets the whole run with `MFENCE` (see [`enable_framebuffer_wc`]).
/// Plain `CLFLUSH` rather than `CLFLUSHOPT`: this runs once per boot over a
/// few megabytes, and `CLFLUSH` is self-serialising, so the extra fencing the
/// optimised form needs buys nothing here.
fn clflush_page(va: usize) {
    const LINE: usize = 64;
    // SAFETY: `va` is a page the caller just converted in the live page table,
    // so it is mapped. `CLFLUSH` touches only the cache line containing the
    // address and faults on nothing an ordinary read of it would not.
    unsafe {
        for off in (0..4096).step_by(LINE) {
            core::arch::x86_64::_mm_clflush((va + off) as *const u8);
        }
    }
}

enum PteConvert {
    Converted,
    AlreadyWc,
    HugeLeaf,
    NotMapped,
}

/// One-shot store-throughput probe of the framebuffer mapping, klogged as
/// cycles/byte (x100). Reads back the first 256 KiB (slow uncached read,
/// tolerable once), then times rewriting the SAME bytes — screen content is
/// unchanged. Interpretation is scale-free: UC stores land around 7000-10000
/// (x100 cycles/byte), write-combining around 100-300. Together with
/// [`fb_mapping_diag`] this splits "the PTE says WC but the stores still
/// crawl" (hardware/BAR-side limit) from "the mapping never became WC here".
pub(crate) fn fb_store_bench_klog(tag: &str) {
    const LEN: usize = 256 << 10;
    if KCONFIG.fb_addr == 0 || (KCONFIG.fb_size as usize) < LEN {
        return;
    }
    let va = phys_to_virt(KCONFIG.fb_addr as usize) as *mut u8;
    let mut saved = alloc::vec![0u8; LEN];
    // SAFETY: the fb physmap alias is mapped (callers run after the retype
    // passes); saved is LEN bytes; rewriting identical bytes is visually a
    // no-op and racing the console blitter at worst repaints a stale tile.
    unsafe {
        core::ptr::copy_nonoverlapping(va as *const u8, saved.as_mut_ptr(), LEN);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let t0 = core::arch::x86_64::_rdtsc();
        core::ptr::copy_nonoverlapping(saved.as_ptr(), va, LEN);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let t1 = core::arch::x86_64::_rdtsc();
        let cycles = t1.wrapping_sub(t0);
        crate::klog_info!(
            "pat: fb store bench ({}): {} KiB, {} cycles, {} cycles/byte x100",
            tag,
            LEN >> 10,
            cycles,
            cycles.saturating_mul(100) / (LEN as u64)
        );
    }
}

/// Diagnostic: walk the CURRENT page table (this CPU's CR3 — i.e. the calling
/// process's tree, not necessarily the boot tree `enable_framebuffer_wc` last
/// edited) for the boot framebuffer's physmap vaddr, and name the *effective*
/// memory type the calling context reaches it through: raw leaf PTE, the PAT
/// index its PWT/PCD/PAT bits select, and what this CPU's IA32_PAT says that
/// index means. This is the ground truth behind "the blit is still 42 MB/s
/// after the retype": either the PTE here lacks the WC bits (the trees don't
/// share), or it has them and the slowness is beyond the mapping.
pub(crate) fn fb_mapping_diag() -> alloc::string::String {
    use alloc::format;
    if KCONFIG.fb_addr == 0 {
        return alloc::string::String::from("no boot framebuffer");
    }
    let root = x86_64::registers::control::Cr3::read()
        .0
        .start_address()
        .as_u64() as usize;
    let va = phys_to_virt(KCONFIG.fb_addr as usize);
    let idx = |level: usize| (va >> (12 + 9 * level)) & 0x1ff;
    let mut table_pa = root;
    for level in (1..=3).rev() {
        // SAFETY: the physmap covers all page-table frames; entries are u64.
        let entry = unsafe {
            core::ptr::read_volatile((phys_to_virt(table_pa) as *const u64).add(idx(level)))
        };
        if entry & PTE_PRESENT == 0 {
            return format!("cr3={:#x} va={:#x} NOT MAPPED (level {})", root, va, level);
        }
        if level < 3 && entry & PTE_HUGE != 0 {
            return format!(
                "cr3={:#x} va={:#x} HUGE leaf at level {}: {:#x}",
                root, va, level, entry
            );
        }
        table_pa = (entry & PHYS_ADDR_MASK) as usize;
    }
    // SAFETY: as above; and IA32_PAT exists on every supported CPU.
    let (pte, pat_msr) = unsafe {
        let pte = core::ptr::read_volatile((phys_to_virt(table_pa) as *const u64).add(idx(0)));
        (pte, Msr::new(IA32_PAT).read())
    };
    let patidx = (((pte >> 7) & 1) * 4 + ((pte >> 4) & 1) * 2 + ((pte >> 3) & 1)) as usize;
    let mtype = (pat_msr >> (patidx * 8)) & 0xff;
    let name = match mtype {
        0 => "UC",
        1 => "WC",
        4 => "WT",
        5 => "WP",
        6 => "WB",
        7 => "UC-",
        _ => "?",
    };
    format!(
        "cr3={:#x} va={:#x} pte={:#x} patidx={} type={:#x}({}) pat_msr={:#x}",
        root, va, pte, patidx, mtype, name, pat_msr
    )
}

/// Walk the 4-level tree for `va` and set `PAT|PCD|PWT` on its 4 KiB PTE.
fn convert_pte_to_wc(root: usize, va: usize) -> PteConvert {
    let idx = |level: usize| (va >> (12 + 9 * level)) & 0x1ff;
    let mut table_pa = root;
    // Levels 3..1 (PML4, PDPT, PD): descend, refusing huge leaves.
    for level in (1..=3).rev() {
        // SAFETY: the physmap covers all page-table frames; entries are u64.
        let entry = unsafe {
            core::ptr::read_volatile((phys_to_virt(table_pa) as *const u64).add(idx(level)))
        };
        if entry & PTE_PRESENT == 0 {
            return PteConvert::NotMapped;
        }
        if level < 3 && entry & PTE_HUGE != 0 {
            return PteConvert::HugeLeaf;
        }
        table_pa = (entry & PHYS_ADDR_MASK) as usize;
    }
    let pte_ptr = unsafe { (phys_to_virt(table_pa) as *mut u64).add(idx(0)) };
    // SAFETY: `pte_ptr` addresses a live PTE through the physmap.
    unsafe {
        let pte = core::ptr::read_volatile(pte_ptr);
        if pte & PTE_PRESENT == 0 {
            return PteConvert::NotMapped;
        }
        let wc = pte | PTE_PWT | PTE_PCD | PTE_PAT_4K;
        if wc == pte {
            return PteConvert::AlreadyWc;
        }
        core::ptr::write_volatile(pte_ptr, wc);
    }
    PteConvert::Converted
}
