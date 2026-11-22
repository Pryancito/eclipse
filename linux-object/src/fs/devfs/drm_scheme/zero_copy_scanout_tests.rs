//! Block-linear framebuffers on a driver that scans them out as they are
//! (nvkms, `nvidia.nvkms` + `nvidia.gem_vram`): the modifiers are
//! advertised the way nvidia-drm advertises them, `DRM_CAP_ADDFB2_MODIFIERS`
//! and the primary plane's `IN_FORMATS`, a tiled `ADDFB2` reaches the
//! driver with its block height, and one the driver will not take is
//! refused there, so the compositor goes back to linear before any frame.
use super::gl_client_sequence_tests::Client;
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use alloc::vec::Vec;
use core::convert::TryInto;

fn modifiers_cap(c: &Client) -> u64 {
    let mut req = DrmGetCap {
        capability: 0x10,
        value: 0xdead_beef,
    };
    c.ioctl(DRM_IOCTL_GET_CAP, &mut req).expect("GET_CAP");
    req.value
}

/// The primary plane's properties, as `drmModeObjectGetProperties` reads
/// them.
fn plane_props_of(c: &Client) -> Vec<(u32, u64)> {
    let mut ids = [0u32; 32];
    let mut values = [0u64; 32];
    let mut req = DrmModeObjGetProperties {
        props_ptr: ids.as_mut_ptr() as u64,
        prop_values_ptr: values.as_mut_ptr() as u64,
        count_props: 32,
        obj_id: drm::SYNTH_PLANE_ID,
        obj_type: DRM_MODE_OBJECT_PLANE,
    };
    c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut req)
        .expect("OBJ_GETPROPERTIES");
    (0..req.count_props as usize)
        .map(|i| (ids[i], values[i]))
        .collect()
}

fn blob(c: &Client, id: u32) -> Result<Vec<u8>> {
    let mut req = DrmModeGetBlob {
        blob_id: id,
        length: 0,
        data: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut req)?;
    let mut bytes = alloc::vec![0u8; req.length as usize];
    req.data = bytes.as_mut_ptr() as u64;
    c.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut req)?;
    Ok(bytes)
}

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

fn quad(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

fn tiled_cmd(buf: &DrmModeCreateDumb, h: u64) -> DrmModeFbCmd2 {
    DrmModeFbCmd2 {
        fb_id: 0,
        width: buf.width,
        height: buf.height,
        pixel_format: drm::DRM_FORMAT_XRGB8888,
        flags: DRM_MODE_FB_MODIFIERS,
        handles: [buf.handle, 0, 0, 0],
        pitches: [buf.pitch, 0, 0, 0],
        offsets: [0; 4],
        modifier: [nvidia_block_linear_2d(0, 1, 2, 0x06, h), 0, 0, 0],
    }
}

/// nvidia-drm's list for Turing: block-linear from 32 GOBs high down to
/// one, then linear, for every format; and each one is a modifier
/// `ADDFB2` takes, or wlroots would pick a swapchain it cannot use.
#[test]
fn nvkms_scanout_advertises_the_turing_modifiers_on_in_formats() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-nvkms").console_at(0x17, 0x00));
    let c = Client::open(0);
    assert_eq!(modifiers_cap(&c), 0, "a linear-only driver advertises none");
    assert!(
        plane_props_of(&c)
            .iter()
            .all(|(p, _)| *p != PROP_IN_FORMATS),
        "no IN_FORMATS without modifiers"
    );

    gpu.set_block_linear(true, true);
    assert_eq!(modifiers_cap(&c), 1);
    let props = plane_props_of(&c);
    let (_, id) = props
        .iter()
        .find(|(p, _)| *p == PROP_IN_FORMATS)
        .copied()
        .expect("the primary plane lists IN_FORMATS");
    let b = blob(&c, id as u32).expect("its blob is served");
    assert_eq!(b, scanout_modifier::in_formats());

    // struct drm_format_modifier_blob, version 1.
    assert_eq!(word(&b, 0), 1, "version");
    let (n_formats, formats_at) = (word(&b, 8) as usize, word(&b, 12) as usize);
    let (n_mods, mods_at) = (word(&b, 16) as usize, word(&b, 20) as usize);
    assert_eq!(n_formats, drm::SCANOUT_FORMATS.len());
    for (i, f) in drm::SCANOUT_FORMATS.iter().enumerate() {
        assert_eq!(word(&b, formats_at + 4 * i), *f);
    }
    assert_eq!(mods_at % 8, 0, "drm_format_modifier holds a u64");
    assert_eq!(n_mods, 7);
    assert_eq!(b.len(), mods_at + 24 * n_mods);
    let mods: Vec<u64> = (0..n_mods)
        .map(|i| quad(&b, mods_at + 24 * i + 16))
        .collect();
    for (i, h) in (0..=5u64).rev().enumerate() {
        assert_eq!(mods[i], nvidia_block_linear_2d(0, 1, 2, 0x06, h));
    }
    assert_eq!(mods[6], DRM_FORMAT_MOD_LINEAR);
    for (i, m) in mods.iter().enumerate() {
        assert_eq!(quad(&b, mods_at + 24 * i), 0b11, "every format");
        assert!(decode_scanout_modifier(*m).is_some(), "{:#x}", m);
    }

    gpu.set_block_linear(false, true);
    assert_eq!(modifiers_cap(&c), 0, "off again with the driver");
    assert_eq!(blob(&c, id as u32), Err(FsError::EntryNotFound));
}

/// A tiled `ADDFB2` reaches the driver with its block height and its pitch
/// in bytes, and the flip goes to the driver's fb with no copy.
#[test]
fn a_block_linear_fb_goes_to_the_driver_and_is_flipped_there() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-nvkms").console_at(0x17, 0x00));
    gpu.set_block_linear(true, true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    // Blocks two GOBs high: 16 rows, exactly this buffer.
    let mut cmd = tiled_cmd(&buf, 1);
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd), Ok(0));
    assert_ne!(cmd.fb_id, 0);
    let created = gpu.created_fbs();
    let last = created.last().expect("the driver was asked for the fb");
    assert_eq!(last.block_linear, Some(1));
    assert_eq!(last.pitch, buf.pitch, "the pitch stays bytes");
    c.page_flip(drm::SYNTH_CRTC_ID, cmd.fb_id, 7).expect("flip");
    assert_eq!(gpu.flips().last(), Some(&last.driver_fb_id));
    drm::flush_pending_flip_completions();
}

/// One the driver will not take is `ADDFB2`'s EINVAL, and a linear one
/// still goes through: that refusal is what sends wlroots back to linear.
#[test]
fn a_block_linear_fb_the_driver_refuses_is_refused_at_addfb2() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-nvkms").console_at(0x17, 0x00));
    gpu.set_block_linear(true, false);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    let before = drm::table_sizes_for_test().0;
    let mut cmd = tiled_cmd(&buf, 1);
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd),
        Err(FsError::InvalidParam)
    );
    assert_eq!(cmd.fb_id, 0);
    assert_eq!(drm::table_sizes_for_test().0, before, "no fb was made");
    assert_ne!(c.addfb2(&buf), 0, "linear still works");
}
