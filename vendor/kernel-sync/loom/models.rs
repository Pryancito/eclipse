//! Every interleaving of the kernel locks, checked by loom.
//!
//! Run with (from the top of the tree)
//!
//! ```text
//! cargo test --release --manifest-path vendor/kernel-sync/loom/Cargo.toml
//! ```
//!
//! `cfg(loom)` (set by `build.rs`) swaps the atomics in `../src/prims.rs` for
//! loom's, and loom then runs each `model` body once per schedule it can tell
//! apart, including every reordering the C11 memory model permits for the
//! orderings the lock asked for -- not the handful an x86 machine shows. The
//! protected value is a `loom::cell::UnsafeCell`, so a critical section that
//! is not ordered after the previous one (a release that should have been
//! `Release`, an acquire that should have been `Acquire`) is reported as a
//! data race on it even when no schedule ever produces a wrong number.
//!
//! Each model has two contenders and a couple of operations each, and is
//! searched **without** a preemption bound, so it really is every schedule:
//! the whole file takes well under a second in release. Three spinners do
//! not finish (see `MAX_BRANCHES`); the bugs locks have are almost all
//! visible with two contenders and one hand-over.

use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;

use crate::interrupt::lock_depth;
use crate::mcslock::{LockChannel, MCSLock};
use crate::rwlock::RwLock;
use crate::spin::SpinMutex;
use crate::ticket::TicketMutex;

/// `loom::model` with no bound on preemptions: every schedule loom can tell
/// apart. The two-contender models below finish in well under a second this
/// way, so nothing is cut. `LOOM_MAX_PREEMPTIONS` still bounds it by hand.
fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut b = loom::model::Builder::new();
    b.max_branches = MAX_BRANCHES;
    b.check(f);
}

/// Turns of a spin loop are branches too, so a schedule can be longer than
/// loom's default of 1000 allows.
///
/// Three contenders spinning on one word do not finish even bounded to two
/// preemptions and 20 000 branches: every turn of every spin is a choice
/// point. That is why every model here has at most two threads that spin.
const MAX_BRANCHES: usize = 20_000;

/// A value loom watches for unsynchronised access. `Sync` by fiat: the lock
/// around it is what is under test, and loom's cell is what would object if
/// the lock did not actually order the accesses.
struct Cell(UnsafeCell<u64>);

unsafe impl Sync for Cell {}

impl Cell {
    fn new(v: u64) -> Self {
        Cell(UnsafeCell::new(v))
    }

    fn get(&self) -> u64 {
        self.0.with(|p| unsafe { *p })
    }

    fn bump(&self) {
        self.0.with_mut(|p| unsafe { *p += 1 })
    }

    fn set(&self, v: u64) {
        self.0.with_mut(|p| unsafe { *p = v })
    }
}

/// `threads` contenders, each running `body` once; then `check` on what is
/// left, from the thread that spawned them.
fn contend<L: Send + Sync + 'static>(lock: L, threads: usize, body: fn(&L), check: fn(&L)) {
    let lock = Arc::new(lock);
    let handles: std::vec::Vec<_> = (0..threads)
        .map(|_| {
            let l = lock.clone();
            thread::spawn(move || {
                body(&l);
                assert_eq!(lock_depth(), 0, "a guard left a push_off behind");
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    check(&lock);
}

// ── the ticket lock (the kernel's `Mutex`) ──────────────────────────────────

#[test]
fn loom_ticket_two_lockers_exclude_and_hand_over() {
    model(|| {
        contend(
            TicketMutex::new(Cell::new(0)),
            2,
            |m| m.lock().bump(),
            |m| assert_eq!(m.lock().get(), 2),
        );
    });
}

#[test]
fn loom_ticket_try_lock_never_shares() {
    model(|| {
        contend(
            TicketMutex::new(Cell::new(0)),
            2,
            |m| {
                if let Some(g) = m.try_lock() {
                    g.bump();
                } else {
                    m.lock().bump();
                }
            },
            |m| {
                assert_eq!(m.lock().get(), 2);
                assert!(!m.is_locked());
            },
        );
    });
}

#[test]
fn loom_ticket_failed_try_lock_does_not_wedge_the_queue() {
    // A `try_lock` that lost must leave no ticket behind: if it took one and
    // never served it, the next `lock` would wait for ever (loom reports a
    // livelock as a deadlock once every thread spins).
    model(|| {
        let m = Arc::new(TicketMutex::new(Cell::new(0)));
        let g = m.lock();
        let m2 = m.clone();
        let t = thread::spawn(move || {
            assert!(m2.try_lock().is_none());
        });
        t.join().unwrap();
        g.bump();
        drop(g);
        let m3 = m.clone();
        thread::spawn(move || m3.lock().bump()).join().unwrap();
        assert_eq!(m.lock().get(), 2);
    });
}

// ── the unfair spin lock ─────────────────────────────────────────────────────

#[test]
fn loom_spin_two_lockers_exclude() {
    model(|| {
        contend(
            SpinMutex::new(Cell::new(0)),
            2,
            |m| m.lock().bump(),
            |m| assert_eq!(m.lock().get(), 2),
        );
    });
}

#[test]
fn loom_spin_try_lock_never_shares() {
    model(|| {
        contend(
            SpinMutex::new(Cell::new(0)),
            2,
            |m| match m.try_lock() {
                Some(g) => g.bump(),
                None => m.lock().bump(),
            },
            |m| {
                assert_eq!(m.lock().get(), 2);
                assert!(!m.is_locked());
            },
        );
    });
}

// ── the two-channel mutex ────────────────────────────────────────────────────

#[test]
fn loom_mcs_channels_exclude_each_other() {
    model(|| {
        let m = Arc::new(MCSLock::new(Cell::new(0)));
        let m2 = m.clone();
        let t = thread::spawn(move || m2.lock(LockChannel::Interrupt).bump());
        m.lock(LockChannel::Normal).bump();
        t.join().unwrap();
        assert_eq!(m.lock(LockChannel::Normal).get(), 2);
        assert!(!m.is_held());
    });
}

#[test]
fn loom_mcs_try_lock_never_shares() {
    model(|| {
        contend(
            MCSLock::new(Cell::new(0)),
            2,
            |m| match m.try_lock(LockChannel::Interrupt) {
                Some(g) => g.bump(),
                None => m.lock(LockChannel::Normal).bump(),
            },
            |m| assert_eq!(m.lock(LockChannel::Normal).get(), 2),
        );
    });
}

// ── the reader-writer lock ───────────────────────────────────────────────────

/// Two fields one writer changes together: a reader that ever sees them
/// differ saw a write half done.
struct Pair(Cell, Cell);

impl Pair {
    fn new() -> Self {
        Pair(Cell::new(0), Cell::new(0))
    }
    fn write(&self, v: u64) {
        self.0.set(v);
        self.1.set(v);
    }
    fn read(&self) -> (u64, u64) {
        (self.0.get(), self.1.get())
    }
}

#[test]
fn loom_rwlock_readers_never_see_a_half_write() {
    model(|| {
        let l = Arc::new(RwLock::new(Pair::new()));
        let w = l.clone();
        let writer = thread::spawn(move || w.write().write(1));
        let r = l.clone();
        let reader = thread::spawn(move || {
            let (a, b) = r.read().read();
            assert_eq!(a, b, "a reader saw a write half done");
        });
        writer.join().unwrap();
        reader.join().unwrap();
        assert_eq!(l.read().read(), (1, 1));
        assert_eq!(l.reader_count(), 0);
        assert_eq!(l.writer_count(), 0);
    });
}

#[test]
fn loom_rwlock_two_writers_exclude() {
    model(|| {
        contend(
            RwLock::new(Cell::new(0)),
            2,
            |l| l.write().bump(),
            |l| assert_eq!(l.read().get(), 2),
        );
    });
}

#[test]
fn loom_rwlock_upgrade_excludes_a_writer() {
    // The upgradeable reader reads, upgrades and writes back what it read
    // plus one: a writer slipping in between the read and the upgrade would
    // be a lost update.
    model(|| {
        let l = Arc::new(RwLock::new(Cell::new(0)));
        let w = l.clone();
        let writer = thread::spawn(move || w.write().bump());
        {
            let u = l.upgradeable_read();
            let seen = u.get();
            let g = u.upgrade();
            g.set(seen + 1);
        }
        writer.join().unwrap();
        assert_eq!(l.read().get(), 2);
        assert_eq!(l.reader_count(), 0);
        assert_eq!(l.writer_count(), 0);
    });
}

#[test]
fn loom_rwlock_two_upgradeable_readers_exclude() {
    model(|| {
        contend(
            RwLock::new(Cell::new(0)),
            2,
            |l| {
                let u = l.upgradeable_read();
                let seen = u.get();
                u.upgrade().set(seen + 1);
            },
            |l| assert_eq!(l.read().get(), 2),
        );
    });
}

#[test]
fn loom_rwlock_try_upgrade_and_downgrade_keep_counts() {
    model(|| {
        let l = Arc::new(RwLock::new(Cell::new(0)));
        let r = l.clone();
        let reader = thread::spawn(move || {
            if let Some(g) = r.try_read() {
                let _ = g.get();
            }
        });
        {
            let u = l.upgradeable_read();
            let w = match u.try_upgrade() {
                Ok(w) => w,
                Err(u) => u.upgrade(),
            };
            w.bump();
            let d = w.downgrade();
            assert_eq!(d.get(), 1);
        }
        reader.join().unwrap();
        assert_eq!(l.reader_count(), 0);
        assert_eq!(l.writer_count(), 0);
        assert!(l.try_write().is_some());
    });
}

#[test]
fn loom_rwlock_try_write_vs_reader() {
    model(|| {
        contend(
            RwLock::new(Pair::new()),
            2,
            |l| match l.try_write() {
                Some(g) => g.write(7),
                None => {
                    let (a, b) = l.read().read();
                    assert_eq!(a, b);
                }
            },
            |l| {
                assert_eq!(l.reader_count(), 0);
                assert_eq!(l.writer_count(), 0);
            },
        );
    });
}
