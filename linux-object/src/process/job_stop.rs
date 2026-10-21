//! Where the threads of a job-control-stopped process park, and what takes
//! them out: a `SIGCONT`, or a `SIGKILL` they have to wake up to die of.

use super::*;

/// Park the current task until this process leaves a job-control stop (or dies).
pub async fn wait_while_job_stopped(proc: &Arc<Process>) {
    loop {
        let stopped = proc
            .try_linux()
            .map(|lp| lp.is_job_stopped())
            .unwrap_or(false);
        if !stopped || matches!(proc.status(), Status::Exited(_)) {
            return;
        }
        proc.signal_clear(JOB_CONTINUE_SIGNAL);
        let stopped = proc
            .try_linux()
            .map(|lp| lp.is_job_stopped())
            .unwrap_or(false);
        if !stopped || matches!(proc.status(), Status::Exited(_)) {
            return;
        }
        let obj: Arc<dyn KernelObject> = proc.clone();
        obj.wait_signal(JOB_CONTINUE_SIGNAL).await;
    }
}

/// The job-control half of `complete_signal()`, for a signal aimed at ONE
/// thread (`tkill`, `tgkill`, `rt_tgsigqueueinfo`, and so `pthread_kill`).
///
/// Queueing the signal on the thread is not enough, and this is the half that
/// was missing: a thread of a job-control-stopped process is parked in
/// `wait_while_job_stopped`, which nothing but `job_continue` clears, and a
/// stopped thread does not run to notice a pending `SIGKILL` either. So
/// `tgkill(pid, tid, SIGCONT)` on a Ctrl-Z'd process never resumed it and
/// `tgkill(pid, tid, SIGKILL)` never woke it to die -- and Go, the JVM and
/// glibc route nearly every signal they send through `tgkill`.
pub fn complete_signal_for_job_control(process: &Arc<Process>, signal: LinuxSignal) {
    wake_for_job_control(process, signal)
}

/// `complete_signal()`: the wake-up half, once the signal is queued.
///
/// It goes here, in the SENDER, because the work is only ever needed when the
/// target is job-control stopped -- and a stopped process's threads are
/// parked in [`wait_while_job_stopped`], so they cannot do it for themselves.
/// After the queueing, as Linux does it, so the thread that wakes already has
/// the signal to act on.
pub(super) fn wake_for_job_control(process: &Arc<Process>, signal: LinuxSignal) {
    let lp = match process.try_linux() {
        Some(lp) => lp,
        None => return,
    };
    match send_effect(signal) {
        SendEffect::Resume => {
            lp.job_continue(process);
        }
        SendEffect::WakeToDie => lp.job_wake_to_die(process),
        SendEffect::Stop | SendEffect::None => {}
    }
}
