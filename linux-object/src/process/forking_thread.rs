//! The thread a child was forked by, and what its exit tells the child.
//!
//! `PR_SET_PDEATHSIG` fires when the THREAD that forked the child exits,
//! not only when its whole process does: a child's `real_parent` is that
//! thread, and `forget_original_parent` runs for every task on its way out.
//! A program that forks from a worker thread and lets the worker finish has
//! its children told that their parent died, while `getppid` goes on naming
//! the same process. Here the signal only went out when the process died.

use super::*;

impl LinuxProcess {
    /// Record the thread that forked this process.
    pub fn set_forking_thread(&self, tid: KoID) {
        self.inner.lock().forking_thread = tid;
    }

    /// The thread that forked this process, or the one standing in for it
    /// since that thread exited; 0 for a process no thread forked.
    pub fn forking_thread(&self) -> KoID {
        self.inner.lock().forking_thread
    }
}

/// Thread `tid` of `proc` is leaving while others live on: every live child
/// it forked gets its parent-death signal, when it asked for one, and
/// another thread of `proc` stands in as its forking thread from now on.
///
/// Nothing happens when `tid` is the last thread. Then the process itself is
/// dying, and its exit tells the children and hands them over.
pub fn forget_forking_thread(proc: &Process, tid: KoID) {
    let heir = proc
        .threads()
        .into_iter()
        .find(|t| t.id() != tid && !is_on_its_way_out(t));
    let (Some(heir), Some(lp)) = (heir, proc.try_linux()) else {
        return;
    };
    let children: Vec<Arc<Process>> = lp
        .inner
        .lock()
        .children
        .values()
        .filter(|c| !matches!(c.status(), Status::Exited(_)))
        .cloned()
        .collect();
    for child in children {
        let Some(child_lp) = child.try_linux() else {
            continue;
        };
        let sig = {
            let mut inner = child_lp.inner.lock();
            if inner.forking_thread != tid {
                continue;
            }
            inner.forking_thread = heir.id();
            inner.pdeathsig
        };
        if sig != 0 {
            if let Ok(signal) = LinuxSignal::try_from(sig) {
                let _ = send_signal_to_process(child.id() as usize, signal);
            }
        }
    }
}

#[cfg(test)]
mod tests;
