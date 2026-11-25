//! Kani proofs: every parser of what an NFS server sends, for **every**
//! input up to a size, not a sample of them.
//!
//! Run from this directory with `cargo kani` (install once with
//! `cargo install --locked kani-verifier && cargo kani setup`). Kani turns
//! each harness into a formula over symbolic bytes and asks a SAT solver
//! whether any assignment panics, overflows, indexes out of bounds or breaks
//! an `assert!`; "VERIFICATION:- SUCCESSFUL" means none does, for every
//! buffer the harness allows, which a test or a fuzzer cannot say.
//!
//! The server is untrusted: the kernel's NFS client parses these replies
//! with no process in between, so a parser that panics on a crafted reply
//! is a remote kernel crash. The bound on the input (`MAX`) is what keeps the
//! solver finite; it covers every fixed-size reply whole and every
//! variable-length one up to a couple of entries, which is where length
//! arithmetic goes wrong.

use crate::xdr::{padded_len, XdrDecoder, XdrEncoder};

/// Symbolic bytes: `N` of them, of which the first `len` (any `len <= N`)
/// are the input.
fn any_input<const N: usize>() -> ([u8; N], usize) {
    let bytes: [u8; N] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= N);
    (bytes, len)
}

// ── XDR ─────────────────────────────────────────────────────────────────────

#[kani::proof]
fn padded_len_is_the_next_multiple_of_four_or_an_error() {
    let len: usize = kani::any();
    match padded_len(len) {
        Ok(p) => {
            assert!(p >= len && p - len < 4 && p % 4 == 0);
        }
        Err(_) => assert!(len > usize::MAX - 3),
    }
}

/// Any sequence of three reads, of any kind, on any buffer up to 24 bytes:
/// none panics, the position never passes the end, and an opaque body is a
/// sub-slice of the buffer.
#[kani::proof]
#[kani::unwind(26)]
fn xdr_decoder_never_reads_past_the_buffer() {
    let (bytes, len) = any_input::<24>();
    let buf = &bytes[..len];
    let mut dec = XdrDecoder::new(buf);
    for _ in 0..3 {
        let op: u8 = kani::any();
        match op % 7 {
            0 => {
                let _ = dec.u32();
            }
            1 => {
                let _ = dec.u64();
            }
            2 => {
                let _ = dec.bool();
            }
            3 => {
                if let Ok(s) = dec.opaque() {
                    let start = s.as_ptr() as usize - buf.as_ptr() as usize;
                    assert!(start + s.len() <= buf.len());
                }
            }
            4 => {
                let max: usize = kani::any();
                if let Ok(s) = dec.opaque_max(max) {
                    assert!(s.len() <= max);
                }
            }
            5 => {
                let n: usize = kani::any();
                let _ = dec.opaque_fixed(n);
            }
            _ => {
                let n: usize = kani::any();
                let _ = dec.skip(n);
            }
        }
        assert!(dec.position() <= buf.len());
        assert_eq!(dec.position() + dec.remaining(), buf.len());
    }
}

/// What the encoder writes, the decoder reads back: an opaque of any
/// content and any length up to 5, then a `u64`, then a `bool`.
#[kani::proof]
#[kani::unwind(10)]
fn xdr_opaque_u64_bool_round_trip() {
    let (data, len) = any_input::<5>();
    let v: u64 = kani::any();
    let b: bool = kani::any();
    let mut enc = XdrEncoder::new();
    enc.opaque(&data[..len]).u64(v).bool(b);
    let bytes = enc.into_bytes();
    assert_eq!(bytes.len() % 4, 0);
    let mut dec = XdrDecoder::new(&bytes);
    assert_eq!(dec.opaque().unwrap(), &data[..len]);
    assert_eq!(dec.u64().unwrap(), v);
    assert_eq!(dec.bool().unwrap(), b);
    assert!(dec.is_empty());
}

// ── ONC RPC ─────────────────────────────────────────────────────────────────

/// Any bytes at all from the wire: the reassembler may refuse them, never
/// panic, and never hand back more than it was fed.
#[kani::proof]
#[kani::unwind(10)]
fn record_assembler_survives_any_stream() {
    let (bytes, len) = any_input::<8>();
    let mut asm = crate::rpc::RecordAssembler::new();
    asm.feed(&bytes[..len]);
    for _ in 0..2 {
        match asm.next_message() {
            Ok(Some(m)) => assert!(m.len() + 4 <= len),
            Ok(None) | Err(_) => break,
        }
    }
}

#[kani::proof]
#[kani::unwind(34)]
fn rpc_reply_decoding_never_panics() {
    let (bytes, len) = any_input::<32>();
    let xid: u32 = kani::any();
    if let Ok(body) = crate::rpc::decode_reply(&bytes[..len], xid) {
        assert_eq!(body.xid, xid);
        assert!(body.results.len() <= len);
    }
}

// ── the NFS and MOUNT replies ───────────────────────────────────────────────

macro_rules! never_panics {
    ($name:ident, $n:literal, $unwind:literal, $decode:path) => {
        #[kani::proof]
        #[kani::unwind($unwind)]
        fn $name() {
            let (bytes, len) = any_input::<$n>();
            let _ = $decode(&bytes[..len]);
        }
    };
}

// Fattr3 is 84 bytes; status + attributes + a little more. The directory
// replies stop at 40 bytes: one entry past the header, which is where their
// length arithmetic is, and the solver gives up much beyond it.
never_panics!(getattr_never_panics, 96, 100, crate::nfs3::decode_getattr);
never_panics!(access_never_panics, 96, 100, crate::nfs3::decode_access);
never_panics!(lookup_never_panics, 128, 132, crate::nfs3::decode_lookup);
never_panics!(readlink_never_panics, 64, 68, crate::nfs3::decode_readlink);
never_panics!(read_never_panics, 64, 68, crate::nfs3::decode_read);
never_panics!(fsstat_never_panics, 64, 68, crate::nfs3::decode_fsstat);
never_panics!(fsinfo_never_panics, 64, 68, crate::nfs3::decode_fsinfo);
never_panics!(
    readdir_never_panics,
    40,
    44,
    crate::nfs3_dir::decode_readdir
);
never_panics!(
    readdirplus_never_panics,
    40,
    44,
    crate::nfs3_dir::decode_readdirplus
);
never_panics!(write_never_panics, 64, 68, crate::nfs3_write::decode_write);
never_panics!(
    commit_never_panics,
    64,
    68,
    crate::nfs3_write::decode_commit
);
never_panics!(
    create_never_panics,
    64,
    68,
    crate::nfs3_write::decode_create
);
never_panics!(
    setattr_never_panics,
    64,
    68,
    crate::nfs3_write::decode_setattr
);
never_panics!(
    remove_never_panics,
    64,
    68,
    crate::nfs3_write::decode_remove
);
never_panics!(
    rename_never_panics,
    64,
    68,
    crate::nfs3_write::decode_rename
);
never_panics!(link_never_panics, 64, 68, crate::nfs3_write::decode_link);
never_panics!(mnt_never_panics, 64, 68, crate::mount3::decode_mnt);
never_panics!(getport_never_panics, 16, 20, crate::portmap::decode_getport);
