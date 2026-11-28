//! A server the kernel found hung, and killed: who, what it served, and
//! when. The kernel's watches (a served disk, file system or device that
//! answers nothing, a network stack that leaves a ping unanswered) end such
//! a server with `SIGKILL`, which is all its parent learns from `waitpid`:
//! the same death as a `kill -9`. `/proc/hung` lists the last of them, so
//! that init can tell its own state file, and the monitor its reader, that
//! the service was killed for hanging ([`kill_hung`]).

use alloc::collections::VecDeque;
use alloc::string::String;
use core::fmt::Write;

use lock::Mutex;

use super::send_signal_to_process;
use crate::error::LxResult;
use crate::signal::Signal;

/// How many kills `/proc/hung` remembers: the last ones, a boot's worth of
/// a server that keeps hanging.
pub const HUNG_KEPT: usize = 32;

/// One kill: the pid, what it served, and when (ms since boot).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Hung {
    pid: usize,
    what: String,
    at_ms: u64,
}

static HUNG: Mutex<VecDeque<Hung>> = Mutex::new(VecDeque::new());

/// Kill `pid`, a server found hung serving `what` ("disk vda", "/dev/dsp"),
/// with `SIGKILL`, and remember it for `/proc/hung`.
pub fn kill_hung(pid: usize, what: &str) -> LxResult<()> {
    let at_ms = (kernel_hal::timer::timer_now().as_nanos() / 1_000_000) as u64;
    note_hung(pid, what, at_ms);
    send_signal_to_process(pid, Signal::SIGKILL)
}

fn note_hung(pid: usize, what: &str, at_ms: u64) {
    let mut hung = HUNG.lock();
    if hung.len() >= HUNG_KEPT {
        hung.pop_front();
    }
    hung.push_back(Hung {
        pid,
        what: what
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
        at_ms,
    });
}

/// `/proc/hung`: one line a kill, the oldest first, tab-separated: the pid,
/// the milliseconds since boot, and what it served.
pub fn hung_text() -> String {
    let mut s = String::new();
    for h in HUNG.lock().iter() {
        let _ = writeln!(s, "{}\t{}\t{}", h.pid, h.at_ms, h.what);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_kills_are_listed_oldest_first_one_to_a_line() {
        for i in 0..HUNG_KEPT + 2 {
            note_hung(100_000 + i, "disk\tvda", i as u64);
        }
        let text = hung_text();
        let ours: alloc::vec::Vec<&str> = text.lines().filter(|l| l.starts_with("1000")).collect();
        assert_eq!(ours.len(), HUNG_KEPT);
        assert_eq!(ours[0], "100002\t2\tdisk vda");
        assert_eq!(
            *ours.last().unwrap(),
            alloc::format!("{}\t{}\tdisk vda", 100_000 + HUNG_KEPT + 1, HUNG_KEPT + 1)
        );
    }
}
