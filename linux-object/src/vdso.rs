//! The kernel half of the Linux-compatible vDSO.
//!
//! `linux-vdso` holds the image and the code that runs inside it. This module
//! gives that image a home in physical memory, keeps the clock parameters
//! inside it current, and maps it into every Linux process — the three things
//! that turn a shared object into a working `clock_gettime`.
//!
//! ## Why the image lives in a physical VMO
//!
//! There is exactly one copy of the image in the whole system, and every
//! process must map *that* copy. Not for the memory — it is two pages — but
//! because one of those pages is the clock. When the wall clock is set, or the
//! TSC turns out to be unusable, the kernel writes the new parameters once and
//! every process has to see them. A process holding a private snapshot would
//! keep answering `CLOCK_REALTIME` from a stale offset forever, silently, which
//! is precisely the failure the whole design is arranged to avoid.
//!
//! `fork` is what makes this delicate. `VmMapping::clone_map` gives the child a
//! *copy* of a paged VMO — copy-on-write now, but still a copy — and shares the
//! object only when it is physical, on the grounds that a physical VMO is a
//! window onto a fixed range that both processes must see identically. That
//! description fits the vDSO exactly, so it is built as one: frames allocated
//! once at first use and never freed, wrapped in `VmObject::new_physical`. The
//! sharing then falls out of machinery that already exists and already means
//! what we need it to mean, rather than a special case bolted onto fork.
//!
//! (Physical VMOs default to an uncached policy, which is right for a device
//! BAR and very wrong for code on the hot path of every clock read, so the
//! policy is set back to cached before anything maps it.)
//!
//! ## Where it is mapped
//!
//! Immediately below the process stack, with a page of gap. Not
//! `map(None, ...)`: kernel-chosen placement is first-fit from the bottom of
//! the address space, which lands the image at the end of the loaded ELF —
//! exactly where `brk` grows, and `brk` maps at a fixed address, so the first
//! heap extension would fail. Below the stack is both out of the way and where
//! Linux puts it.

use alloc::sync::Arc;
use core::sync::atomic::{compiler_fence, Ordering};

use kernel_hal::mem::PhysFrame;
use kernel_hal::{PhysAddr, VirtAddr};
use spin::Once;
use zircon_object::object::KernelObject;
use zircon_object::vm::{pages, roundup_pages, CachePolicy, MMUFlags, VmAddressRegion, VmObject};

use linux_vdso::VdsoData;

/// `AT_SYSINFO_EHDR`: the aux-vector entry a C library reads to find the vDSO.
///
/// Its value is the address of the image's ELF header, which is the mapping
/// base — the image is linked with a single `PT_LOAD` at vaddr 0 covering the
/// header, so the two coincide by construction.
pub const AT_SYSINFO_EHDR: u8 = 33;

/// A gap left between the vDSO and the bottom of the stack, so a stack overflow
/// faults instead of running quietly into executable pages.
const STACK_GUARD: usize = zircon_object::vm::PAGE_SIZE;

struct Vdso {
    /// The image, shared by every process that maps it.
    vmo: Arc<VmObject>,
    /// Kernel virtual address of `_vdso_data` inside the image's frames. The
    /// kernel writes the clock parameters straight through here.
    data: *mut VdsoData,
    /// Mapped length: the image rounded up to whole pages.
    len: usize,
}

// `data` points into frames that are allocated once and never freed, and is
// only ever written through `publish`, which the clock notification path may
// call from any CPU. See `publish` for why those writes need no lock.
unsafe impl Send for Vdso {}
unsafe impl Sync for Vdso {}

static VDSO: Once<Option<Vdso>> = Once::new();

/// The vDSO, built on first use. `None` when this kernel has none — either the
/// build had no C compiler, or physical memory for it could not be obtained.
fn vdso() -> Option<&'static Vdso> {
    VDSO.call_once(build).as_ref()
}

fn build() -> Option<Vdso> {
    if !linux_vdso::AVAILABLE {
        warn!(
            "vdso: esta compilacion no incluye imagen ({}); clock_gettime seguira siendo un syscall",
            linux_vdso::UNAVAILABLE_REASON.unwrap_or("sin motivo registrado")
        );
        return None;
    }

    let len = roundup_pages(linux_vdso::IMAGE_LEN);
    let frames = PhysFrame::new_contiguous(pages(len), 0);
    // Short as well as empty. `new_physical` below is told `pages(len)` and does
    // not go looking: a VMO wider than the frames actually obtained is a window
    // onto memory nobody owns, which the allocator is free to hand to somebody
    // else while every process in the system has it mapped.
    if frames.len() < pages(len) {
        warn!(
            "vdso: no hay memoria fisica contigua para la imagen ({} de {} paginas)",
            frames.len(),
            pages(len)
        );
        return None;
    }
    let paddr: PhysAddr = frames[0].paddr();
    // The image outlives every process and is never reclaimed, so the frames
    // are deliberately leaked rather than tracked. Dropping this `Vec` would
    // free them out from under every mapping in the system.
    core::mem::forget(frames);

    let base: VirtAddr = kernel_hal::mem::phys_to_virt(paddr);
    // SAFETY: `base` is the direct-map address of `pages(len)` frames this
    // function just took exclusive ownership of, and nothing else refers to
    // them yet.
    unsafe {
        let dst = core::slice::from_raw_parts_mut(base as *mut u8, len);
        // Zero first: the image is shorter than the pages it occupies, and the
        // tail — including the rest of the `_vdso_data` page — must not expose
        // whatever the allocator handed us to userspace.
        dst.fill(0);
        dst[..linux_vdso::IMAGE_LEN].copy_from_slice(linux_vdso::IMAGE);
    }

    let vmo = VmObject::new_physical(paddr, pages(len));
    // Physical VMOs default to uncached, which for a device window is correct
    // and here would put every instruction of the read path and the clock
    // parameters themselves on uncached memory. Must happen before the first
    // mapping: `set_cache_policy` refuses once a VMO has mappings.
    if let Err(e) = vmo.set_cache_policy(CachePolicy::Cached) {
        warn!("vdso: no se pudo marcar la imagen como cacheable: {:?}", e);
        return None;
    }
    vmo.set_name("vdso");

    let vdso = Vdso {
        vmo,
        data: (base + linux_vdso::DATA_OFFSET) as *mut VdsoData,
        len,
    };

    info!(
        "vdso: imagen en {:#x} ({} paginas), _vdso_data en {:#x}",
        paddr,
        pages(len),
        base + linux_vdso::DATA_OFFSET
    );
    Some(vdso)
}

/// Write the current clock parameters into the image.
///
/// Called on every change to the wall-clock offset or to the TSC's usability,
/// through the observer registered in [`init`]. Cheap enough — four stores —
/// that there is no value in trying to skip redundant calls.
///
/// ## Why there is no lock and no seqlock
///
/// Readers are in userspace and cannot take a kernel lock, so a lock here would
/// protect nothing. A seqlock would work, and the real Linux vDSO uses one,
/// because it publishes a whole coherent snapshot: a clock source, its mask,
/// its shift, a base time and a cycle count that only make sense together.
///
/// This publishes numbers that do not depend on each other. `tsc_mult` converts
/// ticks to nanoseconds; `wall_off_ns` shifts monotonic to realtime; `tsc_base`
/// is this boot's time zero and is latched once, before the image exists, and
/// never republished with a different value. A reader that catches a fresh one
/// alongside a stale one gets an answer correct for the instant it read, which
/// is everything a clock owes its caller. All are naturally aligned 64-bit
/// fields, and an aligned 64-bit load on x86_64 cannot tear, so each is
/// individually whole. What is left is ordering, and
/// only in one direction: `enabled` must not turn on before the values it
/// vouches for are in memory, and must turn off before they stop being true.
fn publish() {
    let Some(vdso) = vdso() else { return };
    let mult = kernel_hal::timer::vdso_tsc_mult();
    let wall_off_ns = kernel_hal::timer::wall_clock_offset_ns();
    let tsc_base = kernel_hal::timer::vdso_tsc_base();

    // SAFETY: `data` addresses `_vdso_data` inside frames owned for the
    // lifetime of the system, whose extent the build script has checked lies
    // wholly within the image.
    unsafe { publish_into(vdso.data, mult, wall_off_ns, tsc_base) }
}

/// The write sequence itself, against any `VdsoData`.
///
/// Separate from [`publish`] so that what lands in the struct, and in what
/// order, can be driven from a test: the libos build of `kernel_hal` answers
/// `vdso_tsc_mult()` with a hardcoded `None`, so through `publish` the enabling
/// arm is unreachable on the host and the ordering below -- the only thing here
/// that can be wrong -- would never be exercised anywhere but on Moebius's own
/// machine.
///
/// # Safety
///
/// `d` must point at a live, aligned `VdsoData` that the caller may write.
unsafe fn publish_into(d: *mut VdsoData, mult: Option<u64>, wall_off_ns: u64, tsc_base: u64) {
    // Volatile writes because the reader is userspace and invisible to the
    // compiler.
    let enabled = core::ptr::addr_of_mut!((*d).enabled);
    let tsc_mult = core::ptr::addr_of_mut!((*d).tsc_mult);
    let wall = core::ptr::addr_of_mut!((*d).wall_off_ns);
    let base = core::ptr::addr_of_mut!((*d).tsc_base);

    match mult {
        None => {
            // Turning off: `enabled` goes first, so no reader can act on
            // parameters already known to be wrong.
            core::ptr::write_volatile(enabled, 0);
            compiler_fence(Ordering::SeqCst);
            core::ptr::write_volatile(tsc_mult, 0);
            core::ptr::write_volatile(wall, wall_off_ns);
            // The base goes too, and for the same reason the multiplier does: a
            // stale base left standing is what a reader that forgets to check
            // `enabled` would subtract, and subtracting last boot's base from
            // this boot's counter is a wrong time rather than an absent one.
            core::ptr::write_volatile(base, 0);
        }
        Some(mult) => {
            // Turning on, or updating: the parameters land first, so a reader
            // that sees `enabled` sees them too. The base belongs to that set:
            // without it a reader would scale the absolute counter and answer a
            // monotonic clock days ahead of the kernel's.
            core::ptr::write_volatile(tsc_mult, mult);
            core::ptr::write_volatile(wall, wall_off_ns);
            core::ptr::write_volatile(base, tsc_base);
            compiler_fence(Ordering::SeqCst);
            core::ptr::write_volatile(enabled, 1);
        }
    }
}

/// Build the vDSO and start keeping its clock parameters current.
///
/// Idempotent, and safe to call before or after the clock has been calibrated:
/// registering the observer publishes once immediately, and every later change
/// republishes.
pub fn init() {
    // Build first, register second, and never the other way round. The TSC
    // watchdog notifies from the timer interrupt, so the observer can fire in
    // IRQ context on any CPU — and `publish` starts by asking for the image. If
    // an observer could be installed before `VDSO` was initialized, that first
    // notification would run `build` from an interrupt, allocating physical
    // frames underneath whatever the interrupted code was doing. With this
    // order the `call_once` inside `publish` is always already resolved, so it
    // is a plain load and `publish` is nothing but volatile stores.
    if vdso().is_none() {
        return;
    }
    static REGISTERED: Once<()> = Once::new();
    REGISTERED.call_once(|| kernel_hal::timer::set_clock_observer(publish));
    publish_getcpu();
}

/// Publish whether userspace may answer `getcpu` from the CPU itself.
///
/// Once, from [`init`], and not from the clock observer: this says nothing about
/// the clock and never changes after the APs are up. The CPUs have all written
/// their own id into the register the reader reads by the time `init` runs --
/// `secondary_init` does it before signalling online -- so there is no window
/// in which the flag is on and some CPU would report a stale id.
fn publish_getcpu() {
    let Some(vdso) = vdso() else { return };
    let usable = kernel_hal::cpu::getcpu_usable();
    // SAFETY: see `publish`; same pointer, same lifetime argument.
    unsafe {
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*vdso.data).getcpu_enabled),
            u32::from(usable),
        );
    }
}

/// One line describing why userspace is, or is not, reading the clock without
/// a syscall. Reported by `/proc/perf/kernel`.
///
/// Every way this feature can fail to engage is silent — the guest keeps
/// working and clock reads just stay expensive — so without this the only
/// symptom of a broken vDSO is a benchmark number that did not move, which is
/// indistinguishable from the feature not helping.
pub fn status() -> alloc::string::String {
    use alloc::format;
    if !linux_vdso::AVAILABLE {
        // El motivo lo da la compilacion, no se adivina aqui. Esta linea decia
        // «no encontro un cc utilizable», que es UNO de los motivos: en aarch64
        // y riscv64 no hay imagen porque el vDSO es codigo x86_64, y mandaba a
        // buscar un compilador que nunca fue el problema.
        return match linux_vdso::UNAVAILABLE_REASON {
            Some(r) => format!("sin imagen ({r})"),
            None => "sin imagen (la compilacion no dijo por que)".into(),
        };
    }
    let Some(vdso) = vdso() else {
        return "imagen presente pero no instalada (sin memoria fisica)".into();
    };
    // Read back what userspace would read, rather than recomputing it: the
    // question this answers is what is actually published, not what should be.
    // SAFETY: see `publish`.
    let (enabled, mult) = unsafe {
        (
            core::ptr::read_volatile(core::ptr::addr_of!((*vdso.data).enabled)),
            core::ptr::read_volatile(core::ptr::addr_of!((*vdso.data).tsc_mult)),
        )
    };
    if enabled == 0 {
        return "instalada pero inactiva (CPUID no declara el TSC invariante, \
                asi que clock_gettime sigue siendo un syscall; bajo QEMU/TCG \
                eso es inevitable y se fuerza con VDSOFORCE=1)"
            .into();
    }
    let getcpu =
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*vdso.data).getcpu_enabled)) };
    format!(
        "activa, tsc_mult={} ({}.{:03} ns/tick), getcpu {}",
        mult,
        mult >> 32,
        ((mult & 0xffff_ffff) * 1000) >> 32,
        if getcpu != 0 {
            "sin syscall"
        } else {
            "por syscall (la CPU no declara RDTSCP)"
        },
    )
}

/// Where the image goes in a process: its base address, and that base as an
/// offset into the VMAR, or `None` when it does not fit.
///
/// Immediately below `stack_bottom` with [`STACK_GUARD`] of gap, so a stack
/// overflow faults instead of running quietly into executable pages. Every
/// subtraction is checked: a small `stack_bottom` -- a thread whose stack the
/// loader placed low, or a caller that passed a size where an address was
/// wanted -- would otherwise wrap to an enormous address that `map_ext` might
/// well accept, mapping executable pages nowhere near the stack they were
/// supposed to sit under.
///
/// Split out from [`map_into`] because reaching it there needs a process, a
/// VMAR and a vDSO whose clock is live, and none of the three says anything
/// about the arithmetic.
fn placement(stack_bottom: VirtAddr, vmar_addr: VirtAddr, len: usize) -> Option<(VirtAddr, usize)> {
    let top = stack_bottom.checked_sub(STACK_GUARD)?;
    let base = top.checked_sub(len)?;
    let offset = base.checked_sub(vmar_addr)?;
    Some((base, offset))
}

/// Map the vDSO into a process, just below `stack_bottom`, and return the
/// address to publish as `AT_SYSINFO_EHDR`.
///
/// `None` when there is no vDSO to map, or when the mapping fails. Both are
/// non-fatal: without `AT_SYSINFO_EHDR` the C library never looks for a vDSO
/// and every clock read goes to the kernel, exactly as before this existed.
pub fn map_into(vmar: &Arc<VmAddressRegion>, stack_bottom: VirtAddr) -> Option<VirtAddr> {
    init();
    let vdso = vdso()?;

    // Do not advertise a vDSO that cannot answer. It would still be *correct* —
    // every call returns -ENOSYS and the libc falls through to the syscall —
    // but the process would pay an indirect call before every clock read for
    // nothing, which is worse than never having been offered one.
    //
    // Only checked here, at exec. A TSC demoted later leaves already-running
    // processes holding a mapping that has gone quiet; they take the fallback
    // path and keep telling the right time, which is the whole point of the
    // -ENOSYS contract.
    // SAFETY: see `publish`.
    if unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*vdso.data).enabled)) } == 0 {
        return None;
    }

    let (base, offset) = placement(stack_bottom, vmar.addr(), vdso.len)?;

    // Read and execute, never write. A process that could write the data page
    // could lie to itself about the time and to nothing else, but there is no
    // reason to allow even that — and a writable mapping of a shared physical
    // VMO would let it lie to every other process too.
    let flags = MMUFlags::READ | MMUFlags::EXECUTE | MMUFlags::USER;
    // `map_range: true` — populate both PTEs now rather than taking two page
    // faults on the first clock read. The frames are already there; this is a
    // page-table write, and it makes the very first `clock_gettime` as cheap as
    // every later one instead of paying for the trap this exists to remove.
    match vmar.map_ext(
        Some(offset),
        vdso.vmo.clone(),
        0,
        vdso.len,
        flags,
        flags,
        false,
        true,
        false,
    ) {
        Ok(addr) => {
            debug!("vdso: mapeada en {:#x}..{:#x}", addr, addr + vdso.len);
            Some(addr)
        }
        Err(e) => {
            warn!("vdso: no se pudo mapear en {:#x}: {:?}", base, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zircon_object::vm::PAGE_SIZE;

    /// A `VdsoData` a test owns, so `publish_into` writes somewhere harmless.
    fn scratch_data() -> VdsoData {
        VdsoData {
            enabled: 0xdead_beef,
            getcpu_enabled: 0xdead_beef,
            tsc_mult: 0xdead_beef_dead_beef,
            wall_off_ns: 0xdead_beef_dead_beef,
            tsc_base: 0xdead_beef_dead_beef,
        }
    }

    /// Enabling: the parameters have to be in memory before `enabled` vouches
    /// for them, and both of them, not just the multiplier. A reader that sees
    /// `enabled` and then loads a `wall_off_ns` still holding the previous
    /// boot's value answers `CLOCK_REALTIME` off by that much -- silently, which
    /// is the failure this whole module is arranged to avoid.
    #[test]
    fn turning_the_clock_on_publishes_both_parameters_and_then_enables() {
        let mut d = scratch_data();
        // SAFETY: `d` is a live, aligned `VdsoData` this test owns.
        unsafe { publish_into(&mut d, Some(0x1234_5678_9abc_def0), 42, 7_000) };
        assert_eq!(d.enabled, 1);
        assert_eq!(d.tsc_mult, 0x1234_5678_9abc_def0);
        assert_eq!(d.wall_off_ns, 42);
        assert_eq!(
            d.tsc_base, 7_000,
            "the base is a clock parameter like the rest"
        );
    }

    /// The base is the field this whole struct was extended for, and forgetting
    /// it is not a clock that declines to answer: it is a clock that answers the
    /// time since the machine was last powered on, which on the machine the bug
    /// was found on was 8.2 days ahead of the kernel's own monotonic clock.
    /// Nothing else in userspace would notice, so the check lives here.
    #[test]
    fn enabling_never_leaves_the_base_behind_the_multiplier() {
        let mut d = scratch_data();
        // SAFETY: `d` is a live, aligned `VdsoData` this test owns.
        unsafe { publish_into(&mut d, Some(3), 0, 2_631_000_000_000_000) };
        assert_eq!(d.tsc_base, 2_631_000_000_000_000);
        assert_ne!(
            d.tsc_base, 0xdead_beef_dead_beef,
            "the scratch value survived: the base was never written"
        );
    }

    /// Disabling: `enabled` goes to zero, and the multiplier goes with it. Left
    /// standing, a stale multiplier is what a reader that forgets to check
    /// `enabled` would compute a time from; zero makes that reader return zero,
    /// which is wrong in a way somebody notices.
    #[test]
    fn turning_the_clock_off_clears_the_multiplier_as_well_as_the_flag() {
        let mut d = scratch_data();
        // SAFETY: as above.
        unsafe { publish_into(&mut d, Some(99), 7, 5) };
        // SAFETY: as above.
        unsafe { publish_into(&mut d, None, 8, 5) };
        assert_eq!(d.enabled, 0);
        assert_eq!(d.tsc_mult, 0, "a stale multiplier must not survive");
        assert_eq!(d.wall_off_ns, 8, "the wall offset is published either way");
        assert_eq!(d.tsc_base, 0, "a stale base must not survive either");
    }

    /// The clock's publisher must not touch the `getcpu` flag, in either
    /// direction. The two say nothing about each other -- a machine whose TSC is
    /// no use as a *time source* can still report which CPU it is on -- and the
    /// clock republishes on every recalibration and every `settimeofday`, so a
    /// stray write here would turn the fast path off at the first clock change.
    #[test]
    fn publishing_the_clock_never_touches_the_getcpu_flag() {
        let mut d = scratch_data();
        // SAFETY: as above.
        unsafe { publish_into(&mut d, Some(1), 1, 1) };
        assert_eq!(d.getcpu_enabled, 0xdead_beef);
        // SAFETY: as above.
        unsafe { publish_into(&mut d, None, 1, 1) };
        assert_eq!(d.getcpu_enabled, 0xdead_beef);
    }

    /// Republishing is the ordinary case -- `settimeofday`, a recalibration --
    /// and has to overwrite rather than accumulate.
    #[test]
    fn publishing_again_replaces_what_was_there() {
        let mut d = scratch_data();
        // SAFETY: as above.
        unsafe { publish_into(&mut d, Some(10), 100, 1_000) };
        // SAFETY: as above.
        unsafe { publish_into(&mut d, Some(20), 200, 2_000) };
        assert_eq!(
            (d.enabled, d.tsc_mult, d.wall_off_ns, d.tsc_base),
            (1, 20, 200, 2_000)
        );
    }

    /// The guard page is the whole reason the image is not flush against the
    /// stack: a stack overflow must fault, not run into executable pages.
    #[test]
    fn the_image_sits_a_guard_page_below_the_stack() {
        let len = 2 * PAGE_SIZE;
        let stack_bottom = 0x7fff_0000_0000;
        let (base, offset) = placement(stack_bottom, 0, len).unwrap();
        assert_eq!(base + len + STACK_GUARD, stack_bottom);
        assert_eq!(offset, base, "with the VMAR at zero the two coincide");
        assert_eq!(STACK_GUARD, PAGE_SIZE);
    }

    /// The offset is measured from the VMAR's own base, because that is what
    /// `map_ext` takes. Passing an absolute address as an offset would map the
    /// image at the VMAR's base plus that address.
    #[test]
    fn the_offset_is_measured_from_the_vmars_base() {
        let len = 2 * PAGE_SIZE;
        let vmar = 0x1000_0000;
        let (base, offset) = placement(0x7fff_0000_0000, vmar, len).unwrap();
        assert_eq!(base - vmar, offset);
        assert!(offset < base, "an offset is not an address");
    }

    /// Every subtraction is checked, and this is why: without that, a
    /// `stack_bottom` smaller than the guard plus the image wraps to an
    /// enormous address, and `map_ext` has no way to know it was nonsense.
    #[test]
    fn a_stack_too_low_to_fit_the_image_is_refused_rather_than_wrapped() {
        let len = 2 * PAGE_SIZE;
        assert_eq!(placement(0, 0, len), None, "a zero stack bottom");
        assert_eq!(
            placement(STACK_GUARD - 1, 0, len),
            None,
            "less than the guard"
        );
        assert_eq!(
            placement(STACK_GUARD, 0, len),
            None,
            "the guard and no room"
        );
        assert_eq!(
            placement(STACK_GUARD + len - 1, 0, len),
            None,
            "one byte short"
        );
        assert!(
            placement(STACK_GUARD + len, 0, len).is_some(),
            "exactly enough"
        );
    }

    /// A stack below the VMAR is not this VMAR's stack. Answering with a
    /// wrapped offset would map the image outside the region that was asked
    /// about.
    #[test]
    fn a_stack_below_the_vmar_is_refused() {
        let len = 2 * PAGE_SIZE;
        let vmar = 0x8000_0000;
        assert_eq!(placement(vmar, vmar, len), None);
        assert_eq!(placement(vmar + STACK_GUARD + len - 1, vmar, len), None);
        assert_eq!(
            placement(vmar + STACK_GUARD + len, vmar, len),
            Some((vmar, 0)),
            "flush against the VMAR's base is the first address that fits"
        );
    }

    /// Placement keeps page alignment: the guard is a page and the image is
    /// rounded up to whole pages, so a page-aligned stack gives a page-aligned
    /// base. A misaligned base is a mapping request `map_ext` would refuse.
    #[test]
    fn a_page_aligned_stack_gives_a_page_aligned_base() {
        for len in [PAGE_SIZE, 2 * PAGE_SIZE, 5 * PAGE_SIZE] {
            let (base, offset) = placement(0x7fff_0000_0000, PAGE_SIZE, len).unwrap();
            assert_eq!(base % PAGE_SIZE, 0, "len={}", len);
            assert_eq!(offset % PAGE_SIZE, 0, "len={}", len);
        }
    }

    /// The host suite really does build the image -- contiguous frames, the
    /// copy, the physical VMO, the cache policy -- so everything below is
    /// asking about the real thing and not about a fixture.
    #[test]
    fn the_image_is_actually_installed_on_this_build() {
        assert!(
            linux_vdso::AVAILABLE,
            "this test is about the installed image; a build without one has nothing to check"
        );
        assert!(vdso().is_some(), "{}", status());
    }

    /// What every process maps has to be the image, byte for byte, and the tail
    /// of the last page has to be zero. The frames come from the physical
    /// allocator holding whatever was there before, and the image is shorter
    /// than the pages it occupies: without the zero-fill, every Linux process
    /// in the system gets a readable window onto that leftover kernel memory.
    #[test]
    fn what_userspace_maps_is_the_image_and_then_zeros() {
        let Some(vdso) = vdso() else { return };
        assert_eq!(vdso.len, roundup_pages(linux_vdso::IMAGE_LEN));
        assert_eq!(vdso.vmo.len(), vdso.len);

        let mut got = alloc::vec![0xabu8; vdso.len];
        vdso.vmo.read(0, &mut got).expect("the image reads back");
        assert_eq!(
            &got[..linux_vdso::IMAGE_LEN],
            linux_vdso::IMAGE,
            "the mapped image differs from the one that was linked"
        );
        assert!(
            got[linux_vdso::IMAGE_LEN..].iter().all(|&b| b == 0),
            "the tail of the last page is leftover physical memory unless it is zeroed"
        );
    }

    /// The image's own clock bytes are zero as linked, which is what makes the
    /// zero-fill in `build` and the copy length agree: copying `IMAGE_LEN` bytes
    /// and copying only the first page produce the same memory. Worth pinning,
    /// because it is also why `publish` may write there without reading first.
    #[test]
    fn the_clock_bytes_of_the_linked_image_are_zero() {
        assert!(
            linux_vdso::IMAGE[linux_vdso::DATA_OFFSET..]
                .iter()
                .all(|&b| b == 0),
            "the linked image already carries clock parameters"
        );
        assert_eq!(
            linux_vdso::IMAGE_LEN - linux_vdso::DATA_OFFSET,
            core::mem::size_of::<VdsoData>(),
            "the image ends exactly at the end of the clock struct"
        );
    }

    /// The clock lives inside those frames, and `publish` writes straight
    /// through a raw pointer at `DATA_OFFSET`. If the struct ran off the end,
    /// those writes would land on whatever follows the image's frames.
    #[test]
    fn the_clock_struct_lies_wholly_inside_the_frames_that_were_allocated() {
        let Some(vdso) = vdso() else { return };
        let end = linux_vdso::DATA_OFFSET + core::mem::size_of::<VdsoData>();
        assert!(end <= vdso.len, "{end} > {}", vdso.len);
        assert_eq!(
            linux_vdso::DATA_OFFSET % core::mem::align_of::<VdsoData>(),
            0,
            "the writes are through a *mut VdsoData, so the offset has to be aligned"
        );
    }

    /// The `data` pointer has to address the clock page of the frames this
    /// build installed, and not some other page of them.
    #[test]
    fn the_data_pointer_addresses_the_clock_page_of_the_image() {
        let Some(vdso) = vdso() else { return };
        let mut page = alloc::vec![0xabu8; core::mem::size_of::<VdsoData>()];
        vdso.vmo
            .read(linux_vdso::DATA_OFFSET, &mut page)
            .expect("the clock page reads back");
        // SAFETY: `data` is the direct-map address of that same offset.
        let live = unsafe { core::ptr::read_volatile(vdso.data) };
        assert_eq!(
            page,
            unsafe {
                core::slice::from_raw_parts(
                    &live as *const VdsoData as *const u8,
                    core::mem::size_of::<VdsoData>(),
                )
            },
            "the VMO and the kernel pointer are two views of one page"
        );
    }

    /// Being asked twice must not build twice: the frames are leaked on
    /// purpose, so a second build would leak a second image and leave every
    /// process that already mapped the first one reading a clock nobody
    /// updates.
    #[test]
    fn the_image_is_built_once_and_handed_out_again() {
        let first = vdso().map(|v| v.data as usize);
        let second = vdso().map(|v| v.data as usize);
        assert_eq!(first, second);
        init();
        init();
        assert_eq!(vdso().map(|v| v.data as usize), first);
    }

    /// A vDSO whose clock is off answers -ENOSYS for every call, so offering it
    /// costs an indirect call before every clock read and buys nothing. libos
    /// answers `vdso_tsc_mult()` with a hardcoded `None`, which makes this the
    /// case the host suite is always in -- and the one the check exists for.
    #[test]
    fn a_vdso_with_no_clock_is_not_advertised_to_the_process() {
        let Some(vdso) = vdso() else { return };
        // SAFETY: `data` addresses the live clock struct.
        let enabled =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*vdso.data).enabled)) };
        if enabled != 0 {
            return;
        }
        let vmar = VmAddressRegion::new_root();
        assert_eq!(
            map_into(&vmar, vmar.addr() + 0x10_0000),
            None,
            "an inactive clock must not be published as AT_SYSINFO_EHDR"
        );
    }

    /// `status()` is the only symptom a broken vDSO has -- every other way it
    /// can fail to engage is silent -- so it has to name the reason rather than
    /// report success for a clock that is not running.
    #[test]
    fn the_status_line_says_which_of_the_ways_this_can_fail_happened() {
        let s = status();
        let Some(vdso) = vdso() else {
            assert!(
                s.contains("sin imagen") || s.contains("sin memoria fisica"),
                "no vDSO, but the reason given was {:?}",
                s
            );
            return;
        };
        // SAFETY: `data` addresses the live clock struct.
        let enabled =
            unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*vdso.data).enabled)) };
        if enabled == 0 {
            assert!(s.contains("inactiva"), "{:?}", s);
            assert!(
                !s.contains("activa,"),
                "an inactive clock reported as active: {:?}",
                s
            );
        } else {
            assert!(s.contains("activa, tsc_mult="), "{:?}", s);
        }
    }

    /// The aux-vector tag a C library looks the vDSO up by. Get this wrong and
    /// the library never finds the image, which looks exactly like not having
    /// one.
    #[test]
    fn the_aux_vector_tag_is_the_one_linux_uses() {
        assert_eq!(AT_SYSINFO_EHDR, 33);
    }
}
