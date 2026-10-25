//! `CRTC_GET_SEQUENCE` and `CRTC_QUEUE_SEQUENCE` (`drm_vblank.c`), the
//! two ioctls Xorg's modesetting driver tries before `WAIT_VBLANK`
//! (`ms_get_crtc_ust_msc`, `ms_queue_vblank`). Neither existed: both
//! were the unhandled-ioctl ENOTTY, so Xorg fell back on every query.
use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu;

const RELATIVE: u32 = 0x1;
const NEXT_ON_MISS: u32 = 0x2;

/// `drmCrtcGetSequence`: (active, sequence, sequence_ns).
pub(super) fn get_sequence(c: &Client, crtc_id: u32) -> Result<(u32, u64, i64)> {
    let mut req = DrmCrtcGetSequence {
        crtc_id,
        active: 0xdead_beef,
        sequence: u64::MAX,
        sequence_ns: -1,
    };
    c.ioctl(DRM_IOCTL_CRTC_GET_SEQUENCE, &mut req)?;
    assert_eq!(req.crtc_id, crtc_id, "crtc_id is not an out field");
    Ok((req.active, req.sequence, req.sequence_ns))
}

/// `drmCrtcQueueSequence`: the sequence the event was queued for.
pub(super) fn queue(
    c: &Client,
    crtc_id: u32,
    flags: u32,
    sequence: u64,
    user_data: u64,
) -> Result<u64> {
    let mut req = DrmCrtcQueueSequence {
        crtc_id,
        flags,
        sequence,
        user_data,
    };
    c.ioctl(DRM_IOCTL_CRTC_QUEUE_SEQUENCE, &mut req)
        .map(|_| req.sequence)
}

/// The counter as `WAIT_VBLANK` reports it (relative 0: no wait).
fn vblank_now(c: &Client) -> u32 {
    let mut req = DrmWaitVblank {
        typ: 0x1,
        sequence: 0,
        val1: 0,
        val2: 0,
    };
    c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req)
        .expect("WAIT_VBLANK");
    req.sequence
}

fn within(lo: u32, got: u32, hi: u32) -> bool {
    (got.wrapping_sub(lo) as i32) >= 0 && (hi.wrapping_sub(got) as i32) >= 0
}

fn crtc_req(crtc_id: u32) -> DrmModeGetCrtc {
    DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    }
}

/// `GETCRTC`'s `mode_valid`: the `crtc_state->enable` `active` reads.
fn mode_valid(c: &Client, crtc_id: u32) -> u32 {
    let mut req = crtc_req(crtc_id);
    c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut req).expect("GETCRTC");
    req.mode_valid
}

/// `drmModeSetCrtc(fd, crtc, 0, 0, 0, NULL, 0, NULL)`: the pipe off.
fn set_crtc_off(c: &Client, crtc_id: u32) {
    let mut req = crtc_req(crtc_id);
    c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
        .expect("SETCRTC off");
}

#[test]
fn get_sequence_reports_the_counter_the_vblanks_time_and_whether_the_pipe_is_on() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let crtc = drm::SYNTH_CRTC_ID;
    assert_eq!(
        get_sequence(&c, 0xC0FFEE),
        Err(FsError::EntryNotFound),
        "an unknown CRTC"
    );

    let before = vblank_now(&c);
    let (active, seq, ns) = get_sequence(&c, crtc).expect("GET_SEQUENCE");
    let after = vblank_now(&c);
    assert!(
        seq <= u32::MAX as u64 && within(before, seq as u32, after),
        "sequence {} is not WAIT_VBLANK's {}..{}",
        seq,
        before,
        after
    );
    let now_ns = kernel_hal::timer::timer_now().as_nanos() as i64;
    assert!(
        ns > 0 && ns <= now_ns && now_ns - ns < 1_000_000_000,
        "sequence_ns {} is not the last vblank's (now {})",
        ns,
        now_ns
    );
    assert_eq!(mode_valid(&c, crtc), 1, "a fresh panel has its mode up");
    assert_eq!(active, 1, "`active` is `crtc_state->enable`");

    // The pipe off: `drm_crtc_vblank_off` ran, and `drm_crtc_vblank_get`
    // is EINVAL from then on. `active = 0` with the counter, which this
    // answered, is not a reply Linux gives (`disable_outputs` checks the
    // driver turned the vblank off). The counter itself does not stop:
    // lit again (DPMS on, the mode still gone), it is ahead.
    set_crtc_off(&c, crtc);
    assert_eq!(mode_valid(&c, crtc), 0, "the pipe is off");
    assert_eq!(
        get_sequence(&c, crtc),
        Err(FsError::InvalidParam),
        "`drm_vblank_get` with the pipe off"
    );
    drm::set_crtc_blanked(false);
    let (active, seq_on, _) = get_sequence(&c, crtc).expect("GET_SEQUENCE, lit again");
    assert_eq!(active, 0, "`active` is the mode, which SETCRTC took away");
    assert!(
        ((seq_on as u32).wrapping_sub(seq as u32) as i32) >= 0,
        "the counter ran backwards: {} after {}",
        seq_on,
        seq
    );
}

#[test]
fn queue_sequence_checks_the_crtc_then_the_flags_and_replies_with_the_sequence_it_queued() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let crtc = drm::SYNTH_CRTC_ID;
    // The lookup comes first (`drm_crtc_find`, then the flag check).
    assert_eq!(
        queue(&c, 0xC0FFEE, 0x4, 0, 0),
        Err(FsError::EntryNotFound),
        "an unknown CRTC, even with bad flags"
    );
    for flags in [0x4, 0x8, 0x8000_0000, RELATIVE | 0x4] {
        assert_eq!(
            queue(&c, crtc, flags, 0, 0),
            Err(FsError::InvalidParam),
            "flags {flags:#x}"
        );
    }

    // A far target: queued for exactly that, and no event until then.
    let now = vblank_now(&c);
    let far = now.wrapping_add(5_000) as u64;
    assert_eq!(queue(&c, crtc, 0, far, 0xA), Ok(far));
    let mut buf = [0u8; 32];
    assert_eq!(
        c.read_events(&mut buf),
        Err(FsError::Again),
        "an event before its vblank"
    );

    // Relative: counted from now. The counter runs on the clock, so each
    // reply is bracketed between two readings rather than pinned.
    let b = vblank_now(&c);
    let got = queue(&c, crtc, RELATIVE, 2, 0xB).expect("relative +2");
    let a = vblank_now(&c);
    assert!(
        within(b + 2, got as u32, a + 2),
        "relative +2 replied {} at {}..{}",
        got,
        b,
        a
    );
    // Passed: the current count (Linux sends that event at once; here it
    // comes with the next vblank).
    let b = vblank_now(&c);
    let got = queue(&c, crtc, 0, now.wrapping_sub(3) as u64, 0xC).expect("passed");
    let a = vblank_now(&c);
    assert!(
        within(b, got as u32, a),
        "a passed target replied {} at {}..{}",
        got,
        b,
        a
    );
    // Passed with NEXT_ON_MISS: the next vblank.
    let b = vblank_now(&c);
    let got = queue(&c, crtc, NEXT_ON_MISS, now.wrapping_sub(3) as u64, 0xD).expect("missed");
    let a = vblank_now(&c);
    assert!(
        within(b + 1, got as u32, a + 1),
        "NEXT_ON_MISS replied {} at {}..{}",
        got,
        b,
        a
    );
    // Relative 0 is "now", which has passed: with NEXT_ON_MISS, the next.
    let b = vblank_now(&c);
    let got = queue(&c, crtc, RELATIVE | NEXT_ON_MISS, 0, 0xE).expect("relative 0, missed");
    let a = vblank_now(&c);
    assert!(
        within(b + 1, got as u32, a + 1),
        "RELATIVE | NEXT_ON_MISS 0 replied {} at {}..{}",
        got,
        b,
        a
    );
}
