//! `drm_mode_getfb`/`drm_mode_getfb2_ioctl`: the geometry to everyone, the
//! backing handle to the master or root only, and as a handle of the
//! caller's.

use super::gl_client_sequence_tests::Client;
use super::*;
use zcore_drivers::scheme::gem_mmap;

/// The caller's pid for the handle lookup, and the one before it back after.
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

/// `(handle, width, height)` as `GETFB` reports them.
fn getfb(c: &Client, fb_id: u32) -> Result<(u32, u32, u32)> {
    let mut cmd = DrmModeFbCmd {
        fb_id,
        width: 0,
        height: 0,
        pitch: 0,
        bpp: 0,
        depth: 0,
        handle: 0xdead,
    };
    c.ioctl(DRM_IOCTL_MODE_GETFB, &mut cmd)?;
    Ok((cmd.handle, cmd.width, cmd.height))
}
fn getfb2(c: &Client, fb_id: u32) -> Result<[u32; 4]> {
    let mut cmd = DrmModeFbCmd2 {
        fb_id,
        width: 0,
        height: 0,
        pixel_format: 0,
        flags: 0,
        handles: [0xdead; 4],
        pitches: [0; 4],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    c.ioctl(DRM_IOCTL_MODE_GETFB2, &mut cmd)?;
    Ok(cmd.handles)
}
fn handle_of(c: &Client, fb_id: u32) -> u32 {
    let h = getfb(c, fb_id).expect("GETFB").0;
    assert_eq!(
        getfb2(c, fb_id).expect("GETFB2"),
        [h, 0, 0, 0],
        "GETFB2 agrees"
    );
    h
}

/// The geometry goes to every open; the handle only to the node's current
/// master or to root, and 0 to the rest, so it follows the master when it
/// changes hands. For a buffer the caller holds (its own, or the kernel's)
/// that is the handle it knows; for another process's dumb buffer a new
/// handle of the caller's over the same frames (the same one on every call,
/// like a PRIME self-import); for another process's nouveau object the
/// object's handle with the caller added as a holder, once. A framebuffer
/// nobody has is ENOENT. This gave the handle to the framebuffer's creator,
/// master or not, and to nobody else.
#[test]
fn getfb_hands_the_handle_to_the_master_or_root_only_and_as_the_callers_own() {
    let _serialised = drm::test_globals::lock();
    drm::set_test_root(false);
    let master = Client::open(86);
    let other = Client::open(86);
    let buf = master.create_dumb(16, 16);
    let fb = master.addfb2(&buf);

    assert_eq!(getfb(&master, fb), Ok((buf.handle, 16, 16)), "the master");
    assert_eq!(getfb2(&master, fb), Ok([buf.handle, 0, 0, 0]));
    assert_eq!(
        getfb(&other, fb),
        Ok((0, 16, 16)),
        "the geometry, no handle"
    );
    assert_eq!(getfb2(&other, fb), Ok([0; 4]));
    drm::set_test_root(true);
    assert_eq!(handle_of(&other, fb), buf.handle, "root, not the master");
    assert_eq!(getfb(&other, fb + 1000), Err(FsError::EntryNotFound));
    // The master changes hands: so does the handle.
    master.ioctl(DRM_IOCTL_DROP_MASTER, &mut 0u8).unwrap();
    other.ioctl(DRM_IOCTL_SET_MASTER, &mut 0u8).unwrap();
    drm::set_test_root(false);
    assert_eq!(handle_of(&other, fb), buf.handle, "the new master");
    assert_eq!(handle_of(&master, fb), 0, "the open that dropped it");

    // Another process's dumb buffer behind the framebuffer: the master
    // (pid 41) gets a handle of its own over the same frames, once.
    let _pid = Pid::new(41);
    assert!(drm::fb_handle::set_fb_owner_for_test(fb, 7));
    assert!(drm::fb_handle::set_handle_owner_for_test(buf.handle, 7));
    let alias = handle_of(&other, fb);
    assert_ne!(alias, 0);
    assert_ne!(alias, buf.handle, "a handle of the caller's");
    assert_eq!(
        drm::resolve_gem_backing(alias),
        drm::resolve_gem_backing(buf.handle),
        "over the same frames"
    );
    assert_eq!(handle_of(&other, fb), alias, "the same one next time");
    assert_eq!(handle_of(&master, fb), 0, "still nothing for a non-master");
    drm::set_test_root(true);
    assert_eq!(handle_of(&master, fb), alias, "root: pid 41 too");
    drm::set_test_root(false);
    assert!(drm::fb_handle::set_handle_owner_for_test(buf.handle, 0));
    assert_eq!(
        handle_of(&other, fb),
        buf.handle,
        "the kernel's buffer is everyone's"
    );

    // Another process's nouveau object: the same handle, the caller added
    // as a holder once; untracked (freed under the framebuffer), 0.
    let (pa, size) = drm::resolve_gem_backing(buf.handle).unwrap();
    let nv = gem_mmap::DRIVER_HANDLE_BASE + 0x8600;
    gem_mmap::register(nv, pa, size as u64, 7);
    let fb_nv = {
        let mut on_nv = buf;
        on_nv.handle = nv;
        other.addfb2(&on_nv)
    };
    assert!(drm::fb_handle::set_fb_owner_for_test(fb_nv, 7));
    assert!(!gem_mmap::holds(nv, 41));
    // The creator and the framebuffer itself hold it; the caller is next.
    let held = gem_mmap::ref_count(nv).unwrap();
    assert_eq!(handle_of(&other, fb_nv), nv);
    assert!(gem_mmap::holds(nv, 41), "added as a holder");
    assert_eq!(gem_mmap::ref_count(nv), Some(held + 1));
    assert_eq!(handle_of(&other, fb_nv), nv);
    assert_eq!(gem_mmap::ref_count(nv), Some(held + 1), "once");
    assert_eq!(handle_of(&master, fb_nv), 0);
    assert_eq!(
        gem_mmap::ref_count(nv),
        Some(held + 1),
        "a refusal takes nothing"
    );
    assert!(gem_mmap::unregister(nv));
    assert_eq!(handle_of(&other, fb_nv), 0, "no table knows it any more");
    other.rmfb(fb_nv).unwrap();
    other.rmfb(fb).unwrap();
    master.destroy_dumb(buf.handle).unwrap();
    master.destroy_dumb(alias).unwrap();
}
