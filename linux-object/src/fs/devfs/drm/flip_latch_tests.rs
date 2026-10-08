extern crate std;

use super::*;

/// Start from a clean slate; these statics are process-wide.
fn reset() {
    PENDING_DRM_TIMERS.lock().clear();
    FLIPS_IN_FLIGHT.store(0, Ordering::Release);
    FLIP_EVENT_PENDING.store(false, Ordering::Release);
    DRM_TIMER_ARMED.store(false, Ordering::Release);
    // Align the synthetic vblank phase to "just now" so a later
    // `schedule_flip_event` waits a full refresh period. A mid-period
    // leftover from an earlier test left only ~1 ms of slack; under libos
    // that timer could fire (or the lattice catch-up path could deliver
    // on the spot) before flip-latch assertions ran.
    DRM_STATE.lock().next_vblank = kernel_hal::timer::timer_now();
}

/// Queue a flip the way `schedule_flip_event` does, without arming a real
/// timer (which under libos would spawn an async sleep task and make this
/// non-deterministic).
fn queue_one(file: &Arc<DrmFileState>) {
    let mut q = PENDING_DRM_TIMERS.lock();
    FLIPS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    FLIP_EVENT_PENDING.store(true, Ordering::Release);
    q.push_back(PendingDrmTimer::Flip {
        crtc_id: SYNTH_CRTC_ID,
        user_data: 0xF11D,
        file: Arc::downgrade(file),
    });
}

/// The regression this guards. `deliver_pending_drm_timer` drains the whole
/// queue into a local before delivering anything, so a flip that is
/// mid-delivery is in neither the queue nor yet delivered.
/// `clear_stale_flip_pending` looked only at the queue, decided the latch
/// was stale, and cleared it -- so a concurrent PAGE_FLIP was accepted while
/// the previous one had not completed, and two frames could reach the
/// scanout inside one vblank period.
#[test]
fn a_flip_being_delivered_still_counts_as_pending() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    queue_one(&file);
    assert!(FLIP_EVENT_PENDING.load(Ordering::Acquire));

    // Reproduce the window: the queue has been drained but the event has
    // not been pushed to the fd yet.
    let drained: Vec<PendingDrmTimer> = PENDING_DRM_TIMERS.lock().drain(..).collect();
    assert_eq!(drained.len(), 1);
    assert!(
        !PENDING_DRM_TIMERS
            .lock()
            .iter()
            .any(|j| matches!(j, PendingDrmTimer::Flip { .. })),
        "the queue is empty, which is what fooled the old check"
    );

    clear_stale_flip_pending();
    assert!(
        FLIP_EVENT_PENDING.load(Ordering::Acquire),
        "the latch must survive: this flip has not been delivered yet"
    );

    // Delivering it is what clears the latch.
    queue_flip_event(&file, SYNTH_CRTC_ID, 0xF11D);
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
    assert!(file.has_events(), "the completion reached the card fd");
    reset();
}

/// The other half: `queue_flip_event` cleared the latch unconditionally, so
/// delivering the first of two outstanding flips advertised "nothing
/// pending" while the second was still queued -- after which a further flip
/// skipped the flush and the queue held two.
#[test]
fn delivering_one_of_two_flips_leaves_the_latch_set() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    queue_one(&file);
    queue_one(&file);
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 2);

    queue_flip_event(&file, SYNTH_CRTC_ID, 0xF11D);
    assert!(
        FLIP_EVENT_PENDING.load(Ordering::Acquire),
        "one flip is still outstanding"
    );
    queue_flip_event(&file, SYNTH_CRTC_ID, 0xF11D);
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire), "now none is");
    reset();
}

/// A genuinely stale latch -- set with nothing queued and nothing in flight
/// -- still self-heals. That is what `clear_stale_flip_pending` is for: a
/// stuck latch means a persistent EBUSY, which wlroots escalates into an
/// output teardown.
#[test]
fn a_truly_stale_latch_is_still_cleared() {
    let _serialised = super::test_globals::lock();
    reset();
    FLIP_EVENT_PENDING.store(true, Ordering::Release);
    clear_stale_flip_pending();
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    reset();
}

/// The flush is the other deliverer, and it kept the other half of the
/// accounting: a flip whose drm_file was gone by the time the flush
/// reached it cleared the latch but not the count. From then on every
/// delivery found one flip more in flight than there was, none was ever
/// the last, the latch stayed set after every real flip, and the next
/// PAGE_FLIP spun `settle_outstanding_flip` out into EBUSY -- which
/// wlroots escalates into an output teardown.
#[test]
fn a_flushed_flip_whose_file_is_gone_leaves_the_population_too() {
    let _serialised = super::test_globals::lock();
    reset();
    {
        let mut q = PENDING_DRM_TIMERS.lock();
        FLIPS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        FLIP_EVENT_PENDING.store(true, Ordering::Release);
        q.push_back(PendingDrmTimer::Flip {
            crtc_id: SYNTH_CRTC_ID,
            user_data: 0xF11D,
            file: Weak::new(),
        });
    }
    flush_pending_flip_completions();
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    assert_eq!(
        FLIPS_IN_FLIGHT.load(Ordering::Acquire),
        0,
        "the flip went with its file"
    );
    // The next real flip is delivered and is the last one in flight, so
    // the latch clears and the flip after it is accepted.
    let file = DrmFileState::new();
    queue_one(&file);
    flush_pending_flip_completions();
    assert!(file.has_events(), "the completion reached the card fd");
    assert!(
        !FLIP_EVENT_PENDING.load(Ordering::Acquire),
        "nothing outstanding"
    );
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
    assert!(settle_outstanding_flip());
    reset();
}

/// Cancelling is accounted for too, and can never drive the count below
/// zero: an underflow would wrap and latch the flag on forever.
#[test]
fn cancelling_clears_the_latch_and_never_underflows() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    queue_one(&file);
    cancel_pending_events();
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);

    // More completions than flips (a cancel racing a delivery) must not wrap.
    flip_in_flight_done();
    flip_in_flight_done();
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    reset();
}

/// The window as the timer IRQ leaves it on another CPU: queued, drained,
/// not yet posted. [`finish_delivery`] is the other half.
fn leave_one_mid_delivery(file: &Arc<DrmFileState>) {
    queue_one(file);
    let drained: Vec<PendingDrmTimer> = PENDING_DRM_TIMERS.lock().drain(..).collect();
    assert_eq!(drained.len(), 1);
    assert!(FLIP_EVENT_PENDING.load(Ordering::Acquire));
}

/// Post the mid-delivery completion and pin the vblank phase.
///
/// `next_vblank = now` is set *before* clearing the latch so the racer,
/// which wakes in `settle_outstanding_flip` the instant the latch drops,
/// sees a fresh phase: `next_vblank_deadline` then returns one full period
/// out. Pinning after `queue_flip_event` would race the racer's own
/// `schedule_flip_event` against a leftover mid-period slot.
fn finish_delivery(file: &DrmFileState) {
    DRM_STATE.lock().next_vblank = kernel_hal::timer::timer_now();
    queue_flip_event(file, SYNTH_CRTC_ID, 0xF11D);
}

/// Whether a DRM event with this `user_data` is already on the card fd.
fn fd_has_user_data(file: &DrmFileState, want: u64) -> bool {
    let mut buf = [0u8; 512];
    let n = match file.read_events(&mut buf) {
        EventRead::Read(n) => n,
        _ => return false,
    };
    let mut off = 0usize;
    while off + 16 <= n {
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(&buf[off + 4..off + 8]);
        let len = u32::from_ne_bytes(len_bytes) as usize;
        let mut ud_bytes = [0u8; 8];
        ud_bytes.copy_from_slice(&buf[off + 8..off + 16]);
        if u64::from_ne_bytes(ud_bytes) == want {
            return true;
        }
        if len < 16 || off + len > n {
            break;
        }
        off += len;
    }
    false
}

/// Run `f` on another thread -- the compositor's syscall on another CPU
/// than the timer -- with the delivery held open until `f` has certainly
/// reached its settle step, then finish the delivery. Returns what `f`
/// answered and whether the first completion was already on the fd when
/// it did: a caller that went ahead early sees an empty fd.
fn race_against_delivery<R: Send + 'static>(
    file: &Arc<DrmFileState>,
    f: impl FnOnce(&Arc<DrmFileState>) -> R + Send + 'static,
) -> (R, bool) {
    let entered = Arc::new(AtomicBool::new(false));
    let racer = {
        let file = file.clone();
        let entered = entered.clone();
        std::thread::spawn(move || {
            entered.store(true, Ordering::Release);
            let answer = f(&file);
            (answer, file.has_events())
        })
    };
    while !entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(
        FLIPS_IN_FLIGHT.load(Ordering::Acquire),
        1,
        "the racer went ahead while the delivery was still open"
    );
    finish_delivery(file);
    racer.join().expect("the racer panicked")
}

/// A completion that `atomic_commit` scheduled has armed the real timer,
/// which fires on another thread here. Let it, so it does not go off in
/// the middle of whichever test runs next.
fn let_the_armed_timer_fire() {
    for _ in 0..200 {
        if !DRM_TIMER_ARMED.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the vblank timer never fired");
}

/// The compositor's next PAGE_FLIP lands on one CPU while the timer IRQ on
/// another has the previous completion drained but not yet posted. This
/// used to be EBUSY -- the "safety net" in `page_flip` -- and wlroots tears
/// the output down on that. It waits for the delivery instead.
#[test]
fn a_flip_that_lands_mid_delivery_waits_for_it_instead_of_ebusy() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    leave_one_mid_delivery(&file);

    let (answer, first_completion_was_on_the_fd) = race_against_delivery(&file, |file| {
        page_flip(0x77, SYNTH_CRTC_ID, 0xF1A9, true, file)
    });
    // 0x77 is no framebuffer, so past the settle step the answer is the
    // fb's own. What matters is that it is not EBUSY.
    assert_eq!(answer, Err(FlipError::Present(PresentError::NoSuchFb)));
    assert!(
        first_completion_was_on_the_fd,
        "the flip went ahead before the completion reached the fd"
    );
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
    reset();
}

/// The atomic path has the same step, and the same used-to-be-EBUSY.
#[test]
fn an_atomic_commit_that_lands_mid_delivery_waits_for_it_too() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    leave_one_mid_delivery(&file);

    // An event is only owed on a CRTC that is (or becomes) active: the
    // check phase refuses one on a pipe that is off and stays off, as
    // `drm_atomic_crtc_check` does. This is a frame on a running output.
    let was_active = core::mem::replace(&mut DRM_STATE.lock().atomic.active, true);
    let (answer, first_completion_was_on_the_fd) = race_against_delivery(&file, |file| {
        atomic_commit(&AtomicUpdate::default(), false, false, true, 0xA70, file)
    });
    DRM_STATE.lock().atomic.active = was_active;
    assert_eq!(
        answer,
        Ok(()),
        "a commit on an active CRTC that asks for an event is accepted"
    );
    assert!(
        first_completion_was_on_the_fd,
        "the commit went ahead before the completion reached the fd"
    );
    // Own completion (user_data 0xA70): normally still paced on the
    // synthetic vblank. Under libos `timer_set` is an async task, and the
    // lattice catch-up path can also deliver on the committing thread when
    // the previous slot is already due -- so by the time we look the event
    // may already be on the fd. Either form means the mid-delivery wait
    // worked and the new PAGE_FLIP_EVENT was owed; the bug was EBUSY.
    let queued_for_vblank = PENDING_DRM_TIMERS.lock().iter().any(|j| {
        matches!(
            j,
            PendingDrmTimer::Flip {
                user_data: 0xA70,
                ..
            }
        )
    });
    if queued_for_vblank {
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 1);
        let_the_armed_timer_fire();
    } else {
        assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
        assert!(
            fd_has_user_data(&file, 0xA70),
            "atomic commit with PAGE_FLIP_EVENT must queue or deliver its completion"
        );
    }
    reset();
}

/// A commit that asks for no event owes the CRTC no ordering with the
/// previous completion, and never did: it must not sit out the delivery.
#[test]
fn a_commit_without_an_event_does_not_wait_for_the_delivery() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    leave_one_mid_delivery(&file);

    assert_eq!(
        atomic_commit(&AtomicUpdate::default(), false, false, false, 0, &file),
        Ok(())
    );
    assert!(
        FLIP_EVENT_PENDING.load(Ordering::Acquire),
        "the delivery is still open; the commit did not touch it"
    );
    assert!(!file.has_events());
    finish_delivery(&file);
    reset();
}

/// A completion still queued for the vblank is delivered now, not waited
/// for: the compositor's next frame must not sit out the rest of the
/// period. No timer is armed here, so waiting would be the whole limit.
#[test]
fn a_queued_completion_is_delivered_now_not_waited_for() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    queue_one(&file);

    assert!(settle_outstanding_flip());
    assert!(file.has_events(), "the queued completion reached the fd");
    assert_eq!(FLIPS_IN_FLIGHT.load(Ordering::Acquire), 0);
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    reset();
}

/// A latch with nothing behind it -- no queued flip, none in flight -- is
/// the self-heal case, and costs the next flip nothing.
#[test]
fn a_stale_latch_does_not_hold_up_the_next_flip() {
    let _serialised = super::test_globals::lock();
    reset();
    FLIP_EVENT_PENDING.store(true, Ordering::Release);
    assert!(settle_outstanding_flip());
    assert!(!FLIP_EVENT_PENDING.load(Ordering::Acquire));
    reset();
}

/// The limit: a delivery that never finishes (a counter that lost its
/// deliverer) is EBUSY, not a syscall that never returns.
#[test]
fn a_delivery_that_never_finishes_is_ebusy_not_a_hang() {
    let _serialised = super::test_globals::lock();
    reset();
    let file = DrmFileState::new();
    leave_one_mid_delivery(&file);

    assert!(!settle_outstanding_flip());
    assert_eq!(
        page_flip(0x77, SYNTH_CRTC_ID, 0, true, &file),
        Err(FlipError::Busy)
    );
    reset();
}
