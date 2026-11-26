//! Both ends on the host: a [`Remote`] over a transport that hands each
//! message straight to [`answer`] on a card with something to say, and
//! every answer compared with what the card itself says.
use super::*;
use alloc::sync::Arc;
use alloc::vec;
use core::convert::TryInto;
use core::sync::atomic::{AtomicU32, Ordering};

/// A card with two connectors, one CRTC, a primary and a cursor plane.
#[derive(Default)]
struct Card {
    flipped: AtomicU32,
    cursor: lock::Mutex<Option<(i32, i32)>>,
    ready: core::sync::atomic::AtomicBool,
    /// The last ioctl's (cmd, pid, file, arg), and what was told.
    asked: lock::Mutex<Option<(u32, u64, usize, usize)>>,
    told: lock::Mutex<Vec<(&'static str, u64)>>,
}

/// `DRM_IOCTL_NOUVEAU_EXEC`: `_IOWR('d', 0x52, struct drm_nouveau_exec)`.
const EXEC: u32 = 0xc028_6452;

/// `struct drm_nouveau_exec`, as a caller lays it out.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Exec {
    channel: u32,
    push_count: u32,
    wait_count: u32,
    sig_count: u32,
    wait_ptr: u64,
    sig_ptr: u64,
    push_ptr: u64,
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
    /// EXEC: the sum of the pushes' `va` and the waits' handles, in
    /// `channel`; `channel` 99 is refused after it is written, as
    /// GEM_PUSHBUF writes its suffixes before an error. Any other command
    /// answers its argument word.
    fn ioctl_file(&self, cmd: u32, arg: usize, pid: u64, file: usize) -> Result<usize, i32> {
        *self.asked.lock() = Some((cmd, pid, file, arg));
        if cmd != EXEC {
            return Ok(arg);
        }
        let e = unsafe { &mut *(arg as *mut Exec) };
        let pushes = unsafe {
            core::slice::from_raw_parts(e.push_ptr as *const [u64; 2], e.push_count as usize)
        };
        let waits = match e.wait_count {
            0 => &[][..],
            n => unsafe { core::slice::from_raw_parts(e.wait_ptr as *const [u32; 4], n as usize) },
        };
        let refuse = e.channel == 99;
        e.channel = pushes.iter().map(|p| p[0] as u32).sum::<u32>()
            + waits.iter().map(|w| w[1]).sum::<u32>();
        if refuse {
            return Err(16);
        }
        Ok(7)
    }
    fn nouveau_file_closed(&self, file: usize) {
        self.told.lock().push(("closed", file as u64));
    }
    /// Handle 7 is the driver's, VRAM for pid 3 and sysmem for the rest.
    fn nouveau_gem_close(&self, handle: u32, pid: u64) -> bool {
        self.told.lock().push(("gem_close", pid));
        handle == 7
    }
    fn gem_aperture(&self, handle: u32, pid: u64) -> GemAperture {
        match (handle, pid) {
            (7, 3) => GemAperture::Vram,
            (7, _) => GemAperture::Sysmem,
            _ => GemAperture::Unknown,
        }
    }
    fn import_buffer(&self, h: GemHandle) -> bool {
        self.told
            .lock()
            .push(("import", h.phys_addr + h.size as u64 + h.id as u64));
        h.size > 0
    }
    fn free_buffer(&self, h: GemHandle) {
        self.told
            .lock()
            .push(("free", h.phys_addr + h.size as u64 + h.id as u64));
    }
    fn nouveau_release_process(&self, pid: u64) {
        self.told.lock().push(("released", pid));
    }
    /// The handles of the waits, each at its value's low word plus one;
    /// none for pid 9.
    fn exec_cpu_waits(&self, arg: usize, pid: u64) -> Option<(Vec<u32>, Vec<u64>)> {
        if pid == 9 {
            return None;
        }
        // SAFETY: the copy `answer` made of the caller's struct and array.
        let e = unsafe { &*(arg as *const Exec) };
        let waits = unsafe {
            core::slice::from_raw_parts(e.wait_ptr as *const [u32; 4], e.wait_count as usize)
        };
        Some((
            waits.iter().map(|w| w[1]).collect(),
            waits.iter().map(|w| w[2] as u64 + 1).collect(),
        ))
    }
    /// Two fences for handle 7, the second outside the driver's memory.
    fn cpu_prep_fences(&self, handle: u32, pid: u64) -> Vec<(usize, u32)> {
        if handle != 7 {
            return Vec::new();
        }
        vec![(FENCES + 0x40, pid as u32), (0xdead_0000, 1)]
    }
    fn render_fence_for_scanout(&self, handle: u32, _pid: u64) -> Option<(usize, u32)> {
        (handle == 7).then_some((FENCES + 0x80, 9))
    }
    fn queued_fences(&self, handle: u32) -> Vec<crate::scheme::syncobj::HwFence> {
        if handle != 7 {
            return Vec::new();
        }
        vec![crate::scheme::syncobj::HwFence {
            fence_va: FENCES + 0xc0,
            fence_gpu_va: 0x10_0000,
            payload: 12,
            ctx_idx: 3,
        }]
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
/// With each answer, the flags, as the driver's process sends them. The
/// caller's memory is this process's, and its writes are counted.
struct Direct(Arc<Card>, AtomicU32);

impl Direct {
    fn new(card: Arc<Card>) -> Self {
        Direct(card, AtomicU32::new(0))
    }
}

impl Transport for Direct {
    fn call(&self, op: u32, a: u64, b: u64, data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        let mut out = Vec::new();
        answer(&*self.0, op, a, b, data, &mut out).map(|w| (w, out))
    }
    fn flags(&self) -> Option<Flags> {
        Some(Flags::of(&*self.0))
    }
    fn read_user(&self, addr: usize, len: usize) -> Result<Vec<u8>, i32> {
        if addr == 0 {
            return Err(ioctl::EFAULT);
        }
        Ok(unsafe { core::slice::from_raw_parts(addr as *const u8, len) }.to_vec())
    }
    fn fence(&self, f: &Fence) -> Option<(usize, u32)> {
        let off = (f.phys as usize)
            .checked_sub(FENCES)
            .filter(|&o| o < 4096)?;
        Some((KERNEL_FENCES + off, 100 + f.ctx))
    }
    fn write_user(&self, addr: usize, bytes: &[u8]) -> Result<(), i32> {
        self.1.fetch_add(1, Ordering::Relaxed);
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
        Ok(())
    }
}

/// Where the test driver's fences are: a page the kernel maps at
/// [`KERNEL_FENCES`], and the channels it numbers from 100.
const FENCES: usize = 0x5000_0000;
const KERNEL_FENCES: usize = 0xffff_8000_0000_0000;

/// A driver that is gone: every call is ENODEV.
struct Gone;

impl Transport for Gone {
    fn call(&self, _op: u32, _a: u64, _b: u64, _data: &[u8]) -> Result<(u64, Vec<u8>), i32> {
        Err(ENODEV)
    }
}

fn pair() -> (Arc<Card>, Remote<Direct>) {
    let card = Arc::new(Card::default());
    let remote = Remote::new(Direct::new(card.clone())).expect("the card says who it is");
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

/// An EXEC as a caller makes it: the struct and its arrays in its memory.
fn exec_of(pushes: &[[u64; 2]], waits: &[[u32; 4]]) -> Exec {
    Exec {
        push_count: pushes.len() as u32,
        wait_count: waits.len() as u32,
        push_ptr: pushes.as_ptr() as u64,
        wait_ptr: if waits.is_empty() {
            0
        } else {
            waits.as_ptr() as u64
        },
        ..Exec::default()
    }
}

#[test]
fn an_ioctl_reaches_the_driver_with_its_struct_and_arrays_and_comes_back() {
    let (card, remote) = pair();
    let pushes = [[3u64, 0], [4, 0]];
    let waits = [[0u32, 10, 0, 0]];
    let mut e = exec_of(&pushes, &waits);
    let asked = e;
    let arg = &mut e as *mut Exec as usize;
    assert_eq!(remote.ioctl_file(EXEC, arg, 77, 0xf11e), Ok(7));
    // The driver wrote the sum into the caller's struct; every pointer
    // there is still the caller's own.
    assert_eq!(
        e,
        Exec {
            channel: 17,
            ..asked
        }
    );
    let (cmd, pid, file, at) = card.asked.lock().unwrap();
    assert_eq!((cmd, pid, file), (EXEC, 77, 0xf11e));
    // The driver was handed its own copy, not the caller's address.
    assert_ne!(at, arg);
}

#[test]
fn an_array_bigger_than_any_message_is_refused_before_it_is_copied() {
    let (card, remote) = pair();
    // A count the caller made up: the pointer is real, but nothing is read
    // from it (the test transport would read past the array if it were).
    let pushes = [[3u64, 0]];
    let mut e = exec_of(&pushes, &[]);
    e.push_count = (ioctl::MESSAGE_MAX / 16 + 1) as u32;
    assert_eq!(
        remote.ioctl_file(EXEC, &mut e as *mut Exec as usize, 77, 0xf11e),
        Err(ioctl::E2BIG)
    );
    // The driver was never asked.
    assert!(card.asked.lock().is_none());
}

#[test]
fn what_the_driver_writes_before_an_error_still_reaches_the_caller() {
    let (_, remote) = pair();
    let pushes = [[5u64, 0]];
    let mut e = Exec {
        channel: 99,
        ..exec_of(&pushes, &[])
    };
    assert_eq!(remote.ioctl(EXEC, &mut e as *mut Exec as usize), Err(16));
    assert_eq!(e.channel, 5);
}

#[test]
fn a_struct_the_driver_left_alone_is_not_written_back() {
    let (_, remote) = pair();
    // `_IOW('d', 0x50, u64)`: a struct the test card does not touch.
    let cmd = 0x4008_6450;
    let word = 0x1234u64;
    let arg = &word as *const u64 as usize;
    // The card answers the address it was handed: the copy's.
    assert!(remote.ioctl(cmd, arg).is_ok());
    assert_eq!(remote.transport().1.load(Ordering::Relaxed), 0);
}

#[test]
fn a_command_with_no_struct_hands_its_word_through() {
    let (card, remote) = pair();
    // `_IO('d', 0x50)`: no struct, the argument is a value.
    assert_eq!(remote.ioctl(0x6450, 0xabcd), Ok(0xabcd));
    assert_eq!(card.asked.lock().unwrap().3, 0xabcd);
}

#[test]
fn an_array_with_a_count_and_no_pointer_is_efault_before_the_driver() {
    let (card, remote) = pair();
    let mut e = Exec {
        push_count: 1,
        ..Exec::default()
    };
    assert_eq!(
        remote.ioctl(EXEC, &mut e as *mut Exec as usize),
        Err(ioctl::EFAULT)
    );
    assert!(card.asked.lock().is_none());
}

#[test]
fn a_message_short_of_its_arrays_is_refused_and_writes_nothing() {
    let card = Card::default();
    let e = Exec {
        push_count: 2,
        push_ptr: 0x1000,
        ..Exec::default()
    };
    let top = unsafe {
        core::slice::from_raw_parts(&e as *const Exec as *const u8, core::mem::size_of::<Exec>())
    };
    let mut msg = vec![0u8; 16];
    msg.extend_from_slice(top);
    msg.extend_from_slice(&[0u8; 16]); // one push of the two
    let mut out = Vec::new();
    answer(&card, op::IOCTL, EXEC as u64, 1, &msg, &mut out).unwrap();
    assert_eq!(
        i64::from_le_bytes(out[..8].try_into().unwrap()),
        -(EINVAL as i64)
    );
    assert_eq!(&out[8..], top);
    assert!(card.asked.lock().is_none());
}

#[test]
fn a_closed_file_and_a_gone_process_are_told_to_the_driver() {
    let (card, remote) = pair();
    remote.nouveau_file_closed(0xf11e);
    remote.nouveau_release_process(42);
    assert_eq!(
        *card.told.lock(),
        vec![("closed", 0xf11e), ("released", 42)]
    );
}

#[test]
fn an_ioctl_to_a_gone_driver_is_its_errno() {
    let remote = Remote::with_hello(Gone, &hello(&Card::default())).unwrap();
    let word = 0u64;
    assert_eq!(
        remote.ioctl(0x4008_6450, &word as *const u64 as usize),
        Err(ioctl::EFAULT)
    );
    assert_eq!(remote.ioctl(0x6450, 1), Err(ENODEV));
}

#[test]
fn the_driver_s_gem_objects_are_its_own_to_answer_for() {
    let (card, remote) = pair();
    assert!(remote.nouveau_gem_close(7, 42));
    assert!(!remote.nouveau_gem_close(8, 42));
    assert_eq!(remote.gem_aperture(7, 3), GemAperture::Vram);
    assert_eq!(remote.gem_aperture(7, 4), GemAperture::Sysmem);
    assert_eq!(remote.gem_aperture(9, 4), GemAperture::Unknown);
    let h = GemHandle {
        id: 5,
        size: 0x1000,
        phys_addr: 0x1_0000_0000,
    };
    assert!(remote.import_buffer(h));
    assert!(!remote.import_buffer(GemHandle { size: 0, ..h }));
    remote.free_buffer(h);
    let sum = 0x1_0000_0000 + 0x1000 + 5;
    assert_eq!(
        *card.told.lock(),
        vec![
            ("gem_close", 42),
            ("gem_close", 42),
            ("import", sum),
            ("import", sum - 0x1000),
            ("free", sum)
        ]
    );
}

#[test]
fn the_kernel_asks_the_driver_for_an_exec_s_cpu_waits_with_the_caller_s_arrays() {
    let (_card, remote) = pair();
    assert_eq!(ioctl::exec_cmd(), EXEC);
    let pushes = [[3u64, 0]];
    let waits = [[0u32, 10, 4, 0], [0, 11, 0, 0]];
    let mut e = exec_of(&pushes, &waits);
    let asked = e;
    let arg = &mut e as *mut Exec as usize;
    assert_eq!(
        remote.exec_cpu_waits(arg, 77),
        Some((vec![10, 11], vec![5, 1]))
    );
    assert_eq!(e, asked, "the caller's struct is only read");
    assert_eq!(remote.exec_cpu_waits(arg, 9), None);
    // A caller's struct that is not there asks nothing.
    assert_eq!(remote.exec_cpu_waits(0, 77), None);
}

#[test]
fn the_driver_s_fences_reach_the_kernel_where_the_kernel_maps_them() {
    let (_card, remote) = pair();
    // The one outside the driver's memory is left out.
    assert_eq!(
        remote.cpu_prep_fences(7, 33),
        vec![(KERNEL_FENCES + 0x40, 33)]
    );
    assert!(remote.cpu_prep_fences(8, 33).is_empty());
    assert_eq!(
        remote.render_fence_for_scanout(7, 1),
        Some((KERNEL_FENCES + 0x80, 9))
    );
    assert_eq!(remote.render_fence_for_scanout(8, 1), None);
    let q = remote.queued_fences(7);
    assert_eq!(q.len(), 1);
    assert_eq!(
        (q[0].fence_va, q[0].fence_gpu_va, q[0].payload, q[0].ctx_idx),
        (KERNEL_FENCES + 0xc0, 0x10_0000, 12, 103)
    );
    // A transport that cannot say where a fence is waits for none.
    let gone = Remote::with_hello(Gone, &hello(&Card::default())).unwrap();
    assert!(gone.cpu_prep_fences(7, 33).is_empty());
    assert!(gone.queued_fences(7).is_empty());
}
