//! `drm_mode_getproperty_ioctl`: the two arrays go out as far as the
//! caller made room, the counts come back as totals, and which counts are
//! written depends on the property's type.

use super::gl_client_sequence_tests::Client;
use super::*;
use alloc::vec;
use alloc::vec::Vec;

const UNTOUCHED: u64 = 0xEEEE_EEEE_EEEE_EEEE;

fn untouched_enum() -> DrmModePropertyEnum {
    DrmModePropertyEnum {
        value: UNTOUCHED,
        name: [0xEE; 32],
    }
}

fn getprop(
    c: &Client,
    prop_id: u32,
    values: Option<&mut Vec<u64>>,
    enums: Option<&mut Vec<DrmModePropertyEnum>>,
    count_enum_blobs: u32,
) -> (Result<usize>, DrmModeGetProperty) {
    let mut gp = DrmModeGetProperty {
        values_ptr: values.as_ref().map_or(0, |v| v.as_ptr() as u64),
        enum_blob_ptr: enums.as_ref().map_or(0, |e| e.as_ptr() as u64),
        prop_id,
        flags: 0,
        name: [0; 32],
        count_values: values.as_ref().map_or(0, |v| v.len() as u32),
        count_enum_blobs,
    };
    let r = c.ioctl(DRM_IOCTL_MODE_GETPROPERTY, &mut gp);
    (r, gp)
}

fn name_of(e: &DrmModePropertyEnum) -> &str {
    let n = e.name.iter().position(|&b| b == 0).unwrap_or(32);
    core::str::from_utf8(&e.name[..n]).unwrap()
}

/// With no room the counts come back as totals and nothing is written;
/// with room for part of a list, that part goes out and the count still
/// says the total; room beyond the total is left alone. `count_values`
/// comes back for every property; `count_enum_blobs` only for an enum (the
/// total) or a blob (0), while a range or an object property leaves the
/// caller's in place, as `drm_mode_getproperty_ioctl` does. Each array was
/// filled all or nothing, and `count_enum_blobs` was zeroed for every
/// property.
#[test]
fn getproperty_fills_as_far_as_the_caller_made_room() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(85);

    // No room: the totals, and the type decides which counts are written.
    // (77 with a null array is room claimed for it: for an enum property
    // that is EFAULT, below; for the others there is nothing to write.)
    let (r, gp) = getprop(&c, PROP_DPMS, None, None, 0);
    assert_eq!(r, Ok(0));
    assert_eq!((gp.count_values, gp.count_enum_blobs), (4, 4), "DPMS: enum");
    assert_eq!(&gp.name[..4], b"DPMS");
    let (_, gp) = getprop(&c, PROP_NON_DESKTOP, None, None, 77);
    assert_eq!(
        (gp.count_values, gp.count_enum_blobs),
        (2, 77),
        "range: the caller's"
    );
    let (_, gp) = getprop(&c, PROP_FB_ID, None, None, 77);
    assert_eq!(
        (gp.count_values, gp.count_enum_blobs),
        (1, 77),
        "object: the caller's"
    );
    let (_, gp) = getprop(&c, PROP_EDID, None, None, 77);
    assert_eq!((gp.count_values, gp.count_enum_blobs), (0, 0), "blob: zero");

    // Room for part of each list: that much, and the totals.
    let mut values = vec![UNTOUCHED; 3];
    let mut enums = vec![untouched_enum(); 4];
    let (r, gp) = getprop(&c, PROP_DPMS, Some(&mut values), Some(&mut enums), 3);
    assert_eq!(r, Ok(0));
    assert_eq!((gp.count_values, gp.count_enum_blobs), (4, 4));
    assert_eq!(values, [0, 1, 2], "the first three values");
    assert_eq!(
        enums[..3].iter().map(name_of).collect::<Vec<_>>(),
        ["On", "Standby", "Suspend"],
        "the first three enum records"
    );
    assert_eq!(enums[2].value, 2);
    assert_eq!(enums[3].value, UNTOUCHED, "no room was made for the fourth");

    // Room beyond the total: the list, and the rest left alone.
    let mut values = vec![UNTOUCHED; 6];
    let mut enums = vec![untouched_enum(); 6];
    let (r, gp) = getprop(&c, PROP_DPMS, Some(&mut values), Some(&mut enums), 6);
    assert_eq!(r, Ok(0));
    assert_eq!((gp.count_values, gp.count_enum_blobs), (4, 4));
    assert_eq!(values, [0, 1, 2, 3, UNTOUCHED, UNTOUCHED]);
    assert_eq!(name_of(&enums[3]), "Off");
    assert_eq!(enums[4].value, UNTOUCHED);

    // A range with room for one: its minimum only.
    let mut values = vec![UNTOUCHED; 1];
    let (r, gp) = getprop(&c, PROP_CRTC_W, Some(&mut values), None, 0);
    assert_eq!(r, Ok(0));
    assert_eq!(gp.count_values, 2);
    assert_eq!(values, [0]);
}

/// A null array with room claimed for it is EFAULT (`put_user` to
/// nowhere), not silently skipped; values go before enums, so a bad values
/// array faults before any enum record is written. A null array with no
/// room is simply not written.
#[test]
fn a_null_array_with_room_is_efault_and_values_fault_before_enums() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(85);
    let efault = Err(FsError::BadAddress);

    let mut enums = vec![untouched_enum(); 4];
    let mut gp = DrmModeGetProperty {
        values_ptr: 0,
        enum_blob_ptr: enums.as_mut_ptr() as u64,
        prop_id: PROP_DPMS,
        flags: 0,
        name: [0; 32],
        count_values: 1,
        count_enum_blobs: 4,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_GETPROPERTY, &mut gp),
        efault,
        "values: null with room"
    );
    assert_eq!(enums[0].value, UNTOUCHED, "the enums were not reached");

    let mut values = vec![UNTOUCHED; 4];
    let (r, _) = getprop(&c, PROP_DPMS, Some(&mut values), None, 1);
    assert_eq!(r, efault, "enums: null with room");
    assert_eq!(values, [0, 1, 2, 3], "after the values went out");

    let (r, gp) = getprop(&c, PROP_DPMS, None, None, 0);
    assert_eq!(r, Ok(0), "null with no room is not written");
    assert_eq!(gp.count_values, 4);
}
