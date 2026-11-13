//! What did not get where it was sent: the errors ICMP brings.
//!
//! A router with no way on, or a host with nobody at the port, says so in
//! an ICMP message that carries the start of what it could not deliver:
//! its IPv4 header and the first bytes behind it, which for UDP and for
//! TCP are the two ports. That is enough to know whose it was, and this
//! says it ([`Undelivered::of`]).
//!
//! What the program is told is not the stack's to decide. The kernel
//! hears which socket it was and what the message said, in a record of an
//! `EVENTS` ([`super::tcp`]):
//!
//! ```text
//!     UNDELIVERED  socket: a datagram socket's.
//!                  bytes: [u8; 4] to, u16 port, u8 type, u8 code
//!     FAILED       number: unreachable.  bytes: u8 type, u8 code
//! ```
//!
//! The first for a datagram, whatever socket of the kernel's sent it from
//! the port it left by; whether that socket's program hears of it is the
//! kernel's business (on Linux a connected one does, of some). The second
//! for a connect under way, which ends there: the socket is forgotten
//! without another word to a peer there is no way to. A connection that
//! is made is told nothing: what it sent is sent again, as Linux does.
//!
//! Two types are errors of this kind, destination unreachable (3) and
//! time exceeded (11). A message that says a datagram was too long for
//! the way (3, code 4) is passed on like the rest: this stack sends
//! nothing longer than a frame and does not learn a shorter way from it.

use smoltcp::wire::{Icmpv4Packet, IpProtocol, Ipv4Address, Ipv4Packet};

/// The record of an `EVENTS` that says a datagram was not delivered.
pub const EV_UNDELIVERED: u16 = 7;
/// The bytes of that record.
pub const UNDELIVERED_LEN: usize = 8;

/// ICMP's destination unreachable.
pub const UNREACHABLE: u8 = 3;
/// ICMP's time exceeded.
pub const TIME_EXCEEDED: u8 = 11;
/// The code of an unreachable that is about the length of what was sent,
/// not about where it went.
pub const TOO_LONG: u8 = 4;
/// The last code of an unreachable that means something.
pub const LAST_CODE: u8 = 15;

/// The IPv4 header a message quotes, without options.
const QUOTED_HEADER: usize = 20;

/// What an ICMP message says was not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undelivered {
    /// The message's type and code.
    pub kind: u8,
    pub code: u8,
    /// UDP or TCP.
    pub protocol: IpProtocol,
    /// Who sent it, and from which port.
    pub from: Ipv4Address,
    pub local: u16,
    /// Where it was going.
    pub to: Ipv4Address,
    pub port: u16,
}

impl Undelivered {
    /// What `packet`, an IPv4 packet of ICMP, says was not delivered:
    /// `None` when it is no such message, when its checksum does not hold
    /// or when what it quotes is not the start of a UDP datagram or of a
    /// TCP segment.
    pub fn of(packet: &[u8]) -> Option<Undelivered> {
        let packet = Ipv4Packet::new_checked(packet).ok()?;
        if packet.protocol() != IpProtocol::Icmp {
            return None;
        }
        let message = packet.payload();
        if !Icmpv4Packet::new_checked(message).ok()?.verify_checksum() {
            return None;
        }
        let (kind, code) = (message[0], message[1]);
        if !matches!(kind, UNREACHABLE | TIME_EXCEEDED) {
            return None;
        }
        // Behind the type, the code, the checksum and four bytes more.
        let quoted = message.get(8..)?;
        if quoted.len() < QUOTED_HEADER || quoted[0] >> 4 != 4 {
            return None;
        }
        let header = usize::from(quoted[0] & 0x0f) * 4;
        if header < QUOTED_HEADER {
            return None;
        }
        // The ports are in the first fragment and in no other.
        if u16::from_be_bytes([quoted[6], quoted[7]]) & 0x1fff != 0 {
            return None;
        }
        let protocol = IpProtocol::from(quoted[9]);
        if !matches!(protocol, IpProtocol::Udp | IpProtocol::Tcp) {
            return None;
        }
        let ports = quoted.get(header..header + 4)?;
        Some(Undelivered {
            kind,
            code,
            protocol,
            from: Ipv4Address::from_bytes(&quoted[12..16]),
            local: u16::from_be_bytes([ports[0], ports[1]]),
            to: Ipv4Address::from_bytes(&quoted[16..20]),
            port: u16::from_be_bytes([ports[2], ports[3]]),
        })
    }

    /// The bytes of the `UNDELIVERED` that says it.
    pub fn bytes(&self) -> [u8; UNDELIVERED_LEN] {
        let mut bytes = [0u8; UNDELIVERED_LEN];
        bytes[..4].copy_from_slice(&self.to.0);
        bytes[4..6].copy_from_slice(&self.port.to_le_bytes());
        bytes[6] = self.kind;
        bytes[7] = self.code;
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::testing::*;

    /// The IPv4 packet of a frame from THEIRS, which is what the stack is
    /// given of it.
    fn packet(frame: &[u8]) -> Vec<u8> {
        frame[14..].to_vec()
    }

    #[test]
    fn an_unreachable_says_whose_datagram_it_was() {
        let said = undelivered_frame(3, 3, IpProtocol::Udp, 7000, THEIR_IP, 53);
        assert_eq!(
            Undelivered::of(&packet(&said)),
            Some(Undelivered {
                kind: 3,
                code: 3,
                protocol: IpProtocol::Udp,
                from: OUR_IP,
                local: 7000,
                to: THEIR_IP,
                port: 53,
            })
        );
        let said = undelivered_frame(11, 0, IpProtocol::Tcp, 49152, Ipv4Address([1, 2, 3, 4]), 80);
        let said = Undelivered::of(&packet(&said)).unwrap();
        assert_eq!(
            (said.kind, said.code, said.protocol),
            (11, 0, IpProtocol::Tcp)
        );
        assert_eq!(
            (said.local, said.to, said.port),
            (49152, Ipv4Address([1, 2, 3, 4]), 80)
        );
        // The record: where it went, then what was said.
        assert_eq!(said.bytes(), [1, 2, 3, 4, 80, 0, 11, 0]);
        assert_eq!((EV_UNDELIVERED, UNDELIVERED_LEN), (7, 8));
    }

    #[test]
    fn what_is_no_such_message_says_nothing() {
        // An echo's answer, and an echo.
        assert_eq!(Undelivered::of(&packet(&echo_reply(7, 1, b"pong"))), None);
        assert_eq!(
            Undelivered::of(&packet(&echo_request(OUR_IP, 7, 1, b"x"))),
            None
        );
        // Not ICMP at all.
        assert_eq!(
            Undelivered::of(&packet(&udp_frame(4000, 7000, b"hello"))),
            None
        );
        // A quench, a redirect, a parameter that is wrong: not said here.
        for kind in [4, 5, 12] {
            let said = undelivered_frame(kind, 0, IpProtocol::Udp, 7000, THEIR_IP, 53);
            assert_eq!(Undelivered::of(&packet(&said)), None, "type {kind}");
        }
        // About an echo of ours: no ports to know a socket by.
        let said = undelivered_frame(3, 1, IpProtocol::Icmp, 7000, THEIR_IP, 53);
        assert_eq!(Undelivered::of(&packet(&said)), None);

        let good = packet(&undelivered_frame(
            3,
            3,
            IpProtocol::Udp,
            7000,
            THEIR_IP,
            53,
        ));
        assert!(Undelivered::of(&good).is_some());
        // A checksum that does not hold.
        let mut bad = good.clone();
        bad[20 + 1] ^= 1;
        assert_eq!(Undelivered::of(&bad), None);
        // Cut before the ports: the packet's own length says so, and
        // its checksum is redone so that only the length is wrong.
        let cut = icmp_frame(&undelivered_message(3, 3, &good[28..28 + 22]));
        assert_eq!(Undelivered::of(&packet(&cut)), None);
        // A later fragment of something, where the ports are not.
        let mut quoted = good[28..].to_vec();
        quoted[7] = 1;
        let later = icmp_frame(&undelivered_message(3, 3, &quoted));
        assert_eq!(Undelivered::of(&packet(&later)), None);
        // IPv4 options in what is quoted move the ports, and are read.
        let mut quoted = good[28..48].to_vec();
        quoted[0] = 0x46;
        quoted.extend_from_slice(&[1, 1, 1, 1]);
        quoted.extend_from_slice(&good[48..]);
        let with_options = icmp_frame(&undelivered_message(3, 3, &quoted));
        let said = Undelivered::of(&packet(&with_options)).unwrap();
        assert_eq!((said.local, said.port), (7000, 53));
    }
}
