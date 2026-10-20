//! The nested user arrays of the ioctls at an odd address. Linux reads
//! and writes them with `get_user`/`put_user`/`copy_to_user`, which take
//! any address a client hands over; every one here was a `*(ptr as
//! *const T).add(i)`, which wants `T`'s alignment -- UB on an odd
//! pointer, and under debug assertions an abort of the whole kernel
//! ("misaligned pointer dereference"). The first-level argument is
//! copied to a kernel buffer by `drm_ioctl_reconciled` and needs none
//! of this.

use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu;
use alloc::vec::Vec;

/// Room for `bytes` at an odd address: the one alignment no `u32`,
/// `u64`, clip rect or enum record has.
struct Odd {
    _raw: Vec<u8>,
    at: u64,
}

impl Odd {
    fn new(bytes: usize) -> Odd {
        let raw = alloc::vec![0u8; bytes + 2];
        let base = raw.as_ptr() as u64;
        Odd {
            _raw: raw,
            at: base + 1 + (base & 1),
        }
    }
    fn put<T: Copy>(&mut self, i: usize, v: T) {
        unsafe { core::ptr::write_unaligned((self.at as *mut T).add(i), v) }
    }
    fn get<T: Copy>(&self, i: usize) -> T {
        unsafe { core::ptr::read_unaligned((self.at as *const T).add(i)) }
    }
}

/// The nouveau uAPI switch, on for one test and put back after (see
/// `syncobj_array_tests`).
struct NouveauOn(bool);
impl NouveauOn {
    fn new() -> Self {
        let was = zcore_drivers::display::nouveau_uapi_enabled();
        zcore_drivers::display::set_nouveau_uapi_enabled(true);
        NouveauOn(was)
    }
}
impl Drop for NouveauOn {
    fn drop(&mut self) {
        zcore_drivers::display::set_nouveau_uapi_enabled(self.0);
    }
}

/// Every KMS list, in and out, through an odd pointer: the id lists of
/// GETRESOURCES and GETPLANERESOURCES, the formats of GETPLANE, the
/// encoder and property lists of GETCONNECTOR and OBJ_GETPROPERTIES,
/// the values and enum records of GETPROPERTY, the connectors of
/// SETCRTC (read, so 4242 is ENOENT), the clips of DIRTYFB, the object
/// ids of CREATE_LEASE and the lessees of LIST_LESSEES.
#[test]
fn every_kms_list_is_read_and_written_at_whatever_address_the_client_gave() {
    let _screen = kms_emu::attach(64, 16);
    // A node of its own: the first open is the master the lease needs.
    let c = Client::open(76);
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap).unwrap();

    // GETRESOURCES: the probe, then the three card lists at odd
    // addresses. The fb list is the process's, and the rest of the
    // suite leaves framebuffers behind, so it is not asked for (room
    // without an array would be EFAULT).
    let mut probe: DrmModeCardRes = unsafe { core::mem::zeroed() };
    c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe).unwrap();
    let (nc, nn, ne) = (
        probe.count_crtcs as usize,
        probe.count_connectors as usize,
        probe.count_encoders as usize,
    );
    assert!(nc >= 1 && nn >= 1 && ne >= 1);
    let ids = Odd::new(4 * (nc + nn + ne));
    let mut r = probe;
    r.fb_id_ptr = 0;
    r.count_fbs = 0;
    r.crtc_id_ptr = ids.at;
    r.connector_id_ptr = ids.at + 4 * nc as u64;
    r.encoder_id_ptr = ids.at + 4 * (nc + nn) as u64;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut r), Ok(0));
    let crtc: u32 = ids.get(0);
    let conn: u32 = ids.get(nc);
    assert!(drm::get_crtc(crtc).is_some(), "crtc {}", crtc);
    assert!(drm::get_connector(conn).is_some(), "connector {}", conn);
    assert_eq!(ids.get::<u32>(nc + nn), drm::SYNTH_ENCODER_ID);

    // GETPLANERESOURCES, then GETPLANE's formats.
    let mut pr = DrmModeGetPlaneRes {
        plane_id_ptr: 0,
        count_planes: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut pr).unwrap();
    assert!(pr.count_planes >= 1);
    let planes = Odd::new(4 * pr.count_planes as usize);
    pr.plane_id_ptr = planes.at;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut pr), Ok(0));
    let plane: u32 = planes.get(0);
    assert!(drm::get_plane(plane).is_some(), "plane {}", plane);
    let mut gp = DrmModeGetPlane {
        plane_id: plane,
        crtc_id: 0,
        fb_id: 0,
        possible_crtcs: 0,
        gamma_size: 0,
        count_format_types: 0,
        format_type_ptr: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut gp).unwrap();
    let nf = gp.count_format_types as usize;
    assert_eq!(nf, drm::SCANOUT_FORMATS.len());
    let formats = Odd::new(4 * nf);
    gp.format_type_ptr = formats.at;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut gp), Ok(0));
    for (i, &f) in drm::SCANOUT_FORMATS.iter().enumerate() {
        assert_eq!(formats.get::<u32>(i), f, "format {}", i);
    }

    // GETCONNECTOR: the encoder, the modes and both property arrays.
    let mut cp: DrmModeGetConnector = unsafe { core::mem::zeroed() };
    cp.connector_id = conn;
    c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut cp).unwrap();
    let (encs, modes, props) = (
        cp.count_encoders as usize,
        cp.count_modes as usize,
        cp.count_props as usize,
    );
    assert!(encs >= 1 && modes >= 1 && props >= 1);
    let enc = Odd::new(4 * encs);
    let mode_bytes = Odd::new(68 * modes);
    let pids = Odd::new(4 * props);
    let pvals = Odd::new(8 * props);
    cp.encoders_ptr = enc.at;
    cp.modes_ptr = mode_bytes.at;
    cp.props_ptr = pids.at;
    cp.prop_values_ptr = pvals.at;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut cp), Ok(0));
    assert_eq!(enc.get::<u32>(0), drm::SYNTH_ENCODER_ID);
    assert!(
        (0..props).all(|i| pids.get::<u32>(i) != 0),
        "the whole list"
    );
    let dpms_at = (0..props).find(|&i| pids.get::<u32>(i) == PROP_DPMS);
    assert!(dpms_at.is_some(), "DPMS is a connector property");

    // OBJ_GETPROPERTIES of the connector: the same two arrays, the
    // other way in.
    let mut op = DrmModeObjGetProperties {
        props_ptr: pids.at,
        prop_values_ptr: pvals.at,
        count_props: props as u32,
        obj_id: conn,
        obj_type: DRM_MODE_OBJECT_CONNECTOR,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut op), Ok(0));
    assert_eq!(op.count_props as usize, props);
    assert_eq!(pids.get::<u32>(dpms_at.unwrap()), PROP_DPMS);

    // GETPROPERTY: the values of a range property (non_desktop) and the
    // enum records and values of DPMS.
    let getprop = |prop_id: u32| -> DrmModeGetProperty {
        let mut gp = DrmModeGetProperty {
            values_ptr: 0,
            enum_blob_ptr: 0,
            prop_id,
            flags: 0,
            name: [0; 32],
            count_values: 0,
            count_enum_blobs: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_GETPROPERTY, &mut gp).unwrap();
        gp
    };
    let mut nd = getprop(PROP_NON_DESKTOP);
    assert!(nd.count_values >= 1, "a range has its bounds");
    let nd_vals = Odd::new(8 * nd.count_values as usize);
    nd.values_ptr = nd_vals.at;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPROPERTY, &mut nd), Ok(0));
    assert_eq!(nd_vals.get::<u64>(nd.count_values as usize - 1), 1);
    let mut dpms = getprop(PROP_DPMS);
    assert!(dpms.count_enum_blobs >= 1 && dpms.count_values >= 1);
    let enums =
        Odd::new(core::mem::size_of::<DrmModePropertyEnum>() * dpms.count_enum_blobs as usize);
    let dpms_vals = Odd::new(8 * dpms.count_values as usize);
    dpms.enum_blob_ptr = enums.at;
    dpms.values_ptr = dpms_vals.at;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPROPERTY, &mut dpms), Ok(0));
    let first: DrmModePropertyEnum = enums.get(0);
    assert_eq!(first.value, dpms_vals.get::<u64>(0));
    assert_eq!(&first.name[..2], b"On", "{:?}", first.name);

    // SETCRTC: the connector list is read (a stranger is ENOENT), then
    // the modeset goes through with the real one.
    let buf = c.create_dumb(64, 16);
    let fb = c.addfb2(&buf);
    let mut conns = Odd::new(4);
    let setcrtc = |conns: &Odd| {
        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: conns.at,
            count_connectors: 1,
            crtc_id: crtc,
            fb_id: fb,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: make_modeinfo(64, 16),
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
    };
    conns.put(0, 4242u32);
    assert_eq!(setcrtc(&conns), Err(FsError::EntryNotFound));
    conns.put(0, conn);
    assert_eq!(setcrtc(&conns), Ok(0));

    // DIRTYFB: two clips.
    let mut clips = Odd::new(2 * core::mem::size_of::<DrmClipRect>());
    clips.put(
        0,
        DrmClipRect {
            x1: 0,
            y1: 0,
            x2: 8,
            y2: 8,
        },
    );
    clips.put(
        1,
        DrmClipRect {
            x1: 8,
            y1: 8,
            x2: 16,
            y2: 16,
        },
    );
    let mut dirty = DrmModeFbDirtyCmd {
        fb_id: fb,
        flags: 0,
        color: 0,
        num_clips: 2,
        clips_ptr: clips.at,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut dirty), Ok(0));

    // CREATE_LEASE reads the object ids (the plane too: this client
    // asked for universal planes) and LIST_LESSEES writes the lessees.
    let mut objs = Odd::new(12);
    objs.put(0, crtc);
    objs.put(1, conn);
    objs.put(2, plane);
    let mut lease = DrmModeCreateLease {
        object_ids: objs.at,
        object_count: 3,
        flags: 0,
        lessee_id: 0,
        fd: -1,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_CREATE_LEASE, &mut lease), Ok(0));
    assert!(lease.lessee_id >= 1);
    let lessees = Odd::new(4 * 4);
    let mut list = DrmModeListLessees {
        count_lessees: 4,
        pad: 0,
        lessees_ptr: lessees.at,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_LIST_LESSEES, &mut list), Ok(0));
    assert_eq!(list.count_lessees, 1);
    assert_eq!(lessees.get::<u32>(0), lease.lessee_id);
}

/// The handle and point arrays of the syncobj ioctls: RESET, SIGNAL,
/// WAIT, TIMELINE_SIGNAL, QUERY (written) and TIMELINE_WAIT.
#[test]
fn the_syncobj_arrays_are_read_and_written_at_whatever_address_the_client_gave() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _on = NouveauOn::new();
    let c = Client::open(0);
    let create = || {
        let mut req = DrmSyncobjCreate {
            handle: 0,
            flags: DRM_SYNCOBJ_CREATE_SIGNALED,
        };
        c.ioctl(DRM_IOCTL_SYNCOBJ_CREATE, &mut req).expect("CREATE");
        req.handle
    };
    let (h0, h1) = (create(), create());
    let mut handles = Odd::new(8);
    handles.put(0, h0);
    handles.put(1, h1);
    let mut points = Odd::new(16);
    points.put(0, 3u64);
    points.put(1, 4u64);

    let array = |cmd: u32| {
        let mut req = DrmSyncobjArray {
            handles: handles.at,
            count_handles: 2,
            pad: 0,
        };
        c.ioctl(cmd, &mut req)
    };
    assert_eq!(array(DRM_IOCTL_SYNCOBJ_RESET), Ok(0));
    assert_eq!(array(DRM_IOCTL_SYNCOBJ_SIGNAL), Ok(0));
    let mut wait = DrmSyncobjWait {
        handles: handles.at,
        timeout_nsec: 0,
        count_handles: 2,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
        first_signaled: 0xdead,
        pad: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_WAIT, &mut wait), Ok(0));

    let timeline = |cmd: u32, points: u64| {
        let mut req = DrmSyncobjTimelineArray {
            handles: handles.at,
            points,
            count_handles: 2,
            flags: 0,
        };
        c.ioctl(cmd, &mut req)
    };
    assert_eq!(
        timeline(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, points.at),
        Ok(0)
    );
    let out = Odd::new(16);
    assert_eq!(timeline(DRM_IOCTL_SYNCOBJ_QUERY, out.at), Ok(0));
    assert_eq!((out.get::<u64>(0), out.get::<u64>(1)), (3, 4));
    let mut twait = DrmSyncobjTimelineWait {
        handles: handles.at,
        points: points.at,
        timeout_nsec: 0,
        count_handles: 2,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
        first_signaled: 0xdead,
        pad: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut twait), Ok(0));

    for h in [h0, h1] {
        let mut req = DrmSyncobjDestroy { handle: h, pad: 0 };
        c.ioctl(DRM_IOCTL_SYNCOBJ_DESTROY, &mut req)
            .expect("DESTROY");
    }
}

/// The four arrays of an atomic commit.
#[test]
fn an_atomic_commit_reads_its_four_arrays_at_whatever_address_the_client_gave() {
    let (_screen, c) = super::out_fence_tests::atomic_client(32, 8);
    let mut objs = Odd::new(4);
    objs.put(0, drm::SYNTH_CRTC_ID);
    let mut counts = Odd::new(4);
    counts.put(0, 1u32);
    let mut props = Odd::new(4);
    props.put(0, PROP_ACTIVE);
    let mut values = Odd::new(8);
    values.put(0, 0u64);
    let mut req = DrmModeAtomic {
        flags: 0,
        count_objs: 1,
        objs_ptr: objs.at,
        count_props_ptr: counts.at,
        props_ptr: props.at,
        prop_values_ptr: values.at,
        reserved: 0,
        user_data: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_ATOMIC, &mut req), Ok(0));
}
