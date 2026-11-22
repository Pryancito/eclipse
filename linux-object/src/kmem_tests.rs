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
    assert_eq!(a.charged(), 0);
    assert_eq!(q.charged(), 0);
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
    assert_eq!(a.charged(), 0);
}

#[test]
fn releasing_more_than_was_charged_stops_at_the_end() {
    let a = KmemAccount::with_limit(0);
    let mut q = ChargedBytes::new();
    q.charge_to(Some(a.clone()), 10).unwrap();
    q.release(1000);
    assert_eq!(a.charged(), 0);
    q.release(1);
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
        acct.set_limit(limit);
        let r = async_std::task::block_on(async {
            kernel_hal::thread::set_current_thread(Some(thread.clone()));
            let r = f(&acct);
            kernel_hal::thread::set_current_thread(None);
            r
        });
        thread.terminate_abandoned();
        r
    }

    #[test]
    fn a_pipe_write_over_the_quota_is_enomem_and_a_read_gives_it_back() {
        as_a_process_limited_to(45_201, 4096 + RUN_COST, |acct| {
            let (r, w) = Pipe::create_pair();
            assert_eq!(w.write_at(0, &[1u8; 4096]), Ok(4096));
            assert_eq!(acct.charged(), 4096 + RUN_COST);
            assert_eq!(w.write_at(0, &[1u8; 1]), Err(FsError::NoMemory));
            let mut sink = [0u8; 1000];
            assert_eq!(r.read_at(0, &mut sink), Ok(1000));
            assert_eq!(acct.charged(), 3096 + RUN_COST);
            assert_eq!(w.write_at(0, &[1u8; 1000]), Ok(1000));
            drop((r, w));
            assert_eq!(acct.charged(), 0);
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
            assert_eq!(acct.charged(), sent + RUN_COST);
            drop((a, b));
            assert_eq!(acct.charged(), 0);
        });
    }
}
