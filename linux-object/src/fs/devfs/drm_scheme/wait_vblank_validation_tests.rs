//! What `WAIT_VBLANK` refuses, driven through the ioctl entry point the
//! way `drmWaitVBlank` drives it: `_DRM_VBLANK_SIGNAL`, a bit outside
//! the masks, and a pipe the card does not have, each EINVAL in
//! `drm_wait_vblank_ioctl` before the sequence is even looked at. This
//! arm answered all of them with pipe 0's counter.
extern crate std;

use super::crtc_sequence_tests::{get_sequence, queue};
use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use core::convert::TryInto;

const RELATIVE: u32 = 0x1;

fn wait(c: &Client, typ: u32, sequence: u32) -> Result<u32> {
    let mut req = DrmWaitVblank {
        typ,
        sequence,
        val1: 0,
        val2: 0,
    };
    c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req)
        .map(|_| req.sequence)
}

fn high_crtc(index: u32) -> u32 {
    index << _DRM_VBLANK_HIGH_CRTC_SHIFT
}

#[test]
fn a_signal_an_unknown_bit_or_a_missing_pipe_is_refused_before_the_wait() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);

    // The shapes libdrm sends on a one-head card all go through: a
    // relative query, the same with the high-CRTC field naming pipe 0,
    // NEXTONMISS, and the event form.
    let now = wait(&c, RELATIVE, 0).expect("a relative query");
    assert_eq!(wait(&c, RELATIVE | high_crtc(0), 0), Ok(now));
    assert_eq!(
        wait(&c, RELATIVE | _DRM_VBLANK_NEXTONMISS_FLAG, 0).map(|s| s >= now),
        Ok(true)
    );
    assert_eq!(wait(&c, RELATIVE | _DRM_VBLANK_EVENT, 1).is_ok(), true);

    // Signals: EINVAL, whatever else is set.
    for typ in [
        _DRM_VBLANK_SIGNAL,
        _DRM_VBLANK_SIGNAL | RELATIVE,
        _DRM_VBLANK_SIGNAL | _DRM_VBLANK_EVENT | RELATIVE,
    ] {
        assert_eq!(wait(&c, typ, 0), Err(FsError::InvalidParam), "{typ:#x}");
    }
    // A bit outside the type, flag and high-CRTC masks.
    for bit in [1 << 6, 1 << 12, 1 << 25, 1 << 27, 1 << 31] {
        assert_eq!(
            wait(&c, RELATIVE | bit, 0),
            Err(FsError::InvalidParam),
            "bit {bit:#x}"
        );
    }
    // A pipe this card does not have: the high-CRTC field past 0, or
    // SECONDARY (pipe 1).
    for typ in [
        RELATIVE | high_crtc(1),
        RELATIVE | high_crtc(31),
        RELATIVE | _DRM_VBLANK_SECONDARY,
        RELATIVE | _DRM_VBLANK_EVENT | _DRM_VBLANK_SECONDARY,
        RELATIVE | _DRM_VBLANK_EVENT | high_crtc(1),
    ] {
        assert_eq!(wait(&c, typ, 0), Err(FsError::InvalidParam), "{typ:#x}");
    }
    // Refused before anything is scheduled: the fd carries nothing (the
    // one accepted event above is owed at the next vblank, which no
    // timer delivers here).
    let mut buf = [0u8; 128];
    assert_eq!(c.read_events(&mut buf), Err(FsError::Again));
}

/// The event form answers with the vblank the event was queued for:
/// the resolved target, or the current count when the target had
/// already passed (`drm_queue_vblank_event`). Xorg's modesetting
/// driver keeps that as the MSC it queued; this arm left the request's
/// own sequence in place, so a relative "+2" read back as 2.
#[test]
fn the_event_form_replies_with_the_vblank_it_queued() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    const EVENT: u32 = _DRM_VBLANK_EVENT;
    // The counter runs on the clock, so bracket each reply between two
    // readings rather than pinning it to one.
    let bracket = |typ: u32, seq: u32| {
        let before = wait(&c, RELATIVE, 0).expect("now");
        let got = wait(&c, typ, seq).expect("the event form");
        let after = wait(&c, RELATIVE, 0).expect("now");
        (before, got, after)
    };
    let within = |lo: u32, got: u32, hi: u32| {
        (got.wrapping_sub(lo) as i32) >= 0 && (hi.wrapping_sub(got) as i32) >= 0
    };

    // Relative +2: two past the count at the time.
    let (b, got, a) = bracket(RELATIVE | EVENT, 2);
    assert!(
        within(b + 2, got, a + 2),
        "relative +2 replied {} at {}..{}",
        got,
        b,
        a
    );
    // Absolute, ahead: exactly that.
    let now = wait(&c, RELATIVE, 0).expect("now");
    assert_eq!(wait(&c, EVENT, now + 50), Ok(now + 50));
    // Absolute, already passed: the current count, like Linux, which
    // sends the event at once in that case.
    let (b, got, a) = bracket(EVENT, now.wrapping_sub(3));
    assert!(
        within(b, got, a),
        "a passed target replied {} at {}..{}",
        got,
        b,
        a
    );
    // Passed with NEXTONMISS: the target moved to the next vblank.
    let (b, got, a) = bracket(EVENT | _DRM_VBLANK_NEXTONMISS_FLAG, now.wrapping_sub(3));
    assert!(
        within(b + 1, got, a + 1),
        "NEXTONMISS replied {} at {}..{}",
        got,
        b,
        a
    );
}

/// With two heads (two drivers that own scanout, each with a CRTC of
/// its own), pipe 1 exists -- by the high-CRTC field or as SECONDARY --
/// and pipe 2 does not. What separates reading the field from merely
/// refusing anything in it.
#[test]
fn a_second_head_makes_pipe_one_a_real_pipe_and_pipe_two_still_not() {
    let screen = kms_emu::attach(64, 16);
    let _a = screen.attach_gpu(EmuGpu::hardware_kms("emu-a").with_ids(40, 41, 42));
    let _b = screen.attach_gpu(EmuGpu::hardware_kms("emu-b").with_ids(60, 61, 62));
    let c = Client::open(0);
    assert_eq!(drm::crtc_count(), 2);

    assert!(wait(&c, RELATIVE, 0).is_ok());
    assert!(
        wait(&c, RELATIVE | high_crtc(1), 0).is_ok(),
        "pipe 1 by the field"
    );
    assert!(
        wait(&c, RELATIVE | _DRM_VBLANK_SECONDARY, 0).is_ok(),
        "pipe 1 as SECONDARY"
    );
    for typ in [
        RELATIVE | high_crtc(2),
        RELATIVE | high_crtc(31),
        RELATIVE | _DRM_VBLANK_SECONDARY | high_crtc(2),
    ] {
        assert_eq!(wait(&c, typ, 0), Err(FsError::InvalidParam), "{typ:#x}");
    }
}

/// A blocking wait whose vblank does not come within the cap (Linux's 3 s,
/// `DRM_WAIT_ON`) answers EBUSY with the count there is, as
/// `drm_wait_vblank_ioctl` does -- the reply filled all the same, since
/// `drm_ioctl` copies it out whatever the code. This arm answered 0 with
/// that count, so a client pacing on the ioctl (X's Present, SDL) took the
/// stall for its vblank and paced on from a sequence it never reached.
///
/// The mark is the request's own: a query another thread of the same open
/// makes in between is not the wait that was stopped, and the same struct
/// asked again is a fresh wait. The sleep is the one `sys_ioctl` awaits,
/// run here with its cap brought down to 40 ms.
#[test]
fn a_blocking_wait_the_cap_stops_is_ebusy_with_the_count_there_is() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let within = |lo: u32, got: u32, hi: u32| {
        (got.wrapping_sub(lo) as i32) >= 0 && (hi.wrapping_sub(got) as i32) >= 0
    };
    let sleep = |req: &mut DrmWaitVblank, cap: Duration| {
        let data = req as *mut DrmWaitVblank as usize;
        async_std::task::block_on(c.dev().wait_vblank_sleep_capped(data, cap))
            .expect("no signal can reach this sleep");
    };
    let now = wait(&c, RELATIVE, 0).expect("now");
    // Absolute, a thousand vblanks out: sixteen seconds, past any cap.
    let far = now.wrapping_add(1000);
    let mut req = DrmWaitVblank {
        typ: 0,
        sequence: far,
        val1: 0,
        val2: 0,
    };
    let before = kernel_hal::timer::timer_now();
    let cap = before + Duration::from_millis(40);
    sleep(&mut req, cap);
    assert!(
        kernel_hal::timer::timer_now() >= cap,
        "the sleep ended before its cap"
    );
    // Another thread's query in between: answered as ever, mark untouched.
    assert!(wait(&c, RELATIVE, 0).is_ok());
    // The stopped request: EBUSY, the reply filled with the count there is
    // and the time, exactly as the plain answer would be.
    assert_eq!(c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req), Err(FsError::Busy));
    let after = kernel_hal::timer::timer_now();
    let now2 = wait(&c, RELATIVE, 0).expect("now");
    assert_eq!(req.typ, 0, "_DRM_VBLANK_ABSOLUTE");
    assert!(
        within(now, req.sequence, now2),
        "replied {} at {}..{}",
        req.sequence,
        now,
        now2
    );
    let stamp = Duration::new(req.val1, 0) + Duration::from_micros(req.val2);
    assert!(
        stamp >= before.saturating_sub(Duration::from_micros(1)) && stamp <= after,
        "stamped {:?} outside {:?}..{:?}",
        stamp,
        before,
        after
    );
    // Taken once: the same struct asked again (its target still ahead) is
    // the plain answer.
    req.typ = 0;
    req.sequence = far;
    assert!(c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req).is_ok());
    // A wait the vblank ends in time leaves no mark: one vblank ahead
    // (16.7 ms) under a cap of a second.
    let now = wait(&c, RELATIVE, 0).expect("now");
    let mut soon = DrmWaitVblank {
        typ: 0,
        sequence: now.wrapping_add(1),
        val1: 0,
        val2: 0,
    };
    let cap = kernel_hal::timer::timer_now() + Duration::from_secs(1);
    sleep(&mut soon, cap);
    assert!(
        kernel_hal::timer::timer_now() < cap,
        "slept to the cap, not to the vblank"
    );
    assert!(c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut soon).is_ok());
    assert!(
        (soon.sequence.wrapping_sub(now.wrapping_add(1)) as i32) >= 0,
        "the vblank asked for had not come: {} < {}",
        soon.sequence,
        now.wrapping_add(1)
    );
}

/// The CRTC off -- `set_crtc_blanked`, what DPMS off, SETCRTC without a
/// mode and RMFB of the scanout do -- and back on when dropped, panic or
/// not, so a failed assertion does not leave the next test on a dark pipe.
struct CrtcOff;

impl CrtcOff {
    fn now() -> Self {
        drm::set_crtc_blanked(true);
        CrtcOff
    }

    /// The CRTC goes off from another thread `after` this call: a wait
    /// parked in the sleep by then sees the pipe go off under it.
    fn after(delay: Duration) -> (Self, std::thread::JoinHandle<()>) {
        let off = std::thread::spawn(move || {
            std::thread::sleep(delay);
            drm::set_crtc_blanked(true);
        });
        (CrtcOff, off)
    }
}

impl Drop for CrtcOff {
    fn drop(&mut self) {
        drm::set_crtc_blanked(false);
    }
}

/// With the CRTC off (DPMS off, SETCRTC without a mode, RMFB of its
/// scanout: what `drm_crtc_vblank_off` turns off) `drm_vblank_get` is
/// EINVAL: WAIT_VBLANK in both forms, after the type and pipe checks;
/// CRTC_GET_SEQUENCE after its ENOENT; CRTC_QUEUE_SEQUENCE after ENOENT
/// and the flags. All three served the counter here, so a client paced
/// on a pipe that was off as if it were on. The sleep `sys_ioctl` awaits
/// returns at once: there is nothing to wait for. On again, all three
/// answer as ever, the counter not having stopped. (Xorg's modesetting
/// driver probes GET_SEQUENCE once at start, with the CRTC on; told
/// EINVAL later it stays on WAIT_VBLANK, as on Linux.)
#[test]
fn with_the_crtc_off_no_vblank_can_be_held() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let crtc = drm::SYNTH_CRTC_ID;
    let now = wait(&c, RELATIVE, 0).expect("on: the counter");
    let off = CrtcOff::now();
    assert_eq!(
        wait(&c, RELATIVE, 0),
        Err(FsError::InvalidParam),
        "the query"
    );
    assert_eq!(
        wait(&c, RELATIVE | _DRM_VBLANK_EVENT, 1),
        Err(FsError::InvalidParam),
        "the event form"
    );
    assert_eq!(
        wait(&c, RELATIVE | high_crtc(2), 0),
        Err(FsError::InvalidParam),
        "a missing pipe, still"
    );
    // The blocking form: the sleep returns at once, and the arm refuses.
    let mut req = DrmWaitVblank {
        typ: 0,
        sequence: now.wrapping_add(1000),
        val1: 0,
        val2: 0,
    };
    let before = kernel_hal::timer::timer_now();
    let cap = before + Duration::from_secs(1);
    let data = &mut req as *mut DrmWaitVblank as usize;
    async_std::task::block_on(c.dev().wait_vblank_sleep_capped(data, cap))
        .expect("no signal can reach this sleep");
    assert!(
        kernel_hal::timer::timer_now() < before + Duration::from_millis(500),
        "the sleep parked on a pipe that is off"
    );
    assert_eq!(
        c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req),
        Err(FsError::InvalidParam)
    );
    // GET_SEQUENCE: the lookup first, then the pipe.
    assert_eq!(get_sequence(&c, 0xC0FFEE), Err(FsError::EntryNotFound));
    assert_eq!(get_sequence(&c, crtc), Err(FsError::InvalidParam));
    // QUEUE_SEQUENCE: the lookup, the flags, then the pipe.
    assert_eq!(
        queue(&c, 0xC0FFEE, RELATIVE, 1, 0),
        Err(FsError::EntryNotFound)
    );
    assert_eq!(
        queue(&c, crtc, RELATIVE, 1, 0xabc),
        Err(FsError::InvalidParam)
    );
    drop(off);
    let later = wait(&c, RELATIVE, 0).expect("on again");
    assert!(
        (later.wrapping_sub(now) as i32) >= 0,
        "the counter ran backwards: {} after {}",
        later,
        now
    );
    assert_eq!(wait(&c, RELATIVE | _DRM_VBLANK_EVENT, 1), Ok(later + 1));
    assert_eq!(
        get_sequence(&c, crtc).map(|(active, _, _)| active),
        Ok(1),
        "the mode is still up"
    );
    assert!(queue(&c, crtc, RELATIVE, 1, 0).is_ok());
}

/// A blocking wait parked when the CRTC goes off ends there and then with
/// 0 and the count there is, ahead of the cap: `drm_crtc_vblank_off`
/// wakes the waiters and `wait_event_interruptible_timeout` ends on
/// `!vblank->enabled`. The mark is the request's own and taken before the
/// arm looks at the CRTC: a query another thread makes in between is the
/// EINVAL an off pipe gets, and the same struct asked again, the pipe
/// still off, is EINVAL too.
#[test]
fn a_wait_the_crtc_goes_off_under_ends_with_the_count_there_is() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let now = wait(&c, RELATIVE, 0).expect("now");
    let far = now.wrapping_add(1000);
    let mut req = DrmWaitVblank {
        typ: 0,
        sequence: far,
        val1: 0,
        val2: 0,
    };
    let before = kernel_hal::timer::timer_now();
    let cap = before + Duration::from_secs(1);
    let (off, going_off) = CrtcOff::after(Duration::from_millis(5));
    let data = &mut req as *mut DrmWaitVblank as usize;
    async_std::task::block_on(c.dev().wait_vblank_sleep_capped(data, cap))
        .expect("no signal can reach this sleep");
    let ended = kernel_hal::timer::timer_now();
    going_off
        .join()
        .expect("the thread that turned the CRTC off");
    assert!(drm::crtc_blanked(), "the CRTC went off under the wait");
    assert!(
        ended < before + Duration::from_millis(500),
        "the wait ran on after the CRTC went off: {:?}",
        ended - before
    );
    // Another request in between: an off pipe's EINVAL, the mark not its.
    assert_eq!(wait(&c, RELATIVE, 0), Err(FsError::InvalidParam));
    // The wait that was cut short: 0, with the count there is.
    assert_eq!(c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req), Ok(0));
    let now2 = drm::vblank_seq_now();
    assert_eq!(req.typ, 0, "_DRM_VBLANK_ABSOLUTE");
    assert!(
        (req.sequence.wrapping_sub(now) as i32) >= 0
            && (now2.wrapping_sub(req.sequence) as i32) >= 0,
        "replied {} at {}..{}",
        req.sequence,
        now,
        now2
    );
    assert!(
        (far.wrapping_sub(req.sequence) as i32) > 0,
        "the vblank asked for never came"
    );
    // Taken once: the same struct again, the pipe still off, is EINVAL.
    req.typ = 0;
    req.sequence = far;
    assert_eq!(
        c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req),
        Err(FsError::InvalidParam)
    );
    drop(off);
    assert!(wait(&c, RELATIVE, 0).is_ok(), "on again");
}

/// The events queued for a vblank ahead -- `WAIT_VBLANK` in its event
/// form and `CRTC_QUEUE_SEQUENCE` -- arrive the moment the CRTC goes
/// off, each with the count there is, not with the vblank they named:
/// `drm_crtc_vblank_off` sends them at once ("Sending premature vblank
/// event on disable"), since a pipe that is off has no vblank coming.
/// Here they kept waiting, and a client paced on the event waited with
/// them for as long as the pipe stayed off.
#[test]
fn the_crtc_going_off_delivers_the_queued_events_at_once() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let crtc = drm::SYNTH_CRTC_ID;
    let now = wait(&c, RELATIVE, 0).expect("now");
    let asked = wait(&c, RELATIVE | _DRM_VBLANK_EVENT, 1_000).expect("the event form");
    let queued = queue(&c, crtc, RELATIVE, 1_000, 0x5EC0_0FF0).expect("queued");
    assert!(
        (asked.wrapping_sub(now) as i32) >= 1_000 && queued >= asked as u64,
        "both named a vblank far ahead: {} and {} at {}",
        asked,
        queued,
        now
    );
    assert!(
        !c.file_state().has_events(),
        "an event before its vblank, the pipe on"
    );
    let off = CrtcOff::now();
    let now2 = drm::vblank_seq_now();
    let mut buf = [0u8; 96];
    assert_eq!(
        c.read_events(&mut buf),
        Ok(64),
        "the two events, at once, and nothing else"
    );
    let le32 = |at: usize| u32::from_ne_bytes(buf[at..at + 4].try_into().expect("4 bytes"));
    let le64 = |at: usize| u64::from_ne_bytes(buf[at..at + 8].try_into().expect("8 bytes"));
    assert_eq!(le32(0), 1, "DRM_EVENT_VBLANK");
    let seq = le32(24);
    assert!(
        (seq.wrapping_sub(now) as i32) >= 0 && (now2.wrapping_sub(seq) as i32) >= 0,
        "the vblank event carries {}, not a count in {}..{}",
        seq,
        now,
        now2
    );
    assert!(
        (asked.wrapping_sub(seq) as i32) > 0,
        "the vblank asked for never came"
    );
    assert_eq!(le32(32), 3, "DRM_EVENT_CRTC_SEQUENCE");
    assert_eq!(le64(40), 0x5EC0_0FF0, "its user_data");
    assert_eq!(le64(56), seq as u64, "the same count");
    // Nothing more comes: the pipe is off, and new requests are EINVAL.
    assert!(!c.file_state().has_events());
    assert_eq!(
        wait(&c, RELATIVE | _DRM_VBLANK_EVENT, 1),
        Err(FsError::InvalidParam)
    );
    drop(off);
    assert!(wait(&c, RELATIVE, 0).is_ok(), "on again");
}
