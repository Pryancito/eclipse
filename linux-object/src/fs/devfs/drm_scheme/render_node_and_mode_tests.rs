//! Two things in this file that regressed once each and had no test.
//!
//! The render node's allow-list used to read the ioctl TYPE byte where it
//! meant the NR. Every DRM ioctl has type `'d'` (0x64), and 0x64 falls
//! inside the driver-private `0x40..=0x9F` arm, so the filter matched
//! *everything*: an unprivileged render client could issue modesetting.
//!
//! And the synthetic mode's porches were once computed as +10%/+5%, which
//! put `hsync_end` past `htotal` at 1366x768. That is MODE_H_ILLEGAL, and
//! compositors that recompute the refresh from the porches then advertised
//! 55-59 Hz instead of 60.

use super::*;

/// Decode `struct drm_mode_modeinfo` far enough to check its timings.
fn timings(m: &[u8; 68]) -> (u32, [u16; 4], [u16; 4], u32) {
    let u16_at = |o: usize| u16::from_ne_bytes([m[o], m[o + 1]]);
    let u32_at = |o: usize| u32::from_ne_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]]);
    (
        u32_at(0),
        [u16_at(4), u16_at(6), u16_at(8), u16_at(10)],
        [u16_at(14), u16_at(16), u16_at(18), u16_at(20)],
        u32_at(24),
    )
}

/// Every resolution the GOP is likely to hand us, plus the one that broke.
const MODES: &[(u32, u32)] = &[
    (640, 480),
    (800, 600),
    (1024, 768),
    (1280, 720),
    (1280, 1024),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1440),
    (3840, 2160),
];

#[test]
fn every_mode_has_strictly_increasing_porches() {
    // `hdisplay < hsync_start < hsync_end < htotal`, and the vertical
    // analogue. Anything else is MODE_H_ILLEGAL / MODE_V_ILLEGAL and the
    // mode is rejected or mis-timed by whoever reads it.
    for &(w, h) in MODES {
        let m = make_modeinfo(w, h);
        let (_, hor, vert, _) = timings(&m);
        assert!(
            hor[0] < hor[1] && hor[1] < hor[2] && hor[2] < hor[3],
            "{}x{} horizontal timings not strictly increasing: {:?}",
            w,
            h,
            hor
        );
        assert!(
            vert[0] < vert[1] && vert[1] < vert[2] && vert[2] < vert[3],
            "{}x{} vertical timings not strictly increasing: {:?}",
            w,
            h,
            vert
        );
    }
}

#[test]
fn every_mode_reports_the_resolution_it_was_asked_for() {
    for &(w, h) in MODES {
        let m = make_modeinfo(w, h);
        let (_, hor, vert, _) = timings(&m);
        assert_eq!(hor[0] as u32, w, "hdisplay for {}x{}", w, h);
        assert_eq!(vert[0] as u32, h, "vdisplay for {}x{}", w, h);
    }
}

#[test]
fn the_refresh_recomputed_from_the_porches_is_sixty_hertz() {
    // This is the check the compositor makes. wlroots and Mesa do not
    // trust `vrefresh`; they recompute it in millihertz from the clock and
    // the totals, and that is what showed 55-59 Hz when the porches were
    // wrong.
    for &(w, h) in MODES {
        let m = make_modeinfo(w, h);
        let (clock, hor, vert, vrefresh) = timings(&m);
        let htotal = hor[3] as u64;
        let vtotal = vert[3] as u64;
        let mhz = (clock as u64 * 1_000_000 / htotal + vtotal / 2) / vtotal;
        // A tolerance, not an equality: the pixel clock is a whole number
        // of kHz and is rounded UP, so the recomputed refresh lands on
        // 60_000 or a hair above it (800x600 gives 60_001).
        //
        // Worth knowing what this test does NOT catch: the clock is
        // derived from `htotal` and `vtotal`, so the arithmetic is
        // self-consistent and *any* porch values recompute to 60 Hz.
        // Wrong porches are caught by
        // `every_mode_has_strictly_increasing_porches`, not here. This one
        // guards the clock, the rounding and the advertised `vrefresh`.
        assert!(
            (60_000..=60_010).contains(&mhz),
            "{}x{} recomputes to {} mHz (clock {} kHz, htotal {}, vtotal {})",
            w,
            h,
            mhz,
            clock,
            htotal,
            vtotal
        );
        assert_eq!(vrefresh, 60, "the advertised vrefresh must agree");
    }
}

#[test]
fn the_mode_name_is_the_resolution_and_is_nul_terminated() {
    let m = make_modeinfo(1920, 1080);
    let name = &m[36..68];
    let end = name
        .iter()
        .position(|&b| b == 0)
        .expect("name must be terminated");
    assert_eq!(&name[..end], b"1920x1080");
}

#[test]
fn a_zero_refresh_target_does_not_underflow() {
    // `refresh_mhz * vtotal - vtotal / 2` goes negative when the target is
    // zero, which is a panic in debug. Only one caller passes 60_000
    // today, but the parameter is there to be passed.
    assert_eq!(
        clock_khz_for_refresh_mhz(2080, 1121, 0),
        1,
        "a zero target gives the slowest clock that is still a clock"
    );
    assert_eq!(clock_khz_for_refresh_mhz(0, 1121, 60_000), 0);
    assert_eq!(clock_khz_for_refresh_mhz(2080, 0, 60_000), 0);
}

#[test]
fn the_clock_is_never_zero_for_a_real_mode() {
    // A mode with clock 0 is rejected outright by every compositor.
    for &(w, h) in MODES {
        let (clock, _, _, _) = timings(&make_modeinfo(w, h));
        assert!(clock > 0, "{}x{} got a zero pixel clock", w, h);
    }
}

/// A `DetailedTiming` built straight, so these tests never touch the
/// process-wide boot EDID (which no test sets and every one of them reads).
#[allow(clippy::too_many_arguments)]
fn panel(
    clock_khz: u32,
    (hd, hss, hse, ht): (u32, u32, u32, u32),
    (vd, vss, vse, vt): (u32, u32, u32, u32),
    interlaced: bool,
) -> edid::DetailedTiming {
    edid::DetailedTiming {
        clock_khz,
        hdisplay: hd,
        hsync_start: hss,
        hsync_end: hse,
        htotal: ht,
        vdisplay: vd,
        vsync_start: vss,
        vsync_end: vse,
        vtotal: vt,
        interlaced,
        separate_sync: true,
        hsync_positive: false,
        vsync_positive: true,
    }
}

/// `1920x1080` at the given refresh, with the DMT geometry. The clock is
/// chosen so the refresh is exact, which is what makes the assertions
/// numbers instead of ranges.
fn dmt_1080p(hz: u32) -> edid::DetailedTiming {
    panel(
        hz * 2200 * 1125 / 1000,
        (1920, 2008, 2052, 2200),
        (1080, 1084, 1089, 1125),
        false,
    )
}

#[test]
fn with_no_edid_the_mode_is_the_nominal_one_byte_for_byte() {
    // The regression guard for everything below: a machine whose firmware
    // read no EDID -- every VM, and the case the whole suite runs in --
    // must get exactly the mode it got before the panel timing existed.
    for &(w, h) in MODES {
        let m = make_modeinfo_with(w, h, None);
        let (clock, hor, vert, vrefresh) = timings(&m);
        let hd = w as u16;
        let vd = h as u16;
        assert_eq!(hor, [hd, hd + 48, hd + 80, hd + 160], "{}x{}", w, h);
        assert_eq!(vert, [vd, vd + 3, vd + 9, vd + 41], "{}x{}", w, h);
        assert_eq!(vrefresh, 60, "{}x{}", w, h);
        assert_eq!(
            clock,
            clock_khz_for_refresh_mhz(hor[3] as u32, vert[3] as u32, 60_000),
            "{}x{}",
            w,
            h
        );
        // -hsync/-vsync, and no interlace.
        assert_eq!(u32::from_ne_bytes([m[28], m[29], m[30], m[31]]), 0x0A);
        // And that is what `make_modeinfo` itself builds, since no test
        // sets a boot EDID.
        assert_eq!(make_modeinfo(w, h), m, "{}x{}", w, h);
    }
}

#[test]
fn a_panel_faster_than_sixty_is_advertised_at_its_own_refresh() {
    // The reason this path exists. The kernel used to answer 60 Hz for
    // every monitor, because 60 was the only refresh it could name: the
    // EDID's pixel clock was decoded nowhere. A compositor told 60 paces
    // its repaints and its WAIT_VBLANK sleeps to 16.7 ms, so on this panel
    // better than half the scanouts show a frame that is already up.
    let m = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(144)));
    let (clock, hor, vert, vrefresh) = timings(&m);
    assert_eq!(vrefresh, 144);
    assert_eq!(clock, 356_400, "the panel's own pixel clock, in kHz");
    assert_eq!(hor, [1920, 2008, 2052, 2200], "the panel's own porches");
    assert_eq!(vert, [1080, 1084, 1089, 1125]);
    // The number that matters is not the one in the `vrefresh` field but
    // the one the pacing is derived from, and both have to agree: a client
    // that leaves `vrefresh` at 0 gets the refresh recomputed from these
    // porches, and that is the path `set_vblank_period_from_modeinfo` runs.
    assert_eq!(drm::refresh_hz_from_modeinfo(&m), Some(144));
    let mut no_vrefresh = m;
    no_vrefresh[24..28].copy_from_slice(&0u32.to_ne_bytes());
    assert_eq!(drm::refresh_hz_from_modeinfo(&no_vrefresh), Some(144));
}

#[test]
fn every_refresh_a_panel_can_state_survives_the_round_trip() {
    // Not just 144: the mode has to carry whatever the monitor says, and
    // the two answers (the stated field and the one recomputed from the
    // porches) have to agree for each, or a compositor gets a different
    // rate depending on which it trusts.
    for hz in [50u32, 60, 75, 100, 120, 144, 165, 240] {
        let m = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(hz)));
        let (_, _, _, vrefresh) = timings(&m);
        assert_eq!(vrefresh, hz, "stated refresh for {} Hz", hz);
        assert_eq!(
            drm::refresh_hz_from_modeinfo(&m),
            Some(hz as u64),
            "recomputed refresh for {} Hz",
            hz
        );
    }
}

#[test]
fn a_panel_whose_native_mode_is_not_the_one_on_screen_is_not_used() {
    // Firmware picks the mode; a 4K panel driven at 1080p is the ordinary
    // case. Its preferred timing then describes 3840x2160 at some refresh
    // that has nothing to do with what is scanning out, so using it would
    // pace the compositor to a mode nobody is displaying.
    let native_4k = panel(
        594_000,
        (3840, 4016, 4104, 4400),
        (2160, 2168, 2178, 2250),
        false,
    );
    let m = make_modeinfo_with(1920, 1080, Some(&native_4k));
    assert_eq!(m, make_modeinfo_with(1920, 1080, None), "must be nominal");
    // One axis matching is not matching.
    let same_width = panel(
        148_500,
        (1920, 2008, 2052, 2200),
        (1200, 1204, 1209, 1245),
        false,
    );
    assert_eq!(
        make_modeinfo_with(1920, 1080, Some(&same_width)),
        make_modeinfo_with(1920, 1080, None)
    );
    // And when it does match, it is used -- otherwise this test would pass
    // with the panel timing wired to nothing.
    assert_ne!(
        make_modeinfo_with(1920, 1080, Some(&dmt_1080p(144))),
        make_modeinfo_with(1920, 1080, None)
    );
}

#[test]
fn an_illegal_panel_timing_falls_back_instead_of_being_advertised() {
    // The one way this change could cost Moebius his desktop. A sync pulse
    // ending past the total is MODE_H_ILLEGAL, and wlroots handed such a
    // mode drops the output rather than picking another -- the desktop
    // falls back to the text console. So a monitor whose descriptor is
    // line noise has to land on the nominal mode, not on the screen.
    let nominal = make_modeinfo_with(1920, 1080, None);
    for bad in [
        // hsync_end past htotal
        panel(
            148_500,
            (1920, 2008, 2300, 2200),
            (1080, 1084, 1089, 1125),
            false,
        ),
        // hsync_start before hdisplay
        panel(
            148_500,
            (1920, 1900, 2052, 2200),
            (1080, 1084, 1089, 1125),
            false,
        ),
        // vsync_end past vtotal
        panel(
            148_500,
            (1920, 2008, 2052, 2200),
            (1080, 1084, 1200, 1125),
            false,
        ),
        // a zero clock: no refresh to derive
        panel(0, (1920, 2008, 2052, 2200), (1080, 1084, 1089, 1125), false),
        // a total that does not fit the 16-bit uAPI field
        panel(
            148_500,
            (1920, 2008, 2052, 70_000),
            (1080, 1084, 1089, 1125),
            false,
        ),
        // a clock so slow the refresh rounds to zero
        panel(1, (1920, 2008, 2052, 2200), (1080, 1084, 1089, 1125), false),
    ] {
        let m = make_modeinfo_with(1920, 1080, Some(&bad));
        assert_eq!(m, nominal, "an unusable timing reached the mode: {:?}", bad);
    }
}

#[test]
fn the_panels_sync_polarity_and_interlace_reach_the_flags() {
    const PHSYNC: u32 = 1 << 0;
    const NHSYNC: u32 = 1 << 1;
    const PVSYNC: u32 = 1 << 2;
    const NVSYNC: u32 = 1 << 3;
    const INTERLACE: u32 = 1 << 4;
    let flags_of = |m: &[u8; 68]| u32::from_ne_bytes([m[28], m[29], m[30], m[31]]);

    let mut t = dmt_1080p(60);
    assert_eq!(
        flags_of(&make_modeinfo_with(1920, 1080, Some(&t))),
        NHSYNC | PVSYNC,
        "-hsync/+vsync, what the timing states"
    );
    t.hsync_positive = true;
    t.vsync_positive = false;
    assert_eq!(
        flags_of(&make_modeinfo_with(1920, 1080, Some(&t))),
        PHSYNC | NVSYNC
    );
    // A descriptor that does not use digital separate sync states no
    // polarity at all, and inventing one is what Linux declines to do.
    t.separate_sync = false;
    assert_eq!(flags_of(&make_modeinfo_with(1920, 1080, Some(&t))), 0);

    // 1080i60: the decoder has already doubled the vertical numbers, so
    // the mode is 1920x1080 with an odd total and the interlace flag. A
    // compositor that is not told it is interlaced renders half a frame.
    let i = panel(
        74_250,
        (1920, 2008, 2052, 2200),
        (1080, 1084, 1094, 1125),
        true,
    );
    let m = make_modeinfo_with(1920, 1080, Some(&i));
    assert_eq!(flags_of(&m) & INTERLACE, INTERLACE);
    let (_, _, vert, vrefresh) = timings(&m);
    assert_eq!(vert[3] % 2, 1, "an interlaced frame has an odd line count");
    assert_eq!(vrefresh, 60, "1080i60 is 60 FIELDS a second");
}

#[test]
fn a_panel_timing_is_still_a_legal_mode_by_the_rule_the_nominal_one_keeps() {
    // The porch invariant the nominal mode is held to, applied to the
    // other source. Non-strict here, because a real panel is allowed a
    // zero front porch or no back porch and Linux accepts it.
    for hz in [50u32, 60, 144, 240] {
        let m = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(hz)));
        let (clock, hor, vert, _) = timings(&m);
        assert!(clock > 0);
        assert!(
            hor[0] <= hor[1] && hor[1] <= hor[2] && hor[2] <= hor[3],
            "{:?}",
            hor
        );
        assert!(
            vert[0] <= vert[1] && vert[1] <= vert[2] && vert[2] <= vert[3],
            "{:?}",
            vert
        );
    }
    // And a zero-porch reduced-blanking panel is accepted, not refused.
    let rb = panel(
        148_500,
        (1920, 1920, 2000, 2000),
        (1080, 1080, 1125, 1125),
        false,
    );
    let m = make_modeinfo_with(1920, 1080, Some(&rb));
    assert_ne!(m, make_modeinfo_with(1920, 1080, None));
    let (_, hor, _, _) = timings(&m);
    assert_eq!(hor, [1920, 1920, 2000, 2000]);
}

#[test]
fn a_partly_read_edid_is_refused_rather_than_decoded() {
    // The buffer is a fixed 128 bytes whatever firmware managed to read, so
    // a short read leaves the tail as whatever was there before it. Taking
    // those bytes gives a pixel clock for a monitor that may not even be
    // plugged in, and from there a vblank period for a mode nobody has.
    let mut block = [0u8; edid::BLOCK_LEN];
    block[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    block[18] = 1;
    block[19] = 4;
    // The DMT 1080p60 descriptor, written straight into slot 0.
    let d: [u8; 18] = [
        0x02, 0x3A, 0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, 0x45, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x1E,
    ];
    block[54..72].copy_from_slice(&d);
    let sum = block[..edid::BLOCK_LEN - 1]
        .iter()
        .fold(0u8, |a, b| a.wrapping_add(*b));
    block[edid::BLOCK_LEN - 1] = sum.wrapping_neg();

    // A whole block decodes.
    let whole = panel_timing_in(&block, 128).expect("a whole block was refused");
    assert_eq!((whole.hdisplay, whole.vdisplay), (1920, 1080));
    assert_eq!(whole.refresh_hz(), 60);
    // And the same bytes, reported as a short read, do not.
    for len in [0u32, 1, 64, 127] {
        assert_eq!(
            panel_timing_in(&block, len),
            None,
            "a {}-byte read was decoded as a whole block",
            len
        );
    }
}

#[test]
fn the_mode_name_is_the_resolution_whichever_source_the_timings_came_from() {
    // Userspace matches modes by name, so the two sources must not name
    // the same mode differently.
    let a = make_modeinfo_with(1920, 1080, None);
    let b = make_modeinfo_with(1920, 1080, Some(&dmt_1080p(144)));
    assert_eq!(a[36..68], b[36..68]);
    assert_eq!(&a[36..45], b"1920x1080");
    assert_eq!(a[45], 0);
}

#[test]
fn the_render_node_refuses_modesetting() {
    // The regression: reading the TYPE byte instead of the NR matched
    // everything, because 'd' is 0x64 and 0x64 sits inside the
    // driver-private arm. These are the ioctls that must NEVER reach a
    // render client.
    for (nr, what) in [
        (0xA1u32, "MODE_GETRESOURCES"),
        (0xA2, "MODE_GETCRTC"),
        (0xA3, "MODE_SETCRTC"),
        (0xA6, "MODE_GETENCODER"),
        (0xA7, "MODE_GETCONNECTOR"),
        (0xAE, "MODE_ADDFB"),
        (0xAF, "MODE_RMFB"),
        (0xB0, "MODE_PAGE_FLIP"),
        (0xB7, "MODE_ADDFB2"),
        (0xBC, "MODE_ATOMIC"),
        (0x3A, "WAIT_VBLANK"),
        (0x02, "GET_MAGIC"),
        (0x11, "AUTH_MAGIC"),
        (0x07, "SET_MASTER"),
        (0x08, "DROP_MASTER"),
    ] {
        assert!(
            !render_allowed(nr),
            "{} (NR {:#04x}) must not be allowed on a render node",
            what,
            nr
        );
    }
}

#[test]
fn the_render_node_allows_what_a_render_client_needs() {
    for (nr, what) in [
        (0x00u32, "VERSION"),
        (0x09, "GEM_CLOSE"),
        (0x0C, "GET_CAP"),
        (0x2D, "PRIME_HANDLE_TO_FD"),
        (0x2E, "PRIME_FD_TO_HANDLE"),
        (0x40, "driver-private, first"),
        (0x9F, "driver-private, last"),
        (0xBF, "SYNCOBJ_CREATE"),
        (0xC5, "SYNCOBJ_SIGNAL"),
        (0xCA, "SYNCOBJ_TIMELINE_WAIT"),
        (0xCD, "SYNCOBJ_TIMELINE_SIGNAL"),
        (0xCF, "SYNCOBJ_EVENTFD"),
    ] {
        assert!(
            render_allowed(nr),
            "{} (NR {:#04x}) must be allowed on a render node",
            what,
            nr
        );
    }
}

#[test]
fn the_render_filter_reads_the_nr_and_not_the_type_byte() {
    // The shape of the bug, pinned directly: a full ioctl command word
    // whose TYPE byte is 'd' (0x64, inside the driver-private arm) but
    // whose NR is a modesetting one must still be refused. If the filter
    // ever goes back to reading `(cmd >> 8) & 0xff`, this is the test that
    // catches it.
    let setcrtc = 0xC068_64A3u32; // dir=RW, size=0x68, type='d', nr=0xA3
    assert_eq!((setcrtc >> 8) & 0xff, 0x64, "the type byte really is 'd'");
    assert!(
        (0x40..=0x9F).contains(&((setcrtc >> 8) & 0xff)),
        "and it really does fall inside the driver-private arm"
    );
    assert!(
        !render_allowed(setcrtc),
        "SETCRTC must be refused however the command word is dressed up"
    );
}

#[test]
fn a_render_node_refuses_modeset_and_dumb_with_eacces() {
    // Enforcement used to be observe-only: renderD128 accepted CREATE_DUMB
    // and SETCRTC. Linux answers EACCES; the helper already knew, the
    // ioctl path did not.
    use super::gl_client_sequence_tests::Client;
    use crate::error::LxError;
    let render = Client::open(128);
    let mut dumb = DrmModeCreateDumb {
        height: 16,
        width: 16,
        bpp: 32,
        flags: 0,
        handle: 0,
        pitch: 0,
        size: 0,
    };
    assert_eq!(
        render.ioctl(DRM_IOCTL_MODE_CREATE_DUMB, &mut dumb),
        Err(FsError::NoPermission)
    );
    assert_eq!(
        LxError::from(FsError::NoPermission),
        LxError::EACCES,
        "userspace must see EACCES on a render-node modeset/dumb"
    );
    // GET_CAP stays allowed on a render node.
    let mut cap = DrmGetCap {
        capability: 0x1, // DRM_CAP_DUMB_BUFFER
        value: 0,
    };
    assert!(render.ioctl(DRM_IOCTL_GET_CAP, &mut cap).is_ok());
}

#[test]
fn the_interception_filter_checks_type_number_and_a_size_floor() {
    // `is_drm_ioctl_nr` gates what `sys_ioctl` grabs before the inode
    // dispatch. Matching on the number alone would steal another
    // subsystem's ioctl that happens to share it.
    let cmd = |dir: u32, size: u32, ty: u32, nr: u32| (dir << 30) | (size << 16) | (ty << 8) | nr;
    let (vb_nr, vb_min) = nr::WAIT_VBLANK;
    assert!(is_drm_ioctl_nr(cmd(3, 24, 0x64, vb_nr), vb_nr, vb_min));
    assert!(
        !is_drm_ioctl_nr(cmd(3, 24, 0x65, vb_nr), vb_nr, vb_min),
        "another subsystem's type byte must not be intercepted"
    );
    assert!(
        !is_drm_ioctl_nr(cmd(3, 24, 0x64, vb_nr + 1), vb_nr, vb_min),
        "a different NR must not match"
    );
    assert!(
        !is_drm_ioctl_nr(cmd(3, 23, 0x64, vb_nr), vb_nr, vb_min),
        "a struct shorter than the one the helper parses must not match"
    );
    assert!(
        is_drm_ioctl_nr(cmd(3, 40, 0x64, vb_nr), vb_nr, vb_min),
        "a GROWN struct must still match: the floor is a minimum, not an equality"
    );
}
