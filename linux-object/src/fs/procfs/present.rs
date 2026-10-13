//! `/proc/present`: what the present path did with the frames it put on the
//! panel. Split out of `procfs.rs` for the size ratchet.

use alloc::format;
use alloc::string::String;

/// `/proc/present`: what the present path did with the frames it put on the
/// panel, over the whole boot.
///
/// This exists for a flicker nobody can reproduce on demand. Everything here is
/// already said in the klog, and every one of those lines is budgeted -- twelve
/// probe mismatches, eight fence timeouts, two fence outcomes and then one every
/// few hundred frames -- so by the time a user notices a window flickering the
/// lines that would explain it were written minutes ago and have stopped. These
/// are counters, so the answer is still here when they look.
///
/// Three questions, in the order worth asking:
///
/// 1. **Did anything wait for the GPU before the blit read the buffer?** The
///    `fence` line. A session whose presents are nearly all `no-fence` is one
///    where the implicit-sync wait finds nothing, and the blit copies a buffer
///    the GPU may still be writing.
/// 2. **Was the source moving under the copy?** The `probe` line: `moved` over
///    `reads` is the rate. The repair can only patch what it catches, so
///    `rounds` at twice `repaired` means every repair spent its whole budget
///    and the span never settled.
/// 3. **WHERE on screen?** The `bands` histogram, and nothing else here can
///    answer it. The klog's mismatch line names the presented window, which
///    for every `PAGE_FLIP` and `SETCRTC` is the whole screen, so it says
///    nothing about which client is at fault. The bands are 64-pixel column
///    ranges of that window: a contiguous run of hot bands with the rest cold
///    names the columns one window occupies, and bands hot all the way across
///    say the whole frame is handed over unfinished. Those are different faults
///    with different fixes, and today they produce the same log line.
pub(super) fn proc_present_content() -> String {
    use crate::fs::devfs::{drm, drm_scheme};
    let mut s = String::new();
    let c = drm::present_probe_counters();
    let (presents, no_fence, landed, timed_out, wait_us) = drm_scheme::present_fence_counters();
    s.push_str("[present] --- legacy present fence wait (SETCRTC / PAGE_FLIP) ---\n");
    s.push_str(&format!(
        "[present] fence: presents={} no-fence={} landed={} timed-out={} waited={}us \
         (avg {}us/landed) flip_fence={}\n",
        presents,
        no_fence,
        landed,
        timed_out,
        wait_us,
        wait_us.checked_div(landed).unwrap_or(0),
        if drm::flip_fence_enabled() {
            "on"
        } else {
            "off"
        },
    ));
    s.push_str("[present] --- source probe (the two reads bracketing the CPU blit) ---\n");
    s.push_str(&format!(
        "[present] probe: reads={} moved={} black-source={} black-grew={} \
         repaired={} rounds={} probe={} repair={}\n",
        c.reads,
        c.mismatches,
        c.black_source,
        c.black_grew,
        c.repaired,
        c.repair_rounds,
        if c.probe_on { "on" } else { "off" },
        if c.repair_on { "on" } else { "off" },
    ));
    match c.last {
        Some((fb, x, w, mask)) => s.push_str(&format!(
            "[present] last moved: fb={} window x={} w={} mask={:#010x}\n",
            fb, x, w, mask
        )),
        None => s.push_str("[present] last moved: (none this boot)\n"),
    }
    s.push_str(&format_band_hits(
        &drm::probe_band_hits(),
        drm::probe_band_px(),
    ));
    s
}

/// The band histogram, one line per band that ever moved.
///
/// Only the bands that moved, and the columns each one covers. Thirty-two rows
/// of zero would bury the two or three that matter, and a bare band number is
/// not something a reader can point at on their screen -- the column range is,
/// and it is what turns the histogram into "the window on the left" rather than
/// "bands 3 to 7".
///
/// The columns are counted from the PRESENTED WINDOW's left edge, not the
/// screen's, because that is what band 0 means. For a whole-frame present --
/// every `PAGE_FLIP` and `SETCRTC`, so very nearly all of them -- the window
/// IS the screen and the two are the same number; only a `DIRTYFB` damage box
/// makes them differ, and the `last moved` line above carries that box's `x` so
/// a reader can add it. Printing them as screen columns outright would be a
/// lie on exactly the frames where it matters.
fn format_band_hits(hits: &[u32], px: u32) -> String {
    let mut s = String::new();
    for (i, n) in hits.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let x0 = (i as u32).saturating_mul(px);
        s.push_str(&format!(
            "[present]  band {:>2} (window x {:>5}..{:>5}): {}\n",
            i,
            x0,
            x0.saturating_add(px).saturating_sub(1),
            n
        ));
    }
    if s.is_empty() {
        s.push_str("[present]  bands: none moved this boot\n");
    }
    s
}

#[cfg(test)]
mod present_band_report_tests {
    use super::*;
    use alloc::vec::Vec;

    /// A band is reported by the screen columns it covers, not by its number: a
    /// reader has to be able to point at the window the hot run belongs to.
    #[test]
    fn a_hot_band_names_its_column_range() {
        let mut hits = [0u32; 32];
        hits[3] = 41;
        let out = format_band_hits(&hits, 64);
        assert!(
            out.contains("band  3 (window x   192..  255): 41"),
            "band 3 of 64px must read as columns 192..255, got {:?}",
            out
        );
        // And only that band: thirty-two rows of zero would bury the one that
        // matters, which is the whole reason this report is filtered.
        assert_eq!(out.lines().count(), 1);
    }

    /// Several hot bands come out lowest first, so a contiguous run reads as a
    /// run of adjacent column ranges instead of a set a reader has to sort.
    #[test]
    fn hot_bands_come_out_lowest_first() {
        let mut hits = [0u32; 8];
        hits[5] = 2;
        hits[1] = 7;
        let out = format_band_hits(&hits, 64);
        let bands: Vec<&str> = out.lines().collect();
        assert_eq!(bands.len(), 2);
        assert!(bands[0].contains("band  1"), "{:?}", bands);
        assert!(bands[1].contains("band  5"), "{:?}", bands);
    }

    /// Nothing moved is an answer, and it has to be said. An empty section
    /// would read as "the probe is not running", which is a different fact with
    /// a different fix.
    #[test]
    fn no_hot_band_says_so_rather_than_printing_nothing() {
        let out = format_band_hits(&[0u32; 32], 64);
        assert_eq!(out, "[present]  bands: none moved this boot\n");
    }
}

#[cfg(test)]
mod present_report_tests {
    use super::*;

    /// The report answers its three questions in order and in full, with no
    /// section silently missing: a `cat` that is short one line reads as "this
    /// kernel does not track that", which is the wrong conclusion to hand
    /// someone chasing a flicker.
    #[test]
    fn the_report_carries_the_fence_the_probe_and_the_bands() {
        let out = proc_present_content();
        for needle in [
            "[present] fence: presents=",
            "flip_fence=",
            "[present] probe: reads=",
            "repair=",
            "[present] last moved:",
        ] {
            assert!(out.contains(needle), "missing {:?} in\n{}", needle, out);
        }
        // Either a band line or the explicit "none moved", never neither.
        assert!(
            out.contains("[present]  band ") || out.contains("bands: none moved"),
            "no band section in\n{}",
            out
        );
    }
}
