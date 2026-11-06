//! The RPC envelope and the record framing.
//!
//! Most of these are about [`RecordAssembler`], because that is the part a
//! hand-rolled RPC client gets wrong in a way that only shows up on a loaded
//! link. The tests that matter are the ones that feed it badly: one byte at a
//! time, two replies in one buffer, a message split across fragments. A test
//! that hands it a whole reply in a single `feed` passes against an
//! implementation that assumes "one read, one message", which is exactly the
//! implementation this one exists not to be.

use super::*;
use alloc::vec;

/// Build the bytes of an accepted, successful reply carrying `results`.
fn accepted_reply(xid: u32, results: &[u8]) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0); // MSG_ACCEPTED
    enc.u32(AUTH_NULL); // verifier flavour
    enc.opaque(&[]); // verifier body
    enc.u32(0); // SUCCESS
    let mut out = enc.into_bytes();
    out.extend_from_slice(results);
    out
}

/// Build the bytes of a denied reply.
fn denied_reply(xid: u32, reject_stat: u32, a: u32, b: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(1); // MSG_DENIED
    enc.u32(reject_stat);
    enc.u32(a);
    enc.u32(b);
    enc.into_bytes()
}

/// Build the bytes of an accepted reply whose `accept_stat` is not SUCCESS.
fn accepted_error_reply(xid: u32, accept_stat: u32, a: u32, b: u32) -> Vec<u8> {
    let mut enc = XdrEncoder::new();
    enc.u32(xid);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0); // MSG_ACCEPTED
    enc.u32(AUTH_NULL);
    enc.opaque(&[]);
    enc.u32(accept_stat);
    enc.u32(a);
    enc.u32(b);
    enc.into_bytes()
}

// ----------------------------------------------------------- the call side

/// The call header's field order, asserted field by field against RFC 5531
/// section 9. A transposition here (program and version swapped, say) gets a
/// `PROG_UNAVAIL` from the server and looks like a misconfigured export.
#[test]
fn a_call_header_is_the_eight_fields_rfc_5531_names_in_order() {
    let target = CallTarget::new(100_003, 3, 1);
    let msg = encode_call(0xaabb_ccdd, target, &Credential::Null, &[1, 2, 3, 4]);

    let mut dec = XdrDecoder::new(&msg);
    assert_eq!(dec.u32(), Ok(0xaabb_ccdd), "xid");
    assert_eq!(dec.u32(), Ok(MSG_TYPE_CALL), "msg_type = CALL");
    assert_eq!(dec.u32(), Ok(RPC_VERSION), "rpcvers = 2");
    assert_eq!(dec.u32(), Ok(100_003), "prog");
    assert_eq!(dec.u32(), Ok(3), "vers");
    assert_eq!(dec.u32(), Ok(1), "proc");
    assert_eq!(dec.u32(), Ok(AUTH_NULL), "cred flavour");
    assert_eq!(dec.opaque(), Ok(&[][..]), "cred body");
    assert_eq!(dec.u32(), Ok(AUTH_NULL), "verf flavour");
    assert_eq!(dec.opaque(), Ok(&[][..]), "verf body");
    assert_eq!(dec.rest(), &[1, 2, 3, 4], "the arguments follow the header");
}

/// `AUTH_UNIX` carries no verifier. A client that put the credential in both
/// places is refused by the server with an `auth_stat` that says nothing
/// about why.
#[test]
fn an_auth_unix_call_still_has_a_null_verifier() {
    let cred = Credential::Unix(AuthUnix::new(b"eclipse", 1000, 1000));
    let msg = encode_call(1, CallTarget::new(100_003, 3, 1), &cred, &[]);

    let mut dec = XdrDecoder::new(&msg);
    for _ in 0..6 {
        dec.u32().unwrap();
    }
    assert_eq!(dec.u32(), Ok(AUTH_UNIX), "cred flavour is AUTH_UNIX");
    let body = dec.opaque().unwrap();
    assert!(!body.is_empty(), "the AUTH_UNIX body is not empty");
    assert_eq!(dec.u32(), Ok(AUTH_NULL), "but the VERIFIER is still null");
    assert_eq!(dec.opaque(), Ok(&[][..]));
}

#[test]
fn an_auth_unix_body_is_stamp_machine_uid_gid_and_groups() {
    let cred = AuthUnix {
        stamp: 7,
        machine_name: b"eclipse".to_vec(),
        uid: 1000,
        gid: 100,
        gids: vec![4, 20, 27],
    };
    let body = cred.encode();

    let mut dec = XdrDecoder::new(&body);
    assert_eq!(dec.u32(), Ok(7), "stamp");
    assert_eq!(dec.opaque(), Ok(&b"eclipse"[..]), "machine name");
    assert_eq!(dec.u32(), Ok(1000), "uid");
    assert_eq!(dec.u32(), Ok(100), "gid");
    assert_eq!(dec.u32(), Ok(3), "group count");
    assert_eq!(dec.u32(), Ok(4));
    assert_eq!(dec.u32(), Ok(20));
    assert_eq!(dec.u32(), Ok(27));
    assert!(dec.is_empty());
}

/// RFC 1831 caps the group list at 16. A longer one is truncated here, where
/// the limit is written down, rather than left to be refused by some servers
/// and truncated by others.
#[test]
fn an_auth_unix_group_list_is_truncated_at_sixteen() {
    let cred = AuthUnix {
        stamp: 0,
        machine_name: b"e".to_vec(),
        uid: 0,
        gid: 0,
        gids: (0..40).collect(),
    };
    let body = cred.encode();

    let mut dec = XdrDecoder::new(&body);
    dec.u32().unwrap();
    dec.opaque().unwrap();
    dec.u32().unwrap();
    dec.u32().unwrap();
    assert_eq!(
        dec.u32(),
        Ok(AUTH_UNIX_MAX_GIDS as u32),
        "the count on the wire must be the capped one, not 40"
    );
    for expected in 0..AUTH_UNIX_MAX_GIDS as u32 {
        assert_eq!(dec.u32(), Ok(expected), "the first 16 groups are kept");
    }
    assert!(dec.is_empty(), "and nothing follows them");
}

// -------------------------------------------------------- the framing side

#[test]
fn a_record_mark_sets_the_last_fragment_bit_and_the_length() {
    assert_eq!(record_mark(0), [0x80, 0x00, 0x00, 0x00]);
    assert_eq!(record_mark(4), [0x80, 0x00, 0x00, 0x04]);
    assert_eq!(record_mark(0x1234), [0x80, 0x00, 0x12, 0x34]);
}

#[test]
fn framing_prepends_the_mark_to_the_message() {
    let framed = frame_message(&[1, 2, 3, 4]);
    assert_eq!(framed, vec![0x80, 0, 0, 4, 1, 2, 3, 4]);
}

#[test]
fn an_assembler_with_nothing_fed_has_no_message() {
    let mut asm = RecordAssembler::new();
    assert_eq!(asm.next_message(), Ok(None));
    assert!(!asm.in_progress());
}

#[test]
fn a_whole_framed_message_comes_back_whole() {
    let mut asm = RecordAssembler::new();
    asm.feed(&frame_message(&[1, 2, 3, 4]));
    assert_eq!(asm.next_message(), Ok(Some(vec![1, 2, 3, 4])));
    assert_eq!(asm.next_message(), Ok(None));
    assert!(!asm.in_progress());
}

/// The test the whole type exists for. TCP hands back whatever it has, and a
/// byte at a time is a legal thing for it to hand back — so the assembler
/// must report "not yet" for every prefix of a message and the message on the
/// last byte, and never before.
#[test]
fn reply_arrives_one_byte_at_a_time() {
    let payload = accepted_reply(0x1111_2222, &[9, 9, 9, 9]);
    let framed = frame_message(&payload);

    let mut asm = RecordAssembler::new();
    for (i, byte) in framed.iter().enumerate() {
        asm.feed(&[*byte]);
        let got = asm.next_message();
        if i + 1 == framed.len() {
            assert_eq!(
                got,
                Ok(Some(payload.clone())),
                "the message must appear on the last byte"
            );
        } else {
            assert_eq!(
                got,
                Ok(None),
                "no message may be reported after only {} of {} bytes",
                i + 1,
                framed.len()
            );
        }
    }
}

/// Half a record header is the nastiest prefix: three bytes look like
/// nothing, and code that read the header before checking it had four bytes
/// would read uninitialised or wrong length bytes here.
#[test]
fn a_partial_record_header_is_not_a_message() {
    let mut asm = RecordAssembler::new();
    asm.feed(&[0x80, 0x00, 0x00]);
    assert_eq!(asm.next_message(), Ok(None));
    assert!(asm.in_progress(), "three bytes are buffered");
    asm.feed(&[0x00]);
    assert_eq!(
        asm.next_message(),
        Ok(Some(Vec::new())),
        "a zero-length final fragment is an empty message"
    );
}

/// A message may be several fragments, and only the last one has the bit
/// set. Nothing in the protocol says a server sends one fragment per message,
/// however common that is.
#[test]
fn multi_fragment_message_is_concatenated() {
    let mut stream = Vec::new();
    // Two non-final fragments, then a final one.
    stream.extend_from_slice(&0x0000_0004u32.to_be_bytes());
    stream.extend_from_slice(&[1, 2, 3, 4]);
    stream.extend_from_slice(&0x0000_0004u32.to_be_bytes());
    stream.extend_from_slice(&[5, 6, 7, 8]);
    stream.extend_from_slice(&0x8000_0004u32.to_be_bytes());
    stream.extend_from_slice(&[9, 10, 11, 12]);

    let mut asm = RecordAssembler::new();
    asm.feed(&stream);
    assert_eq!(
        asm.next_message(),
        Ok(Some(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])),
        "the three fragments are one message, in order"
    );
    assert!(!asm.in_progress());
}

/// A multi-fragment message must not be reported before its final fragment.
#[test]
fn a_non_final_fragment_alone_is_not_a_message() {
    let mut asm = RecordAssembler::new();
    asm.feed(&0x0000_0004u32.to_be_bytes());
    asm.feed(&[1, 2, 3, 4]);
    assert_eq!(
        asm.next_message(),
        Ok(None),
        "the last-fragment bit is not set, so the message is not done"
    );
    assert!(asm.in_progress(), "and the bytes are held, not dropped");
}

/// One `feed` can complete more than one message. A caller loops on
/// `next_message`, and an assembler that could only ever return the first
/// would leave the second buffered while the caller waited for bytes that had
/// already arrived — a deadlock against a server that has answered.
#[test]
fn two_messages_in_one_feed_both_come_back() {
    let mut stream = frame_message(&[1, 1, 1, 1]);
    stream.extend_from_slice(&frame_message(&[2, 2, 2, 2]));

    let mut asm = RecordAssembler::new();
    asm.feed(&stream);
    assert_eq!(asm.next_message(), Ok(Some(vec![1, 1, 1, 1])));
    assert_eq!(asm.next_message(), Ok(Some(vec![2, 2, 2, 2])));
    assert_eq!(asm.next_message(), Ok(None));
    assert!(!asm.in_progress());
}

/// A second message arriving part-way is held, not confused with the first.
#[test]
fn a_second_message_that_arrives_partially_is_held() {
    let mut stream = frame_message(&[1, 1, 1, 1]);
    stream.extend_from_slice(&[0x80, 0x00]);

    let mut asm = RecordAssembler::new();
    asm.feed(&stream);
    assert_eq!(asm.next_message(), Ok(Some(vec![1, 1, 1, 1])));
    assert_eq!(asm.next_message(), Ok(None));
    assert!(
        asm.in_progress(),
        "the two bytes of the next header are still buffered"
    );
}

/// The cost, not the behaviour: consuming a message must not copy the bytes
/// behind it.
///
/// The first version of the assembler did `pending.drain(..4 + len)` per
/// fragment, which shifts every byte still buffered. The number of fragments
/// is the server's choice, so that was quadratic in a quantity an attacker
/// picks: a one-mebibyte message in one-byte fragments is about five
/// mebibytes on the wire -- comfortably under `MAX_MESSAGE_LEN` -- and cost
/// on the order of terabytes of copying inside the kernel, without exceeding
/// a single limit in this module. Found by review.
///
/// Asserted structurally rather than by the clock, which on a shared runner
/// would be a flaky test: with a read cursor, taking a message out moves the
/// cursor and leaves the buffer alone, so its raw length cannot change. The
/// `drain` version emptied it a message at a time, copying as it went, and
/// fails this outright.
#[test]
fn consuming_messages_does_not_copy_the_bytes_behind_them() {
    const MESSAGES: usize = 2048;
    let mut stream = Vec::new();
    for i in 0..MESSAGES {
        stream.extend_from_slice(&frame_message(&[i as u8; 4]));
    }

    let mut asm = RecordAssembler::new();
    asm.feed(&stream);
    let raw_len = asm.raw_buffer_len();
    assert_eq!(raw_len, stream.len());

    for i in 0..MESSAGES {
        assert_eq!(
            asm.next_message(),
            Ok(Some(vec![i as u8; 4])),
            "message {} of {}",
            i,
            MESSAGES
        );
        assert_eq!(
            asm.raw_buffer_len(),
            raw_len,
            "taking message {} must not have re-copied the buffer",
            i
        );
    }
    assert!(!asm.in_progress(), "every message was taken");
}

/// And the cursor must not let the buffer grow forever: the next `feed`
/// reclaims the consumed prefix, which is where the amortisation is paid.
#[test]
fn the_consumed_prefix_is_reclaimed_on_a_later_feed() {
    let mut asm = RecordAssembler::new();
    for _ in 0..64 {
        asm.feed(&frame_message(&[1, 2, 3, 4]));
        assert_eq!(asm.next_message(), Ok(Some(vec![1, 2, 3, 4])));
    }
    assert!(
        asm.raw_buffer_len() <= 64,
        "after 64 messages the buffer still holds {} bytes of consumed prefix",
        asm.raw_buffer_len()
    );
}

/// The compaction must not lose bytes at its boundary: a header or a payload
/// that spans one has to survive it.
#[test]
fn compaction_does_not_lose_a_message_that_spans_it() {
    let mut asm = RecordAssembler::new();
    asm.feed(&frame_message(&[1, 2, 3, 4]));
    assert_eq!(asm.next_message(), Ok(Some(vec![1, 2, 3, 4])));
    // Fed one byte at a time, so a compaction falls inside the next message.
    let second = frame_message(&[5, 6, 7, 8]);
    for byte in &second {
        asm.feed(&[*byte]);
    }
    assert_eq!(asm.next_message(), Ok(Some(vec![5, 6, 7, 8])));
    assert!(!asm.in_progress());
}

/// The cap exists so a server cannot make this buffer without bound in the
/// kernel's heap. It is checked against the running total, so a server cannot
/// get past it with many small fragments either.
#[test]
fn a_fragment_over_the_cap_is_refused() {
    let mut asm = RecordAssembler::new();
    let too_long = LAST_FRAGMENT | (MAX_MESSAGE_LEN as u32 + 1);
    asm.feed(&too_long.to_be_bytes());
    assert_eq!(asm.next_message(), Err(RpcError::BadFraming));
}

#[test]
fn many_fragments_that_together_exceed_the_cap_are_refused() {
    let mut asm = RecordAssembler::new();
    // Each fragment is half the cap, so the third one takes the total over.
    let half = (MAX_MESSAGE_LEN / 2) as u32;
    for _ in 0..3 {
        asm.feed(&half.to_be_bytes());
        asm.feed(&vec![0u8; half as usize]);
    }
    assert_eq!(
        asm.next_message(),
        Err(RpcError::BadFraming),
        "the cap is on the assembled total, not on one fragment"
    );
}

/// A zero-length non-final fragment is legal: RFC 1831 section 10 puts no
/// floor on a fragment's length. This was refused at first, on the reasoning
/// that it "advances nothing" — which was wrong, since it consumes its
/// four-byte header, so the loop does advance and a server sending them
/// forever is bounded by the bytes it is willing to send. Refusing it turned
/// a conforming server away. Found by review.
#[test]
fn a_zero_length_non_final_fragment_is_accepted_and_carries_nothing() {
    let mut asm = RecordAssembler::new();
    asm.feed(&0u32.to_be_bytes());
    assert_eq!(
        asm.next_message(),
        Ok(None),
        "it is consumed and the message is still unfinished"
    );
    assert!(
        !asm.in_progress(),
        "an empty fragment leaves nothing buffered"
    );

    // ...and a data fragment after it completes the message.
    asm.feed(&frame_message(&[1, 2, 3, 4]));
    assert_eq!(asm.next_message(), Ok(Some(vec![1, 2, 3, 4])));
}

/// Several empty fragments then a final one with data: the empties contribute
/// nothing and the message is exactly the data.
#[test]
fn empty_fragments_before_the_data_contribute_nothing() {
    let mut stream = Vec::new();
    for _ in 0..8 {
        stream.extend_from_slice(&0u32.to_be_bytes());
    }
    stream.extend_from_slice(&0x8000_0004u32.to_be_bytes());
    stream.extend_from_slice(&[9, 9, 9, 9]);

    let mut asm = RecordAssembler::new();
    asm.feed(&stream);
    assert_eq!(asm.next_message(), Ok(Some(vec![9, 9, 9, 9])));
}

/// But a zero-length *final* fragment is legal: it ends the message.
#[test]
fn a_zero_length_final_fragment_ends_a_message() {
    let mut stream = Vec::new();
    stream.extend_from_slice(&0x0000_0004u32.to_be_bytes());
    stream.extend_from_slice(&[1, 2, 3, 4]);
    stream.extend_from_slice(&LAST_FRAGMENT.to_be_bytes());

    let mut asm = RecordAssembler::new();
    asm.feed(&stream);
    assert_eq!(asm.next_message(), Ok(Some(vec![1, 2, 3, 4])));
}

// ---------------------------------------------------------- the reply side

#[test]
fn an_accepted_successful_reply_yields_its_results() {
    let msg = accepted_reply(42, &[7, 7, 7, 7]);
    let body = decode_reply(&msg, 42).unwrap();
    assert_eq!(body.xid, 42);
    assert_eq!(body.results, &[7, 7, 7, 7]);
}

/// A reply for another `xid` means the TCP stream is out of step. It gets its
/// own error because the connection has to be torn down, not reused.
#[test]
fn a_reply_for_another_xid_is_a_mismatch_not_a_parse_error() {
    let msg = accepted_reply(42, &[]);
    assert_eq!(
        decode_reply(&msg, 43),
        Err(RpcError::XidMismatch {
            expected: 43,
            got: 42
        })
    );
}

#[test]
fn a_call_where_a_reply_was_expected_is_refused() {
    let mut enc = XdrEncoder::new();
    enc.u32(1).u32(MSG_TYPE_CALL);
    assert_eq!(
        decode_reply(&enc.into_bytes(), 1),
        Err(RpcError::NotAReply(MSG_TYPE_CALL))
    );
}

#[test]
fn an_rpc_version_mismatch_carries_the_servers_range() {
    let msg = denied_reply(1, 0, 2, 4);
    assert_eq!(
        decode_reply(&msg, 1),
        Err(RpcError::Rejected(RpcRejection::VersionMismatch {
            low: 2,
            high: 4
        }))
    );
}

/// A refused credential is not an NFS error. Mapping it onto one is what
/// makes a client retry forever against a server that has already said no.
#[test]
fn a_refused_credential_is_an_auth_error_with_the_servers_stat() {
    let msg = denied_reply(1, 1, 5, 0);
    assert_eq!(
        decode_reply(&msg, 1),
        Err(RpcError::Rejected(RpcRejection::AuthError(5)))
    );
}

/// The numbers are RFC 5531 section 9's, and they were swapped here at
/// first. Checked against Linux's own `enum rpc_accept_stat`:
/// `RPC_PROG_UNAVAIL = 1`, `RPC_PROG_MISMATCH = 2`.
#[test]
fn every_accept_stat_maps_to_its_own_rejection() {
    for (stat, expected) in [
        (1u32, RpcRejection::ProgramUnavailable),
        (2, RpcRejection::ProgramMismatch { low: 2, high: 3 }),
        (3, RpcRejection::ProcedureUnavailable),
        (4, RpcRejection::GarbageArgs),
        (5, RpcRejection::SystemError),
        (99, RpcRejection::Unknown(99)),
    ] {
        let msg = accepted_error_reply(1, stat, 2, 3);
        assert_eq!(
            decode_reply(&msg, 1),
            Err(RpcError::Rejected(expected)),
            "accept_stat {} should map to {:?}",
            stat,
            expected
        );
    }
}

/// The test the swap slipped past: a conforming `PROG_UNAVAIL` reply **ends
/// at the status word**, because only `PROG_MISMATCH` carries a version
/// range. With the arms swapped, this read two absent fields off the end and
/// came back `Xdr(Truncated)` — so a server that does not run NFS, which is
/// the commonest thing a misconfigured mount hits, reported itself as broken.
///
/// The test above could not catch it: its helper always appends two words, so
/// the wrong arm found something to read.
#[test]
fn a_program_unavailable_reply_that_ends_at_its_status_still_decodes() {
    let mut enc = XdrEncoder::new();
    enc.u32(1);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0); // MSG_ACCEPTED
    enc.u32(AUTH_NULL);
    enc.opaque(&[]);
    enc.u32(1); // PROG_UNAVAIL, and nothing after it
    assert_eq!(
        decode_reply(&enc.into_bytes(), 1),
        Err(RpcError::Rejected(RpcRejection::ProgramUnavailable)),
        "it must say the program is not there, not that the reply was truncated"
    );
}

/// And the other half: `PROG_MISMATCH` does carry the range, and it is read.
#[test]
fn a_program_mismatch_reply_carries_the_servers_version_range() {
    let msg = accepted_error_reply(1, 2, 3, 4);
    assert_eq!(
        decode_reply(&msg, 1),
        Err(RpcError::Rejected(RpcRejection::ProgramMismatch {
            low: 3,
            high: 4
        }))
    );
}

/// The server's verifier is read past as an `opaque_auth` rather than assumed
/// empty, so a server that answers with a non-empty one does not push every
/// field after it out of step.
#[test]
fn a_non_empty_server_verifier_is_read_past() {
    let mut enc = XdrEncoder::new();
    enc.u32(5);
    enc.u32(MSG_TYPE_REPLY);
    enc.u32(0);
    enc.u32(6); // some flavour of the server's own
    enc.opaque(&[1, 2, 3]); // ...with a body
    enc.u32(0); // SUCCESS
    let mut msg = enc.into_bytes();
    msg.extend_from_slice(&[4, 4, 4, 4]);

    let body = decode_reply(&msg, 5).unwrap();
    assert_eq!(
        body.results,
        &[4, 4, 4, 4],
        "the results must start after the verifier, whatever its size"
    );
}

#[test]
fn a_truncated_reply_is_an_xdr_error() {
    let msg = [0, 0, 0, 1, 0, 0];
    assert!(matches!(decode_reply(&msg, 1), Err(RpcError::Xdr(_))));
}

#[test]
fn an_unknown_reply_stat_is_rejected_as_unknown() {
    let mut enc = XdrEncoder::new();
    enc.u32(1).u32(MSG_TYPE_REPLY).u32(7);
    assert_eq!(
        decode_reply(&enc.into_bytes(), 1),
        Err(RpcError::Rejected(RpcRejection::Unknown(7)))
    );
}

/// The two sides together: a call is framed, the framing is undone by the
/// assembler, and the reply decodes. This is the only test that exercises the
/// whole path, and it is here to catch a layering mistake (framing applied
/// twice, say) that each layer's own tests would both pass.
#[test]
fn a_framed_reply_round_trips_through_the_assembler_and_the_decoder() {
    let payload = accepted_reply(0xdead, &[1, 2, 3, 4]);
    let mut asm = RecordAssembler::new();
    asm.feed(&frame_message(&payload));
    let msg = asm.next_message().unwrap().unwrap();
    let body = decode_reply(&msg, 0xdead).unwrap();
    assert_eq!(body.results, &[1, 2, 3, 4]);
}
