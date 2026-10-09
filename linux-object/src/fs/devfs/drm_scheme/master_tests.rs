//! `drm_auth.c`: one master per node, and the magic handshake.

use super::gl_client_sequence_tests::Client;
use super::*;

fn set_master(c: &Client) -> Result<usize> {
    c.ioctl(DRM_IOCTL_SET_MASTER, &mut 0u8)
}
fn drop_master(c: &Client) -> Result<usize> {
    c.ioctl(DRM_IOCTL_DROP_MASTER, &mut 0u8)
}
fn magic(c: &Client) -> u32 {
    let mut magic = 0u32;
    c.ioctl(DRM_IOCTL_GET_MAGIC, &mut magic).expect("GET_MAGIC");
    magic
}
fn auth(c: &Client, magic: u32) -> Result<usize> {
    let mut magic = magic;
    c.ioctl(DRM_IOCTL_AUTH_MAGIC, &mut magic)
}

/// `drm_master_open` / `drm_setmaster_ioctl` / `drm_dropmaster_ioctl`:
/// the first open of a node is its master; a second open's SET_MASTER is
/// EBUSY while the first holds it and its DROP_MASTER is EINVAL; the
/// master's own SET_MASTER is a no-op; once dropped, or once the holding
/// file closes, the next SET_MASTER takes it. Nothing was recorded, so
/// every SET_MASTER and every DROP_MASTER succeeded for everyone.
#[test]
fn one_open_holds_the_master_until_it_drops_it_or_closes() {
    let _serialised = drm::test_globals::lock();
    // A node of its own: minor 0's master is whichever test opened it.
    let first = Client::open(77);
    let second = Client::open(77);
    assert_eq!(
        set_master(&second),
        Err(FsError::Busy),
        "held by the first open"
    );
    assert_eq!(set_master(&first), Ok(0), "the master again: a no-op");
    assert_eq!(
        drop_master(&second),
        Err(FsError::InvalidParam),
        "not the master"
    );
    assert_eq!(drop_master(&first), Ok(0));
    assert_eq!(
        drop_master(&first),
        Err(FsError::InvalidParam),
        "already dropped"
    );
    assert_eq!(set_master(&second), Ok(0), "free, so taken");
    assert_eq!(
        set_master(&first),
        Err(FsError::Busy),
        "and now held by the second"
    );
    drop(second);
    assert_eq!(
        set_master(&first),
        Ok(0),
        "released with the file that held it"
    );
}

/// `drm_getmagic` / `drm_authmagic`: a magic is minted per file, once;
/// only the master authenticates (EACCES), only a magic a file of this
/// node holds (EINVAL), and a magic is spent by the AUTH_MAGIC that
/// names it or by its file closing. Every file was told magic 1 and
/// every AUTH_MAGIC from anyone, of anything, was "authenticated".
#[test]
fn a_magic_is_minted_per_file_and_only_the_master_spends_it() {
    let _serialised = drm::test_globals::lock();
    let master = Client::open(78);
    let client = Client::open(78);
    let m_client = magic(&client);
    assert_ne!(m_client, 0);
    assert_eq!(magic(&client), m_client, "the same file, the same magic");
    assert_ne!(magic(&master), m_client, "another file, another magic");

    assert_eq!(
        auth(&client, m_client),
        Err(FsError::NoPermission),
        "not the master"
    );
    assert_eq!(
        auth(&master, m_client + 1000),
        Err(FsError::InvalidParam),
        "never minted"
    );
    assert_eq!(auth(&master, m_client), Ok(0));
    assert_eq!(auth(&master, m_client), Err(FsError::InvalidParam), "spent");

    let elsewhere = Client::open(79);
    assert_eq!(
        auth(&master, magic(&elsewhere)),
        Err(FsError::InvalidParam),
        "minted on another node"
    );
    let closing = Client::open(78);
    let m_closing = magic(&closing);
    drop(closing);
    assert_eq!(
        auth(&master, m_closing),
        Err(FsError::InvalidParam),
        "died with its file"
    );
}

/// libdrm `drmIsMaster()`: AUTH_MAGIC(0). Linux answers EINVAL when the
/// caller is master and EACCES otherwise. Success would also pass the
/// probe, but EINVAL is the ABI, and it is what the einval-hunt was
/// reporting as a fault at compositor start.
#[test]
fn auth_magic_zero_is_einval_from_the_master_and_eacces_from_anyone_else() {
    let _serialised = drm::test_globals::lock();
    let master = Client::open(80);
    let client = Client::open(80);
    assert_eq!(auth(&master, 0), Err(FsError::InvalidParam));
    assert_eq!(auth(&client, 0), Err(FsError::NoPermission));
}

/// Linux keeps magics on `drm_device`, so `card{n}` and `renderD{128+n}`
/// share the map. GET_MAGIC on the render node is EACCES (not
/// DRM_RENDER_ALLOW); a mint on that file must still be spendable from
/// the card, which is how a DRI2 client that opened the render node
/// gets authenticated by the compositor on `card0`.
#[test]
fn a_magic_minted_on_the_render_node_authenticates_on_the_card() {
    let _serialised = drm::test_globals::lock();
    let card = Client::open(81);
    let render = Client::open(drm::RENDER_MINOR_BASE + 81);
    let mut probe = 0u32;
    assert_eq!(
        render.ioctl(DRM_IOCTL_GET_MAGIC, &mut probe),
        Err(FsError::NoPermission),
        "GET_MAGIC is not DRM_RENDER_ALLOW"
    );
    let minted = render.file_state().magic();
    assert_ne!(minted, 0);
    assert_eq!(auth(&card, minted), Ok(0));
    assert_eq!(auth(&card, minted), Err(FsError::InvalidParam), "spent");
}
