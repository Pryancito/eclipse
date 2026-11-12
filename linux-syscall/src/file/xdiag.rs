//! What a graphics program says when it cannot start, copied into the kernel
//! log.
//!
//! Xorg, the dynamic linker and an X client each print the one line that names
//! their failure to a stderr that is often nobody's: a server started by a
//! service, a client launched from a menu or from a terminal that has since
//! been closed. `write` and `writev` on descriptors 0 to 2 pass the start of
//! what is written through [`tee`], which copies the lines to the log
//! (prefixed `XLOG:`) when they carry one of the markers below.
//!
//! That is every write to a standard descriptor, of anything: a `dd` whose
//! output is its stdout, each stage of a shell pipeline, a `cat` of a binary.
//! So the search is one pass over the bytes. It used to be one pass per
//! marker, each of them comparing the marker at every offset, which for the
//! 512 bytes `write` hands over was some 12 000 comparisons a call: a third
//! of the time of a 4 KiB `dd` write to a disk (QEMU, one CPU), spent looking
//! for `(EE)` in zeroes.

/// How much of a write is looked at.
const SCAN: usize = 1024;

/// Xorg's own log markers.
const SERVER: u8 = 1;
/// What the dynamic linker prints when a program dies before `main`. Xorg
/// pulls in far more shared libraries than a typical program, so one missing
/// `.so` or unresolved symbol makes musl's `ld` abort with one of these, and
/// that path produces no Xorg log at all.
const LINKER: u8 = 2;
/// What an X *client* prints: Xlib's wording for a server it cannot reach or
/// that went away, the refusals of a visual, and the GL loader's failures.
/// None of the server's markers appear for these.
const CLIENT: u8 = 4;
const EVERY_GROUP: u8 = SERVER | LINKER | CLIENT;

/// The markers, each with the group it belongs to.
///
/// `open display` alone covers every phrasing in the wild ("couldn't open
/// display", "cannot open display", "Can't open display", "unable to open
/// display"); `XIO:` and `X connection to` catch a connection that dies in
/// mid-session; the RGB and GLX visual refusals are what mesa-demos print
/// when the server answers but offers no usable visual; and the last five are
/// where a client that reached the server and then could not get a usable
/// context reports itself ("MESA: error: ZINK: vkQueueSubmit failed
/// (VK_ERROR_DEVICE_LOST)", the libGL and libEGL loader errors, a DRI3
/// handshake that fell back or failed).
const MARKERS: &[(&[u8], u8)] = &[
    (b"(EE)", SERVER),
    (b"(WW)", SERVER),
    (b"Fatal", SERVER),
    (b"no screens", SERVER),
    (b"(II) ", SERVER),
    (b"Error loading shared library", LINKER),
    (b"Error relocating", LINKER),
    (b"symbol not found", LINKER),
    (b"No such file", LINKER),
    (b"cannot open shared object", LINKER),
    (b"version `", LINKER),
    (b"undefined symbol", LINKER),
    (b"open display", CLIENT),
    (b"XIO:", CLIENT),
    (b"X connection to", CLIENT),
    (b"Xlib:", CLIENT),
    (b"xcb_connection", CLIENT),
    (b"couldn't get an RGB", CLIENT),
    (b"RGB GLX visual", CLIENT),
    (b"MESA: error", CLIENT),
    (b"libGL error", CLIENT),
    (b"libEGL warning", CLIENT),
    (b"failed to load driver", CLIENT),
    (b"DRI3", CLIENT),
];

/// The bytes a marker can start with. A byte that is not one of them is
/// passed over with one lookup, which is all a write of zeroes or of
/// compressed data ever costs.
const STARTS: [bool; 256] = {
    let mut starts = [false; 256];
    let mut i = 0;
    while i < MARKERS.len() {
        starts[MARKERS[i].0[0] as usize] = true;
        i += 1;
    }
    starts
};

/// Which groups have a marker somewhere in `scan`.
fn groups_in(scan: &[u8]) -> u8 {
    let mut found = 0;
    for (at, &byte) in scan.iter().enumerate() {
        if !STARTS[byte as usize] {
            continue;
        }
        let rest = &scan[at..];
        for &(marker, group) in MARKERS {
            if found & group == 0 && rest.starts_with(marker) {
                found |= group;
            }
        }
        if found == EVERY_GROUP {
            break;
        }
    }
    found
}

/// Copy what `buf` says into the kernel log if it reads like a graphics
/// program failing to start. See the module.
pub(super) fn tee(buf: &[u8]) {
    let scan = &buf[..buf.len().min(SCAN)];
    let found = groups_in(scan);
    if found == 0 {
        return;
    }
    // Budget the client group only: a program looping on the same refusal
    // (a respawning service, a shell loop) must not flood the console. The
    // server and linker groups are unbudgeted: they fire once per start
    // attempt.
    if found == CLIENT {
        use core::sync::atomic::{AtomicU32, Ordering};
        static CLIENT_BUDGET: AtomicU32 = AtomicU32::new(0);
        if CLIENT_BUDGET.fetch_add(1, Ordering::Relaxed) >= 24 {
            return;
        }
    }
    if let Ok(s) = core::str::from_utf8(scan) {
        for line in s.split('\n').filter(|l| !l.is_empty()).take(6) {
            kernel_hal::klog_info!("XLOG: {}", line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    /// The search this replaced, kept as the definition of the right answer:
    /// one pass per marker, the marker compared at every offset.
    fn groups_the_slow_way(scan: &[u8]) -> u8 {
        let has = |needle: &[u8]| scan.windows(needle.len()).any(|w| w == needle);
        MARKERS
            .iter()
            .filter(|(marker, _)| has(marker))
            .fold(0, |found, &(_, group)| found | group)
    }

    #[test]
    fn every_marker_is_found_wherever_it_sits() {
        for &(marker, group) in MARKERS {
            for padding in [0usize, 1, 7, 300] {
                let mut text = vec![b'.'; padding];
                text.extend_from_slice(marker);
                assert_eq!(
                    groups_in(&text) & group,
                    group,
                    "at the end, after {padding}"
                );
                text.extend_from_slice(b" and then some\n");
                assert_eq!(
                    groups_in(&text) & group,
                    group,
                    "in the middle, after {padding}"
                );
                assert_eq!(groups_in(&text), groups_the_slow_way(&text));
            }
        }
    }

    #[test]
    fn a_marker_cut_short_by_the_end_is_not_one() {
        for &(marker, _) in MARKERS {
            let mut text = b"line one\n".to_vec();
            text.extend_from_slice(&marker[..marker.len() - 1]);
            assert_eq!(
                groups_in(&text),
                groups_the_slow_way(&text),
                "{:?}",
                core::str::from_utf8(marker)
            );
        }
    }

    #[test]
    fn data_that_says_nothing_finds_nothing() {
        assert_eq!(groups_in(&[]), 0);
        assert_eq!(groups_in(&[0u8; 1024]), 0);
        assert_eq!(groups_in(b"total 12\ndrwxr-xr-x 2 root root 4096 bin\n"), 0);
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        assert_eq!(groups_in(&noise), groups_the_slow_way(&noise));
    }

    #[test]
    fn each_kind_of_failure_is_told_apart() {
        assert_eq!(groups_in(b"(EE) no screens found(EE)\n"), SERVER);
        assert_eq!(
            groups_in(b"Error loading shared library libGLX.so.0: No such file or directory"),
            LINKER
        );
        assert_eq!(
            groups_in(b"xterm: Xt error: Can't open display: :0\n"),
            CLIENT
        );
        assert_eq!(
            groups_in(b"(EE) Error relocating /usr/lib/xorg/modules: symbol not found\nDRI3\n"),
            EVERY_GROUP
        );
    }

    #[test]
    fn the_same_answer_as_the_slow_search_on_text_full_of_near_misses() {
        // Every marker's first bytes, over and over, then the real thing.
        let mut text = Vec::new();
        for &(marker, _) in MARKERS {
            for cut in 1..marker.len() {
                text.extend_from_slice(&marker[..cut]);
                text.push(b' ');
            }
        }
        assert_eq!(groups_in(&text), groups_the_slow_way(&text));
        for &(marker, _) in MARKERS {
            let mut with = text.clone();
            with.extend_from_slice(marker);
            assert_eq!(groups_in(&with), groups_the_slow_way(&with));
        }
    }

    /// The point of the single pass: bytes no marker starts with cost a
    /// lookup each and nothing else, and those are most bytes of most writes.
    #[test]
    fn most_bytes_cannot_start_a_marker() {
        let starts = STARTS.iter().filter(|&&s| s).count();
        assert!(starts <= 20, "{} of 256 bytes start a marker", starts);
        assert!(!STARTS[0] && !STARTS[b'\n' as usize] && !STARTS[b' ' as usize]);
    }
}
