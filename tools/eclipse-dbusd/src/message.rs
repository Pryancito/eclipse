//! D-Bus wire format: marshalling and unmarshalling of messages.
//!
//! Only what a message bus actually needs is implemented. The daemon routes
//! messages it does not understand, so the body of a forwarded message is kept
//! as OPAQUE BYTES in the sender's own byte order and copied verbatim; only the
//! fixed header and the header-field array are re-serialised (the bus has to
//! stamp the `SENDER` field on everything it forwards). That is why there is no
//! general value tree here: a full type system would buy nothing and would be
//! one more place to get alignment wrong.
//!
//! Alignment rules (D-Bus specification, "Marshaling"):
//!
//! | type                                   | alignment |
//! |----------------------------------------|-----------|
//! | BYTE, SIGNATURE, VARIANT               | 1         |
//! | INT16, UINT16                          | 2         |
//! | BOOLEAN, INT32, UINT32, STRING, PATH   | 4         |
//! | INT64, UINT64, DOUBLE, STRUCT, DICT    | 8         |
//! | ARRAY                                  | 4 (the length; elements then align to the element type) |
//!
//! The header is `yyyyuua(yv)` padded to a multiple of 8; the body starts right
//! after that padding and is `body_len` bytes long.

use std::collections::HashMap;

/// `METHOD_CALL` message type.
pub const MSG_METHOD_CALL: u8 = 1;
/// `METHOD_RETURN` message type.
pub const MSG_METHOD_RETURN: u8 = 2;
/// `ERROR` message type.
pub const MSG_ERROR: u8 = 3;
/// `SIGNAL` message type.
pub const MSG_SIGNAL: u8 = 4;

/// `NO_REPLY_EXPECTED`: the sender does not want a reply, so a failed delivery
/// must not produce an error message either.
pub const FLAG_NO_REPLY_EXPECTED: u8 = 1;

/// Header field codes (D-Bus specification, "Header fields").
const FIELD_PATH: u8 = 1;
const FIELD_INTERFACE: u8 = 2;
const FIELD_MEMBER: u8 = 3;
const FIELD_ERROR_NAME: u8 = 4;
const FIELD_REPLY_SERIAL: u8 = 5;
const FIELD_DESTINATION: u8 = 6;
const FIELD_SENDER: u8 = 7;
const FIELD_SIGNATURE: u8 = 8;
const FIELD_UNIX_FDS: u8 = 9;

/// The largest message the daemon will assemble. The specification's own
/// ceiling is 128 MiB; the session bus never legitimately carries anything
/// close, and an unbounded value would let one client make the daemon allocate
/// until the box dies.
pub const MAX_MESSAGE_LEN: usize = 32 * 1024 * 1024;

/// A parsed message. `body` stays in `endian` and is never re-encoded.
#[derive(Clone, Debug, Default)]
pub struct Message {
    /// `b'l'` (little) or `b'B'` (big). Preserved on forward so the body bytes
    /// stay valid.
    pub endian: u8,
    pub kind: u8,
    pub flags: u8,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: Option<String>,
    /// Number of file descriptors carried out of band with this message.
    pub unix_fds: u32,
    pub body: Vec<u8>,
}

impl Message {
    /// A method call with an empty body.
    pub fn method_call(dest: &str, path: &str, iface: &str, member: &str) -> Self {
        Message {
            endian: native_endian(),
            kind: MSG_METHOD_CALL,
            destination: Some(dest.to_string()),
            path: Some(path.to_string()),
            interface: Some(iface.to_string()),
            member: Some(member.to_string()),
            ..Default::default()
        }
    }

    /// A signal with an empty body.
    pub fn signal(path: &str, iface: &str, member: &str) -> Self {
        Message {
            endian: native_endian(),
            kind: MSG_SIGNAL,
            path: Some(path.to_string()),
            interface: Some(iface.to_string()),
            member: Some(member.to_string()),
            ..Default::default()
        }
    }

    /// A `METHOD_RETURN` answering `call`.
    pub fn method_return(call: &Message) -> Self {
        Message {
            endian: native_endian(),
            kind: MSG_METHOD_RETURN,
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            ..Default::default()
        }
    }

    /// An `ERROR` answering `call`, with `text` as the single string argument
    /// (every D-Bus error carries a human-readable message).
    pub fn error(call: &Message, name: &str, text: &str) -> Self {
        let mut m = Message {
            endian: native_endian(),
            kind: MSG_ERROR,
            error_name: Some(name.to_string()),
            reply_serial: Some(call.serial),
            destination: call.sender.clone(),
            ..Default::default()
        };
        m.set_body(&[Arg::Str(text.to_string())]);
        m
    }

    /// Encode `args` as this message's body and set the matching signature.
    /// The body is written in the message's own byte order.
    pub fn set_body(&mut self, args: &[Arg]) {
        let mut w = Writer::new(self.endian);
        let mut sig = String::new();
        for a in args {
            a.signature(&mut sig);
            a.write(&mut w);
        }
        self.body = w.into_bytes();
        self.signature = if sig.is_empty() { None } else { Some(sig) };
    }

    /// Decode the body according to its signature. Only the types a bus and its
    /// clients exchange are decoded; anything else stops the walk and returns
    /// what was read so far, which is enough for match-rule `arg0` and for the
    /// bus's own methods (every one of them takes strings and `UINT32`s).
    pub fn args(&self) -> Vec<Arg> {
        let sig = match &self.signature {
            Some(s) => s.clone(),
            None => return Vec::new(),
        };
        let mut r = Reader::new(&self.body, self.endian);
        let mut out = Vec::new();
        let bytes = sig.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match read_one(&mut r, bytes, &mut i) {
                Some(a) => out.push(a),
                None => break,
            }
        }
        out
    }

    /// The first body argument when it is a string (`s`, `o` or `g`) — what a
    /// match rule's `arg0=` is compared against.
    pub fn arg0_str(&self) -> Option<String> {
        match self.args().first() {
            Some(Arg::Str(s)) | Some(Arg::Path(s)) | Some(Arg::Sig(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Serialise the whole message (header + padding + body).
    pub fn encode(&self) -> Vec<u8> {
        // Header fields first, into their own buffer: the array's byte length
        // has to be known before it can be written.
        let mut f = Writer::new(self.endian);
        let push = |f: &mut Writer, code: u8, v: &Arg| {
            f.align(8); // each field is a STRUCT
            f.put_u8(code);
            v.write_variant(f);
        };
        if let Some(v) = &self.path {
            push(&mut f, FIELD_PATH, &Arg::Path(v.clone()));
        }
        if let Some(v) = &self.interface {
            push(&mut f, FIELD_INTERFACE, &Arg::Str(v.clone()));
        }
        if let Some(v) = &self.member {
            push(&mut f, FIELD_MEMBER, &Arg::Str(v.clone()));
        }
        if let Some(v) = &self.error_name {
            push(&mut f, FIELD_ERROR_NAME, &Arg::Str(v.clone()));
        }
        if let Some(v) = self.reply_serial {
            push(&mut f, FIELD_REPLY_SERIAL, &Arg::U32(v));
        }
        if let Some(v) = &self.destination {
            push(&mut f, FIELD_DESTINATION, &Arg::Str(v.clone()));
        }
        if let Some(v) = &self.sender {
            push(&mut f, FIELD_SENDER, &Arg::Str(v.clone()));
        }
        if let Some(v) = &self.signature {
            push(&mut f, FIELD_SIGNATURE, &Arg::Sig(v.clone()));
        }
        if self.unix_fds > 0 {
            push(&mut f, FIELD_UNIX_FDS, &Arg::U32(self.unix_fds));
        }
        let fields = f.into_bytes();

        let mut w = Writer::new(self.endian);
        w.put_u8(self.endian);
        w.put_u8(self.kind);
        w.put_u8(self.flags);
        w.put_u8(1); // protocol version
        w.put_u32(self.body.len() as u32);
        w.put_u32(self.serial);
        w.put_u32(fields.len() as u32);
        w.raw(&fields);
        w.align(8); // body starts 8-aligned
        w.raw(&self.body);
        w.into_bytes()
    }

    /// Try to decode one message from the front of `buf`.
    ///
    /// Returns `Ok(None)` when `buf` does not hold a whole message yet (the
    /// caller reads more and retries), `Ok(Some((msg, len)))` with the number of
    /// bytes consumed, or `Err` on a malformed message — which is fatal for the
    /// connection, since the stream can no longer be resynchronised.
    pub fn decode(buf: &[u8]) -> Result<Option<(Message, usize)>, String> {
        if buf.len() < 16 {
            return Ok(None);
        }
        let endian = buf[0];
        if endian != b'l' && endian != b'B' {
            return Err(format!("bad endianness byte {endian:#x}"));
        }
        let kind = buf[1];
        let flags = buf[2];
        if buf[3] != 1 {
            return Err(format!("unsupported protocol version {}", buf[3]));
        }
        let mut head = Reader::new(&buf[4..16], endian);
        let body_len = head.u32().ok_or("short header")? as usize;
        let serial = head.u32().ok_or("short header")?;
        let fields_len = head.u32().ok_or("short header")? as usize;

        let fields_end = 16 + fields_len;
        let body_start = align_up(fields_end, 8);
        let total = body_start
            .checked_add(body_len)
            .ok_or("message length overflow")?;
        if total > MAX_MESSAGE_LEN {
            return Err(format!("message too large ({total} bytes)"));
        }
        if buf.len() < total {
            return Ok(None);
        }

        let mut m = Message {
            endian,
            kind,
            flags,
            serial,
            body: buf[body_start..total].to_vec(),
            ..Default::default()
        };

        let mut r = Reader::new(&buf[..fields_end], endian);
        r.skip(16);
        while r.pos() < fields_end {
            r.align(8);
            if r.pos() >= fields_end {
                break;
            }
            let code = r.u8().ok_or("truncated header field")?;
            let sig = r.signature().ok_or("truncated field signature")?;
            let sb = sig.as_bytes();
            let mut i = 0;
            let val = read_one(&mut r, sb, &mut i).ok_or("truncated field value")?;
            match (code, val) {
                (FIELD_PATH, Arg::Path(v)) | (FIELD_PATH, Arg::Str(v)) => m.path = Some(v),
                (FIELD_INTERFACE, Arg::Str(v)) => m.interface = Some(v),
                (FIELD_MEMBER, Arg::Str(v)) => m.member = Some(v),
                (FIELD_ERROR_NAME, Arg::Str(v)) => m.error_name = Some(v),
                (FIELD_REPLY_SERIAL, Arg::U32(v)) => m.reply_serial = Some(v),
                (FIELD_DESTINATION, Arg::Str(v)) => m.destination = Some(v),
                (FIELD_SENDER, Arg::Str(v)) => m.sender = Some(v),
                (FIELD_SIGNATURE, Arg::Sig(v)) => m.signature = Some(v),
                (FIELD_UNIX_FDS, Arg::U32(v)) => m.unix_fds = v,
                // Unknown codes are explicitly allowed to be ignored, and a
                // known code with an unexpected type is the sender's bug: drop
                // the field rather than the connection.
                _ => {}
            }
        }
        Ok(Some((m, total)))
    }
}

/// The argument types the daemon marshals itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Byte(u8),
    Bool(bool),
    U32(u32),
    Str(String),
    Path(String),
    Sig(String),
    /// `as`
    StrArray(Vec<String>),
    /// `a{ss}` — the shape `UpdateActivationEnvironment` takes, kept so a
    /// caller can build one without another marshaller.
    #[allow(dead_code)]
    DictSS(Vec<(String, String)>),
    /// A variant wrapping one of the above.
    Variant(Box<Arg>),
    /// Anything the reader met but does not model. The signature is kept so a
    /// caller can tell what was skipped.
    Other(String),
}

impl Arg {
    fn signature(&self, out: &mut String) {
        match self {
            Arg::Byte(_) => out.push('y'),
            Arg::Bool(_) => out.push('b'),
            Arg::U32(_) => out.push('u'),
            Arg::Str(_) => out.push('s'),
            Arg::Path(_) => out.push('o'),
            Arg::Sig(_) => out.push('g'),
            Arg::StrArray(_) => out.push_str("as"),
            Arg::DictSS(_) => out.push_str("a{ss}"),
            Arg::Variant(_) => out.push('v'),
            Arg::Other(s) => out.push_str(s),
        }
    }

    fn write(&self, w: &mut Writer) {
        match self {
            Arg::Byte(v) => w.put_u8(*v),
            Arg::Bool(v) => w.put_u32(u32::from(*v)),
            Arg::U32(v) => w.put_u32(*v),
            Arg::Str(v) | Arg::Path(v) => w.put_string(v),
            Arg::Sig(v) => w.put_signature(v),
            Arg::StrArray(items) => {
                // ARRAY: u32 byte length, then the elements, which start at the
                // element type's own alignment (4 for STRING) AFTER the length.
                w.align(4);
                let len_at = w.reserve_u32();
                w.align(4);
                let start = w.len();
                for s in items {
                    w.put_string(s);
                }
                let n = w.len() - start;
                w.patch_u32(len_at, n as u32);
            }
            Arg::DictSS(items) => {
                w.align(4);
                let len_at = w.reserve_u32();
                w.align(8); // DICT_ENTRY
                let start = w.len();
                for (k, v) in items {
                    w.align(8);
                    w.put_string(k);
                    w.put_string(v);
                }
                let n = w.len() - start;
                w.patch_u32(len_at, n as u32);
            }
            Arg::Variant(inner) => inner.write_variant(w),
            Arg::Other(_) => {}
        }
    }

    /// Write `VARIANT`: the value's signature, then the value.
    ///
    /// An [`Arg::Other`] is refused here, and that is the whole point of the
    /// guard: `write` does nothing for it while `signature` happily emits the
    /// type, so the pair produced a message whose signature promised a value the
    /// body did not contain -- unrecoverable for the reader, since from there on
    /// every following argument is read at the wrong offset. And the text an
    /// `Other` carries is not even always a signature: `read_one` returns
    /// `Other("(...)")` for a struct. `Other` only ever comes out of a decode,
    /// and nothing re-encodes a decoded body today (the daemon forwards it
    /// verbatim), so this is a latent hole being closed rather than a live bug.
    /// It is written as an empty variant, `g` + nothing, which is well formed.
    fn write_variant(&self, w: &mut Writer) {
        if let Arg::Other(_) = self {
            // `v` of `g""`: a signature this writer can produce and any reader
            // can walk, in place of a message nobody could parse.
            w.put_signature("g");
            w.put_signature("");
            return;
        }
        let mut sig = String::new();
        self.signature(&mut sig);
        w.put_signature(&sig);
        self.write(w);
    }
}

/// Read one complete value of the type starting at `sig[*i]`, advancing `*i`
/// past that type. Returns `None` when the buffer is short or the type is one
/// this reader cannot walk (see [`Arg::Other`] for the difference).
fn read_one(r: &mut Reader, sig: &[u8], i: &mut usize) -> Option<Arg> {
    let t = *sig.get(*i)?;
    *i += 1;
    match t {
        b'y' => r.u8().map(Arg::Byte),
        b'b' => r.u32().map(|v| Arg::Bool(v != 0)),
        b'u' => r.u32().map(Arg::U32),
        b'i' => r.u32().map(Arg::U32),
        b'n' | b'q' => {
            r.align(2);
            r.skip(2);
            Some(Arg::Other("q".into()))
        }
        b'x' | b't' | b'd' => {
            r.align(8);
            r.skip(8);
            Some(Arg::Other("t".into()))
        }
        b'h' => r.u32().map(Arg::U32), // UNIX_FD: an index into the fd array
        b's' => r.string().map(Arg::Str),
        b'o' => r.string().map(Arg::Path),
        b'g' => r.signature().map(Arg::Sig),
        b'v' => {
            let s = r.signature()?;
            let sb = s.as_bytes();
            let mut j = 0;
            let inner = read_one(r, sb, &mut j)?;
            Some(Arg::Variant(Box::new(inner)))
        }
        b'a' => {
            let n = r.u32()? as usize;
            let elem = *sig.get(*i)?;
            // The element type's alignment applies to the FIRST element, after
            // the length word — padding that is not counted in `n`.
            r.align(alignment_of(elem));
            // `n` is a `u32` out of the client's bytes and the position is
            // small, so on a 64-bit target this can never overflow and a plain
            // `+` would answer the same; it is checked for the 32-bit build,
            // where `n` alone can be most of the address space.
            let end = r.pos().checked_add(n)?;
            if end > r.buf.len() {
                return None;
            }
            if elem == b's' {
                let mut items = Vec::new();
                while r.pos() < end {
                    items.push(r.string()?);
                }
                *i += 1;
                return Some(Arg::StrArray(items));
            }
            // Not a string array: skip the whole thing and the element type.
            r.seek(end);
            skip_type(sig, i);
            let mut s = String::from("a");
            s.push(elem as char);
            Some(Arg::Other(s))
        }
        b'(' => {
            // STRUCT: align, then walk members until the closing paren.
            r.align(8);
            while *i < sig.len() && sig[*i] != b')' {
                read_one(r, sig, i)?;
            }
            *i += 1; // ')'
            Some(Arg::Other("(...)".into()))
        }
        _ => None,
    }
}

/// Advance `*i` past one complete type in `sig` without reading any data.
fn skip_type(sig: &[u8], i: &mut usize) {
    let t = match sig.get(*i) {
        Some(t) => *t,
        None => return,
    };
    *i += 1;
    match t {
        b'a' => skip_type(sig, i),
        b'(' | b'{' => {
            let close = if t == b'(' { b')' } else { b'}' };
            while *i < sig.len() && sig[*i] != close {
                skip_type(sig, i);
            }
            if *i < sig.len() {
                *i += 1;
            }
        }
        _ => {}
    }
}

/// The alignment of one marshalled type, used for an array's FIRST element.
///
/// The 2-byte row cannot be told apart from the 4-byte one by any message: the
/// only caller aligns right after the array's length word, which is itself
/// 4-aligned and four bytes long, so the position is always a multiple of 4 and
/// rounding it to 2 or to 4 lands in the same place. It is written as 2 because
/// that is what the specification says; no message can tell, so the test that
/// holds it walks this table directly instead of going through a body.
fn alignment_of(t: u8) -> usize {
    match t {
        b'y' | b'g' | b'v' => 1,
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b's' | b'o' | b'a' | b'h' => 4,
        _ => 8, // x t d ( {
    }
}

pub fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// `b'l'` on every target Eclipse builds for; kept as a function so the byte
/// order is decided in one place.
pub fn native_endian() -> u8 {
    if cfg!(target_endian = "little") {
        b'l'
    } else {
        b'B'
    }
}

/// Byte-order-aware writer with D-Bus alignment.
pub struct Writer {
    buf: Vec<u8>,
    endian: u8,
}

impl Writer {
    pub fn new(endian: u8) -> Self {
        Writer {
            buf: Vec::new(),
            endian,
        }
    }
    pub fn len(&self) -> usize {
        self.buf.len()
    }
    pub fn align(&mut self, a: usize) {
        let pad = align_up(self.buf.len(), a) - self.buf.len();
        self.buf.extend(std::iter::repeat(0u8).take(pad));
    }
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn put_u32(&mut self, v: u32) {
        self.align(4);
        if self.endian == b'l' {
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else {
            self.buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    /// Write a placeholder `UINT32` and return its offset for [`patch_u32`].
    fn reserve_u32(&mut self) -> usize {
        self.align(4);
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0u8; 4]);
        at
    }
    fn patch_u32(&mut self, at: usize, v: u32) {
        let b = if self.endian == b'l' {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        };
        self.buf[at..at + 4].copy_from_slice(&b);
    }
    pub fn put_string(&mut self, s: &str) {
        self.put_u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    pub fn put_signature(&mut self, s: &str) {
        self.buf.push(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Byte-order-aware reader with D-Bus alignment. Every accessor returns `None`
/// rather than panicking on a short or malformed buffer: the input is whatever
/// a client put on the socket.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    endian: u8,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8], endian: u8) -> Self {
        Reader {
            buf,
            pos: 0,
            endian,
        }
    }
    pub fn pos(&self) -> usize {
        self.pos
    }
    fn seek(&mut self, to: usize) {
        self.pos = to.min(self.buf.len());
    }
    pub fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.buf.len());
    }
    pub fn align(&mut self, a: usize) {
        self.pos = align_up(self.pos, a).min(self.buf.len());
    }
    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.align(4);
        let b = self.buf.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if self.endian == b'l' {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }
    pub fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let b = self.buf.get(self.pos..self.pos + n)?;
        // +1 for the trailing NUL, which is not counted in the length.
        self.pos += n + 1;
        if self.pos > self.buf.len() {
            return None;
        }
        String::from_utf8(b.to_vec()).ok()
    }
    pub fn signature(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        let b = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n + 1;
        if self.pos > self.buf.len() {
            return None;
        }
        String::from_utf8(b.to_vec()).ok()
    }
}

/// A parsed `AddMatch` rule.
///
/// Only the keys a session bus client actually sends are honoured. An unknown
/// key makes the rule match nothing, which is the safe direction: a client that
/// asked for something the daemon cannot express gets no traffic instead of
/// everything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MatchRule {
    pub kind: Option<u8>,
    pub sender: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub path: Option<String>,
    pub path_namespace: Option<String>,
    pub destination: Option<String>,
    pub arg0: Option<String>,
    pub arg0namespace: Option<String>,
    /// `true` when the rule used a key this daemon does not implement.
    pub unsupported: bool,
    /// The rule exactly as the client sent it: `RemoveMatch` matches on the
    /// original text, not on the parse.
    pub text: String,
}

/// Is `value` inside the namespace `ns`, whose elements are joined by `sep`?
///
/// `sep` is `/` for `path_namespace` and `.` for `arg0namespace`. The rule is
/// the one dbus-daemon applies: the namespace itself matches, and so does
/// anything below it, but a longer element does NOT -- `path_namespace='/org/a'`
/// takes `/org/a` and `/org/a/b` and leaves `/org/ab` alone.
///
/// The case the hand-written version got wrong: **a namespace that already ends
/// in `sep` needs no separator of its own**, so `path_namespace='/'` -- which is
/// how dbus-monitor and GLib say "every path" -- used to match the single path
/// `/` and nothing else, because the byte after the prefix in `/org/example` is
/// `o` and not `/`. A subscription to the whole tree silently received nothing.
fn in_namespace(value: &str, ns: &str, sep: u8) -> bool {
    if !value.starts_with(ns) {
        return false;
    }
    // Exactly the namespace, or the namespace already carries the separator, or
    // the next byte is one. The `==` could be `<=` without changing a single
    // answer, because `starts_with` above already proves `value` is at least as
    // long; `==` is what the sentence means.
    value.len() == ns.len()
        || ns.as_bytes().last() == Some(&sep)
        || value.as_bytes().get(ns.len()) == Some(&sep)
}

impl MatchRule {
    /// Parse `key='value',key2='value2'`. Values may be quoted with `'` and a
    /// literal `'` inside is written `'\''` — the sequence libdbus emits.
    /// A quoted value is collected as BYTES and decoded as UTF-8 at the end.
    /// It used to be built with `val.push(b as char)`, which is Latin-1: every
    /// byte of a multi-byte character became its own `char`, so `member='Senal'`
    /// with an enye parsed as two mojibake characters and never matched the
    /// message, whose member came back through `String::from_utf8`. The
    /// unquoted branch has always been right (it slices the original `&str`),
    /// so the same rule matched or not depending only on whether the client
    /// quoted it -- and libdbus quotes every value it emits, so the broken
    /// branch is the one that runs.
    pub fn parse(text: &str) -> MatchRule {
        let mut rule = MatchRule {
            text: text.to_string(),
            ..Default::default()
        };
        let mut pairs: HashMap<String, String> = HashMap::new();
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            while i < b.len() && (b[i] == b',' || b[i] == b' ') {
                i += 1;
            }
            let ks = i;
            while i < b.len() && b[i] != b'=' && b[i] != b',' {
                i += 1;
            }
            if i >= b.len() || b[i] != b'=' {
                break;
            }
            let key = text[ks..i].trim().to_string();
            i += 1; // '='
            let val = if i < b.len() && b[i] == b'\'' {
                i += 1;
                let mut raw: Vec<u8> = Vec::new();
                while i < b.len() {
                    if b[i] == b'\'' {
                        // `'\''` -> a literal quote, and the value continues.
                        if b.get(i + 1) == Some(&b'\\')
                            && b.get(i + 2) == Some(&b'\'')
                            && b.get(i + 3) == Some(&b'\'')
                        {
                            raw.push(b'\'');
                            i += 4;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    raw.push(b[i]);
                    i += 1;
                }
                // Invalid UTF-8 cannot match any field of a decoded message
                // (every one of them came through `String::from_utf8`), so a
                // lossy decode is as good as any and keeps the rule parseable.
                String::from_utf8_lossy(&raw).into_owned()
            } else {
                let vs = i;
                while i < b.len() && b[i] != b',' {
                    i += 1;
                }
                text[vs..i].trim().to_string()
            };
            pairs.insert(key, val);
        }

        for (k, v) in pairs {
            match k.as_str() {
                "type" => {
                    rule.kind = match v.as_str() {
                        "method_call" => Some(MSG_METHOD_CALL),
                        "method_return" => Some(MSG_METHOD_RETURN),
                        "error" => Some(MSG_ERROR),
                        "signal" => Some(MSG_SIGNAL),
                        _ => {
                            rule.unsupported = true;
                            None
                        }
                    }
                }
                "sender" => rule.sender = Some(v),
                "interface" => rule.interface = Some(v),
                "member" => rule.member = Some(v),
                "path" => rule.path = Some(v),
                "path_namespace" => rule.path_namespace = Some(v),
                "destination" => rule.destination = Some(v),
                "arg0" => rule.arg0 = Some(v),
                "arg0namespace" => rule.arg0namespace = Some(v),
                // eavesdrop=true asks to see messages addressed to somebody
                // else. The bus offers every matching rule a copy of a unicast
                // message anyway (see `Bus::broadcast`), so the key needs no
                // behaviour of its own — only not to poison the rule, which is
                // what dbus-monitor's fallback path depends on.
                "eavesdrop" => {}
                _ => rule.unsupported = true,
            }
        }
        rule
    }

    /// Does `msg` match? `sender_unique` is the sender's `:1.x` name and
    /// `sender_names` the well-known names it owns, because a rule may name
    /// either.
    pub fn matches(&self, msg: &Message, sender_unique: &str, sender_names: &[String]) -> bool {
        if self.unsupported {
            return false;
        }
        if let Some(k) = self.kind {
            if k != msg.kind {
                return false;
            }
        }
        if let Some(s) = &self.sender {
            if s != sender_unique && !sender_names.iter().any(|n| n == s) {
                return false;
            }
        }
        if let Some(v) = &self.interface {
            if msg.interface.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(v) = &self.member {
            if msg.member.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(v) = &self.path {
            if msg.path.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if let Some(ns) = &self.path_namespace {
            match msg.path.as_deref() {
                Some(p) if in_namespace(p, ns, b'/') => {}
                _ => return false,
            }
        }
        if let Some(v) = &self.destination {
            if msg.destination.as_deref() != Some(v.as_str()) {
                return false;
            }
        }
        if self.arg0.is_some() || self.arg0namespace.is_some() {
            let a0 = msg.arg0_str();
            if let Some(want) = &self.arg0 {
                if a0.as_deref() != Some(want.as_str()) {
                    return false;
                }
            }
            if let Some(ns) = &self.arg0namespace {
                match a0.as_deref() {
                    Some(v) if in_namespace(v, ns, b'.') => {}
                    _ => return false,
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: &Message) -> Message {
        let bytes = m.encode();
        let (got, used) = Message::decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len(), "decode consumed the whole message");
        got
    }

    #[test]
    fn method_call_roundtrip() {
        let mut m = Message::method_call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
        );
        m.serial = 7;
        m.set_body(&[Arg::Str("org.example.Foo".into()), Arg::U32(4)]);
        let got = roundtrip(&m);
        assert_eq!(got.kind, MSG_METHOD_CALL);
        assert_eq!(got.serial, 7);
        assert_eq!(got.member.as_deref(), Some("RequestName"));
        assert_eq!(got.signature.as_deref(), Some("su"));
        assert_eq!(
            got.args(),
            vec![Arg::Str("org.example.Foo".into()), Arg::U32(4)]
        );
    }

    #[test]
    fn string_array_roundtrip() {
        let mut m = Message::signal("/", "org.example", "Names");
        m.serial = 1;
        let names = vec![":1.0".to_string(), "org.example.A".to_string()];
        m.set_body(&[Arg::StrArray(names.clone())]);
        let got = roundtrip(&m);
        assert_eq!(got.signature.as_deref(), Some("as"));
        assert_eq!(got.args(), vec![Arg::StrArray(names)]);
    }

    #[test]
    fn empty_string_array_roundtrip() {
        let mut m = Message::signal("/", "org.example", "Names");
        m.serial = 1;
        m.set_body(&[Arg::StrArray(Vec::new())]);
        let got = roundtrip(&m);
        assert_eq!(got.args(), vec![Arg::StrArray(Vec::new())]);
    }

    #[test]
    fn three_string_signal_roundtrip() {
        // NameOwnerChanged: the one signal every client listens for.
        let mut m = Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        );
        m.serial = 3;
        m.sender = Some("org.freedesktop.DBus".into());
        m.set_body(&[
            Arg::Str("org.example.Foo".into()),
            Arg::Str(String::new()),
            Arg::Str(":1.4".into()),
        ]);
        let got = roundtrip(&m);
        assert_eq!(
            got.args(),
            vec![
                Arg::Str("org.example.Foo".into()),
                Arg::Str(String::new()),
                Arg::Str(":1.4".into())
            ]
        );
        assert_eq!(got.arg0_str().as_deref(), Some("org.example.Foo"));
    }

    #[test]
    fn big_endian_body_survives() {
        let mut m = Message::method_call("a.b", "/", "a.b", "C");
        m.endian = b'B';
        m.serial = 9;
        m.set_body(&[Arg::U32(0x1234_5678), Arg::Str("hi".into())]);
        let bytes = m.encode();
        assert_eq!(bytes[0], b'B');
        let got = roundtrip(&m);
        assert_eq!(
            got.args(),
            vec![Arg::U32(0x1234_5678), Arg::Str("hi".into())]
        );
    }

    #[test]
    fn partial_buffer_is_not_a_message() {
        let mut m = Message::method_call("a.b", "/", "a.b", "C");
        m.serial = 1;
        m.set_body(&[Arg::Str("some payload".into())]);
        let bytes = m.encode();
        for cut in [0, 1, 15, 16, bytes.len() - 1] {
            assert!(
                Message::decode(&bytes[..cut]).unwrap().is_none(),
                "{cut} bytes should be incomplete"
            );
        }
    }

    #[test]
    fn two_messages_in_one_buffer() {
        let mut a = Message::method_call("a.b", "/", "a.b", "First");
        a.serial = 1;
        let mut b = Message::method_call("a.b", "/", "a.b", "Second");
        b.serial = 2;
        b.set_body(&[Arg::Str("x".into())]);
        let mut buf = a.encode();
        buf.extend_from_slice(&b.encode());
        let (m1, n1) = Message::decode(&buf).unwrap().unwrap();
        assert_eq!(m1.member.as_deref(), Some("First"));
        let (m2, n2) = Message::decode(&buf[n1..]).unwrap().unwrap();
        assert_eq!(m2.member.as_deref(), Some("Second"));
        assert_eq!(n1 + n2, buf.len());
    }

    #[test]
    fn bad_endian_byte_is_fatal() {
        let mut m = Message::method_call("a.b", "/", "a.b", "C");
        m.serial = 1;
        let mut bytes = m.encode();
        bytes[0] = b'x';
        assert!(Message::decode(&bytes).is_err());
    }

    #[test]
    fn unix_fds_field_roundtrip() {
        let mut m = Message::method_call("a.b", "/", "a.b", "C");
        m.serial = 1;
        m.unix_fds = 2;
        m.set_body(&[Arg::U32(0), Arg::U32(1)]);
        let got = roundtrip(&m);
        assert_eq!(got.unix_fds, 2);
    }

    #[test]
    fn match_rule_parse_and_match() {
        let r = MatchRule::parse(
            "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.a.B'",
        );
        assert_eq!(r.kind, Some(MSG_SIGNAL));
        assert!(!r.unsupported);
        let mut m = Message::signal("/x", "org.freedesktop.DBus", "NameOwnerChanged");
        m.set_body(&[Arg::Str("org.a.B".into())]);
        assert!(r.matches(&m, ":1.1", &[]));
        m.set_body(&[Arg::Str("org.a.C".into())]);
        assert!(!r.matches(&m, ":1.1", &[]));
    }

    #[test]
    fn match_rule_sender_accepts_well_known_name() {
        let r = MatchRule::parse("sender='org.a.B'");
        let m = Message::signal("/x", "org.x", "Y");
        assert!(!r.matches(&m, ":1.1", &[]));
        assert!(r.matches(&m, ":1.1", &["org.a.B".to_string()]));
    }

    #[test]
    fn match_rule_path_namespace() {
        let r = MatchRule::parse("path_namespace='/org/a'");
        let mut m = Message::signal("/org/a/b", "i", "M");
        assert!(r.matches(&m, ":1.1", &[]));
        m.path = Some("/org/a".into());
        assert!(r.matches(&m, ":1.1", &[]));
        m.path = Some("/org/ab".into());
        assert!(!r.matches(&m, ":1.1", &[]));
    }
    #[test]
    fn the_root_namespace_takes_every_path() {
        // `path_namespace='/'` is how dbus-monitor and GLib say "the whole
        // tree", and it used to match the single path `/` and nothing else,
        // because the byte after the one-character prefix is never `/`. A client
        // subscribed to everything received nothing, which reads as a dead bus.
        let r = MatchRule::parse("type='signal',path_namespace='/'");
        assert!(!r.unsupported);
        for p in ["/", "/org", "/org/example", "/org/example/Deep/Path", "/a"] {
            let m = Message::signal(p, "org.example", "Ping");
            assert!(r.matches(&m, ":1.1", &[]), "{p} quedo fuera de la raiz");
        }
        // A namespace that already ends in the separator needs no second one,
        // which is the same rule dbus-daemon applies.
        let r = MatchRule::parse("path_namespace='/org/'");
        for p in ["/org/", "/org/a", "/org/a/b"] {
            let m = Message::signal(p, "org.example", "Ping");
            assert!(r.matches(&m, ":1.1", &[]), "{p}");
        }
        // And it is still a namespace, not a prefix: a longer ELEMENT is out.
        let r = MatchRule::parse("path_namespace='/org/a'");
        for p in ["/org/ab", "/org/a-b", "/orgs", "/"] {
            let m = Message::signal(p, "org.example", "Ping");
            assert!(!r.matches(&m, ":1.1", &[]), "{p} entro y no debia");
        }
        // A message with no path cannot be in any namespace.
        let mut m = Message::signal("/org/a", "org.example", "Ping");
        m.path = None;
        assert!(!MatchRule::parse("path_namespace='/'").matches(&m, ":1.1", &[]));
    }

    #[test]
    fn arg0namespace_separates_on_the_dot_not_the_slash() {
        // The same prefix rule with the other separator, so one function serves
        // both and neither can drift: it used to be written out twice.
        let rule = MatchRule::parse("arg0namespace='org.example'");
        for a in ["org.example", "org.example.Foo", "org.example.a.b"] {
            let mut m = Message::signal("/", "i", "M");
            m.set_body(&[Arg::Str(a.to_string())]);
            assert!(rule.matches(&m, ":1.1", &[]), "{a}");
        }
        for a in ["org.examplebar", "org.exampl", "org", "com.example.Foo"] {
            let mut m = Message::signal("/", "i", "M");
            m.set_body(&[Arg::Str(a.to_string())]);
            assert!(!rule.matches(&m, ":1.1", &[]), "{a} entro y no debia");
        }
        // The dot is NOT a slash: a path-shaped arg0 is not in the namespace.
        let rule = MatchRule::parse("arg0namespace='org'");
        let mut m = Message::signal("/", "i", "M");
        m.set_body(&[Arg::Str("org/example".into())]);
        assert!(!rule.matches(&m, ":1.1", &[]));
    }

    #[test]
    fn a_quoted_value_is_the_same_string_as_an_unquoted_one() {
        // The quoted branch built the value with `push(byte as char)`, which is
        // Latin-1, while the unquoted branch slices the original `&str`. So the
        // two disagreed on every non-ASCII value -- and libdbus quotes
        // everything it emits, so the broken branch is the one that runs.
        for v in [
            "Senal",
            "Se\u{f1}al",
            "caf\u{e9}",
            "\u{c1}rbol",
            "a\u{300}",
            "\u{4e2d}\u{6587}",
            "emoji\u{1f600}",
        ] {
            let quoted = MatchRule::parse(&format!("member='{v}'"));
            let bare = MatchRule::parse(&format!("member={v}"));
            assert_eq!(quoted.member.as_deref(), Some(v), "entrecomillado {v:?}");
            assert_eq!(bare.member.as_deref(), Some(v), "sin comillas {v:?}");
            assert_eq!(
                quoted.member, bare.member,
                "las dos ramas difieren en {v:?}"
            );
        }
    }

    #[test]
    fn a_rule_with_a_non_ascii_value_matches_the_message_it_names() {
        // End to end, because the halves have to agree: the rule's value comes
        // from the rule text and the message's from `String::from_utf8`, and
        // Latin-1 on one side means the two are never equal.
        let rule = MatchRule::parse("type='signal',member='Se\u{f1}al'");
        let m = Message::signal("/org/a", "org.example", "Se\u{f1}al");
        assert!(rule.matches(&m, ":1.1", &[]));
        // arg0 too, which is what a name-owner watcher compares.
        let rule = MatchRule::parse("arg0='org.ejemplo.Cami\u{f3}n'");
        let mut m = Message::signal("/", "i", "M");
        m.set_body(&[Arg::Str("org.ejemplo.Cami\u{f3}n".into())]);
        assert!(rule.matches(&m, ":1.1", &[]));
        // And a value that is not the same string still does not match.
        let mut other = Message::signal("/", "i", "M");
        other.set_body(&[Arg::Str("org.ejemplo.Camion".into())]);
        assert!(!rule.matches(&other, ":1.1", &[]));
    }

    #[test]
    fn a_variant_the_writer_cannot_marshal_does_not_derail_the_body() {
        // `Arg::Other` has a signature and no value: `signature()` emitted the
        // type and `write()` wrote nothing, so the body promised an argument it
        // did not carry and EVERY following argument was then read at the wrong
        // offset. Unrecoverable for the reader, and `Other("(...)")` is not even
        // a signature.
        let mut m = Message::signal("/", "org.example", "V");
        m.set_body(&[
            Arg::Variant(Box::new(Arg::Other("(...)".into()))),
            Arg::Str("detras".into()),
        ]);
        let bytes = m.encode();
        let (got, used) = Message::decode(&bytes).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        // The argument after it still arrives, which is the whole point.
        let args = got.args();
        assert_eq!(
            args.last(),
            Some(&Arg::Str("detras".into())),
            "el argumento de detras se leyo del sitio equivocado: {args:?}"
        );
    }

    #[test]
    fn an_array_carries_its_element_padding_even_when_it_is_empty() {
        // The spec's array layout is "a UINT32 length, followed by alignment
        // padding to the alignment boundary of the array element type, followed
        // by each element" -- with no exception for zero elements. A DICT_ENTRY
        // aligns to 8, so an empty `a{ss}` is eight bytes and not four. The
        // daemon's own hand-rolled empty `a{sv}` used to be four.
        let mut m = Message::signal("/", "org.example", "D");
        m.set_body(&[Arg::DictSS(Vec::new())]);
        assert_eq!(m.body.len(), 8, "a{{ss}} vacio: {:?}", m.body);
        assert_eq!(m.body, vec![0u8; 8], "y la longitud es cero");
        // A STRING element aligns to 4, which the length word already satisfies,
        // so an empty `as` is four bytes: the rule is the element's alignment,
        // not a blanket eight.
        let mut m = Message::signal("/", "org.example", "S");
        m.set_body(&[Arg::StrArray(Vec::new())]);
        assert_eq!(m.body.len(), 4, "as vacio: {:?}", m.body);
    }

    #[test]
    fn the_fixed_header_is_eight_aligned_so_the_field_structs_land_right() {
        // `encode` builds the header-field array in a Writer of its OWN, from
        // offset 0, and aligns each field to 8 as a STRUCT. That is only valid
        // because the fixed header it gets appended to is a multiple of 8 --
        // `yyyyuuu` is 16 bytes. Were it not, every field's padding would be
        // computed against the wrong origin and no other implementation could
        // read a single message this daemon sends.
        let mut m = Message::method_call("org.example", "/org/a", "org.example", "M");
        m.serial = 1;
        let bytes = m.encode();
        assert_eq!(bytes[3], 1, "version del protocolo");
        let mut head = Reader::new(&bytes[4..16], bytes[0]);
        head.u32().unwrap(); // body_len
        head.u32().unwrap(); // serial
        let fields_len = head.u32().unwrap() as usize;
        assert_eq!(16 % 8, 0, "la cabecera fija tiene que ser multiplo de 8");
        // And the body starts at the next multiple of 8 after the fields.
        assert_eq!(bytes.len(), align_up(16 + fields_len, 8) + m.body.len());
    }

    #[test]
    fn a_type_the_reader_cannot_model_is_still_walked_over() {
        // `read_one` returns `Arg::Other` for the types it does not decode, but
        // it must still ADVANCE the reader by exactly that type's width, or
        // every argument behind it comes out of the wrong offset. The widths are
        // the spec's: 2 for q, 8 for t and d.
        for (sig, filler) in [("q", 2usize), ("t", 8), ("d", 8)] {
            let mut w = Writer::new(native_endian());
            w.align(filler);
            for _ in 0..filler {
                w.put_u8(0xAB);
            }
            w.put_string("detras");
            let mut m = Message::signal("/", "i", "M");
            m.endian = native_endian();
            m.body = w.into_bytes();
            m.signature = Some(format!("{sig}s"));
            let args = m.args();
            assert_eq!(
                args.last(),
                Some(&Arg::Str("detras".into())),
                "tras un {sig} la cadena se leyo mal: {args:?}"
            );
        }
        // The same, with a BYTE behind instead of a string. A string realigns
        // to 4 on its way in and hides an off-by-one in the skip; a byte has no
        // alignment of its own, so it reads exactly where the skip left off.
        for (sig, width) in [("q", 2usize), ("n", 2), ("t", 8), ("x", 8), ("d", 8)] {
            let mut w = Writer::new(native_endian());
            for _ in 0..width {
                w.put_u8(0xAB);
            }
            w.put_u8(0x5A);
            let mut m = Message::signal("/", "i", "M");
            m.endian = native_endian();
            m.body = w.into_bytes();
            m.signature = Some(format!("{sig}y"));
            assert_eq!(
                m.args().last(),
                Some(&Arg::Byte(0x5A)),
                "un {sig} no ocupa {width} bytes"
            );
        }
        // And the skip has to ALIGN first, which only shows when the value is
        // not already at its boundary: a byte, then a 64-bit type, starts at 8.
        let mut w = Writer::new(native_endian());
        w.put_u8(0x11);
        w.align(8);
        for _ in 0..8 {
            w.put_u8(0xAB);
        }
        w.put_u8(0x5A);
        let mut m = Message::signal("/", "i", "M");
        m.endian = native_endian();
        m.body = w.into_bytes();
        m.signature = Some("yty".to_string());
        assert_eq!(
            m.args(),
            vec![Arg::Byte(0x11), Arg::Other("t".into()), Arg::Byte(0x5A)],
            "un t detras de un y no se alineo a 8"
        );
    }

    #[test]
    fn an_array_starts_its_elements_at_the_element_alignment() {
        // `alignment_of` decides where an array's FIRST element sits, and that
        // padding is not counted in the array's own length word -- so a wrong
        // alignment there does not shorten the array, it shifts every element
        // and everything behind it. The types the reader only skips (`q`, `v`)
        // are the ones no other test reaches.
        for (elem, align) in [("q", 2usize), ("n", 2), ("y", 1), ("t", 8), ("d", 8)] {
            let mut w = Writer::new(native_endian());
            w.put_u32(4); // cuatro bytes de elementos
            w.align(align);
            let start = w.len();
            for _ in 0..4 {
                w.put_u8(0xAB);
            }
            assert_eq!(w.len() - start, 4);
            w.put_u8(0x5A);
            let mut m = Message::signal("/", "i", "M");
            m.endian = native_endian();
            m.body = w.into_bytes();
            m.signature = Some(format!("a{elem}y"));
            assert_eq!(
                m.args().last(),
                Some(&Arg::Byte(0x5A)),
                "un a{elem} no empieza en su alineacion de {align}"
            );
        }
        // A VARIANT element aligns to 1, so `av` has no padding after the
        // length at all -- the one the table could lose without any other test
        // noticing, because nothing else asks for a variant's alignment.
        let mut w = Writer::new(native_endian());
        w.put_u32(0);
        w.put_u8(0x5A);
        let mut m = Message::signal("/", "i", "M");
        m.endian = native_endian();
        m.body = w.into_bytes();
        m.signature = Some("avy".to_string());
        assert_eq!(
            m.args().last(),
            Some(&Arg::Byte(0x5A)),
            "un av vacio metio relleno que no lleva"
        );
    }

    #[test]
    fn match_rule_unknown_key_matches_nothing() {
        let r = MatchRule::parse("arg3='x'");
        assert!(r.unsupported);
        let m = Message::signal("/x", "i", "M");
        assert!(!r.matches(&m, ":1.1", &[]));
    }

    #[test]
    fn match_rule_escaped_quote() {
        let r = MatchRule::parse(r"member='it'\''s'");
        assert_eq!(r.member.as_deref(), Some("it's"));
    }

    /// Every type code the reader knows reads back as its OWN kind. The
    /// roundtrips above all go through `Arg::write`, which can emit five of
    /// the thirteen codes `read_one` accepts, so the other eight were only
    /// ever read from a client's bytes -- never from a test's.
    #[test]
    fn every_type_code_the_reader_knows_reads_back_as_its_own_kind() {
        fn str_bytes(v: &str) -> Vec<u8> {
            let mut w = Writer::new(b'l');
            w.put_string(v);
            w.into_bytes()
        }
        fn sig_bytes(v: &str) -> Vec<u8> {
            let mut w = Writer::new(b'l');
            w.put_signature(v);
            w.into_bytes()
        }
        fn one(sig: &str, body: &[u8]) -> Option<Arg> {
            let mut r = Reader::new(body, b'l');
            let mut i = 0;
            read_one(&mut r, sig.as_bytes(), &mut i)
        }
        let eight = vec![0u8; 8];
        let cases: Vec<(&str, Vec<u8>, Option<Arg>)> = vec![
            ("y", vec![0x2a], Some(Arg::Byte(0x2a))),
            ("b", 1u32.to_le_bytes().to_vec(), Some(Arg::Bool(true))),
            ("b", 0u32.to_le_bytes().to_vec(), Some(Arg::Bool(false))),
            ("b", 2u32.to_le_bytes().to_vec(), Some(Arg::Bool(true))),
            ("u", 7u32.to_le_bytes().to_vec(), Some(Arg::U32(7))),
            ("i", 9u32.to_le_bytes().to_vec(), Some(Arg::U32(9))),
            ("h", 3u32.to_le_bytes().to_vec(), Some(Arg::U32(3))),
            ("s", str_bytes("hi"), Some(Arg::Str("hi".into()))),
            // An object path is its own kind: a rule that names `path=` and a
            // body that carries one are matched against different fields.
            ("o", str_bytes("/org/p"), Some(Arg::Path("/org/p".into()))),
            ("g", sig_bytes("su"), Some(Arg::Sig("su".into()))),
            ("n", vec![0u8; 2], Some(Arg::Other("q".into()))),
            ("q", vec![0u8; 2], Some(Arg::Other("q".into()))),
            ("x", eight.clone(), Some(Arg::Other("t".into()))),
            ("t", eight.clone(), Some(Arg::Other("t".into()))),
            ("d", eight.clone(), Some(Arg::Other("t".into()))),
            // Unknown codes are refused rather than guessed at.
            ("Z", vec![0u8; 8], None),
            ("", vec![], None),
        ];
        for (sig, body, want) in cases {
            assert_eq!(one(sig, &body), want, "reading '{sig}'");
        }
        // A variant carries its own signature and then one value of it.
        let mut w = Writer::new(b'l');
        w.put_signature("s");
        w.put_string("inner");
        assert_eq!(
            one("v", &w.into_bytes()),
            Some(Arg::Variant(Box::new(Arg::Str("inner".into()))))
        );
    }

    /// The alignment of every type code, as the specification gives it.
    ///
    /// This is a table, and the only way to hold a table is to walk it:
    /// `read_one` asks about an array's element type and nothing else, so one
    /// wrong row shifts the first element of exactly one shape of array and
    /// every other message decodes the same.
    #[test]
    fn every_type_code_has_the_alignment_the_specification_gives_it() {
        for (t, want) in [
            (b'y', 1usize),
            (b'g', 1),
            (b'v', 1),
            (b'n', 2),
            (b'q', 2),
            (b'b', 4),
            (b'i', 4),
            (b'u', 4),
            (b's', 4),
            (b'o', 4),
            (b'a', 4),
            (b'h', 4),
            (b'x', 8),
            (b't', 8),
            (b'd', 8),
            (b'(', 8),
            (b'{', 8),
        ] {
            assert_eq!(alignment_of(t), want, "alignment of '{}'", t as char);
        }
    }

    /// `skip_type` steps over one COMPLETE type however deeply nested,
    /// because the index it leaves behind is where the next argument's type
    /// is read from. Nothing walked it before: its only caller is the array
    /// arm that is NOT a string array, and every array in the tests above is
    /// an `as`, which takes the other branch.
    #[test]
    fn one_whole_type_is_skipped_and_no_more() {
        for (sig, want) in [
            ("s", 1usize),
            ("as", 2),
            ("aas", 3),
            ("(ii)", 4),
            ("(ii)s", 4),
            ("(i(ii))", 7),
            ("{sv}", 4),
            ("a{sv}", 5),
            ("a{sv}u", 5),
            ("a(ii)u", 5),
            // Unterminated or empty: stop at the end rather than run past it.
            ("(", 1),
            ("", 0),
        ] {
            let mut i = 0;
            skip_type(sig.as_bytes(), &mut i);
            assert_eq!(i, want, "skipping the first type of \"{sig}\"");
        }
    }

    /// A STRUCT is padded to eight bytes before its first member, so the
    /// `u` of `y(u)` is read from byte 8 and not from byte 4. The reader
    /// cannot be asked for the member's value -- a struct comes back as a
    /// placeholder -- so the proof is where the cursor and the signature
    /// index land.
    #[test]
    fn a_struct_starts_on_its_eight_byte_boundary() {
        let mut w = Writer::new(b'l');
        w.put_u8(1);
        w.align(8);
        w.put_u32(0xdead_beef);
        let body = w.into_bytes();
        assert_eq!(body.len(), 12, "a byte, seven of padding and a word");

        let mut r = Reader::new(&body, b'l');
        let mut i = 0;
        assert_eq!(read_one(&mut r, b"y(u)", &mut i), Some(Arg::Byte(1)));
        assert_eq!(r.pos(), 1);
        assert_eq!(
            read_one(&mut r, b"y(u)", &mut i),
            Some(Arg::Other("(...)".into()))
        );
        assert_eq!(r.pos(), 12, "the struct's word was read from byte 8");
        assert_eq!(i, 4, "the index is past the closing paren");
    }

    /// An array has to leave the signature index past its ELEMENT type, or
    /// the next argument is read with the array's own type: `asu` would take
    /// the `s` for the second argument and hand back a string where a number
    /// belongs.
    #[test]
    fn an_array_leaves_the_signature_index_past_its_element_type() {
        let mut m = Message::signal("/", "org.example", "Pair");
        m.serial = 1;
        m.set_body(&[
            Arg::StrArray(vec!["one".into(), "two".into()]),
            Arg::U32(0x5a5a),
        ]);
        assert_eq!(m.signature.as_deref(), Some("asu"));
        let got = roundtrip(&m);
        assert_eq!(
            got.args(),
            vec![
                Arg::StrArray(vec!["one".into(), "two".into()]),
                Arg::U32(0x5a5a),
            ]
        );
    }

    /// Every key a client may send goes to its OWN field. The parser is a
    /// table of ten keys and the tests above read three rows of it, so a key
    /// wired to the wrong field -- or to nothing at all -- changed which
    /// traffic a subscription received and no test moved.
    #[test]
    fn every_key_of_a_match_rule_lands_in_its_own_field() {
        let r = MatchRule::parse(
            "type='signal',sender=':1.5',interface='org.a',member='M',\
             path='/p',destination=':1.9',arg0='x',arg0namespace='org.b',\
             eavesdrop='true'",
        );
        assert_eq!(r.kind, Some(MSG_SIGNAL));
        assert_eq!(r.sender.as_deref(), Some(":1.5"));
        assert_eq!(r.interface.as_deref(), Some("org.a"));
        assert_eq!(r.member.as_deref(), Some("M"));
        assert_eq!(r.path.as_deref(), Some("/p"));
        assert_eq!(r.destination.as_deref(), Some(":1.9"));
        assert_eq!(r.arg0.as_deref(), Some("x"));
        assert_eq!(r.arg0namespace.as_deref(), Some("org.b"));
        assert!(r.path_namespace.is_none(), "nothing names it");
        // `eavesdrop` needs no behaviour of its own, but poisoning the rule
        // is what dbus-monitor's fallback path would fall foul of.
        assert!(!r.unsupported, "eavesdrop is accepted, not refused");

        let ns = MatchRule::parse("path_namespace='/org/a'");
        assert_eq!(ns.path_namespace.as_deref(), Some("/org/a"));
        assert!(ns.path.is_none());
    }

    /// The four message types by name, and anything else poisons the rule: a
    /// client that asked for a type this daemon cannot express gets no
    /// traffic instead of all of it.
    #[test]
    fn a_rule_names_its_message_type_or_matches_nothing() {
        for (text, want) in [
            ("type='method_call'", MSG_METHOD_CALL),
            ("type='method_return'", MSG_METHOD_RETURN),
            ("type='error'", MSG_ERROR),
            ("type='signal'", MSG_SIGNAL),
        ] {
            let r = MatchRule::parse(text);
            assert_eq!(r.kind, Some(want), "{text}");
            assert!(!r.unsupported, "{text}");
        }
        let m = Message::signal("/", "org.a", "M");
        for text in ["type='whatever'", "nosuchkey='v'"] {
            let r = MatchRule::parse(text);
            assert!(r.unsupported, "{text} is not a rule that takes everything");
            assert!(!r.matches(&m, ":1.1", &[]), "{text}");
        }
        assert_eq!(MatchRule::parse("type='whatever'").kind, None);
    }

    /// libdbus writes `key='value',key='value'` with no spaces, but a
    /// hand-written rule has them anywhere. Key and value are both trimmed
    /// and a run of commas and spaces separates one pair from the next, so
    /// the same rule spelt loosely is the same rule -- which is what
    /// `RemoveMatch` leans on.
    #[test]
    fn a_rule_spelt_with_spaces_is_the_same_rule() {
        let tight = MatchRule::parse("type='signal',member='M',path=/p");
        assert_eq!(tight.path.as_deref(), Some("/p"));
        for text in [
            "type='signal', member='M', path= /p ",
            " type='signal' ,  member='M' , path=/p",
            "type='signal',,member='M',,path=/p",
            // A space BEHIND the key, which the run of separators in front
            // of it cannot eat.
            "type ='signal',member ='M',path =/p",
        ] {
            let loose = MatchRule::parse(text);
            assert_eq!(loose.kind, tight.kind, "{text}");
            assert_eq!(loose.member, tight.member, "{text}");
            assert_eq!(loose.path, tight.path, "{text}");
        }
    }

    /// A rule reads the field it names and no other. Each `false` row below
    /// holds a value that IS in the message, just under a different field, so
    /// a check wired to the wrong one would let the message through on a
    /// field the client never mentioned.
    #[test]
    fn a_rule_reads_the_field_it_names_and_no_other() {
        let mut m = Message::signal("/org/p", "org.iface", "Member");
        m.serial = 1;
        m.destination = Some(":1.9".into());
        m.set_body(&[Arg::Str("org.b.x".into())]);

        for (text, want) in [
            ("path='/org/p'", true),
            ("path='org.iface'", false),
            ("interface='org.iface'", true),
            ("interface='Member'", false),
            ("member='Member'", true),
            ("member='org.iface'", false),
            ("destination=':1.9'", true),
            ("destination='/org/p'", false),
            ("arg0='org.b.x'", true),
            ("arg0='org.b'", false),
            ("arg0namespace='org.b'", true),
            ("arg0namespace='org.c'", false),
            ("path_namespace='/org'", true),
            ("path_namespace='/or'", false),
        ] {
            let r = MatchRule::parse(text);
            assert_eq!(r.matches(&m, ":1.1", &[]), want, "{text}");
        }
    }
}
