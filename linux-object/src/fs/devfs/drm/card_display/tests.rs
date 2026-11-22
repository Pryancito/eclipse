//! A second card that lights its own monitors, and the hotplug poll.
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu};
use zcore_drivers::scheme::drm::DisplayMode;
use zircon_object::vm::VmObject;

/// Drops the node table however the test ends.
struct NodeTable;
impl Drop for NodeTable {
    fn drop(&mut self) {
        clear_gpu_nodes_for_test();
    }
}

/// The panel's card and a second one. The `Screen` is declared LAST so it
/// drops last and the test lock is held through the whole teardown (see
/// `prime_import_card_tests::TwoCards`).
struct TwoCards {
    _table: NodeTable,
    console: kms_emu::Gpu,
    other: kms_emu::Gpu,
    other_minor: u32,
    _screen: kms_emu::Screen,
}

fn mode(width: u32, height: u32) -> DisplayMode {
    DisplayMode {
        width,
        height,
        refresh_hz: 60,
        pixel_clock_khz: 148_500,
        preferred: true,
    }
}

fn two_cards() -> TwoCards {
    let screen = kms_emu::attach(16, 16);
    let table = NodeTable;
    let console = screen.attach_gpu(EmuGpu::new("emu-panel").console_at(0x01, 0x00));
    let other = screen.attach_gpu(
        EmuGpu::new("emu-second")
            .compute_at(0x65, 0x00)
            .with_ids(0x7711, 0x7712, 0x7713)
            .with_modes(&[mode(64, 16)]),
    );
    build_gpu_nodes();
    let other_minor = gpu_nodes()
        .iter()
        .find(|n| other.is_driver(&n.driver))
        .map(|n| n.card_minor())
        .expect("the second card has a node");
    TwoCards {
        _table: table,
        console,
        other,
        other_minor,
        _screen: screen,
    }
}

/// A dumb buffer planted straight in the handle table.
fn plant(id: u32, size: usize) {
    let vmo = VmObject::new_paged(size.div_ceil(4096));
    DRM_STATE.lock().handles.push((
        GemHandle {
            id,
            size,
            phys_addr: 0,
        },
        vmo,
        0,
    ));
}

fn unplant(id: u32) {
    let mut state = DRM_STATE.lock();
    state.handles.retain(|(h, _, _)| h.id != id);
    state.framebuffers.retain(|fb| fb.gem_handle_id != id);
}

/// Linux: the second card's framebuffer is its own driver's, its modeset
/// goes to its own monitors and its flip to its own heads. None of it
/// reaches the panel's driver.
#[test]
fn a_card_that_lights_its_own_monitors_gets_its_own_fbs_modesets_and_flips() {
    let cards = two_cards();
    cards.other.set_own_display(true);
    build_gpu_nodes();
    let card = card_of_minor(cards.other_minor);
    assert_ne!(card, PANEL_CARD);

    plant(9511, 64 * 1024);
    let fb = create_fb_on(card, 9511, 64, 16, 256, 0, ScanoutLayout::Linear)
        .expect("ADDFB on the second card");
    let made = cards.other.created_fbs();
    assert_eq!(made.len(), 1, "made by the second card's driver");
    assert!(
        cards
            .console
            .created_fbs()
            .iter()
            .all(|f| f.gem_handle != 9511),
        "not by the panel's"
    );
    let own = made[0].driver_fb_id;

    assert_eq!(set_mode_on(cards.other_minor, 64, 16, 60, fb), Ok(()));
    assert_eq!(cards.other.modesets(), [(64, 16, 60, own)]);
    assert!(cards.console.modesets().is_empty());
    assert!(matches!(
        set_mode_on(cards.other_minor, 640, 480, 60, fb),
        Err(ModeRefused::Driver(_))
    ));

    assert!(present_on_checked(cards.other_minor, fb, 0x7711, None).is_ok());
    assert_eq!(cards.other.flips(), [own], "flipped with its own id");
    assert!(cards.console.flips().is_empty(), "the panel is untouched");
    assert_eq!(crtc_fb_on(card), fb);
    assert_eq!(driver_modes_on(cards.other_minor).len(), 1);
    unplant(9511);
}

/// One NVIDIA card, or a second one without nvkms: nothing changes, the
/// second card's frames are only recorded and it does no modesets.
#[test]
fn a_card_that_does_not_light_its_own_monitors_behaves_as_before() {
    let cards = two_cards();
    let card = card_of_minor(cards.other_minor);

    plant(9512, 64 * 1024);
    let fb = create_fb_on(card, 9512, 64, 16, 256, 0, ScanoutLayout::Linear)
        .expect("ADDFB on the second card");
    assert!(cards.other.created_fbs().is_empty());
    assert_eq!(
        set_mode_on(cards.other_minor, 64, 16, 60, fb),
        Err(ModeRefused::Fixed)
    );
    assert!(cards.other.modesets().is_empty());
    assert!(present_on_checked(cards.other_minor, fb, 0x7711, None).is_ok());
    assert!(cards.other.flips().is_empty());
    assert!(driver_modes_on(cards.other_minor).is_empty());
    unplant(9512);
}

/// Linux's output poll: a uevent for the card whose connectors changed,
/// once, and nothing for a card whose did not.
#[test]
fn the_poll_sends_one_hotplug_for_the_card_that_changed() {
    let cards = two_cards();
    assert_eq!(poll_hotplug(), 0, "nothing changed");
    cards.other.plug();
    assert_eq!(poll_hotplug(), 1, "the second card's monitor came");
    assert_eq!(poll_hotplug(), 0, "and it is reported once");
}
