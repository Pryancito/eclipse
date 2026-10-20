//! A blocking D-Bus client, used by `--selftest` (and by the unit tests) to
//! exercise a running bus over a real socket.
//!
//! It is deliberately small: connect, authenticate, send, wait for a reply.
//! No threads, no main loop. What it proves is exactly what a broken kernel
//! would break — `connect`, `SO_PEERCRED`-backed EXTERNAL auth, and reading a
//! stream of framed messages back.

use std::io::{self, Read, Write};
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

    /// Wait up to `within` for the next message. `Ok(None)` means the time
    /// ran out with nothing on the wire, which for a daemon is not an error:
    /// it is the moment to look at everything else it is doing (here, whether
    /// the file dialog it launched has finished).
    ///
    /// A whole message already in the buffer is returned without touching the
    /// socket at all, because one `read` can bring several: polling the socket
    /// first would leave the second message sitting there until the NEXT
    /// message arrived, which for a bus that can go quiet for minutes is a
    /// reply the client never gets.
    pub fn poll(&mut self, within: Duration) -> Result<Option<Message>, String> {
        if let Some((m, used)) = Message::decode(&self.buf)? {
            self.buf.drain(..used);
            return Ok(Some(m));
        }
        self.sock
            .set_read_timeout(Some(within))
            .map_err(|e| e.to_string())?;
        let mut chunk = [0u8; 4096];
        let got = self.sock.read(&mut chunk);
        // Put the long timeout back, so a later `call` keeps its own bound
        // rather than inheriting this poll's few milliseconds.
        let _ = self.sock.set_read_timeout(Some(TIMEOUT));
        match got {
            Ok(0) => Err("the bus closed the connection".into()),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                match Message::decode(&self.buf)? {
                    Some((m, used)) => {
                        self.buf.drain(..used);
                        Ok(Some(m))
                    }
                    // A partial message: not an error and not a message yet.
                    None => Ok(None),
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("read: {e}")),
        }
    }

    /// Take a well-known name, refusing to queue behind an existing owner.
    /// `true` when this connection is now the primary owner.
    ///
    /// `DO_NOT_QUEUE` is the flag that matters: a portal that queued would sit
    /// there owning nothing while every client talked to the other portal, and
    /// would silently become the owner later, in the middle of a session.
    pub fn request_name(&mut self, name: &str) -> Result<bool, String> {
        const DO_NOT_QUEUE: u32 = 4;
        const PRIMARY_OWNER: u32 = 1;
        let r = self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            &[Arg::Str(name.to_string()), Arg::U32(DO_NOT_QUEUE)],
        )?;
        Ok(matches!(r.args().first(), Some(Arg::U32(PRIMARY_OWNER))))
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
        // Waiting for the socket FILE is not waiting for the daemon: `bind`
        // creates the file and `listen` comes after it, so a connect that lands
        // in between gets ECONNREFUSED. That window made
        // `the_handshake_gets_a_unique_name_from_a_real_bus` fail about one run
        // in twelve, on the `connect` of its first line, with the daemon
        // perfectly healthy. So probe with a real connection and only come back
        // once one lands; the probe is dropped straight away, which is a peer
        // that connects and goes without authenticating -- a case the daemon
        // handles and these tests do not look at.
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if std::path::Path::new(&path).exists()
                && std::os::unix::net::UnixStream::connect(&path).is_ok()
            {
                break;
            }
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

    /// A socket that answers the four things `connect` asks of a bus and then
    /// hands the stream to the test: the leading NUL and AUTH line, the OK
    /// line, AGREE_UNIX_FD, and a reply to Hello.
    ///
    /// The auth phase is TEXT, so a bus that answers something NEARLY right --
    /// `OKAY`, `AGREE_UNIX_FDX` -- is exactly what a real daemon cannot be
    /// asked to answer, and it is where this client decides whether it is
    /// talking to a bus at all. Past the handshake the test writes the bytes
    /// itself, which is the only way to put half a message on the wire or to
    /// answer a call out of order.
    fn fake_bus<F>(ok: &str, agree: &str, after: F) -> String
    where
        F: FnOnce(&mut UnixStream) + Send + 'static,
    {
        let path = format!(
            "/tmp/eclipse-dbusd-fake-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let (ok, agree) = (ok.to_string(), agree.to_string());
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let mut scratch = [0u8; 512];
            // The NUL and the AUTH line arrive together.
            let _ = s.read(&mut scratch);
            let _ = s.write_all(ok.as_bytes());
            let _ = s.read(&mut scratch); // NEGOTIATE_UNIX_FD
            let _ = s.write_all(agree.as_bytes());
            let _ = s.read(&mut scratch); // BEGIN, and maybe the Hello call
                                          // Hello is always serial 1, because `send` counts up from zero.
            let mut call = Message::method_call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Hello",
            );
            call.serial = 1;
            let mut hello = Message::method_return(&call);
            hello.serial = 1;
            hello.set_body(&[Arg::Str(":1.7".into())]);
            let _ = s.write_all(&hello.encode());
            after(&mut s);
            // Hold the socket open a moment: a close is a message of its own
            // and only the test that wants one should send it.
            std::thread::sleep(Duration::from_millis(1500));
        });
        path
    }

    #[test]
    fn a_bus_that_nearly_says_ok_is_not_a_bus() {
        // `OKAY-but-not-ok` is not `OK `, and `AGREE_UNIX_FDX` is not
        // `AGREE_UNIX_FD`. These are SASL keywords, so a prefix match would
        // take a strange server's word for it and then start speaking binary
        // D-Bus at something that never agreed to listen.
        let path = fake_bus("OKAY-but-not-ok\r\n", "AGREE_UNIX_FD\r\n", |_| {});
        let e = Client::connect(&path).err().expect("OKAY no es OK");
        assert!(e.contains("auth rejected"), "{e:?}");
        let _ = std::fs::remove_file(&path);

        let path = fake_bus("OK 1234\r\n", "AGREE_UNIX_FDX\r\n", |_| {});
        let e = Client::connect(&path)
            .err()
            .expect("AGREE_UNIX_FDX no es AGREE_UNIX_FD");
        assert!(e.contains("AGREE_UNIX_FD"), "{e:?}");
        let _ = std::fs::remove_file(&path);

        // And the handshake this same fake bus CAN complete, so the two above
        // are verdicts on the keyword and not on the harness.
        let path = fake_bus("OK 1234\r\n", "AGREE_UNIX_FD\r\n", |_| {});
        let c = Client::connect(&path).expect("connect");
        assert_eq!(c.unique, ":1.7");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_message_split_across_two_reads_is_not_a_poll_error() {
        // One `read` can bring half a message: the bus writes when it likes.
        // Half a message is not a message, and it is not an error either -- a
        // client that reported it as one would turn a slow write into a
        // session that has to be rebuilt.
        let sig = {
            let mut m = Message::signal("/p", "org.ejemplo.Senal", "Mitad");
            m.serial = 7;
            m.set_body(&[Arg::Str("entera".into())]);
            m.encode()
        };
        let half = sig.len() / 2;
        let (head, tail) = (sig[..half].to_vec(), sig[half..].to_vec());
        let path = fake_bus("OK 1234\r\n", "AGREE_UNIX_FD\r\n", move |s| {
            // Late enough that `connect` has finished and the client's buffer
            // is empty, so the half message arrives during the poll's OWN
            // read. Written any earlier it lands in the read that `connect`
            // makes for the Hello reply, and then the poll answers from the
            // buffer and never gets to the part that decides this.
            std::thread::sleep(Duration::from_millis(120));
            let _ = s.write_all(&head);
            std::thread::sleep(Duration::from_millis(200));
            let _ = s.write_all(&tail);
        });
        let mut c = Client::connect(&path).expect("connect");
        assert!(
            c.poll(Duration::from_millis(500))
                .expect("media senal no es un error")
                .is_none(),
            "media senal no es una senal"
        );
        // And once it IS in the buffer, a poll that finds only half of it
        // there says the same thing without touching the socket.
        assert!(c
            .poll(Duration::from_millis(5))
            .expect("media senal guardada tampoco es un error")
            .is_none());
        // And it survives the split: what comes back is the whole of it and
        // not the tail.
        let m = c
            .wait_for(|m| m.kind == MSG_SIGNAL)
            .expect("la senal se perdio por el camino");
        assert_eq!(m.arg0_str().as_deref(), Some("entera"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_short_poll_does_not_shorten_the_next_wait() {
        // `poll` sets the socket's read timeout to its own few milliseconds.
        // Without putting the long one back, the NEXT wait inherits them: a
        // `call` that should wait ten seconds gives up in five milliseconds,
        // and on the guest that reads as the bus not answering.
        let path = fake_bus("OK 1234\r\n", "AGREE_UNIX_FD\r\n", |s| {
            std::thread::sleep(Duration::from_millis(300));
            let mut m = Message::signal("/p", "org.ejemplo.Senal", "Tarde");
            m.serial = 9;
            let _ = s.write_all(&m.encode());
        });
        let mut c = Client::connect(&path).expect("connect");
        assert!(c.poll(Duration::from_millis(5)).expect("poll").is_none());
        // The signal lands 300 ms later, which only a wait longer than the
        // poll's five milliseconds can see.
        let m = c.recv().expect("la espera larga se quedo con los 5 ms");
        assert_eq!(m.member.as_deref(), Some("Tarde"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_reply_to_someone_elses_call_is_not_this_calls_answer() {
        // A reply is matched by SERIAL. Taking the next reply that comes along
        // is wrong whenever two are in flight -- another client's reply the
        // bus routed here, or a late answer to a call that already timed out
        // -- and the caller gets the answer to a question it never asked.
        let path = fake_bus("OK 1234\r\n", "AGREE_UNIX_FD\r\n", |s| {
            // A reply to serial 99, which nobody here is waiting for.
            let mut other = Message::method_call("a", "/a", "a.I", "Otra");
            other.serial = 99;
            let mut stray = Message::method_return(&other);
            stray.serial = 50;
            stray.set_body(&[Arg::Str("de otro".into())]);
            let _ = s.write_all(&stray.encode());
            // Then the reply to the call this client is about to make, which
            // is serial 2 because Hello was 1.
            let mut mine = Message::method_call("a", "/a", "a.I", "Mia");
            mine.serial = 2;
            let mut reply = Message::method_return(&mine);
            reply.serial = 51;
            reply.set_body(&[Arg::Str("la mia".into())]);
            let _ = s.write_all(&reply.encode());
        });
        let mut c = Client::connect(&path).expect("connect");
        let r = c.call("a", "/a", "a.I", "Mia", &[]).expect("call");
        assert_eq!(r.arg0_str().as_deref(), Some("la mia"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_bus_that_closes_the_connection_says_so() {
        // A read of zero bytes is end of stream, not a quiet moment. Read as
        // "nothing yet" it turns every wait into a spin until its deadline and
        // then blames the timeout, which is the opposite of what happened: the
        // bus is gone and the session has to be built again.
        //
        // The close waits for `connect` to come back, and that wait is the
        // test and not politeness. `fake_bus` answers the handshake on the
        // guess that BEGIN and the Hello call arrive in one read; when the
        // client is descheduled between the two writes the bus replies and
        // runs this closure first, and the shutdown reaches the client as
        // `write: Broken pipe` out of `connect`. That is a red `Unit Test`
        // naming this test for something it does not test, and it is what it
        // did on the CI runner.
        let (connected, wait) = std::sync::mpsc::channel::<()>();
        let path = fake_bus("OK 1234\r\n", "AGREE_UNIX_FD\r\n", move |s| {
            let _ = wait.recv();
            let _ = s.shutdown(std::net::Shutdown::Both);
        });
        let mut c = Client::connect(&path).expect("connect");
        let _ = connected.send(());
        let e = c.recv().expect_err("un socket cerrado no es un mensaje");
        assert!(e.contains("closed the connection"), "{e:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_name_that_is_taken_is_refused_and_never_queued() {
        // DO_NOT_QUEUE is the flag that matters: a portal that queued would
        // sit there owning nothing while every client talked to the other one,
        // and would then become the owner later, in the middle of a session,
        // without anybody asking for it.
        let path = daemon();
        let mut first = Client::connect(&path).expect("connect");
        let mut second = Client::connect(&path).expect("connect 2");
        assert!(first.request_name("org.ejemplo.Unico").expect("pedir 1"));
        assert!(
            !second.request_name("org.ejemplo.Unico").expect("pedir 2"),
            "dos duenos primarios de un nombre"
        );
        // The refusal is not a queue: when the first owner lets the name go,
        // the second does NOT inherit it.
        first
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "ReleaseName",
                &[Arg::Str("org.ejemplo.Unico".into())],
            )
            .expect("ReleaseName");
        let e = second
            .call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                &[Arg::Str("org.ejemplo.Unico".into())],
            )
            .expect_err("el nombre se quedo en cola");
        assert!(e.contains("NameHasNoOwner"), "{e:?}");
        drop((first, second));
        let _ = std::fs::remove_file(&path);
    }
}
