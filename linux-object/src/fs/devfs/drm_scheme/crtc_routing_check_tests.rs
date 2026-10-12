//! `drm_atomic_crtc_check` and `drm_atomic_helper_check_modeset` on the
//! CRTC's resulting state: ACTIVE without a mode is "active without
//! enabled" (EINVAL) whichever property the commit names, and
//! `crtc_state->enable` must agree with the connectors routed to the
//! CRTC ("enabled/connectors mismatch", EINVAL), so a mode is set with
//! the connector's CRTC_ID and unset with it. This accepted a mode with
//! no connector, a connector with no mode, MODE_ID = 0 alone on an
//! active CRTC, and read the connector's CRTC_ID back from ACTIVE.
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

const CRTC: u32 = drm::SYNTH_CRTC_ID;
const CONN: u32 = drm::SYNTH_CONNECTOR_ID;

fn connector_crtc() -> u64 {
    connector_props(CONN, true)
        .into_iter()
        .find(|&(id, _)| id == PROP_CRTC_ID)
        .expect("the connector advertises CRTC_ID")
        .1
}

#[test]
fn a_mode_is_set_and_unset_together_with_the_connector_routed_to_the_crtc() {
    let (_screen, c) = atomic_client(32, 8);
    let panel = make_modeinfo(32, 8);
    let mut blob = DrmModeCreateBlob {
        data: panel.as_ptr() as u64,
        length: panel.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
        .expect("CREATEPROPBLOB");
    let mode = blob.blob_id as u64;
    let einval = Err(FsError::InvalidParam);
    let modeset = DRM_MODE_ATOMIC_ALLOW_MODESET;
    let crtc_req =
        |props: &[u32], values: &[u64]| Request::new(&[CRTC], &[props.len() as u32], props, values);
    let conn_req = |crtc: u64| Request::new(&[CONN], &[1], &[PROP_CRTC_ID], &[crtc]);
    let full = |mode: u64, active: u64, conn_crtc: u64| {
        Request::new(
            &[CRTC, CONN],
            &[2, 1],
            &[PROP_MODE_ID, PROP_ACTIVE, PROP_CRTC_ID],
            &[mode, active, conn_crtc],
        )
    };

    assert_eq!(connector_crtc(), 0, "nothing routed before a mode");
    assert_eq!(
        commit(
            &c,
            &crtc_req(&[PROP_MODE_ID, PROP_ACTIVE], &[mode, 1]),
            modeset
        ),
        einval,
        "a mode with no connector routed: enabled/connectors mismatch"
    );
    assert_eq!(
        commit(&c, &conn_req(CRTC as u64), modeset),
        einval,
        "a connector routed to a CRTC with no mode"
    );
    assert_eq!(
        commit(&c, &crtc_req(&[PROP_ACTIVE], &[1]), modeset),
        einval,
        "active without enabled"
    );
    assert_eq!(connector_crtc(), 0, "and none of it was applied");
    assert_eq!(commit(&c, &full(mode, 1, CRTC as u64), modeset), Ok(0));
    assert_eq!(connector_crtc(), CRTC as u64, "routed with the mode");

    // ACTIVE = 0 keeps the mode and the routing (DPMS off does too).
    assert_eq!(commit(&c, &crtc_req(&[PROP_ACTIVE], &[0]), modeset), Ok(0));
    assert_eq!(
        connector_crtc(),
        CRTC as u64,
        "still routed while the mode is set"
    );
    assert_eq!(commit(&c, &crtc_req(&[PROP_ACTIVE], &[1]), modeset), Ok(0));

    // Unsetting the mode: with the CRTC active it is "active without
    // enabled", and with the connector still routed it is the mismatch;
    // detaching the connector alone leaves an enabled CRTC with none.
    assert_eq!(
        commit(&c, &crtc_req(&[PROP_MODE_ID], &[0]), modeset),
        einval,
        "MODE_ID = 0 alone on an active CRTC"
    );
    assert_eq!(
        commit(
            &c,
            &crtc_req(&[PROP_MODE_ID, PROP_ACTIVE], &[0, 0]),
            modeset
        ),
        einval,
        "no mode with the connector still routed"
    );
    assert_eq!(
        commit(&c, &conn_req(0), modeset),
        einval,
        "a mode with its connector detached"
    );
    assert_eq!(
        commit(
            &c,
            &Request::new(
                &[CRTC, CONN],
                &[1, 1],
                &[PROP_MODE_ID, PROP_CRTC_ID],
                &[0, 0]
            ),
            modeset
        ),
        einval,
        "mode unset and connector detached, but ACTIVE left on"
    );
    assert_eq!(connector_crtc(), CRTC as u64, "none of it applied");
    assert!(
        drm::atomic_snapshot().0.active,
        "and the CRTC is still active"
    );
    assert_eq!(commit(&c, &full(0, 0, 0), modeset), Ok(0));
    assert_eq!(connector_crtc(), 0, "unrouted with the mode");
    assert!(!drm::crtc_enabled());
}
