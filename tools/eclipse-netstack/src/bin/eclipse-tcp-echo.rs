//! `eclipse-tcp-echo PORT [sink|source]`: take connections on TCP port
//! `PORT` and send back on each whatever arrives on it.
//!
//! With `sink` it only reads what arrives, and with `source` it only
//! writes, until the peer has enough: one direction of a connection at a
//! time, which is how throughput is told apart from an echo's.
//!
//! A test program, and an ordinary one: `socket`, `bind`, `listen`,
//! `accept`, `read`, `write`, a thread for each connection, with nothing
//! of Eclipse's in it. It is what the stack that runs as a process is
//! measured with against the kernel's (`scripts/tcp_bench.py`): started as
//! it is, its sockets are the kernel's; started by `eclipse-netstack-use`,
//! the process's.
//!
//! Exit status: 2 for a command line that is not one, 1 when the port
//! cannot be had or no more connections can be taken.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::exit;

const NAME: &str = "eclipse-tcp-echo";

/// Send back what arrives until the peer says it will send no more, and
/// then say the same.
fn echo(mut conn: TcpStream) {
    let mut bytes = vec![0u8; 65536];
    loop {
        match conn.read(&mut bytes) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if conn.write_all(&bytes[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

/// Read what arrives until the peer says it will send no more.
fn sink(mut conn: TcpStream) {
    let mut bytes = vec![0u8; 65536];
    while matches!(conn.read(&mut bytes), Ok(n) if n > 0) {}
}

/// The bytes a source sends, over and over: what `scripts/tcp_bench.py`
/// expects, so that it can tell they arrived as they were sent.
fn pattern() -> Vec<u8> {
    (0..65536usize).map(|i| (i * 7) as u8).collect()
}

/// Write until the peer takes no more.
fn source(mut conn: TcpStream) {
    let bytes = pattern();
    while conn.write_all(&bytes).is_ok() {}
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let args: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let (port, serve, doing): (_, fn(TcpStream), _) = match args[..] {
        [port] => (port, echo, "echoing"),
        [port, "sink"] => (port, sink, "reading"),
        [port, "source"] => (port, source, "writing"),
        _ => ("", echo, ""),
    };
    let Some(port) = port.parse::<u16>().ok().filter(|&p| p != 0) else {
        println!("usage: {NAME} PORT [sink|source]");
        exit(2);
    };
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| {
        println!("{NAME}: port {port}: {e}");
        exit(1);
    });
    println!("{NAME}: {doing} on port {port}");
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                std::thread::spawn(move || serve(conn));
            }
            Err(e) => {
                println!("{NAME}: {e}");
                exit(1);
            }
        }
    }
}
