//! What `WAIT_VBLANK` refuses, driven through the ioctl entry point the
//! way `drmWaitVBlank` drives it: `_DRM_VBLANK_SIGNAL`, a bit outside
//! the masks, and a pipe the card does not have, each EINVAL in
//! `drm_wait_vblank_ioctl` before the sequence is even looked at. This
//! arm answered all of them with pipe 0's counter.
use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

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
