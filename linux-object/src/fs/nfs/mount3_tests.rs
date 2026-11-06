//! `MNT`, the call that turns an export path into a filehandle, and the
//! filehandle type.
//!
//! Two of these are about the handle being a credential rather than data: it
//! must not be longer than the protocol allows, and it must not end up in a
//! log line.

use super::*;
use alloc::format;
use alloc::string::String;
use alloc::vec;

/// Build a successful `MNT` reply.
fn mnt_ok(handle: &[u8], flavors: &[u32]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0); // MNT3_OK
    enc.opaque(handle);
    enc.u32(flavors.len() as u32);
    for &f in flavors {
        enc.u32(f);
    }
    enc.into_bytes()
}

#[test]
fn mnt_and_umnt_target_mount_v3() {
    assert_eq!(mnt_target().program, 100_005);
    assert_eq!(mnt_target().version, MOUNT_VERSION);
    assert_eq!(mnt_target().procedure, PROC_MNT);
    assert_eq!(umnt_target().procedure, PROC_UMNT);
    assert_eq!(MOUNT_VERSION, 3);
    assert_eq!(PROC_MNT, 1);
    assert_eq!(PROC_UMNT, 3);
}

#[test]
fn the_export_path_is_encoded_as_an_xdr_string() {
    let args = encode_path("/srv/media").unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.string_bytes(), Ok(&b"/srv/media"[..]));
    assert!(dec.is_empty());
}

#[test]
fn an_export_path_over_the_protocol_limit_is_refused_before_it_is_sent() {
    let long: String = core::iter::repeat_n('a', MNTPATHLEN + 1).collect();
    assert_eq!(encode_path(&long), Err(XdrError::LengthTooLarge));

    let at_limit: String = core::iter::repeat_n('a', MNTPATHLEN).collect();
    assert!(
        encode_path(&at_limit).is_ok(),
        "a path exactly at the limit is allowed"
    );
}

#[test]
fn a_successful_mnt_yields_the_root_handle_and_the_flavours() {
    let reply = mnt_ok(&[1, 2, 3, 4, 5, 6, 7, 8], &[0, 1]);
    let got = decode_mnt(&reply).unwrap().unwrap();
    assert_eq!(got.root.as_bytes(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(got.auth_flavors, vec![0, 1]);
}

/// An empty flavour list is not a refusal; the field is advisory.
#[test]
fn an_empty_flavour_list_is_not_an_error() {
    let reply = mnt_ok(&[9; 32], &[]);
    let got = decode_mnt(&reply).unwrap().unwrap();
    assert_eq!(got.root.len(), 32);
    assert!(got.auth_flavors.is_empty());
}

/// A handle of exactly 64 bytes is the largest legal one, and the test is
/// here because an off-by-one in the cap refuses a handle real servers send
/// (Linux's `nfsd` uses the full size for some export configurations).
#[test]
fn a_handle_of_exactly_fhsize_is_accepted() {
    let reply = mnt_ok(&[0xab; FHSIZE3], &[1]);
    let got = decode_mnt(&reply).unwrap().unwrap();
    assert_eq!(got.root.len(), FHSIZE3);
    assert_eq!(FHSIZE3, 64);
}

#[test]
fn a_handle_over_fhsize_is_refused() {
    let reply = mnt_ok(&[0xab; FHSIZE3 + 1], &[1]);
    assert!(
        decode_mnt(&reply).is_err(),
        "a handle longer than the protocol allows is a malformed reply"
    );
    assert_eq!(
        FileHandle::new(&[0u8; FHSIZE3 + 1]),
        Err(XdrError::LengthTooLarge)
    );
}

/// Every `mountstat3` the RFC lists, including the two that are not errno
/// values. `MNT3ERR_NOTSUPP` is 10004, and reading it as an errno would be a
/// nonsense error number escaping into userspace.
#[test]
fn every_mountstat_maps_to_its_own_status() {
    for (wire, expected) in [
        (0u32, MountStatus::Ok),
        (1, MountStatus::Perm),
        (2, MountStatus::NoEnt),
        (5, MountStatus::Io),
        (13, MountStatus::Acces),
        (20, MountStatus::NotDir),
        (22, MountStatus::Inval),
        (63, MountStatus::NameTooLong),
        (10004, MountStatus::NotSupp),
        (10006, MountStatus::ServerFault),
        (12345, MountStatus::Unknown(12345)),
    ] {
        assert_eq!(
            MountStatus::from_wire(wire),
            expected,
            "mountstat3 {} should be {:?}",
            wire,
            expected
        );
    }
}

/// A server that answered and refused is a different thing from a reply that
/// did not decode, and the return type keeps them apart: the first is the
/// server's verdict on the export path, the second is a broken server.
#[test]
fn a_refused_mount_is_an_answer_and_not_a_decode_error() {
    let mut enc = XdrEncoder::new();
    enc.u32(13); // MNT3ERR_ACCES
    let got = decode_mnt(&enc.into_bytes());
    assert_eq!(
        got,
        Ok(Err(MountStatus::Acces)),
        "the server answered; it answered no"
    );
}

/// A refusal carries no handle, so the decoder must not try to read one.
#[test]
fn a_refused_mount_reply_carries_no_handle_to_read() {
    let mut enc = XdrEncoder::new();
    enc.u32(2); // MNT3ERR_NOENT, and nothing after it
    assert_eq!(decode_mnt(&enc.into_bytes()), Ok(Err(MountStatus::NoEnt)));
}

#[test]
fn a_truncated_mnt_reply_is_an_error() {
    let mut enc = XdrEncoder::new();
    enc.u32(0); // OK, then nothing
    assert!(decode_mnt(&enc.into_bytes()).is_err());
}

/// A flavour count longer than the bytes behind it must fail rather than
/// reserve for it. The count is hostile like every length on the wire.
#[test]
fn a_flavour_count_longer_than_the_reply_is_refused() {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    enc.opaque(&[1, 2, 3, 4]);
    enc.u32(0xffff_ffff); // claims four billion flavours
    assert!(
        decode_mnt(&enc.into_bytes()).is_err(),
        "the count must be checked against the bytes that arrived"
    );
}

/// The handle is a bearer token (RFC 1813 section 2.6): possession is the
/// access. `Debug` prints its length, so a struct containing one can be
/// logged without putting the token in `/var/log`.
#[test]
fn a_filehandle_does_not_print_its_bytes() {
    let fh = FileHandle::new(&[0xde, 0xad, 0xbe, 0xef]).unwrap();
    let shown = format!("{:?}", fh);
    assert_eq!(shown, "FileHandle(4 bytes)");
    assert!(
        !shown.contains("de") && !shown.contains("222"),
        "neither hex nor decimal bytes may appear in {}",
        shown
    );
}

/// And the same when the handle is inside something else, which is how it
/// will actually be logged.
#[test]
fn a_filehandle_nested_in_a_struct_still_does_not_print_its_bytes() {
    let reply = MountReply {
        root: FileHandle::new(&[0x41; 16]).unwrap(),
        auth_flavors: vec![1],
    };
    let shown = format!("{:?}", reply);
    assert!(shown.contains("FileHandle(16 bytes)"), "got {}", shown);
    assert!(
        !shown.contains("65") && !shown.contains("41"),
        "the handle's bytes leaked into {}",
        shown
    );
}

#[test]
fn an_empty_handle_is_recognisable_as_empty() {
    let fh = FileHandle::new(&[]).unwrap();
    assert!(fh.is_empty());
    assert_eq!(fh.len(), 0);
}
