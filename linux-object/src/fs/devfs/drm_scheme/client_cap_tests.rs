//! `GET_CAP` and `SET_CLIENT_CAP` against `drm_getcap` and
//! `drm_setclientcap`: what is known, what is boolean, and what is EINVAL.
use super::gl_client_sequence_tests::Client;
use super::*;

fn get_cap(c: &Client, capability: u64) -> Result<u64> {
    let mut req = DrmGetCap {
        capability,
        value: 0xdead_beef,
    };
    c.ioctl(DRM_IOCTL_GET_CAP, &mut req).map(|_| req.value)
}

fn set_cap(c: &Client, cap: u64, value: u64) -> Result<usize> {
    let mut req: [u64; 2] = [cap, value];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut req)
}

#[test]
fn get_cap_knows_high_crtc_and_refuses_a_capability_it_has_never_heard_of() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    assert_eq!(get_cap(&c, 0x1), Ok(1), "DUMB_BUFFER");
    assert_eq!(
        get_cap(&c, 0x2),
        Ok(1),
        "VBLANK_HIGH_CRTC: WAIT_VBLANK reads it"
    );
    assert_eq!(get_cap(&c, 0x6), Ok(1), "TIMESTAMP_MONOTONIC");
    assert_eq!(get_cap(&c, 0x12), Ok(1), "CRTC_IN_VBLANK_EVENT");
    for known_zero in [0x7u64, 0x11, 0x15] {
        assert_eq!(get_cap(&c, known_zero), Ok(0), "cap {:#x}", known_zero);
    }
    for unknown in [0x0u64, 0xa, 0xf, 0x16, 0x100, u64::MAX] {
        let mut req = DrmGetCap {
            capability: unknown,
            value: 0xdead_beef,
        };
        assert_eq!(
            c.ioctl(DRM_IOCTL_GET_CAP, &mut req).err(),
            Some(FsError::InvalidParam),
            "cap {:#x}",
            unknown
        );
        assert_eq!(req.value, 0, "the value is zeroed, not left as it came");
    }
}

#[test]
fn set_client_cap_takes_booleans_and_refuses_hotspot_and_the_unknown() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    for boolean in [
        DRM_CLIENT_CAP_STEREO_3D,
        DRM_CLIENT_CAP_UNIVERSAL_PLANES,
        DRM_CLIENT_CAP_ASPECT_RATIO,
    ] {
        assert_eq!(set_cap(&c, boolean, 0), Ok(0), "cap {} off", boolean);
        assert_eq!(set_cap(&c, boolean, 1), Ok(0), "cap {} on", boolean);
        for bad in [2u64, 5, u64::MAX] {
            assert_eq!(
                set_cap(&c, boolean, bad),
                Err(FsError::InvalidParam),
                "cap {} value {}",
                boolean,
                bad
            );
        }
    }
    assert_eq!(
        set_cap(&c, DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT, 1),
        Err(FsError::OpNotSupported),
        "no virtualised cursor plane here"
    );
    assert_eq!(
        set_cap(&c, DRM_CLIENT_CAP_WRITEBACK_CONNECTORS, 1),
        Err(FsError::InvalidParam),
        "writeback needs an atomic client first"
    );
    for unknown in [0u64, 7, 8, 0x100, u64::MAX] {
        assert_eq!(
            set_cap(&c, unknown, 1),
            Err(FsError::InvalidParam),
            "cap {}",
            unknown
        );
        assert_eq!(set_cap(&c, unknown, 0), Err(FsError::InvalidParam));
    }
}

/// `drm_setclientcap` stores the ATOMIC value in `atomic`,
/// `universal_planes` AND `aspect_ratio_allowed` ("No atomic user-space
/// blows up on aspect ratio mode data"): an atomic client gets the
/// aspect-ratio codes through SETCRTC and back from GETCRTC without ever
/// asking for `DRM_CLIENT_CAP_ASPECT_RATIO`, and ATOMIC 0 takes them
/// away again. Here the cap set the first two fields only, so every
/// atomic compositor had EINVAL on a SETCRTC with an aspect-ratio code
/// ("Unexpected aspect-ratio flag bits") and read none back.
#[test]
fn set_client_cap_atomic_grants_the_aspect_ratio_codes_too() {
    let (_screen, c) = super::out_fence_tests::atomic_client(32, 8);
    let state = c.file_state();
    assert!(state.atomic_client() && state.universal_planes());
    assert!(state.aspect_ratio(), "ATOMIC 1 is aspect_ratio_allowed too");

    let (crtc, conn) = (drm::SYNTH_CRTC_ID, drm::SYNTH_CONNECTOR_ID);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let mut with_code = make_modeinfo(32, 8);
    let flags = u32::from_ne_bytes([with_code[28], with_code[29], with_code[30], with_code[31]]);
    let code = 4 << 19;
    with_code[28..32].copy_from_slice(&(flags | code).to_ne_bytes());
    let setcrtc = |mode: [u8; 68]| {
        let connectors = [conn];
        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: connectors.as_ptr() as u64,
            count_connectors: 1,
            crtc_id: crtc,
            fb_id: fb,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode,
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
    };
    let aspect_bits_read_back = || {
        let mut res = DrmModeGetCrtc {
            set_connectors_ptr: 0,
            count_connectors: 0,
            crtc_id: crtc,
            fb_id: 0,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 0,
            mode: [0xAB; 68],
        };
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut res).expect("GETCRTC");
        u32::from_ne_bytes([res.mode[28], res.mode[29], res.mode[30], res.mode[31]])
            & DRM_MODE_FLAG_PIC_AR_MASK
    };

    // The code goes through and comes back, with no ASPECT_RATIO cap set.
    assert_eq!(
        setcrtc(with_code),
        Ok(0),
        "aspect-ratio code 4 as an atomic client"
    );
    assert_eq!(aspect_bits_read_back(), code);

    // ATOMIC 0: the three fields go together, and the code is refused
    // and hidden again.
    assert_eq!(set_cap(&c, DRM_CLIENT_CAP_ATOMIC, 0), Ok(0));
    assert!(!state.atomic_client() && !state.universal_planes());
    assert!(
        !state.aspect_ratio(),
        "ATOMIC 0 takes aspect_ratio_allowed away"
    );
    assert_eq!(setcrtc(with_code), Err(FsError::InvalidParam));
    assert_eq!(
        aspect_bits_read_back(),
        0,
        "the bits are masked without the cap"
    );

    // ATOMIC 2 (relaxed checking) grants them like 1 does.
    assert_eq!(set_cap(&c, DRM_CLIENT_CAP_ATOMIC, 2), Ok(0));
    assert!(state.aspect_ratio());
    assert_eq!(aspect_bits_read_back(), code);
}
