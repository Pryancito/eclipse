use super::gl_client_sequence_tests::Client;
use super::*;
use crate::error::LxError;

#[test]
fn destroypropblob_of_a_kernel_blob_is_eperm_to_userspace() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let kernel = drm::create_blob(alloc::vec![0u8; 68], false);
    let mut id = kernel;
    assert_eq!(
        client.ioctl(DRM_IOCTL_MODE_DESTROYPROPBLOB, &mut id),
        Err(FsError::NotPermitted)
    );
    assert_eq!(LxError::from(FsError::NotPermitted), LxError::EPERM);
    assert!(drm::get_blob(kernel).is_some());
}

#[test]
fn get_client_zero_is_self_and_one_is_enoent() {
    let c = Client::open(0);
    let mut client = DrmClient {
        idx: 0,
        auth: 0,
        pid: 0,
        uid: 0,
        magic: 0,
        iocs: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_GET_CLIENT, &mut client), Ok(0));
    assert_eq!(client.auth, 1);
    assert_eq!(client.magic, 0);
    client.idx = 1;
    assert_eq!(
        c.ioctl(DRM_IOCTL_GET_CLIENT, &mut client),
        Err(FsError::EntryNotFound)
    );
}

#[test]
fn dirtyfb_rejects_unknown_flags() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let buf = c.create_dumb(16, 16);
    let fb = c.addfb2(&buf);
    let mut cmd = DrmModeFbDirtyCmd {
        fb_id: fb,
        flags: 1 << 3,
        color: 0,
        num_clips: 0,
        clips_ptr: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut cmd),
        Err(FsError::InvalidParam)
    );
    assert_eq!(c.rmfb(fb), Ok(0));
    assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
}

#[test]
fn create_dumb_oom_is_enomem_not_enospc() {
    assert_eq!(LxError::from(FsError::NoMemory), LxError::ENOMEM);
    assert_ne!(LxError::from(FsError::NoMemory), LxError::ENOSPC);
}

#[test]
fn create_dumb_refuses_bpp_zero_and_honours_bpp_sixteen() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let mut zero = DrmModeCreateDumb {
        height: 16,
        width: 16,
        bpp: 0,
        flags: 0,
        handle: 0,
        pitch: 0,
        size: 0,
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut zero),
        Err(FsError::InvalidParam)
    );
    let mut bpp16 = DrmModeCreateDumb {
        height: 16,
        width: 16,
        bpp: 16,
        flags: 0,
        handle: 0,
        pitch: 0,
        size: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut bpp16)
        .expect("CREATE_DUMB bpp=16");
    // pitch = round_up(width * bpp/8, 64) = round_up(32, 64) = 64
    assert_eq!(bpp16.pitch, 64, "bpp=16 must not be inflated to 32");
    assert_eq!(bpp16.size, 64 * 16);
    assert_eq!(c.destroy_dumb(bpp16.handle), Ok(0));
}

#[test]
fn a_drm_chardev_write_is_einval() {
    let dev = DrmDev::new(0);
    assert_eq!(dev.write_at(0, b"x"), Err(FsError::InvalidParam));
    assert_eq!(LxError::from(FsError::InvalidParam), LxError::EINVAL);
}

#[test]
fn addfb2_of_an_unknown_handle_is_enoent() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let mut cmd = DrmModeFbCmd2 {
        fb_id: 0,
        width: 16,
        height: 16,
        pixel_format: drm::DRM_FORMAT_XRGB8888,
        flags: 0,
        handles: [0xDEAD_u32, 0, 0, 0],
        pitches: [64, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd),
        Err(FsError::EntryNotFound)
    );
    assert_eq!(LxError::from(FsError::EntryNotFound), LxError::ENOENT);
    assert_eq!(cmd.fb_id, 0);
}
