//! `READDIR` and `READDIRPLUS`.
//!
//! The shapes worth testing here are the ones with no counterpart elsewhere
//! in the protocol:
//!
//! - the entry list is a chain of `(bool, entry)` pairs ending in a `false`
//!   and then the `eof` flag, so there is no count to check against;
//! - an empty list with `eof` false is legal and means "ask again bigger",
//!   which is the one reading that silently truncates directories;
//! - `entryplus3` puts the attributes before the handle, and both are
//!   optional;
//! - the `cookieverf3` is eight raw bytes with no length prefix, and all
//!   zeros is a value, not an absence.

use super::super::nfs3::MAXNAMLEN;
use super::*;
use crate::error::LxError;

/// A handle of `len` bytes, each one different from its neighbours so a test
/// that compares them cannot pass on a slice of the wrong part.
fn handle(len: usize) -> FileHandle {
    let bytes: Vec<u8> = (0..len)
        .map(|i| (i as u8).wrapping_mul(7).wrapping_add(3))
        .collect();
    FileHandle::new(&bytes).unwrap()
}

/// A present `post_op_attr`. Every field a distinct value, because a repeated
/// one makes a swap between two fields invisible — the lesson the `FSINFO`
/// fixture taught in the read-path batch.
fn put_fattr_present(enc: &mut XdrEncoder) {
    enc.bool(true);
    enc.u32(2); // NF3DIR
    enc.u32(0o040_755);
    enc.u32(5); // nlink
    enc.u32(1000); // uid
    enc.u32(1001); // gid
    enc.u64(4096); // size
    enc.u64(8); // used
    enc.u32(11); // rdev.major
    enc.u32(13); // rdev.minor
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
    use rcore_fs::vfs::Timespec;
    Fattr3 {
        ftype: Ftype3::Dir,
        mode: 0o040_755,
        nlink: 5,
        uid: 1000,
        gid: 1001,
        size: 4096,
        used: 8,
        rdev: (11, 13),
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

/// A `cookieverf3` whose bytes are not all the same, so a test cannot pass
/// on a misread slice of it.
const VERF: CookieVerf = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04];

/// Append one `entry3`, or one `entryplus3` when `attr`/`fh` are given.
fn put_entry(
    enc: &mut XdrEncoder,
    fileid: u64,
    name: &str,
    cookie: u64,
    plus: Option<(bool, Option<&FileHandle>)>,
) {
    enc.bool(true); // an entry follows
    enc.u64(fileid);
    enc.string(name);
    enc.u64(cookie);
    if let Some((with_attr, fh)) = plus {
        if with_attr {
            put_fattr_present(enc);
        } else {
            enc.bool(false);
        }
        match fh {
            Some(h) => {
                enc.bool(true);
                enc.opaque(h.as_bytes());
            }
            None => {
                enc.bool(false);
            }
        }
    }
}

/// The head of a successful reply: status, the directory's attributes, the
/// verifier.
fn put_ok_head(enc: &mut XdrEncoder, verf: &CookieVerf) {
    enc.u32(0); // NFS3_OK
    put_fattr_present(enc); // dir_attributes
    enc.opaque_fixed(verf);
}

/// Close an entry list: the terminating `false` and the `eof` flag.
fn put_tail(enc: &mut XdrEncoder, eof: bool) {
    enc.bool(false); // no more entries
    enc.bool(eof);
}

/// A plain `READDIR` reply holding the three entries every test wants.
fn readdir_ok(eof: bool) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 2, ".", 1, None);
    put_entry(&mut enc, 7, "..", 2, None);
    put_entry(&mut enc, 4097, "informe.txt", 0x7fff_0000_0000_0003, None);
    put_tail(&mut enc, eof);
    enc.into_bytes()
}

#[test]
fn the_procedure_numbers_are_the_ones_the_protocol_assigns() {
    // Verified against RFC 1813 section 3 and Linux's uapi/linux/nfs3.h, not
    // from memory: an off-by-one here reaches a real server as a different
    // procedure, and 16 and 17 are each other's neighbours.
    assert_eq!(PROC_READDIR, 16);
    assert_eq!(PROC_READDIRPLUS, 17);
    assert_eq!(COOKIEVERF_SIZE, 8);
}

#[test]
fn a_first_call_sends_a_zero_cookie_and_a_zero_verifier() {
    // RFC 1813: both are zero on the first request. The verifier's zero is a
    // real eight-byte value on the wire, not an omission.
    assert_eq!(COOKIE_START, 0);
    assert_eq!(VERF_START, [0u8; 8]);
}

#[test]
fn readdir_sends_the_handle_then_the_cookie_then_the_verifier_then_the_count() {
    let dir = handle(32);
    let bytes = encode_readdir(&dir, 0x0102_0304_0506_0708, &VERF, 8192);

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.u64().unwrap(), 0x0102_0304_0506_0708);
    assert_eq!(dec.opaque_fixed(8).unwrap(), &VERF);
    assert_eq!(dec.u32().unwrap(), 8192);
    assert!(dec.is_empty(), "and nothing after it");
}

#[test]
fn the_verifier_goes_out_as_eight_raw_bytes_with_no_length_prefix() {
    // A `cookieverf3` is `opaque[8]`, a fixed array. Writing it as a
    // variable-length opaque would prepend a length word and put every later
    // field four bytes out of place.
    let short = encode_readdir(&handle(4), 0, &VERF, 1);
    let long = encode_readdir(&handle(4), 0, &[0; 8], 1);
    assert_eq!(
        short.len(),
        long.len(),
        "the verifier's contents cannot change the length"
    );
    // handle(4): 4 length + 4 data. cookie 8. verf 8. count 4.
    assert_eq!(short.len(), 8 + 8 + 8 + 4);
}

#[test]
fn readdirplus_sends_two_different_counts_in_order() {
    // dircount excludes the attributes and handles, maxcount includes them,
    // and the server sizes its own directory read from the first. Sending
    // them swapped asks the server to read far more than the client wants and
    // then truncate the answer to far less.
    let dir = handle(16);
    let bytes = encode_readdirplus(&dir, 42, &VERF, 1024, 32768);

    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque().unwrap(), dir.as_bytes());
    assert_eq!(dec.u64().unwrap(), 42);
    assert_eq!(dec.opaque_fixed(8).unwrap(), &VERF);
    assert_eq!(dec.u32().unwrap(), 1024, "dircount first");
    assert_eq!(dec.u32().unwrap(), 32768, "maxcount second");
    assert!(dec.is_empty());
}

#[test]
fn readdirplus_args_are_one_word_longer_than_readdir_args() {
    // The only difference in the argument shape is the extra count. A
    // encoder that forgot it would leave the server reading maxcount off the
    // end of the call.
    let dir = handle(20);
    let plain = encode_readdir(&dir, 1, &VERF, 100);
    let plus = encode_readdirplus(&dir, 1, &VERF, 100, 200);
    assert_eq!(plus.len(), plain.len() + 4);
}

#[test]
fn an_entry_list_is_a_chain_of_booleans_and_not_a_counted_array() {
    // Three entries, each introduced by its own `true`, then a `false` and
    // the eof flag. Nothing on the wire says "three".
    let got = decode_readdir(&readdir_ok(true)).unwrap().unwrap();
    assert_eq!(got.list.entries.len(), 3);
    assert_eq!(got.list.entries[0].name, ".");
    assert_eq!(got.list.entries[1].name, "..");
    assert_eq!(got.list.entries[2].name, "informe.txt");
    assert_eq!(got.list.entries[2].fileid, 4097);
    assert_eq!(got.list.entries[2].cookie, 0x7fff_0000_0000_0003);
    assert!(got.list.eof);
}

#[test]
fn a_successful_reply_carries_the_directorys_attributes_and_the_verifier() {
    let got = decode_readdir(&readdir_ok(true)).unwrap().unwrap();
    assert_eq!(got.dir_attr, Some(sample_fattr()));
    assert_eq!(got.verf, VERF, "to be sent back with any of these cookies");
}

#[test]
fn a_plain_readdir_entry_carries_no_attributes_and_no_handle() {
    // `entry3` is fileid, name, cookie and nothing else. Linux's own comment
    // above `nfs3_decode_dirent` lists a filehandle and attributes in
    // `entry3`; the comment is wrong and the code beneath it is right (it
    // reads them only when `plus`). RFC 1813 section 3.3.16 is the authority.
    // A decoder that believed the comment would read the *next* entry's
    // boolean and fileid as this entry's attributes.
    let got = decode_readdir(&readdir_ok(true)).unwrap().unwrap();
    for e in &got.list.entries {
        assert_eq!(e.attr, None);
        assert_eq!(e.fh, None);
    }
}

#[test]
fn an_empty_list_with_eof_is_an_empty_directory() {
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_tail(&mut enc, true);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();

    assert!(got.list.entries.is_empty());
    assert!(got.list.eof);
    assert!(
        !got.list.needs_a_bigger_count(),
        "nothing fit because there is nothing"
    );
    assert_eq!(got.list.next_cookie(), None);
}

#[test]
fn an_empty_list_without_eof_means_ask_again_bigger_and_not_end_of_directory() {
    // The trap this module exists to not fall into. A server whose `count`
    // was too small for one entry answers with no entries and eof false, and
    // a client that read that as the end of the directory would return a
    // prefix of every large directory with no error anywhere.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_tail(&mut enc, false);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();

    assert!(got.list.entries.is_empty());
    assert!(!got.list.eof, "the directory is NOT finished");
    assert!(got.list.needs_a_bigger_count());
    assert_eq!(
        got.list.next_cookie(),
        None,
        "there is no new cookie: the caller re-sends the one it had"
    );
}

#[test]
fn entries_without_eof_hand_back_the_last_cookie_to_resume_from() {
    // A cookie3 names the position *after* its entry, so resuming uses the
    // last one. Using the first, or the count, repeats or skips entries.
    let got = decode_readdir(&readdir_ok(false)).unwrap().unwrap();
    assert!(!got.list.eof);
    assert_eq!(got.list.next_cookie(), Some(0x7fff_0000_0000_0003));
}

#[test]
fn the_end_of_the_directory_has_no_next_cookie_even_with_entries() {
    // eof with entries is the normal last page. Asking again from the last
    // cookie would be one wasted round trip per listing, every listing.
    let got = decode_readdir(&readdir_ok(true)).unwrap().unwrap();
    assert!(got.list.eof);
    assert_eq!(got.list.next_cookie(), None);
}

#[test]
fn a_verifier_of_all_zeros_is_a_value_and_not_an_absence() {
    // RFC 1813: a server whose cookies are always valid may use zero as its
    // verifier forever. A client that read all-zeros as "the server sent none"
    // and substituted something would get BAD_COOKIE from exactly the servers
    // that never have stale cookies.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &[0; 8]);
    put_entry(&mut enc, 9, "a", 1, None);
    put_tail(&mut enc, true);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();

    assert_eq!(got.verf, [0u8; 8]);
    assert_eq!(got.verf, VERF_START, "and it is what a first call sends");
}

#[test]
fn dot_and_dotdot_are_ordinary_entries_that_can_be_told_apart() {
    let got = decode_readdir(&readdir_ok(true)).unwrap().unwrap();
    assert!(got.list.entries[0].is_dot_or_dotdot());
    assert!(got.list.entries[1].is_dot_or_dotdot());
    assert!(!got.list.entries[2].is_dot_or_dotdot());
}

#[test]
fn a_hidden_file_is_not_dot_or_dotdot() {
    // The two names, not the leading dot. A check written as "starts with a
    // dot" would make the caller skip every hidden file in the directory,
    // which is the one wrong answer that looks like a working `ls` -- a plain
    // `ls` does not show them either, so only `ls -a` reveals the loss.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 1, ".perfil", 1, None);
    put_entry(&mut enc, 2, "..copia", 2, None);
    put_entry(&mut enc, 3, "...", 3, None);
    put_tail(&mut enc, true);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();

    for e in &got.list.entries {
        assert!(
            !e.is_dot_or_dotdot(),
            "{:?} is a name of its own, not a directory link",
            e.name
        );
    }
}

#[test]
fn a_failed_readdir_still_hands_back_the_directorys_attributes() {
    // READDIR3resfail carries a post_op_attr. For BAD_COOKIE in particular it
    // is how the client learns the directory changed under its cookie.
    let mut enc = XdrEncoder::new();
    enc.u32(10003); // NFS3ERR_BAD_COOKIE
    put_fattr_present(&mut enc);
    let (status, attr) = decode_readdir(&enc.into_bytes()).unwrap().unwrap_err();

    assert_eq!(status, Nfs3Status::BadCookie);
    assert_eq!(attr, Some(sample_fattr()), "refreshed anyway");
}

#[test]
fn a_failed_readdir_reads_only_the_attributes_and_not_a_verifier() {
    // The failure arm is a post_op_attr and stops. A decoder that went on to
    // read the verifier would need eight bytes that are not there, so the
    // bug shows up as a truncated reply rather than as a wrong answer.
    let mut enc = XdrEncoder::new();
    enc.u32(70); // NFS3ERR_STALE
    enc.bool(false); // no attributes either
    let (status, attr) = decode_readdir(&enc.into_bytes()).unwrap().unwrap_err();

    assert_eq!(status, Nfs3Status::Stale);
    assert_eq!(attr, None);
    assert_eq!(status.to_lx_error(), LxError::ESTALE);
}

#[test]
fn a_stale_cookie_is_a_restart_and_a_small_count_is_a_retry() {
    // Neither is an I/O error to report upward: one means start the listing
    // over, the other means ask again with a bigger count.
    assert!(cookie_is_stale(Nfs3Status::BadCookie));
    assert!(!cookie_is_stale(Nfs3Status::TooSmall));
    assert!(count_was_too_small(Nfs3Status::TooSmall));
    assert!(!count_was_too_small(Nfs3Status::BadCookie));
    assert!(!cookie_is_stale(Nfs3Status::Ok));
    assert!(!count_was_too_small(Nfs3Status::Ok));
}

#[test]
fn a_readdirplus_entry_carries_the_attributes_before_the_handle() {
    // entryplus3: fileid, name, cookie, post_op_attr, post_op_fh3. Reading the
    // handle first mistakes the attributes' "are there any" boolean for the
    // handle's and desynchronizes every entry after it.
    let fh = handle(28);
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 4097, "informe.txt", 9, Some((true, Some(&fh))));
    put_tail(&mut enc, true);
    let got = decode_readdirplus(&enc.into_bytes()).unwrap().unwrap();

    assert_eq!(got.list.entries.len(), 1);
    let e = &got.list.entries[0];
    assert_eq!(e.fileid, 4097);
    assert_eq!(e.name, "informe.txt");
    assert_eq!(e.cookie, 9);
    assert_eq!(e.attr, Some(sample_fattr()));
    assert_eq!(e.fh, Some(fh));
}

#[test]
fn a_readdirplus_entry_may_carry_neither_attributes_nor_handle() {
    // Both halves are optional and a server declining them is not an error:
    // it means this entry costs a LOOKUP and a GETATTR after all. A decoder
    // that unwrapped either would panic on a reply the RFC allows.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 5, "a", 1, Some((false, None)));
    put_tail(&mut enc, true);
    let got = decode_readdirplus(&enc.into_bytes()).unwrap().unwrap();

    let e = &got.list.entries[0];
    assert_eq!(e.attr, None);
    assert_eq!(e.fh, None);
    assert_eq!(e.name, "a", "and the entry is still an entry");
}

#[test]
fn a_readdirplus_entry_may_carry_attributes_but_no_handle() {
    // The mixed case, which is the one a decoder that treats the two
    // optionals as one gets wrong.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 5, "a", 1, Some((true, None)));
    put_entry(&mut enc, 6, "b", 2, Some((false, Some(&handle(8)))));
    put_tail(&mut enc, true);
    let got = decode_readdirplus(&enc.into_bytes()).unwrap().unwrap();

    assert_eq!(got.list.entries[0].attr, Some(sample_fattr()));
    assert_eq!(got.list.entries[0].fh, None);
    assert_eq!(got.list.entries[1].attr, None);
    assert_eq!(got.list.entries[1].fh, Some(handle(8)));
}

#[test]
fn several_readdirplus_entries_stay_in_step_with_each_other() {
    // The reason the optionals matter: each entry's length depends on what the
    // server attached to it, so one misread field does not stop at that entry.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 1, "uno", 11, Some((true, Some(&handle(12)))));
    put_entry(&mut enc, 2, "dos", 22, Some((false, None)));
    put_entry(&mut enc, 3, "tres", 33, Some((true, None)));
    put_entry(&mut enc, 4, "cuatro", 44, Some((false, Some(&handle(64)))));
    put_tail(&mut enc, false);
    let got = decode_readdirplus(&enc.into_bytes()).unwrap().unwrap();

    let names: Vec<&str> = got.list.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["uno", "dos", "tres", "cuatro"]);
    let cookies: Vec<u64> = got.list.entries.iter().map(|e| e.cookie).collect();
    assert_eq!(cookies, [11, 22, 33, 44]);
    assert_eq!(got.list.next_cookie(), Some(44));
}

#[test]
fn a_readdirplus_handle_longer_than_the_protocol_allows_is_refused() {
    // post_op_fh3 is `opaque<NFS3_FHSIZE>`, so 64 bytes is the most. A server
    // sending more is either broken or probing, and the handle is a bearer
    // token, so the reply is refused rather than truncated into one.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    enc.bool(true);
    enc.u64(1);
    enc.string("a");
    enc.u64(1);
    enc.bool(false); // no attributes
    enc.bool(true); // a handle follows
    enc.opaque(&[0xaa; FHSIZE3 + 4]);
    put_tail(&mut enc, true);

    assert!(decode_readdirplus(&enc.into_bytes()).is_err());
}

#[test]
fn a_name_the_server_sends_with_a_slash_in_it_is_refused() {
    // An NFS server is whatever answered on port 2049, and these names get
    // joined onto paths. An entry called `../../etc` is a directory listing
    // that escapes the mount, so the check that `encode_dir_op` applies on
    // the way out is applied on the way in too -- which is the side that was
    // never trusted. Linux checks only the length here.
    for bad in ["a/b", "../../etc", "/", "sub/"] {
        let mut enc = XdrEncoder::new();
        put_ok_head(&mut enc, &VERF);
        put_entry(&mut enc, 1, bad, 1, None);
        put_tail(&mut enc, true);
        assert!(
            decode_readdir(&enc.into_bytes()).is_err(),
            "{:?} is not a name",
            bad
        );
    }
}

#[test]
fn a_bad_name_fails_the_reply_rather_than_disappearing_from_it() {
    // Skipping the entry would let a server hide a file from the client by
    // naming it badly, and the listing would look complete. The good entries
    // around it do not make the reply usable.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 1, "bueno", 1, None);
    put_entry(&mut enc, 2, "ma/lo", 2, None);
    put_entry(&mut enc, 3, "tambien-bueno", 3, None);
    put_tail(&mut enc, true);

    assert!(decode_readdir(&enc.into_bytes()).is_err());
}

#[test]
fn an_empty_name_and_an_overlong_name_are_both_refused() {
    let long = "a".repeat(MAXNAMLEN + 1);
    for bad in ["", long.as_str()] {
        let mut enc = XdrEncoder::new();
        put_ok_head(&mut enc, &VERF);
        put_entry(&mut enc, 1, bad, 1, None);
        put_tail(&mut enc, true);
        assert!(decode_readdir(&enc.into_bytes()).is_err());
    }
    // And the longest legal one is still legal, so the check is not off by one.
    let longest = "a".repeat(MAXNAMLEN);
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 1, &longest, 1, None);
    put_tail(&mut enc, true);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(got.list.entries[0].name.len(), MAXNAMLEN);
}

#[test]
fn a_dot_and_a_dotdot_from_the_server_are_accepted() {
    // `..` holds no slash, so the name check must not reject it: it is one
    // legitimate component and every directory has it.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 1, ".", 1, None);
    put_entry(&mut enc, 2, "..", 2, None);
    put_tail(&mut enc, true);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(got.list.entries.len(), 2);
}

#[test]
fn a_fileid_of_zero_is_kept_as_it_was_sent() {
    // RFC 1813 asks servers to avoid zero and clients to map it, because zero
    // means "no file" to a lot of userspace. Mapping it here would need an
    // inode number this layer does not have, and inventing one would make the
    // entry disagree with its own attributes. It is passed up intact.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 0, "raro", 1, None);
    put_tail(&mut enc, true);
    let got = decode_readdir(&enc.into_bytes()).unwrap().unwrap();
    assert_eq!(got.list.entries[0].fileid, 0);
}

#[test]
fn a_reply_cut_off_inside_the_entry_list_is_an_error_and_not_a_short_list() {
    // Every prefix of a real reply. A decoder that stopped at the cut and
    // returned what it had would turn a truncated reply into a short
    // directory, which is the same silent truncation as reading an empty list
    // as the end -- reached a different way.
    let whole = readdir_ok(true);
    for cut in [0, 4, 8, 48, 90, 100, whole.len() - 1] {
        assert!(
            decode_readdir(&whole[..cut]).is_err(),
            "cut off at {} bytes",
            cut
        );
    }
    assert!(
        decode_readdir(&whole).is_ok(),
        "and the whole thing is fine"
    );
}

#[test]
fn a_list_that_never_terminates_runs_out_of_bytes_and_does_not_loop() {
    // The chain has no count, so the only thing that ends the loop is a
    // `false` or the end of the buffer. A reply that keeps saying `true`
    // must therefore end as a truncation, not as a spin: the decoder consumes
    // bytes on every pass.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    for i in 0..64u64 {
        put_entry(&mut enc, i + 1, "x", i, None);
    }
    enc.bool(true); // one more follows, and then the buffer stops
    assert!(decode_readdir(&enc.into_bytes()).is_err());
}

#[test]
fn the_eof_flag_after_the_terminator_is_what_decides_the_end() {
    // Same bytes twice over, differing in one boolean. If anything other than
    // that last flag decided `eof`, these two would not disagree.
    let ended = decode_readdir(&readdir_ok(true)).unwrap().unwrap();
    let more = decode_readdir(&readdir_ok(false)).unwrap().unwrap();

    assert_eq!(ended.list.entries, more.list.entries);
    assert!(ended.list.eof);
    assert!(!more.list.eof);
}

#[test]
fn a_readdirplus_reply_is_not_decoded_as_a_readdir_one() {
    // The two differ only inside the entry, so the wrong decoder does not
    // fail at the entry: it reads the attributes as the next entry's
    // introductory boolean and carries on. Both directions are checked,
    // because only one of them errors -- the other one quietly returns
    // something.
    let mut enc = XdrEncoder::new();
    put_ok_head(&mut enc, &VERF);
    put_entry(&mut enc, 1, "a", 1, Some((true, Some(&handle(16)))));
    put_tail(&mut enc, true);
    let plus_bytes = enc.into_bytes();

    let right = decode_readdirplus(&plus_bytes).unwrap().unwrap();
    assert_eq!(right.list.entries.len(), 1);
    assert!(right.list.eof);

    match decode_readdir(&plus_bytes) {
        Err(_) => {}
        Ok(Err(_)) => panic!("a READDIRPLUS reply is not a failed READDIR"),
        Ok(Ok(wrong)) => assert!(
            wrong.list.entries.len() != 1 || !wrong.list.eof,
            "the wrong decoder must not get it right"
        ),
    }
}
