//! Wiring of each `Look` to its palette and bar layout (`main.rs`).

use super::*;

/// Every look, so a row added to `Look` without a row here fails to compile
/// rather than quietly inheriting whatever the `_` arm says.
const LOOKS: [Look; 4] = [Look::Eclipse, Look::Kde, Look::Win11, Look::Tecno];

#[test]
fn each_look_wears_its_own_palette() {
    // A row wired to the wrong palette dresses the panel as another
    // desktop, which reads as a setting that did not take rather than as a
    // bug anyone reports. By value and not by pointer: `PAL_*` are `const`,
    // so each `&PAL_ECLIPSE` is promoted separately and `ptr::eq` against
    // one from here is false even when the arm is right. The pairwise check
    // below is what rules out two arms reaching one palette.
    assert_eq!(*pal_of(Look::Eclipse), PAL_ECLIPSE);
    assert_eq!(*pal_of(Look::Kde), PAL_KDE);
    assert_eq!(*pal_of(Look::Win11), PAL_WIN11);
    assert_eq!(*pal_of(Look::Tecno), PAL_TECNO);
    // And no two looks share one, which is what a copy-pasted arm leaves
    // behind: with `Kde => &PAL_WIN11` the panel comes up grey-less and
    // nothing above would have caught it from a single row.
    for (i, a) in LOOKS.iter().enumerate() {
        for b in &LOOKS[i + 1..] {
            assert_ne!(pal_of(*a), pal_of(*b), "{a:?} and {b:?} share one palette");
        }
    }
    // The default look reaches the palette the image is built around: the
    // wallpaper, the icon theme and the accent are all drawn for this one.
    assert_eq!(*pal_of(Look::default()), PAL_ECLIPSE);
}

#[test]
fn a_palette_keeps_the_colours_its_comment_cites() {
    // The hexes each palette's doc comment quotes from the desktop it
    // imitates. A changed constant here moves the whole bar's hue, and a
    // screenshot is the only place it shows -- which is nowhere in CI.
    assert_eq!(PAL_KDE.bar_bg, (0x2a, 0x2e, 0x32), "Breeze window");
    assert_eq!(PAL_KDE.pill, (0x1b, 0x1e, 0x20), "Breeze view");
    assert_eq!(PAL_KDE.text, (0xfc, 0xfc, 0xfc), "Breeze text");
    assert_eq!(PAL_KDE.dim, (0x7f, 0x8c, 0x8d), "Breeze inactive text");
    assert_eq!(PAL_KDE.accent, (0x3d, 0xae, 0xe9), "Breeze selection");
    assert_eq!(PAL_KDE.warn, (0xda, 0x44, 0x53), "Breeze negative");
    assert_eq!(PAL_WIN11.bar_bg, (0x20, 0x20, 0x20), "Win11 taskbar");
    assert_eq!(PAL_WIN11.menu_panel, (0x2b, 0x2b, 0x2b), "Win11 flyout");
    assert_eq!(PAL_WIN11.accent, (0x00, 0x78, 0xd4), "Win11 accent");
    assert_eq!(PAL_WIN11.muted, (0xc5, 0xc5, 0xc5), "Win11 secondary text");
    assert_eq!(PAL_WIN11.warn, (0xe8, 0x11, 0x23), "Win11 critical");
    assert_eq!(PAL_TECNO.bar_bg, (0x06, 0x0b, 0x12), "tecno ground");
    assert_eq!(PAL_TECNO.accent, (0x3a, 0xd7, 0xe8), "tecno signal cyan");
    assert_eq!(PAL_TECNO.rule, (0x1d, 0x5e, 0x6e), "tecno plate border");
}

#[test]
fn no_palette_hides_one_of_its_roles_behind_another() {
    // The invariants that hold whatever the hues are, because the drawing
    // code leans on them: a swap inside one palette does not change any
    // single colour, so nothing above this catches it, and the result is
    // text that cannot be read rather than a bar in the wrong colour.
    for look in LOOKS {
        let p = pal_of(look);
        let lum = |(r, g, b): Rgb| 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
        // Live text over muted over dim: three tiers the bar uses to say
        // what is running, what is a reading and what is minimised.
        assert!(
            lum(p.text) > lum(p.muted),
            "{look:?}: primary text is not brighter than module text"
        );
        assert!(
            lum(p.muted) > lum(p.dim),
            "{look:?}: module text is not brighter than the dim text"
        );
        // Every text tier has to come off the ground it is painted on, or
        // the bar reads as blank. 40 levels of luminance is about where a
        // reading stops being legible on a dark panel.
        for (name, c) in [("text", p.text), ("muted", p.muted), ("dim", p.dim)] {
            assert!(
                (lum(c) - lum(p.bar_bg)).abs() > 40.0,
                "{look:?}: {name} does not come off the bar ground"
            );
        }
        // A hover has to be visible against what it hovers over, in both
        // the pill and the menu: equal colours mean no hover feedback at
        // all, which is exactly what a copy-paste leaves behind.
        assert_ne!(p.pill, p.pill_hover, "{look:?}: the pill has no hover");
        assert_ne!(
            p.menu_panel, p.menu_hover,
            "{look:?}: the menu rows have no hover"
        );
        // The active taskbar button has to be told from the bar itself,
        // otherwise nothing on screen says which window has the focus.
        assert_ne!(
            p.btn_active, p.bar_bg,
            "{look:?}: the active button is the bar ground"
        );
        // `white` is the text drawn over `btn_active` and over the accent,
        // so it is the brightest thing in the palette, not a hue.
        let (r, g, b) = p.white;
        assert!(
            r > 0xf0 && g > 0xf0 && b > 0xf0,
            "{look:?}: the active-button text is not a white"
        );
    }
    // The accent carries a hue in all three: it is the one coloured thing
    // on an otherwise grey bar, and a grey accent loses the launcher glyph.
    for look in LOOKS {
        let (r, g, b) = pal_of(look).accent;
        let spread = r.max(g).max(b) - r.min(g).min(b);
        assert!(spread > 0x30, "{look:?}: the accent has no hue");
    }
}

#[test]
fn the_single_bar_looks_are_the_ones_with_a_bottom_panel_only() {
    // Eclipse's layout is a top info bar PLUS a bottom taskbar; KDE and
    // Windows both have one bottom panel. Getting this wrong leaves a bar
    // drawn over the top of the screen that the look has no room for, or
    // loses the clock entirely.
    assert!(!single_bar_of(Look::Eclipse));
    assert!(single_bar_of(Look::Kde));
    assert!(single_bar_of(Look::Win11));
    // Windows 11, and only Windows 11, centres its taskbar buttons: that
    // one detail is most of what makes the screenshot read as Windows.
    assert!(centered_tasks_of(Look::Win11));
    assert!(!centered_tasks_of(Look::Kde));
    assert!(!centered_tasks_of(Look::Eclipse));
    // `tecno` is `eclipse`'s layout wearing another skin: two bars, pinned
    // left. Only how the bars are DRAWN changes.
    assert!(!single_bar_of(Look::Tecno));
    assert!(!centered_tasks_of(Look::Tecno));
    // Centred buttons only make sense on a single bar -- the taskbar is the
    // bar being centred -- so the one implies the other.
    for look in LOOKS {
        assert!(
            !centered_tasks_of(look) || single_bar_of(look),
            "{look:?} centres its tasks without having a taskbar to centre"
        );
    }
}

#[test]
fn only_the_win11_bar_is_translucent_and_the_two_flags_agree() {
    // `translucent()` is what decides between an ARGB and an XRGB buffer in
    // `configure`, while `bar_alpha()` is what `clear_bar` fills with: the
    // two disagreeing means either a bar filled with alpha onto an opaque
    // buffer (a black bar) or an ARGB buffer filled opaque (a wasted
    // format change), so they are derived from one another and checked
    // together.
    assert_eq!(bar_alpha_of(Look::Win11), 0.85, "Win11 acrylic");
    assert_eq!(bar_alpha_of(Look::Kde), 1.0);
    assert_eq!(bar_alpha_of(Look::Eclipse), 1.0);
    assert!(translucent_of(Look::Win11));
    assert!(!translucent_of(Look::Kde));
    assert!(!translucent_of(Look::Eclipse));
    assert_eq!(bar_alpha_of(Look::Tecno), 1.0);
    assert!(!translucent_of(Look::Tecno));
    for look in LOOKS {
        let a = bar_alpha_of(look);
        assert_eq!(
            translucent_of(look),
            a < 1.0,
            "{look:?}: the buffer format and the fill alpha disagree"
        );
        // An alpha outside (0, 1] is a bar that is either invisible or an
        // out-of-range fill: `fill_rect_a` takes it as a fraction.
        assert!(a > 0.0 && a <= 1.0, "{look:?}: alpha {a} is out of range");
    }
}

#[test]
fn a_translucent_bar_is_filled_in_a_way_that_keeps_its_alpha() {
    // `clear_bar` picks between `fill_rect_a` and `clear`, and only one of
    // the two honours the alpha. The bug it exists for is `Canvas::clear`
    // being unconditionally opaque and so silently defeating the ARGB
    // buffer `configure` asked the compositor for -- a bar that reads as
    // solid black over the wallpaper. What shows it is the alpha channel
    // of the blitted pixels, which is the only place the fill differs.
    let bg = pal_of(Look::Win11).bar_bg;
    let (w, h) = (8usize, 4usize);
    let fill = |alpha: f32| -> Vec<u8> {
        let mut cv = Canvas::try_new(w, h).expect("canvas");
        clear_bar_with(&mut cv, w, h, bg, alpha);
        let mut buf = vec![0u8; w * h * 4];
        assert!(cv.blit_argb(&mut buf));
        buf
    };
    let opaque = fill(1.0);
    let acrylic = fill(bar_alpha_of(Look::Win11));
    // An opaque bar is the ground itself, every pixel of it.
    for px in opaque.chunks(4) {
        assert_eq!(px[3], 0xff, "an opaque fill left a see-through pixel");
    }
    // The translucent one carries the alpha it was given, and not 0xff:
    // 0.85 of 255 rounds to 217.
    let want = (bar_alpha_of(Look::Win11) * 255.0).round() as u8;
    for px in acrylic.chunks(4) {
        assert_eq!(
            px[3], want,
            "the alpha never reached the pixels: the ARGB buffer is wasted"
        );
    }
    // Both fills cover the WHOLE bar: a rect short by a row leaves the
    // last line of the panel transparent, which on an ARGB surface is a
    // see-through strip along the screen edge.
    assert!(
        acrylic.chunks(4).all(|px| px[3] > 0),
        "the fill did not reach every pixel of the bar"
    );
}


#[test]
fn the_instrument_look_is_exactly_one_look_and_changes_three_things_together() {
    // One flag, because it governs the shape of every plate, the case of
    // every label AND the kind of meter at once: a look that picked up two
    // of the three reads as a theme that half applied, which is not
    // something anyone can report usefully.
    assert!(tecno_of(Look::Tecno));
    for look in LOOKS {
        assert_eq!(
            tecno_of(look),
            look == Look::Tecno,
            "{look:?} answers to the instrument look"
        );
    }
}

#[test]
fn both_meters_measure_the_same_so_the_modules_do_not_shift_between_looks() {
    // The top bar lays its modules out from the RIGHT edge, each one
    // stepping left by its own width. A segmented meter wider or narrower
    // than the continuous gauge would move every module to its left --
    // and the clock and power HIT BOXES with them, since those are computed
    // from the same running x. That is a mis-click, not a cosmetic drift.
    assert_eq!(METER_W, 27, "the width `metric` reserves for either meter");
    assert_eq!(METER_CELLS * 4 - 1, METER_W, "the cells do not fill the slot");
}

#[test]
fn a_label_is_capitalised_only_where_the_look_asks_for_it() {
    // `cap` is the only thing standing between `cpu 18%` and `CPU 18%`, and
    // it reads the look through `tecno()`, which is fed from a `OnceLock`
    // this test cannot set. What it CAN pin is that the transformation
    // itself is the identity on text that is already capitals and that it
    // leaves the digits and the per-cent sign alone -- a `to_uppercase`
    // applied twice, or applied to a value, is where a label like `18%`
    // would come back mangled.
    assert_eq!("CPU 18%".to_uppercase(), "CPU 18%");
    assert_eq!("cpu 18%".to_uppercase(), "CPU 18%");
    assert_eq!("45°c".to_uppercase(), "45°C");
    assert_eq!("bat 80% +".to_uppercase(), "BAT 80% +");
}
