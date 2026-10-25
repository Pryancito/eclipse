//! The PNG the build writes, decoded back.
//!
//! `encode_png` is a whole PNG writer with no dependencies: chunk framing
//! with its own CRC32, a zlib stream of stored deflate blocks, a filter byte
//! per scanline and an Adler-32 over the lot. What it produces is read by
//! labwc, by GTK and by every image viewer on the system, and there was no
//! test that read it. The two tests it had check `Crc32` and `adler32`
//! against published reference values -- the helpers, not the file -- and
//! `png_structure_is_valid` checks the signature, the four bytes at offset 12
//! and the four before the last four. A wrong chunk length, a `!len` that is
//! not the complement, `BFINAL` on the first block instead of the last, a
//! missing filter byte or an Adler-32 over the wrong bytes all pass those
//! three assertions and produce a file every decoder refuses.
//!
//! So this file decodes. The decoder below walks the chunks by their own
//! declared lengths, verifies each CRC, inflates the stored blocks, checks
//! the Adler-32 and un-filters the scanlines, and none of it reuses the
//! encoder's arithmetic.
//!
//! Two paths the existing tests cannot reach, because of the sizes they use:
//! `render_png(64, 36)` makes 6948 bytes of scanline data, so it is always
//! **one** stored block and the block loop never runs twice; and `adler32`
//! was only ever given nine bytes, so the 5552-byte folding never ran.
//! Both are reached here.

use super::*;

/// The eight bytes every PNG starts with.
const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// One chunk as it was found in the file.
struct Chunk {
    kind: [u8; 4],
    data: Vec<u8>,
}

/// Walk the chunks, checking each declared length and CRC against the bytes
/// that follow it. A length that does not match the file lands us in the
/// middle of a chunk and the next CRC fails, so this also catches a wrong
/// length without having to look for one.
fn chunks_of(png: &[u8]) -> Vec<Chunk> {
    assert_eq!(png[..8], SIG, "not a PNG");
    let mut out = Vec::new();
    let mut i = 8;
    while i < png.len() {
        assert!(
            i + 12 <= png.len(),
            "a chunk header runs off the end at {i}"
        );
        let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = png[i + 4..i + 8].try_into().unwrap();
        let end = i + 8 + len;
        assert!(
            end + 4 <= png.len(),
            "{:?} says {len} bytes and the file has {}",
            String::from_utf8_lossy(&kind),
            png.len() - i - 12
        );
        let data = png[i + 8..end].to_vec();
        let stated = u32::from_be_bytes(png[end..end + 4].try_into().unwrap());
        // The CRC is over the type and the data, and not over the length.
        let mut crc = Crc32::new();
        crc.update(&kind);
        crc.update(&data);
        assert_eq!(
            crc.finish(),
            stated,
            "{:?} does not match its own CRC",
            String::from_utf8_lossy(&kind)
        );
        out.push(Chunk { kind, data });
        i = end + 4;
    }
    out
}

/// Pull the stored deflate blocks out of a zlib stream, checking the framing
/// as it goes: the two header bytes, `BFINAL` set on the last block and only
/// the last, `BTYPE` stored, and `nlen` the one's complement of `len`.
fn inflate_stored(z: &[u8]) -> Vec<u8> {
    assert_eq!(z[0], 0x78, "the zlib CMF is not deflate with a 32K window");
    assert_eq!(
        (u16::from_be_bytes([z[0], z[1]])) % 31,
        0,
        "the zlib header check bits do not add up"
    );
    let mut out = Vec::new();
    let mut i = 2;
    let mut blocks = 0;
    loop {
        assert!(
            i + 5 <= z.len() - 4,
            "a block header runs into the checksum"
        );
        let header = z[i];
        assert_eq!(header & 0b110, 0, "block {blocks} is not a stored block");
        let last = header & 1 == 1;
        let len = u16::from_le_bytes([z[i + 1], z[i + 2]]);
        let nlen = u16::from_le_bytes([z[i + 3], z[i + 4]]);
        assert_eq!(nlen, !len, "block {blocks} has a bad length complement");
        let from = i + 5;
        let to = from + len as usize;
        assert!(to <= z.len() - 4, "block {blocks} runs past the checksum");
        out.extend_from_slice(&z[from..to]);
        blocks += 1;
        i = to;
        if last {
            break;
        }
    }
    assert_eq!(
        i,
        z.len() - 4,
        "the last block does not end where the checksum begins"
    );
    let stated = u32::from_be_bytes(z[z.len() - 4..].try_into().unwrap());
    assert_eq!(stated, adler32(&out), "the Adler-32 is not of these bytes");
    out
}

/// How many stored blocks a zlib stream was written in.
fn stored_block_count(z: &[u8]) -> usize {
    let mut i = 2;
    let mut n = 0;
    loop {
        let last = z[i] & 1 == 1;
        let len = u16::from_le_bytes([z[i + 1], z[i + 2]]) as usize;
        n += 1;
        i += 5 + len;
        if last {
            return n;
        }
    }
}

/// Everything a decoder gets out of one of these files.
struct Decoded {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    idat: Vec<u8>,
}

fn decode(png: &[u8]) -> Decoded {
    let chunks = chunks_of(png);
    let kinds: Vec<String> = chunks
        .iter()
        .map(|c| String::from_utf8_lossy(&c.kind).to_string())
        .collect();
    assert_eq!(
        kinds,
        ["IHDR", "IDAT", "IEND"],
        "the chunks are not the three this encoder writes"
    );

    let ihdr = &chunks[0].data;
    assert_eq!(ihdr.len(), 13, "IHDR is thirteen bytes");
    let width = u32::from_be_bytes(ihdr[0..4].try_into().unwrap());
    let height = u32::from_be_bytes(ihdr[4..8].try_into().unwrap());
    assert_eq!(ihdr[8], 8, "bit depth");
    assert_eq!(ihdr[9], 2, "colour type: truecolour RGB");
    assert_eq!(ihdr[10], 0, "compression: deflate");
    assert_eq!(ihdr[11], 0, "filter method: adaptive");
    assert_eq!(ihdr[12], 0, "interlace: none");
    assert!(chunks[2].data.is_empty(), "IEND carries no data");

    let raw = inflate_stored(&chunks[1].data);
    let stride = width as usize * 3;
    assert_eq!(
        raw.len(),
        (stride + 1) * height as usize,
        "the scanline data is not one filter byte plus one row per line"
    );
    let mut rgb = Vec::with_capacity(stride * height as usize);
    for (y, line) in raw.chunks_exact(stride + 1).enumerate() {
        assert_eq!(line[0], 0, "row {y} is filtered, and None was declared");
        rgb.extend_from_slice(&line[1..]);
    }
    Decoded {
        width,
        height,
        rgb,
        idat: chunks[1].data.clone(),
    }
}

/// A buffer whose every byte is a known function of its position, so that a
/// row or a channel arriving out of place shows up as a specific wrong byte
/// rather than as a difference somewhere.
fn ramp(w: usize, h: usize) -> Vec<u8> {
    (0..w * h * 3)
        .map(|i| {
            let px = i / 3;
            let (x, y, c) = (px % w, px / w, i % 3);
            (x.wrapping_mul(7) ^ y.wrapping_mul(31) ^ ((c + 1) * 97)) as u8
        })
        .collect()
}

/// The pixels handed in are the pixels that come back out, byte for byte, at
/// the size that was declared. This is the assertion the file had none of.
#[test]
fn the_pixels_handed_in_are_the_pixels_a_decoder_gets_back() {
    for (w, h) in [(1, 1), (1, 17), (17, 1), (8, 5), (64, 36)] {
        let rgb = ramp(w, h);
        let got = decode(&encode_png(w as u32, h as u32, &rgb));
        assert_eq!(got.width, w as u32, "{w}x{h}");
        assert_eq!(got.height, h as u32, "{w}x{h}");
        assert_eq!(got.rgb, rgb, "{w}x{h} came back with other pixels");
    }
}

/// A single pixel of a known colour, written out by hand. A round trip of a
/// buffer this file made could agree with itself about the channel order; one
/// pixel of three different values cannot.
#[test]
fn the_three_channels_come_back_in_the_order_they_went_in() {
    let got = decode(&encode_png(1, 1, &[0x12, 0x34, 0x56]));
    assert_eq!(got.rgb, [0x12, 0x34, 0x56]);
}

/// More scanline data than one stored block can hold is written as several,
/// and only the last one says it is the last. `BFINAL` on the first block
/// instead of the last stops every decoder after 64 KiB, which on the real
/// wallpaper would leave about a fortieth of the image and the rest blank.
#[test]
fn a_picture_too_big_for_one_block_is_written_in_several() {
    // 400x200: a stride of 1200, so 200 * 1201 = 240_200 bytes of scanline
    // data, which is four stored blocks.
    let (w, h) = (400usize, 200usize);
    let rgb = ramp(w, h);
    let png = encode_png(w as u32, h as u32, &rgb);
    let got = decode(&png);
    assert_eq!(
        stored_block_count(&got.idat),
        4,
        "240200 bytes is four blocks of at most 65535"
    );
    assert_eq!(got.rgb, rgb, "the blocks were not reassembled in order");
}

/// And the block boundary itself. 771 rows of 28 pixels is exactly 65535
/// bytes of scanline data: one block, full to the byte. One row more is two
/// blocks, the second of them 85 bytes long.
#[test]
fn the_block_boundary_is_at_sixty_five_thousand_five_hundred_and_thirty_five() {
    let w = 28usize;
    let stride_plus_filter = w * 3 + 1;
    assert_eq!(stride_plus_filter * 771, 65535, "the premise of this test");

    let exact = encode_png(w as u32, 771, &ramp(w, 771));
    let exact = decode(&exact);
    assert_eq!(stored_block_count(&exact.idat), 1, "65535 bytes fit in one");
    assert_eq!(exact.rgb, ramp(w, 771));

    let over = encode_png(w as u32, 772, &ramp(w, 772));
    let over = decode(&over);
    assert_eq!(
        stored_block_count(&over.idat),
        2,
        "65620 bytes do not fit in one"
    );
    assert_eq!(over.rgb, ramp(w, 772));
}

/// The real scene encodes to a file a decoder accepts, at the size asked
/// for. The scene itself is not asserted on -- it is a picture -- but it has
/// to be a picture that comes out of the encoder whole.
#[test]
fn the_scene_the_build_writes_decodes_at_the_size_it_was_asked_for() {
    let (w, h) = (240u32, 135u32);
    let got = decode(&render_png(w as usize, h as usize));
    assert_eq!((got.width, got.height), (w, h));
    assert_eq!(got.rgb.len(), (w * h * 3) as usize);
    assert_eq!(
        got.rgb,
        render(w as usize, h as usize),
        "the encoder and the renderer disagree about the pixels"
    );
    assert!(
        got.rgb.iter().any(|&b| b > 0x80),
        "the night scene came out with nothing bright in it"
    );
}

/// `adler32` folds its two sums every 5552 bytes, which is the largest run
/// that cannot overflow a `u32`. Nine bytes of "Wikipedia" never reached the
/// fold; this compares a long input against the sums kept in `u64`, where no
/// folding is needed at all, so the two agree only if the fold is in the
/// right place and over the right bytes.
#[test]
fn the_adler_sum_is_folded_in_the_right_place_on_a_long_input() {
    fn reference(data: &[u8]) -> u32 {
        let (mut a, mut b) = (1u64, 0u64);
        for &byte in data {
            a = (a + byte as u64) % 65521;
            b = (b + a) % 65521;
        }
        ((b << 16) | a) as u32
    }
    // Either side of one fold, either side of two, and the length the real
    // wallpaper actually hands it.
    for n in [0, 1, 5551, 5552, 5553, 11104, 11105, 240_200] {
        let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
        assert_eq!(adler32(&data), reference(&data), "{n} bytes");
    }

    // And the bytes the bound is derived from. 5552 is the longest run of
    // 0xff whose running sums still fit in a `u32`; middling bytes fold just
    // as well a good way past it, so a test that never sends 0xff cannot see
    // the bound move. These do: one byte more than 5552 of them overflows
    // `b`, which in a debug build is a panic rather than a wrong checksum.
    for n in [1, 5551, 5552] {
        let data = vec![0xffu8; n];
        assert_eq!(adler32(&data), reference(&data), "{n} bytes of 0xff");
    }
    let saturated = vec![0xffu8; 5552 * 4];
    assert_eq!(
        adler32(&saturated),
        reference(&saturated),
        "four folds of 0xff"
    );
}

/// The sky is a two-stop gradient and the seam between the stops is at
/// 0.55. A gradient with a step in it is the one artefact that reads as a
/// broken image rather than as a design, so the two halves have to meet.
#[test]
fn the_sky_gradient_has_no_step_where_its_two_stops_meet() {
    let (lr, lg, lb) = sky_color(0.55 - 1e-4);
    let (rr, rg, rb) = sky_color(0.55 + 1e-4);
    for (l, r) in [(lr, rr), (lg, rg), (lb, rb)] {
        assert!(
            (l - r).abs() < 1e-3,
            "the gradient steps at the seam: {l} {r}"
        );
    }
    // And it runs from the first stop to the last, both ends included.
    assert_eq!(sky_color(0.0), (0.051, 0.043, 0.118));
    assert_eq!(sky_color(1.0), (0.239, 0.165, 0.388));
    // Every channel only ever gets brighter going down the sky.
    let mut prev = sky_color(0.0);
    for i in 1..=100 {
        let next = sky_color(i as f32 / 100.0);
        for (p, n) in [(prev.0, next.0), (prev.1, next.1), (prev.2, next.2)] {
            assert!(n >= p - 1e-6, "the sky darkens at {i}: {p} then {n}");
        }
        prev = next;
    }
}

/// `lerp3` stays between its ends however far outside [0,1] it is asked. The
/// callers compute `t` by dividing by a span, so a `t` out of range is one
/// rounding away, and an unclamped lerp there writes a negative or
/// over-bright colour that wraps when it is cast to a byte.
#[test]
fn an_interpolation_outside_its_range_is_clamped_to_its_ends() {
    let a = (0.0, 0.25, 1.0);
    let b = (1.0, 0.75, 0.0);
    assert_eq!(lerp3(a, b, 0.0), a);
    assert_eq!(lerp3(a, b, 1.0), b);
    assert_eq!(lerp3(a, b, -5.0), a, "below the range");
    assert_eq!(lerp3(a, b, 5.0), b, "above the range");
    let mid = lerp3(a, b, 0.5);
    assert!((mid.0 - 0.5).abs() < 1e-6);
    assert!((mid.1 - 0.5).abs() < 1e-6);
    assert!((mid.2 - 0.5).abs() < 1e-6);
}

/// `span` is the row or column range a round thing is drawn over, and it is
/// the only thing keeping the three drawing passes inside the buffer: each one
/// iterates `lo..hi` and indexes the buffer with what comes out. So what has
/// to hold is that every index it yields is in bounds, and that a centre off
/// the canvas yields none.
///
/// Note what it does *not* promise: `lo` is clamped at the bottom and `hi` at
/// the top, so a centre past the limit gives `lo > hi` -- an empty range,
/// which is harmless only because every caller iterates it. A caller that
/// took `hi - lo` would underflow. Checked here so that stays deliberate.
#[test]
fn every_index_a_drawing_span_yields_is_inside_the_buffer() {
    for &(c, r, limit) in &[
        (10.0f32, 3.0f32, 64usize),
        (0.0, 5.0, 64),
        (63.0, 5.0, 64),
        (-20.0, 1.0, 64),
        (200.0, 1.0, 64),
        (5.0, 0.0, 64),
        (0.0, 0.0, 1),
        (0.5, 0.5, 1),
    ] {
        let (lo, hi) = span(c, r, limit);
        assert!(hi <= limit, "span({c}, {r}, {limit}) ends past the buffer");
        for i in lo..hi {
            assert!(i < limit, "span({c}, {r}, {limit}) yields {i}");
        }
    }
    // A disc entirely past the right edge yields nothing, rather than
    // wrapping onto the far side of the picture.
    let (lo, hi) = span(200.0, 1.0, 64);
    assert_eq!(
        (lo..hi).count(),
        0,
        "a disc off the right edge draws {lo}..{hi}"
    );

    // Off the LEFT edge it is not empty: a negative float cast to `usize`
    // saturates at zero, so `hi` comes back as 1 and column 0 is visited.
    // Nothing is drawn there anyway, because `coverage` of a distance of 200
    // with a radius of 1 is zero -- so the bound that matters is the one
    // above, and this is the arithmetic that makes it hold rather than a
    // second guarantee.
    let (lo, hi) = span(-200.0, 1.0, 64);
    assert_eq!((lo, hi), (0, 1));
    assert_eq!(coverage(1.0, 200.0), 0.0, "and nothing is drawn in it");

    // A disc on the canvas is covered whole: a pixel either side of the
    // radius, so the anti-aliased edge is inside the range.
    let (lo, hi) = span(10.0, 3.0, 64);
    assert!(
        lo <= 7,
        "the left edge of the disc is outside the span: {lo}"
    );
    assert!(
        hi > 13,
        "the right edge of the disc is outside the span: {hi}"
    );
}

/// `coverage` is the anti-aliasing: opaque well inside the disc, clear well
/// outside, and a one-pixel ramp across the edge. A coverage that never
/// reaches 1 makes everything translucent; one that never reaches 0 fills
/// the whole span.
#[test]
fn the_edge_of_a_disc_is_one_pixel_of_ramp_and_nothing_either_side() {
    assert_eq!(coverage(10.0, 0.0), 1.0, "the middle is opaque");
    assert_eq!(coverage(10.0, 9.0), 1.0, "well inside is opaque");
    assert_eq!(coverage(10.0, 11.0), 0.0, "well outside is clear");
    assert!(
        (coverage(10.0, 10.0) - 0.5).abs() < 1e-6,
        "half at the radius"
    );
    // Monotone across the edge, and never outside [0, 1].
    let mut prev = 1.0;
    for i in 0..=40 {
        let got = coverage(10.0, 9.0 + i as f32 / 20.0);
        assert!((0.0..=1.0).contains(&got), "coverage left [0,1]: {got}");
        assert!(got <= prev + 1e-6, "coverage rises with distance");
        prev = got;
    }
}

/// The distance to a segment is the distance to the nearest point ON it, not
/// to the infinite line it lies on: the stripes across the Eclipse disc are
/// capsules, and a point past one end has to measure to that end.
#[test]
fn the_distance_to_a_stripe_measures_to_the_segment_and_not_to_its_line() {
    // A horizontal segment from (10,10) to (20,10).
    let d = |px, py| capsule_dist(px, py, 10.0, 10.0, 20.0, 10.0);
    assert_eq!(d(15.0, 10.0), 0.0, "on the segment");
    assert!(
        (d(15.0, 13.0) - 3.0).abs() < 1e-5,
        "straight off the middle"
    );
    assert!((d(10.0, 10.0)).abs() < 1e-5, "the near end");
    assert!((d(20.0, 10.0)).abs() < 1e-5, "the far end");
    // Five past the end is five away, not zero: the line would say zero.
    assert!((d(25.0, 10.0) - 5.0).abs() < 1e-5, "past the far end");
    assert!((d(5.0, 10.0) - 5.0).abs() < 1e-5, "before the near end");
    // A segment of no length is a point, and measuring to it must not divide
    // by its own zero length.
    let p = capsule_dist(3.0, 4.0, 0.0, 0.0, 0.0, 0.0);
    assert!(
        (p - 5.0).abs() < 1e-5,
        "a zero-length segment is its endpoint"
    );
}

/// `add_px` is the only bounds check the star field has: the stars are placed
/// from a hash and drawn with a radius, so some of them fall off the edge.
/// Without the check the write wraps onto the opposite row, or panics.
#[test]
fn a_pixel_written_off_the_edge_is_dropped_and_not_wrapped() {
    let (w, h) = (4usize, 3usize);
    let mut buf = vec![0f32; w * h * 3];
    for (x, y) in [(-1, 0), (0, -1), (4, 0), (0, 3), (-7, -7), (99, 99)] {
        add_px(&mut buf, w, h, x, y, (1.0, 1.0, 1.0));
    }
    assert!(
        buf.iter().all(|&v| v == 0.0),
        "a pixel off the edge landed somewhere: {buf:?}"
    );
    // And one inside accumulates rather than replacing, which is what makes
    // overlapping stars brighter.
    add_px(&mut buf, w, h, 1, 2, (0.25, 0.5, 0.75));
    add_px(&mut buf, w, h, 1, 2, (0.25, 0.5, 0.75));
    let i = (2 * w + 1) * 3;
    assert_eq!(&buf[i..i + 3], &[0.5, 1.0, 1.5]);
    assert_eq!(buf.iter().filter(|&&v| v != 0.0).count(), 3);
}

/// `blend_px` is the other way pixels are written, and the one the
/// anti-aliasing uses: alpha 0 has to leave what was there and alpha 1 has to
/// replace it. The two being swapped draws the moon behind the sky.
#[test]
fn blending_with_no_alpha_keeps_what_was_there_and_full_alpha_replaces_it() {
    let w = 2usize;
    let mut buf = vec![0.5f32; w * 3];
    blend_px(&mut buf, w, 0, 0, (1.0, 0.0, 0.0), 0.0);
    assert_eq!(&buf[0..3], &[0.5, 0.5, 0.5], "alpha 0 changed the pixel");
    blend_px(&mut buf, w, 0, 0, (1.0, 0.0, 0.25), 1.0);
    assert_eq!(&buf[0..3], &[1.0, 0.0, 0.25], "alpha 1 did not replace it");
    blend_px(&mut buf, w, 1, 0, (1.0, 1.0, 1.0), 0.5);
    assert_eq!(
        &buf[3..6],
        &[0.75, 0.75, 0.75],
        "half alpha is the midpoint"
    );
    assert_eq!(
        &buf[0..3],
        &[1.0, 0.0, 0.25],
        "writing one pixel moved another"
    );
}

/// The star field is placed from `hash2`, so the wallpaper is the same file
/// on every build only if the hash is. Both arguments have to matter: a hash
/// that ignores one puts every star in a row.
#[test]
fn the_star_hash_is_the_same_every_time_and_both_inputs_change_it() {
    assert_eq!(hash2(0, 0), hash2(0, 0), "not deterministic");
    assert_eq!(hash2(7, 11), hash2(7, 11), "not deterministic");
    assert_ne!(
        hash2(1, 2),
        hash2(2, 1),
        "the arguments are interchangeable"
    );
    assert_ne!(hash2(0, 0), hash2(0, 1), "the second argument is ignored");
    assert_ne!(hash2(0, 0), hash2(1, 0), "the first argument is ignored");
    // The two the renderer actually asks for, over the whole star count, land
    // in enough different places to be a field rather than a line.
    let xs: std::collections::BTreeSet<u32> = (0..240).map(|i| hash2(i, 1) % 97).collect();
    let ys: std::collections::BTreeSet<u32> = (0..240).map(|i| hash2(i, 2) % 97).collect();
    assert!(xs.len() > 60, "the stars share columns: {} used", xs.len());
    assert!(ys.len() > 60, "the stars share rows: {} used", ys.len());
}

/// The wordmark is drawn from a 5x7 bitmap font. Each row is one byte with
/// five bits used, so a glyph row with a bit set above the fifth draws a
/// column that is not there, over whatever is beside it.
#[test]
fn every_glyph_row_is_five_bits_wide_and_an_unknown_character_is_blank() {
    let mut drawn = 0;
    for c in ' '..='~' {
        let rows = glyph5x7(c);
        for (y, row) in rows.iter().enumerate() {
            assert_eq!(
                row & !0b1_1111,
                0,
                "{c:?} row {y} sets a bit outside the five: {row:#09b}"
            );
        }
        if rows != [0; 7] {
            drawn += 1;
        }
    }
    assert!(drawn >= 8, "only {drawn} characters have a glyph");

    // Every letter the wordmark asks for has one. The string is written in
    // `draw_wordmark`; it is the product name rather than a number, so the
    // copy here is the thing being checked against, and a letter the font
    // does not have is drawn as a blank gap in the middle of the name.
    for c in "Eclipse OS".chars().filter(|c| *c != ' ') {
        assert_ne!(
            glyph5x7(c),
            [0; 7],
            "the wordmark asks for {c:?} and the font has no glyph for it"
        );
    }
    // A character the font does not have draws nothing rather than whatever
    // the last arm happened to be -- a space among them, which is how the
    // gap in the name is drawn.
    for c in ['\0', '\u{7f}', ' ', '@', '%', 'z', 'C', 'P'] {
        assert_eq!(glyph5x7(c), [0; 7], "{c:?} draws something");
    }
}
