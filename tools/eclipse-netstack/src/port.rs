//! The stack's side of a card that a driver serves: the device smoltcp
//! reads frames from and writes them to.
//!
//! The driver and the stack share a window, cut into slots of
//! [`BUF_SIZE`] bytes -- receive slots first, then transmit slots -- and a
//! FIFO of [`Msg`]s says what is in them (`udrivers::net` has the protocol,
//! which is the one the kernel speaks in `loader/src/linux/net_serve.rs`).
//! A [`Port`] is the bookkeeping: which receive slots hold a frame not yet
//! read, which transmit slots are free, and the messages to send on the
//! next turn. It makes no system call, so it runs on any memory.
//!
//! Frames are not copied. smoltcp reads a received frame where the card
//! wrote it and builds the answer where the card will read it; a receive
//! slot goes back to the card with the turn's messages, after the stack is
//! done with it.

use std::cell::RefCell;
use std::collections::VecDeque;

use smoltcp::phy::{self, DeviceCapabilities};
use smoltcp::time::Instant;
use udrivers::net::{Msg, BUF_SIZE, FRAME_MAX, OP_RX, OP_TX, TX_HEADROOM};

// A frame of any length fits a transmit slot past its headroom.
const _: () = assert!(TX_HEADROOM + FRAME_MAX <= BUF_SIZE);

/// The most transmit slots: one bit each in a `u32`.
pub const MAX_TX_SLOTS: usize = 32;
/// The most receive slots a window is taken with.
pub const MAX_RX_SLOTS: usize = 1024;

/// Where in the window the received frame a message names is, or `None`
/// when it names a place outside its slot or a frame no card delivers.
pub fn rx_place(m: &Msg, rx_slots: usize) -> Option<usize> {
    let (slot, len, off) = (m.slot as usize, m.len as usize, m.off as usize);
    if slot >= rx_slots || len == 0 || len > FRAME_MAX || off > BUF_SIZE || len > BUF_SIZE - off {
        return None;
    }
    Some(slot * BUF_SIZE + off)
}

struct State {
    /// Frames the card delivered that the stack has not read: where each
    /// is in the window, its length, and its slot.
    incoming: VecDeque<(usize, usize, u32)>,
    /// What to tell the driver on the next turn: receive slots it can have
    /// back and frames to send.
    outgoing: Vec<Msg>,
    /// One bit per free transmit slot.
    tx_free: u32,
}

/// A served card, from the stack's side.
pub struct Port {
    window: *mut u8,
    rx_slots: usize,
    tx_slots: usize,
    state: RefCell<State>,
}

impl Port {
    /// The port of a window with `rx_slots` receive slots followed by
    /// `tx_slots` transmit slots. `None` when a card cannot have that many.
    ///
    /// # Safety
    /// `window` is `(rx_slots + tx_slots) * BUF_SIZE` bytes, readable and
    /// writable for as long as the port lives, and nothing else in this
    /// process reads or writes them.
    pub unsafe fn new(window: *mut u8, rx_slots: usize, tx_slots: usize) -> Option<Port> {
        if !(1..=MAX_RX_SLOTS).contains(&rx_slots) || !(1..=MAX_TX_SLOTS).contains(&tx_slots) {
            return None;
        }
        Some(Port {
            window,
            rx_slots,
            tx_slots,
            state: RefCell::new(State {
                incoming: VecDeque::new(),
                outgoing: Vec::new(),
                tx_free: ((1u64 << tx_slots) - 1) as u32,
            }),
        })
    }

    /// How many messages a turn can have to send: every slot once.
    pub fn slots(&self) -> usize {
        self.rx_slots + self.tx_slots
    }

    /// A message from the driver: a frame that arrived, or a transmit slot
    /// the card is done with. `false` for one that means nothing, which is
    /// dropped.
    pub fn take(&self, m: &Msg) -> bool {
        let mut state = self.state.borrow_mut();
        match m.op {
            OP_RX => match rx_place(m, self.rx_slots) {
                Some(at) => state.incoming.push_back((at, m.len as usize, m.slot)),
                None => return false,
            },
            OP_TX if (m.slot as usize) < self.tx_slots => state.tx_free |= 1 << m.slot,
            _ => return false,
        }
        true
    }

    /// Move the messages for the driver to the front of `buf`: how many.
    /// What does not fit waits for the next turn.
    pub fn outgoing(&self, buf: &mut [Msg]) -> usize {
        let mut state = self.state.borrow_mut();
        let n = state.outgoing.len().min(buf.len());
        for (to, from) in buf.iter_mut().zip(state.outgoing.drain(..n)) {
            *to = from;
        }
        n
    }

    /// Whether a frame waits to be sent.
    pub fn has_frames(&self) -> bool {
        self.state.borrow().outgoing.iter().any(|m| m.op == OP_TX)
    }

    /// Whether what waits for the driver is worth waking it for: a frame
    /// to send, or a quarter of the receive slots to give back. A slot
    /// that waits goes with the next frame, and until then the card has
    /// the other three quarters to receive into.
    pub fn due(&self) -> bool {
        self.state.borrow().outgoing.len() >= (self.rx_slots / 4).max(1) || self.has_frames()
    }

    /// Messages [`outgoing`](Self::outgoing) gave that did not go: they
    /// are the first of the next turn, in the order they were.
    pub fn put_back(&self, msgs: &[Msg]) {
        self.state
            .borrow_mut()
            .outgoing
            .splice(0..0, msgs.iter().copied());
    }

    /// How many frames wait for the stack.
    pub fn waiting(&self) -> usize {
        self.state.borrow().incoming.len()
    }

    /// Whether a frame could be sent now.
    pub fn can_send(&self) -> bool {
        self.state.borrow().tx_free != 0
    }
}

// SAFETY: the window is this port's alone (`Port::new`), so the port can
// go to whichever thread has it. It is not `Sync`: the stack's threads
// take turns with it under a lock.
unsafe impl Send for Port {}

/// A frame that arrived, where the card left it.
pub struct RxToken<'a> {
    frame: &'a mut [u8],
}

/// Leave to send one frame.
pub struct TxToken<'a> {
    port: &'a Port,
}

impl<'a> phy::Device<'a> for Port {
    type RxToken = RxToken<'a>;
    type TxToken = TxToken<'a>;

    fn receive(&'a mut self) -> Option<(RxToken<'a>, TxToken<'a>)> {
        let port: &'a Port = self;
        let mut state = port.state.borrow_mut();
        // No frame without room to answer it: smoltcp answers some frames
        // on the spot and counts the answer as sent whatever becomes of it
        // (see `ServedDriver` in `drivers/src/net/served.rs`). The frame
        // waits in its slot until a transmit slot comes back.
        if state.tx_free == 0 {
            return None;
        }
        let (at, len, slot) = state.incoming.pop_front()?;
        // The slot goes back with this turn's messages, which leave when
        // the stack has finished with every frame of the turn.
        state.outgoing.push(Msg {
            op: OP_RX,
            slot,
            len: 0,
            off: 0,
        });
        // SAFETY: `rx_place` put `at..at + len` inside one receive slot of
        // the window, and a slot is in `incoming` once, so no other
        // reference to these bytes exists.
        let frame = unsafe { core::slice::from_raw_parts_mut(port.window.add(at), len) };
        Some((RxToken { frame }, TxToken { port }))
    }

    fn transmit(&'a mut self) -> Option<TxToken<'a>> {
        let port: &'a Port = self;
        port.can_send().then_some(TxToken { port })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = FRAME_MAX;
        // As the kernel's served interface: no burst size, which smoltcp
        // would clamp the TCP window with.
        caps.max_burst_size = None;
        caps
    }
}

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F>(self, _timestamp: Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        f(self.frame)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F>(self, _timestamp: Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let port = self.port;
        if len == 0 || len > FRAME_MAX {
            return Err(smoltcp::Error::Exhausted);
        }
        let slot = {
            let mut state = port.state.borrow_mut();
            if state.tx_free == 0 {
                // Never a frame reported sent that was not: `Exhausted` is
                // what makes smoltcp emit it again.
                return Err(smoltcp::Error::Exhausted);
            }
            let slot = state.tx_free.trailing_zeros();
            state.tx_free &= !(1 << slot);
            slot
        };
        let at = (port.rx_slots + slot as usize) * BUF_SIZE + TX_HEADROOM;
        // SAFETY: a transmit slot of the window, past its headroom, and
        // `len` fits what is left of it; the slot was free, so the card is
        // not reading it and no other reference to it exists.
        let frame = unsafe { core::slice::from_raw_parts_mut(port.window.add(at), len) };
        // What the last frame in this slot left must not go out with one
        // that does not write every byte.
        frame.fill(0);
        // The closure only writes the frame: the port is not borrowed
        // while it runs.
        let result = f(frame);
        let mut state = port.state.borrow_mut();
        match result {
            Ok(result) => {
                state.outgoing.push(Msg {
                    op: OP_TX,
                    slot,
                    len: len as u32,
                    off: TX_HEADROOM as u32,
                });
                Ok(result)
            }
            Err(e) => {
                state.tx_free |= 1 << slot;
                Err(e)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A card that is memory on the heap, and THEIRS, the machine at the
    //! other end of its wire: what the tests of the port and of the
    //! sockets over it are written with.

    use super::*;
    use smoltcp::iface::{Interface, InterfaceBuilder, NeighborCache, Routes};
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::socket::SocketSet;
    use smoltcp::wire::{
        ArpOperation, ArpPacket, ArpRepr, DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress,
        EthernetFrame, EthernetProtocol, EthernetRepr, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr,
        IpProtocol, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket, TcpRepr,
        TcpSeqNumber, UdpPacket, UdpRepr,
    };
    use std::collections::BTreeMap;

    pub const RX: usize = 4;
    pub const TX: usize = 2;
    pub const OURS: EthernetAddress = EthernetAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x57]);
    pub const THEIRS: EthernetAddress = EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
    pub const OUR_IP: Ipv4Address = Ipv4Address([10, 0, 9, 15]);
    pub const THEIR_IP: Ipv4Address = Ipv4Address([10, 0, 9, 1]);

    /// The window of a bench's card, as the card's side sees it.
    #[derive(Clone, Copy)]
    pub struct Window(*mut u8);

    impl Window {
        /// The card delivers `frame` in receive slot `slot`, ten bytes in,
        /// as behind a legacy virtio header.
        pub fn deliver(&self, port: &Port, slot: u32, frame: &[u8]) {
            let at = slot as usize * BUF_SIZE + 10;
            // SAFETY: inside receive slot `slot` of the window.
            unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), self.0.add(at), frame.len()) };
            assert!(port.take(&Msg {
                op: OP_RX,
                slot,
                len: frame.len() as u32,
                off: 10,
            }));
        }

        /// The frame a transmit message names.
        pub fn sent(&self, m: &Msg) -> Vec<u8> {
            assert_eq!(m.op, OP_TX);
            assert_eq!(m.off as usize, TX_HEADROOM);
            assert!((m.slot as usize) < TX);
            let at = (RX + m.slot as usize) * BUF_SIZE + TX_HEADROOM;
            // SAFETY: inside transmit slot `m.slot` of the window.
            unsafe { core::slice::from_raw_parts(self.0.add(at), m.len as usize) }.to_vec()
        }

        /// The card is done with what message `m` of a turn told it: a
        /// frame to send is sent and its slot free again. A receive slot
        /// given back needs no answer.
        pub fn done(&self, port: &Port, m: &Msg) {
            if m.op == OP_TX {
                assert!(port.take(&Msg {
                    len: 0,
                    off: 0,
                    ..*m
                }));
            }
        }
    }

    /// This turn's messages of `port`.
    pub fn turn(port: &Port) -> Vec<Msg> {
        let mut buf = [Msg::default(); 96];
        let n = port.outgoing(&mut buf);
        buf[..n].to_vec()
    }

    /// A window on the heap, and a stack with our address over it.
    pub struct Bench {
        pub window: Window,
        pub iface: Interface<'static, Port>,
        pub sockets: SocketSet<'static>,
    }

    impl Bench {
        pub fn new() -> Bench {
            // Not zeroes: what a slot held before must not reach the wire.
            let window = Box::leak(vec![0xa5u8; (RX + TX) * BUF_SIZE].into_boxed_slice());
            let window = window.as_mut_ptr();
            // SAFETY: the window above, leaked, and this port's alone.
            let port = unsafe { Port::new(window, RX, TX) }.unwrap();
            let iface = InterfaceBuilder::new(port)
                .ethernet_addr(OURS)
                .neighbor_cache(NeighborCache::new(BTreeMap::new()))
                .ip_addrs(vec![IpCidr::new(IpAddress::Ipv4(OUR_IP), 24)])
                .routes(Routes::new(BTreeMap::new()))
                .finalize();
            Bench {
                window: Window(window),
                iface,
                sockets: SocketSet::new(vec![]),
            }
        }

        pub fn deliver(&mut self, slot: u32, frame: &[u8]) {
            self.window.deliver(self.iface.device(), slot, frame);
        }

        pub fn poll(&mut self) {
            let _ = self.iface.poll(&mut self.sockets, Instant::from_millis(1));
        }

        /// THEIRS asks who has our address, as whoever pings does first:
        /// smoltcp learns a neighbour from that and from nothing else, and
        /// a ping from a stranger gets a question, not its answer. The
        /// card then finishes with the reply, so the bench is as it began.
        pub fn meet(&mut self) {
            self.deliver(RX as u32 - 1, &arp_request(OUR_IP));
            self.poll();
            let turn = self.turn();
            assert_eq!(turn.len(), 2);
            self.window.done(self.iface.device(), &turn[1]);
        }

        /// This turn's messages.
        pub fn turn(&mut self) -> Vec<Msg> {
            turn(self.iface.device())
        }

        /// The frame a transmit message names.
        pub fn sent(&self, m: &Msg) -> Vec<u8> {
            self.window.sent(m)
        }
    }

    pub fn rx_back(slot: u32) -> Msg {
        Msg {
            op: OP_RX,
            slot,
            len: 0,
            off: 0,
        }
    }

    fn arp(operation: ArpOperation, to: EthernetAddress, target: Ipv4Address) -> Vec<u8> {
        arp_from(THEIR_IP, operation, to, target)
    }

    fn arp_from(
        from: Ipv4Address,
        operation: ArpOperation,
        to: EthernetAddress,
        target: Ipv4Address,
    ) -> Vec<u8> {
        let arp = ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr: THEIRS,
            source_protocol_addr: from,
            target_hardware_addr: if to == EthernetAddress::BROADCAST {
                EthernetAddress([0; 6])
            } else {
                to
            },
            target_protocol_addr: target,
        };
        let eth = EthernetRepr {
            src_addr: THEIRS,
            dst_addr: to,
            ethertype: EthernetProtocol::Arp,
        };
        let mut bytes = vec![0u8; eth.buffer_len() + arp.buffer_len()];
        let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
        eth.emit(&mut frame);
        arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
        bytes
    }

    /// "Who has `target`?", from THEIRS to everybody.
    pub fn arp_request(target: Ipv4Address) -> Vec<u8> {
        arp(ArpOperation::Request, EthernetAddress::BROADCAST, target)
    }

    /// "I have THEIR_IP", from THEIRS to us.
    pub fn arp_reply() -> Vec<u8> {
        arp(ArpOperation::Reply, OURS, OUR_IP)
    }

    /// "I have `from`", from THEIRS to us: the card of THEIRS answering for
    /// another address, as a gateway's does.
    pub fn arp_reply_from(from: Ipv4Address) -> Vec<u8> {
        arp_from(from, ArpOperation::Reply, OURS, OUR_IP)
    }

    /// Whether `frame` is us asking everybody who has `target`.
    pub fn is_arp_request_for(frame: &[u8], target: Ipv4Address) -> bool {
        let Ok(frame) = EthernetFrame::new_checked(frame) else {
            return false;
        };
        let asked = ArpPacket::new_checked(frame.payload())
            .ok()
            .and_then(|p| ArpRepr::parse(&p).ok());
        frame.ethertype() == EthernetProtocol::Arp
            && matches!(
                asked,
                Some(ArpRepr::EthernetIpv4 {
                    operation: ArpOperation::Request,
                    source_hardware_addr: OURS,
                    source_protocol_addr: OUR_IP,
                    target_protocol_addr,
                    ..
                }) if target_protocol_addr == target
            )
    }

    /// An IPv4 packet from THEIRS to `to`, in its frame: `fill` writes
    /// what it carries, `len` bytes of `protocol`.
    fn ipv4_frame(
        to: Ipv4Address,
        protocol: IpProtocol,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Vec<u8> {
        ipv4_frame_for(OURS, to, protocol, len, fill)
    }

    /// The same, to the card `card`: ours, or everybody's.
    fn ipv4_frame_for(
        card: EthernetAddress,
        to: Ipv4Address,
        protocol: IpProtocol,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Vec<u8> {
        let ip = Ipv4Repr {
            src_addr: THEIR_IP,
            dst_addr: to,
            protocol,
            payload_len: len,
            hop_limit: 64,
        };
        let eth = EthernetRepr {
            src_addr: THEIRS,
            dst_addr: card,
            ethertype: EthernetProtocol::Ipv4,
        };
        let mut bytes = vec![0u8; eth.buffer_len() + ip.buffer_len() + len];
        let mut frame = EthernetFrame::new_unchecked(&mut bytes[..]);
        eth.emit(&mut frame);
        let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
        ip.emit(&mut packet, &ChecksumCapabilities::default());
        fill(packet.payload_mut());
        bytes
    }

    /// An echo request from THEIRS to `to` with this payload.
    pub fn echo_request(to: Ipv4Address, ident: u16, seq_no: u16, data: &[u8]) -> Vec<u8> {
        let icmp = Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        };
        ipv4_frame(to, IpProtocol::Icmp, icmp.buffer_len(), |payload| {
            icmp.emit(
                &mut Icmpv4Packet::new_unchecked(payload),
                &ChecksumCapabilities::default(),
            )
        })
    }

    /// A UDP datagram from THEIRS, port `from`, to our port `to`.
    pub fn udp_frame(from: u16, to: u16, data: &[u8]) -> Vec<u8> {
        let udp = UdpRepr {
            src_port: from,
            dst_port: to,
        };
        ipv4_frame(OUR_IP, IpProtocol::Udp, 8 + data.len(), |payload| {
            udp.emit(
                &mut UdpPacket::new_unchecked(payload),
                &THEIR_IP.into(),
                &OUR_IP.into(),
                data.len(),
                |room| room.copy_from_slice(data),
                &ChecksumCapabilities::default(),
            )
        })
    }

    /// What a DHCP server offers, and then agrees to.
    pub struct Offered {
        pub address: Ipv4Address,
        pub mask: Ipv4Address,
        pub router: Option<Ipv4Address>,
        pub dns: [Option<Ipv4Address>; 3],
        /// How long the lease lasts.
        pub seconds: u32,
    }

    /// THEIRS, as the DHCP server, answers transaction `xid` with `kind`
    /// (an offer, or the acknowledgement of a request): to everybody, as a
    /// server does for a client that has no address yet.
    pub fn dhcp_frame(kind: DhcpMessageType, xid: u32, offered: &Offered) -> Vec<u8> {
        let dhcp = DhcpRepr {
            message_type: kind,
            transaction_id: xid,
            client_hardware_address: OURS,
            client_ip: Ipv4Address::UNSPECIFIED,
            your_ip: offered.address,
            server_ip: THEIR_IP,
            router: offered.router,
            subnet_mask: Some(offered.mask),
            relay_agent_ip: Ipv4Address::UNSPECIFIED,
            broadcast: false,
            requested_ip: None,
            client_identifier: None,
            server_identifier: Some(THEIR_IP),
            parameter_request_list: None,
            dns_servers: Some(offered.dns),
            max_size: None,
            lease_duration: Some(offered.seconds),
        };
        let udp = UdpRepr {
            src_port: 67,
            dst_port: 68,
        };
        let (everybody, len) = (Ipv4Address::BROADCAST, dhcp.buffer_len());
        ipv4_frame_for(
            EthernetAddress::BROADCAST,
            everybody,
            IpProtocol::Udp,
            8 + len,
            |payload| {
                udp.emit(
                    &mut UdpPacket::new_unchecked(payload),
                    &THEIR_IP.into(),
                    &everybody.into(),
                    len,
                    |room| dhcp.emit(&mut DhcpPacket::new_unchecked(room)).unwrap(),
                    &ChecksumCapabilities::default(),
                )
            },
        )
    }

    /// The DHCP message `frame` carries from us to everybody's server: its
    /// kind, its transaction, and the address it asks for.
    pub fn dhcp_of(frame: &[u8]) -> (DhcpMessageType, u32, Option<Ipv4Address>) {
        let frame = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!(frame.ethertype(), EthernetProtocol::Ipv4);
        assert_eq!(
            (frame.src_addr(), frame.dst_addr()),
            (OURS, EthernetAddress::BROADCAST)
        );
        let ip = Ipv4Packet::new_checked(frame.payload()).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!(ip.protocol(), IpProtocol::Udp);
        assert_eq!(ip.dst_addr(), Ipv4Address::BROADCAST);
        let udp = UdpPacket::new_checked(ip.payload()).unwrap();
        assert_eq!((udp.src_port(), udp.dst_port()), (68, 67));
        let dhcp = DhcpPacket::new_checked(udp.payload()).unwrap();
        let dhcp = DhcpRepr::parse(&dhcp).unwrap();
        assert_eq!(dhcp.client_hardware_address, OURS);
        (dhcp.message_type, dhcp.transaction_id, dhcp.requested_ip)
    }

    /// A TCP segment: its ports, what it does, its numbers and what it
    /// carries.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Segment {
        pub from: u16,
        pub to: u16,
        pub control: TcpControl,
        pub seq: u32,
        pub ack: Option<u32>,
        pub data: Vec<u8>,
    }

    impl Segment {
        /// The sequence number after this segment.
        pub fn end(&self) -> u32 {
            let flag = matches!(self.control, TcpControl::Syn | TcpControl::Fin) as u32;
            self.seq
                .wrapping_add(self.data.len() as u32)
                .wrapping_add(flag)
        }
    }

    /// The frame of a TCP segment from THEIRS to us, with a window of
    /// 65535 bytes.
    pub fn tcp_frame(segment: &Segment) -> Vec<u8> {
        let tcp = TcpRepr {
            src_port: segment.from,
            dst_port: segment.to,
            control: segment.control,
            seq_number: TcpSeqNumber(segment.seq as i32),
            ack_number: segment.ack.map(|ack| TcpSeqNumber(ack as i32)),
            window_len: u16::MAX,
            window_scale: None,
            max_seg_size: (segment.control == TcpControl::Syn).then_some(1460),
            sack_permitted: false,
            sack_ranges: [None; 3],
            payload: &segment.data,
        };
        ipv4_frame(OUR_IP, IpProtocol::Tcp, tcp.buffer_len(), |payload| {
            tcp.emit(
                &mut TcpPacket::new_unchecked(payload),
                &THEIR_IP.into(),
                &OUR_IP.into(),
                &ChecksumCapabilities::default(),
            )
        })
    }

    /// The TCP segment `frame` carries from us to THEIRS, when its
    /// checksums hold.
    pub fn tcp_of(frame: &[u8]) -> Segment {
        let ip = ipv4_of(frame);
        let ip = Ipv4Packet::new_checked(&ip[..]).unwrap();
        assert!(ip.verify_checksum());
        assert_eq!(ip.protocol(), IpProtocol::Tcp);
        assert_eq!((ip.src_addr(), ip.dst_addr()), (OUR_IP, THEIR_IP));
        let tcp = TcpPacket::new_checked(ip.payload()).unwrap();
        let tcp = TcpRepr::parse(
            &tcp,
            &OUR_IP.into(),
            &THEIR_IP.into(),
            &ChecksumCapabilities::default(),
        )
        .unwrap();
        Segment {
            from: tcp.src_port,
            to: tcp.dst_port,
            control: tcp.control,
            seq: tcp.seq_number.0 as u32,
            ack: tcp.ack_number.map(|ack| ack.0 as u32),
            data: tcp.payload.to_vec(),
        }
    }

    /// The IPv4 packet `frame` carries from us to THEIRS.
    pub fn ipv4_of(frame: &[u8]) -> Vec<u8> {
        let frame = EthernetFrame::new_checked(frame).unwrap();
        assert_eq!(frame.ethertype(), EthernetProtocol::Ipv4);
        assert_eq!((frame.src_addr(), frame.dst_addr()), (OURS, THEIRS));
        frame.payload().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{
        ArpOperation, ArpPacket, ArpRepr, EthernetFrame, Icmpv4Packet, Icmpv4Repr, Ipv4Address,
        Ipv4Packet, Ipv4Repr,
    };

    #[test]
    fn a_card_has_some_slots_each_way_and_not_too_many() {
        let p = core::ptr::null_mut();
        // SAFETY: nothing is read or written through these.
        unsafe {
            assert!(Port::new(p, 64, 32).is_some());
            assert!(Port::new(p, 1, 1).is_some());
            assert!(Port::new(p, 0, 32).is_none());
            assert!(Port::new(p, 64, 0).is_none());
            assert!(Port::new(p, 64, 33).is_none());
            assert!(Port::new(p, MAX_RX_SLOTS + 1, 32).is_none());
            assert_eq!(Port::new(p, 64, 32).unwrap().slots(), 96);
        }
    }

    #[test]
    fn a_message_that_means_nothing_is_refused() {
        let bench = Bench::new();
        let port = bench.iface.device();
        let msg = |op, slot, len, off| Msg { op, slot, len, off };
        // Not a receive slot; no frame; more than a frame; off the slot.
        assert!(!port.take(&msg(OP_RX, RX as u32, 60, 10)));
        assert!(!port.take(&msg(OP_RX, 0, 0, 10)));
        assert!(!port.take(&msg(OP_RX, 0, FRAME_MAX as u32 + 1, 10)));
        assert!(!port.take(&msg(OP_RX, 0, 1514, 600)));
        assert!(!port.take(&msg(OP_RX, 0, 60, u32::MAX)));
        // Not a transmit slot, and no operation at all.
        assert!(!port.take(&msg(OP_TX, TX as u32, 0, 0)));
        assert!(!port.take(&msg(7, 0, 0, 0)));
        assert_eq!(port.waiting(), 0);
        // And the ones that do.
        assert!(port.take(&msg(OP_RX, RX as u32 - 1, 1514, 12)));
        assert!(port.take(&msg(OP_TX, TX as u32 - 1, 0, 0)));
        assert_eq!(port.waiting(), 1);
    }

    #[test]
    fn who_has_our_address_is_answered_and_the_slot_goes_back() {
        let mut bench = Bench::new();
        bench.deliver(2, &arp_request(OUR_IP));
        bench.poll();
        let turn = bench.turn();
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0], rx_back(2));
        let answer = bench.sent(&turn[1]);
        let frame = EthernetFrame::new_checked(&answer[..]).unwrap();
        assert_eq!(frame.dst_addr(), THEIRS);
        assert_eq!(frame.src_addr(), OURS);
        let arp = ArpRepr::parse(&ArpPacket::new_checked(frame.payload()).unwrap()).unwrap();
        assert_eq!(
            arp,
            ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr: OURS,
                source_protocol_addr: OUR_IP,
                target_hardware_addr: THEIRS,
                target_protocol_addr: THEIR_IP,
            }
        );
        // Said once: the next turn has nothing.
        assert!(bench.turn().is_empty());
    }

    #[test]
    fn who_has_another_address_only_gets_its_slot_back() {
        let mut bench = Bench::new();
        bench.deliver(0, &arp_request(Ipv4Address([10, 0, 9, 99])));
        bench.poll();
        assert_eq!(bench.turn(), [rx_back(0)]);
    }

    #[test]
    fn a_ping_is_answered_with_what_it_carried() {
        let mut bench = Bench::new();
        bench.meet();
        let data: Vec<u8> = (0..1400u32).map(|i| i as u8).collect();
        bench.deliver(1, &echo_request(OUR_IP, 0x1234, 7, &data));
        bench.poll();
        let turn = bench.turn();
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0], rx_back(1));
        let answer = bench.sent(&turn[1]);
        let frame = EthernetFrame::new_checked(&answer[..]).unwrap();
        assert_eq!(frame.dst_addr(), THEIRS);
        let caps = ChecksumCapabilities::default();
        let packet = Ipv4Packet::new_checked(frame.payload()).unwrap();
        let ip = Ipv4Repr::parse(&packet, &caps).unwrap();
        assert_eq!((ip.src_addr, ip.dst_addr), (OUR_IP, THEIR_IP));
        let icmp = Icmpv4Packet::new_checked(packet.payload()).unwrap();
        assert_eq!(
            Icmpv4Repr::parse(&icmp, &caps).unwrap(),
            Icmpv4Repr::EchoReply {
                ident: 0x1234,
                seq_no: 7,
                data: &data,
            }
        );
    }

    #[test]
    fn a_frame_waits_in_its_slot_for_room_to_answer_it() {
        let mut bench = Bench::new();
        bench.meet();
        // Three pings and two transmit slots.
        for slot in 0..3 {
            bench.deliver(slot, &echo_request(OUR_IP, 1, slot as u16, b"abc"));
        }
        bench.poll();
        let turn = bench.turn();
        assert_eq!(
            turn.iter().map(|m| (m.op, m.slot)).collect::<Vec<_>>(),
            [(OP_RX, 0), (OP_TX, 0), (OP_RX, 1), (OP_TX, 1)]
        );
        assert!(!bench.iface.device().can_send());
        assert_eq!(bench.iface.device().waiting(), 1);
        // Nothing more until the card is done with a frame: the third
        // ping's slot is not given back unread.
        bench.poll();
        assert!(bench.turn().is_empty());
        assert!(bench.iface.device().take(&Msg {
            op: OP_TX,
            slot: 1,
            len: 0,
            off: 0,
        }));
        bench.poll();
        let turn = bench.turn();
        assert_eq!(turn[0], rx_back(2));
        assert_eq!((turn[1].op, turn[1].slot), (OP_TX, 1));
        assert_eq!(bench.iface.device().waiting(), 0);
    }

    #[test]
    fn a_frame_to_send_starts_from_zeroes_and_one_that_fails_frees_its_slot() {
        use smoltcp::phy::{Device, TxToken};
        let mut bench = Bench::new();
        let port = bench.iface.device_mut();
        // The window is 0xa5 all over: none of it reaches whoever writes
        // the frame, so a frame that leaves a byte unwritten sends a zero
        // and not what the slot held before.
        let wrote = port
            .transmit()
            .unwrap()
            .consume(Instant::from_millis(1), 60, |frame| {
                assert!(frame.iter().all(|&b| b == 0));
                frame[0] = 1;
                Ok(frame.len())
            });
        assert_eq!(wrote, Ok(60));
        let mut turn = [Msg::default(); 4];
        assert_eq!(port.outgoing(&mut turn), 1);
        assert_eq!(
            turn[0],
            Msg {
                op: OP_TX,
                slot: 0,
                len: 60,
                off: TX_HEADROOM as u32,
            }
        );
        // A frame that could not be built is not sent, and its slot is
        // free again: the next one gets the same.
        let failed: smoltcp::Result<()> =
            port.transmit()
                .unwrap()
                .consume(Instant::from_millis(1), 60, |_| {
                    Err(smoltcp::Error::Truncated)
                });
        assert_eq!(failed, Err(smoltcp::Error::Truncated));
        assert_eq!(port.outgoing(&mut turn), 0);
        // No frame of no bytes, and none longer than a frame.
        for len in [0, FRAME_MAX + 1] {
            let refused =
                port.transmit()
                    .unwrap()
                    .consume(Instant::from_millis(1), len, |_| Ok(()));
            assert_eq!(refused, Err(smoltcp::Error::Exhausted));
        }
        // Both slots taken: no leave to send a third.
        for _ in 0..TX - 1 {
            let _ = port
                .transmit()
                .unwrap()
                .consume(Instant::from_millis(1), 60, |_| Ok(()));
        }
        assert!(port.transmit().is_none());
    }

    #[test]
    fn messages_that_did_not_go_are_the_first_of_the_next_turn() {
        let mut bench = Bench::new();
        bench.deliver(0, &arp_request(Ipv4Address([10, 0, 9, 99])));
        bench.deliver(1, &arp_request(Ipv4Address([10, 0, 9, 98])));
        bench.poll();
        let first = bench.turn();
        assert_eq!(first, [rx_back(0), rx_back(1)]);
        // Meanwhile the stack made more to say; what was not said goes
        // before it.
        bench.deliver(2, &arp_request(Ipv4Address([10, 0, 9, 97])));
        bench.poll();
        bench.iface.device().put_back(&first[1..]);
        assert_eq!(bench.turn(), [rx_back(1), rx_back(2)]);
        bench.iface.device().put_back(&[]);
        assert!(bench.turn().is_empty());
    }

    #[test]
    fn a_turn_that_does_not_fit_keeps_the_rest_for_the_next() {
        let mut bench = Bench::new();
        bench.deliver(0, &arp_request(OUR_IP));
        bench.poll();
        let mut one = [Msg::default(); 1];
        assert_eq!(bench.iface.device().outgoing(&mut one), 1);
        assert_eq!(one[0], rx_back(0));
        assert_eq!(bench.iface.device().outgoing(&mut one), 1);
        assert_eq!(one[0].op, OP_TX);
        assert_eq!(bench.iface.device().outgoing(&mut one), 0);
    }

    #[test]
    fn the_driver_is_woken_for_a_frame_or_a_quarter_of_the_slots() {
        use smoltcp::phy::{Device, RxToken as _, TxToken as _};
        const SLOTS: usize = 16;
        let mut mem = vec![0u8; (SLOTS + 2) * BUF_SIZE];
        // SAFETY: the window is `mem`, which outlives the port and is
        // nobody else's.
        let mut port = unsafe { Port::new(mem.as_mut_ptr(), SLOTS, 2) }.unwrap();
        let now = Instant::from_millis(0);
        let read = |port: &mut Port, slot: u32| {
            assert!(port.take(&Msg {
                op: OP_RX,
                slot,
                len: 60,
                off: 0,
            }));
            let (rx, _) = port.receive().unwrap();
            rx.consume(now, |_| Ok(())).unwrap();
        };
        assert!(!port.due(), "nothing to say");
        for slot in 0..3 {
            read(&mut port, slot);
            assert!(!port.due(), "{} slots to give back", slot + 1);
        }
        assert!(!port.has_frames());
        // A frame to send takes them with it.
        let tx = port.transmit().unwrap();
        tx.consume(now, 60, |_| Ok(())).unwrap();
        assert!(port.has_frames() && port.due());
        assert_eq!(turn(&port).len(), 4);
        assert!(!port.has_frames() && !port.due());
        // And a quarter of the slots is worth the wake by itself.
        for slot in 3..7 {
            read(&mut port, slot);
        }
        assert!(port.due() && !port.has_frames());
    }
}
