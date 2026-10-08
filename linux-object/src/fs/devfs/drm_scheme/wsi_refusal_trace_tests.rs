use super::*;

/// The id a refusal names comes from a different field in each struct, so
/// a common-prefix read would print a pointer. The two enumerating
/// queries name no object at all and must say `0`, not whatever word
/// happens to sit at their front (`fb_id_ptr`, `plane_id_ptr`).
#[test]
fn each_query_reports_the_object_it_actually_asked_about() {
    let conn = DrmModeGetConnector {
        encoders_ptr: 0xdead_0001,
        modes_ptr: 0xdead_0002,
        props_ptr: 0xdead_0003,
        prop_values_ptr: 0xdead_0004,
        count_modes: 1,
        count_props: 5,
        count_encoders: 1,
        encoder_id: 77,
        connector_id: 1001,
        connector_type: 11,
        connector_type_id: 1,
        connection: 1,
        mm_width: 0,
        mm_height: 0,
        subpixel: 0,
        pad: 0,
    };
    assert_eq!(
        wsi_query_object_id(DRM_IOCTL_MODE_GETCONNECTOR, &conn as *const _ as usize),
        1001,
        "GETCONNECTOR must report connector_id, not the encoder before it"
    );

    let crtc = DrmModeGetCrtc {
        set_connectors_ptr: 0xdead_0005,
        count_connectors: 0,
        crtc_id: 2001,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    assert_eq!(
        wsi_query_object_id(DRM_IOCTL_MODE_GETCRTC, &crtc as *const _ as usize),
        2001
    );

    let plane = DrmModeGetPlane {
        plane_id: 3001,
        crtc_id: 0,
        fb_id: 0,
        possible_crtcs: 1,
        gamma_size: 0,
        count_format_types: 0,
        format_type_ptr: 0,
    };
    assert_eq!(
        wsi_query_object_id(DRM_IOCTL_MODE_GETPLANE, &plane as *const _ as usize),
        3001
    );

    let enc = DrmModeGetEncoder {
        encoder_id: 4001,
        encoder_type: 6,
        crtc_id: 0,
        possible_crtcs: 1,
        possible_clones: 0,
    };
    assert_eq!(
        wsi_query_object_id(DRM_IOCTL_MODE_GETENCODER, &enc as *const _ as usize),
        4001
    );

    // The enumerating pair: no object, and their first word is a pointer.
    let res = DrmModeGetPlaneRes {
        plane_id_ptr: 0x7fff_dead_beef,
        count_planes: 2,
    };
    assert_eq!(
        wsi_query_object_id(DRM_IOCTL_MODE_GETPLANERESOURCES, &res as *const _ as usize),
        0,
        "GETPLANERESOURCES names no object; it must not print its pointer"
    );
    // And an ioctl that is not a KMS query is not on this channel at all.
    assert!(wsi_query_name(DRM_IOCTL_SYNCOBJ_CREATE).is_none());
    assert_eq!(
        wsi_query_name(DRM_IOCTL_MODE_GETRESOURCES),
        Some("GETRESOURCES")
    );
}

/// The claim-a-slot rule, over a table of this test's own: the kernel's
/// is global and every other test in this binary fills it through the
/// dispatch wrapper, so capacity there is nobody's to assert.
///
/// What has to hold: a repeated refusal prints once (libdrm's
/// `drmModeGetConnector` retries, and a client that polls the KMS queries
/// must not storm the console), a *different* refusal still gets through
/// after it (the bug that hid the failing connector behind everything
/// logged before it), and the table stops rather than grows when a client
/// invents ids.
#[test]
fn a_repeated_refusal_prints_once_and_never_crowds_out_a_new_one() {
    use core::sync::atomic::{AtomicBool, AtomicU64};
    const GETCONNECTOR_NR: u32 = 0xA7;
    const GETRESOURCES_NR: u32 = 0xA0;
    const SLOTS: usize = 4;
    let seen: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    let full = AtomicBool::new(false);
    let take = |nr, id| wsi_fail_take_in(&seen, &full, nr, id);

    assert!(take(GETCONNECTOR_NR, 1001), "first sight prints");
    assert!(
        !take(GETCONNECTOR_NR, 1001),
        "the same refusal, repeated, is silent"
    );
    assert!(
        take(GETCONNECTOR_NR, 1002),
        "a different connector is a different refusal"
    );
    assert!(
        take(GETRESOURCES_NR, 0),
        "object 0 on another query is its own entry, not an empty slot"
    );
    assert!(
        !take(GETRESOURCES_NR, 0),
        "...and it is remembered like any other"
    );

    // Three slots spent on three distinct refusals; one left.
    assert!(take(GETCONNECTOR_NR, 1003));
    assert!(
        !take(GETCONNECTOR_NR, 1004),
        "past the last slot the channel goes quiet"
    );
    // A refusal already in the table is still recognised as a repeat
    // rather than re-reported now that the table is full.
    assert!(!take(GETCONNECTOR_NR, 1001));

    // The one pair that collides with the empty-slot sentinel: without
    // the +1 the key for `(0, 0)` IS zero, so claiming the slot leaves it
    // reading as empty and the same refusal prints for ever.
    let fresh: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let fresh_full = AtomicBool::new(false);
    assert!(wsi_fail_take_in(&fresh, &fresh_full, 0, 0));
    assert!(
        !wsi_fail_take_in(&fresh, &fresh_full, 0, 0),
        "nr 0 / id 0 must be remembered like any other key"
    );
}
