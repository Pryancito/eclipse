//! `drm_atomic_plane_check` runs on the plane's *resulting* state, not on
//! the properties the commit names: CRTC and FB both set or both unset,
//! the source rectangle (16.16) inside the FB the plane ends with
//! (ENOSPC), and every damage clip a real rectangle. This checked only
//! what was staged, with EINVAL for the rectangle and whole-pixel
//! truncation, and let a bad clip through to be skipped at present time.
use super::gl_client_sequence_tests::Client;
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

const CRTC: u32 = drm::SYNTH_CRTC_ID;
const PLANE: u32 = drm::SYNTH_PLANE_ID;

/// The pipe up on `fb`: mode, active, and the primary plane on the CRTC.
fn bring_up(c: &Client, fb: u32) {
    let panel = make_modeinfo(32, 8);
    let mut mode = DrmModeCreateBlob {
        data: panel.as_ptr() as u64,
        length: panel.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut mode)
        .expect("CREATEPROPBLOB");
    commit(
        c,
        &Request::new(
            &[CRTC, PLANE],
            &[2, 2],
            &[PROP_MODE_ID, PROP_ACTIVE, PROP_FB_ID, PROP_CRTC_ID],
            &[mode.blob_id as u64, 1, fb as u64, CRTC as u64],
        ),
        DRM_MODE_ATOMIC_ALLOW_MODESET,
    )
    .expect("the pipe comes up");
}

fn plane_req(props: &[u32], values: &[u64]) -> Request {
    Request::new(&[PLANE], &[props.len() as u32], props, values)
}

fn plane_state(c: &Client) -> (u32, u32) {
    let mut info: DrmModeGetPlane = unsafe { core::mem::zeroed() };
    info.plane_id = PLANE;
    c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut info)
        .expect("GETPLANE");
    (info.crtc_id, info.fb_id)
}

fn blob(c: &Client, bytes: &[u8]) -> u32 {
    let mut b = DrmModeCreateBlob {
        data: bytes.as_ptr() as u64,
        length: bytes.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut b)
        .expect("CREATEPROPBLOB");
    b.blob_id
}

fn clips(rects: &[[i32; 4]]) -> alloc::vec::Vec<u8> {
    rects
        .iter()
        .flat_map(|r| r.iter().flat_map(|v| v.to_ne_bytes()))
        .collect()
}

/// "either *both* CRTC and FB must be set, or neither", on the state the
/// commit leaves: a plane is disabled with FB_ID = 0 and CRTC_ID = 0 in
/// one commit and enabled with both, and either alone is EINVAL with
/// nothing applied.
#[test]
fn a_plane_ends_with_a_crtc_and_an_fb_together_or_with_neither() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    bring_up(&c, fb);
    assert_eq!(plane_state(&c), (CRTC, fb));
    let einval = Err(FsError::InvalidParam);

    assert_eq!(
        commit(&c, &plane_req(&[PROP_FB_ID], &[0]), 0),
        einval,
        "CRTC set but no FB"
    );
    assert_eq!(plane_state(&c), (CRTC, fb), "and the scanout stayed");
    assert_eq!(
        commit(&c, &plane_req(&[PROP_CRTC_ID], &[0]), 0),
        einval,
        "FB set but no CRTC"
    );
    assert_eq!(plane_state(&c), (CRTC, fb));
    assert_eq!(
        commit(
            &c,
            &plane_req(&[PROP_FB_ID, PROP_CRTC_ID], &[0, 0]),
            DRM_MODE_ATOMIC_TEST_ONLY
        ),
        Ok(0),
        "both unset passes the check"
    );
    assert_eq!(
        commit(&c, &plane_req(&[PROP_FB_ID, PROP_CRTC_ID], &[0, 0]), 0),
        Ok(0)
    );
    assert_eq!(plane_state(&c), (0, 0), "the plane is off");

    // Off, the plane is enabled with both again; one of them is EINVAL.
    assert_eq!(
        commit(&c, &plane_req(&[PROP_FB_ID], &[fb as u64]), 0),
        einval,
        "FB set but no CRTC"
    );
    assert_eq!(plane_state(&c), (0, 0));
    assert_eq!(
        commit(&c, &plane_req(&[PROP_CRTC_ID], &[CRTC as u64]), 0),
        einval,
        "CRTC set but no FB"
    );
    assert_eq!(plane_state(&c), (0, 0));
    assert_eq!(
        commit(
            &c,
            &plane_req(&[PROP_CRTC_ID, PROP_FB_ID], &[CRTC as u64, fb as u64]),
            0
        ),
        Ok(0)
    );
    assert_eq!(plane_state(&c), (CRTC, fb));
}

/// The source rectangle is held against the FB of the resulting state
/// -- a `SRC_W` alone against the fb the plane keeps, a `FB_ID` alone
/// against the rectangle it keeps -- in 16.16 (`src_w > fb_width ||
/// src_x > fb_width - src_w`), and the answer is ENOSPC. Nothing of it is
/// checked on a plane that ends disabled.
#[test]
fn the_source_rectangle_must_fit_the_fb_the_plane_ends_with() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let narrow_buf = c.create_dumb(16, 8);
    let narrow = c.addfb2(&narrow_buf);
    bring_up(&c, fb);
    let enospc = Err(FsError::NoDeviceSpace);
    let src = |x: u64, y: u64, w: u64, h: u64| {
        plane_req(
            &[PROP_SRC_X, PROP_SRC_Y, PROP_SRC_W, PROP_SRC_H],
            &[x, y, w, h],
        )
    };

    assert_eq!(commit(&c, &src(0, 0, 32 << 16, 8 << 16), 0), Ok(0));
    assert_eq!(
        commit(&c, &plane_req(&[PROP_SRC_W], &[33 << 16]), 0),
        enospc,
        "wider than the fb, with no FB_ID in the commit"
    );
    assert_eq!(
        drm::atomic_snapshot().0.src_w,
        32 << 16,
        "the refused width was not applied"
    );
    assert_eq!(
        commit(&c, &plane_req(&[PROP_SRC_H], &[9 << 16]), 0),
        enospc,
        "taller than the fb"
    );
    assert_eq!(
        commit(&c, &plane_req(&[PROP_SRC_X], &[1 << 16]), 0),
        enospc,
        "x pushes it past the edge"
    );
    assert_eq!(
        commit(&c, &plane_req(&[PROP_SRC_Y], &[1 << 16]), 0),
        enospc,
        "y pushes it past the bottom"
    );
    // A fractional edge counts: 31.5 wide from x = 0.75 is past 32.
    assert_eq!(
        commit(&c, &src(3 << 14, 0, (31 << 16) | (1 << 15), 8 << 16), 0),
        enospc,
        "a fractional edge past the fb"
    );
    assert_eq!(
        commit(&c, &src(1 << 15, 0, (31 << 16) | (1 << 15), 8 << 16), 0),
        Ok(0),
        "31.5 from 0.5 is exactly 32"
    );
    // A FB_ID alone is held against the rectangle the plane keeps.
    assert_eq!(
        commit(&c, &plane_req(&[PROP_FB_ID], &[narrow as u64]), 0),
        enospc,
        "a narrower fb under the same rectangle"
    );
    assert_eq!(
        commit(
            &c,
            &plane_req(
                &[PROP_FB_ID, PROP_SRC_X, PROP_SRC_W],
                &[narrow as u64, 0, 16 << 16]
            ),
            0
        ),
        Ok(0),
        "and fits once the rectangle is cut to it"
    );
    // A disabled plane's rectangle is not looked at.
    assert_eq!(
        commit(
            &c,
            &plane_req(
                &[PROP_FB_ID, PROP_CRTC_ID, PROP_SRC_W],
                &[0, 0, u32::MAX as u64]
            ),
            0
        ),
        Ok(0),
        "if disabled, we don't care about the rest of the state"
    );
}

/// `FB_DAMAGE_CLIPS`: a blob that is not a whole number of rectangles is
/// EINVAL as it is staged (`drm_property_replace_blob_from_id` with the
/// rectangle as element size); a clip that is not a rectangle at or past
/// the origin is the check's EINVAL (`drm_atomic_plane_check`), which
/// compares its far edge to the fb's size shifted by 16, so a clip past
/// the edge in pixels is let through (and trimmed at present time). On a
/// plane that ends disabled the clips are not looked at.
#[test]
fn damage_clips_are_refused_when_malformed_and_checked_with_the_plane() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    bring_up(&c, fb);
    let einval = Err(FsError::InvalidParam);
    let with_clips =
        |b: u32| plane_req(&[PROP_FB_ID, PROP_FB_DAMAGE_CLIPS], &[fb as u64, b as u64]);

    let good = blob(&c, &clips(&[[0, 0, 16, 8], [16, 0, 32, 8]]));
    assert_eq!(commit(&c, &with_clips(good), 0), Ok(0));
    let short = blob(&c, &clips(&[[0, 0, 16, 8]])[..12]);
    assert_eq!(
        commit(&c, &with_clips(short), 0),
        einval,
        "not a whole number of rectangles"
    );
    for (bad, why) in [
        ([4, 0, 4, 8], "x1 == x2"),
        ([0, 4, 16, 4], "y1 == y2"),
        ([8, 0, 4, 8], "x2 < x1"),
        ([-1, 0, 16, 8], "x1 < 0"),
        ([0, -1, 16, 8], "y1 < 0"),
        ([0, 0, (32 << 16) + 1, 8], "x2 past width << 16"),
        ([0, 0, 16, (8 << 16) + 1], "y2 past height << 16"),
    ] {
        let b = blob(&c, &clips(&[[0, 0, 16, 8], bad]));
        assert_eq!(commit(&c, &with_clips(b), 0), einval, "{}", why);
    }
    let past_the_edge = blob(&c, &clips(&[[0, 0, 33, 9]]));
    assert_eq!(
        commit(&c, &with_clips(past_the_edge), 0),
        Ok(0),
        "past the edge in pixels, short of width << 16"
    );
    let bad = blob(&c, &clips(&[[-1, 0, 16, 8]]));
    assert_eq!(
        commit(
            &c,
            &plane_req(
                &[PROP_FB_ID, PROP_CRTC_ID, PROP_FB_DAMAGE_CLIPS],
                &[0, 0, bad as u64]
            ),
            0
        ),
        Ok(0),
        "a disabled plane's clips are not looked at"
    );
}
