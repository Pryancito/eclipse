//! The two-call pattern of every KMS list query: room in the count
//! without an array behind it.

use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu;

/// `drm_mode_getresources`, `drm_mode_getplane_res`,
/// `drm_mode_getconnector`, `drm_mode_getplane` and
/// `drm_mode_obj_get_properties_ioctl` all copy with `put_user` /
/// `copy_to_user` as far as the count allows (the connector's encoders
/// and modes and the plane's formats only when the whole list fits),
/// so a count with a null array is EFAULT, and a count of 0 is the
/// size probe that answers the totals. Every one of them took the
/// null array as "do not copy" and answered 0 with the counts
/// written, so a client that mis-sized its second call read a list
/// it never got.
#[test]
fn room_in_the_count_without_an_array_is_a_fault_in_every_kms_list_query() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(81);
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap).unwrap();
    let efault = Err(FsError::BadAddress);

    // GETRESOURCES: a probe, then room without arrays.
    let probe = |c: &Client| -> Result<DrmModeCardRes> {
        let mut r: DrmModeCardRes = unsafe { core::mem::zeroed() };
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut r)?;
        Ok(r)
    };
    let res = probe(&c).unwrap();
    assert!(res.count_crtcs >= 1 && res.count_connectors >= 1 && res.count_encoders >= 1);
    for which in 0..3 {
        let mut r: DrmModeCardRes = unsafe { core::mem::zeroed() };
        match which {
            0 => r.count_crtcs = res.count_crtcs,
            1 => r.count_connectors = res.count_connectors,
            _ => r.count_encoders = res.count_encoders,
        }
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut r),
            efault,
            "list {which} with room and no array"
        );
    }

    // GETPLANERESOURCES.
    let mut pr = DrmModeGetPlaneRes {
        plane_id_ptr: 0,
        count_planes: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut pr).unwrap();
    assert!(pr.count_planes >= 1, "universal planes list the primary");
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut pr), efault);
    let mut planes = alloc::vec![0u32; pr.count_planes as usize];
    pr.plane_id_ptr = planes.as_mut_ptr() as u64;
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut pr).unwrap();
    let plane_id = planes[0];

    // GETCONNECTOR: encoders, modes and properties.
    let mut conn: DrmModeGetConnector = unsafe { core::mem::zeroed() };
    conn.connector_id = drm::SYNTH_CONNECTOR_ID;
    c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn).unwrap();
    let (encoders, modes, props) = (conn.count_encoders, conn.count_modes, conn.count_props);
    assert!(encoders >= 1 && modes >= 1 && props >= 1);
    for which in 0..3 {
        let mut r: DrmModeGetConnector = unsafe { core::mem::zeroed() };
        r.connector_id = drm::SYNTH_CONNECTOR_ID;
        match which {
            0 => r.count_encoders = encoders,
            1 => r.count_modes = modes,
            _ => r.count_props = props,
        }
        assert_eq!(
            c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut r),
            efault,
            "connector list {which} with room and no array"
        );
    }
    // A property list needs both arrays.
    let mut ids = alloc::vec![0u32; props as usize];
    let mut r: DrmModeGetConnector = unsafe { core::mem::zeroed() };
    r.connector_id = drm::SYNTH_CONNECTOR_ID;
    r.count_props = props;
    r.props_ptr = ids.as_mut_ptr() as u64;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut r),
        efault,
        "no values array"
    );
    // Less room than the whole list is not a copy of the encoders or
    // the modes (still the size probe), but it is of the properties.
    let mut r: DrmModeGetConnector = unsafe { core::mem::zeroed() };
    r.connector_id = drm::SYNTH_CONNECTOR_ID;
    r.count_encoders = encoders;
    r.count_modes = modes;
    r.count_props = props;
    let mut enc = alloc::vec![0u32; encoders as usize];
    let mut mode_bytes = alloc::vec![0u8; 68 * modes as usize];
    let mut vals = alloc::vec![0u64; props as usize];
    r.encoders_ptr = enc.as_mut_ptr() as u64;
    r.modes_ptr = mode_bytes.as_mut_ptr() as u64;
    r.props_ptr = ids.as_mut_ptr() as u64;
    r.prop_values_ptr = vals.as_mut_ptr() as u64;
    c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut r).unwrap();
    assert_eq!(enc[0], drm::SYNTH_ENCODER_ID);
    assert!(ids.iter().all(|&id| id != 0), "the whole property list");

    // GETPLANE: the formats.
    let mut gp = DrmModeGetPlane {
        plane_id,
        crtc_id: 0,
        fb_id: 0,
        possible_crtcs: 0,
        gamma_size: 0,
        count_format_types: 0,
        format_type_ptr: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut gp).unwrap();
    let formats = gp.count_format_types;
    assert!(formats >= 1);
    gp.count_format_types = formats;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut gp), efault);
    gp.count_format_types = formats - 1;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut gp),
        Ok(0),
        "not enough room: no copy, the total again"
    );
    assert_eq!(gp.count_format_types, formats);

    // OBJ_GETPROPERTIES.
    let mut op = DrmModeObjGetProperties {
        props_ptr: 0,
        prop_values_ptr: 0,
        count_props: 0,
        // The connector: its properties show to a legacy client too
        // (the CRTC's are all atomic).
        obj_id: drm::SYNTH_CONNECTOR_ID,
        obj_type: DRM_MODE_OBJECT_CONNECTOR,
    };
    c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut op).unwrap();
    assert!(op.count_props >= 1);
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut op), efault);
    // One entry of room is one entry copied (partial, as
    // `drm_mode_object_get_properties`), and the total comes back.
    let (mut one_id, mut one_val) = (0u32, 0u64);
    op.count_props = 1;
    op.props_ptr = &mut one_id as *mut u32 as u64;
    op.prop_values_ptr = &mut one_val as *mut u64 as u64;
    let total = {
        c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut op).unwrap();
        op.count_props
    };
    assert!(one_id != 0 && total >= 1);
}
