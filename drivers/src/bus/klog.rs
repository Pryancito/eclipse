//! Kernel log (dmesg) helpers for drivers.
//!
//! These bypass the `log` crate level filter so vital events (link up/down, etc.)
//! are always visible via `dmesg` / `syslog(2)`.

use core::fmt::{self, Write};

extern "C" {
    fn drivers_klog_emit(priority: u8, msg: *const u8, len: usize);
}

/// Syslog priority (same as Linux `syslog.h`).
pub const LOG_ERR: u8 = 3;
pub const LOG_WARNING: u8 = 4;
pub const LOG_INFO: u8 = 6;

fn emit(priority: u8, msg: &str) {
    if msg.is_empty() {
        return;
    }
    unsafe { drivers_klog_emit(priority, msg.as_ptr(), msg.len()) };
}

/// Format and emit one line to the kernel log (dmesg).
pub fn klog_emit(priority: u8, args: fmt::Arguments<'_>) {
    let mut buf = [0u8; 256];
    emit(priority, format_line(&mut buf, args));
}

/// Format one line into `buf` and return it, ready for [`emit`].
///
/// The result is always valid UTF-8 and always ends in exactly one `\n`,
/// which is the whole job: 256 bytes is a real bound and these lines do
/// reach it.
fn format_line<'a>(buf: &'a mut [u8], args: fmt::Arguments<'_>) -> &'a str {
    let mut w = KlogBufWriter::new(buf);
    let _ = w.write_fmt(args);
    w.finish()
}

/// The largest `i <= at` at which `buf[..i]` does not end inside a UTF-8
/// character.
///
/// Everything written here came from a `&str`, so backing up over the
/// continuation bytes (`10xx_xxxx`) is enough, and a character is at most four
/// bytes long, so this looks at three of them at most.
fn floor_char_boundary(buf: &[u8], at: usize) -> usize {
    if at >= buf.len() {
        return buf.len();
    }
    let mut i = at;
    while i > 0 && buf[i] & 0b1100_0000 == 0b1000_0000 {
        i -= 1;
    }
    i
}

/// A fixed-size assembler for one dmesg line.
///
/// It used to cut at `min(len, free)` and append the terminator only
/// `if pos < buf.len()`, which got two things wrong. Both are about the 256th
/// byte, and nothing held that boundary:
///
/// * **A cut could land inside a character, and then the line VANISHED.** The
///   drivers do put multi-byte characters in these lines -- the em dash in the
///   e1000e's all-zero-MAC warning, in both of the ioapic's, in the AHCI reset
///   timeout. With the cut inside one, `from_utf8` failed, the `unwrap_or("")`
///   below made the message empty, and `emit` dropped it for being empty: no
///   truncated line in `dmesg`, **no line at all**. In a module whose whole
///   stated purpose is that vital events are always visible.
/// * **A line of exactly 256 bytes lost its terminator** and came out glued to
///   the next record, giving one `dmesg` line with two events in it.
///
/// The same pair, in the same shape, was in `zCore/src/logging.rs`.
struct KlogBufWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
    /// Set once something did not fit, so a truncated line is a prefix of the
    /// whole line rather than its beginning glued to its end.
    full: bool,
}

impl<'a> KlogBufWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            full: false,
        }
    }

    /// The finished line: valid UTF-8, terminated by exactly one `\n`, or
    /// empty when nothing was written (a blank `dmesg` line is not a line).
    fn finish(self) -> &'a str {
        let Self { buf, mut pos, .. } = self;
        if pos == 0 {
            return "";
        }
        if buf[pos - 1] != b'\n' {
            if pos == buf.len() {
                // Nothing fits: the line is truncated either way, so give up a
                // character of the message rather than the terminator that
                // separates it from the next record.
                pos = floor_char_boundary(buf, pos - 1);
            }
            buf[pos] = b'\n';
            pos += 1;
        }
        // `write_str` only ever stops on a character boundary, so this cannot
        // fail; the fallback stays because a log line is not worth a panic in
        // the middle of a driver.
        core::str::from_utf8(&buf[..pos]).unwrap_or("")
    }
}

impl Write for KlogBufWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.full {
            return Ok(());
        }
        let bytes = s.as_bytes();
        let free = self.buf.len().saturating_sub(self.pos);
        let n = if bytes.len() <= free {
            bytes.len()
        } else {
            self.full = true;
            floor_char_boundary(bytes, free)
        };
        self.buf[self.pos..self.pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n;
        Ok(())
    }
}

#[macro_export]
macro_rules! klog_info {
    ($($arg:tt)*) => {
        $crate::bus::klog::klog_emit(
            $crate::bus::klog::LOG_INFO,
            core::format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! klog_warn {
    ($($arg:tt)*) => {
        $crate::bus::klog::klog_emit(
            $crate::bus::klog::LOG_WARNING,
            core::format_args!($($arg)*),
        )
    };
}

#[macro_export]
macro_rules! klog_err {
    ($($arg:tt)*) => {
        $crate::bus::klog::klog_emit(
            $crate::bus::klog::LOG_ERR,
            core::format_args!($($arg)*),
        )
    };
}

/// The 256-byte bound on a dmesg line, which had no tests at all.
#[cfg(test)]
mod tests {
    use super::*;

    /// What `klog_emit` would hand to `emit`, at a size a test can reach.
    fn line<const N: usize>(args: fmt::Arguments<'_>) -> alloc::string::String {
        let mut buf = [0u8; N];
        alloc::string::String::from(format_line(&mut buf, args))
    }

    #[test]
    fn a_line_that_fits_gets_exactly_one_terminator() {
        assert_eq!(line::<32>(format_args!("[dev] up")), "[dev] up\n");
    }

    /// Several call sites write their own `\n`; it must not be doubled.
    #[test]
    fn a_line_that_already_ends_in_a_newline_is_left_alone() {
        assert_eq!(line::<32>(format_args!("[dev] up\n")), "[dev] up\n");
    }

    #[test]
    fn nothing_written_is_no_line_rather_than_a_blank_one() {
        assert_eq!(line::<32>(format_args!("")), "");
    }

    /// The bug that mattered: the cut landed inside a multi-byte character,
    /// `from_utf8` failed, `unwrap_or("")` emptied the message and `emit`
    /// dropped it for being empty. No truncated line in dmesg -- NO line.
    #[test]
    fn a_line_cut_inside_a_character_still_reaches_dmesg() {
        // "[e1000e] MAC all-zero" is 21 bytes; the em dash starts at 22 and
        // needs three, so a 24-byte buffer cuts inside it.
        let got = line::<24>(format_args!("[e1000e] MAC all-zero \u{2014} placeholder"));
        assert!(!got.is_empty(), "the line disappeared from dmesg");
        assert_eq!(got, "[e1000e] MAC all-zero \n");
    }

    /// Every multi-byte character the drivers actually log, at every cut that
    /// can land inside it.
    #[test]
    fn no_cut_anywhere_can_drop_a_line_or_break_its_utf8() {
        for ch in ['\u{2014}', '\u{2248}', '\u{2192}', '\u{00b5}'] {
            let msg = alloc::format!("[drv] {} {} tail", ch, ch);
            for n in 1..=msg.len() + 2 {
                let mut buf = alloc::vec![0u8; n];
                let got = format_line(&mut buf, format_args!("{}", msg));
                assert!(
                    !got.is_empty(),
                    "a {}-byte buffer dropped the line for {:?}",
                    n,
                    ch
                );
                assert!(
                    got.ends_with('\n'),
                    "a {}-byte buffer lost the terminator for {:?}: {:?}",
                    n,
                    ch,
                    got
                );
                assert!(
                    msg.starts_with(got.trim_end_matches('\n')),
                    "a {}-byte buffer invented content for {:?}: {:?}",
                    n,
                    ch,
                    got
                );
            }
        }
    }

    /// The other bug: a line of exactly the buffer's length used to come out
    /// with no terminator, glued to the next record -- one dmesg line carrying
    /// two events.
    #[test]
    fn a_line_that_fills_the_buffer_still_ends_where_it_says_it_does() {
        let got = line::<8>(format_args!("01234567"));
        assert_eq!(got, "0123456\n");
        let got = line::<8>(format_args!("0123456789abcdef"));
        assert_eq!(got, "0123456\n");
    }

    #[test]
    fn a_buffer_with_room_for_nothing_but_a_terminator_holds_one() {
        assert_eq!(line::<1>(format_args!("whatever")), "\n");
    }

    /// Truncation is sticky, so a cut line is a prefix of the whole line and
    /// never its head glued to its tail. Written as a `Display` that emits in
    /// several goes, because `format_args!` folds adjacent string literals into
    /// one piece and would make this a single write.
    #[test]
    fn a_cut_line_does_not_pick_up_what_comes_after_it() {
        struct InThreeGoes;
        impl fmt::Display for InThreeGoes {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a\u{2014}")?;
                f.write_str("b")?;
                f.write_str("XY")
            }
        }
        assert_eq!(line::<3>(format_args!("{InThreeGoes}")), "a\n");
    }

    #[test]
    fn the_syslog_priorities_are_the_ones_from_syslog_h() {
        assert_eq!((LOG_ERR, LOG_WARNING, LOG_INFO), (3, 4, 6));
    }
}
