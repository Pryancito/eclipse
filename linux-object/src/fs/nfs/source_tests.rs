//! The `host:/export` split and the mount options.
//!
//! The IPv6 cases are the reason this is a tested function. Every one of them
//! fails *silently* under a naive `split(':')`: the mount is refused for a
//! bad hostname when the hostname was fine, which sends whoever is debugging
//! it to look at DNS.

use super::*;
use alloc::string::String;

// ------------------------------------------------------- the device string

#[test]
fn a_hostname_and_export_split_at_the_colon() {
    let s = NfsSource::parse("nas.local:/srv/media").unwrap();
    assert_eq!(s.host, "nas.local");
    assert_eq!(s.export, "/srv/media");
}

#[test]
fn an_ipv4_literal_splits_the_same_way() {
    let s = NfsSource::parse("192.168.1.10:/export").unwrap();
    assert_eq!(s.host, "192.168.1.10");
    assert_eq!(s.export, "/export");
}

#[test]
fn the_root_export_is_a_single_slash() {
    let s = NfsSource::parse("nas:/").unwrap();
    assert_eq!(s.host, "nas");
    assert_eq!(s.export, "/");
}

/// A bracketed IPv6 literal. Split on the FIRST colon this gives host
/// `"[fe80"`, which then fails to parse as an address — and the mount is
/// reported as a bad hostname for a hostname that was correct.
#[test]
fn a_bracketed_ipv6_literal_keeps_its_colons() {
    let s = NfsSource::parse("[fe80::1]:/export").unwrap();
    assert_eq!(
        s.host, "fe80::1",
        "the brackets are stripped, the colons are not"
    );
    assert_eq!(s.export, "/export");
}

#[test]
fn a_full_length_ipv6_literal_keeps_every_colon() {
    let s = NfsSource::parse("[2001:db8:85a3:0:0:8a2e:370:7334]:/srv").unwrap();
    assert_eq!(s.host, "2001:db8:85a3:0:0:8a2e:370:7334");
    assert_eq!(s.export, "/srv");
}

/// Splitting from the right is what makes an unbracketed IPv6 literal work
/// too — which is not valid `mount(8)` syntax, but is what someone types.
#[test]
fn an_unbracketed_ipv6_literal_splits_at_the_last_colon() {
    let s = NfsSource::parse("fe80::1:/export").unwrap();
    assert_eq!(s.host, "fe80::1");
    assert_eq!(s.export, "/export");
}

#[test]
fn an_unclosed_bracket_is_refused() {
    assert_eq!(NfsSource::parse("[fe80::1:/export"), Err(LxError::EINVAL));
}

#[test]
fn a_bracketed_host_with_no_colon_after_it_is_refused() {
    assert_eq!(NfsSource::parse("[fe80::1]/export"), Err(LxError::EINVAL));
}

#[test]
fn a_string_with_no_colon_is_refused() {
    assert_eq!(NfsSource::parse("nas.local"), Err(LxError::EINVAL));
    assert_eq!(NfsSource::parse("/dev/sda1"), Err(LxError::EINVAL));
}

#[test]
fn an_empty_host_is_refused() {
    assert_eq!(NfsSource::parse(":/export"), Err(LxError::EINVAL));
    assert_eq!(NfsSource::parse("[]:/export"), Err(LxError::EINVAL));
}

#[test]
fn an_empty_export_is_refused() {
    assert_eq!(NfsSource::parse("nas:"), Err(LxError::EINVAL));
    assert_eq!(NfsSource::parse("[fe80::1]:"), Err(LxError::EINVAL));
}

/// Linux refuses a relative export path: the server names its exports
/// absolutely, so a relative one has nothing to be relative to.
#[test]
fn a_relative_export_path_is_refused() {
    assert_eq!(NfsSource::parse("nas:srv/media"), Err(LxError::EINVAL));
}

#[test]
fn an_empty_string_is_refused() {
    assert_eq!(NfsSource::parse(""), Err(LxError::EINVAL));
}

// ----------------------------------------------------------- mount options

#[test]
fn the_defaults_are_the_documented_ones() {
    let o = NfsOptions::default();
    assert_eq!(o.port, None, "ask portmap, or fall back to 2049");
    assert_eq!(o.mountport, None);
    assert_eq!(o.rsize, DEFAULT_TRANSFER_SIZE);
    assert_eq!(o.wsize, DEFAULT_TRANSFER_SIZE);
    assert_eq!(o.timeout_ms, 60_000);
    assert_eq!(o.retrans, 3);
    assert!(o.soft, "soft by default, unlike Linux; see the type's docs");
    assert!(!o.read_only);
}

#[test]
fn an_empty_option_string_gives_the_defaults() {
    assert_eq!(NfsOptions::parse("").unwrap(), NfsOptions::default());
}

#[test]
fn ports_are_parsed() {
    let o = NfsOptions::parse("port=2049,mountport=20048").unwrap();
    assert_eq!(o.port, Some(2049));
    assert_eq!(o.mountport, Some(20048));
}

#[test]
fn transfer_sizes_are_parsed() {
    let o = NfsOptions::parse("rsize=8192,wsize=16384").unwrap();
    assert_eq!(o.rsize, 8192);
    assert_eq!(o.wsize, 16384);
}

/// `timeo` is in TENTHS OF A SECOND, which is the unit an `fstab` line copied
/// from a Linux box uses. Reading `timeo=600` as milliseconds would make
/// every timeout 600 ms instead of a minute — a mount that works but drops
/// every call to a server under load.
#[test]
fn timeo_is_tenths_of_a_second_as_linux_spells_it() {
    let o = NfsOptions::parse("timeo=600").unwrap();
    assert_eq!(o.timeout_ms, 60_000, "600 tenths of a second is a minute");
    let o = NfsOptions::parse("timeo=7").unwrap();
    assert_eq!(o.timeout_ms, 700);
}

/// And it must not overflow for an absurd value in an `fstab` line.
#[test]
fn an_enormous_timeo_saturates_rather_than_wrapping() {
    let o = NfsOptions::parse("timeo=18446744073709551615").unwrap();
    assert_eq!(o.timeout_ms, u64::MAX);
}

#[test]
fn soft_and_hard_set_the_same_flag_either_way() {
    assert!(NfsOptions::parse("soft").unwrap().soft);
    assert!(!NfsOptions::parse("hard").unwrap().soft);
    assert!(
        !NfsOptions::parse("soft,hard").unwrap().soft,
        "the last one wins, as mount(8) does"
    );
    assert!(NfsOptions::parse("hard,soft").unwrap().soft);
}

#[test]
fn ro_and_rw_set_read_only() {
    assert!(NfsOptions::parse("ro").unwrap().read_only);
    assert!(!NfsOptions::parse("rw").unwrap().read_only);
    assert!(!NfsOptions::parse("ro,rw").unwrap().read_only);
}

/// Unknown options are ignored, not refused. `mount(8)` passes along a dozen
/// options this client has no opinion about, and refusing them would fail a
/// mount over a flag that changes nothing.
#[test]
fn unknown_options_are_ignored() {
    let o = NfsOptions::parse("relatime,nosuid,vers=3,proto=tcp,rsize=4096,addr=1.2.3.4").unwrap();
    assert_eq!(o.rsize, 4096, "the one it knows is still parsed");
    assert_eq!(o.timeout_ms, 60_000);
}

/// A whole `fstab`-shaped line, which is the realistic input.
#[test]
fn a_realistic_fstab_option_line_parses() {
    let o = NfsOptions::parse("rw,relatime,vers=3,rsize=65536,wsize=65536,namlen=255,hard,proto=tcp,timeo=600,retrans=2,sec=sys,mountport=20048").unwrap();
    assert_eq!(o.rsize, 65536);
    assert_eq!(o.wsize, 65536);
    assert!(!o.soft, "hard was asked for");
    assert_eq!(o.timeout_ms, 60_000);
    assert_eq!(o.retrans, 2);
    assert_eq!(o.mountport, Some(20048));
    assert!(!o.read_only);
}

/// A malformed VALUE is refused, unlike an unknown KEY: it is a mistake in
/// the line, and mounting with a default instead hides it until the mount
/// behaves unlike what was asked for.
#[test]
fn a_non_numeric_value_is_refused() {
    for bad in [
        "port=abc",
        "rsize=lots",
        "timeo=-1",
        "retrans=x",
        "port=20x48",
    ] {
        assert_eq!(
            NfsOptions::parse(bad),
            Err(LxError::EINVAL),
            "{} should be refused",
            bad
        );
    }
}

#[test]
fn an_option_that_needs_a_value_and_has_none_is_refused() {
    assert_eq!(NfsOptions::parse("port"), Err(LxError::EINVAL));
    assert_eq!(NfsOptions::parse("port="), Err(LxError::EINVAL));
    assert_eq!(NfsOptions::parse("rsize"), Err(LxError::EINVAL));
}

#[test]
fn port_zero_and_an_out_of_range_port_are_refused() {
    assert_eq!(NfsOptions::parse("port=0"), Err(LxError::EINVAL));
    assert_eq!(NfsOptions::parse("port=65536"), Err(LxError::EINVAL));
    assert_eq!(
        NfsOptions::parse("port=65535").unwrap().port,
        Some(65535),
        "the highest real port is allowed"
    );
}

#[test]
fn a_zero_transfer_size_is_refused() {
    assert_eq!(NfsOptions::parse("rsize=0"), Err(LxError::EINVAL));
    assert_eq!(NfsOptions::parse("wsize=0"), Err(LxError::EINVAL));
}

/// A transfer size is rounded up to a multiple of 512, as Linux does with
/// `rsize=100`.
#[test]
fn a_transfer_size_is_rounded_up_to_a_block_multiple() {
    assert_eq!(NfsOptions::parse("rsize=100").unwrap().rsize, 512);
    assert_eq!(NfsOptions::parse("rsize=513").unwrap().rsize, 1024);
    assert_eq!(
        NfsOptions::parse("rsize=4096").unwrap().rsize,
        4096,
        "an exact multiple is left alone"
    );
}

/// The clamp is what keeps `rsize=1073741824` from becoming a gibibyte
/// allocation in the kernel heap per in-flight read.
#[test]
fn an_enormous_transfer_size_is_clamped_not_honoured() {
    let o = NfsOptions::parse("rsize=1073741824,wsize=1073741824").unwrap();
    assert_eq!(o.rsize, MAX_TRANSFER_SIZE);
    assert_eq!(o.wsize, MAX_TRANSFER_SIZE);
}

/// The clamp must not then be rounded back UP over the cap, which is what a
/// `next_multiple_of` after the `min` would do if the cap were not itself a
/// multiple of 512.
#[test]
fn clamping_never_rounds_back_over_the_cap() {
    let o = NfsOptions::parse("rsize=18446744073709551615").unwrap();
    assert!(
        o.rsize <= MAX_TRANSFER_SIZE,
        "{} is over the cap of {}",
        o.rsize,
        MAX_TRANSFER_SIZE
    );
}

#[test]
fn an_enormous_retrans_saturates_into_a_u32() {
    let o = NfsOptions::parse("retrans=18446744073709551615").unwrap();
    assert_eq!(o.retrans, u32::MAX);
}

#[test]
fn empty_items_in_the_option_list_are_skipped() {
    let o = NfsOptions::parse(",,rsize=1024,,").unwrap();
    assert_eq!(o.rsize, 1024);
}

#[test]
fn the_machine_name_is_a_fixed_non_empty_string() {
    let name = machine_name();
    assert!(!name.is_empty());
    assert_eq!(
        String::from_utf8(name).unwrap(),
        "eclipse",
        "a fixed name is one fewer thing that varies between machines"
    );
}
