//! The interrupt hand-off of the e1000e, the watchdog it schedules, and the
//! two smoltcp tokens the stack drives the card through.
//!
//! `scheme_tests` next door covers the addresses and the routes, and
//! `rx_ring_tests`/`tx_ring_tests` cover the descriptor rings. What is here is
//! everything between them, and none of it had a test: the top half, the
//! deferred bottom half it queues, the guard that puts the interrupt mask back
//! even when that job never runs, the link watchdog, and the RX and TX tokens.
//!
//! Every comment in this part of the driver names a failure that was seen on
//! real hardware and nowhere else -- a NIC left interrupt-deaf for the life of
//! the kernel, link supervision dead for good, a download that stalls
//! mid-transfer -- so the asserts below are those claims written down.
//!
//! No seam is needed. The register window is host memory, the clock is the
//! virtual one, and `scheme_tests::interface` already stands a whole
//! `E1000eInterface` up over `rx_ring_tests::make_hw`.

use super::rx_ring_tests::{hw_deliver, ipv4_udp_frame};
use super::scheme_tests::interface;
use super::*;
use crate::nvme::nvme_queue::test_clock;
use crate::utils::deferred_job::{
    drain_deferred_jobs, drain_deferred_jobs_max, evicted_deferred_jobs, pending_deferred_jobs,
    push_deferred_job, push_deferred_job_front,
};
use smoltcp::phy::{Device as _, RxToken as _, TxToken as _};
extern crate std;

/// The causes the driver unmasks: TXDW, LSC and the three RX bits, plus the
/// RX-queue bit. Spelled out rather than taken from `IMS_REARM`, which is what
/// is under test.
const IMS_REARMED: u32 = (1 << 0) | (1 << 2) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 8);

/// An interface with the clock pinned at `now_us` and standing still.
///
/// The virtual clock is a thread-local that an earlier test in the same thread
/// may have left auto-advancing, which would make every deadline comparison
/// below depend on how many times the driver happens to read it. It is pinned
/// here rather than assumed.
fn at(now_us: u64) -> E1000eInterface {
    test_clock::set_auto_advance(0);
    test_clock::set(now_us);
    empty_queue();
    interface()
}

/// Run the deferred queue down to empty. One drain runs at most
/// `MAX_JOBS_PER_DRAIN` jobs, so a queue that another test filled takes
/// several.
fn empty_queue() {
    for _ in 0..64 {
        if pending_deferred_jobs() == 0 {
            return;
        }
        drain_deferred_jobs();
    }
}

fn reg(base: usize, r: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + r * 4) as *const u32) }
}

/// Write a register as the *hardware* would, bypassing the fake MAC's reaction
/// to a driver write: these tests play the card, not the driver.
fn poke(base: usize, r: usize, val: u32) {
    unsafe { core::ptr::write_volatile((base + r * 4) as *mut u32, val) };
}

// --- the top half

#[test]
fn an_interrupt_on_another_line_is_not_this_interfaces() {
    let iface = at(1_000);
    let base = iface.base;
    poke(base, E1000E_ICR, 0x1234);
    poke(base, E1000E_IMS, 0);
    iface.handle_irq(iface.irq + 1);
    assert_eq!(
        reg(base, E1000E_ICR),
        0x1234,
        "it read-cleared the cause register of a line that is not its own"
    );
    assert_eq!(
        reg(base, E1000E_IMS),
        0,
        "it unmasked its card on somebody else's interrupt"
    );
    assert_eq!(
        pending_deferred_jobs(),
        0,
        "it queued a poll for another line's interrupt"
    );
}

#[test]
fn an_interrupt_with_no_cause_only_puts_the_mask_back() {
    let iface = at(1_000);
    let base = iface.base;
    poke(base, E1000E_ICR, 0);
    poke(base, E1000E_IMS, 0);
    iface.handle_irq(iface.irq);
    assert_eq!(
        reg(base, E1000E_IMS),
        IMS_REARMED,
        "a spurious interrupt did not leave the serviced causes unmasked"
    );
    assert_eq!(
        pending_deferred_jobs(),
        0,
        "a poll was queued for an interrupt with no cause behind it"
    );
}

/// The subtle half of the no-cause path. A bottom half that is already queued
/// holds the mask down for the whole of its poll; re-arming here lets a later
/// edge clear ICR again after that poll has taken the stash, and those causes
/// are then owned by nobody.
#[test]
fn a_spurious_interrupt_does_not_unmask_a_card_a_bottom_half_already_owns() {
    let iface = at(1_000);
    let base = iface.base;
    iface.poll_pending.store(true, Ordering::SeqCst);
    poke(base, E1000E_ICR, 0);
    poke(base, E1000E_IMS, 0);
    iface.handle_irq(iface.irq);
    assert_eq!(
        reg(base, E1000E_IMS),
        0,
        "the mask went back while a bottom half still owned it"
    );
}

#[test]
fn an_interrupt_with_a_cause_masks_the_card_and_queues_one_poll() {
    let iface = at(7_000);
    let base = iface.base;
    poke(base, E1000E_ICR, ICR_RXT0);
    poke(base, E1000E_IMS, 0);
    poke(base, E1000E_IMC, 0);
    iface.handle_irq(iface.irq);
    assert_eq!(
        reg(base, E1000E_IMC),
        0xffff_ffff,
        "the card was left unmasked with a poll outstanding"
    );
    assert!(
        iface.poll_pending.load(Ordering::SeqCst),
        "the pending flag was not taken, so a second interrupt queues a second poll"
    );
    assert_eq!(
        iface.poll_pending_set_us.load(Ordering::Relaxed),
        7_000,
        "the flag was not stamped, so a stuck bottom half can never be timed out"
    );
    assert_eq!(
        pending_deferred_jobs(),
        1,
        "the bottom half was not queued, or was queued twice"
    );
    empty_queue();
}

/// The ICR read in the top half clears those causes in hardware, so a second
/// edge that finds a bottom half already queued has nowhere to put its own
/// but the stash. Dropping them loses a link-state change or an RX overrun.
#[test]
fn a_second_interrupt_stashes_its_causes_instead_of_dropping_them() {
    let iface = at(1_000);
    let base = iface.base;
    poke(base, E1000E_ICR, ICR_RXT0);
    poke(base, E1000E_IMS, 0);
    iface.handle_irq(iface.irq);
    assert_eq!(pending_deferred_jobs(), 1);

    poke(base, E1000E_ICR, ICR_LSC);
    iface.handle_irq(iface.irq);
    poke(base, E1000E_ICR, ICR_RXO);
    iface.handle_irq(iface.irq);
    assert_eq!(
        iface.pending_icr.load(Ordering::SeqCst),
        ICR_LSC | ICR_RXO,
        "the stash kept one edge's causes instead of accumulating both"
    );
    assert_eq!(
        pending_deferred_jobs(),
        1,
        "a second bottom half was queued on top of the first"
    );
    assert_eq!(
        reg(base, E1000E_IMS),
        0,
        "the mask went back while the first bottom half still held it"
    );
    empty_queue();
}

/// The bottom half is pushed to the FRONT of the shared queue. Behind a
/// backlog it would be the one aged out of the 256-entry FIFO, which is the
/// NIC going deaf rather than one late job.
#[test]
fn the_bottom_half_goes_to_the_front_of_the_queue() {
    let iface = at(1_000);
    let base = iface.base;
    for _ in 0..4 {
        push_deferred_job(|| {});
    }
    poke(base, E1000E_ICR, ICR_RXT0);
    poke(base, E1000E_IMS, 0);
    iface.handle_irq(iface.irq);
    // One job only. If the NIC's bottom half is not the one that ran, the
    // mask is still down.
    drain_deferred_jobs_max(1);
    assert_eq!(
        reg(base, E1000E_IMS),
        IMS_REARMED,
        "the bottom half was queued behind the backlog, so the card stayed masked"
    );
    empty_queue();
}

/// The release guard is built outside the closure and moved in, so the mask
/// goes back and the flag clears whether the job runs or is dropped unrun.
/// Built inside the body, an evicted bottom half left the NIC masked and the
/// flag set for the life of the kernel.
#[test]
fn a_bottom_half_dropped_unexecuted_still_clears_the_flag_and_puts_the_mask_back() {
    let iface = at(1_000);
    let base = iface.base;
    poke(base, E1000E_ICR, ICR_RXT0);
    poke(base, E1000E_IMS, 0);
    iface.handle_irq(iface.irq);
    assert_eq!(pending_deferred_jobs(), 1, "the queue did not start empty");

    // Age it out the way another interface's bottom halves would: a front push
    // into a full queue evicts the tail, and with the queue filled from the
    // front this job IS the tail by the time the cap is reached.
    let evicted = evicted_deferred_jobs();
    for _ in 0..256 {
        push_deferred_job_front(|| {});
    }
    assert_eq!(
        evicted_deferred_jobs(),
        evicted + 1,
        "the queue never reached its cap, so nothing was aged out"
    );
    assert!(
        !iface.poll_pending.load(Ordering::SeqCst),
        "the flag survived the eviction, so every later interrupt takes the already-pending path for good"
    );
    assert_eq!(
        reg(base, E1000E_IMS),
        IMS_REARMED,
        "the card was left masked by a bottom half that never ran"
    );
    empty_queue();
}

// --- the bottom half

#[test]
fn the_poll_takes_the_stashed_causes_and_leaves_the_stash_empty() {
    let iface = at(1_000);
    iface.pending_icr.store(ICR_RXO | ICR_LSC, Ordering::SeqCst);
    iface.poll_with_irq_hint(0).expect("the poll failed");
    assert_eq!(
        iface.pending_icr.load(Ordering::SeqCst),
        0,
        "the stash was not emptied, so the next poll services the same causes again"
    );
}

/// And it acts on them. An RX overrun arriving while a frame is half
/// assembled has to throw that frame away and count it: the rest of it was
/// never written, so handing it up would be a truncated packet.
#[test]
fn an_overrun_the_poll_inherits_discards_the_half_assembled_frame() {
    let iface = at(1_000);
    {
        let mut hw = iface.driver.hw.lock();
        hw.rx_pending = Some(alloc::vec![1, 2, 3]);
        hw.stats.rx_dropped = 0;
    }
    iface.pending_icr.store(ICR_RXO, Ordering::SeqCst);
    iface.poll_with_irq_hint(0).expect("the poll failed");
    let hw = iface.driver.hw.lock();
    assert!(
        hw.rx_pending.is_none(),
        "the half-assembled frame survived the overrun and would be handed up truncated"
    );
    assert_eq!(
        hw.stats.rx_dropped, 1,
        "the discarded frame was not counted"
    );
}

#[test]
fn a_poll_with_the_watchdog_due_schedules_it() {
    let iface = at(3_000_000);
    iface.driver.hw.lock().link_watchdog_next_us = 1_000;
    iface.poll_with_irq_hint(0).expect("the poll failed");
    assert_eq!(
        pending_deferred_jobs(),
        1,
        "the watchdog was not scheduled although its period was up"
    );
    empty_queue();
}

/// The `due` check in the poll is an optimisation, not a decision: it saves
/// taking the hw lock a second time, and `schedule_watchdog` re-tests the very
/// same predicate (`now < next` is the exact complement of `next <= now`). So
/// what this holds is the behaviour, not that branch.
#[test]
fn a_poll_with_the_watchdog_not_due_leaves_it_alone() {
    let iface = at(1_000);
    iface.driver.hw.lock().link_watchdog_next_us = 2_000_000;
    iface.poll_with_irq_hint(0).expect("the poll failed");
    assert_eq!(
        pending_deferred_jobs(),
        0,
        "the watchdog ran ahead of its period"
    );
}

/// The periodic poll is the only caller of the self-healing path, and it is
/// reached whether or not interrupts are still arriving -- which is what makes
/// it the escape hatch for an evicted bottom half.
#[test]
fn the_periodic_poll_heals_a_bottom_half_that_will_never_run() {
    let iface = at(POLL_PENDING_STUCK_US * 4);
    let base = iface.base;
    iface.poll_pending.store(true, Ordering::SeqCst);
    iface.poll_pending_set_us.store(1, Ordering::Relaxed);
    iface.driver.hw.lock().link_watchdog_next_us = u64::MAX;
    poke(base, E1000E_IMS, 0);
    iface.poll().expect("the periodic poll failed");
    assert!(
        !iface.poll_pending.load(Ordering::SeqCst),
        "a bottom half stuck far past the deadline was not healed"
    );
    assert_eq!(
        reg(base, E1000E_IMS),
        IMS_REARMED,
        "the periodic poll did not put the mask back itself"
    );
}

/// The other direction: a flag that is not set is not evidence of anything,
/// and the healing path has nothing to do.
#[test]
fn a_poll_on_an_interface_with_no_bottom_half_outstanding_heals_nothing() {
    let iface = at(POLL_PENDING_STUCK_US * 4);
    iface.poll_pending.store(false, Ordering::SeqCst);
    iface.poll_pending_set_us.store(1, Ordering::Relaxed);
    iface.driver.hw.lock().link_watchdog_next_us = u64::MAX;
    iface.poll().expect("the periodic poll failed");
    assert!(
        !iface.poll_pending.load(Ordering::SeqCst),
        "the healing path set a flag instead of leaving it alone"
    );
}

// --- the link watchdog

#[test]
fn the_watchdog_is_not_rescheduled_before_its_period_is_up() {
    let iface = at(1_000);
    iface.driver.hw.lock().link_watchdog_next_us = 2_000_000;
    iface.schedule_watchdog(false);
    assert_eq!(
        pending_deferred_jobs(),
        0,
        "a watchdog was queued inside its own period"
    );
    assert_eq!(
        iface.driver.hw.lock().link_watchdog_next_us,
        2_000_000,
        "the deadline moved although nothing was scheduled"
    );
}

/// `fast` is the link-state-change path: an LSC interrupt means the carrier
/// may have just gone, so the next look is 50 ms away rather than two seconds.
#[test]
fn a_fast_watchdog_runs_inside_the_period_and_shortens_the_next_one() {
    let iface = at(1_000);
    iface.driver.hw.lock().link_watchdog_next_us = 2_000_000;
    iface.schedule_watchdog(true);
    assert_eq!(
        pending_deferred_jobs(),
        1,
        "a link-change watchdog was held back by the ordinary period"
    );
    assert_eq!(
        iface.driver.hw.lock().link_watchdog_next_us,
        1_000 + E1000E_WATCHDOG_FAST_US,
        "the fast path did not shorten the next deadline"
    );
    empty_queue();
}

#[test]
fn a_due_watchdog_pushes_its_deadline_a_whole_period_out() {
    let iface = at(3_000_000);
    iface.driver.hw.lock().link_watchdog_next_us = 1_000;
    iface.schedule_watchdog(false);
    assert_eq!(
        iface.driver.hw.lock().link_watchdog_next_us,
        3_000_000 + E1000E_WATCHDOG_PERIOD_US,
        "the deadline was not pushed out, so every poll queues another watchdog"
    );
    empty_queue();
}

#[test]
fn only_one_watchdog_job_is_queued_however_many_times_it_is_asked_for() {
    let iface = at(1_000);
    for _ in 0..5 {
        iface.schedule_watchdog(true);
    }
    assert_eq!(
        pending_deferred_jobs(),
        1,
        "five requests queued five jobs instead of collapsing into one"
    );
    empty_queue();
}

/// The watchdog's own release guard, for the same reason as the bottom half's:
/// an evicted job that left the flag set meant link supervision stopped for
/// good, with nothing to report it.
#[test]
fn a_watchdog_job_dropped_unexecuted_lets_the_next_one_be_queued() {
    let iface = at(1_000);
    iface.schedule_watchdog(true);
    assert_eq!(pending_deferred_jobs(), 1, "the queue did not start empty");
    assert!(iface.watchdog_job_scheduled.load(Ordering::Acquire));

    // This one goes in at the back, so filling the queue from the front ages
    // it out without running it.
    let evicted = evicted_deferred_jobs();
    for _ in 0..256 {
        push_deferred_job_front(|| {});
    }
    assert_eq!(
        evicted_deferred_jobs(),
        evicted + 1,
        "the queue never reached its cap, so nothing was aged out"
    );
    assert!(
        !iface.watchdog_job_scheduled.load(Ordering::Acquire),
        "the flag survived the eviction, so link supervision is dead for good"
    );
    empty_queue();
}

#[test]
fn the_watchdog_job_reports_what_it_found_to_the_interface() {
    let iface = at(1_000);
    // The card says the carrier is down; the interface still believes the
    // optimistic value the probe left behind.
    iface.link_up_seen.store(true, Ordering::Release);
    {
        let mut hw = iface.driver.hw.lock();
        hw.link_up = true;
        poke(hw.base, E1000E_STATUS, 0);
    }
    iface.schedule_watchdog(true);
    empty_queue();
    assert!(
        !iface.link_up_seen.load(Ordering::Acquire),
        "the watchdog saw the carrier go and did not tell the interface"
    );
}

// --- what the kernel reads off the interface

#[test]
fn the_interface_names_itself_and_the_card_it_drives() {
    let iface = at(1_000);
    assert_eq!(
        iface.name(),
        "e1000e",
        "the driver name is what binds the scheme"
    );
    assert_eq!(iface.ifname(), "eth0");
    assert_eq!(iface.get_ifname(), "eth0");
    assert!(
        !iface.is_loopback(),
        "an Ethernet card that says it is loopback is put last in the device list"
    );
    assert_eq!(
        iface.get_mac(),
        EthernetAddress([0x52, 0x54, 0, 0, 0, 1]),
        "the MAC the kernel reports is not the one the stack uses"
    );
    assert!(iface.can_recv(), "the receive path is always open");
}

#[test]
fn the_statistics_handed_out_are_a_copy_and_not_the_counters_themselves() {
    let iface = at(1_000);
    iface.driver.hw.lock().stats.rx_packets = 7;
    let mut stats = iface.get_stats();
    assert_eq!(stats.rx_packets, 7);
    stats.rx_packets = 0;
    assert_eq!(
        iface.get_stats().rx_packets,
        7,
        "the caller was handed the live counters and zeroed them"
    );
}

// --- the smoltcp token layer

#[test]
fn a_pair_of_tokens_comes_out_only_when_a_frame_is_waiting() {
    let iface = at(1_000);
    let mut driver = iface.driver.clone();
    assert!(
        driver.receive().is_none(),
        "it handed the stack a frame out of an empty ring"
    );
    let frame = ipv4_udp_frame(b"hola");
    hw_deliver(&iface.driver.hw.lock(), 0, &frame);
    assert!(
        driver.receive().is_some(),
        "the frame the card delivered did not reach the stack"
    );
}

/// Deliver one frame into `slot` and let the stack consume its token,
/// returning what the stack was shown. The slot matters: the card fills them
/// in order, so a second frame goes into the one after the first.
fn round_trip_one_frame(iface: &E1000eInterface, slot: usize, frame: &[u8]) -> Vec<u8> {
    hw_deliver(&iface.driver.hw.lock(), slot, frame);
    let mut driver = iface.driver.clone();
    let (rx, _tx) = driver.receive().expect("no frame came out of the ring");
    rx.consume(Instant::from_micros(0), |buf| Ok(buf.to_vec()))
        .expect("the stack refused the frame")
}

#[test]
fn consuming_an_rx_token_hands_the_frame_over_and_gives_its_buffer_back() {
    let iface = at(1_000);
    iface.driver.hw.lock().rx_frame_pool.clear();
    let frame = ipv4_udp_frame(&[0x5a; 64]);
    assert_eq!(
        round_trip_one_frame(&iface, 0, &frame),
        frame,
        "the frame the stack saw is not the one the card delivered"
    );
    assert_eq!(
        iface.driver.hw.lock().rx_frame_pool.len(),
        1,
        "the buffer was not recycled, so every frame costs an allocation"
    );
}

/// The pool only takes allocations big enough to be worth reusing. A runt's
/// buffer would have to be grown by the next frame anyway, and keeping it
/// would hand `take_rx_frame` a buffer that reallocates on first use.
#[test]
fn the_buffer_of_a_runt_is_too_small_to_be_worth_keeping() {
    let iface = at(1_000);
    iface.driver.hw.lock().rx_frame_pool.clear();
    // 14 + 20 + 8 + 1 = 43 bytes, under the 64-byte floor.
    let frame = ipv4_udp_frame(b"x");
    assert_eq!(round_trip_one_frame(&iface, 0, &frame), frame);
    assert_eq!(
        iface.driver.hw.lock().rx_frame_pool.len(),
        0,
        "a buffer too small to reuse was put in the pool anyway"
    );
}

/// The cap is a limit on how much RX memory the driver keeps idle, so it has
/// to hold against a buffer that did NOT come out of the pool.
///
/// Delivering one frame into a full pool does not test it: `take_rx_frame`
/// pops a buffer before the frame is built, so the pool is one short by the
/// time the recycle comes back and the cap is never reached. Two frames are
/// taken out first and the pool filled while both are still in flight, so
/// each recycle really does arrive at a pool of `RX_FRAME_POOL_CAP`.
#[test]
fn a_full_pool_takes_no_more_buffers() {
    let iface = at(1_000);
    let first = ipv4_udp_frame(&[0x5a; 64]);
    let second = ipv4_udp_frame(&[0xa5; 64]);
    {
        let hw = iface.driver.hw.lock();
        hw_deliver(&hw, 0, &first);
    }
    let mut driver = iface.driver.clone();
    let (rx_a, _tx_a) = driver.receive().expect("the first frame did not come out");
    {
        let hw = iface.driver.hw.lock();
        hw_deliver(&hw, 1, &second);
    }
    let (rx_b, _tx_b) = driver.receive().expect("the second frame did not come out");

    {
        let mut hw = iface.driver.hw.lock();
        hw.rx_frame_pool.clear();
        for _ in 0..RX_FRAME_POOL_CAP {
            hw.rx_frame_pool.push(Vec::with_capacity(128));
        }
    }
    for rx in [rx_a, rx_b] {
        rx.consume(Instant::from_micros(0), |_buf| Ok(()))
            .expect("the stack refused the frame");
    }
    assert_eq!(
        iface.driver.hw.lock().rx_frame_pool.len(),
        RX_FRAME_POOL_CAP,
        "the pool grew past its cap, which is an unbounded hold on RX memory"
    );
}

/// The other side of the pool: a recycled buffer is reused, and a longer frame
/// still comes out whole out of a buffer that held a shorter one.
///
/// Not a test of `take_rx_frame`'s `reserve`: the `extend_from_slice` right
/// after it reserves the same amount on an empty buffer, so that call is an
/// allocation the Vec would make anyway.
#[test]
fn a_pooled_buffer_is_reused_and_a_longer_frame_still_comes_out_whole() {
    let iface = at(1_000);
    iface.driver.hw.lock().rx_frame_pool.clear();
    let short = ipv4_udp_frame(&[0x11; 64]);
    assert_eq!(round_trip_one_frame(&iface, 0, &short), short);
    assert_eq!(iface.driver.hw.lock().rx_frame_pool.len(), 1);

    let long = ipv4_udp_frame(&[0x22; 512]);
    assert_eq!(
        round_trip_one_frame(&iface, 1, &long),
        long,
        "the frame that reused a shorter buffer came back truncated"
    );
    let hw = iface.driver.hw.lock();
    assert_eq!(
        hw.rx_frame_pool.len(),
        1,
        "the reused buffer was not taken out of the pool, or not put back"
    );
    assert!(
        hw.rx_frame_pool[0].capacity() >= long.len(),
        "the buffer was handed back without the room the longer frame needed"
    );
}

#[test]
fn a_tx_token_comes_out_only_while_the_ring_has_room() {
    let iface = at(1_000);
    let mut driver = iface.driver.clone();
    assert!(
        driver.transmit().is_some(),
        "a ring that starts entirely free refused a token"
    );
    // Fill every slot but the guard the hardware needs.
    {
        let mut hw = iface.driver.hw.lock();
        for i in 0..NUM_TX - 1 {
            hw.send(&alloc::vec![i as u8; 64])
                .expect("a free slot refused a frame");
        }
    }
    assert!(
        driver.transmit().is_none(),
        "a full ring handed out a token the token cannot honour"
    );
}

/// The closure is promised a slice of exactly `len`. Clamping an oversized
/// request to the stack scratch would either panic inside the closure's
/// `copy_from_slice` or put a truncated frame on the wire.
#[test]
fn a_tx_token_hands_the_closure_a_slice_of_exactly_the_length_asked_for() {
    let iface = at(1_000);
    let mut driver = iface.driver.clone();
    for len in [60, TX_SCRATCH_LEN, TX_SCRATCH_LEN + 1] {
        let token = driver.transmit().expect("the ring had room");
        let got = token
            .consume(Instant::from_micros(0), len, |buf| Ok(buf.len()))
            .expect("the frame was refused");
        assert_eq!(
            got, len,
            "a frame of {} bytes was handed a buffer of {}",
            len, got
        );
    }
}

#[test]
fn what_a_tx_token_writes_is_what_lands_in_the_ring() {
    let iface = at(1_000);
    let slot = iface.driver.hw.lock().tx_tail;
    let mut driver = iface.driver.clone();
    let token = driver.transmit().expect("the ring had room");
    token
        .consume(Instant::from_micros(0), 64, |buf| {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = i as u8;
            }
            Ok(())
        })
        .expect("the frame was refused");
    let hw = iface.driver.hw.lock();
    let posted = unsafe {
        let d = &*hw.tx_ring.as_ptr::<TxDesc>().add(slot);
        core::slice::from_raw_parts(hw.tx_buf_vaddr(slot) as *const u8, d.len as usize)
    };
    assert_eq!(
        posted.len(),
        64,
        "the descriptor does not carry the length the stack asked for"
    );
    assert!(
        posted.iter().enumerate().all(|(i, b)| *b == i as u8),
        "the bytes in the buffer are not the ones the stack wrote"
    );
}

/// Nothing completes with the carrier down, so waiting is pointless. It is not
/// counted as a drop either: `tx_dropped` means "lost with the link up", which
/// is the deadlock signature the watchdog reports.
#[test]
fn a_frame_offered_with_the_link_down_is_refused_instead_of_spun_on() {
    let iface = at(1_000);
    {
        let mut hw = iface.driver.hw.lock();
        hw.link_up = false;
        poke(hw.base, E1000E_STATUS, 0);
        hw.tx_dropped = 0;
    }
    let mut driver = iface.driver.clone();
    let token = driver.transmit().expect("the ring had room");
    let result = token.consume(Instant::from_micros(0), 64, |_buf| Ok(()));
    assert_eq!(
        result.err(),
        Some(smoltcp::Error::Exhausted),
        "a frame was accepted with no carrier to send it on"
    );
    assert_eq!(
        iface.driver.hw.lock().tx_dropped,
        0,
        "a frame refused with the link down was counted as a link-up loss"
    );
}

/// A full ring with the carrier up is spun on, because the NIC drains it by
/// DMA on its own, and only counted as a drop once the spin limit is spent.
/// Dropping it silently is the one thing this path must never do: the ACK
/// smoltcp generates in response to a received segment goes out through this
/// token, and smoltcp has already advanced its window state by the time it
/// learns the send failed.
#[test]
fn a_full_ring_with_the_link_up_is_counted_as_a_drop_rather_than_lost_silently() {
    let iface = at(1_000);
    let mut driver = iface.driver.clone();
    let token = driver.transmit().expect("the ring had room");
    {
        let mut hw = iface.driver.hw.lock();
        for i in 0..NUM_TX - 1 {
            hw.send(&alloc::vec![i as u8; 64])
                .expect("a free slot refused a frame");
        }
        assert!(!hw.can_send(), "the ring did not fill");
        hw.tx_dropped = 0;
    }
    let result = token.consume(Instant::from_micros(0), 64, |_buf| Ok(()));
    assert_eq!(
        result.err(),
        Some(smoltcp::Error::Exhausted),
        "a frame was accepted onto a ring with no room for it"
    );
    assert_eq!(
        iface.driver.hw.lock().tx_dropped,
        1,
        "a frame lost with the link up was not counted, so the watchdog has nothing to report"
    );
}

/// `send`/`recv` are the raw path, the one a socket syscall takes without
/// going through smoltcp at all.
#[test]
fn the_raw_send_and_receive_path_carries_a_frame_end_to_end() {
    let iface = at(1_000);
    assert!(iface.can_send(), "a free ring says it cannot send");
    let frame = alloc::vec![0xA5u8; 128];
    assert_eq!(
        iface.send(&frame).expect("the frame was refused"),
        frame.len(),
        "the count reported is not the whole frame"
    );

    let delivered = ipv4_udp_frame(b"vuelta");
    hw_deliver(&iface.driver.hw.lock(), 0, &delivered);
    let mut buf = alloc::vec![0u8; 2048];
    let n = iface.recv(&mut buf).expect("nothing came back");
    assert_eq!(
        &buf[..n],
        &delivered[..],
        "the frame read back is not the one delivered"
    );
    assert_eq!(
        iface.recv(&mut buf).err(),
        Some(DeviceError::NotReady),
        "an empty ring answered with a frame"
    );
}
