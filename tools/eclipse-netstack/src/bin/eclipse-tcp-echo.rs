//! `eclipse-tcp-echo PORT`: take connections on TCP port `PORT` and send
//! back on each whatever arrives on it.
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port = match &args[1..] {
        [port] => port.parse::<u16>().ok().filter(|&p| p != 0),
        _ => None,
    };
    let Some(port) = port else {
        println!("usage: {NAME} PORT");
        exit(2);
    };
    let listener = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| {
        println!("{NAME}: port {port}: {e}");
        exit(1);
    });
    println!("{NAME}: echoing on port {port}");
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                std::thread::spawn(move || echo(conn));
            }
            Err(e) => {
                println!("{NAME}: {e}");
                exit(1);
            }
        }
    }
}
