//! The `siginfo_t` of each pending signal, queued as Linux 6.18 queues it
//! (`struct sigqueue` on `struct sigpending`, `__send_signal_locked` and
//! `collect_signal` in `kernel/signal.c`), and counted against the receiving
//! user's `RLIMIT_SIGPENDING`.
//!
//! What was here was one `siginfo_t` per signal NUMBER, kept from the first
//! sender. That is right for the 31 standard signals and wrong for the 33
//! real-time ones, which POSIX queues: the second `sigqueue(3)` of a blocked
//! `SIGRTMIN` was dropped with success, so a program that hands work to a
//! thread through `sigqueue` values (each carrying a pointer or an index) got
//! the first item and lost every other one without an error anywhere. The
//! `rtq` probe measures 52 cases on Linux and they are the tests below.
//!
//! The pending BITS stay where they were (`LinuxThread::signals`, the
//! process's `shared_pending`): what this keeps is the entries, and every
//! method takes the bit set it goes with, so the two move together. A bit
//! with no entry is a signal whose information was lost (or a `SIGKILL`,
//! which Linux never gives one), and it is taken as `SI_USER` from nobody,
//! as `collect_signal` takes it.

use super::{SigInfo, Signal, SignalCode, Sigset};
use alloc::collections::{BTreeMap, VecDeque};
use lock::Mutex;

#[cfg(test)]
mod tests;

/// The entries queued right now for each real user, the count Linux keeps
/// in `ucounts->rlimit[UCOUNT_RLIMIT_SIGPENDING]` and `/proc/<pid>/status`
/// reports as the first half of `SigQ:`. A user with none has no row.
static QUEUED: Mutex<BTreeMap<u32, u64>> = Mutex::new(BTreeMap::new());

/// How many signals are queued for real user `uid`, on every task.
pub fn queued_for_user(uid: u32) -> u64 {
    QUEUED.lock().get(&uid).copied().unwrap_or(0)
}

/// Whom a signal is counted against, and how far: the receiving process's
/// real uid and its `RLIMIT_SIGPENDING` soft limit (`task_ucounts(t)` and
/// `task_rlimit(t, RLIMIT_SIGPENDING)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receiver {
    pub uid: u32,
    pub limit: u64,
}

/// One entry counted against its user for as long as it exists, and given
/// back when it goes: taken, discarded, or dropped with its task.
#[derive(Debug)]
pub struct Charge {
    uid: u32,
}

impl Charge {
    /// `sig_get_ucounts()`: one more for `to.uid`, refused once that would
    /// pass `to.limit` unless `override_limit`, which counts it anyway.
    pub fn take(to: Receiver, override_limit: bool) -> Option<Charge> {
        let mut queued = QUEUED.lock();
        let now = queued.get(&to.uid).copied().unwrap_or(0);
        let after = now.saturating_add(1);
        if !override_limit && after > to.limit {
            return None;
        }
        queued.insert(to.uid, after);
        Some(Charge { uid: to.uid })
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let mut queued = QUEUED.lock();
        if let Some(n) = queued.get_mut(&self.uid) {
            *n -= 1;
            if *n == 0 {
                queued.remove(&self.uid);
            }
        }
    }
}

/// `__send_signal_locked` refused the signal: a real-time one sent by
/// something other than `kill(2)` with its user's queue full (`EAGAIN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueFull;

/// One `struct sigqueue`.
struct Queued {
    info: SigInfo,
    /// `None` for the entries nobody is charged for: a POSIX timer's, which
    /// was charged once at `timer_create`, and the ones queued without a
    /// receiver.
    _charge: Option<Charge>,
    /// The preallocated entry of a POSIX timer (`k_itimer::sigq`), which
    /// only that timer finds and takes back.
    timer: bool,
}

impl Queued {
    fn bare(signal: Signal) -> Queued {
        Queued {
            info: SigInfo::bare(signal),
            _charge: None,
            timer: false,
        }
    }
}

/// The entries of one pending set, by signal, each signal's in the order
/// they were sent. Linux keeps one list for all of them, but every reader
/// takes the first entry OF A SIGNAL (`collect_signal`), so the order across
/// signals is never looked at: the lowest pending number goes first.
#[derive(Default)]
pub struct SigQueue {
    by_signal: BTreeMap<u8, VecDeque<Queued>>,
}

impl SigQueue {
    /// `__send_signal_locked()` from `legacy_queue` on: make `signal`
    /// pending in `pending`, the set this queue goes with.
    ///
    /// - A standard signal already pending is not queued again, and keeps
    ///   the first sender's information.
    /// - `SIGKILL` gets no entry.
    /// - Every other send is an entry, counted against `to`. A standard
    ///   signal from the kernel or from `kill` (`si_code >= 0`, or no
    ///   information at all) is counted past the limit; nothing else is.
    /// - Past the limit, a real-time signal sent by something other than
    ///   `kill` is refused ([`QueueFull`]); anything else is still made
    ///   pending, and loses its information.
    ///
    /// `to: None` counts nothing and refuses nothing.
    pub fn send(
        &mut self,
        pending: &mut Sigset,
        signal: Signal,
        info: Option<SigInfo>,
        to: Option<Receiver>,
    ) -> Result<(), QueueFull> {
        if signal.is_standard() && pending.contains(signal) {
            return Ok(());
        }
        if signal != Signal::SIGKILL {
            let charge = match to {
                None => None,
                Some(to) => {
                    let override_limit = signal.is_standard() && info.is_none_or(|i| i.code.0 >= 0);
                    match Charge::take(to, override_limit) {
                        Some(charge) => Some(charge),
                        None if !signal.is_standard()
                            && info.is_some_and(|i| i.code != SignalCode::USER) =>
                        {
                            return Err(QueueFull);
                        }
                        None => {
                            pending.insert(signal);
                            return Ok(());
                        }
                    }
                }
            };
            self.push(
                signal,
                Queued {
                    info: info.unwrap_or_else(|| SigInfo::bare(signal)),
                    _charge: charge,
                    timer: false,
                },
            );
        }
        pending.insert(signal);
        Ok(())
    }

    /// `posixtimer_send_sigqueue()`: queue the entry of a POSIX timer, which
    /// was charged when the timer was created and so is never refused, and
    /// which goes in even when its signal is already pending (there is no
    /// `legacy_queue` on this path).
    pub fn send_timer(&mut self, pending: &mut Sigset, signal: Signal, info: SigInfo) {
        self.push(
            signal,
            Queued {
                info,
                _charge: None,
                timer: true,
            },
        );
        pending.insert(signal);
    }

    /// `collect_signal()`: take the first entry of `signal`, which stays
    /// pending while it has another. One with none is `SI_USER` from nobody.
    pub fn take(&mut self, pending: &mut Sigset, signal: Signal) -> SigInfo {
        let key = signal as u8;
        let first = self.by_signal.get_mut(&key).and_then(VecDeque::pop_front);
        if self.by_signal.get(&key).is_none_or(VecDeque::is_empty) {
            self.by_signal.remove(&key);
            pending.remove(signal);
        }
        first.map_or_else(|| SigInfo::bare(signal), |q| q.info)
    }

    /// `flush_sigqueue_mask()` for one signal: it stops being pending, and
    /// every entry it had goes, with its charge.
    pub fn discard(&mut self, pending: &mut Sigset, signal: Signal) {
        pending.remove(signal);
        self.by_signal.remove(&(signal as u8));
    }

    /// Every entry, with its charge: what goes when the task does.
    pub fn clear(&mut self) {
        self.by_signal.clear();
    }

    /// Hand `act` the `siginfo_t` of POSIX timer `timerid`'s entry for
    /// `signal`, and take the entry out when `act` says so; say whether it
    /// was there. Nobody else's entry is looked at, even one carrying the
    /// same `SI_TIMER` and id (`rt_sigqueueinfo` may send that code).
    pub fn at_timer(
        &mut self,
        pending: &mut Sigset,
        signal: Signal,
        timerid: i32,
        act: impl FnOnce(&mut SigInfo) -> bool,
    ) -> bool {
        let key = signal as u8;
        if !pending.contains(signal) {
            return false;
        }
        let Some(entries) = self.by_signal.get_mut(&key) else {
            return false;
        };
        let Some(at) = entries
            .iter()
            .position(|q| q.timer && q.info.timer_id() == Some(timerid))
        else {
            return false;
        };
        if act(&mut entries[at].info) {
            entries.remove(at);
            if entries.is_empty() {
                self.by_signal.remove(&key);
                pending.remove(signal);
            }
        }
        true
    }

    /// Move `signal` from this set (`pending`) onto `into` (`into_pending`),
    /// with its entries after the ones already there, charges and all: a
    /// process-directed signal pulled onto the thread that takes it.
    ///
    /// Linux never moves it: it stays on the shared list, the thread's own
    /// list is read first (`dequeue_signal`), and both are delivered. So
    /// when the thread has `signal` pending already, a bit with no entry on
    /// either side becomes one, so the two stay two.
    pub fn move_into(
        &mut self,
        pending: &mut Sigset,
        signal: Signal,
        into: &mut SigQueue,
        into_pending: &mut Sigset,
    ) {
        if !pending.contains(signal) {
            return;
        }
        pending.remove(signal);
        let moved = self.by_signal.remove(&(signal as u8)).unwrap_or_default();
        let both = into_pending.contains(signal);
        into_pending.insert(signal);
        if !both && moved.is_empty() {
            return;
        }
        let entries = into.by_signal.entry(signal as u8).or_default();
        if both && entries.is_empty() {
            entries.push_back(Queued::bare(signal));
        }
        if both && moved.is_empty() {
            entries.push_back(Queued::bare(signal));
        }
        entries.extend(moved);
    }

    /// How many entries `signal` has.
    pub fn entries_of(&self, signal: Signal) -> usize {
        self.by_signal.get(&(signal as u8)).map_or(0, VecDeque::len)
    }

    fn push(&mut self, signal: Signal, entry: Queued) {
        self.by_signal
            .entry(signal as u8)
            .or_default()
            .push_back(entry);
    }
}
