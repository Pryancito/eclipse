//! Device drivers.

use alloc::{sync::Arc, vec::Vec};
use core::convert::From;

use crate::sync::{RwLock, RwLockReadGuard};

use zcore_drivers::scheme::{
    AudioScheme, BlockScheme, DisplayScheme, DrmScheme, InputScheme, IrqScheme, NetScheme, Scheme,
    UartScheme,
};
use zcore_drivers::{Device, DeviceError};

/// Re-exported modules from crate [`zcore_drivers`].
pub use zcore_drivers::{prelude, scheme, utils};

/// A wrapper of a device array with the same [`Scheme`].
pub struct DeviceList<T: Scheme + ?Sized>(RwLock<Vec<Arc<T>>>);

impl<T: Scheme + ?Sized> DeviceList<T> {
    fn add(&self, dev: Arc<T>) {
        self.0.write().push(dev);
    }

    /// Drop EVERY entry that IS `dev` (same allocation, not merely equal),
    /// returning whether any were found. Hosted builds only -- see
    /// [`remove_device_hosted`].
    ///
    /// Every entry, not the first one: [`Self::add`] is an unconditional
    /// `push`, so the same `Arc` can be in here twice (`sysfs`'s `Disks` adds
    /// one per disk name it is given, and the software-KMS emulation attaches
    /// its pair on each `open`). Dropping only the first and answering `true`
    /// said the device was gone while a live copy stayed in the list -- and
    /// because every lookup that matters takes the FIRST entry
    /// (`primary_display`, `all_uart().first()`), the copy is the one the
    /// kernel would then go on using.
    #[cfg(feature = "libos")]
    fn remove(&self, dev: &Arc<T>) -> bool {
        let mut list = self.0.write();
        let before = list.len();
        list.retain(|d| !Arc::ptr_eq(d, dev));
        before != list.len()
    }

    /// Convert self into a vector.
    pub fn as_vec(&self) -> RwLockReadGuard<'_, Vec<Arc<T>>> {
        self.0.read()
    }

    /// Returns the device at given position, or `None` if out of bounds.
    pub fn try_get(&self, idx: usize) -> Option<Arc<T>> {
        self.0.read().get(idx).cloned()
    }

    /// Returns the device with the given name, or `None` if not found.
    pub fn find(&self, name: &str) -> Option<Arc<T>> {
        self.0.read().iter().find(|d| d.name() == name).cloned()
    }

    /// Returns the first device of this device array, or `None` if it is empty.
    pub fn first(&self) -> Option<Arc<T>> {
        self.try_get(0)
    }

    /// Returns the first device of this device array.
    ///
    /// # Panic
    ///
    /// Panics if the array is empty.
    pub fn first_unwrap(&self) -> Arc<T> {
        self.first()
            .unwrap_or_else(|| panic!("device not initialized: {}", core::any::type_name::<T>()))
    }
}

impl<T: Scheme + ?Sized> Default for DeviceList<T> {
    fn default() -> Self {
        Self(RwLock::new(Vec::new()))
    }
}

#[derive(Default)]
struct AllDeviceList {
    block: DeviceList<dyn BlockScheme>,
    display: DeviceList<dyn DisplayScheme>,
    input: DeviceList<dyn InputScheme>,
    irq: DeviceList<dyn IrqScheme>,
    net: DeviceList<dyn NetScheme>,
    uart: DeviceList<dyn UartScheme>,
    drm: DeviceList<dyn DrmScheme>,
    audio: DeviceList<dyn AudioScheme>,
}

impl AllDeviceList {
    #[cfg(feature = "libos")]
    pub fn remove_device(&self, dev: &Device) -> bool {
        match dev {
            Device::Block(d) => self.block.remove(d),
            Device::Display(d) => self.display.remove(d),
            Device::Input(d) => self.input.remove(d),
            Device::Irq(d) => self.irq.remove(d),
            Device::Net(d) => self.net.remove(d),
            Device::Uart(d) => self.uart.remove(d),
            Device::Drm(d) => self.drm.remove(d),
            // Registered as a pair, so "removed" means both halves went. `||`
            // reported success when only one did, and the caller -- which is a
            // test emulation detaching the display it attached -- believed it
            // and moved on, leaving the other half registered for every later
            // test in the same process.
            Device::DrmDisplay(drm, display) => {
                let a = self.drm.remove(drm);
                let b = self.display.remove(display);
                if a != b {
                    warn!(
                        "remove_device: half a DrmDisplay pair was registered (drm={}, display={})",
                        a, b
                    );
                }
                a && b
            }
            Device::Audio(d) => self.audio.remove(d),
        }
    }

    pub fn add_device(&self, dev: Device) {
        match dev {
            Device::Block(d) => self.block.add(d),
            Device::Display(d) => self.display.add(d),
            Device::Input(d) => self.input.add(d),
            Device::Irq(d) => self.irq.add(d),
            Device::Net(d) => self.net.add(d),
            Device::Uart(d) => self.uart.add(d),
            Device::Drm(d) => self.drm.add(d),
            Device::DrmDisplay(drm, display) => {
                self.drm.add(drm);
                self.display.add(display);
            }
            Device::Audio(d) => self.audio.add(d),
        }
    }
}

lazy_static! {
    static ref DEVICES: AllDeviceList = AllDeviceList::default();
}

pub(crate) fn add_device(dev: Device) {
    DEVICES.add_device(dev)
}

/// Register a device from a hosted (libos) build.
///
/// On bare metal every device comes from this crate's own bus probes, so
/// `add_device` stays crate-private there. A hosted build has no buses: the
/// devices it owns are mocks, and the ones a *unit test* needs are whatever
/// the code under test reaches for through `all_*()`. The DRM software-KMS
/// path is the case that forced this open — `primary_display()` is
/// `all_display().first()`, so with nothing registered `software_kms_active()`
/// is false, the synthetic CRTC/connector never exist, and every present
/// stops at `PresentError::NoDisplay` before a single pixel is copied. Half
/// the present path was therefore unreachable from a test on a machine with
/// no GPU, which is every machine in CI.
///
/// Note there is deliberately no *removal*: `DeviceList` is append-only, and
/// `primary_display()` takes the first entry, so the first display a process
/// registers is the one every later caller sees. A test-side emulation
/// registers exactly one device and reprograms it instead (see
/// `linux-object`'s `kms_emu`).
#[cfg(feature = "libos")]
pub fn add_device_hosted(dev: Device) {
    DEVICES.add_device(dev)
}

/// Unregister a device that [`add_device_hosted`] registered, matched by
/// identity rather than by value, and report whether one was found.
///
/// The device lists are process-wide and a unit-test binary runs every test of
/// a crate in one process, so a test that registers an emulated display to
/// exercise the scanout path would otherwise leave it registered for the tests
/// that assert there is no display at all -- and `primary_display()` takes the
/// FIRST entry, so it would win over anything registered later too. A test
/// therefore attaches its device for its own duration and detaches it here.
/// Bare metal never unregisters a device, which is why this is hosted-only.
#[cfg(feature = "libos")]
pub fn remove_device_hosted(dev: &Device) -> bool {
    DEVICES.remove_device(dev)
}

/// Returns all devices which implement the [`BlockScheme`].
pub fn all_block() -> &'static DeviceList<dyn BlockScheme> {
    &DEVICES.block
}

/// Returns all devices which implement the [`DisplayScheme`].
pub fn all_display() -> &'static DeviceList<dyn DisplayScheme> {
    &DEVICES.display
}

/// Returns all devices which implement the [`InputScheme`].
pub fn all_input() -> &'static DeviceList<dyn InputScheme> {
    &DEVICES.input
}

/// Returns all devices which implement the [`IrqScheme`].
pub fn all_irq() -> &'static DeviceList<dyn IrqScheme> {
    &DEVICES.irq
}

/// Cached `&'static dyn IrqScheme` for the primary (boot-time) IRQ
/// controller — i.e. what `all_irq().first_unwrap()` would resolve to. The
/// IRQ-dispatch hot path runs on every interrupt (timer × 250 Hz × N CPUs,
/// plus every device IRQ), and going through the regular accessor each time
/// would acquire an `RwLock` and clone an `Arc` per interrupt. The primary
/// controller is registered at boot and never replaced, so we can stash it
/// once and hand out a borrowed reference.
///
/// The lookup happens OUTSIDE `call_once`. `first_unwrap` panics while no
/// controller is registered yet, and a panic that escapes `call_once` poisons a
/// `spin::Once` **permanently**: every later call then panics with
/// `Once panicked` instead of resolving, even once the controller exists. One
/// early call -- a device IRQ arriving before `irq_init`, or a probe asking
/// whether a GSI is valid -- would therefore have turned the interrupt path
/// into a panic for the rest of the boot, which is not something a second
/// attempt could undo. Panicking on the way in is fine; panicking inside is
/// what costs the cache.
pub fn primary_irq() -> &'static (dyn IrqScheme + Send + Sync + 'static) {
    static PRIMARY_IRQ: spin::Once<Arc<dyn IrqScheme>> = spin::Once::new();
    if let Some(arc) = PRIMARY_IRQ.get() {
        return &**arc;
    }
    let first = all_irq().first_unwrap();
    &**PRIMARY_IRQ.call_once(|| first)
}

/// Returns all devices which implement the [`NetScheme`].
pub fn all_net() -> &'static DeviceList<dyn NetScheme> {
    &DEVICES.net
}

/// Returns all devices which implement the [`UartScheme`].
pub fn all_uart() -> &'static DeviceList<dyn UartScheme> {
    &DEVICES.uart
}

/// Returns all devices which implement the [`DrmScheme`].
pub fn all_drm() -> &'static DeviceList<dyn DrmScheme> {
    &DEVICES.drm
}

/// Returns all devices which implement the [`AudioScheme`].
pub fn all_audio() -> &'static DeviceList<dyn AudioScheme> {
    &DEVICES.audio
}

/// What the last NVIDIA HDMI/DP audio enable did, or why it never ran (see
/// `zcore_drivers::display::hdmi_audio_status`). Surfaced at `/proc/gpusnd`,
/// because on a GPU a healthy-looking codec proves nothing on its own: the
/// display engine is the half that puts audio on the cable.
#[cfg(target_arch = "x86_64")]
pub fn hdmi_audio_status() -> alloc::string::String {
    zcore_drivers::display::hdmi_audio_status()
}
/// The only one of these display knobs that had no non-x86_64 half, so its one
/// caller (`/proc/gpusnd`) had to carry the `cfg` itself -- and a second caller
/// written without it would simply not build on aarch64 or riscv64.
#[cfg(not(target_arch = "x86_64"))]
pub fn hdmi_audio_status() -> alloc::string::String {
    alloc::string::String::from(
        "[gpusnd] display side: no NVIDIA display engine on this architecture\n",
    )
}

/// Enables the nouveau-compatible driver-specific ioctl surface on the
/// NVIDIA DRM driver (see `zcore_drivers::display::nouveau_uapi` and
/// `docs/README-nouveau-uapi.md`). No-op where the NVIDIA driver doesn't
/// exist (non-x86_64 builds).
#[cfg(target_arch = "x86_64")]
pub fn set_nouveau_uapi_enabled(v: bool) {
    zcore_drivers::display::set_nouveau_uapi_enabled(v);
}

/// Enables on-demand console-GPU GSP bring-up (`nvidia.console_gsp`). Default off.
#[cfg(target_arch = "x86_64")]
pub fn set_console_gsp_enabled(v: bool) {
    zcore_drivers::display::set_console_gsp_enabled(v);
}
#[cfg(not(target_arch = "x86_64"))]
pub fn set_console_gsp_enabled(_v: bool) {}

#[cfg(target_arch = "x86_64")]
pub fn console_gsp_enabled() -> bool {
    zcore_drivers::display::console_gsp_enabled()
}
#[cfg(not(target_arch = "x86_64"))]
pub fn console_gsp_enabled() -> bool {
    false
}

/// Hands the NVIDIA RM a provider of real per-thread identity (see
/// `zcore_drivers::display::set_rm_thread_id_provider`). No-op off x86_64.
#[cfg(target_arch = "x86_64")]
pub fn set_rm_thread_id_provider(f: fn() -> u64) {
    zcore_drivers::display::set_rm_thread_id_provider(f);
}
#[cfg(not(target_arch = "x86_64"))]
pub fn set_rm_thread_id_provider(_f: fn() -> u64) {}
#[cfg(not(target_arch = "x86_64"))]
pub fn set_nouveau_uapi_enabled(_v: bool) {}

/// Whether the nouveau-uAPI `EXEC` ioctl takes the direct-submit path (GP
/// entries, GPPut and doorbell written by the kernel, fence resolved lazily)
/// or enters the NVIDIA RM on every submission. Default on; the
/// `nvidia.exec_rm` cmdline flag turns it off for A/B measurement.
#[cfg(target_arch = "x86_64")]
pub fn set_exec_fast_enabled(v: bool) {
    zcore_drivers::display::set_exec_fast_enabled(v);
}
#[cfg(not(target_arch = "x86_64"))]
pub fn set_exec_fast_enabled(_v: bool) {}

/// Opt-in CE present on DRM `page_flip` (`nvidia.hwflip`). Default off.
#[cfg(target_arch = "x86_64")]
pub fn set_hwflip_enabled(v: bool) {
    zcore_drivers::display::set_hwflip_enabled(v);
}
#[cfg(not(target_arch = "x86_64"))]
pub fn set_hwflip_enabled(_v: bool) {}

#[cfg(target_arch = "x86_64")]
pub fn hwflip_enabled() -> bool {
    zcore_drivers::display::hwflip_enabled()
}
#[cfg(not(target_arch = "x86_64"))]
pub fn hwflip_enabled() -> bool {
    false
}

/// Opt-in NVC57E ISO surface flip (`nvidia.surfaceflip`). Default off.
#[cfg(target_arch = "x86_64")]
pub fn set_surfaceflip_enabled(v: bool) {
    zcore_drivers::display::set_surfaceflip_enabled(v);
}
#[cfg(not(target_arch = "x86_64"))]
pub fn set_surfaceflip_enabled(_v: bool) {}

#[cfg(target_arch = "x86_64")]
pub fn surfaceflip_enabled() -> bool {
    zcore_drivers::display::surfaceflip_enabled()
}
#[cfg(not(target_arch = "x86_64"))]
pub fn surfaceflip_enabled() -> bool {
    false
}

/// Whether the nouveau-compatible ioctl surface is currently enabled --
/// the read side of [`set_nouveau_uapi_enabled`], needed by
/// `linux-syscall` (which, unlike `linux-object`, has no direct
/// `zcore-drivers` dependency on the real kernel target -- only under the
/// `libos` feature) to gate `SYNCOBJ_HANDLE_TO_FD`/`FD_TO_HANDLE`.
#[cfg(target_arch = "x86_64")]
pub fn nouveau_uapi_enabled() -> bool {
    zcore_drivers::display::nouveau_uapi_enabled()
}
#[cfg(not(target_arch = "x86_64"))]
pub fn nouveau_uapi_enabled() -> bool {
    false
}

/// Writes a summary of registered graphics devices to the kernel log (dmesg only).
///
/// `active_console` describes which output path is driving the graphical console, if any.
pub fn klog_graphics_device_summary(active_console: Option<&str>) {
    #[cfg(not(feature = "graphic"))]
    {
        crate::klog_info!(
            "graphics: kernel built without `graphic` feature — framebuffer console disabled"
        );
    }

    let displays = all_display().as_vec();
    let drms = all_drm().as_vec();
    let nd = displays.len();
    let nr = drms.len();

    if nd == 0 && nr == 0 {
        crate::klog_info!("graphics: no framebuffer (Display) or DRM devices registered");
    } else {
        crate::klog_info!(
            "graphics: {} framebuffer device(s), {} DRM / GPU device(s)",
            nd,
            nr
        );

        for (i, d) in displays.iter().enumerate() {
            let info = d.info();
            let fb_kib = info.fb_size.saturating_add(1023) / 1024;
            crate::klog_info!(
                "graphics: display[{}] driver={} {}x{} {:?} pitch={} bpp={} fb≈{} KiB",
                i,
                d.name(),
                info.width,
                info.height,
                info.format,
                info.pitch(),
                info.format.depth(),
                fb_kib,
            );
        }

        for (i, d) in drms.iter().enumerate() {
            let c = d.get_caps();
            crate::klog_info!(
                "graphics: drm[{}] driver={} max_mode={}x{} 3d={} cursor={}",
                i,
                d.name(),
                c.max_width,
                c.max_height,
                c.has_3d,
                c.has_cursor,
            );
        }
    }

    match active_console {
        Some(note) if !note.is_empty() => {
            crate::klog_info!("graphics: active framebuffer console: {}", note);
        }
        _ => {
            #[cfg(feature = "graphic")]
            crate::klog_info!("graphics: active framebuffer console: (none — serial/text only)");
        }
    }
}

/// Publish one kernel-log line that arrived across the C ABI, keeping whatever
/// prefix of it is valid UTF-8.
///
/// `zcore_drivers`' `klog_emit` formats into a fixed 256-byte buffer and
/// truncates at a byte boundary, so any line longer than that whose cut lands
/// inside a multi-byte character arrives here as invalid UTF-8 -- and driver log
/// lines do carry them (`—` in the e1000e MAC warning and the AHCI reset
/// timeout, `≈` in the graphics summary below). Dropping the line on a failed
/// `from_utf8` threw away the readable part too, so the one dmesg line saying
/// what a driver had just decided disappeared for the sake of its last three
/// bytes. Publish the valid prefix instead, and when not even the first byte is
/// valid, leave a line saying a line was lost: a driver log that goes silent
/// looks exactly like a driver that stopped logging.
///
/// # Safety
///
/// `msg` must be valid for reads of `len` bytes, as `drivers_klog_emit`'s
/// contract requires.
unsafe fn klog_emit_ffi(priority: u8, msg: *const u8, len: usize) {
    if msg.is_null() || len == 0 {
        return;
    }
    let slice = core::slice::from_raw_parts(msg, len);
    match core::str::from_utf8(slice) {
        Ok(s) => crate::console::klog_emit(priority, s),
        Err(e) if e.valid_up_to() > 0 => {
            // `valid_up_to()` is by definition a UTF-8 boundary.
            let good = core::str::from_utf8_unchecked(&slice[..e.valid_up_to()]);
            crate::console::klog_emit(priority, good);
        }
        Err(_) => crate::console::klog_emit(
            priority,
            "[klog] a driver log line was dropped: not UTF-8 from its first byte\n",
        ),
    }
}

impl From<DeviceError> for crate::HalError {
    fn from(err: DeviceError) -> Self {
        warn!("{:?}", err);
        Self
    }
}

/// Re-enter the quarantine after a userspace pin that was holding a free has
/// dropped. Called from `stack_guard::dma_unpin_user`.
#[cfg(not(feature = "libos"))]
pub(crate) fn dma_quarantine_release_held(paddr: crate::PhysAddr, pages: usize) {
    dma_quarantine_dealloc(paddr, pages);
}

/// FIFO quarantine + poison-trap for freed DMA blocks, shared by
/// `virtio_dma_dealloc` and `drivers_dma_dealloc`.
///
/// Freed DMA memory used to go straight back to the frame pool, so a device
/// descriptor or a userspace `VmObject::new_physical` mapping still pointing at
/// a just-freed block could write it AFTER `frame_alloc` had already handed the
/// recycled frames to a fresh coroutine stack (or another process's memory) —
/// the SMP `[null-exec]` zero-smash and the GEM-recycle SIGSEGV that crashes a
/// wl_shm client (lunarbg) at desktop start on a `killall`+relaunch: the killed
/// session's teardown drops its VM_BIND / userspace mappings correctly, but a
/// GPU engine with in-flight work (or a stale ring descriptor) still writes the
/// physical frames AFTER the mapping is gone — which no mapping-refcount can
/// see. Holding the freed block out of circulation for a window lets that
/// in-flight DMA drain onto memory nothing owns yet; the poison in the first
/// word of every page names a stale writer as a one-shot `[dma-uaf]` report
/// instead of a silent corruption.
///
/// The block that matters most here is a GPU framebuffer/texture — MiB-sized.
/// The old ring capped entries at 32 pages, so exactly those large GEM buffers
/// SKIPPED the quarantine and were recycled immediately: the crash's real hole.
/// This FIFO covers blocks up to `MAX_BLOCK_PAGES` and evicts the oldest once
/// either the block count or the total held RAM (`BUDGET_PAGES`) is exceeded, so
/// large buffers are held without letting the held RAM grow unbounded. A single
/// block larger than `MAX_BLOCK_PAGES` still returns immediately (its own UAF
/// window is comparatively tiny and holding it would blow the budget alone).
#[cfg(not(feature = "libos"))]
fn dma_quarantine_dealloc(paddr: crate::PhysAddr, pages: usize) {
    use crate::dma_quarantine::{Quarantine, Verdict};
    use lock::Mutex;
    const POISON: u64 = 0xDEAD_D3AD_F0F0_F0F0;

    // Which block to evict and when is `crate::dma_quarantine`, which compiles
    // everywhere and is tested; what is left here is the frame pool, the
    // poison and the report.
    static QUAR: Mutex<Quarantine> = Mutex::new(Quarantine::new());

    let free_now = |base: usize, n: usize| {
        for i in 0..n {
            crate::KHANDLER.frame_dealloc(base + i * crate::PAGE_SIZE);
        }
    };
    if pages == 0 {
        return;
    }
    // Userspace still maps this range through a physical VMO (nouveau GEM
    // CPU-mmap). Hold the free until the last pin drops — returning to the
    // pool now is the free-while-mapped UAF. One call, so the last unpin
    // cannot land between asking and parking and leave the block parked
    // behind a pin that no longer exists.
    if crate::stack_guard::dma_hold_if_pinned(paddr, pages) {
        return;
    }
    let phys_to_va = |pa: usize| pa + crate::KCONFIG.phys_to_virt_offset;
    // Make room for the new block (evict the OLDEST until within both bounds),
    // then enqueue it. Collect the evicted blocks and process them AFTER the
    // lock is dropped: `frame_dealloc` and the clflush/read loop must not run
    // under the quarantine lock.
    let mut evicted: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    // The poison goes into the first word of every page, and only for a block
    // the quarantine actually takes.
    let poison = || unsafe {
        for i in 0..pages {
            core::ptr::write_volatile(phys_to_va(paddr + i * crate::PAGE_SIZE) as *mut u64, POISON);
        }
    };
    match QUAR.lock().push(paddr, pages, &mut evicted, poison) {
        Verdict::Held => {}
        Verdict::TooBig => {
            free_now(paddr, pages);
            return;
        }
        Verdict::DoubleFree => {
            // Returning these frames to the pool a second time would hand one
            // frame to two owners, which is the corruption everything here
            // exists to catch. Drop the free and name it: the quarantine is
            // the one place in the system that can see this.
            crate::console::serial_write_fmt_spin(format_args!(
                "\n[dma-double-free] paddr={:#x} pages={} was freed again while still in \
                 quarantine — dropping the second free; one more and the frame pool would \
                 have handed these pages to two owners.\n",
                paddr, pages,
            ));
            return;
        }
    }
    for (old_base, old_pages) in evicted {
        // Verify the evicted block's poison. Flush the sentinel line first so a
        // (cache-coherent) device DMA that overwrote it is not masked by a stale
        // cache line still holding the poison this CPU wrote.
        let mut smashed_page = usize::MAX;
        let mut smashed_val = 0u64;
        unsafe {
            for i in 0..old_pages {
                let va = phys_to_va(old_base + i * crate::PAGE_SIZE);
                #[cfg(target_arch = "x86_64")]
                core::arch::x86_64::_mm_clflush(va as *const u8);
                let w = core::ptr::read_volatile(va as *const u64);
                if w != POISON {
                    smashed_page = i;
                    smashed_val = w;
                    break;
                }
            }
        }
        if smashed_page != usize::MAX {
            crate::console::serial_write_fmt_spin(format_args!(
                "\n[dma-uaf] STALE WRITE into a freed DMA block while quarantined: \
                 paddr={:#x} pages={} page={} word0={:#x} (expected poison {:#x}) — a device \
                 descriptor or a userspace mapping wrote memory the driver already freed; this is \
                 the SMP null-exec / GEM-recycle corruptor.\n",
                old_base, old_pages, smashed_page, smashed_val, POISON,
            ));
        }
        free_now(old_base, old_pages);
    }
}

#[cfg(not(feature = "libos"))]
mod virtio_drivers_ffi {
    use crate::{PhysAddr, VirtAddr, KCONFIG, KHANDLER};

    #[unsafe(no_mangle)]
    extern "C" fn virtio_dma_alloc(pages: usize) -> PhysAddr {
        let paddr = KHANDLER.frame_alloc_contiguous(pages, 0).unwrap_or_else(|| {
            panic!(
                "virtio_dma_alloc: no hay {} páginas físicas contiguas (RAM insuficiente o fragmentación)",
                pages
            );
        });
        trace!("alloc DMA: paddr={:#x}, pages={}", paddr, pages);
        paddr
    }

    #[unsafe(no_mangle)]
    extern "C" fn virtio_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32 {
        // See `drivers_dma_dealloc`: record the freed block for the fault
        // path's device/mapping-UAF detector (virtio-gpu/blk rings go here).
        crate::stack_guard::dma_free_note(paddr, pages);
        // Quarantine before the frames re-enter the pool. A device descriptor
        // (NIC RX ring, virtio-gpu/blk, GPU pushbuffer/GEM, NVMe completion) or a
        // userspace `VmObject::new_physical` mapping (the nouveau GEM CPU-mmap
        // path) can still reference a just-freed block and write it after the
        // driver moved on; `frame_alloc` then hands the recycled memory to a
        // fresh coroutine stack and the stale write zeroes a live frame — the SMP
        // `[null-exec]` smash. Holding the block out of circulation for a window
        // closes that race; the poison-trap on eviction names the writer in one
        // run. (See `dma_quarantine_dealloc`.)
        super::dma_quarantine_dealloc(paddr, pages);
        trace!("dealloc DMA: paddr={:#x}, pages={}", paddr, pages);
        0
    }

    #[unsafe(no_mangle)]
    extern "C" fn virtio_phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        paddr + KCONFIG.phys_to_virt_offset
    }

    #[unsafe(no_mangle)]
    extern "C" fn virtio_virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        {
            use crate::vm::{GenericPageTable, PageTable};
            let pt = PageTable::from_current();
            if let Ok((paddr, _, _)) = pt.query(vaddr) {
                return paddr;
            }
        }
        vaddr - KCONFIG.phys_to_virt_offset
    }
}

/// Minimal FFI shims for hosted (libos) builds: zcore_drivers' portable code
/// (e.g. the loopback NetScheme) still links against these symbols.
#[cfg(feature = "libos")]
mod drivers_ffi_libos {
    use crate::hal_fn::timer::timer_now;

    #[no_mangle]
    extern "C" fn drivers_timer_now_as_micros() -> u64 {
        timer_now().as_micros() as _
    }

    // The scheduler's `wait_for_interrupt` calls `hal_cpu_idle`. On bare metal
    // that halts the CPU; under libos we run in a host user process where `hlt`
    // would fault, so just spin briefly and let the host scheduler reclaim us.
    #[no_mangle]
    extern "C" fn hal_cpu_idle() {
        core::hint::spin_loop();
    }

    // `zcore_drivers` always links its kernel-log helper; provide the emitter in
    // libos too (forwards to the registered dmesg sink, or a no-op if none),
    // otherwise the hosted build fails to link with `undefined: drivers_klog_emit`.
    // Both copies of this shim are the same three lines over `klog_emit_ffi`,
    // which is where the byte-slice handling lives: it used to be duplicated,
    // and a fix to one copy would have left the other as it was.
    #[no_mangle]
    extern "C" fn drivers_klog_emit(priority: u8, msg: *const u8, len: usize) {
        unsafe { super::klog_emit_ffi(priority, msg, len) }
    }

    // `zcore_drivers::utils::dma::DmaRegion` (used by every PCI NIC/storage
    // driver, all of which are linked even though no PCI bus exists under
    // libos) references these in its alloc/Drop glue. Back them with the
    // hosted frame allocator; no UC-remap bookkeeping applies here.
    use crate::{PhysAddr, KHANDLER};
    #[no_mangle]
    extern "C" fn drivers_dma_alloc(pages: usize) -> PhysAddr {
        KHANDLER
            .frame_alloc_contiguous(pages, 0)
            .expect("drivers_dma_alloc (libos): out of contiguous frames")
    }

    #[no_mangle]
    extern "C" fn drivers_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32 {
        for i in 0..pages {
            KHANDLER.frame_dealloc(paddr + i * crate::PAGE_SIZE);
        }
        0
    }

    #[no_mangle]
    extern "C" fn drivers_dma_mark_uncached(_paddr: PhysAddr, _pages: usize) -> i32 {
        0
    }

    #[no_mangle]
    extern "C" fn drivers_dma_verify_uncached(_paddr: PhysAddr, _pages: usize) -> i32 {
        0
    }
}

#[cfg(not(feature = "libos"))]
mod drivers_ffi {
    use crate::{PhysAddr, VirtAddr, KCONFIG, KHANDLER, PAGE_SIZE};

    #[unsafe(no_mangle)]
    extern "C" fn drivers_dma_alloc(pages: usize) -> PhysAddr {
        let paddr = KHANDLER.frame_alloc_contiguous(pages, 0).unwrap_or_else(|| {
            panic!(
                "drivers_dma_alloc: no hay {} páginas físicas contiguas (RAM insuficiente o fragmentación)",
                pages
            );
        });
        trace!("alloc DMA: paddr={:#x}, pages={}", paddr, pages);
        paddr
    }

    #[unsafe(no_mangle)]
    extern "C" fn drivers_dma_dealloc(paddr: PhysAddr, pages: usize) -> i32 {
        // Restore the kernel-physmap mapping of every page to the default
        // cacheable (WB) state BEFORE returning it to the general frame pool.
        // `drivers_dma_mark_uncached` flips these PTEs to UC for DMA buffers
        // (NIC rings, the NVIDIA RM's UNCACHED sysmem allocs); without this
        // restore, a freed page re-enters the pool with a poisoned UC kernel
        // alias. The next owner (a userspace VMO page, mapped WB in the user's
        // page table) then has two aliases with CONFLICTING memory types —
        // architecturally undefined on x86 — and every kernel access through
        // the physmap (e.g. `PhysFrame::zero()` on commit) goes uncached
        // underneath the user's cached view. Stale dirty lines evicting later
        // silently overwrite the new owner's data: random SIGSEGVs in fresh
        // processes while old ones stay healthy.
        {
            use crate::vm::{GenericPageTable, PageTable};
            use crate::{CachePolicy, MMUFlags};
            let vaddr_base = paddr + KCONFIG.phys_to_virt_offset;
            let mut pt = PageTable::from_current();
            for i in 0..pages {
                let va = vaddr_base + i * PAGE_SIZE;
                if let Ok((_, flags, _)) = pt.query(va) {
                    if flags.bits() & 3 != CachePolicy::Cached as usize {
                        // Flush any lines for this physical page first (lines are
                        // physically tagged, so flushing via this alias covers
                        // every mapping), then re-flag the PTE WB.
                        #[cfg(target_arch = "x86_64")]
                        unsafe {
                            for line in (0..PAGE_SIZE).step_by(64) {
                                core::arch::x86_64::_mm_clflush((va + line) as *const u8);
                            }
                        }
                        let _ = pt.update(va, None, Some(MMUFlags::READ | MMUFlags::WRITE));
                    }
                }
            }
            core::mem::forget(pt);
        }
        // Record the block BEFORE returning it to the pool, so the
        // null-execute/soft-smash fault path can later recognise a corrupted
        // stack frame that was a DMA buffer freed while a device descriptor or
        // a userspace `VmObject::new_physical` mapping still referenced it (the
        // device/mapping UAF the physmap guard cannot see). Diagnostic only.
        crate::stack_guard::dma_free_note(paddr, pages);
        // Quarantine before the frames re-enter the pool. A device descriptor
        // (NIC RX ring, virtio-gpu/blk, GPU pushbuffer/GEM, NVMe completion) or a
        // userspace `VmObject::new_physical` mapping (the nouveau GEM CPU-mmap
        // path) can still reference a just-freed block and write it after the
        // driver moved on; `frame_alloc` then hands the recycled memory to a
        // fresh coroutine stack and the stale write zeroes a live frame — the SMP
        // `[null-exec]` smash. Holding the block out of circulation for a window
        // closes that race; the poison-trap on eviction names the writer in one
        // run. (See `dma_quarantine_dealloc`.)
        super::dma_quarantine_dealloc(paddr, pages);
        trace!("dealloc DMA: paddr={:#x}, pages={}", paddr, pages);
        0
    }

    /// Remap contiguous DMA pages as uncacheable (UC) in the kernel page tables.
    /// Descriptor rings and NIC DMA buffers on bare metal must not use WB without snooping.
    #[unsafe(no_mangle)]
    extern "C" fn drivers_dma_mark_uncached(paddr: PhysAddr, pages: usize) -> i32 {
        use crate::hal_fn::vm::flush_tlb;
        use crate::vm::{GenericPageTable, PageTable};
        use crate::{CachePolicy, MMUFlags, PAGE_SIZE};

        if paddr == 0 || pages == 0 {
            return -1;
        }
        let vaddr = paddr + KCONFIG.phys_to_virt_offset;
        let flags = MMUFlags::READ
            | MMUFlags::WRITE
            | MMUFlags::from_bits_truncate(CachePolicy::Uncached as usize);
        let mut pt = PageTable::from_current();
        for i in 0..pages {
            let va = vaddr + i * PAGE_SIZE;
            match pt.query(va) {
                Ok((_, _, size)) => {
                    // Refuse to re-flag an entry bigger than the page we were
                    // asked about: `update` writes the flags of WHATEVER entry
                    // covers `va`, so on a huge-mapped region it would turn the
                    // whole 2M/1G window UC — poisoning frames owned by other
                    // subsystems/processes. (The x86_64 physmap is 4K-mapped by
                    // rboot, so this only guards future/other-arch layouts.)
                    if size as usize != PAGE_SIZE {
                        trace!(
                            "drivers_dma_mark_uncached: {:#x} covered by a {:?} entry; refusing",
                            va,
                            size
                        );
                        return -1;
                    }
                    // Never pass a paddr here: `update` would `set_addr` the
                    // entry, and `query` returns the offset-adjusted physical
                    // address — harmlessly redundant for a 4K entry, but a
                    // catastrophic repoint should a huge entry ever slip
                    // through. Flags-only is all this function means.
                    if let Err(e) = pt.update(va, None, Some(flags)) {
                        trace!("drivers_dma_mark_uncached: update {:#x} failed {:?}", va, e);
                        return -1;
                    }
                    // WB -> UC transition: flush any cached lines for this page
                    // (physically tagged, so this alias covers all mappings) so
                    // no stale dirty line can later evict on top of device DMA.
                    #[cfg(target_arch = "x86_64")]
                    unsafe {
                        for line in (0..PAGE_SIZE).step_by(64) {
                            core::arch::x86_64::_mm_clflush((va + line) as *const u8);
                        }
                    }
                }
                Err(_) => {
                    if let Err(e) = pt.map_cont(va, PAGE_SIZE, paddr + i * PAGE_SIZE, flags) {
                        trace!("drivers_dma_mark_uncached: map {:#x} failed {:?}", va, e);
                        return -1;
                    }
                }
            }
        }
        flush_tlb(None);
        core::mem::forget(pt);
        0
    }

    /// Verify contiguous DMA pages are mapped uncacheable (PAT/PCD/PWT set in PTE).
    #[unsafe(no_mangle)]
    extern "C" fn drivers_dma_verify_uncached(paddr: PhysAddr, pages: usize) -> i32 {
        use crate::vm::{GenericPageTable, PageTable};
        use crate::{CachePolicy, PAGE_SIZE};

        if paddr == 0 || pages == 0 {
            return -1;
        }
        let vaddr = paddr + KCONFIG.phys_to_virt_offset;
        let pt = PageTable::from_current();
        for i in 0..pages {
            let va = vaddr + i * PAGE_SIZE;
            let Ok((_, flags, _)) = pt.query(va) else {
                return -1;
            };
            let policy = flags.bits() & 3;
            if policy != CachePolicy::Uncached as usize
                && policy != CachePolicy::UncachedDevice as usize
            {
                return -1;
            }
        }
        core::mem::forget(pt);
        0
    }

    #[unsafe(no_mangle)]
    extern "C" fn drivers_phys_to_virt(paddr: PhysAddr) -> VirtAddr {
        paddr + KCONFIG.phys_to_virt_offset
    }

    #[unsafe(no_mangle)]
    extern "C" fn drivers_virt_to_phys(vaddr: VirtAddr) -> PhysAddr {
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        {
            use crate::vm::{GenericPageTable, PageTable};
            let pt = PageTable::from_current();
            if let Ok((paddr, _, _)) = pt.query(vaddr) {
                return paddr;
            }
        }
        vaddr - KCONFIG.phys_to_virt_offset
    }

    use crate::hal_fn::timer::timer_now;
    #[unsafe(no_mangle)]
    extern "C" fn drivers_timer_now_as_micros() -> u64 {
        timer_now().as_micros() as _
    }

    use crate::hal_fn::interrupt::{intr_get, intr_off, intr_on};
    #[no_mangle]
    extern "C" fn drivers_intr_on() {
        intr_on();
    }
    #[no_mangle]
    extern "C" fn drivers_intr_off() {
        intr_off();
    }
    #[no_mangle]
    extern "C" fn drivers_intr_get() -> bool {
        intr_get()
    }

    #[no_mangle]
    extern "C" fn drivers_klog_emit(priority: u8, msg: *const u8, len: usize) {
        unsafe { super::klog_emit_ffi(priority, msg, len) }
    }

    /// Wake tasks blocked on NIC RX (TCP/UDP recv, poll/epoll).
    #[no_mangle]
    extern "C" fn drivers_wake_net_rx_waiters() {
        crate::net::wake_net_rx_waiters();
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;
    use zcore_drivers::prelude::{ColorFormat, DisplayInfo, FrameBuffer, IrqHandler};
    use zcore_drivers::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane};
    use zcore_drivers::DeviceResult;

    // ── the process-wide bits ────────────────────────────────────────────────

    /// `primary_irq` and `klog_graphics_device_summary` read the ONE device list
    /// the whole process shares, and the dmesg sink below is equally global, so
    /// the handful of tests that touch either run one at a time whatever
    /// `--test-threads` says.
    ///
    /// The body runs inside `catch_unwind` and the panic is raised again after
    /// the guard is dropped: a `spin::Mutex` neither poisons nor unlocks on
    /// unwind, so a failed assertion would otherwise leave the lock held and
    /// turn one red test into a hung suite.
    fn one_at_a_time<R>(f: impl FnOnce() -> R) -> R {
        static GUARD: spin::Mutex<()> = spin::Mutex::new(());
        let held = GUARD.lock();
        let out = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(f));
        drop(held);
        match out {
            Ok(v) => v,
            Err(e) => ::std::panic::resume_unwind(e),
        }
    }

    static SINK: spin::Mutex<Vec<(u8, String)>> = spin::Mutex::new(Vec::new());

    fn sink_emit(priority: u8, msg: &str) {
        SINK.lock().push((priority, String::from(msg)));
    }
    fn sink_read(_dst: &mut [u8]) -> usize {
        0
    }
    fn sink_size() -> usize {
        0
    }

    /// Install the recording dmesg sink (once per process) and start from an
    /// empty transcript.
    ///
    /// `console::klog_emit` judges its slot through `lock::fn_slot::live_fn`
    /// first. Nothing publishes a `.text` window in a hosted build --
    /// `set_kernel_text` is only called from `bare/arch/x86_64` -- so the slot
    /// comes back `Unchecked`, which is allowed, and the sink really is reached.
    fn sink_reset() {
        static INSTALLED: spin::Once<()> = spin::Once::new();
        INSTALLED.call_once(|| crate::console::klog_register(sink_read, sink_size, sink_emit));
        SINK.lock().clear();
    }

    fn sink_lines() -> Vec<(u8, String)> {
        SINK.lock().clone()
    }

    fn line_with(lines: &[(u8, String)], needle: &str) -> Option<String> {
        lines
            .iter()
            .find(|(_, m)| m.contains(needle))
            .map(|(_, m)| m.clone())
    }

    // ── devices of mentira ───────────────────────────────────────────────────

    struct FakeBlock(String);
    impl Scheme for FakeBlock {
        fn name(&self) -> &str {
            &self.0
        }
    }
    impl BlockScheme for FakeBlock {
        fn read_block(&self, _b: usize, _buf: &mut [u8]) -> DeviceResult {
            Ok(())
        }
        fn write_block(&self, _b: usize, _buf: &[u8]) -> DeviceResult {
            Ok(())
        }
        fn flush(&self) -> DeviceResult {
            Ok(())
        }
        fn block_count(&self) -> usize {
            8
        }
    }

    fn block(name: &str) -> Arc<dyn BlockScheme> {
        Arc::new(FakeBlock(String::from(name)))
    }

    struct FakeDisplay(String);
    impl Scheme for FakeDisplay {
        fn name(&self) -> &str {
            &self.0
        }
    }
    impl DisplayScheme for FakeDisplay {
        fn info(&self) -> DisplayInfo {
            DisplayInfo {
                width: 8,
                height: 4,
                pitch: 32,
                format: ColorFormat::ARGB8888,
                fb_base_vaddr: 0,
                fb_size: 128,
            }
        }
        fn fb(&self) -> FrameBuffer<'_> {
            static mut BUF: [u8; 128] = [0; 128];
            unsafe { FrameBuffer::from_raw_parts_mut(core::ptr::addr_of_mut!(BUF) as *mut u8, 128) }
        }
    }

    fn display(name: &str) -> Arc<dyn DisplayScheme> {
        Arc::new(FakeDisplay(String::from(name)))
    }

    struct FakeDrm(String);
    impl Scheme for FakeDrm {
        fn name(&self) -> &str {
            &self.0
        }
    }
    impl DrmScheme for FakeDrm {
        fn get_caps(&self) -> DrmCaps {
            DrmCaps {
                has_3d: false,
                has_cursor: true,
                max_width: 8,
                max_height: 4,
            }
        }
        fn create_fb(&self, _h: u32, _w: u32, _ht: u32, _p: u32) -> Option<u32> {
            None
        }
        fn page_flip(&self, _fb: u32) -> bool {
            false
        }
        fn set_cursor(&self, _c: u32, _x: i32, _y: i32, _h: u32, _f: u32) -> bool {
            false
        }
        fn wait_vblank(&self, _c: u32) -> bool {
            false
        }
        fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
            (Vec::new(), Vec::new(), Vec::new())
        }
        fn get_connector(&self, _id: u32) -> Option<DrmConnector> {
            None
        }
        fn get_crtc(&self, _id: u32) -> Option<DrmCrtc> {
            None
        }
        fn get_plane(&self, _id: u32) -> Option<DrmPlane> {
            None
        }
        fn get_planes(&self) -> Vec<u32> {
            Vec::new()
        }
        #[allow(clippy::too_many_arguments)]
        fn set_plane(
            &self,
            _p: u32,
            _c: u32,
            _fb: u32,
            _x: i32,
            _y: i32,
            _w: u32,
            _h: u32,
            _sx: u32,
            _sy: u32,
            _sw: u32,
            _sh: u32,
        ) -> bool {
            false
        }
    }

    fn drm(name: &str) -> Arc<dyn DrmScheme> {
        Arc::new(FakeDrm(String::from(name)))
    }

    struct FakeIrq(String);
    impl Scheme for FakeIrq {
        fn name(&self) -> &str {
            &self.0
        }
    }
    impl IrqScheme for FakeIrq {
        fn is_valid_irq(&self, _n: usize) -> bool {
            true
        }
        fn mask(&self, _n: usize) -> DeviceResult {
            Ok(())
        }
        fn unmask(&self, _n: usize) -> DeviceResult {
            Ok(())
        }
        fn register_handler(&self, _n: usize, _h: IrqHandler) -> DeviceResult {
            Ok(())
        }
        fn unregister(&self, _n: usize) -> DeviceResult {
            Ok(())
        }
    }

    // ── DeviceList, on lists of its own ─────────────────────────────────────

    #[test]
    fn una_lista_de_dispositivos_recien_hecha_esta_vacia() {
        let list = DeviceList::<dyn BlockScheme>::default();
        assert!(list.as_vec().is_empty());
        assert!(list.first().is_none());
        assert!(list.try_get(0).is_none());
        assert!(list.find("cualquiera").is_none());
    }

    #[test]
    fn first_es_el_primero_que_se_registro_y_no_el_ultimo() {
        let list = DeviceList::<dyn BlockScheme>::default();
        list.add(block("sda"));
        list.add(block("sdb"));
        // `primary_display` and `all_uart().first()` are this call, so which end
        // of the list it takes decides which device the kernel actually drives.
        assert_eq!(list.first().unwrap().name(), "sda");
        assert_eq!(list.try_get(1).unwrap().name(), "sdb");
        assert!(list.try_get(2).is_none());
    }

    #[test]
    fn find_busca_por_nombre_y_no_se_inventa_ninguno() {
        let list = DeviceList::<dyn BlockScheme>::default();
        list.add(block("sda"));
        list.add(block("sdb"));
        assert_eq!(list.find("sdb").unwrap().name(), "sdb");
        assert!(list.find("sdc").is_none());
        // Prefixes are not matches: `find("sd")` must not hand back `sda`.
        assert!(list.find("sd").is_none());
    }

    #[test]
    fn first_unwrap_dice_de_que_rasgo_era_la_lista_vacia() {
        let list = DeviceList::<dyn IrqScheme>::default();
        let err = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
            list.first_unwrap();
        }))
        .unwrap_err();
        let msg = err
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| String::from("?"));
        assert!(msg.contains("device not initialized"), "{}", msg);
        // The trait name is the whole diagnostic value of this panic: it is what
        // tells you WHICH device list was empty when the kernel died.
        assert!(msg.contains("IrqScheme"), "{}", msg);
    }

    #[test]
    fn un_dispositivo_registrado_dos_veces_se_va_entero_al_desregistrarlo() {
        let list = DeviceList::<dyn BlockScheme>::default();
        let disk = block("sda");
        list.add(disk.clone());
        list.add(disk.clone());
        assert_eq!(list.as_vec().len(), 2);
        // Used to drop only the first copy and answer `true` anyway, so the
        // caller believed the device was gone while `first()` still found it.
        assert!(list.remove(&disk));
        assert!(
            list.as_vec().is_empty(),
            "quedo una copia viva: {}",
            list.as_vec().len()
        );
        assert!(list.first().is_none());
    }

    #[test]
    fn desregistrar_un_dispositivo_que_no_esta_contesta_que_no() {
        let list = DeviceList::<dyn BlockScheme>::default();
        list.add(block("sda"));
        let otro = block("sdb");
        assert!(!list.remove(&otro));
        assert_eq!(list.as_vec().len(), 1);
    }

    #[test]
    fn dos_dispositivos_con_el_mismo_nombre_se_distinguen_por_identidad() {
        let list = DeviceList::<dyn BlockScheme>::default();
        let uno = block("sda");
        let otro = block("sda");
        list.add(uno.clone());
        list.add(otro.clone());
        // Same name, different allocation: removal is by `Arc::ptr_eq`, so the
        // one that stays is the one that was not asked for.
        assert!(list.remove(&uno));
        assert_eq!(list.as_vec().len(), 1);
        assert!(Arc::ptr_eq(&list.first().unwrap(), &otro));
    }

    // ── AllDeviceList, and the DrmDisplay pair ──────────────────────────────

    #[test]
    fn cada_dispositivo_va_a_la_lista_de_su_rasgo() {
        let all = AllDeviceList::default();
        all.add_device(Device::Block(block("sda")));
        all.add_device(Device::Display(display("fb0")));
        all.add_device(Device::Drm(drm("card0")));
        assert_eq!(all.block.as_vec().len(), 1);
        assert_eq!(all.display.as_vec().len(), 1);
        assert_eq!(all.drm.as_vec().len(), 1);
        assert!(all.net.as_vec().is_empty());
        assert!(all.uart.as_vec().is_empty());
        assert!(all.irq.as_vec().is_empty());
        assert!(all.audio.as_vec().is_empty());
        assert!(all.input.as_vec().is_empty());
    }

    #[test]
    fn un_par_drm_display_se_registra_en_las_dos_listas() {
        let all = AllDeviceList::default();
        all.add_device(Device::DrmDisplay(drm("card0"), display("fb0")));
        assert_eq!(all.drm.as_vec().len(), 1);
        assert_eq!(all.display.as_vec().len(), 1);
    }

    #[test]
    fn un_par_drm_display_entero_se_va_de_las_dos_listas() {
        let all = AllDeviceList::default();
        let pair = Device::DrmDisplay(drm("card0"), display("fb0"));
        all.add_device(pair.clone());
        assert!(all.remove_device(&pair));
        assert!(all.drm.as_vec().is_empty());
        assert!(all.display.as_vec().is_empty());
    }

    #[test]
    fn un_par_drm_display_a_medias_no_dice_que_se_fue_entero() {
        let all = AllDeviceList::default();
        let card = drm("card0");
        let fb = display("fb0");
        let pair = Device::DrmDisplay(card.clone(), fb.clone());
        // Only the DRM half is registered -- the state the software-KMS
        // emulation is in between its two registrations.
        all.add_device(Device::Drm(card));
        // Used to answer `a || b`: `true`, because one half did go. The caller
        // then stopped worrying about a display that was still registered.
        assert!(!all.remove_device(&pair));
        // What was there is still removed: only the answer changes.
        assert!(all.drm.as_vec().is_empty());
    }

    #[test]
    fn un_par_drm_display_con_solo_la_pantalla_tampoco_se_fue_entero() {
        let all = AllDeviceList::default();
        let card = drm("card0");
        let fb = display("fb0");
        let pair = Device::DrmDisplay(card.clone(), fb.clone());
        // The other way round from the test above, because a wrong answer here
        // is just as wrong: neither half alone is the pair.
        all.add_device(Device::Display(fb));
        assert!(!all.remove_device(&pair));
        assert!(all.display.as_vec().is_empty());
    }

    #[test]
    fn desregistrar_un_par_que_no_esta_contesta_que_no() {
        let all = AllDeviceList::default();
        let pair = Device::DrmDisplay(drm("card0"), display("fb0"));
        assert!(!all.remove_device(&pair));
    }

    #[test]
    fn desregistrar_un_dispositivo_no_toca_las_demas_listas() {
        let all = AllDeviceList::default();
        let disk = block("sda");
        all.add_device(Device::Block(disk.clone()));
        all.add_device(Device::Display(display("fb0")));
        assert!(all.remove_device(&Device::Block(disk)));
        assert!(all.block.as_vec().is_empty());
        assert_eq!(all.display.as_vec().len(), 1);
    }

    // ── primary_irq's one-shot cache ────────────────────────────────────────

    /// The regression test for the cache that poisoned itself.
    ///
    /// There is exactly one of these because `PRIMARY_IRQ` is a process-wide
    /// `spin::Once`: whatever this test leaves it in, it stays in for the rest
    /// of the binary, so a second test could not ask the same question.
    #[test]
    fn primary_irq_no_se_envenena_cuando_todavia_no_hay_controlador() {
        one_at_a_time(|| {
            // Nothing registered yet: the panic comes from `first_unwrap`, on
            // the way in.
            let sin = ::std::panic::catch_unwind(|| primary_irq().name());
            assert!(sin.is_err(), "sin controlador no deberia haber contestado");

            let irq: Arc<dyn IrqScheme> = Arc::new(FakeIrq(String::from("fake-plic")));
            add_device_hosted(Device::Irq(irq.clone()));

            // The bug: that first panic happened INSIDE `call_once`, so the
            // `Once` was poisoned for good and this call panicked with
            // "Once panicked" -- on bare metal, every interrupt for the rest of
            // the boot.
            let con = ::std::panic::catch_unwind(|| primary_irq().name());
            assert_eq!(con.ok(), Some("fake-plic"));

            // And it is a cache: the second answer is the first one, not a
            // fresh lookup.
            assert_eq!(primary_irq().name(), "fake-plic");

            assert!(remove_device_hosted(&Device::Irq(irq)));
            // And it outlives the list it was taken from, which is what makes it
            // a cache and not just a shortcut: the IRQ-dispatch path reads this
            // on every interrupt and must not go back to the `RwLock`.
            assert_eq!(primary_irq().name(), "fake-plic");
        });
    }

    // ── the log line that came in over the C ABI ─────────────────────────────

    fn emit_bytes(priority: u8, bytes: &[u8]) {
        unsafe { klog_emit_ffi(priority, bytes.as_ptr(), bytes.len()) }
    }

    #[test]
    fn una_linea_de_log_legible_se_publica_tal_cual_y_con_su_prioridad() {
        one_at_a_time(|| {
            sink_reset();
            emit_bytes(
                crate::console::LOG_WARNING,
                b"[e1000e] link down, renegotiating\n",
            );
            let lines = sink_lines();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].0, crate::console::LOG_WARNING);
            assert_eq!(lines[0].1, "[e1000e] link down, renegotiating\n");
        });
    }

    #[test]
    fn una_linea_de_log_cortada_a_media_letra_publica_lo_legible() {
        one_at_a_time(|| {
            sink_reset();
            // Exactly what `zcore_drivers::bus::klog` produces for a line that
            // does not fit its 256-byte buffer: the cut lands inside the em
            // dash, and there is no way to put those three bytes back.
            let mut bytes = Vec::from(&b"[e1000e] MAC all-zero/FF after reset "[..]);
            bytes.extend_from_slice(&"\u{2014}".as_bytes()[..2]);
            assert!(core::str::from_utf8(&bytes).is_err());

            emit_bytes(crate::console::LOG_WARNING, &bytes);
            let lines = sink_lines();
            // Used to publish nothing at all: the whole line was thrown away
            // for the sake of its last two bytes.
            assert_eq!(lines.len(), 1, "no publico nada");
            assert_eq!(lines[0].1, "[e1000e] MAC all-zero/FF after reset ");
        });
    }

    #[test]
    fn una_linea_de_log_ilegible_desde_el_primer_byte_deja_constancia() {
        one_at_a_time(|| {
            sink_reset();
            emit_bytes(crate::console::LOG_ERR, &[0xff, 0xfe, 0xfd]);
            let lines = sink_lines();
            // A driver log that goes silent looks exactly like a driver that
            // stopped logging, so say a line was lost rather than lose it.
            assert_eq!(lines.len(), 1);
            assert!(lines[0].1.contains("not UTF-8"), "{}", lines[0].1);
            assert_eq!(lines[0].0, crate::console::LOG_ERR);
        });
    }

    #[test]
    fn una_linea_de_log_vacia_o_sin_puntero_no_publica_nada() {
        one_at_a_time(|| {
            sink_reset();
            emit_bytes(crate::console::LOG_INFO, b"");
            unsafe { klog_emit_ffi(crate::console::LOG_INFO, core::ptr::null(), 32) };
            assert!(sink_lines().is_empty());
        });
    }

    // ── the graphics summary Moebius reads at boot ──────────────────────────

    #[test]
    fn el_resumen_de_graficos_sin_dispositivos_lo_dice() {
        one_at_a_time(|| {
            sink_reset();
            klog_graphics_device_summary(None);
            let lines = sink_lines();
            assert!(
                line_with(&lines, "no framebuffer (Display) or DRM devices registered").is_some(),
                "{:?}",
                lines
            );
        });
    }

    #[test]
    fn el_resumen_de_graficos_nombra_cada_pantalla_con_su_modo() {
        one_at_a_time(|| {
            sink_reset();
            let fb = display("resumen-fb");
            add_device_hosted(Device::Display(fb.clone()));
            klog_graphics_device_summary(None);
            let lines = sink_lines();
            assert!(remove_device_hosted(&Device::Display(fb)));

            let cuenta = line_with(&lines, "framebuffer device(s)").expect("falta la cuenta");
            assert!(cuenta.contains("1 framebuffer device(s)"), "{}", cuenta);
            assert!(cuenta.contains("0 DRM / GPU device(s)"), "{}", cuenta);

            let linea = line_with(&lines, "resumen-fb").expect("falta la pantalla");
            assert!(linea.contains("display[0]"), "{}", linea);
            assert!(linea.contains("8x4"), "{}", linea);
            assert!(linea.contains("pitch=32"), "{}", linea);
        });
    }

    #[test]
    fn el_resumen_de_graficos_nombra_cada_gpu_con_sus_capacidades() {
        one_at_a_time(|| {
            sink_reset();
            let card = drm("resumen-gpu");
            add_device_hosted(Device::Drm(card.clone()));
            klog_graphics_device_summary(None);
            let lines = sink_lines();
            assert!(remove_device_hosted(&Device::Drm(card)));

            let cuenta = line_with(&lines, "DRM / GPU device(s)").expect("falta la cuenta");
            assert!(cuenta.contains("1 DRM / GPU device(s)"), "{}", cuenta);

            let linea = line_with(&lines, "resumen-gpu").expect("falta la gpu");
            assert!(linea.contains("drm[0]"), "{}", linea);
            assert!(linea.contains("max_mode=8x4"), "{}", linea);
            assert!(linea.contains("cursor=true"), "{}", linea);
            assert!(linea.contains("3d=false"), "{}", linea);
        });
    }

    #[test]
    fn el_resumen_de_graficos_dice_cual_es_la_consola_activa() {
        one_at_a_time(|| {
            sink_reset();
            klog_graphics_device_summary(Some("nvidia-drm scanout"));
            let lines = sink_lines();
            let linea = line_with(&lines, "active framebuffer console").expect("falta la consola");
            assert!(linea.contains("nvidia-drm scanout"), "{}", linea);
        });
    }

    #[test]
    fn el_resumen_de_graficos_con_una_consola_sin_nombre_no_se_la_inventa() {
        one_at_a_time(|| {
            sink_reset();
            // An empty note is not a name: it must read the same as `None`.
            klog_graphics_device_summary(Some(""));
            let vacia = sink_lines();
            sink_reset();
            klog_graphics_device_summary(None);
            let ninguna = sink_lines();
            assert_eq!(vacia, ninguna);
        });
    }

    // ── what a DeviceError becomes ──────────────────────────────────────────

    #[test]
    fn cualquier_error_de_dispositivo_se_vuelve_el_mismo_halerror() {
        // `HalError` is a unit struct, so the conversion is lossy by
        // construction: which of these it was survives only in the `warn!`.
        // Pinned here so that stops being a surprise to whoever adds the first
        // real variant.
        for e in [
            DeviceError::BufferTooSmall,
            DeviceError::NotReady,
            DeviceError::InvalidParam,
            DeviceError::DmaError,
            DeviceError::IoError,
            DeviceError::AlreadyExists,
            DeviceError::NoResources,
            DeviceError::NotSupported,
        ] {
            let hal: crate::HalError = e.into();
            assert_eq!(::std::format!("{:?}", hal), "HalError");
        }
    }
}
