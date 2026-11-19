//! `GETPORT`: its four argument words and the two answers that are not
//! ports.

use super::*;

#[test]
fn getport_targets_portmap_v2_procedure_3() {
    let t = getport_target();
    assert_eq!(t.program, PROGRAM_PORTMAP);
    assert_eq!(t.version, PORTMAP_VERSION);
    assert_eq!(t.procedure, PROC_GETPORT);
}

/// The `mapping` struct is four words in this order (RFC 1833 section 3.2).
/// A transposition answers with a port for the wrong program, which then
/// fails as a connection refused somewhere else entirely.
#[test]
fn getport_arguments_are_prog_vers_prot_and_a_zero_port() {
    let args = encode_getport(PROGRAM_MOUNT, 3, IPPROTO_TCP);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.u32(), Ok(PROGRAM_MOUNT), "prog");
    assert_eq!(dec.u32(), Ok(3), "vers");
    assert_eq!(dec.u32(), Ok(IPPROTO_TCP), "prot");
    assert_eq!(dec.u32(), Ok(0), "port is an output, sent as zero");
    assert!(dec.is_empty());
}

#[test]
fn the_program_numbers_are_the_assigned_ones() {
    // RFC 1813 appendix I and RFC 1833: these are not configurable and a
    // typo in one is a mount that fails against every server.
    assert_eq!(PROGRAM_PORTMAP, 100_000);
    assert_eq!(PROGRAM_NFS, 100_003);
    assert_eq!(PROGRAM_MOUNT, 100_005);
    assert_eq!(IPPROTO_TCP, 6);
    assert_eq!(IPPROTO_UDP, 17);
    assert_eq!(PORTMAP_PORT, 111);
    assert_eq!(NFS_PORT, 2049);
}

#[test]
fn a_port_in_the_reply_is_returned() {
    let mut enc = XdrEncoder::new();
    enc.u32(20048);
    assert_eq!(decode_getport(&enc.into_bytes()), Ok(Some(20048)));
}

/// Portmap answers an unregistered program with port 0, not an error. Taking
/// it as a port means connecting to port 0 and reporting whatever the stack
/// says about that, instead of "the server does not run this service".
#[test]
fn port_zero_means_not_registered_and_not_a_port() {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    assert_eq!(decode_getport(&enc.into_bytes()), Ok(None));
}

/// The field is 32 bits on the wire and a port is 16, so the check has to
/// happen somewhere.
#[test]
fn a_port_above_65535_is_refused() {
    let mut enc = XdrEncoder::new();
    enc.u32(70_000);
    assert!(decode_getport(&enc.into_bytes()).is_err());
}

#[test]
fn the_highest_real_port_is_accepted() {
    let mut enc = XdrEncoder::new();
    enc.u32(65_535);
    assert_eq!(decode_getport(&enc.into_bytes()), Ok(Some(65_535)));
}

#[test]
fn a_truncated_getport_reply_is_an_error() {
    assert!(decode_getport(&[0, 0]).is_err());
}
