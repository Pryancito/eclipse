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
    use core::cell::Cell;
    std::thread_local! {
        static MINE: Cell<Option<u8>> = const { Cell::new(None) };
    }
    MINE.with(|m| {
        if let Some(id) = m.get() {
            return id;
        }
        let logical = NEXT_LOGICAL.fetch_add(1, Ordering::Relaxed);
        assert!(
            logical < UNREGISTERED_LOGICAL as u32,
            "test harness ran out of simulated CPUs"
        );
        let logical = logical as u8;
        // A sparse hardware id, on purpose: a dense one would pass even if the
        // map were indexing by hardware id.
        let hw = 0x1000 + (logical as u32) * 7;
        set_test_hw_id(hw);
        set_test_published(Some(logical));
        assert!(set_logical_cpu_id(hw, logical));
        m.set(Some(logical));
        logical
    })
}

/// Run `body` on a CPU whose interrupts start enabled, and assert it left the
/// slot exactly as it found it.
fn on_a_cpu(body: impl FnOnce()) {
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
