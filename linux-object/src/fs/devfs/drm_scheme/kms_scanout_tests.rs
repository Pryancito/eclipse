use super::gl_client_sequence_tests::{blank_card_res, parse_events, Client, FLIP_COMPLETE};
use super::*;
use crate::fs::devfs::kms_emu::{self, UNTOUCHED};
use kernel_hal::mem::phys_to_virt;

/// The dumb buffer's pixels, reached the way its owner reaches them through
/// its CPU mapping: `MAP_DUMB` hands out an offset into the backing VMO, and
/// the backing is contiguous physical memory the kernel can address
/// directly.
fn map_dumb(buf: &DrmModeCreateDumb) -> &'static mut [u32] {
    let (pa, size) = drm::resolve_gem_backing(buf.handle).expect("a dumb buffer must have backing");
    assert!(size as u64 >= buf.size, "backing smaller than the buffer");
    let va = phys_to_virt(pa as usize);
    // SAFETY: `size` bytes of contiguous physical memory, identity-mapped
    // into the kernel window at `va`, owned by this buffer for as long as
    // the handle lives.
    unsafe { core::slice::from_raw_parts_mut(va as *mut u32, size / 4) }
}

/// Paint every pixel of `buf`, PADDING INCLUDED, with `f(x, y)` in the
/// buffer's own stride coordinates. A swapchain buffer really does have
/// pixels past the visible width (`CREATE_DUMB` rounds the pitch up to 64
/// bytes, and matches the display's pitch outright for a full-screen
/// request), and whether the present is allowed to carry them to the screen
/// is exactly what the write-combining tests below check.
fn paint(buf: &DrmModeCreateDumb, f: impl Fn(u32, u32) -> u32) {
    let stride = (buf.pitch / 4) as usize;
    let px = map_dumb(buf);
    for y in 0..buf.height as usize {
        for x in 0..stride {
            px[y * stride + x] = f(x as u32, y as u32);
        }
    }
}

/// `drmModeSetCrtc`: the modeset that puts the first frame up.
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

/// `drmModeDirtyFB`: "these boxes changed, put them on the screen".
fn dirtyfb(c: &Client, fb_id: u32, clips: &[DrmClipRect]) {
    let mut cmd = DrmModeFbDirtyCmd {
        fb_id,
        flags: 0,
        color: 0,
        num_clips: clips.len() as u32,
        clips_ptr: clips.as_ptr() as u64,
    };
    c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut cmd).expect("DIRTYFB");
}

fn clip(x1: u16, y1: u16, x2: u16, y2: u16) -> DrmClipRect {
    DrmClipRect { x1, y1, x2, y2 }
}

/// What `drm_mode_dirtyfb_ioctl` refuses before any driver sees the
/// flush: an unknown flag (EINVAL), a framebuffer that does not exist
/// (ENOENT), a clip count and a clip pointer that disagree about whether
/// there are clips (EINVAL), an odd count with ANNOTATE_COPY, whose clips
/// come in pairs (EINVAL), and more than 256 clips (EINVAL). None of it
/// was read: every one of these came back as a flush done. The shapes
/// Xorg's modesetting shadow sends keep going through.
#[test]
fn dirtyfb_refuses_what_linux_refuses() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |x, y| tag(0x0066_0000, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    let clips = [clip(0, 0, 16, 8), clip(16, 8, 32, 16)];
    let ptr = clips.as_ptr() as u64;
    let dirty = |fb_id: u32, flags: u32, num_clips: u32, clips_ptr: u64| {
        let mut cmd = DrmModeFbDirtyCmd {
            fb_id,
            flags,
            color: 0,
            num_clips,
            clips_ptr,
        };
        c.ioctl(DRM_IOCTL_MODE_DIRTYFB, &mut cmd)
    };
    const ANNOTATE_COPY: u32 = 0x01;
    const ANNOTATE_FILL: u32 = 0x02;
    let einval = Err(FsError::InvalidParam);

    assert_eq!(dirty(4242, 0, 1, ptr), Err(FsError::EntryNotFound));
    assert_eq!(
        dirty(fb, 0x4, 1, ptr),
        einval,
        "a flag Linux does not define"
    );
    assert_eq!(dirty(fb, 0, 1, 0), einval, "clips without a pointer");
    assert_eq!(dirty(fb, 0, 0, ptr), einval, "a pointer without clips");
    assert_eq!(
        dirty(fb, ANNOTATE_COPY, 1, ptr),
        einval,
        "copy clips come in pairs"
    );
    assert_eq!(
        dirty(fb, 0, 257, ptr),
        einval,
        "more clips than the kernel reads"
    );

    assert_eq!(dirty(fb, 0, 256, ptr), Ok(0), "exactly the kernel's limit");
    assert_eq!(dirty(fb, ANNOTATE_COPY, 2, ptr), Ok(0));
    assert_eq!(dirty(fb, ANNOTATE_FILL, 1, ptr), Ok(0));
    assert_eq!(dirty(fb, 0, 2, ptr), Ok(0));
    assert_eq!(dirty(fb, 0, 0, 0), Ok(0), "no clips: the whole frame");

    assert_eq!(c.rmfb(fb), Ok(0));
    assert_eq!(c.destroy_dumb(buf.handle), Ok(0));
}

/// `struct drm_mode_cursor`, 28 bytes -- the layout the ioctl number
/// encodes, so a wrong one here would not even reach the arm.
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

const CURSOR_BO: u32 = 0x01;
const CURSOR_MOVE: u32 = 0x02;

/// `drmModeSetCursor`: hand the kernel a pointer bitmap and place it.
fn set_cursor(c: &Client, crtc_id: u32, handle: u32, w: u32, h: u32, x: i32, y: i32) {
    let mut cur = ModeCursor {
        flags: CURSOR_BO | CURSOR_MOVE,
        crtc_id,
        x,
        y,
        width: w,
        height: h,
        handle,
    };
    c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut cur).expect("CURSOR BO");
}

/// `drmModeMoveCursor`.
fn move_cursor(c: &Client, crtc_id: u32, x: i32, y: i32) {
    let mut cur = ModeCursor {
        flags: CURSOR_MOVE,
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

/// A pixel value that is recognisable per coordinate, so a wrapped or
/// shifted copy is visible rather than merely "different".
fn tag(base: u32, x: u32, y: u32) -> u32 {
    base | (y << 8) | x
}

/// The test the module exists for: a flip really does copy the client's
/// pixels onto the output, unchanged and in the right place.
#[test]
fn a_page_flip_puts_the_clients_pixels_on_the_screen() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |x, y| tag(0x0011_0000, x, y));
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    for y in 0..16 {
        for x in 0..64 {
            assert_eq!(
                screen.pixel(x, y),
                tag(0x0011_0000, x, y),
                "pixel ({}, {}) never reached the screen",
                x,
                y
            );
        }
    }
    // And the client still gets its completion, so its frame loop advances.
    drm::flush_pending_flip_completions();
    let mut b = [0u8; 32];
    assert_eq!(c.read_events(&mut b).expect("completion"), 32);
    let ev = parse_events(&b);
    assert_eq!(ev[0].ev_type, FLIP_COMPLETE);
    assert_eq!(ev[0].user_data, 0xF00D);

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A framebuffer larger than the mode is CLIPPED, not wrapped. The source is
/// strided, so an implementation that walked it as a flat run would fill the
/// screen with the framebuffer's first `width * height` pixels -- every row
/// after the first shifted left. That is the classic "the desktop is skewed"
/// symptom and it is invisible to any test that does not compare per pixel.
#[test]
fn a_framebuffer_bigger_than_the_mode_is_clipped_not_wrapped() {
    let screen = kms_emu::attach(24, 6);
    let c = Client::open(0);
    // 40 columns wide, so `CREATE_DUMB` rounds the pitch up to 48 pixels:
    // the stride and the width differ, which is what makes a flat walk of
    // the source visible at all.
    let buf = c.create_dumb(40, 12);
    assert_eq!(buf.pitch / 4, 48, "the pitch is rounded up to 64 bytes");
    paint(&buf, |x, y| tag(0x0022_0000, x, y));
    let fb = c.addfb2(&buf);

    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 24, 6);

    for y in 0..6 {
        for x in 0..24 {
            assert_eq!(
                screen.pixel(x, y),
                tag(0x0022_0000, x, y),
                "pixel ({}, {}) came from the wrong source row",
                x,
                y
            );
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A framebuffer smaller than the mode leaves the rest of the screen alone.
/// Writing past it would be an out-of-bounds store into the scanout aperture
/// on real hardware, and the pixels it would land on belong to whatever was
/// there before -- the text console, usually. `SETCRTC` refuses such a
/// modeset outright (ENOSPC, `drm_crtc_check_viewport`), so the scanout
/// is reached the way the kernel's own callers reach it.
#[test]
fn a_framebuffer_smaller_than_the_mode_leaves_the_rest_of_the_screen_alone() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(16, 4);
    paint(&buf, |x, y| tag(0x0033_0000, x, y));
    let fb = c.addfb2(&buf);

    let mut req = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: drm::SYNTH_CRTC_ID,
        fb_id: fb,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 1,
        mode: make_modeinfo(64, 16),
    };
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut req),
        Err(FsError::NoDeviceSpace),
        "a mode the fb cannot hold"
    );
    drm::present_now_checked(fb, drm::SYNTH_CRTC_ID, None).expect("present");

    for y in 0..16 {
        for x in 0..64 {
            let want = if x < 16 && y < 4 {
                tag(0x0033_0000, x, y)
            } else {
                UNTOUCHED
            };
            assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A damage rectangle repaints its own rows and nothing else. This is what
/// keeps a `DIRTYFB` client (Xorg's modesetting shadow, simple toolkits)
/// from paying for a full-frame copy per damage box, and getting it wrong in
/// the other direction -- copying the whole frame -- is what smeared stale
/// tiles over the screen from a swapchain buffer with only the boxes drawn.
#[test]
fn a_damage_rectangle_repaints_only_its_own_box() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |x, y| tag(0x0044_0000, x, y));
    let fb = c.addfb2(&buf);

    // Frame one, whole screen.
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    // The client now changes EVERY pixel of its buffer but declares only one
    // box dirty. That asymmetry is the point: with a full-frame copy the
    // screen would show the new pixels everywhere and the test could not
    // tell the two apart. It is also the real case -- the frame a client has
    // drawn only the damage boxes into is the one whose untouched areas hold
    // a previous frame, and copying them is what put stale tiles on screen.
    // Box edges are on 16-pixel boundaries so the write-combining expansion
    // (which is unconditional here) does not widen them; that widening has
    // its own test below.
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    dirtyfb(&c, fb, &[clip(16, 4, 32, 8)]);

    for y in 0..16 {
        for x in 0..64 {
            let want = if (16..32).contains(&x) && (4..8).contains(&y) {
                tag(0x0055_0000, x, y)
            } else {
                tag(0x0044_0000, x, y)
            };
            assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A damage box that does not sit on a 64-byte boundary is widened to whole
/// write-combining lines. A partial store to a write-combining aperture
/// flushes a half-full combine buffer over the neighbouring pixels, which is
/// the leftover-squares corruption; the present rounds the box out to
/// 16-pixel (64-byte) lines so every store completes a line.
#[test]
fn a_damage_box_is_widened_to_whole_write_combining_lines() {
    let screen = kms_emu::attach(64, 4);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 4);
    paint(&buf, |_, _| 0x0000_00AA);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 4);

    // One pixel, at x = 21: inside the line [16, 32).
    paint(&buf, |x, y| {
        if x == 21 && y == 1 {
            0x0000_00BB
        } else {
            0x0000_00AA
        }
    });
    screen.repaint(UNTOUCHED);
    dirtyfb(&c, fb, &[clip(21, 1, 22, 2)]);

    for x in 0..64 {
        let want = if (16..32).contains(&x) {
            if x == 21 {
                0x0000_00BB
            } else {
                0x0000_00AA
            }
        } else {
            UNTOUCHED
        };
        assert_eq!(screen.pixel(x, 1), want, "row 1, x = {}", x);
    }
    for y in [0u32, 2, 3] {
        assert!(
            (0..64).all(|x| screen.pixel(x, y) == UNTOUCHED),
            "row {} was repainted for a box that does not touch it",
            y
        );
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The expansion at the right edge lands in the scanline's OFF-SCREEN
/// PADDING, and this is the end of the chain that could not be checked
/// before: `expand_x_for_wc` deliberately rounds the right edge up past the
/// visible width and caps it at the pitch, and `blit_from` has to accept
/// that wider run. It clamped to `info.width` instead, which silently
/// truncated the tail back off and made the whole mitigation inert on
/// exactly the hardware that needs it -- a padded pitch is the normal case
/// (a UEFI GOP reports 2048 pixels per scanline for a 1920-wide mode).
///
/// The geometry is the real one: a full-screen `CREATE_DUMB` is given the
/// DISPLAY's pitch, so the client's own buffer carries those padding pixels
/// too, and a test can tell padding written from padding skipped.
#[test]
fn the_right_edge_expansion_reaches_the_off_screen_padding() {
    // 40 visible columns, 64 per scanline: 24 columns of padding.
    let screen = kms_emu::attach_with(40, 4, 64, true);
    let c = Client::open(0);
    let buf = c.create_dumb(40, 4);
    assert_eq!(
        buf.pitch / 4,
        64,
        "a full-screen dumb buffer takes the display's pitch"
    );
    // Visible columns and padding columns carry different values, so the
    // assertion can say WHERE a pixel came from.
    let src = |x: u32, y: u32| {
        if x < 40 {
            tag(0x0066_0000, x, y)
        } else {
            tag(0x0077_0000, x, y)
        }
    };
    paint(&buf, src);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 40, 4);

    // A box at the right edge: [36, 40). Expanded, that is [32, 48) --
    // eight visible columns and eight of padding.
    screen.repaint(UNTOUCHED);
    dirtyfb(&c, fb, &[clip(36, 1, 40, 2)]);

    for x in 0..screen.pitch_px() {
        let want = if (32..48).contains(&x) {
            src(x, 1)
        } else {
            UNTOUCHED
        };
        assert_eq!(
            screen.pixel(x, 1),
            want,
            "row 1, x = {} (visible width 40, pitch {})",
            x,
            screen.pitch_px()
        );
    }
    // And nothing spilled into the next scanline, which is what the cap at
    // the pitch is for: past the padding is row 2's pixel 0.
    assert!(
        (0..screen.pitch_px()).all(|x| screen.pixel(x, 2) == UNTOUCHED),
        "the expansion ran past the end of the scanline"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The software pointer is composited on top of the frame, and a move
/// restores what it was covering from the framebuffer. wlroots is held on
/// the legacy KMS path, so it never re-renders the scene for a pointer
/// move: if the erase half of this is wrong the cursor leaves a trail, and
/// if the composite half is wrong there is no pointer at all.
#[test]
fn the_software_cursor_is_drawn_over_the_frame_and_erased_when_it_moves() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |_, _| 0x0000_1111);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    // An 8x8 fully opaque pointer. The bitmap is read as `w * h`
    // consecutive pixels, so its own stride is its width.
    let cur = c.create_dumb(8, 8);
    {
        let px = map_dumb(&cur);
        for p in px.iter_mut().take(64) {
            *p = 0xFF00_00FF;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);

    for y in 0..16 {
        for x in 0..64 {
            let want = if (4..12).contains(&x) && (2..10).contains(&y) {
                0xFF00_00FF
            } else {
                0x0000_1111
            };
            assert_eq!(screen.pixel(x, y), want, "cursor at (4, 2): ({}, {})", x, y);
        }
    }

    move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 6);

    for y in 0..16 {
        for x in 0..64 {
            let want = if (40..48).contains(&x) && (6..14).contains(&y) {
                0xFF00_00FF
            } else {
                0x0000_1111
            };
            assert_eq!(
                screen.pixel(x, y),
                want,
                "after the move to (40, 6): ({}, {})",
                x,
                y
            );
        }
    }

    // Leave no pointer behind for the tests that follow.
    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_getcrtc` reports `mode_valid` from `crtc_state->enable`,
/// `drm_mode_getencoder` names `encoder->crtc` and `drm_mode_getconnector`
/// `connector->encoder`. A `SETCRTC` without a mode
/// (`__drm_atomic_helper_set_config` with `.mode = NULL`, which also
/// ignores the fb the request carries) and an `RMFB` of the scanout
/// framebuffer (`atomic_remove_fb`) set the mode to NULL and detach the
/// connectors, so all three answer 0 until the next modeset; DPMS off
/// only clears `active`, so they stay. Here `mode_valid` was 1 whenever
/// the panel had native timings and the encoder was always on the CRTC,
/// so a compositor starting after another had disabled the output (a VT
/// switch) took the console's mode for a current one; and a `SETCRTC`
/// without a mode showed the fb it named instead of turning the pipe off.
#[test]
fn a_disabled_crtc_reports_no_mode_and_no_encoder_until_the_next_modeset() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let other = c.create_dumb(32, 8);
    let fb_other = c.addfb2(&other);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);

    // (GETCRTC mode_valid, GETCRTC fb_id, GETENCODER crtc_id, GETCONNECTOR
    // encoder_id): the pipe as the three lookups describe it.
    let pipe = || {
        let mut crtc: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
        crtc.crtc_id = drm::SYNTH_CRTC_ID;
        c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
        let mut enc: DrmModeGetEncoder = unsafe { core::mem::zeroed() };
        enc.encoder_id = drm::SYNTH_ENCODER_ID;
        c.ioctl(DRM_IOCTL_MODE_GETENCODER, &mut enc)
            .expect("GETENCODER");
        let mut conn: DrmModeGetConnector = unsafe { core::mem::zeroed() };
        conn.connector_id = drm::SYNTH_CONNECTOR_ID;
        c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn)
            .expect("GETCONNECTOR");
        (crtc.mode_valid, crtc.fb_id, enc.crtc_id, conn.encoder_id)
    };
    let on = (1, fb, drm::SYNTH_CRTC_ID, drm::SYNTH_ENCODER_ID);
    let off = (0, 0, 0, 0);
    assert_eq!(pipe(), on, "with a mode set");

    // SETCRTC without a mode: off, and the fb it names is not shown.
    let mut disable: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
    disable.crtc_id = drm::SYNTH_CRTC_ID;
    disable.fb_id = fb_other;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
    assert_eq!(pipe(), off, "after SETCRTC without a mode");
    assert!(drm::crtc_blanked(), "the pipe is off");
    assert_eq!(drm::crtc_fb(), 0, "the fb of a modeless SETCRTC was shown");

    // The next modeset brings everything back.
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);
    assert_eq!(pipe(), on, "after the modeset");

    // DPMS off keeps the mode and the encoder: only `active` goes.
    #[repr(C)]
    struct ConnectorSetProperty {
        value: u64,
        prop_id: u32,
        connector_id: u32,
    }
    let mut dpms = ConnectorSetProperty {
        value: 3, // Off
        prop_id: PROP_DPMS,
        connector_id: drm::SYNTH_CONNECTOR_ID,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));
    assert!(drm::crtc_blanked());
    assert_eq!(pipe(), on, "DPMS off is not a disable");
    dpms.value = DRM_MODE_DPMS_ON;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));

    // RMFB of the scanout framebuffer disables the CRTC with it.
    c.rmfb(fb).expect("RMFB");
    assert_eq!(pipe(), off, "after RMFB of the scanout framebuffer");

    c.rmfb(fb_other).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(other.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_getplane` reports `plane->state->crtc` and `plane->state->fb`:
/// the CRTC and framebuffer the primary plane shows, 0 and 0 once the
/// pipe is disabled (a `SETCRTC` without a mode, an `RMFB` of the
/// scanout) and unchanged under DPMS off, and the fb follows a page
/// flip. Here the synthetic plane answered its CRTC always and no
/// framebuffer ever, so a client reading the plane back saw a plane on a
/// CRTC with nothing on it whatever was on the screen.
#[test]
fn the_primary_plane_reports_its_crtc_and_framebuffer_only_while_it_has_them() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    let fb = c.addfb2(&buf);
    let next = c.create_dumb(32, 8);
    let fb_next = c.addfb2(&next);
    let plane = || {
        let mut res: DrmModeGetPlane = unsafe { core::mem::zeroed() };
        res.plane_id = drm::SYNTH_PLANE_ID;
        c.ioctl(DRM_IOCTL_MODE_GETPLANE, &mut res)
            .expect("GETPLANE");
        (res.crtc_id, res.fb_id)
    };

    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);
    assert_eq!(plane(), (drm::SYNTH_CRTC_ID, fb), "with the fb on the CRTC");

    let mut disable: DrmModeGetCrtc = unsafe { core::mem::zeroed() };
    disable.crtc_id = drm::SYNTH_CRTC_ID;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETCRTC, &mut disable), Ok(0));
    assert_eq!(plane(), (0, 0), "after SETCRTC without a mode");

    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 8);
    #[repr(C)]
    struct ConnectorSetProperty {
        value: u64,
        prop_id: u32,
        connector_id: u32,
    }
    let mut dpms = ConnectorSetProperty {
        value: 3, // Off
        prop_id: PROP_DPMS,
        connector_id: drm::SYNTH_CONNECTOR_ID,
    };
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));
    assert_eq!(
        plane(),
        (drm::SYNTH_CRTC_ID, fb),
        "DPMS off keeps the plane state"
    );
    dpms.value = DRM_MODE_DPMS_ON;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_SETPROPERTY, &mut dpms), Ok(0));

    assert_eq!(c.page_flip(drm::SYNTH_CRTC_ID, fb_next, 0), Ok(0));
    assert_eq!(
        plane(),
        (drm::SYNTH_CRTC_ID, fb_next),
        "the fb follows a flip"
    );
    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 64];
    let _ = c.read_events(&mut sink);

    c.rmfb(fb_next).expect("RMFB");
    assert_eq!(plane(), (0, 0), "after RMFB of the scanout framebuffer");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(next.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_cursor_common` reads the flags before it looks the CRTC up:
/// no flag at all, or one it does not know, is EINVAL, ahead of the
/// ENOENT of a CRTC that does not exist. And a handle is wrapped in a
/// framebuffer of `width x height`, so a zero width or height is EINVAL
/// (`drm_internal_framebuffer_create`), where only a handle of 0 hides
/// the pointer. Here a request with no flag or an unknown one answered
/// success having done nothing, and a zero-sized image hid the pointer
/// with success; both refusals leave the pointer where it was.
#[test]
fn the_cursor_ioctl_reads_its_flags_first_and_refuses_an_empty_image() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |_, _| 0x0000_1111);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    let cur = c.create_dumb(8, 8);
    {
        let px = map_dumb(&cur);
        for p in px.iter_mut().take(64) {
            *p = 0xFF00_00FF;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);
    let pointer_at = |x0: i32, y0: i32, what: &str| {
        for y in 0..16 {
            for x in 0..64 {
                let inside =
                    (x0..x0 + 8).contains(&(x as i32)) && (y0..y0 + 8).contains(&(y as i32));
                let want = if inside { 0xFF00_00FF } else { 0x0000_1111 };
                assert_eq!(screen.pixel(x, y), want, "{}: ({}, {})", what, x, y);
            }
        }
    };
    pointer_at(4, 2, "before");

    let cursor = |flags: u32, crtc_id: u32, handle: u32, w: u32, h: u32| {
        let mut req = ModeCursor {
            flags,
            crtc_id,
            x: 40,
            y: 6,
            width: w,
            height: h,
            handle,
        };
        c.ioctl(DRM_IOCTL_MODE_CURSOR, &mut req)
    };
    const NO_SUCH_CRTC: u32 = 4242;
    const UNKNOWN: u32 = 0x04;
    let einval = Err(FsError::InvalidParam);
    assert_eq!(
        cursor(0, drm::SYNTH_CRTC_ID, cur.handle, 8, 8),
        einval,
        "no flag"
    );
    assert_eq!(
        cursor(UNKNOWN, drm::SYNTH_CRTC_ID, cur.handle, 8, 8),
        einval,
        "unknown flag"
    );
    assert_eq!(
        cursor(CURSOR_MOVE | UNKNOWN, drm::SYNTH_CRTC_ID, 0, 0, 0),
        einval,
        "an unknown flag next to a known one"
    );
    assert_eq!(
        cursor(0, NO_SUCH_CRTC, cur.handle, 8, 8),
        einval,
        "the flags are read before the CRTC"
    );
    assert_eq!(
        cursor(CURSOR_MOVE, NO_SUCH_CRTC, 0, 0, 0),
        Err(FsError::EntryNotFound),
        "a CRTC that does not exist"
    );
    assert_eq!(
        cursor(CURSOR_BO, drm::SYNTH_CRTC_ID, cur.handle, 0, 8),
        einval,
        "zero width"
    );
    assert_eq!(
        cursor(CURSOR_BO, drm::SYNTH_CRTC_ID, cur.handle, 8, 0),
        einval,
        "zero height"
    );
    pointer_at(4, 2, "after the refusals");

    // The operations themselves are still there: a move, a new image
    // with a move, and a hide with handle 0, whatever the size says.
    assert_eq!(cursor(CURSOR_MOVE, drm::SYNTH_CRTC_ID, 0, 0, 0), Ok(0));
    pointer_at(40, 6, "after the move");
    assert_eq!(
        cursor(
            CURSOR_BO | CURSOR_MOVE,
            drm::SYNTH_CRTC_ID,
            cur.handle,
            8,
            8
        ),
        Ok(0)
    );
    pointer_at(40, 6, "after the image and move");
    assert_eq!(
        cursor(CURSOR_BO, drm::SYNTH_CRTC_ID, 0, 0, 0),
        Ok(0),
        "hide"
    );
    pointer_at(-8, -8, "hidden");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The topology a compositor reads before it presents anything. With an
/// output attached this is a KMS card: `drmIsKMS` wants a CRTC, a connector
/// and an encoder, and wlroots then wants the connector CONNECTED with at
/// least one mode. Any one of those at zero and the output is skipped
/// entirely -- the black screen that reports nothing.
#[test]
fn the_synthetic_topology_is_what_a_compositor_reads() {
    let _screen = kms_emu::attach(128, 32);
    let c = Client::open(0);

    // Pass one: counts.
    let mut probe = blank_card_res();
    c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut probe)
        .expect("GETRESOURCES");
    assert_eq!(probe.count_crtcs, 1, "drmIsKMS needs a CRTC");
    assert_eq!(probe.count_connectors, 1, "drmIsKMS needs a connector");
    assert_eq!(probe.count_encoders, 1, "drmIsKMS needs an encoder");

    // Pass two: ids, into arrays the caller sized from pass one.
    let mut crtcs = [0u32; 1];
    let mut conns = [0u32; 1];
    let mut encs = [0u32; 1];
    let mut fill = blank_card_res();
    fill.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
    fill.connector_id_ptr = conns.as_mut_ptr() as u64;
    fill.encoder_id_ptr = encs.as_mut_ptr() as u64;
    fill.count_crtcs = 1;
    fill.count_connectors = 1;
    fill.count_encoders = 1;
    c.ioctl(DRM_IOCTL_MODE_GETRESOURCES, &mut fill)
        .expect("GETRESOURCES fill");
    assert_eq!(crtcs[0], drm::SYNTH_CRTC_ID);
    assert_eq!(encs[0], drm::SYNTH_ENCODER_ID);

    // The connector, with the mode wlroots will pick.
    let mut mode = [0u8; 68];
    let mut conn = DrmModeGetConnector {
        encoders_ptr: 0,
        modes_ptr: mode.as_mut_ptr() as u64,
        props_ptr: 0,
        prop_values_ptr: 0,
        count_modes: 1,
        count_props: 0,
        count_encoders: 0,
        encoder_id: 0,
        connector_id: conns[0],
        connector_type: 0,
        connector_type_id: 0,
        connection: 0,
        mm_width: 0,
        mm_height: 0,
        subpixel: 0,
        pad: 0,
    };
    c.ioctl(DRM_IOCTL_MODE_GETCONNECTOR, &mut conn)
        .expect("GETCONNECTOR");
    assert_eq!(conn.connection, 1, "the output must report CONNECTED");
    assert_eq!(conn.count_modes, 1, "and offer a mode");
    assert_eq!(
        u16::from_ne_bytes([mode[4], mode[5]]),
        128,
        "hdisplay is the attached output's width"
    );
    assert_eq!(
        u16::from_ne_bytes([mode[14], mode[15]]),
        32,
        "vdisplay is its height"
    );
    assert!(
        conn.mm_width > 0 && conn.mm_height > 0,
        "a physical size of 0 is an infinite DPI to every client that divides by it"
    );

    // One primary plane on that CRTC -- to a client that asked for
    // universal planes, as every compositor does.
    let mut cap: [u64; 2] = [DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1];
    c.ioctl(DRM_IOCTL_SET_CLIENT_CAP, &mut cap)
        .expect("SET_CLIENT_CAP UNIVERSAL_PLANES");
    let mut planes = [0u32; 1];
    let mut plane_res = DrmModeGetPlaneRes {
        plane_id_ptr: planes.as_mut_ptr() as u64,
        count_planes: 1,
    };
    c.ioctl(DRM_IOCTL_MODE_GETPLANERESOURCES, &mut plane_res)
        .expect("GETPLANERESOURCES");
    assert_eq!(plane_res.count_planes, 1);
    assert_eq!(planes[0], drm::SYNTH_PLANE_ID);
}

/// `GETCRTC` reports the framebuffer that is really on screen. A compositor
/// reads this back to decide whether its modeset took, and the id has to be
/// in the DRM core's namespace, not a driver-private one.
#[test]
fn getcrtc_reports_the_framebuffer_that_was_flipped_to() {
    let _screen = kms_emu::attach(32, 8);
    let c = Client::open(0);
    let buf = c.create_dumb(32, 8);
    paint(&buf, |_, _| 0x0000_2222);
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 1).expect("flip");
    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 32];
    let _ = c.read_events(&mut sink);

    let mut crtc = DrmModeGetCrtc {
        set_connectors_ptr: 0,
        count_connectors: 0,
        crtc_id: drm::SYNTH_CRTC_ID,
        fb_id: 0,
        x: 0,
        y: 0,
        gamma_size: 0,
        mode_valid: 0,
        mode: [0; 68],
    };
    c.ioctl(DRM_IOCTL_MODE_GETCRTC, &mut crtc).expect("GETCRTC");
    assert_eq!(crtc.fb_id, fb, "the CRTC does not name the flipped fb");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A full-frame present fills the scanline out to the last write-combining
/// line and stops. The lines past it belong to no visible pixel, and the
/// byte after the last one is the NEXT ROW's leftmost pixel -- running into
/// it is how a blit smears a frame diagonally down the screen.
#[test]
fn a_full_frame_present_stops_at_the_end_of_the_scanline() {
    // 40 visible columns of a 64-pixel scanline, write-combining: so the
    // expansion of [0, 40) is [0, 48) and 16 columns must stay untouched.
    let screen = kms_emu::attach_with(40, 8, 64, true);
    let c = Client::open(0);
    let buf = c.create_dumb(40, 8);
    let src = |x: u32, y: u32| {
        if x < 40 {
            tag(0x0088_0000, x, y)
        } else {
            tag(0x0099_0000, x, y)
        }
    };
    paint(&buf, src);
    let fb = c.addfb2(&buf);

    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 40, 8);

    for y in 0..8 {
        for x in 0..screen.pitch_px() {
            let want = if x < 48 { src(x, y) } else { UNTOUCHED };
            assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The pointer at the right edge of a padded scanline does not wrap onto the
/// next row. Its patch is widened to whole write-combining lines just like a
/// present, so it can legitimately reach into the off-screen padding -- but
/// past the padding is the next row's leftmost pixel, and a pointer whose
/// tail appears on the far left of the line below is the visible form of
/// that off-by-one.
#[test]
fn the_pointer_at_the_right_edge_does_not_wrap_onto_the_next_row() {
    let screen = kms_emu::attach_with(40, 8, 64, true);
    let c = Client::open(0);
    let buf = c.create_dumb(40, 8);
    paint(&buf, |_, _| 0x0000_3333);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 40, 8);

    // A pointer bitmap tagged by position, so a row or column read from the
    // wrong place in it is visible rather than merely "opaque".
    let cur = c.create_dumb(8, 8);
    {
        let px = map_dumb(&cur);
        for (i, p) in px.iter_mut().take(64).enumerate() {
            *p = 0xFF00_0000 | ((i as u32 / 8) << 8) | (i as u32 % 8);
        }
    }
    // x = 36: four columns visible, four in the padding.
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 36, 1);

    for row in 0..8u32 {
        for x in 0..screen.pitch_px() {
            let p = screen.pixel(x, row);
            let in_cursor = (36..44).contains(&x) && (1..8).contains(&row);
            if in_cursor {
                // Exactly the pointer pixel for this position, taken from
                // the right row and column of the bitmap.
                let want = 0xFF00_0000 | ((row - 1) << 8) | (x - 36);
                assert_eq!(want, p, "pointer pixel at ({}, {})", x, row);
            } else {
                assert_ne!(
                    p >> 24,
                    0xFF,
                    "a pointer pixel landed at ({}, {}) -- outside the pointer",
                    x,
                    row
                );
            }
        }
    }

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A damage box that runs off the end of the framebuffer is clamped to it.
/// The clip rectangle comes straight from a client, and the present reads
/// the framebuffer at `(y * stride + x)` -- an unclamped box is an
/// out-of-bounds read of whatever follows the buffer, painted on screen.
#[test]
fn a_damage_box_that_runs_off_the_framebuffer_is_clamped_to_it() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    paint(&buf, |x, y| tag(0x00BB_0000, x, y));
    screen.repaint(UNTOUCHED);
    // Bottom-right corner, running far past both edges.
    dirtyfb(&c, fb, &[clip(56, 12, 200, 200)]);

    for y in 0..16 {
        for x in 0..64 {
            // [56, 64) widened to the 16-pixel line [48, 64), rows 12..16.
            let want = if x >= 48 && y >= 12 {
                tag(0x00BB_0000, x, y)
            } else {
                UNTOUCHED
            };
            assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// Drain whatever flip completions are outstanding, so a later read only
/// sees the ones the test is about.
///
/// One read is not a drain: the queue hands out as much as fits and keeps the
/// rest, and an empty queue answers EAGAIN rather than zero. A test that
/// queued more than this buffer holds would leave completions behind for a
/// later assertion to trip over -- a suite that fails somewhere else, which
/// is the worst kind of noise to build in. Read until the queue says it has
/// nothing, with a bound so a queue that always answers cannot hang the
/// suite instead of failing it.
fn drain_completions(c: &Client) {
    drm::flush_pending_flip_completions();
    let mut sink = [0u8; 256];
    for _ in 0..1024 {
        match c.read_events(&mut sink) {
            Ok(n) if n > 0 => continue,
            _ => return,
        }
    }
    panic!("the event queue never drained");
}

/// The bug the pause machinery had, from the compositor's side. During the
/// deferred console-GPU bring-up scanout is parked: every flip is reported
/// complete and no pixel is written, which is deliberate -- labwc's BAR1
/// traffic must stay out of the SEC2 window. What was missing is the other
/// half. The compositor was TOLD those frames landed, so it will not draw
/// them again, and nothing put the last one up when the window closed: an
/// idle desktop sat on a pre-pause frame with scanout fully alive.
#[test]
fn the_frame_dropped_while_scanout_was_paused_reaches_the_panel_on_resume() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let before = c.create_dumb(64, 16);
    paint(&before, |x, y| tag(0x0011_0000, x, y));
    let fb_before = c.addfb2(&before);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_before, 1)
        .expect("the frame before the pause");
    assert_eq!(
        screen.pixel(7, 3),
        tag(0x0011_0000, 7, 3),
        "frame one is up"
    );
    drain_completions(&c);

    // The bring-up parks scanout. labwc knows nothing about it and renders.
    drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
    let during = c.create_dumb(64, 16);
    paint(&during, |x, y| tag(0x0022_0000, x, y));
    let fb_during = c.addfb2(&during);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 2)
        .expect("a flip during the pause is still accepted");

    // Nothing reached the panel: that is what the pause is for.
    assert_eq!(
        screen.pixel(7, 3),
        tag(0x0011_0000, 7, 3),
        "the pause let a frame through to the framebuffer"
    );
    // And the client was told it completed, which is why it will never
    // draw that frame again and why the kernel owes it a repaint.
    drm::flush_pending_flip_completions();
    let mut b = [0u8; 32];
    assert_eq!(c.read_events(&mut b).expect("completion"), 32);
    assert_eq!(parse_events(&b)[0].user_data, 2);

    drm::set_scanout_paused(false);

    for y in 0..16 {
        for x in 0..64 {
            assert_eq!(
                screen.pixel(x, y),
                tag(0x0022_0000, x, y),
                "pixel ({}, {}) is still the pre-pause frame: the resume \
                 left the desktop frozen with scanout running",
                x,
                y
            );
        }
    }

    c.rmfb(fb_before).expect("RMFB");
    c.rmfb(fb_during).expect("RMFB");
    c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
}

/// And the visible half of the same disagreement. A pointer move does not
/// re-blit the frame; it restores the two ~64x64 windows it touches FROM
/// `crtc_fb`. With the panel a frame behind that buffer -- which is exactly
/// what a dropped present leaves -- those windows paste pieces of a frame
/// nobody has seen into the one still on screen: a ring of garbage that
/// follows the cursor, invisible on a flat wallpaper and obvious over a
/// window shadow.
///
/// The watchdog is what gets there: it lifts the pause on a clock read
/// without anyone presenting, so the first thing to run afterwards can well
/// be a mouse move. A zero-length window is that same code path without a
/// test that sleeps.
#[test]
fn a_pointer_move_after_the_watchdog_does_not_paste_pieces_of_the_unseen_frame() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let before = c.create_dumb(64, 16);
    paint(&before, |x, y| tag(0x0011_0000, x, y));
    let fb_before = c.addfb2(&before);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_before, 64, 16);
    drain_completions(&c);

    // A pointer the kernel composites itself, placed off to one side.
    let ptr = c.create_dumb(8, 8);
    paint(&ptr, |_, _| 0xFFFF_FFFF);
    set_cursor(&c, drm::SYNTH_CRTC_ID, ptr.handle, 8, 8, 4, 4);
    drain_completions(&c);

    drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
    let during = c.create_dumb(64, 16);
    paint(&during, |x, y| tag(0x0022_0000, x, y));
    let fb_during = c.addfb2(&during);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 3)
        .expect("a flip during the pause is still accepted");
    drain_completions(&c);

    // The bring-up never came back, so the watchdog is what resumes -- with
    // no present of its own. `Duration::ZERO` is a window already closed.
    drm::set_scanout_paused_for(core::time::Duration::ZERO);
    assert!(
        !drm::scanout_paused(),
        "the watchdog did not lift the pause"
    );

    move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 8);

    // Every pixel the pointer does not cover belongs to ONE frame. Before
    // the fix the answer was "frame one, except two windows of frame two".
    let mut saw_second = false;
    for y in 0..16 {
        for x in 0..64 {
            let px = screen.pixel(x, y);
            if px == tag(0x0022_0000, x, y) {
                saw_second = true;
                continue;
            }
            assert_ne!(
                px,
                tag(0x0011_0000, x, y),
                "pixel ({}, {}) is still the frame the panel was showing \
                 while the rest came from the one it never saw -- that is \
                 the garbage around the cursor",
                x,
                y
            );
        }
    }
    assert!(saw_second, "the pointer move put nothing on the screen");

    c.rmfb(fb_before).expect("RMFB");
    c.rmfb(fb_during).expect("RMFB");
    c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(ptr.handle).expect("DESTROY_DUMB");
}

/// The other side of the damage rule: a `DIRTYFB` clip that covers the whole
/// framebuffer IS a catch-up, so it clears the mark. Leaving it set would
/// cost a redundant full repaint on the next pointer move.
#[test]
fn a_damage_rect_over_the_whole_screen_does_catch_the_panel_up() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let before = c.create_dumb(64, 16);
    paint(&before, |x, y| tag(0x0011_0000, x, y));
    let fb_before = c.addfb2(&before);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_before, 64, 16);
    drain_completions(&c);

    drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
    let during = c.create_dumb(64, 16);
    paint(&during, |x, y| tag(0x0022_0000, x, y));
    let fb_during = c.addfb2(&during);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 5)
        .expect("a flip during the pause is still accepted");
    drain_completions(&c);
    drm::set_scanout_paused_for(core::time::Duration::ZERO);
    assert!(!drm::scanout_paused());
    assert!(drm::scanout_is_stale_for_test());

    dirtyfb(&c, fb_during, &[clip(0, 0, 64, 16)]);

    assert!(
        !drm::scanout_is_stale_for_test(),
        "a clip over the whole framebuffer put every row up, so the mark \
         must go -- keeping it costs a full repaint on the next mouse move"
    );

    c.rmfb(fb_before).expect("RMFB");
    c.rmfb(fb_during).expect("RMFB");
    c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
}

/// The popup's geometry, swept. Moebius's power menu comes up with whole
/// runs of it missing -- the desktop showing through where the panel should
/// be -- and the runs are in the same place on every frame, so whatever
/// drops them is arithmetic tied to the box, not a race or a stale cache.
///
/// This asks the narrowest version of that question the kernel can answer on
/// its own: for a damage box, does the present write EVERY pixel inside it?
/// A box is not a set of independent rows here -- the blit widens columns to
/// write-combining lines and walks bands -- so an off-by-one in any of that
/// arithmetic shows up as pixels inside the box still carrying the previous
/// frame, which is exactly the symptom. Geometries chosen to be hostile:
/// the real panel (152x135) at several offsets, odd sizes, the single pixel,
/// a box on each edge, and the whole frame.
#[test]
fn every_pixel_inside_a_damage_box_is_written_whatever_its_geometry() {
    const W: u32 = 200;
    const H: u32 = 160;
    let screen = kms_emu::attach(W, H);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    let fb = c.addfb2(&buf);

    let boxes: [(u32, u32, u32, u32); 10] = [
        (0, 0, 152, 135),   // the panel, flush at the origin
        (7, 3, 152, 135),   // and at an odd offset, both axes
        (48, 25, 152, 135), // and where it does not fit: clipped right
        (1, 1, 7, 2),       // narrower than one WC line
        (13, 11, 31, 17),   // odd on every number
        (W - 1, H - 1, 1, 1),
        (0, H - 1, W, 1), // the last row, whole
        (W - 3, 0, 3, H), // the last columns, whole
        (0, 0, W, H),     // the whole frame through the damage path
        (9, 9, 16, 16),   // exactly one WC line wide, aligned to none
    ];

    for (i, &(bx, by, bw, bh)) in boxes.iter().enumerate() {
        let old = 0x0100_0000 * (2 * i as u32 + 1);
        let new = 0x0100_0000 * (2 * i as u32 + 2);

        // The frame that is already on the panel.
        paint(&buf, |x, y| tag(old, x, y));
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 200 + i as u64)
            .expect("the frame before the damage");
        drain_completions(&c);
        assert_eq!(
            screen.pixel(0, 0),
            tag(old, 0, 0),
            "box {:?}: the first frame never got up",
            (bx, by, bw, bh)
        );

        // The client repaints the same buffer and names only its box.
        paint(&buf, |x, y| tag(new, x, y));
        dirtyfb(
            &c,
            fb,
            &[clip(
                bx as u16,
                by as u16,
                (bx + bw) as u16,
                (by + bh) as u16,
            )],
        );

        let (cw, ch) = (bw.min(W - bx), bh.min(H - by));
        for y in by..by + ch {
            for x in bx..bx + cw {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(new, x, y),
                    "box {:?}: pixel ({}, {}) inside it still carries the \
                     previous frame",
                    (bx, by, bw, bh),
                    x,
                    y
                );
            }
        }
        // Columns are widened to write-combining lines on purpose, so only
        // the ROWS outside the box are guaranteed untouched.
        for y in (0..by).chain(by + ch..H) {
            for x in 0..W {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(old, x, y),
                    "box {:?}: row {} is outside it and was repainted",
                    (bx, by, bw, bh),
                    y
                );
            }
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The same sweep on a WRITE-COMBINING output with a padded scanline, which
/// is what real hardware is: UEFI reports a `PixelsPerScanLine` wider than
/// the mode, the framebuffer is mapped WC, and on x86_64 `blit_from` then
/// takes the non-temporal store loop instead of the ordinary copy. That loop
/// is a different implementation of the same promise, so the promise has to
/// be checked against it too -- and it is the one that runs on the machine
/// where Moebius sees the popup come up with pieces missing.
#[test]
fn every_pixel_inside_a_damage_box_is_written_on_a_write_combining_output() {
    const W: u32 = 204;
    const H: u32 = 184;
    // 204 -> the pitch UEFI would report, wider than the mode.
    let screen = kms_emu::attach_with(W, H, 256, true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    let fb = c.addfb2(&buf);

    // The popup's own surface, and the offsets the report names: pieces
    // missing every 64 px, i.e. every 256 bytes of a row.
    let boxes: [(u32, u32, u32, u32); 6] = [
        (0, 0, 204, 184),
        (0, 0, 180, 160),
        (12, 12, 180, 160),
        (13, 11, 63, 65),
        (64, 0, 8, H),
        (W - 1, H - 1, 1, 1),
    ];

    for (i, &(bx, by, bw, bh)) in boxes.iter().enumerate() {
        let old = 0x0100_0000 * (2 * i as u32 + 1);
        let new = 0x0100_0000 * (2 * i as u32 + 2);

        paint(&buf, |x, y| tag(old, x, y));
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 300 + i as u64)
            .expect("the frame before the damage");
        drain_completions(&c);

        paint(&buf, |x, y| tag(new, x, y));
        dirtyfb(
            &c,
            fb,
            &[clip(
                bx as u16,
                by as u16,
                (bx + bw) as u16,
                (by + bh) as u16,
            )],
        );

        let (cw, ch) = (bw.min(W - bx), bh.min(H - by));
        for y in by..by + ch {
            for x in bx..bx + cw {
                assert_eq!(
                    screen.pixel(x, y),
                    tag(new, x, y),
                    "box {:?}: pixel ({}, {}) inside it still carries the \
                     previous frame -- byte {} of its row",
                    (bx, by, bw, bh),
                    x,
                    y,
                    x * 4
                );
            }
        }
    }

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A damage rect is not a catch-up. `DRM_IOCTL_MODE_DIRTYFB` copies the
/// boxes the client names and nothing else, so after a dropped present the
/// panel is still a frame behind everywhere outside them -- and the cursor
/// repaint would go back to restoring rects from a buffer the panel does not
/// show. Only a whole frame may clear the mark.
///
/// The box has to name the framebuffer the panel already carries for this to
/// be reachable at all: a box on any other one is promoted to a whole frame
/// before it gets here (see
/// `a_damage_box_on_a_fresh_buffer_puts_the_whole_frame_up`), and a whole
/// frame is a catch-up. What is left is the client re-damaging the buffer
/// that IS up while `crtc_fb` points at the one the pause swallowed: the
/// panel does carry that buffer, so the box is honoured, and the panel is
/// still not showing what the cursor repaint would read.
#[test]
fn a_damage_rect_does_not_catch_a_panel_up_from_a_dropped_frame() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let before = c.create_dumb(64, 16);
    paint(&before, |x, y| tag(0x0011_0000, x, y));
    let fb_before = c.addfb2(&before);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_before, 64, 16);
    drain_completions(&c);

    drm::set_scanout_paused_for(drm::SCANOUT_PAUSE_MAX);
    let during = c.create_dumb(64, 16);
    paint(&during, |x, y| tag(0x0022_0000, x, y));
    let fb_during = c.addfb2(&during);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_during, 4)
        .expect("a flip during the pause is still accepted");
    drain_completions(&c);
    drm::set_scanout_paused_for(core::time::Duration::ZERO);
    assert!(!drm::scanout_paused());

    // A four-pixel box, the way a blinking cursor in a terminal damages, on
    // the buffer the panel really carries.
    assert_eq!(drm::panel_fb_for_test(), fb_before);
    dirtyfb(&c, fb_before, &[clip(0, 0, 4, 4)]);

    assert!(
        drm::scanout_is_stale_for_test(),
        "a damage rect cleared the mark, so the next pointer move will \
         restore its windows from a frame the panel is not showing"
    );

    // And the other half of the same situation: the box that names the
    // framebuffer the pause swallowed cannot be honoured -- the panel does
    // not carry it -- so it becomes a whole frame, which heals the frame the
    // pause dropped instead of waiting for a pointer move to expose it.
    dirtyfb(&c, fb_during, &[clip(0, 0, 4, 4)]);
    assert!(
        !drm::scanout_is_stale_for_test(),
        "a box on a framebuffer the panel does not carry has to become a \
         whole frame, and a whole frame catches the panel up"
    );
    assert_eq!(drm::panel_fb_for_test(), fb_during);

    c.rmfb(fb_before).expect("RMFB");
    c.rmfb(fb_during).expect("RMFB");
    c.destroy_dumb(before.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(during.handle).expect("DESTROY_DUMB");
}

/// A damage-clipped present is counted, and counted as its own kind.
///
/// The present's phase-timing line used to sit behind `if rect.is_none()`, so
/// the path a compositor with damage tracking actually drives -- every frame
/// labwc puts up -- printed nothing at all, and the number that says whether
/// the source flush is oversized was the one number never reported. Nothing
/// can see a klog line from here, so the counters are what this asserts: the
/// two kinds are tallied separately, because they happen at rates nothing
/// alike and one divisor for both either drowns the log or hides the clipped
/// path again.
#[test]
fn a_damage_clipped_present_is_counted_as_its_own_kind() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let buf = c.create_dumb(64, 16);
    paint(&buf, |x, y| tag(0x0066_0000, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    drain_completions(&c);

    let (frames0, rects0) = drm::present_report_counts_for_test();

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 9).expect("flip");
    drain_completions(&c);
    let (frames1, rects1) = drm::present_report_counts_for_test();
    assert!(frames1 > frames0, "a full-frame present was not counted");
    assert_eq!(rects1, rects0, "a full frame was counted as a damage box");

    dirtyfb(&c, fb, &[clip(8, 4, 24, 8)]);
    let (frames2, rects2) = drm::present_report_counts_for_test();
    assert!(
        rects2 > rects1,
        "a damage-clipped present was not counted, so it will never be \
         reported either -- which is the defect this change is about"
    );
    assert_eq!(frames2, frames1, "a damage box was counted as a full frame");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A damage box says "only these pixels changed in the frame already on the
/// panel". A compositor with a swapchain presents a DIFFERENT framebuffer
/// almost every frame, and a recycled swapchain buffer holds, outside the
/// region it was just drawn into, whatever frame it was last used for.
///
/// So honouring the box against another buffer leaves the panel carrying two
/// frames at once. That is invisible until something reads the panel's own
/// content back -- and `repaint_for_cursor` does exactly that, restoring its
/// two ~64x64 windows from `crtc_fb`. A pointer move over a region the box
/// did not touch then pastes the new buffer's older content into the frame
/// still up: garbage in a ring around the cursor, appearing exactly when a
/// popup opens, because that is when a fresh buffer arrives with a box around
/// the popup and nothing else.
///
/// Linux throws the clips away and declares a full update whenever
/// `state->fb != old_state->fb` (`drm_atomic_helper_damage_iter_init`).
/// The whole point, staged: the client keeps writing the buffer AFTER the
/// present has already copied those rows, and the repair pass picks up what
/// arrived. Without it the panel keeps the pixels the copy happened to catch,
/// which is the stain Moebius sees on a freshly redrawn title bar or menu.
///
/// The screen is 200 rows so the blit takes two bands of `BLIT_CHUNK_ROWS`,
/// and the hook writes on the SECOND band -- rows the first band already
/// carried to the panel. That ordering is the whole test: a write before the
/// first band would simply be copied, and would prove nothing.
#[test]
fn the_repair_pass_picks_up_what_arrived_after_the_copy_passed() {
    let screen = kms_emu::attach(192, 200);
    drm::set_present_repair_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    paint(&buf, |x, y| tag(0x0066_0000, x, y));
    let fb = c.addfb2(&buf);
    let pixels = map_dumb(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = pixels.as_mut_ptr() as usize;

    kms_emu::on_blit_band(move |band| {
        // Second band only: by now rows 0..128 are already on the panel.
        if band != 1 {
            return;
        }
        // SAFETY: the dumb buffer outlives this present, and nothing else
        // writes it while the hook runs.
        let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
        for y in 0..128usize {
            for x in 64..128usize {
                p[y * stride + x] = tag(0x0077_0000, x as u32, y as u32);
            }
        }
    });

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    assert!(
        kms_emu::mid_blit_calls() >= 2,
        "the blit has to take at least two bands for this to stage anything, took {}",
        kms_emu::mid_blit_calls()
    );
    assert!(
        drm::repair_rounds_for_test() >= 1,
        "a source that moved under the copy has to cost at least one repair round"
    );
    for y in 0..128 {
        for x in 64..128 {
            assert_eq!(
                screen.pixel(x, y),
                tag(0x0077_0000, x, y),
                "pixel ({}, {}) was written after the copy passed and never repaired",
                x,
                y
            );
        }
    }
    // And the repair touched only the band that moved.
    for y in 0..128 {
        for x in (0..64).chain(128..192) {
            assert_eq!(
                screen.pixel(x, y),
                tag(0x0066_0000, x, y),
                "pixel ({}, {}) outside the band that moved",
                x,
                y
            );
        }
    }
}

/// The source report has to fire on a present where NOTHING changed, because
/// that is the case it exists for: a black rectangle that just sits there is
/// black in both reads, so it differs in no band and the mismatch line never
/// fires. If this line shared the mismatch line's trigger, the static case --
/// the one Moebius is looking at -- would never be described at all.
#[test]
fn the_source_report_fires_even_when_nothing_changed() {
    let _screen = kms_emu::attach(192, 200);
    drm::set_present_probe_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    // Half opaque, half fully transparent black: a settled buffer that holds
    // a black region, which is exactly the shape being diagnosed.
    paint(&buf, |x, y| if x < 96 { tag(0x0044_0000, x, y) } else { 0 });
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x3333).expect("flip");

    assert_eq!(
        drm::probe_reports_for_test(),
        0,
        "nothing moved under the copy, so there is no mismatch to report"
    );
    // The range, not just the floor: a count above the budget would be
    // reads that wrote no line, and then this would pass without the line
    // this test is about ever having been written.
    let zero_reads = drm::zero_reports_for_test();
    assert!(
        (1..=drm::probe_report_budget_for_test()).contains(&zero_reads),
        "the source report has to fire anyway -- that is the whole point of \
         it -- and inside the budget, so it really wrote its line; got {}",
        zero_reads
    );
}

/// "Not one sampled pixel is black" is worth saying once. On Moebius's boot
/// it got said eight times and the budget was gone 27.2 s in, before the menu
/// whose black rectangle the flag exists to explain had been opened at all.
#[test]
fn a_source_with_no_black_is_described_once_and_then_stops() {
    let _screen = kms_emu::attach(64, 64);
    drm::set_present_probe_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 64);
    // Not one zero pixel anywhere: `tag` is seeded from a non-zero base, so
    // every pixel is opaque and distinct.
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    let fb = c.addfb2(&buf);

    for i in 0..4 {
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x4400 + i)
            .expect("flip");
    }

    assert_eq!(
        drm::zero_reports_for_test(),
        4,
        "every present still reads the source -- the budget cuts the lines, \
         not the reads"
    );
    assert_eq!(
        drm::clean_source_lines_for_test(),
        drm::clean_source_report_budget_for_test(),
        "four black-free frames say the same sentence, so only the baseline \
         line gets written"
    );
}

/// What Moebius's 27-sep 16:4x boot narrowed the search to. Its klog says the
/// source was clean in BOTH reads for 32 seconds and 320-odd frames, with the
/// "carries black" and "went black mid-copy" budgets sitting unspent -- so if
/// a black rectangle was on screen in that window, the kernel put it there.
///
/// This pins the invariant that claim rests on: a present copies every visible
/// pixel and INVENTS nothing. Black is the interesting failure, but the
/// assertion is exact equality, because the two ways the kernel could show
/// black it was not given are writing a zero and **not writing at all** -- and
/// an unwritten pixel keeps whatever the panel held, which at boot is black.
/// `UNTOUCHED` is the emulator's sentinel for "never written", so equality
/// catches that case by name instead of it hiding as a plausible colour.
///
/// Four geometries, because the two machines differ where it matters: QEMU
/// reports an UNPADDED pitch (7680 for 1920, exactly 4 bytes a pixel) and the
/// RTX's UEFI reports `PixelsPerScanLine` PADDED (2048 for a 1920-wide mode),
/// and `blit_from` picks a different right limit for each -- `padded_w` for row
/// copies, `visible_w` per pixel. Write-combining picks the store path, and on
/// x86_64 the WC one is the non-temporal loop for real.
#[test]
fn a_present_copies_every_visible_pixel_and_invents_no_black() {
    for &(w, h, pitch_px, wc) in &[
        (64u32, 32u32, 64u32, false),
        (64, 32, 64, true),
        // Padded, which is the real-hardware case and the one where the two
        // right limits stop agreeing.
        (40, 16, 64, true),
        (40, 16, 64, false),
    ] {
        let screen = kms_emu::attach_with(w, h, pitch_px, wc);
        let c = Client::open(0);
        let buf = c.create_dumb(w, h);
        // Every pixel opaque and distinct, and not one of them zero: `tag`
        // is seeded from a non-zero base, so a zero on the panel can only
        // have been invented by the copy.
        paint(&buf, |x, y| tag(0x0044_0000, x, y));
        let fb = c.addfb2(&buf);
        let src = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;

        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x9009).expect("flip");

        for y in 0..h {
            for x in 0..w {
                let got = screen.pixel(x, y);
                let want = src[y as usize * stride + x as usize];
                assert_eq!(
                    got,
                    want,
                    "{}x{} pitch {} wc {}: panel pixel ({}, {}) is {:#010x}, source \
                     says {:#010x}{}",
                    w,
                    h,
                    pitch_px,
                    wc,
                    x,
                    y,
                    got,
                    want,
                    if got == 0 {
                        " -- the copy INVENTED black"
                    } else if got == kms_emu::UNTOUCHED {
                        " -- never written, so the panel keeps what it held"
                    } else {
                        ""
                    }
                );
            }
        }
    }
}

/// The hole Moebius's 27-sep QEMU boot opened. Its klog said, on every frame
/// of a 65-second run, that not one sampled pixel was black -- AND said on
/// nearly every one of those same frames that the client was still writing
/// the buffer. Both are true at once, because the source is sampled BEFORE
/// the copy: a clear-to-black that lands mid-copy gets blitted to the screen
/// and the old source check never saw it. So "not handed over black" only
/// ever meant "not black when we looked".
#[test]
fn black_that_arrives_while_the_kernel_is_copying_gets_its_own_line() {
    let _screen = kms_emu::attach(192, 200);
    drm::set_present_probe_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    // Opaque and distinct everywhere: the FIRST read finds no black at all,
    // which is exactly what Moebius's boot reported.
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    let fb = c.addfb2(&buf);
    let pixels = map_dumb(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = pixels.as_mut_ptr() as usize;

    // The compositor clears a region to black while the copy is in flight --
    // a popup being repainted over the desktop.
    kms_emu::on_blit_band(move |band| {
        if band != 1 {
            return;
        }
        // SAFETY: the dumb buffer outlives this present, and nothing else
        // writes it while the hook runs.
        let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
        for y in 0..64usize {
            for x in 64..128usize {
                p[y * stride + x] = 0;
            }
        }
    });

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x7007).expect("flip");

    assert!(
        kms_emu::mid_blit_calls() >= 2,
        "the hook has to have fired mid-copy for this test to mean anything"
    );
    assert_eq!(
        drm::clean_source_lines_for_test(),
        1,
        "the first read still found no black, so the clean line is what the \
         OLD probe would have said -- and on its own it is misleading"
    );
    assert_eq!(
        drm::grew_source_lines_for_test(),
        1,
        "black that was not there before the copy and is there after was \
         handed over, just later than the sample, and that needs saying"
    );
}

/// And it must not double-report: a black rectangle that just sits there is
/// black in BOTH reads, so it is the source line's business and not this
/// one's. Without the `>` this would fire on every static black frame and
/// bury the frames where black actually arrived mid-copy.
#[test]
fn black_that_was_already_there_is_not_reported_as_having_arrived() {
    let _screen = kms_emu::attach(192, 200);
    drm::set_present_probe_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    // A black rectangle sitting still, and nothing touching the buffer
    // during the copy.
    paint(&buf, |x, y| if x < 64 { 0 } else { tag(0x0066_0000, x, y) });
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x8008).expect("flip");

    assert_eq!(
        drm::black_source_lines_for_test(),
        1,
        "the source carried black, so that line fires"
    );
    assert_eq!(
        drm::grew_source_lines_for_test(),
        0,
        "it was black before the copy too, so nothing arrived mid-copy"
    );
}

/// And the point of the split: the cheap answer cannot eat the budget the
/// deciding frames need. This is Moebius's boot in miniature -- a long
/// black-free run first, and THEN the frame that carries black.
#[test]
fn a_frame_that_carries_black_is_still_reported_after_a_long_black_free_run() {
    let _screen = kms_emu::attach(64, 64);
    drm::set_present_probe_enabled(true);
    let c = Client::open(0);

    let clean = c.create_dumb(64, 64);
    paint(&clean, |x, y| tag(0x0055_0000, x, y));
    let clean_fb = c.addfb2(&clean);
    // More than the whole shared budget, which is what makes this test bite:
    // with one budget between the two answers these presents spend it and the
    // frame below gets no line.
    let run = drm::probe_report_budget_for_test() + 2;
    for i in 0..run {
        c.page_flip(drm::SYNTH_CRTC_ID, clean_fb, 0x5500 + u64::from(i))
            .expect("flip");
    }
    assert_eq!(
        drm::black_source_lines_for_test(),
        0,
        "no frame carried black yet, so that budget is untouched"
    );

    // Now the frame that decides: a black region the compositor handed over.
    let black = c.create_dumb(64, 64);
    paint(
        &black,
        |x, y| if x < 32 { tag(0x0066_0000, x, y) } else { 0 },
    );
    let black_fb = c.addfb2(&black);
    c.page_flip(drm::SYNTH_CRTC_ID, black_fb, 0x6666)
        .expect("flip");

    assert_eq!(
        drm::black_source_lines_for_test(),
        1,
        "the frame that carries black has to get its line even after a long \
         black-free run -- that run is exactly what spent the shared budget \
         on Moebius's boot"
    );
}

/// The repair is one more writer that goes around the band skip, so it has to
/// make the skip forget -- the same rule the cursor, a damage box, a blank, a
/// VT and the copy engine all follow.
///
/// The skip's stored hash means "the panel holds these pixels in these rows".
/// A repair writes the panel with a plain `blit_chunked`, so after it the
/// panel holds what the REPAIR copied while the hash still describes what the
/// first copy put there. Leave that stale and a later frame whose pixels
/// happen to match the old hash gets skipped over a panel that does not hold
/// them -- stale pixels left on screen by the very path that exists to stop
/// leaving stale pixels on screen.
///
/// The control for this one is
/// `a_band_the_panel_already_holds_is_not_copied_again`: there a second
/// present of the same pixels skips every band. Here the first present
/// repairs, so the second must skip none.
#[test]
fn a_present_that_repaired_makes_the_next_one_copy_again() {
    let screen = kms_emu::attach(192, 200);
    drm::set_present_repair_enabled(true);
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    let fb = c.addfb2(&buf);
    let pixels = map_dumb(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = pixels.as_mut_ptr() as usize;

    // Present 1: the source moves under the copy, so the repair runs.
    kms_emu::on_blit_band(move |band| {
        if band != 1 {
            return;
        }
        // SAFETY: the dumb buffer outlives this present, and nothing else
        // writes it while the hook runs.
        let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
        for y in 0..128usize {
            for x in 64..128usize {
                p[y * stride + x] = tag(0x0099_0000, x as u32, y as u32);
            }
        }
    });
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x1111)
        .expect("first flip");
    assert!(
        drm::repair_rounds_for_test() >= 1,
        "the staging did not make the source move, so this test proves nothing"
    );

    // Present 2: nothing touches the source, and it is exactly what the panel
    // was last left holding. Without the invalidation the skip would believe
    // its own stale hash and skip.
    kms_emu::clear_mid_blit();
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0x2222)
        .expect("second flip");
    assert_eq!(
        drm::skipped_bands_for_test(),
        0,
        "a repair wrote the panel outside the skip, so the skip must have forgotten those rows"
    );
    // And the panel still agrees with the source everywhere, which is the
    // outcome the invalidation is protecting.
    for y in (0..200).step_by(17) {
        for x in (0..192).step_by(23) {
            let want = if y < 128 && (64..128).contains(&x) {
                tag(0x0099_0000, x, y)
            } else {
                tag(0x0055_0000, x, y)
            };
            assert_eq!(screen.pixel(x, y), want, "pixel ({}, {})", x, y);
        }
    }
}

/// A repair round copies the span that moved and NOT the whole window. The
/// claim that a round costs what actually moved rests on this, and reading the
/// destination afterwards cannot show it when the source agrees everywhere:
/// so the hook drops a sentinel on the panel outside the span, between the
/// present's own blit and the repair's, and a repair that repainted the whole
/// window would erase it.
#[test]
fn a_repair_round_copies_only_the_span_that_moved() {
    const SENTINEL: u32 = 0xDEAD_BEEF;
    let screen = kms_emu::attach(192, 200);
    let pitch_px = screen.pitch_px();
    drm::set_present_repair_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    paint(&buf, |x, y| tag(0x0088_0000, x, y));
    let fb = c.addfb2(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = map_dumb(&buf).as_mut_ptr() as usize;

    kms_emu::on_blit_band(move |band| match band {
        // Second band of the present's own blit: move one band of the source
        // after the rows carrying it have already gone to the panel.
        1 => {
            // SAFETY: the dumb buffer outlives this present.
            let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
            for y in 0..128usize {
                for x in 64..128usize {
                    p[y * stride + x] = tag(0x0099_0000, x as u32, y as u32);
                }
            }
        }
        // First band of the repair's blit: mark the panel outside the span.
        2 => {
            for y in 0..128u32 {
                kms_emu::poke(pitch_px, 0, y, SENTINEL);
                kms_emu::poke(pitch_px, 191, y, SENTINEL);
            }
        }
        _ => {}
    });

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    assert!(
        drm::repair_rounds_for_test() >= 1,
        "a source that moved under the copy has to cost at least one repair \
         round; the blit took {} bands",
        kms_emu::mid_blit_calls()
    );
    for y in 0..128 {
        assert_eq!(
            (screen.pixel(0, y), screen.pixel(191, y)),
            (SENTINEL, SENTINEL),
            "row {}: the repair repainted columns outside the span that moved",
            y
        );
    }
    // And the span itself still got repaired.
    assert_eq!(screen.pixel(64, 0), tag(0x0099_0000, 64, 0));
}

/// With the probe armed and the repair NOT armed the bracket IS taken -- the
/// probe needs it -- so this is the one arrangement where the repair's own
/// check is the only thing standing between a measurement and a copy nobody
/// asked for. It must stay a measurement.
#[test]
fn the_probe_alone_measures_and_does_not_repair() {
    let screen = kms_emu::attach(192, 200);
    drm::set_present_probe_enabled(true);
    // Repair is ON by default; this test is specifically the probe WITHOUT
    // the repair pass, so disarm it for the duration.
    drm::set_present_repair_enabled(false);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    paint(&buf, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = map_dumb(&buf).as_mut_ptr() as usize;

    kms_emu::on_blit_band(move |band| {
        if band != 1 {
            return;
        }
        // SAFETY: the dumb buffer outlives this present.
        let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
        for y in 0..128usize {
            for x in 64..128usize {
                p[y * stride + x] = tag(0x00BB_0000, x as u32, y as u32);
            }
        }
    });

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    assert_eq!(
        drm::repair_rounds_for_test(),
        0,
        "the probe must not repair -- it reports"
    );
    assert_eq!(
        screen.pixel(64, 0),
        tag(0x00AA_0000, 64, 0),
        "the panel keeps what the copy caught"
    );
}

/// A source that is still moving during the repair costs a SECOND round, and
/// the second round is the one that puts the latest pixels up. Without this
/// the budget could be one and nothing would notice.
#[test]
fn a_source_still_moving_during_the_repair_costs_a_second_round() {
    let screen = kms_emu::attach(192, 200);
    drm::set_present_repair_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    paint(&buf, |x, y| tag(0x00CC_0000, x, y));
    let fb = c.addfb2(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = map_dumb(&buf).as_mut_ptr() as usize;

    // Band 1 moves the span during the present's own blit; band 2 moves it
    // AGAIN during the first repair round, so only a second round can catch up.
    kms_emu::on_blit_band(move |band| {
        let base = match band {
            1 => 0x00DD_0000u32,
            2 => 0x00EE_0000u32,
            _ => return,
        };
        // SAFETY: the dumb buffer outlives this present.
        let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
        for y in 0..128usize {
            for x in 64..128usize {
                p[y * stride + x] = tag(base, x as u32, y as u32);
            }
        }
    });

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    assert_eq!(
        drm::repair_rounds_for_test(),
        2,
        "a source that moved again under the repair owes a second round"
    );
    assert_eq!(
        screen.pixel(64, 0),
        tag(0x00EE_0000, 64, 0),
        "the second round has to put the latest pixels up"
    );
}

/// The same race with the repair NOT armed: the panel keeps the stale pixels.
/// This is the defect itself, pinned, so the test above cannot pass for some
/// reason other than the repair -- and so that turning the flag off is known
/// to still mean what it says.
#[test]
fn without_the_repair_the_stale_pixels_stay_on_the_panel() {
    let screen = kms_emu::attach(192, 200);
    // The repair is ON by default since `drm.present_repair` was flipped
    // (it is `=off` that disarms it now), and this test IS the unrepaired
    // race, so it has to disarm the flag itself -- as its neighbours do.
    drm::set_present_repair_enabled(false);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 200);
    paint(&buf, |x, y| tag(0x0066_0000, x, y));
    let fb = c.addfb2(&buf);
    let pixels = map_dumb(&buf);
    let stride = (buf.pitch / 4) as usize;
    let late = pixels.as_mut_ptr() as usize;

    kms_emu::on_blit_band(move |band| {
        if band != 1 {
            return;
        }
        // SAFETY: as above.
        let p = unsafe { core::slice::from_raw_parts_mut(late as *mut u32, stride * 200) };
        for y in 0..128usize {
            for x in 64..128usize {
                p[y * stride + x] = tag(0x0077_0000, x as u32, y as u32);
            }
        }
    });

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    assert_eq!(
        drm::repair_rounds_for_test(),
        0,
        "the repair must not run once the cmdline has disarmed it"
    );
    assert_eq!(
        screen.pixel(64, 0),
        tag(0x0066_0000, 64, 0),
        "with no repair the panel keeps what the copy caught"
    );
}

/// The repair pass must be free on a buffer nobody is writing: zero extra
/// rounds, and the frame on screen is exactly the frame in the buffer. Every
/// claim about what the repair costs rests on this -- a pass that ran rounds
/// on a settled present would be paying on every frame of a healthy desktop.
#[test]
fn the_repair_pass_runs_no_rounds_on_a_settled_buffer() {
    let screen = kms_emu::attach(192, 8);
    drm::set_present_repair_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 8);
    paint(&buf, |x, y| tag(0x0033_0000, x, y));
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    assert_eq!(
        drm::repair_rounds_for_test(),
        0,
        "a buffer nobody is writing must cost no repair rounds"
    );
    for y in 0..8 {
        for x in 0..192 {
            assert_eq!(
                screen.pixel(x, y),
                tag(0x0033_0000, x, y),
                "pixel ({}, {}) with the repair armed",
                x,
                y
            );
        }
    }
}

// --- the unchanged-band skip (`drm.present_skip`) ---
//
// Every one of these turns on a sentinel: a value poked straight onto the
// emulated panel between two presents. A band the present copies overwrites
// it; a band the present skips leaves it there. That is the only way to see
// the difference from outside, because the source says the same thing either
// way -- and it is also the shape of the defect, since a skip that is wrong
// shows up as exactly such a leftover pixel.

/// Presenting the same buffer twice copies nothing the second time. This is
/// the whole point: on real hardware CPU stores into the console GPU's BAR1
/// serve at ~42 MB/s, so a frame nobody changed costs ~99 ms of pure waste.
#[test]
fn a_band_the_panel_already_holds_is_not_copied_again() {
    const SENTINEL: u32 = 0x0BAD_F00D;
    let screen = kms_emu::attach(64, 48);
    let pitch_px = screen.pitch_px();
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0011_0000, x, y));
    let fb = c.addfb2(&buf);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
    assert_eq!(
        drm::skipped_bands_for_test(),
        0,
        "the first present of a boot knows nothing and must copy everything"
    );
    // One pixel of each band, marked on the panel and not in the buffer.
    for b in 0..3u32 {
        kms_emu::poke(pitch_px, 0, b * 16, SENTINEL);
    }

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(
        drm::skipped_bands_for_test(),
        3,
        "the same buffer again: every band is already up there"
    );
    for b in 0..3u32 {
        assert_eq!(
            screen.pixel(0, b * 16),
            SENTINEL,
            "band {} was copied again although nothing changed",
            b
        );
    }
}

/// And without the flag the same two presents copy the frame twice, which is
/// what fixes the behaviour to the flag rather than to the state: the
/// sentinel goes away.
#[test]
fn without_the_skip_every_band_is_copied_again() {
    const SENTINEL: u32 = 0x0BAD_BEEF;
    let screen = kms_emu::attach(64, 48);
    let pitch_px = screen.pitch_px();
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0022_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
    kms_emu::poke(pitch_px, 0, 16, SENTINEL);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(drm::skipped_bands_for_test(), 0);
    assert_eq!(
        screen.pixel(0, 16),
        tag(0x0022_0000, 0, 16),
        "with no skip armed the present must copy every band"
    );
}

/// A band whose pixels moved is copied; the bands around it are not. The
/// saving and the correctness are the same claim, and this is where they meet:
/// a desktop changes a few rows per frame and must still show them.
#[test]
fn only_the_band_that_changed_is_copied() {
    const SENTINEL: u32 = 0x0FEE_1DEA;
    let screen = kms_emu::attach(64, 48);
    let pitch_px = screen.pitch_px();
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0033_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    // Mark all three bands on the panel, then change one pixel of the middle
    // one in the BUFFER.
    for b in 0..3u32 {
        kms_emu::poke(pitch_px, 0, b * 16, SENTINEL);
    }
    let stride = (buf.pitch / 4) as usize;
    map_dumb(&buf)[20 * stride + 5] = 0x00C0_FFEE;

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(
        drm::skipped_bands_for_test(),
        2,
        "one band moved, so two were already on the panel"
    );
    assert_eq!(
        screen.pixel(5, 20),
        0x00C0_FFEE,
        "the pixel that changed has to reach the panel"
    );
    assert_eq!(
        screen.pixel(0, 16),
        tag(0x0033_0000, 0, 16),
        "the band that changed is copied whole, sentinel and all"
    );
    assert_eq!(screen.pixel(0, 0), SENTINEL, "band 0 did not change");
    assert_eq!(screen.pixel(0, 32), SENTINEL, "band 2 did not change");
}

/// The cursor is composited ON TOP of the frame, so the rows it covers are
/// not the frame's pixels and the next present must copy them again. Without
/// that the pointer's previous position stays on screen until something else
/// happens to change those rows -- the very defect this path exists to stop
/// causing.
#[test]
fn the_rows_under_the_cursor_are_copied_again() {
    const SENTINEL: u32 = 0x0C0F_FEE0;
    let screen = kms_emu::attach(64, 48);
    let pitch_px = screen.pitch_px();
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0044_0000, x, y));
    let fb = c.addfb2(&buf);
    // An opaque 8x8 pointer parked inside band 0.
    let cur = c.create_dumb(8, 8);
    paint(&cur, |_, _| 0xFF00_FF00);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 0, 0);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    // Row 12 is in the pointer's band (rows 0..16) but BELOW the 8-row
    // pointer itself: a sentinel under the pointer would be overwritten by
    // the pointer's own pixels and say nothing about the band.
    kms_emu::poke(pitch_px, 0, 12, SENTINEL);
    kms_emu::poke(pitch_px, 0, 32, SENTINEL);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(
        screen.pixel(0, 12),
        tag(0x0044_0000, 0, 12),
        "the band under the pointer must be copied again"
    );
    assert_eq!(
        screen.pixel(0, 32),
        SENTINEL,
        "and a band nowhere near the pointer must not"
    );
}

/// Two bands change with a skipped band between them, and BOTH have to reach
/// the panel. This is the one that says the dirty run is closed when a skipped
/// band interrupts it: a run left open across the gap blits the right number
/// of rows from the wrong place and the second changed band never arrives.
#[test]
fn two_bands_with_a_gap_between_them_both_arrive() {
    const SENTINEL: u32 = 0x0A11_0A11;
    let screen = kms_emu::attach(64, 48);
    let pitch_px = screen.pitch_px();
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0077_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    kms_emu::poke(pitch_px, 0, 20, SENTINEL);
    let stride = (buf.pitch / 4) as usize;
    {
        let m = map_dumb(&buf);
        m[4 * stride + 1] = 0x0011_1111;
        m[36 * stride + 2] = 0x0022_2222;
    }

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(
        drm::skipped_bands_for_test(),
        1,
        "the band between the two that moved is the only one still up there"
    );
    assert_eq!(
        screen.pixel(1, 4),
        0x0011_1111,
        "the first band that moved has to arrive"
    );
    assert_eq!(
        screen.pixel(2, 36),
        0x0022_2222,
        "and so does the one past the gap"
    );
    assert_eq!(
        screen.pixel(0, 20),
        SENTINEL,
        "the band between them was not copied"
    );
}

/// A damage box does not take the skip at all. The box is already the
/// client's own answer to "what changed", and the skip's state is indexed
/// from its window's top row -- so a box would have it describing rows by a
/// different origin than the cursor invalidation uses. The pixels look the
/// same either way, which is why this asserts on which path ran.
#[test]
fn a_damage_box_does_not_take_the_skip() {
    let screen = kms_emu::attach(64, 48);
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0088_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
    let after_frame = drm::skip_presents_for_test();
    assert_eq!(after_frame, 1, "a whole frame takes the skip");

    paint(&buf, |x, y| tag(0x0099_0000, x, y));
    dirtyfb(&c, fb, &[clip(0, 16, 64, 32)]);

    assert_eq!(
        drm::skip_presents_for_test(),
        after_frame,
        "a damage box must not go through the skip"
    );
    assert_eq!(
        screen.pixel(0, 20),
        tag(0x0099_0000, 0, 20),
        "and the box is still copied"
    );
    assert_eq!(
        screen.pixel(0, 4),
        tag(0x0088_0000, 0, 4),
        "while the rows outside it keep what they had"
    );
}

/// A damage box copies part of the frame without the skip's knowledge, so
/// everything it remembers stops being true. Keeping the hashes would leave
/// the rows the box did NOT cover claiming to hold pixels that a later
/// present then refuses to copy.
#[test]
fn a_damage_box_forgets_what_the_panel_held() {
    const SENTINEL: u32 = 0x0D06_0D06;
    let screen = kms_emu::attach(64, 48);
    let pitch_px = screen.pitch_px();
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    dirtyfb(&c, fb, &[clip(0, 0, 64, 16)]);
    kms_emu::poke(pitch_px, 0, 32, SENTINEL);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(
        drm::skipped_bands_for_test(),
        0,
        "after a damage box the skip knows nothing again"
    );
    assert_eq!(
        screen.pixel(0, 32),
        tag(0x0055_0000, 0, 32),
        "a band the skip still claimed was left stale on the panel"
    );
}

/// Blanking paints the panel black, which is not the frame's pixels. A skip
/// that kept its hashes across a blank would leave the screen black: every
/// band would match, so the present that comes back would copy nothing.
#[test]
fn a_blank_forgets_what_the_panel_held() {
    let screen = kms_emu::attach(64, 48);
    drm::set_present_skip_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(64, 48);
    paint(&buf, |x, y| tag(0x0066_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    drm::set_crtc_blanked(true);
    assert_eq!(screen.pixel(0, 32), 0, "blanking paints the panel black");
    drm::set_crtc_blanked(false);

    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00E).expect("flip");

    assert_eq!(
        drm::skipped_bands_for_test(),
        0,
        "nothing is known about a panel that was blanked"
    );
    for y in [0u32, 16, 32, 47] {
        assert_eq!(
            screen.pixel(0, y),
            tag(0x0066_0000, 0, y),
            "row {} stayed black after the blank",
            y
        );
    }
}

/// And the repair pass changes nothing about a damage box on the buffer the
/// panel already carries: the box is still the only thing copied. Arming a
/// repair must not quietly turn every present into a whole frame.
#[test]
fn the_repair_pass_does_not_widen_a_damage_box() {
    let screen = kms_emu::attach(192, 8);
    drm::set_present_repair_enabled(true);
    let c = Client::open(0);
    let buf = c.create_dumb(192, 8);
    paint(&buf, |x, y| tag(0x0044_0000, x, y));
    let fb = c.addfb2(&buf);
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");

    // Repaint the whole buffer, then damage only one band of it.
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    dirtyfb(&c, fb, &[clip(64, 0, 128, 8)]);

    assert_eq!(drm::repair_rounds_for_test(), 0);
    for y in 0..8 {
        for x in 0..192 {
            let want = if (64..128).contains(&x) {
                tag(0x0055_0000, x, y)
            } else {
                tag(0x0044_0000, x, y)
            };
            assert_eq!(
                screen.pixel(x, y),
                want,
                "pixel ({}, {}) -- the repair widened the box",
                x,
                y
            );
        }
    }
}

#[test]
fn a_damage_box_on_a_fresh_buffer_puts_the_whole_frame_up() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    // The frame on the panel.
    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb_a = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_a, 64, 16);
    drain_completions(&c);
    assert_eq!(screen.pixel(0, 0), tag(0x00AA_0000, 0, 0));
    assert_eq!(drm::panel_fb_for_test(), fb_a);

    // The next swapchain buffer. Every pixel of it differs from the frame up.
    let b = c.create_dumb(64, 16);
    paint(&b, |x, y| tag(0x0011_0000, x, y));
    let fb_b = c.addfb2(&b);

    dirtyfb(&c, fb_b, &[clip(8, 4, 24, 8)]);

    assert_eq!(
        screen.pixel(10, 5),
        tag(0x0011_0000, 10, 5),
        "the damaged region itself did not reach the panel"
    );
    assert_eq!(
        screen.pixel(40, 12),
        tag(0x0011_0000, 40, 12),
        "outside the box the panel still carries the PREVIOUS framebuffer, so              it is holding two frames at once -- and `repaint_for_cursor` reads              that region back from `crtc_fb` on the next pointer move"
    );
    assert_eq!(drm::panel_fb_for_test(), fb_b);

    c.rmfb(fb_a).expect("RMFB a");
    c.rmfb(fb_b).expect("RMFB b");
    c.destroy_dumb(a.handle).expect("DESTROY_DUMB a");
    c.destroy_dumb(b.handle).expect("DESTROY_DUMB b");
}

/// And the optimisation is still there for the case it exists for: the client
/// re-presents the framebuffer the panel already carries, so the pixels
/// outside the box really are the ones on screen. Copying the whole frame
/// here is the 8.3 MB of CPU stores per blinking caret that the damage path
/// was added to avoid, so "promote everything" would not be a fix.
#[test]
fn a_damage_box_on_the_buffer_already_up_still_copies_only_the_box() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    drain_completions(&c);

    // Repaint the WHOLE buffer but report one box, which is the client lying
    // about its damage -- and the kernel is entitled to believe it here.
    paint(&a, |x, y| tag(0x0011_0000, x, y));
    dirtyfb(&c, fb, &[clip(8, 4, 24, 8)]);

    assert_eq!(
        screen.pixel(10, 5),
        tag(0x0011_0000, 10, 5),
        "the box was not copied at all"
    );
    assert_eq!(
        screen.pixel(40, 12),
        tag(0x00AA_0000, 40, 12),
        "the whole frame was copied, so the damage path no longer shrinks              anything"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
}

/// Blanking paints the panel black, so black is what a damage box would be
/// leaving in place. Un-blanking happens on the next present, at the top of
/// `present_now_checked` -- and if that present is a damage box the screen
/// stays black with one rectangle of desktop in it.
#[test]
fn the_present_that_unblanks_does_not_leave_one_rectangle_on_black() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    drain_completions(&c);

    drm::set_crtc_blanked(true);
    assert_eq!(screen.pixel(40, 12), 0, "blanking left the panel lit");
    assert_eq!(
        drm::panel_fb_for_test(),
        0,
        "black pixels are not this framebuffer's pixels"
    );

    dirtyfb(&c, fb, &[clip(8, 4, 24, 8)]);
    assert_eq!(
        screen.pixel(40, 12),
        tag(0x00AA_0000, 40, 12),
        "the panel is still black everywhere the box did not touch"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
}

/// A present a pause acknowledged without drawing does not make the panel
/// carry that framebuffer -- it carries the one from before, which is the
/// whole point of `SCANOUT_STALE`. Recording it here would tell the next
/// damage box it may keep its region, on a panel a frame behind.
#[test]
fn a_present_a_pause_acknowledged_does_not_claim_the_panel() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb_a = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_a, 64, 16);
    drain_completions(&c);

    let b = c.create_dumb(64, 16);
    paint(&b, |x, y| tag(0x0011_0000, x, y));
    let fb_b = c.addfb2(&b);

    drm::set_scanout_paused(true);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_b, 21).expect("flip");
    drain_completions(&c);
    assert_eq!(
        screen.pixel(0, 0),
        tag(0x00AA_0000, 0, 0),
        "a paused present drew"
    );
    assert_eq!(
        drm::panel_fb_for_test(),
        fb_a,
        "the panel was credited with a frame nobody drew"
    );

    // Resuming puts the whole frame up, which is what makes the panel carry
    // it -- and only then is a box on it meaningful again.
    drm::set_scanout_paused(false);
    assert_eq!(screen.pixel(0, 0), tag(0x0011_0000, 0, 0));
    assert_eq!(drm::panel_fb_for_test(), fb_b);

    c.rmfb(fb_a).expect("RMFB a");
    c.rmfb(fb_b).expect("RMFB b");
    c.destroy_dumb(a.handle).expect("DESTROY_DUMB a");
    c.destroy_dumb(b.handle).expect("DESTROY_DUMB b");
}

/// Retiring the framebuffer the panel carries forgets it. Ids are handed out
/// again, so without this a later buffer landing on the same number would
/// inherit "already on screen" and have its first box honoured against a
/// frame that is not its own.
#[test]
fn retiring_the_framebuffer_on_the_panel_forgets_it() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    drain_completions(&c);
    assert_eq!(drm::panel_fb_for_test(), fb);

    c.rmfb(fb).expect("RMFB");
    assert_eq!(
        drm::panel_fb_for_test(),
        0,
        "a retired id is still recorded as the frame on the panel"
    );

    c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
}

/// A present the console swallowed leaves the panel carrying nothing. While a
/// text VT is foreground the compositor's pixels are dropped -- reported as
/// complete so its frame loop keeps running -- and the console prints over the
/// last frame. So when the graphics VT comes back, the panel is not showing
/// any framebuffer, and the first present's damage box would paint one
/// rectangle of desktop into a screen full of console text.
#[test]
fn a_present_the_console_swallowed_leaves_the_panel_carrying_nothing() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    drain_completions(&c);
    assert_eq!(drm::panel_fb_for_test(), fb);

    // A text VT is foreground: the compositor owns VT 7, the user is on 1.
    drm::set_graphics_vt_for_test(Some(7));
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 31)
        .expect("a suppressed flip is still reported complete");
    drain_completions(&c);
    assert_eq!(
        drm::panel_fb_for_test(),
        0,
        "the panel is still credited with a frame the console is printing over"
    );

    // Back to the desktop. Repaint the buffer so a box on it would be
    // visibly different from what is up, then damage one corner of it.
    drm::set_graphics_vt_for_test(None);
    paint(&a, |x, y| tag(0x0033_0000, x, y));
    dirtyfb(&c, fb, &[clip(0, 0, 4, 4)]);
    assert_eq!(
        screen.pixel(40, 12),
        tag(0x0033_0000, 40, 12),
        "the first present after the VT came back honoured its box, so the \
         screen is console text with one rectangle of desktop in it"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
}

/// A present that could not put pixels anywhere does not get to say the panel
/// carries its framebuffer. Crediting it would hand the next damage box a
/// reference frame that was never drawn.
#[test]
fn a_present_that_failed_does_not_claim_the_panel() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let a = c.create_dumb(64, 16);
    paint(&a, |x, y| tag(0x00AA_0000, x, y));
    let fb = c.addfb2(&a);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);
    drain_completions(&c);
    assert_eq!(drm::panel_fb_for_test(), fb);

    assert!(
        drm::present_now_checked(0x0BAD_F00D, drm::SYNTH_CRTC_ID, None).is_err(),
        "an id nobody registered must not present"
    );
    assert_eq!(
        drm::panel_fb_for_test(),
        fb,
        "a present that put no pixels anywhere was credited with the panel"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(a.handle).expect("DESTROY_DUMB");
}

/// Does the cursor patch read past the framebuffer's own right edge?
///
/// The call site's comment says it clips "to what the framebuffer covers
/// (`fb_width`/`fb_height`), not to the screen: a client fb narrower or
/// shorter than the display would otherwise have the patch read past the end
/// of a row -- the next row's pixels -- and paint that onto the scanout as a
/// shifted square trailing the pointer". The height half does clip to `fh`.
/// The width half never mentions `fw` again: it bounds `x` at the row PITCH,
/// which is >= `fw` by construction. So a framebuffer narrower than its own
/// pitch has the columns in between read and painted.
#[test]
fn the_cursor_patch_does_not_paint_the_framebuffers_row_padding() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    // A 40-pixel buffer registered as a 32-pixel-wide framebuffer: eight
    // columns of row padding, and the screen is wider than either.
    let buf = c.create_dumb(40, 16);
    paint(&buf, |x, y| tag(0x0055_0000, x, y));
    let fb = c.addfb2_narrow(&buf, 32);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 32, 16);
    drain_completions(&c);

    // Whatever the present put on screen for the padding columns is the
    // baseline: the cursor must not change it.
    let before: alloc::vec::Vec<u32> = (32..48u32).map(|x| screen.pixel(x, 4)).collect();

    // An 8x8 opaque pointer at the framebuffer's right edge: its patch
    // reaches columns 24..32, and the write-combining widening takes the
    // read out to the row pitch.
    set_cursor(&c, drm::SYNTH_CRTC_ID, buf.handle, 8, 8, 24, 0);
    move_cursor(&c, drm::SYNTH_CRTC_ID, 24, 2);

    let after: alloc::vec::Vec<u32> = (32..48u32).map(|x| screen.pixel(x, 4)).collect();
    assert_eq!(
        before, after,
        "the pointer painted the framebuffer's off-screen row padding onto \
         the visible screen, past the framebuffer's own right edge"
    );

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The probe must be able to say "nothing wrote this window" -- on a buffer
/// nobody is writing.
///
/// That is the half of the diagnostic that is easy to get wrong and fatal to
/// get wrong: if a settled present reports, every frame reports, and the
/// finding it exists to deliver is buried in noise. Here nothing but the
/// test touches the dumb buffer between the two reads, so the honest answer
/// is silence.
#[test]
fn an_armed_probe_says_nothing_about_a_buffer_nobody_is_writing() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let first = c.create_dumb(64, 16);
    paint(&first, |x, y| tag(0x0011_0000, x, y));
    let fb_first = c.addfb2(&first);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_first, 64, 16);
    drain_completions(&c);

    drm::set_present_probe_enabled(true);
    let reports_before = drm::probe_reports_for_test();

    let second = c.create_dumb(64, 16);
    paint(&second, |x, y| tag(0x0022_0000, x, y));
    let fb_second = c.addfb2(&second);
    c.page_flip(drm::SYNTH_CRTC_ID, fb_second, 7).expect("flip");
    drain_completions(&c);
    // And the damage path too, because that is the one labwc drives.
    dirtyfb(&c, fb_second, &[clip(8, 4, 24, 8)]);

    assert_eq!(
        drm::probe_reports_for_test(),
        reports_before,
        "a settled buffer was reported as changing under the blit, so every \
         frame will report and the log will say nothing"
    );

    c.rmfb(fb_first).expect("RMFB");
    c.rmfb(fb_second).expect("RMFB");
    c.destroy_dumb(first.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(second.handle).expect("DESTROY_DUMB");
}

/// And it must not change what reaches the panel. A diagnostic that alters
/// the thing it measures is not one: the probe reads the window twice and
/// invalidates it in between, and the pixels on screen have to be exactly
/// the ones an unarmed boot would have put there.
#[test]
fn an_armed_probe_puts_the_same_pixels_on_the_panel() {
    let _screen = kms_emu::attach(64, 16);
    let c = Client::open(0);

    let fb_buf = c.create_dumb(64, 16);
    paint(&fb_buf, |x, y| tag(0x0033_0000, x, y));
    let fb_id = c.addfb2(&fb_buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_id, 64, 16);
    drain_completions(&c);

    drm::set_present_probe_enabled(true);
    // Repaint to a value the panel does not already hold, then present a
    // damage box over part of it -- the shape the probe is armed for.
    paint(&fb_buf, |x, y| tag(0x0044_0000, x, y));
    // `clip` is a DRM clip rect: x1, y1, x2, y2, not x/y/w/h.
    dirtyfb(&c, fb_id, &[clip(5, 3, 26, 12)]);

    for y in 3..12u32 {
        for x in 5..26u32 {
            assert_eq!(
                _screen.pixel(x, y),
                tag(0x0044_0000, x, y),
                "the probe changed what the present wrote"
            );
        }
    }

    c.rmfb(fb_id).expect("RMFB");
    c.destroy_dumb(fb_buf.handle).expect("DESTROY_DUMB");
}

// -----------------------------------------------------------------------
// The desktop, simulated: labwc presenting while the pointer moves.
//
// The garbage this exists for cannot be photographed into a test. What CAN
// be written down is the contract the screen owes its user: it shows the
// frame the compositor last presented, with the pointer on top, and nothing
// else. Every step below checks the WHOLE panel against that, so a stray
// rectangle is caught wherever it lands and whatever it holds -- a piece of
// an older frame, a piece of one not presented yet, or black.
// -----------------------------------------------------------------------

/// One pixel of the desktop labwc composes for frame `n`.
///
/// The frame number is in every pixel and the alpha byte is always `0xff`,
/// which buys two things. A window of another frame pasted into this one
/// does not resemble anything here, so it is caught by value and not merely
/// by position. And `0x00000000` -- the black of the rectangles -- cannot
/// come out of any legitimate scene, so if the panel holds it, the kernel
/// put it there.
fn desktop_px(n: u32, x: u32, y: u32) -> u32 {
    0xFF00_0000 | ((n & 0xff) << 16) | ((y & 0xff) << 8) | (x & 0xff)
}

/// What the screen ought to show, kept beside the emulated panel.
struct Panel {
    w: u32,
    h: u32,
    /// The frame the compositor last PRESENTED, pixel for pixel. Not the
    /// buffer it currently holds: a buffer being drawn into is not a frame
    /// anybody has asked for.
    scene: alloc::vec::Vec<u32>,
    /// Where the pointer is drawn and how big it is. The position is signed
    /// because a pointer really does hang off the left and top edges: the
    /// kernel clips it, and a model that could not express that would never
    /// ask what the clipping does.
    cursor: Option<(i32, i32, u32, u32)>,
    /// The pointer image, `bmp_w` pixels per row, alpha `0xff` or `0x00`
    /// only -- a partly transparent pointer would need the blend written
    /// out twice, and what these tests are about is what shows THROUGH it.
    bmp: alloc::vec::Vec<u32>,
    bmp_w: u32,
}

impl Panel {
    fn new(w: u32, h: u32, frame: u32, bmp: alloc::vec::Vec<u32>, bmp_w: u32) -> Self {
        let mut p = Panel {
            w,
            h,
            scene: alloc::vec::Vec::new(),
            cursor: None,
            bmp,
            bmp_w,
        };
        p.present(frame);
        p
    }

    /// The panel was painted one colour, by a blank, and nothing is drawn
    /// on top of it.
    fn fill(&mut self, px: u32) {
        self.scene.clear();
        self.scene.resize((self.w * self.h) as usize, px);
        self.cursor = None;
    }

    /// A damage box of frame `n` went up and the rest of the panel kept the
    /// frame that was already there -- the panel holding two frames at once,
    /// on purpose, which is legitimate only while the box really is all that
    /// changed.
    fn present_box(&mut self, n: u32, x: u32, y: u32, w: u32, h: u32) {
        for py in y..(y + h).min(self.h) {
            for px in x..(x + w).min(self.w) {
                self.scene[(py * self.w + px) as usize] = desktop_px(n, px, py);
            }
        }
    }

    /// The compositor put frame `n` up, whole.
    fn present(&mut self, n: u32) {
        self.scene.resize((self.w * self.h) as usize, 0);
        for y in 0..self.h {
            for x in 0..self.w {
                self.scene[(y * self.w + x) as usize] = desktop_px(n, x, y);
            }
        }
    }

    fn want(&self, x: u32, y: u32) -> u32 {
        if let Some((cx, cy, cw, ch)) = self.cursor {
            let (dx, dy) = (x as i64 - cx as i64, y as i64 - cy as i64);
            if dx >= 0 && dx < cw as i64 && dy >= 0 && dy < ch as i64 {
                let s = self.bmp[(dy as u32 * self.bmp_w + dx as u32) as usize];
                if s >> 24 != 0 {
                    return s | 0xFF00_0000;
                }
            }
        }
        self.scene[(y * self.w + x) as usize]
    }

    /// Compare every visible pixel, and say in the failure which of the two
    /// ways it is wrong -- the two need work in opposite places.
    fn check(&self, screen: &kms_emu::Screen, step: &str) {
        for y in 0..self.h {
            for x in 0..self.w {
                let want = self.want(x, y);
                let got = screen.pixel(x, y);
                if got == want {
                    continue;
                }
                let how = if got == 0 {
                    ", and it is BLACK: no scene of this desktop holds a \
                     zero pixel, so the kernel put it there"
                } else if got == UNTOUCHED {
                    ", and nothing ever wrote it"
                } else {
                    ", which is a piece of another frame"
                };
                panic!(
                    "{}: pixel ({}, {}) reads {:#010x} and the frame on \
                     screen says {:#010x}{}",
                    step, x, y, got, want, how
                );
            }
        }
    }
}

/// A pointer image: opaque in a cross, transparent in the corners, so the
/// desktop shows through it and a wrong read under the pointer is visible
/// rather than hidden behind an opaque square.
fn pointer_bitmap(w: u32, h: u32) -> alloc::vec::Vec<u32> {
    let mut v = alloc::vec::Vec::new();
    for y in 0..h {
        for x in 0..w {
            let on = x >= w / 4 && x < w - w / 4 || y >= h / 4 && y < h - h / 4;
            v.push(if on { 0xFFFF_FFFF } else { 0x0000_0000 });
        }
    }
    v
}

/// A damage box must not let the pointer paste the rest of the client's
/// buffer onto the screen.
///
/// The pointer is composited on top of every present, over a window widened
/// to whole write-combining lines, so its window routinely reaches outside a
/// damage box. Inside the box the client's buffer and the panel hold the same
/// pixels. Outside it they do not: the panel holds the frame that was there
/// before, and the buffer holds whatever the client has in it now -- for a
/// compositor that redraws only its damage an older frame, and for one that
/// has begun the next frame transparent black. Taking the buffer's pixels
/// there puts a rectangle the client never declared on the screen, right
/// where the pointer is.
///
/// Same fault as the one `CursorUnder` is named for, reached through the
/// present instead of through a pointer move, and found by the damage-box
/// steps of the soak below rather than by anybody thinking of it.
#[test]
fn a_damage_box_must_not_let_the_pointer_paste_the_rest_of_the_clients_buffer() {
    const W: u32 = 120;
    const H: u32 = 96;
    const CUR: u32 = 16;
    let screen = kms_emu::attach_with(W, H, 128, true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    paint(&buf, |x, y| desktop_px(1, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
    drain_completions(&c);

    let bmp = pointer_bitmap(CUR, CUR);
    let cur = c.create_dumb(CUR, CUR);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    // Near the top, far above the damage box: the pointer's window lies
    // wholly outside what the present is about to copy.
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, CUR, CUR, 38, 4);
    let mut panel = Panel::new(W, H, 1, bmp.clone(), CUR);
    panel.cursor = Some((38, 4, CUR, CUR));
    panel.check(&screen, "frame 1 with the pointer on it");

    // The client redraws ONE box and starts the next frame everywhere else,
    // which is a renderer clearing to transparent black. Then it declares
    // just the box.
    let (bx, by, bw, bh) = (32u32, 48u32, 64u32, 32u32);
    paint(&buf, |x, y| {
        if (bx..bx + bw).contains(&x) && (by..by + bh).contains(&y) {
            desktop_px(2, x, y)
        } else {
            0x0000_0000
        }
    });
    dirtyfb(
        &c,
        fb,
        &[clip(
            bx as u16,
            by as u16,
            (bx + bw) as u16,
            (by + bh) as u16,
        )],
    );
    drain_completions(&c);

    panel.present_box(2, bx, by, bw, bh);
    panel.check(&screen, "the damage box went up and the pointer did not");
    // And it is the save that did it, not a lucky agreement between two
    // sources: every pixel of this window came from outside the box.
    assert!(
        drm::cursor_px_from_save_for_test() > 0,
        "the pointer composited without reading the save, so this test \
         passed for some other reason"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// Two small pieces of damage far apart must not drag the whole span between
/// them through the aperture.
///
/// A DIRTYFB with several clip rects used to go up as their bounding union,
/// and for the two boxes a toolkit really sends -- a menu here, the shadow it
/// dropped over there -- that union is most of the screen. The panel's
/// aperture takes writes at about 42 MB/s, so a whole 1920x1080 span is ~99 ms
/// and three dropped frames for what the client said was two 16x16 boxes.
///
/// The screen cannot tell the two apart for a client that painted its whole
/// buffer, which is every client a test writes, so this paints the WHOLE
/// buffer with the new frame and then asserts that what is between the boxes
/// still holds the old one. That is only true if the boxes went up as boxes.
/// The span count is asserted as well, for the cases where the union is the
/// right answer and the screen agrees either way.
#[test]
fn two_far_apart_damage_boxes_do_not_drag_the_whole_span_between_them() {
    const W: u32 = 120;
    const H: u32 = 96;
    let screen = kms_emu::attach_with(W, H, 128, true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    paint(&buf, |x, y| desktop_px(1, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
    drain_completions(&c);
    let mut panel = Panel::new(W, H, 1, alloc::vec::Vec::new(), 0);
    panel.check(&screen, "the first frame");

    // Two 16x16 boxes in opposite corners. Their union is 80x72, thirty times
    // what they are.
    paint(&buf, |x, y| desktop_px(2, x, y));
    dirtyfb(&c, fb, &[clip(16, 8, 32, 24), clip(80, 64, 96, 80)]);
    drain_completions(&c);
    assert_eq!(
        dirty_spans_blitted_for_test(),
        2,
        "two far-apart boxes went up as one span"
    );
    panel.present_box(2, 16, 8, 16, 16);
    panel.present_box(2, 80, 64, 16, 16);
    panel.check(
        &screen,
        "two boxes went up and the span between them did not",
    );

    // Two boxes that touch. Splitting these copies the same bytes twice for
    // nothing, so the union is the right answer and the count says so.
    paint(&buf, |x, y| desktop_px(3, x, y));
    dirtyfb(&c, fb, &[clip(16, 8, 48, 24), clip(32, 8, 64, 24)]);
    drain_completions(&c);
    assert_eq!(
        dirty_spans_blitted_for_test(),
        1,
        "two touching boxes went up separately, copying the overlap twice"
    );
    panel.present_box(3, 16, 8, 48, 16);
    panel.check(&screen, "two touching boxes went up as one span");

    // More boxes than the kernel will track one by one, and far enough apart
    // that it would otherwise rather split them: the union, because keeping
    // only the first eight would DROP the ninth box and leave that piece of
    // the client's redraw off the screen.
    paint(&buf, |x, y| desktop_px(4, x, y));
    let many: alloc::vec::Vec<DrmClipRect> =
        (0..9).map(|i| clip(0, i * 10, 16, i * 10 + 4)).collect();
    dirtyfb(&c, fb, &many);
    drain_completions(&c);
    assert_eq!(
        dirty_spans_blitted_for_test(),
        1,
        "nine boxes were tracked one by one, so the ninth went nowhere"
    );
    panel.present_box(4, 0, 0, 16, 84);
    panel.check(&screen, "nine boxes went up as one span");

    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// Destroying the framebuffer that is ON THE SCREEN turns the pipe off, and
/// the next whole frame brings the pointer back with nothing left behind.
///
/// `drm_mode_rmfb` goes through `drm_framebuffer_remove`, and on an atomic
/// driver `atomic_remove_fb` disables the CRTC whose primary plane showed
/// the fb (mode NULL, `active = false`): the output goes dark, which is why
/// no compositor removes the framebuffer it is scanning out (wlroots keeps
/// the buffer locked until the next flip has landed). This test used to
/// expect the panel to keep the freed frame, which is what the kernel did
/// before it followed Linux here. What it guards is the pointer across that
/// state: a move while the pipe is off draws nothing, because there is no
/// scanout to draw on, and when a client puts a whole frame up again the
/// pointer arrives at its latest position with no ghost of the old one --
/// the fault this was written for was a move with `crtc_fb == 0` that moved
/// the bookkeeping (`cursor.drawn`) without moving the image.
#[test]
fn destroying_the_framebuffer_on_screen_turns_the_pipe_off_and_the_pointer_comes_back_with_the_next_frame(
) {
    const W: u32 = 120;
    const H: u32 = 96;
    const CUR: u32 = 16;
    let screen = kms_emu::attach_with(W, H, 128, true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    paint(&buf, |x, y| desktop_px(1, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
    drain_completions(&c);

    let bmp = pointer_bitmap(CUR, CUR);
    let cur = c.create_dumb(CUR, CUR);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, CUR, CUR, 20, 20);
    let mut panel = Panel::new(W, H, 1, bmp.clone(), CUR);
    panel.cursor = Some((20, 20, CUR, CUR));
    panel.check(&screen, "the pointer is on the frame");

    // The client drops the framebuffer it presented: the CRTC that showed
    // it is disabled, so the panel goes dark, pointer included.
    c.rmfb(fb).expect("RMFB the framebuffer on the CRTC");
    assert_eq!(
        drm::crtc_fb(),
        0,
        "RMFB really unbinds the CRTC's framebuffer"
    );
    assert!(
        drm::crtc_blanked(),
        "RMFB of the framebuffer on the CRTC disables it (atomic_remove_fb)"
    );
    let dark = |what: &str| {
        for y in 0..H {
            for x in 0..W {
                assert_eq!(
                    screen.pixel(x, y),
                    0,
                    "{}: pixel ({}, {}) is lit",
                    what,
                    x,
                    y
                );
            }
        }
    };
    dark("the pipe went dark");

    // A move with the pipe off: nothing to draw on, so nothing is drawn --
    // and nothing of the old image is left to find later.
    move_cursor(&c, drm::SYNTH_CRTC_ID, 60, 40);
    dark("a pointer move on a dark pipe");

    // The client puts a whole frame up again. The pointer is where it was
    // last moved to, and nowhere else.
    let buf2 = c.create_dumb(W, H);
    paint(&buf2, |x, y| desktop_px(2, x, y));
    let fb2 = c.addfb2(&buf2);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb2, W, H);
    drain_completions(&c);
    let mut panel = Panel::new(W, H, 2, bmp.clone(), CUR);
    panel.cursor = Some((60, 40, CUR, CUR));
    panel.check(
        &screen,
        "the frame came back with the pointer at its new place",
    );

    // And it still comes off entirely.
    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    panel.cursor = None;
    panel.check(&screen, "the pointer was hidden");

    c.rmfb(fb2).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    c.destroy_dumb(buf2.handle).expect("DESTROY_DUMB 2");
}

/// A pointer straddling the right edge of a framebuffer SMALLER than the mode
/// must still be erasable, or its outer columns stay on the panel for good.
///
/// The two paths clip the pointer's window against different things. The
/// present composites into the client's buffer, so it clips to the buffer:
/// for a 96-wide framebuffer on a 120-wide mode, a pointer at x=92 gets a
/// window that stops at x=96. The move path draws straight onto the panel, so
/// it clips to the PANEL: the same pointer gets a window reaching x=112. Each
/// one saves what its own window covered, so a present after a move recorded
/// the narrow window over the wide one -- and the next erase put back only 16
/// of the 20 columns. The four it did not own were pointer pixels, on top of
/// the desktop, with nothing left that knew they were there.
///
/// This is not the fault Moebius is looking at (labwc presents a framebuffer
/// the size of the mode), but it is the same family: a window written in one
/// place and restored in another.
#[test]
fn a_pointer_past_the_edge_of_a_narrow_framebuffer_must_still_come_off() {
    const W: u32 = 120;
    const H: u32 = 96;
    const SMALL_W: u32 = 96;
    const SMALL_H: u32 = 80;
    const CUR: u32 = 16;
    let screen = kms_emu::attach_with(W, H, 128, true);
    let c = Client::open(0);

    // Frame 1 over the whole panel, so the part the narrow framebuffer never
    // touches holds a frame of its own and a leftover there is visible.
    let big = c.create_dumb(W, H);
    paint(&big, |x, y| desktop_px(1, x, y));
    let fb_big = c.addfb2(&big);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb_big, W, H);
    drain_completions(&c);

    let small = c.create_dumb(SMALL_W, SMALL_H);
    let fb_small = c.addfb2(&small);

    let bmp = pointer_bitmap(CUR, CUR);
    let cur = c.create_dumb(CUR, CUR);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    let mut panel = Panel::new(W, H, 1, bmp.clone(), CUR);

    // Straddling x=96: four columns of the pointer land where the narrow
    // framebuffer does not reach. A pointer move draws them; only a present
    // that knows they are there can undo them.
    let (px0, py0) = (92i32, 15i32);
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, CUR, CUR, px0, py0);
    panel.cursor = Some((px0, py0, CUR, CUR));
    panel.check(&screen, "the pointer hangs off the narrow framebuffer");

    // The narrow present composites the pointer again, and this is where the
    // window used to shrink.
    paint(&small, |x, y| desktop_px(2, x, y));
    // A flip onto a framebuffer the mode does not fit in is ENOSPC
    // (`drm_mode_page_flip_ioctl`, like `SETCRTC` in
    // `a_framebuffer_smaller_than_the_mode_leaves_the_rest_of_the_screen_alone`),
    // so the narrow scanout is reached the way the kernel's own callers
    // reach it, and the CRTC is left holding the fb as the flip did.
    drm::present_now_checked(fb_small, drm::SYNTH_CRTC_ID, None)
        .expect("present the narrow framebuffer");
    drm::set_crtc_fb(drm::SYNTH_CRTC_ID, fb_small);
    panel.present_box(2, 0, 0, SMALL_W, SMALL_H);
    panel.check(&screen, "frame 2 from the narrow framebuffer");
    assert!(
        drm::cursor_windows_widened_for_test() > 0,
        "the composite never widened its window, so this test is not \
         exercising the fix"
    );

    // Now take the pointer away. Everything it drew has to come off, columns
    // past the framebuffer's edge included.
    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    panel.cursor = None;
    panel.check(&screen, "the pointer was hidden and left nothing behind");

    c.rmfb(fb_small).expect("RMFB small");
    c.rmfb(fb_big).expect("RMFB big");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(small.handle).expect("DESTROY_DUMB small");
    c.destroy_dumb(big.handle).expect("DESTROY_DUMB big");
}

/// Three hundred steps of the desktop, in a deterministic order nobody chose
/// by hand, with the whole panel checked against the contract after every
/// one of them.
///
/// The single-scenario tests above each say "this exact sequence must not do
/// this exact wrong thing", which only ever catches the fault whoever wrote
/// them had already thought of. Moebius reports rectangles that are still
/// there after the one cause we found, so what is needed now is the opposite
/// shape of test: put the panel through combinations of presents, pointer
/// moves, pointer hides, buffer reuse and blank round-trips, and assert the
/// one rule the user reads off the screen -- the panel shows the frame that
/// was last PRESENTED, with the pointer on top, and nothing else -- after
/// every single step. A failing seed prints the step that broke it and the
/// operation log, which is a reproduction.
///
/// Two buffers, alternating, because that is what wlroots does, and the
/// scribble step blacks out a box in the buffer the kernel last copied FROM:
/// a released buffer the compositor has started drawing into. Every pixel of
/// a legitimate frame is opaque and carries its frame number, so a black
/// pixel or a pixel from another frame is caught by value, not by position.
#[test]
fn three_hundred_steps_of_desktop_never_leave_anything_but_the_frame_and_the_pointer() {
    // Every combination of the two present flags, because the band skip is
    // the one mechanism that can decide a band is already on the panel and
    // never copy it again -- so a stale pixel it leaves stays for good --
    // and the repair is the other writer of the panel that does not go
    // through the blit. Two seeds each, so the order of operations is not
    // one order.
    let mut widened = 0usize;
    for (skip, repair) in [(false, false), (true, false), (false, true), (true, true)] {
        for seed in [0x5EED_1234u32, 0x0BAD_C0DE] {
            widened += soak_the_desktop(seed, skip, repair);
        }
    }
    // A present on a framebuffer smaller than the mode has to have caught the
    // pointer across its edge at least once in all of this, because that is
    // the only thing that leaves a piece of the pointer to erase later. If it
    // never happened, those steps are not exercising what they were added for
    // -- and a whole fix would be untested with every test still green.
    assert!(
        widened > 0,
        "no composite in 2400 steps widened its window, so the \
         smaller-than-the-mode steps never put the pointer across the edge"
    );
}

fn soak_the_desktop(seed_in: u32, skip: bool, repair: bool) -> usize {
    const W: u32 = 120;
    const H: u32 = 96;
    const CUR: u32 = 16;
    let screen = kms_emu::attach_with(W, H, 128, true);
    drm::set_present_skip_enabled(skip);
    drm::set_present_repair_enabled(repair);
    let c = Client::open(0);

    let bufs = [c.create_dumb(W, H), c.create_dumb(W, H)];
    let mut fbs = [c.addfb2(&bufs[0]), c.addfb2(&bufs[1])];
    // A framebuffer SMALLER than the mode, which is a case the present path
    // has its own arithmetic for (`image_pitch_px`, and the pointer clipped
    // to what the buffer covers rather than to the row stride). Its width is
    // a multiple of 16 so the write-combining widening of the copy is a
    // no-op and the model does not have to know how the kernel widens.
    const SMALL_W: u32 = 96;
    const SMALL_H: u32 = 80;
    let small = c.create_dumb(SMALL_W, SMALL_H);
    let fb_small = c.addfb2(&small);
    let bmp = pointer_bitmap(CUR, CUR);
    let cur = c.create_dumb(CUR, CUR);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    // A pointer of a DIFFERENT size, because a theme change or a client
    // setting its own cursor really does hand the kernel another bitmap while
    // the old one is on the screen. What the old one covered has to come back
    // even though the new window is not the old window.
    const CUR2: u32 = 8;
    let bmp2 = pointer_bitmap(CUR2, CUR2);
    let cur2 = c.create_dumb(CUR2, CUR2);
    {
        let px = map_dumb(&cur2);
        for (i, v) in bmp2.iter().enumerate() {
            px[i] = *v;
        }
    }
    // And the size a real theme actually uses. On a 120x96 panel a 64x64
    // pointer is a third of the screen, so its window is clipped on two edges
    // at once for most positions -- and what it covers is 16 KB of save, where
    // the 16x16 one was 1 KB.
    const CUR3: u32 = 64;
    let bmp3 = pointer_bitmap(CUR3, CUR3);
    let cur3 = c.create_dumb(CUR3, CUR3);
    {
        let px = map_dumb(&cur3);
        for (i, v) in bmp3.iter().enumerate() {
            px[i] = *v;
        }
    }
    // A framebuffer BIGGER than the mode. The copy stops at the panel's edge,
    // so the pointer's window is computed against a buffer whose rows run past
    // the screen -- the opposite arithmetic to the smaller one, and the case
    // the window widening has to refuse.
    const BIG_W: u32 = 160;
    const BIG_H: u32 = 128;
    let big = c.create_dumb(BIG_W, BIG_H);
    let fb_big = c.addfb2(&big);
    let mut cur_sz = CUR;
    let mut cur_h = cur.handle;

    let mut frame: u32 = 1;
    let mut slot = 0usize;
    paint(&bufs[slot], |x, y| desktop_px(frame, x, y));
    set_crtc(&c, drm::SYNTH_CRTC_ID, fbs[slot], W, H);
    drain_completions(&c);
    let mut model = Panel::new(W, H, frame, bmp.clone(), CUR);
    model.check(&screen, "the first frame");

    // Which framebuffer the panel is holding entirely. A damage box is only
    // honoured while the panel already holds that framebuffer: otherwise the
    // rest of the panel belongs to a different frame and honouring the box
    // would leave two frames on screen at once, which is what put a menu on
    // the screen twice once already.
    let mut panel_fb = fbs[slot];
    let mut pos = (8i32, 8i32);
    let mut shown = false;
    // The most bands any one present left alone. Asserted below, because a
    // soak that never took the skip would say nothing about it -- the one
    // lesson of the probe that measured in the wrong place.
    let mut most_skipped = 0usize;
    // Damage flushes that went up as their own boxes rather than as one
    // bounding span.
    let mut by_box = 0usize;
    let mut log: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    // A 32-bit LCG: the sequence is fixed, so a failure here reproduces on
    // any machine, and the log below names the step.
    let mut seed: u32 = seed_in;
    let mut rnd = |m: u32| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 13) % m
    };
    for step in 0..300u32 {
        let what;
        match rnd(18) {
            0..=3 => {
                // The compositor renders a finished frame into the other
                // buffer of its chain and puts it up, whole, as labwc does.
                frame += 1;
                slot ^= 1;
                let f = frame;
                paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                c.page_flip(drm::SYNTH_CRTC_ID, fbs[slot], step as u64)
                    .expect("flip");
                drain_completions(&c);
                model.present(frame);
                panel_fb = fbs[slot];
                // A full present composites the pointer at wherever it is
                // now, so that is what is on the panel.
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
                what = alloc::format!("step {}: presented frame {}", step, frame);
            }
            4..=6 => {
                // Anywhere from wholly off the left or top edge to wholly
                // past the right or bottom one. The kernel clips both the
                // window it draws and the window it reads back, and those
                // two have to clip the same way or the pointer leaves its
                // widened margins on the screen for good.
                pos = (
                    rnd(W + 2 * CUR) as i32 - CUR as i32,
                    rnd(H + 2 * CUR) as i32 - CUR as i32,
                );
                move_cursor(&c, drm::SYNTH_CRTC_ID, pos.0, pos.1);
                if shown {
                    model.cursor = Some((pos.0, pos.1, cur_sz, cur_sz));
                }
                what = alloc::format!("step {}: pointer moved to {:?}", step, pos);
            }
            7 => {
                // The released buffer is the compositor's again and it has
                // started the next frame in it by clearing a box to
                // transparent black. Nothing is presented: this changes
                // nothing that may reach the screen.
                let (bx, by) = (rnd(W - 8), rnd(H - 8));
                let (bw, bh) = (rnd(W - bx) + 1, rnd(H - by) + 1);
                let stride = (bufs[slot].pitch / 4) as usize;
                let px = map_dumb(&bufs[slot]);
                for y in by..by + bh {
                    for x in bx..bx + bw {
                        px[y as usize * stride + x as usize] = 0x0000_0000;
                    }
                }
                what = alloc::format!(
                    "step {}: the client cleared {}x{}+{}+{} in the buffer it \
                     had presented",
                    step,
                    bw,
                    bh,
                    bx,
                    by
                );
            }
            8 => {
                // The scene has not changed and the compositor puts it up
                // again, which is what an idle desktop does all day. This is
                // the ONLY operation the band skip can act on: every band
                // hashes to what the panel is already holding, so a band
                // whose pixels are not really there stays wrong for good.
                slot ^= 1;
                let f = frame;
                paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                c.page_flip(drm::SYNTH_CRTC_ID, fbs[slot], step as u64)
                    .expect("flip");
                drain_completions(&c);
                // The same pixels the model already held -- unless a bare
                // blank has painted the panel one colour since, which is
                // exactly what a re-present has to put right.
                model.present(frame);
                panel_fb = fbs[slot];
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
                most_skipped = most_skipped.max(drm::skipped_bands_for_test());
                what = alloc::format!("step {}: presented frame {} AGAIN", step, frame);
            }
            12 => {
                shown = !shown;
                if shown {
                    set_cursor(&c, drm::SYNTH_CRTC_ID, cur_h, cur_sz, cur_sz, pos.0, pos.1);
                    model.cursor = Some((pos.0, pos.1, cur_sz, cur_sz));
                } else {
                    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
                    model.cursor = None;
                }
                what = alloc::format!("step {}: pointer shown={}", step, shown);
            }
            9 => {
                // A client with damage tracking: it redraws a box and asks
                // for that box only. Sometimes into the buffer the panel is
                // already holding, where the box may be honoured, and
                // sometimes into the other one, where honouring it would put
                // two frames on the screen at once.
                frame += 1;
                if rnd(2) == 0 {
                    slot ^= 1;
                }
                let f = frame;
                paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                // x aligned to 16 so the write-combining widening of the box
                // is a no-op: the model then says what the SCREEN must show
                // without having to know how the kernel widens anything.
                let bx = rnd(W / 16) * 16;
                let bw = (rnd(W / 16 - bx / 16) + 1) * 16;
                let by = rnd(H - 1);
                let bh = rnd(H - by) + 1;
                dirtyfb(
                    &c,
                    fbs[slot],
                    &[clip(
                        bx as u16,
                        by as u16,
                        (bx + bw) as u16,
                        (by + bh) as u16,
                    )],
                );
                drain_completions(&c);
                if fbs[slot] == panel_fb {
                    model.present_box(frame, bx, by, bw, bh);
                    what = alloc::format!(
                        "step {}: frame {} as a damage box {}x{}+{}+{}",
                        step,
                        frame,
                        bw,
                        bh,
                        bx,
                        by
                    );
                } else {
                    // The panel was holding another framebuffer, so the box
                    // cannot stand alone and the whole frame has to go up.
                    model.present(frame);
                    what = alloc::format!(
                        "step {}: frame {} as a damage box on a framebuffer the \
                         panel was not holding, so the whole frame",
                        step,
                        frame
                    );
                }
                panel_fb = fbs[slot];
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
            }
            13 => {
                // A client whose framebuffer is smaller than the mode. The
                // present copies what the buffer covers and the rest of the
                // panel keeps the frame it had, so the panel holds two frames
                // at once -- and the pointer is composited over a window the
                // copy may not reach.
                frame += 1;
                let f = frame;
                paint(&small, |x, y| desktop_px(f, x, y));
                // A flip onto it is ENOSPC (the mode does not fit), so the
                // present is made the way the kernel's own callers make it
                // and the CRTC is left holding the fb as the flip did.
                drm::present_now_checked(fb_small, drm::SYNTH_CRTC_ID, None)
                    .expect("present the narrow framebuffer");
                drm::set_crtc_fb(drm::SYNTH_CRTC_ID, fb_small);
                model.present_box(frame, 0, 0, SMALL_W, SMALL_H);
                // The panel does not hold this framebuffer entirely -- only
                // its top-left corner -- so no damage box on it may be
                // honoured on its own.
                panel_fb = 0;
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
                what = alloc::format!(
                    "step {}: frame {} from a {}x{} framebuffer, smaller than the mode",
                    step,
                    frame,
                    SMALL_W,
                    SMALL_H
                );
            }
            10 => {
                // DPMS off and on again, with the frame that follows it --
                // which is what a screen coming back actually looks like.
                drm::set_crtc_blanked(true);
                drm::set_crtc_blanked(false);
                frame += 1;
                slot ^= 1;
                let f = frame;
                paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                c.page_flip(drm::SYNTH_CRTC_ID, fbs[slot], step as u64)
                    .expect("flip");
                drain_completions(&c);
                model.present(frame);
                panel_fb = fbs[slot];
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
                what = alloc::format!("step {}: blanked, unblanked, frame {}", step, frame);
            }
            14 => {
                // A client whose framebuffer is BIGGER than the mode.
                frame += 1;
                let f = frame;
                paint(&big, |x, y| desktop_px(f, x, y));
                c.page_flip(drm::SYNTH_CRTC_ID, fb_big, step as u64)
                    .expect("flip the oversized framebuffer");
                drain_completions(&c);
                // Its top-left corner holds the same pixels a framebuffer of
                // the mode's size would, so the screen must show this frame
                // entirely.
                model.present(frame);
                // The panel holds a corner of this framebuffer, not the whole
                // of it, so no damage box may be honoured on its own after
                // this.
                panel_fb = 0;
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
                what = alloc::format!(
                    "step {}: frame {} from a {}x{} framebuffer, bigger than the mode",
                    step,
                    frame,
                    BIG_W,
                    BIG_H
                );
            }
            15 => {
                // TWO damage boxes in one DIRTYFB, which is what a client with
                // real damage tracking sends: a menu and the shadow it dropped
                // somewhere else. The kernel may copy them in one span or in
                // two, and either way nothing between them may move.
                frame += 1;
                let f = frame;
                paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                let bx = rnd(W / 32) * 16;
                let bw = (rnd(2) + 1) * 16;
                let by = rnd(H / 2);
                let bh = rnd(H / 2 - by) + 1;
                let cx2 = 64 + rnd(2) * 16;
                let cw2 = (rnd(2) + 1) * 16;
                let cy2 = H / 2 + rnd(H / 4);
                let ch2 = rnd(H - cy2) + 1;
                dirtyfb(
                    &c,
                    fbs[slot],
                    &[
                        clip(bx as u16, by as u16, (bx + bw) as u16, (by + bh) as u16),
                        clip(
                            cx2 as u16,
                            cy2 as u16,
                            (cx2 + cw2) as u16,
                            (cy2 + ch2) as u16,
                        ),
                    ],
                );
                drain_completions(&c);
                if fbs[slot] == panel_fb {
                    // The TWO boxes and nothing between them: far apart and
                    // small, so the kernel blits each one instead of their
                    // bounding union, and everything outside them keeps the
                    // frame it had. Both x spans are 16-aligned so the
                    // write-combining widening of each is a no-op.
                    model.present_box(frame, bx, by, bw, bh);
                    model.present_box(frame, cx2, cy2, cw2, ch2);
                    assert_eq!(
                        dirty_spans_blitted_for_test(),
                        2,
                        "step {}: two boxes {}x{}+{}+{} and {}x{}+{}+{} went up \
                         as one span, so the screen agreeing means nothing",
                        step,
                        bw,
                        bh,
                        bx,
                        by,
                        cw2,
                        ch2,
                        cx2,
                        cy2
                    );
                    by_box += 1;
                    what = alloc::format!(
                        "step {}: frame {} as TWO damage boxes {}x{}+{}+{} and {}x{}+{}+{}",
                        step,
                        frame,
                        bw,
                        bh,
                        bx,
                        by,
                        cw2,
                        ch2,
                        cx2,
                        cy2
                    );
                } else {
                    model.present(frame);
                    what = alloc::format!(
                        "step {}: frame {} as two damage boxes on a framebuffer the \
                         panel was not holding, so the whole frame",
                        step,
                        frame
                    );
                }
                panel_fb = fbs[slot];
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
            }
            16 => {
                // The pointer changes SIZE where it stands. The old window is
                // not the new window, so what the old pointer covered can only
                // come back from what the blit that drew it saved.
                cur_sz = match cur_sz {
                    CUR => CUR2,
                    CUR2 => CUR3,
                    _ => CUR,
                };
                cur_h = match cur_sz {
                    CUR => cur.handle,
                    CUR2 => cur2.handle,
                    _ => cur3.handle,
                };
                model.bmp = match cur_sz {
                    CUR => bmp.clone(),
                    CUR2 => bmp2.clone(),
                    _ => bmp3.clone(),
                };
                model.bmp_w = cur_sz;
                shown = true;
                set_cursor(&c, drm::SYNTH_CRTC_ID, cur_h, cur_sz, cur_sz, pos.0, pos.1);
                model.cursor = Some((pos.0, pos.1, cur_sz, cur_sz));
                what = alloc::format!("step {}: pointer resized to {}x{}", step, cur_sz, cur_sz);
            }
            17 => {
                // The client DESTROYS the framebuffer the panel is scanning
                // out and makes another one from the same buffer. In Linux
                // `drm_framebuffer_remove` disables the CRTC that showed it
                // (`atomic_remove_fb`), so the panel goes dark, pointer and
                // all, until a whole frame goes up again -- which is what a
                // client that remade its framebuffer does next, and what
                // keeps a compositor from ever removing the fb on screen.
                // (Only when it IS the fb on the CRTC: after a narrow present
                // or a damage flush into the other buffer the CRTC holds a
                // different one, and removing this one touches nothing.)
                let on_crtc = drm::crtc_fb() == fbs[slot];
                c.rmfb(fbs[slot])
                    .expect("RMFB the framebuffer on the panel");
                if on_crtc {
                    assert_eq!(
                        screen.pixel(0, 0),
                        0,
                        "step {}: RMFB of the fb on the CRTC left the pipe lit",
                        step
                    );
                }
                fbs[slot] = c.addfb2(&bufs[slot]);
                let f = frame;
                paint(&bufs[slot], |x, y| desktop_px(f, x, y));
                set_crtc(&c, drm::SYNTH_CRTC_ID, fbs[slot], W, H);
                drain_completions(&c);
                model.present(frame);
                panel_fb = fbs[slot];
                model.cursor = if shown {
                    Some((pos.0, pos.1, cur_sz, cur_sz))
                } else {
                    None
                };
                what = alloc::format!(
                    "step {}: the framebuffer on the panel was destroyed, the pipe \
                     went dark, and frame {} went up on the remade one",
                    step,
                    frame
                );
            }
            _ => {
                // DPMS off and on again with NOTHING presented after it,
                // which a stray DPMS write really can do. The blank paints
                // over the pointer too, so now the panel is one colour and
                // nothing is drawn -- and whatever the pointer was covering
                // is not under it any more. Anything the next move puts back
                // there is a rectangle of the old desktop on a black screen.
                drm::set_crtc_blanked(true);
                drm::set_crtc_blanked(false);
                // Black pixels are not a framebuffer's pixels, so the panel
                // holds nothing now and the next damage box cannot be
                // honoured on its own.
                panel_fb = 0;
                model.fill(screen.pixel(0, 0));
                what = alloc::format!("step {}: blanked and unblanked, no frame", step);
            }
        }
        log.push(what.clone());
        if log.len() > 8 {
            log.remove(0);
        }
        model.check(
            &screen,
            &alloc::format!(
                "{} [skip={} repair={} seed={:#x}] HISTORY {:?}",
                what,
                skip,
                repair,
                seed_in,
                log
            ),
        );
    }
    assert!(frame < 256, "the frame number has to stay in one byte");
    // How many composites had to widen their window, for the caller to sum:
    // that only happens when a smaller-than-the-mode present catches the
    // pointer across the edge of the client's framebuffer, which no single
    // run of 300 steps is guaranteed to reach.
    assert!(
        by_box > 0,
        "no damage flush ever went up box by box, so the two-box steps say \
         nothing about the span the kernel picks"
    );
    let widened = drm::cursor_windows_widened_for_test();
    assert_eq!(
        skip && most_skipped > 0,
        skip,
        "with the band skip armed some present had to skip a band, or this \
         soak did not exercise it at all"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    drm::set_present_skip_enabled(false);
    drm::set_present_repair_enabled(false);
    c.rmfb(fb_small).expect("RMFB small");
    c.destroy_dumb(small.handle).expect("DESTROY_DUMB small");
    for fb in fbs {
        c.rmfb(fb).expect("RMFB");
    }
    c.destroy_dumb(cur3.handle)
        .expect("DESTROY_DUMB big cursor");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(cur2.handle)
        .expect("DESTROY_DUMB small cursor");
    c.rmfb(fb_big).expect("RMFB big");
    c.destroy_dumb(big.handle).expect("DESTROY_DUMB big");
    for b in bufs {
        c.destroy_dumb(b.handle).expect("DESTROY_DUMB");
    }
    widened
}

/// The bug Moebius sees: a pointer move pastes the frame labwc is STILL
/// DRAWING into the frame that is on the screen.
///
/// Our software-KMS present does not hold the client's buffer the way a real
/// display engine does -- it copies it and completes the flip -- so the
/// compositor is free to start the next frame in that same buffer. Nothing
/// is wrong with that; it is what a released buffer is for. What is wrong is
/// that `repaint_for_cursor` then goes back and READS that buffer to erase
/// and redraw its two ~64x64 windows, and a renderer begins a frame by
/// clearing to transparent black. So the window under the pointer gets a
/// black rectangle pasted into a frame that has no black in it, and it
/// appears exactly when something new is being drawn -- a menu, a popup --
/// which is precisely when Moebius sees it and precisely why labwc and
/// lunarbar are not at fault.
///
/// Nothing here needs a GPU: the race is not a race at all from the
/// kernel's side, because the two events are ordered by the ioctls.
#[test]
fn a_pointer_move_must_not_paste_the_frame_the_compositor_is_still_drawing() {
    const W: u32 = 120;
    const H: u32 = 64;
    // A padded, write-combining scanline: the UEFI shape, and the one where
    // the cursor patch widens its columns for real.
    let screen = kms_emu::attach_with(W, H, 128, true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    paint(&buf, |x, y| desktop_px(0, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
    drain_completions(&c);

    let bmp = pointer_bitmap(16, 16);
    let cur = c.create_dumb(16, 16);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 16, 16, 8, 8);

    let mut panel = Panel::new(W, H, 0, bmp, 16);
    panel.cursor = Some((8, 8, 16, 16));
    panel.check(&screen, "frame 0 up and the pointer composited on it");

    // labwc starts frame 1 in the buffer it just presented: a renderer opens
    // a frame by clearing the region it is about to draw to transparent
    // black. It has not presented anything, so the screen still shows frame
    // 0 -- and must keep showing it.
    let popup = (40u32, 16u32, 104u32, 48u32);
    {
        let px = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;
        for y in popup.1..popup.3 {
            for x in popup.0..popup.2 {
                px[y as usize * stride + x as usize] = 0x0000_0000;
            }
        }
    }
    panel.check(
        &screen,
        "the compositor cleared a popup box in its own buffer and \
         presented nothing",
    );

    // The pointer moves into that box, which is what a user does to open the
    // menu they are pointing at.
    move_cursor(&c, drm::SYNTH_CRTC_ID, 56, 24);
    panel.cursor = Some((56, 24, 16, 16));
    panel.check(&screen, "the pointer moved over the box being drawn");

    // A few pixels further, which is what a mouse actually does: the old and
    // new windows OVERLAP, so an erase that read the panel instead of what
    // was saved would capture the pointer it is erasing and blend the new one
    // over it, baking a trail in.
    move_cursor(&c, drm::SYNTH_CRTC_ID, 59, 27);
    panel.cursor = Some((59, 27, 16, 16));
    panel.check(&screen, "the pointer moved three pixels inside the box");

    // And out again, which is where the erase half used to read that buffer.
    move_cursor(&c, drm::SYNTH_CRTC_ID, 8, 8);
    panel.cursor = Some((8, 8, 16, 16));
    panel.check(&screen, "the pointer moved back out of the box");

    // The right edge, where the pointer's window runs off the visible width
    // into the scanline's off-screen padding. Those columns are written, so
    // they have to be read and put back too -- and the model above cannot see
    // them, which is exactly why a pointer left behind there would never be
    // noticed. Read them, visit, leave, and they must be as they were.
    let padding = |s: &kms_emu::Screen| {
        let mut v = alloc::vec::Vec::new();
        for y in 24..40 {
            for x in W..s.pitch_px() {
                v.push(s.pixel(x, y));
            }
        }
        v
    };
    let before = padding(&screen);
    move_cursor(&c, drm::SYNTH_CRTC_ID, 112, 24);
    panel.cursor = Some((112, 24, 16, 16));
    panel.check(&screen, "the pointer at the right edge");
    move_cursor(&c, drm::SYNTH_CRTC_ID, 8, 8);
    panel.cursor = Some((8, 8, 16, 16));
    panel.check(&screen, "the pointer left the right edge");
    assert_eq!(
        padding(&screen),
        before,
        "the pointer stayed in the scanline padding: the columns its blit \
         wrote are not the columns the restore put back"
    );

    // Now labwc finishes frame 1 and presents it. The screen catches up, the
    // pointer is composited on top of it -- and what it is covering has to be
    // re-remembered from the frame that just went up, or the next move erases
    // with frame 0's pixels.
    paint(&buf, |x, y| desktop_px(1, x, y));
    c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xC0DE).expect("flip");
    drain_completions(&c);
    panel.present(1);
    panel.check(&screen, "frame 1 presented with the pointer on it");
    move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 40);
    panel.cursor = Some((40, 40, 16, 16));
    panel.check(&screen, "the pointer moved after frame 1 went up");

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// The saved pixels describe the panel, so anything that repaints the panel
/// behind the pointer's back has to make the kernel forget them.
///
/// Blanking is the case with no second chance. It paints the panel black and
/// does NOT touch `cursor.drawn`, and un-blanking is allowed to happen on a
/// bare DPMS write with no frame behind it (see `set_crtc_blanked`). So the
/// first pointer move after that would erase the pointer by putting back what
/// it was covering before the screen went black -- one rectangle of the old
/// desktop on a black screen, which is the same class of bug as the one the
/// save exists to fix and would have been introduced by fixing it.
/// llvmpipe hands over a frame it has not finished: its tiles arrive in
/// worker threads while the kernel is already copying. That one frame is not
/// the kernel's to fix -- there is no fence to wait on (`SYNCOBJ_TIMELINE`
/// answers 0) and the buffer really did hold those pixels when they were
/// read.
///
/// What IS the kernel's is whether the black it copied SURVIVES. labwc
/// presents a WHOLE FRAME every time -- measured on Moebius's boot, 1600
/// presents and not one `DIRTYFB` -- so the very next present carries every
/// pixel and nothing of the mix may be left on the panel. A rectangle that
/// stays while a menu sits open is a rectangle nobody is overwriting, and
/// that would be ours.
#[test]
fn black_copied_mid_frame_does_not_survive_the_next_present() {
    for skip in [false, true] {
        const W: u32 = 120;
        // Tall enough that the copy takes two bands, derived from the real
        // band size: the boundary between two bands is the only place a
        // test can get INSIDE a copy, so a height that hardcoded it would
        // stop testing anything the day the constant moves -- and the
        // assertion further down that the race landed is what would say so.
        let chunk = drm::blit_chunk_rows_for_test();
        let h = chunk + chunk / 4;
        // The box the client blacks out: inside the rows the SECOND band
        // copies, so the kernel reaches it after the hook has run.
        let (by, bh) = (chunk + 4, chunk / 4 - 8);
        let (bx, bw) = (40u32, 64u32);
        let screen = kms_emu::attach_with(W, h, 128, true);
        // The band-skipping present is what Moebius has been booting with,
        // and it is the one mechanism that could decide a band already
        // matches and never copy it again. Both ways, same assertion.
        drm::set_present_skip_enabled(skip);
        let c = Client::open(0);
        let buf = c.create_dumb(W, h);
        paint(&buf, |x, y| desktop_px(0, x, y));
        let fb = c.addfb2(&buf);
        set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, h);
        drain_completions(&c);

        // The copy goes band by band, asking the output for its framebuffer
        // once per band. Clearing a box of the second band's rows on the
        // FIRST ask is a tile that goes black after the source was sampled
        // and before the kernel reaches it: the copy carries it and neither
        // read of the probe sees anything else.
        let stride = (buf.pitch / 4) as usize;
        let px = map_dumb(&buf);
        // The hook wants `Send`, and a raw pointer is not, so the address
        // travels as an integer; the buffer it names is this test's own and
        // outlives the hook, which `clear_mid_blit` takes down below.
        let base = px.as_mut_ptr() as usize;
        kms_emu::on_blit_band(move |n| {
            if n != 0 {
                return;
            }
            // SAFETY: the dumb buffer outlives this test and the hook runs
            // inside the present, on this thread, while nothing else writes
            // those rows.
            let p =
                unsafe { core::slice::from_raw_parts_mut(base as *mut u32, stride * h as usize) };
            for y in by..by + bh {
                for x in bx..bx + bw {
                    p[y as usize * stride + x as usize] = 0x0000_0000;
                }
            }
        });
        paint(&buf, |x, y| desktop_px(1, x, y));
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xBEEF).expect("flip");
        drain_completions(&c);
        kms_emu::clear_mid_blit();

        // The race really happened, or the rest of this test proves nothing.
        assert_eq!(
            screen.pixel(bx + bw / 2, by + bh / 2),
            0x0000_0000,
            "skip={}: the hook did not land inside the copy, so this test is \
             not about anything",
            skip
        );

        // Now the compositor presents a finished frame, whole, as labwc does
        // every time. Nothing of the mix may be left.
        paint(&buf, |x, y| desktop_px(2, x, y));
        c.page_flip(drm::SYNTH_CRTC_ID, fb, 0xF00D).expect("flip");
        drain_completions(&c);

        for y in 0..h {
            for x in 0..W {
                let got = screen.pixel(x, y);
                assert_eq!(
                    got,
                    desktop_px(2, x, y),
                    "skip={}: ({}, {}) reads {:#010x} after a whole finished \
                     frame went up{}",
                    skip,
                    x,
                    y,
                    got,
                    if got == 0 {
                        " -- the black the kernel copied mid-frame is STILL \
                         THERE, so nothing overwrote it"
                    } else {
                        ""
                    }
                );
            }
        }

        drm::set_present_skip_enabled(false);
        c.rmfb(fb).expect("RMFB");
        c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
    }
}

/// The escape hatch really takes the old path, and this is what the old path
/// does.
///
/// `drm.cursor_from_client` exists for one risk the fix cannot measure from
/// here: reading the panel puts an aperture read on the pointer path, and
/// nobody has measured how slow a read of an NVIDIA BAR1 window is. If that
/// turns out to drag the pointer, this flag makes the machine usable again.
/// Its price is exactly the defect, so the test says so: the same steps as
/// `a_pointer_move_must_not_paste_the_frame_the_compositor_is_still_drawing`
/// put the black rectangle back. A flag whose only honest test is "the bug
/// returns" is a flag nobody should leave on, which is the point.
#[test]
fn the_escape_hatch_reads_the_clients_framebuffer_again_black_rectangles_and_all() {
    const W: u32 = 120;
    const H: u32 = 64;
    let screen = kms_emu::attach_with(W, H, 128, true);
    drm::set_cursor_from_client(true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    paint(&buf, |x, y| desktop_px(0, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
    drain_completions(&c);

    let bmp = pointer_bitmap(16, 16);
    let cur = c.create_dumb(16, 16);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 16, 16, 8, 8);

    // The compositor starts the next frame in the buffer it presented.
    {
        let px = map_dumb(&buf);
        let stride = (buf.pitch / 4) as usize;
        for y in 16..48usize {
            for x in 40..104usize {
                px[y * stride + x] = 0x0000_0000;
            }
        }
    }
    move_cursor(&c, drm::SYNTH_CRTC_ID, 56, 24);

    // The pointer's window is columns 48..72 -- widened to the
    // write-combining boundary -- and every pixel of it the pointer does not
    // cover now holds the black the compositor had not finished drawing over.
    let mut black = 0;
    for y in 24..40u32 {
        for x in 48..72u32 {
            if screen.pixel(x, y) == 0 {
                black += 1;
            }
        }
    }
    assert!(
        black > 0,
        "the flag did not take the old path: nothing pasted the half-drawn \
         frame, so there is nothing for the hatch to be an escape from"
    );

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// A panel that will not hand its pixels back keeps the older route, and that
/// route still has to draw and erase a pointer.
///
/// The fix above reads the panel, which a panel that is not ARGB8888 cannot
/// do, so the read-the-client's-framebuffer path is still there for it. No
/// machine this kernel meets has such a panel -- a UEFI GOP, virtio-gpu and an
/// NVIDIA BAR1 aperture are all 32-bit -- so nothing else would ever run it,
/// and an untested fallback is one that stops working without anyone finding
/// out. This is the only test that takes it, and it is the reason the
/// emulated panel can be told to refuse.
#[test]
fn a_panel_that_cannot_be_read_back_still_gets_its_pointer_drawn_and_erased() {
    let screen = kms_emu::attach(64, 16);
    screen.refuse_read_back();
    let c = Client::open(0);
    let buf = c.create_dumb(64, 16);
    paint(&buf, |_, _| 0xFF00_1111);
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, 64, 16);

    let cur = c.create_dumb(8, 8);
    {
        let px = map_dumb(&cur);
        for p in px.iter_mut().take(64) {
            *p = 0xFF00_00FF;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 8, 8, 4, 2);
    move_cursor(&c, drm::SYNTH_CRTC_ID, 40, 6);

    for y in 0..16 {
        for x in 0..64 {
            let want = if (40..48).contains(&x) && (6..14).contains(&y) {
                0xFF00_00FF
            } else {
                0xFF00_1111
            };
            assert_eq!(
                screen.pixel(x, y),
                want,
                "({}, {}) after a move on a panel that refuses read-back",
                x,
                y
            );
        }
    }

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

#[test]
fn blanking_the_panel_makes_the_kernel_forget_what_the_pointer_was_covering() {
    const W: u32 = 120;
    const H: u32 = 64;
    let screen = kms_emu::attach_with(W, H, 128, true);
    let c = Client::open(0);
    let buf = c.create_dumb(W, H);
    paint(&buf, |x, y| desktop_px(3, x, y));
    let fb = c.addfb2(&buf);
    set_crtc(&c, drm::SYNTH_CRTC_ID, fb, W, H);
    drain_completions(&c);

    let bmp = pointer_bitmap(16, 16);
    let cur = c.create_dumb(16, 16);
    {
        let px = map_dumb(&cur);
        for (i, v) in bmp.iter().enumerate() {
            px[i] = *v;
        }
    }
    set_cursor(&c, drm::SYNTH_CRTC_ID, cur.handle, 16, 16, 8, 8);

    drm::set_crtc_blanked(true);
    // A DPMS write on its own, with nothing presented after it.
    drm::set_crtc_blanked(false);
    move_cursor(&c, drm::SYNTH_CRTC_ID, 60, 30);

    // Whatever colour the blank left, the panel is one colour: the only thing
    // allowed on it is the pointer. A restored save would be a rectangle of
    // the desktop, wherever the pointer had been.
    let blank = screen.pixel(0, 0);
    for y in 0..H {
        for x in 0..W {
            if (60..76).contains(&x) && (30..46).contains(&y) {
                continue;
            }
            assert_eq!(
                screen.pixel(x, y),
                blank,
                "({}, {}) is not the blanked panel: the pointer put back what \
                 it was covering before the screen went black",
                x,
                y
            );
        }
    }

    set_cursor(&c, drm::SYNTH_CRTC_ID, 0, 0, 0, 0, 0);
    c.rmfb(fb).expect("RMFB");
    c.destroy_dumb(cur.handle).expect("DESTROY_DUMB cursor");
    c.destroy_dumb(buf.handle).expect("DESTROY_DUMB");
}

/// `drm_mode_rmfb` removes the framebuffer and, through
/// `drm_framebuffer_remove`, disables the CRTC that was showing it; a
/// framebuffer that is not on the CRTC goes without touching it; and
/// `CLOSEFB` only drops the object, the scanout stays. Here RMFB never
/// turned anything off: the panel kept a frame the client had freed and
/// the CRTC read as on.
#[test]
fn removing_the_framebuffer_on_the_crtc_turns_it_off_and_closing_it_does_not() {
    let screen = kms_emu::attach(64, 16);
    let c = Client::open(0);
    let fb_of = |base: u32| {
        let buf = c.create_dumb(64, 16);
        paint(&buf, |x, y| tag(base, x, y));
        c.addfb2(&buf)
    };
    let fb_a = fb_of(0x0077_0000);
    let fb_b = fb_of(0x0088_0000);
    let fb_c = fb_of(0x0099_0000);

    c.page_flip(drm::SYNTH_CRTC_ID, fb_a, 0xF00D).expect("flip");
    assert_eq!(screen.pixel(3, 2), tag(0x0077_0000, 3, 2));

    // A framebuffer that is not on the CRTC: nothing changes on screen.
    let mut id = fb_b;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_RMFB, &mut id), Ok(0));
    assert!(
        !drm::crtc_blanked(),
        "removing another fb turned the CRTC off"
    );
    assert_eq!(screen.pixel(3, 2), tag(0x0077_0000, 3, 2));
    assert_eq!(drm::crtc_fb(), fb_a);

    // The one being scanned out: the CRTC goes off with it.
    let mut id = fb_a;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_RMFB, &mut id), Ok(0));
    assert!(drm::crtc_blanked(), "the CRTC stayed on without its fb");
    assert_eq!(screen.pixel(3, 2), 0, "the panel kept the freed frame");
    assert_eq!(drm::crtc_fb(), 0);

    // CLOSEFB: the object goes, the scanout stays.
    c.page_flip(drm::SYNTH_CRTC_ID, fb_c, 0xF00E).expect("flip");
    assert!(!drm::crtc_blanked());
    let mut id = fb_c;
    assert_eq!(c.ioctl(DRM_IOCTL_MODE_CLOSEFB, &mut id), Ok(0));
    assert!(!drm::crtc_blanked(), "CLOSEFB turned the CRTC off");
    assert_eq!(screen.pixel(3, 2), tag(0x0099_0000, 3, 2));
    let mut id = fb_c;
    assert_eq!(
        c.ioctl(DRM_IOCTL_MODE_RMFB, &mut id),
        Err(FsError::EntryNotFound),
        "a closed fb is gone"
    );
}

/// `drm_mode_create_dumb` refuses a width, height or bpp of 0 and a bpp
/// past `U32_MAX - 8` (EINVAL), and sizes the buffer from the bpp asked
/// for: `DIV_ROUND_UP(bpp, 8)` bytes per pixel. Here every bpp below 32
/// became 32 -- a bpp of 0 got a 32-bit buffer, and a 16-bit request
/// was sized and reported as 32-bit.
#[test]
fn a_dumb_buffer_is_sized_by_the_bpp_asked_for_and_a_zero_is_refused() {
    let _serialised = drm::test_globals::lock();
    let c = Client::open(0);
    let einval = Err(FsError::InvalidParam);
    let create = |width: u32, height: u32, bpp: u32| {
        let mut req = DrmModeCreateDumb {
            height,
            width,
            bpp,
            flags: 0,
            handle: 0,
            pitch: 0,
            size: 0,
        };
        c.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut req)
            .map(|_| (req.pitch, req.size, req.handle))
    };
    assert_eq!(create(64, 4, 0), einval, "a bpp of 0 got a buffer");
    assert_eq!(create(0, 4, 32), einval);
    assert_eq!(create(64, 0, 32), einval);
    assert_eq!(create(64, 4, u32::MAX - 7), einval);

    let mut handles = alloc::vec::Vec::new();
    for (bpp, pitch) in [(32, 256), (16, 128), (8, 64), (24, 192), (12, 128), (1, 64)] {
        let (p, size, handle) = create(64, 4, bpp).expect("CREATE_DUMB");
        assert_eq!(p, pitch, "pitch for bpp {}", bpp);
        assert_eq!(size, pitch as u64 * 4, "size for bpp {}", bpp);
        handles.push(handle);
    }
    for mut handle in handles {
        c.ioctl(DRM_IOCTL_MODE_DESTROY_DUMB, &mut handle)
            .expect("DESTROY_DUMB");
    }
}
