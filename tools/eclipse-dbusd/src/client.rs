//! A blocking D-Bus client, used by `--selftest` (and by the unit tests) to
//! exercise a running bus over a real socket.
//!
//! It is deliberately small: connect, authenticate, send, wait for a reply.
//! No threads, no main loop. What it proves is exactly what a broken kernel
//! would break — `connect`, `SO_PEERCRED`-backed EXTERNAL auth, and reading a
//! stream of framed messages back.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use crate::message::{Arg, Message};

/// How long any one wait may take. One constant, because it used to be the
/// literal `10` repeated at four call sites and `call` was the one that had
/// none: its loop was `loop { let r = self.recv()?; ... }`, and `recv`'s own
/// deadline only fires when the socket goes QUIET. A peer that keeps talking
/// without ever answering -- a service emitting signals while it starts up, a
/// monitor's copy of the traffic -- reset that deadline on every message, so
/// `--selftest` could hang for as long as the bus stayed busy. `wait_for`, right
/// below it, has always had the bound; `call` now shares it.
pub const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Client {
    sock: UnixStream,
    buf: Vec<u8>,
    serial: u32,
    pub unique: String,
}

impl Client {
    /// Connect to `path`, run the SASL EXTERNAL handshake and say `Hello`.
    pub fn connect(path: &str) -> Result<Client, String> {
        let mut sock = UnixStream::connect(path).map_err(|e| format!("connect {path}: {e}"))?;
        sock.set_read_timeout(Some(TIMEOUT))
            .map_err(|e| e.to_string())?;

        // The protocol starts with a single NUL byte on the socket, before any
        // auth line (it is what lets fd passing be negotiated at all).
        sock.write_all(&[0u8]).map_err(|e| e.to_string())?;
        let uid = unsafe { libc::getuid() };
        sock.write_all(format!("AUTH EXTERNAL {}\r\n", auth_hex(uid)).as_bytes())
            .map_err(|e| e.to_string())?;

        let mut client = Client {
            sock,
            buf: Vec::new(),
            serial: 0,
            unique: String::new(),
        };
        let line = client.read_line()?;
        if !line.starts_with("OK ") {
            return Err(format!("auth rejected: {line}"));
        }
        client
            .sock
            .write_all(b"NEGOTIATE_UNIX_FD\r\n")
            .map_err(|e| e.to_string())?;
        let line = client.read_line()?;
        if line != "AGREE_UNIX_FD" {
            return Err(format!("expected AGREE_UNIX_FD, got {line}"));
        }
        client
            .sock
            .write_all(b"BEGIN\r\n")
            .map_err(|e| e.to_string())?;

        let reply = client.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            &[],
        )?;
        client.unique = match reply.args().first() {
            Some(Arg::Str(s)) => s.clone(),
            _ => return Err("Hello returned no unique name".into()),
        };
        Ok(client)
    }

    fn read_line(&mut self) -> Result<String, String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some((line, used)) = split_line(&self.buf) {
                self.buf.drain(..used);
                return Ok(line);
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for an auth line".into());
            }
            self.fill()?;
        }
    }

    fn fill(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 4096];
        let n = self
            .sock
            .read(&mut chunk)
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("the bus closed the connection".into());
        }
        self.buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    /// Read the next complete message off the wire.
    pub fn recv(&mut self) -> Result<Message, String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some((m, used)) = Message::decode(&self.buf)? {
                self.buf.drain(..used);
                return Ok(m);
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for a message".into());
            }
            self.fill()?;
        }
    }

    /// Send one message, assigning it the next serial. Returns that serial.
    pub fn send(&mut self, mut msg: Message) -> Result<u32, String> {
        self.serial += 1;
        msg.serial = self.serial;
        self.sock
            .write_all(&msg.encode())
            .map_err(|e| format!("write: {e}"))?;
        Ok(self.serial)
    }

    /// Send a method call and wait for its reply, skipping anything else that
    /// arrives in the meantime (signals are asynchronous and may interleave).
    pub fn call(
        &mut self,
        dest: &str,
        path: &str,
        iface: &str,
        member: &str,
        args: &[Arg],
    ) -> Result<Message, String> {
        let mut m = Message::method_call(dest, path, iface, member);
        m.set_body(args);
        let serial = self.send(m)?;
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if Instant::now() > deadline {
                return Err(format!("timed out waiting for the reply to {member}"));
            }
            let r = self.recv()?;
            if r.reply_serial == Some(serial) {
                if r.kind == crate::message::MSG_ERROR {
                    let text = match r.args().first() {
                        Some(Arg::Str(s)) => s.clone(),
                        _ => String::new(),
                    };
                    return Err(format!(
                        "{}: {text}",
                        r.error_name.as_deref().unwrap_or("error")
                    ));
                }
                return Ok(r);
            }
        }
    }

    /// Wait until a message satisfying `pred` arrives.
    pub fn wait_for<F: Fn(&Message) -> bool>(&mut self, pred: F) -> Result<Message, String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let m = self.recv()?;
            if pred(&m) {
                return Ok(m);
            }
            if Instant::now() > deadline {
                return Err("timed out waiting for the expected message".into());
            }
        }
    }
}

/// The uid as SASL `EXTERNAL` wants it: the hex of its DECIMAL TEXT, not of the
/// number.
///
/// uid 0 is `30` (the hex of the byte `'0'`) and not `00`, and uid 1000 is
/// `31303030`. Getting it wrong is a bus that refuses every connection with
/// `REJECTED`, which looks exactly like a permissions problem on the socket. It
/// lived inside `connect`, where nothing could check it.
///
/// The `02` and the lower case are belt and braces, not behaviour: the input is
/// always ASCII digits (`0x30`..`0x39`), every one of which is two hex digits
/// with no letter in it, so neither padding nor case can show. Two mutants live
/// on that and are said here rather than hidden.
fn auth_hex(uid: u32) -> String {
    format!("{uid}")
        .bytes()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The first CRLF-terminated line in `buf`, and how many bytes it used.
///
/// Extracted from `read_line` so the framing can be checked without a socket.
/// The auth phase is line-based and the message phase is not, so this must find
/// the FIRST CRLF and never look past it: the daemon may have the reply and the
/// start of a binary message in the same read.
fn split_line(buf: &[u8]) -> Option<(String, usize)> {
    let pos = buf.windows(2).position(|w| w == b"\r\n")?;
    Some((String::from_utf8_lossy(&buf[..pos]).to_string(), pos + 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::MSG_SIGNAL;

    /// Start a real daemon on its own socket and hand back the path.
    fn daemon() -> String {
        let path = format!(
            "/tmp/eclipse-dbusd-client-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = crate::serve(&p, false, false);
        });
        let deadline = Instant::now() + TIMEOUT;
        while !std::path::Path::new(&path).exists() {
            assert!(Instant::now() < deadline, "el demonio no llego a escuchar");
            std::thread::yield_now();
        }
        path
    }

    #[test]
    fn the_uid_goes_out_as_the_hex_of_its_decimal_text() {
        // SASL EXTERNAL carries the uid hex-encoded as TEXT, not as a number:
        // uid 0 is "30", the hex of the byte '0'. Encoding the integer instead
        // gives "00" and the bus answers REJECTED to every connection, which on
        // a desktop looks like a permissions problem on the socket.
        assert_eq!(auth_hex(0), "30");
        assert_eq!(auth_hex(1), "31");
        assert_eq!(auth_hex(9), "39");
        assert_eq!(auth_hex(10), "3130");
        assert_eq!(auth_hex(1000), "31303030");
        assert_eq!(auth_hex(65534), "3635353334");
        // Two hex digits per digit, always lower case, and it decodes back.
        for uid in [0u32, 7, 42, 999, 100_000, u32::MAX] {
            let hex = auth_hex(uid);
            assert_eq!(hex.len(), format!("{uid}").len() * 2, "uid {uid}");
            assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()), "uid {uid}");
            assert!(!hex.bytes().any(|b| b.is_ascii_uppercase()), "uid {uid}");
            let back: String = hex
                .as_bytes()
                .chunks(2)
                .map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap() as char)
                .collect();
            assert_eq!(back, format!("{uid}"), "uid {uid} no vuelve");
        }
    }

    #[test]
    fn an_auth_line_ends_at_the_first_crlf_and_not_a_byte_later() {
        // The auth phase is line-based and the message phase is not, so the
        // daemon's reply and the first bytes of a binary message can land in one
        // read. Stopping at the first CRLF is what keeps the message intact.
        assert_eq!(split_line(b"OK abc\r\n"), Some(("OK abc".to_string(), 8)));
        assert_eq!(
            split_line(b"AGREE_UNIX_FD\r\nl\x01\x00\x01"),
            Some(("AGREE_UNIX_FD".to_string(), 15)),
            "se comio el principio del mensaje"
        );
        // Two lines in one read: the first only.
        assert_eq!(split_line(b"A\r\nB\r\n"), Some(("A".to_string(), 3)));
        // An empty line is a line.
        assert_eq!(split_line(b"\r\n"), Some((String::new(), 2)));
        // A bare CR or LF is not a terminator, and neither is a partial line:
        // returning here would hand the caller half an answer.
        for part in [
            &b""[..],
            &b"O"[..],
            &b"OK abc"[..],
            &b"OK abc\r"[..],
            &b"OK abc\n"[..],
            &b"\r"[..],
            &b"\n"[..],
        ] {
            assert_eq!(split_line(part), None, "{part:?}");
        }
        // Invalid UTF-8 is lossy rather than fatal: the line is compared against
        // ASCII keywords, so mangling it can only make the comparison fail.
        assert_eq!(split_line(b"OK \xff\r\n").map(|(_, n)| n), Some(6));
    }

    #[test]
    fn the_handshake_gets_a_unique_name_from_a_real_bus() {
        // Everything `connect` does in one go, over a real socket: the leading
        // NUL, EXTERNAL with SO_PEERCRED, NEGOTIATE_UNIX_FD and Hello. Any of
        // them wrong and no desktop client can reach the bus at all.
        let path = daemon();
        let c = Client::connect(&path).expect("connect");
        assert!(
            c.unique.starts_with(":1."),
            "nombre unico raro: {:?}",
            c.unique
        );
        assert!(c.unique[3..].parse::<u64>().is_ok(), "{:?}", c.unique);
        // A second connection gets a different name, which is what makes
        // `NameOwnerChanged` mean anything.
        let d = Client::connect(&path).expect("connect 2");
        assert_ne!(c.unique, d.unique);
        drop((c, d));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_call_skips_the_signals_that_arrive_while_it_waits() {
        // The contract `call` documents: a reply is matched by serial and
        // anything else on the wire is passed over. Signals are asynchronous, so
        // a client that took the next message as its answer would read a
        // NameOwnerChanged as the return value of whatever it just asked.
        let path = daemon();
        let mut watcher = Client::connect(&path).expect("connect");
        watcher
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "AddMatch",
                &[Arg::Str("type='signal'".into())],
            )
            .expect("AddMatch");

        // A second client takes a name, which makes the bus emit
        // NameOwnerChanged to the watcher -- traffic the watcher did not ask for.
        let mut other = Client::connect(&path).expect("connect 2");
        other
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "RequestName",
                &[Arg::Str("org.ejemplo.Prueba".into()), Arg::U32(0)],
            )
            .expect("RequestName");

        // Now the watcher asks something of its own. The signal is already
        // queued in front of the reply.
        let r = watcher
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                &[Arg::Str("org.ejemplo.Prueba".into())],
            )
            .expect("GetNameOwner");
        assert_eq!(
            r.args().first(),
            Some(&Arg::Str(other.unique.clone())),
            "la respuesta no es la de GetNameOwner: {:?}",
            r.args()
        );
        // Skipping means DISCARDING, which is the part of the contract worth
        // pinning: the signals that arrived while the call was in flight are
        // gone, so this client cannot both call and watch. What must survive is
        // the STREAM -- a signal emitted after the call still arrives, in
        // order, so the connection was not left mid-message.
        other
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReleaseName",
                &[Arg::Str("org.ejemplo.Prueba".into())],
            )
            .expect("ReleaseName");
        let sig = watcher
            .wait_for(|m| m.kind == MSG_SIGNAL && m.member.as_deref() == Some("NameOwnerChanged"))
            .expect("la senal de despues se perdio");
        assert_eq!(
            sig.arg0_str().as_deref(),
            Some("org.ejemplo.Prueba"),
            "{:?}",
            sig.args()
        );
        // The bus stamps its own name on it and a client cannot forge that.
        assert_eq!(sig.sender.as_deref(), Some("org.freedesktop.DBus"));
        drop((watcher, other));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_error_reply_comes_back_as_an_error_and_names_itself() {
        // A D-Bus ERROR is a reply like any other on the wire: only its type
        // says so. A client that returned it as success would hand its caller an
        // empty body where it expected a name.
        let path = daemon();
        let mut c = Client::connect(&path).expect("connect");
        let e = c
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                &[Arg::Str("org.no.Existe".into())],
            )
            .expect_err("tendria que fallar");
        assert!(e.contains("NameHasNoOwner"), "el error no se nombra: {e:?}");
        // The connection survives it: an error is not a broken stream.
        c.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetId",
            &[],
        )
        .expect("GetId despues del error");
        drop(c);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn every_wait_in_this_client_is_bounded() {
        // `call` used to be the one loop with no deadline of its own: `recv`'s
        // deadline only fires when the socket goes QUIET, so a peer that keeps
        // talking without ever answering reset it on every message and the call
        // never returned. On the guest that is `--selftest` hanging with no
        // output, which reads as a kernel problem rather than a bus one.
        // Only the code, not this module: a test that quotes a pattern would
        // otherwise count itself, and comments explaining the rule would break
        // it.
        let src = include_str!("client.rs");
        let code = src.split("#[cfg(test)]").next().unwrap();
        let body: Vec<&str> = code
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        // The timeout is named once and used, never written out again.
        assert_eq!(
            body.iter()
                .filter(|l| l.contains("Duration::from_secs("))
                .count(),
            1,
            "hay un plazo escrito a mano fuera de TIMEOUT"
        );
        // Every loop that waits on the peer checks a deadline. There are four:
        // read_line, recv, call and wait_for. Both halves are counted, because
        // a deadline that is computed and never consulted reads as a bound and
        // is not one -- which is exactly the shape `call` was in.
        assert_eq!(
            body.iter().filter(|l| l.contains("+ TIMEOUT")).count(),
            4,
            "un bucle de espera se quedo sin plazo"
        );
        assert_eq!(
            body.iter().filter(|l| l.contains("> deadline")).count(),
            4,
            "un plazo se calcula y no se mira"
        );
        assert!(TIMEOUT.as_secs() > 0, "un plazo de cero no espera nada");
    }
}
