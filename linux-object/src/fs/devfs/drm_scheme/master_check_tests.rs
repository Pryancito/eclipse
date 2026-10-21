//! `drm_ioctl_permit` for the modeset ioctls `drm_ioctls[]` marks
//! `DRM_MASTER`, under `drm.master_check`.

use super::gl_client_sequence_tests::Client;
use super::*;

/// The `drm.master_check` switch, set for one test and put back after;
/// under `drm::test_globals::lock()`, like every process-wide DRM knob.
struct MasterCheck(bool);
impl MasterCheck {
    fn new(on: bool) -> Self {
        let was = master_check_enabled();
        set_master_check_enabled(on);
        MasterCheck(was)
    }
}
impl Drop for MasterCheck {
    fn drop(&mut self) {
        set_master_check_enabled(self.0);
    }
}

fn set_master(c: &Client) -> Result<usize> {
    c.ioctl(DRM_IOCTL_SET_MASTER, &mut 0u8)
}
fn drop_master(c: &Client) -> Result<usize> {
    c.ioctl(DRM_IOCTL_DROP_MASTER, &mut 0u8)
}
fn authenticate(master: &Client, other: &Client) {
    let mut magic = 0u32;
    other
        .ioctl(DRM_IOCTL_GET_MAGIC, &mut magic)
        .expect("GET_MAGIC");
    master
        .ioctl(DRM_IOCTL_AUTH_MAGIC, &mut magic)
        .expect("AUTH_MAGIC");
}

/// The twelve, by name, so a refusal says which.
const MASTER_IOCTLS: [(u32, &str); 12] = [
    (DRM_IOCTL_MODE_SETCRTC, "SETCRTC"),
    (DRM_IOCTL_MODE_SETPLANE, "SETPLANE"),
    (DRM_IOCTL_MODE_CURSOR, "CURSOR"),
    (DRM_IOCTL_MODE_CURSOR2, "CURSOR2"),
    (DRM_IOCTL_MODE_SETGAMMA, "SETGAMMA"),
    (DRM_IOCTL_MODE_ATTACHMODE, "ATTACHMODE"),
    (DRM_IOCTL_MODE_DETACHMODE, "DETACHMODE"),
    (DRM_IOCTL_MODE_SETPROPERTY, "SETPROPERTY"),
    (DRM_IOCTL_MODE_PAGE_FLIP, "PAGE_FLIP"),
    (DRM_IOCTL_MODE_DIRTYFB, "DIRTYFB"),
    (DRM_IOCTL_MODE_OBJ_SETPROPERTY, "OBJ_SETPROPERTY"),
    (DRM_IOCTL_MODE_ATOMIC, "ATOMIC"),
];

/// Every ioctl `drm_ioctls[]` marks `DRM_MASTER` is EACCES to an open that
/// is not the current master, before its argument is read, and only under
/// `drm.master_check`; the master gets its usual answer to an all-zero
/// argument (ENOENT, EINVAL, or the no-op's 0), never EACCES. An
/// authenticated open is still not the master. Once the master drops and
/// the other open takes it, the refusals swap sides. Nothing checked the
/// master, so every open could set a mode or flip over the compositor.
#[test]
fn the_modeset_ioctls_want_the_master_before_they_read_their_argument() {
    let _serialised = drm::test_globals::lock();
    let master = Client::open(78);
    let other = Client::open(78);
    authenticate(&master, &other);
    let eacces = Err(FsError::NoPermission);
    // Wide enough for the largest of the twelve (SETCRTC, 104 bytes).
    let mut zeros = [0u8; 128];

    {
        let _off = MasterCheck::new(false);
        for (cmd, name) in MASTER_IOCTLS {
            assert_ne!(
                other.ioctl(cmd, &mut zeros),
                eacces,
                "{} without the flag",
                name
            );
        }
    }
    let _on = MasterCheck::new(true);
    for (cmd, name) in MASTER_IOCTLS {
        assert_eq!(
            other.ioctl(cmd, &mut zeros),
            eacces,
            "{} from a non-master",
            name
        );
        assert_ne!(
            master.ioctl(cmd, &mut zeros),
            eacces,
            "{} from the master",
            name
        );
    }

    assert_eq!(drop_master(&master), Ok(0));
    assert_eq!(set_master(&other), Ok(0));
    for (cmd, name) in MASTER_IOCTLS {
        assert_ne!(
            other.ioctl(cmd, &mut zeros),
            eacces,
            "{} from the new master",
            name
        );
        assert_eq!(
            master.ioctl(cmd, &mut zeros),
            eacces,
            "{} from the open that dropped it",
            name
        );
    }
}
