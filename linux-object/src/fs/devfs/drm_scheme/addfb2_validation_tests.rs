//! What `ADDFB2` refuses, driven through the ioctl entry point the way
//! `drmModeAddFB2WithModifiers` drives it. Linux's
//! `drm_internal_framebuffer_create` / `framebuffer_check` reject an
//! unknown flag, a modifier without `DRM_MODE_FB_MODIFIERS`, a format no
//! plane scans out, a zero dimension, a missing handle, a pitch shorter
//! than a row, and anything on the planes the format does not have,
//! every one of them EINVAL and none of them creating a framebuffer.
//! This arm used to look only at the handle and the pitch: a client
//! probing formats got a framebuffer of `NV12` that scanned out as
//! `XRGB8888`, and the test helper above registered `XRC4`, a fourcc
//! that does not exist, for a hundred tests without anyone noticing.
use super::gl_client_sequence_tests::Client;
use super::*;

/// `DRM_FORMAT_NV12`: a real fourcc, two planes, and nothing here scans
/// it out.
const DRM_FORMAT_NV12: u32 = 0x3231_564e;
/// The fourcc the test helper used to send, which is no format at all.
const NOT_A_FOURCC: u32 = 0x3443_5258;

fn cmd(buf: &DrmModeCreateDumb) -> DrmModeFbCmd2 {
    DrmModeFbCmd2 {
        fb_id: 0,
        width: buf.width,
        height: buf.height,
        pixel_format: drm::DRM_FORMAT_XRGB8888,
        flags: 0,
        handles: [buf.handle, 0, 0, 0],
        pitches: [buf.pitch, 0, 0, 0],
        offsets: [0; 4],
        modifier: [0; 4],
    }
}

/// `ADDFB2` with `cmd`, and the framebuffer table before and after.
fn addfb2(client: &Client, cmd: &mut DrmModeFbCmd2) -> (Result<usize>, usize, usize) {
    let before = drm::table_sizes_for_test().0;
    let r = client.ioctl(DRM_IOCTL_MODE_ADDFB2, cmd);
    (r, before, drm::table_sizes_for_test().0)
}

/// Every refusal is the same three things: EINVAL, no fb id written,
/// and the framebuffer table untouched.
#[track_caller]
fn refused(client: &Client, mut cmd: DrmModeFbCmd2, what: &str) {
    let (r, before, after) = addfb2(client, &mut cmd);
    assert_eq!(r, Err(FsError::InvalidParam), "{what}: not EINVAL");
    assert_eq!(cmd.fb_id, 0, "{what}: an fb id came back with the error");
    assert_eq!(after, before, "{what}: a framebuffer was created anyway");
}

#[track_caller]
fn accepted(client: &Client, mut cmd: DrmModeFbCmd2, what: &str) -> u32 {
    let (r, before, after) = addfb2(client, &mut cmd);
    assert_eq!(r, Ok(0), "{what}: refused");
    assert_ne!(cmd.fb_id, 0, "{what}: no fb id");
    assert_eq!(after, before + 1, "{what}: no framebuffer in the table");
    cmd.fb_id
}

fn getfb2(client: &Client, fb_id: u32) -> DrmModeFbCmd2 {
    let mut q = DrmModeFbCmd2 {
        fb_id,
        width: 0,
        height: 0,
        pixel_format: 0,
        flags: 0,
        handles: [0; 4],
        pitches: [0; 4],
        offsets: [0; 4],
        modifier: [0; 4],
    };
    client.ioctl(DRM_IOCTL_MODE_GETFB2, &mut q).expect("GETFB2");
    q
}

fn getfb_depth(client: &Client, fb_id: u32) -> u32 {
    let mut q = DrmModeFbCmd {
        fb_id,
        width: 0,
        height: 0,
        pitch: 0,
        bpp: 0,
        depth: 0,
        handle: 0,
    };
    client.ioctl(DRM_IOCTL_MODE_GETFB, &mut q).expect("GETFB");
    q.depth
}

/// A format no plane scans out is EINVAL, and so is a fourcc that is
/// not a format. The two this tree knows go through, and `GETFB2` gives
/// each one back as registered, with `GETFB` reporting the depth Linux
/// derives from it (24 without alpha, 32 with).
#[test]
fn only_the_scanout_formats_are_accepted_and_come_back_as_registered() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    let mut nv12 = cmd(&buf);
    nv12.pixel_format = DRM_FORMAT_NV12;
    refused(&client, nv12, "NV12");
    let mut xrc4 = cmd(&buf);
    xrc4.pixel_format = NOT_A_FOURCC;
    refused(&client, xrc4, "XRC4");
    let mut zero = cmd(&buf);
    zero.pixel_format = 0;
    refused(&client, zero, "format 0");

    let xr24 = accepted(&client, cmd(&buf), "XR24");
    let mut ar = cmd(&buf);
    ar.pixel_format = drm::DRM_FORMAT_ARGB8888;
    let ar24 = accepted(&client, ar, "AR24");

    assert_eq!(getfb2(&client, xr24).pixel_format, drm::DRM_FORMAT_XRGB8888);
    assert_eq!(getfb2(&client, ar24).pixel_format, drm::DRM_FORMAT_ARGB8888);
    assert_eq!(
        getfb_depth(&client, xr24),
        24,
        "XR24 has no alpha: depth 24"
    );
    assert_eq!(getfb_depth(&client, ar24), 32, "AR24 has alpha: depth 32");

    assert_eq!(client.rmfb(xr24), Ok(0));
    assert_eq!(client.rmfb(ar24), Ok(0));
    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}

/// `DRM_MODE_FB_INTERLACED` is a flag this tree accepts (and ignores, as
/// most drivers do). `DRM_MODE_FB_MODIFIERS` is refused because no
/// modifier is supported, and so is any bit Linux does not define.
#[test]
fn interlaced_is_the_only_flag_a_client_may_set() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    let mut interlaced = cmd(&buf);
    interlaced.flags = DRM_MODE_FB_INTERLACED;
    let fb = accepted(&client, interlaced, "INTERLACED");
    assert_eq!(client.rmfb(fb), Ok(0));

    let mut modifiers = cmd(&buf);
    modifiers.flags = DRM_MODE_FB_MODIFIERS;
    refused(&client, modifiers, "MODIFIERS with a linear modifier");

    let mut unknown = cmd(&buf);
    unknown.flags = 1 << 2;
    refused(&client, unknown, "flag bit 2");
    let mut high = cmd(&buf);
    high.flags = 1 << 31;
    refused(&client, high, "flag bit 31");
    let mut both = cmd(&buf);
    both.flags = DRM_MODE_FB_INTERLACED | (1 << 5);
    refused(&client, both, "INTERLACED plus an unknown bit");

    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}

/// `DRM_CAP_ADDFB2_MODIFIERS` as a client reads it.
fn addfb2_modifiers_cap(c: &Client) -> u64 {
    let mut req = DrmGetCap {
        capability: 0x10,
        value: 0xdead_beef,
    };
    c.ioctl(DRM_IOCTL_GET_CAP, &mut req).expect("GET_CAP");
    req.value
}

/// With modifiers off -- the default -- the cap and `ADDFB2` have to
/// give the same answer: a client that asked `DRM_CAP_ADDFB2_MODIFIERS`
/// and was told 0 must not then find `DRM_MODE_FB_MODIFIERS` accepted,
/// and the other way round.
///
/// With them on, a framebuffer whose modifier names a layout this GPU
/// really produces is accepted, and the present declines it, which is
/// how a DRM driver says "not scanout-able". Accepting it and copying it
/// as if it were pitched is the desktop full of garbage this gate
/// exists to prevent.
#[test]
fn the_modifier_cap_and_what_addfb2_takes_are_the_same_switch() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    // DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0, 1, 2, 0x06, 0): one GOB
    // per block, so the surface is its own 64x8 block and a 64x64 dumb
    // buffer is big enough. The pitch counts BLOCKS: 64 pixels of 4
    // bytes is 256 bytes, which is 4 blocks.
    let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, 0);
    let tiled = |b: &DrmModeCreateDumb| {
        let mut c = cmd(b);
        c.flags = DRM_MODE_FB_MODIFIERS;
        c.modifier[0] = turing;
        c.pitches[0] = 4;
        c
    };

    // Off (the default).
    assert!(!drm::scanout_modifiers_enabled());
    assert_eq!(addfb2_modifiers_cap(&client), 0, "DRM_CAP_ADDFB2_MODIFIERS");
    refused(&client, tiled(&buf), "a modifier while the cap says 0");
    let mut linear_flagged = cmd(&buf);
    linear_flagged.flags = DRM_MODE_FB_MODIFIERS;
    refused(
        &client,
        linear_flagged,
        "even DRM_FORMAT_MOD_LINEAR behind the flag, while the cap says 0",
    );

    // On.
    drm::set_scanout_modifiers_enabled(true);
    assert_eq!(addfb2_modifiers_cap(&client), 1, "DRM_CAP_ADDFB2_MODIFIERS");
    let fb = accepted(&client, tiled(&buf), "a Turing block-linear modifier");

    // ...and the present declines it rather than painting it.
    assert_eq!(
        drm::present_now_checked(fb, drm::SYNTH_CRTC_ID, None),
        Err(drm::PresentError::UnsupportedLayout),
        "a tiled framebuffer must not be copied as if it were pitched"
    );
    // A linear one alongside it is NOT refused for its layout, so the
    // decline is about the tiling and not about the flag being on. (It
    // still fails for want of an emulated display in this test, which is
    // a different error and the point: the layout check comes first,
    // because an unreadable layout is the framebuffer's own defect and
    // holds whether or not anything is plugged in.)
    let mut linear = cmd(&buf);
    linear.flags = DRM_MODE_FB_MODIFIERS;
    let plain = accepted(&client, linear, "DRM_FORMAT_MOD_LINEAR with the flag");
    assert_ne!(
        drm::present_now_checked(plain, drm::SYNTH_CRTC_ID, None),
        Err(drm::PresentError::UnsupportedLayout),
        "a linear framebuffer is readable whatever the flag says"
    );

    drm::set_scanout_modifiers_enabled(false);
}

/// `GETFB2` has to describe the framebuffer that exists, modifier and
/// all. Reporting 0 for a tiled one describes a DIFFERENT surface --
/// same handle, same pitch number, linear -- and a client that
/// re-creates it from the readback gets the garbage this layout is
/// gated against.
#[test]
fn a_tiled_framebuffer_reads_back_as_the_modifier_it_was_made_with() {
    let _serialised = drm::test_globals::lock();
    drm::set_scanout_modifiers_enabled(true);
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    for h in 0..=2u64 {
        let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, h);
        let mut c = cmd(&buf);
        c.flags = DRM_MODE_FB_MODIFIERS;
        c.modifier[0] = turing;
        c.pitches[0] = 4;
        let fb = accepted(&client, c, "a Turing block-linear modifier");
        let back = getfb2(&client, fb);
        assert_eq!(back.modifier[0], turing, "h={} did not round-trip", h);
        assert_ne!(
            back.flags & DRM_MODE_FB_MODIFIERS,
            0,
            "h={}: the modifier is only meaningful with the flag",
            h
        );
    }

    // And a linear framebuffer still reads back as one, with no flag.
    let plain = accepted(&client, cmd(&buf), "a plain linear framebuffer");
    let back = getfb2(&client, plain);
    assert_eq!(back.modifier[0], 0);
    assert_eq!(back.flags & DRM_MODE_FB_MODIFIERS, 0);

    drm::set_scanout_modifiers_enabled(false);
}

/// `GETFB2` answers the pitch in the units `ADDFB2` took it, which for a
/// tiled framebuffer is 64-byte blocks.
///
/// The framebuffer stores bytes, so reporting the stored number straight
/// describes a surface 64 times wider than the one that exists -- and a
/// client that re-creates the framebuffer from its own readback gets
/// EINVAL at best and the wrong surface at worst. Round-tripping it is
/// the whole point of answering the modifier in the first place.
#[test]
fn a_tiled_framebuffer_reads_back_the_pitch_it_was_made_with() {
    let _serialised = drm::test_globals::lock();
    drm::set_scanout_modifiers_enabled(true);
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);
    let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, 0);

    // 64 pixels x 4 bytes = 256 bytes = 4 blocks of 64.
    let mut c = cmd(&buf);
    c.flags = DRM_MODE_FB_MODIFIERS;
    c.modifier[0] = turing;
    c.pitches[0] = 4;
    let fb = accepted(&client, c, "a Turing block-linear modifier");
    assert_eq!(
        getfb2(&client, fb).pitches[0],
        4,
        "the readback has to be in blocks, like the request was"
    );

    // A linear framebuffer keeps speaking bytes.
    let plain = accepted(&client, cmd(&buf), "a plain linear framebuffer");
    assert_eq!(getfb2(&client, plain).pitches[0], cmd(&buf).pitches[0]);

    drm::set_scanout_modifiers_enabled(false);
}

/// The pitch of a block-linear framebuffer counts 64-byte blocks, so the
/// "shorter than a row" check has to multiply before it compares.
/// Reading it as bytes rejects every real tiled framebuffer by a factor
/// of 64; not reading it at all accepts one 64 times too small and lets
/// the present walk off the buffer.
#[test]
fn a_block_linear_pitch_is_counted_in_blocks() {
    let _serialised = drm::test_globals::lock();
    drm::set_scanout_modifiers_enabled(true);
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);
    let turing = nvidia_block_linear_2d(0, 1, 2, 0x06, 0);
    let with_pitch = |blocks: u32| {
        let mut c = cmd(&buf);
        c.flags = DRM_MODE_FB_MODIFIERS;
        c.modifier[0] = turing;
        c.pitches[0] = blocks;
        c
    };

    // 64 pixels x 4 bytes = 256 bytes = 4 blocks. Four is exactly a row.
    let fb = accepted(&client, with_pitch(4), "a pitch of exactly one row");
    assert_ne!(fb, 0);
    // Three blocks is 192 bytes, short of the 256 a row needs.
    refused(&client, with_pitch(3), "a pitch shorter than a row");
    // And the byte count that would be right for a LINEAR fb is 256
    // blocks, i.e. 16 KiB per row -- far past this 16 KiB buffer once
    // the eight rows of the block are counted.
    refused(
        &client,
        with_pitch(256),
        "a pitch given in bytes by mistake",
    );

    drm::set_scanout_modifiers_enabled(false);
}

/// A modifier without the flag, and anything at all on planes 1 to 3 of
/// a one-plane format, are `framebuffer_check`'s "bad fb modifier" and
/// "buffer object handle for plane N" refusals.
#[test]
fn a_modifier_or_a_second_plane_on_a_linear_one_plane_format_is_refused() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    // DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(0,0,0,0,0): a real modifier.
    let mut tiled = cmd(&buf);
    tiled.modifier[0] = 0x0300_0000_0000_0010;
    refused(&client, tiled, "a tiled modifier without the flag");
    let mut inv = cmd(&buf);
    inv.modifier[0] = u64::MAX;
    refused(&client, inv, "DRM_FORMAT_MOD_INVALID");

    for plane in 1..4 {
        let mut h = cmd(&buf);
        h.handles[plane] = buf.handle;
        refused(&client, h, "a handle on an extra plane");
        let mut p = cmd(&buf);
        p.pitches[plane] = buf.pitch;
        refused(&client, p, "a pitch on an extra plane");
        let mut o = cmd(&buf);
        o.offsets[plane] = 64;
        refused(&client, o, "an offset on an extra plane");
        let mut m = cmd(&buf);
        m.modifier[plane] = 1;
        refused(&client, m, "a modifier on an extra plane");
    }

    // What a real NV12 client sends: two planes, second handle and
    // pitch set. Refused for the format before the planes are looked
    // at, and still refused.
    let mut nv12 = cmd(&buf);
    nv12.pixel_format = DRM_FORMAT_NV12;
    nv12.handles[1] = buf.handle;
    nv12.pitches[1] = buf.pitch;
    nv12.offsets[1] = buf.pitch * 64;
    refused(&client, nv12, "a two-plane NV12");

    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}

/// Plane 0 itself: a zero dimension, no handle, a pitch shorter than a
/// row of pixels, or an offset into the buffer (which this tree does not
/// scan out from) are all EINVAL, where the short pitch used to be
/// EFAULT-flavoured `DeviceError` and the rest were accepted.
#[test]
fn plane_zero_needs_a_size_a_handle_a_full_pitch_and_no_offset() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    let mut w0 = cmd(&buf);
    w0.width = 0;
    refused(&client, w0, "width 0");
    let mut h0 = cmd(&buf);
    h0.height = 0;
    refused(&client, h0, "height 0");
    let mut nh = cmd(&buf);
    nh.handles[0] = 0;
    refused(&client, nh, "handle 0");
    let mut short = cmd(&buf);
    short.pitches[0] = buf.width * 4 - 4;
    refused(&client, short, "a pitch one pixel short");
    let mut zero_pitch = cmd(&buf);
    zero_pitch.pitches[0] = 0;
    refused(&client, zero_pitch, "pitch 0");
    let mut wide = cmd(&buf);
    wide.width = u32::MAX;
    refused(&client, wide, "a width whose row overflows u32");
    // A width whose row, multiplied in u32, wraps to 64 bytes: the pitch
    // comparison has to be done wider than the fields are.
    let mut wrap = cmd(&buf);
    wrap.width = 0x4000_0010;
    refused(&client, wrap, "a width whose row wraps to a short one");
    let mut off = cmd(&buf);
    off.offsets[0] = 64;
    refused(&client, off, "an offset on plane 0");

    // The exact pitch is fine, and so is one wider than the row: a
    // narrow framebuffer over a wide buffer is what `addfb2_narrow`
    // registers for the alignment tests.
    let mut exact = cmd(&buf);
    exact.pitches[0] = buf.width * 4;
    let fb = accepted(&client, exact, "an exact pitch");
    assert_eq!(client.rmfb(fb), Ok(0));
    let mut narrow = cmd(&buf);
    narrow.width = 32;
    let fb = accepted(&client, narrow, "a narrow fb over a wide buffer");
    assert_eq!(client.rmfb(fb), Ok(0));

    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}

/// `ADDFB` with `bpp`/`depth`, and the framebuffer table before and
/// after.
fn addfb(
    client: &Client,
    buf: &DrmModeCreateDumb,
    bpp: u32,
    depth: u32,
) -> (Result<usize>, u32, usize, usize) {
    let mut cmd = DrmModeFbCmd {
        fb_id: 0,
        width: buf.width,
        height: buf.height,
        pitch: buf.pitch,
        bpp,
        depth,
        handle: buf.handle,
    };
    let before = drm::table_sizes_for_test().0;
    let r = client.ioctl(DRM_IOCTL_MODE_ADDFB, &mut cmd);
    (r, cmd.fb_id, before, drm::table_sizes_for_test().0)
}

/// `drm_mode_legacy_fb_format`'s table, and nothing outside it.
#[test]
fn the_legacy_bpp_depth_table_is_linuxs() {
    assert_eq!(legacy_fb_format(32, 24), Some(drm::DRM_FORMAT_XRGB8888));
    assert_eq!(legacy_fb_format(32, 32), Some(drm::DRM_FORMAT_ARGB8888));
    // Real formats no plane here scans out: fourccs, spelled as Linux
    // spells them.
    assert_eq!(legacy_fb_format(8, 8), Some(u32::from_le_bytes(*b"C8  ")));
    assert_eq!(legacy_fb_format(16, 15), Some(u32::from_le_bytes(*b"XR15")));
    assert_eq!(legacy_fb_format(16, 16), Some(u32::from_le_bytes(*b"RG16")));
    assert_eq!(legacy_fb_format(24, 24), Some(u32::from_le_bytes(*b"RG24")));
    assert_eq!(legacy_fb_format(32, 30), Some(u32::from_le_bytes(*b"XR30")));
    for (bpp, depth) in [
        (0, 0),
        (32, 16),
        (16, 24),
        (24, 32),
        (64, 64),
        (32, 0),
        (0, 24),
    ] {
        assert_eq!(legacy_fb_format(bpp, depth), None, "{bpp}/{depth}");
    }
}

/// The legacy `ADDFB` is an `ADDFB2` with the fourcc derived from
/// (bpp, depth): 32/24 registers XRGB8888 and 32/32 ARGB8888 (`GETFB2`
/// gives the format back and `GETFB` the depth), every other pair is
/// EINVAL with no framebuffer created, whether the table knows it
/// (16/16 is RGB565, which nothing here scans out) or not (32/16), and
/// a pitch shorter than a row is refused as it is on `ADDFB2`. This
/// arm read neither field: a 16-bit framebuffer scanned out as
/// XRGB8888 garbage, and an ARGB8888 one came back as depth 24.
#[test]
fn legacy_addfb_derives_the_format_from_bpp_and_depth() {
    let _serialised = drm::test_globals::lock();
    let client = Client::open(0);
    let buf = client.create_dumb(64, 64);

    let (r, fb, before, after) = addfb(&client, &buf, 32, 24);
    assert_eq!(r, Ok(0));
    assert_eq!(after, before + 1);
    assert_eq!(getfb2(&client, fb).pixel_format, drm::DRM_FORMAT_XRGB8888);
    assert_eq!(getfb_depth(&client, fb), 24);
    assert_eq!(client.rmfb(fb), Ok(0));

    let (r, fb, before, after) = addfb(&client, &buf, 32, 32);
    assert_eq!(r, Ok(0));
    assert_eq!(after, before + 1);
    assert_eq!(getfb2(&client, fb).pixel_format, drm::DRM_FORMAT_ARGB8888);
    assert_eq!(getfb_depth(&client, fb), 32, "32/32 is ARGB8888, depth 32");
    assert_eq!(client.rmfb(fb), Ok(0));

    for (bpp, depth) in [
        (16, 16),
        (16, 15),
        (24, 24),
        (8, 8),
        (32, 30),
        (32, 16),
        (0, 0),
    ] {
        let (r, fb, before, after) = addfb(&client, &buf, bpp, depth);
        assert_eq!(r, Err(FsError::InvalidParam), "{bpp}/{depth}: not EINVAL");
        assert_eq!(fb, 0, "{bpp}/{depth}: an fb id came back with the error");
        assert_eq!(
            after, before,
            "{bpp}/{depth}: a framebuffer was created anyway"
        );
    }

    // The ADDFB2 checks apply to the legacy form too: a pitch shorter
    // than a row of 4-byte pixels.
    let mut short = DrmModeFbCmd {
        fb_id: 0,
        width: buf.width,
        height: buf.height,
        pitch: buf.width * 4 - 4,
        bpp: 32,
        depth: 24,
        handle: buf.handle,
    };
    assert_eq!(
        client.ioctl(DRM_IOCTL_MODE_ADDFB, &mut short),
        Err(FsError::InvalidParam)
    );
    assert_eq!(short.fb_id, 0);

    assert_eq!(client.destroy_dumb(buf.handle), Ok(0));
}
