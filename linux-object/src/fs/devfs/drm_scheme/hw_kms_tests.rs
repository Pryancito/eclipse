use super::gl_client_sequence_tests::{blank_card_res, parse_events, Client, FLIP_COMPLETE};
use super::*;
use crate::fs::devfs::kms_emu::{self, EmuGpu, UNTOUCHED};
use alloc::vec::Vec;
use kernel_hal::mem::phys_to_virt;

/// Paint a dumb buffer through its physical backing (see
/// `kms_scanout_tests::paint`, which this deliberately mirrors).
fn paint(buf: &DrmModeCreateDumb, value: u32) {
    let (pa, size) = drm::resolve_gem_backing(buf.handle).expect("a dumb buffer must have backing");
    let va = phys_to_virt(pa as usize);
    // SAFETY: `size` bytes of contiguous physical memory owned by this
    // buffer, identity-mapped into the kernel window at `va`.
    let px = unsafe { core::slice::from_raw_parts_mut(va as *mut u32, size / 4) };
    for p in px.iter_mut() {
        *p = value;
    }
}

fn set_crtc(c: &Client, crtc_id: u32, fb_id: u32, w: u32, h: u32) {
    let mut req = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id,
        fb_id,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 1,
        mode: make_modeinfo(w, h),
    };
    c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req).expect("SETCRTC");
}

fn get_crtc_fb(c: &Client, crtc_id: u32) -> u32 {
    let mut crtc = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
    crtc.fb_id
}

/// `struct drm_mode_cursor`, 28 bytes.
#[repr(C)]
struct ModeCursor {
    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle: u32,
}

fn set_cursor(c: &Client, crtc_id: u32, handle: u32, w: u32, h: u32, x: i32, y: i32) {
    let mut cur = ModeCursor {
        flags: 0x01 | 0x02, // BO | MOVE
        crtc_id,
        x,
        y,
        width: w,
        height: h,
        handle,
    };
    c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur).expect("CURSOR");
}

/// `drmWaitVBlank` with a relative target of 0, which asks for the current
/// sequence and must not block.
fn wait_vblank(c: &Client) {
    const DRM_VBLANK_RELATIVE: u32 = 0x1;
    let mut req = DrmWaitVblank {
        typ: DRM_VBLANK_RELATIVE,
        sequence: 0,
        val1: 0,
        val2: 0,
    };
    c.ioctl(DRM_IOCTL_WAIT_VBLANK, &mut req)
        .expect("WAIT_VBLANK");
}

/// Read the CRTC and connector ids `drmModeGetResources` would hand a
/// compositor, in the two passes libdrm makes.
fn topology(c: &Client) -> (Vec<u32>, Vec<u32>) {
    let mut probe = blank_card_res();
    c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe)
        .expect("GETRESOURCES");
    let mut crtcs = alloc::vec![0u32; probe.count_crtcs as usize];
    let mut conns = alloc::vec![0u32; probe.count_connectors as usize];
    let mut fill = blank_card_res();
    fill.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
    fill.connector_id_ptr = conns.as_mut_ptr() as u64;
    fill.count_crtcs = probe.count_crtcs;
    fill.count_connectors = probe.count_connectors;
    c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut fill)
        .expect("GETRESOURCES fill");
    (crtcs, conns)
}

/// `DRM_CLIENT_CAP_UNIVERSAL_PLANES` on or off for this client.
fn universal_planes(c: &Client, on: bool) {
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, on as u64];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("SET_CLIENT_CAP UNIVERSAL_PLANES");
}

fn planes(c: &Client) -> Vec<u32> {
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

/// The frame goes to the driver and the CPU never touches the scanout. If
/// the software blit ran too, every present would pay for a full-frame copy
/// over PCIe that the display engine had already made unnecessary -- which
/// is the whole reason the hardware path exists.
#[test]
fn a_driver_that_owns_scanout_gets_the_frame_and_the_cpu_does_not_blit() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_ABCD);
    let fb = c.addfb2(&buf);

    // ADDFB2 asked the driver for its own framebuffer object, with the
    // geometry the client asked for.
    let created = gpu.created_fbs();
    assert_eq!(created.len(), 1, "the driver was not asked for an fb");
    assert_eq!(created[0].gem_handle, buf.handle);
    assert_eq!((created[0].width, created[0].height), (64, 16));
    assert_eq!(created[0].pitch, buf.pitch);
    let driver_fb = created[0].driver_fb_id;
    assert_ne!(driver_fb, fb, "the two namespaces must not coincide");

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x1234).expect("flip");

    // The driver was flipped to ITS OWN id, not the core's.
    assert_eq!(gpu.flips(), alloc::vec![driver_fb]);
    // And nothing was copied into the framebuffer.
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
        "the software blit ran even though the driver took the flip"
    );
    // The client still gets its completion.
    drm::flush_pending_flip_completions();
    let mut b = [0u8; 32];
    assert_eq!(c.read_events(&mut b).expect("completion"), 32);
    let ev = parse_events(&b);
    assert_eq!(ev[0].ev_type, FLIP_COMPLETE);
    assert_eq!(ev[0].user_data, 0x1234);

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A driver that refuses the flip must not leave the panel dark: the core
/// falls back to the software blit. This is the failure mode the fallback
/// was written for -- a display engine that will not take the surface -- and
/// it is unreachable without a driver that can say no.
#[test]
fn a_driver_that_refuses_the_flip_falls_back_to_the_software_blit() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
    gpu.refuse_flips();
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_BEEF);
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");

    assert_eq!(
        gpu.flips().len(),
        1,
        "the driver was still offered the flip"
    );
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_BEEF)),
        "the refused flip left the screen unwritten"
    );

    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 32];
    let _ = c.read_events(&mut sink);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The pointer is put back on top of a frame the driver flipped. A driver
/// flip short-circuits the only place the software cursor is composited, so
/// every accepted flip used to land a frame with no pointer in it -- and a
/// cursor move on this path stands down entirely, so nothing else would ever
/// draw it again.
#[test]
fn the_pointer_is_put_back_on_top_of_a_frame_the_driver_flipped() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_1111);
    let fb = c.addfb2(&buf);
    // A first flip so the CRTC names this framebuffer.
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 32];
    let _ = c.read_events(&mut sink);

    let cur = c.create_dumb(8, 8);
    paint(&cur, 0xFF00_00FF);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

    // Setting the pointer draws nothing on this path: the software repaint
    // stands down, because on real hardware the display engine is scanning
    // out the client's own surface and a CPU composite into the boot
    // framebuffer would be invisible.
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
        "a cursor move repainted while a driver owns scanout"
    );

    screen.repaint(UNTOUCHED);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 2).expect("flip");

    // Now the pointer is there, over the pixels of the frame it belongs to.
    // The patch the composite writes is the pointer's rectangle widened to
    // whole 16-pixel write-combining lines -- [0, 16) here, for a pointer at
    // x = 4 -- and it carries the frame's own pixels in the columns the
    // pointer does not cover, which is what makes it a composite rather than
    // a stamp. Everything outside the patch stays as the driver left it.
    for y in 0..16 {
        for x in 0..64 {
            let in_patch = x < 16 && (2..10).contains(&y);
            let want = if (4..12).contains(&x) && (2..10).contains(&y) {
                0xFF00_00FF
            } else if in_patch {
                0x0000_1111
            } else {
                UNTOUCHED
            };
            assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
        }
    }
    assert_eq!(gpu.flips().len(), 2);

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    drm::flush_pending_flip_completions();
    let _ = c.read_events(&mut sink);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// Only a hardware-KMS driver's topology is exposed once one exists. Mixing
/// a non-KMS driver's CRTCs in alongside produces a topology with two CRTCs
/// sharing one synthetic encoder, and wlroots answers that with "Failed to
/// create DRM backend" -- no desktop at all.
#[test]
fn a_non_kms_drivers_topology_is_not_mixed_in_with_a_kms_one() {
    let screen = kms_emu::attach(64, 16);
    let _virtio = screen.attach_gpu(EmuGpu::new("emu-virtio").with_ids(50, 51, 52));
    let _nvidia = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    universal_planes(&c, true);

    let (crtcs, conns) = topology(&c);
    assert_eq!(crtcs, alloc::vec![60], "the non-KMS CRTC was exposed too");
    assert_eq!(conns, alloc::vec![61]);
    assert_eq!(planes(&c), alloc::vec![62]);
}

/// `drm_mode_getplane_res` lists only overlay planes until the client
/// sets `DRM_CLIENT_CAP_UNIVERSAL_PLANES` (or ATOMIC, which implies it):
/// a legacy client was written when the primary and the cursor were not
/// planes, and would drive the scanout plane as an overlay. The cap was
/// accepted and forgotten, and the list was the same for everyone. The
/// flag is per open file, and the list is filled as far as the caller's
/// buffer goes, with the full count reported.
#[test]
fn only_a_client_that_asked_for_universal_planes_is_told_about_the_primaries() {
    let screen = kms_emu::attach(64, 16);
    let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0").with_ids(60, 61, 62));
    let _second = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-1").with_ids(70, 71, 72));
    let c = Client::open(0);

    assert_eq!(
        planes(&c),
        Vec::<u32>::new(),
        "a legacy client was handed a primary plane"
    );
    universal_planes(&c, true);
    let mut all = planes(&c);
    all.sort_unstable();
    assert_eq!(all, alloc::vec![62, 72]);
    universal_planes(&c, false);
    assert_eq!(planes(&c), Vec::<u32>::new(), "the cap can be taken back");

    // Per file: what one client asked for does not change another's list.
    universal_planes(&c, true);
    let legacy = Client::open(0);
    assert_eq!(planes(&legacy), Vec::<u32>::new());

    // Room for one of the two: that one is filled, and the count says two.
    let mut one = [0u32; 1];
    let mut fill = DrmModeGetPlaneRes {
        plane_id_ptr: one.as_mut_ptr() as u64,
        count_planes: 1,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut fill)
        .expect("GETPLANERESOURCES with room for one");
    assert_eq!(fill.count_planes, 2);
    assert!(
        all.contains(&one[0]),
        "the slot the caller had was left empty"
    );
}

/// `drm_setclientcap` stores the ATOMIC value in `universal_planes` too:
/// an atomic client sees the primary plane without asking for universal
/// planes by name, and giving atomic back takes the planes with it.
#[test]
fn atomic_carries_universal_planes_with_it() {
    let (_screen, c) = super::out_fence_tests::atomic_client(64, 16);
    assert_eq!(planes(&c), alloc::vec![drm::SYNTH_PLANE_ID]);
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_ATOMIC, 0];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("SET_CLIENT_CAP ATOMIC off");
    assert_eq!(
        planes(&c),
        Vec::<u32>::new(),
        "atomic off, planes still listed"
    );
}

/// Two GPUs of the same model return the SAME synthetic ids, and a topology
/// that repeats an id makes wlroots create two outputs with identical
/// resource ids -- which ends in 0x0 dumb-buffer allocations and EINVAL.
/// This is the dual-card case, so it is the one that has to hold.
#[test]
fn two_gpus_reporting_the_same_ids_are_each_reported_once() {
    let screen = kms_emu::attach(64, 16);
    let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0").with_ids(60, 61, 62));
    let _second = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-1").with_ids(60, 61, 62));
    let c = Client::open(0);

    let (crtcs, conns) = topology(&c);
    assert_eq!(
        crtcs,
        alloc::vec![60],
        "a duplicate CRTC id reached userspace"
    );
    assert_eq!(
        conns,
        alloc::vec![61],
        "a duplicate connector id reached userspace"
    );
}

/// `GETCRTC` reports the framebuffer id in the DRM CORE's namespace, even
/// though the driver answers with its own. Handing a client a driver-private
/// id would make its next `RMFB` or `GETFB` name a framebuffer that does not
/// exist -- and the ids look alike, so nothing would say so.
#[test]
fn getcrtc_reports_the_core_framebuffer_id_not_the_drivers() {
    let screen = kms_emu::attach(32, 8);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    paint(&buf, 0x0000_2222);
    let fb = c.addfb2(&buf);
    let driver_fb = gpu.created_fbs()[0].driver_fb_id;

    set_crtc(&c, 60, fb, 32, 8);

    let reported = get_crtc_fb(&c, 60);
    assert_eq!(reported, fb, "GETCRTC did not report the core's fb id");
    assert_ne!(reported, driver_fb, "the driver's private id leaked out");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `WAIT_VBLANK` reaches a driver that really has hardware vblank.
#[test]
fn wait_vblank_reaches_a_driver_that_owns_scanout() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
    let c = Client::open(0);

    wait_vblank(&c);

    assert_eq!(
        gpu.vblank_waits(),
        1,
        "the driver was not asked for a vblank"
    );
}

/// And never reaches one without it, in either configuration that can
/// arise. A driver with no hardware KMS implements `wait_vblank` as a busy
/// 16.7 ms spin, so calling it per `WAIT_VBLANK` starves a cooperative async
/// runtime and the whole system looks frozen; the synthetic timer paces the
/// software path instead. Two guards stand between the ioctl and that spin,
/// and they cover different cases: with an output attached the software-KMS
/// check stops it, and with no output at all (a VirtIO-only guest) only the
/// driver's own `has_hardware_kms()` does.
#[test]
fn wait_vblank_never_reaches_a_driver_that_does_not_own_scanout() {
    // No output: `software_kms_active()` is false, so the per-driver check
    // is the only thing left.
    {
        let headless = kms_emu::headless();
        let gpu = headless.attach_gpu(EmuGpu::new("emu-virtio"));
        wait_vblank(&Client::open(0));
        assert_eq!(
            gpu.vblank_waits(),
            0,
            "a 16.7 ms driver spin was entered with no output attached"
        );
    }
    // With an output, the software path owns the pacing.
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::new("emu-virtio"));
    wait_vblank(&Client::open(0));
    assert_eq!(
        gpu.vblank_waits(),
        0,
        "a 16.7 ms driver spin was entered while software KMS drives the output"
    );
}

/// Under pure software KMS the driver is NOT asked to make a framebuffer of
/// its own. It has no destroy path here, so one per ADDFB2 is a leak for
/// every frame a compositor ever allocates -- and nothing would ever use it,
/// because the software path does the copy itself.
#[test]
fn a_driver_that_does_not_own_scanout_is_not_asked_for_framebuffers() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::new("emu-virtio"));
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_3333);
    let fb = c.addfb2(&buf);

    assert!(
        gpu.created_fbs().is_empty(),
        "a driver framebuffer was created with nothing to use it"
    );
    // And the software path still put the frame on the screen.
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
    assert!(
        gpu.flips().is_empty(),
        "a non-KMS driver was offered a flip"
    );
    assert_eq!(screen.pixel(0, 0), 0x0000_3333);

    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 32];
    let _ = c.read_events(&mut sink);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `DRM_MODE_CURSOR` with the MOVE bit alone, which is what a compositor
/// sends on every pointer motion.
fn move_cursor(c: &Client, crtc_id: u32, x: i32, y: i32) {
    let mut cur = ModeCursor {
        flags: 0x02, // MOVE
        crtc_id,
        x,
        y,
        width: 0,
        height: 0,
        handle: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur)
        .expect("CURSOR MOVE");
}

/// Paint a dumb buffer so every word says which word it is. The cursor
/// bitmap is read tightly packed at `w * h` words while the buffer's own
/// pitch is rounded up to 64 bytes, so a bitmap taken by stride instead of
/// by row would hand the plane the wrong words -- and a flat fill could not
/// tell the two apart.
fn paint_indexed(buf: &DrmModeCreateDumb, base: u32) {
    let (pa, size) = drm::resolve_gem_backing(buf.handle).expect("a dumb buffer must have backing");
    let va = phys_to_virt(pa as usize);
    // SAFETY: `size` bytes of contiguous physical memory owned by this
    // buffer, identity-mapped into the kernel window at `va`.
    let px = unsafe { core::slice::from_raw_parts_mut(va as *mut u32, size / 4) };
    for (i, p) in px.iter_mut().enumerate() {
        *p = base | i as u32;
    }
}

/// Wipe the scanout, flip `fb`, and report the pixel under the pointer's
/// top-left corner. `UNTOUCHED` means the CPU composited nothing there,
/// which is what must happen once the display engine owns the pointer -- a
/// hardware plane and a software composite both drawing leaves two pointers
/// on screen, the CPU one a frame behind.
fn pointer_pixel_after_a_flip(screen: &kms_emu::Screen, c: &Client, fb: u32, seq: u64) -> u32 {
    screen.repaint(UNTOUCHED);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, seq).expect("flip");
    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 32];
    let _ = c.read_events(&mut sink);
    screen.pixel(4, 2)
}

/// With `nvidia.hwcursor` on and a plane that takes the image, the display
/// engine owns the pointer: it gets the bitmap and every motion, and the CPU
/// never composites again. That last half is the point -- the hardware plane
/// and the software compositor drawing the same pointer leaves two of them
/// on screen, the CPU one smearing a frame behind.
#[test]
fn the_display_engine_cursor_plane_takes_the_pointer_when_the_driver_accepts_it() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_cursor_plane());
    drm::set_hw_cursor_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_1111);
    let fb = c.addfb2(&buf);
    // SETCRTC, not just a flip: it is what makes the CRTC name this
    // framebuffer, and the software compositor has nothing to repaint from
    // until it does. Without it the stand-downs below would hold for the
    // wrong reason.
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    screen.repaint(UNTOUCHED);

    // 8 wide by 4 high, deliberately not square and not a multiple of the
    // buffer's own 16-pixel stride: the bitmap is 32 consecutive words, so a
    // plane fed by stride, or given the dimensions the other way round, gets
    // caught here rather than drawing a garbled pointer on real hardware.
    let cur = c.create_dumb(8, 4);
    paint_indexed(&cur, 0xFF00_0000);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 4, 4, 2);

    let images = gpu.cursor_images();
    assert_eq!(images.len(), 1, "the plane was not offered the image");
    assert_eq!((images[0].width, images[0].height), (8, 4));
    let want: Vec<u32> = (0..32).map(|i| 0xFF00_0000 | i).collect();
    assert_eq!(images[0].argb, want, "the plane got the wrong words");

    // And it was landed on the pointer's position. Two moves: taking the
    // image puts the plane where the pointer already is (without that, a
    // client that sets an image and never moves again leaves the pointer
    // wherever the plane happened to be), then the MOVE half of the same
    // ioctl carries it to (4, 2).
    assert_eq!(gpu.cursor_moves(), alloc::vec![(0, 0), (4, 2)]);

    // Nothing was drawn by the CPU, either by the cursor ioctl...
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
        "the CPU composited a pointer the display engine owns"
    );
    // ...or by the frame that follows it.
    assert_eq!(
        pointer_pixel_after_a_flip(&screen, &c, fb, 2),
        UNTOUCHED,
        "a driver flip put a second, software pointer on the screen"
    );
    // A motion is one register write in the driver and nothing else.
    screen.repaint(UNTOUCHED);
    move_cursor(&c, drm::SYNTH_CRTC_ID, 9, 3);
    assert_eq!(gpu.cursor_moves().last(), Some(&(9, 3)));
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED)),
        "a pointer motion repainted while the display engine owns the plane"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A driver whose plane will not take the image must leave the software
/// pointer in charge. The upload goes through the RM gate and can fail on a
/// real card (no cursor surface on the head, a format the engine refuses);
/// standing down on the CPU side anyway is a desktop with no pointer at all.
#[test]
fn a_driver_that_refuses_the_cursor_image_leaves_the_software_pointer_in_charge() {
    let screen = kms_emu::attach(64, 16);
    // Hardware KMS, but no cursor plane to give.
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu"));
    drm::set_hw_cursor_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_1111);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    let cur = c.create_dumb(8, 8);
    paint(&cur, 0xFF00_00FF);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

    // The image was offered and refused, so the plane was never moved.
    assert_eq!(gpu.cursor_images().len(), 1, "the plane was not offered");
    assert!(
        gpu.cursor_moves().is_empty(),
        "a refused plane was moved anyway"
    );
    // And the CPU is drawing the pointer again.
    assert_eq!(
        pointer_pixel_after_a_flip(&screen, &c, fb, 2),
        0xFF00_00FF,
        "the pointer is drawn by nobody"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// Hiding the pointer switches the hardware plane off. The software path
/// hides by simply not drawing, so a plane that is never told stays
/// composited by the display engine -- the pointer sticks on screen after
/// the compositor has hidden it (fullscreen video, a game grabbing it) and
/// nothing userspace does can take it away.
#[test]
fn hiding_the_pointer_switches_the_hardware_plane_off() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_cursor_plane());
    drm::set_hw_cursor_enabled(true);
    let c = Client::open(0);

    let cur = c.create_dumb(8, 8);
    paint(&cur, 0xFF00_00FF);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);
    assert_eq!(gpu.cursor_images().len(), 1);
    assert_eq!(gpu.cursor_hides(), 0, "the plane was hidden while in use");

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    assert_eq!(gpu.cursor_hides(), 1, "the plane was left switched on");

    // And a second hide does not go back to the driver: there is nothing on
    // the plane to switch off, and this runs per hidden frame.
    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    assert_eq!(gpu.cursor_hides(), 1);

    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
}

/// A cursor plane without hardware KMS: the display engine composites the
/// pointer while the CPU still blits the frame. That is `nvidia.hwcursor`
/// without `nvidia.hwflip`, the configuration the flag was added for, and it
/// is the only one where the software compositor is running AND has to leave
/// the pointer alone -- draw it anyway and there are two pointers on screen,
/// the CPU one lagging a frame behind the plane.
#[test]
fn the_cursor_plane_can_own_the_pointer_while_the_cpu_still_blits_the_frame() {
    let screen = kms_emu::attach(64, 16);
    let gpu = screen.attach_gpu(EmuGpu::new("emu-gpu").with_cursor_plane());
    drm::set_hw_cursor_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_1111);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    // The CPU really is driving the scanout here.
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
        "the software blit did not run"
    );

    let cur = c.create_dumb(8, 8);
    paint(&cur, 0xFF00_00FF);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

    assert_eq!(gpu.cursor_images().len(), 1, "the plane was not offered");
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
        "the CPU composited a pointer the plane already owns"
    );

    // A motion goes to the plane and repaints nothing.
    move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 6);
    assert_eq!(gpu.cursor_moves().last(), Some(&(40, 6)));
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
        "a pointer motion repainted while the plane owns the pointer"
    );

    // And the frame after it is still blitted by the CPU, pointer-free.
    screen.repaint(UNTOUCHED);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 32];
    let _ = c.read_events(&mut sink);
    assert!(
        (0..16).all(|y| (0..64).all(|x| screen.pixel(x, y) == 0x0000_1111)),
        "the frame was not blitted, or carried a software pointer"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The plane is not offered the image unless the flag is on. The
/// display-engine cursor is opt-in (`nvidia.hwcursor`) precisely because it
/// is the half of the bring-up that is not trusted yet, so a driver that
/// implements it must not start owning the pointer on a default boot.
#[test]
fn the_plane_is_not_offered_the_image_unless_the_flag_is_on() {
    let screen = kms_emu::attach(64, 16);
    // A plane that WOULD take it, which is what makes the flag the only
    // thing standing between this boot and the hardware path.
    let gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_cursor_plane());
    drm::set_hw_cursor_enabled(false);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, 0x0000_1111);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    let cur = c.create_dumb(8, 8);
    paint(&cur, 0xFF00_00FF);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

    assert!(
        gpu.cursor_images().is_empty(),
        "the plane was offered the image with the flag off"
    );
    assert!(gpu.cursor_moves().is_empty());
    assert_eq!(
        pointer_pixel_after_a_flip(&screen, &c, fb, 2),
        0xFF00_00FF,
        "the pointer is drawn by nobody"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    assert_eq!(
        gpu.cursor_hides(),
        0,
        "a plane that never took the pointer was told to hide it"
    );
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}
// ---- the EDID a driver reports, and whether the core serves it ----

/// A whole block with a correct header and checksum, as a monitor sends one.
fn real_edid() -> [u8; 128] {
    let mut b = [0u8; 128];
    b[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    b[18] = 1;
    b[19] = 4;
    b[21] = 60;
    b[22] = 34;
    let sum = b[..127].iter().fold(0u8, |s, x| s.wrapping_add(*x));
    b[127] = sum.wrapping_neg();
    b
}

/// The defect this pair of tests exists for. `NvidiaGpu::get_connector_edid`
/// used to build a block out of the 32 bytes the RM gives it by padding the
/// rest with zeros, and the DRM core served whatever a driver reported after
/// a length check alone. Those bytes cannot pass a checksum, so wlroots --
/// through libdisplay-info, which checks -- threw the whole block away and
/// the output lost the make, the model and the size that WERE in the 32 real
/// bytes. Worse, the kernel refused the same block for its own mode (every
/// decoder gates on `block_valid`), so the EDID it handed out and the mode it
/// advertised could disagree about the same monitor.
#[test]
fn a_driver_that_reports_something_that_is_not_an_edid_has_it_refused() {
    let screen = kms_emu::attach(64, 16);
    let mut padded = real_edid();
    // Keep the header, drop the checksum: exactly the shape zero-padding
    // produces, and exactly the shape a length check lets through.
    padded[127] = padded[127].wrapping_add(1);
    let gpu = screen.attach_gpu(EmuGpu::new("emu-edid").with_edid(padded));

    assert_eq!(
        drm::get_connector_edid(41),
        None,
        "a block that fails its own checksum was served as a monitor's identity"
    );
    drop(gpu);
}

/// And the other direction, so the refusal is not simply "always none":
/// a driver reporting a real block still has it served, byte for byte.
#[test]
fn a_driver_that_reports_a_real_edid_has_it_served_unchanged() {
    let screen = kms_emu::attach(64, 16);
    let good = real_edid();
    let gpu = screen.attach_gpu(EmuGpu::new("emu-edid").with_edid(good));

    assert_eq!(drm::get_connector_edid(41), Some(good));
    drop(gpu);
}

/// The 32 bytes the RM actually gives, completed the way the driver now
/// completes them, go through. The two halves of the fix have to agree: a
/// core that refuses without a driver that repairs would just lose the
/// monitor's identity instead of keeping it.
#[test]
fn the_thirty_two_byte_head_the_rm_gives_is_served_once_completed() {
    let screen = kms_emu::attach(64, 16);
    let head = &real_edid()[..32];
    let completed =
        zcore_drivers::display::edid::finish_partial_block(head).expect("a real head completes");
    let gpu = screen.attach_gpu(EmuGpu::new("emu-edid").with_edid(completed));

    assert_eq!(drm::get_connector_edid(41), Some(completed));
    drop(gpu);
}

/// A zeroed ioctl argument, for the arms whose structs have no
/// `Default`. All of them are `repr(C)` integers, for which zero is a
/// value.
fn zeroed<T: Copy>() -> T {
    // SAFETY: every struct this is used for is plain integers.
    unsafe { core::mem::zeroed() }
}

/// Every mode-object lookup Linux answers ENOENT for an id that does not
/// exist (`drm_mode_object_find` and its typed wrappers), and the one
/// encoder is the only encoder. Here GETCRTC, GETPLANE, GETCONNECTOR,
/// GETPROPBLOB and CLOSEFB said EINVAL; GETENCODER answered any id with
/// the synthetic encoder and rewrote the id; OBJ_GETPROPERTIES gave an
/// unknown id the encoder's empty list; and SETPLANE, CURSOR, the gamma
/// pair, OBJ_SETPROPERTY and SETPROPERTY never looked the object up at
/// all and reported success. The ids the client really has keep
/// working.
#[test]
fn unknown_mode_object_ids_answer_enoent_like_linux() {
    let screen = kms_emu::attach(32, 8);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    const BOGUS: u32 = 4242;
    let enoent = Err(FsError::EntryNotFound);

    // GETCRTC / GETPLANE / GETCONNECTOR: the typed lookups.
    let mut crtc: DrmModeGetCrtc = zeroed();
    crtc.crtc_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc), enoent);
    crtc.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc), Ok(0));

    let mut plane: DrmModeGetPlane = zeroed();
    plane.plane_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut plane), enoent);
    plane.plane_id = 62;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut plane), Ok(0));

    let mut conn: DrmModeGetConnector = zeroed();
    conn.connector_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn), enoent);
    conn.connector_id = 61;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn), Ok(0));

    // GETENCODER: one encoder, and an unknown id is not renamed to it.
    let mut enc: DrmModeGetEncoder = zeroed();
    enc.encoder_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc), enoent);
    assert_eq!(
        enc.encoder_id, BOGUS,
        "the id the client asked about was rewritten"
    );
    enc.encoder_id = drm::SYNTH_ENCODER_ID;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc), Ok(0));
    assert_eq!(enc.encoder_id, drm::SYNTH_ENCODER_ID);

    // OBJ_GETPROPERTIES: an unknown id is not "an object with no
    // properties"; the encoder still is.
    let mut props: DrmModeObjGetProperties = zeroed();
    props.obj_id = BOGUS;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut props),
        enoent
    );
    props.obj_id = drm::SYNTH_ENCODER_ID;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut props), Ok(0));
    assert_eq!(props.count_props, 0, "the encoder carries no properties");
    props.obj_id = 61;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut props), Ok(0));
    assert!(props.count_props > 0, "the connector does");

    // GETPROPBLOB: neither a blob id nor an EDID id of a connector that
    // does not exist.
    let mut blob: DrmModeGetBlob = zeroed();
    blob.blob_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut blob), enoent);
    blob.blob_id = edid_blob_id(BOGUS);
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETPROPBLOB, &mut blob), enoent);

    // CLOSEFB, like RMFB.
    let mut fb_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_CLOSEFB, &mut fb_id), enoent);

    // SETPLANE looks the plane up first; disabling the real one is fine.
    let mut set_plane: DrmModeSetPlane = zeroed();
    set_plane.plane_id = BOGUS;
    set_plane.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), enoent);
    set_plane.plane_id = 62;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), Ok(0));

    // CURSOR: "Unknown CRTC ID".
    let mut cur = ModeCursor {
        flags: 0x02, // MOVE
        crtc_id: BOGUS,
        x: 1,
        y: 1,
        width: 0,
        height: 0,
        handle: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur), enoent);
    cur.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur), Ok(0));

    // GETGAMMA / SETGAMMA: `struct drm_mode_crtc_lut` starts with the
    // CRTC id.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CrtcLut {
        crtc_id: u32,
        gamma_size: u32,
        red: u64,
        green: u64,
        blue: u64,
    }
    let mut lut: CrtcLut = zeroed();
    lut.crtc_id = BOGUS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETGAMMA, &mut lut), enoent);
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETGAMMA, &mut lut), enoent);
    lut.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETGAMMA, &mut lut), Ok(0));
    // A CRTC with no gamma store: `drm_crtc_supports_legacy_gamma`.
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_SETGAMMA, &mut lut),
        Err(FsError::NotSupported)
    );

    // OBJ_SETPROPERTY: the object and the property must both exist.
    let mut set = DrmModeObjSetProperty {
        value: DRM_MODE_DPMS_ON,
        prop_id: PROP_DPMS,
        obj_id: BOGUS,
        obj_type: 0,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut set), enoent);
    set.obj_id = 61;
    set.prop_id = BOGUS;
    // A property the object does not carry: `drm_mode_obj_find_prop_id`
    // misses and the ioctl's EINVAL stands (it was ENOENT here).
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut set),
        Err(FsError::InvalidParam)
    );
    set.prop_id = PROP_DPMS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut set), Ok(0));

    // SETPROPERTY: `struct drm_mode_connector_set_property`.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ConnectorSetProperty {
        value: u64,
        prop_id: u32,
        connector_id: u32,
    }
    let mut cset = ConnectorSetProperty {
        value: DRM_MODE_DPMS_ON,
        prop_id: PROP_DPMS,
        connector_id: BOGUS,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut cset), enoent);
    cset.connector_id = 61;
    cset.prop_id = BOGUS;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut cset),
        Err(FsError::InvalidParam)
    );
    cset.prop_id = PROP_DPMS;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut cset), Ok(0));
}

/// `drm_mode_setcrtc` finds the CRTC first and every connector it is
/// handed (ENOENT), and refuses a connector list with no mode or no fb
/// to set, or longer than the card's connectors (EINVAL);
/// `drm_mode_page_flip_ioctl` and `drm_mode_setplane` find the CRTC
/// too. None of the three read the CRTC id, and SETCRTC never read its
/// connector list: a modeset or a flip aimed at a CRTC the card does not
/// have landed on the one it has. The real ids keep working.
#[test]
fn setcrtc_page_flip_and_setplane_look_the_crtc_and_the_connectors_up() {
    let screen = kms_emu::attach(32, 8);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    const BOGUS: u32 = 4242;
    let enoent = Err(FsError::EntryNotFound);
    let buf = c.create_dumb(32, 8);
    paint(&buf, 0x0000_3333);
    let fb = c.addfb2(&buf);
    // What the CRTC shows before this test touches it (the core's
    // `crtc_fb` is process-wide, so it may carry a neighbour's id).
    let before = get_crtc_fb(&c, 60);
    assert_ne!(before, fb);

    let setcrtc = |crtc_id: u32, connectors: &[u32], mode_valid: u32, fb_id: u32| {
        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: connectors.as_ptr() as u64,
            count_connectors: connectors.len() as u32,
            crtc_id,
            fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid,
            mode: make_modeinfo(32, 8),
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
    };
    assert_eq!(
        setcrtc(BOGUS, &[61], 1, fb),
        enoent,
        "a CRTC the card does not have"
    );
    assert_eq!(
        setcrtc(60, &[BOGUS], 1, fb),
        enoent,
        "a connector it does not have"
    );
    assert_eq!(
        setcrtc(60, &[61, BOGUS], 1, fb),
        Err(FsError::InvalidParam),
        "more connectors than the card has"
    );
    assert_eq!(
        setcrtc(60, &[61], 0, fb),
        Err(FsError::InvalidParam),
        "connectors but no mode"
    );
    assert_eq!(
        setcrtc(60, &[61], 1, 0),
        enoent,
        "a mode with fb 0: the fb lookup comes before the connector rules"
    );
    assert_eq!(
        get_crtc_fb(&c, 60),
        before,
        "a refused modeset presented anyway"
    );
    assert_eq!(
        setcrtc(60, &[61], 1, fb),
        Ok(0),
        "the real CRTC and connector"
    );
    assert_eq!(get_crtc_fb(&c, 60), fb);

    let mut flip = DrmModeCrtcPageFlip {
        crtc_id: BOGUS,
        fb_id: fb,
        flags: 0x01, // DRM_MODE_PAGE_FLIP_EVENT
        reserved: 0,
        user_data: 7,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_PAGE_FLIP, &mut flip), enoent);
    let mut events = [0u8; 256];
    assert!(
        matches!(c.read_events(&mut events), Err(_) | Ok(0)),
        "a refused flip queued a completion"
    );

    let mut set_plane: DrmModeSetPlane = zeroed();
    set_plane.plane_id = 62;
    set_plane.crtc_id = BOGUS;
    set_plane.fb_id = fb;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), enoent);
    set_plane.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut set_plane), Ok(0));

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `__setplane_check`: the plane has to be usable on the CRTC named
/// (`possible_crtcs`, EINVAL) and the source rectangle, in 16.16, has to
/// lie inside the fb (ENOSPC). Neither was read: a plane went onto a CRTC
/// it does not reach, and a crop past the fb's edge was accepted, so a
/// client believed it was showing a crop the scanout never made.
#[test]
fn setplane_wants_a_crtc_the_plane_reaches_and_a_source_inside_the_fb() {
    let screen = kms_emu::attach(32, 8);
    let _first = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-0").with_ids(60, 61, 62));
    let _second = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu-1").with_ids(70, 71, 72));
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    paint(&buf, 0x0000_7777);
    let fb = c.addfb2(&buf);
    // The resource list decides the CRTC indices; the plane of each
    // card is its CRTC id plus two.
    let (crtcs, _) = topology(&c);
    assert_eq!(crtcs.len(), 2);
    let (front, back) = (crtcs[0], crtcs[1]);
    let plane_of = |crtc: u32| crtc + 2;

    let set_plane = |plane_id: u32, crtc_id: u32, src: [u32; 4]| {
        let mut req: DrmModeSetPlane = zeroed();
        req.plane_id = plane_id;
        req.crtc_id = crtc_id;
        req.fb_id = fb;
        req.crtc_w = 32;
        req.crtc_h = 8;
        req.src_x = src[0] << 16;
        req.src_y = src[1] << 16;
        req.src_w = src[2] << 16;
        req.src_h = src[3] << 16;
        c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req)
    };
    let enospc = Err(FsError::NoDeviceSpace);

    // Every plane advertises `possible_crtcs = 1`, CRTC index 0: the
    // plane of the card listed second is not usable on its own CRTC, by
    // what the client was told, and the first's is.
    assert_eq!(
        set_plane(plane_of(back), back, [0, 0, 32, 8]),
        Err(FsError::InvalidParam),
        "a CRTC the plane's mask does not reach"
    );
    assert_eq!(set_plane(plane_of(front), front, [0, 0, 32, 8]), Ok(0));

    assert_eq!(
        set_plane(plane_of(front), front, [0, 0, 33, 8]),
        enospc,
        "wider than the fb"
    );
    assert_eq!(
        set_plane(plane_of(front), front, [0, 0, 32, 9]),
        enospc,
        "taller than the fb"
    );
    assert_eq!(
        set_plane(plane_of(front), front, [1, 0, 32, 8]),
        enospc,
        "x pushes it past the edge"
    );
    assert_eq!(
        set_plane(plane_of(front), front, [0, 1, 32, 8]),
        enospc,
        "y pushes it past the bottom"
    );
    assert_eq!(
        set_plane(plane_of(front), front, [16, 4, 16, 4]),
        Ok(0),
        "a crop that fits"
    );
    assert_eq!(
        set_plane(plane_of(front), front, [0, 0, 0, 0]),
        Ok(0),
        "no source rectangle at all"
    );

    // A fractional source edge counts: 31.5 wide from x = 0.75 is past 32.
    let mut req: DrmModeSetPlane = zeroed();
    req.plane_id = plane_of(front);
    req.crtc_id = front;
    req.fb_id = fb;
    req.src_x = 3 << 14;
    req.src_w = (31 << 16) | (1 << 15);
    req.src_h = 8 << 16;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPLANE, &mut req), enospc);

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_page_flip_ioctl`, once the CRTC and the fb are found: the
/// CRTC's mode has to fit in the new fb (`drm_crtc_check_viewport`,
/// ENOSPC), and "page flip is not allowed to change frame buffer format"
/// (EINVAL). Neither was read: a flip onto an fb narrower than the mode,
/// or of another format, was scanned out as the frame the CRTC was set
/// up with, and the client was told it had flipped.
#[test]
fn page_flip_wants_an_fb_that_holds_the_mode_and_keeps_the_format() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let (crtcs, _) = topology(&c);
    let crtc = crtcs[0];
    let xr24 = c.create_dumb(32, 8);
    paint(&xr24, 0x0000_5555);
    let fb_xr24 = c.addfb2(&xr24);
    set_crtc(&c, crtc, fb_xr24, 32, 8);
    assert_eq!(get_crtc_fb(&c, crtc), fb_xr24);

    let addfb2 = |buf: &DrmModeCreateDumb, pixel_format: u32| {
        let mut cmd = DrmModeFbCmd2 {
            fb_id: 0,
            width: buf.width,
            height: buf.height,
            pixel_format,
            flags: 0,
            handles: [buf.handle, 0, 0, 0],
            pitches: [buf.pitch, 0, 0, 0],
            offsets: [0; 4],
            modifier: [0; 4],
        };
        c.ioctl(DRM_IOCTL_MODE_ADDFB2, &mut cmd).expect("ADDFB2");
        cmd.fb_id
    };
    let narrow = c.create_dumb(16, 8);
    let fb_narrow = addfb2(&narrow, drm::DRM_FORMAT_XRGB8888);
    let short = c.create_dumb(32, 4);
    let fb_short = addfb2(&short, drm::DRM_FORMAT_XRGB8888);
    let ar24 = c.create_dumb(32, 8);
    paint(&ar24, 0xff00_6666);
    let fb_ar24 = addfb2(&ar24, drm::DRM_FORMAT_ARGB8888);
    let ar24_too = c.create_dumb(32, 8);
    let fb_ar24_too = addfb2(&ar24_too, drm::DRM_FORMAT_ARGB8888);

    assert_eq!(
        c.page_flip(crtc, fb_narrow, 1),
        Err(FsError::NoDeviceSpace),
        "narrower than the mode"
    );
    assert_eq!(
        c.page_flip(crtc, fb_short, 2),
        Err(FsError::NoDeviceSpace),
        "shorter than the mode"
    );
    assert_eq!(
        c.page_flip(crtc, fb_ar24, 3),
        Err(FsError::InvalidParam),
        "XRGB8888 on the CRTC, ARGB8888 flipped"
    );
    assert_eq!(
        get_crtc_fb(&c, crtc),
        fb_xr24,
        "a refused flip presented anyway"
    );
    let mut events = [0u8; 256];
    assert!(
        matches!(c.read_events(&mut events), Err(_) | Ok(0)),
        "a refused flip queued a completion"
    );

    // A modeset may change the format; a flip may then keep the new one.
    set_crtc(&c, crtc, fb_ar24, 32, 8);
    assert_eq!(
        c.page_flip(crtc, fb_xr24, 4),
        Err(FsError::InvalidParam),
        "ARGB8888 on the CRTC, XRGB8888 flipped"
    );
    assert_eq!(c.page_flip(crtc, fb_ar24_too, 5), Ok(0));
    drm::flush_pending_flip_completions();
    assert_eq!(get_crtc_fb(&c, crtc), fb_ar24_too);
    let _ = c.read_events(&mut events);

    for fb in [fb_xr24, fb_narrow, fb_short, fb_ar24, fb_ar24_too] {
        c.rmfb(fb).expect("RMFB");
    }
    for buf in [&xr24, &narrow, &short, &ar24, &ar24_too] {
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }
}

/// `drm_mode_obj_set_property_ioctl`, and SETPROPERTY through it: the
/// object of the type named (ENOENT), a property the object carries
/// (EINVAL; an encoder carries none), then `drm_property_change_valid_get`:
/// not immutable, and a value the property's type admits (EINVAL). Any
/// value for any known property on any existing object was "set".
#[test]
fn a_property_write_is_checked_against_the_object_and_the_property() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let (crtcs, conns) = topology(&c);
    let (crtc, conn) = (crtcs[0], conns[0]);
    universal_planes(&c, true);
    let plane = planes(&c)[0];
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let einval = Err(FsError::InvalidParam);
    let enoent = Err(FsError::EntryNotFound);
    let set = |obj_id: u32, obj_type: u32, prop_id: u32, value: u64| {
        let mut req = DrmModeObjSetProperty {
            value,
            prop_id,
            obj_id,
            obj_type,
        };
        c.ioctl(DRM_IOCTL_MODE_OBJ_SETPROPERTY, &mut req)
    };
    // The object, of the type asked for.
    assert_eq!(
        set(crtc, DRM_MODE_OBJECT_CONNECTOR, PROP_ACTIVE, 1),
        enoent,
        "a CRTC is not a connector"
    );
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 1), Ok(0));
    assert_eq!(set(crtc, DRM_MODE_OBJECT_ANY, PROP_ACTIVE, 1), Ok(0));
    // A property the object carries.
    assert_eq!(
        set(crtc, DRM_MODE_OBJECT_CRTC, PROP_DPMS, DRM_MODE_DPMS_ON),
        einval,
        "DPMS is the connector's"
    );
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_TYPE, 1),
        einval,
        "type is the plane's"
    );
    assert_eq!(
        set(drm::SYNTH_ENCODER_ID, DRM_MODE_OBJECT_ENCODER, PROP_DPMS, 0),
        einval,
        "an encoder carries none"
    );
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, 0xdead, 0),
        einval,
        "no such property"
    );
    // Immutable.
    assert_eq!(set(plane, DRM_MODE_OBJECT_PLANE, PROP_TYPE, 1), einval);
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_NON_DESKTOP, 0),
        einval
    );
    // An enum takes one of its listed values.
    assert_eq!(set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_DPMS, 7), einval);
    assert_eq!(
        set(conn, DRM_MODE_OBJECT_CONNECTOR, PROP_LINK_STATUS, 1),
        Ok(0)
    );
    // A range and a signed range, by their bounds.
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_ACTIVE, 2), einval);
    assert_eq!(
        set(
            plane,
            DRM_MODE_OBJECT_PLANE,
            PROP_CRTC_X,
            i32::MIN as i64 as u64
        ),
        Ok(0)
    );
    assert_eq!(
        set(
            plane,
            DRM_MODE_OBJECT_PLANE,
            PROP_CRTC_X,
            i32::MAX as u64 + 1
        ),
        einval
    );
    assert_eq!(
        set(
            plane,
            DRM_MODE_OBJECT_PLANE,
            PROP_CRTC_W,
            i32::MAX as u64 + 1
        ),
        einval
    );
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_SRC_W, u32::MAX as u64),
        Ok(0)
    );
    // An object property: 0, or an object of the property's type.
    assert_eq!(set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, 0), Ok(0));
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, fb as u64),
        Ok(0)
    );
    // (Not "the CRTC's id": framebuffer ids and mode-object ids are
    // separate namespaces here, so fb 1 and CRTC 1 can both exist.)
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_FB_ID, 0xdead_0000),
        einval,
        "no such fb"
    );
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_CRTC_ID, crtc as u64),
        Ok(0)
    );
    assert_eq!(
        set(plane, DRM_MODE_OBJECT_PLANE, PROP_CRTC_ID, conn as u64),
        einval
    );
    assert_eq!(
        set(
            plane,
            DRM_MODE_OBJECT_PLANE,
            PROP_CRTC_ID,
            (1 << 32) | crtc as u64
        ),
        einval,
        "not a 32-bit id, whatever its low word names"
    );
    // A blob property: 0, or an existing blob.
    let bytes = [7u8; 68];
    let mut blob = DrmModeCreateBlob {
        data: bytes.as_ptr() as u64,
        length: bytes.len() as u32,
        blob_id: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_CREATEPROPBLOB, &mut blob)
        .expect("CREATEPROPBLOB");
    assert_eq!(set(crtc, DRM_MODE_OBJECT_CRTC, PROP_MODE_ID, 0), Ok(0));
    assert_eq!(
        set(
            crtc,
            DRM_MODE_OBJECT_CRTC,
            PROP_MODE_ID,
            blob.blob_id as u64
        ),
        Ok(0)
    );
    assert_eq!(
        set(crtc, DRM_MODE_OBJECT_CRTC, PROP_MODE_ID, 0xdead_beef),
        einval
    );
    let mut blob_id = blob.blob_id;
    c.ioctl(DRM_IOCTL_MODE_DESTROYPROPBLOB, &mut blob_id)
        .expect("DESTROYPROPBLOB");

    // SETPROPERTY is the same call with the connector type.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ConnectorSetProperty {
        value: u64,
        prop_id: u32,
        connector_id: u32,
    }
    let cset = |connector_id: u32, prop_id: u32, value: u64| {
        let mut req = ConnectorSetProperty {
            value,
            prop_id,
            connector_id,
        };
        c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut req)
    };
    assert_eq!(
        cset(crtc, PROP_DPMS, DRM_MODE_DPMS_ON),
        enoent,
        "a CRTC is not a connector"
    );
    assert_eq!(cset(conn, PROP_TYPE, 1), einval);
    assert_eq!(cset(conn, PROP_DPMS, 9), einval);
    assert_eq!(cset(conn, PROP_DPMS, 3), Ok(0), "Off");
    assert!(drm::crtc_blanked(), "and the DPMS write still lands");
    assert_eq!(cset(conn, PROP_DPMS, DRM_MODE_DPMS_ON), Ok(0));
    assert!(!drm::crtc_blanked());
}

/// `drm_mode_gamma_{get,set}_ioctl` on a CRTC whose `gamma_size` is 0,
/// which is what GETCRTC reports here: SETGAMMA is ENOSYS
/// (`drm_crtc_supports_legacy_gamma`), GETGAMMA wants the caller's
/// `gamma_size` to be the CRTC's (EINVAL) and then copies that many
/// entries, none. Both answered "done": a 256-entry ramp was "set", and
/// a 256-entry GETGAMMA returned without writing one.
#[test]
fn gamma_ioctls_hold_the_caller_to_a_crtc_with_no_gamma_store() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let (crtcs, _) = topology(&c);
    let crtc = crtcs[0];
    let mut info: DrmModeGetCrtc = zeroed();
    info.crtc_id = crtc;
    c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut info).expect("GETCRTC");
    assert_eq!(info.gamma_size, 0, "no gamma store, as GETCRTC says");

    let mut red = [0x1111u16; 256];
    let mut green = [0x2222u16; 256];
    let mut blue = [0x3333u16; 256];
    let (r, g, b) = (
        red.as_mut_ptr() as u64,
        green.as_mut_ptr() as u64,
        blue.as_mut_ptr() as u64,
    );
    let gamma = |cmd: u32, crtc_id: u32, gamma_size: u32| {
        let mut lut = DrmModeCrtcLut {
            crtc_id,
            gamma_size,
            red: r,
            green: g,
            blue: b,
        };
        c.ioctl(cmd, &mut lut)
    };
    for size in [0u32, 256] {
        assert_eq!(
            gamma(DRM_IOCTL_MODE_SETGAMMA, crtc, size),
            Err(FsError::NotSupported),
            "SETGAMMA with {} entries",
            size
        );
    }
    assert_eq!(
        gamma(DRM_IOCTL_MODE_GETGAMMA, crtc, 256),
        Err(FsError::InvalidParam),
        "not the CRTC's gamma_size"
    );
    assert_eq!(gamma(DRM_IOCTL_MODE_GETGAMMA, crtc, 0), Ok(0));
    assert!(
        red.iter().all(|&v| v == 0x1111)
            && green.iter().all(|&v| v == 0x2222)
            && blue.iter().all(|&v| v == 0x3333),
        "zero entries copied"
    );
    assert_eq!(
        gamma(DRM_IOCTL_MODE_GETGAMMA, 0xdead_0000, 0),
        Err(FsError::EntryNotFound)
    );
}

/// With a mode, `drm_mode_setcrtc` looks the fb up (ENOENT; -1 is the
/// fb already on the CRTC, EINVAL when there is none), refuses a mode
/// `drm_mode_validate_basic` would not have (a zero clock, a zero active
/// area, sync timings out of order) or an aspect-ratio code it does not
/// define (EINVAL), and wants the active area at (x, y) inside the fb
/// (ENOSPC). None of it was read: a 64-wide mode on a 32-wide fb was
/// scanned out, and a clockless mode was paced from a fallback.
#[test]
fn setcrtc_wants_a_mode_that_is_one_and_an_fb_that_holds_it() {
    // The synthetic pipe: its CRTC reports exactly the fb the core has on
    // it, so "nothing on the CRTC" is a state this test can reach.
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let (crtcs, conns) = topology(&c);
    let (crtc, conn) = (crtcs[0], conns[0]);
    let buf = c.create_dumb(32, 8);
    paint(&buf, 0x0000_4444);
    let fb = c.addfb2(&buf);
    let einval = Err(FsError::InvalidParam);
    let enospc = Err(FsError::NoDeviceSpace);

    let setcrtc = |fb_id: u32, x: u32, y: u32, mode: [u8; 68]| {
        let connectors = [conn];
        let mut req = DrmModeGetCrtc {
            set_connectors_ptr: connectors.as_ptr() as u64,
            count_connectors: 1,
            crtc_id: crtc,
            fb_id,
            x,
            y,
            gamma_size: 0,
            mode_valid: 1,
            mode,
        };
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req)
    };
    let good = make_modeinfo(32, 8);
    let put_u16 =
        |m: &mut [u8; 68], at: usize, v: u16| m[at..at + 2].copy_from_slice(&v.to_ne_bytes());

    // The fb: -1 with nothing on the CRTC, and an id that is not one.
    let mut off = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: crtc,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut off)
        .expect("SETCRTC off");
    assert_eq!(get_crtc_fb(&c, crtc), 0);
    assert_eq!(
        setcrtc(u32::MAX, 0, 0, good),
        einval,
        "-1 with no fb on the CRTC"
    );
    assert_eq!(
        setcrtc(4242, 0, 0, good),
        Err(FsError::EntryNotFound),
        "an fb that does not exist"
    );

    // The viewport: the active area at (x, y) has to lie inside the fb.
    assert_eq!(
        setcrtc(fb, 0, 0, make_modeinfo(64, 8)),
        enospc,
        "wider than the fb"
    );
    assert_eq!(
        setcrtc(fb, 0, 0, make_modeinfo(32, 16)),
        enospc,
        "taller than the fb"
    );
    assert_eq!(
        setcrtc(fb, 1, 0, good),
        enospc,
        "x pushes it past the right edge"
    );
    assert_eq!(
        setcrtc(fb, 0, 1, good),
        enospc,
        "y pushes it past the bottom"
    );
    assert_eq!(
        setcrtc(fb, 0x1_0000, 0, good),
        enospc,
        "an x with high bits set is outside any fb"
    );

    // The mode: what `drm_mode_validate_basic` refuses.
    let mut m = good;
    m[0..4].copy_from_slice(&0u32.to_ne_bytes());
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "clock 0");
    let mut m = good;
    put_u16(&mut m, 4, 0);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "hdisplay 0");
    let mut m = good;
    put_u16(&mut m, 6, 31);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "hsync_start before hdisplay");
    let mut m = good;
    put_u16(&mut m, 8, u16::from_ne_bytes([good[6], good[7]]) - 1);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "hsync_end before hsync_start");
    let mut m = good;
    put_u16(&mut m, 10, u16::from_ne_bytes([good[8], good[9]]) - 1);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "htotal before hsync_end");
    let mut m = good;
    put_u16(&mut m, 14, 0);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "vdisplay 0");
    let mut m = good;
    put_u16(&mut m, 16, 7);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "vsync_start before vdisplay");
    let mut m = good;
    put_u16(&mut m, 18, u16::from_ne_bytes([good[16], good[17]]) - 1);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "vsync_end before vsync_start");
    let mut m = good;
    put_u16(&mut m, 20, u16::from_ne_bytes([good[18], good[19]]) - 1);
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "vtotal before vsync_end");
    let mut m = good;
    let flags = u32::from_ne_bytes([good[28], good[29], good[30], good[31]]);
    m[28..32].copy_from_slice(&(flags | (5 << 19)).to_ne_bytes());
    assert_eq!(setcrtc(fb, 0, 0, m), einval, "aspect-ratio code 5");
    assert_eq!(
        get_crtc_fb(&c, crtc),
        0,
        "a refused modeset presented anyway"
    );

    // What passes: the fb that fits, with the last aspect-ratio code the
    // kernel defines, and then -1 for the same fb again.
    let mut m = good;
    m[28..32].copy_from_slice(&(flags | (4 << 19)).to_ne_bytes());
    assert_eq!(setcrtc(fb, 0, 0, m), Ok(0));
    assert_eq!(get_crtc_fb(&c, crtc), fb);
    assert_eq!(
        setcrtc(u32::MAX, 0, 0, good),
        Ok(0),
        "-1 is the fb on the CRTC"
    );
    assert_eq!(get_crtc_fb(&c, crtc), fb);

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// On the hardware-KMS path the driver's `get_crtc` carries its own
/// framebuffer id, in its own namespace (`EMU_DRIVER_FB_BASE` here), and
/// the core only overrides it while a DRM framebuffer is on the CRTC.
/// `drm_mode_getcrtc` reports `fb_id = 0` and `mode_valid = 0` for a CRTC
/// that `SETCRTC` disabled, so the driver's id must not show through
/// once the DRM one is gone.
#[test]
fn a_disabled_hardware_crtc_reports_no_framebuffer_and_no_mode() {
    let screen = kms_emu::attach(32, 8);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let read = || {
        let mut crtc: DrmModeGetCrtc = zeroed();
        crtc.crtc_id = 60;
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
        (crtc.mode_valid, crtc.fb_id)
    };
    set_crtc(&c, 60, fb, 32, 8);
    assert_eq!(read(), (1, fb), "with the framebuffer on the CRTC");

    let mut disable: DrmModeGetCrtc = zeroed();
    disable.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
    assert_eq!(read(), (0, 0), "the driver's own fb id showed through");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The hardware driver's `get_plane` carries its own framebuffer id, in
/// its own namespace (`EMU_DRIVER_FB_BASE`), like its `get_crtc`. The
/// core reports the DRM framebuffer on the plane instead, and 0 with no
/// CRTC once the pipe is disabled; the driver's id must never show.
#[test]
fn the_hardware_primary_plane_never_shows_the_drivers_own_fb_id() {
    let screen = kms_emu::attach(32, 8);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let plane = || {
        let mut res: DrmModeGetPlane = zeroed();
        res.plane_id = 62;
        c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut res)
            .expect("GETPLANE");
        (res.crtc_id, res.fb_id)
    };
    assert_eq!(plane(), (0, 0), "nothing on the plane yet");
    set_crtc(&c, 60, fb, 32, 8);
    assert_eq!(plane(), (60, fb), "the DRM framebuffer, not the driver's");

    let mut disable: DrmModeGetCrtc = zeroed();
    disable.crtc_id = 60;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
    assert_eq!(plane(), (0, 0), "the driver's own fb id showed through");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `CURSOR` with a buffer: Linux wraps the handle in a framebuffer, so a
/// handle the file does not hold is ENOENT and a buffer too small for
/// `width x height` pixels is EINVAL. Both came back as success with the
/// pointer quietly hidden, so a compositor whose cursor upload went
/// wrong was never told. A buffer that fits keeps working.
#[test]
fn a_cursor_needs_a_handle_of_its_own_that_fits_the_image() {
    let screen = kms_emu::attach(32, 8);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    const BOGUS: u32 = 4242;
    let cursor = |handle: u32, w: u32, h: u32| {
        let mut cur = ModeCursor {
            flags: 0x01, // BO
            crtc_id: 60,
            x: 0,
            y: 0,
            width: w,
            height: h,
            handle,
        };
        c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur)
    };
    assert_eq!(cursor(BOGUS, 16, 16), Err(FsError::EntryNotFound));

    // 16 rows of a 64-byte pitch: 1 KiB, room for 16x16 and not 64x64.
    let small = c.create_dumb(16, 16);
    assert_eq!(cursor(small.handle, 64, 64), Err(FsError::InvalidParam));
    assert_eq!(cursor(small.handle, 16, 17), Err(FsError::InvalidParam));
    assert_eq!(cursor(small.handle, 16, 16), Ok(0));
    // Hiding the cursor names no buffer and needs none.
    assert_eq!(cursor(0, 0, 0), Ok(0));

    c.destroy_dumb(small.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_getresources` and `drm_mode_object_get_properties` (which
/// GETCONNECTOR uses for its property list too) write as many entries
/// as the caller made room for and report the full length, so a short
/// array gets a prefix and the count to allocate; and
/// `drm_mode_obj_get_properties_ioctl` finds the object by id and type,
/// so a connector asked about as a plane is ENOENT. Here the lists
/// were copied all or nothing, and any type matched any id.
#[test]
fn lists_are_filled_as_far_as_the_caller_made_room_and_objects_match_their_type() {
    let screen = kms_emu::attach(32, 8);
    let _gpu = screen.attach_gpu(EmuGpu::hardware_kms("emu-gpu").with_ids(60, 61, 62));
    let c = Client::open(0);
    const SENTINEL: u32 = 0xfeed_beef;
    let enoent = Err(FsError::EntryNotFound);

    // GETRESOURCES: two framebuffers, room for one.
    let a = c.create_dumb(32, 8);
    let b = c.create_dumb(32, 8);
    let fb_a = c.addfb2(&a);
    let fb_b = c.addfb2(&b);
    let mut probe = blank_card_res();
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe), Ok(0));
    let total = probe.count_fbs;
    assert!(total >= 2, "both framebuffers are listed");
    let mut all = alloc::vec![SENTINEL; total as usize];
    let mut full = blank_card_res();
    full.fb_id_ptr = all.as_mut_ptr() as u64;
    full.count_fbs = total;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut full), Ok(0));
    assert!(all.contains(&fb_a) && all.contains(&fb_b));
    let mut one = [SENTINEL; 2];
    let mut short = blank_card_res();
    short.fb_id_ptr = one.as_mut_ptr() as u64;
    short.count_fbs = 1;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut short), Ok(0));
    assert_eq!(
        short.count_fbs, total,
        "the full length comes back with a short array"
    );
    assert_eq!(
        one[0], all[0],
        "the first entry is written into the room there is"
    );
    assert_eq!(one[1], SENTINEL, "nothing is written past the room");

    // OBJ_GETPROPERTIES on the connector: room for one of its properties.
    let mut count: DrmModeObjGetProperties = zeroed();
    count.obj_id = 61;
    count.obj_type = DRM_MODE_OBJECT_CONNECTOR;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut count), Ok(0));
    let n = count.count_props;
    assert!(n >= 2, "the connector carries several properties");
    let mut ids = alloc::vec![SENTINEL; n as usize];
    let mut vals = alloc::vec![u64::MAX; n as usize];
    let mut full = count;
    full.props_ptr = ids.as_mut_ptr() as u64;
    full.prop_values_ptr = vals.as_mut_ptr() as u64;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut full), Ok(0));
    assert!(!ids.contains(&SENTINEL));
    let mut one_id = [SENTINEL; 2];
    let mut one_val = [u64::MAX; 2];
    let mut short = count;
    short.props_ptr = one_id.as_mut_ptr() as u64;
    short.prop_values_ptr = one_val.as_mut_ptr() as u64;
    short.count_props = 1;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut short), Ok(0));
    assert_eq!(short.count_props, n);
    assert_eq!((one_id[0], one_val[0]), (ids[0], vals[0]));
    assert_eq!((one_id[1], one_val[1]), (SENTINEL, u64::MAX));

    // GETCONNECTOR's property list, the same way.
    let mut conn: DrmModeGetConnector = zeroed();
    conn.connector_id = 61;
    let mut conn_id = [SENTINEL; 2];
    let mut conn_val = [u64::MAX; 2];
    conn.props_ptr = conn_id.as_mut_ptr() as u64;
    conn.prop_values_ptr = conn_val.as_mut_ptr() as u64;
    conn.count_props = 1;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn), Ok(0));
    assert_eq!(conn.count_props, n);
    assert_eq!((conn_id[0], conn_val[0]), (ids[0], vals[0]));
    assert_eq!((conn_id[1], conn_val[1]), (SENTINEL, u64::MAX));

    // The object must be of the type asked for; ANY matches every type.
    universal_planes(&c, true);
    let plane = planes(&c)[0];
    let by_type = |id: u32, ty: u32| {
        let mut req: DrmModeObjGetProperties = zeroed();
        req.obj_id = id;
        req.obj_type = ty;
        c.ioctl(DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &mut req)
    };
    assert_eq!(by_type(61, DRM_MODE_OBJECT_PLANE), enoent);
    assert_eq!(by_type(61, DRM_MODE_OBJECT_CONNECTOR), Ok(0));
    assert_eq!(by_type(plane, DRM_MODE_OBJECT_CRTC), enoent);
    assert_eq!(by_type(plane, DRM_MODE_OBJECT_PLANE), Ok(0));
    assert_eq!(by_type(60, DRM_MODE_OBJECT_CONNECTOR), enoent);
    assert_eq!(by_type(60, DRM_MODE_OBJECT_CRTC), Ok(0));
    assert_eq!(by_type(drm::SYNTH_ENCODER_ID, DRM_MODE_OBJECT_CRTC), enoent);
    assert_eq!(
        by_type(drm::SYNTH_ENCODER_ID, DRM_MODE_OBJECT_ENCODER),
        Ok(0)
    );
    assert_eq!(by_type(60, DRM_MODE_OBJECT_ANY), Ok(0));
}
