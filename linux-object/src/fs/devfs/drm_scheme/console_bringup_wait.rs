//! Hold a GPU client's `GEM_NEW`/`VM_BIND`/`EXEC` while the console GPU's
//! deferred bring-up runs, instead of answering it `ENODEV`.
//!
//! Linux has no equivalent of this wait because it has no equivalent of the
//! race: `nouveau_drm_device_init` runs in `probe`, so by the time anything
//! can `open` a node the device is initialised. Eclipse brings the card that
//! scans out the console up in the MIDDLE of a live session -- it has to, the
//! GSP boot wedges with scanout live -- so the first GPU client arrives while
//! that card is still cold, and the three ioctls that need a real RM behind
//! them used to refuse it.
//!
//! **One refusal costs the whole session.** From a dual-2060 box, the boot
//! that prompted this:
//!
//! ```text
//!  6.864  labwc starts
//!  9.497  CHANNEL_ALLOC -> DISCOVERY ONLY (GPU not attached to the RM)
//! 10.050  first GEM_NEW -> errno 19 (ENODEV)
//! 10.051  MESA: ZINK: vkCreateDevice failed (VK_ERROR_OUT_OF_DEVICE_MEMORY)
//! 10.068  [EGL] eglInitialize: EGL_NOT_INITIALIZED, "DRI2: failed to load driver"
//! 10.102  init greps labwc's log for `unable to create renderer`
//! 10.667  the deferred bring-up starts -- 0.6 s too late
//! 13.208  the card is up, with no compositor left that believes in it
//! ```
//!
//! `CHANNEL_ALLOC` already triggers the on-demand bring-up
//! (`ensure_console_gpu_brought_up`), but it succeeds in discovery-only mode
//! and its gate is shut until scanout is parked, so the first call that truly
//! needs the RM is also the first one that can fail -- and it is the one NVK
//! makes its device out of.
//!
//! So: park the call. An async pre-wait ahead of the synchronous arm, the same
//! split as `wait_vblank_sleep`, `syncobj_wait_sleep`, `atomic_in_fence_sleep`
//! and `cpu_prep_sleep`; `INode::io_control` cannot sleep, and a busy-wait in
//! the driver would peg a core for the length of a GSP boot *and* starve the
//! very task it is waiting for when there is one CPU.
//!
//! Waiting here deliberately changes NOTHING about when the bring-up runs.
//! The first version of this also cut the deferred task's 3-second courtesy
//! window the moment a client parked -- the window is there "to let labwc
//! finish its first few frames", frames labwc cannot draw without this very
//! card, so cutting it looked free. It was not: the parked client arrives
//! before `KD_GRAPHICS`, so on a dual-2060 box the bring-up started at 6.8 s
//! instead of 10.7 s, the SEC2 `STARTCPU` store that had completed in 2.3 s
//! at the later moment did not come back at all at the earlier one, and the
//! monitor was gone for the session. A parked client costs nothing by
//! waiting, and the old timing is the only recipe that has ever finished
//! with both cards up, so it is left exactly as it was.
//!
//! [`drm::note_cold_client_waiting`] therefore only REPORTS: the deferred
//! task names a parked client in its log line, and the lines below say which
//! pid parked, when, and how long it waited.

use super::*;
use alloc::string::String;

/// The most one ioctl waits for the console GPU.
///
/// Generous on purpose, and the number comes from the hardware: the comment in
/// `schedule_deferred_console_bringup` records "a 46-second wait that finished
/// with both cards up" on a real dual-2060 box, so anything tighter would give
/// up inside the one case known to work. It also stays under the scanout
/// pause watchdog (`drm::SCANOUT_PAUSE_MAX`), so a wedged GSP unfreezes the
/// screen before this wait ends rather than after. The normal boot pays
/// whatever is left of the bring-up (2.3 s, measured), not this number: the
/// wait ends the instant the card attaches.
///
/// What it is weighed against is not a fast failure -- it is a session on
/// pixman. `nvidia.noconsole_gpu` skips the bring-up, and then nothing is
/// expected and nothing is waited for.
const AWAIT_BRINGUP_US: u64 = 60_000_000;

/// How often the parked call looks. The bring-up it waits for takes seconds,
/// so this is about how promptly the client resumes once the card is up, not
/// about polling cost.
const POLL_US: u64 = 20_000;

/// Said once per boot: every client blocked on the same bring-up would
/// otherwise repeat it, and they all wake together.
static ANNOUNCED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// This node's card, when it is a console GPU that is not usable yet, with the
/// driver's own one-line description of it.
///
/// `console_gpu_ready` answers `None` for every card that does not scan out
/// the console, which is exactly the filter this needs: a `GEM_NEW` on the
/// compute card must never wait for the console card's bring-up. On the box
/// above that distinction is the whole point -- NVK enumerates `renderD128`
/// (the console card, cold) first and `renderD129` (compute, state-loaded at
/// boot) second, and builds its device from the one that fails.
fn cold_console_card(dev: &DrmDev) -> Option<String> {
    match dev.driver()?.console_gpu_ready() {
        Some((false, detail)) => Some(detail),
        _ => None,
    }
}

impl DrmDev {
    /// Wait for the console GPU's bring-up before the sync arm answers
    /// `ENODEV` to a `GEM_NEW`, `VM_BIND` or `EXEC` on it.
    ///
    /// Called from `sys_ioctl` (async) before the request reaches
    /// `io_control` (sync), which then finds the card attached and serves the
    /// call normally. Every early return leaves the ioctl behaving exactly as
    /// it did before this existed, and so does the timeout: the sync arm
    /// still refuses what it cannot serve.
    pub async fn console_bringup_sleep(&self, cmd: u32) -> LxResult<()> {
        if !zcore_drivers::display::nouveau_uapi_enabled() {
            return Ok(());
        }
        if !zcore_drivers::display::needs_rm_attached(cmd) {
            return Ok(());
        }
        // Cheapest first, and the one that is false for almost every boot:
        // no bring-up scheduled, or one that has already run.
        if !drm::console_bringup_unfinished() {
            return Ok(());
        }
        let Some(detail) = cold_console_card(self) else {
            return Ok(());
        };
        // Before the first sleep, so the task we are waiting for sees it on
        // its very next look.
        drm::note_cold_client_waiting();
        let began = kernel_hal::timer::timer_now();
        let announce = !ANNOUNCED.swap(true, core::sync::atomic::Ordering::Relaxed);
        if announce {
            // pid and clock on purpose: the deferred task's own window is
            // timed off `KD_GRAPHICS`, and which process parks here and WHEN
            // is what says whether the two are about to collide. The boot
            // that lost its monitor had something park before 6.6 s and no
            // line named it.
            kernel_hal::klog_warn!(
                "[drm] {} from pid {} arrived on the console GPU at {}.{:03} s, before its RM \
                 is attached: holding the call while the deferred bring-up runs, instead of \
                 ENODEV (a refusal here is VK_ERROR_OUT_OF_DEVICE_MEMORY and pixman for the \
                 session) -- {}",
                zcore_drivers::display::nouveau_ioctl_name(cmd & 0xff),
                drm::current_pid(),
                began.as_secs(),
                began.subsec_millis(),
                detail.trim_end()
            );
        }
        let deadline = began + Duration::from_micros(AWAIT_BRINGUP_US);
        loop {
            // Asked on every look: a `^C` landing in a 60 s wait must not
            // wait it out.
            crate::process::check_signals()?;
            if cold_console_card(self).is_none() || !drm::console_bringup_unfinished() {
                if announce {
                    let held = kernel_hal::timer::timer_now().saturating_sub(began);
                    kernel_hal::klog_warn!(
                        "[drm] console GPU ready; the parked call goes through after {}.{:03} s",
                        held.as_secs(),
                        held.subsec_millis()
                    );
                }
                return Ok(());
            }
            let now = kernel_hal::timer::timer_now();
            if now >= deadline {
                kernel_hal::klog_warn!(
                    "[drm] the console GPU was still not attached after {} s; letting the \
                     call through to the refusal it would have had at once. See \
                     `dmesg | grep gpustep14` for where the bring-up stopped.",
                    AWAIT_BRINGUP_US / 1_000_000
                );
                return Ok(());
            }
            let wake = core::cmp::min(now + Duration::from_micros(POLL_US), deadline);
            crate::process::interruptible(kernel_hal::thread::sleep_until(wake)).await?;
        }
    }
}
