use super::gl_client_sequence_tests::{blank_card_res, Client};
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use alloc::vec::Vec;

fn blank_connector(id: u32) -> DrmModeGetConnector {
    DrmModeGetConnector {
        encoders_ptr: 0,
        modes_ptr: 0,
        props_ptr: 0,
        prop_values_ptr: 0,
        count_modes: 0,
        count_props: 0,
        count_encoders: 0,
        encoder_id: 0,
        connector_id: id,
        connector_type: 0,
        connector_type_id: 0,
        connection: 0,
        mm_width: 0,
        mm_height: 0,
        subpixel: 0,
        pad: 0,
    }
}

/// `drmModeGetResources`, pass for pass. `None` is libdrm's NULL.
fn drm_mode_get_resources(c: &Client) -> Option<(Vec<u32>, Vec<u32>, u32)> {
    for _ in 0..4 {
        let mut res = blank_card_res();
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res).ok()?;
        let counts = res;
        let mut crtcs = alloc::vec![0u32; res.count_crtcs as usize];
        let mut conns = alloc::vec![0u32; res.count_connectors as usize];
        let mut encs = alloc::vec![0u32; res.count_encoders as usize];
        if res.count_crtcs != 0 {
            res.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
        }
        if res.count_connectors != 0 {
            res.connector_id_ptr = conns.as_mut_ptr() as u64;
        }
        if res.count_encoders != 0 {
            res.encoder_id_ptr = encs.as_mut_ptr() as u64;
        }
        c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut res).ok()?;
        if counts.count_crtcs < res.count_crtcs
            || counts.count_connectors < res.count_connectors
            || counts.count_encoders < res.count_encoders
        {
            continue; // libdrm's `goto retry`
        }
        // libdrm copies out only as many ids as the FILL pass reported
        // (`drmAllocCpy(ptr, res.count_x, ...)`), so a count that SHRANK
        // between the passes leaves the tail of the probe-sized buffer
        // untouched -- and a helper that returned it whole would hand the
        // caller trailing zeros and probe connector 0.
        crtcs.truncate(res.count_crtcs as usize);
        conns.truncate(res.count_connectors as usize);
        return Some((crtcs, conns, res.count_encoders));
    }
    None
}

/// `drmModeGetConnector`, pass for pass. `None` is libdrm's NULL, which is
/// what Mesa turns into `VK_ERROR_OUT_OF_HOST_MEMORY`.
fn drm_mode_get_connector(c: &Client, id: u32) -> Option<(u32, u32, Vec<u32>)> {
    for _ in 0..4 {
        let mut conn = blank_connector(id);
        c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn).ok()?;
        let counts = conn;
        let mut props = alloc::vec![0u32; conn.count_props as usize];
        let mut prop_values = alloc::vec![0u64; conn.count_props as usize];
        let mut modes = alloc::vec![0u8; conn.count_modes as usize * 68];
        let mut encoders = alloc::vec![0u32; conn.count_encoders as usize];
        if conn.count_props != 0 {
            conn.props_ptr = props.as_mut_ptr() as u64;
            conn.prop_values_ptr = prop_values.as_mut_ptr() as u64;
        }
        if conn.count_modes != 0 {
            conn.modes_ptr = modes.as_mut_ptr() as u64;
        }
        if conn.count_encoders != 0 {
            conn.encoders_ptr = encoders.as_mut_ptr() as u64;
        }
        c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn).ok()?;
        if counts.count_props < conn.count_props
            || counts.count_modes < conn.count_modes
            || counts.count_encoders < conn.count_encoders
        {
            continue;
        }
        props.truncate(conn.count_props as usize);
        return Some((conn.connection, conn.count_modes, props));
    }
    None
}

/// Mesa's `wsi_get_connectors`: every id `drmModeGetResources` advertised
/// has to answer `drmModeGetConnector`, or the whole `VK_KHR_display`
/// query dies with `ERROR_OUT_OF_HOST_MEMORY`.
fn wsi_get_connectors(c: &Client) -> core::result::Result<usize, &'static str> {
    let (_, conns, _) = drm_mode_get_resources(c).ok_or("drmModeGetResources -> NULL")?;
    for id in &conns {
        if drm_mode_get_connector(c, *id).is_none() {
            return Err("drmModeGetConnector -> NULL");
        }
    }
    Ok(conns.len())
}

#[test]
fn vulkaninfo_probes_the_software_kms_topology_without_an_error() {
    let _screen = kms_emu::attach(640, 480);
    let c = Client::open(0);
    assert_eq!(wsi_get_connectors(&c), Ok(1));
}

#[test]
fn vulkaninfo_probes_a_hardware_kms_gpu_without_an_error() {
    let screen = kms_emu::attach(640, 480);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
    let c = Client::open(0);
    assert!(
        wsi_get_connectors(&c).is_ok(),
        "{:?}",
        wsi_get_connectors(&c)
    );
}

/// The invariant: an id GETRESOURCES advertised stays answerable even if
/// scanout ownership changes before the client's next ioctl.
///
/// The lookups used to ask `software_kms_active()` before deciding whether
/// a DRIVER id was answerable AT ALL, and the fallback behind that gate
/// only knows the synthetic ids (1..4). So a client that read the topology
/// with the driver owning scanout got driver ids, and the moment the
/// answer flipped, every one of them came back EINVAL. Mesa's
/// `wsi_get_connectors` reports a miss on an advertised id as
/// `VK_ERROR_OUT_OF_HOST_MEMORY` for the whole `VK_KHR_display` query.
///
/// On the flip's DIRECTION, because it matters for what this does and does
/// not claim: in production `software_kms_active()` is
/// `primary_display().is_some() && !drivers.first().has_hardware_kms()`,
/// and the NVIDIA driver's `has_hardware_kms()` is
/// `surfaceflip_enabled() && hwflip_ready()`. `g_hwflip.ready` is only
/// ever assigned `NV_TRUE` and never cleared
/// (`nvidia-rm-sys/vendor/eclipse_rm_init.c`), so THAT lever moves
/// `software_kms_active()` true -> false only, which is the harmless
/// direction: the synthetic fallback still answers the synthetic ids. The
/// levers that can move it the other way are the boot framebuffer
/// appearing and `register_driver` putting a different driver at index 0,
/// both of which are boot-time events here. So this is a latent hole, not
/// a proven cause of anything; the emulator moves the KMS flag because it
/// is the one lever it has, and the code under test reads nothing but
/// `software_kms_active()`.
#[test]
fn a_connector_stays_answerable_when_scanout_moves_mid_probe() {
    let screen = kms_emu::attach(640, 480);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("gpu0").with_ids(2001, 1001, 3001));
    let c = Client::open(0);

    // What `drmModeGetResources` advertised while the driver owned scanout.
    let (crtcs, conns, _) = drm_mode_get_resources(&c).expect("GETRESOURCES");
    assert_eq!(conns, alloc::vec![1001]);
    assert_eq!(crtcs, alloc::vec![2001]);
    assert_eq!(drm_mode_get_plane_resources(&c), alloc::vec![3001]);

    // The flip ladder goes away between the two ioctls, as it does when
    // `hwflip_ready()` has not latched yet.
    gpu.set_hardware_kms(false);

    assert!(
        drm_mode_get_connector(&c, 1001).is_some(),
        "GETCONNECTOR refused an id GETRESOURCES had just advertised: \
         that is the EINVAL Mesa reports as ERROR_OUT_OF_HOST_MEMORY"
    );
    assert_eq!(wsi_get_connectors(&c), Ok(1));

    // The same window exists for the other two object types a compositor
    // reads back after GETRESOURCES/GETPLANERESOURCES.
    let mut crtc = blank_get_crtc(2001);
    assert!(
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).is_ok(),
        "GETCRTC refused an advertised CRTC id"
    );
    let mut plane = blank_get_plane(3001);
    assert!(
        c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut plane).is_ok(),
        "GETPLANE refused an advertised plane id"
    );
}

fn blank_get_crtc(id: u32) -> DrmModeGetCrtc {
    DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: id,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0u8; 68],
    }
}

fn blank_get_plane(id: u32) -> DrmModeGetPlane {
    DrmModeGetPlane {
        plane_id: id,
        crtc_id: 0,
        fb_id: 0,
        possible_crtcs: 0,
        gamma_size: 0,
        count_format_types: 0,
        format_type_ptr: 0,
    }
}

/// `drmModeGetPlaneResources`, both passes, after the one call every
/// plane-aware client makes first: `drm_mode_getplane_res` lists only
/// overlays to a file without `DRM_CLIENT_CAP_UNIVERSAL_PLANES`, and every
/// plane here is a primary, so without the cap the list is empty for
/// everyone, as on Linux.
fn drm_mode_get_plane_resources(c: &Client) -> Vec<u32> {
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("SET_CLIENT_CAP UNIVERSAL_PLANES");
    let mut probe = DrmModeGetPlaneRes {
        plane_id_ptr: 0,
        count_planes: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut probe)
        .expect("GETPLANERESOURCES");
    let mut ids = alloc::vec![0u32; probe.count_planes as usize];
    let mut fill = DrmModeGetPlaneRes {
        plane_id_ptr: ids.as_mut_ptr() as u64,
        count_planes: probe.count_planes,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut fill)
        .expect("GETPLANERESOURCES fill");
    ids
}

/// Two cards, as on the machine the bug was photographed on.
#[test]
fn vulkaninfo_probes_two_hardware_kms_gpus_without_an_error() {
    let screen = kms_emu::attach(640, 480);
    let _g0 = screen.attach_gpu(EmuGpu::hardware_kms("gpu0").with_ids(2001, 1001, 3001));
    let _g1 = screen.attach_gpu(EmuGpu::hardware_kms("gpu1").with_ids(2101, 1101, 3101));
    let c = Client::open(0);
    assert!(
        wsi_get_connectors(&c).is_ok(),
        "{:?}",
        wsi_get_connectors(&c)
    );
}

/// Two cards of the SAME model, which is what is actually in the machine:
/// the driver returns the same synthetic ids from both, and
/// GETPLANERESOURCES used to list the id twice. `drmModeGetPlane` on
/// either entry answers with the one plane, so the second entry
/// contradicts the first. `get_resources` has de-duplicated CRTCs and
/// connectors for this reason all along; the plane list had not.
#[test]
fn two_cards_of_the_same_model_do_not_list_the_same_plane_twice() {
    let screen = kms_emu::attach(640, 480);
    let _g0 = screen.attach_gpu(EmuGpu::hardware_kms("gpu0").with_ids(2001, 1001, 3001));
    let _g1 = screen.attach_gpu(EmuGpu::hardware_kms("gpu1").with_ids(2001, 1001, 3001));
    let c = Client::open(0);
    assert_eq!(drm_mode_get_plane_resources(&c), alloc::vec![3001]);
    let (crtcs, conns, _) = drm_mode_get_resources(&c).expect("GETRESOURCES");
    assert_eq!(crtcs, alloc::vec![2001]);
    assert_eq!(conns, alloc::vec![1001]);
}
