use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};

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
