//! The kernel locks, exercised on the host.
//!
//! Every lock in this crate brackets its critical section with
//! `push_off`/`pop_off`: interrupts off on the way in, and back to however they
//! were on the way out of the *outermost* one. The whole family shares one
//! failure mode — a guard whose acquire and release do not come in pairs. One
//! push too few and the CPU keeps interrupts disabled for good; one pop too
//! many and it re-enables them **inside** somebody else's critical section,
//! then trips `pop_off`'s underflow somewhere else entirely.
//!
//! None of this could be tested before: see `KERNEL_LOCKS_ON_HOST` in `lib.rs`.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::cpuid::NO_CPU;
use crate::interrupt::{
    bogus_cpu_id_events, current_cpu_id, current_cpu_id_via_apic, hardware_id_of, lock_depth,
    set_logical_cpu_id, set_test_hw_id, set_test_published, with_ap_boot_logical,
};
use crate::rwlock::RwLock;
use crate::spin::SpinMutex;
use crate::ticket::TicketMutex;

/// Logical ids handed out to test threads. Deliberately capped well below
/// `MAX_CORE_NUM` so [`UNREGISTERED_LOGICAL`] can never be one of them.
static NEXT_LOGICAL: AtomicU32 = AtomicU32::new(0);

/// Ids whose test thread has finished, waiting to be handed to the next one.
///
/// Without this the pool was one id per **test in the crate** rather than one
/// per *live* test thread, so the harness had a ceiling on how many tests this
/// crate could hold at all -- reached the day `mcslock.rs` got its own, at which
/// point two unrelated rwlock tests started failing with "ran out of simulated
/// CPUs". The ids are recycled rather than the ceiling raised, because the
/// ceiling is real: `MAX_CORE_NUM` slots is what the per-CPU arrays have.
static FREE_LOGICAL: std::sync::Mutex<std::vec::Vec<u8>> =
    std::sync::Mutex::new(std::vec::Vec::new());

fn free_logical() -> std::sync::MutexGuard<'static, std::vec::Vec<u8>> {
    FREE_LOGICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// One test thread's claim on a simulated CPU, given back when it exits.
struct CpuSlot(u8);

impl Drop for CpuSlot {
    fn drop(&mut self) {
        free_logical().push(self.0);
    }
}

/// In range for every per-CPU array, and registered by nobody — which is the
/// exact shape of the id a corrupted GS reported on a 6-vCPU guest.
const UNREGISTERED_LOGICAL: u8 = (crate::MAX_CORE_NUM - 1) as u8;

/// Make the calling test thread a CPU of its own: its own hardware id, its own
/// logical id, its own per-CPU slot. Idempotent per thread.
///
/// Without it every thread answers `cpu_id() == 0` and they all nest their
/// interrupt-disable depth in one slot — which is precisely the bug this file
/// exists to catch, so the harness must not reproduce it.
fn this_cpu() -> u8 {
    use core::cell::RefCell;
    std::thread_local! {
        static MINE: RefCell<Option<CpuSlot>> = const { RefCell::new(None) };
    }
    if let Some(id) = MINE.with(|m| m.borrow().as_ref().map(|slot| slot.0)) {
        return id;
    }
    // A finished thread's id first; a fresh one only when none is waiting.
    let logical = match free_logical().pop() {
        Some(id) => id,
        None => {
            let next = NEXT_LOGICAL.fetch_add(1, Ordering::Relaxed);
            assert!(
                next < UNREGISTERED_LOGICAL as u32,
                "test harness ran out of simulated CPUs: {} test threads alive at once",
                next
            );
            next as u8
        }
    };
    // A sparse hardware id, on purpose: a dense one would pass even if the
    // map were indexing by hardware id. Derived from the logical id, so a
    // recycled slot re-registers the very same pair.
    let hw = 0x1000 + (logical as u32) * 7;
    set_test_hw_id(hw);
    set_test_published(Some(logical));
    assert!(set_logical_cpu_id(hw, logical));
    MINE.with(|m| *m.borrow_mut() = Some(CpuSlot(logical)));
    logical
}

/// Run `body` on a CPU whose interrupts start enabled, and assert it left the
/// slot exactly as it found it.
///
/// `pub(crate)` because `mcslock.rs` keeps its tests next to the lock and needs
/// the same harness: every flavour is held to the same pairing.
pub(crate) fn on_a_cpu(body: impl FnOnce()) {
    let me = this_cpu();
    crate::interrupt::intr_on_for_test();
    assert_eq!(lock_depth(), 0, "cpu {} entered the test holding locks", me);
    body();
    assert_eq!(
        lock_depth(),
        0,
        "cpu {} left the test holding {} lock(s)",
        me,
        lock_depth()
    );
    assert!(
        crate::interrupt::intr_get_for_test(),
        "cpu {} left the test with interrupts disabled",
        me
    );
}

// ── who this CPU is ──────────────────────────────────────────────────────────

#[test]
fn a_registered_cpu_answers_its_own_logical_id() {
    let me = this_cpu();
    assert_eq!(current_cpu_id(), me);
    // ...and by the long road too, which is the one the NMI path takes.
    assert_eq!(current_cpu_id_via_apic(), me);
    assert!(hardware_id_of(me).is_some());
}

#[test]
fn a_published_id_that_names_no_cpu_is_rejected_and_counted() {
    let me = this_cpu();
    let (_, before) = bogus_cpu_id_events();
    // The twin of a `swapgs` imbalance or a wild write into the per-CPU area:
    // an id that is in range, so the bounds check waves it through, and names
    // no CPU that bring-up ever registered.
    set_test_published(Some(UNREGISTERED_LOGICAL));
    // The hardware id is what no memory corruption can reach, so that is the
    // answer — not the published one, and not 0.
    assert_eq!(current_cpu_id(), me);
    let (last, after) = bogus_cpu_id_events();
    assert!(after > before);
    assert_eq!(last, UNREGISTERED_LOGICAL as u32);
    set_test_published(Some(me));
}

#[test]
fn a_cpu_nobody_registered_is_not_the_boot_cpu() {
    this_cpu();
    // A core running kernel code before bring-up gave it an id: no published
    // id it can be believed on, and a hardware id in no table.
    set_test_published(None);
    set_test_hw_id(0x7fff_0000);
    assert_eq!(current_cpu_id(), NO_CPU);
    assert_eq!(current_cpu_id_via_apic(), NO_CPU);
    // And the panic handler's question gets the conservative answer rather
    // than the boot CPU's depth.
    assert_eq!(lock_depth(), i32::MAX);
    // Put the thread back.
    let me = this_cpu();
    set_test_hw_id(hardware_id_of(me).unwrap());
    set_test_published(Some(me));
    assert_eq!(current_cpu_id(), me);
}

#[test]
fn the_ap_boot_window_answers_before_anything_is_published() {
    let me = this_cpu();
    set_test_published(None);
    let inside = with_ap_boot_logical(me, current_cpu_id);
    assert_eq!(inside, me);
    // Closing the window puts the question back to the hardware id.
    assert_eq!(current_cpu_id(), me);
    set_test_published(Some(me));
}

// ── the depth every guard keeps ──────────────────────────────────────────────

#[test]
fn a_ticket_lock_leaves_the_slot_as_it_found_it() {
    on_a_cpu(|| {
        let m = TicketMutex::new(7u32);
        {
            let g = m.lock();
            assert_eq!(*g, 7);
            assert_eq!(lock_depth(), 1);
            assert!(!crate::interrupt::intr_get_for_test());
        }
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn nested_ticket_locks_nest_the_depth() {
    on_a_cpu(|| {
        let a = TicketMutex::new(1u32);
        let b = TicketMutex::new(2u32);
        let ga = a.lock();
        let gb = b.lock();
        assert_eq!(lock_depth(), 2);
        drop(gb);
        // The inner release must NOT put interrupts back: `ga` is still held.
        assert_eq!(lock_depth(), 1);
        assert!(!crate::interrupt::intr_get_for_test());
        drop(ga);
        assert_eq!(lock_depth(), 0);
        assert!(crate::interrupt::intr_get_for_test());
    });
}

#[test]
fn a_refused_try_lock_gives_its_level_back() {
    on_a_cpu(|| {
        let m = TicketMutex::new(0u32);
        let g = m.lock();
        assert_eq!(lock_depth(), 1);
        // `try_lock` pushes before it knows whether it will get the lock.
        assert!(m.try_lock().is_none());
        assert_eq!(lock_depth(), 1, "the refused try_lock kept a level");
        drop(g);
    });
}

#[test]
fn a_spin_mutex_leaves_the_slot_as_it_found_it() {
    on_a_cpu(|| {
        let m = SpinMutex::new(3u32);
        {
            let mut g = m.lock();
            *g += 1;
            assert_eq!(lock_depth(), 1);
        }
        assert_eq!(lock_depth(), 0);
        assert_eq!(*m.lock(), 4);
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn a_refused_spin_try_lock_gives_its_level_back() {
    on_a_cpu(|| {
        let m = SpinMutex::new(0u32);
        let g = m.lock();
        assert!(m.try_lock().is_none());
        assert_eq!(lock_depth(), 1);
        drop(g);
    });
}

#[test]
fn interrupts_that_were_off_stay_off() {
    let me = this_cpu();
    crate::interrupt::intr_off_for_test();
    let m = TicketMutex::new(0u32);
    {
        let _g = m.lock();
        assert_eq!(lock_depth(), 1);
    }
    assert_eq!(lock_depth(), 0);
    assert!(
        !crate::interrupt::intr_get_for_test(),
        "cpu {} had interrupts off and the lock turned them on",
        me
    );
    crate::interrupt::intr_on_for_test();
}

// ── the rwlock, where the pairs came apart ───────────────────────────────────

#[test]
fn readers_and_writers_each_take_one_level() {
    on_a_cpu(|| {
        let l = RwLock::new(5u32);
        {
            let r1 = l.read();
            let r2 = l.read();
            assert_eq!(lock_depth(), 2);
            assert_eq!(*r1 + *r2, 10);
        }
        assert_eq!(lock_depth(), 0);
        {
            let mut w = l.write();
            *w = 6;
            assert_eq!(lock_depth(), 1);
        }
        assert_eq!(lock_depth(), 0);
        {
            let u = l.upgradeable_read();
            assert_eq!(*u, 6);
            assert_eq!(lock_depth(), 1);
        }
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn refused_rwlock_attempts_give_their_level_back() {
    on_a_cpu(|| {
        let l = RwLock::new(0u32);
        let w = l.write();
        assert!(l.try_read().is_none());
        assert!(l.try_write().is_none());
        assert!(l.try_upgradeable_read().is_none());
        assert_eq!(lock_depth(), 1);
        drop(w);
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn downgrading_a_writer_to_a_reader_keeps_one_level() {
    on_a_cpu(|| {
        let l = RwLock::new(1u32);
        let w = l.write();
        assert_eq!(lock_depth(), 1);
        // One guard becomes another guard. The lock is held throughout, so the
        // level must be held throughout too: the old guard's destructor must
        // not run, because the new guard's destructor is the one that will
        // release it.
        let r = w.downgrade();
        assert_eq!(*r, 1);
        assert_eq!(
            lock_depth(),
            1,
            "downgrade dropped a level while still holding the lock"
        );
        assert!(!crate::interrupt::intr_get_for_test());
        // Readers can join, which is the whole point of downgrading.
        let r2 = l.read();
        assert_eq!(lock_depth(), 2);
        drop(r2);
        drop(r);
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn downgrading_an_upgradeable_reader_keeps_one_level() {
    on_a_cpu(|| {
        let l = RwLock::new(2u32);
        let u = l.upgradeable_read();
        assert_eq!(lock_depth(), 1);
        let r = u.downgrade();
        assert_eq!(*r, 2);
        assert_eq!(
            lock_depth(),
            1,
            "downgrade dropped a level while still holding the lock"
        );
        // The UPGRADED bit really was released: another upgradeable reader can
        // now take it.
        let u2 = l.upgradeable_read();
        assert_eq!(lock_depth(), 2);
        drop(u2);
        drop(r);
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn downgrading_a_writer_to_an_upgradeable_reader_keeps_one_level() {
    on_a_cpu(|| {
        let l = RwLock::new(3u32);
        let w = l.write();
        let u = w.downgrade_to_upgradeable();
        assert_eq!(*u, 3);
        assert_eq!(lock_depth(), 1);
        drop(u);
        assert_eq!(lock_depth(), 0);
        // The WRITER bit is gone: a writer can take it again.
        assert!(l.try_write().is_some());
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn upgrading_a_reader_to_a_writer_keeps_one_level() {
    on_a_cpu(|| {
        let l = RwLock::new(4u32);
        let u = l.upgradeable_read();
        assert_eq!(lock_depth(), 1);
        let mut w = u.upgrade();
        *w = 5;
        assert_eq!(lock_depth(), 1);
        drop(w);
        assert_eq!(lock_depth(), 0);
        assert_eq!(*l.read(), 5);
    });
}

#[test]
fn a_refused_upgrade_hands_the_guard_back_with_its_level() {
    on_a_cpu(|| {
        let l = RwLock::new(6u32);
        let r = l.read();
        let u = l.upgradeable_read();
        assert_eq!(lock_depth(), 2);
        // A reader is still in, so the upgrade cannot take WRITER.
        let u = match u.try_upgrade() {
            Ok(_) => panic!("upgraded past a live reader"),
            Err(u) => u,
        };
        assert_eq!(lock_depth(), 2, "the refused upgrade lost a level");
        drop(u);
        drop(r);
        assert_eq!(lock_depth(), 0);
    });
}

#[test]
fn leaking_a_guard_keeps_the_lock_but_not_the_interrupt_disable() {
    on_a_cpu(|| {
        // A leaked guard holds its lock for the rest of the machine's life.
        // What it must NOT hold is this CPU's interrupt-disable level: nobody
        // is ever going to release that, so the CPU would run deaf forever —
        // including to the TLB-shootdown IPI a peer is spin-waiting on.
        let a = RwLock::new(1u32);
        let _ = crate::rwlock::RwLockReadGuard::leak(a.read());
        assert_eq!(lock_depth(), 0);

        let b = RwLock::new(2u32);
        let _ = crate::rwlock::RwLockWriteGuard::leak(b.write());
        assert_eq!(lock_depth(), 0);

        let c = RwLock::new(3u32);
        let _ = crate::rwlock::RwLockUpgradableGuard::leak(c.upgradeable_read());
        assert_eq!(
            lock_depth(),
            0,
            "the leaked upgradeable guard kept interrupts disabled for good"
        );
        assert!(crate::interrupt::intr_get_for_test());
    });
}

#[test]
fn a_whole_rwlock_sequence_comes_back_to_zero() {
    on_a_cpu(|| {
        let l = RwLock::new(0u32);
        for _ in 0..4 {
            let u = l.upgradeable_read();
            let mut w = u.upgrade();
            *w += 1;
            let r = w.downgrade();
            let r2 = l.read();
            assert_eq!(lock_depth(), 2);
            drop(r2);
            drop(r);
            assert_eq!(lock_depth(), 0);
        }
        assert_eq!(*l.read(), 4);
    });
}

// ── waiting without going deaf ───────────────────────────────────────────────
//
// Every acquire in this crate brackets itself with `push_off`, so a waiter
// that got here from a caller who already had interrupts off spins with them
// still off — deaf to the TLB-shootdown IPI. A peer doing the shootdown
// spin-waits for that ack while holding the VMAR lock, so a silent waiter
// wedges it and everything queued behind it. The ticket lock and the spin
// mutex each drain their own queue every 512 turns; `rwlock.rs` had four
// waiting loops and not one of them did.
//
// A second thread here is a second CPU (see `this_cpu`), so the waiting in
// these is real: this CPU holds a guard that blocks the other's acquire, and
// only lets go once the waiter has been seen to drain its queue.

static PUMPS: AtomicU32 = AtomicU32::new(0);

std::thread_local! {
    /// Set on the one thread whose acquire a test is measuring.
    static IS_THE_WAITER: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// The pump is process-wide: once armed it fires for every spinner anywhere in
/// the process, and the four tests below assert only that it fired *at all*.
/// So a pump from some other test's contending peer read as proof that the
/// loop under test drains its queue -- a discipline removed from that loop
/// would have gone on passing. The shared hook lock cannot help: it keeps
/// another test from arming the pump, not from spinning into it.
///
/// Counting only the waiter's own turns closes that: the acquire under test is
/// the only thing that can move this number.
fn count_pump() {
    if IS_THE_WAITER.with(|c| c.get()) {
        PUMPS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Run the acquire whose waiting is under test. Only turns taken inside here
/// count towards [`a_pump_was_seen`].
fn as_the_waiter<R>(f: impl FnOnce() -> R) -> R {
    IS_THE_WAITER.with(|c| c.set(true));
    let r = f();
    IS_THE_WAITER.with(|c| c.set(false));
    r
}

/// Arm the recording pump. Returns the shared hook lock, so `rwlock.rs`'s own
/// tests cannot swap the recorder out from under us.
fn armed() -> std::sync::MutexGuard<'static, ()> {
    let guard = crate::deadlock::hook_test_lock();
    PUMPS.store(0, Ordering::SeqCst);
    crate::deadlock::set_spin_pump(count_pump);
    guard
}

/// Wait for the other CPU to go round its waiting loop enough times to drain
/// its queue once. The deadline is a hang guard, not the assertion: a working
/// discipline gets there in microseconds, and a broken one has to fail the
/// test rather than stop the suite.
fn a_pump_was_seen() -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while PUMPS.load(Ordering::SeqCst) == 0 {
        if std::time::Instant::now() > deadline {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

#[test]
fn a_reader_waiting_on_a_writer_drains_its_own_shootdown_queue() {
    static LOCK: RwLock<u32> = RwLock::new(7);
    let _g = armed();
    on_a_cpu(|| {
        let w = LOCK.write();
        let other = std::thread::spawn(|| {
            on_a_cpu(|| as_the_waiter(|| assert_eq!(*LOCK.read(), 7)));
        });
        let pumped = a_pump_was_seen();
        drop(w);
        other.join().unwrap();
        assert!(pumped, "a reader waited on a writer in silence");
    });
}

#[test]
fn a_writer_waiting_on_a_reader_drains_its_own_shootdown_queue() {
    static LOCK: RwLock<u32> = RwLock::new(1);
    let _g = armed();
    on_a_cpu(|| {
        let r = LOCK.read();
        let other = std::thread::spawn(|| {
            on_a_cpu(|| as_the_waiter(|| *LOCK.write() = 2));
        });
        let pumped = a_pump_was_seen();
        drop(r);
        other.join().unwrap();
        assert!(pumped, "a writer waited on a reader in silence");
        assert_eq!(*LOCK.read(), 2);
    });
}

#[test]
fn an_upgradeable_reader_waiting_on_another_drains_its_own_shootdown_queue() {
    static LOCK: RwLock<u32> = RwLock::new(3);
    let _g = armed();
    on_a_cpu(|| {
        // This loop had neither half of the discipline: no pump and no
        // deadlock report, so a wedge here left the console empty too.
        let up = LOCK.upgradeable_read();
        let other = std::thread::spawn(|| {
            on_a_cpu(|| as_the_waiter(|| assert_eq!(*LOCK.upgradeable_read(), 3)));
        });
        let pumped = a_pump_was_seen();
        drop(up);
        other.join().unwrap();
        assert!(pumped, "an upgradeable reader waited in silence");
    });
}

#[test]
fn an_upgrade_waiting_on_a_reader_drains_its_own_shootdown_queue() {
    static LOCK: RwLock<u32> = RwLock::new(4);
    static READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    let _g = armed();
    READY.store(false, Ordering::SeqCst);
    on_a_cpu(|| {
        // An upgradeable guard may be taken while readers exist; it is the
        // upgrade that has to wait for the last of them to leave. That is the
        // fourth loop, and the other one that spun in silence.
        let r = LOCK.read();
        let other = std::thread::spawn(|| {
            on_a_cpu(|| {
                as_the_waiter(|| {
                    let up = LOCK.upgradeable_read();
                    READY.store(true, Ordering::SeqCst);
                    *up.upgrade() = 5;
                })
            });
        });
        while !READY.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // Only the upgrade's own waiting counts: the acquire above took the
        // guard without spinning, but say so rather than rely on it.
        PUMPS.store(0, Ordering::SeqCst);
        let pumped = a_pump_was_seen();
        drop(r);
        other.join().unwrap();
        assert!(pumped, "an upgrade waited on a reader in silence");
        assert_eq!(*LOCK.read(), 5);
    });
}

// ── the rwlock's own state word ──────────────────────────────────────────────
//
// Everything above tests what the guards do to this CPU's interrupt-disable
// depth. None of it tests what they do to the lock, and the lock is one
// `AtomicUsize` carrying three things at once: a WRITER bit, an UPGRADED bit,
// and a reader count in every bit above them. The three are written by nine
// different call sites — three acquires, three drops, two downgrades and an
// upgrade — and one of them wiped the count.

use crate::rwlock::{READER, UPGRADED, WRITER};

#[test]
fn a_free_lock_is_a_zero_word() {
    on_a_cpu(|| {
        let l = RwLock::new(0u32);
        assert_eq!(l.raw_state(), 0);
        assert_eq!(l.reader_count(), 0);
        assert_eq!(l.writer_count(), 0);
    });
}

#[test]
fn a_writer_excludes_everyone_and_says_so() {
    on_a_cpu(|| {
        let l = RwLock::new(1u32);
        let w = l.write();
        assert_eq!(l.raw_state(), WRITER);
        assert_eq!(l.writer_count(), 1);
        assert_eq!(l.reader_count(), 0);
        assert!(l.try_read().is_none(), "a reader got in under a writer");
        assert!(l.try_write().is_none(), "two writers at once");
        assert!(l.try_upgradeable_read().is_none());
        drop(w);
        assert_eq!(l.raw_state(), 0, "the writer owes both bits back");
    });
}

#[test]
fn readers_share_and_are_counted_one_by_one() {
    on_a_cpu(|| {
        let l = RwLock::new(2u32);
        let a = l.read();
        let b = l.read();
        assert_eq!(l.raw_state(), 2 * READER);
        assert_eq!(l.reader_count(), 2);
        assert_eq!(l.writer_count(), 0);
        assert!(l.try_write().is_none(), "a writer got in under two readers");
        drop(a);
        assert_eq!(l.reader_count(), 1);
        drop(b);
        assert_eq!(l.raw_state(), 0);
        assert!(l.try_write().is_some(), "the last reader left it held");
    });
}

#[test]
fn an_upgradeable_reader_shuts_the_door_behind_it() {
    on_a_cpu(|| {
        let l = RwLock::new(3u32);
        let r = l.read();
        let u = l.upgradeable_read();
        // It may be taken alongside existing readers, and it counts as one.
        assert_eq!(l.raw_state(), READER | UPGRADED);
        assert_eq!(l.reader_count(), 2, "the upgradeable reader is a reader");
        // But no NEW reader joins, which is what keeps the upgrade from
        // starving behind an endless stream of them.
        assert!(l.try_read().is_none());
        assert!(l.try_upgradeable_read().is_none(), "two upgraders at once");
        assert!(l.try_write().is_none());
        drop(u);
        assert_eq!(l.raw_state(), READER);
        assert!(l.try_read().is_some(), "the door stayed shut");
        drop(r);
    });
}

#[test]
fn an_upgrade_waits_for_the_last_reader_and_then_owns_the_lock_alone() {
    on_a_cpu(|| {
        let l = RwLock::new(4u32);
        let r = l.read();
        let u = l.upgradeable_read();
        let u = match u.try_upgrade() {
            Ok(_) => panic!("upgraded while a reader still held the lock"),
            Err(u) => u,
        };
        drop(r);
        assert_eq!(l.raw_state(), UPGRADED, "only the upgradeable guard left");
        let mut w = u.try_upgrade().expect("the last reader is gone");
        assert_eq!(l.raw_state(), WRITER, "an upgrade is a writer, not both");
        *w = 5;
        drop(w);
        assert_eq!(l.raw_state(), 0);
        assert_eq!(*l.read(), 5);
    });
}

#[test]
fn a_refused_upgrade_keeps_the_lock_exactly_as_it_was() {
    on_a_cpu(|| {
        let l = RwLock::new(6u32);
        let r = l.read();
        let u = l.upgradeable_read();
        let before = l.raw_state();
        let u = u.try_upgrade().err().expect("a reader still holds it");
        assert_eq!(
            l.raw_state(),
            before,
            "a failed upgrade must give the word back untouched, or the \
             guard it hands back is no longer describing the lock"
        );
        drop(u);
        drop(r);
        assert_eq!(l.raw_state(), 0);
    });
}

#[test]
fn a_writer_that_downgrades_keeps_the_lock_the_whole_way() {
    on_a_cpu(|| {
        let l = RwLock::new(7u32);
        let mut w = l.write();
        *w = 8;
        let r = w.downgrade();
        assert_eq!(l.raw_state(), READER, "the writer bit had to go, alone");
        assert_eq!(*r, 8, "the downgrade read a value the writer never wrote");
        assert!(
            l.try_read().is_some(),
            "a downgrade opens the door to readers"
        );
        assert!(l.try_write().is_none(), "and keeps it shut to writers");
        drop(r);
        assert_eq!(l.raw_state(), 0);
    });
}

#[test]
fn an_upgradeable_reader_that_downgrades_becomes_an_ordinary_one() {
    on_a_cpu(|| {
        let l = RwLock::new(9u32);
        let u = l.upgradeable_read();
        let r = u.downgrade();
        assert_eq!(l.raw_state(), READER, "the upgraded bit had to go, alone");
        assert!(l.try_read().is_some(), "the door has to open again");
        assert!(l.try_upgradeable_read().is_some());
        drop(r);
    });
}

#[test]
fn a_writer_that_downgrades_to_upgradeable_keeps_a_reader_in_flight() {
    // The bug: this downgrade was one blind `store(UPGRADED)` — the whole
    // word — so it wiped the reader count in the bits above the two flags.
    //
    // Holding WRITER does not mean that count is zero. `try_read` adds its
    // READER *before* it looks at the flags and takes it back only on the
    // next line, so a lock held for writing reads `WRITER | n*READER` for as
    // long as `n` readers are in that window. The store ate those additions;
    // their `fetch_sub` then underflowed the word, and once this guard
    // dropped the lock read `0xffff_ffff_ffff_fffc`: no WRITER, no UPGRADED,
    // and a reader count of 2^62. `try_write` compare-exchanges from zero, so
    // from that instant no writer could ever take this lock again, and no
    // reader either.
    on_a_cpu(|| {
        let l = RwLock::new(10u32);
        let w = l.write();
        // Another CPU is halfway through `try_read`.
        l.add_reader_bit_as_try_read_does();
        let u = w.downgrade_to_upgradeable();
        // …and now runs the line it was about to run.
        l.take_reader_bit_back();
        assert_eq!(
            l.raw_state(),
            UPGRADED,
            "the reader took back a count this downgrade had already wiped"
        );
        drop(u);
        assert_eq!(l.raw_state(), 0, "the lock has to be free again");
        assert!(
            l.try_write().is_some(),
            "and acquirable — this is the state the lock never came back from"
        );
    });
}

#[test]
fn a_reader_in_flight_survives_every_release_that_can_race_it() {
    // The same window, against the other three sites that rewrite the word
    // while the lock is held. None of them may use a blind store either.
    on_a_cpu(|| {
        let l = RwLock::new(11u32);

        let w = l.write();
        l.add_reader_bit_as_try_read_does();
        drop(w);
        l.take_reader_bit_back();
        assert_eq!(l.raw_state(), 0, "the write guard's drop ate a count");

        let w = l.write();
        l.add_reader_bit_as_try_read_does();
        let r = w.downgrade();
        l.take_reader_bit_back();
        assert_eq!(l.raw_state(), READER, "the downgrade to a reader ate one");
        drop(r);

        let u = l.upgradeable_read();
        l.add_reader_bit_as_try_read_does();
        let r = u.downgrade();
        l.take_reader_bit_back();
        assert_eq!(l.raw_state(), READER, "the upgradeable downgrade ate one");
        drop(r);
        assert_eq!(l.raw_state(), 0);
    });
}

#[test]
fn a_refused_try_read_leaves_the_word_as_it_found_it() {
    on_a_cpu(|| {
        let l = RwLock::new(12u32);
        let w = l.write();
        assert!(l.try_read().is_none());
        assert!(l.try_read().is_none());
        assert_eq!(
            l.raw_state(),
            WRITER,
            "a refused reader has to take its own count back"
        );
        drop(w);
        assert_eq!(l.raw_state(), 0);
    });
}

#[test]
fn a_refused_upgradeable_read_leaves_its_bit_for_the_holder_to_clear() {
    // The one acquire that does NOT tidy up after itself, and on purpose: the
    // bit it set is indistinguishable from the holder's own, so taking it back
    // would release somebody else's lock. The holder clears both bits.
    on_a_cpu(|| {
        let l = RwLock::new(13u32);
        let w = l.write();
        assert!(l.try_upgradeable_read().is_none());
        assert_eq!(l.raw_state(), WRITER | UPGRADED);
        drop(w);
        assert_eq!(
            l.raw_state(),
            0,
            "the writer owes back the upgraded bit a refused acquire left"
        );
        assert!(l.try_write().is_some());
    });
}

#[test]
fn the_counts_read_the_word_the_way_the_protocol_writes_it() {
    on_a_cpu(|| {
        let l = RwLock::new(14u32);
        let a = l.read();
        let b = l.read();
        let u = l.upgradeable_read();
        // Three readers' worth, and the flags must not be counted as one of
        // them: `reader_count` divides by READER, which is 4, so a stray
        // WRITER or UPGRADED bit would have to be added by hand — and is.
        assert_eq!(l.reader_count(), 3);
        assert_eq!(l.writer_count(), 0);
        drop(u);
        drop(b);
        drop(a);
        let w = l.write();
        assert_eq!(l.writer_count(), 1);
        assert_eq!(l.reader_count(), 0, "a writer is not a reader");
        drop(w);
    });
}

// ── who holds a lock, and who is asking ──────────────────────────────────────
//
// `HeldByCurrentCpu` is not a diagnostic. The global heap allocator asks it
// before every heap lock it takes and, told "you already hold this", refuses
// the allocation rather than deadlock against itself. So the answer has to be
// about *this* CPU and no other.

use crate::spin::SpinMutex as Spin;
use crate::ticket::TicketMutex as Ticket;

/// Make this test thread a core that bring-up never gave a dense id: no
/// published id worth believing, and a hardware id in no table. Returns the
/// id it had, to hand back to [`back_from_nobody`].
fn become_nobody() -> u8 {
    let me = this_cpu();
    set_test_published(None);
    set_test_hw_id(0x7fff_0000);
    assert_eq!(current_cpu_id(), NO_CPU);
    me
}

fn back_from_nobody(me: u8) {
    set_test_hw_id(hardware_id_of(me).unwrap());
    set_test_published(Some(me));
    assert_eq!(current_cpu_id(), me);
}

#[test]
fn a_lock_nobody_holds_is_held_by_nobody() {
    on_a_cpu(|| {
        let t = Ticket::new(0u32);
        assert!(!t.holder_is_current_cpu());
        let s = Spin::new(0u32);
        assert!(!s.holder_is_current_cpu());
    });
}

#[test]
fn a_lock_this_cpu_took_says_so_and_forgets_on_release() {
    on_a_cpu(|| {
        let t = Ticket::new(0u32);
        let g = t.try_lock().expect("free");
        assert!(t.holder_is_current_cpu());
        drop(g);
        assert!(
            !t.holder_is_current_cpu(),
            "the record has to be cleared before the lock is handed over, or \
             the next waiter's banner names an owner that already left"
        );

        let s = Spin::new(0u32);
        let g = s.try_lock().expect("free");
        assert!(s.holder_is_current_cpu());
        drop(g);
        assert!(!s.holder_is_current_cpu());
    });
}

#[test]
fn a_core_with_no_id_is_stopped_before_it_can_take_a_lock() {
    // The guarantee the two locks' `(lc >> 32) as u32 == current_cpu_id()`
    // quietly rests on, said out loud.
    //
    // `current_cpu_id` answers `NO_CPU` for **every** core that never got a
    // dense logical id — one past `MAX_CORE_NUM`, or an AP whose id bring-up
    // took back — so two of them compared for identity would read as one CPU.
    // What keeps that out of the lock records is `mycpu`: it refuses to turn
    // "we do not know which CPU this is" into slot 0, which is the boot CPU's,
    // and stops instead. So an id-less core cannot acquire, cannot record
    // itself, and is told it holds nothing.
    static LOCK: Ticket<u32> = Ticket::new(0);
    let me = become_nobody();
    assert!(
        !LOCK.holder_is_current_cpu(),
        "a core that cannot take a lock cannot be holding one"
    );
    let stopped = std::panic::catch_unwind(|| {
        let _g = LOCK.try_lock();
    });
    assert!(
        stopped.is_err(),
        "an id-less core got a lock slot; whichever CPU owns that slot now \
         shares its interrupt-disable depth with a core it knows nothing about"
    );
    back_from_nobody(me);
    // And the lock is untouched: the stop came before the ticket was drawn.
    assert!(!LOCK.is_locked());
}

#[test]
fn a_core_with_an_id_is_never_confused_with_one_without() {
    // The other half: an id-less core asking about a lock a real CPU holds
    // must be told it holds nothing. That is the answer that makes it wait,
    // and waiting is right whoever the holder turns out to be — whereas being
    // told "you already hold this" is what makes the heap allocator refuse
    // the allocation outright. It holds only because `NO_CPU` is a value no
    // real CPU has; the day `current_cpu_id` answers 0 for an unknown core
    // instead, every such core becomes the boot CPU's twin.
    static LOCK: Ticket<u32> = Ticket::new(0);
    let me = this_cpu();
    let g = LOCK.try_lock().expect("free");
    let other = std::thread::spawn(|| {
        let mine = become_nobody();
        assert!(!LOCK.holder_is_current_cpu());
        back_from_nobody(mine);
    });
    other.join().unwrap();
    assert!(LOCK.holder_is_current_cpu(), "it is still ours");
    drop(g);
    assert_eq!(current_cpu_id(), me);
}

// ── the ticket the lock hands out ────────────────────────────────────────────

#[test]
fn a_ticket_lock_serves_in_the_order_it_handed_the_tickets_out() {
    // What a ticket lock is *for*: an unfair mutex can starve a waiter
    // indefinitely, and this one cannot. Three CPUs queue behind a holder in a
    // known order and must come out in that order — and, while the holder has
    // it, none of them comes out at all.
    static LOCK: Ticket<std::vec::Vec<u8>> = Ticket::new(std::vec::Vec::new());
    static ENTERED: AtomicU32 = AtomicU32::new(0);
    on_a_cpu(|| {
        LOCK.lock().clear();
        ENTERED.store(0, Ordering::SeqCst);
        let held = LOCK.lock();
        let mut joins = std::vec::Vec::new();
        for who in 1u8..=3 {
            // Each thread draws its ticket before the next one is spawned, so
            // the queue order is the spawn order and not a coin toss.
            let before = LOCK.next_ticket_for_test();
            let t = std::thread::spawn(move || {
                on_a_cpu(|| {
                    let mut g = LOCK.lock();
                    // Counted outside the data, so the check below is a read
                    // of an atomic and not a race with whoever got in.
                    ENTERED.fetch_add(1, Ordering::SeqCst);
                    g.push(who);
                })
            });
            while LOCK.next_ticket_for_test() == before {
                std::thread::yield_now();
            }
            joins.push(t);
        }
        assert_eq!(
            ENTERED.load(Ordering::SeqCst),
            0,
            "three CPUs hold a ticket each and one of them is already inside: \
             the queue is decoration and the lock excludes nobody"
        );
        drop(held);
        for t in joins {
            t.join().unwrap();
        }
        assert_eq!(
            *LOCK.lock(),
            std::vec![1u8, 2, 3],
            "a ticket lock that serves out of order is an unfair mutex with \
             extra steps"
        );
    });
}

#[test]
fn a_try_lock_takes_a_ticket_and_gives_the_queue_back_when_it_fails() {
    on_a_cpu(|| {
        let l = Ticket::new(0u32);
        assert!(!l.is_locked());
        let before = l.next_ticket_for_test();
        let g = l.try_lock().expect("a free lock cannot refuse");
        assert!(l.is_locked());
        assert_eq!(
            l.next_ticket_for_test(),
            before + 1,
            "a successful try_lock draws a ticket like any other acquire"
        );
        assert!(l.try_lock().is_none(), "it is taken");
        assert_eq!(
            l.next_ticket_for_test(),
            before + 1,
            "a refused try_lock must NOT draw one: a ticket nobody will ever \
             be served wedges every acquire behind it, for good"
        );
        drop(g);
        assert!(!l.is_locked());
        assert!(l.try_lock().is_some());
    });
}

#[test]
fn releasing_serves_exactly_the_next_ticket() {
    on_a_cpu(|| {
        let l = Ticket::new(0u32);
        let start = l.next_serving_for_test();
        let g = l.lock();
        assert_eq!(l.next_serving_for_test(), start, "we are the one served");
        drop(g);
        assert_eq!(
            l.next_serving_for_test(),
            start + 1,
            "one release, one ticket: skipping one strands the waiter holding \
             it and over-serving hands the lock to two CPUs at once"
        );
    });
}

// ── the turn a waiter takes and nobody counted ───────────────────────────────

#[test]
fn a_spin_waiter_that_loses_an_acquire_counts_the_turn_it_lost() {
    // `SpinMutex::lock` waits in two loops, not one: the outer one is the
    // acquire attempt itself, the inner one the read-only wait for the lock to
    // look free. Only the second used to count turns, and the counter is what
    // drives both the 512-turn shootdown pump and the stuck-lock report.
    //
    // So consider the waiter that loses its attempt and then finds the lock
    // already free again — the handover race, which on an unfair mutex is what
    // the most-starved CPU keeps hitting. Its inner loop never runs a single
    // iteration, so it goes round and round the outer one with the counter
    // stuck at zero: it never pumps, so it is an ack black hole for as long as
    // it spins there, and it never crosses the threshold, so the wedge it is
    // part of is never named.
    //
    // The pair the ledger hands back states the rule without reaching inside
    // the loop: an acquire that lost an attempt waited, and a wait has to be
    // counted. Two CPUs hammering a critical section of nothing is the
    // shortest way to make the race happen over and over.
    static LOCK: Spin<u64> = Spin::new(0);
    on_a_cpu(|| {
        let stop = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let noise = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                on_a_cpu(|| {
                    while !stop.load(Ordering::Relaxed) {
                        *LOCK.lock() += 1;
                    }
                })
            })
        };
        let mut contended = 0u32;
        for _ in 0..200_000 {
            *LOCK.lock() += 1;
            let (lost, turns) = crate::spin::turn_ledger::last();
            if lost == 0 {
                continue;
            }
            contended += 1;
            assert!(
                turns > 0,
                "an acquire lost {} attempt(s) and counted no waiting at \
                 all: that waiter drained no shootdown queue and would never \
                 be reported stuck, however long it span",
                lost
            );
        }
        stop.store(true, Ordering::Relaxed);
        noise.join().unwrap();
        assert!(
            contended > 0,
            "the two CPUs never collided, so this proved nothing"
        );
    });
}
