//! `drm_lease.c`: who may lease, what a lease may hold, and the lessee
//! list the lessor sees.

use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu;

fn create(c: &Client, ids: &[u32], flags: u32) -> Result<u32> {
    let mut req = DrmModeCreateLease {
        object_ids: if ids.is_empty() {
            0
        } else {
            ids.as_ptr() as u64
        },
        object_count: ids.len() as u32,
        flags,
        lessee_id: 0,
        fd: -1,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATE_LEASE, &mut req)?;
    assert_eq!(req.fd, -1, "the fd is the syscall layer's to install");
    Ok(req.lessee_id)
}
fn list(c: &Client, room: u32, pad: u32) -> Result<(u32, alloc::vec::Vec<u32>)> {
    let mut out = [u32::MAX; 4];
    let mut req = DrmModeListLessees {
        count_lessees: room,
        pad,
        lessees_ptr: out.as_mut_ptr() as u64,
    };
    c.ioctl(DRM_IOCTL_MODE_LIST_LESSEES, &mut req)?;
    let written = out
        .iter()
        .take_while(|&&id| id != u32::MAX)
        .copied()
        .collect();
    Ok((req.count_lessees, written))
}
fn revoke(c: &Client, lessee_id: u32) -> Result<usize> {
    let mut id = lessee_id;
    c.ioctl(DRM_IOCTL_MODE_REVOKE_LEASE, &mut id)
}

/// `drm_mode_create_lease_ioctl` / `drm_mode_list_lessees_ioctl` /
/// `drm_mode_revoke_lease_ioctl` over the synthetic CRTC, connector and
/// primary plane. All three are `DRM_MASTER` (EACCES). A lease is
/// refused for a flag that is not the fd's (EINVAL), an object list
/// that cannot be read (EFAULT), an id that names nothing (ENOENT) or
/// an encoder (EINVAL), a set without a CRTC and a connector, or
/// without a plane once the lessor has universal planes (EINVAL), and
/// an id that lands in the set twice, which is what naming the primary
/// plane beside its CRTC does without universal planes (ENOSPC). Lessee
/// ids are the lowest free from 1; `LIST_LESSEES` refuses `pad`, lists
/// only the lessees still holding objects, as many as fit, and answers
/// the total; `REVOKE_LEASE` empties a lessee (ENOENT for none) and
/// keeps it. Every caller, master or not, got an unrestricted fd as
/// lessee 1 for any request at all; `LIST_LESSEES` was a constant zero
/// and `REVOKE_LEASE` an unknown number.
#[test]
fn a_lease_holds_what_drm_lease_lets_it_and_the_lessor_lists_and_revokes_it() {
    let _screen = kms_emu::attach(64, 16);
    // A node of its own, so the lessee ids start at 1 here.
    let master = Client::open(79);
    let client = Client::open(79);
    let (crtc, conn, plane) = (
        drm::SYNTH_CRTC_ID,
        drm::SYNTH_CONNECTOR_ID,
        drm::SYNTH_PLANE_ID,
    );
    let einval = Err(FsError::InvalidParam);
    let eacces = Err(FsError::NoPermission);
    let enospc = Err(FsError::NoDeviceSpace);

    assert_eq!(create(&client, &[crtc, conn], 0), eacces, "not the master");
    assert_eq!(list(&client, 4, 0), Err(FsError::NoPermission));
    assert_eq!(revoke(&client, 1), Err(FsError::NoPermission));

    assert_eq!(create(&master, &[crtc, conn], 1), einval, "not an fd flag");
    assert_eq!(
        create(&master, &[crtc, conn], O_CLOEXEC | O_NONBLOCK | 0x10000),
        einval
    );
    let mut unreadable = DrmModeCreateLease {
        object_ids: 0,
        object_count: 1,
        flags: 0,
        lessee_id: 0,
        fd: -1,
    };
    assert_eq!(
        master.ioctl(DRM_IOCTL_MODE_CREATE_LEASE, &mut unreadable),
        Err(FsError::BadAddress),
        "a count without a list: memdup_array_user faults"
    );
    assert_eq!(
        create(&master, &[crtc, 4242], 0),
        Err(FsError::EntryNotFound),
        "no such object"
    );
    assert_eq!(
        create(&master, &[crtc, conn, drm::SYNTH_ENCODER_ID], 0),
        einval,
        "an encoder cannot be leased"
    );
    assert_eq!(create(&master, &[crtc], 0), einval, "no connector");
    assert_eq!(create(&master, &[conn, plane], 0), einval, "no CRTC");
    assert_eq!(
        create(&master, &[crtc, conn, crtc], 0),
        enospc,
        "an id twice"
    );
    assert_eq!(
        create(&master, &[crtc, conn, plane], 0),
        enospc,
        "the CRTC brings its primary plane, so naming it too collides"
    );
    assert_eq!(
        list(&master, 4, 0),
        Ok((0, alloc::vec![])),
        "none of those made a lease"
    );

    assert_eq!(
        create(&master, &[], O_CLOEXEC | O_NONBLOCK),
        Ok(1),
        "an empty lease, with the fd's own flags"
    );
    assert_eq!(create(&master, &[crtc, conn], 0), Ok(2));
    assert_eq!(list(&master, 4, 1), Err(FsError::InvalidParam), "pad");
    assert_eq!(
        list(&master, 4, 0),
        Ok((1, alloc::vec![2])),
        "the empty lease holds nothing and is not listed"
    );
    assert_eq!(
        list(&master, 0, 0),
        Ok((1, alloc::vec![])),
        "the total, with no room to list it in"
    );
    let mut nowhere = DrmModeListLessees {
        count_lessees: 4,
        pad: 0,
        lessees_ptr: 0,
    };
    assert_eq!(
        master.ioctl(DRM_IOCTL_MODE_LIST_LESSEES, &mut nowhere),
        Err(FsError::BadAddress),
        "room but no list: put_user faults"
    );
    assert_eq!(nowhere.count_lessees, 4, "and the count is not written");

    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
    master
        .ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("UNIVERSAL_PLANES");
    assert_eq!(
        create(&master, &[crtc, conn], 0),
        einval,
        "with universal planes the lease needs a plane"
    );
    assert_eq!(
        create(&master, &[crtc, conn, plane], 0),
        Ok(3),
        "named, not brought along"
    );
    assert_eq!(list(&master, 4, 0), Ok((2, alloc::vec![2, 3])));
    assert_eq!(list(&master, 1, 0), Ok((2, alloc::vec![2])), "room for one");

    assert_eq!(
        revoke(&master, 42),
        Err(FsError::EntryNotFound),
        "no such lessee"
    );
    assert_eq!(revoke(&master, 2), Ok(0));
    assert_eq!(
        list(&master, 4, 0),
        Ok((1, alloc::vec![3])),
        "revoked: empty, so not listed"
    );
    assert_eq!(revoke(&master, 2), Ok(0), "the lessee is still there");
    assert_eq!(revoke(&master, 1), Ok(0), "so is the empty one");
    assert_eq!(
        create(&master, &[crtc, conn, plane], 0),
        Ok(4),
        "ids are not reused while the lessees live"
    );
}
