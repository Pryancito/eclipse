//! `drm_ioctl_permit` for the modeset ioctls `drm_ioctls[]` marks
//! `DRM_MASTER`, and `drm_master_check_perm` for `SET_MASTER` and
//! `DROP_MASTER`, under `drm.master_check`.

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

/// Root for the scope, and back to an unprivileged caller after.
struct Root;
impl Root {
    fn new() -> Self {
        drm::set_test_root(true);
        Root
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        drm::set_test_root(false);
    }
}

/// The caller's pid for the scope (the tests have no thread of their own),
/// and the one before it back after.
struct Pid(u64);
impl Pid {
    fn new(pid: u64) -> Self {
        let was = master_check::caller_pid();
        master_check::set_test_pid(pid);
        Pid(was)
    }
}
impl Drop for Pid {
    fn drop(&mut self) {
        master_check::set_test_pid(self.0);
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

    // The swap itself is root's: `other` never held the master, so under
    // `drm_master_check_perm` only `CAP_SYS_ADMIN` may hand it over.
    {
        let _root = Root::new();
        assert_eq!(drop_master(&master), Ok(0));
        assert_eq!(set_master(&other), Ok(0));
    }
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

/// `SET_MASTER` and `DROP_MASTER` are EACCES, before their own EBUSY and
/// EINVAL, to an open that never held the master, and to one that did
/// when the caller is not the process that opened it; the open that was
/// master may drop and retake it from its own process (the no-op of the
/// master asking again included), root may do either from anywhere, and an
/// open root made master keeps that right. Only
/// under `drm.master_check`: without it an unprivileged open could take
/// the display from a compositor that dropped it for a VT switch, and a
/// compositor's fd handed to another process could drop it.
#[test]
fn set_and_drop_master_are_for_the_open_that_was_master_from_its_process_or_root() {
    let _serialised = drm::test_globals::lock();
    let _pid = Pid::new(41);
    let master = Client::open(78);
    let other = Client::open(78);
    let eacces = Err(FsError::NoPermission);

    {
        let _off = MasterCheck::new(false);
        assert_eq!(set_master(&other), Err(FsError::Busy), "without the flag");
        assert_eq!(
            drop_master(&other),
            Err(FsError::InvalidParam),
            "without the flag"
        );
    }
    let _on = MasterCheck::new(true);
    assert_eq!(set_master(&other), eacces, "never master: before EBUSY");
    assert_eq!(drop_master(&other), eacces, "never master: before EINVAL");
    assert_eq!(drop_master(&master), Ok(0), "the master from its process");
    assert_eq!(set_master(&other), eacces, "still never master");
    assert_eq!(set_master(&master), Ok(0), "was master, from its process");

    {
        let _elsewhere = Pid::new(42);
        assert_eq!(drop_master(&master), eacces, "the fd in another process");
        assert_eq!(
            set_master(&master),
            eacces,
            "the fd in another process, even as the master"
        );
        let _root = Root::new();
        assert_eq!(drop_master(&master), Ok(0), "root from anywhere");
        assert_eq!(set_master(&other), Ok(0), "root hands it over");
    }
    assert_eq!(
        drop_master(&other),
        Ok(0),
        "root made it master: it keeps the right"
    );
    assert_eq!(set_master(&other), Ok(0));
    assert_eq!(drop_master(&other), Ok(0));
    assert_eq!(
        set_master(&master),
        Ok(0),
        "and the first open never lost it"
    );
}
