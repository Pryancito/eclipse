//! On an atomic driver there is one state: a legacy `SETCRTC` goes through
//! `__drm_atomic_helper_set_config` and a legacy `SETPLANE` through
//! `drm_atomic_helper_update_plane`, so what they set is what `GETCRTC`
//! (`x`/`y` from the primary plane's `src_x`/`src_y`), the plane's
//! properties, `ACTIVE`, `MODE_ID` and the connector's `CRTC_ID` read
//! back; removing the scanout fb (`atomic_remove_fb`) takes the mode and
//! `ACTIVE` with it. None of the legacy ioctls wrote the atomic state and
//! `GETCRTC` answered `x = y = 0` whatever had been set.
use super::gl_client_sequence_tests::Client;
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

const CRTC: u32 = drm::SYNTH_CRTC_ID;
const CONN: u32 = drm::SYNTH_CONNECTOR_ID;
const PLANE: u32 = drm::SYNTH_PLANE_ID;

fn prop(props: &[(u32, u64)], id: u32) -> u64 {
    props
        .iter()
        .find(|&&(p, _)| p == id)
        .unwrap_or_else(|| panic!("property {} is advertised", id))
        .1
}

fn crtc_prop(id: u32) -> u64 {
    prop(&crtc_props(true), id)
}

fn plane_prop(id: u32) -> u64 {
    prop(
        &plane_props(&drm::get_plane(PLANE).expect("the plane"), true),
        id,
    )
}

fn connector_crtc() -> u64 {
    prop(&connector_props(CONN, true), PROP_CRTC_ID)
}

fn plane_rects() -> [u64; 8] {
    [
        PROP_CRTC_X,
        PROP_CRTC_Y,
        PROP_CRTC_W,
        PROP_CRTC_H,
        PROP_SRC_X,
        PROP_SRC_Y,
        PROP_SRC_W,
        PROP_SRC_H,
    ]
    .map(plane_prop)
}

fn get_crtc(c: &Client) -> DrmModeGetCrtc {
    let mut crtc = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: CRTC,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
    crtc
}

/// `SETCRTC` on the connector, mode `w`x`h` at `(x, y)` of `fb`; `None`
/// is the call without a mode.
fn set_crtc(c: &Client, fb: u32, mode: Option<(u32, u32, u32, u32)>) {
    let conn = [CONN];
    let mut req = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: CRTC,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    if let Some((w, h, x, y)) = mode {
        req.set_connectors_ptr = conn.as_ptr() as u64;
        req.count_connectors = 1;
        req.fb_id = fb;
        req.x = x;
        req.y = y;
        req.mode_valid = 1;
        req.mode = make_modeinfo(w, h);
    }
    c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req).expect("SETCRTC");
}

#[test]
fn a_legacy_setcrtc_is_a_commit_of_the_state_the_atomic_uapi_reads_back() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(48, 24);
    let fb = c.addfb2(&buf);
    assert_eq!(crtc_prop(PROP_ACTIVE), 0);
    assert_eq!(crtc_prop(PROP_MODE_ID), 0);

    set_crtc(&c, fb, Some((32, 8, 16, 4)));
    let crtc = get_crtc(&c);
    assert_eq!(
        (crtc.x, crtc.y),
        (16, 4),
        "GETCRTC x/y are src_x/src_y >> 16"
    );
    assert_eq!((crtc.fb_id, crtc.mode_valid), (fb, 1));
    assert_eq!(crtc_prop(PROP_ACTIVE), 1, "crtc_state->active = true");
    let mode = crtc_prop(PROP_MODE_ID);
    assert_ne!(mode, 0, "the mode is a kernel-owned blob");
    assert_eq!(
        drm::get_blob(mode as u32).as_deref(),
        Some(&make_modeinfo(32, 8)[..]),
        "holding the mode SETCRTC set"
    );
    assert_eq!(connector_crtc(), CRTC as u64, "the connector is routed");
    assert_eq!(
        plane_rects(),
        [0, 0, 32, 8, 16 << 16, 4 << 16, 32 << 16, 8 << 16],
        "the primary plane covers the CRTC from (x, y) of the fb"
    );
    assert_eq!(plane_prop(PROP_FB_ID), fb as u64);
    assert_eq!(plane_prop(PROP_CRTC_ID), CRTC as u64);

    // A second modeset reuses the blob (the id is stable) and moves the
    // plane with the new viewport.
    set_crtc(&c, fb, Some((32, 8, 8, 16)));
    assert_eq!(crtc_prop(PROP_MODE_ID), mode, "same blob across modesets");
    let crtc = get_crtc(&c);
    assert_eq!((crtc.x, crtc.y), (8, 16));
    assert_eq!(plane_rects()[4..6], [8 << 16, 16 << 16]);

    // The state the atomic uAPI reads is the state it checks against: an
    // atomic client committing what it read back, unchanged, is not a
    // modeset and needs no ALLOW_MODESET.
    let as_read = Request::new(
        &[CRTC, CONN],
        &[2, 1],
        &[PROP_MODE_ID, PROP_ACTIVE, PROP_CRTC_ID],
        &[mode, 1, CRTC as u64],
    );
    assert_eq!(
        commit(&c, &as_read, DRM_MODE_ATOMIC_TEST_ONLY),
        Ok(0),
        "re-committing the legacy modeset as read back is not a modeset"
    );

    // Without a mode: the mode and ACTIVE go, the connector is detached,
    // and the plane's rectangles are left as they were (Linux clears the
    // plane's crtc and fb, not its `src_*`/`crtc_*`).
    set_crtc(&c, 0, None);
    let crtc = get_crtc(&c);
    assert_eq!((crtc.fb_id, crtc.mode_valid), (0, 0));
    assert_eq!((crtc.x, crtc.y), (8, 16), "src_x/src_y are not cleared");
    assert_eq!(crtc_prop(PROP_ACTIVE), 0);
    assert_eq!(crtc_prop(PROP_MODE_ID), 0);
    assert_eq!(connector_crtc(), 0);
    assert_eq!(plane_rects()[4..6], [8 << 16, 16 << 16]);
    assert_eq!(
        commit(
            &c,
            &Request::new(&[CRTC], &[1], &[PROP_ACTIVE], &[1]),
            DRM_MODE_ATOMIC_ALLOW_MODESET
        ),
        Err(FsError::InvalidParam),
        "and the atomic check agrees the pipe has no mode"
    );
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

#[test]
fn a_legacy_setplane_writes_the_rectangles_the_atomic_uapi_reads_back() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(48, 24);
    let fb = c.addfb2(&buf);
    set_crtc(&c, fb, Some((32, 8, 0, 0)));
    assert_eq!(plane_rects(), [0, 0, 32, 8, 0, 0, 32 << 16, 8 << 16]);

    let mut req: DrmModeSetPlane = unsafe { core::mem::zeroed() };
    req.plane_id = PLANE;
    req.crtc_id = CRTC;
    req.fb_id = fb;
    req.crtc_x = 2;
    req.crtc_y = 1;
    req.crtc_w = 30;
    req.crtc_h = 7;
    req.src_x = 12 << 16;
    req.src_y = 5 << 16;
    req.src_w = 30 << 16;
    req.src_h = 7 << 16;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req), Ok(0));
    assert_eq!(
        plane_rects(),
        [2, 1, 30, 7, 12 << 16, 5 << 16, 30 << 16, 7 << 16],
        "drm_atomic_helper_update_plane: the request's eight rectangles"
    );
    let crtc = get_crtc(&c);
    assert_eq!((crtc.x, crtc.y), (12, 5), "GETCRTC follows the plane");
    assert_eq!(crtc_prop(PROP_ACTIVE), 1, "SETPLANE is not a modeset");

    // A source rectangle the fb does not hold is refused before anything
    // is written (`drm_framebuffer_check_src_coords`, ENOSPC).
    req.src_x = 20 << 16;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req),
        Err(FsError::NoDeviceSpace)
    );
    assert_eq!(plane_rects()[4], 12 << 16, "nothing written by a refusal");
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_page_flip_ioctl`: the new fb is measured against the
/// primary plane's source rectangle (`drm_framebuffer_check_src_coords`),
/// which SETCRTC's viewport, SETPLANE and an atomic commit all set. This
/// measured the mode at (0, 0).
#[test]
fn a_page_flip_measures_the_new_fb_against_the_plane_source_rectangle() {
    let (_screen, c) = atomic_client(32, 8);
    let wide = c.create_dumb(64, 8);
    let fb_wide = c.addfb2(&wide);
    let exact = c.create_dumb(32, 8);
    let fb_exact = c.addfb2(&exact);
    let mid = c.create_dumb(48, 8);
    let fb_mid = c.addfb2(&mid);
    let enospc = Err(FsError::NoDeviceSpace);

    // A plane never positioned (no SETCRTC yet: this tree lets a client
    // start with a flip) is measured against the mode.
    let narrow = c.create_dumb(16, 8);
    let fb_narrow = c.addfb2(&narrow);
    assert_eq!(
        c.page_flip(CRTC, fb_narrow, 0),
        enospc,
        "no rectangle yet: the mode is the yardstick"
    );
    // As soon as any of the rectangle is set, the rectangle is. SRC_W
    // alone on the off plane: 32 wide, 0 tall, which a 4-row fb holds
    // and the 8-row mode would not.
    assert_eq!(
        commit(
            &c,
            &Request::new(&[PLANE], &[1], &[PROP_SRC_W], &[32 << 16]),
            0
        ),
        Ok(0)
    );
    let short = c.create_dumb(32, 4);
    let fb_short = c.addfb2(&short);
    assert_eq!(
        c.page_flip(CRTC, fb_short, 0),
        Ok(0),
        "a 32 x 0 rectangle fits"
    );
    drm::flush_pending_flip_completions();
    assert_eq!(
        c.page_flip(CRTC, fb_narrow, 0),
        enospc,
        "and 16 columns do not hold it"
    );

    // The plane 16 pixels into a 64-wide fb: a flip needs 48 columns.
    set_crtc(&c, fb_wide, Some((32, 8, 16, 0)));
    assert_eq!(
        c.page_flip(CRTC, fb_exact, 1),
        enospc,
        "the size of the mode, but the plane starts 16 pixels in"
    );
    assert_eq!(c.page_flip(CRTC, fb_mid, 2), Ok(0), "16 + 32 fits in 48");
    drm::flush_pending_flip_completions();
    let crtc = get_crtc(&c);
    assert_eq!(
        (crtc.fb_id, crtc.x),
        (fb_mid, 16),
        "the flip keeps the viewport"
    );

    // An atomic commit moving the plane to the fb's origin: the exact fb
    // fits again.
    assert_eq!(
        commit(&c, &Request::new(&[PLANE], &[1], &[PROP_SRC_X], &[0]), 0),
        Ok(0)
    );
    assert_eq!(c.page_flip(CRTC, fb_exact, 3), Ok(0));
    drm::flush_pending_flip_completions();

    // SETPLANE scrolls the plane down 4 rows: an fb of 8 rows is short.
    let mut req: DrmModeSetPlane = unsafe { core::mem::zeroed() };
    req.plane_id = PLANE;
    req.crtc_id = CRTC;
    req.fb_id = fb_wide;
    req.crtc_w = 32;
    req.crtc_h = 4;
    req.src_y = 4 << 16;
    req.src_w = 32 << 16;
    req.src_h = 4 << 16;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req), Ok(0));
    let tall = c.create_dumb(32, 12);
    let fb_tall = c.addfb2(&tall);
    assert_eq!(c.page_flip(CRTC, fb_exact, 4), Ok(0), "4 + 4 rows fit in 8");
    drm::flush_pending_flip_completions();
    // Rows 6..10 of the 12-row fb: the 8-row fb cannot hold them.
    req.fb_id = fb_tall;
    req.src_y = 6 << 16;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req), Ok(0));
    assert_eq!(c.page_flip(CRTC, fb_exact, 5), enospc, "6 + 4 rows do not");
    assert_eq!(c.page_flip(CRTC, fb_tall, 6), Ok(0), "but fit in 12");
    drm::flush_pending_flip_completions();
    for fb in [fb_wide, fb_exact, fb_mid, fb_tall, fb_narrow, fb_short] {
        c.rmfb(fb).expect("RMFB");
    }
    for buf in [&wide, &exact, &mid, &tall, &narrow, &short] {
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }
}

#[test]
fn removing_the_scanout_fb_takes_the_mode_and_active_with_it() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    set_crtc(&c, fb, Some((32, 8, 0, 0)));
    assert_eq!(crtc_prop(PROP_ACTIVE), 1);
    assert_ne!(crtc_prop(PROP_MODE_ID), 0);

    // `atomic_remove_fb`: the primary plane loses its fb and CRTC, and the
    // CRTC its mode and `active`; the connector is detached with it.
    c.rmfb(fb).expect("RMFB");
    assert_eq!(crtc_prop(PROP_ACTIVE), 0);
    assert_eq!(crtc_prop(PROP_MODE_ID), 0);
    assert_eq!(connector_crtc(), 0);
    assert_eq!(plane_prop(PROP_FB_ID), 0);
    assert_eq!(plane_prop(PROP_CRTC_ID), 0);
    let crtc = get_crtc(&c);
    assert_eq!((crtc.fb_id, crtc.mode_valid), (0, 0));

    // An fb that is not on the CRTC takes nothing with it.
    let fb = c.addfb2(&buf);
    set_crtc(&c, fb, Some((32, 8, 0, 0)));
    let other_buf = c.create_dumb(32, 8);
    let other = c.addfb2(&other_buf);
    c.rmfb(other).expect("RMFB");
    assert_eq!(crtc_prop(PROP_ACTIVE), 1, "the scanout fb is untouched");
    assert_ne!(crtc_prop(PROP_MODE_ID), 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(other_buf.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}
