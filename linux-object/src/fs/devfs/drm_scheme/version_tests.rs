//! `drm_version` and `drm_getunique` copy their strings with `strlen`
//! bytes and no terminator (`drm_copy_field`: cut to the room, length
//! written back in full; `drm_getunique`: only when the whole of it
//! fits), and `drm_setversion` reports the driver's own version, the
//! pair VERSION reports, and refuses a request past it. Here the three
//! strings carried the NUL in both the copy and the length, GET_UNIQUE
//! copied a prefix, and SET_VERSION said driver 1.0 on a nouveau node
//! (VERSION: 1.4) while reading only the requested major.
use super::gl_client_sequence_tests::Client;
use super::*;

fn zeroed<T>() -> T {
    // SAFETY: integers and raw pointers, for which zero is a value.
    unsafe { core::mem::zeroed() }
}

struct NouveauOn(bool);
impl NouveauOn {
    fn new(on: bool) -> Self {
        let was = zcore_drivers::display::nouveau_uapi_enabled();
        zcore_drivers::display::set_nouveau_uapi_enabled(on);
        NouveauOn(was)
    }
}
impl Drop for NouveauOn {
    fn drop(&mut self) {
        zcore_drivers::display::set_nouveau_uapi_enabled(self.0);
    }
}

#[test]
fn version_and_unique_count_and_copy_their_strings_like_linux() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let _nouveau = NouveauOn::new(false);
    let c = Client::open(0);
    const SENTINEL: u8 = 0xEE;

    // The count call: lengths without the terminator.
    let mut v: DrmVersion = zeroed();
    assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut v), Ok(0));
    assert_eq!(
        (v.version_major, v.version_minor, v.version_patchlevel),
        (1, 0, 0)
    );
    assert_eq!(v.name_len, "zcore".len());
    assert_eq!(v.date_len, "20260503".len());
    assert_eq!(v.desc_len, "zCore DRM Driver".len());

    // Exact room: the bytes, no NUL after them.
    let mut name = [SENTINEL; 6];
    let mut desc = [SENTINEL; 17];
    let mut fill: DrmVersion = zeroed();
    fill.name = name.as_mut_ptr();
    fill.name_len = 5;
    fill.desc = desc.as_mut_ptr();
    fill.desc_len = 16;
    assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut fill), Ok(0));
    assert_eq!(&name[..5], b"zcore");
    assert_eq!(name[5], SENTINEL, "a NUL was written past the room");
    assert_eq!(&desc[..16], b"zCore DRM Driver");
    assert_eq!(desc[16], SENTINEL);
    assert_eq!((fill.name_len, fill.desc_len), (5, 16));

    // Less room than the string: a prefix, and the full length back.
    let mut short = [SENTINEL; 4];
    let mut fill: DrmVersion = zeroed();
    fill.name = short.as_mut_ptr();
    fill.name_len = 3;
    assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut fill), Ok(0));
    assert_eq!(&short[..3], b"zco");
    assert_eq!(short[3], SENTINEL);
    assert_eq!(fill.name_len, 5);

    // GET_UNIQUE: nothing for a buffer the bus id does not fit in, the
    // whole of it and no NUL when it does, `strlen` back both times.
    let mut u: DrmUnique = zeroed();
    assert_eq!(c.ioctl(DRM_IOCTL_GET_UNIQUE, &mut u), Ok(0));
    assert_eq!(u.unique_len, "zcore-gpu".len());
    let mut buf = [SENTINEL; 10];
    let mut u: DrmUnique = zeroed();
    u.unique = buf.as_mut_ptr();
    u.unique_len = 8;
    assert_eq!(c.ioctl(DRM_IOCTL_GET_UNIQUE, &mut u), Ok(0));
    assert_eq!(buf, [SENTINEL; 10], "a prefix of the bus id was copied");
    assert_eq!(u.unique_len, 9);
    u.unique_len = 9;
    assert_eq!(c.ioctl(DRM_IOCTL_GET_UNIQUE, &mut u), Ok(0));
    assert_eq!(&buf[..9], b"zcore-gpu");
    assert_eq!(buf[9], SENTINEL);
    assert_eq!(u.unique_len, 9);
}

#[test]
fn set_version_reports_the_driver_version_of_the_node_and_checks_the_minor() {
    let _serialised = drm::test_globals::lock();
    let _hook = crate::fs::syncobj_eventfd::hardware_fence_tests::TEST_SERIAL.lock();
    let einval = || Err(FsError::InvalidParam);
    let query = |c: &Client, di: (i32, i32), dd: (i32, i32)| {
        let mut sv = DrmSetVersion {
            drm_di_major: di.0,
            drm_di_minor: di.1,
            drm_dd_major: dd.0,
            drm_dd_minor: dd.1,
        };
        let r = c.ioctl(DRM_IOCTL_SET_VERSION, &mut sv);
        (r, (sv.drm_dd_major, sv.drm_dd_minor))
    };
    for (nouveau, dd) in [(false, (1, 0)), (true, (1, 4))] {
        let _nouveau = NouveauOn::new(nouveau);
        let c = Client::open(0);
        // VERSION and SET_VERSION name the same driver version.
        let mut v: DrmVersion = zeroed();
        assert_eq!(c.ioctl(DRM_IOCTL_VERSION, &mut v), Ok(0));
        assert_eq!((v.version_major, v.version_minor), dd);
        assert_eq!(query(&c, (1, 4), (-1, 0)), (Ok(0), dd));
        // The interface: 1.0 to 1.4, nothing else.
        assert_eq!(query(&c, (1, 0), (-1, 0)).0, Ok(0));
        assert_eq!(query(&c, (1, 5), (-1, 0)).0, einval());
        assert_eq!(query(&c, (2, 0), (-1, 0)).0, einval());
        // The driver: its major, and a minor up to its own. The answer
        // is written back on a refusal too.
        assert_eq!(query(&c, (-1, 0), dd), (Ok(0), dd));
        assert_eq!(query(&c, (-1, 0), (dd.0, 0)), (Ok(0), dd));
        assert_eq!(query(&c, (-1, 0), (dd.0, dd.1 + 1)), (einval(), dd));
        assert_eq!(query(&c, (-1, 0), (dd.0, -1)), (einval(), dd));
        assert_eq!(query(&c, (-1, 0), (dd.0 + 1, 0)), (einval(), dd));
    }
}
