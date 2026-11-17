//! Raw sockets of IPv4 for the kernel's sockets: what a program opens
//! with `socket(AF_INET, SOCK_RAW, protocol)` when the protocol is not
//! ICMP, which has its own bargain ([`super`]).
//!
//! ```text
//! RAW     (kernel to stack)  16  u8 protocol  17  three zero
//! PACKET  (kernel to stack)  16  the IPv4 packet, whole
//! ```
//!
//! A socket that speaks a protocol says so with `RAW`, and is gone with
//! its `CLOSE`. While any does, every IPv4 packet of that protocol the
//! card brings goes to the kernel whole, as a record of a `DATA` for
//! socket 0 and port 0, like what is heard of ICMP: which sockets it is
//! for is the kernel's to say, by the protocol its header names. The
//! stack goes on doing with the packet what it did: a datagram still
//! reaches the socket that has its port, a segment its connection.
//! Protocol 255 is the one nothing arrives for (`IPPROTO_RAW`): a socket
//! of it only sends.
//!
//! A `PACKET` is a packet to send for such a socket, header and all: the
//! kernel wrote the header, or the program did (`IP_HDRINCL`). It may be
//! of another protocol than the socket's, as Linux lets it. What the
//! stack does to it is what a host does to a packet a program made: one
//! from nowhere is from the card's address, and its sum is written
//! again. smoltcp carries a header of twenty bytes that is never a
//! fragment, so options and the identification do not travel.

use std::collections::BTreeMap;

use smoltcp::phy::Device;
use smoltcp::socket::{RawPacketMetadata, RawSocket, RawSocketBuffer, SocketHandle};
use smoltcp::wire::{IpProtocol, IpVersion, Ipv4Packet};

use super::{push_record, Refused, Stack};
use super::{HEADER_LEN, HEARD, MESSAGE_MAX, RECORD_LEN, RX_DATAGRAMS, TX_DATAGRAMS};

/// The operations of a message.
pub const OP_RAW: u32 = 29;
pub const OP_PACKET: u32 = 30;

/// ICMP, whose sockets say `HEAR`.
const ICMP: u8 = 1;
/// `IPPROTO_RAW`: no packet is of it.
const NONE: u8 = 255;
/// How many protocols one socket sends packets of.
const MOUTHS: usize = 4;

/// A socket of whole IPv4 packets of `protocol`, the longest of `room`
/// bytes, that holds so many that arrived and so many to send.
fn socket(protocol: u8, room: usize, arrived: usize, to_send: usize) -> RawSocket<'static> {
    let buffer = |packets: usize| {
        RawSocketBuffer::new(
            vec![RawPacketMetadata::EMPTY; packets],
            vec![0u8; packets * room],
        )
    };
    RawSocket::new(
        IpVersion::Ipv4,
        IpProtocol::from(protocol),
        buffer(arrived),
        buffer(to_send),
    )
}

/// A raw socket of the kernel's.
struct Raw {
    protocol: u8,
    /// What it sends by, one for each protocol it has sent a packet of:
    /// sockets that none reaches, its own, as each datagram socket has
    /// its queue.
    mouths: Vec<(u8, SocketHandle)>,
}

/// The raw sockets, and what hears for them.
#[derive(Default)]
pub(super) struct Raws {
    socks: BTreeMap<u64, Raw>,
    /// For each protocol a socket speaks, the socket every packet of it
    /// reaches, and how many speak it.
    ears: BTreeMap<u8, (SocketHandle, usize)>,
}

impl Raws {
    /// Whether `sock` is a raw socket.
    pub(super) fn knows(&self, sock: u64) -> bool {
        self.socks.contains_key(&sock)
    }

    /// How many raw sockets there are.
    pub(super) fn len(&self) -> usize {
        self.socks.len()
    }
}

impl<D: for<'d> Device<'d> + crate::port::Listened> Stack<D> {
    /// How many raw sockets there are.
    pub fn raws(&self) -> usize {
        self.raws.len()
    }

    /// `RAW`: `sock` speaks the protocol the message names from now on.
    pub(super) fn raw_open(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        let &[protocol, 0, 0, 0] = &m[HEADER_LEN..] else {
            return Err(Refused::Malformed);
        };
        if protocol == ICMP {
            return Err(Refused::Malformed);
        }
        if sock == HEARD || self.knows(sock) {
            return Err(Refused::Bind);
        }
        if protocol != NONE {
            let room = self.iface.device().capabilities().ip_mtu();
            let set = &mut self.set;
            let ear = self
                .raws
                .ears
                .entry(protocol)
                .or_insert_with(|| (set.add(socket(protocol, room, RX_DATAGRAMS, 0)), 0));
            ear.1 += 1;
        }
        let mouths = Vec::new();
        self.raws.socks.insert(sock, Raw { protocol, mouths });
        Ok(false)
    }

    /// `PACKET`: a packet of `sock`'s to send.
    pub(super) fn raw_packet(&mut self, sock: u64, m: &[u8]) -> Result<bool, Refused> {
        let said = &m[HEADER_LEN..];
        if !self.raws.knows(sock) {
            return Err(Refused::NoSocket);
        }
        let header = Ipv4Packet::new_checked(said).map_err(|_| Refused::Malformed)?;
        if header.version() != 4 || usize::from(header.total_len()) != said.len() {
            return Err(Refused::Malformed);
        }
        if header.hop_limit() == 0 {
            return Err(Refused::Malformed);
        }
        let room = self.iface.device().capabilities().ip_mtu();
        if said.len() > room {
            return Err(Refused::TooLong);
        }
        // What is sent before the card has an address has nobody to come
        // back to.
        let ours = self.address().ok_or(Refused::Unreachable)?.address();
        if !self.reaches(header.dst_addr()) {
            return Err(Refused::Unreachable);
        }
        let from_nowhere = header.src_addr().is_unspecified();
        let protocol = u8::from(header.protocol());
        let raw = self.raws.socks.get_mut(&sock).ok_or(Refused::NoSocket)?;
        let handle = match raw.mouths.iter().find(|(of, _)| *of == protocol) {
            Some(&(_, handle)) => handle,
            None if raw.mouths.len() < MOUTHS => {
                let handle = self.set.add(socket(protocol, room, 0, TX_DATAGRAMS));
                raw.mouths.push((protocol, handle));
                handle
            }
            None => return Err(Refused::Full),
        };
        let mut socket = self.set.get::<RawSocket>(handle);
        let packet = socket.send(said.len()).map_err(|_| Refused::Full)?;
        packet.copy_from_slice(said);
        if from_nowhere {
            Ipv4Packet::new_unchecked(packet).set_src_addr(ours);
        }
        Ok(true)
    }

    /// `CLOSE` of a raw socket, if `sock` is one. What it sent and can
    /// leave now does; with the last that speaks its protocol, what
    /// arrived of it and was not told is nobody's.
    pub(super) fn raw_close(&mut self, sock: u64) -> bool {
        let Some(raw) = self.raws.socks.remove(&sock) else {
            return false;
        };
        let _ = self.iface.poll(&mut self.set, self.now);
        for (_, handle) in raw.mouths {
            self.set.remove(handle);
        }
        if let Some(ear) = self.raws.ears.get_mut(&raw.protocol) {
            ear.1 -= 1;
            if ear.1 == 0 {
                let (handle, _) = self.raws.ears.remove(&raw.protocol).expect("it is there");
                self.set.remove(handle);
            }
        }
        true
    }

    /// Add to `m`, a `DATA`, the packets that arrived for the raw
    /// sockets, taking them off the ears. `false` when `m` has no room
    /// for the rest.
    pub(super) fn raw_arrived(&mut self, m: &mut Vec<u8>) -> bool {
        let room = self.iface.device().capabilities().ip_mtu();
        for &(handle, _) in self.raws.ears.values() {
            let mut socket = self.set.get::<RawSocket>(handle);
            while socket.can_recv() {
                if m.len() + RECORD_LEN + room > MESSAGE_MAX {
                    return false;
                }
                let Ok(packet) = socket.recv() else {
                    break;
                };
                if let Ok(header) = Ipv4Packet::new_checked(packet) {
                    push_record(m, HEARD, header.src_addr().0, 0, packet);
                    self.brought = true;
                }
            }
        }
        true
    }
}
