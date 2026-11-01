//! The sleep ahead of a nouveau `EXEC` whose waits go to the CPU: `sys_ioctl`
//! awaits it before the sync arm runs. Out of `drm_scheme.rs` for its size
//! ceiling; the shape tests in `pre_wait_resolve_tests.rs` read this file
//! next to it.

use super::*;

/// How long the pre-wait may hold an `EXEC`: the arm's own `WAIT_TIMEOUT_US`.
/// Past it the arm answers EIO either way, so a sleep here can never outlast
/// the wait it stands in for.
pub(super) const EXEC_WAIT_TIMEOUT_US: u64 = 10_000_000;

impl DrmDev {
    /// Sleep until the waits of a nouveau `EXEC` that its arm would do on the
    /// CPU have landed.
    ///
    /// An `EXEC` hands the GPU the waits it can (an ACQUIRE on a semaphore
    /// of the submitting context, or of a peer context it can map) and waits
    /// for the rest on the CPU, inside the synchronous arm -- spinning,
    /// because `io_control` cannot sleep. On a desktop that rest is the
    /// compositor's release of the client's buffer, so every Vulkan or zink
    /// frame spun a core until the compositor had let go of the image.
    ///
    /// Same split as [`Self::syncobj_wait_sleep`]: the driver names the set
    /// ([`drm::exec_cpu_waits`], exactly the one its arm would CPU-wait, so a
    /// wait the GPU takes is not held back here), this sleeps on it with
    /// `wait_ready`, and the arm then finds those syncobjs signaled. Every
    /// early return leaves the `EXEC` behaving exactly as before.
    pub async fn exec_wait_sleep(&self, cmd: u32, data: usize) -> LxResult<()> {
        if !zcore_drivers::display::nouveau_uapi_enabled()
            || !zcore_drivers::display::is_exec_ioctl(cmd)
        {
            return Ok(());
        }
        // `data` and the wait array are the caller's own pointers, checked
        // before they are read: the driver's arm reads them after
        // `io_control` has vetted the fd, and this runs on anything
        // userspace hands us.
        if ucheck(data, zcore_drivers::display::EXEC_REQUEST_BYTES).is_err() {
            return Ok(());
        }
        // SAFETY: `ucheck` just said those bytes are readable.
        let (wait_ptr, wait_count) = unsafe { zcore_drivers::display::exec_wait_list(data) };
        if wait_count == 0 || wait_count > zcore_drivers::display::EXEC_MAX_WAITS {
            return Ok(());
        }
        if ucheck(
            wait_ptr,
            wait_count as usize * zcore_drivers::display::EXEC_SYNC_BYTES,
        )
        .is_err()
        {
            return Ok(());
        }
        let Some((handles, points)) = drm::exec_cpu_waits(data, drm::current_pid()) else {
            return Ok(());
        };
        let deadline_us = kernel_hal::timer::timer_now().as_micros() as u64 + EXEC_WAIT_TIMEOUT_US;
        let deadline = Duration::from_micros(deadline_us);
        let mut acct = PreWaitAccount::new(zcore_drivers::scheme::prewait::Kind::ExecWait);
        loop {
            // `wait_ready` resolves the pending fences itself, under the
            // table lock it takes anyway: no `poll_pending()` in front of it.
            match zcore_drivers::scheme::syncobj::wait_ready(
                &handles,
                Some(&points),
                true,
                deadline_us,
            ) {
                Some(answer) => {
                    if matches!(
                        answer,
                        Err(zcore_drivers::scheme::syncobj::WaitOutcome::Timeout)
                    ) {
                        acct.timed_out();
                    }
                    return Ok(());
                }
                None => {
                    if !fence_poll_wait(acct.probes, deadline).await? {
                        acct.timed_out();
                        return Ok(());
                    }
                    acct.probe();
                }
            }
        }
    }
}
