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
