//! The OOM killer's half of `mm/oom_kill.c`: whose memory to take when there
//! is none left, and taking it.
//!
//! Until now `oom_score_adj` was a number programs could set and nothing read
//! (systemd's `OOMScoreAdjust=`, `choom`, a browser ranking its renderers),
//! because there was no killer. What happened instead was
//! `#[alloc_error_handler]`: a report of who held the memory, and then the
//! machine stopped. That handler is `-> !` and runs on a heap that is already
//! gone, so nothing can be chosen or killed from it -- by the time it runs the
//! machine is lost, and every process dies, not the worst one.
//!
//! So the caller is the reclaim worker, which is where Linux's is: after
//! reclaim has failed, on a thread that can still allocate, take locks and
//! send a signal. [`kill_worst`] is the whole of it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::*;

/// The page the badness is counted in, which is the page `/proc` reports in
/// (`PROC_PAGE_SIZE`): the score is a number userspace reads, so it is counted
/// in the unit userspace reads it in.
const BADNESS_PAGE: u64 = 4096;

/// `oom_badness`, as `proc_oom_score` scales it: the pages a process holds
/// (resident, swapped out and in page tables) plus `adj` thousandths of all
/// memory, as a share of all memory, on a scale where a process of no size and
/// no adjustment is 666 and the top is 2000.
///
/// 0 for `init` and for `OOM_SCORE_ADJ_MIN`, which an OOM killer never picks
/// -- the two cases this function exists to make unpickable rather than merely
/// unlikely. Never below 0: `adj` thousandths of `total_pages`, thousands
/// taken whole, are less than all of it.
pub fn oom_badness(pid: KoID, adj: i16, held_pages: u64, total_pages: u64) -> u64 {
    if pid == INIT_PID || adj == OOM_SCORE_ADJ_MIN {
        return 0;
    }
    let total = total_pages.max(1) as i64;
    let badness = held_pages as i64 + i64::from(adj) * (total / 1000);
    ((1000 + badness * 1000 / total) * 2 / 3) as u64
}

/// Every live process on the machine, the whole job tree walked once.
///
/// `for_each_process`. One census, because two would drift: `/proc` reads this
/// same list for `ps`, for `/proc/loadavg` and for `/proc/memhogs`.
pub fn live_processes() -> Vec<Arc<Process>> {
    fn collect(job: &Arc<Job>, out: &mut Vec<Arc<Process>>) {
        for id in job.process_ids() {
            if let Some(proc) = job.find_process(id) {
                if !matches!(proc.status(), Status::Exited(_)) {
                    out.push(proc);
                }
            }
        }
        for child_id in job.children_ids() {
            if let Ok(child) = job.get_child(child_id) {
                if let Ok(child_job) = child.downcast_arc::<Job>() {
                    collect(&child_job, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    collect(&ROOT_JOB, &mut out);
    out
}

/// What a kill would take: who, how badly they scored, and how much of the
/// machine's memory goes back when they die.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Victim {
    /// The process id, which for a thread group is its leader's.
    pub pid: KoID,
    /// Its `oom_badness`.
    pub score: u64,
    /// Bytes it holds: resident, shared and its page tables.
    pub held_bytes: u64,
}

/// Rank `rows` the way `select_bad_process` does and answer the worst.
///
/// Taken as rows rather than walking the census itself so that the ranking can
/// be tested without a machine full of processes. Ties go to the one holding
/// more, and then to the higher pid: a tie broken by whichever order the job
/// tree happened to be walked in would make the kill unrepeatable, and an OOM
/// report nobody can reproduce is the one thing worse than no report.
pub fn worst_of(rows: impl IntoIterator<Item = Victim>) -> Option<Victim> {
    rows.into_iter()
        .filter(|v| v.score > 0)
        .max_by_key(|v| (v.score, v.held_bytes, v.pid))
}

/// The pid [`kill_worst`] last signalled, or 0.
///
/// `TIF_MEMDIE` in Linux, and for the same reason: the kill is asynchronous --
/// `SIGKILL` has to reach the process, every thread has to leave the kernel,
/// and only then do the pages come back. A second pass 200 ms later would
/// otherwise see the same shortage, pick the next process and kill that too,
/// and a machine briefly short of memory would lose every process it has in
/// the order of their size.
static DYING: AtomicU64 = AtomicU64::new(0);

/// Whether a process this killer signalled is still on its way out, and so
/// whether a new kill must wait for it.
///
/// Answered by the census rather than by a flag the dying process clears: the
/// process that is leaving cannot be relied on to say so, and a flag nothing
/// clears is a killer that kills once per boot.
fn still_dying(live: impl IntoIterator<Item = KoID>) -> bool {
    let dying = DYING.load(Ordering::Relaxed);
    dying != 0 && live.into_iter().any(|pid| pid == dying)
}

/// Pick the worst process and kill it. Answers whom, or `None` when there was
/// nobody to pick or a previous victim is still leaving.
///
/// `reason` goes in the report, which is the whole point of having one: an OOM
/// kill that leaves no line is a process that "just vanished".
pub fn kill_worst(reason: &str) -> Option<Victim> {
    let live = live_processes();
    if still_dying(live.iter().map(|p| p.id())) {
        return None;
    }
    let (_, total) = kernel_hal::mem::memory_usage();
    let total_pages = total as u64 / BADNESS_PAGE;
    let rows = live.iter().map(|proc| {
        let vmar = proc.vmar();
        let stats = vmar.get_task_stats();
        let held = stats.private_bytes() + stats.shared_bytes() + vmar.page_table_bytes() as u64;
        Victim {
            pid: proc.id(),
            score: oom_badness(
                proc.id(),
                proc.try_linux()
                    .map(LinuxProcess::oom_adj)
                    .unwrap_or_default()
                    .adj,
                held / BADNESS_PAGE,
                total_pages,
            ),
            held_bytes: held,
        }
    });
    let victim = worst_of(rows)?;

    // The report goes out BEFORE the signal: the kill frees memory, and the
    // first thing that happens with freed memory is that something else
    // allocates. A line written after that can be a line that never gets
    // written.
    let name = live
        .iter()
        .find(|p| p.id() == victim.pid)
        .map(|p| p.name())
        .unwrap_or_default();
    zcore_drivers::klog_warn!(
        "[oom] {}: killing pid {} ({}), score {}, holding {} MiB of {} MiB",
        reason,
        victim.pid,
        name,
        victim.score,
        victim.held_bytes / (1024 * 1024),
        total / (1024 * 1024),
    );
    DYING.store(victim.pid, Ordering::Relaxed);
    // Best effort: a process that has exited between the census and here is
    // one whose memory is already coming back, which is what was wanted.
    let _ = send_signal_to_process(victim.pid as usize, LinuxSignal::SIGKILL);
    Some(victim)
}

#[cfg(test)]
#[path = "oom_kill_tests.rs"]
mod oom_kill_tests;
