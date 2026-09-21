//! An emulated DRM/KMS output, so the present path can be tested with no GPU.
//!
//! Everything from `CREATE_DUMB` to the completion event is bookkeeping and was
//! already reachable from a host test. The copy in the middle was not: the
//! software-KMS scanout blits into `primary_display()`, which is
//! `kernel_hal::drivers::all_display().first()`, and a unit-test binary never
//! runs the hosted kernel's device bring-up. With nothing registered,
//! `software_kms_active()` is false, the synthetic CRTC/connector/plane do not
//! exist, `GETRESOURCES` reports no KMS at all, and every present stops at
//! `PresentError::NoDisplay` before touching a pixel. So the half of the path
//! that decides *which* pixels reach the screen -- the clip against the mode,
//! the damage rectangle, the write-combining line expansion, the cursor
//! composite -- had no test on any machine without a GPU, which is every
//! machine in CI.
//!
//! This registers one [`DisplayScheme`] over a heap buffer and reprograms it per
//! test, which is as close to a real output as the software-KMS path can tell:
//! it goes through `DisplayScheme::blit_from` exactly as a UEFI GOP or a GPU
//! BAR1 aperture does, honours a padded pitch, and can claim to be
//! write-combining so the `MOVNTDQ` store path runs for real.
//!
//! The buffer is filled with [`UNTOUCHED`] before each test, so "the present
//! wrote this" and "the present left this alone" are distinguishable -- which
//! is the whole point when the bug under test is writing too few columns or too
//! many.

extern crate std;

use alloc::{sync::Arc, vec::Vec};

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use kernel_hal::drivers::prelude::{ColorFormat, DisplayInfo, FrameBuffer};
use kernel_hal::drivers::scheme::{DisplayScheme, Scheme};
use spin::{Mutex, Once};
use zcore_drivers::Device;

/// Filler for every pixel of the emulated scanout before a test runs. Chosen to
/// be a value no test writes on purpose, so an assertion can say "nothing
/// touched this" instead of only "this is zero" -- a freshly committed buffer is
/// zero too, which made the two indistinguishable.
pub(crate) const UNTOUCHED: u32 = 0x5A5A_5A5A;

/// Largest scanout the emulation backs. 8 MiB is a 1024-pixel pitch at 2048
/// rows, or any realistic test mode with room for padding; allocated once and
/// never freed, because the registered display hands out `&mut [u8]` over it for
/// as long as the process lives.
const MAX_BYTES: usize = 8 * 1024 * 1024;

struct Config {
    width: u32,
    height: u32,
    /// In BYTES, like `DisplayInfo::pitch`.
    pitch: u32,
    wc: bool,
}

static CONFIG: Mutex<Config> = Mutex::new(Config {
    width: 0,
    height: 0,
    pitch: 0,
    wc: false,
});

/// The scanout bytes. 64-byte aligned because that is one write-combining line
/// and one PCIe burst: an unaligned base would make every row's expansion land
/// off by a few pixels and quietly weaken what the WC tests below check.
fn fb_ptr() -> *mut u8 {
    static BUF: Once<usize> = Once::new();
    *BUF.call_once(|| {
        let layout = core::alloc::Layout::from_size_align(MAX_BYTES, 64).unwrap();
        // SAFETY: non-zero size, valid alignment. Leaked on purpose (see above).
        let p = unsafe { alloc::alloc::alloc_zeroed(layout) };
        assert!(!p.is_null(), "kms_emu: could not allocate the scanout");
        p as usize
    }) as *mut u8
}

fn fb_bytes() -> usize {
    let c = CONFIG.lock();
    (c.pitch as usize).saturating_mul(c.height as usize)
}

/// The emulated output itself. Stateless: its geometry lives in [`CONFIG`], so
/// one registered instance can be reprogrammed by each test instead of every
/// test leaking another display into a process-wide, append-only device list.
struct EmuDisplay;

impl Scheme for EmuDisplay {
    fn name(&self) -> &str {
        "kms-emu"
    }
}

impl DisplayScheme for EmuDisplay {
    fn info(&self) -> DisplayInfo {
        let c = CONFIG.lock();
        DisplayInfo {
            width: c.width,
            height: c.height,
            pitch: c.pitch,
            format: ColorFormat::ARGB8888,
            fb_base_vaddr: fb_ptr() as usize,
            fb_size: (c.pitch as usize).saturating_mul(c.height as usize),
        }
    }

    fn fb(&self) -> FrameBuffer<'_> {
        // SAFETY: `fb_ptr()` owns `MAX_BYTES`, and `attach` refuses a geometry
        // that needs more than that, so `fb_bytes()` is always within it.
        unsafe { FrameBuffer::from_raw_parts_mut(fb_ptr(), fb_bytes()) }
    }

    fn fb_write_combining(&self) -> bool {
        CONFIG.lock().wc
    }
}

/// An attached emulated output, for the duration of one test.
///
/// Holds the DRM test lock as well, because a registered display changes what
/// `software_kms_active()` answers for the whole process: a neighbouring test
/// that asserts there is no KMS card must not observe this one. Dropping it
/// unregisters the display again, including while a panic unwinds.
pub(crate) struct Screen {
    _serialised: std::sync::MutexGuard<'static, ()>,
    /// `None` for a [`headless`] guard, which takes the lock without attaching
    /// an output at all.
    dev: Option<Device>,
    height: u32,
    pitch_px: u32,
}

impl Drop for Screen {
    fn drop(&mut self) {
        // Detach first, THEN reset: unblanking with a display still registered
        // would clear it, and the reset has to happen with nothing to clear.
        if let Some(dev) = self.dev.take() {
            kernel_hal::drivers::remove_device_hosted(&dev);
        }
        super::drm::reset_output_state_for_test();
    }
}

/// Attach an output whose scanline is exactly as wide as the mode.
pub(crate) fn attach(width: u32, height: u32) -> Screen {
    attach_with(width, height, width, false)
}

/// Attach an output of `width` x `height` whose scanlines are `pitch_px` pixels
/// apart, claiming to be write-combining or not.
///
/// A padded pitch is the normal case on real hardware, not an exotic one: UEFI
/// reports `PixelsPerScanLine` (2048 for a 1920-wide mode), and the padding is
/// precisely where the write-combining expansion is allowed to write. `wc` picks
/// which store path `blit_from` takes, and on x86_64 the write-combining one is
/// the non-temporal store loop for real.
pub(crate) fn attach_with(width: u32, height: u32, pitch_px: u32, wc: bool) -> Screen {
    assert!(
        pitch_px >= width,
        "a scanline cannot be shorter than the mode"
    );
    let pitch = pitch_px.checked_mul(4).expect("pitch overflow");
    let bytes = (pitch as usize)
        .checked_mul(height as usize)
        .expect("scanout overflow");
    assert!(
        bytes <= MAX_BYTES,
        "kms_emu backs at most {} bytes, asked for {}",
        MAX_BYTES,
        bytes
    );

    let serialised = super::drm::test_globals::lock();
    let dev = Device::Display(Arc::new(EmuDisplay));
    {
        let mut c = CONFIG.lock();
        c.width = width;
        c.height = height;
        c.pitch = pitch;
        c.wc = wc;
    }
    // Fill AFTER the geometry is set (`fb_bytes` reads it) and BEFORE the
    // display is visible to the present path.
    fill(UNTOUCHED, bytes);
    kernel_hal::drivers::add_device_hosted(dev.clone());

    Screen {
        _serialised: serialised,
        dev: Some(dev),
        height,
        pitch_px,
    }
}

/// Take the DRM test lock with NO output attached.
///
/// The configuration where a DRM driver is the only graphics device: no boot
/// framebuffer, so `software_kms_active()` is false whatever the driver claims,
/// and the core has to fall through to the driver for everything. A VirtIO-only
/// guest with no framebuffer display is exactly this, and it is the only place
/// some of the per-driver guards are load-bearing rather than redundant.
pub(crate) fn headless() -> Screen {
    Screen {
        _serialised: super::drm::test_globals::lock(),
        dev: None,
        height: 0,
        pitch_px: 0,
    }
}

fn fill(value: u32, bytes: usize) {
    let words = bytes / 4;
    // SAFETY: `bytes` was checked against `MAX_BYTES` by the caller.
    let buf = unsafe { core::slice::from_raw_parts_mut(fb_ptr() as *mut u32, words) };
    for w in buf.iter_mut() {
        *w = value;
    }
}

impl Screen {
    /// Scanline stride in pixels -- the mode's width plus any off-screen
    /// padding.
    pub(crate) fn pitch_px(&self) -> u32 {
        self.pitch_px
    }

    /// Put every pixel, padding included, back to `value`.
    pub(crate) fn repaint(&self, value: u32) {
        fill(value, (self.pitch_px as usize) * 4 * (self.height as usize));
    }

    /// One pixel of the scanout. `x` may address the off-screen padding, which
    /// is exactly what the write-combining tests need to look at.
    pub(crate) fn pixel(&self, x: u32, y: u32) -> u32 {
        assert!(x < self.pitch_px && y < self.height, "off the scanout");
        let off = (y as usize) * (self.pitch_px as usize) + x as usize;
        // SAFETY: bounds checked above against the geometry `attach_with`
        // validated against `MAX_BYTES`.
        unsafe { core::ptr::read((fb_ptr() as *const u32).add(off)) }
    }
}

/// ---------------------------------------------------------------------------
/// The hardware-KMS half: an emulated GPU that owns scanout itself.
/// ---------------------------------------------------------------------------
///
/// A display alone puts the DRM core on the SOFTWARE KMS path: it blits dumb
/// buffers into the framebuffer itself. That is what runs under QEMU and what
/// [`Screen`] above exercises. On Moebius's machine it is not the only path:
/// with `nvidia.hwflip` (or NVC57E surface flip) the NVIDIA driver declares
/// `has_hardware_kms()` and the core hands it the frame instead, which changes
/// four things at once -- ADDFB2 asks the driver to make its OWN framebuffer
/// object, the flip goes to the driver by that private id, the software blit is
/// skipped entirely, and the pointer has to be recomposited on top afterwards
/// because the driver's flip replaced the whole scanout.
///
/// None of that ran in CI either, and it cannot: it needs a driver that claims
/// hardware KMS, and the only one is `NvidiaGpu` behind MMIO. So this is one,
/// recording what it was asked to do and answering as a real driver would --
/// including refusing a flip, which is the case the fallback exists for.
use kernel_hal::drivers::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane};
use kernel_hal::drivers::scheme::DrmScheme;

/// First framebuffer id the emulated GPU hands out. Deliberately nowhere near
/// the DRM core's ids (which start at 1): the core and the driver keep separate
/// framebuffer namespaces, and every test that watches a flip checks the driver
/// was given ITS OWN id. Mixing the two is invisible while both count from 1.
pub(crate) const EMU_DRIVER_FB_BASE: u32 = 0x9000;

/// One `create_fb` the emulated GPU served: the core's GEM handle and the
/// geometry it was given, plus the private id it answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CreatedFb {
    pub(crate) gem_handle: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pitch: u32,
    pub(crate) driver_fb_id: u32,
}

#[derive(Default)]
struct GpuCalls {
    created_fbs: Vec<CreatedFb>,
    /// Framebuffer ids `page_flip` was called with, in order.
    flips: Vec<u32>,
    vblank_waits: u32,
}

/// An emulated GPU driver. Build one with [`EmuGpu::new`], set what it claims,
/// and attach it with [`Screen::attach_gpu`].
pub(crate) struct EmuGpu {
    name: &'static str,
    hardware_kms: bool,
    crtcs: Vec<u32>,
    connectors: Vec<u32>,
    planes: Vec<u32>,
    /// Whether `page_flip` succeeds. A driver that refuses is the whole reason
    /// the software fallback exists, so it has to be reachable.
    accepts_flips: AtomicBool,
    next_fb: AtomicU32,
    calls: Mutex<GpuCalls>,
}

impl EmuGpu {
    /// A GPU that does NOT own scanout -- a VirtIO-like driver, which is what
    /// the mixed-topology filter is about.
    pub(crate) fn new(name: &'static str) -> EmuGpu {
        EmuGpu {
            name,
            hardware_kms: false,
            crtcs: alloc::vec![40],
            connectors: alloc::vec![41],
            planes: alloc::vec![42],
            accepts_flips: AtomicBool::new(true),
            next_fb: AtomicU32::new(EMU_DRIVER_FB_BASE),
            calls: Mutex::new(GpuCalls::default()),
        }
    }

    /// A GPU that declares `has_hardware_kms()`, like the NVIDIA driver with
    /// `nvidia.hwflip`.
    pub(crate) fn hardware_kms(name: &'static str) -> EmuGpu {
        EmuGpu {
            hardware_kms: true,
            ..EmuGpu::new(name)
        }
    }

    /// The CRTC, connector and plane ids this GPU reports. Two GPUs given the
    /// same ids is not a contrived case: two cards of the same model really do
    /// return the same synthetic ids, and that is what the de-duplication in
    /// `get_resources` is for.
    pub(crate) fn with_ids(mut self, crtc: u32, connector: u32, plane: u32) -> EmuGpu {
        self.crtcs = alloc::vec![crtc];
        self.connectors = alloc::vec![connector];
        self.planes = alloc::vec![plane];
        self
    }
}

impl Scheme for EmuGpu {
    fn name(&self) -> &str {
        self.name
    }
}

impl DrmScheme for EmuGpu {
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: false,
            has_cursor: true,
            max_width: 4096,
            max_height: 4096,
        }
    }

    fn has_hardware_kms(&self) -> bool {
        self.hardware_kms
    }

    fn create_fb(&self, handle_id: u32, width: u32, height: u32, pitch: u32) -> Option<u32> {
        let driver_fb_id = self.next_fb.fetch_add(1, Ordering::Relaxed);
        self.calls.lock().created_fbs.push(CreatedFb {
            gem_handle: handle_id,
            width,
            height,
            pitch,
            driver_fb_id,
        });
        Some(driver_fb_id)
    }

    fn page_flip(&self, fb_id: u32) -> bool {
        self.calls.lock().flips.push(fb_id);
        self.accepts_flips.load(Ordering::Relaxed)
    }

    /// No hardware cursor plane, which is the default on this tree: the
    /// `nvidia.hwcursor` flag is off, so the pointer stays software-composited.
    fn set_cursor(&self, _crtc_id: u32, _x: i32, _y: i32, _handle: u32, _flags: u32) -> bool {
        false
    }

    fn wait_vblank(&self, _crtc_id: u32) -> bool {
        self.calls.lock().vblank_waits += 1;
        true
    }

    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (Vec::new(), self.crtcs.clone(), self.connectors.clone())
    }

    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        self.connectors.contains(&id).then_some(DrmConnector {
            id,
            connected: true,
            mm_width: 530,
            mm_height: 300,
            connector_type: 11,
        })
    }

    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        self.crtcs.contains(&id).then_some(DrmCrtc {
            id,
            // The driver's own idea of what it is scanning out, in its own
            // framebuffer namespace. The core must not pass this to userspace.
            fb_id: EMU_DRIVER_FB_BASE,
            x: 0,
            y: 0,
        })
    }

    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        self.planes.contains(&id).then_some(DrmPlane {
            id,
            crtc_id: self.crtcs[0],
            fb_id: EMU_DRIVER_FB_BASE,
            possible_crtcs: 1,
            plane_type: 1,
        })
    }

    fn get_planes(&self) -> Vec<u32> {
        self.planes.clone()
    }

    #[allow(clippy::too_many_arguments)]
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
        false
    }
}

/// An attached emulated GPU, for the duration of one test. Dropping it takes the
/// driver back out of both registries, including while a panic unwinds -- a
/// driver left behind would put every later test in the process on the hardware
/// path (see `drm::unregister_driver`).
pub(crate) struct Gpu {
    dev: Device,
    driver: Arc<dyn DrmScheme>,
    gpu: Arc<EmuGpu>,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        kernel_hal::drivers::remove_device_hosted(&self.dev);
        super::drm::unregister_driver(&self.driver);
    }
}

impl Gpu {
    /// The framebuffer ids the driver was asked to flip, in order. These are the
    /// DRIVER's ids, not the DRM core's.
    pub(crate) fn flips(&self) -> Vec<u32> {
        self.gpu.calls.lock().flips.clone()
    }

    /// Every `create_fb` the driver served.
    pub(crate) fn created_fbs(&self) -> Vec<CreatedFb> {
        self.gpu.calls.lock().created_fbs.clone()
    }

    /// How many times the driver's `wait_vblank` was called.
    pub(crate) fn vblank_waits(&self) -> u32 {
        self.gpu.calls.lock().vblank_waits
    }

    /// Make the driver refuse every following flip, as a real one does when the
    /// display engine will not take the surface. The core then has to fall back
    /// to the software blit rather than leave the panel dark.
    pub(crate) fn refuse_flips(&self) {
        self.gpu.accepts_flips.store(false, Ordering::Relaxed);
    }
}

impl Screen {
    /// Register `gpu` as a DRM driver for as long as the returned guard lives.
    ///
    /// Taking `&self` is the point: an attached [`Screen`] is already holding
    /// the DRM test lock, and a driver registered without it would change what
    /// `software_kms_active()` answers under a test running in another thread.
    /// It is also the honest configuration -- a hardware-KMS GPU on this tree
    /// still has the boot framebuffer beside it, and that is where the pointer
    /// is composited after a driver flip.
    pub(crate) fn attach_gpu(&self, gpu: EmuGpu) -> Gpu {
        let gpu = Arc::new(gpu);
        let driver: Arc<dyn DrmScheme> = gpu.clone();
        super::drm::register_driver(driver.clone());
        let dev = Device::Drm(driver.clone());
        kernel_hal::drivers::add_device_hosted(dev.clone());
        Gpu { dev, driver, gpu }
    }
}
