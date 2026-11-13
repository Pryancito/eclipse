//! `eclipse-tcp-bench ADDR PORT echo COUNT SIZE | connect COUNT | send MB |
//! recv MB`: time TCP against `eclipse-tcp-echo` from inside the machine.
//!
//! `scripts/tcp_bench.py` does this from the host, through the card. This
//! is the same from a program of the guest, which is the only way to time
//! what never reaches a card: two programs of one machine talking at
//! 127.0.0.1.
//!
//! - `echo COUNT SIZE`: one connection, `COUNT` times `SIZE` bytes there
//!   and back, to `eclipse-tcp-echo PORT`.
//! - `connect COUNT`: `COUNT` connections, one after another, each with
//!   one echo of 56 bytes, to the same.
//! - `send MB`: that many megabytes to `eclipse-tcp-echo PORT sink`,
//!   timed until the other end has read them all.
//! - `recv MB`: that many megabytes from `eclipse-tcp-echo PORT source`,
//!   checked against what a source sends.
//!
//! One line of result, `RESULT what number unit`, after a line that says
//! it in words. A test program, and an ordinary one, like the server:
//! started as it is, its sockets are the kernel's; started by
//! `eclipse-netstack-use`, the process's.
//!
//! Exit status: 2 for a command line that is not one, 1 when the server
//! is not there or did not answer as it should.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::process::exit;
use std::time::{Duration, Instant};

const NAME: &str = "eclipse-tcp-bench";
const USAGE: &str =
    "usage: eclipse-tcp-bench ADDR PORT echo COUNT SIZE | connect COUNT | send MB | recv MB";
const MB: usize = 1024 * 1024;
/// How long to wait for anything.
const PATIENCE: Duration = Duration::from_secs(60);

fn fail(what: impl std::fmt::Display) -> ! {
    println!("{NAME}: {what}");
    exit(1)
}

fn connect(to: (Ipv4Addr, u16)) -> TcpStream {
    let conn = TcpStream::connect(to).unwrap_or_else(|e| fail(format_args!("connect: {e}")));
    // One segment for each write, when it is written. Not every stack has
    // the option, and the ones that do not have nothing to turn off.
    let _ = conn.set_nodelay(true);
    let _ = conn.set_read_timeout(Some(PATIENCE));
    let _ = conn.set_write_timeout(Some(PATIENCE));
    conn
}

/// `bytes` there and back on `conn`.
fn trip(conn: &mut TcpStream, bytes: &[u8], back: &mut [u8]) {
    conn.write_all(bytes)
        .and_then(|()| conn.read_exact(back))
        .unwrap_or_else(|e| fail(format_args!("echo: {e}")));
    if bytes != back {
        fail("echo: what came back is not what was sent");
    }
}

/// The middle one, the mean, and the one nine in ten are under.
fn summary(mut micros: Vec<u64>) -> (u64, u64, u64) {
    micros.sort_unstable();
    let n = micros.len();
    let mean = micros.iter().sum::<u64>() / n as u64;
    (micros[n / 2], mean, micros[n * 9 / 10])
}

fn report(what: &str, said: &str, micros: Vec<u64>) {
    let (median, mean, p90) = summary(micros);
    println!("{said}: median {median} us, mean {mean} us, nine in ten under {p90} us");
    println!("RESULT {what} {median} us");
}

fn echo(to: (Ipv4Addr, u16), count: usize, size: usize) {
    let mut conn = connect(to);
    let bytes: Vec<u8> = (0..size).map(|i| (i * 13) as u8).collect();
    let mut back = vec![0u8; size];
    let mut micros = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        trip(&mut conn, &bytes, &mut back);
        micros.push(start.elapsed().as_micros() as u64);
    }
    let said = format!("{count} echoes of {size} bytes on one connection");
    report(&format!("echo-{size}B"), &said, micros);
}

fn connections(to: (Ipv4Addr, u16), count: usize) {
    let bytes = [0x5au8; 56];
    let mut back = [0u8; 56];
    let mut micros = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let mut conn = connect(to);
        trip(&mut conn, &bytes, &mut back);
        drop(conn);
        micros.push(start.elapsed().as_micros() as u64);
    }
    let said = format!("{count} connections with an echo of 56 bytes each");
    report("connect", &said, micros);
}

fn megabytes(what: &str, said: &str, bytes: usize, took: Duration) {
    let rate = bytes as f64 / MB as f64 / took.as_secs_f64();
    println!("{said}: {bytes} bytes in {:.3} s", took.as_secs_f64());
    println!("RESULT {what} {rate:.2} MB/s");
}

fn send(to: (Ipv4Addr, u16), mb: usize) {
    let mut conn = connect(to);
    let bytes = vec![0xa5u8; 65536];
    let start = Instant::now();
    let mut left = mb * MB;
    while left > 0 {
        let n = left.min(bytes.len());
        conn.write_all(&bytes[..n])
            .unwrap_or_else(|e| fail(format_args!("send: {e}")));
        left -= n;
    }
    // Sent is not taken: the sink reads to the end and closes, and this
    // end hears that when it has.
    let _ = conn.shutdown(Shutdown::Write);
    let mut end = [0u8; 1];
    match conn.read(&mut end) {
        Ok(0) => {}
        Ok(_) => fail("send: the other end is not a sink"),
        Err(e) => fail(format_args!("send: {e}")),
    }
    megabytes(
        "send",
        &format!("{mb} MB to a program that only reads"),
        mb * MB,
        start.elapsed(),
    );
}

fn recv(to: (Ipv4Addr, u16), mb: usize) {
    let mut conn = connect(to);
    let mut bytes = vec![0u8; 65536];
    let start = Instant::now();
    let (mut got, total) = (0usize, mb * MB);
    while got < total {
        let room = (total - got).min(bytes.len());
        let n = match conn.read(&mut bytes[..room]) {
            Ok(0) => fail("recv: the other end closed early"),
            Ok(n) => n,
            Err(e) => fail(format_args!("recv: {e}")),
        };
        // What `eclipse-tcp-echo PORT source` writes, over and over.
        if bytes[..n]
            .iter()
            .enumerate()
            .any(|(i, &b)| b != (((got + i) % 65536) * 7) as u8)
        {
            fail("recv: what arrived is not what a source sends");
        }
        got += n;
    }
    megabytes(
        "recv",
        &format!("{mb} MB from a program that only writes"),
        got,
        start.elapsed(),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let number = |word: &str| word.parse::<usize>().ok().filter(|&n| n > 0);
    let to = match args[..] {
        [addr, port, ..] => addr.parse::<Ipv4Addr>().ok().zip(port.parse::<u16>().ok()),
        _ => None,
    };
    let (Some(to), Some(test)) = (to, args.get(2..)) else {
        println!("{USAGE}");
        exit(2);
    };
    // A count or a size that is no number, or is zero, is no test.
    let numbers: Option<Vec<usize>> = test.iter().skip(1).map(|word| number(word)).collect();
    match (test.first().copied(), numbers.as_deref()) {
        (Some("echo"), Some(&[count, size])) => echo(to, count, size),
        (Some("connect"), Some(&[count])) => connections(to, count),
        (Some("send"), Some(&[mb])) => send(to, mb),
        (Some("recv"), Some(&[mb])) => recv(to, mb),
        _ => {
            println!("{USAGE}");
            exit(2);
        }
    }
}
