//! The socket against a stack that is the test: it holds the other end
//! of the channel, reads what the kernel tells it and writes what
//! arrived. Each test's stack serves sockets of its own.

extern crate std;

use super::*;
use crate::net::NET_TEST_LOCK as LOCK;

/// The process's end of the channel.
pub(super) struct Stack {
    pub(super) link: Arc<StackLink>,
    pub(super) end: Arc<Channel>,
}

/// A stack that serves sockets of its own: no test sees another's.
pub(super) fn stack() -> Stack {
    stack_of(&StackLink::new())
}

/// A stack that serves the sockets of `link`, which has none now.
pub(super) fn stack_of(link: &Arc<StackLink>) -> Stack {
    let (kernel, end) = Channel::create();
    link.attach(kernel, MAX_FOR_TESTS, STACK_ADDR)
        .expect("the stack before is gone");
    Stack {
        link: link.clone(),
        end,
    }
}

impl Stack {
    fn socket(&self) -> ServedUdpSocket {
        let s = ServedUdpSocket::new(self.link.clone(), false).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    }

    /// The next message the kernel sent, if it sent one.
    pub(super) fn told(&self) -> Option<Vec<u8>> {
        self.end.read().ok().map(|m| m.data)
    }

    /// A datagram arrived for socket `sock`.
    pub(super) fn arrives(&self, sock: u64, from: [u8; 4], port: u16, data: &[u8]) {
        let mut m = data_message();
        push_record(&mut m, sock, from, port, data);
        self.say(m);
    }

    /// An ICMP message of this type and code said that what `sock`
    /// sent to `to:port` was not delivered.
    fn undelivered(&self, sock: u64, to: [u8; 4], port: u16, kind: u8, code: u8) {
        let mut m = events_message();
        let bytes = undelivered_bytes(to, port, kind, code);
        push_event(&mut m, sock, EV_UNDELIVERED, 0, &bytes);
        self.say(m);
    }

    /// The same, of what went as IPv6 and what ICMPv6 said.
    fn undelivered6(&self, sock: u64, to: &str, port: u16, kind: u8, code: u8) {
        let to: core::net::Ipv6Addr = to.parse().unwrap();
        let mut m = events_message();
        let bytes = undelivered6_bytes(to.octets(), port, kind, code);
        push_event(&mut m, sock, EV_UNDELIVERED6, 0, &bytes);
        self.say(m);
    }

    pub(super) fn say(&self, data: Vec<u8>) {
        let msg = MessagePacket {
            data,
            handles: Vec::new(),
        };
        self.end.write(msg).unwrap();
        self.link.pump();
    }
}

pub(super) fn any(port: u16) -> Endpoint {
    Endpoint::Ip(IpEndpoint::new(
        IpAddress::Ipv4(Ipv4Address::UNSPECIFIED),
        port,
    ))
}

pub(super) fn at(addr: [u8; 4], port: u16) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(addr)), port)
}

/// A destination, as `sendto` gives it.
fn to(addr: [u8; 4], port: u16) -> Option<Endpoint> {
    Some(Endpoint::Ip(at(addr, port)))
}

pub(super) fn ip(endpoint: Endpoint) -> IpEndpoint {
    match endpoint {
        Endpoint::Ip(ip) => ip,
        _ => panic!("not an IP endpoint"),
    }
}

/// The socket's port, 0 when it has none.
fn port(s: &ServedUdpSocket) -> u16 {
    ip(Socket::endpoint(s).unwrap()).port
}

fn recv(s: &ServedUdpSocket, buf: &mut [u8]) -> (SysResult, IpEndpoint) {
    let (n, from) = async_std::task::block_on(Socket::read(s, buf));
    (n, ip(from))
}

fn peek(s: &ServedUdpSocket, buf: &mut [u8]) -> (SysResult, IpEndpoint) {
    let (n, from) = async_std::task::block_on(Socket::peek(s, buf));
    (n, ip(from))
}

fn readable(s: &ServedUdpSocket) -> bool {
    Socket::poll(s, PollEvents::IN).0
}

pub(super) const HOST: [u8; 4] = [10, 0, 9, 1];
/// The address the stack of the tests says it has.
pub(super) const STACK_ADDR: [u8; 4] = [10, 0, 9, 15];
/// What a stack over Ethernet carries: a frame's 1500 bytes less the
/// IP and UDP headers.
pub(super) const MAX_FOR_TESTS: usize = 1472;

#[test]
fn a_message_is_its_header_and_what_the_operation_adds() {
    let bind = bind_message(7, 5000, Takes::Four);
    assert_eq!(bind.len(), HEADER_LEN + 4);
    assert_eq!(&bind[..4], &[0; 4], "not a reply to any call");
    assert_eq!(parse_header(&bind), Some((OP_BIND, 7)));
    assert_eq!(&bind[16..18], &5000u16.to_le_bytes());
    // And which versions of IP the port is the socket's for.
    assert_eq!(&bind[18..], &[4, 0]);
    assert_eq!(&bind_message(7, 5000, Takes::Both)[18..], &[0, 0]);
    assert_eq!(&bind_message(7, 5000, Takes::Six)[18..], &[6, 0]);

    // An `UNDELIVERED`: where it went, then what was said of it.
    assert_eq!(EV_UNDELIVERED, 7);
    let bytes = undelivered_bytes([10, 0, 9, 1], 0x1234, 3, 3);
    assert_eq!(bytes, [10, 0, 9, 1, 0x34, 0x12, 3, 3]);
    assert_eq!(
        parse_undelivered(&bytes),
        Some((at([10, 0, 9, 1], 0x1234), 3, 3))
    );
    assert_eq!(parse_undelivered(&bytes[..7]), None);

    // An `UNDELIVERED6`: the same, with the longer address.
    assert_eq!(EV_UNDELIVERED6, 9);
    assert_eq!(WHY_UNREACHABLE6, 6);
    let there = at6("fec0::99", 0x1234);
    let IpAddress::Ipv6(addr) = there.addr else {
        unreachable!()
    };
    let bytes = undelivered6_bytes(addr.0, 0x1234, 1, 4);
    assert_eq!(bytes.len(), 20);
    assert_eq!(bytes[..16], addr.0);
    assert_eq!(bytes[16..], [0x34, 0x12, 1, 4]);
    assert_eq!(parse_undelivered6(&bytes), Some((there, 1, 4)));
    assert_eq!(parse_undelivered6(&bytes[..19]), None);
    assert_eq!(parse_undelivered6(&bytes[..8]), None);

    let send = send_message(u64::MAX, [10, 0, 9, 1], 53, 64, b"hola");
    assert_eq!(parse_header(&send), Some((OP_SEND, u64::MAX)));
    assert_eq!(parse_send(&send), Some(([10, 0, 9, 1], 53, &b"hola"[..])));
    // The hop limit and a zero between the port and the datagram.
    assert_eq!(&send[20..24], &[53, 0, 64, 0]);
    // An empty datagram is one.
    let empty = send_message(1, [1, 2, 3, 4], 9, 64, b"");
    assert_eq!(empty.len(), DATAGRAM_AT);
    assert_eq!(parse_send(&empty), Some(([1, 2, 3, 4], 9, &b""[..])));

    // A DATA is its records, one after another.
    let mut data = data_message();
    assert_eq!(parse_header(&data), Some((OP_DATA, 0)));
    assert_eq!(parse_record(&data[HEADER_LEN..]), None);
    push_record(&mut data, 7, [10, 0, 9, 1], 53, b"uno");
    push_record(&mut data, 8, [10, 0, 9, 2], 54, b"");
    let (first, rest) = parse_record(&data[HEADER_LEN..]).unwrap();
    assert_eq!(first, (7, [10, 0, 9, 1], 53, &b"uno"[..]));
    let (second, rest) = parse_record(rest).unwrap();
    assert_eq!(second, (8, [10, 0, 9, 2], 54, &b""[..]));
    assert!(rest.is_empty());
    // A record cut short, in its head or in its datagram, is none.
    assert_eq!(parse_record(&data[HEADER_LEN..HEADER_LEN + 15]), None);
    assert_eq!(parse_record(&data[HEADER_LEN..HEADER_LEN + 18]), None);

    assert_eq!(parse_header(&close_message(3)), Some((OP_CLOSE, 3)));
    assert_eq!(close_message(3).len(), HEADER_LEN);

    // What `ping`'s sockets say, written out as the stack reads it.
    assert_eq!(
        hear_message(7),
        [0, 0, 0, 0, 11, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        icmp_message(7, [10, 0, 9, 1], 64, &[8, 0]),
        [0, 0, 0, 0, 12, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 64, 0, 0, 0, 8, 0]
    );
    assert_eq!(HEARD, 0);

    // The same bytes `tools/eclipse-netstack/src/socks.rs` reads and
    // builds, written out: neither side can move a field alone.
    let mut data = data_message();
    push_record(
        &mut data,
        0x0807_0605_0403_0201,
        [10, 0, 9, 1],
        0x1234,
        b"hi",
    );
    assert_eq!(
        data,
        [
            0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 0, 9, 1,
            0x34, 0x12, 2, 0, b'h', b'i'
        ]
    );
    assert_eq!(
        send_message(7, [10, 0, 9, 1], 0x1234, 64, b"hi"),
        [
            0, 0, 0, 0, 2, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 10, 0, 9, 1, 0x34, 0x12, 64, 0, b'h',
            b'i'
        ]
    );
    assert_eq!(
        bind_message(7, 5000, Takes::Four),
        [0, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x13, 4, 0]
    );
    assert_eq!((OP_SEND, OP_CLOSE), (2, 3));

    // Too short to be a message, or to be a datagram.
    assert_eq!(parse_header(&send[..HEADER_LEN - 1]), None);
    assert_eq!(parse_send(&send[..DATAGRAM_AT - 1]), None);
    // Longer than UDP carries.
    let long = alloc::vec![0u8; DATAGRAM_AT + MAX_DATAGRAM + 1];
    assert_eq!(parse_send(&long), None);
}

#[test]
fn a_datagram_for_this_machine_goes_from_socket_to_socket() {
    let _g = LOCK.lock();
    let stack = stack();
    let here = [127, 0, 0, 1];
    let server = stack.socket();
    assert_eq!(Socket::bind(&server, any(5353)), Ok(0));
    let client = stack.socket();
    while stack.told().is_some() {}

    // The sender gets a port, which the stack is told as of any bound
    // socket, and that is all the stack hears of it.
    assert_eq!(Socket::write(&client, b"hola", to(here, 5353)), Ok(4));
    let sent_from = port(&client);
    assert_eq!(
        stack.told(),
        Some(bind_message(client.sock, sent_from, Takes::Four))
    );
    assert_eq!(stack.told(), None);
    let mut buf = [0u8; 16];
    assert!(readable(&server));
    assert_eq!(recv(&server, &mut buf), (Ok(4), at(here, sent_from)));
    assert_eq!(&buf[..4], b"hola");
    // And the answer, to a socket connected to who it asked: it hears
    // it, since it comes from there.
    async_std::task::block_on(Socket::connect(&client, to(here, 5353).unwrap())).unwrap();
    assert_eq!(
        Socket::write(&server, b"que tal", to(here, sent_from)),
        Ok(7)
    );
    assert_eq!(recv(&client, &mut buf), (Ok(7), at(here, 5353)));

    // The stack's own address is this machine too, and is where the
    // datagram then comes from.
    assert_eq!(Socket::write(&client, b"yo", to(STACK_ADDR, 5353)), Ok(2));
    assert_eq!(recv(&server, &mut buf), (Ok(2), at(STACK_ADDR, sent_from)));
    // No frame of a card to fit: longer than the stack carries goes.
    let long = alloc::vec![9u8; MAX_FOR_TESTS + 1000];
    assert_eq!(
        Socket::write(&client, &long, to(here, 5353)),
        Ok(long.len())
    );
    let mut big = alloc::vec![0u8; 4096];
    assert_eq!(recv(&server, &mut big).0, Ok(long.len()));
    // A port nobody has: lost, as a datagram is.
    assert_eq!(Socket::write(&client, b"nadie", to(here, 9)), Ok(5));
    assert_eq!(stack.told(), None);
}

#[test]
fn a_boot_that_asks_for_every_program_has_asked_for_each() {
    let _g = LOCK.lock();
    let proc = crate::process::LinuxProcess::new(rcore_fs_ramfs::RamFS::new(), 0);
    assert!(!proc.uses_user_netstack());
    // `unetuse=all`: every program is one that asked.
    use_for_all(true);
    assert!(is_for_all());
    assert!(proc.uses_user_netstack());
    use_for_all(false);
    assert!(!proc.uses_user_netstack());
    // And one that asks still has what it asked for.
    proc.use_user_netstack();
    assert!(proc.uses_user_netstack());
}

/// `addr` as an `AF_INET6` socket is given it and gives it.
pub(super) fn at6(addr: &str, port: u16) -> IpEndpoint {
    let addr: core::net::Ipv6Addr = addr.parse().unwrap();
    IpEndpoint::new(IpAddress::Ipv6(Ipv6Address(addr.octets())), port)
}

/// A `DATA6` of one datagram, as the stack writes it.
pub(super) fn data6(sock: u64, from: &str, port: u16, data: &[u8]) -> Vec<u8> {
    let from: core::net::Ipv6Addr = from.parse().unwrap();
    let mut m = header(OP_DATA6, 0, RECORD6_LEN + data.len());
    m.extend_from_slice(&sock.to_le_bytes());
    m.extend_from_slice(&from.octets());
    m.extend_from_slice(&port.to_le_bytes());
    m.extend_from_slice(&(data.len() as u16).to_le_bytes());
    m.extend_from_slice(data);
    m
}

#[test]
fn an_ipv6_datagram_socket_carries_ipv6() {
    let _g = LOCK.lock();
    let stack = stack();
    let six = |addr: &str| match at6(addr, 0).addr {
        IpAddress::Ipv6(addr) => addr,
        _ => unreachable!(),
    };
    let cards = [
        Ipv6Cidr::new(six("fe80::7"), 64),
        Ipv6Cidr::new(six("fec0::7"), 64),
    ];
    stack.say(six_message(None, &cards));

    let server = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
    FileLike::set_flags(&server, OpenFlags::NON_BLOCK).unwrap();
    assert_eq!(Socket::bind(&server, Endpoint::Ip(at6("::", 5353))), Ok(0));
    assert_eq!(
        stack.told(),
        Some(bind_message(server.sock, 5353, Takes::Both))
    );

    // The messages, as the stack reads and writes them.
    let near = six("fec0::99");
    let said = send6_message(server.sock, near.0, 53, 0, b"q");
    assert_eq!(said.len(), HEADER_LEN + 20 + 1);
    assert_eq!(parse_header(&said), Some((OP_SEND6, server.sock)));
    assert_eq!(said[16..32], near.0);
    assert_eq!(said[32..36], [53, 0, 0, 0]);
    assert_eq!(said[36..], *b"q");
    let arrived = data6(server.sock, "fec0::99", 53, b"r");
    assert_eq!(parse_header(&arrived), Some((OP_DATA6, 0)));
    assert_eq!(
        parse_record6(&arrived[HEADER_LEN..]),
        Some(((server.sock, near.0, 53, &b"r"[..]), &[][..]))
    );
    assert_eq!(parse_record6(&arrived[HEADER_LEN..arrived.len() - 1]), None);

    // To a neighbour of the card's network it goes as IPv6, and what
    // comes back is said with the address it came from.
    let there = Endpoint::Ip(at6("fec0::99", 53));
    assert_eq!(Socket::write(&server, b"q", Some(there.clone())), Ok(1));
    assert_eq!(stack.told(), Some(said));
    stack.say(arrived);
    let mut buf = [0u8; 16];
    assert_eq!(recv(&server, &mut buf), (Ok(1), at6("fec0::99", 53)));
    // To a group, to stay on the link.
    let group = Endpoint::Ip(at6("ff02::fb", 5353));
    assert_eq!(Socket::write(&server, b"g", Some(group)), Ok(1));
    let group = six("ff02::fb");
    assert_eq!(
        stack.told(),
        Some(send6_message(server.sock, group.0, 5353, 1, b"g"))
    );
    // Further than the card's networks there is no way while there
    // is no router, for a send or a connect; with one there is.
    let far = Endpoint::Ip(at6("2001:db8::1", 53));
    assert_eq!(
        Socket::write(&server, b"q", Some(far.clone())),
        Err(LxError::ENETUNREACH)
    );
    assert_eq!(
        async_std::task::block_on(Socket::connect(&server, far.clone())),
        Err(LxError::ENETUNREACH)
    );
    stack.say(six_message(Some(six("fe80::2")), &cards));
    assert_eq!(
        async_std::task::block_on(Socket::connect(&server, far.clone())),
        Ok(0)
    );
    assert_eq!(
        ip(Socket::remote_endpoint(&server).unwrap()),
        at6("2001:db8::1", 53)
    );
    assert_eq!(Socket::write(&server, b"qq", None), Ok(2));
    let far = six("2001:db8::1");
    assert_eq!(
        stack.told(),
        Some(send6_message(server.sock, far.0, 53, 0, b"qq"))
    );
    // The longest is twenty bytes shorter than IPv4's.
    let long = vec![0u8; MAX_FOR_TESTS - 20];
    assert_eq!(Socket::write(&server, &long, None), Ok(long.len()));
    assert!(stack.told().is_some());
    let longer = vec![0u8; MAX_FOR_TESTS - 19];
    assert_eq!(
        Socket::write(&server, &longer, None),
        Err(LxError::EMSGSIZE)
    );
    assert_eq!(stack.told(), None);

    // An `AF_INET` socket is not given what arrives as IPv6 for its
    // port.
    let four = stack.socket();
    assert_eq!(
        Socket::bind(&four, Endpoint::Ip(at([0, 0, 0, 0], 5354))),
        Ok(0)
    );
    stack.say(data6(four.sock, "fec0::99", 53, b"not yours"));
    assert_eq!(recv(&four, &mut buf).0, Err(LxError::EAGAIN));
    while stack.told().is_some() {}

    // An address of the card's is this machine: nothing goes to the
    // stack, and who has the port hears it from that address. A
    // socket can be bound to one, and to no other.
    let other = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
    FileLike::set_flags(&other, OpenFlags::NON_BLOCK).unwrap();
    assert_eq!(
        Socket::bind(&other, Endpoint::Ip(at6("fec0::8", 5355))),
        Err(LxError::EADDRNOTAVAIL)
    );
    assert_eq!(
        Socket::bind(&other, Endpoint::Ip(at6("fec0::7", 5355))),
        Ok(0)
    );
    assert_eq!(
        stack.told(),
        Some(bind_message(other.sock, 5355, Takes::Both))
    );
    let own = Endpoint::Ip(at6("fec0::7", 5353));
    assert_eq!(Socket::write(&other, b"here", Some(own)), Ok(4));
    assert_eq!(stack.told(), None);
    assert_eq!(recv(&server, &mut buf).0, Err(LxError::EAGAIN));
    // (The server is connected: it hears who it is connected to.)
    let third = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
    FileLike::set_flags(&third, OpenFlags::NON_BLOCK).unwrap();
    assert_eq!(Socket::bind(&third, Endpoint::Ip(at6("::", 5356))), Ok(0));
    let own = Endpoint::Ip(at6("fec0::7", 5356));
    assert_eq!(Socket::write(&other, b"here", Some(own)), Ok(4));
    assert_eq!(recv(&third, &mut buf), (Ok(4), at6("fec0::7", 5355)));
    while stack.told().is_some() {}

    // The stack dies, and its addresses with it.
    let Stack { link, end } = stack;
    drop(end);
    link.pump();
    assert_eq!(
        Socket::write(&third, b"q", Some(there)),
        Err(LxError::ENETDOWN)
    );
}

#[test]
fn an_ipv6_datagram_socket_carries_ipv4() {
    let _g = LOCK.lock();
    let stack = stack();
    let here = [127, 0, 0, 1];
    // `::` is every address, and the port is the stack's to hear on.
    let server = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
    FileLike::set_flags(&server, OpenFlags::NON_BLOCK).unwrap();
    assert_eq!(Socket::bind(&server, Endpoint::Ip(at6("::", 5353))), Ok(0));
    assert_eq!(
        stack.told(),
        Some(bind_message(server.sock, 5353, Takes::Both))
    );
    assert_eq!(ip(Socket::endpoint(&server).unwrap()), at6("::", 5353));
    // It is the IPv6 table's, with the address it has, which is none.
    let listed = stack.link.listed_datagrams();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].v6);
    assert_eq!(listed[0].local, at([0, 0, 0, 0], 5353));

    // What an IPv4 socket sends it comes from an IPv4 address, said
    // the way such a socket says one; and it answers there.
    let client = stack.socket();
    assert_eq!(Socket::write(&client, b"hola", to(here, 5353)), Ok(4));
    let sent_from = port(&client);
    let mut buf = [0u8; 16];
    let from = at6("::ffff:127.0.0.1", sent_from);
    assert_eq!(recv(&server, &mut buf), (Ok(4), from));
    assert_eq!(
        Socket::write(&server, b"que tal", Some(Endpoint::Ip(from))),
        Ok(7)
    );
    assert_eq!(recv(&client, &mut buf), (Ok(7), at(here, 5353)));
    // `::1` is this machine as well.
    assert_eq!(
        Socket::write(&server, b"yo", Some(Endpoint::Ip(at6("::1", sent_from)))),
        Ok(2)
    );
    assert_eq!(recv(&client, &mut buf), (Ok(2), at(here, 5353)));
    while stack.told().is_some() {}

    // An IPv4 address elsewhere goes to the stack as what it is, and
    // what comes from there is said as it was written.
    let there = Endpoint::Ip(at6("::ffff:10.0.9.1", 53));
    assert_eq!(Socket::write(&server, b"q", Some(there.clone())), Ok(1));
    assert_eq!(
        stack.told(),
        Some(send_message(server.sock, HOST, 53, 64, b"q"))
    );
    stack.arrives(server.sock, HOST, 53, b"r");
    stack.link.pump();
    assert_eq!(recv(&server, &mut buf), (Ok(1), at6("::ffff:10.0.9.1", 53)));
    async_std::task::block_on(Socket::connect(&server, there)).unwrap();
    assert_eq!(
        ip(Socket::remote_endpoint(&server).unwrap()),
        at6("::ffff:10.0.9.1", 53)
    );

    // The card has no IPv6 address: one that is only that is nowhere
    // it reaches and none it has. And an IPv4 address is not one an
    // `AF_INET6` socket is given.
    let other = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
    let v6 = Endpoint::Ip(at6("2001:db8::1", 53));
    assert_eq!(
        Socket::write(&other, b"q", Some(v6.clone())),
        Err(LxError::ENETUNREACH)
    );
    assert_eq!(
        async_std::task::block_on(Socket::connect(&other, v6.clone())),
        Err(LxError::ENETUNREACH)
    );
    assert_eq!(Socket::bind(&other, v6), Err(LxError::EADDRNOTAVAIL));
    assert_eq!(
        Socket::bind(&other, Endpoint::Ip(at(here, 5354))),
        Err(LxError::EAFNOSUPPORT)
    );
    // Bound to `::1` it is this machine's alone, like one bound to
    // 127.0.0.1.
    while stack.told().is_some() {}
    assert_eq!(Socket::bind(&other, Endpoint::Ip(at6("::1", 5354))), Ok(0));
    assert_eq!(stack.told(), None);
    assert_eq!(
        ip(Socket::endpoint(&other).unwrap()),
        at6("::ffff:127.0.0.1", 5354)
    );
}

#[test]
fn a_datagram_socket_bound_to_this_machine_is_not_the_stacks() {
    let _g = LOCK.lock();
    let stack = stack();
    let here = [127, 0, 0, 1];
    let server = stack.socket();
    assert_eq!(
        Socket::bind(&server, Endpoint::Ip(at([127, 0, 0, 53], 53))),
        Ok(0)
    );
    // The stack would bring it what the card hears for the port.
    assert_eq!(stack.told(), None);
    assert_eq!(
        ip(Socket::endpoint(&server).unwrap()),
        at([127, 0, 0, 53], 53)
    );

    let client = stack.socket();
    assert_eq!(Socket::write(&client, b"a", to([127, 0, 0, 53], 53)), Ok(1));
    // For another address of this machine's it is not there.
    assert_eq!(Socket::write(&client, b"b", to(here, 53)), Ok(1));
    assert_eq!(Socket::write(&client, b"c", to(STACK_ADDR, 53)), Ok(1));
    let mut buf = [0u8; 16];
    let (n, from) = recv(&server, &mut buf);
    assert_eq!((n, &buf[..1]), (Ok(1), &b"a"[..]));
    assert_eq!(recv(&server, &mut buf).0, Err(LxError::EAGAIN));
    // It answers from the address it has, and to this machine only.
    assert_eq!(
        Socket::write(&server, b"r", Some(Endpoint::Ip(from))),
        Ok(1)
    );
    assert_eq!(recv(&client, &mut buf), (Ok(1), at([127, 0, 0, 53], 53)));
    assert_eq!(
        Socket::write(&server, b"x", to(HOST, 53)),
        Err(LxError::EINVAL)
    );

    // The stack heard of the client's port and nothing else, and does
    // not hear of the server's close.
    while let Some(m) = stack.told() {
        assert_eq!(m, bind_message(client.sock, port(&client), Takes::Four));
    }
    drop(server);
    assert_eq!(stack.told(), None);
    let rebound = stack.socket();
    assert_eq!(Socket::bind(&rebound, any(53)), Ok(0));
}

#[test]
fn bind_gives_the_port_asked_for_once_and_tells_the_stack() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    let b = stack.socket();
    assert_ne!(a.sock, b.sock);
    assert_eq!(port(&a), 0);

    assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
    assert_eq!(stack.told(), Some(bind_message(a.sock, 7000, Takes::Four)));
    assert_eq!(port(&a), 7000);
    // A socket has one port, and a port one socket.
    assert_eq!(Socket::bind(&a, any(7001)), Err(LxError::EINVAL));
    assert_eq!(Socket::bind(&b, any(7000)), Err(LxError::EADDRINUSE));
    assert_eq!(stack.told(), None, "a bind refused is not told");

    // Closed, its port is free and the stack knows.
    let closed = a.sock;
    drop(a);
    assert_eq!(stack.told(), Some(close_message(closed)));
    assert_eq!(Socket::bind(&b, any(7000)), Ok(0));
    assert_eq!(stack.told(), Some(bind_message(b.sock, 7000, Takes::Four)));
}

#[test]
fn a_send_from_a_socket_without_a_port_gets_it_one_first() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    let b = stack.socket();
    assert_eq!(Socket::write(&a, b"uno", to(HOST, 9)), Ok(3));
    let port_a = port(&a);
    assert!(EPHEMERAL.contains(&port_a));
    assert_eq!(
        stack.told(),
        Some(bind_message(a.sock, port_a, Takes::Four))
    );
    assert_eq!(
        stack.told(),
        Some(send_message(a.sock, HOST, 9, 64, b"uno"))
    );
    // The second send has the port already; another socket gets another.
    assert_eq!(Socket::write(&a, b"dos", to(HOST, 9)), Ok(3));
    assert_eq!(
        stack.told(),
        Some(send_message(a.sock, HOST, 9, 64, b"dos"))
    );
    assert_eq!(Socket::write(&b, b"", to(HOST, 9)), Ok(0));
    let bind_b = stack.told().unwrap();
    assert_eq!(parse_header(&bind_b), Some((OP_BIND, b.sock)));
    assert_ne!(&bind_b[16..], &port_a.to_le_bytes());

    // No destination and no peer; a peer that is not IPv4; too long.
    assert_eq!(Socket::write(&a, b"x", None), Err(LxError::ENOTCONN));
    let v6 = Endpoint::Ip(IpEndpoint::new(
        IpAddress::Ipv6(smoltcp::wire::Ipv6Address::LOOPBACK),
        9,
    ));
    assert_eq!(
        Socket::write(&a, b"x", Some(v6)),
        Err(LxError::EAFNOSUPPORT)
    );
    // The longest the stack carries goes; one byte more does not.
    let long = alloc::vec![0u8; MAX_FOR_TESTS + 1];
    assert_eq!(
        Socket::write(&a, &long[..MAX_FOR_TESTS], to(HOST, 9)),
        Ok(MAX_FOR_TESTS)
    );
    assert_eq!(
        Socket::write(&a, &long, to(HOST, 9)),
        Err(LxError::EMSGSIZE)
    );
}

#[test]
fn ephemeral_ports_skip_the_ones_taken_and_wrap() {
    let _g = LOCK.lock();
    let stack = stack();
    let first = *EPHEMERAL.start();
    let taken = stack.socket();
    assert_eq!(Socket::bind(&taken, any(first)), Ok(0));
    let a = stack.socket();
    assert_eq!(Socket::bind(&a, any(0)), Ok(0));
    assert_eq!(port(&a), first + 1);
    // From the last of the range the next is the first one free.
    stack.link.table.lock().ports.next = *EPHEMERAL.end();
    let b = stack.socket();
    let c = stack.socket();
    assert_eq!(Socket::bind(&b, any(0)), Ok(0));
    assert_eq!(Socket::bind(&c, any(0)), Ok(0));
    assert_eq!(port(&b), *EPHEMERAL.end());
    assert_eq!(port(&c), first + 2);
}

#[test]
fn a_datagram_that_arrives_is_read_from_its_socket_with_who_sent_it() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    let b = stack.socket();
    let mut buf = [0u8; 16];
    assert!(!readable(&a));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
    assert_eq!(FileLike::readable_bytes(&a), Some(0));

    // Two datagrams of one turn of the stack, in one message.
    let mut turn = data_message();
    push_record(&mut turn, a.sock, HOST, 4000, b"primero");
    push_record(&mut turn, a.sock, [10, 0, 9, 2], 4001, b"segundo, y largo");
    stack.say(turn);
    assert!(readable(&a));
    assert!(!readable(&b), "it was not for this one");
    assert_eq!(FileLike::readable_bytes(&a), Some(7));

    // Looked at, it is still there.
    assert_eq!(peek(&a, &mut buf), (Ok(7), at(HOST, 4000)));
    assert_eq!(recv(&a, &mut buf), (Ok(7), at(HOST, 4000)));
    assert_eq!(&buf[..7], b"primero");
    assert_eq!(Socket::take_msg_flags(&a), 0);

    // A buffer too short takes what fits, says so, and the rest is gone.
    let mut short = [0u8; 7];
    assert_eq!(recv(&a, &mut short), (Ok(7), at([10, 0, 9, 2], 4001)));
    assert_eq!(&short, b"segundo");
    assert_eq!(Socket::take_msg_flags(&a), 0x20);
    assert_eq!(Socket::take_msg_flags(&a), 0);
    assert!(!readable(&a));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));

    // An empty datagram is read as one.
    stack.arrives(a.sock, HOST, 4000, b"");
    assert!(readable(&a));
    assert_eq!(recv(&a, &mut buf), (Ok(0), at(HOST, 4000)));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
}

#[test]
fn what_the_stack_says_that_is_no_datagram_for_a_socket_is_dropped() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    let gone = stack.socket();
    let gone_sock = gone.sock;
    drop(gone);
    stack.arrives(gone_sock, HOST, 1, b"late");
    stack.arrives(9999, HOST, 1, b"nobody's");
    stack.say(alloc::vec![1, 2, 3]);
    stack.say(bind_message(a.sock, 80, Takes::Four));
    stack.say(send_message(a.sock, HOST, 1, 64, b"not DATA"));
    // A DATA with nothing in it, and one whose second record is cut
    // short: the first is delivered all the same.
    stack.say(data_message());
    assert!(!readable(&a));
    let mut cut = data_message();
    push_record(&mut cut, a.sock, HOST, 1, b"whole");
    push_record(&mut cut, a.sock, HOST, 1, b"cut short");
    cut.truncate(cut.len() - 1);
    stack.say(cut);
    let mut buf = [0u8; 16];
    assert_eq!(recv(&a, &mut buf).0, Ok(5));
    assert!(!readable(&a));
    assert!(!stack.link.is_down(), "nonsense does not end the stack");
    stack.arrives(a.sock, HOST, 1, b"real");
    assert!(readable(&a));
}

#[test]
fn a_socket_holds_so_many_bytes_unread_and_drops_the_next_datagram() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    let big = alloc::vec![7u8; 60_000];
    for _ in 0..RX_QUEUE_BYTES / big.len() {
        stack.arrives(a.sock, HOST, 1, &big);
    }
    stack.arrives(a.sock, HOST, 2, &big);
    let mut buf = alloc::vec![0u8; 65536];
    for _ in 0..RX_QUEUE_BYTES / big.len() {
        assert_eq!(recv(&a, &mut buf), (Ok(big.len()), at(HOST, 1)));
    }
    assert_eq!(
        recv(&a, &mut buf).0,
        Err(LxError::EAGAIN),
        "the one past the limit"
    );
    // With room again, the next is taken.
    stack.arrives(a.sock, HOST, 3, b"after");
    assert_eq!(recv(&a, &mut buf), (Ok(5), at(HOST, 3)));
}

#[test]
fn a_connected_socket_sends_to_its_peer_and_hears_nobody_else() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    assert!(Socket::remote_endpoint(&a).is_none());
    assert_eq!(
        async_std::task::block_on(Socket::connect(&a, Endpoint::Ip(at(HOST, 7)))),
        Ok(0)
    );
    assert_eq!(Socket::remote_endpoint(&a).map(ip), Some(at(HOST, 7)));
    assert_eq!(
        parse_header(&stack.told().unwrap()),
        Some((OP_BIND, a.sock))
    );
    assert_eq!(Socket::write(&a, b"ping", None), Ok(4));
    assert_eq!(
        stack.told(),
        Some(send_message(a.sock, HOST, 7, 64, b"ping"))
    );

    stack.arrives(a.sock, [10, 0, 9, 9], 7, b"a stranger");
    stack.arrives(a.sock, HOST, 8, b"another port");
    stack.arrives(a.sock, HOST, 7, b"pong");
    let mut buf = [0u8; 16];
    assert_eq!(recv(&a, &mut buf), (Ok(4), at(HOST, 7)));
    assert_eq!(&buf[..4], b"pong");
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
}

/// The socket's `SO_ERROR`, which asking takes.
fn so_error(s: &ServedUdpSocket) -> Result<(), LxError> {
    match Socket::take_so_error(s) {
        0 => Ok(()),
        e if e == LxError::ECONNREFUSED as isize as i32 => Err(LxError::ECONNREFUSED),
        e if e == LxError::ENOPROTOOPT as isize as i32 => Err(LxError::ENOPROTOOPT),
        e if e == LxError::EMSGSIZE as isize as i32 => Err(LxError::EMSGSIZE),
        e if e == LxError::EACCES as isize as i32 => Err(LxError::EACCES),
        e if e == LxError::EPROTO as isize as i32 => Err(LxError::EPROTO),
        e => panic!("an error nobody expected: {}", e),
    }
}

/// A socket of `stack` connected to the host's port 7, with what the
/// kernel said to the stack for that read.
fn connected(stack: &Stack) -> ServedUdpSocket {
    let a = stack.socket();
    let peer = Endpoint::Ip(at(HOST, 7));
    assert_eq!(async_std::task::block_on(Socket::connect(&a, peer)), Ok(0));
    assert!(stack.told().is_some());
    a
}

#[test]
fn a_connected_socket_is_told_once_that_nobody_is_at_its_peers_port() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = connected(&stack);
    assert_eq!(Socket::write(&a, b"ping", None), Ok(4));
    assert!(stack.told().is_some());
    assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, false));

    // Nobody there. Whoever polls sees an error, and nothing to
    // read; whoever reads is told.
    stack.undelivered(a.sock, HOST, 7, 3, 3);
    assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, true));
    let mut buf = [0u8; 16];
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::ECONNREFUSED));
    // Once.
    assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, false));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));

    // Or the next write hears it, and sends nothing; the one after
    // it goes.
    stack.undelivered(a.sock, HOST, 7, 3, 3);
    assert_eq!(Socket::write(&a, b"ping", None), Err(LxError::ECONNREFUSED));
    assert_eq!(stack.told(), None);
    assert_eq!(Socket::write(&a, b"ping", None), Ok(4));
    assert_eq!(
        stack.told(),
        Some(send_message(a.sock, HOST, 7, 64, b"ping"))
    );

    // Or who asks for `SO_ERROR`.
    stack.undelivered(a.sock, HOST, 7, 3, 3);
    assert_eq!(so_error(&a), Err(LxError::ECONNREFUSED));
    assert_eq!(so_error(&a), Ok(()));
    assert_eq!(peek(&a, &mut buf).0, Err(LxError::EAGAIN));

    // It is told before what waits to be read, which is still there.
    stack.arrives(a.sock, HOST, 7, b"pong");
    stack.undelivered(a.sock, HOST, 7, 3, 3);
    assert_eq!(peek(&a, &mut buf).0, Err(LxError::ECONNREFUSED));
    assert_eq!(Socket::poll(&a, PollEvents::IN), (true, true, false));
    assert_eq!(recv(&a, &mut buf), (Ok(4), at(HOST, 7)));

    // The other errors that do not mend: no such protocol there, and
    // a datagram too long for the way.
    stack.undelivered(a.sock, HOST, 7, 3, 2);
    assert_eq!(so_error(&a), Err(LxError::ENOPROTOOPT));
    stack.undelivered(a.sock, HOST, 7, 3, 4);
    assert_eq!(so_error(&a), Err(LxError::EMSGSIZE));
}

#[test]
fn what_may_mend_or_went_somewhere_else_is_not_told() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = connected(&stack);
    // A router with no way just now, a datagram out of hops, and
    // what is no error of something sent.
    for (kind, code) in [(3, 0), (3, 1), (3, 5), (11, 0), (11, 1), (12, 0), (0, 0)] {
        stack.undelivered(a.sock, HOST, 7, kind, code);
        assert_eq!(so_error(&a), Ok(()), "type {} code {}", kind, code);
        assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, false));
    }
    // What it sent to another port or another host before it was
    // connected to this one.
    stack.undelivered(a.sock, HOST, 8, 3, 3);
    stack.undelivered(a.sock, [10, 0, 9, 9], 7, 3, 3);
    assert_eq!(so_error(&a), Ok(()));

    // A socket that is not connected is told nothing.
    let b = stack.socket();
    assert_eq!(Socket::write(&b, b"ping", to(HOST, 7)), Ok(4));
    stack.undelivered(b.sock, HOST, 7, 3, 3);
    assert_eq!(so_error(&b), Ok(()));
    let mut buf = [0u8; 16];
    assert_eq!(recv(&b, &mut buf).0, Err(LxError::EAGAIN));

    // A socket that closed, a number that is nobody's, and a record
    // that is not one.
    let gone = a.sock;
    drop(a);
    stack.undelivered(gone, HOST, 7, 3, 3);
    stack.undelivered(9999, HOST, 7, 3, 3);
    let mut m = events_message();
    push_event(&mut m, b.sock, EV_UNDELIVERED, 0, &[10, 0, 9, 1, 7, 0, 3]);
    stack.say(m);
    assert_eq!(so_error(&b), Ok(()));
}

#[test]
fn what_icmpv6_says_of_a_datagram_is_told_as_linux_tells_it() {
    let _g = LOCK.lock();
    let stack = stack();
    let six = |addr: &str| match at6(addr, 0).addr {
        IpAddress::Ipv6(addr) => addr,
        _ => unreachable!(),
    };
    stack.say(six_message(None, &[Ipv6Cidr::new(six("fec0::7"), 64)]));
    let socket = || {
        let s = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    };
    let a = socket();
    let peer = Endpoint::Ip(at6("fec0::99", 7));
    assert_eq!(async_std::task::block_on(Socket::connect(&a, peer)), Ok(0));
    assert!(stack.told().is_some());

    // Nobody at the port: told once, to who polls and reads.
    stack.undelivered6(a.sock, "fec0::99", 7, 1, 4);
    assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, true));
    let mut buf = [0u8; 16];
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::ECONNREFUSED));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
    // Or to the next write, which sends nothing.
    stack.undelivered6(a.sock, "fec0::99", 7, 1, 4);
    assert_eq!(Socket::write(&a, b"ping", None), Err(LxError::ECONNREFUSED));
    assert_eq!(stack.told(), None);
    assert_eq!(Socket::write(&a, b"ping", None), Ok(4));
    assert!(stack.told().is_some());

    // The others that do not mend: forbidden on the way, too big for
    // it, a header nobody understood, a code that means nothing.
    for (kind, code, error) in [
        (1, 1, LxError::EACCES),
        (1, 5, LxError::EACCES),
        (1, 6, LxError::EACCES),
        (1, 7, LxError::EPROTO),
        (2, 0, LxError::EMSGSIZE),
        (4, 0, LxError::EPROTO),
        (4, 1, LxError::EPROTO),
    ] {
        stack.undelivered6(a.sock, "fec0::99", 7, kind, code);
        assert_eq!(so_error(&a), Err(error), "type {} code {}", kind, code);
        assert_eq!(so_error(&a), Ok(()));
    }

    // What may mend is not told: no way just now, nobody answering
    // at the address (which the stack says of a neighbour that does
    // not answer), out of hops; nor what is no error; nor IPv4's
    // numbers for nobody at the port.
    for (kind, code) in [(1, 0), (1, 2), (1, 3), (3, 0), (3, 1), (129, 0), (3, 3)] {
        stack.undelivered6(a.sock, "fec0::99", 7, kind, code);
        assert_eq!(so_error(&a), Ok(()), "type {} code {}", kind, code);
    }
    // Nor what went to another port or to another host.
    stack.undelivered6(a.sock, "fec0::99", 8, 1, 4);
    stack.undelivered6(a.sock, "fec0::98", 7, 1, 4);
    assert_eq!(so_error(&a), Ok(()));

    // A socket that is not connected is told nothing, and a record
    // that is not one is nobody's.
    let b = socket();
    let there = Endpoint::Ip(at6("fec0::99", 7));
    assert_eq!(Socket::write(&b, b"ping", Some(there)), Ok(4));
    stack.undelivered6(b.sock, "fec0::99", 7, 1, 4);
    assert_eq!(so_error(&b), Ok(()));
    let mut m = events_message();
    push_event(&mut m, a.sock, EV_UNDELIVERED6, 0, &[0; 19]);
    stack.say(m);
    assert_eq!(so_error(&a), Ok(()));
}

#[test]
fn a_datagram_for_a_port_of_this_machine_that_nobody_has_is_refused() {
    let _g = LOCK.lock();
    let stack = stack();
    let here = [127, 0, 0, 1];
    let a = stack.socket();
    let peer = Endpoint::Ip(at(here, 9));
    assert_eq!(async_std::task::block_on(Socket::connect(&a, peer)), Ok(0));
    // It is sent, as far as the program knows, and then refused.
    assert_eq!(Socket::write(&a, b"anybody?", None), Ok(8));
    assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, true));
    let mut buf = [0u8; 16];
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::ECONNREFUSED));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));

    // A socket bound there to another address does not have it.
    let other = stack.socket();
    assert_eq!(
        Socket::bind(&other, Endpoint::Ip(at([127, 0, 0, 2], 9))),
        Ok(0)
    );
    assert_eq!(Socket::write(&a, b"anybody?", None), Ok(8));
    assert_eq!(so_error(&a), Err(LxError::ECONNREFUSED));
    assert_eq!(recv(&other, &mut buf).0, Err(LxError::EAGAIN));
    drop(other);

    // With somebody at the port there is nothing to say.
    let b = stack.socket();
    assert_eq!(Socket::bind(&b, any(9)), Ok(0));
    assert_eq!(Socket::write(&a, b"anybody?", None), Ok(8));
    assert_eq!(so_error(&a), Ok(()));
    assert_eq!(recv(&b, &mut buf).0, Ok(8));

    // And a socket that is not connected is told nothing.
    let c = stack.socket();
    assert_eq!(Socket::write(&c, b"anybody?", to(here, 10)), Ok(8));
    assert_eq!(so_error(&c), Ok(()));
    assert_eq!(recv(&c, &mut buf).0, Err(LxError::EAGAIN));
}

#[test]
fn shutdown_of_the_reading_side_reads_the_queue_and_then_the_end() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    stack.arrives(a.sock, HOST, 1, b"queued");
    assert_eq!(Socket::shutdown(&a, 3), Err(LxError::EINVAL));
    // Not connected: ENOTCONN, and the side is shut all the same.
    assert_eq!(Socket::shutdown(&a, 0), Err(LxError::ENOTCONN));
    stack.arrives(a.sock, HOST, 1, b"after");
    let mut buf = [0u8; 16];
    assert_eq!(recv(&a, &mut buf).0, Ok(6));
    assert_eq!(recv(&a, &mut buf).0, Ok(0));
    assert!(readable(&a));
}

#[test]
fn a_send_is_refused_while_the_stack_has_too_many_unread() {
    let _g = LOCK.lock();
    let stack = stack();
    let a = stack.socket();
    assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
    // The bind is one of them.
    for _ in 1..SEND_BACKLOG {
        assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Ok(1));
    }
    assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENOBUFS));
    // A close is told all the same: the stack must not keep the socket.
    let b = stack.socket();
    drop(b);
    let (channel, _) = stack.link.current().unwrap();
    assert_eq!(channel.peer_pending(), SEND_BACKLOG + 1);
    // Once the stack has read enough, a send goes through again.
    assert!(stack.told().is_some());
    assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENOBUFS));
    assert!(stack.told().is_some());
    assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Ok(1));
    let mut last = None;
    while let Some(m) = stack.told() {
        last = Some(m);
    }
    assert_eq!(last, Some(send_message(a.sock, HOST, 9, 64, b"x")));
}

#[test]
fn when_the_stack_dies_its_sockets_wait_for_the_next() {
    let _g = LOCK.lock();
    let stack = stack();
    let here = [127, 0, 0, 1];
    let a = stack.socket();
    assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
    let local = stack.socket();
    assert_eq!(Socket::bind(&local, Endpoint::Ip(at(here, 7001))), Ok(0));
    stack.arrives(a.sock, HOST, 1, b"last");
    // One stack at a time.
    let (other, _other_end) = Channel::create();
    assert_eq!(
        stack.link.attach(other, MAX_FOR_TESTS, STACK_ADDR).err(),
        Some(ZxError::ALREADY_BOUND)
    );
    // And one that carries nothing, or more than UDP does, is none.
    for bad in [0, MAX_DATAGRAM + 1] {
        let (other, _other_end) = Channel::create();
        assert_eq!(
            stack.link.attach(other, bad, STACK_ADDR).err(),
            Some(ZxError::INVALID_ARGS)
        );
    }
    assert!(!stack.link.is_down());

    let Stack { link, end } = stack;
    drop(end);
    link.pump();
    assert!(link.is_down());

    // What had arrived is read, and then there is nothing to read:
    // the socket is not in error, it waits. Nothing leaves the
    // machine, and that is said to who tries.
    let mut buf = [0u8; 8];
    assert_eq!(recv(&a, &mut buf).0, Ok(4));
    assert_eq!(recv(&a, &mut buf).0, Err(LxError::EAGAIN));
    assert_eq!(Socket::poll(&a, PollEvents::IN), (false, true, false));
    assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENETDOWN));
    // What stays in the machine does not notice.
    assert_eq!(Socket::write(&a, b"aqui", to(here, 7001)), Ok(4));
    assert_eq!(recv(&local, &mut buf), (Ok(4), at(here, 7000)));
    // And a socket is made, and takes a port, with no stack at all.
    let b = ServedUdpSocket::new(link.clone(), false).unwrap();
    FileLike::set_flags(&b, OpenFlags::NON_BLOCK).unwrap();
    assert_eq!(Socket::bind(&b, any(7002)), Ok(0));
    assert_eq!(
        Socket::bind(&stack_socket(&link), any(7002)),
        Err(LxError::EADDRINUSE)
    );

    // The next stack is told of the sockets that have a port, less
    // the one that is this machine's alone, and they are its: what
    // it hears for them arrives, and it is told when one closes.
    let next = stack_of(&link);
    assert_eq!(next.told(), Some(bind_message(a.sock, 7000, Takes::Four)));
    assert_eq!(next.told(), Some(bind_message(b.sock, 7002, Takes::Four)));
    assert_eq!(next.told(), None);
    next.arrives(a.sock, HOST, 1, b"again");
    link.pump();
    assert_eq!(recv(&a, &mut buf), (Ok(5), at(HOST, 1)));
    assert_eq!(Socket::write(&b, b"x", to(HOST, 9)), Ok(1));
    assert_eq!(next.told(), Some(send_message(b.sock, HOST, 9, 64, b"x")));
    let closed = a.sock;
    drop(a);
    assert_eq!(next.told(), Some(close_message(closed)));
}

#[test]
fn a_datagram_read_waits_as_long_as_so_rcvtimeo_says() {
    let _lock = LOCK.lock();
    let stack = stack();
    let s = stack.socket();
    assert_eq!(Socket::bind(&s, any(5300)), Ok(0));
    FileLike::set_flags(&s, OpenFlags::empty()).unwrap();

    let mut tv = [0u8; 16];
    tv[8..].copy_from_slice(&30_000i64.to_ne_bytes());
    assert_eq!(Socket::so_rcvtimeo(&s), [0u8; 16]);
    assert_eq!(Socket::setsockopt(&s, 1, 20, &tv), Ok(0));
    assert_eq!(Socket::so_rcvtimeo(&s), tv);
    // SO_SNDTIMEO is kept too, and waits for nothing: a datagram that
    // cannot be sent is refused.
    assert_eq!(Socket::setsockopt(&s, 1, 21, &tv), Ok(0));
    assert_eq!(Socket::so_sndtimeo(&s), tv);
    let mut wrong = tv;
    wrong[8..].copy_from_slice(&(-1i64).to_ne_bytes());
    assert_eq!(Socket::setsockopt(&s, 1, 20, &wrong), Err(LxError::EINVAL));
    // What it keeps nowhere is answered as before.
    assert_eq!(Socket::setsockopt(&s, 1, 6, &1u32.to_ne_bytes()), Ok(0));
    assert_eq!(
        Socket::setsockopt(&s, 6, 1, &1u32.to_ne_bytes()),
        Err(LxError::ENOPROTOOPT)
    );

    // Nothing comes: the read gives up when the time is over and not
    // before, and the socket is as it was.
    let mut buf = [0u8; 16];
    let before = kernel_hal::timer::timer_now();
    assert_eq!(recv(&s, &mut buf).0, Err(LxError::EAGAIN));
    let took = kernel_hal::timer::timer_now() - before;
    assert!(took >= core::time::Duration::from_millis(30), "{:?}", took);
    stack.arrives(s.sock, HOST, 53, b"tarde");
    assert_eq!(recv(&s, &mut buf), (Ok(5), at(HOST, 53)));
}

#[test]
fn a_question_of_the_kernels_goes_by_the_stack_and_waits_for_its_answer() {
    let _g = LOCK.lock();
    let stack = stack();
    let patience = core::time::Duration::from_secs(5);
    let waiter = {
        let link = stack.link.clone();
        std::thread::spawn(move || {
            async_std::task::block_on(async {
                let asked = ask(&link, at(HOST, 53), b"who?", patience).await?;
                let first = asked.answer().await?;
                let second = asked.answer().await?;
                Ok::<_, LxError>((first, second))
            })
        })
    };
    // It leaves as any program's datagram does: a socket with a port
    // of its own, then the datagram.
    let sock = loop {
        match stack.told() {
            Some(m) if parse_header(&m).map(|h| h.0) == Some(OP_SEND) => {
                assert_eq!(parse_send(&m), Some((HOST, 53, &b"who?"[..])));
                break parse_header(&m).unwrap().1;
            }
            _ => std::thread::yield_now(),
        }
    };
    // What somebody else sends to that port is no answer.
    stack.arrives(sock, [10, 0, 9, 77], 53, b"not who was asked");
    stack.arrives(sock, HOST, 53, b"one");
    stack.arrives(sock, HOST, 53, b"two");
    assert_eq!(
        waiter.join().unwrap(),
        Ok((b"one".to_vec(), b"two".to_vec()))
    );
    // And the socket goes with the question.
    assert_eq!(stack.told(), Some(close_message(sock)));

    // No answer is `ETIMEDOUT` when the patience runs out, and from
    // then on.
    let short = core::time::Duration::from_millis(30);
    let asked = async_std::task::block_on(ask(&stack.link, at(HOST, 53), b"again", short)).unwrap();
    let before = kernel_hal::timer::timer_now();
    assert_eq!(
        async_std::task::block_on(asked.answer()),
        Err(LxError::ETIMEDOUT)
    );
    assert!(kernel_hal::timer::timer_now() - before >= short);
    assert_eq!(
        async_std::task::block_on(asked.answer()),
        Err(LxError::ETIMEDOUT)
    );

    // What cannot be sent is said when it is asked: a server of IPv6
    // while the stack has no way to it, and no stack at all.
    let six = at6("2001:db8::1", 53);
    let refused = async_std::task::block_on(ask(&stack.link, six, b"x", short)).err();
    assert_eq!(refused, Some(LxError::ENETUNREACH));
    let nobody = StackLink::new();
    let refused = async_std::task::block_on(ask(&nobody, at(HOST, 53), b"x", short)).err();
    assert_eq!(refused, Some(LxError::ENETDOWN));

    // With a way there, who has an address of IPv6 is asked as IPv6,
    // from a socket of IPv6, and its answer is read the same.
    let (IpAddress::Ipv6(card), IpAddress::Ipv6(router), IpAddress::Ipv6(server)) =
        (at6("fec0::7", 0).addr, at6("fe80::2", 0).addr, six.addr)
    else {
        unreachable!()
    };
    stack.say(six_message(Some(router), &[Ipv6Cidr::new(card, 64)]));
    while stack.told().is_some() {}
    let asked = async_std::task::block_on(ask(&stack.link, six, b"who6?", patience)).unwrap();
    let sock = loop {
        let m = stack.told().expect("the question leaves");
        if let Some((OP_SEND6, sock)) = parse_header(&m) {
            assert_eq!(m, send6_message(sock, server.0, 53, 0, b"who6?"));
            break sock;
        }
    };
    stack.say(data6(sock, "2001:db8::9", 53, b"not who was asked"));
    stack.say(data6(sock, "2001:db8::1", 53, b"six"));
    assert_eq!(
        async_std::task::block_on(asked.answer()),
        Ok(b"six".to_vec())
    );
}

#[test]
fn a_datagram_travels_as_many_hops_as_its_socket_was_told() {
    let _g = LOCK.lock();
    let stack = stack();
    let s = stack_socket(&stack.link);
    const IPPROTO_IP: usize = 0;
    const IP_TTL: usize = 2;
    const IP_MULTICAST_TTL: usize = 33;
    let group = [224, 0, 0, 251];
    let sent = |whom: [u8; 4], data: &[u8]| {
        assert_eq!(Socket::write(&s, data, to(whom, 9)), Ok(data.len()));
        loop {
            let m = stack.told().unwrap();
            if parse_header(&m).map(|h| h.0) == Some(OP_SEND) {
                break m;
            }
        }
    };
    // As Linux has them before anybody says: 64, and 1 to a group.
    assert_eq!(Socket::ip_ttl(&s), 64);
    assert_eq!(sent(HOST, b"a"), send_message(s.sock, HOST, 9, 64, b"a"));
    assert_eq!(sent(group, b"b"), send_message(s.sock, group, 9, 1, b"b"));

    // What `traceroute` does: one hop, then two.
    assert_eq!(
        Socket::setsockopt(&s, IPPROTO_IP, IP_TTL, &1u32.to_ne_bytes()),
        Ok(0)
    );
    assert_eq!(Socket::ip_ttl(&s), 1);
    assert_eq!(sent(HOST, b"c"), send_message(s.sock, HOST, 9, 1, b"c"));
    assert_eq!(
        Socket::setsockopt(&s, IPPROTO_IP, IP_TTL, &2u32.to_ne_bytes()),
        Ok(0)
    );
    assert_eq!(sent(HOST, b"d"), send_message(s.sock, HOST, 9, 2, b"d"));
    // One byte is an option too, and 255 the most.
    assert_eq!(Socket::setsockopt(&s, IPPROTO_IP, IP_TTL, &[255]), Ok(0));
    assert_eq!(sent(HOST, b"e"), send_message(s.sock, HOST, 9, 255, b"e"));

    // A group's datagrams have their own, and the others keep theirs.
    let ttl = 5u32.to_ne_bytes();
    assert_eq!(
        Socket::setsockopt(&s, IPPROTO_IP, IP_MULTICAST_TTL, &ttl),
        Ok(0)
    );
    assert_eq!(Socket::ip_multicast_ttl(&s), 5);
    assert_eq!(sent(group, b"f"), send_message(s.sock, group, 9, 5, b"f"));
    assert_eq!(sent(HOST, b"g"), send_message(s.sock, HOST, 9, 255, b"g"));

    // The other options a program sets are the ones it reads back.
    const SOL_SOCKET: usize = 1;
    const SO_BROADCAST: usize = 6;
    assert!(!Socket::so_broadcast(&s));
    assert_eq!(
        Socket::setsockopt(&s, SOL_SOCKET, SO_BROADCAST, &1u32.to_ne_bytes()),
        Ok(0)
    );
    assert!(Socket::so_broadcast(&s));
}

#[test]
fn a_datagram_travels_as_ipv6_as_many_hops_as_its_socket_was_told() {
    let _g = LOCK.lock();
    let stack = stack();
    let six = |addr: &str| match at6(addr, 0).addr {
        IpAddress::Ipv6(addr) => addr,
        _ => unreachable!(),
    };
    stack.say(six_message(None, &[Ipv6Cidr::new(six("fec0::7"), 64)]));
    let s = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
    const IPPROTO_IPV6: usize = 41;
    const IPV6_UNICAST_HOPS: usize = 16;
    const IPV6_MULTICAST_HOPS: usize = 18;
    const IPV6_V6ONLY: usize = 26;
    let (far, group) = (six("2001:db8::5").0, six("ff02::fb").0);
    stack.say(six_message(
        Some(six("fe80::99")),
        &[Ipv6Cidr::new(six("fec0::7"), 64)],
    ));
    let sent = |whom: [u8; 16], data: &[u8]| {
        let there = IpEndpoint::new(IpAddress::Ipv6(Ipv6Address(whom)), 33434);
        assert_eq!(
            Socket::write(&s, data, Some(Endpoint::Ip(there))),
            Ok(data.len())
        );
        loop {
            let m = stack.told().unwrap();
            if parse_header(&m).map(|h| h.0) == Some(OP_SEND6) {
                break m;
            }
        }
    };
    let said =
        |whom: [u8; 16], hops: u8, data: &[u8]| send6_message(s.sock, whom, 33434, hops, data);
    let set =
        |opt: usize, hops: i32| Socket::setsockopt(&s, IPPROTO_IPV6, opt, &hops.to_ne_bytes());
    let get = |opt: usize| {
        let said = Socket::getsockopt_own(&s, IPPROTO_IPV6, opt)?;
        Some(i32::from_ne_bytes(said.try_into().unwrap()))
    };
    // Before anybody says: the stack's own, and 1 to a group; asked,
    // the socket says what that is.
    assert_eq!(sent(far, b"a"), said(far, 0, b"a"));
    assert_eq!(sent(group, b"b"), said(group, 1, b"b"));
    assert_eq!(get(IPV6_UNICAST_HOPS), Some(64));
    assert_eq!(get(IPV6_MULTICAST_HOPS), Some(1));

    // What `traceroute -6` does: one hop, then two.
    assert_eq!(set(IPV6_UNICAST_HOPS, 1), Ok(0));
    assert_eq!(sent(far, b"c"), said(far, 1, b"c"));
    assert_eq!(get(IPV6_UNICAST_HOPS), Some(1));
    assert_eq!(set(IPV6_UNICAST_HOPS, 2), Ok(0));
    assert_eq!(sent(far, b"d"), said(far, 2, b"d"));
    // A group's datagrams have their own, and the others keep theirs.
    assert_eq!(sent(group, b"e"), said(group, 1, b"e"));
    assert_eq!(set(IPV6_MULTICAST_HOPS, 5), Ok(0));
    assert_eq!(sent(group, b"f"), said(group, 5, b"f"));
    assert_eq!(sent(far, b"g"), said(far, 2, b"g"));
    assert_eq!(get(IPV6_MULTICAST_HOPS), Some(5));
    // 255 is the most, and -1 is as when nothing was said.
    assert_eq!(set(IPV6_UNICAST_HOPS, 255), Ok(0));
    assert_eq!(sent(far, b"h"), said(far, 255, b"h"));
    assert_eq!(set(IPV6_UNICAST_HOPS, -1), Ok(0));
    assert_eq!(set(IPV6_MULTICAST_HOPS, -1), Ok(0));
    assert_eq!(sent(far, b"i"), said(far, 0, b"i"));
    assert_eq!(sent(group, b"j"), said(group, 1, b"j"));
    assert_eq!(get(IPV6_UNICAST_HOPS), Some(64));
    // What is no number of hops, or not an `int`.
    for opt in [IPV6_UNICAST_HOPS, IPV6_MULTICAST_HOPS] {
        assert_eq!(set(opt, 256), Err(LxError::EINVAL));
        assert_eq!(set(opt, -2), Err(LxError::EINVAL));
        assert_eq!(
            Socket::setsockopt(&s, IPPROTO_IPV6, opt, &[1, 0]),
            Err(LxError::EINVAL)
        );
    }
    assert_eq!(sent(far, b"k"), said(far, 0, b"k"));

    // The socket takes both families, and says so; what it does not
    // keep it does not answer for.
    assert_eq!(get(IPV6_V6ONLY), Some(0));
    assert_eq!(get(51), None, "IPV6_RECVHOPLIMIT");
    assert_eq!(get(8), None, "IPV6_2292HOPLIMIT");
    assert_eq!(get(1000), None);
    assert_eq!(Socket::getsockopt_own(&s, 58, 1), None);

    // IPv6's are no options of an `AF_INET` socket.
    let four = stack_socket(&stack.link);
    let hops = 3i32.to_ne_bytes();
    assert_eq!(
        Socket::setsockopt(&four, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &hops),
        Err(LxError::ENOPROTOOPT)
    );
    assert_eq!(
        Socket::getsockopt_own(&four, IPPROTO_IPV6, IPV6_UNICAST_HOPS),
        None
    );
}

/// A socket of `link` that does not wait.
#[test]
fn the_datagram_sockets_that_have_a_port_are_listed() {
    let _g = LOCK.lock();
    let stack = stack();
    // No port, not in the table; nor a socket that pings, which has
    // an identifier and no port of UDP's.
    let fresh = stack.socket();
    let pings = ServedIcmpSocket::new(stack.link.clone(), false).unwrap();
    assert_eq!(stack.link.listed_datagrams(), []);

    let a = stack.socket();
    assert_eq!(Socket::bind(&a, any(7000)), Ok(0));
    let local = stack.socket();
    assert_eq!(
        Socket::bind(&local, Endpoint::Ip(at([127, 0, 0, 1], 7001))),
        Ok(0)
    );
    let row = |local, remote: Option<IpEndpoint>, unread| Listed {
        local,
        remote,
        state: if remote.is_some() {
            TCP_ESTABLISHED
        } else {
            TCP_CLOSE
        },
        unread,
        v6: false,
    };
    let here = row(at([127, 0, 0, 1], 7001), None, 0);
    assert_eq!(
        stack.link.listed_datagrams(),
        [row(at([0, 0, 0, 0], 7000), None, 0), here]
    );
    // What arrived and nobody read is counted, and a peer is shown.
    stack.arrives(a.sock, HOST, 53, b"hello");
    stack.arrives(a.sock, HOST, 53, b"you");
    assert_eq!(
        stack.link.listed_datagrams(),
        [row(at([0, 0, 0, 0], 7000), None, 8), here]
    );
    let mut buf = [0u8; 8];
    assert_eq!(recv(&a, &mut buf).0, Ok(5));
    let connected = async_std::task::block_on(Socket::connect(&a, Endpoint::Ip(at(HOST, 53))));
    assert_eq!(connected, Ok(0));
    assert_eq!(
        stack.link.listed_datagrams(),
        [row(at([0, 0, 0, 0], 7000), Some(at(HOST, 53)), 3), here]
    );
    // Closed, it is gone from the table; and the table outlives the
    // stack, as the sockets do.
    drop(a);
    assert_eq!(stack.link.listed_datagrams(), [here]);
    let Stack { link, end } = stack;
    drop(end);
    link.pump();
    assert_eq!(link.listed_datagrams(), [here]);
    drop((fresh, pings, local));
    assert_eq!(link.listed_datagrams(), []);
}

#[test]
fn what_the_card_has_carried_is_what_its_stacks_said_one_after_another() {
    let _g = LOCK.lock();
    let stack = stack();
    let carried = stack.link.carried();
    let shown = |carried: &Carried| {
        let c = carried.so_far();
        (c.rx_bytes, c.rx_packets, c.tx_bytes, c.tx_packets)
    };
    assert_eq!(shown(&carried), (0, 0, 0, 0));

    // The message, as the stack writes it.
    let said = counts_message(&[600, 4, 1, 2, 420, 3, 5, 6]);
    assert_eq!(said.len(), HEADER_LEN + COUNTS * 8);
    assert_eq!(parse_header(&said), Some((OP_COUNTS, 0)));
    assert_eq!(&said[16..24], &600u64.to_le_bytes());
    assert_eq!(&said[48..56], &420u64.to_le_bytes());
    stack.say(said);
    let c = carried.so_far();
    assert_eq!((c.rx_bytes, c.rx_packets), (600, 4));
    assert_eq!((c.rx_errors, c.rx_dropped), (1, 2));
    assert_eq!((c.tx_bytes, c.tx_packets), (420, 3));
    assert_eq!((c.tx_errors, c.tx_dropped), (5, 6));

    // Said again, nothing more; said since the start, so what is new
    // is the difference; and a number that goes back adds nothing.
    stack.say(counts_message(&[600, 4, 1, 2, 420, 3, 5, 6]));
    assert_eq!(shown(&carried), (600, 4, 420, 3));
    stack.say(counts_message(&[700, 5, 1, 2, 420, 3, 5, 6]));
    assert_eq!(shown(&carried), (700, 5, 420, 3));
    stack.say(counts_message(&[100, 1, 0, 0, 500, 4, 5, 6]));
    assert_eq!(shown(&carried), (700, 5, 500, 4));
    // What a later stack adds after the eight is not read, and fewer
    // than eight are no counts.
    let mut longer = counts_message(&[160, 2, 0, 0, 500, 4, 5, 6]);
    longer.extend_from_slice(&[0xff; 8]);
    stack.say(longer);
    assert_eq!(shown(&carried), (760, 6, 500, 4));
    let mut short = counts_message(&[9000, 9000, 0, 0, 9000, 9000, 0, 0]);
    short.truncate(HEADER_LEN + COUNTS * 8 - 1);
    stack.say(short);
    assert_eq!(shown(&carried), (760, 6, 500, 4));

    // The stack dies. The interface has carried what it had, and the
    // next stack counts from nothing: what it says is all new.
    let Stack { link, end } = stack;
    drop(end);
    link.pump();
    assert!(link.is_down());
    assert_eq!(shown(&carried), (760, 6, 500, 4));
    let next = stack_of(&link);
    next.say(counts_message(&[40, 1, 0, 0, 50, 2, 0, 0]));
    assert_eq!(shown(&link.carried()), (800, 7, 550, 6));
    assert!(Arc::ptr_eq(&carried, &link.carried()));
}

#[test]
fn the_cards_ipv6_addresses_are_what_its_stack_said_last() {
    let _g = LOCK.lock();
    let stack = stack();
    let sixes = stack.link.sixes();
    assert_eq!((sixes.addresses(), sixes.router()), (vec![], None));

    // The message, as the stack writes it.
    let link = Ipv6Cidr::new(Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 7), 64);
    let given = Ipv6Cidr::new(Ipv6Address::new(0xfec0, 0, 0, 0, 0, 0, 0, 7), 64);
    let router = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
    let said = six_message(Some(router), &[link, given]);
    assert_eq!(said.len(), HEADER_LEN + 16 + 2 * 24);
    assert_eq!(parse_header(&said), Some((OP_SIX, 0)));
    assert_eq!(said[16..32], router.0);
    assert_eq!(said[32..48], link.address().0);
    assert_eq!(said[48..56], [64, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(said[56..72], given.address().0);
    assert_eq!(six_message(None, &[link])[16..32], [0; 16]);

    stack.say(six_message(None, &[link]));
    assert_eq!((sixes.addresses(), sixes.router()), (vec![link], None));
    stack.say(said.clone());
    assert_eq!(sixes.addresses(), [link, given]);
    assert_eq!(sixes.router(), Some(router));

    // What is not whole, or names what no card has, changes nothing.
    let group = Ipv6Cidr::new(Ipv6Address::new(0xff02, 0, 0, 0, 0, 0, 0, 1), 64);
    let mut wide = six_message(Some(router), &[link]);
    wide[HEADER_LEN + 16 + 16] = 129;
    let many = [link; SIX_ADDRESSES_MAX + 1];
    for mut wrong in [
        said[..said.len() - 1].to_vec(),
        said[..HEADER_LEN + 15].to_vec(),
        six_message(Some(router), &[group]),
        wide,
        six_message(Some(group.address()), &[link]),
        six_message(Some(router), &many),
    ] {
        stack.say(core::mem::take(&mut wrong));
        assert_eq!(sixes.addresses(), [link, given]);
        assert_eq!(sixes.router(), Some(router));
    }

    // The router is gone and an address with it.
    stack.say(six_message(None, &[link]));
    assert_eq!((sixes.addresses(), sixes.router()), (vec![link], None));

    // The stack dies: its card had them, and the next says its own.
    stack.say(said);
    let Stack { link: served, end } = stack;
    drop(end);
    served.pump();
    assert!(served.is_down());
    assert_eq!((sixes.addresses(), sixes.router()), (vec![], None));
    let next = stack_of(&served);
    next.say(six_message(None, &[given]));
    assert_eq!(served.sixes().addresses(), [given]);
    assert!(Arc::ptr_eq(&sixes, &served.sixes()));
}

fn stack_socket(link: &Arc<StackLink>) -> ServedUdpSocket {
    let s = ServedUdpSocket::new(link.clone(), false).unwrap();
    FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
    s
}

#[test]
fn a_stack_found_dead_by_a_write_is_gone() {
    let _g = LOCK.lock();
    let Stack { link, end } = stack();
    let a = stack_socket(&link);
    drop(end);
    assert_eq!(Socket::write(&a, b"x", to(HOST, 9)), Err(LxError::ENETDOWN));
    assert!(link.is_down());
    // And the next one finds it out by itself when nobody pumped.
    let Stack { link, end } = stack();
    drop(end);
    assert!(!link.is_down());
    let _next = stack_of(&link);
    assert!(!link.is_down());
}

#[test]
fn the_machine_has_sockets_before_it_has_a_stack() {
    let _g = LOCK.lock();
    let link = StackLink::new();
    assert!(link.is_down());
    let here = [127, 0, 0, 1];
    // Two programs talk with nobody serving anything.
    let server = stack_socket(&link);
    assert_eq!(Socket::bind(&server, any(5353)), Ok(0));
    let client = stack_socket(&link);
    assert_eq!(Socket::write(&client, b"hola", to(here, 5353)), Ok(4));
    let mut buf = [0u8; 8];
    assert_eq!(recv(&server, &mut buf).0, Ok(4));
    // Outside there is no way yet.
    assert_eq!(
        Socket::write(&client, b"x", to(HOST, 9)),
        Err(LxError::ENETDOWN)
    );
    // The stack comes and is told of both, each with its port.
    let stack = stack_of(&link);
    assert_eq!(
        stack.told(),
        Some(bind_message(server.sock, 5353, Takes::Four))
    );
    assert_eq!(
        stack.told(),
        Some(bind_message(client.sock, port(&client), Takes::Four))
    );
    assert_eq!(Socket::write(&client, b"x", to(HOST, 9)), Ok(1));
    assert_eq!(
        stack.told(),
        Some(send_message(client.sock, HOST, 9, 64, b"x"))
    );
}

/// `IPV6_V6ONLY`, said `on` or not.
fn only6(s: &dyn Socket, on: i32) -> SysResult {
    s.setsockopt(41, 26, &on.to_ne_bytes())
}

#[test]
fn a_socket_for_ipv6_only_leaves_its_port_to_one_of_ipv4() {
    let _g = LOCK.lock();
    let stack = stack();
    let six = |addr: &str| match at6(addr, 0).addr {
        IpAddress::Ipv6(addr) => addr,
        _ => unreachable!(),
    };
    stack.say(six_message(None, &[Ipv6Cidr::new(six("fec0::7"), 64)]));
    let socket6 = || {
        let s = ServedUdpSocket::new(stack.link.clone(), true).unwrap();
        FileLike::set_flags(&s, OpenFlags::NON_BLOCK).unwrap();
        s
    };
    let said = |s: &ServedUdpSocket| Socket::getsockopt_own(s, 41, 26);
    let yes = Some(1i32.to_ne_bytes().to_vec());
    let no = Some(0i32.to_ne_bytes().to_vec());

    // Said before the socket has a port, by a socket of that family, as
    // an `int` (`do_ipv6_setsockopt`).
    let v6only = socket6();
    assert_eq!(said(&v6only), no);
    assert_eq!(v6only.setsockopt(41, 26, &[1, 0]), Err(LxError::EINVAL));
    assert_eq!(only6(&v6only, 1), Ok(0));
    assert_eq!(said(&v6only), yes);
    assert_eq!(only6(&v6only, 0), Ok(0));
    assert_eq!(said(&v6only), no);
    assert_eq!(only6(&v6only, 1), Ok(0));
    let v4 = stack.socket();
    assert_eq!(only6(&v4, 1), Err(LxError::ENOPROTOOPT));

    // An address of IPv4 is none of its own, nor anywhere it has a way
    // to, however it is written.
    let mapped = Endpoint::Ip(at6("::ffff:10.0.9.1", 53));
    assert_eq!(Socket::bind(&v6only, mapped.clone()), Err(LxError::EINVAL));
    assert_eq!(
        Socket::write(&v6only, b"q", Some(mapped.clone())),
        Err(LxError::ENETUNREACH)
    );
    assert_eq!(
        async_std::task::block_on(Socket::connect(&v6only, mapped)),
        Err(LxError::ENETUNREACH)
    );
    assert_eq!(port(&v6only), 0, "none of that gave it a port");

    // Its port is its own for IPv6, and the stack is told so: a socket
    // of IPv4 has the same one, and no third has it for either.
    assert_eq!(Socket::bind(&v6only, Endpoint::Ip(at6("::", 7000))), Ok(0));
    assert_eq!(
        stack.told(),
        Some(bind_message(v6only.sock, 7000, Takes::Six))
    );
    assert_eq!(Socket::bind(&v4, any(7000)), Ok(0));
    assert_eq!(stack.told(), Some(bind_message(v4.sock, 7000, Takes::Four)));
    let both = socket6();
    let another = socket6();
    assert_eq!(only6(&another, 1), Ok(0));
    for third in [&both, &another, &stack.socket()] {
        assert_eq!(
            Socket::bind(
                third,
                Endpoint::Ip(shown(third.shared.takes(), at([0; 4], 7000)))
            ),
            Err(LxError::EADDRINUSE)
        );
    }
    // With a port, what it carries is said.
    assert_eq!(only6(&v6only, 0), Err(LxError::EINVAL));
    assert_eq!(only6(&v6only, 1), Err(LxError::EINVAL));
    assert_eq!(said(&v6only), yes);

    // What arrives is the one's whose version of IP it arrived as,
    // whatever a stack says.
    let mut buf = [0u8; 16];
    stack.arrives(v6only.sock, HOST, 53, b"four");
    assert_eq!(recv(&v6only, &mut buf).0, Err(LxError::EAGAIN));
    stack.say(data6(v4.sock, "fec0::99", 53, b"six"));
    assert_eq!(recv(&v4, &mut buf).0, Err(LxError::EAGAIN));
    stack.say(data6(v6only.sock, "fec0::99", 53, b"six"));
    assert_eq!(recv(&v6only, &mut buf), (Ok(3), at6("fec0::99", 53)));
    stack.arrives(v4.sock, HOST, 53, b"four");
    assert_eq!(recv(&v4, &mut buf), (Ok(4), at(HOST, 53)));

    // In this machine too: what is sent to `::1` is for the one, which
    // is told it came from there and answers there, and what is sent to
    // 127.0.0.1 is for the other.
    assert_eq!(Socket::bind(&both, Endpoint::Ip(at6("::", 7001))), Ok(0));
    let there = Endpoint::Ip(at6("::1", 7000));
    assert_eq!(Socket::write(&both, b"six", Some(there.clone())), Ok(3));
    assert_eq!(recv(&v6only, &mut buf), (Ok(3), at6("::1", 7001)));
    assert_eq!(recv(&v4, &mut buf).0, Err(LxError::EAGAIN));
    let back = Endpoint::Ip(at6("::1", 7001));
    assert_eq!(Socket::write(&v6only, b"si", Some(back)), Ok(2));
    assert_eq!(recv(&both, &mut buf).0, Ok(2));
    // Connected, the same.
    async_std::task::block_on(Socket::connect(&both, there)).unwrap();
    assert_eq!(Socket::write(&both, b"more", None), Ok(4));
    assert_eq!(recv(&v6only, &mut buf), (Ok(4), at6("::1", 7001)));
    let client = stack.socket();
    assert_eq!(
        Socket::write(&client, b"four", to([127, 0, 0, 1], 7000)),
        Ok(4)
    );
    assert_eq!(recv(&v4, &mut buf).0, Ok(4));
    assert_eq!(recv(&v6only, &mut buf).0, Err(LxError::EAGAIN));

    // A stack that comes later is told of each with what it is for.
    while stack.told().is_some() {}
    let Stack { link, end } = stack;
    drop(end);
    link.pump();
    let next = stack_of(&link);
    let mut told = Vec::new();
    while let Some(m) = next.told() {
        told.push(m);
    }
    assert!(told.contains(&bind_message(v4.sock, 7000, Takes::Four)));
    assert!(told.contains(&bind_message(v6only.sock, 7000, Takes::Six)));
    assert!(told.contains(&bind_message(both.sock, 7001, Takes::Both)));
    let binds = |told: &[Vec<u8>]| {
        let is_bind = |m: &&Vec<u8>| parse_header(m).is_some_and(|(op, _)| op == OP_BIND);
        told.iter().filter(is_bind).count()
    };
    assert_eq!(binds(&told), 4, "each once: those and the client");

    // The one of IPv4 gone, the port is free for IPv4 and no more: with
    // nobody there, 127.0.0.1 is refused, and a socket for both cannot
    // have it while the one for IPv6 does.
    drop(v4);
    assert_eq!(
        Socket::bind(&another, Endpoint::Ip(at6("::", 7000))),
        Err(LxError::EADDRINUSE)
    );
    let fresh = ServedUdpSocket::new(link.clone(), false).unwrap();
    assert_eq!(Socket::bind(&fresh, any(7000)), Ok(0));
    drop(fresh);
    drop(v6only);
    assert_eq!(Socket::bind(&another, Endpoint::Ip(at6("::", 7000))), Ok(0));
}
