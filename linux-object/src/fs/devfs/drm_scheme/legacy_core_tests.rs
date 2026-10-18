//! The hollowed-out core ioctls of `drm_ioctl.c` and the
//! `drm_file.authenticated` they answer for or require.

use super::gl_client_sequence_tests::Client;
use super::*;

fn get_client(c: &Client, idx: i32) -> Result<DrmClient> {
    let mut req = DrmClient {
        idx,
        auth: -1,
        pid: u64::MAX,
        uid: u64::MAX,
        magic: u64::MAX,
        iocs: u64::MAX,
    };
    c.ioctl(DRM_IOCTL_GET_CLIENT, &mut req)?;
    Ok(req)
}
fn auth(c: &Client) -> i32 {
    get_client(c, 0).expect("idx 0 answers").auth
}
fn noop<const N: usize>(c: &Client, cmd: u32) -> Result<usize> {
    let mut arg = [0u8; N];
    c.ioctl(cmd, &mut arg)
}
/// The five `drm_noop`s and the `drm_invalid_op` behind
/// `DRM_AUTH | DRM_MASTER | DRM_ROOT_ONLY`, with what each answers
/// once the permit passes.
fn root_only(c: &Client) -> [Result<usize>; 6] {
    [
        noop::<16>(c, DRM_IOCTL_SET_UNIQUE),
        noop::<4>(c, DRM_IOCTL_BLOCK),
        noop::<4>(c, DRM_IOCTL_UNBLOCK),
        noop::<4>(c, DRM_IOCTL_ADD_DRAW),
        noop::<4>(c, DRM_IOCTL_RM_DRAW),
        noop::<24>(c, DRM_IOCTL_UPDATE_DRAW),
    ]
}

/// `drm_getclient` answers `idx == 0` only, with the open's
/// `authenticated`, the caller's pid, `overflowuid` and zeros; the
/// open that takes the master is authenticated (`drm_new_set_master`,
/// at open or by `SET_MASTER`), so is the owner of a magic the master
/// `AUTH_MAGIC`s (`drm_authmagic`) and so is any open made by root
/// (`drm_file_alloc`). `FINISH` is `DRM_AUTH` (EACCES until then);
/// `SET_UNIQUE`, `BLOCK`, `UNBLOCK`, `ADD_DRAW`, `RM_DRAW` and
/// `UPDATE_DRAW` are `DRM_ROOT_ONLY` too (EACCES for anyone else,
/// EINVAL and 0 for root); `GET_STATS` zeroes its struct; `ATTACHMODE`
/// and `DETACHMODE` are no-ops. All of them were unknown numbers
/// (ENOSYS), and nothing recorded who was authenticated.
#[test]
fn get_client_reports_who_is_authenticated_and_the_hollowed_ioctls_answer_as_linux() {
    let _serialised = drm::test_globals::lock();
    drm::set_test_root(false);
    let eacces = Err(FsError::NoPermission);
    // A node of its own: the first open takes its master.
    let master = Client::open(80);
    let client = Client::open(80);

    assert_eq!(
        get_client(&master, 1).map(|c| c.idx),
        Err(FsError::InvalidParam),
        "only idx 0 exists"
    );
    let me = get_client(&master, 0).unwrap();
    assert_eq!(
        (me.auth, me.pid, me.uid, me.magic, me.iocs),
        (1, 0, 65534, 0, 0),
        "the master is authenticated; pid 0 is the test's, uid overflowuid"
    );
    assert_eq!(auth(&client), 0, "a second open is not");
    assert_eq!(noop::<8>(&client, DRM_IOCTL_FINISH), eacces, "DRM_AUTH");
    assert_eq!(noop::<8>(&master, DRM_IOCTL_FINISH), Ok(0));

    // `drm_authmagic` authenticates the magic's owner.
    let mut magic = 0u32;
    client.ioctl(DRM_IOCTL_GET_MAGIC, &mut magic).unwrap();
    master.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut magic).unwrap();
    assert_eq!(auth(&client), 1, "authenticated by the master");
    assert_eq!(noop::<8>(&client, DRM_IOCTL_FINISH), Ok(0));
    assert_eq!(auth(&master), 1, "and the master still is");

    // Taking the master by `SET_MASTER` authenticates too.
    let third = Client::open(80);
    assert_eq!(auth(&third), 0);
    master.ioctl(DRM_IOCTL_DROP_MASTER, &mut 0u8).unwrap();
    third.ioctl(DRM_IOCTL_SET_MASTER, &mut 0u8).unwrap();
    assert_eq!(auth(&third), 1, "drm_new_set_master");
    assert_eq!(auth(&master), 1, "dropping the master keeps it");

    // `GET_STATS` is zeros, whatever was there.
    let mut stats = [0xFFu8; DRM_STATS_SIZE];
    assert_eq!(client.ioctl(DRM_IOCTL_GET_STATS, &mut stats), Ok(0));
    assert!(stats.iter().all(|&b| b == 0));

    // `DRM_ROOT_ONLY`: EACCES for a non-root, authenticated or not.
    for r in root_only(&client).iter().chain(root_only(&third).iter()) {
        assert_eq!(*r, eacces, "not CAP_SYS_ADMIN");
    }
    assert_eq!(noop::<72>(&client, DRM_IOCTL_MODE_ATTACHMODE), Ok(0));
    assert_eq!(noop::<72>(&client, DRM_IOCTL_MODE_DETACHMODE), Ok(0));

    // Root: authenticated from the open, and the root-only ones answer.
    drm::set_test_root(true);
    let root = Client::open(80);
    assert_eq!(auth(&root), 1, "root is always authenticated");
    assert_eq!(noop::<8>(&root, DRM_IOCTL_FINISH), Ok(0));
    assert_eq!(
        root_only(&root),
        [
            Err(FsError::InvalidParam),
            Ok(0),
            Ok(0),
            Ok(0),
            Ok(0),
            Ok(0)
        ],
        "SET_UNIQUE is drm_invalid_op, the rest drm_noop"
    );
    drm::set_test_root(false);
    // Being root is the caller's, per call: the same open loses them.
    for r in root_only(&root).iter() {
        assert_eq!(*r, eacces);
    }
    assert_eq!(auth(&root), 1, "but the authentication stays");
    // The authentication goes with the file (`drm_master_release`), and
    // a magic of a closed open cannot authenticate anyone.
    let mut stale = 0u32;
    let gone_owner = {
        drm::set_test_root(true);
        let gone = Client::open(80);
        drm::set_test_root(false);
        let owner = Arc::as_ptr(gone.file_state()) as usize;
        assert!(drm::is_authenticated(80, owner));
        gone.ioctl(DRM_IOCTL_GET_MAGIC, &mut stale).unwrap();
        owner
    };
    assert!(!drm::is_authenticated(80, gone_owner), "gone with the file");
    assert_eq!(
        third.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut stale),
        Err(FsError::InvalidParam)
    );
}
