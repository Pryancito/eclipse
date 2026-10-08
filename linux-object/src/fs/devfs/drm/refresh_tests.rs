use super::*;

/// Build the fields of a `drm_mode_modeinfo` that the reader looks at.
fn modeinfo(clock_khz: u32, htotal: u16, vtotal: u16, vrefresh: u32) -> [u8; 68] {
    let mut m = [0u8; 68];
    m[0..4].copy_from_slice(&clock_khz.to_ne_bytes());
    m[10..12].copy_from_slice(&htotal.to_ne_bytes());
    m[20..22].copy_from_slice(&vtotal.to_ne_bytes());
    m[24..28].copy_from_slice(&vrefresh.to_ne_bytes());
    m
}

/// A mode that states its own refresh is believed, timings or not: that is
/// the field Linux fills in and the one every mode this driver advertises
/// carries.
#[test]
fn a_stated_vrefresh_wins_over_the_timings() {
    let _serialised = super::test_globals::lock();
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(139_900, 2080, 1121, 144)),
        Some(144)
    );
}

/// The regression this guards. A pixel clock is stored in whole kHz, so it
/// cannot express an exact 60 Hz for most timings: the 1920x1080 mode this
/// driver itself advertises is 139900 kHz over 2080x1121, i.e. 59.9995 Hz.
/// Truncating division called that 59 and
/// `set_vblank_period_from_modeinfo` paced the synthetic vblank at 16.95 ms
/// instead of 16.67 ms. Linux rounds to nearest here
/// (`drm_mode_vrefresh`'s `DIV_ROUND_CLOSEST`), and a client that leaves
/// `vrefresh` at 0 -- legal, and what makes the kernel compute it -- is
/// exactly how SETCRTC and an atomic MODE_ID blob reach this path.
#[test]
fn a_derived_refresh_rounds_to_nearest_like_linux() {
    let _serialised = super::test_globals::lock();
    // 1920x1080, the mode `make_modeinfo` builds.
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(139_900, 2080, 1121, 0)),
        Some(60),
        "59.9995 Hz is 60, not 59"
    );
    // A few more of the modes that were reading one Hz slow.
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(65_750, 1440, 761, 0)),
        Some(60),
        "1280x720"
    );
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(74_072, 1526, 809, 0)),
        Some(60),
        "1366x768"
    );
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(528_236, 4000, 2201, 0)),
        Some(60),
        "3840x2160"
    );
    // Rounding to nearest, not simply up: a mode that really is closer to
    // 59 must not be promoted.
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(137_600, 2080, 1121, 0)),
        Some(59)
    );
}

/// An unusable blob must not be mistaken for a refresh rate; the caller
/// falls back to 60 Hz rather than dividing by zero or pacing off garbage.
#[test]
fn an_unusable_modeinfo_has_no_refresh() {
    let _serialised = super::test_globals::lock();
    assert_eq!(refresh_hz_from_modeinfo(&[]), None);
    assert_eq!(refresh_hz_from_modeinfo(&[0u8; 27]), None, "truncated blob");
    assert_eq!(refresh_hz_from_modeinfo(&modeinfo(0, 2080, 1121, 0)), None);
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(139_900, 0, 1121, 0)),
        None
    );
    assert_eq!(
        refresh_hz_from_modeinfo(&modeinfo(139_900, 2080, 0, 0)),
        None
    );
}

/// The vblank period the timer is armed from. It must never be 0 (a zero
/// period is an immediately-and-forever-due timer), and it must follow the
/// mode.
#[test]
fn the_vblank_period_tracks_the_mode_and_is_never_zero() {
    let _serialised = super::test_globals::lock();
    set_vblank_period_from_modeinfo(&modeinfo(139_900, 2080, 1121, 0));
    assert_eq!(vblank_period_ns(), 1_000_000_000 / 60);
    set_vblank_period_from_modeinfo(&modeinfo(0, 0, 0, 144));
    assert_eq!(vblank_period_ns(), 1_000_000_000 / 144);
    // No usable refresh at all falls back rather than producing 0.
    set_vblank_period_from_modeinfo(&[0u8; 68]);
    assert_eq!(vblank_period_ns(), 1_000_000_000 / FALLBACK_VBLANK_HZ);
    assert!(vblank_period_ns() > 0);
    reset_vblank_period();
    assert_eq!(vblank_period_ns(), 1_000_000_000 / FALLBACK_VBLANK_HZ);
}
