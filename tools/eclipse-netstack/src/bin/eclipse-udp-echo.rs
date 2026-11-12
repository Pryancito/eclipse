//! `eclipse-udp-echo PORT`: send every datagram that arrives on UDP port
//! `PORT` back to who sent it.
//!
//! A test program, and an ordinary one: `socket`, `bind`, `recvfrom`,
//! `sendto`, with nothing of Eclipse's in it. It is what the stack that
//! runs as a process is measured with against the kernel's
//! (`scripts/udp_ping.py --udp PORT`): started as it is, its socket is the
//! kernel's; started by `eclipse-netstack-use`, the process's.
//!
//! Exit status: 2 for a command line that is not one, 1 when the port
//! cannot be had or the socket fails.

use std::net::UdpSocket;
use std::process::exit;

const NAME: &str = "eclipse-udp-echo";

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
    let socket = UdpSocket::bind(("0.0.0.0", port)).unwrap_or_else(|e| {
        println!("{NAME}: port {port}: {e}");
        exit(1);
    });
    println!("{NAME}: echoing on port {port}");
    // The longest datagram there is.
    let mut datagram = vec![0u8; 65536];
    loop {
        let sent = socket
            .recv_from(&mut datagram)
            .and_then(|(len, from)| socket.send_to(&datagram[..len], from));
        if let Err(e) = sent {
            println!("{NAME}: {e}");
            exit(1);
        }
    }
}
