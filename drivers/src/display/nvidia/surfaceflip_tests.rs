use super::super::nouveau_uapi as nv;
use super::rm_host_shims::{reset_fake_hwflip, FAKE_HWFLIP};
use super::*;
use crate::nvme::nvme_queue::test_clock;
use crate::scheme::drm::DrmScheme;

/// The fixture below writes process-wide state (the boot framebuffer,
/// the opt-in flag, the ladder latch and its counters), so the tests
/// take turns.
pub(super) static SERIAL: lock::Mutex<()> = lock::Mutex::new(());

pub(super) const FB: u32 = 7;
pub(super) const H_MEMORY: u32 = 0x1234;

/// A GPU that drives the panel (the boot framebuffer lies in its BAR1
/// window), RM up, `nvidia.surfaceflip` opted in, with one VRAM-backed
/// KMS framebuffer registered -- the shape of the compositor's output
/// buffer under NVK.
/// `pub(super)` so the bench module beside this one measures the present
/// path on the same fixture these tests assert on -- a second one could
/// drift and end up pricing a different GPU.
pub(super) fn console_gpu() -> NvidiaGpu {
    reset_fake_hwflip();
    SURFACEFLIP_STATE.store(0, Ordering::Release);
    for c in [
        &SURFACEFLIP_FLIPS,
        &SURFACEFLIP_DRAIN_WAITS,
        &SURFACEFLIP_DRAIN_US,
        &SURFACEFLIP_DRAIN_MAX_US,
        &SURFACEFLIP_SUBMIT_MAX_US,
        &SURFACEFLIP_STUCK,
        &SURFACEFLIP_BUSY_REFUSED,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    test_clock::set(1_000_000);
    test_clock::set_auto_advance(0);
    nv::set_enabled(true);
    nv::set_surfaceflip_enabled(true);
    set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
    let gpu = NvidiaGpu::for_test(0x1f06, 8192);
    assert!(gpu.drives_boot_display());
    *gpu.rm_device_instance.lock() = Some(0);
    gpu.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
        id: FB,
        handle_id: 1,
        width: 1920,
        height: 1080,
        pitch: 1920 * 4,
        phys_addr: 0,
        size: 0,
        h_memory: H_MEMORY,
        vram_offset: Some(0),
    });
    gpu
}

fn polls() -> u64 {
    FAKE_HWFLIP.lock().polls
}

fn surfaces() -> usize {
    FAKE_HWFLIP.lock().surfaces.len()
}

/// The flip is a submission: the ioctl is over once the methods are
/// kicked, however long the panel then takes to fetch them.
#[test]
fn a_flip_returns_as_soon_as_it_is_kicked_and_does_not_wait_for_the_panel() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    // A panel that takes a thousand polls to fetch a flip.
    FAKE_HWFLIP.lock().fetch_polls = 1000;
    assert!(
        !gpu.has_hardware_kms(),
        "the ladder comes up on the first flip"
    );
    assert!(gpu.page_flip(FB));
    let f = FAKE_HWFLIP.lock();
    assert_eq!(f.init_calls, 1);
    assert_eq!(f.surfaces, [(H_MEMORY, 0, 1920, 1080, 1920 * 4)]);
    assert_eq!(
        f.polls, 1,
        "one look at the front end before writing, none after the kick"
    );
    assert_eq!(
        f.pending_polls, 1000,
        "the panel is still fetching, and nobody waited"
    );
    drop(f);
    assert!(gpu.has_hardware_kms());
    assert_eq!(SURFACEFLIP_FLIPS.load(Ordering::Relaxed), 1);
    assert_eq!(SURFACEFLIP_DRAIN_WAITS.load(Ordering::Relaxed), 0);
    let kms = gpu.kms_state.lock();
    assert_eq!((kms.crtc_fb, kms.plane_fb), (FB, FB));
}

/// A compositor rotates a swapchain, so the buffer handed to consecutive
/// flips alternates. Each distinct buffer needs an ISO context DMA, and
/// that object is what a flip must NOT be paying for: the display front
/// end may still be scanning out the buffer whose ctxdma a rebuild would
/// free, and an RM object free plus alloc sits inside the RM API lock and
/// the GPU locks, on the compositor's critical path.
///
/// The count the flip path reports is therefore the number of distinct
/// buffers, not the number of flips. This pins the number down to what
/// `/proc/gpudbg` says, which is the only place either of us can read it
/// on a machine with a display; the table itself lives in
/// `eclipse_rm_hwflip_surface`, which the fake stands in for here.
#[test]
fn rotating_a_swapchain_builds_one_ctxdma_per_buffer_not_one_per_flip() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    const FB_B: u32 = FB + 1;
    const H_MEMORY_B: u32 = H_MEMORY + 1;
    gpu.kms_framebuffers.lock().push(NvidiaKmsFramebuffer {
        id: FB_B,
        handle_id: 2,
        width: 1920,
        height: 1080,
        pitch: 1920 * 4,
        phys_addr: 0,
        size: 0,
        h_memory: H_MEMORY_B,
        vram_offset: Some(0),
    });
    for fb in [FB, FB_B, FB, FB_B, FB, FB_B] {
        assert!(gpu.page_flip(fb));
    }
    assert_eq!(surfaces(), 6, "six frames went out");
    assert_eq!(
        FAKE_HWFLIP.lock().iso_mems,
        [H_MEMORY, H_MEMORY_B],
        "two buffers, whatever the frame count"
    );
    let line = surfaceflip_stats_line();
    assert!(
        line.contains("iso-ctxdma-builds=2"),
        "the count has to be readable on a real machine: {}",
        line
    );
    assert!(line.contains("flips=6"), "{}", line);
}

/// The next flip is the one that waits, and only until the front end has
/// fetched the previous one -- then it writes.
#[test]
fn the_next_flip_waits_exactly_until_the_previous_one_has_been_fetched() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    FAKE_HWFLIP.lock().fetch_polls = 5;
    test_clock::set_auto_advance(10);
    assert!(gpu.page_flip(FB));
    assert_eq!(polls(), 1);
    assert!(gpu.page_flip(FB));
    assert_eq!(surfaces(), 2, "the second flip went through");
    assert_eq!(
        polls(),
        1 + 5 + 1,
        "five pending answers, then the one that read Get == Put"
    );
    assert_eq!(
        FAKE_HWFLIP.lock().refused_busy,
        0,
        "it never wrote into a busy ring"
    );
    assert_eq!(SURFACEFLIP_DRAIN_WAITS.load(Ordering::Relaxed), 1);
    let max = SURFACEFLIP_DRAIN_MAX_US.load(Ordering::Relaxed);
    assert!((40..=80).contains(&max), "five 10 us polls, got {} us", max);
    assert_eq!(SURFACEFLIP_DRAIN_US.load(Ordering::Relaxed), max);
}

/// A front end that stopped fetching costs one frame, not the boot: the
/// drain gives up at its bound, the flip is refused so the present falls
/// back, and nothing is written into the ring behind the stuck UPDATE.
#[test]
fn a_panel_that_never_fetches_loses_the_frame_at_the_bound_and_no_later() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    // Bring the ladder up with one clean flip first.
    assert!(gpu.page_flip(FB));
    FAKE_HWFLIP.lock().pending_forever = true;
    // Every look at the clock moves it 100 us.
    test_clock::set_auto_advance(100);
    let t0 = test_clock::now();
    assert!(!gpu.page_flip(FB), "the frame is dropped");
    test_clock::set_auto_advance(0);
    let waited = test_clock::now() - t0;
    assert!(
        (SURFACEFLIP_DRAIN_TIMEOUT_US..SURFACEFLIP_DRAIN_TIMEOUT_US + 1_000).contains(&waited),
        "gave up after {} us, bound is {}",
        waited,
        SURFACEFLIP_DRAIN_TIMEOUT_US
    );
    let f = FAKE_HWFLIP.lock();
    assert!(!f.overpolled, "the drain never gave up");
    assert_eq!(
        f.surfaces.len(),
        1,
        "nothing was written behind the stuck UPDATE"
    );
    assert_eq!(f.refused_busy, 0, "the driver did not even ask");
    drop(f);
    assert_eq!(SURFACEFLIP_STUCK.load(Ordering::Relaxed), 1);
    assert_eq!(SURFACEFLIP_FLIPS.load(Ordering::Relaxed), 1);
}

/// The C side's own refusal (something else kicked the core between the
/// drain and the write) is a dropped frame too, counted apart, and the
/// ladder stays up for the next one.
#[test]
fn a_busy_refusal_from_the_rm_drops_the_frame_and_keeps_the_ladder() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    assert!(gpu.page_flip(FB));
    FAKE_HWFLIP.lock().refuse_busy_once = true;
    assert!(!gpu.page_flip(FB));
    assert_eq!(SURFACEFLIP_BUSY_REFUSED.load(Ordering::Relaxed), 1);
    assert_eq!(SURFACEFLIP_STUCK.load(Ordering::Relaxed), 0);
    assert!(gpu.page_flip(FB), "the next frame flips again");
    assert_eq!(surfaces(), 2);
    assert_eq!(SURFACEFLIP_FLIPS.load(Ordering::Relaxed), 2);
    assert!(gpu.has_hardware_kms());
}

/// The ladder is built once; a failed bring-up is latched for the boot
/// and every later flip goes straight to the fallback.
#[test]
fn the_ladder_is_built_once_and_a_failed_build_is_latched() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    FAKE_HWFLIP.lock().init_ok = false;
    assert!(!gpu.page_flip(FB));
    assert!(!gpu.page_flip(FB));
    assert_eq!(FAKE_HWFLIP.lock().init_calls, 1, "not retried");
    assert!(!gpu.has_hardware_kms());
    assert_eq!(surfaces(), 0);
    assert_eq!(SURFACEFLIP_STATE.load(Ordering::Relaxed), 2);
    assert!(surfaceflip_stats_line().contains("state=failed"));
}

/// `WAIT_VBLANK` used to busy-spin a whole frame here on top of the sleep
/// the syscall path had already taken. The synthetic vblank counter is
/// the pacing; the driver has nothing to add.
#[test]
fn wait_vblank_does_not_spin_a_frame_on_the_calling_cpu() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    assert!(gpu.page_flip(FB), "a flip stamps last_vblank_us");
    // Each clock read is a microsecond, so a 16.7 ms spin is measurable
    // and finite either way.
    test_clock::set_auto_advance(1);
    let t0 = test_clock::now();
    assert!(gpu.wait_vblank(0));
    test_clock::set_auto_advance(0);
    let spent = test_clock::now() - t0;
    assert!(spent < 1_000, "wait_vblank spun {} us", spent);
}

/// The counters land in `/proc/gpudbg` with the numbers a boot leaves.
#[test]
fn the_flip_counters_are_reported() {
    let _g = SERIAL.lock();
    let gpu = console_gpu();
    FAKE_HWFLIP.lock().fetch_polls = 2;
    assert!(gpu.page_flip(FB));
    assert!(gpu.page_flip(FB));
    let line = surfaceflip_stats_line();
    assert!(line.contains("state=ready"), "{}", line);
    assert!(
        line.contains("flips=2 iso-ctxdma-builds=1 drain-waits=1"),
        "{}",
        line
    );
    assert!(line.contains("stuck-dropped=0 busy-refused=0"), "{}", line);
}
