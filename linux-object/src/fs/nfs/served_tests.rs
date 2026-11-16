//! The transport against a stack that is the test: it holds the other end
//! of the link's channel and says what a stack process would.

extern crate std;

use alloc::vec::Vec;
use core::convert::TryInto;
use std::time::Duration;

use zircon_object::ipc::{Channel, MessagePacket};

use super::*;
use crate::net::user_stack::{
    events_message, push_event, Serving, EV_BYTES, EV_CONNECTED, EV_CREDIT, EV_EOF, EV_FAILED,
    HEADER_LEN, OP_CONNECT, OP_SHUTDOWN, OP_WRITE, WHY_REFUSED,
};
use crate::net::NET_TEST_LOCK as LOCK;

/// The first socket of a link that had none.
const SOCK: u64 = 1;
const SERVER: [u8; 4] = [10, 0, 9, 1];

/// The process's end of the channel.
struct Stack {
    link: Arc<StackLink>,
    end: Arc<Channel>,
    _serving: Serving,
}

fn stack() -> Stack {
    let link = StackLink::new();
    let (kernel, end) = Channel::create();
    let serving = link.attach(kernel, 1472, [10, 0, 9, 15]).unwrap();
    Stack {
        link,
        end,
        _serving: serving,
    }
}

/// `records`, in one message, left in the channel: nobody has read it.
fn said(end: &Channel, records: &[(u64, u16, u32, &[u8])]) {
    let mut data = events_message();
    for &(sock, what, number, bytes) in records {
        push_event(&mut data, sock, what, number, bytes);
    }
    end.write(MessagePacket {
        data,
        handles: Vec::new(),
    })
    .unwrap();
}

impl Stack {
    fn says(&self, records: &[(u64, u16, u32, &[u8])]) {
        said(&self.end, records);
    }

    /// The same, a while from now, from another thread: the stack is
    /// another process, and answers while the client waits.
    fn says_later(&self, ms: u64, what: u16, number: u32, bytes: &'static [u8]) {
        let end = self.end.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            said(&end, &[(SOCK, what, number, bytes)]);
        });
    }

    /// The operations the kernel asked for, in order, and each one's
    /// bytes after the header.
    fn told(&self) -> Vec<(u32, Vec<u8>)> {
        let mut all = Vec::new();
        while let Ok(m) = self.end.read() {
            let op = u32::from_le_bytes(m.data[4..8].try_into().unwrap());
            all.push((op, m.data[HEADER_LEN..].to_vec()));
        }
        all
    }

    fn connect(&self, timeout_ms: u64) -> LxResult<ServedTransport> {
        ServedTransport::connect(
            self.link.clone(),
            IpAddress::v4(SERVER[0], SERVER[1], SERVER[2], SERVER[3]),
            2049,
            timeout_ms,
        )
    }

    /// A connection the stack made at once, with `credit` bytes to send.
    fn connected(&self, credit: u32) -> ServedTransport {
        self.says(&[(SOCK, EV_CONNECTED, credit, &[])]);
        let t = self.connect(1000).unwrap();
        assert_eq!(self.told()[0].0, OP_CONNECT);
        t
    }
}

#[test]
fn a_connection_the_stack_made_is_made() {
    let _g = LOCK.lock();
    let stack = stack();
    stack.says(&[(SOCK, EV_CONNECTED, 1000, &[])]);
    assert!(stack.connect(1000).is_ok());
    let told = stack.told();
    assert_eq!(told[0].0, OP_CONNECT);
    assert_eq!(&told[0].1[..6], &[10, 0, 9, 1, 0x01, 0x08]);
}

#[test]
fn a_connection_made_while_waiting_is_made() {
    let _g = LOCK.lock();
    let stack = stack();
    stack.says_later(30, EV_CONNECTED, 1000, &[]);
    assert!(stack.connect(5000).is_ok());
}

#[test]
fn a_refused_connection_says_so() {
    let _g = LOCK.lock();
    let stack = stack();
    stack.says(&[(SOCK, EV_FAILED, WHY_REFUSED, &[])]);
    assert_eq!(stack.connect(1000).err(), Some(LxError::ECONNREFUSED));
}

#[test]
fn a_stack_that_does_not_answer_is_not_waited_for_ever() {
    let _g = LOCK.lock();
    let stack = stack();
    let before = std::time::Instant::now();
    assert_eq!(stack.connect(40).err(), Some(LxError::ETIMEDOUT));
    assert!(before.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_link_with_no_stack_connects_to_nobody() {
    let _g = LOCK.lock();
    let link = StackLink::new();
    let to = IpAddress::v4(10, 0, 9, 1);
    assert!(ServedTransport::connect(link, to, 2049, 1000).is_err());
}

#[test]
fn what_arrived_is_read_and_nothing_is_not_the_end() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(1000);
    let mut buf = [0u8; 16];
    assert_eq!(t.recv(&mut buf), Ok(0));
    stack.says(&[(SOCK, EV_BYTES, 0, b"hola")]);
    t.poll();
    assert_eq!(t.recv(&mut buf), Ok(4));
    assert_eq!(&buf[..4], b"hola");
    assert_eq!(t.recv(&mut buf), Ok(0));
}

#[test]
fn a_wait_ends_when_bytes_arrive() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(1000);
    let mut buf = [0u8; 16];
    // Empty reads lengthen the wait, as a caller's loop does.
    for _ in 0..8 {
        assert_eq!(t.recv(&mut buf), Ok(0));
    }
    stack.says_later(20, EV_BYTES, 0, b"tarde");
    let mut got = 0;
    for _ in 0..10_000 {
        t.poll();
        got = t.recv(&mut buf).unwrap();
        if got > 0 {
            break;
        }
    }
    assert_eq!(&buf[..got], b"tarde");
}

#[test]
fn the_end_of_the_stream_is_a_closed_connection() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(1000);
    stack.says(&[(SOCK, EV_BYTES, 0, b"fin"), (SOCK, EV_EOF, 0, &[])]);
    t.poll();
    let mut buf = [0u8; 16];
    assert_eq!(t.recv(&mut buf), Ok(3));
    assert_eq!(t.recv(&mut buf), Err(TransportError::Closed));
}

#[test]
fn what_is_sent_reaches_the_stack_whole() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(1000);
    assert_eq!(t.send(b"una llamada"), Ok(()));
    let written: Vec<u8> = stack
        .told()
        .into_iter()
        .filter(|(op, _)| *op == OP_WRITE)
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(written, b"una llamada");
}

#[test]
fn a_send_with_no_room_waits_for_the_stack_to_give_some() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(4);
    stack.says_later(20, EV_CREDIT, 100, &[]);
    assert_eq!(t.send(b"mas de cuatro"), Ok(()));
    let written: Vec<u8> = stack
        .told()
        .into_iter()
        .filter(|(op, _)| *op == OP_WRITE)
        .flat_map(|(_, bytes)| bytes)
        .collect();
    assert_eq!(written, b"mas de cuatro");
}

#[test]
fn a_reset_is_told_as_one() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(1000);
    stack.says(&[(SOCK, EV_FAILED, crate::net::user_stack::WHY_RESET, &[])]);
    t.poll();
    let mut buf = [0u8; 16];
    assert_eq!(t.recv(&mut buf), Err(TransportError::Reset));
}

#[test]
fn a_connection_given_up_carries_nothing_more() {
    let _g = LOCK.lock();
    let stack = stack();
    let mut t = stack.connected(1000);
    stack.told();
    t.give_up();
    assert!(stack.told().iter().any(|(op, _)| *op == OP_SHUTDOWN));
    let mut buf = [0u8; 16];
    assert_eq!(t.send(b"x"), Err(TransportError::NotConnected));
    assert_eq!(t.recv(&mut buf), Err(TransportError::NotConnected));
}
