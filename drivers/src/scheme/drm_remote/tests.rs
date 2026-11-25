//! Both ends on the host: a [`Remote`] over a transport that hands each
//! message straight to [`answer`] on a card with something to say, and
//! every answer compared with what the card itself says.
use super::*;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicU32, Ordering};

/// A card with two connectors, one CRTC, a primary and a cursor plane.
#[derive(Default)]
struct Card {
    flipped: AtomicU32,
    cursor: lock::Mutex<Option<(i32, i32)>>,
    ready: core::sync::atomic::AtomicBool,
}

impl Scheme for Card {
    fn name(&self) -> &str {
        "card-for-test"
    }
}

impl DrmScheme for Card {
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: true,
            has_cursor: true,
            max_width: 8192,
            max_height: 4096,
        }
    }
    fn pci_bdf(&self) -> Option<(u32, u8, u8, u8)> {
        Some((0, 1, 0, 0))
    }
    fn has_hardware_kms(&self) -> bool {
        true
    }
    fn nouveau_uapi_capable(&self) -> bool {
        true
    }
    fn nouveau_uapi_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
    fn create_fb(&self, handle: u32, w: u32, h: u32, pitch: u32) -> Option<u32> {
        (pitch >= w * 4 && h > 0).then_some(1000 + handle)
    }
    fn create_fb_tiled(
        &self,
        handle: u32,
        w: u32,
        h: u32,
        pitch: u32,
        gobs: Option<u8>,
    ) -> Option<u32> {
        match gobs {
            None => self.create_fb(handle, w, h, pitch),
            Some(4) => Some(2000 + handle),
            Some(_) => None,
        }
    }
    fn page_flip(&self, fb: u32) -> bool {
        self.flipped.store(fb, Ordering::Relaxed);
        fb >= 1000
    }
    fn set_cursor(&self, crtc: u32, x: i32, y: i32, _handle: u32, _flags: u32) -> bool {
        *self.cursor.lock() = Some((x, y));
        crtc == 40
    }
    fn hw_cursor_set(&self, argb: &[u32], w: u32, h: u32) -> bool {
        argb.len() == (w * h) as usize && argb[0] == 0xff00_00ff
    }
    fn hw_cursor_move(&self, x: i32, y: i32) -> bool {
        *self.cursor.lock() = Some((x, y));
        true
    }
    fn wait_vblank(&self, crtc: u32) -> bool {
        crtc == 40
    }
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (vec![1001], vec![40], vec![50, 51])
    }
    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        (id == 50 || id == 51).then_some(DrmConnector {
            id,
            connected: id == 50,
            mm_width: 600,
            mm_height: 340,
            connector_type: 11,
        })
    }
    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        (id == 40).then_some(DrmCrtc {
            id,
            fb_id: 1001,
            x: 0,
            y: 0,
        })
    }
    fn get_plane(&self, id: u32) -> Option<DrmPlane> {
        (id == 60 || id == 61).then_some(DrmPlane {
            id,
            crtc_id: 40,
            fb_id: 0,
            possible_crtcs: 1,
            plane_type: if id == 60 { 1 } else { 2 },
        })
    }
    fn get_planes(&self) -> Vec<u32> {
        vec![60, 61]
    }
    fn set_plane(
        &self,
        plane: u32,
        crtc: u32,
        fb: u32,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        sx: u32,
        sy: u32,
        sw: u32,
        sh: u32,
    ) -> bool {
        (plane, crtc, fb, x, y, w, h, sx, sy, sw, sh)
            == (61, 40, 1001, -3, 7, 64, 64, 0, 0, 64 << 16, 64 << 16)
    }
    fn display_modes(&self) -> Vec<DisplayMode> {
        vec![
            DisplayMode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                pixel_clock_khz: 148_500,
                preferred: true,
            },
            DisplayMode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
                pixel_clock_khz: 74_250,
                preferred: false,
            },
        ]
    }
    fn set_display_mode(
        &self,
        w: u32,
        _h: u32,
        _hz: u32,
        _fb: u32,
        test: bool,
    ) -> Option<Result<(), String>> {
        match (w, test) {
            (1920, _) => Some(Ok(())),
            (_, true) => Some(Err(String::from("no such mode"))),
            _ => None,
        }
    }
    fn get_connector_edid(&self, id: u32) -> Option<[u8; 128]> {
        (id == 50).then(|| {
            let mut e = [0u8; 128];
            e[1..7].fill(0xff);
            e[127] = 0x42;
            e
        })
    }
}

/// The transport of the test: the message goes to `answer` at once.
/// With each answer, the flags, as the driver's process sends them.
struct Direct(Arc<Card>);

impl Transport for Direct {
    fn call(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        let mut out = Vec::new();
        answer(&*self.0, op, a, b, data, &mut out).map(|w| (w, out))
    }
    fn flags(&self) -> Option<Flags> {
        Some(Flags::of(&*self.0))
    }
}

/// A driver that is gone: every call is ENODEV.
struct Gone;

impl Transport for Gone {
    fn call(&self, _op: u32, _a: u64, _b: u64, _data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        Err(ENODEV)
    }
}

fn pair() -> (Arc<Card>, Remote<Direct>) {
    let card = Arc::new(Card::default());
    let remote = Remote::new(Direct(card.clone())).expect("the card says who it is");
    (card, remote)
}

#[test]
fn the_remote_card_is_who_the_driver_says() {
    let (card, remote) = pair();
    assert_eq!(remote.name(), "card-for-test");
    let (a, b) = (remote.get_caps(), card.get_caps());
    assert_eq!(
        (a.has_3d, a.has_cursor, a.max_width, a.max_height),
        (b.has_3d, b.has_cursor, b.max_width, b.max_height)
    );
    assert_eq!(remote.pci_bdf(), Some((0, 1, 0, 0)));
}

#[test]
fn the_flags_follow_the_driver_as_it_comes_up() {
    let (card, remote) = pair();
    assert!(remote.has_hardware_kms() && remote.nouveau_uapi_capable());
    assert!(!remote.is_console_gpu() && !remote.drives_own_display());
    assert!(!remote.nouveau_uapi_ready());
    card.ready.store(true, Ordering::Relaxed);
    assert!(
        !remote.nouveau_uapi_ready(),
        "never asked on its own: the core asks under its lock"
    );
    // The next answer brings them.
    let _ = remote.get_planes();
    assert!(remote.nouveau_uapi_ready());
    card.ready.store(false, Ordering::Relaxed);
    remote.refresh();
    assert!(!remote.nouveau_uapi_ready());
}

#[test]
fn the_topology_crosses_whole() {
    let (card, remote) = pair();
    assert_eq!(remote.get_resources(), card.get_resources());
    let c = remote.get_connector(50).unwrap();
    assert_eq!(
        (c.id, c.connected, c.mm_width, c.mm_height, c.connector_type),
        (50, true, 600, 340, 11)
    );
    assert!(!remote.get_connector(51).unwrap().connected);
    assert!(remote.get_connector(52).is_none());
    let crtc = remote.get_crtc(40).unwrap();
    assert_eq!((crtc.id, crtc.fb_id), (40, 1001));
    assert!(remote.get_crtc(41).is_none());
    assert_eq!(remote.get_planes(), vec![60, 61]);
    assert_eq!(remote.get_plane(61).unwrap().plane_type, 2);
    assert_eq!(remote.display_modes(), card.display_modes());
    assert_eq!(remote.get_connector_edid(50), card.get_connector_edid(50));
    assert_eq!(remote.get_connector_edid(51), None);
}

#[test]
fn framebuffers_flips_and_the_cursor_reach_the_driver() {
    let (card, remote) = pair();
    assert_eq!(remote.create_fb(7, 1920, 1080, 7680), Some(1007));
    assert_eq!(remote.create_fb(7, 1920, 1080, 100), None);
    assert_eq!(
        remote.create_fb_tiled(7, 1920, 1080, 7680, Some(4)),
        Some(2007)
    );
    assert_eq!(
        remote.create_fb_tiled(7, 1920, 1080, 7680, Some(0)),
        None,
        "GOBs 0 is not linear"
    );
    assert!(remote.page_flip(1007));
    assert_eq!(card.flipped.load(Ordering::Relaxed), 1007);
    assert!(!remote.page_flip(3));
    assert!(remote.set_cursor(40, -5, 9, 3, 1));
    assert_eq!(*card.cursor.lock(), Some((-5, 9)));
    assert!(remote.hw_cursor_move(-1, -2));
    assert_eq!(
        *card.cursor.lock(),
        Some((-1, -2)),
        "negative positions survive the trip"
    );
    let mut argb = vec![0u32; 64 * 64];
    argb[0] = 0xff00_00ff;
    assert!(remote.hw_cursor_set(&argb, 64, 64));
    assert!(!remote.hw_cursor_set(&argb[..10], 64, 64));
    assert!(remote.wait_vblank(40) && !remote.wait_vblank(41));
    assert!(remote.set_plane(61, 40, 1001, -3, 7, 64, 64, 0, 0, 64 << 16, 64 << 16));
    assert!(!remote.set_plane(61, 40, 1001, 3, 7, 64, 64, 0, 0, 64 << 16, 64 << 16));
}

#[test]
fn a_modeset_says_whether_it_was_done_and_why_not() {
    let (_, remote) = pair();
    assert_eq!(
        remote.set_display_mode(1920, 1080, 60, 1001, false),
        Some(Ok(()))
    );
    assert_eq!(
        remote.set_display_mode(640, 480, 60, 1001, true),
        Some(Err(String::from("no such mode")))
    );
    assert_eq!(remote.set_display_mode(640, 480, 60, 1001, false), None);
}

#[test]
fn a_card_whose_driver_is_gone_answers_nothing_and_never_panics() {
    let (card, _) = pair();
    // Made while the driver was there...
    let mut out = Vec::new();
    answer(&*card, op::HELLO, 0, 0, &[], &mut out).unwrap();
    struct Once(lock::Mutex<Option<Vec<u8>>>);
    impl Transport for Once {
        fn call(&self, _: u32, _: u64, _: u64, _: &[u8]) -> Result<(u64, Vec<u8>), i32> {
            self.0.lock().take().map(|b| (0, b)).ok_or(ENODEV)
        }
    }
    let remote = Remote::new(Once(lock::Mutex::new(Some(out)))).unwrap();
    // ...and asked after it went.
    assert_eq!(remote.get_resources(), (vec![], vec![], vec![]));
    assert!(remote.get_connector(50).is_none());
    assert!(!remote.page_flip(1007));
    assert!(remote.has_hardware_kms(), "the flags last heard");
    assert!(remote.display_modes().is_empty());
    assert!(matches!(
        remote.set_display_mode(1920, 1080, 60, 1, false),
        Some(Err(_))
    ));
    assert!(
        Remote::new(Gone).is_err(),
        "no card from a driver that never answered"
    );
}

#[test]
fn a_message_that_does_not_parse_is_einval() {
    let card = Card::default();
    let mut out = Vec::new();
    assert_eq!(
        answer(&card, op::CREATE_FB, 0, 0, &[1, 0, 0, 0], &mut out),
        Err(EINVAL)
    );
    assert_eq!(
        answer(&card, op::SET_PLANE, 0, 0, &[0; 40], &mut out),
        Err(EINVAL)
    );
    assert_eq!(answer(&card, 9999, 0, 0, &[], &mut out), Err(EINVAL));
    // A list that claims more than the bytes hold.
    let mut i = In(&[0xff, 0xff, 0xff, 0x7f, 1, 0, 0, 0]);
    assert_eq!(i.list(), None);
}
