//! What the chrome and the software present path cost on their own.
//!
//! Servo, Mesa and the Wayland round trip are not in these numbers. A frame
//! the compositor can see is this work plus those, so a row here is the floor:
//! if painting the toolbar is tens of microseconds, a slow window is the page,
//! not the strip above it.
//!
//! The harness is the native `#[bench]` one (`test::Bencher`). This tree pins
//! a nightly toolchain, and `zircon-object/benches/vm.rs` is the precedent, so
//! no benchmarking crate enters the lockfile.
//!
//! Inputs and results both go through `test::black_box`. These are tight loops
//! over data the compiler can see; black-boxing only the result lets it delete
//! the work and leaves the row reporting the loop overhead.
//!
//! Run (the engine feature pulls Servo in, and these rows do not need it):
//!
//! ```sh
//! cargo bench --manifest-path tools/SilverSurfer/Cargo.toml \
//!     --no-default-features --bench chrome
//! ```

#![feature(test)]

extern crate test;

use silversurfer::chrome::{self, Hit, View};
use silversurfer::files::{arguments, PickRequest};
use silversurfer::location::{parse_location, resolve_omnibox, SchemeHint, DEFAULT_SEARCH_TEMPLATE};
use silversurfer::look::Look;
use std::path::Path;
use test::Bencher;

fn titles(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("pestaña {i} — example.com")).collect()
}

fn paint(width: i32, scale: i32, titles: &[String]) -> chrome::Pixels {
    let loading = vec![false; titles.len().max(1)];
    chrome::render(&View {
        width,
        look: Look::Eclipse,
        titles,
        loading: &loading,
        active: 0,
        location: "https://example.com/documentacion",
        location_focused: false,
        location_cursor: 0,
        scheme: SchemeHint::Https,
        gpu: true,
        private: false,
        can_back: true,
        can_forward: false,
        scale,
    })
}

#[bench]
fn chrome_1280_scale_1(b: &mut Bencher) {
    let titles = titles(4);
    b.iter(|| {
        let pixels = paint(
            test::black_box(1280),
            test::black_box(1),
            test::black_box(&titles),
        );
        test::black_box(pixels.argb.len())
    });
}

#[bench]
fn chrome_1920_scale_2(b: &mut Bencher) {
    let titles = titles(8);
    b.iter(|| {
        let pixels = paint(
            test::black_box(1920),
            test::black_box(2),
            test::black_box(&titles),
        );
        test::black_box(pixels.argb.len())
    });
}

#[bench]
fn hit_across_a_full_strip(b: &mut Bencher) {
    let width = 1280;
    let tabs = 12;
    b.iter(|| {
        let mut found = 0;
        for x in (0..width).step_by(7) {
            for y in [4, 14, 40, 70] {
                if chrome::hit(test::black_box(width), test::black_box(tabs), x, y).is_some() {
                    found += 1;
                }
            }
        }
        test::black_box(found)
    });
}

#[bench]
fn hit_close_box(b: &mut Bencher) {
    let lay = chrome::layout(1280, 6);
    let close = lay.closes[3];
    b.iter(|| {
        let hit = chrome::hit(
            test::black_box(1280),
            test::black_box(6),
            test::black_box(close.x + 1),
            test::black_box(close.y + 1),
        );
        assert_eq!(hit, Some(Hit::Close(3)));
        test::black_box(hit)
    });
}

#[bench]
fn blit_1080p(b: &mut Bencher) {
    let w = 1920u32;
    let h = 1080u32;
    let src = vec![0x11u8; (w * h * 4) as usize];
    let mut dst = vec![0u8; (w * (h + chrome::HEIGHT as u32) * 4) as usize];
    let stride = (w as usize) * 4;
    b.iter(|| {
        chrome::blit_rgba(
            test::black_box(&src),
            test::black_box(w),
            test::black_box(h),
            test::black_box(&mut dst),
            w,
            stride,
            chrome::HEIGHT as u32,
        );
        test::black_box(dst[stride * 80])
    });
}

#[bench]
fn swizzle_toolbar_buffer(b: &mut Bencher) {
    let src = vec![10u8; 1280 * chrome::HEIGHT as usize * 4];
    let mut dst = Vec::new();
    b.iter(|| {
        chrome::rgba_premul_to_argb(test::black_box(&src), test::black_box(&mut dst));
        test::black_box(dst.len())
    });
}

#[bench]
fn parse_typed_locations(b: &mut Bencher) {
    let samples = [
        "  servo.org/demo ",
        "https://example.com/a",
        "//cdn.example/video.m3u8",
        "/etc/eclipse/look",
        "about:blank",
    ];
    b.iter(|| {
        let mut n = 0;
        for sample in samples {
            n += parse_location(test::black_box(sample)).unwrap().as_str().len();
        }
        test::black_box(n)
    });
}

#[bench]
fn resolve_omnibox_mix(b: &mut Bencher) {
    let samples = [
        "servo.org",
        "hola mundo",
        "firefox",
        "https://example.com",
        "/tmp/x.html",
    ];
    let tpl = DEFAULT_SEARCH_TEMPLATE;
    b.iter(|| {
        let mut n = 0;
        for sample in samples {
            n += resolve_omnibox(test::black_box(sample), test::black_box(tpl))
                .unwrap()
                .as_str()
                .len();
        }
        test::black_box(n)
    });
}

#[bench]
fn resolve_look(b: &mut Bencher) {
    let file = "look=kde\n";
    b.iter(|| {
        let look = Look::resolve(test::black_box(Some("nope")), test::black_box(Some(file)));
        test::black_box(look)
    });
}

#[bench]
fn lunarfile_argv(b: &mut Bencher) {
    let request = PickRequest {
        multiple: true,
        save_name: Some("informe.html".into()),
    };
    let out = Path::new("/tmp/silversurfer-pick");
    b.iter(|| {
        let args = arguments(test::black_box(&request), test::black_box(out));
        test::black_box(args.len())
    });
}
