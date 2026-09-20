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
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;

        // The protocol starts with a single NUL byte on the socket, before any
        // auth line (it is what lets fd passing be negotiated at all).
        sock.write_all(&[0u8]).map_err(|e| e.to_string())?;
        let uid = unsafe { libc::getuid() };
        let hex: String = format!("{uid}")
            .bytes()
            .map(|b| format!("{b:02x}"))
            .collect();
        sock.write_all(format!("AUTH EXTERNAL {hex}\r\n").as_bytes())
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
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[..pos]).to_string();
                self.buf.drain(..pos + 2);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
        loop {
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
