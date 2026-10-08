//! `ATOMIC` with `count_objs == 0`. Linux accepts an empty commit (there
//! is nothing to check and nothing to apply) but refuses one that asks
//! for a page-flip event: with no CRTC in the state there is nothing to
//! signal it from, and `prepare_signaling` says so with EINVAL rather
//! than let the client pend on an event that never comes. This arm
//! answered 0 to both.
use super::out_fence_tests::{atomic_client, commit, Request};
use super::*;

fn empty() -> Request {
    Request::new(&[], &[], &[], &[])
}

#[test]
fn an_empty_commit_is_fine_unless_it_asks_for_an_event() {
    let (_screen, c) = atomic_client(32, 8);
    let req = empty();
    for flags in [
        0,
        DRM_MODE_ATOMIC_TEST_ONLY,
        DRM_MODE_ATOMIC_NONBLOCK,
        DRM_MODE_ATOMIC_ALLOW_MODESET,
        DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_ATOMIC_ALLOW_MODESET,
    ] {
        assert_eq!(commit(&c, &req, flags), Ok(0), "flags {flags:#x}");
    }
    for flags in [
        DRM_MODE_PAGE_FLIP_EVENT,
        DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK,
        DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET,
    ] {
        assert_eq!(
            commit(&c, &req, flags),
            Err(FsError::InvalidParam),
            "flags {flags:#x}: an event with nothing to signal it from"
        );
    }
    // Nothing was queued for the refused ones: the client reads no
    // event, now or after the vblank that would have carried one.
    drm::flush_pending_flip_completions();
    let mut buf = [0u8; 64];
    assert_eq!(
        c.read_events(&mut buf),
        Err(FsError::Again),
        "a refused commit still queued an event"
    );
    // The refusal is about the missing CRTC, not the flag: the same
    // event on a commit that names the CRTC (here, the one that turns it
    // on) is owed and delivered.
    let mode = make_modeinfo(32, 8);
    let mut blob = DrmModeCreateBlob {
        data: mode.as_ptr() as u64,
        length: mode.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
        .expect("CREATEPROPBLOB");
    let on_crtc = Request::new(
        &[drm::SYNTH_CRTC_ID],
        &[2],
        &[PROP_MODE_ID, PROP_ACTIVE],
        &[u64::from(blob.blob_id), 1],
    );
    assert_eq!(
        commit(
            &c,
            &on_crtc,
            DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET
        ),
        Ok(0)
    );
    drm::flush_pending_flip_completions();
    assert_eq!(c.read_events(&mut buf).map(|n| n / 32), Ok(1));
}
