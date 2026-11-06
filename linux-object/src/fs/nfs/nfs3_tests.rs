//! The NFSv3 types and the read path.
//!
//! The tests that matter here are not the happy paths. They are the four
//! places an NFSv3 decoder is usually wrong: the failure arm of a reply
//! carries fields too and has to be consumed, `post_op_attr` is optional and
//! absent is not an error, `eof` is the only end-of-file signal there is, and
//! the statuses above ten thousand are not errno.

use super::*;
use alloc::vec;

/// Append an `nfstime3`.
fn put_time(enc: &mut XdrEncoder, sec: u32, nsec: u32) {
    enc.u32(sec);
    enc.u32(nsec);
}

/// A `fattr3` with a different value in every field, so a decoder that read
/// two of them in the wrong order cannot pass.
///
/// `mode` deliberately has bits above the permission bits set
/// (`0o100644` is what a Linux server sends for a regular file) so that
/// [`Fattr3::to_metadata`] masking them off is actually measured, and `used`
/// is deliberately not a multiple of 512 so the block count has to round.
fn put_sample_fattr(enc: &mut XdrEncoder) {
    enc.u32(1); // NF3REG
    enc.u32(0o100_644); // mode, with the type bits a server also sets
    enc.u32(3); // nlink
    enc.u32(1000); // uid
    enc.u32(1001); // gid
    enc.u64(4097); // size
    enc.u64(2561); // used, in bytes and not a block multiple
    enc.u32(7); // rdev.major
    enc.u32(9); // rdev.minor
    enc.u64(0x1122_3344_5566_7788); // fsid
    enc.u64(0x0099_aabb_ccdd_eeff); // fileid
    put_time(enc, 111, 222); // atime
    put_time(enc, 333, 444); // mtime
    put_time(enc, 555, 666); // ctime
}

/// What [`put_sample_fattr`] decodes to.
fn sample_fattr() -> Fattr3 {
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

/// A `post_op_attr` that is present, holding [`sample_fattr`].
fn put_attr_present(enc: &mut XdrEncoder) {
    enc.bool(true);
    put_sample_fattr(enc);
}

/// A `post_op_attr` the server declined to send.
fn put_attr_absent(enc: &mut XdrEncoder) {
    enc.bool(false);
}

/// A filehandle of `len` distinguishable bytes.
fn handle(len: usize) -> FileHandle {
    let bytes: Vec<u8> = (0..len).map(|i| (i as u8) ^ 0x5a).collect();
    FileHandle::new(&bytes).unwrap()
}

// ---------------------------------------------------------------- the numbers

#[test]
fn the_program_and_procedure_numbers_are_the_protocols() {
    assert_eq!(PROGRAM_NFS, 100_003);
    assert_eq!(NFS_VERSION, 3);
    assert_eq!(PROC_GETATTR, 1);
    assert_eq!(PROC_LOOKUP, 3);
    assert_eq!(PROC_ACCESS, 4);
    assert_eq!(PROC_READLINK, 5);
    assert_eq!(PROC_READ, 6);
    assert_eq!(PROC_FSSTAT, 18);
    assert_eq!(PROC_FSINFO, 19);
    assert_eq!(MAXNAMLEN, 255);
    let t = target(PROC_READ);
    assert_eq!(t.program, PROGRAM_NFS);
    assert_eq!(t.version, 3);
    assert_eq!(t.procedure, PROC_READ);
}

#[test]
fn the_access_and_property_bits_are_the_protocols() {
    assert_eq!(access::READ, 0x0001);
    assert_eq!(access::LOOKUP, 0x0002);
    assert_eq!(access::MODIFY, 0x0004);
    assert_eq!(access::EXTEND, 0x0008);
    assert_eq!(access::DELETE, 0x0010);
    assert_eq!(access::EXECUTE, 0x0020);
    assert_eq!(fsf3::LINK, 0x0001);
    assert_eq!(fsf3::SYMLINK, 0x0002);
    assert_eq!(fsf3::HOMOGENEOUS, 0x0008);
    assert_eq!(fsf3::CANSETTIME, 0x0010);
}

// --------------------------------------------------------------- the statuses

#[test]
fn every_status_the_protocol_names_decodes_to_its_own_variant() {
    let table: &[(u32, Nfs3Status)] = &[
        (0, Nfs3Status::Ok),
        (1, Nfs3Status::Perm),
        (2, Nfs3Status::NoEnt),
        (5, Nfs3Status::Io),
        (6, Nfs3Status::NxIo),
        (13, Nfs3Status::Acces),
        (17, Nfs3Status::Exist),
        (18, Nfs3Status::XDev),
        (19, Nfs3Status::NoDev),
        (20, Nfs3Status::NotDir),
        (21, Nfs3Status::IsDir),
        (22, Nfs3Status::Inval),
        (27, Nfs3Status::FBig),
        (28, Nfs3Status::NoSpc),
        (30, Nfs3Status::RoFs),
        (31, Nfs3Status::MLink),
        (63, Nfs3Status::NameTooLong),
        (66, Nfs3Status::NotEmpty),
        (69, Nfs3Status::DQuot),
        (70, Nfs3Status::Stale),
        (71, Nfs3Status::Remote),
        (10001, Nfs3Status::BadHandle),
        (10002, Nfs3Status::NotSync),
        (10003, Nfs3Status::BadCookie),
        (10004, Nfs3Status::NotSupp),
        (10005, Nfs3Status::TooSmall),
        (10006, Nfs3Status::ServerFault),
        (10007, Nfs3Status::BadType),
        (10008, Nfs3Status::JukeBox),
    ];
    for &(wire, expected) in table {
        assert_eq!(Nfs3Status::from_wire(wire), expected, "status {}", wire);
    }
    assert_eq!(table.len(), 29, "a status was added without a test");
}

#[test]
fn a_status_this_client_does_not_know_is_kept_rather_than_guessed() {
    // 4 is EINTR locally and is not an `nfsstat3`; 10009 is past the last one
    // the RFC defines. Both have to survive as themselves: collapsing an
    // unknown status into a known one would make a server's new error look
    // like an old one.
    assert_eq!(Nfs3Status::from_wire(4), Nfs3Status::Unknown(4));
    assert_eq!(Nfs3Status::from_wire(10009), Nfs3Status::Unknown(10009));
    assert_eq!(
        Nfs3Status::from_wire(u32::MAX),
        Nfs3Status::Unknown(u32::MAX)
    );
    assert_eq!(Nfs3Status::Unknown(10009).to_lx_error(), LxError::EIO);
}

#[test]
fn only_jukebox_is_retried_rather_than_reported() {
    assert!(Nfs3Status::JukeBox.is_retryable());
    for s in [
        Nfs3Status::Ok,
        Nfs3Status::Io,
        Nfs3Status::Stale,
        Nfs3Status::ServerFault,
        Nfs3Status::NotSupp,
        Nfs3Status::Unknown(10009),
    ] {
        assert!(!s.is_retryable(), "{:?}", s);
    }
}

#[test]
fn the_statuses_above_ten_thousand_do_not_reach_userspace_as_numbers() {
    // The bug this guards: `nfsstat3` looks like errno for its first twenty
    // values, so a cast is tempting, and then `NFS3ERR_NOTSUPP` leaves the
    // kernel as errno 10004 -- a number no `errno` names and that `strerror`
    // prints as "Unknown error". Every one of these has to land on an errno
    // that exists.
    let mapped: &[(Nfs3Status, LxError)] = &[
        (Nfs3Status::BadHandle, LxError::ESTALE),
        (Nfs3Status::NotSync, LxError::EINVAL),
        (Nfs3Status::BadCookie, LxError::EINVAL),
        (Nfs3Status::NotSupp, LxError::EOPNOTSUPP),
        (Nfs3Status::TooSmall, LxError::EIO),
        (Nfs3Status::ServerFault, LxError::EIO),
        (Nfs3Status::BadType, LxError::EINVAL),
        (Nfs3Status::JukeBox, LxError::EAGAIN),
    ];
    for &(status, err) in mapped {
        assert_eq!(status.to_lx_error(), err, "{:?}", status);
        assert!(
            (err as isize) < 10_000,
            "{:?} maps to a number that is not an errno",
            status
        );
    }
}

#[test]
fn the_statuses_that_are_errno_map_to_the_same_one() {
    let table: &[(Nfs3Status, LxError)] = &[
        (Nfs3Status::Perm, LxError::EPERM),
        (Nfs3Status::NoEnt, LxError::ENOENT),
        (Nfs3Status::Io, LxError::EIO),
        (Nfs3Status::NxIo, LxError::ENXIO),
        (Nfs3Status::Acces, LxError::EACCES),
        (Nfs3Status::Exist, LxError::EEXIST),
        (Nfs3Status::XDev, LxError::EXDEV),
        (Nfs3Status::NoDev, LxError::ENODEV),
        (Nfs3Status::NotDir, LxError::ENOTDIR),
        (Nfs3Status::IsDir, LxError::EISDIR),
        (Nfs3Status::Inval, LxError::EINVAL),
        (Nfs3Status::FBig, LxError::EFBIG),
        (Nfs3Status::NoSpc, LxError::ENOSPC),
        (Nfs3Status::RoFs, LxError::EROFS),
        (Nfs3Status::MLink, LxError::EMLINK),
        (Nfs3Status::NameTooLong, LxError::ENAMETOOLONG),
        (Nfs3Status::NotEmpty, LxError::ENOTEMPTY),
        (Nfs3Status::DQuot, LxError::EDQUOT),
        (Nfs3Status::Remote, LxError::EREMOTE),
    ];
    for &(status, err) in table {
        assert_eq!(status.to_lx_error(), err, "{:?}", status);
    }
}

#[test]
fn a_stale_handle_is_estale_and_nothing_else() {
    // NFS's own error, and the reason `ESTALE` had to be added to the kernel's
    // errno table: a local inode cannot vanish from under an open file and an
    // NFS one can. Reporting `ENOENT` instead would make a program retry the
    // lookup, which is exactly the wrong move -- the *name* may still be
    // there, it is the handle that is dead.
    assert_eq!(Nfs3Status::Stale.to_lx_error(), LxError::ESTALE);
    assert_ne!(Nfs3Status::Stale.to_lx_error(), LxError::ENOENT);
    assert_eq!(LxError::ESTALE as isize, 116);
    assert_eq!(LxError::EDQUOT as isize, 122);
    assert_eq!(LxError::EREMOTE as isize, 66);
}

#[test]
fn ok_is_not_an_error_and_says_so() {
    assert_eq!(Nfs3Status::Ok.to_lx_error(), LxError::EUNDEF);
}

// ------------------------------------------------------------------- ftype3

#[test]
fn every_file_type_maps_to_its_vfs_kind() {
    let table: &[(u32, Ftype3, FileType)] = &[
        (1, Ftype3::Reg, FileType::File),
        (2, Ftype3::Dir, FileType::Dir),
        (3, Ftype3::Blk, FileType::BlockDevice),
        (4, Ftype3::Chr, FileType::CharDevice),
        (5, Ftype3::Lnk, FileType::SymLink),
        (6, Ftype3::Sock, FileType::Socket),
        (7, Ftype3::Fifo, FileType::NamedPipe),
    ];
    for &(wire, ftype, vfs) in table {
        assert_eq!(Ftype3::from_wire(wire), Some(ftype), "type {}", wire);
        assert_eq!(ftype.to_vfs(), vfs, "type {}", wire);
    }
}

#[test]
fn a_file_type_no_server_should_send_is_refused() {
    // Zero in particular: it is what an all-zero buffer decodes to, so
    // accepting it would make a reply of padding look like a valid object.
    assert_eq!(Ftype3::from_wire(0), None);
    assert_eq!(Ftype3::from_wire(8), None);
    assert_eq!(Ftype3::from_wire(u32::MAX), None);
}

// -------------------------------------------------------------------- fattr3

#[test]
fn a_fattr3_is_twenty_one_words_and_every_field_lands_where_it_belongs() {
    let mut enc = XdrEncoder::new();
    put_sample_fattr(&mut enc);
    let bytes = enc.into_bytes();
    assert_eq!(bytes.len(), 21 * 4, "a fattr3 is a fixed twenty-one words");

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(Fattr3::decode(&mut dec).unwrap(), sample_fattr());
    assert_eq!(dec.remaining(), 0, "the decoder read exactly the fattr3");
}

#[test]
fn a_fattr3_with_a_type_no_server_sends_is_refused_rather_than_guessed() {
    let mut enc = XdrEncoder::new();
    put_sample_fattr(&mut enc);
    let mut bytes = enc.into_bytes();
    bytes[3] = 0; // the type word, now zero
    let mut dec = XdrDecoder::new(&bytes);
    assert!(Fattr3::decode(&mut dec).is_err());
}

#[test]
fn a_truncated_fattr3_is_an_error_and_not_a_half_decoded_object() {
    let mut enc = XdrEncoder::new();
    put_sample_fattr(&mut enc);
    let bytes = enc.into_bytes();
    for cut in [0, 4, 20, 40, 80, 83] {
        let mut dec = XdrDecoder::new(&bytes[..cut]);
        assert!(
            matches!(
                Fattr3::decode(&mut dec),
                Err(RpcError::Xdr(XdrError::Truncated))
            ),
            "a fattr3 cut at {} bytes decoded",
            cut
        );
    }
}

#[test]
fn metadata_counts_blocks_in_five_hundred_twelve_byte_units() {
    // `du` agreeing with the server depends on this and on nothing else:
    // NFSv3 reports `used` in bytes and the VFS counts 512-byte blocks.
    let m = sample_fattr().to_metadata(32 * 1024);
    assert_eq!(m.blocks, 6, "2561 bytes is six 512-byte blocks, rounded up");
    let mut sparse = sample_fattr();
    sparse.size = 1 << 20;
    sparse.used = 0;
    assert_eq!(
        sparse.to_metadata(4096).blocks,
        0,
        "a hole occupies nothing"
    );
    assert_eq!(sparse.to_metadata(4096).size, 1 << 20);
}

#[test]
fn metadata_keeps_only_the_permission_bits() {
    // A Linux server sends `0o100644` for a regular file: the type bits are in
    // `mode` as well as in `type`, and letting them through would make the
    // VFS's `mode` disagree with its `type_`.
    let m = sample_fattr().to_metadata(4096);
    assert_eq!(m.mode, 0o644);
    assert_eq!(m.type_, FileType::File);

    let mut setuid = sample_fattr();
    setuid.mode = 0o104_755;
    assert_eq!(
        setuid.to_metadata(4096).mode,
        0o4755,
        "setuid is a permission bit and must survive"
    );
}

#[test]
fn metadata_takes_its_block_size_from_the_caller() {
    // `fattr3` carries no block size -- it is a property of the filesystem,
    // and `FSINFO` is where it comes from -- so the caller passes the
    // negotiated transfer size. A wrong answer here makes every buffered
    // reader in userspace do short reads.
    assert_eq!(sample_fattr().to_metadata(4096).blk_size, 4096);
    assert_eq!(sample_fattr().to_metadata(32 * 1024).blk_size, 32 * 1024);
}

#[test]
fn metadata_carries_the_rest_of_the_fields_across() {
    let m = sample_fattr().to_metadata(512);
    assert_eq!(m.size, 4097);
    assert_eq!(m.nlinks, 3);
    assert_eq!(m.uid, 1000);
    assert_eq!(m.gid, 1001);
    assert_eq!(m.inode, 0x0099_aabb_ccdd_eeff_usize);
    assert_eq!(m.dev, 0x1122_3344_5566_7788_usize);
    assert_eq!(m.rdev, (7 << 8) | 9);
    assert_eq!(
        m.atime,
        Timespec {
            sec: 111,
            nsec: 222
        }
    );
    assert_eq!(
        m.mtime,
        Timespec {
            sec: 333,
            nsec: 444
        }
    );
    assert_eq!(
        m.ctime,
        Timespec {
            sec: 555,
            nsec: 666
        }
    );
}

// -------------------------------------------------------------- post_op_attr

#[test]
fn a_server_that_declines_to_send_attributes_is_not_an_error() {
    // `post_op_attr` is optional on the wire. A client that treated the absent
    // form as a malformed reply would refuse to talk to a server that is
    // within the protocol.
    let mut enc = XdrEncoder::new();
    put_attr_absent(&mut enc);
    enc.u32(0xdead_beef); // whatever follows
    let bytes = enc.into_bytes();
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(decode_post_op_attr(&mut dec).unwrap(), None);
    assert_eq!(
        dec.u32().unwrap(),
        0xdead_beef,
        "the absent form is one word and the decoder must not skip the attrs"
    );
}

#[test]
fn attributes_that_are_present_are_decoded_and_consumed_whole() {
    let mut enc = XdrEncoder::new();
    put_attr_present(&mut enc);
    enc.u32(0xdead_beef);
    let bytes = enc.into_bytes();
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(decode_post_op_attr(&mut dec).unwrap(), Some(sample_fattr()));
    assert_eq!(dec.u32().unwrap(), 0xdead_beef);
}

#[test]
fn wcc_data_is_consumed_in_both_its_forms() {
    // Nothing in the read path reads `wcc_data`, but every modifying reply
    // carries it, and a decoder that stopped short of it would be however many
    // fields out of step for whatever follows. The sentinel is what measures
    // that.
    for present in [false, true] {
        let mut enc = XdrEncoder::new();
        enc.bool(present); // pre_op_attr
        if present {
            enc.u64(4097); // size
            put_time(&mut enc, 1, 2); // mtime
            put_time(&mut enc, 3, 4); // ctime
        }
        put_attr_present(&mut enc);
        enc.u32(0x1234_5678);
        let bytes = enc.into_bytes();
        let mut dec = XdrDecoder::new(&bytes);
        let wcc = decode_wcc_data(&mut dec).unwrap();
        assert_eq!(
            wcc.before,
            present.then_some(PreOpAttr {
                size: 4097,
                mtime: Timespec { sec: 1, nsec: 2 },
                ctime: Timespec { sec: 3, nsec: 4 },
            })
        );
        assert_eq!(wcc.after, Some(sample_fattr()));
        assert_eq!(
            dec.u32().unwrap(),
            0x1234_5678,
            "wcc_data with pre_op_attr {} was not consumed whole",
            present
        );
    }
}

#[test]
fn a_wcc_data_with_neither_half_is_two_booleans() {
    // The shortest legal form, and the one a client meets from a server that
    // sends no attributes at all.
    let mut enc = XdrEncoder::new();
    enc.bool(false);
    enc.bool(false);
    enc.u32(0xfeed_face);
    let bytes = enc.into_bytes();
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(
        decode_wcc_data(&mut dec).unwrap(),
        WccData {
            before: None,
            after: None
        }
    );
    assert_eq!(dec.u32().unwrap(), 0xfeed_face);
}

#[test]
fn the_before_half_of_a_wcc_data_is_what_a_cache_check_compares() {
    // Three fields and not a whole fattr3: size, mtime, ctime. Reading a
    // fattr3 there would take the post half's fields as the pre half's and
    // leave the decoder twenty-one words out of step.
    let mut enc = XdrEncoder::new();
    enc.bool(true);
    enc.u64(1 << 33); // a size over 32 bits
    put_time(&mut enc, 700, 800); // mtime
    put_time(&mut enc, 900, 1000); // ctime
    enc.bool(false); // no post half
    let bytes = enc.into_bytes();
    assert_eq!(bytes.len(), 4 + 8 + 16 + 4, "a pre_op_attr is five words");
    let mut dec = XdrDecoder::new(&bytes);
    let wcc = decode_wcc_data(&mut dec).unwrap();
    assert_eq!(
        wcc.before,
        Some(PreOpAttr {
            size: 1 << 33,
            mtime: Timespec {
                sec: 700,
                nsec: 800
            },
            ctime: Timespec {
                sec: 900,
                nsec: 1000
            },
        })
    );
    assert_eq!(wcc.after, None);
    assert_eq!(dec.remaining(), 0);
}

// ------------------------------------------------------------------- GETATTR

#[test]
fn a_filehandle_only_call_sends_just_the_handle() {
    let fh = handle(32);
    let args = encode_fh_only(&fh);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn a_getattr_reply_decodes_to_the_attributes() {
    let mut enc = XdrEncoder::new();
    enc.u32(0); // NFS3_OK
    put_sample_fattr(&mut enc);
    let got = decode_getattr(&enc.into_bytes()).unwrap();
    assert_eq!(got, Ok(sample_fattr()));
}

#[test]
fn a_getattr_failure_is_a_status_and_not_a_decode_error() {
    // The distinction the whole module keeps: a server that answered and
    // refused is `Ok(Err(status))`, a reply that did not parse is `Err(_)`.
    // Collapsing them would make a dead handle look like a broken connection,
    // and the connection would be torn down for nothing.
    let mut enc = XdrEncoder::new();
    enc.u32(70); // NFS3ERR_STALE, and nothing after it
    let got = decode_getattr(&enc.into_bytes()).unwrap();
    assert_eq!(got, Err(Nfs3Status::Stale));
}

#[test]
fn a_getattr_reply_that_ends_at_its_status_word_still_decodes() {
    // The failure arm of `GETATTR3res` is empty, so a four-byte reply is a
    // complete one.
    let bytes = vec![0, 0, 0, 2]; // NFS3ERR_NOENT
    assert_eq!(decode_getattr(&bytes).unwrap(), Err(Nfs3Status::NoEnt));
}

#[test]
fn a_getattr_that_says_ok_and_then_stops_is_a_decode_error() {
    let bytes = vec![0, 0, 0, 0]; // NFS3_OK with no attributes behind it
    assert!(decode_getattr(&bytes).is_err());
}

// -------------------------------------------------------------------- LOOKUP

#[test]
fn a_dir_op_sends_the_handle_and_then_the_name() {
    let dir = handle(16);
    let args = encode_dir_op(&dir, "notes.txt").unwrap();
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.string_bytes().unwrap(), b"notes.txt");
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn a_dir_op_refuses_a_name_with_a_slash_in_it() {
    // A `filename3` is one component. A server handed `a/b` would either
    // refuse it or -- worse -- resolve it, and then a path could escape the
    // directory the client believes it is looking in.
    let dir = handle(16);
    assert!(encode_dir_op(&dir, "a/b").is_err());
    assert!(encode_dir_op(&dir, "/etc").is_err());
    // `..` is one component and a legitimate name to look up; the server
    // resolves it. Only the separator itself is refused.
    assert!(encode_dir_op(&dir, "..").is_ok());
    assert!(encode_dir_op(&dir, "a..b").is_ok());
}

#[test]
fn a_dir_op_refuses_an_empty_name_and_one_over_the_protocols_limit() {
    let dir = handle(16);
    assert!(encode_dir_op(&dir, "").is_err());
    let at_limit = "a".repeat(MAXNAMLEN);
    assert!(encode_dir_op(&dir, &at_limit).is_ok());
    let over = "a".repeat(MAXNAMLEN + 1);
    assert!(encode_dir_op(&dir, &over).is_err());
}

#[test]
fn a_lookup_reply_carries_the_handle_and_both_sets_of_attributes() {
    let found = handle(28);
    let mut enc = XdrEncoder::new();
    enc.u32(0); // NFS3_OK
    enc.opaque(found.as_bytes());
    put_attr_present(&mut enc); // obj_attributes
    put_attr_absent(&mut enc); // dir_attributes, which a server may omit
    let got = decode_lookup(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(got.fh, found);
    assert_eq!(got.attr, Some(sample_fattr()));
    assert_eq!(got.dir_attr, None);
}

#[test]
fn a_failed_lookup_still_carries_the_directorys_attributes() {
    // The detail worth getting right: a `LOOKUP` that answers `NFS3ERR_NOENT`
    // still refreshes what the client knows about the directory, and that is
    // how a negative lookup updates the cache instead of leaving it stale.
    let mut enc = XdrEncoder::new();
    enc.u32(2); // NFS3ERR_NOENT
    put_attr_present(&mut enc);
    let (status, dir_attr) = decode_lookup(&enc.into_bytes()).unwrap().unwrap_err();
    assert_eq!(status, Nfs3Status::NoEnt);
    assert_eq!(dir_attr, Some(sample_fattr()));
}

#[test]
fn a_lookup_handle_longer_than_the_protocol_allows_is_refused() {
    // 64 bytes is `NFS3_FHSIZE`. A longer one is either a server bug or a
    // reply that is not the shape this decoder thinks, and in both cases
    // keeping it would mean holding a handle no call can send.
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    enc.opaque(&[0xab; FHSIZE3 + 1]);
    put_attr_absent(&mut enc);
    put_attr_absent(&mut enc);
    assert!(decode_lookup(&enc.into_bytes()).is_err());

    let mut ok = XdrEncoder::new();
    ok.u32(0);
    ok.opaque(&[0xab; FHSIZE3]);
    put_attr_absent(&mut ok);
    put_attr_absent(&mut ok);
    assert!(
        decode_lookup(&ok.into_bytes()).is_ok(),
        "the limit itself is fine"
    );
}

// -------------------------------------------------------------------- ACCESS

#[test]
fn access_asks_with_a_mask() {
    let fh = handle(32);
    let args = encode_access(&fh, access::READ | access::EXECUTE);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    assert_eq!(dec.u32().unwrap(), 0x21);
    assert_eq!(dec.remaining(), 0);
}

#[test]
fn access_is_answered_with_what_was_granted_which_may_be_less() {
    let mut enc = XdrEncoder::new();
    enc.u32(0); // NFS3_OK
    put_attr_present(&mut enc);
    enc.u32(access::READ); // asked for read and execute, granted read
    let (granted, attr) = decode_access(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(granted, access::READ);
    assert_eq!(attr, Some(sample_fattr()));
}

#[test]
fn an_access_reply_granting_nothing_is_a_complete_answer() {
    // `ACCESS` answers "of these, which may you", so an empty mask means "none
    // of them" and is not a failure. A client that read it as one would refuse
    // operations the server permits -- and would do so for the *file*, since
    // the call itself succeeded.
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_attr_absent(&mut enc);
    enc.u32(0);
    let bytes = enc.into_bytes();
    assert_eq!(decode_access(&bytes).unwrap(), Ok((0, None)));
    assert!(
        decode_access(&bytes).unwrap().is_ok(),
        "granting nothing is not a failure"
    );
}

#[test]
fn a_failed_access_still_carries_the_attributes_it_was_refused_with() {
    // Both arms of `ACCESS3res` carry `obj_attributes`, so the attributes are
    // read before the branch and handed back either way. Dropping them on the
    // failure arm is what the first version of this did, and it threw away a
    // cache refresh on exactly the call that says the client's idea of the
    // file's mode is out of date.
    let mut enc = XdrEncoder::new();
    enc.u32(13); // NFS3ERR_ACCES
    put_attr_present(&mut enc);
    assert_eq!(
        decode_access(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::Acces, Some(sample_fattr())))
    );
}

// ------------------------------------------------------------------ READLINK

#[test]
fn a_link_target_that_is_not_utf8_survives_being_read() {
    // A path on the server is bytes. This client does not get to decide that a
    // link whose target is not UTF-8 does not exist.
    let target = b"/srv/\xff\xfe/here";
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_attr_present(&mut enc);
    enc.opaque(target);
    let (bytes, attr) = decode_readlink(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(bytes, target.to_vec());
    assert_eq!(attr, Some(sample_fattr()));
}

#[test]
fn a_failed_readlink_is_a_status_and_the_attributes_behind_it() {
    let mut enc = XdrEncoder::new();
    enc.u32(22); // NFS3ERR_INVAL, which is what a non-link gets
    put_attr_present(&mut enc);
    assert_eq!(
        decode_readlink(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::Inval, Some(sample_fattr()))),
        "the failure arm carries attributes too"
    );
}

#[test]
fn the_vfs_gets_a_lossy_name_rather_than_no_link_at_all() {
    assert_eq!(link_target_to_string(b"/srv/media"), "/srv/media");
    let lossy = link_target_to_string(b"/srv/\xff");
    assert!(lossy.starts_with("/srv/"));
    assert!(
        lossy.contains('\u{fffd}'),
        "the bad byte became a replacement"
    );
}

// ---------------------------------------------------------------------- READ

#[test]
fn read_sends_the_handle_the_offset_and_the_count() {
    let fh = handle(32);
    let args = encode_read(&fh, 0x1_0000_0000, 32768);
    let mut dec = XdrDecoder::new(&args);
    assert_eq!(dec.opaque().unwrap(), fh.as_bytes());
    assert_eq!(dec.u64().unwrap(), 0x1_0000_0000, "the offset is 64 bits");
    assert_eq!(dec.u32().unwrap(), 32768);
    assert_eq!(dec.remaining(), 0);
}

/// Build a successful `READ` reply.
fn read_ok(data: &[u8], count: u32, eof: bool) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0); // NFS3_OK
    put_attr_present(&mut enc);
    enc.u32(count);
    enc.bool(eof);
    enc.opaque(data);
    enc.into_bytes()
}

#[test]
fn a_read_reply_carries_the_data_the_flag_and_the_attributes() {
    let data = b"the quick brown fox";
    let got = decode_read(&read_ok(data, data.len() as u32, true))
        .unwrap()
        .unwrap();
    assert_eq!(got.data, data.to_vec());
    assert!(got.eof);
    assert_eq!(got.attr, Some(sample_fattr()));
}

#[test]
fn a_read_shorter_than_asked_for_is_not_the_end_of_the_file() {
    // `eof` is the only end-of-file signal there is. A server may return fewer
    // bytes than asked for at any time, and a client that read a short reply
    // as end-of-file truncates files silently -- which is the worst kind of
    // bug this layer can have, because nothing reports it.
    let data = [7u8; 100];
    let got = decode_read(&read_ok(&data, 100, false)).unwrap().unwrap();
    assert_eq!(got.data.len(), 100);
    assert!(!got.eof, "a short read did not claim to be the end");
}

#[test]
fn a_read_of_nothing_at_the_end_of_the_file_decodes() {
    let got = decode_read(&read_ok(&[], 0, true)).unwrap().unwrap();
    assert!(got.data.is_empty());
    assert!(got.eof);
}

#[test]
fn a_read_whose_count_contradicts_its_data_is_refused() {
    // The server stated one length and sent another. Believing `count` would
    // hand a caller a length the data does not support; believing the data
    // would ignore a sign the reply is not the shape it looks like. Refusing
    // is the only safe answer, in both directions.
    let data = [1u8; 64];
    assert!(
        decode_read(&read_ok(&data, 65, false)).is_err(),
        "count too high"
    );
    assert!(
        decode_read(&read_ok(&data, 63, false)).is_err(),
        "count too low"
    );
    assert!(
        decode_read(&read_ok(&data, 0, false)).is_err(),
        "count zero"
    );
    assert!(
        decode_read(&read_ok(&data, 64, false)).is_ok(),
        "the agreeing case still works"
    );
}

#[test]
fn a_failed_read_carries_the_attributes_it_was_sent_with() {
    let mut enc = XdrEncoder::new();
    enc.u32(10008); // NFS3ERR_JUKEBOX
    put_attr_present(&mut enc);
    let (status, attr) = decode_read(&enc.into_bytes()).unwrap().unwrap_err();
    assert_eq!(status, Nfs3Status::JukeBox);
    assert!(status.is_retryable());
    assert_eq!(attr, Some(sample_fattr()));
}

// -------------------------------------------------------------------- FSSTAT

#[test]
fn fsstat_decodes_its_six_counters_and_the_validity_window() {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_attr_absent(&mut enc);
    enc.u64(1 << 40); // tbytes
    enc.u64(1 << 39); // fbytes
    enc.u64(1 << 38); // abytes
    enc.u64(1_000_000); // tfiles
    enc.u64(900_000); // ffiles
    enc.u64(800_000); // afiles
    enc.u32(30); // invarsec
    let (st, attr) = decode_fsstat(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(attr, None);
    assert_eq!(
        st,
        FsStat {
            tbytes: 1 << 40,
            fbytes: 1 << 39,
            abytes: 1 << 38,
            tfiles: 1_000_000,
            ffiles: 900_000,
            afiles: 800_000,
            invarsec: 30,
        }
    );
}

#[test]
fn a_failed_fsstat_is_a_status_and_the_attributes_behind_it() {
    let mut enc = XdrEncoder::new();
    enc.u32(10006); // NFS3ERR_SERVERFAULT
    put_attr_present(&mut enc);
    assert_eq!(
        decode_fsstat(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::ServerFault, Some(sample_fattr())))
    );
}

// -------------------------------------------------------------------- FSINFO

/// Build a successful `FSINFO` reply from the ten fields, in order.
#[allow(clippy::too_many_arguments)]
fn fsinfo_ok(info: &FsInfo) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(0);
    put_attr_absent(&mut enc);
    enc.u32(info.rtmax);
    enc.u32(info.rtpref);
    enc.u32(info.rtmult);
    enc.u32(info.wtmax);
    enc.u32(info.wtpref);
    enc.u32(info.wtmult);
    enc.u32(info.dtpref);
    enc.u64(info.maxfilesize);
    enc.u32(info.time_delta.sec as u32);
    enc.u32(info.time_delta.nsec as u32);
    enc.u32(info.properties);
    enc.into_bytes()
}

/// Roughly what a Linux `nfsd` reports, with **every field a different value**.
///
/// A real server sends the same number for `rtmax` and `wtmax`, and for
/// `rtmult` and `wtmult`. Using its numbers here hid a swap between the read
/// and the write triple: the mutation sweep killed nothing, because both sides
/// of the swap decoded to the same value.
fn sample_fsinfo() -> FsInfo {
    FsInfo {
        rtmax: 1 << 20,
        rtpref: 1 << 19,
        rtmult: 4096,
        wtmax: 1 << 18,
        wtpref: 1 << 17,
        wtmult: 2048,
        dtpref: 32768,
        maxfilesize: u64::MAX - 1,
        time_delta: Timespec { sec: 0, nsec: 1 },
        properties: fsf3::LINK | fsf3::SYMLINK | fsf3::HOMOGENEOUS | fsf3::CANSETTIME,
    }
}

#[test]
fn fsinfo_decodes_in_order_and_ends_with_the_properties() {
    // Ten fields of which six are the same width and mean different things:
    // the order is the whole test, and distinct values in every field are what
    // makes a swap fail.
    let expected = sample_fsinfo();
    let (got, attr) = decode_fsinfo(&fsinfo_ok(&expected)).unwrap().unwrap();
    assert_eq!(got, expected);
    assert_eq!(attr, None);
    assert_eq!(got.properties & fsf3::SYMLINK, fsf3::SYMLINK);
}

#[test]
fn a_failed_fsinfo_is_a_status_and_the_attributes_behind_it() {
    let mut enc = XdrEncoder::new();
    enc.u32(70); // NFS3ERR_STALE
    put_attr_present(&mut enc);
    assert_eq!(
        decode_fsinfo(&enc.into_bytes()).unwrap(),
        Err((Nfs3Status::Stale, Some(sample_fattr())))
    );
}

// --------------------------------------------------------------- negotiation

#[test]
fn negotiation_never_exceeds_what_the_server_will_serve() {
    let info = FsInfo {
        rtmax: 32768,
        rtpref: 32768,
        rtmult: 4096,
        ..sample_fsinfo()
    };
    assert_eq!(info.read_size(1 << 20), 32768, "capped at rtmax");
    assert_eq!(info.read_size(8192), 8192, "a smaller ask is left alone");
}

#[test]
fn negotiation_takes_the_smaller_of_the_maximum_and_the_preference() {
    let info = FsInfo {
        rtmax: 1 << 20,
        rtpref: 65536,
        rtmult: 1,
        ..sample_fsinfo()
    };
    assert_eq!(info.read_size(1 << 20), 65536);
}

#[test]
fn negotiation_rounds_down_to_the_servers_multiple() {
    // Down, not up. `rtmax` is the bound the server enforces, so a client that
    // rounded up would have its largest reads refused -- and would only find
    // out on a full-size read, long after the mount looked fine.
    assert_eq!(negotiate(5000, 1 << 20, 0, 4096), 4096);
    assert_eq!(negotiate(32768, 32768, 32768, 4096), 32768);
    assert_eq!(negotiate(32769, 1 << 20, 0, 4096), 32768);
}

#[test]
fn a_server_that_states_no_limit_leaves_the_clients_size_alone() {
    // Zero is "unstated", not "zero bytes". Treating it as the limit would
    // negotiate every transfer down to nothing and the mount would read no
    // data at all.
    assert_eq!(negotiate(8192, 0, 0, 0), 8192);
    assert_eq!(negotiate(8192, 0, 4096, 0), 4096, "only rtpref stated");
    assert_eq!(negotiate(8192, 4096, 0, 0), 4096, "only rtmax stated");
}

#[test]
fn negotiation_never_returns_zero() {
    // Whatever the server said and whatever was asked for, a transfer size of
    // zero is a read loop that makes no progress.
    assert_eq!(negotiate(0, 1 << 20, 1 << 20, 4096), 1);
    assert_eq!(negotiate(100, 0, 0, 4096), 100, "below the multiple");
    assert_eq!(negotiate(1, 1, 1, 512), 1);
    for wanted in [0, 1, 511, 512, 4095, 4096, 1 << 20] {
        for mult in [0, 1, 512, 4096] {
            assert!(
                negotiate(wanted, 1 << 20, 0, mult) > 0,
                "wanted {} mult {} negotiated to zero",
                wanted,
                mult
            );
        }
    }
}

#[test]
fn reads_and_writes_negotiate_against_their_own_numbers() {
    // The one mistake this catches: `write_size` reading the READ fields.
    let info = FsInfo {
        rtmax: 65536,
        rtpref: 65536,
        rtmult: 1,
        wtmax: 8192,
        wtpref: 8192,
        wtmult: 1,
        ..sample_fsinfo()
    };
    assert_eq!(info.read_size(1 << 20), 65536);
    assert_eq!(info.write_size(1 << 20), 8192);
}
