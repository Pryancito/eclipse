//! `drm_mode_obj_set_property_ioctl` past its checks: on an atomic driver
//! it is `set_property_atomic`, the one property staged on a fresh atomic
//! state and committed at once -- no event, and no ALLOW_MODESET, so a
//! write that needs a modeset is the check phase's EINVAL -- and on a
//! legacy driver `set_property_legacy`, the object's own `set_property`
//! hook, which this KMS has for nothing but DPMS: EINVAL. This answered
//! "done" to every value that passed the checks and applied none, so a
//! legacy client that moved the primary plane's FB_ID here was told yes
//! and kept scanning out the old buffer.
use super::gl_client_sequence_tests::Client;
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

fn crtc_fb() -> u32 {
    drm::get_crtc(drm::SYNTH_CRTC_ID).expect("the CRTC").fb_id
}

#[test]
fn a_property_write_is_a_one_property_atomic_commit_or_einval_on_a_legacy_driver() {
    let (_screen, atomic) = atomic_client(32, 8);
    let (crtc, plane, conn) = (
        drm::SYNTH_CRTC_ID,
        drm::SYNTH_PLANE_ID,
        drm::SYNTH_CONNECTOR_ID,
    );
    let panel = make_modeinfo(32, 8);
    let mut mode = DrmModeCreateBlob {
        data: panel.as_ptr() as u64,
        length: panel.len() as u32,
        blob_id: 0,
    };
    atomic
        .ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut mode)
        .expect("CREATEPROPBLOB");
    let buf1 = atomic.create_dumb(32, 8);
    let fb1 = atomic.addfb2(&buf1);
    commit(
        &atomic,
        &Request::new(
            &[crtc, plane, conn],
            &[2, 2, 1],
            &[
                PROP_MODE_ID,
                PROP_ACTIVE,
                PROP_FB_ID,
                PROP_CRTC_ID,
                PROP_CRTC_ID,
            ],
            &[mode.blob_id as u64, 1, fb1 as u64, crtc as u64, crtc as u64],
        ),
        DRM_MODE_ATOMIC_ALLOW_MODESET,
    )
    .expect("the pipe comes up");
    assert_eq!(crtc_fb(), fb1);

    // A client that never asked for DRM_CLIENT_CAP_ATOMIC: the ioctl is
    // not gated on it -- `set_property_atomic` is the driver's choice,
    // not the client's -- and this is how a legacy compositor flips.
    let legacy = Client::open(0);
    let buf2 = legacy.create_dumb(32, 8);
    let fb2 = legacy.addfb2(&buf2);
    let einval = Err(FsError::InvalidParam);
    let set = |obj_id: u32, obj_type: u32, prop_id: u32, value: u64| {
        let mut req = DrmModeObjSetProperty {
            value,
            prop_id,
            obj_id,
            obj_type,
        };
        legacy.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut req)
    };

    // FB_ID on the primary plane presents the buffer.
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, fb2 as u64),
        Ok(0)
    );
    assert_eq!(crtc_fb(), fb2, "the write reached the scanout");
    // ACTIVE and MODE_ID changes are modesets, which the commit never
    // allows; the value already set is a commit that changes nothing.
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 0), einval);
    assert!(
        drm::atomic_snapshot().0.active,
        "the refused commit applied nothing"
    );
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_MODE_ID, 0), einval);
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 1), Ok(0));
    assert_eq!(crtc_fb(), fb2);
    // OUT_FENCE_PTR: `drm_atomic_crtc_set_property` writes the -1 and
    // the commit runs, but no fence follows -- `prepare_signaling` is
    // the ATOMIC ioctl's alone -- and no event, since the commit asks
    // for none.
    let mut slot: i32 = 7;
    assert_eq!(
        set(
            crtc,
            DRM_MODE_OBJECT_CRTC,
            PROP_OUT_FENCE_PTR,
            &mut slot as *mut i32 as u64
        ),
        Ok(0)
    );
    assert_eq!(slot, -1, "the slot says no fence");
    assert_eq!(
        set(crtc, DRM_MODE_OBJECT_CRTC, PROP_OUT_FENCE_PTR, 0),
        Ok(0)
    );
    drm::flush_pending_flip_completions();
    let mut events = [0u8; 64];
    assert_eq!(
        legacy.read_events(&mut events),
        Err(FsError::Again),
        "no event: the commit carries none"
    );
    // A connector write the commit takes: link-status, never downgraded.
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_LINK_STATUS, 1),
        Ok(0)
    );
    // DPMS keeps its own path (`drm_atomic_connector_commit_dpms`).
    assert_eq!(set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_DPMS, 3), Ok(0));
    assert!(drm::crtc_blanked());
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_DPMS, DRM_MODE_DPMS_ON),
        Ok(0)
    );
    assert!(!drm::crtc_blanked());

    // A legacy driver has no `set_property` hook here but DPMS: EINVAL,
    // and the scanout stays where the atomic commit left it.
    drm::set_atomic_enabled(false);
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, fb1 as u64),
        einval
    );
    assert_eq!(crtc_fb(), fb2, "nothing was applied");
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 1), einval);
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_LINK_STATUS, 0),
        einval
    );
    assert_eq!(set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_DPMS, 3), Ok(0));
    assert!(
        drm::crtc_blanked(),
        "DPMS is the one hook a legacy driver has"
    );
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_DPMS, DRM_MODE_DPMS_ON),
        Ok(0)
    );
}
