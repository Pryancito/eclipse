//! `drm_atomic_connector_commit_dpms`: a DPMS write on a connector routed
//! to a CRTC moves the CRTC's `active` and commits it, so DPMS off is
//! `ACTIVE = 0` and DPMS on `ACTIVE = 1` with the plane's fb back on the
//! panel; with no CRTC routed only the connector's value changes. And a
//! CRTC going active again -- by DPMS or by an atomic `ACTIVE = 1` with
//! no new fb -- shows its plane's fb (`commit_modeset_enables`). DPMS
//! only blanked here, `ACTIVE` never moved, and a CRTC turned back on
//! stayed black until the next frame.
use super::gl_client_sequence_tests::Client;
use super::hw_kms_tests::paint;
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

const CRTC: u32 = drm::SYNTH_CRTC_ID;
const CONN: u32 = drm::SYNTH_CONNECTOR_ID;
const DPMS_OFF: u64 = 3;
const DPMS_STANDBY: u64 = 1;
const RED: u32 = 0x00ff_0000;

fn active() -> u64 {
    crtc_props(true)
        .into_iter()
        .find(|&(id, _)| id == PROP_ACTIVE)
        .expect("ACTIVE")
        .1
}

fn dpms_readback() -> u64 {
    connector_props(CONN, true)
        .into_iter()
        .find(|&(id, _)| id == PROP_DPMS)
        .expect("DPMS")
        .1
}

fn dpms(c: &Client, value: u64) -> Result<usize> {
    let mut req = DrmModeObjSetProperty {
        value,
        prop_id: PROP_DPMS,
        obj_id: CONN,
        obj_type: DRM_MODE_OBJECT_CONNECTOR,
    };
    c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut req)
}

/// Legacy modeset onto `fb`, which routes the connector.
fn set_crtc(c: &Client, fb: u32) {
    let conn = [CONN];
    let mut req = DrmModeGetCrtc {
        set_connectors_ptr: conn.as_ptr() as u64,
        count_connectors: 1,
        crtc_id: CRTC,
        fb_id: fb,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 1,
        mode: make_modeinfo(32, 8),
    };
    c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req).expect("SETCRTC");
}

fn active_req(value: u64) -> Request {
    Request::new(&[CRTC], &[1], &[PROP_ACTIVE], &[value])
}

#[test]
fn dpms_moves_active_of_the_routed_crtc_and_relights_the_plane_fb() {
    let (screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    paint(&buf, RED);
    let fb = c.addfb2(&buf);
    set_crtc(&c, fb);
    assert_eq!(active(), 1);
    assert_eq!(screen.pixel(3, 2), RED);

    assert_eq!(dpms(&c, DPMS_OFF), Ok(0));
    assert_eq!(active(), 0, "crtc_state->active = false");
    assert!(drm::crtc_blanked());
    assert_eq!(dpms_readback(), DPMS_OFF);
    assert_eq!(screen.pixel(3, 2), 0, "the panel is dark");
    // A commit that leaves the CRTC as DPMS left it is no change; one
    // that turns it back on is the modeset it is in Linux.
    assert_eq!(
        commit(&c, &active_req(0), 0),
        Ok(0),
        "ACTIVE 0 on an inactive CRTC"
    );
    assert_eq!(
        commit(&c, &active_req(1), 0),
        Err(FsError::InvalidParam),
        "ACTIVE 1 after DPMS off needs ALLOW_MODESET"
    );
    assert_eq!(active(), 0);

    // Anything but ON is OFF.
    assert_eq!(dpms(&c, DPMS_STANDBY), Ok(0));
    assert_eq!(dpms_readback(), DPMS_OFF);
    assert_eq!(active(), 0);

    assert_eq!(dpms(&c, DRM_MODE_DPMS_ON), Ok(0));
    assert_eq!(active(), 1, "crtc_state->active = true");
    assert!(!drm::crtc_blanked());
    assert_eq!(dpms_readback(), DRM_MODE_DPMS_ON);
    assert_eq!(
        screen.pixel(3, 2),
        RED,
        "the plane's fb is back without a new present"
    );
    assert_eq!(
        commit(&c, &active_req(1), 0),
        Ok(0),
        "and ACTIVE 1 is no change again"
    );
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

#[test]
fn dpms_with_no_crtc_routed_moves_the_connector_only() {
    let (screen, c) = atomic_client(32, 8);
    // A flip with no modeset: the panel shows the fb, nothing is routed.
    let buf = c.create_dumb(32, 8);
    paint(&buf, RED);
    let fb = c.addfb2(&buf);
    let mut flip: DrmModeCrtcPageFlip = unsafe { core::mem::zeroed() };
    flip.crtc_id = CRTC;
    flip.fb_id = fb;
    c.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip)
        .expect("PAGE_FLIP");
    drm::flush_pending_flip_completions();
    assert_eq!(screen.pixel(3, 2), RED);
    assert_eq!(active(), 0, "no mode: not active");

    assert_eq!(dpms(&c, DPMS_OFF), Ok(0));
    assert_eq!(dpms_readback(), DPMS_OFF);
    assert_eq!(active(), 0, "no CRTC to move");
    assert_eq!(screen.pixel(3, 2), 0, "the panel still obeys");
    assert_eq!(dpms(&c, DRM_MODE_DPMS_ON), Ok(0));
    assert_eq!(dpms_readback(), DRM_MODE_DPMS_ON);
    assert_eq!(
        active(),
        0,
        "no CRTC: ACTIVE untouched, so no active-without-mode"
    );
    assert_eq!(screen.pixel(3, 2), RED, "and shows the fb it had");
    // The state is still one the check accepts as is.
    assert_eq!(commit(&c, &active_req(0), 0), Ok(0));

    // With no fb on the CRTC there is nothing to show, and DPMS on still
    // un-blanks: the next present is not owed a black frame.
    c.rmfb(fb).expect("RMFB");
    assert!(drm::crtc_blanked(), "RMFB of the scanout fb blanks");
    assert_eq!(dpms(&c, DRM_MODE_DPMS_ON), Ok(0));
    assert!(!drm::crtc_blanked(), "DPMS on with no fb un-blanks");
    assert_eq!(dpms_readback(), DRM_MODE_DPMS_ON);
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

#[test]
fn an_atomic_active_1_with_no_new_fb_shows_the_plane_fb_again() {
    let (screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    paint(&buf, RED);
    let fb = c.addfb2(&buf);
    set_crtc(&c, fb);
    assert_eq!(screen.pixel(3, 2), RED);

    let modeset = DRM_MODE_ATOMIC_ALLOW_MODESET;
    assert_eq!(commit(&c, &active_req(0), modeset), Ok(0));
    assert_eq!(screen.pixel(3, 2), 0, "ACTIVE 0 blanks");
    assert_eq!(dpms_readback(), DPMS_OFF, "and DPMS reads Off");
    assert_eq!(
        commit(&c, &active_req(1), DRM_MODE_ATOMIC_TEST_ONLY | modeset),
        Ok(0)
    );
    assert_eq!(screen.pixel(3, 2), 0, "TEST_ONLY shows nothing");
    assert_eq!(commit(&c, &active_req(1), modeset), Ok(0));
    assert_eq!(screen.pixel(3, 2), RED, "ACTIVE 1 relights the plane's fb");
    assert_eq!(dpms_readback(), DRM_MODE_DPMS_ON);
    // ACTIVE 1 on an already active CRTC presents nothing again.
    screen.repaint(0);
    assert_eq!(commit(&c, &active_req(1), 0), Ok(0));
    assert_eq!(screen.pixel(3, 2), 0, "no relight without a 0 -> 1 edge");
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}
