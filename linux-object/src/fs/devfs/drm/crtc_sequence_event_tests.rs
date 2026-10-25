//! `DRM_EVENT_CRTC_SEQUENCE`, the event `CRTC_QUEUE_SEQUENCE` owes the
//! fd: its wire layout (`struct drm_event_crtc_sequence`: `user_data`,
//! `time_ns`, a 64-bit `sequence`; not the vblank event's `tv_sec`,
//! `tv_usec` and `crtc_id`), its pacing (a job waits for its vblank like
//! a vblank wait) and its life (it goes with the file that queued it).
extern crate std;

use super::*;
use core::convert::TryInto;

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

fn pending_sequence_jobs() -> usize {
    PENDING_DRM_TIMERS
        .lock()
        .iter()
        .filter(|j| matches!(j, PendingDrmTimer::Sequence { .. }))
        .count()
}

#[test]
fn the_event_names_the_vblank_in_nanoseconds_waits_for_it_and_dies_with_its_file() {
    let _serialised = super::test_globals::lock();
    PENDING_DRM_TIMERS.lock().clear();
    let file = DrmFileState::new();
    let other = DrmFileState::new();

    // Due now: on the fd that asked, and on no other.
    let (seq_before, _) = vblank_seq_and_time_now();
    PENDING_DRM_TIMERS
        .lock()
        .push_back(PendingDrmTimer::Sequence {
            user_data: 0x5EC0_0001,
            due_seq: seq_before,
            file: Arc::downgrade(&file),
        });
    deliver_pending_drm_timer();
    let (seq_after, time_after) = vblank_seq_and_time_now();
    assert!(!other.has_events(), "another file's fd got the event");
    let mut buf = [0u8; 64];
    let EventRead::Read(n) = file.read_events(&mut buf) else {
        panic!("the due sequence event did not reach the fd");
    };
    assert_eq!(n, 32, "one `struct drm_event_crtc_sequence`");
    assert_eq!(le32(&buf, 0), 3, "DRM_EVENT_CRTC_SEQUENCE");
    assert_eq!(le32(&buf, 4), 32, "base.length");
    assert_eq!(le64(&buf, 8), 0x5EC0_0001, "user_data");
    let time_ns = le64(&buf, 16);
    let now_ns = u64::try_from(kernel_hal::timer::timer_now().as_nanos()).unwrap_or(0);
    assert!(
        time_ns <= now_ns && now_ns - time_ns < 2 * vblank_period_ns(),
        "time_ns {} is not the last vblank's (now {})",
        time_ns,
        now_ns
    );
    let seq = le64(&buf, 24);
    // The stamp is the boundary the lattice gives that sequence: the one
    // read back now, less a period per vblank in between. (Not `% period`:
    // a modeset earlier in the process re-anchors the lattice, so a boundary
    // is a multiple of the period from the anchor, not from zero.)
    assert_eq!(
        time_ns,
        time_after - (seq_after as u64 - seq) * vblank_period_ns(),
        "the stamp is the vblank boundary of sequence {}",
        seq
    );
    assert!(
        seq >= seq_before as u64 && seq <= seq_after as u64,
        "sequence {} outside {}..{}",
        seq,
        seq_before,
        seq_after
    );

    // Not due: no event, the job waits; and the file closing takes it.
    PENDING_DRM_TIMERS
        .lock()
        .push_back(PendingDrmTimer::Sequence {
            user_data: 2,
            due_seq: seq_after.wrapping_add(1_000),
            file: Arc::downgrade(&file),
        });
    deliver_pending_drm_timer();
    assert!(!file.has_events(), "an event before its vblank");
    assert_eq!(
        pending_sequence_jobs(),
        1,
        "the job has to wait for its vblank"
    );
    cancel_pending_timers_for_file(Arc::as_ptr(&file));
    assert_eq!(pending_sequence_jobs(), 0, "the job outlived its file");
    DRM_TIMER_ARMED.store(false, Ordering::Release);
}

/// The CRTC back on when dropped, panic or not: a failed assertion must
/// not leave the next test on a dark pipe.
struct CrtcOn;

impl Drop for CrtcOn {
    fn drop(&mut self) {
        set_crtc_blanked(false);
    }
}

fn pending_jobs() -> (usize, usize, usize) {
    let q = PENDING_DRM_TIMERS.lock();
    let count = |f: fn(&PendingDrmTimer) -> bool| q.iter().filter(|j| f(j)).count();
    (
        count(|j| matches!(j, PendingDrmTimer::Flip { .. })),
        count(|j| matches!(j, PendingDrmTimer::Vblank { .. })),
        count(|j| matches!(j, PendingDrmTimer::Sequence { .. })),
    )
}

/// The CRTC going off (`drm_crtc_vblank_off`) sends the vblank and
/// sequence events still waiting for their vblank, at once and with the
/// count there is ("Sending premature vblank event on disable"), to the
/// fd that queued each; a flip completion stays queued (the function
/// walks only `vblank_event_list`). Only the transition sends: a pipe
/// already off sends nothing more.
#[test]
fn the_crtc_going_off_sends_the_waiting_events_with_the_count_there_is() {
    let _serialised = super::test_globals::lock();
    PENDING_DRM_TIMERS.lock().clear();
    set_crtc_blanked(false);
    let _on_again = CrtcOn;
    let file = DrmFileState::new();
    let other = DrmFileState::new();
    let seq_before = vblank_seq_now();
    let far = seq_before.wrapping_add(1_000);
    {
        let mut q = PENDING_DRM_TIMERS.lock();
        q.push_back(PendingDrmTimer::Vblank {
            signal: 0x0FF0_0001,
            due_seq: far,
            file: Arc::downgrade(&file),
        });
        q.push_back(PendingDrmTimer::Flip {
            crtc_id: SYNTH_CRTC_ID,
            user_data: 0x0FF0_0002,
            file: Arc::downgrade(&file),
            event: true,
            out_fence: None,
        });
        q.push_back(PendingDrmTimer::Sequence {
            user_data: 0x0FF0_0003,
            due_seq: far,
            file: Arc::downgrade(&file),
        });
    }
    assert_eq!(pending_jobs(), (1, 1, 1));

    set_crtc_blanked(true);
    let seq_after = vblank_seq_now();
    assert_eq!(
        pending_jobs(),
        (1, 0, 0),
        "the vblank and sequence jobs leave the queue, the flip stays"
    );
    assert!(!other.has_events(), "another file's fd got an event");
    let mut buf = [0u8; 96];
    let EventRead::Read(n) = file.read_events(&mut buf) else {
        panic!("the events did not reach the fd");
    };
    assert_eq!(n, 64, "the vblank event and the sequence event, no flip");
    // The vblank event, with the count there is.
    assert_eq!(le32(&buf, 0), 1, "DRM_EVENT_VBLANK");
    assert_eq!(le64(&buf, 8), 0x0FF0_0001, "its user_data");
    let seq = le32(&buf, 24);
    assert!(
        (seq.wrapping_sub(seq_before) as i32) >= 0 && (seq_after.wrapping_sub(seq) as i32) >= 0,
        "vblank sequence {} outside {}..{}",
        seq,
        seq_before,
        seq_after
    );
    assert!(
        (far.wrapping_sub(seq) as i32) > 0,
        "the vblank asked for never came"
    );
    // The sequence event, with the same count.
    assert_eq!(le32(&buf, 32), 3, "DRM_EVENT_CRTC_SEQUENCE");
    assert_eq!(le64(&buf, 40), 0x0FF0_0003, "its user_data");
    assert_eq!(le64(&buf, 56), seq as u64, "the same count");

    // Already off: a job queued now waits for the pipe, and nothing sends it.
    PENDING_DRM_TIMERS
        .lock()
        .push_back(PendingDrmTimer::Vblank {
            signal: 0x0FF0_0005,
            due_seq: far,
            file: Arc::downgrade(&file),
        });
    set_crtc_blanked(true);
    assert_eq!(pending_jobs(), (1, 1, 0), "no transition, nothing sent");
    assert!(!file.has_events());
    PENDING_DRM_TIMERS.lock().clear();
    DRM_TIMER_ARMED.store(false, Ordering::Release);
}
