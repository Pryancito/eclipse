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
pub fn parse_cmdline(cmdline: &str) -> BTreeMap<&str, &str> {
    let mut options = BTreeMap::new();
    for opt in cmdline.split(':') {
        let mut iter = opt.trim().splitn(2, '=');
        if let Some(key) = iter.next() {
            if let Some(value) = iter.next() {
                options.insert(key.trim(), value.trim());
            }
        }
    }
    options
}

/// `key`/`value` pairs in order. A bare key yields an empty value.
fn pairs(cmdline: &str) -> impl Iterator<Item = (&str, &str)> {
    cmdline.split(':').filter_map(|opt| {
        let mut it = opt.splitn(2, '=');
        let k = it.next()?.trim();
        if k.is_empty() {
            return None;
        }
        Some((k, it.next().unwrap_or("").trim()))
    })
}

/// Same rules as `kernel_hal::cmdline::parse_number`: decimal or `0x` hex,
/// refuse overflow and refuse trailing junk (`6spins` is not six).
fn parse_number(v: &str) -> Option<u64> {
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
    let spelled = pairs(cmdline)
        .find(|(k, _)| k.eq_ignore_ascii_case("eclipse.tlbhammer"))
        .map(|(_, v)| v)?;
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
        assert!(m.get("missing").is_none());
    }

    #[test]
    fn parse_cmdline_ignores_bare_tokens() {
        let m = parse_cmdline("solo:KEY=val:another");
        assert_eq!(m.get("KEY"), Some(&"val"));
        assert!(m.get("solo").is_none());
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
