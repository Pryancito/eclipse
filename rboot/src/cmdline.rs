//! Kernel command-line parsing, shared by the bootloader and its tests.
//!
//! The cmdline rboot reads out of `rboot.conf` is a colon-separated list of
//! `KEY=value` pairs, e.g.
//!
//! ```text
//! LOG=debug:ROOTPROC=/bin/busybox?sh:TERM=xterm-256color
//! ```
//!
//! The parser is deliberately tiny (no alloc) and tolerant: nothing here may
//! panic or reject, because a bootloader that refuses its own config does not
//! boot.

use log::LevelFilter;

/// Iterate the `KEY=value` pairs of a cmdline. A part with no `=` yields an
/// empty value (that is how a bare flag such as `FB_ROT180` is written).
fn pairs(cmdline: &str) -> impl Iterator<Item = (&str, &str)> {
    cmdline.split(':').map(|part| {
        let mut it = part.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        (k, v)
    })
}

/// The `LOG=` level of a cmdline, if it carries one.
///
/// An unrecognised level is *not* an error: it degrades to `Info` rather than
/// leaving the boot silent.
pub fn parse_log_level(cmdline: &str) -> Option<LevelFilter> {
    for (k, v) in pairs(cmdline) {
        if k.eq_ignore_ascii_case("LOG") {
            return Some(v.parse().unwrap_or(LevelFilter::Info));
        }
    }
    None
}

/// Whether a boolean flag is set on the cmdline.
///
/// `KEY`, `KEY=1`, `KEY=true` and `KEY=on` are true; `KEY=0` (and anything
/// else) is false. Writing `KEY=0` to turn something off has to work: the
/// kernel side of this same idiom once matched with `str::contains` and
/// turned the flag *on* for `nvidia.hwcursor=0`.
pub fn has_flag(cmdline: &str, key: &str) -> bool {
    for (k, v) in pairs(cmdline) {
        if k.eq_ignore_ascii_case(key) {
            return v.is_empty()
                || v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("on");
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_level_is_read_and_is_case_insensitive() {
        assert_eq!(parse_log_level("LOG=debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_log_level("log=TRACE"), Some(LevelFilter::Trace));
        assert_eq!(
            parse_log_level("TERM=linux:LOG=warn:ROOTPROC=/bin/sh"),
            Some(LevelFilter::Warn)
        );
    }

    #[test]
    fn missing_log_key_keeps_the_firmware_default() {
        assert_eq!(parse_log_level(""), None);
        assert_eq!(parse_log_level("TERM=linux:ROOTPROC=/bin/sh"), None);
        // `LOGO` is not `LOG`.
        assert_eq!(parse_log_level("LOGO=1"), None);
    }

    #[test]
    fn unparsable_log_level_falls_back_to_info() {
        assert_eq!(parse_log_level("LOG=chatty"), Some(LevelFilter::Info));
        assert_eq!(parse_log_level("LOG="), Some(LevelFilter::Info));
    }

    #[test]
    fn a_value_may_contain_anything_but_a_colon() {
        // The separator is `:`; `=` and `?` belong to the value.
        assert_eq!(
            parse_log_level("ROOTPROC=/bin/busybox?sh:LOG=off"),
            Some(LevelFilter::Off)
        );
    }

    #[test]
    fn flag_forms_that_are_true() {
        for c in [
            "FB_ROT180",
            "FB_ROT180=",
            "FB_ROT180=1",
            "FB_ROT180=true",
            "FB_ROT180=TRUE",
            "FB_ROT180=on",
            "LOG=debug:FB_ROT180=1",
        ] {
            assert!(has_flag(c, "FB_ROT180"), "{c:?} should set the flag");
        }
    }

    #[test]
    fn flag_equals_zero_turns_it_off() {
        // The regression this mirrors: `nvidia.hwcursor=0` *enabling* hwcursor.
        for c in [
            "FB_ROT180=0",
            "FB_ROT180=false",
            "FB_ROT180=off",
            "FB_ROT180=no",
        ] {
            assert!(!has_flag(c, "FB_ROT180"), "{c:?} should clear the flag");
        }
    }

    #[test]
    fn a_flag_is_not_matched_inside_another_key_or_value() {
        assert!(!has_flag("FB_ROT180_EXTRA=1", "FB_ROT180"));
        assert!(!has_flag("ROOTPROC=/bin/FB_ROT180", "FB_ROT180"));
        assert!(!has_flag("", "FB_ROT180"));
    }

    #[test]
    fn surrounding_spaces_are_ignored() {
        assert!(has_flag(" FB_MIRROR_X = on ", "FB_MIRROR_X"));
        assert_eq!(parse_log_level(" LOG = debug "), Some(LevelFilter::Debug));
    }
}
