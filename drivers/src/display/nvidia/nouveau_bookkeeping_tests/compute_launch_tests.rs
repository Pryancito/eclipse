//! The compute ioctl (`eclipse-compute`) on the test GPU. A child of
//! `nouveau_bookkeeping_tests` for the harness; a file of its own because the
//! parent is at its size ceiling (`xtask tamano`).

use super::*;

/// The boot framebuffer as a test found it, put back on drop. `console`
/// makes the test GPU the one that drives it.
struct BootFb(Option<BootFbInfo>);

impl BootFb {
    fn none() -> Self {
        Self(BOOT_FB_INFO.lock().take())
    }

    fn console() -> Self {
        let saved = Self(BOOT_FB_INFO.lock().take());
        set_boot_fb_info(0x1000, 1920, 1080, 1920 * 4);
        saved
    }
}

impl Drop for BootFb {
    fn drop(&mut self) {
        *BOOT_FB_INFO.lock() = self.0;
    }
}

/// The `exec_fast` switch as a test found it, put back on drop.
struct ExecFast(bool);

impl ExecFast {
    fn pinned(v: bool) -> Self {
        let saved = Self(nv::exec_fast_enabled());
        nv::set_exec_fast_enabled(v);
        saved
    }
}

impl Drop for ExecFast {
    fn drop(&mut self) {
        nv::set_exec_fast_enabled(self.0);
    }
}

/// The compute ioctl's dispatcher answers by **readiness**, never by role:
/// INFO is the card's two facts plus the two switches and is ready only with
/// the RM attached, SAXPY runs the GR ladder up to step 23 and BENCH the
/// benchmark (both say skipped on a cold GPU and FAILED on the fake RM,
/// status 5 either way), and an op nobody defined is EINVAL.
///
/// What this test used to pin, and no longer does, is the refusal by role:
/// `-ENODEV` and «is the console GPU — compute lives on the secondary» for
/// every op on the card that scans out, whatever state it was in. On a
/// one-card box that was all of compute, for ever, including after the
/// deferred bring-up had the RM up and serving.
#[test]
fn compute_launch_answers_by_readiness_and_never_by_role() {
    use crate::scheme::drm::{COMPUTE_OP_BENCH, COMPUTE_OP_INFO, COMPUTE_OP_SAXPY};
    let _g = LOCK.lock();
    let console = BootFb::console();
    let gpu = super::gpu();
    // Scans out AND cold: refused, but for the reason that is true -- and the
    // refusal is the readiness one every other cold card gets, with the card's
    // own line rather than a sentence about roles.
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_INFO);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (-19, 0, 0));
    assert!(
        r.report
            .starts_with("00:00.0 GeForce RTX 2060 Super scanout=boot-console rm=cold\n"),
        "{}",
        r.report
    );
    assert!(
        !r.report.contains("compute lives on the secondary"),
        "the refusal by role is gone: {}",
        r.report
    );
    // The one that matters: the SAME card, scanning out, with its RM up. This
    // answered -ENODEV for ever before, which is why a one-card box had no
    // compute at all.
    let ready_console = super::gpu_rm();
    let r = DrmScheme::compute_launch(&ready_console, COMPUTE_OP_INFO);
    assert_eq!(r.status, 0, "{}", r.report);
    assert!(
        r.report
            .starts_with("00:00.0 GeForce RTX 2060 Super scanout=boot-console rm=ready\n"),
        "{}",
        r.report
    );
    drop(ready_console);
    drop(console);
    let _no_boot_fb = BootFb::none();

    let _fast = ExecFast::pinned(false);
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_INFO);
    assert_eq!(r.status, -19, "cold");
    assert_eq!(
        r.report,
        "00:00.0 GeForce RTX 2060 Super scanout=no rm=cold\n\
         nouveau_uapi=on\nexec_fast=off\n\
         nvk_compute=/dev/dri/card0 (nouveau); this ioctl is eclipse-compute (Mesa skips card1)\n"
    );
    nv::set_exec_fast_enabled(true);
    assert!(DrmScheme::compute_launch(&gpu, COMPUTE_OP_INFO)
        .report
        .contains("\nnouveau_uapi=on\nexec_fast=on\n"));
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_SAXPY);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (5, 0, 32));
    assert_eq!(
        r.report,
        "[gpustep23] skipped (bring the GPU up first, then gpustep16/17)\n"
    );
    let r = DrmScheme::compute_launch(&gpu, COMPUTE_OP_BENCH);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (5, 0, 0));
    assert_eq!(r.report, "[gpubench] skipped (run /proc/gpuinit first)\n");
    let r = DrmScheme::compute_launch(&gpu, 9);
    assert_eq!((r.status, r.elapsed_ns, r.grid_threads), (-22, 0, 0));
    assert_eq!(r.report, "unknown compute op 9\n");

    let ready = super::gpu_rm();
    let r = DrmScheme::compute_launch(&ready, COMPUTE_OP_INFO);
    assert_eq!(r.status, 0);
    assert!(
        r.report
            .starts_with("00:00.0 GeForce RTX 2060 Super scanout=no rm=ready\n"),
        "{}",
        r.report
    );
    let r = DrmScheme::compute_launch(&ready, COMPUTE_OP_SAXPY);
    assert_eq!((r.status, r.grid_threads), (5, 32));
    assert!(
        r.report.contains("[gpustep23]") && r.report.contains("FAILED"),
        "{}",
        r.report
    );
    assert!(!r.report.contains("LOAD-COMPUTE-STORE PROVEN"));
    let r = DrmScheme::compute_launch(&ready, COMPUTE_OP_BENCH);
    assert_eq!((r.status, r.elapsed_ns), (5, 0));
    assert!(
        r.report.contains("[gpubench]") && r.report.contains("FAILED"),
        "{}",
        r.report
    );
}
