use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu;
use alloc::vec::Vec;

/// A value that is neither a valid fd nor `-1`, so "was written" and "was
/// left alone" are distinguishable.
const UNWRITTEN: i32 = 0x5A5A_5A5A;

/// The property arrays of one atomic request, kept alive for as long as the
/// request points at them.
pub(super) struct Request {
    objs: Vec<u32>,
    counts: Vec<u32>,
    props: Vec<u32>,
    values: Vec<u64>,
}

impl Request {
    pub(super) fn new(objs: &[u32], counts: &[u32], props: &[u32], values: &[u64]) -> Request {
        Request {
            objs: objs.to_vec(),
            counts: counts.to_vec(),
            props: props.to_vec(),
            values: values.to_vec(),
        }
    }

    fn ioctl(&self, flags: u32) -> DrmModeAtomic {
        DrmModeAtomic {
            flags,
            count_objs: self.objs.len() as u32,
            objs_ptr: self.objs.as_ptr() as u64,
            count_props_ptr: self.counts.as_ptr() as u64,
            props_ptr: self.props.as_ptr() as u64,
            prop_values_ptr: self.values.as_ptr() as u64,
            reserved: 0,
            user_data: 0,
        }
    }
}

/// An atomic client on an output, with the cmdline flag on. Returns the
/// screen (which holds the test lock and puts the flag back on Drop) and the
/// client.
pub(super) fn atomic_client(width: u32, height: u32) -> (kms_emu::Screen, Client) {
    let screen = kms_emu::attach(width, height);
    drm::set_atomic_enabled(true);
    let c = Client::open(0);
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_ATOMIC, 1];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("SET_CLIENT_CAP ATOMIC");
    (screen, c)
}

/// A commit the check phase accepts as is: leaving the CRTC inactive needs
/// no mode and no modeset flag. It is the baseline every refusal below is
/// measured against, so a refusal cannot be the request's own fault.
fn benign_request() -> Request {
    Request::new(&[drm::SYNTH_CRTC_ID], &[1], &[PROP_ACTIVE], &[0])
}

pub(super) fn commit(c: &Client, req: &Request, flags: u32) -> Result<usize> {
    let mut ioctl = req.ioctl(flags);
    c.ioctl(DRM_IOCTL_MODE_ATOMIC, &mut ioctl)
}

/// Without the cap the ioctl is refused, and the cap itself is refused
/// unless the boot asked for it. That is the gate the whole arm sits behind
/// -- and with it shut, the rest of this module is unreachable, which is why
/// nothing tested it.
#[test]
fn the_atomic_ioctl_is_refused_until_the_boot_and_the_client_both_opt_in() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_ATOMIC, 1];

    // Flag off: the capability is EOPNOTSUPP, like a Linux driver without
    // DRIVER_ATOMIC, so a compositor falls back to legacy KMS.
    assert_eq!(
        c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap),
        Err(FsError::OpNotSupported)
    );
    let req = benign_request();
    assert_eq!(
        commit(&c, &req, 0),
        Err(FsError::InvalidParam),
        "a non-atomic client got an atomic commit"
    );

    // Flag on, cap negotiated: now it is reachable.
    drm::set_atomic_enabled(true);
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("SET_CLIENT_CAP ATOMIC");
    assert!(commit(&c, &req, 0).is_ok());
}

/// A commit that FAILS still writes the out-fence slot, and writes `-1`.
/// The writeback sits before the commit's error is mapped on purpose: the
/// slot is the client's `int out_fence` local, and leaving it untouched
/// means the client reads whatever was on its stack and then closes or waits
/// on a descriptor that belongs to something else.
#[test]
fn a_failed_commit_still_writes_minus_one_into_the_out_fence_slot() {
    let (_screen, c) = atomic_client(32, 8);
    let mut slot: i32 = UNWRITTEN;
    // A plane given a framebuffer but no CRTC: every value is legal for
    // its property, so it stages fine and is refused by the commit's
    // check phase (`drm_atomic_plane_check`: "FB set but no CRTC").
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let req = Request::new(
        &[drm::SYNTH_CRTC_ID, drm::SYNTH_PLANE_ID],
        &[1, 2],
        &[PROP_OUT_FENCE_PTR, PROP_FB_ID, PROP_CRTC_ID],
        &[&mut slot as *mut i32 as u64, fb as u64, 0],
    );

    assert_eq!(
        commit(&c, &req, 0),
        Err(FsError::InvalidParam),
        "a framebuffer on a plane with no CRTC was accepted"
    );
    assert_eq!(slot, -1, "the client's fence slot was left uninitialised");
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `drm_atomic_set_property` runs `drm_property_change_valid_get` on every
/// value before anything is staged: an object id that names no
/// framebuffer or CRTC, a blob id that names no blob, and a value outside
/// the property's range are EINVAL at the property, and the out-fence
/// slot is never touched because the commit never ran. Here a
/// non-existent FB_ID, CRTC_ID and MODE_ID were staged and answered
/// ENOENT by the commit, with `-1` written into the slot; and the values
/// were truncated to the field's width instead of checked, so
/// `FB_ID = fb | 1 << 32` presented `fb`, `IN_FENCE_FD = 1 << 32` waited
/// on fd 0, `CRTC_W = 1 << 31` wrapped negative and `SRC_X = 1 << 32`
/// became 0. A compositor that reads ENOENT retires the object; one whose
/// bad value is accepted never learns it sent one.
#[test]
fn a_value_the_property_cannot_take_is_refused_at_the_property_not_by_the_commit() {
    let (_screen, c) = atomic_client(32, 8);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    const NO_SUCH: u64 = 0x999;
    let plane = drm::SYNTH_PLANE_ID;
    let crtc = drm::SYNTH_CRTC_ID;

    for (obj, prop, value, what) in [
        (
            plane,
            PROP_FB_ID,
            NO_SUCH,
            "a framebuffer that does not exist",
        ),
        (plane, PROP_CRTC_ID, NO_SUCH, "a CRTC that does not exist"),
        (
            crtc,
            PROP_MODE_ID,
            NO_SUCH,
            "a mode blob that does not exist",
        ),
        (
            plane,
            PROP_FB_DAMAGE_CLIPS,
            NO_SUCH,
            "a damage blob that does not exist",
        ),
        (
            plane,
            PROP_FB_ID,
            fb as u64 | 1 << 32,
            "a framebuffer id above 32 bits",
        ),
        (
            plane,
            PROP_CRTC_ID,
            crtc as u64 | 1 << 32,
            "a CRTC id above 32 bits",
        ),
        (
            plane,
            PROP_IN_FENCE_FD,
            1 << 32,
            "an in-fence fd above INT_MAX",
        ),
        (
            plane,
            PROP_IN_FENCE_FD,
            -2i64 as u64,
            "an in-fence fd below -1",
        ),
        (plane, PROP_CRTC_W, 1 << 31, "a CRTC_W above INT_MAX"),
        (plane, PROP_CRTC_H, u64::MAX, "a CRTC_H above INT_MAX"),
        (plane, PROP_SRC_X, 1 << 32, "a SRC_X above UINT_MAX"),
        (plane, PROP_SRC_W, 1 << 32, "a SRC_W above UINT_MAX"),
        (plane, PROP_CRTC_X, 1 << 31, "a CRTC_X above INT_MAX"),
        (crtc, PROP_ACTIVE, 2, "an ACTIVE that is neither 0 nor 1"),
    ] {
        let mut slot: i32 = UNWRITTEN;
        let req = Request::new(
            &[crtc, obj],
            &[1, 1],
            &[PROP_OUT_FENCE_PTR, prop],
            &[&mut slot as *mut i32 as u64, value],
        );
        assert_eq!(
            commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY),
            Err(FsError::InvalidParam),
            "{} was not refused as an invalid value",
            what,
        );
        assert_eq!(
            slot, UNWRITTEN,
            "{}: the commit never ran, so the fence slot must be left alone",
            what,
        );
    }

    // A property some other object owns is ENOENT even with a value it
    // could never take, and so is one nobody has: `drm_mode_atomic_ioctl`
    // looks the property up on the object before anything is checked.
    for (obj, prop, value, what) in [
        (crtc, PROP_FB_ID, NO_SUCH, "FB_ID on the CRTC"),
        (plane, PROP_ACTIVE, 2, "ACTIVE on the plane"),
        (plane, 0xDEAD, NO_SUCH, "a property nobody has"),
    ] {
        let mut slot: i32 = UNWRITTEN;
        let req = Request::new(
            &[crtc, obj],
            &[1, 1],
            &[PROP_OUT_FENCE_PTR, prop],
            &[&mut slot as *mut i32 as u64, value],
        );
        assert_eq!(
            commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY),
            Err(FsError::EntryNotFound),
            "{} was not refused as a property the object does not have",
            what,
        );
        assert_eq!(
            slot, UNWRITTEN,
            "{}: the fence slot must be left alone",
            what
        );
    }

    // The same properties with values they do take are staged: the
    // refusals above are the values, not the properties. FB_ID names the
    // real framebuffer and CRTC_ID the real CRTC, so the check phase
    // accepts the plane too.
    let mut slot: i32 = UNWRITTEN;
    let req = Request::new(
        &[crtc, plane],
        &[1, 11],
        &[
            PROP_OUT_FENCE_PTR,
            PROP_FB_ID,
            PROP_CRTC_ID,
            PROP_IN_FENCE_FD,
            PROP_CRTC_W,
            PROP_CRTC_H,
            PROP_SRC_X,
            PROP_SRC_W,
            PROP_CRTC_X,
            PROP_FB_DAMAGE_CLIPS,
            PROP_SRC_Y,
            PROP_SRC_H,
        ],
        &[
            &mut slot as *mut i32 as u64,
            fb as u64,
            crtc as u64,
            -1i64 as u64,
            32,
            8,
            0,
            32 << 16,
            0,
            0,
            0,
            8 << 16,
        ],
    );
    assert_eq!(commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY), Ok(0));
    assert_eq!(slot, -1, "TEST_ONLY writes -1 into the slot");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `TEST_ONLY` writes `-1` too: nothing was committed, so there is nothing
/// to fence. Handing back a real (already signaled) fd here would leak one
/// descriptor per `TEST_ONLY` probe, and wlroots probes on every output
/// reconfiguration.
#[test]
fn a_test_only_commit_writes_minus_one_and_presents_nothing() {
    let (screen, c) = atomic_client(32, 8);
    let mut slot: i32 = UNWRITTEN;
    let req = Request::new(
        &[drm::SYNTH_CRTC_ID],
        &[2],
        &[PROP_OUT_FENCE_PTR, PROP_ACTIVE],
        &[&mut slot as *mut i32 as u64, 0],
    );

    commit(&c, &req, DRM_MODE_ATOMIC_TEST_ONLY).expect("TEST_ONLY commit");

    // What this pins is that the slot IS written, with `-1`. That it is `-1`
    // *rather than a real fd* is not separable here and no sharper test will
    // separate it: installing the signaled stub needs a current thread with
    // a Linux fd table, and a hosted test has neither, so
    // `try_signaled_out_fence_fd` returns `None` and the success leg writes
    // `-1` too. Swapping the two legs therefore survives this module by
    // construction; the leg that is checkable is checked above and in
    // `a_failed_commit_still_writes_minus_one_into_the_out_fence_slot`.
    assert_eq!(slot, -1, "TEST_ONLY did not write the fence slot");
    assert!(
        (0..8).all(|y| (0..32).all(|x| screen.pixel(x, y) == kms_emu::UNTOUCHED)),
        "a TEST_ONLY commit put pixels on the screen"
    );
}

/// A property the walk rejects aborts the commit BEFORE the fence is
/// written. The client sees an error and its slot untouched, which is the
/// one case where not writing is right: no commit was attempted, so there is
/// no fence to describe -- and `-1` would look like "committed, no fence".
#[test]
fn a_rejected_property_aborts_before_the_fence_is_written() {
    let (_screen, c) = atomic_client(32, 8);
    let mut slot: i32 = UNWRITTEN;
    // The fence pointer is staged first, then an unknown property id.
    let req = Request::new(
        &[drm::SYNTH_CRTC_ID],
        &[2],
        &[PROP_OUT_FENCE_PTR, 0xDEAD],
        &[&mut slot as *mut i32 as u64, 0],
    );

    assert!(
        commit(&c, &req, 0).is_err(),
        "an unknown property was staged"
    );
    assert_eq!(
        slot, UNWRITTEN,
        "a commit that never ran handed the client a fence"
    );
}

/// A NULL out-fence pointer is legal and writes nothing. libdrm passes NULL
/// whenever the caller did not ask for a fence, so faulting on it would
/// refuse every ordinary commit.
#[test]
fn a_null_out_fence_pointer_is_accepted_and_writes_nothing() {
    let (_screen, c) = atomic_client(32, 8);
    let req = Request::new(
        &[drm::SYNTH_CRTC_ID],
        &[2],
        &[PROP_OUT_FENCE_PTR, PROP_ACTIVE],
        &[0, 0],
    );

    commit(&c, &req, 0).expect("a commit with no fence must be accepted");
}

/// The flag guards of the arm, all of which Linux enforces: an unknown flag,
/// a non-zero `reserved`, an async flip (this tree advertises no async
/// support) and `TEST_ONLY` carrying a flip event are each `EINVAL`. A
/// kernel that quietly accepts a flag it does not implement is worse than
/// one that refuses it: the client then waits for behaviour that never
/// arrives.
#[test]
fn the_flag_guards_refuse_what_this_tree_does_not_implement() {
    let (_screen, c) = atomic_client(32, 8);
    let req = benign_request();

    let unknown = DRM_MODE_ATOMIC_FLAGS.wrapping_add(1) & !DRM_MODE_ATOMIC_FLAGS;
    assert_eq!(commit(&c, &req, unknown), Err(FsError::InvalidParam));
    assert_eq!(
        commit(&c, &req, DRM_MODE_PAGE_FLIP_ASYNC),
        Err(FsError::InvalidParam),
        "an async flip was accepted without async support"
    );
    assert_eq!(
        commit(
            &c,
            &req,
            DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_PAGE_FLIP_EVENT
        ),
        Err(FsError::InvalidParam),
        "a test commit was allowed to queue a flip event"
    );
    // `reserved` must be zero.
    let mut ioctl = req.ioctl(0);
    ioctl.reserved = 1;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_ATOMIC, &mut ioctl),
        Err(FsError::InvalidParam)
    );
    // And the same request with none of that is fine, so the refusals above
    // are the flags and not the request.
    assert!(commit(&c, &req, 0).is_ok());
}

/// Turning a CRTC on is a modeset, and a modeset needs both the client's
/// `ALLOW_MODESET` flag and a mode. Linux refuses `ACTIVE=1` without the
/// flag ("[CRTC] requires full modeset") and again without a mode; a kernel
/// that let either through would light a CRTC with no timings programmed,
/// which on real hardware is a blank panel the compositor believes is up.
#[test]
fn activating_a_crtc_needs_both_the_modeset_flag_and_a_mode() {
    let (_screen, c) = atomic_client(32, 8);
    let req = Request::new(&[drm::SYNTH_CRTC_ID], &[1], &[PROP_ACTIVE], &[1]);

    assert_eq!(
        commit(&c, &req, 0),
        Err(FsError::InvalidParam),
        "a modeset went through without ALLOW_MODESET"
    );
    assert_eq!(
        commit(&c, &req, DRM_MODE_ATOMIC_ALLOW_MODESET),
        Err(FsError::InvalidParam),
        "a CRTC was activated with no mode set"
    );
    // Leaving it off is not a modeset, so the same property with the other
    // value needs neither.
    assert!(commit(&c, &benign_request(), 0).is_ok());
}

/// And with a mode in hand, the `ALLOW_MODESET` flag is still required on
/// its own. Both guards refuse the same request with the same errno, so this
/// is the only shape that tells them apart: a commit that carries a mode has
/// nothing left to object to except the missing flag. Linux's rule is that a
/// client which has not opted into modesetting never gets one -- wlroots
/// relies on it to probe configurations without disturbing the screen.
#[test]
fn a_modeset_that_carries_a_mode_still_needs_the_allow_modeset_flag() {
    let (_screen, c) = atomic_client(32, 8);
    // The panel's own mode: anything else is refused for a different reason.
    let mode = make_modeinfo(32, 8);
    let mut blob = DrmModeCreateBlob {
        data: mode.as_ptr() as u64,
        length: mode.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
        .expect("CREATEPROPBLOB");
    assert_ne!(blob.blob_id, 0, "the mode blob was not created");

    let req = Request::new(
        &[drm::SYNTH_CRTC_ID],
        &[2],
        &[PROP_MODE_ID, PROP_ACTIVE],
        &[u64::from(blob.blob_id), 1],
    );

    assert_eq!(
        commit(&c, &req, 0),
        Err(FsError::InvalidParam),
        "a modeset went through without ALLOW_MODESET"
    );
    commit(&c, &req, DRM_MODE_ATOMIC_ALLOW_MODESET)
        .expect("a modeset with the flag and a matching mode must be accepted");
}

/// A `MODE_ID` blob has to be exactly one `drm_mode_modeinfo` and has to
/// name the mode the panel actually scans out. Neither is pedantry: the
/// timings are read out of the blob by offset, so a short one reads past it,
/// and a mode this tree cannot scan out is the "wlroots picked a mode we
/// don't scan out" failure, where the commit succeeds and the screen stays
/// black.
///
/// Two of the three are defended twice, so do not go chasing a surviving
/// mutation here: a short blob and a missing one both end up read as
/// `0x0`, which the panel-mode comparison refuses anyway. Deleting either
/// of those two checks on its own therefore keeps every assertion below
/// green. What the test pins is the contract -- none of the three is ever
/// accepted -- not which line does the refusing.
#[test]
fn a_mode_blob_must_be_one_modeinfo_and_name_the_panels_own_mode() {
    let (_screen, c) = atomic_client(32, 8);

    let new_blob = |bytes: &[u8]| -> u32 {
        let mut blob = DrmModeCreateBlob {
            data: bytes.as_ptr() as u64,
            length: bytes.len() as u32,
            blob_id: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
            .expect("CREATEPROPBLOB");
        blob.blob_id
    };

    let short = new_blob(&[0u8; 32]);
    let wrong_size = new_blob(&make_modeinfo(64, 16));
    let unknown = 0x7FFF_FFFF;

    for (blob_id, what) in [
        (short, "a 32-byte blob"),
        (wrong_size, "a mode the panel does not have"),
        (unknown, "a blob that does not exist"),
    ] {
        let req = Request::new(
            &[drm::SYNTH_CRTC_ID],
            &[2],
            &[PROP_MODE_ID, PROP_ACTIVE],
            &[u64::from(blob_id), 1],
        );
        assert!(
            commit(&c, &req, DRM_MODE_ATOMIC_ALLOW_MODESET).is_err(),
            "{} was accepted as a mode",
            what
        );
    }
}
