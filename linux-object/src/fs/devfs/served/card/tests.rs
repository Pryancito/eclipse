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
    /// Where it says it is on the bus, and whether it scans out the
    /// console.
    place: Option<Place>,
    console: bool,
    /// The times nvidia-modeset was asked for.
    loads: AtomicU32,
    /// Its driver dies of nvidia-modeset's load, answering nothing.
    dies_of_the_load: bool,
    /// Where the pointer's plane was last moved to, and the messages its
    /// driver was told, not asked.
    pointer: AtomicU32,
    told: AtomicU32,
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
    fn pci_bdf(&self) -> Option<Place> {
        self.place
    }
    fn is_console_gpu(&self) -> bool {
        self.console
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
    fn nvkms_load(&self) -> Vec<String> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        vec![
            String::from("nvkms tiene el display"),
            String::from("modeset hecho"),
        ]
    }
    fn set_cursor(&self, _: u32, _: i32, _: i32, _: u32, _: u32) -> bool {
        false
    }
    fn hw_cursor_move(&self, x: i32, y: i32) -> bool {
        self.pointer
            .store(((x as u32) << 16) | y as u32, Ordering::SeqCst);
        true
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
    remote: Arc<Remote<OverLink>>,
    link: Arc<Link>,
    card: Arc<Card>,
    /// Ends once the link, and with it the kernel's end, is dropped.
    _answering: thread::JoinHandle<()>,
}

/// The card's server: each message answered with `answer`, until its end
/// of the FIFO is closed.
fn served() -> Served {
    served_as(Card::default())
}

/// [`served`], the driver's card being `card`.
fn served_as(card: Card) -> Served {
    let (ours, theirs) = Fifo::create(SLOTS * 2, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window.clone()).unwrap());
    let card = Arc::new(card);
    let hello = drm_remote::hello(&*card);
    let remote = Remote::with_hello(OverLink::new(link.clone(), None), &hello).unwrap();
    let remote = Arc::new(remote);
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
        // The process is gone, its end of the FIFO with it.
        if gpu.dies_of_the_load && req.op == drm_remote::op::NVKMS_LOAD {
            return;
        }
        let told = req.reqid == super::super::link::NO_REPLY;
        if told {
            gpu.told.fetch_add(1, Ordering::SeqCst);
        }
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
        link,
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

/// The pointer's motion is told too: it comes with every event of the
/// mouse, and the compositor's ioctl must not wait for the driver's
/// process on each.
#[test]
fn the_pointers_motion_is_told_and_waits_for_no_answer() {
    let s = served();
    assert!(s.remote.hw_cursor_move(3, 4));
    assert!(s.remote.page_flip(1), "what shows it arrived");
    assert_eq!(s.card.pointer.load(Ordering::SeqCst), (3 << 16) | 4);
    assert_eq!(s.card.told.load(Ordering::SeqCst), 1);
}

#[test]
fn a_hello_that_does_not_parse_makes_no_card() {
    let (ours, _theirs) = Fifo::create(SLOTS * 2, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    let link = Arc::new(Link::new(ours, window).unwrap());
    assert!(Remote::with_hello(OverLink::new(link, None), &[1, 2, 3]).is_err());
}

/// `/proc/gpuready` waits for every NVIDIA GPU the boot leaves to
/// `udrv-nvidia` until a card of a GPU is served for each, and no longer
/// than their time to be served: a driver that never serves holds no
/// session for good.
#[test]
fn gpus_left_to_udrv_nvidia_are_waited_for_until_served() {
    let ready = |left, served, overdue| -> alloc::vec::Vec<bool> {
        left_to_userspace(left, served, overdue)
            .map(|(up, _)| up)
            .collect()
    };
    assert_eq!(ready(2, 0, false), [false, false]);
    assert_eq!(ready(2, 1, false), [true, false]);
    assert_eq!(ready(2, 2, false), [true, true]);
    assert!(ready(0, 0, false).is_empty());
    assert_eq!(ready(2, 1, true), [true, true], "nothing more is coming");
    let lines = |overdue| -> alloc::vec::Vec<_> {
        left_to_userspace(2, 1, overdue).map(|(_, l)| l).collect()
    };
    assert_eq!(
        lines(false),
        [
            "nvidia@0: left to udrv-nvidia, served",
            "nvidia@1: left to udrv-nvidia, not served yet"
        ]
    );
    assert_eq!(
        lines(true),
        [
            "nvidia@0: left to udrv-nvidia, served",
            "nvidia@1: left to udrv-nvidia, never served"
        ]
    );
    // From the first time anything asked, not from the boot.
    assert!(!overdue(5_000, 5_000));
    assert!(!overdue(5_000, 5_000 + SERVE_PATIENCE_MS - 1));
    assert!(overdue(5_000, 5_000 + SERVE_PATIENCE_MS));
    assert!(!overdue(5_000, 1), "a clock read before the first asking");
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

/// `nvidia.nvkms` reaches the card's driver over the link, and what
/// nvidia-modeset says there comes back to the boot log.
#[test]
fn nvkms_is_loaded_by_the_driver_over_the_link() {
    let s = served();
    assert_eq!(
        s.remote.nvkms_load(),
        ["nvkms tiene el display", "modeset hecho"]
    );
    assert_eq!(
        s.remote.nvkms_service(),
        None,
        "its timers are the driver's"
    );
}

/// A card at a place on the bus that scans out the console or does not,
/// and nothing else.
struct At(Option<Place>, bool);

impl Scheme for At {
    fn name(&self) -> &str {
        "served-test-gpu"
    }
}

impl DrmScheme for At {
    fn get_caps(&self) -> DrmCaps {
        Card::default().get_caps()
    }
    fn pci_bdf(&self) -> Option<Place> {
        self.0
    }
    fn is_console_gpu(&self) -> bool {
        self.1
    }
    fn create_fb(&self, _: u32, _: u32, _: u32, _: u32) -> Option<u32> {
        None
    }
    fn page_flip(&self, _: u32) -> bool {
        false
    }
    fn set_cursor(&self, _: u32, _: i32, _: i32, _: u32, _: u32) -> bool {
        false
    }
    fn wait_vblank(&self, _: u32) -> bool {
        false
    }
    fn get_resources(&self) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        (vec![], vec![], vec![])
    }
    fn get_connector(&self, _: u32) -> Option<DrmConnector> {
        None
    }
    fn get_crtc(&self, _: u32) -> Option<DrmCrtc> {
        None
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

/// A link nobody answers on.
fn link() -> Arc<Link> {
    let (ours, _theirs) = Fifo::create(SLOTS * 2, MSG);
    let window = VmObject::new_paged((SLOTS * SLOT_LEN).div_ceil(4096));
    Arc::new(Link::new(ours, window).unwrap())
}

/// nvidia-modeset is due for the served card that scans out the console,
/// once a boot: not for the process's other GPUs, not again for the card
/// of a driver that started over, and not for a card whose driver is gone.
#[test]
fn nvkms_is_due_once_for_the_served_console_gpu() {
    let driven = |place, console| Driven {
        card: Arc::new(At(place, console)),
        link: None,
    };
    let due_at = |cards: &[Driven], asked: &[Place]| due(cards, asked).map(|(_, place)| place);
    let (console, compute) = ((0, 0x17, 0, 0), (0, 0x65, 0, 0));
    assert_eq!(due_at(&[], &[]), None, "none served yet");
    // The compute GPU is served first, and is not nvidia-modeset's to take.
    let cards = [driven(Some(compute), false), driven(Some(console), true)];
    assert_eq!(due_at(&cards[..1], &[]), None);
    assert_eq!(due_at(&cards, &[]), Some(console));
    // Asked: not again, and neither is the card a driver that started
    // over serves at the same place.
    assert_eq!(due_at(&cards, &[console]), None);
    // A card with no place on the bus (`udrv-nvidia drm-check`) is no GPU.
    assert_eq!(due_at(&[driven(None, true)], &[]), None);
    // A card whose driver is gone waits for the driver that starts again.
    let dead = link();
    dead.close();
    let gone = Driven {
        card: Arc::new(At(Some(console), true)),
        link: Some(dead),
    };
    assert_eq!(due_at(&[gone], &[]), None);
}

/// A frame to flip to is kept from the served console card while
/// nvidia-modeset is still to take it, and from no other card: its driver
/// would bring up a window channel of its own, and nvidia-modeset could
/// not take the card.
#[test]
fn the_flip_waits_for_nvkms_only_on_the_card_it_is_to_take() {
    let (console, compute) = ((0, 0x17, 0, 0), (0, 0x65, 0, 0));
    // A boot that gives no served card to nvidia-modeset (none with
    // `nvidia.console_gpu`): nothing waits, as before there was a load.
    assert!(!awaited(false, true, Some(console), &[]));
    // One that does: until that GPU's driver has answered the load,
    // whatever it answered, and not for another GPU's answer.
    assert!(awaited(true, true, Some(console), &[]));
    assert!(awaited(true, true, Some(console), &[compute]));
    assert!(!awaited(true, true, Some(console), &[compute, console]));
    // The process's other GPUs are not nvidia-modeset's to take.
    assert!(!awaited(true, false, Some(compute), &[]));
    assert!(awaited(true, true, None, &[console]));
}

/// A card of the kernel's is offered every frame to flip to, whatever the
/// boot expects of nvidia-modeset: only the cards processes drive wait
/// for it.
#[test]
fn a_card_of_the_kernels_is_always_offered_the_flip() {
    let kernels: Arc<dyn DrmScheme> = Arc::new(At(Some((0, 0x17, 0, 0)), true));
    assert!(flip_offered(&kernels));
}

/// What a test left in [`NVKMS`] and [`CARDS`], taken back when it ends.
struct Boot(Vec<Place>, Vec<Arc<dyn DrmScheme>>);

impl Drop for Boot {
    fn drop(&mut self) {
        for card in self.1.iter() {
            forget_served(card);
        }
        let mut nvkms = NVKMS.lock();
        nvkms.expected = false;
        nvkms.asked.retain(|place| !self.0.contains(place));
        nvkms.answered.retain(|place| !self.0.contains(place));
    }
}

/// The whole of it over a link. On a boot that brings the console GPU of
/// a process up, its card is asked for nvidia-modeset once, and only its
/// driver's answer has it offered a frame and the pointer; the compute
/// card beside it is never asked, and is as on any other boot.
#[test]
fn nvkms_is_loaded_for_the_served_console_card_and_no_other() {
    let _globals = drm::test_globals::lock();
    let (console_at, compute_at) = ((0, 0xf2, 0, 0), (0, 0xf3, 0, 0));
    let console = served_as(Card {
        place: Some(console_at),
        console: true,
        ..Card::default()
    });
    let compute = served_as(Card {
        place: Some(compute_at),
        ..Card::default()
    });
    let console_card: Arc<dyn DrmScheme> = console.remote.clone();
    let compute_card: Arc<dyn DrmScheme> = compute.remote.clone();
    let _boot = Boot(
        vec![console_at, compute_at],
        vec![console_card.clone(), compute_card.clone()],
    );
    note_served_over(&compute_card, Some(compute.link.clone()));
    note_served_over(&console_card, Some(console.link.clone()));
    // No console GPU is brought up: the cards are as they always were.
    assert!(flip_offered(&console_card) && flip_offered(&compute_card));
    assert!(!cursor_plane_offered(&*console_card));

    let asked = nvkms_asked();
    expect_nvkms();
    assert!(!flip_offered(&console_card), "nvidia-modeset is to take it");
    assert!(flip_offered(&compute_card), "not nvidia-modeset's to take");
    assert_eq!(
        nvkms_load_due(),
        ["nvkms tiene el display", "modeset hecho"]
    );
    assert_eq!(console.card.loads.load(Ordering::SeqCst), 1);
    assert_eq!(compute.card.loads.load(Ordering::SeqCst), 0);
    assert_eq!(nvkms_asked(), asked + 1);
    assert!(flip_offered(&console_card));
    assert!(cursor_plane_offered(&*console_card));
    assert!(
        !cursor_plane_offered(&*compute_card),
        "the kernel draws the pointer of a card nvidia-modeset did not take"
    );
    assert!(nvkms_load_due().is_empty(), "once a boot");
    assert_eq!(console.card.loads.load(Ordering::SeqCst), 1);
}

/// A driver the load killed answered nothing: the GPU is not asked again
/// in the boot, and the card served at its place afterwards is offered
/// neither a frame nor the pointer. The kernel copies and draws for it.
#[test]
fn a_driver_that_died_of_the_load_did_not_answer_it() {
    let _globals = drm::test_globals::lock();
    let place = (0, 0xf4, 0, 0);
    let console = served_as(Card {
        place: Some(place),
        console: true,
        dies_of_the_load: true,
        ..Card::default()
    });
    let card: Arc<dyn DrmScheme> = console.remote.clone();
    let again: Arc<dyn DrmScheme> = Arc::new(At(Some(place), true));
    let _boot = Boot(vec![place], vec![card.clone(), again.clone()]);
    note_served_over(&card, Some(console.link.clone()));
    expect_nvkms();
    let said = nvkms_load_due();
    assert_eq!(said.len(), 1, "{:?}", said);
    assert!(said[0].contains("did not load nvkms"), "{:?}", said);
    assert!(console.link.is_dead());
    // The driver starts again and serves the same GPU.
    forget_served(&card);
    note_served(&again);
    assert!(nvkms_load_due().is_empty(), "not asked twice in a boot");
    assert!(!flip_offered(&again));
    assert!(!cursor_plane_offered(&*again));
}

/// The pointer a served card's cursor plane had is the kernel's to draw
/// again when that card goes, and stays the plane's when another does.
#[test]
fn the_pointer_is_the_kernels_again_when_the_card_that_had_it_goes() {
    use drm::cursor_blit::plane_has_pointer_for_test as plane_has_pointer;
    let _globals = drm::test_globals::lock();
    let serve = || {
        let card: Arc<dyn DrmScheme> = Arc::new(At(None, false));
        note_served(&card);
        drm::register_driver(card.clone());
        ServedCard { link: link(), card }
    };
    // The last one registered is the first of the DRM core's list: the
    // card with the monitor, when none is a compute GPU.
    let (other, panel) = (serve(), serve());
    assert!(drm::get_display_driver().is_some_and(|d| Arc::ptr_eq(&d, &panel.card)));
    plane_has_pointer(Some(true));
    other.gone();
    assert!(plane_has_pointer(None), "another card's going");
    panel.gone();
    assert!(!plane_has_pointer(None));
}

/// Behind an IOMMU a served card is offered the copy of a frame only when
/// the frame is in a card's own VRAM and the card has the monitor: its
/// copy engine then reads nothing of the kernel's and writes its own
/// framebuffer. Without an IOMMU every card is offered every frame.
#[test]
fn behind_an_iommu_a_card_copies_only_its_own_vram_to_its_own_monitor() {
    for in_vram in [false, true] {
        for console in [false, true] {
            assert!(copy_offered(true, in_vram, console));
        }
    }
    assert!(copy_offered(false, true, true));
    assert!(
        !copy_offered(false, false, true),
        "a frame in the kernel's memory, which the GPU cannot read"
    );
    assert!(
        !copy_offered(false, true, false),
        "a card that would write another card's framebuffer"
    );
    assert!(!copy_offered(false, false, false));
}

/// A console GPU a process drives holds the session as the kernel's own
/// does: while the boot's bring-up of it is still to come, and once up,
/// until nvidia-modeset has been given it.
#[test]
fn a_console_gpu_of_a_process_is_waited_for_until_a_client_can_use_it() {
    let line = "17:00.0 nvidia: rm=no gr=no";
    let said = |more: &str| alloc::format!("{line}, in udrv-nvidia{more}");
    // Its bring-up is still ahead: the session waits.
    assert_eq!(waited_for(true, false, true, line), Some((false, said(""))));
    // The bring-up is over and left it cold, or the boot scheduled none:
    // nothing more is coming, and it holds nobody.
    assert_eq!(waited_for(false, false, true, line), None);
    assert_eq!(waited_for(false, false, false, line), None);
    // It came up: ready, once nvidia-modeset has it on a boot that loads
    // it, whether the bring-up has been declared over yet or not.
    for coming in [true, false] {
        assert_eq!(
            waited_for(coming, true, false, line),
            Some((false, said(", nvidia-modeset not loaded yet")))
        );
        assert_eq!(waited_for(coming, true, true, line), Some((true, said(""))));
    }
}
