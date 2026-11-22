use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use zcore_drivers::scheme::drm::DisplayMode;

/// Moebius's box: one RTX drives the monitor and has real hardware scanout, the
/// other is a headless compute card. Registration order is the reverse of the
/// PCI probe order (`insert(0, ..)`), so the compute card ends up
/// `drivers.first()` -- and asking IT whether the panel needs the software blit
/// used to answer yes, hiding the hardware scanout that is really lighting the
/// screen.
#[test]
fn the_card_with_the_monitor_decides_whether_software_kms_paints() {
    let screen = kms_emu::attach(64, 16);
    let _console = screen.attach_gpu(EmuGpu::hardware_kms("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::new("emu-compute").compute_at(0x65, 0x00));

    assert_eq!(
        get_display_driver().map(|d| d.has_hardware_kms()),
        Some(true),
        "the display question must go to the console card, not to drivers.first()"
    );
    assert!(
        !software_kms_active(),
        "the console card has hardware KMS, so the CPU blit is not what paints the panel"
    );
}

/// The other direction, which is the one that bites hardest: `hwflip_ready()`
/// is a single global for the whole box, so the compute card can end up
/// claiming hardware KMS while the console card cannot scan out at all. Asking
/// the compute card then reports hardware scanout on a panel the CPU blit is
/// painting, and the blit stops being set up.
#[test]
fn a_compute_card_claiming_hardware_kms_does_not_speak_for_the_panel() {
    let screen = kms_emu::attach(64, 16);
    let _console = screen.attach_gpu(EmuGpu::new("emu-console").console_at(0x01, 0x00));
    let _compute = screen.attach_gpu(EmuGpu::hardware_kms("emu-compute").compute_at(0x65, 0x00));

    assert_eq!(
        get_display_driver().map(|d| d.has_hardware_kms()),
        Some(false),
        "the console card drives the panel and it has no hardware KMS"
    );
    assert!(
        software_kms_active(),
        "with no hardware scanout on the console card the panel needs the CPU blit"
    );
}

/// A single-card box must keep answering exactly as before: the one registered
/// driver is both the primary and the display driver.
#[test]
fn one_card_is_its_own_display_driver() {
    let screen = kms_emu::attach(64, 16);
    let _only = screen.attach_gpu(EmuGpu::hardware_kms("emu-only").console_at(0x01, 0x00));

    assert!(
        get_display_driver()
            .zip(get_primary_driver())
            .map(|(a, b)| Arc::ptr_eq(&a, &b))
            .unwrap_or(false),
        "with one card the display driver and the primary driver are the same Arc"
    );
    assert!(!software_kms_active());
}

/// Every card a compute card (no console card registered at all) has no better
/// answer than the primary, and must not fall through to `None`: that would
/// turn the software-KMS answer around on a box where it used to be right.
#[test]
fn all_compute_falls_back_to_the_primary_instead_of_none() {
    let screen = kms_emu::attach(64, 16);
    let _a = screen.attach_gpu(EmuGpu::hardware_kms("emu-a").compute_at(0x01, 0x00));
    let _b = screen.attach_gpu(EmuGpu::new("emu-b").compute_at(0x65, 0x00));

    assert!(
        get_display_driver()
            .zip(get_primary_driver())
            .map(|(a, b)| Arc::ptr_eq(&a, &b))
            .unwrap_or(false),
        "with nothing but compute cards the primary stays the answer"
    );
}

/// What `DRM_IOCTL_GET_CAP` and the mode limits come from. These describe the
/// monitor, so they have to come off the card the monitor is plugged into: a
/// compute card reports its own display engine's limits, and userspace would
/// then size a surface for a panel that card is not connected to.
#[test]
fn the_panel_limits_come_from_the_card_the_panel_is_on() {
    let screen = kms_emu::attach(64, 16);
    let _console = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-console")
            .console_at(0x01, 0x00)
            .with_caps(3840, 2160),
    );
    let _compute = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-compute")
            .compute_at(0x65, 0x00)
            .with_caps(640, 480),
    );

    let caps = get_caps().expect("hardware KMS is active, so a driver answers");
    assert_eq!(
        (caps.max_width, caps.max_height),
        (3840, 2160),
        "the caps came off the compute card, which has no monitor on it"
    );
}

fn mode(width: u32, height: u32, refresh_hz: u32, preferred: bool) -> DisplayMode {
    DisplayMode {
        width,
        height,
        refresh_hz,
        pixel_clock_khz: 1,
        preferred,
    }
}

/// A mode blob for `w`x`h` at `hz`, as a client builds one for MODE_ID.
fn mode_blob(w: u16, h: u16, hz: u32) -> u32 {
    let mut m = alloc::vec![0u8; 68];
    m[0..4].copy_from_slice(&1000u32.to_ne_bytes());
    m[4..6].copy_from_slice(&w.to_ne_bytes());
    m[14..16].copy_from_slice(&h.to_ne_bytes());
    m[24..28].copy_from_slice(&hz.to_ne_bytes());
    create_blob(m, true)
}

/// With nvkms on the panel's card (`nvidia.nvkms`), the panel is no longer
/// stuck at the firmware's mode: a MODE_ID of another size the driver lists
/// passes the check, and one it does not list is refused as before.
#[test]
fn a_driver_that_does_modesets_takes_a_mode_the_firmware_did_not_set() {
    let screen = kms_emu::attach(64, 16);
    super::panel_mode::reset_for_test();
    let _gpu = screen.attach_gpu(
        EmuGpu::hardware_kms("emu-nvkms")
            .console_at(0x17, 0x00)
            .with_modes(&[mode(64, 16, 60, true), mode(32, 8, 75, false)]),
    );
    assert_eq!(driver_modes().len(), 2);
    assert!(!needs_modeset(64, 16), "the firmware's mode is the panel's");
    assert!(needs_modeset(32, 8));

    let file = DrmFileState::new();
    let listed = AtomicUpdate {
        mode_blob: Some(mode_blob(32, 8, 75)),
        connector_crtc_id: Some(SYNTH_CRTC_ID),
        ..AtomicUpdate::default()
    };
    assert_eq!(
        atomic_commit(&listed, true, true, false, 0, &file, None),
        Ok(())
    );
    let unlisted = AtomicUpdate {
        mode_blob: Some(mode_blob(48, 12, 60)),
        connector_crtc_id: Some(SYNTH_CRTC_ID),
        ..AtomicUpdate::default()
    };
    assert_eq!(
        atomic_commit(&unlisted, true, true, false, 0, &file, None),
        Err(AtomicError::Invalid)
    );
    assert_eq!(
        panel_mode(),
        Some((64, 16)),
        "a TEST_ONLY commit sets nothing"
    );

    assert_eq!(set_mode(32, 8, 75, 0, false), Ok(()));
    assert_eq!(panel_mode(), Some((32, 8)));
    assert_eq!(_gpu.modesets(), [(32, 8, 75, 0)]);
    assert_eq!(set_mode(64, 16, 60, 0, false), Ok(()));
    assert_eq!(panel_mode(), Some((64, 16)), "back at the firmware's mode");
    super::panel_mode::reset_for_test();
}

/// Without such a driver nothing changes: one mode, and any other refused.
#[test]
fn without_modesets_the_panel_keeps_the_firmware_mode() {
    let screen = kms_emu::attach(64, 16);
    super::panel_mode::reset_for_test();
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-fixed").console_at(0x17, 0x00));
    assert!(driver_modes().is_empty());
    assert_eq!(set_mode(32, 8, 60, 0, false), Err(ModeRefused::Fixed));
    let file = DrmFileState::new();
    let other = AtomicUpdate {
        mode_blob: Some(mode_blob(32, 8, 60)),
        connector_crtc_id: Some(SYNTH_CRTC_ID),
        ..AtomicUpdate::default()
    };
    assert_eq!(
        atomic_commit(&other, true, true, false, 0, &file, None),
        Err(AtomicError::Invalid)
    );
}
