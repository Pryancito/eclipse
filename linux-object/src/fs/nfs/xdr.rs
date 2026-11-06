//! XDR (External Data Representation, RFC 4506): the encoding every ONC RPC
//! protocol — and so NFS — puts on the wire.
//!
//! The whole format is four rules, which is why this is a few hundred lines
//! and not a dependency:
//!
//! - every scalar is a multiple of four bytes, big-endian;
//! - a signed or unsigned 32-bit integer is four bytes, a 64-bit one is eight
//!   (`hyper`, in RFC 4506's names);
//! - *variable-length opaque* and *string* are a 32-bit length followed by
//!   that many bytes, then zero to three zero bytes so the next field starts
//!   on a four-byte boundary. The padding is written and it is *read*, but it
//!   is not counted in the length;
//! - a *fixed-length opaque* is the bytes and the same padding, with no
//!   length ahead of it.
//!
//! Everything NFSv3 puts on the wire is built out of those, plus two shapes
//! this module gives names to because every message uses them: an *optional*
//! value (a 32-bit boolean followed by the value only when it is true — RFC
//! 1813's `post_op_attr` and friends) and a *discriminated union* (a 32-bit
//! tag followed by the arm, which is how every NFS reply carries either a
//! result or a status).
//!
//! # Why a decoder has to be hostile
//!
//! The bytes come from a file server. A mounted NFS export is as trusted as
//! the machine serving it, and a reply is parsed *before* anything has
//! checked who sent it, so every length on the wire is attacker-controlled
//! as far as this code is concerned. Two rules follow, and both are enforced
//! here rather than left to each caller:
//!
//! - **No length is ever used to allocate.** A 32-bit length can say four
//!   gigabytes; [`XdrDecoder::opaque`] borrows out of the buffer it was given
//!   and fails if the buffer is shorter, so a lie costs an error and not a
//!   four-gigabyte allocation in the kernel heap.
//! - **No arithmetic on a length may wrap.** The padded size of a field is
//!   `len + (4 - len % 4) % 4`, which overflows for a `len` near `u32::MAX`
//!   — on a release build that wraps to a small number and the bounds check
//!   that follows *passes*. Every such sum here is checked
//!   ([`padded_len`]), and the fuzz-shaped tests in `xdr_tests` hold that
//!   down at the boundary.
//!
//! The encoder has no such problem: it writes into a growing `Vec` and every
//! length it writes is one it computed itself.

use alloc::vec::Vec;

/// What went wrong reading or writing XDR.
///
/// There is deliberately no `Io` variant: an [`XdrDecoder`] reads a buffer
/// that is already in memory and an [`XdrEncoder`] writes to a `Vec`, so
/// neither can fail for a reason outside the data itself. Moving the bytes is
/// the transport's job, and the transport's errors are its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdrError {
    /// The buffer ended inside a field: fewer bytes remain than the field
    /// needs. The usual shape of a truncated reply.
    Truncated,
    /// A length on the wire is one no buffer could satisfy — large enough
    /// that computing its padded size would overflow. Refused before the
    /// arithmetic rather than after it.
    LengthTooLarge,
    /// The padding after a variable-length field was not zero bytes.
    ///
    /// RFC 4506 requires the padding to be zero, so a non-zero byte there is
    /// a malformed message. It is also the cheapest signal that a decoder and
    /// an encoder disagree about a field's shape, which is the bug this
    /// catches in practice: a field read as fixed-length where the server
    /// wrote it variable-length lands the decoder one field out of step, and
    /// the first thing it trips over is usually a "padding" byte with data in
    /// it. Linux's XDR decoder is laxer here and ignores the padding; this
    /// refuses it, which is a deliberate divergence — a server that pads with
    /// garbage is a server whose replies this code cannot trust to be the
    /// shape it thinks.
    BadPadding,
    /// A boolean was neither 0 nor 1, or a union tag named no arm this
    /// decoder knows.
    BadDiscriminant(u32),
}

/// A successful read or write of XDR, or [`XdrError`].
pub type XdrResult<T> = Result<T, XdrError>;

/// The size `len` bytes occupy on the wire: `len` rounded up to a multiple of
/// four.
///
/// Returns [`XdrError::LengthTooLarge`] rather than wrapping. The sum can
/// only overflow for a length within three of `usize::MAX`, which no honest
/// message carries — but a hostile one can, and on a release build (no
/// overflow checks) the wrap produces a *small* number that then passes the
/// bounds check it was supposed to fail.
pub fn padded_len(len: usize) -> XdrResult<usize> {
    let pad = (4 - (len % 4)) % 4;
    len.checked_add(pad).ok_or(XdrError::LengthTooLarge)
}

/// Writes XDR into a growing buffer.
///
/// Infallible by construction: every length it writes it computed itself, so
/// unlike [`XdrDecoder`] none of its methods return a `Result`.
#[derive(Debug, Default)]
pub struct XdrEncoder {
    buf: Vec<u8>,
}

impl XdrEncoder {
    /// An encoder writing into an empty buffer.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// An encoder whose buffer has room for `cap` bytes up front.
    ///
    /// Worth using for a call whose size is known: an NFS WRITE carries a
    /// payload of up to the negotiated transfer size, and growing a `Vec`
    /// into it reallocates and copies the whole payload several times.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// How many bytes have been written.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Take the buffer, consuming the encoder.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// A 32-bit unsigned integer (`unsigned int`).
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    /// A 32-bit signed integer (`int`). Same four bytes as [`Self::u32`];
    /// the separate name is so call sites read like the RFC's field types.
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.u32(v as u32)
    }

    /// A 64-bit unsigned integer (`unsigned hyper`).
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    /// A 64-bit signed integer (`hyper`).
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.u64(v as u64)
    }

    /// A boolean, as the 32-bit 0 or 1 XDR defines it to be.
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u32(u32::from(v))
    }

    /// Zero to three zero bytes, so the next field starts on a four-byte
    /// boundary.
    fn pad_to_4(&mut self, len: usize) -> &mut Self {
        let pad = (4 - (len % 4)) % 4;
        self.buf.extend_from_slice(&[0u8; 3][..pad]);
        self
    }

    /// A fixed-length opaque (`opaque v[n]`): the bytes and their padding,
    /// with no length ahead of them.
    ///
    /// The length is part of the protocol, not of the message, so the caller
    /// and the server have to agree on it: nothing on the wire states it.
    pub fn opaque_fixed(&mut self, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(data);
        self.pad_to_4(data.len())
    }

    /// A variable-length opaque (`opaque v<>`): a 32-bit length, the bytes,
    /// then padding.
    ///
    /// This is the shape of an NFS filehandle, of the payload of a READ or a
    /// WRITE, and of the opaque body of an RPC credential.
    pub fn opaque(&mut self, data: &[u8]) -> &mut Self {
        self.u32(data.len() as u32);
        self.opaque_fixed(data)
    }

    /// A string (`string s<>`): encoded exactly as [`Self::opaque`], over the
    /// string's bytes.
    ///
    /// XDR strings are not terminated and carry no encoding; NFSv3 filenames
    /// are bytes, which is what Linux treats them as too, so nothing here
    /// validates UTF-8.
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.opaque(s.as_bytes())
    }

    /// An optional value: the boolean, then `f` only when `value` is `Some`.
    ///
    /// RFC 1813 spells this out field by field (`post_op_attr`,
    /// `post_op_fh3`, the `set_*` fields of `sattr3`); it is one shape, so it
    /// gets one method.
    pub fn optional<T>(
        &mut self,
        value: Option<T>,
        f: impl FnOnce(&mut Self, T) -> &mut Self,
    ) -> &mut Self {
        match value {
            Some(v) => {
                self.bool(true);
                f(self, v)
            }
            None => self.bool(false),
        }
    }
}

/// Reads XDR out of a borrowed buffer.
///
/// Borrowed and not owned on purpose: a decoder never allocates, so no length
/// a server sends can make it allocate. [`Self::opaque`] hands back a slice of
/// the caller's buffer, and copying it — if the caller needs it to outlive the
/// buffer — is the caller's decision, made after the length has been checked
/// against bytes that actually arrived.
#[derive(Debug, Clone)]
pub struct XdrDecoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> XdrDecoder<'a> {
    /// A decoder positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// How many bytes have been consumed.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// How many bytes are left unread.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether every byte has been read.
    ///
    /// Worth asserting after a reply is parsed: bytes left over mean the
    /// decoder's idea of the message's shape is not the server's.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The bytes not yet read.
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// Advance over `n` bytes, or fail if fewer remain.
    fn take(&mut self, n: usize) -> XdrResult<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(XdrError::LengthTooLarge)?;
        if end > self.buf.len() {
            return Err(XdrError::Truncated);
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// A 32-bit unsigned integer.
    pub fn u32(&mut self) -> XdrResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A 32-bit signed integer.
    pub fn i32(&mut self) -> XdrResult<i32> {
        self.u32().map(|v| v as i32)
    }

    /// A 64-bit unsigned integer.
    pub fn u64(&mut self) -> XdrResult<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// A 64-bit signed integer.
    pub fn i64(&mut self) -> XdrResult<i64> {
        self.u64().map(|v| v as i64)
    }

    /// A boolean.
    ///
    /// Only 0 and 1 are booleans. A server that sends 2 is malformed, and
    /// taking "non-zero is true" would hide exactly the field-misalignment
    /// this refuses; see [`XdrError::BadPadding`] for the same argument about
    /// padding.
    pub fn bool(&mut self) -> XdrResult<bool> {
        match self.u32()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(XdrError::BadDiscriminant(other)),
        }
    }

    /// Read the zero to three padding bytes that follow a field of `len`
    /// bytes, and refuse them if they are not zero.
    fn read_padding(&mut self, len: usize) -> XdrResult<()> {
        let pad = (4 - (len % 4)) % 4;
        if self.take(pad)?.iter().any(|&b| b != 0) {
            return Err(XdrError::BadPadding);
        }
        Ok(())
    }

    /// A fixed-length opaque of `len` bytes, and its padding.
    ///
    /// Borrows out of the decoder's buffer; nothing is copied.
    pub fn opaque_fixed(&mut self, len: usize) -> XdrResult<&'a [u8]> {
        // Checked before the read so a length near `usize::MAX` fails here
        // rather than wrapping inside `read_padding`.
        padded_len(len)?;
        let out = self.take(len)?;
        self.read_padding(len)?;
        Ok(out)
    }

    /// A variable-length opaque: the length, the bytes, the padding.
    ///
    /// The length is read from the wire and so is hostile. It is never used
    /// to allocate: the bytes are borrowed, and a length longer than the
    /// buffer is [`XdrError::Truncated`].
    pub fn opaque(&mut self) -> XdrResult<&'a [u8]> {
        let len = self.u32()? as usize;
        self.opaque_fixed(len)
    }

    /// A variable-length opaque whose length the protocol caps.
    ///
    /// For the fields where RFC 1813 states a maximum — a filehandle is at
    /// most `NFS3_FHSIZE` (64) bytes, a filename at most `NFS3_MAXNAMLEN`
    /// — a longer one is a malformed reply and worth refusing at the field
    /// rather than wherever it would next not fit. Returns
    /// [`XdrError::LengthTooLarge`] above `max`.
    pub fn opaque_max(&mut self, max: usize) -> XdrResult<&'a [u8]> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(XdrError::LengthTooLarge);
        }
        self.opaque_fixed(len)
    }

    /// A string, as bytes. See [`XdrEncoder::string`]: nothing validates
    /// UTF-8, because NFSv3 names are bytes.
    pub fn string_bytes(&mut self) -> XdrResult<&'a [u8]> {
        self.opaque()
    }

    /// An optional value: the boolean, then `f` only when it was true.
    pub fn optional<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> XdrResult<T>,
    ) -> XdrResult<Option<T>> {
        if self.bool()? {
            f(self).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Skip `n` bytes of a field this decoder does not need.
    ///
    /// NFS replies carry fields a client can ignore (a `wcc_data` whose
    /// before-attributes nothing here reads, say), and skipping is how the
    /// decoder stays in step with the message without modelling every field.
    pub fn skip(&mut self, n: usize) -> XdrResult<()> {
        self.take(n).map(|_| ())
    }
}

#[cfg(test)]
#[path = "xdr_tests.rs"]
mod xdr_tests;
