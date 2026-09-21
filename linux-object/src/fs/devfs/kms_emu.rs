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

use alloc::sync::Arc;

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
    dev: Device,
    height: u32,
    pitch_px: u32,
}

impl Drop for Screen {
    fn drop(&mut self) {
        // Detach first, THEN reset: unblanking with a display still registered
        // would clear it, and the reset has to happen with nothing to clear.
        kernel_hal::drivers::remove_device_hosted(&self.dev);
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
        dev,
        height,
        pitch_px,
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
