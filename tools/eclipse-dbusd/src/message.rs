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

    fn write_variant(&self, w: &mut Writer) {
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

impl MatchRule {
    /// Parse `key='value',key2='value2'`. Values may be quoted with `'` and a
    /// literal `'` inside is written `'\''` — the sequence libdbus emits.
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
            let mut val = String::new();
            if i < b.len() && b[i] == b'\'' {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\'' {
                        // `'\''` -> a literal quote, and the value continues.
                        if b.get(i + 1) == Some(&b'\\')
                            && b.get(i + 2) == Some(&b'\'')
                            && b.get(i + 3) == Some(&b'\'')
                        {
                            val.push('\'');
                            i += 4;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    val.push(b[i] as char);
                    i += 1;
                }
            } else {
                let vs = i;
                while i < b.len() && b[i] != b',' {
                    i += 1;
                }
                val = text[vs..i].trim().to_string();
            }
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
                Some(p) if p == ns => {}
                Some(p) if p.starts_with(ns) && p.as_bytes().get(ns.len()) == Some(&b'/') => {}
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
                    Some(v) if v == ns => {}
                    Some(v) if v.starts_with(ns) && v.as_bytes().get(ns.len()) == Some(&b'.') => {}
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
}
