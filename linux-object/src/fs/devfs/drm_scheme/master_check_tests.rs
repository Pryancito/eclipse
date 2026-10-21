//! `drm_ioctl_permit` for the ioctls `drm_ioctls[]` marks `DRM_MASTER`
//! (the modeset twelve, `SET_VERSION` and the hollowed legacy six), and `drm_master_check_perm` for `SET_MASTER` and
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

/// The twelve of modeset and `SET_VERSION`, by name, so a refusal says
/// which. (`SET_VERSION` with zeros asks for interface 0.0: EINVAL, which
/// is not EACCES, from the master.)
const MASTER_IOCTLS: [(u32, &str); 13] = [
    (DRM_IOCTL_SET_VERSION, "SET_VERSION"),
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
/// argument (ENOENT, EINVAL, or the no-op's 0), never EACCES. `SET_VERSION`
/// is `DRM_MASTER` like the modeset twelve, and was left out: a second
/// open had it answered (Xorg's modesetting calls it right after open). An
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
    // Wide enough for the largest of them (SETCRTC, 104 bytes).
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

/// The six hollowed legacy ioctls (`SET_UNIQUE`, `BLOCK`, `UNBLOCK`,
/// `ADD_DRAW`, `RM_DRAW`, `UPDATE_DRAW`), answered as root once `legacy_permit`
/// passes, with what each answers.
fn legacy_six(c: &Client) -> [Result<usize>; 6] {
    [
        c.ioctl(DRM_IOCTL_SET_UNIQUE, &mut [0u8; 16]),
        c.ioctl(DRM_IOCTL_BLOCK, &mut 0u32),
        c.ioctl(DRM_IOCTL_UNBLOCK, &mut 0u32),
        c.ioctl(DRM_IOCTL_ADD_DRAW, &mut 0u32),
        c.ioctl(DRM_IOCTL_RM_DRAW, &mut 0u32),
        c.ioctl(DRM_IOCTL_UPDATE_DRAW, &mut [0u8; 24]),
    ]
}

/// The six hollowed legacy ioctls are `DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY`
/// and `SET_VERSION` is `DRM_MASTER`: under `drm.master_check` they are
/// EACCES to an open that is not the master even when it is authenticated
/// root (the gate at the top of the dispatch, before `legacy_permit`), while
/// the master as root gets `SET_UNIQUE`'s EINVAL, the no-ops' 0 and
/// `SET_VERSION`'s answer to a query. Without the flag an authenticated root
/// open gets them all. `AUTH_MAGIC` is `DRM_MASTER` with its own check, flag
/// or no flag, like the leases. `needs_master` began at the modeset ioctls,
/// so a second open had `SET_VERSION` answered and, as authenticated root,
/// the legacy six.
#[test]
fn the_hollowed_legacy_ioctls_want_the_master_too() {
    let _serialised = drm::test_globals::lock();
    let master = Client::open(84);
    let other = Client::open(84);
    authenticate(&master, &other);
    let eacces = Err(FsError::NoPermission);
    let einval = Err(FsError::InvalidParam);
    let answered = [
        Err(FsError::InvalidParam),
        Ok(0),
        Ok(0),
        Ok(0),
        Ok(0),
        Ok(0),
    ];
    // `-1` throughout: "query only", which `drm_setversion` answers with 0.
    let query = |c: &Client| c.ioctl(DRM_IOCTL_SET_VERSION, &mut [-1i32; 4]);
    let auth_magic0 = |c: &Client| c.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut 0u32);
    let _root = Root::new();

    {
        let _off = MasterCheck::new(false);
        assert_eq!(legacy_six(&other), answered, "root, authenticated: no flag");
        assert_eq!(query(&other), Ok(0), "no flag");
        assert_eq!(
            auth_magic0(&other),
            eacces,
            "AUTH_MAGIC checks the master itself"
        );
        assert_eq!(auth_magic0(&master), einval, "magic 0 is never minted");
    }
    let _on = MasterCheck::new(true);
    for r in legacy_six(&other).iter() {
        assert_eq!(*r, eacces, "not the master, however privileged");
    }
    assert_eq!(query(&other), eacces, "not the master");
    assert_eq!(legacy_six(&master), answered, "the master, as root");
    assert_eq!(query(&master), Ok(0), "the master");
    assert_eq!(auth_magic0(&other), eacces);
    assert_eq!(auth_magic0(&master), einval);
}
