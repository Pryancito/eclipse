//! Soft ceilings for state that must not grow without bound across a long
//! desktop session (the KERNEL PAGE FAULT class at ~30–40 min was often a
//! filled EventBus / waker list; userspace must not amplify that with its own
//! unbounded Vec/HashMap growth).

/// Foreign-toplevel handles tracked for the taskbar. A healthy session has a
/// handful; if `Closed` is missed, this bound refuses further inserts so the
/// bar cannot grow without limit (and so we never `destroy()` a still-live
/// handle just to make room).
pub const MAX_TRACKED_TOPLEVELS: usize = 512;

/// Claimed `wl_output` globals we keep metadata for.
pub const MAX_OUTPUTS: usize = 32;

/// Bar surfaces (two per output: top + bottom).
pub const MAX_BARS: usize = MAX_OUTPUTS * 2;

/// Maximum edge of a bar/popup/tooltip buffer in pixels (after scale).
pub const MAX_BUFFER_DIM: u32 = 8192;

/// Peak pixels for one surface (logical or buffer). 64 Mpx covers 8K twice.
pub const MAX_BUFFER_PIXELS: usize = 64 << 20;

/// Which ceiling a buffer request went past.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TooBig {
    /// One edge is past [`MAX_BUFFER_DIM`].
    Dim,
    /// Both edges fit, but the area is past [`MAX_BUFFER_PIXELS`].
    Pixels,
}

/// Is a `w` x `h` buffer renderable? BOTH ceilings, in one function, because a
/// caller that checks only the edges lets a shape through whose area is far
/// past the limit: 8192x8192 is two edges at the cap and 67 Mpx, over it.
///
/// It lives here rather than in either binary so the live surface and the
/// offline `--dump` cannot end up disagreeing about what is renderable -- which
/// they did: `lunarrun --dump` clamped to 16384, twice this crate's own
/// [`MAX_BUFFER_DIM`], and never looked at the area at all, so a size the
/// compositor path declines with one line on stderr made the dump ask for a
/// gigabyte and abort (this crate is `panic = "abort"`).
///
/// **The area check cannot fire today** and is not dead weight: with
/// `MAX_BUFFER_DIM` at 8192 the largest shape the edge check admits is
/// 8192x8192, which is 67108864 pixels -- EXACTLY `MAX_BUFFER_PIXELS`. Raise
/// `MAX_BUFFER_DIM` by one and it starts biting, which is the whole point of
/// keeping it here instead of at the call sites. The test below pins that
/// relationship, so whoever raises the edge cap is told what just came alive.
pub fn check_buffer(w: usize, h: usize) -> Result<(), TooBig> {
    if w > MAX_BUFFER_DIM as usize || h > MAX_BUFFER_DIM as usize {
        return Err(TooBig::Dim);
    }
    check_buffer_area(w, h)
}

/// The area half of [`check_buffer`], on its own so it can be exercised for the
/// day the edge cap is raised past what the area cap allows. `saturating_mul`,
/// so a pair of enormous edges cannot wrap into a product that passes.
pub fn check_buffer_area(w: usize, h: usize) -> Result<(), TooBig> {
    if w.saturating_mul(h) > MAX_BUFFER_PIXELS {
        return Err(TooBig::Pixels);
    }
    Ok(())
}

/// Reject / drop when a list would grow past `max`. Returns false if the push
/// was refused (list unchanged). Prefer this over eviction that destroys a
/// still-live protocol object.
pub fn try_push_bounded<T>(list: &mut Vec<T>, item: T, max: usize) -> bool {
    if list.len() >= max {
        return false;
    }
    list.push(item);
    true
}

/// Truncate a protocol/UI string so a hostile/noisy compositor cannot grow
/// the heap without bound via titles / app_ids.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

pub const MAX_TITLE_CHARS: usize = 512;
pub const MAX_APP_ID_CHARS: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_push_refuses_past_cap() {
        let mut v = Vec::new();
        for i in 0..(MAX_TRACKED_TOPLEVELS + 50) {
            let ok = try_push_bounded(&mut v, i, MAX_TRACKED_TOPLEVELS);
            if i < MAX_TRACKED_TOPLEVELS {
                assert!(ok);
            } else {
                assert!(!ok);
            }
        }
        assert_eq!(v.len(), MAX_TRACKED_TOPLEVELS);
        assert_eq!(v[0], 0);
        assert_eq!(*v.last().unwrap(), MAX_TRACKED_TOPLEVELS - 1);
    }

    #[test]
    fn long_session_missed_closed_cannot_grow_past_cap() {
        let mut v = Vec::new();
        for i in 0..(40 * 60) {
            let _ = try_push_bounded(&mut v, i, MAX_TRACKED_TOPLEVELS);
        }
        assert!(v.len() <= MAX_TRACKED_TOPLEVELS);
    }

    #[test]
    fn check_buffer_admits_every_real_output_and_refuses_the_rest() {
        for (w, h) in [
            (320, 240),
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
            (5120, 2880),
            (7680, 4320),
        ] {
            assert_eq!(check_buffer(w, h), Ok(()), "{w}x{h} is a real monitor");
        }
        // Exactly at the edge cap is allowed; one past it is not, either way up.
        let d = MAX_BUFFER_DIM as usize;
        assert_eq!(check_buffer(d, 1), Ok(()));
        assert_eq!(check_buffer(1, d), Ok(()));
        assert_eq!(check_buffer(d + 1, 1), Err(TooBig::Dim));
        assert_eq!(check_buffer(1, d + 1), Err(TooBig::Dim));
        // What `lunarrun --dump` used to clamp to: twice the edge cap, refused.
        assert_eq!(check_buffer(16384, 16384), Err(TooBig::Dim));
        // A size past both ceilings must not overflow the multiply into a small
        // number that passes -- hence `saturating_mul`.
        assert_eq!(check_buffer(usize::MAX, usize::MAX), Err(TooBig::Dim));
        assert_eq!(check_buffer(0, 0), Ok(()));
    }

    #[test]
    fn the_edge_cap_currently_subsumes_the_area_cap() {
        // The largest shape the edge check admits is exactly the area cap, so
        // the area arm cannot be reached through `check_buffer` today. This is
        // the pin, not an accident: RAISE `MAX_BUFFER_DIM` AND THE AREA CHECK
        // COMES ALIVE -- 8193x8193 would be 16 KPx over the cap, and a caller
        // that only looked at the edges would wave a buffer past it.
        let d = MAX_BUFFER_DIM as usize;
        assert_eq!(d * d, MAX_BUFFER_PIXELS, "the two caps have drifted apart");
        assert_eq!(check_buffer(d, d), Ok(()));
        // And the arm itself works, which is what will matter then.
        assert!((d + 1) * (d + 1) > MAX_BUFFER_PIXELS);
        assert_eq!(
            check_buffer_area(d + 1, d + 1),
            Err(TooBig::Pixels),
            "the area arm must refuse what the edge cap would then allow"
        );
        assert_eq!(check_buffer_area(d, d), Ok(()));
        assert_eq!(check_buffer_area(MAX_BUFFER_PIXELS, 1), Ok(()));
        assert_eq!(
            check_buffer_area(MAX_BUFFER_PIXELS + 1, 1),
            Err(TooBig::Pixels)
        );
        assert_eq!(
            check_buffer_area(usize::MAX, usize::MAX),
            Err(TooBig::Pixels)
        );
    }

    #[test]
    fn truncate_chars_honours_unicode() {
        let s = "áéíóú".repeat(200);
        let t = truncate_chars(&s, 10);
        assert_eq!(t.chars().count(), 10);
    }
}
