//! The NFSv3 procedures that change something.
//!
//! The tests worth having here are about the shapes that are *not* uniform,
//! because those are where an encoder leaves the server a word out of step
//! and the next call is parsed as something else:
//!
//! - `sattr3` is six optionals in a row, and an absent one is a single
//!   boolean.
//! - `set_atime` is a union whose discriminants are 0, 1 and 2, and the
//!   client never writes 0.
//! - `createhow3` carries a `sattr3` for two of its arms and an eight-byte
//!   verifier for the third.
//! - `MKNOD`'s union carries a `specdata3` for devices and nothing for a
//!   socket or a FIFO.
//! - `RENAME`'s reply carries *two* `wcc_data`.

use super::*;
use crate::error::LxError;
use alloc::vec;

/// A `wcc_data` with both halves present, which is the longer and therefore
/// the more demanding form to consume.
fn put_wcc(enc: &mut XdrEncoder) {
    enc.bool(true); // pre_op_attr
    enc.u64(4097); // size
    enc.u32(1); // mtime.sec
    enc.u32(2); // mtime.nsec
    enc.u32(3); // ctime.sec
    enc.u32(4); // ctime.nsec
    put_fattr_present(enc); // post_op_attr
}

/// A `wcc_data` with neither half present: two booleans.
fn put_wcc_absent(enc: &mut XdrEncoder) {
    enc.bool(false);
    enc.bool(false);
}

/// A present `post_op_attr` holding [`sample_fattr`].
fn put_fattr_present(enc: &mut XdrEncoder) {
    enc.bool(true);
    enc.u32(1); // NF3REG
    enc.u32(0o100_644);
    enc.u32(3); // nlink
    enc.u32(1000); // uid
    enc.u32(1001); // gid
    enc.u64(4097); // size
    enc.u64(2561); // used
    enc.u32(7); // rdev.major
    enc.u32(9); // rdev.minor
    enc.u64(0x1122_3344_5566_7788); // fsid
    enc.u64(0x0099_aabb_ccdd_eeff); // fileid
    enc.u32(111);
    enc.u32(222); // atime
    enc.u32(333);
    enc.u32(444); // mtime
    enc.u32(555);
    enc.u32(666); // ctime
}

/// What [`put_fattr_present`] decodes to.
fn sample_fattr() -> super::super::nfs3::Fattr3 {
    use super::super::nfs3::{Fattr3, Ftype3};
    Fattr3 {
        ftype: Ftype3::Reg,
        mode: 0o100_644,
        nlink: 3,
        uid: 1000,
        gid: 1001,
        size: 4097,
        used: 2561,
        rdev: (7, 9),
        fsid: 0x1122_3344_5566_7788,
        fileid: 0x0099_aabb_ccdd_eeff,
        atime: Timespec {
            sec: 111,
            nsec: 222,
        },
        mtime: Timespec {
            sec: 333,
            nsec: 444,
        },
        ctime: Timespec {
            sec: 555,
            nsec: 666,
        },
    }
}

/// What [`put_wcc`] decodes to.
fn sample_wcc() -> WccData {
    use super::super::nfs3::PreOpAttr;
    WccData {
        before: Some(PreOpAttr {
            size: 4097,
            mtime: Timespec { sec: 1, nsec: 2 },
            ctime: Timespec { sec: 3, nsec: 4 },
        }),
        after: Some(sample_fattr()),
    }
}

/// What [`put_wcc_absent`] decodes to.
fn empty_wcc() -> WccData {
    WccData {
        before: None,
        after: None,
    }
}

/// A filehandle of `len` distinguishable bytes.
fn handle(len: usize) -> FileHandle {
    let bytes: Vec<u8> = (0..len).map(|i| (i as u8) ^ 0x3c).collect();
    FileHandle::new(&bytes).unwrap()
}

// ---------------------------------------------------------------- the numbers

#[test]
fn the_procedure_numbers_are_the_protocols() {
    assert_eq!(PROC_SETATTR, 2);
    assert_eq!(PROC_WRITE, 7);
    assert_eq!(PROC_CREATE, 8);
    assert_eq!(PROC_MKDIR, 9);
    assert_eq!(PROC_SYMLINK, 10);
    assert_eq!(PROC_MKNOD, 11);
    assert_eq!(PROC_REMOVE, 12);
    assert_eq!(PROC_RMDIR, 13);
    assert_eq!(PROC_RENAME, 14);
    assert_eq!(PROC_LINK, 15);
    assert_eq!(PROC_COMMIT, 21);
    assert_eq!(VERF_SIZE, 8);
}

// ------------------------------------------------------------------- sattr3

#[test]
fn an_empty_sattr3_is_six_booleans_and_nothing_else() {
    // The shape that matters: "change nothing" is six words of `false`, not
    // an empty encoding and not six zeroed fields. An encoder that wrote the
    // fields anyway would silently chmod a file to 0 on every SETATTR.
    let fh = handle(32);
    let args = encode_setattr(&fh, &SetAttr::nothing(), None);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    for field in 0..6 {
        assert!(!dec.bool().unwrap(), "sattr3 field {}", field);
    }
    assert!(!dec.bool().unwrap(), "the guard");
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn nothing_and_default_are_the_same_and_say_so() {
    assert_eq!(SetAttr::nothing(), SetAttr::default());
    assert!(SetAttr::nothing().is_empty());
    assert!(!mode_to(0o644).is_empty());
    assert!(
        !truncate_to(0).is_empty(),
        "truncating to zero changes the file"
    );
}

#[test]
fn every_sattr3_field_is_encoded_in_the_order_the_protocol_lists_them() {
    let fh = handle(16);
    let attr = SetAttr {
        mode: Some(0o640),
        uid: Some(1000),
        gid: Some(1001),
        size: Some(0x1_0000_0000),
        atime: Some(SetTime::Server),
        mtime: Some(SetTime::Client(Timespec { sec: 99, nsec: 88 })),
    };
    let args = encode_setattr(&fh, &attr, None);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    assert_eq!(dec.optional(|d| d.u32()).unwrap(), Some(0o640));
    assert_eq!(dec.optional(|d| d.u32()).unwrap(), Some(1000));
    assert_eq!(dec.optional(|d| d.u32()).unwrap(), Some(1001));
    assert_eq!(dec.optional(|d| d.u64()).unwrap(), Some(0x1_0000_0000));
    // set_atime: present, SET_TO_SERVER_TIME, and no time behind it.
    assert!(dec.bool().unwrap());
    assert_eq!(dec.u32().unwrap(), 1);
    // set_mtime: present, SET_TO_CLIENT_TIME, and the time.
    assert!(dec.bool().unwrap());
    assert_eq!(dec.u32().unwrap(), 2);
    assert_eq!(dec.u32().unwrap(), 99);
    assert_eq!(dec.u32().unwrap(), 88);
    assert!(!dec.bool().unwrap(), "no guard");
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn server_time_carries_no_time_and_client_time_carries_one() {
    // The length is the test: one word versus three. Getting it wrong leaves
    // the server reading the guard as a timestamp.
    let fh = handle(8);
    let server = encode_setattr(
        &fh,
        &SetAttr {
            atime: Some(SetTime::Server),
            ..SetAttr::default()
        },
        None,
    );
    let client = encode_setattr(
        &fh,
        &SetAttr {
            atime: Some(SetTime::Client(Timespec { sec: 1, nsec: 2 })),
            ..SetAttr::default()
        },
        None,
    );
    assert_eq!(
        client.len(),
        server.len() + 8,
        "two extra words for the time"
    );
}

#[test]
fn the_client_never_writes_dont_change_inside_a_present_field() {
    // `time_how` has DONT_CHANGE = 0, and the outer optional has already said
    // "change it". A 0 there is a call that quietly does nothing.
    let fh = handle(8);
    for when in [
        SetTime::Server,
        SetTime::Client(Timespec { sec: 0, nsec: 0 }),
    ] {
        let args = encode_setattr(
            &fh,
            &SetAttr {
                mtime: Some(when),
                ..SetAttr::default()
            },
            None,
        );
        let mut dec = XdrDecoder::new(&args);
        dec.opaque().unwrap();
        for _ in 0..4 {
            assert!(!dec.bool().unwrap());
        }
        assert!(!dec.bool().unwrap(), "set_atime absent");
        assert!(dec.bool().unwrap(), "set_mtime present");
        assert_ne!(dec.u32().unwrap(), 0, "DONT_CHANGE must not be written");
    }
}

#[test]
fn mode_to_keeps_only_the_permission_bits() {
    assert_eq!(mode_to(0o100_644).mode, Some(0o644));
    assert_eq!(mode_to(0o104_755).mode, Some(0o4755), "setuid survives");
    assert_eq!(mode_to(0o644).uid, None, "a chmod does not chown");
    assert_eq!(mode_to(0o644).gid, None);
    assert_eq!(mode_to(0o644).size, None, "a chmod does not truncate");
}

#[test]
fn truncate_to_changes_the_size_and_nothing_else() {
    let attr = truncate_to(0);
    assert_eq!(attr.size, Some(0));
    assert_eq!(attr.mode, None);
    assert_eq!(attr.atime, None);
    assert_eq!(attr.mtime, None);
}

#[test]
fn the_setattr_guard_is_a_compare_and_set_on_the_ctime() {
    let fh = handle(16);
    let args = encode_setattr(
        &fh,
        &truncate_to(0),
        Some(Timespec {
            sec: 1700,
            nsec: 42,
        }),
    );
    let mut dec = XdrDecoder::new(&args);
    dec.opaque().unwrap();
    assert!(!dec.bool().unwrap()); // mode
    assert!(!dec.bool().unwrap()); // uid
    assert!(!dec.bool().unwrap()); // gid
    assert_eq!(dec.optional(|d| d.u64()).unwrap(), Some(0)); // size
    assert!(!dec.bool().unwrap()); // atime
    assert!(!dec.bool().unwrap()); // mtime
    assert!(dec.bool().unwrap(), "the guard is present");
    assert_eq!(dec.u32().unwrap(), 1700);
    assert_eq!(dec.u32().unwrap(), 42);
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn a_setattr_reply_hands_back_the_wcc_data_on_either_arm() {
    // The whole reason `wcc_data` exists is the case where the call did not
    // do what the client expected, so the failure arm carries it too and the
    // decoder must not throw it away: without it a writer has to invalidate
    // its cached pages after every failed call.
    let mut ok = XdrEncoder::new();
    ok.u32(0);
    put_wcc(&mut ok);
    assert_eq!(decode_setattr(&ok.into_bytes()).unwrap(), Ok(sample_wcc()));

    let mut ro = XdrEncoder::new();
    ro.u32(30); // NFS3ERR_ROFS
    put_wcc(&mut ro);
    assert_eq!(
        decode_setattr(&ro.into_bytes()).unwrap(),
        Err((Nfs3Status::RoFs, sample_wcc()))
    );
}

#[test]
fn a_guard_that_did_not_match_is_not_sync_and_not_an_io_error() {
    let mut enc = XdrEncoder::new();
    enc.u32(10002); // NFS3ERR_NOT_SYNC
    put_wcc_absent(&mut enc);
    assert_eq!(
        decode_setattr(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::NotSync, empty_wcc()))
    );
    assert_eq!(Nfs3Status::NotSync.to_lx_error(), LxError::EINVAL);
}

#[test]
fn a_setattr_reply_whose_wcc_data_is_truncated_is_an_error() {
    // The decoder must not report success on a reply it did not finish
    // reading: on a stream, the bytes it left behind are the next message.
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    enc.bool(true); // pre_op_attr present, and then nothing
    assert!(decode_setattr(&enc.into_bytes()).is_err());
}

// -------------------------------------------------------------------- WRITE

#[test]
fn write_sends_the_handle_offset_count_stability_and_data() {
    let fh = handle(32);
    let data = b"the quick brown fox";
    let args = encode_write(&fh, 0x1_0000_0000, StableHow::FileSync, data);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    assert_eq!(dec.u64().unwrap(), 0x1_0000_0000, "the offset is 64 bits");
    assert_eq!(dec.u32().unwrap(), data.len() as u32);
    assert_eq!(dec.u32().unwrap(), 2, "FILE_SYNC");
    assert_eq!(dec.opaque().unwrap(), data);
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn the_count_a_write_sends_always_matches_its_data() {
    // There is no way to make them disagree, because `count` is not an
    // argument. The test is here so that adding one is a test failure.
    let fh = handle(16);
    for len in [0usize, 1, 3, 4, 5, 512, 4096] {
        let data = vec![0xa5u8; len];
        let args = encode_write(&fh, 0, StableHow::Unstable, &data);
        let mut dec = XdrDecoder::new(&args);
        dec.opaque().unwrap();
        dec.u64().unwrap();
        let count = dec.u32().unwrap();
        dec.u32().unwrap();
        assert_eq!(count as usize, len);
        assert_eq!(dec.opaque().unwrap().len(), len);
    }
}

#[test]
fn every_stability_level_has_its_own_number() {
    let table: &[(StableHow, u32)] = &[
        (StableHow::Unstable, 0),
        (StableHow::DataSync, 1),
        (StableHow::FileSync, 2),
    ];
    for &(how, wire) in table {
        assert_eq!(how.to_wire(), wire, "{:?}", how);
        assert_eq!(StableHow::from_wire(wire), Some(how));
    }
    assert_eq!(StableHow::from_wire(3), None);
    assert_eq!(StableHow::from_wire(u32::MAX), None);
}

#[test]
fn only_an_unstable_write_needs_a_commit() {
    // The check a client is tempted to skip. Skipping it in the other
    // direction -- committing whatever the server answered -- costs a round
    // trip per write; skipping it the way that matters reports durability
    // that is not there.
    assert!(StableHow::Unstable.needs_commit());
    assert!(!StableHow::DataSync.needs_commit());
    assert!(!StableHow::FileSync.needs_commit());
}

/// A successful `WRITE` reply.
fn write_ok(count: u32, committed: u32, verf: &[u8; 8]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_wcc(&mut enc);
    enc.u32(count);
    enc.u32(committed);
    enc.opaque_fixed(verf);
    enc.into_bytes()
}

#[test]
fn a_write_reply_carries_the_count_the_stability_and_the_verifier() {
    let verf = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04];
    let got = decode_write(&write_ok(4096, 2, &verf)).unwrap().unwrap();
    assert_eq!(
        got,
        WriteReply {
            wcc: sample_wcc(),
            count: 4096,
            committed: StableHow::FileSync,
            verf,
        }
    );
}

#[test]
fn a_server_may_answer_with_more_durability_than_was_asked_for() {
    // An `Unstable` write answered `FILE_SYNC` is already durable. Believing
    // the request instead of the reply means a COMMIT per write for nothing.
    let got = decode_write(&write_ok(100, 2, &[0; 8])).unwrap().unwrap();
    assert_eq!(got.committed, StableHow::FileSync);
    assert!(!got.committed.needs_commit());
}

#[test]
fn a_short_write_is_not_an_error() {
    // The server took fewer bytes than were sent. The caller sends the rest
    // at the new offset; a client that read this as a failure would turn flow
    // control into an I/O error.
    let got = decode_write(&write_ok(10, 0, &[0; 8])).unwrap().unwrap();
    assert_eq!(got.count, 10);
    assert!(got.committed.needs_commit());
}

#[test]
fn a_write_reply_with_a_stability_no_server_sends_is_refused() {
    assert!(decode_write(&write_ok(1, 3, &[0; 8])).is_err());
}

#[test]
fn a_failed_write_is_a_status_and_its_wcc_data_is_consumed() {
    let mut enc = XdrEncoder::new();
    enc.u32(28); // NFS3ERR_NOSPC
    put_wcc(&mut enc);
    assert_eq!(
        decode_write(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::NoSpc, sample_wcc())),
        "a full disk still tells the client what the file looks like now"
    );
}

#[test]
fn a_write_verifier_is_eight_bytes_with_no_length_prefix() {
    // Fixed-length opaque, so there is no count word in front of it. Reading
    // it as variable-length would take the first four bytes of the verifier
    // as its length -- which for a real verifier is a length in the
    // gigabytes, and the decode would fail on a perfectly good reply.
    let verf = [1, 2, 3, 4, 5, 6, 7, 8];
    let bytes = write_ok(0, 0, &verf);
    let got = decode_write(&bytes).unwrap().unwrap();
    assert_eq!(got.verf, verf);
    // The whole reply ends exactly at the verifier.
    let mut dec = XdrDecoder::new(&bytes);
    dec.u32().unwrap();
    decode_wcc_data(&mut dec).unwrap();
    dec.u32().unwrap();
    dec.u32().unwrap();
    assert_eq!(dec.remaining(), VERF_SIZE);
}

// ------------------------------------------------------------------- COMMIT

#[test]
fn commit_sends_the_range_and_zero_means_to_the_end() {
    let fh = handle(32);
    let args = encode_commit(&fh, 0, 0);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    assert_eq!(dec.u64().unwrap(), 0);
    assert_eq!(dec.u32().unwrap(), 0, "zero is to the end of the file");
    assert_eq!(dec.remaining(), 0);

    let ranged = encode_commit(&fh, 4096, 8192);
    let mut dec = XdrDecoder::new(&ranged);
    dec.opaque().unwrap();
    assert_eq!(dec.u64().unwrap(), 4096);
    assert_eq!(dec.u32().unwrap(), 8192);
}

#[test]
fn a_commit_whose_verifier_changed_means_the_writes_are_gone() {
    // The whole point of the verifier, and the one failure a filesystem must
    // never have: reporting `fsync` success over data that no longer exists.
    let wrote = [0x11u8; 8];
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_wcc(&mut enc);
    enc.opaque_fixed(&[0x22u8; 8]);
    let committed = decode_commit(&enc.into_bytes()).unwrap().unwrap();
    assert_ne!(
        committed.verf, wrote,
        "a different verifier means the server restarted"
    );

    let mut same = XdrEncoder::new();
    same.u32(0);
    put_wcc_absent(&mut same);
    same.opaque_fixed(&wrote);
    assert_eq!(
        decode_commit(&same.into_bytes()).unwrap(),
        Ok(CommitReply {
            verf: wrote,
            wcc: empty_wcc(),
        })
    );
}

#[test]
fn a_failed_commit_is_a_status() {
    let mut enc = XdrEncoder::new();
    enc.u32(70); // NFS3ERR_STALE
    put_wcc_absent(&mut enc);
    assert_eq!(
        decode_commit(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::Stale, empty_wcc()))
    );
}

// ------------------------------------------------------------------- CREATE

#[test]
fn an_unchecked_create_sends_its_mode_as_a_sattr3() {
    let dir = handle(16);
    let args = encode_create(&dir, "notes.txt", CreateHow::Unchecked(mode_to(0o644))).unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"notes.txt");
    assert_eq!(dec.u32().unwrap(), 0, "UNCHECKED");
    assert_eq!(dec.optional(|d| d.u32()).unwrap(), Some(0o644));
    for _ in 0..5 {
        assert!(!dec.bool().unwrap());
    }
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn a_guarded_create_differs_from_unchecked_only_in_its_mode_word() {
    let dir = handle(16);
    let attr = mode_to(0o600);
    let unchecked = encode_create(&dir, "x", CreateHow::Unchecked(attr)).unwrap();
    let guarded = encode_create(&dir, "x", CreateHow::Guarded(attr)).unwrap();
    assert_eq!(unchecked.len(), guarded.len());
    assert_ne!(unchecked, guarded);
    let mut dec = XdrDecoder::new(&guarded);
    dec.opaque().unwrap();
    dec.string_bytes().unwrap();
    assert_eq!(dec.u32().unwrap(), 1, "GUARDED");
}

#[test]
fn an_exclusive_create_sends_a_verifier_and_no_attributes() {
    // The arm whose shape is different: eight fixed bytes where the other two
    // put six optionals. The verifier is what lets a server tell a retry of
    // this create from somebody else's file of the same name, so `O_EXCL`
    // survives a lost reply.
    let dir = handle(16);
    let verf = [9, 8, 7, 6, 5, 4, 3, 2];
    let args = encode_create(&dir, "lock", CreateHow::Exclusive(verf)).unwrap();
    let mut dec = XdrDecoder::new(&args);
    dec.opaque().unwrap();
    assert_eq!(dec.string_bytes().unwrap(), b"lock");
    assert_eq!(dec.u32().unwrap(), 2, "EXCLUSIVE");
    assert_eq!(dec.opaque_fixed(VERF_SIZE).unwrap(), &verf);
    assert_eq!(dec.remaining(), 0, "no sattr3 behind the verifier");
}

#[test]
fn a_retried_exclusive_create_sends_the_same_verifier() {
    // Which is the reason the verifier is a parameter and not generated
    // inside: the same `open` must send the same bytes every time, or the
    // server cannot recognise its own earlier work and `O_EXCL` fails on the
    // file the caller just created.
    let dir = handle(16);
    let verf = [1, 1, 2, 3, 5, 8, 13, 21];
    let first = encode_create(&dir, "lock", CreateHow::Exclusive(verf)).unwrap();
    let retry = encode_create(&dir, "lock", CreateHow::Exclusive(verf)).unwrap();
    assert_eq!(first, retry);
}

#[test]
fn a_create_refuses_a_name_that_is_not_one_component() {
    let dir = handle(16);
    let how = CreateHow::Unchecked(SetAttr::nothing());
    assert!(encode_create(&dir, "a/b", how).is_err());
    assert!(encode_create(&dir, "", how).is_err());
    assert!(encode_create(&dir, &"a".repeat(MAXNAMLEN + 1), how).is_err());
    assert!(encode_create(&dir, &"a".repeat(MAXNAMLEN), how).is_ok());
    assert!(encode_create(&dir, "..", how).is_ok());
}

/// A successful create-shaped reply.
fn create_ok(fh: Option<&[u8]>, attr: bool) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    match fh {
        Some(bytes) => {
            enc.bool(true);
            enc.opaque(bytes);
        }
        None => {
            enc.bool(false);
        }
    }
    if attr {
        put_fattr_present(&mut enc);
    } else {
        enc.bool(false);
    }
    put_wcc(&mut enc);
    enc.into_bytes()
}

#[test]
fn a_create_reply_carries_the_new_handle_and_attributes() {
    let new = handle(28);
    let got = decode_create(&create_ok(Some(new.as_bytes()), true))
        .unwrap()
        .unwrap();
    assert_eq!(got.fh, Some(new));
    assert_eq!(got.attr, Some(sample_fattr()));
    assert_eq!(got.dir_wcc, sample_wcc(), "the directory's wcc_data too");
}

#[test]
fn a_create_reply_without_a_handle_is_legal_and_not_an_error() {
    // `post_op_fh3` is optional. A client that unwrapped it would panic on a
    // reply the RFC allows, and the recovery is a LOOKUP, not a failure.
    let got = decode_create(&create_ok(None, false)).unwrap().unwrap();
    assert_eq!(got.fh, None);
    assert_eq!(got.attr, None);
    assert_eq!(got.dir_wcc, sample_wcc());
}

#[test]
fn a_create_handle_longer_than_the_protocol_allows_is_refused() {
    let over = vec![0xabu8; FHSIZE3 + 1];
    assert!(decode_create(&create_ok(Some(&over), false)).is_err());
    let at_limit = vec![0xabu8; FHSIZE3];
    assert!(decode_create(&create_ok(Some(&at_limit), false)).is_ok());
}

#[test]
fn a_failed_create_reads_only_the_wcc_data_and_hands_it_back() {
    // Two things at once. The failure arm has no handle and no attributes, so
    // a decoder that went looking for them would read the directory's
    // wcc_data as a handle; and the wcc_data is what tells the client whether
    // its cached listing of that directory is still good, which after an
    // `EEXIST` it very likely is not.
    let mut enc = XdrEncoder::new();
    enc.u32(17); // NFS3ERR_EXIST
    put_wcc(&mut enc);
    assert_eq!(
        decode_create(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::Exist, sample_wcc()))
    );
}

// ---------------------------------------------------- MKDIR, SYMLINK, MKNOD

#[test]
fn mkdir_sends_the_directory_the_name_and_the_attributes() {
    let dir = handle(16);
    let args = encode_mkdir(&dir, "sub", &mode_to(0o755)).unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"sub");
    assert_eq!(dec.optional(|d| d.u32()).unwrap(), Some(0o755));
    assert_eq!(dec.remaining(), 5 * 4, "the five unset sattr3 fields");
}

#[test]
fn a_symlink_target_is_bytes_and_may_hold_slashes() {
    // The name is one component and the target is a whole path: the slash
    // check applies to one and must not apply to the other.
    let dir = handle(16);
    let target = b"/srv/\xff\xfe/elsewhere";
    let args = encode_symlink(&dir, "link", &SetAttr::nothing(), target).unwrap();
    let mut dec = XdrDecoder::new(&args);
    dec.opaque().unwrap();
    assert_eq!(dec.string_bytes().unwrap(), b"link");
    for _ in 0..6 {
        assert!(!dec.bool().unwrap());
    }
    assert_eq!(dec.string_bytes().unwrap(), target);
    assert_eq!(dec.remaining(), 0);
    assert!(encode_symlink(&dir, "a/b", &SetAttr::nothing(), target).is_err());
}

#[test]
fn mknod_sends_a_specdata3_for_a_device_and_none_for_a_socket() {
    // The non-uniform union. Writing a zero `specdata3` for a socket would
    // leave the server two words out of step for the rest of the stream.
    let dir = handle(16);
    let dev = encode_mknod(&dir, "sda", NodeKind::Block(8, 16), &mode_to(0o660)).unwrap();
    let mut dec = XdrDecoder::new(&dev);
    dec.opaque().unwrap();
    assert_eq!(dec.string_bytes().unwrap(), b"sda");
    assert_eq!(dec.u32().unwrap(), 3, "NF3BLK");
    assert_eq!(dec.optional(|d| d.u32()).unwrap(), Some(0o660));
    for _ in 0..5 {
        assert!(!dec.bool().unwrap());
    }
    assert_eq!(dec.u32().unwrap(), 8, "major");
    assert_eq!(dec.u32().unwrap(), 16, "minor");
    assert_eq!(dec.remaining(), 0);

    // The same padded name length on purpose ("sda" and "soc" both pad to
    // four bytes), so the only difference left in the message is the
    // specdata3 the socket does not carry.
    let sock = encode_mknod(&dir, "soc", NodeKind::Socket, &mode_to(0o660)).unwrap();
    assert_eq!(
        sock.len(),
        dev.len() - 8,
        "a socket carries no specdata3: two words fewer and nothing else"
    );
    let mut dec = XdrDecoder::new(&sock);
    dec.opaque().unwrap();
    dec.string_bytes().unwrap();
    assert_eq!(dec.u32().unwrap(), 6, "NF3SOCK");
    dec.optional(|d| d.u32()).unwrap();
    for _ in 0..5 {
        dec.bool().unwrap();
    }
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn every_node_kind_has_the_ftype3_the_protocol_gives_it() {
    let dir = handle(8);
    let table: &[(NodeKind, u32)] = &[
        (NodeKind::Block(1, 2), 3),
        (NodeKind::Char(1, 2), 4),
        (NodeKind::Socket, 6),
        (NodeKind::Fifo, 7),
    ];
    for &(kind, ftype) in table {
        let args = encode_mknod(&dir, "n", kind, &SetAttr::nothing()).unwrap();
        let mut dec = XdrDecoder::new(&args);
        dec.opaque().unwrap();
        dec.string_bytes().unwrap();
        assert_eq!(dec.u32().unwrap(), ftype, "{:?}", kind);
    }
}

// ------------------------------------------------- REMOVE, RMDIR and RENAME

#[test]
fn remove_sends_a_dirop_and_refuses_a_path() {
    let dir = handle(16);
    let args = encode_remove(&dir, "gone").unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"gone");
    assert_eq!(dec.remaining(), 0);
    assert!(encode_remove(&dir, "a/b").is_err());
    assert!(encode_remove(&dir, "").is_err());
}

#[test]
fn an_rmdir_of_a_full_directory_is_not_empty_and_not_a_decode_error() {
    let mut enc = XdrEncoder::new();
    enc.u32(66); // NFS3ERR_NOTEMPTY
    put_wcc(&mut enc);
    assert_eq!(
        decode_remove(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::NotEmpty, sample_wcc()))
    );
    assert_eq!(Nfs3Status::NotEmpty.to_lx_error(), LxError::ENOTEMPTY);
}

#[test]
fn rename_sends_both_directories_and_both_names() {
    let from = handle(16);
    let to = handle(24);
    let args = encode_rename(&from, "old", &to, "new").unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), from.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"old");
    assert_eq!(dec.opaque().unwrap(), to.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"new");
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn rename_refuses_a_path_on_either_side() {
    let dir = handle(16);
    assert!(encode_rename(&dir, "a/b", &dir, "ok").is_err());
    assert!(encode_rename(&dir, "ok", &dir, "a/b").is_err());
    assert!(encode_rename(&dir, "", &dir, "ok").is_err());
    assert!(encode_rename(&dir, "ok", &dir, "").is_err());
    assert!(encode_rename(&dir, "ok", &dir, "fine").is_ok());
}

#[test]
fn a_rename_reply_consumes_two_wcc_data_not_one() {
    // The detail that bites on a stream transport: reading one leaves half a
    // reply in the buffer, and the leftover is parsed as part of the next
    // message rather than reported as an error on this one. The sentinel is
    // what measures it.
    let both = RenameWcc {
        from_dir: sample_wcc(),
        to_dir: empty_wcc(),
    };
    for status in [0u32, 18 /* NFS3ERR_XDEV */] {
        let mut enc = XdrEncoder::new();
        enc.u32(status);
        put_wcc(&mut enc);
        put_wcc_absent(&mut enc);
        enc.u32(0x5eed_1234);
        let bytes = enc.into_bytes();
        // The decoder must read both, keep them apart, and hand them back on
        // either arm...
        let expected = if status == 0 {
            Ok(both)
        } else {
            Err((Nfs3Status::XDev, both))
        };
        assert_eq!(decode_rename(&bytes).unwrap(), expected);
        // ...and must have left the sentinel, not eaten into it.
        let mut dec = XdrDecoder::new(&bytes);
        dec.u32().unwrap();
        decode_wcc_data(&mut dec).unwrap();
        decode_wcc_data(&mut dec).unwrap();
        assert_eq!(dec.u32().unwrap(), 0x5eed_1234);
    }
}

#[test]
fn a_rename_reply_with_only_one_wcc_data_is_an_error() {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_wcc_absent(&mut enc);
    assert!(decode_rename(&enc.into_bytes()).is_err());
}

// --------------------------------------------------------------------- LINK

#[test]
fn link_sends_the_file_first_and_then_where_to_put_it() {
    let file = handle(32);
    let dir = handle(16);
    let args = encode_link(&file, &dir, "alias").unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), file.as_bytes());
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"alias");
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn a_link_reply_carries_the_files_attributes_in_both_arms() {
    let mut ok = XdrEncoder::new();
    ok.u32(0);
    put_fattr_present(&mut ok);
    put_wcc(&mut ok);
    assert_eq!(
        decode_link(&ok.into_bytes()).unwrap(),
        Ok(LinkReply {
            attr: Some(sample_fattr()),
            dir_wcc: sample_wcc(),
        })
    );

    let mut bad = XdrEncoder::new();
    bad.u32(18); // NFS3ERR_XDEV, the classic LINK failure
    put_fattr_present(&mut bad);
    put_wcc_absent(&mut bad);
    assert_eq!(
        decode_link(&bad.into_bytes()).unwrap(),
        Err((
            Nfs3Status::XDev,
            LinkReply {
                attr: Some(sample_fattr()),
                dir_wcc: empty_wcc(),
            }
        ))
    );
}

#[test]
fn a_cross_device_link_is_exdev() {
    assert_eq!(Nfs3Status::XDev.to_lx_error(), LxError::EXDEV);
}
