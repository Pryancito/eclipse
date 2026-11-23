use super::*;

const MIB: usize = 1 << 20;

fn heap(used: usize, total: usize, reach: usize) -> Heap<impl FnOnce() -> usize> {
    Heap {
        used,
        total,
        reach: move || reach,
    }
}

/// The reach must not be asked on the fast path: on x86_64 it sums a counter
/// of every CPU, and the fast path is every queued byte.
fn heap_unasked(used: usize, total: usize) -> Heap<impl FnOnce() -> usize> {
    Heap {
        used,
        total,
        reach: || panic!("the fast path asked for the heap's reach"),
    }
}

#[test]
fn far_from_both_edges_the_reach_is_not_asked() {
    assert_eq!(admit(MIB, MIB, 0, heap_unasked(10 * MIB, 64 * MIB)), Ok(()));
    assert_eq!(
        admit(MIB, MIB, 2 * MIB, heap_unasked(10 * MIB, 64 * MIB)),
        Ok(())
    );
}

#[test]
fn the_default_share_is_a_quarter_of_the_reach_not_of_the_heap_now() {
    // 64 MiB heap now, able to grow to 2 GiB: 100 MiB is under a quarter of
    // the reach though over a quarter of the heap.
    assert_eq!(
        admit(100 * MIB, MIB, 0, heap(10 * MIB, 64 * MIB, 2048 * MIB)),
        Ok(())
    );
    assert_eq!(
        admit(513 * MIB, MIB, 0, heap(10 * MIB, 64 * MIB, 2048 * MIB)),
        Err(Refusal::Quota)
    );
}

#[test]
fn an_explicit_limit_replaces_the_share() {
    assert_eq!(
        admit(3 * MIB, MIB, 2 * MIB, heap(0, 64 * MIB, 2048 * MIB)),
        Err(Refusal::Quota)
    );
    assert_eq!(
        admit(900 * MIB, MIB, 1024 * MIB, heap(0, 64 * MIB, 2048 * MIB)),
        Ok(())
    );
}

#[test]
fn nobody_charged_takes_the_last_eighth() {
    // Reach 64 MiB: the reserve starts at 56 MiB.
    assert_eq!(
        admit(MIB, MIB, 0, heap(55 * MIB, 64 * MIB, 64 * MIB)),
        Ok(())
    );
    assert_eq!(
        admit(MIB, MIB, 0, heap(56 * MIB, 64 * MIB, 64 * MIB)),
        Err(Refusal::Reserve)
    );
    // An explicit limit does not reach into it either.
    assert_eq!(
        admit(MIB, MIB, usize::MAX, heap(56 * MIB, 64 * MIB, 64 * MIB)),
        Err(Refusal::Reserve)
    );
}

#[test]
fn a_reach_below_the_heap_counts_as_the_heap() {
    assert_eq!(admit(MIB, MIB, 0, heap(10 * MIB, 64 * MIB, 0)), Ok(()));
}

#[test]
fn a_heap_of_unknown_size_applies_only_an_explicit_limit() {
    assert_eq!(admit(usize::MAX / 2, 1, 0, heap_unasked(0, 0)), Ok(()));
    assert_eq!(admit(3, 1, 2, heap_unasked(0, 0)), Err(Refusal::Quota));
}

#[test]
fn a_refused_charge_charges_nothing_and_is_counted() {
    let acct = KmemAccount::with_limit(0);
    assert_eq!(acct.try_charge_on(MIB, heap(0, 64 * MIB, 64 * MIB)), Ok(()));
    assert_eq!(
        acct.try_charge_on(MIB, heap(60 * MIB, 64 * MIB, 64 * MIB)),
        Err(Refusal::Reserve)
    );
    assert_eq!(
        acct.try_charge_on(16 * MIB, heap(0, 64 * MIB, 64 * MIB)),
        Err(Refusal::Quota)
    );
    assert_eq!(acct.charged(), MIB);
    assert_eq!(acct.peak(), MIB);
    assert_eq!(acct.refused(), (1, 1));
    acct.uncharge(MIB);
    assert_eq!(acct.charged(), 0);
    assert_eq!(acct.peak(), MIB);
}

#[test]
fn a_fork_starts_empty_with_the_parents_limit() {
    let parent = KmemAccount::with_limit(7 * MIB);
    parent.try_charge(MIB).unwrap();
    let child = parent.forked();
    assert_eq!(child.charged(), 0);
    assert_eq!(child.limit_setting(), 7 * MIB);
}

#[test]
fn bytes_are_given_back_to_whoever_wrote_them() {
    let a = KmemAccount::with_limit(0);
    let b = KmemAccount::with_limit(0);
    let mut q = ChargedBytes::new();
    q.charge_to(Some(a.clone()), 100).unwrap();
    q.charge_to(Some(a.clone()), 50).unwrap();
    q.charge_to(Some(b.clone()), 10).unwrap();
    q.charge_to(None, 5).unwrap();
    q.charge_to(Some(a.clone()), 1).unwrap();
    // Two runs of `a`, one of `b`: each run's bookkeeping is charged with it.
    assert_eq!(a.charged(), 151 + 2 * RUN_COST);
    assert_eq!(b.charged(), 10 + RUN_COST);
    // Part of the first run.
    q.release(120);
    assert_eq!(a.charged(), 31 + 2 * RUN_COST);
    // The rest of it, all of `b`'s and two of the kernel's five.
    q.release(30 + 10 + 2);
    assert_eq!(a.charged(), 1 + RUN_COST);
    assert_eq!(b.charged(), 0);
    // The kernel's last three and `a`'s last byte.
    q.release(4);
    assert_eq!(a.charged(), RUN_COST);
    assert_eq!(q.charged(), RUN_COST);
}

#[test]
fn a_queue_freed_with_bytes_in_it_gives_them_back() {
    let a = KmemAccount::with_limit(0);
    {
        let mut q = ChargedBytes::new();
        q.charge_to(Some(a.clone()), 4096).unwrap();
        assert_eq!(q.charged(), 4096 + RUN_COST);
    }
    assert_eq!(a.charged(), 0);
}

#[test]
fn a_write_over_the_quota_queues_nothing() {
    let a = KmemAccount::with_limit(1000);
    let mut q = ChargedBytes::new();
    q.charge_to(Some(a.clone()), 500).unwrap();
    assert_eq!(q.charge_to(Some(a.clone()), 600), Err(Refusal::Quota));
    // A new run's bookkeeping counts too.
    let b = KmemAccount::with_limit(100);
    assert_eq!(q.charge_to(Some(b.clone()), 100), Err(Refusal::Quota));
    assert_eq!(b.charged(), 0);
    assert_eq!(a.charged(), 500 + RUN_COST);
    q.release(500);
    assert_eq!(a.charged(), RUN_COST);
}

#[test]
fn a_drained_queue_keeps_its_last_run_for_the_next_message() {
    let a = KmemAccount::with_limit(0);
    let b = KmemAccount::with_limit(0);
    let mut q = ChargedBytes::new();
    for _ in 0..3 {
        q.charge_to(Some(a.clone()), 64).unwrap();
        q.release(64);
    }
    // One run all along: its bookkeeping was charged once.
    assert_eq!(a.charged(), RUN_COST);
    // Somebody else's bytes behind the empty run; the empty one goes with
    // the next release.
    q.charge_to(Some(b.clone()), 8).unwrap();
    q.release(8);
    assert_eq!(a.charged(), 0);
    assert_eq!(b.charged(), RUN_COST);
    drop(q);
    assert_eq!(b.charged(), 0);
}

#[test]
fn releasing_more_than_was_charged_stops_at_the_end() {
    let a = KmemAccount::with_limit(0);
    let mut q = ChargedBytes::new();
    q.charge_to(Some(a.clone()), 10).unwrap();
    q.release(1000);
    assert_eq!(a.charged(), RUN_COST);
    q.release(1);
}

#[test]
fn a_charge_is_all_or_nothing_and_goes_back_when_dropped() {
    let a = KmemAccount::with_limit(100);
    let c = Charge::try_to(Some(a.clone()), 60).unwrap();
    assert_eq!((c.bytes(), a.charged()), (60, 60));
    assert_eq!(
        Charge::try_to(Some(a.clone()), 41).unwrap_err(),
        Refusal::Quota
    );
    assert_eq!(a.charged(), 60, "a refused charge takes nothing");
    let kernel = Charge::try_to(None, 1 << 40).unwrap();
    assert_eq!(kernel.bytes(), 0, "the kernel's own work is not charged");
    drop(c);
    assert_eq!(a.charged(), 0);
}

#[test]
fn inotify_watches_stop_at_the_limit() {
    let a = KmemAccount::with_limit(0);
    a.inotify_watches_get(2).unwrap();
    a.inotify_watches_get(2).unwrap();
    assert_eq!(a.inotify_watches_get(2), Err(crate::error::LxError::ENOSPC));
    assert_eq!(a.inotify_watches(), 2);
    a.inotify_watches_put(2);
    assert_eq!(a.inotify_watches(), 0);
}

#[test]
fn descriptors_in_flight_stop_past_the_limit_and_come_back_when_received() {
    use crate::error::LxError;
    let a = KmemAccount::with_limit(0);
    let mut first = InFlight::try_to(Some(a.clone()), 3, 4).unwrap();
    assert_eq!(a.inflight(), 3);
    assert_eq!(a.charged(), 3 * INFLIGHT_FD_COST + INFLIGHT_BATCH_COST);
    // At the limit is not over it: like Linux, the batch that starts there
    // goes, and the one after is refused.
    let second = InFlight::try_to(Some(a.clone()), 2, 4).unwrap();
    assert_eq!(a.inflight(), 5);
    assert_eq!(
        InFlight::try_to(Some(a.clone()), 1, 4).unwrap_err(),
        LxError::ETOOMANYREFS
    );
    assert_eq!(a.inflight(), 5, "a refused batch counts nothing");
    first.received(2);
    assert_eq!((first.fds(), a.inflight()), (1, 3));
    drop((first, second));
    assert_eq!((a.inflight(), a.charged()), (0, 0));
    // Over the memory quota is ENOMEM, and counts nothing either.
    let tight = KmemAccount::with_limit(INFLIGHT_BATCH_COST);
    assert_eq!(
        InFlight::try_to(Some(tight.clone()), 1, 100).unwrap_err(),
        LxError::ENOMEM
    );
    assert_eq!((tight.inflight(), tight.charged()), (0, 0));
}

/// The queues themselves, written by a process whose limit is `limit`.
mod queues {
    extern crate std;

    use super::*;
    use crate::fs::Pipe;
    use crate::net::{Socket, UnixSocketState};
    use crate::process::{LinuxProcess, ProcessExt};
    use crate::thread::ThreadExt;
    use rcore_fs::vfs::{FsError, INode};
    use rcore_fs_ramfs::RamFS;
    use zircon_object::task::{Process, Thread, ROOT_JOB};

    /// `f` run as a thread of a process whose kernel memory limit is
    /// `limit`, handed that process's account.
    fn as_a_process_limited_to<R>(
        pid: u64,
        limit: usize,
        f: impl FnOnce(&Arc<KmemAccount>) -> R,
    ) -> R {
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            pid,
            "kmem",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let thread = Thread::create_linux(&proc).unwrap();
        let acct = proc.linux().kmem().clone();
        // The thread itself is charged to the process: the limit is on top
        // of it, and `charged` leaves it out.
        assert_eq!(acct.charged(), crate::thread::THREAD_KMEM_COST);
        acct.set_limit(if limit == 0 {
            0
        } else {
            limit + crate::thread::THREAD_KMEM_COST
        });
        let r = async_std::task::block_on(async {
            kernel_hal::thread::set_current_thread(Some(thread.clone()));
            let r = f(&acct);
            kernel_hal::thread::set_current_thread(None);
            r
        });
        thread.terminate_abandoned();
        r
    }

    /// What `acct` holds besides the thread of `as_a_process_limited_to`.
    fn charged(acct: &KmemAccount) -> usize {
        acct.charged() - crate::thread::THREAD_KMEM_COST
    }

    #[test]
    fn a_thread_is_charged_to_its_process_until_it_goes() {
        use crate::thread::THREAD_KMEM_COST;
        let proc = Process::create_with_fixed_id_ext(
            &ROOT_JOB,
            45_207,
            "kmem",
            LinuxProcess::new(RamFS::new(), 0),
        )
        .unwrap();
        let acct = proc.linux().kmem().clone();
        acct.set_limit(2 * THREAD_KMEM_COST);
        let one = Thread::create_linux(&proc).unwrap();
        let two = Thread::create_linux(&proc).unwrap();
        assert_eq!(acct.charged(), 2 * THREAD_KMEM_COST);
        assert_eq!(
            Thread::create_linux(&proc).err(),
            Some(zircon_object::ZxError::NO_MEMORY)
        );
        one.terminate_abandoned();
        two.terminate_abandoned();
        drop((one, two));
    }

    #[test]
    fn a_pipe_write_over_the_quota_is_enomem_and_a_read_gives_it_back() {
        as_a_process_limited_to(45_201, 4096 + RUN_COST, |acct| {
            let (r, w) = Pipe::create_pair();
            assert_eq!(w.write_at(0, &[1u8; 4096]), Ok(4096));
            assert_eq!(charged(acct), 4096 + RUN_COST);
            assert_eq!(w.write_at(0, &[1u8; 1]), Err(FsError::NoMemory));
            let mut sink = [0u8; 1000];
            assert_eq!(r.read_at(0, &mut sink), Ok(1000));
            assert_eq!(charged(acct), 3096 + RUN_COST);
            assert_eq!(w.write_at(0, &[1u8; 1000]), Ok(1000));
            drop((r, w));
            assert_eq!(charged(acct), 0);
        });
    }

    #[test]
    fn passed_descriptors_are_the_senders_until_the_peer_takes_them() {
        as_a_process_limited_to(45_203, 0, |acct| {
            let a = UnixSocketState::new();
            let b = UnixSocketState::new();
            UnixSocketState::connect_pair(&a, &b);
            let file = || -> Arc<dyn crate::fs::FileLike> { UnixSocketState::new() };
            Socket::send_fds(&*a, alloc::vec![file(), file(), file()]).unwrap();
            assert_eq!(Socket::write(&*a, b"x", None), Ok(1));
            assert_eq!(acct.inflight(), 3);
            let before = charged(acct);
            let mut byte = [0u8; 1];
            let (r, _) = async_std::task::block_on(Socket::read(&*b, &mut byte));
            assert_eq!(r, Ok(1));
            assert_eq!(Socket::recv_fds(&*b, 2).len(), 2);
            assert_eq!(acct.inflight(), 1, "two received, one still in flight");
            assert_eq!(charged(acct), before - 2 * INFLIGHT_FD_COST - 1);
            // The last one never comes out: closing the socket gives it back.
            drop((a, b));
            assert_eq!((acct.inflight(), charged(acct)), (0, 0));
        });
    }

    #[test]
    fn record_locks_are_charged_to_their_taker_and_a_split_needs_room() {
        use crate::fs::record_lock::{release_description, setlk, LockRequest, HELD_COST};
        as_a_process_limited_to(45_208, 2 * HELD_COST, |acct| {
            let key = (usize::MAX - 3, 1);
            let r = |exclusive, start, end| LockRequest {
                exclusive,
                start,
                end,
                owner: 0xF10C,
                ofd: true,
            };
            assert_eq!(setlk(key, &r(true, 0, 10), false), Ok(true));
            assert_eq!(charged(acct), HELD_COST);
            // Every other byte: one more lock fits, the next is ENOLCK.
            assert_eq!(setlk(key, &r(true, 20, 30), false), Ok(true));
            assert_eq!(
                setlk(key, &r(true, 40, 50), false),
                Err(crate::error::LxError::ENOLCK)
            );
            // A hole needs a lock for the far piece, and leaves both as they were.
            assert_eq!(
                setlk(key, &r(true, 4, 6), true),
                Err(crate::error::LxError::ENOLCK)
            );
            assert_eq!(charged(acct), 2 * HELD_COST);
            // Filling the gap merges all into one, and gives a lock back.
            assert_eq!(setlk(key, &r(true, 10, 20), false), Ok(true));
            assert_eq!(charged(acct), HELD_COST);
            release_description(0xF10C);
            assert_eq!(charged(acct), 0);
        });
    }

    #[test]
    fn a_property_blob_is_charged_to_its_maker_until_it_goes() {
        use crate::fs::devfs::drm;
        let cost = crate::fs::devfs::drm_scheme::blob_cost(64);
        as_a_process_limited_to(45_207, cost, |acct| {
            let owner = 0xB10B_0001;
            let charge = || Charge::try_new(cost);
            let a = drm::create_blob_owned(alloc::vec![0; 64], true, owner, charge().unwrap());
            assert_eq!(charged(acct), cost);
            assert!(charge().is_err(), "a second one is over the limit");
            assert!(matches!(
                drm::destroy_blob(a, owner),
                drm::BlobDestroy::Destroyed
            ));
            assert_eq!(charged(acct), 0);
            drm::create_blob_owned(alloc::vec![0; 64], true, owner, charge().unwrap());
            drm::destroy_blobs_of(owner);
            assert_eq!(charged(acct), 0, "the fd closing frees it");
        });
    }

    #[test]
    fn a_semaphore_set_is_charged_to_its_maker_until_it_goes() {
        use crate::ipc::SemArray;
        let ten = SemArray::cost(10);
        as_a_process_limited_to(45_204, ten, |acct| {
            let set = SemArray::get_or_create(0, 10, 0o1600, 0, 0, &[]).unwrap();
            assert_eq!(charged(acct), ten);
            assert_eq!(
                SemArray::get_or_create(0, 1, 0o1600, 0, 0, &[]).err(),
                Some(crate::error::LxError::ENOMEM)
            );
            drop(set);
            assert_eq!(charged(acct), 0);
        });
    }

    #[test]
    fn epoll_entries_are_charged_to_the_epolls_maker() {
        use crate::fs::epoll::{Epoll, EpollEvent, EPITEM_COST};
        use crate::fs::{FileDesc, OpenFlags};
        as_a_process_limited_to(45_205, 2 * EPITEM_COST, |acct| {
            let ep = Epoll::new(OpenFlags::empty());
            let ev = EpollEvent { events: 1, data: 0 };
            let file = || -> Arc<dyn crate::fs::FileLike> { UnixSocketState::new() };
            const ADD: i32 = 1;
            const DEL: i32 = 2;
            const MOD: i32 = 3;
            ep.ctl(ADD, FileDesc::from(3usize), ev, Some(file()))
                .unwrap();
            ep.ctl(ADD, FileDesc::from(4usize), ev, Some(file()))
                .unwrap();
            assert_eq!(charged(acct), 2 * EPITEM_COST);
            assert_eq!(
                ep.ctl(ADD, FileDesc::from(5usize), ev, Some(file())),
                Err(crate::error::LxError::ENOMEM)
            );
            let f4 = ep.watched(FileDesc::from(4usize), 0).unwrap();
            ep.ctl(MOD, FileDesc::from(4usize), ev, Some(f4)).unwrap();
            assert_eq!(charged(acct), 2 * EPITEM_COST, "a MOD is no new entry");
            ep.ctl(DEL, FileDesc::from(3usize), ev, None).unwrap();
            assert_eq!(charged(acct), EPITEM_COST);
            drop(ep);
            assert_eq!(charged(acct), 0);
        });
    }

    #[test]
    fn inotify_watches_are_charged_and_counted() {
        use crate::fs::{inotify::watch_cost, Inotify, OpenFlags};
        as_a_process_limited_to(45_206, 0, |acct| {
            let ino = Inotify::new(OpenFlags::empty());
            let a = ino.add_watch("/tmp", 2).unwrap();
            assert_eq!(
                ino.add_watch("/tmp", 2),
                Ok(a),
                "the same path, the same watch"
            );
            ino.add_watch("/etc", 2).unwrap();
            assert_eq!(charged(acct), watch_cost("/tmp") + watch_cost("/etc"));
            assert_eq!(acct.inotify_watches(), 2);
            ino.rm_watch(a as i32).unwrap();
            assert_eq!(acct.inotify_watches(), 1);
            drop(ino);
            assert_eq!((charged(acct), acct.inotify_watches()), (0, 0));
        });
    }

    #[test]
    fn a_unix_send_over_the_quota_is_enomem_and_closing_gives_it_back() {
        as_a_process_limited_to(45_202, 64 * 1024, |acct| {
            let a = UnixSocketState::new();
            let b = UnixSocketState::new();
            UnixSocketState::connect_pair(&a, &b);
            let chunk = alloc::vec![7u8; 16 * 1024];
            let mut sent = 0;
            let refused = loop {
                match Socket::write(&*a, &chunk, None) {
                    Ok(n) => sent += n,
                    Err(e) => break e,
                }
                assert!(sent <= 64 * 1024, "the quota did not hold");
            };
            assert_eq!(refused, crate::error::LxError::ENOMEM);
            assert_eq!(charged(acct), sent + RUN_COST);
            drop((a, b));
            assert_eq!(charged(acct), 0);
        });
    }
}
