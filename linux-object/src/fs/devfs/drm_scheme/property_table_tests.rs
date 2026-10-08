//! The KMS property table, and the contract between advertising a property
//! and accepting it in an atomic commit.
//!
//! Userspace matches properties **by name**: libdrm hands a compositor
//! `drmModePropertyRes.name` and wlroots does `strcmp(prop->name, "FB_ID")`.
//! So a typo in this table does not produce an error anywhere -- the
//! property simply stops existing for every client, and the compositor
//! falls back or gives up. Nothing in the tree checked those strings.
//!
//! The other half is drift between the two sides of the same property.
//! `connector_props`/`crtc_props`/`plane_props` say which properties an
//! object has; `prop_spec` describes them; `atomic_stage_on` accepts them.
//! Three lists that must agree, in three different places in this file.

use super::*;

/// Every property id the pipeline uses. Kept honest in both directions by
/// `every_property_the_pipeline_uses_is_in_the_table`, which also sweeps
/// the id space so a `PROP_*` added to `prop_spec` and forgotten here
/// fails rather than quietly escaping every test below.
const ALL_PROPS: &[(u32, &str)] = &[
    (PROP_TYPE, "type"),
    (PROP_EDID, "EDID"),
    (PROP_DPMS, "DPMS"),
    (PROP_LINK_STATUS, "link-status"),
    (PROP_NON_DESKTOP, "non-desktop"),
    (PROP_FB_ID, "FB_ID"),
    (PROP_CRTC_ID, "CRTC_ID"),
    (PROP_CRTC_X, "CRTC_X"),
    (PROP_CRTC_Y, "CRTC_Y"),
    (PROP_CRTC_W, "CRTC_W"),
    (PROP_CRTC_H, "CRTC_H"),
    (PROP_SRC_X, "SRC_X"),
    (PROP_SRC_Y, "SRC_Y"),
    (PROP_SRC_W, "SRC_W"),
    (PROP_SRC_H, "SRC_H"),
    (PROP_ACTIVE, "ACTIVE"),
    (PROP_MODE_ID, "MODE_ID"),
    (PROP_IN_FENCE_FD, "IN_FENCE_FD"),
    (PROP_OUT_FENCE_PTR, "OUT_FENCE_PTR"),
    (PROP_FB_DAMAGE_CLIPS, "FB_DAMAGE_CLIPS"),
];

/// `DRM_MODE_PROP_LEGACY_TYPE` / `DRM_MODE_PROP_EXTENDED_TYPE` from
/// `uapi/drm/drm_mode.h`. BITMASK (1 << 5) is in the legacy mask even
/// though this tree has no bitmask property yet.
const LEGACY_TYPE: u32 = 0x0000_003a;
const EXTENDED_TYPE: u32 = 0x0000_ffc0;

fn spec(prop_id: u32) -> PropSpec {
    prop_spec(prop_id).expect("every property in ALL_PROPS must be in the table")
}

/// The name field of `struct drm_mode_get_property` is `char name[32]`,
/// and `GETPROPERTY` copies at most 31 bytes into it to leave the NUL.
const NAME_FIELD: usize = 32;

#[test]
fn every_property_the_pipeline_uses_is_in_the_table() {
    for (id, name) in ALL_PROPS {
        assert!(
            prop_spec(*id).is_some(),
            "property {} ({}) has no entry, so GETPROPERTY answers ENOENT \
             for a property the object says it has",
            name,
            id,
        );
    }
    // And the other way round. The ids are hand-assigned small integers,
    // so sweeping well past the end of the range is enough to find one
    // that `prop_spec` knows and this module does not -- which would
    // otherwise slip through every test here without a sound.
    let known: alloc::vec::Vec<u32> = (0..1024).filter(|id| prop_spec(*id).is_some()).collect();
    let listed: alloc::vec::Vec<u32> = ALL_PROPS.iter().map(|(id, _)| *id).collect();
    assert_eq!(
        known, listed,
        "the property table and ALL_PROPS have drifted apart",
    );
    assert!(prop_spec(u32::MAX).is_none());
}

/// Two properties sharing an id means the second is unreachable. In
/// `prop_spec` and `atomic_stage_on` the compiler already says so --
/// `unreachable_patterns`, which `deny(warnings)` turns into an error --
/// so what is left for this test is the list above, where a copy-paste
/// that pairs the wrong constant with a name compiles fine and would
/// silently stop testing one of the two properties.
#[test]
fn property_ids_are_unique() {
    for (i, (id, name)) in ALL_PROPS.iter().enumerate() {
        for (other_id, other_name) in &ALL_PROPS[i + 1..] {
            assert_ne!(id, other_id, "{} and {} share id {}", name, other_name, id,);
        }
    }
}

/// The names are the uAPI. A compositor finds a property by comparing this
/// string against a literal, so "CRTC_W" spelled "CRTC_w" is not a
/// warning anywhere -- the plane just stops having a width.
#[test]
fn every_property_name_is_the_one_userspace_matches_on() {
    for (id, name) in ALL_PROPS {
        assert_eq!(
            spec(*id).name,
            *name,
            "property {} is advertised under a different name than Linux's",
            id,
        );
    }
}

/// `GETPROPERTY` truncates into `char name[32]`, keeping 31 bytes. A
/// longer name is not refused; it arrives cut, and the compare fails.
#[test]
fn no_name_is_long_enough_to_be_truncated_on_the_way_out() {
    for (id, name) in ALL_PROPS {
        let s = spec(*id);
        assert!(!s.name.is_empty(), "{} has no name at all", id);
        assert!(
            s.name.len() < NAME_FIELD,
            "{} is {} bytes and would reach userspace cut to {}",
            name,
            s.name.len(),
            NAME_FIELD - 1,
        );
        for (val, enum_name) in s.enums {
            assert!(
                enum_name.len() < NAME_FIELD,
                "{}={} of {} would reach userspace cut",
                enum_name,
                val,
                name,
            );
        }
    }
}

/// `drm_property_type_valid()`: a property carries a legacy type or an
/// extended one, never both and never neither. `GETPROPERTY` reports
/// `flags` verbatim, and libdrm switches on exactly this to decide whether
/// to read the value list as a range, an enum or an object id.
#[test]
fn every_property_has_exactly_one_type() {
    for (id, name) in ALL_PROPS {
        let flags = spec(*id).flags;
        let legacy = flags & LEGACY_TYPE;
        let extended = flags & EXTENDED_TYPE;
        if extended != 0 {
            assert_eq!(
                legacy, 0,
                "{} carries an extended type and a legacy one at once",
                name,
            );
        } else {
            assert_ne!(legacy, 0, "{} has no type at all", name);
            assert!(
                legacy.is_power_of_two(),
                "{} carries more than one legacy type ({:#x})",
                name,
                legacy,
            );
        }
    }
}

/// An enum property serves both lists, and `GETPROPERTY` fills them from
/// two different fields of the same spec. They have to be the same set, in
/// the same order: a client that reads `values` to know what it may set,
/// and `enum_blob` to name it, would otherwise see two different menus.
#[test]
fn enum_properties_list_their_own_values() {
    for (id, name) in ALL_PROPS {
        let s = spec(*id);
        if s.flags & DRM_MODE_PROP_ENUM == 0 {
            assert!(
                s.enums.is_empty(),
                "{} is not an enum but carries enum entries",
                name,
            );
            continue;
        }
        assert!(!s.enums.is_empty(), "{} is an enum with no entries", name);
        let from_enums: alloc::vec::Vec<u64> = s.enums.iter().map(|(v, _)| *v).collect();
        assert_eq!(
            s.values,
            &from_enums[..],
            "{}'s value list and enum list disagree",
            name,
        );
    }
}

/// A range property's value list is `[min, max]` -- exactly two, in order.
/// The order is what tells the two range types apart: `CRTC_X` spans
/// `i32::MIN..=i32::MAX`, whose bit patterns as `u64` run *backwards*, so
/// a property marked plain RANGE when it should be SIGNED_RANGE fails
/// here. That is not cosmetic: libdrm clamps a client's value to the
/// advertised range, and an unsigned reading of `i32::MIN` is 4 billion.
#[test]
fn range_properties_carry_a_min_and_a_max_in_their_own_signedness() {
    for (id, name) in ALL_PROPS {
        let s = spec(*id);
        let signed = s.flags & EXTENDED_TYPE == DRM_MODE_PROP_SIGNED_RANGE;
        if s.flags & DRM_MODE_PROP_RANGE == 0 && !signed {
            continue;
        }
        assert_eq!(
            s.values.len(),
            2,
            "{} is a range and must list exactly [min, max]",
            name,
        );
        let (min, max) = (s.values[0], s.values[1]);
        if signed {
            assert!(
                (min as i64) <= (max as i64),
                "{} has min {} above max {} read as signed",
                name,
                min as i64,
                max as i64,
            );
        } else {
            assert!(min <= max, "{} has min {} above max {}", name, min, max);
        }
    }
}

/// An object property names the one object type it accepts, and a blob
/// property carries no list at all: its value *is* the blob id.
#[test]
fn object_and_blob_properties_carry_the_list_their_type_implies() {
    for (id, name) in ALL_PROPS {
        let s = spec(*id);
        if s.flags & EXTENDED_TYPE == DRM_MODE_PROP_OBJECT {
            assert_eq!(
                s.values.len(),
                1,
                "{} must name exactly one object type",
                name,
            );
        }
        if s.flags & DRM_MODE_PROP_BLOB != 0 {
            assert!(s.values.is_empty(), "{} is a blob with a value list", name);
            assert!(s.enums.is_empty(), "{} is a blob with enum entries", name);
        }
    }
}

/// A plane whose ids do not matter: `atomic_stage_on` is past the lookup.
fn a_plane() -> drm::DrmPlane {
    drm::DrmPlane {
        id: drm::SYNTH_PLANE_ID,
        crtc_id: drm::SYNTH_CRTC_ID,
        fb_id: 0,
        possible_crtcs: 1,
        plane_type: 1,
    }
}

fn advertised() -> alloc::vec::Vec<(AtomicObject, u32, u64)> {
    let mut out = alloc::vec::Vec::new();
    for (p, v) in plane_props(&a_plane(), true) {
        out.push((AtomicObject::Plane, p, v));
    }
    for (p, v) in crtc_props(true) {
        out.push((AtomicObject::Crtc, p, v));
    }
    for (p, v) in connector_props(2, true) {
        out.push((AtomicObject::Connector, p, v));
    }
    out
}

/// The advertised set is the menu a compositor gets from
/// `GETPLANE`/`OBJ_GETPROPERTIES`, and a property left out of it does not
/// exist as far as userspace is concerned -- staging it still works, so
/// nothing else in this module would notice. Pin the whole set.
#[test]
fn each_object_advertises_the_whole_set_of_properties_it_can_stage() {
    let _serialised = drm::test_globals::lock();

    let mut plane: alloc::vec::Vec<u32> = plane_props(&a_plane(), true)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    plane.sort_unstable();
    let mut want = alloc::vec![
        PROP_TYPE,
        PROP_FB_ID,
        PROP_CRTC_ID,
        PROP_CRTC_X,
        PROP_CRTC_Y,
        PROP_CRTC_W,
        PROP_CRTC_H,
        PROP_SRC_X,
        PROP_SRC_Y,
        PROP_SRC_W,
        PROP_SRC_H,
        PROP_IN_FENCE_FD,
        PROP_FB_DAMAGE_CLIPS,
    ];
    want.sort_unstable();
    assert_eq!(plane, want, "the plane's property menu changed");

    let mut crtc: alloc::vec::Vec<u32> = crtc_props(true).into_iter().map(|(p, _)| p).collect();
    crtc.sort_unstable();
    let mut want = alloc::vec![PROP_ACTIVE, PROP_MODE_ID, PROP_OUT_FENCE_PTR];
    want.sort_unstable();
    assert_eq!(crtc, want, "the CRTC's property menu changed");

    // EDID is only advertised when a display actually has one, which is
    // never on the host; everything else on the connector is fixed.
    let mut conn: alloc::vec::Vec<u32> = connector_props(2, true)
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| *p != PROP_EDID)
        .collect();
    conn.sort_unstable();
    let mut want = alloc::vec![PROP_DPMS, PROP_LINK_STATUS, PROP_NON_DESKTOP, PROP_CRTC_ID,];
    want.sort_unstable();
    assert_eq!(conn, want, "the connector's property menu changed");
}

/// Linux hides atomic properties from a client that never set
/// `DRM_CLIENT_CAP_ATOMIC` (`drm_mode_object_get_properties` skips
/// `DRM_MODE_PROP_ATOMIC`). Showing them to a legacy client -- X11, or
/// anything driving the pipeline through SETCRTC -- invites it to set
/// properties the legacy path never reads back.
#[test]
fn a_non_atomic_client_is_shown_no_atomic_properties() {
    let _serialised = drm::test_globals::lock();
    let legacy = plane_props(&a_plane(), false)
        .into_iter()
        .chain(crtc_props(false))
        .chain(connector_props(2, false));
    for (prop_id, _) in legacy {
        let s = spec(prop_id);
        assert_eq!(
            s.flags & DRM_MODE_PROP_ATOMIC,
            0,
            "{} is an atomic property and was shown to a legacy client",
            s.name,
        );
    }
}

#[test]
fn every_property_an_object_advertises_is_described_by_the_table() {
    let _serialised = drm::test_globals::lock();
    for (obj, prop_id, _) in advertised() {
        assert!(
            prop_spec(prop_id).is_some(),
            "{:?} advertises property {} that GETPROPERTY cannot describe",
            obj,
            prop_id,
        );
    }
}

/// The invariant that ties the three lists together. An object advertises
/// a property, so a commit naming it must not be told it does not exist:
/// either it stages, or it is refused as unsettable. ENOENT is reserved
/// for a property the object really does not have.
///
/// This is also where the tree used to diverge from Linux. Linux's
/// `drm_mode_atomic_ioctl` looks the property up *first* and only then
/// refuses an immutable one, so EDID / link-status / non-desktop on a
/// connector are EINVAL there; here they fell through to ENOENT, which
/// reads to a compositor as the property having disappeared between the
/// enumeration and the commit.
#[test]
fn a_property_an_object_advertises_is_never_answered_enoent() {
    let _serialised = drm::test_globals::lock();
    for (obj, prop_id, value) in advertised() {
        let mut upd = drm::AtomicUpdate::default();
        let got = atomic_stage_on(&mut upd, obj, prop_id, value);
        assert_ne!(
            got,
            Err(FsError::EntryNotFound),
            "{:?} advertises property {} and then denies having it",
            obj,
            prop_id,
        );
    }
}

/// The immutable connector properties, whose EINVAL the test above only
/// sees when a display is attached to advertise them.
#[test]
fn the_immutable_connector_properties_are_refused_not_disowned() {
    for prop_id in [PROP_EDID, PROP_LINK_STATUS, PROP_NON_DESKTOP, PROP_DPMS] {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Connector, prop_id, 0),
            Err(FsError::InvalidParam),
            "property {} must be refused as unsettable, not as unknown",
            prop_id,
        );
    }
}

/// Every property the spec marks ATOMIC has to reach a staging arm on the
/// object that advertises it, and no other object may take it. Staging
/// `SRC_W` on a CRTC would silently write the plane's field.
#[test]
fn an_atomic_property_stages_on_its_own_object_and_nowhere_else() {
    let plane = [
        PROP_FB_ID,
        PROP_CRTC_ID,
        PROP_CRTC_X,
        PROP_CRTC_Y,
        PROP_CRTC_W,
        PROP_CRTC_H,
        PROP_SRC_X,
        PROP_SRC_Y,
        PROP_SRC_W,
        PROP_SRC_H,
        PROP_IN_FENCE_FD,
        PROP_FB_DAMAGE_CLIPS,
    ];
    let crtc = [PROP_ACTIVE, PROP_MODE_ID, PROP_OUT_FENCE_PTR];
    let connector = [PROP_CRTC_ID];

    for (obj, own) in [
        (AtomicObject::Plane, &plane[..]),
        (AtomicObject::Crtc, &crtc[..]),
        (AtomicObject::Connector, &connector[..]),
    ] {
        for prop_id in own {
            let s = spec(*prop_id);
            assert_ne!(
                s.flags & DRM_MODE_PROP_ATOMIC,
                0,
                "{} is staged in an atomic commit but not advertised as ATOMIC",
                s.name,
            );
            let mut upd = drm::AtomicUpdate::default();
            // A value every one of them accepts: 0 is "none"/"off"
            // everywhere, and IN_FENCE_FD reads it as fd 0.
            assert_eq!(
                atomic_stage_on(&mut upd, obj, *prop_id, 0),
                Ok(()),
                "{:?} cannot stage its own property {}",
                obj,
                s.name,
            );
        }
        // And the ones that belong to somebody else are unknown here.
        for (other_id, other_name) in ALL_PROPS {
            if own.contains(other_id) {
                continue;
            }
            let mut upd = drm::AtomicUpdate::default();
            let got = atomic_stage_on(&mut upd, obj, *other_id, 0);
            assert_ne!(
                got,
                Ok(()),
                "{:?} accepted {}, which is not its property",
                obj,
                other_name,
            );
        }
    }
}

/// An object id that names nothing is ENOENT, and it is the *object*
/// lookup that says so: with no display attached no id resolves, which is
/// what makes the rest of this module able to run at all.
#[test]
fn an_object_id_that_names_nothing_is_enoent() {
    let _serialised = drm::test_globals::lock();
    let mut upd = drm::AtomicUpdate::default();
    assert_eq!(atomic_object(0xDEAD_BEEF), None);
    assert_eq!(
        atomic_stage(&mut upd, 0xDEAD_BEEF, PROP_FB_ID, 0),
        Err(FsError::EntryNotFound),
    );
}

/// "type" is IMMUTABLE. Accepting it would let a client turn the primary
/// plane into a cursor for the rest of the session.
#[test]
fn the_immutable_plane_type_is_refused() {
    let mut upd = drm::AtomicUpdate::default();
    assert_eq!(
        atomic_stage_on(&mut upd, AtomicObject::Plane, PROP_TYPE, 2),
        Err(FsError::InvalidParam),
    );
    assert_ne!(
        spec(PROP_TYPE).flags & DRM_MODE_PROP_IMMUTABLE,
        0,
        "the refusal above is only right because the property is immutable",
    );
}

/// ACTIVE is advertised as `[0, 1]`, so the guard has to hold that line:
/// `Some(value != 0)` would read 2 as "on" and quietly accept a value the
/// property says is out of range.
#[test]
fn active_takes_only_the_two_values_it_advertises() {
    for (value, want_on) in [(0u64, false), (1, true)] {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Crtc, PROP_ACTIVE, value),
            Ok(()),
        );
        assert_eq!(upd.active, Some(want_on));
    }
    for value in [2u64, 3, u64::MAX] {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Crtc, PROP_ACTIVE, value),
            Err(FsError::InvalidParam),
            "ACTIVE={} is outside the advertised [0, 1]",
            value,
        );
        assert_eq!(upd.active, None, "a refused value must not be staged");
    }
}

/// IN_FENCE_FD is a SIGNED_RANGE whose -1 means "no fence". Anything below
/// that is a bad fd, and anything at or above 0 is a real one to wait on.
/// The sentinel must not be staged: `Some(-1)` would send the commit
/// looking for fd -1 in the caller's table.
#[test]
fn the_in_fence_sentinel_is_accepted_without_being_staged() {
    let mut upd = drm::AtomicUpdate::default();
    assert_eq!(
        atomic_stage_on(
            &mut upd,
            AtomicObject::Plane,
            PROP_IN_FENCE_FD,
            -1i64 as u64
        ),
        Ok(()),
    );
    assert_eq!(upd.in_fence_fd, None, "-1 means no fence, not fd -1");

    let mut upd = drm::AtomicUpdate::default();
    assert_eq!(
        atomic_stage_on(&mut upd, AtomicObject::Plane, PROP_IN_FENCE_FD, 7),
        Ok(()),
    );
    assert_eq!(upd.in_fence_fd, Some(7));

    for bad in [-2i64, -1000, i32::MIN as i64] {
        let mut upd = drm::AtomicUpdate::default();
        assert_eq!(
            atomic_stage_on(&mut upd, AtomicObject::Plane, PROP_IN_FENCE_FD, bad as u64,),
            Err(FsError::InvalidParam),
            "fd {} is below the -1 sentinel",
            bad,
        );
    }
}

/// OUT_FENCE_PTR is a userspace pointer the kernel writes an i32 into. A
/// null one is "no out-fence wanted" and is staged as-is; a non-null one
/// goes through `access_ok()` before anything is written to it, which is
/// what stops a client aiming the write at kernel memory.
#[test]
fn a_null_out_fence_pointer_is_staged_and_a_bad_one_is_refused() {
    let mut upd = drm::AtomicUpdate::default();
    assert_eq!(
        atomic_stage_on(&mut upd, AtomicObject::Crtc, PROP_OUT_FENCE_PTR, 0),
        Ok(()),
    );
    assert_eq!(upd.out_fence_ptr, Some(0));

    // A real address of the right size passes the check.
    let slot = 0i32;
    let mut upd = drm::AtomicUpdate::default();
    assert_eq!(
        atomic_stage_on(
            &mut upd,
            AtomicObject::Crtc,
            PROP_OUT_FENCE_PTR,
            &slot as *const i32 as u64,
        ),
        Ok(()),
    );
    assert_eq!(upd.out_fence_ptr, Some(&slot as *const i32 as u64));
}
