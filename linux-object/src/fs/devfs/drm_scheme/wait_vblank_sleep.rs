//! The sleep behind a blocking `WAIT_VBLANK`: `sys_ioctl` awaits it before
//! the sync arm runs (`README-async-ioctl-vblank.md`). Out of
//! `drm_scheme.rs` for its size ceiling; the shape tests in
//! `pre_wait_resolve_tests.rs` read this file next to it.

use super::*;

impl DrmDev {
    /// Sleep until the vblank a blocking `DRM_IOCTL_WAIT_VBLANK` asked for.
    ///
    /// Called from `sys_ioctl` (async) *before* the request reaches
    /// `io_control` (sync), which then reports the sequence that has by then
    /// genuinely completed. Splitting it this way keeps the whole `FileLike`
    /// stack synchronous while still giving this one ioctl real blocking
    /// semantics.
    ///
    /// It must not spin: an earlier implementation busy-waited the 16.7 ms and
    /// starved every other coroutine on the CPU, which looked like the machine
    /// freezing. The synthetic vblank counter is a pure function of the clock,
    /// so there is an exact deadline to sleep to and no need to poll at all.
    ///
    /// Requests that asked for an event instead of blocking are left alone —
    /// that path already defers correctly through the timer queue.
    pub async fn wait_vblank_sleep(&self, data: usize) -> LxResult<()> {
        // Linux caps this wait at 3 seconds (`DRM_WAIT_ON(..., 3 * HZ, ...)`).
        // Match it: a target far in the future must not park a thread forever.
        const MAX_WAIT: Duration = Duration::from_secs(3);
        self.wait_vblank_sleep_capped(data, kernel_hal::timer::timer_now() + MAX_WAIT)
            .await
    }

    /// [`Self::wait_vblank_sleep`] with its cap handed in, so a test can run
    /// the sleep out in milliseconds.
    ///
    /// A sleep the cap stops leaves its mark on the file for this request
    /// (`DrmFileState::note_vblank_wait_timed_out`): the sync arm that runs
    /// next fills the reply with the count there is and answers EBUSY, as
    /// `drm_wait_vblank_ioctl` does when `DRM_WAIT_ON` runs out. It used to
    /// answer 0 with that count, and a client pacing on the ioctl took the
    /// 3 s stall for the vblank it asked for.
    pub async fn wait_vblank_sleep_capped(&self, data: usize, cap: Duration) -> LxResult<()> {
        if ucheck(data, core::mem::size_of::<DrmWaitVblank>()).is_err() {
            return Ok(()); // io_control will reject it with EFAULT in a moment
        }
        let req = unsafe { *(data as *const DrmWaitVblank) };
        if wait_vblank_check(req.typ).is_err() {
            return Ok(()); // the sync arm refuses it at once; nothing to sleep for
        }
        if req.typ & _DRM_VBLANK_EVENT != 0 {
            return Ok(()); // event form: delivered by the timer queue, never blocks
        }
        const _DRM_VBLANK_RELATIVE: u32 = 0x1;
        const _DRM_VBLANK_NEXTONMISS: u32 = 0x1000_0000;
        // Resolve the target exactly as the sync arm does, or the two would
        // disagree about which vblank was asked for.
        let now_seq = drm::vblank_seq_now();
        let mut target = if req.typ & _DRM_VBLANK_RELATIVE != 0 {
            now_seq.wrapping_add(req.sequence)
        } else {
            req.sequence
        };
        if (target.wrapping_sub(now_seq) as i32) <= 0 {
            if req.typ & _DRM_VBLANK_NEXTONMISS == 0 {
                return Ok(()); // already reached: nothing to wait for
            }
            target = now_seq.wrapping_add(1);
        }
        // Re-check after sleeping instead of trusting one deadline. A wake-up
        // that lands even a nanosecond before the lattice boundary leaves the
        // counter one short, and the sync arm would then report `target - 1`;
        // the caller's next relative request resolves to a vblank that has
        // just passed, returns instantly, and the frame after it waits a full
        // period. That alternation is not a slow kernel -- it is measured as
        // jitter, and it is exactly what a client pacing on this ioctl would
        // see as stutter. The loop is bounded by the same 3 s cap.
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::WaitVblank);
        while (target.wrapping_sub(drm::vblank_seq_now()) as i32) > 0 {
            let Some(deadline) = drm::vblank_deadline_for_seq(target) else {
                break;
            };
            acct.probe();
            if deadline >= cap {
                crate::process::interruptible(kernel_hal::thread::sleep_until(cap)).await?;
                // Stopped by the cap, not by the vblank asked for: the sync
                // arm is to say so (EBUSY), not report the count as the one
                // the caller waited for.
                acct.timed_out();
                self.file.note_vblank_wait_timed_out(data);
                break;
            }
            crate::process::interruptible(kernel_hal::thread::sleep_until(deadline)).await?;
        }
        Ok(())
    }
}
