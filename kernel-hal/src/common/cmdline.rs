//! The kernel command line, read one way.
//!
//! What the bootloader hands over is a colon-separated list of `KEY=value`
//! pairs — `LOG=error:TERM=xterm-256color:console.shell=true:ROOT=/dev/sda2` —
//! and roughly forty places in this kernel ask it a question. They did it six
//! different ways, and the ways disagree.
//!
//! The one that mattered is `str::contains`, which about thirty of them used.
//! It has no notion of a key, so it answers yes to a flag name wherever those
//! characters appear: inside a longer key, inside somebody else's *value*, and
//! — the one that bites — inside the flag's own `=0`. Every switch spelled
//! this way turns **on** when it is explicitly turned off, which is the first
//! thing anyone tries. Meanwhile the framebuffer console had the right parser
//! all along, private to a file only a bare build compiles, so `FB_ROT180=0`
//! did the right thing while `nvidia.hwcursor=0` did the opposite.
//!
//! Underneath that, the map built by `boot_options` **drops any key with no
//! `=`**, so a bare flag could not be looked up in it at all. That is why the
//! call sites reached for `contains` in the first place.
//!
//! A repeated key answers with its **first** occurrence, which is what the
//! bootloader does with the same string.

/// The `key`/`value` pairs, in the order written. A bare key yields an empty
/// value, which is what makes it findable at all.
pub fn pairs(cmdline: &str) -> impl Iterator<Item = (&str, &str)> {
    cmdline.split(':').filter_map(|opt| {
        let mut it = opt.splitn(2, '=');
        let k = it.next()?.trim();
        if k.is_empty() {
            return None;
        }
        Some((k, it.next().unwrap_or("").trim()))
    })
}

/// The value written for `key`, or `None` when the key is absent. A bare key
/// is present with an empty value, so `Some("")` and `None` are different
/// answers and the caller can tell "off" from "unset".
pub fn value<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    pairs(cmdline)
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// How a value spells a boolean, or `None` when it spells neither.
///
/// The empty string is not a spelling: a bare `key` is the *presence* of a
/// flag, which [`flag`] answers, not a value that reads true.
pub fn parse_bool(v: &str) -> Option<bool> {
    const YES: [&str; 4] = ["1", "true", "on", "yes"];
    const NO: [&str; 4] = ["0", "false", "off", "no"];
    if YES.iter().any(|y| v.eq_ignore_ascii_case(y)) {
        Some(true)
    } else if NO.iter().any(|n| v.eq_ignore_ascii_case(n)) {
        Some(false)
    } else {
        None
    }
}

/// Whether the boolean flag `key` is on: present, and not spelled off.
///
/// A value that spells neither — `nvidia.hwcursor=banana` — is not an
/// affirmation, and reads off. That is what the bootloader does with the
/// flags it reads from this same string, and a typo that silently enables a
/// hardware path is worse than one that silently does not.
pub fn flag(cmdline: &str, key: &str) -> bool {
    match value(cmdline, key) {
        None => false,
        Some("") => true,
        Some(v) => parse_bool(v).unwrap_or(false),
    }
}

/// Whether `key` is present and spells off — which is what a `KEY=0`
/// kill-switch asks.
///
/// Not the same question as `!flag`, which is also true for a key nobody
/// wrote: a switch whose default is *on* has to tell "turned off" from "not
/// mentioned", or it defaults to off and the default is a lie.
pub fn is_off(cmdline: &str, key: &str) -> bool {
    value(cmdline, key).and_then(parse_bool) == Some(false)
}

/// The command line out of a NUL-terminated byte string, which is the shape a
/// device tree's `/chosen/bootargs` property has.
///
/// The NUL is not decoration here. [`pairs`] trims ASCII whitespace and a NUL
/// is not whitespace, so `LOG=error:smp=off\0` parses with the value
/// `"off\0"` — which spells no boolean at all, so [`is_off`] says the key was
/// never written and the switch it guards keeps its default. The last flag on
/// the line is the one that gets swallowed, and the last flag on the line is
/// the one somebody just appended to try something.
///
/// `None` when the bytes are not UTF-8.
pub fn from_c_bytes(bytes: &[u8]) -> Option<&str> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// The line a live Eclipse actually boots with.
    const REAL: &str = "LOG=error:TERM=xterm-256color:console.shell=true:\
                        virtcon.disable=true:ROOT=/dev/sda2";

    // ── the reason this module exists ───────────────────────────────────────

    #[test]
    fn a_flag_turned_off_is_off() {
        // `contains` answers yes to the flag's own `=0`, so every switch
        // spelled that way turned ON when it was explicitly turned off.
        for off in ["0", "false", "off", "no", "OFF", "False"] {
            let line = alloc::format!("LOG=error:nvidia.hwcursor={off}");
            assert!(line.contains("nvidia.hwcursor"), "the old test says yes");
            assert!(!flag(&line, "nvidia.hwcursor"), "spelled {}", off);
        }
    }

    #[test]
    fn a_flag_turned_on_is_on() {
        for on in ["", "=", "=1", "=true", "=on", "=yes", "=ON", "=True"] {
            let line = alloc::format!("LOG=error:nvidia.hwcursor{on}:ROOT=/dev/sda2");
            assert!(flag(&line, "nvidia.hwcursor"), "spelled {:?}", on);
        }
    }

    #[test]
    fn a_bootargs_property_keeps_its_last_flag() {
        // A device tree hands the command line over NUL-terminated, and the
        // flag that the NUL lands on is the last one written — which is the
        // one somebody just added to try something.
        let raw = b"LOG=error:smp=off\0";
        let line = from_c_bytes(raw).expect("valid utf-8");
        assert_eq!(line, "LOG=error:smp=off");
        assert!(is_off(line, "smp"), "the flag the NUL was stuck to");
        assert!(
            !is_off(core::str::from_utf8(raw).unwrap(), "smp"),
            "and this is what happens without the strip: the value reads \
             \"off\\0\", which spells no boolean, so the key looks unwritten"
        );
    }

    #[test]
    fn a_bootargs_property_without_a_terminator_is_still_a_command_line() {
        assert_eq!(from_c_bytes(b"LOG=error"), Some("LOG=error"));
        assert_eq!(from_c_bytes(b""), Some(""));
        assert_eq!(from_c_bytes(b"\0"), Some(""));
        assert_eq!(from_c_bytes(&[0xff, 0xfe]), None, "not utf-8");
    }

    #[test]
    fn a_flag_name_inside_a_longer_key_is_not_that_flag() {
        // `contains("smp=off")` is yes for `nosmp=off`, and
        // `contains("noturbo")` is yes for `turbo=noturbo`.
        assert!(!flag("nosmp=off", "smp"));
        assert!(!flag("LOG=error:nvidia.hwcursorx", "nvidia.hwcursor"));
        assert!(!flag("xconsole.overgraphics", "console.overgraphics"));
    }

    #[test]
    fn a_flag_name_inside_somebody_elses_value_is_not_that_flag() {
        // `ROOT=` carries a device path the installer substitutes, and `TERM=`
        // a terminal name; neither is a place to look for a flag.
        assert!(!flag("ROOT=/dev/disk/by-id/smp=on-part2", "smp"));
        assert!(!flag("TERM=noturbo", "noturbo"));
        assert_eq!(value("TERM=noturbo", "noturbo"), None);
    }

    #[test]
    fn a_bare_key_is_findable() {
        // The map `boot_options` built dropped every key with no `=`, so a
        // bare flag could not be looked up in it at all -- which is why the
        // call sites reached for `contains`.
        assert_eq!(
            value("LOG=error:console.overgraphics", "console.overgraphics"),
            Some("")
        );
        assert!(flag(
            "LOG=error:console.overgraphics",
            "console.overgraphics"
        ));
    }

    #[test]
    fn absent_and_off_are_different_answers() {
        assert_eq!(value(REAL, "nvidia.hwcursor"), None);
        assert_eq!(value("nvidia.hwcursor=0", "nvidia.hwcursor"), Some("0"));
        assert!(!flag(REAL, "nvidia.hwcursor"));
        assert!(!flag("nvidia.hwcursor=0", "nvidia.hwcursor"));
    }

    // ── the real command line ───────────────────────────────────────────────

    #[test]
    fn the_shipped_command_line_reads_as_written() {
        assert_eq!(value(REAL, "LOG"), Some("error"));
        assert_eq!(value(REAL, "TERM"), Some("xterm-256color"));
        assert_eq!(value(REAL, "ROOT"), Some("/dev/sda2"));
        assert!(flag(REAL, "console.shell"));
        assert!(flag(REAL, "virtcon.disable"));
    }

    #[test]
    fn a_root_device_keeps_its_slashes_and_colons_are_the_only_separator() {
        assert_eq!(
            value("ROOT=/dev/nvme0n1p3:LOG=info", "ROOT"),
            Some("/dev/nvme0n1p3")
        );
    }

    #[test]
    fn a_value_may_hold_an_equals_sign() {
        // `INIT=/sbin/init?--flag=1` and the like: only the first `=` splits.
        assert_eq!(
            value("INIT=/sbin/init?--x=1", "INIT"),
            Some("/sbin/init?--x=1")
        );
    }

    #[test]
    fn keys_are_case_insensitive_and_values_are_not() {
        assert_eq!(value("log=Error", "LOG"), Some("Error"));
        assert_eq!(value("LOG=Error", "log"), Some("Error"));
        assert_eq!(value(REAL, "root"), Some("/dev/sda2"));
    }

    #[test]
    fn surrounding_spaces_are_not_part_of_the_key_or_the_value() {
        assert_eq!(
            value("  LOG  =  info  : ROOT = /dev/sda2 ", "LOG"),
            Some("info")
        );
        assert_eq!(
            value("  LOG  =  info  : ROOT = /dev/sda2 ", "ROOT"),
            Some("/dev/sda2")
        );
    }

    // ── shape ───────────────────────────────────────────────────────────────

    #[test]
    fn a_repeated_key_answers_with_the_first_one() {
        // Which is what the bootloader does with this same string; the map in
        // `boot_options` kept the last, so the two disagreed.
        assert_eq!(value("LOG=info:LOG=debug", "LOG"), Some("info"));
        assert!(flag("x=on:x=off", "x"));
    }

    #[test]
    fn an_empty_or_separator_only_line_has_no_pairs() {
        assert_eq!(pairs("").count(), 0);
        assert_eq!(pairs(":::").count(), 0);
        assert_eq!(pairs("  :  ").count(), 0);
        assert_eq!(value("", "LOG"), None);
    }

    #[test]
    fn empty_stretches_between_separators_are_skipped_not_counted() {
        let got: Vec<_> = pairs("::LOG=info::ROOT=/dev/sda2:").collect();
        assert_eq!(got, [("LOG", "info"), ("ROOT", "/dev/sda2")]);
    }

    #[test]
    fn pairs_keep_the_order_they_were_written_in() {
        let got: Vec<_> = pairs(REAL).map(|(k, _)| k).collect();
        assert_eq!(
            got,
            ["LOG", "TERM", "console.shell", "virtcon.disable", "ROOT"]
        );
    }

    #[test]
    fn a_bare_key_and_a_key_with_an_empty_value_read_alike() {
        assert_eq!(value("a:b=", "a"), Some(""));
        assert_eq!(value("a:b=", "b"), Some(""));
    }

    // ── is_off ──────────────────────────────────────────────────────────────

    #[test]
    fn a_kill_switch_fires_only_when_it_was_written() {
        // `FORKGATHER=0` and friends: default on, and the `=0` is the whole
        // switch. `!flag` would fire on every boot that never mentioned it.
        assert!(is_off("FORKGATHER=0", "FORKGATHER"));
        assert!(!is_off(REAL, "FORKGATHER"));
        assert!(!is_off("FORKGATHER=1", "FORKGATHER"));
        assert!(!is_off("FORKGATHER", "FORKGATHER"));
    }

    #[test]
    fn a_kill_switch_takes_every_spelling_of_off() {
        for off in ["0", "false", "off", "no", "OFF"] {
            let line = alloc::format!("LOG=error:smp={off}");
            assert!(is_off(&line, "smp"), "spelled {}", off);
        }
    }

    #[test]
    fn a_kill_switch_ignores_a_value_that_spells_neither() {
        // Silently switching off on a typo is the same failure as silently
        // switching on, in the other direction.
        assert!(!is_off("smp=offf", "smp"));
        assert!(!is_off("smp=banana", "smp"));
    }

    #[test]
    fn off_and_not_on_are_different_questions() {
        assert!(!flag(REAL, "FORKGATHER") && !is_off(REAL, "FORKGATHER"));
    }

    // ── parse_bool ──────────────────────────────────────────────────────────

    #[test]
    fn parse_bool_knows_both_spellings() {
        for y in ["1", "true", "on", "yes", "TRUE", "On", "YES"] {
            assert_eq!(parse_bool(y), Some(true), "{}", y);
        }
        for n in ["0", "false", "off", "no", "FALSE", "Off", "NO"] {
            assert_eq!(parse_bool(n), Some(false), "{}", n);
        }
    }

    #[test]
    fn parse_bool_refuses_what_is_not_a_boolean() {
        for other in ["", "banana", "2", "-1", "onward", "of", "ye"] {
            assert_eq!(parse_bool(other), None, "{}", other);
        }
    }

    #[test]
    fn a_value_that_spells_neither_is_not_an_affirmation() {
        // A typo that silently enables a hardware path is worse than one that
        // silently does not.
        assert!(!flag("nvidia.hwcursor=banana", "nvidia.hwcursor"));
        assert!(!flag("nvidia.hwcursor=onward", "nvidia.hwcursor"));
        assert_eq!(
            value("nvidia.hwcursor=banana", "nvidia.hwcursor"),
            Some("banana")
        );
    }
}
