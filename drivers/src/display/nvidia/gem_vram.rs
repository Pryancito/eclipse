//! Where `GEM_NEW` puts an object, and the BAR1 mapping that lets a buffer
//! live in VRAM and still be mapped by the CPU (`nvidia.gem_vram`).
//!
//! # Why
//!
//! A compositor's scanout buffers come from NVK with `VRAM | GART`: NVK asks
//! for device-local memory and, because the buffer is exported as a dma-buf,
//! allows GART as well (`nvk_memory_type_flags`). Linux's nouveau places such
//! an object in VRAM and maps it for the CPU through BAR1 when someone asks.
//! `GEM_NEW` put it in host memory instead, because a VRAM object here had
//! no CPU address at all. That had two costs: the GPU rendered every frame
//! across PCIe, and the display could not scan the buffer out, because the
//! display engine reads VRAM. So every frame was copied into the console by
//! the copy engine (or the CPU), which is the path that has been failing on
//! the console card.
//!
//! With the hatch on, a `VRAM | GART` object is allocated in VRAM, as one
//! contiguous run, and mapped into BAR1 by the RM
//! (`rm_init::gem_map_bar1`, `kbusMapFbApertureSingle`). The address it
//! publishes is BAR1's base plus the VA the RM chose: the VRAM itself, seen
//! through the aperture. PRIME, `ADDFB2` and mmap take it like any other, and
//! the framebuffer carries its VRAM offset, so `nvkms_page_flip` flips the
//! display onto it with no copy at all.
//!
//! This replaces the arithmetic in `vram_bar1.rs` (`bar1_phys + offset`),
//! which assumes BAR1 is a linear window onto VRAM. It is not, now: BAR1 is
//! virtual, and only the console's first megabytes are mapped one to one.
//!
//! # What can go wrong, and what happens then
//!
//! BAR1 is 256 MiB. The mappings together are held under [`bar1_budget`];
//! past it, or when the RM refuses for any other reason, the object goes to
//! host memory exactly as before, and a line says why. The mapping is given
//! back by `gem_free` (the C does it before the memory goes), so a closed
//! buffer frees its BAR1 space.
//!
//! The CPU's access to BAR1 is uncached for the kernel's own window and
//! write-combining for userspace (`gem_mmap::register_device_memory`), never
//! write-back. A present that still copies such a buffer is copied by the
//! copy engine as VRAM (`eclipse_rm_ce_blit_p2p_2d`), not read back over
//! PCIe from the card's own BAR1.
//!
//! Off by default for its first hardware run: without `nvidia.gem_vram`
//! [`placement`] answers what `GEM_NEW` always did.

use super::super::nouveau_uapi as nv;
use super::vram_bar1;
use core::sync::atomic::{AtomicBool, Ordering};
use nvidia_rm_sys::rm_init::{self, GemAlloc};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Called from `zCore`'s cmdline parsing when `nvidia.gem_vram` is present.
pub fn set_enabled(v: bool) {
    ENABLED.store(v, Ordering::Relaxed);
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Where an object goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Placement {
    /// Host memory (`NOUVEAU_GEM_DOMAIN_GART`), CPU-mapped at its own address.
    Sysmem,
    /// VRAM with no CPU mapping: a `VRAM`-only request, which NVK makes for
    /// memory it never maps.
    Vram,
    /// VRAM, contiguous, mapped for the CPU through BAR1.
    VramMapped,
}

/// The placement for a request's `domain`. `vram_mapped` is the hatch.
///
/// Only `VRAM | GART` takes the mapped VRAM path: it is the request that
/// needs both a CPU address and the GPU's own memory, and the one a
/// scanout buffer arrives with. A `VRAM`-only object never gets a CPU
/// mapping (BAR1 would not hold NVK's device-local memory), and `GART`
/// alone stays host memory, as in nouveau.
pub(super) fn placement(domain: u32, vram_mapped: bool) -> Placement {
    let want_vram = domain & nv::NOUVEAU_GEM_DOMAIN_VRAM != 0;
    let want_gart = domain & nv::NOUVEAU_GEM_DOMAIN_GART != 0;
    if want_vram && want_gart && vram_mapped {
        Placement::VramMapped
    } else if want_gart || !want_vram {
        Placement::Sysmem
    } else {
        Placement::Vram
    }
}

/// What the RM keeps of BAR1 for itself: the console mapped one to one at
/// its start (6 MiB on a 1366x768 panel, up to 32 MiB at 4K), USERD and the
/// RM's own transfers.
const BAR1_KEPT_FOR_RM: u64 = 64 << 20;

/// How much of a BAR1 of `bar1_len` bytes the buffers' mappings may hold
/// together. Half of a small one, which leaves the RM what it needs there.
pub(super) fn bar1_budget(bar1_len: u64) -> u64 {
    if bar1_len >= 2 * BAR1_KEPT_FOR_RM {
        bar1_len - BAR1_KEPT_FOR_RM
    } else {
        bar1_len / 2
    }
}

/// The object `GEM_NEW` is creating, and the card it goes on.
pub(super) struct Target {
    pub device_instance: u32,
    pub owner_ctx: u32,
    pub size: u64,
    pub handle: u32,
    /// BAR1 on the host bus: where the card's mappings must land.
    pub bar1_phys: u64,
    pub bar1_len: u64,
}

/// What [`place`] made.
pub(super) struct Placed {
    pub alloc: GemAlloc,
    /// Host memory, i.e. `NOUVEAU_GEM_DOMAIN_GART`.
    pub sysmem: bool,
    /// The CPU's address for the object, when it has one.
    pub phys_addr: Option<u64>,
    /// The object's VRAM offset, for a VRAM object.
    pub vram_offset: Option<u64>,
    /// `phys_addr` is VRAM behind BAR1, not host RAM.
    pub device_memory: bool,
}

/// Allocate the object where `placement` says, and resolve its addresses.
///
/// `window` makes sure the kernel's `phys_to_virt` alias of a BAR1 range is
/// mapped (`nvidia_hooks::map_device_window`). A mapped-VRAM object that
/// cannot have one -- RM refused, BAR1 budget spent, no window -- is put in
/// host memory instead. `Err` is the errno `GEM_NEW` returns.
pub(super) fn place(
    t: &Target,
    placement: Placement,
    window: impl Fn(u64, u64) -> bool,
) -> Result<Placed, i32> {
    if placement == Placement::VramMapped {
        if let Some(placed) = vram_mapped(t, window) {
            return Ok(placed);
        }
    }
    let sysmem = placement != Placement::Vram;
    let what = if sysmem { "sysmem/GART" } else { "vidmem/VRAM" };
    let alloc = match rm_init::gem_alloc(t.device_instance, t.owner_ctx, t.size, sysmem) {
        Ok(a) if a.alloc_status == 0 => a,
        Ok(a) => {
            crate::klog_warn!(
                "[nouveau-uapi] GEM_NEW: RM alloc failed ({}), size={} status={:#x}",
                what,
                t.size,
                a.alloc_status
            );
            return Err(nv::ENOMEM);
        }
        Err(status) => {
            crate::klog_warn!(
                "[nouveau-uapi] GEM_NEW: gem_alloc ({}) failed, NV_STATUS={:#x}",
                what,
                status
            );
            return Err(nv::ENOMEM);
        }
    };
    if sysmem {
        return Ok(Placed {
            alloc,
            sysmem,
            phys_addr: sysmem_pa(t, alloc.h_memory),
            vram_offset: None,
            device_memory: false,
        });
    }
    static FIRST_VRAM_GEM: AtomicBool = AtomicBool::new(false);
    if !FIRST_VRAM_GEM.swap(true, Ordering::Relaxed) {
        crate::klog_info!(
            "[nouveau-uapi] GEM_NEW: first VRAM-only LOCAL alloc size={} hMemory={:#010x}",
            t.size,
            alloc.h_memory
        );
    }
    // The VRAM offset (AT_GPU) for VM_BIND's comptags and scanout-by-offset.
    // If the lookup fails the object is still usable for VM_BIND/EXEC, with
    // no CPU mapping and no scanout.
    let vram_offset =
        rm_init::gem_fbmem_offset(t.device_instance, t.owner_ctx, alloc.h_memory).ok();
    // The old linear-BAR1 arithmetic, behind its own hatch
    // (`nvidia.gem_vram_bar1`); kept until the cleanup.
    let phys_addr = vram_bar1::cpu_mapping_for_vram_gem(
        false,
        t.bar1_phys,
        t.bar1_len,
        vram_offset,
        t.size,
        t.handle,
    );
    Ok(Placed {
        alloc,
        sysmem,
        phys_addr,
        vram_offset,
        device_memory: false,
    })
}

/// The host physical address of a sysmem object, from the RM. `None` (and a
/// line) when it has none: the object is then not CPU-mappable.
fn sysmem_pa(t: &Target, h_memory: u32) -> Option<u64> {
    match rm_init::gem_map_cpu(t.device_instance, t.owner_ctx, h_memory) {
        Ok(m) if m.lookup_status == 0 && m.address_space == rm_init::ADDR_SYSMEM => {
            Some(m.phys_addr)
        }
        Ok(m) => {
            crate::klog_warn!(
                "[nouveau-uapi] GEM_NEW handle={}: gem_map_cpu lookup_status={:#x} address_space={} -- not CPU-mmap-able",
                t.handle,
                m.lookup_status,
                m.address_space
            );
            None
        }
        Err(status) => {
            crate::klog_warn!(
                "[nouveau-uapi] GEM_NEW handle={}: gem_map_cpu failed, NV_STATUS={:#x} -- not CPU-mmap-able",
                t.handle,
                status
            );
            None
        }
    }
}

static FIRST_MAPPED: AtomicBool = AtomicBool::new(false);
static FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

/// Say once why a `VRAM | GART` object went to host memory after all.
fn fall_back(t: &Target, why: alloc::string::String) -> Option<Placed> {
    if !FALLBACK_LOGGED.swap(true, Ordering::Relaxed) {
        crate::klog_warn!(
            "[nouveau-uapi] GEM_NEW handle={} ({} B, VRAM|GART): {} -- va a memoria del \
             sistema como antes (solo se avisa la primera vez)",
            t.handle,
            t.size,
            why
        );
    }
    None
}

fn vram_mapped(t: &Target, window: impl Fn(u64, u64) -> bool) -> Option<Placed> {
    let alloc = match rm_init::gem_alloc_with(
        t.device_instance,
        t.owner_ctx,
        t.size,
        rm_init::GEM_ALLOC_VRAM_CONTIGUOUS,
    ) {
        Ok(a) if a.alloc_status == 0 => a,
        Ok(a) => {
            return fall_back(
                t,
                alloc::format!("el RM no da VRAM contigua (status {:#x})", a.alloc_status),
            )
        }
        Err(status) => {
            return fall_back(
                t,
                alloc::format!("gem_alloc de VRAM contigua fallo ({:#x})", status),
            )
        }
    };
    let free = |why: alloc::string::String| {
        // gem_free gives the BAR1 mapping back too, if there is one.
        let _ = rm_init::gem_free(t.device_instance, t.owner_ctx, alloc.h_memory);
        fall_back(t, why)
    };
    let budget = bar1_budget(t.bar1_len);
    let map = match rm_init::gem_map_bar1(t.device_instance, t.owner_ctx, alloc.h_memory, budget) {
        Ok(m) if m.status == 0 => m,
        Ok(m) => {
            return free(alloc::format!(
                "el RM no lo mapea en BAR1 (status {:#x}; tope {} MiB)",
                m.status,
                budget >> 20
            ))
        }
        Err(status) => return free(alloc::format!("gem_map_bar1 fallo ({:#x})", status)),
    };
    if !mapping_fits(t, map.host_pa, map.length) {
        return free(alloc::format!(
            "el mapeo BAR1 {:#x}+{:#x} no cae dentro de BAR1 {:#x}+{:#x}",
            map.host_pa,
            map.length,
            t.bar1_phys,
            t.bar1_len
        ));
    }
    if !window(map.host_pa, t.size) {
        return free(alloc::format!(
            "el kernel no pudo mapear su ventana sobre BAR1 {:#x}+{:#x}",
            map.host_pa,
            t.size
        ));
    }
    if !FIRST_MAPPED.swap(true, Ordering::Relaxed) {
        crate::klog_info!(
            "[nouveau-uapi] GEM_NEW handle={}: primer buffer VRAM|GART en VRAM ({} B en VRAM \
             {:#x}), mapeado por BAR1 en {:#x}{} -- se puede escanear sin copias",
            t.handle,
            t.size,
            map.vram_offset,
            map.host_pa,
            if map.physical != 0 {
                " (BAR1 fisica)"
            } else {
                ""
            }
        );
    }
    Some(Placed {
        alloc,
        sysmem: false,
        phys_addr: Some(map.host_pa),
        vram_offset: Some(map.vram_offset),
        device_memory: true,
    })
}

/// Whether the RM's mapping covers the whole object inside this card's BAR1.
/// The RM computes the address from its own idea of BAR1's base; a mapping
/// that does not land inside the aperture the PCI scan found is not one the
/// CPU can trust.
fn mapping_fits(t: &Target, host_pa: u64, length: u64) -> bool {
    let Some(end) = host_pa.checked_add(t.size) else {
        return false;
    };
    let Some(bar1_end) = t.bar1_phys.checked_add(t.bar1_len) else {
        return false;
    };
    length >= t.size && host_pa >= t.bar1_phys && end <= bar1_end
}

/// Whether `pa` is a buffer in another card's BAR1: a source the copy engine
/// of the card at `bar1_phys..+bar1_len` must not read (it would be a peer
/// read across PCIe, which root complexes need not forward). Its own card
/// copies it as VRAM. Only device-memory objects count, so a sysmem address
/// is never one.
pub(super) fn foreign_bar1_source(bar1_phys: u64, bar1_len: u64, pa: u64) -> bool {
    let own = pa >= bar1_phys && pa - bar1_phys < bar1_len;
    !own && crate::scheme::gem_mmap::lookup_by_phys_within(pa)
        .is_some_and(|handle| crate::scheme::gem_mmap::is_device_memory(handle))
}

#[cfg(test)]
mod tests;
