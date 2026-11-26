//! The card over a real link, against a server that is a thread answering
//! with `drm_remote::answer` on a card of its own.

extern crate std;

use super::super::wire::{Msg, MSG, SLOTS, SLOT_LEN};
use super::*;
use core::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use zcore_drivers::scheme::drm::{DrmCaps, DrmConnector, DrmCrtc, DrmPlane};
use zcore_drivers::scheme::Scheme;
use zircon_object::ZxError;

#[derive(Default)]
struct Card {
    flipped: AtomicU32,
    closed: AtomicU32,
}

impl Scheme for Card {
    fn name(&self) -> &str {
        "served-test-card"
    }
}

impl DrmScheme for Card {
    fn get_caps(&self) -> DrmCaps {
        DrmCaps {
            has_3d: false,
            has_cursor: true,
            max_width: 4096,
            max_height: 4096,
        }
    }
    fn drives_own_display(&self) -> bool {
        true
    }
    /// Doubles the u32 its struct holds, for the caller `pid`.
    fn ioctl_file(&self, _cmd: u32, arg: usize, pid: u64, _file: usize) -> Result<usize, i32> {
        let v = unsafe { &mut *(arg as *mut u32) };
        *v *= 2;
        Ok(pid as usize)
    }
    fn nouveau_file_closed(&self, file: usize) {
        self.closed.store(file as u32, Ordering::SeqCst);
    }
    fn create_fb(&self, handle: u32, _w: u32, _h: u32, _pitch: u32) -> Option<u32> {
        Some(500 + handle)
    }
    fn page_flip(&self, fb: u32) -> bool {
        self.flipped.store(fb, Ordering::SeqCst);
        true
    }
    fn set_cursor(&self, _: u32, _: i32, _: i32, _: u32, _: u32) -> bool {
        false
    }
    fn wait_vblank(&self, _: u32) -> bool {
        true
    }
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (vec![], vec![31], vec![41])
    }
    fn get_connector(&self, id: u32) -> Option<DrmConnector> {
        (id == 41).then_some(DrmConnector {
            id,
            connected: true,
            mm_width: 1,
            mm_height: 1,
            connector_type: 14,
        })
    }
    fn get_crtc(&self, id: u32) -> Option<DrmCrtc> {
        (id == 31).then_some(DrmCrtc {
            id,
            fb_id: 0,
            x: 0,
            y: 0,
        })
    }
    fn get_plane(&self, _: u32) -> Option<DrmPlane> {
        None
    }
    fn get_planes(&self) -> Vec<u32> {
        vec![]
    }
    #[allow(clippy::too_many_arguments)]
    fn set_plane(
        &self,
        _: u32,
        _: u32,
        _: u32,
        _: i32,
        _: i32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
        _: u32,
    ) -> bool {
        false
    }
}

struct Served {
    remote: Remote<OverLink>,
    card: Arc<Card>,
    /// Ends once the link, and with it the kernel's end, is dropped.
    _answering: thread::JoinHandle<()>,
}

/// The card's server: each message answered with `answer`, until its end
/// of the FIFO is closed.
fn served() -> Served {
    let (ours, theirs) = Fifo::create(SLOTS * 2, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window.clone()).unwrap());
    let card = Arc::new(Card::default());
    let hello = drm_remote::hello(&*card);
    let remote = Remote::with_hello(OverLink::new(link, None), &hello).unwrap();
    let (gpu, server) = (card.clone(), theirs);
    let answering = thread::spawn(move || loop {
        let mut m = [0u8; MSG];
        match server.read(MSG, &mut m, 1) {
            Ok(_) => {}
            Err(ZxError::SHOULD_WAIT) => {
                thread::yield_now();
                continue;
            }
            Err(_) => return,
        }
        let req = Msg::decode(&m);
        let told = req.reqid == super::super::link::NO_REPLY;
        let slot = (req.reqid as usize & (SLOTS - 1)) * SLOT_LEN;
        let mut data = vec![0u8; req.len.min(SLOT_LEN as u32) as usize];
        window.read(slot, &mut data).unwrap();
        let mut out = Vec::new();
        let mut reply = Msg {
            op: req.op,
            reqid: req.reqid,
            fh: drm_remote::Flags::of(&*gpu).0 as u32,
            ..Msg::default()
        };
        match drm_remote::answer(&*gpu, req.op, req.a, req.b, &data, &mut out) {
            Ok(word) => {
                window.write(slot, &out).unwrap();
                reply.a = word;
                reply.len = out.len() as u32;
                reply.b = 0;
            }
            Err(errno) => reply.b = (-(errno as i64)) as u64,
        }
        if !told {
            server.write(MSG, &reply.encode(), 1).unwrap();
        }
    });
    Served {
        remote,
        card,
        _answering: answering,
    }
}

#[test]
fn the_card_is_the_one_the_driver_described() {
    let s = served();
    assert_eq!(s.remote.name(), "served-test-card");
    assert_eq!(s.remote.get_caps().max_width, 4096);
    assert!(s.remote.drives_own_display(), "asked over the link");
    assert!(!s.remote.has_hardware_kms());
}

#[test]
fn calls_cross_the_link_and_come_back() {
    let s = served();
    assert_eq!(s.remote.get_resources(), (vec![], vec![31], vec![41]));
    assert_eq!(
        s.remote.get_connector(41).map(|c| c.connector_type),
        Some(14)
    );
    assert!(
        s.remote.get_connector(40).is_none(),
        "ENOENT is no connector"
    );
    assert_eq!(s.remote.create_fb(3, 640, 480, 2560), Some(503));
    assert!(s.remote.page_flip(503));
    assert_eq!(s.card.flipped.load(Ordering::SeqCst), 503);
    // Many in a row, more than the link has slots.
    for fb in 0..(SLOTS as u32 * 3) {
        assert!(s.remote.page_flip(fb));
    }
}

#[test]
fn an_ioctl_crosses_the_link_with_its_struct() {
    let s = served();
    // `_IOWR('d', 0x50, u32)`.
    let mut v = 21u32;
    let arg = &mut v as *mut u32 as usize;
    assert_eq!(s.remote.ioctl_file(0xc004_6450, arg, 9, 0), Ok(9));
    assert_eq!(v, 42);
}

#[test]
fn a_closed_file_is_told_and_waits_for_no_answer() {
    let s = served();
    s.remote.nouveau_file_closed(0x77);
    // Told, not asked: the next call is what shows it arrived.
    assert!(s.remote.page_flip(1));
    assert_eq!(s.card.closed.load(Ordering::SeqCst), 0x77);
}

#[test]
fn a_hello_that_does_not_parse_makes_no_card() {
    let (ours, _theirs) = Fifo::create(SLOTS * 2, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window).unwrap());
    assert!(Remote::with_hello(OverLink::new(link, None), &[1, 2, 3]).is_err());
}

/// `/proc/gpuready` waits for every NVIDIA GPU the boot leaves to
/// `udrv-nvidia` until a card of a GPU is served for each.
#[test]
fn gpus_left_to_udrv_nvidia_are_waited_for_until_served() {
    let ready = |left, served| -> alloc::vec::Vec<bool> {
        left_to_userspace(left, served).map(|(up, _)| up).collect()
    };
    assert_eq!(ready(2, 0), [false, false]);
    assert_eq!(ready(2, 1), [true, false]);
    assert_eq!(ready(2, 2), [true, true]);
    assert!(ready(0, 0).is_empty());
    let lines: alloc::vec::Vec<_> = left_to_userspace(2, 1).map(|(_, l)| l).collect();
    assert_eq!(
        lines,
        [
            "nvidia@0: left to udrv-nvidia, served",
            "nvidia@1: left to udrv-nvidia, not served yet"
        ]
    );
}

/// The console bring-up's scanout pause outlasts a served card's patience,
/// so it never lifts while the process is still bringing the card up; with
/// no served GPU it is the kernel's own.
#[test]
fn the_bringup_pause_outlasts_a_served_cards_patience() {
    use crate::fs::devfs::drm::SCANOUT_PAUSE_MAX;
    assert_eq!(pause_for(0), SCANOUT_PAUSE_MAX);
    let served = pause_for(2);
    assert!(served > core::time::Duration::from_millis(BRINGUP_PATIENCE_MS));
    assert!(served >= SCANOUT_PAUSE_MAX);
}
