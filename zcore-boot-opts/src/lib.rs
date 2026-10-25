//! Host-testable cmdline parsers for zCore boot options.
//!
//! Kept as a tiny `no_std` crate so unit tests do not pull in the full kernel
//! dependency graph (the `zcore` binary stays `test = false`).

#![no_std]
#![deny(warnings)]

extern crate alloc;

use alloc::collections::BTreeMap;

/// Parse `key=value` pairs separated by `:` (QEMU-style kernel cmdline lists).
///
/// Bare tokens (no `=`) are ignored here — callers that need presence-only
/// flags should use the kernel's `kernel_hal::cmdline` helpers.
///
/// A repeated key answers with its FIRST occurrence, which is what the
/// bootloader does with the same string, and what [`value`] and
/// `kernel_hal::cmdline` answer for it. This was a plain `insert`, so the map
/// said the LAST one: a line carrying `LOG=info` early and `LOG=error` late
/// read as `info` everywhere in the kernel and as `error` here. Nothing in the
/// tree calls this function, which is the only reason the disagreement never
/// shipped; the mirror tests in `zCore` compare the two crates now, so the
/// first caller cannot inherit it.
///
/// An EMPTY key is not a key, and is dropped the way [`pairs`] drops it. This
/// used to keep one: a line beginning with `=`, or carrying a stray `:=`, put
/// a nameless entry in the map holding the text after it, where the kernel's
/// own parser has no such entry to find. That one the mirror tests caught --
/// neither crate's own tests had a line shaped like it.
pub fn parse_cmdline(cmdline: &str) -> BTreeMap<&str, &str> {
    let mut options = BTreeMap::new();
    for opt in cmdline.split(':') {
        let mut iter = opt.trim().splitn(2, '=');
        let key = iter.next().unwrap_or("").trim();
        if key.is_empty() {
            continue;
        }
        if let Some(value) = iter.next() {
            options.entry(key).or_insert(value.trim());
        }
    }
    options
}

/// `key`/`value` pairs in order. A bare key yields an empty value.
///
/// Character for character `kernel_hal::cmdline::pairs`. Public so the copy
/// can be held against the original: `zCore` is the only crate that depends on
/// both, and its `cmdline_mirror_tests` drives the two over one table of lines.
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
/// answers.
///
/// Character for character `kernel_hal::cmdline::value`, and the lookup
/// [`parse_tlbhammer`] used to spell inline. Case-insensitive on the key, like
/// every other cmdline lookup in the kernel.
pub fn value<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    pairs(cmdline)
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// Same rules as `kernel_hal::cmdline::parse_number`: decimal or `0x` hex,
/// refuse overflow and refuse trailing junk (`6spins` is not six).
///
/// Public for the same reason [`pairs`] is: it is the third of the three copies
/// `zCore` holds against the kernel's own.
pub fn parse_number(v: &str) -> Option<u64> {
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

/// Parse `eclipse.tlbhammer=N` from the cmdline. `None` = disabled.
///
/// Matches `zCore::tlb_hammer::parse_tlbhammer` / `kernel_hal::cmdline`:
/// - missing / non-numeric → `None`
/// - `=0` / `=0x0` → `None` (kill-switch; must not clamp up to 3)
/// - `1`/`2` → `Some(3)` (need mapper + holder + churn)
/// - `N >= 3` → `Some(N)`
///
/// Uses a real key lookup, not `contains("eclipse.tlbhammer=")`.
pub fn parse_tlbhammer(cmdline: &str) -> Option<usize> {
    let spelled = value(cmdline, "eclipse.tlbhammer")?;
    let n = parse_number(spelled).and_then(|n| usize::try_from(n).ok())?;
    if n == 0 {
        return None;
    }
    Some(n.max(3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cmdline_splits_colon_key_values() {
        let m = parse_cmdline("LOG=info:INIT=/sbin/init:smp=off");
        assert_eq!(m.get("LOG"), Some(&"info"));
        assert_eq!(m.get("INIT"), Some(&"/sbin/init"));
        assert_eq!(m.get("smp"), Some(&"off"));
        assert!(!m.contains_key("missing"));
    }

    #[test]
    fn parse_cmdline_ignores_bare_tokens() {
        let m = parse_cmdline("solo:KEY=val:another");
        assert_eq!(m.get("KEY"), Some(&"val"));
        assert!(!m.contains_key("solo"));
    }

    /// A line can carry the same key twice -- an installed default followed by
    /// something typed at the boot prompt is the ordinary way it happens -- and
    /// the bootloader reads the first one. So does `kernel_hal::cmdline`, and
    /// the two tests above cannot see which one this map keeps, because neither
    /// of their lines repeats a key.
    #[test]
    fn parse_cmdline_answers_with_the_first_of_a_repeated_key() {
        let m = parse_cmdline("LOG=info:LOG=error");
        assert_eq!(m.get("LOG"), Some(&"info"), "the first one, not the last");
        let m = parse_cmdline("ROOT=/dev/sda2:LOG=warn:ROOT=/dev/vda:LOG=off");
        assert_eq!(m.get("ROOT"), Some(&"/dev/sda2"));
        assert_eq!(m.get("LOG"), Some(&"warn"));
    }

    /// Nothing before the `=` is not a key. The map used to hold a nameless
    /// entry for it, and `kernel_hal::cmdline` has no such entry to answer
    /// with -- which is how the mirror tests in `zCore` found it, since no
    /// line in this file was shaped like that either.
    #[test]
    fn parse_cmdline_drops_an_empty_key() {
        assert!(parse_cmdline("=novalue").is_empty());
        assert!(parse_cmdline(" = novalue ").is_empty());
        let m = parse_cmdline("LOG=info:=stray:ROOT=/dev/sda2");
        assert_eq!(m.get("LOG"), Some(&"info"));
        assert_eq!(m.get("ROOT"), Some(&"/dev/sda2"));
        assert_eq!(m.get(""), None, "a nameless entry is not a key");
        assert_eq!(m.len(), 2);
    }

    /// `Some("")` and `None` are different answers: a key written with nothing
    /// after the `=` is present and says nothing, and a key nobody wrote is
    /// absent. A caller that cannot tell them apart cannot tell "turned off"
    /// from "not mentioned", and a switch whose default is on then defaults
    /// to off.
    #[test]
    fn value_knows_an_empty_value_from_a_missing_key() {
        assert_eq!(value("LOG=", "LOG"), Some(""));
        assert_eq!(value("LOG", "LOG"), Some(""));
        assert_eq!(value("TERM=xterm", "LOG"), None);
        assert_eq!(value("", "LOG"), None);
    }

    #[test]
    fn value_answers_with_the_first_occurrence_whatever_its_case() {
        assert_eq!(value("LOG=info:LOG=error", "LOG"), Some("info"));
        assert_eq!(value("log=info", "LOG"), Some("info"));
        assert_eq!(value("LOG=info", "log"), Some("info"));
        // The key, not a substring of one: the whole point of a real lookup.
        assert_eq!(value("XLOG=info", "LOG"), None);
        assert_eq!(value("NOTE=LOG=info", "LOG"), None);
    }

    /// The pieces, in the order written, which is what makes "the first one"
    /// a question anybody can answer. An empty key is not a pair at all, so a
    /// stray colon or a leading `=` drops out rather than taking the place of
    /// the next one.
    #[test]
    fn pairs_keeps_the_order_and_drops_an_empty_key() {
        let got = |c| pairs(c).collect::<alloc::vec::Vec<_>>();
        assert_eq!(
            got("LOG=error:TERM=xterm:ROOT=/dev/sda2"),
            [("LOG", "error"), ("TERM", "xterm"), ("ROOT", "/dev/sda2")]
        );
        // A bare key is findable, with nothing after it.
        assert_eq!(got("solo:KEY=val"), [("solo", ""), ("KEY", "val")]);
        // Only the first `=` separates, so a value may carry its own.
        assert_eq!(got("KEY=a=b"), [("KEY", "a=b")]);
        // Nothing before the `=`, and the empty pieces a doubled colon leaves.
        assert_eq!(got("=novalue"), []);
        assert_eq!(got("::"), []);
        assert_eq!(got(""), []);
        // The spaces a person leaves around either half are not part of it.
        assert_eq!(got(" LOG = info "), [("LOG", "info")]);
    }

    /// Decimal or `0x` hex, and nothing else: a count that does not fit is not
    /// a smaller count, and `6spins` is not six.
    #[test]
    fn parse_number_refuses_junk_and_overflow() {
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(parse_number("6"), Some(6));
        assert_eq!(parse_number("0x8"), Some(8));
        assert_eq!(parse_number("0X10"), Some(16));
        assert_eq!(parse_number("18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_number("18446744073709551616"), None, "overflow");
        assert_eq!(parse_number("0x10000000000000000"), None, "hex overflow");
        assert_eq!(parse_number("6spins"), None);
        assert_eq!(parse_number("0x"), None);
        assert_eq!(parse_number("0b101"), None, "not a spelling we accept");
        assert_eq!(parse_number("-1"), None);
        assert_eq!(parse_number(""), None);
    }

    #[test]
    fn parse_tlbhammer_disabled_without_token() {
        assert_eq!(parse_tlbhammer("LOG=info"), None);
        assert_eq!(parse_tlbhammer(""), None);
    }

    #[test]
    fn parse_tlbhammer_zero_turns_it_off() {
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=0"), None);
        assert_eq!(parse_tlbhammer("LOG=error:eclipse.tlbhammer=0"), None);
    }

    #[test]
    fn parse_tlbhammer_clamps_small_n() {
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=1"), Some(3));
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=2"), Some(3));
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=6"), Some(6));
        assert_eq!(
            parse_tlbhammer("prefix:eclipse.tlbhammer=8:suffix"),
            Some(8)
        );
    }

    #[test]
    fn parse_tlbhammer_hex_and_junk() {
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=0x8"), Some(8));
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=0x0"), None);
        assert_eq!(parse_tlbhammer("eclipse.tlbhammer=6spins"), None);
        assert_eq!(
            parse_tlbhammer("LOG=error: eclipse.tlbhammer = 6 "),
            Some(6)
        );
    }

    #[test]
    fn parse_tlbhammer_ignores_a_lookalike_key() {
        assert_eq!(parse_tlbhammer("xeclipse.tlbhammer=6"), None);
        assert_eq!(parse_tlbhammer("NOTE=eclipse.tlbhammer=6"), None);
    }
}
