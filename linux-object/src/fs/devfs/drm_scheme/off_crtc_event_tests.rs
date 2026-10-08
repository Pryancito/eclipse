//! `ATOMIC` with `PAGE_FLIP_EVENT` on a CRTC that is off and stays off.
//! `drm_atomic_crtc_check` refuses it with EINVAL on purpose: a client
//! asking to be woken for a frame on a suspended pipe is taken to have a
//! bug in its frame loop, the same answer WAIT_VBLANK and the legacy page
//! flip give on a disabled pipe. This scheduled the event anyway.
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

fn mode_blob(c: &super::gl_client_sequence_tests::Client) -> u32 {
    let mode = make_modeinfo(32, 8);
    let mut blob = DrmModeCreateBlob {
        data: mode.as_ptr() as u64,
        length: mode.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
        .expect("CREATEPROPBLOB");
    blob.blob_id
}

fn active(on: u64) -> Request {
    Request::new(&[drm::SYNTH_CRTC_ID], &[1], &[PROP_ACTIVE], &[on])
}

/// `drm_mode_getcrtc` reads `crtc_state->enable`, which `MODE_ID` sets
/// and unsets (`drm_atomic_set_mode_prop_for_crtc`): after a commit with
/// `MODE_ID = 0` the CRTC answers `mode_valid = 0` and the encoder names
/// no CRTC, until a commit sets a mode again; `ACTIVE = 0` on its own
/// keeps the mode. Here `mode_valid` stayed 1 through all of it.
#[test]
fn a_commit_that_unsets_the_mode_leaves_the_crtc_with_none() {
    let (_screen, c) = atomic_client(32, 8);
    let blob = mode_blob(&c);
    let pipe = || {
        let mut crtc: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
        crtc.crtc_id = drm::SYNTH_CRTC_ID;
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
        let mut enc: DrmModeGetEncoder = unsafe { core::mem::zeroed() };
        enc.encoder_id = drm::SYNTH_ENCODER_ID;
        c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc)
            .expect("GETENCODER");
        (crtc.mode_valid, enc.crtc_id)
    };
    let modeset = |mode: u64, on: u64| {
        Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_MODE_ID, PROP_ACTIVE],
            &[mode, on],
        )
    };
    let on = (1, drm::SYNTH_CRTC_ID);
    let off = (0, 0);

    commit(&c, &modeset(blob as u64, 1), DRM_MODE_ATOMIC_ALLOW_MODESET)
        .expect("a modeset with the panel's mode");
    assert_eq!(pipe(), on, "with a mode set");
    commit(&c, &modeset(0, 0), DRM_MODE_ATOMIC_ALLOW_MODESET).expect("MODE_ID = 0");
    assert_eq!(pipe(), off, "after MODE_ID = 0");
    commit(&c, &modeset(blob as u64, 1), DRM_MODE_ATOMIC_ALLOW_MODESET).expect("the mode again");
    assert_eq!(pipe(), on, "after the mode is set again");
    commit(&c, &active(0), DRM_MODE_ATOMIC_ALLOW_MODESET).expect("ACTIVE = 0");
    assert_eq!(pipe(), on, "ACTIVE = 0 keeps the mode");
}

/// One event read off the fd, or none.
fn events(c: &super::gl_client_sequence_tests::Client) -> usize {
    drm::flush_pending_flip_completions();
    let mut buf = [0u8; 64];
    match c.read_events(&mut buf) {
        Ok(n) => n / 32,
        Err(FsError::Again) => 0,
        Err(e) => panic!("read: {:?}", e),
    }
}

#[test]
fn an_event_on_a_crtc_that_is_off_and_stays_off_is_refused() {
    let (_screen, c) = atomic_client(32, 8);
    const EVENT: u32 = DRM_MODE_PAGE_FLIP_EVENT;
    const MODESET: u32 = DRM_MODE_ATOMIC_ALLOW_MODESET;

    // A fresh CRTC is off. Leaving it off is fine; asking for an event
    // while doing so is not, whether the commit names the CRTC or only
    // its plane (whose state drags the CRTC's in, on Linux).
    assert_eq!(commit(&c, &active(0), 0), Ok(0));
    assert_eq!(commit(&c, &active(0), MODESET), Ok(0));
    assert_eq!(commit(&c, &active(0), EVENT), Err(FsError::InvalidParam));
    assert_eq!(
        commit(&c, &active(0), EVENT | MODESET),
        Err(FsError::InvalidParam)
    );
    let plane_only = Request::new(&[drm::SYNTH_PLANE_ID], &[1], &[PROP_CRTC_X], &[0]);
    assert_eq!(commit(&c, &plane_only, 0), Ok(0));
    assert_eq!(
        commit(&c, &plane_only, EVENT),
        Err(FsError::InvalidParam),
        "a plane update with an event on an off CRTC"
    );
    assert_eq!(events(&c), 0, "a refused commit queued its event");
    // TEST_ONLY carries no event, so it is not refused for this reason
    // (the TEST_ONLY|EVENT combination is refused earlier, on its own).
    assert_eq!(commit(&c, &active(0), DRM_MODE_ATOMIC_TEST_ONLY), Ok(0));

    // Turning it on with an event: allowed, and the event comes.
    let blob = mode_blob(&c);
    let on = Request::new(
        &[drm::SYNTH_CRTC_ID],
        &[2],
        &[PROP_MODE_ID, PROP_ACTIVE],
        &[u64::from(blob), 1],
    );
    assert_eq!(commit(&c, &on, EVENT | MODESET), Ok(0));
    assert_eq!(events(&c), 1);
    // On and staying on: the ordinary frame.
    assert_eq!(commit(&c, &active(1), EVENT), Ok(0));
    assert_eq!(events(&c), 1);
    // Turning it off with an event: allowed too (it was on), and the
    // event comes.
    assert_eq!(commit(&c, &active(0), EVENT | MODESET), Ok(0));
    assert_eq!(events(&c), 1);
    // Off and staying off again: refused again, nothing queued.
    assert_eq!(commit(&c, &active(0), EVENT), Err(FsError::InvalidParam));
    assert_eq!(commit(&c, &plane_only, EVENT), Err(FsError::InvalidParam));
    assert_eq!(events(&c), 0);
}
